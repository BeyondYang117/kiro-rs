use super::*;
use tokio::time::{sleep, timeout};

fn credential(id: u64) -> KiroCredentials {
    KiroCredentials {
        id: Some(id),
        kiro_api_key: Some(format!("concurrency-test-{id}")),
        subscription_title: Some("KIRO PRO".into()),
        ..Default::default()
    }
}

fn manager(settings: serde_json::Value, count: u64) -> Arc<MultiTokenManager> {
    Arc::new(
        MultiTokenManager::new(
            serde_json::from_value(settings).unwrap(),
            (1..=count).map(credential).collect(),
            None,
            None,
            true,
        )
        .unwrap(),
    )
}

async fn wait_for_waiters(manager: &MultiTokenManager, count: usize) {
    timeout(StdDuration::from_secs(2), async {
        while manager.waiting_slots.available_permits()
            != manager.config.concurrency.max_waiting - count
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("waiter registration must complete");
}

#[tokio::test]
async fn rpm_budget_is_shared_by_reservations_and_recovers_without_holding_capacity() {
    let manager = manager(
        serde_json::json!({"concurrency":{
            "requestsPerMinute":2, "waitTimeoutSeconds":1
        }}),
        1,
    );
    let first = manager.acquire_context(None).await.unwrap();
    let second = manager.acquire_context(None).await.unwrap();
    drop((first, second));
    assert_eq!(manager.snapshot().concurrency.available_slots, 0);
    let pending_manager = manager.clone();
    let pending = tokio::spawn(async move { pending_manager.acquire_context(None).await });
    wait_for_waiters(&manager, 1).await;
    assert_eq!(manager.snapshot().concurrency.in_flight, 0);
    pending.abort();
    let _ = pending.await;
    assert_eq!(manager.snapshot().concurrency.waiting, 0);
    // Leave the rolling window about to expire, without sleeping for a minute.
    for time in manager.entries.lock()[0].request_times.iter_mut() {
        *time = Instant::now() - StdDuration::from_millis(59_950);
    }
    let context = timeout(StdDuration::from_secs(1), manager.acquire_context(None))
        .await
        .unwrap()
        .unwrap();
    drop(context);
    assert_eq!(manager.entries.lock()[0].request_times.len(), 1);
    assert_eq!(manager.snapshot().concurrency.in_flight, 0);
}

#[tokio::test]
async fn rpm_limit_is_per_account_and_can_be_disabled_explicitly() {
    let limited = manager(
        serde_json::json!({"concurrency":{
            "requestsPerMinute":1, "maxWaiting":0
        }}),
        2,
    );
    let first = limited.acquire_context(None).await.unwrap();
    let second = limited.acquire_context(None).await.unwrap();
    assert_ne!(first.id, second.id);
    drop((first, second));
    assert!(
        limited
            .acquire_context(None)
            .await
            .err()
            .unwrap()
            .is::<CapacityError>()
    );
    let unlimited = manager(
        serde_json::json!({"concurrency":{"requestsPerMinute":0}}),
        1,
    );
    for _ in 0..80 {
        drop(unlimited.acquire_context(None).await.unwrap());
    }
    assert!(unlimited.entries.lock()[0].request_times.is_empty());
}

#[tokio::test]
async fn affinity_is_per_session_and_remaps_only_after_success_in_both_modes() {
    for mode in ["balanced", "priority"] {
        let manager = manager(
            serde_json::json!({"loadBalancingMode":mode,
            "concurrency":{"maxInFlightPerCredential":1}}),
            2,
        );
        let a = [1; 32];
        let b = [2; 32];
        // Selecting an account alone must not create a successful binding.
        let first = manager
            .acquire_context_with_affinity(None, Some(&a))
            .await
            .unwrap();
        assert_eq!(first.id, 1);
        drop(first);
        manager.report_session_success(Some(a), 1);
        manager.report_session_success(Some(b), 2);
        let held = manager
            .acquire_context_with_affinity(None, Some(&a))
            .await
            .unwrap();
        assert_eq!(held.id, 1);
        let other = manager
            .acquire_context_with_affinity(None, Some(&b))
            .await
            .unwrap();
        assert_eq!(other.id, 2);
        drop(other);
        // A's account is full: use account 2 immediately, even though account 1 is preferred.
        let fallback = manager
            .acquire_context_with_affinity(None, Some(&a))
            .await
            .unwrap();
        assert_eq!(fallback.id, 2);
        drop(fallback);
        drop(held);
        let unchanged = manager
            .acquire_context_with_affinity(None, Some(&a))
            .await
            .unwrap();
        assert_eq!(unchanged.id, 1, "failed/cancelled fallback cannot remap");
        let successful = manager
            .acquire_context_with_affinity(None, Some(&a))
            .await
            .unwrap();
        assert_eq!(successful.id, 2);
        manager.report_session_success(Some(a), successful.id);
        drop((unchanged, successful));
        let rebound = manager
            .acquire_context_with_affinity(None, Some(&a))
            .await
            .unwrap();
        assert_eq!(rebound.id, 2);
        let metrics = manager.snapshot().concurrency;
        assert_eq!(metrics.affinity_miss_total, 1);
        assert_eq!(metrics.affinity_hit_total, 4);
        assert_eq!(metrics.affinity_fallback_total, 2);
        assert_eq!(metrics.affinity_switch_total, 1);
    }
}

#[tokio::test]
async fn affinity_skips_cooling_disabled_deleted_and_model_ineligible_accounts() {
    for unavailable in ["cooling", "disabled", "deleted", "model"] {
        let manager = manager(serde_json::json!({}), 2);
        let key = [1; 32];
        manager.report_session_success(Some(key), 1);
        {
            let mut entries = manager.entries.lock();
            match unavailable {
                "cooling" => {
                    entries[0].cooldown = Some((Instant::now(), StdDuration::from_secs(60)))
                }
                "disabled" => {
                    entries[0].disabled = true;
                    entries[0].disabled_reason = Some(DisabledReason::Manual);
                }
                "deleted" => {
                    entries.remove(0);
                }
                "model" => entries[0].credentials.subscription_title = Some("KIRO FREE".into()),
                _ => unreachable!(),
            }
        }
        let context = timeout(
            StdDuration::from_secs(1),
            manager.acquire_context_with_affinity(Some("claude-opus-4-6"), Some(&key)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(context.id, 2, "{unavailable}");
        assert_eq!(manager.snapshot().concurrency.affinity_fallback_total, 1);
        // A deleted account's delayed completion must not replace the new binding.
        manager.report_session_success(Some(key), 2);
        manager.report_session_success(Some(key), 1);
        if unavailable == "deleted" || unavailable == "disabled" {
            assert_eq!(manager.snapshot().concurrency.affinity_switch_total, 1);
        }
    }
}

#[tokio::test]
async fn affinity_does_not_jump_the_queue_and_can_be_disabled() {
    let manager = manager(serde_json::json!({"concurrency":{"maxInFlight":1}}), 2);
    let key = [1; 32];
    manager.report_session_success(Some(key), 2);
    let active = manager.acquire_context(None).await.unwrap();
    let pending = manager.clone();
    let older = tokio::spawn(async move { pending.acquire_context(None).await.unwrap() });
    wait_for_waiters(&manager, 1).await;
    let pending = manager.clone();
    let sticky = tokio::spawn(async move {
        pending
            .acquire_context_with_affinity(None, Some(&key))
            .await
            .unwrap()
    });
    wait_for_waiters(&manager, 2).await;
    drop(active);
    let older = timeout(StdDuration::from_secs(1), older)
        .await
        .unwrap()
        .unwrap();
    assert!(!sticky.is_finished());
    drop(older);
    assert_eq!(sticky.await.unwrap().id, 2);

    let disabled = self::manager(serde_json::json!({"sessionAffinity":{"enabled":false}}), 2);
    disabled.report_session_success(Some(key), 2);
    assert_eq!(
        disabled
            .acquire_context_with_affinity(None, Some(&key))
            .await
            .unwrap()
            .id,
        1
    );
    assert_eq!(disabled.snapshot().concurrency.affinity_miss_total, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_load_obeys_both_limits_in_both_modes() {
    for mode in ["balanced", "priority"] {
        let manager = manager(
            serde_json::json!({"loadBalancingMode":mode,
                "concurrency":{"maxInFlight":3,"maxInFlightPerCredential":2}}),
            2,
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(32));
        let tasks = (0..32)
            .map(|_| {
                let manager = manager.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    let context = manager.acquire_context(None).await.unwrap();
                    {
                        let entries = manager.entries.lock();
                        let counts = entries
                            .iter()
                            .map(|e| e.in_flight.load(Ordering::Relaxed))
                            .collect::<Vec<_>>();
                        assert!(counts.iter().all(|count| *count <= 2));
                        assert!(counts.iter().sum::<u64>() <= 3);
                    }
                    sleep(StdDuration::from_millis(5)).await;
                    drop(context);
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(manager.in_flight_slots.available_permits(), 3);
        assert_eq!(manager.waiting_slots.available_permits(), 128);
    }
}

#[tokio::test]
async fn full_queue_cancellation_and_context_clones_do_not_leak_slots() {
    let manager = manager(
        serde_json::json!({"concurrency":{"maxInFlight":1,"maxWaiting":1}}),
        1,
    );
    let active = manager.acquire_context(None).await.unwrap();
    let clone = active.clone();
    let pending = manager.clone();
    let queued = tokio::spawn(async move { pending.acquire_context(None).await });
    wait_for_waiters(&manager, 1).await;
    assert_eq!(
        manager
            .acquire_context(None)
            .await
            .err()
            .unwrap()
            .downcast_ref::<CapacityError>(),
        Some(&CapacityError::QueueFull)
    );
    queued.abort();
    assert!(queued.await.err().unwrap().is_cancelled());
    wait_for_waiters(&manager, 0).await;
    drop(active);
    assert_eq!(manager.in_flight_slots.available_permits(), 0);
    let pending = manager.clone();
    let queued = tokio::spawn(async move { pending.acquire_context(None).await });
    wait_for_waiters(&manager, 1).await;
    drop(clone);
    let context = timeout(StdDuration::from_secs(1), queued)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(context.id, 1);
    drop(context);
    assert_eq!(manager.in_flight_slots.available_permits(), 1);
    assert_eq!(
        manager.entries.lock()[0].in_flight.load(Ordering::Relaxed),
        0
    );
    wait_for_waiters(&manager, 0).await;
}

#[tokio::test]
async fn timeout_releases_waiting_capacity_and_zero_queue_fails_fast() {
    let manager = manager(
        serde_json::json!({"concurrency":{
            "maxInFlight":1,"maxWaiting":1,"waitTimeoutSeconds":1}}),
        1,
    );
    let active = manager.acquire_context(None).await.unwrap();
    assert_eq!(
        timeout(StdDuration::from_secs(2), manager.acquire_context(None))
            .await
            .unwrap()
            .err()
            .unwrap()
            .downcast_ref::<CapacityError>(),
        Some(&CapacityError::WaitTimeout)
    );
    assert_eq!(manager.waiting_slots.available_permits(), 1);
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.wait_timeout_total, 1);
    assert_eq!(metrics.cancelled_wait_total, 0);
    assert_eq!(metrics.finished_wait_total, 1);
    assert!(metrics.total_wait_ms >= 900);
    drop(active);
    drop(manager.acquire_context(None).await.unwrap());

    let no_queue = self::manager(
        serde_json::json!({"concurrency":{"maxInFlight":1,"maxWaiting":0}}),
        1,
    );
    let _active = no_queue.acquire_context(None).await.unwrap();
    assert_eq!(
        no_queue
            .acquire_context(None)
            .await
            .err()
            .unwrap()
            .downcast_ref::<CapacityError>(),
        Some(&CapacityError::QueueFull)
    );
}

#[tokio::test]
async fn dispatchable_waiters_keep_arrival_order_even_when_polled_in_reverse() {
    for mode in ["balanced", "priority"] {
        let manager = manager(
            serde_json::json!({"loadBalancingMode":mode,"concurrency":{"maxInFlight":1}}),
            1,
        );
        let active = manager.acquire_context(None).await.unwrap();
        let mut requests = (0..4)
            .map(|_| Box::pin(manager.acquire_context(None)))
            .collect::<Vec<_>>();
        for request in &mut requests {
            assert!(futures::poll!(request.as_mut()).is_pending());
        }
        drop(active);
        // 名额已经释放，刚到的调用依然不能插队。
        let mut newcomer = Box::pin(manager.acquire_context(None));
        assert!(futures::poll!(newcomer.as_mut()).is_pending());
        requests.push(newcomer);
        while !requests.is_empty() {
            for request in requests.iter_mut().skip(1).rev() {
                assert!(futures::poll!(request.as_mut()).is_pending());
            }
            let mut first = requests.remove(0);
            match futures::poll!(first.as_mut()) {
                std::task::Poll::Ready(Ok(context)) => drop(context),
                _ => panic!("oldest dispatchable request must acquire the free slot"),
            }
        }
        let metrics = manager.snapshot().concurrency;
        assert_eq!(metrics.admitted_total, 6);
        assert_eq!(metrics.queued_total, 5);
        assert_eq!(metrics.finished_wait_total, 5);
        assert_eq!(metrics.cancelled_wait_total, 0);
        assert_eq!(metrics.waiting, 0);
        assert_eq!(metrics.in_flight, 0);
    }
}

#[tokio::test]
async fn blocked_opus_waiter_does_not_waste_free_capacity_but_retains_priority_when_ready() {
    for mode in ["balanced", "priority"] {
        let manager = manager(
            serde_json::json!({"loadBalancingMode":mode,"concurrency":{
                "maxInFlight":2,"maxInFlightPerCredential":1}}),
            2,
        );
        manager.entries.lock()[1].credentials.subscription_title = Some("KIRO FREE".into());
        let pro = manager
            .acquire_context(Some("claude-opus-4.6"))
            .await
            .unwrap();
        let mut opus = Box::pin(manager.acquire_context(Some("claude-opus-4.6")));
        assert!(futures::poll!(opus.as_mut()).is_pending());
        let free = manager
            .acquire_context(Some("claude-sonnet-4.6"))
            .await
            .unwrap();
        assert_eq!(free.id, 2);
        let mut sonnet = Box::pin(manager.acquire_context(Some("claude-sonnet-4.6")));
        assert!(futures::poll!(sonnet.as_mut()).is_pending());
        drop(pro);
        assert!(futures::poll!(sonnet.as_mut()).is_pending());
        let pro = opus.await.unwrap();
        assert_eq!(pro.id, 1);
        drop(free);
        assert_eq!(sonnet.await.unwrap().id, 2);
        drop(pro);
    }
}

#[tokio::test]
async fn cancelling_head_wakes_queued_successor_without_another_capacity_release() {
    let manager = manager(serde_json::json!({"concurrency":{"maxInFlight":1}}), 1);
    let active = manager.acquire_context(None).await.unwrap();
    let mut head = Box::pin(manager.acquire_context(None));
    assert!(futures::poll!(head.as_mut()).is_pending());
    let pending = manager.clone();
    let successor = tokio::spawn(async move { pending.acquire_context(None).await });
    wait_for_waiters(&manager, 2).await;
    drop(active);
    // 后继先醒来也必须等队首，不需要依赖任务调度顺序。
    tokio::task::yield_now().await;
    assert!(!successor.is_finished());
    drop(head);
    let context = timeout(StdDuration::from_secs(1), successor)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.waiting, 0);
    assert_eq!(metrics.in_flight, 1);
    assert_eq!(metrics.cancelled_wait_total, 1);
    assert_eq!(metrics.finished_wait_total, 2);
    drop(context);
}

#[tokio::test]
async fn waiting_opus_snapshot_tracks_live_queue_after_cancellation() {
    let manager = manager(
        serde_json::json!({"concurrency":{"maxInFlight":1,"maxInFlightPerCredential":1}}),
        1,
    );
    let active = manager.acquire_context(None).await.unwrap();
    let mut opus = Box::pin(manager.acquire_context(Some("claude-opus-4.6")));
    let mut general = Box::pin(manager.acquire_context(Some("claude-sonnet-4.6")));
    assert!(futures::poll!(opus.as_mut()).is_pending());
    assert!(futures::poll!(general.as_mut()).is_pending());
    let snapshot = manager.snapshot().concurrency;
    assert_eq!(snapshot.waiting, 2);
    assert_eq!(snapshot.waiting_opus, 1);

    drop(opus);
    let snapshot = manager.snapshot().concurrency;
    assert_eq!(snapshot.waiting, 1);
    assert_eq!(snapshot.waiting_opus, 0);
    assert_eq!(snapshot.cancelled_wait_total, 1);

    drop(active);
    drop(general.await.unwrap());
    let snapshot = manager.snapshot().concurrency;
    assert_eq!(snapshot.waiting, 0);
    assert_eq!(snapshot.waiting_opus, 0);
}

#[tokio::test]
async fn snapshot_distinguishes_enabled_cooling_saturated_and_deleted_active_accounts() {
    let manager = manager(
        serde_json::json!({"concurrency":{
        "maxInFlight":2,"maxInFlightPerCredential":1,"maxWaiting":0}}),
        2,
    );
    let active = manager.acquire_context(None).await.unwrap();
    manager.report_rate_limited(2, Some(StdDuration::from_secs(u64::MAX)));
    assert!(
        manager
            .acquire_context(None)
            .await
            .err()
            .unwrap()
            .is::<CapacityError>()
    );
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.available, 2);
    assert_eq!(snapshot.entries[0].in_flight, 1);
    assert_eq!(snapshot.entries[1].cooldown_remaining_ms, u64::MAX);
    assert_eq!(snapshot.concurrency.cooling_credentials, 1);
    assert_eq!(snapshot.concurrency.saturated_credentials, 1);
    assert_eq!(snapshot.concurrency.queue_full_total, 1);
    assert_eq!(snapshot.concurrency.rate_limited_total, 1);
    manager.set_disabled(active.id, true).unwrap();
    manager.delete_credential(active.id).unwrap();
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.in_flight, 1);
    assert_eq!(metrics.saturated_credentials, 0);
    manager.report_rate_limited(active.id, None);
    assert_eq!(manager.snapshot().concurrency.rate_limited_total, 2);
    drop(active);
    assert_eq!(manager.snapshot().concurrency.in_flight, 0);
}

#[tokio::test]
async fn available_slots_respect_model_eligibility_cooldown_and_disabled_accounts() {
    let manager = manager(
        serde_json::json!({"concurrency":{
            "maxInFlight":3,"maxInFlightPerCredential":2,"adaptiveConcurrency":false
        }}),
        3,
    );
    manager.entries.lock()[1].credentials.subscription_title = Some("KIRO FREE".into());
    manager.set_disabled(3, true).unwrap();
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.global_available_slots, 3);
    assert_eq!(metrics.available_slots, 3);
    assert_eq!(metrics.available_opus_slots, 2);

    let active = manager
        .acquire_context(Some("claude-opus-4.6"))
        .await
        .unwrap();
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.global_available_slots, 2);
    assert_eq!(metrics.available_slots, 2);
    assert_eq!(metrics.available_opus_slots, 1);

    manager.report_rate_limited_for_context(&active, None);
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.available_slots, 2);
    assert_eq!(metrics.available_opus_slots, 0);
    manager.set_disabled(2, true).unwrap();
    assert_eq!(manager.snapshot().concurrency.available_slots, 0);

    drop(active);
    expire_cooldown(&manager, 1);
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.available_slots, 2);
    assert_eq!(metrics.available_opus_slots, 2);
}

