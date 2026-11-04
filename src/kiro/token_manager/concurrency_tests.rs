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