#[tokio::test]
async fn deleted_calls_still_limit_available_slots_and_large_account_limits_do_not_overflow() {
    let manager = manager(
        serde_json::json!({"concurrency":{"maxInFlight":7,"maxInFlightPerCredential":u64::MAX}}),
        2,
    );
    assert_eq!(manager.snapshot().concurrency.available_slots, 7);
    let active = manager.acquire_context(None).await.unwrap();
    manager.set_disabled(active.id, true).unwrap();
    manager.delete_credential(active.id).unwrap();
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.in_flight, 1);
    assert_eq!(metrics.global_available_slots, 6);
    assert_eq!(metrics.available_slots, 6);
    assert_eq!(metrics.available_opus_slots, 6);

    drop(active);
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.available_slots, 7);
    assert_eq!(metrics.available_opus_slots, 7);
    manager.set_disabled(2, true).unwrap();
    manager.delete_credential(2).unwrap();
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.global_available_slots, 7);
    assert_eq!(metrics.available_slots, 0);
    assert_eq!(metrics.available_opus_slots, 0);
}

#[tokio::test]
async fn available_slots_follow_adaptive_draining_probe_and_recovery() {
    let manager = manager(
        serde_json::json!({"concurrency": {
            "adaptiveConcurrency": true, "maxInFlight":8, "maxInFlightPerCredential":8
        }}),
        1,
    );
    let mut active = Vec::new();
    for _ in 0..4 {
        active.push(manager.acquire_context(None).await.unwrap());
    }
    manager.report_rate_limited_for_context(&active[0], None);
    expire_cooldown(&manager, 1);
    assert_eq!(manager.snapshot().concurrency.available_slots, 0);
    drop(active);
    assert_eq!(manager.snapshot().concurrency.available_slots, 1);

    let probe = manager.acquire_context(None).await.unwrap();
    assert_eq!(manager.snapshot().concurrency.available_slots, 0);
    manager.report_transport_success(&probe);
    let metrics = manager.snapshot().concurrency;
    assert_eq!(metrics.available_slots, 3);
    assert_eq!(metrics.available_opus_slots, 3);
    drop(probe);
    assert_eq!(manager.snapshot().concurrency.available_slots, 4);
}

#[tokio::test]
async fn cooling_accounts_are_skipped_and_expiry_wakes_waiters_without_a_release() {
    for mode in ["balanced", "priority"] {
        let manager = manager(
            serde_json::json!({"loadBalancingMode":mode,"concurrency":{
                "maxInFlightPerCredential":1,"rateLimitCooldownSeconds":1}}),
            2,
        );
        manager.report_rate_limited(1, None);
        assert_eq!(manager.acquire_context(None).await.unwrap().id, 2);
        manager.set_disabled(2, true).unwrap();
        let started = Instant::now();
        let context = timeout(StdDuration::from_secs(2), manager.acquire_context(None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(context.id, 1);
        assert!(started.elapsed() >= StdDuration::from_millis(900));
        assert_eq!(manager.snapshot().entries[0].failure_count, 0);
    }
}

#[tokio::test]
async fn cooldown_never_shortens_retry_after_and_accepts_unrepresentable_deadlines() {
    let manager = manager(serde_json::json!({}), 2);
    manager.report_rate_limited(1, Some(StdDuration::from_secs(120)));
    manager.report_rate_limited(1, Some(StdDuration::from_secs(1)));
    let (started, duration) = manager.entries.lock()[0].cooldown.unwrap();
    assert!(duration.saturating_sub(started.elapsed()) > StdDuration::from_secs(119));
    manager.report_rate_limited(1, Some(StdDuration::from_secs(u64::MAX)));
    assert_eq!(manager.acquire_context(None).await.unwrap().id, 2);
    assert_eq!(manager.snapshot().available, 2);
}

#[tokio::test]
async fn full_eligible_accounts_do_not_route_opus_to_free_accounts() {
    let manager = manager(
        serde_json::json!({"concurrency":{"maxInFlightPerCredential":1,"maxWaiting":0}}),
        2,
    );
    manager.entries.lock()[1].credentials.subscription_title = Some("KIRO FREE".into());
    let _active = manager
        .acquire_context(Some("claude-opus-4.6"))
        .await
        .unwrap();
    assert!(
        manager
            .acquire_context(Some("claude-opus-4.6"))
            .await
            .err()
            .unwrap()
            .is::<CapacityError>()
    );
    assert_eq!(
        manager
            .acquire_context(Some("claude-sonnet-4.6"))
            .await
            .unwrap()
            .id,
        2
    );
}

#[tokio::test]
async fn deleting_an_active_account_keeps_global_capacity_reserved_and_addition_wakes_waiters() {
    let manager = manager(
        serde_json::json!({"concurrency":{"maxInFlight":2,"maxInFlightPerCredential":1}}),
        2,
    );
    let first = manager.acquire_context(None).await.unwrap();
    let second = manager.acquire_context(None).await.unwrap();
    manager.set_disabled(first.id, true).unwrap();
    manager.delete_credential(first.id).unwrap();
    let third_id = manager.add_credential(credential(3)).await.unwrap();
    let pending = manager.clone();
    let queued = tokio::spawn(async move { pending.acquire_context(None).await });
    wait_for_waiters(&manager, 1).await;
    drop(first);
    let third = timeout(StdDuration::from_secs(1), queued)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(third.id, third_id);
    drop(third);

    let pending = manager.clone();
    manager.report_rate_limited(third_id, None);
    let queued = tokio::spawn(async move { pending.acquire_context(None).await });
    wait_for_waiters(&manager, 1).await;
    let fourth_id = manager.add_credential(credential(4)).await.unwrap();
    let fourth = timeout(StdDuration::from_secs(1), queued)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(fourth.id, fourth_id);
    drop((second, fourth));
    assert_eq!(manager.in_flight_slots.available_permits(), 2);
}

#[tokio::test]
async fn refreshes_recheck_latest_tokens_and_other_accounts_progress_independently() {
    let credentials = (1..=2)
        .map(|id| KiroCredentials {
            id: Some(id),
            refresh_token: Some(format!("refresh-{id}")),
            expires_at: Some("2020-01-01T00:00:00Z".into()),
            ..Default::default()
        })
        .collect::<Vec<_>>();
    let manager = Arc::new(
        MultiTokenManager::new(Config::default(), credentials.clone(), None, None, true).unwrap(),
    );
    let lock1 = manager.refresh_lock_for(1).unwrap();
    let lock2 = manager.refresh_lock_for(2).unwrap();
    let guard1 = lock1.lock().await;
    let guard2 = lock2.lock().await;
    let mut tasks = Vec::new();
    // 两个同账号请求都必须读取刷新后的 Token，无需真实网络。
    for index in [0, 0, 1] {
        let pending = manager.clone();
        let old_credentials = credentials[index].clone();
        tasks.push(tokio::spawn(async move {
            pending
                .try_ensure_token(old_credentials.id.unwrap(), &old_credentials)
                .await
                .unwrap()
        }));
    }
    {
        let mut entries = manager.entries.lock();
        for entry in entries.iter_mut() {
            entry.credentials.access_token = Some(format!("new-token-{}", entry.id));
            entry.credentials.expires_at = Some("2080-01-01T00:00:00Z".into());
        }
    }
    drop(guard2);
    let context2 = timeout(StdDuration::from_secs(1), tasks.pop().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context2.token, "new-token-2");
    assert!(tasks.iter().all(|task| !task.is_finished()));
    drop(guard1);
    for task in tasks {
        assert_eq!(task.await.unwrap().token, "new-token-1");
    }
}

fn expire_cooldown(manager: &MultiTokenManager, id: u64) {
    manager
        .entries
        .lock()
        .iter_mut()
        .find(|entry| entry.id == id)
        .unwrap()
        .cooldown = None;
}

#[tokio::test]
async fn repeated_rate_limits_back_off_per_round_and_reset_only_on_current_success() {
    for adaptive in [false, true] {
        let manager = manager(
            serde_json::json!({"loadBalancingMode": "priority", "concurrency": {
                "adaptiveConcurrency": adaptive, "requestsPerMinute": 0
            }}),
            2,
        );
        let first = manager.acquire_context(None).await.unwrap();
        let late = manager.acquire_context(None).await.unwrap();
        assert_eq!(first.id, late.id);
        manager.report_rate_limited_for_context(&first, None);
        manager.report_rate_limited_for_context(&late, None);
        assert_eq!(manager.entries.lock()[0].cooldown.unwrap().1.as_secs(), 30);
        // A successful response already in flight before the 429 cannot reset backoff.
        manager.report_transport_success(&late);
        assert_eq!(
            manager.entries.lock()[0]
                .adaptive_limit
                .cooldown_seconds(30),
            30
        );
        assert_eq!(
            manager.entries.lock()[1]
                .adaptive_limit
                .cooldown_seconds(30),
            30
        );
        assert!(manager.entries.lock()[1].cooldown.is_none());
        drop((first, late));

        for seconds in [60, 120, 240, 480, 900, 900] {
            expire_cooldown(&manager, 1);
            let probe = manager.acquire_context(None).await.unwrap();
            assert_eq!(probe.id, 1);
            manager.report_rate_limited_for_context(&probe, None);
            assert_eq!(
                manager.entries.lock()[0].cooldown.unwrap().1.as_secs(),
                seconds
            );
            drop(probe);
        }

        expire_cooldown(&manager, 1);
        let probe = manager.acquire_context(None).await.unwrap();
        manager.report_transport_success(&probe);
        assert_eq!(
            manager.entries.lock()[0]
                .adaptive_limit
                .cooldown_seconds(30),
            30
        );
        drop(probe);
        let next = manager.acquire_context(None).await.unwrap();
        manager.report_rate_limited_for_context(&next, None);
        assert_eq!(manager.entries.lock()[0].cooldown.unwrap().1.as_secs(), 30);
    }
}

#[test]
fn rate_limit_backoff_preserves_longer_configured_cooldowns_and_retry_after() {
    let manager = manager(
        serde_json::json!({"concurrency": {"rateLimitCooldownSeconds": 1800}}),
        1,
    );
    for _ in 0..3 {
        manager.report_rate_limited(1, None);
        assert_eq!(
            manager.entries.lock()[0].cooldown.unwrap().1.as_secs(),
            1800
        );
    }
    manager.report_rate_limited(1, Some(StdDuration::from_secs(3600)));
    assert_eq!(
        manager.entries.lock()[0].cooldown.unwrap().1.as_secs(),
        3600
    );
}

#[tokio::test]
async fn adaptive_monitoring_counts_only_real_reductions_and_exposes_probe_target() {
    let manager = manager(
        serde_json::json!({"concurrency": {
            "adaptiveConcurrency": true, "maxInFlightPerCredential": 4
        }}),
        1,
    );
    let first = manager.acquire_context(None).await.unwrap();
    let second = manager.acquire_context(None).await.unwrap();
    manager.report_rate_limited_for_context(&first, None);
    manager.report_rate_limited_for_context(&second, Some(StdDuration::from_secs(120)));
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.concurrency.rate_limited_total, 2);
    assert_eq!(snapshot.concurrency.adaptive_reduction_total, 1);
    assert_eq!(snapshot.concurrency.probing_credentials, 1);
    assert_eq!(snapshot.concurrency.recovering_credentials, 1);
    assert_eq!(snapshot.entries[0].effective_concurrency_limit, 1);
    assert_eq!(snapshot.entries[0].adaptive_probe_target, Some(2));
    assert!(snapshot.entries[0].cooldown_remaining_ms > 119_000);

    drop((first, second));
    expire_cooldown(&manager, 1);
    let probe = manager.acquire_context(None).await.unwrap();
    manager.report_transport_success(&probe);
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.concurrency.probing_credentials, 0);
    assert_eq!(snapshot.concurrency.recovering_credentials, 1);
    assert_eq!(snapshot.entries[0].adaptive_probe_target, None);
    assert_eq!(snapshot.entries[0].effective_concurrency_limit, 2);
    drop(probe);

    let next = manager.acquire_context(None).await.unwrap();
    manager.report_rate_limited_for_context(&next, None);
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.concurrency.adaptive_reduction_total, 2);
    assert_eq!(snapshot.entries[0].adaptive_probe_target, Some(1));
}

#[tokio::test]
async fn adaptive_limits_probe_then_increase_by_success_windows_in_both_modes() {
    for mode in ["balanced", "priority"] {
        let manager = manager(
            serde_json::json!({"loadBalancingMode": mode, "concurrency": {
                "adaptiveConcurrency": true, "maxInFlightPerCredential": 4, "maxWaiting": 0
            }}),
            1,
        );
        manager.report_rate_limited(1, None);
        assert_eq!(manager.snapshot().concurrency.recovering_credentials, 1);
        expire_cooldown(&manager, 1);
        let probe = manager.acquire_context(None).await.unwrap();
        assert!(
            manager
                .acquire_context(None)
                .await
                .err()
                .unwrap()
                .is::<CapacityError>()
        );
        assert_eq!(manager.snapshot().concurrency.saturated_credentials, 1);
        manager.report_transport_success(&probe);
        // 4 -> 减半至 2；探测成功后恢复至 2，而非直接恢复至 4。
        assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 2);
        let second = manager.acquire_context(None).await.unwrap();
        assert!(
            manager
                .acquire_context(None)
                .await
                .err()
                .unwrap()
                .is::<CapacityError>()
        );
        manager.report_transport_success(&second);
        assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 2);
        // 克隆上下文也只能上报一次，不能将一次响应算成多次成功。
        manager.report_transport_success(&second.clone());
        drop((probe, second));
        for expected in [3, 3, 3, 4, 4] {
            let context = manager.acquire_context(None).await.unwrap();
            manager.report_transport_success(&context);
            assert_eq!(
                manager.snapshot().entries[0].effective_concurrency_limit,
                expected
            );
        }
        assert_eq!(manager.snapshot().concurrency.recovering_credentials, 0);
    }
}

#[tokio::test]
async fn adaptive_recovery_ignores_old_successes_and_abandoned_probes() {
    let manager = manager(
        serde_json::json!({"concurrency": {
            "adaptiveConcurrency": true, "maxInFlightPerCredential": 8
        }}),
        2,
    );
    let old = manager.acquire_context(None).await.unwrap();
    manager.report_rate_limited(old.id, None);
    manager.report_transport_success(&old);
    assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 1);
    drop(old);
    expire_cooldown(&manager, 1);
    // 其他账号不受影响；取消探测会释放名额，但不会提高上限。
    assert_eq!(manager.snapshot().entries[1].effective_concurrency_limit, 8);
    let probe = manager.acquire_context(None).await.unwrap();
    assert_eq!(probe.id, 1);
    drop(probe);
    assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 1);
    let probe = manager.acquire_context(None).await.unwrap();
    manager.report_rate_limited(1, None);
    manager.report_transport_success(&probe);
    assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 1);
    drop(probe);
    expire_cooldown(&manager, 1);
    let probe = manager.acquire_context(None).await.unwrap();
    manager.report_transport_success(&probe);
    assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 2);
}

#[tokio::test]
async fn adaptive_recovery_wakes_waiters_before_the_successful_response_is_dropped() {
    let manager = manager(
        serde_json::json!({"concurrency": {
            "adaptiveConcurrency": true, "maxInFlightPerCredential": 4
        }}),
        1,
    );
    manager.report_rate_limited(1, None);
    expire_cooldown(&manager, 1);
    let probe = manager.acquire_context(None).await.unwrap();
    let pending = manager.clone();
    let queued = tokio::spawn(async move { pending.acquire_context(None).await });
    wait_for_waiters(&manager, 1).await;
    manager.report_transport_success(&probe);
    let next = timeout(StdDuration::from_secs(1), queued)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(manager.snapshot().concurrency.in_flight, 2);
    drop((probe, next));
}

#[tokio::test]
async fn adaptive_control_can_be_disabled_and_handles_a_single_slot() {
    for enabled in [false, true] {
        let manager = manager(
            serde_json::json!({"concurrency": {
                "adaptiveConcurrency": enabled, "maxInFlightPerCredential": 1
            }}),
            1,
        );
        manager.report_rate_limited(1, None);
        expire_cooldown(&manager, 1);
        let context = manager.acquire_context(None).await.unwrap();
        manager.report_transport_success(&context);
        assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 1);
        assert_eq!(manager.snapshot().concurrency.recovering_credentials, 0);
    }
    let manager = manager(
        serde_json::json!({"concurrency":{"adaptiveConcurrency":false}}),
        1,
    );
    manager.report_rate_limited(1, None);
    assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 8);
    assert!(!manager.snapshot().concurrency.adaptive_concurrency);
}

#[tokio::test]
async fn adaptive_reduction_drains_existing_calls_before_admitting_a_probe() {
    let manager = manager(
        serde_json::json!({"concurrency": {
            "adaptiveConcurrency": true, "maxInFlightPerCredential": 4, "maxWaiting": 0
        }}),
        1,
    );
    let mut active = Vec::new();
    for _ in 0..4 {
        active.push(manager.acquire_context(None).await.unwrap());
    }
    manager.report_rate_limited(1, None);
    expire_cooldown(&manager, 1);
    assert_eq!(manager.snapshot().concurrency.in_flight, 4);
    while let Some(context) = active.pop() {
        // 旧请求的成功不会结束本轮探测，也不取消现有调用。
        manager.report_transport_success(&context);
        assert!(
            manager
                .acquire_context(None)
                .await
                .err()
                .unwrap()
                .is::<CapacityError>()
        );
        drop(context);
    }
    let probe = manager.acquire_context(None).await.unwrap();
    assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 1);
    manager.report_transport_success(&probe);
    assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 2);
}
