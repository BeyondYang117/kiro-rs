use super::*;
use crate::kiro::models::AvailableModel;

const SONNET: &str = "claude-sonnet-4.6";
const OPUS: &str = "claude-opus-4.6";

fn manager(mode: &str, tier: &str) -> MultiTokenManager {
    MultiTokenManager::new(
        serde_json::from_value(serde_json::json!({
            "loadBalancingMode": mode,
            "sessionAffinity": {"enabled": true},
            "concurrency": {"maxInFlight": 2, "maxInFlightPerCredential": 1}
        }))
        .unwrap(),
        (1..=2)
            .map(|id| KiroCredentials {
                id: Some(id),
                priority: id as u32,
                kiro_api_key: Some(format!("routing-test-{id}")),
                subscription_title: Some(tier.into()),
                ..Default::default()
            })
            .collect(),
        None,
        None,
        true,
    )
    .unwrap()
}

fn catalog(ids: &[&str]) -> ModelCatalog {
    ModelCatalog {
        models: ids
            .iter()
            .map(|id| AvailableModel {
                model_id: (*id).into(),
                model_name: (*id).into(),
                provider: None,
                rate_multiplier: None,
                rate_unit: None,
                max_input_tokens: None,
                max_output_tokens: None,
                effort_levels: vec![],
                effort_schema_path: None,
                default_effort: None,
                thinking_types: vec![],
                default_thinking: None,
                proxy_supported: crate::anthropic::map_model(id).as_deref() == Some(*id),
            })
            .collect(),
        default_model_id: None,
        region: "us-east-1".into(),
    }
}

fn publish(manager: &MultiTokenManager, id: u64, ids: &[&str]) -> Instant {
    let at = Instant::now();
    manager.update_model_catalog(
        id,
        &manager.model_catalog_key(id).unwrap(),
        &catalog(ids),
        at,
        false,
    );
    at
}

#[tokio::test]
async fn catalog_overrides_tier_and_current_account_for_each_model() {
    for mode in ["priority", "balanced"] {
        let manager = manager(mode, "KIRO FREE");
        publish(&manager, 1, &[SONNET]);
        publish(&manager, 2, &[OPUS]);
        for (model, expected) in [(SONNET, 1), (OPUS, 2), (SONNET, 1), ("CLAUDE-OPUS-4-6", 2)] {
            let ctx = manager.acquire_context(Some(model)).await.unwrap();
            assert_eq!(ctx.id, expected, "{mode}: {model}");
        }
        assert_eq!(manager.snapshot().concurrency.available_opus_slots, 1);
        assert_eq!(manager.available_count(), 2);
    }
}

#[tokio::test]
async fn known_support_wins_over_unknown_affinity_but_busy_accounts_allow_fallback() {
    for mode in ["priority", "balanced"] {
        let manager = manager(mode, "KIRO PRO");
        let affinity = session_key("e326b4d0-e944-41d9-aa08-35f3ace938fa").unwrap();
        manager.report_session_success(Some(affinity), 1);
        publish(&manager, 2, &[SONNET]);
        let known = manager
            .acquire_context_with_affinity(Some(SONNET), Some(&affinity))
            .await
            .unwrap();
        assert_eq!(known.id, 2);
        let fallback = manager.acquire_context(Some(SONNET)).await.unwrap();
        assert_eq!(fallback.id, 1);
        drop((known, fallback));
    }
}

#[tokio::test]
async fn complete_empty_or_missing_model_does_not_penalize_accounts_or_block_tools() {
    let manager = manager("priority", "KIRO PRO");
    publish(&manager, 1, &[]);
    publish(&manager, 2, &[OPUS]);
    assert!(
        manager
            .acquire_context(Some(SONNET))
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("没有支持")
    );
    assert_eq!(manager.available_count(), 2);
    assert!(
        manager
            .snapshot()
            .entries
            .iter()
            .all(|entry| entry.failure_count == 0)
    );
    // MCP calls have no model and retain access to every healthy credential.
    assert!(manager.acquire_context(None).await.is_ok());
    assert_eq!(manager.acquire_context(Some(OPUS)).await.unwrap().id, 2);
}

#[tokio::test]
async fn cached_reads_do_not_extend_expiry_and_stale_cannot_be_reactivated() {
    let manager = manager("priority", "KIRO PRO");
    let key = manager.model_catalog_key(1).unwrap();
    let expired = Instant::now() - CATALOG_TTL;
    manager.update_model_catalog(1, &key, &catalog(&[]), expired, false);
    // An expired deny list must not remove the current account from routing.
    assert_eq!(manager.acquire_context(Some(SONNET)).await.unwrap().id, 1);
    manager.update_model_catalog(1, &key, &catalog(&[]), expired, false);
    assert_eq!(
        manager.entries.lock()[0]
            .model_catalog
            .as_ref()
            .unwrap()
            .fetched_at,
        expired
    );

    let fetched = publish(&manager, 1, &[]);
    assert_eq!(manager.acquire_context(Some(SONNET)).await.unwrap().id, 2);
    manager.update_model_catalog(1, &key, &catalog(&[]), fetched, true);
    manager.update_model_catalog(1, &key, &catalog(&[]), fetched, false);
    assert!(
        manager.entries.lock()[0]
            .model_catalog
            .as_ref()
            .unwrap()
            .stale
    );
    manager.set_priority(1, 0).unwrap();
    assert_eq!(manager.acquire_context(Some(SONNET)).await.unwrap().id, 1);
    publish(&manager, 1, &[]);
    manager.update_model_catalog(1, &key, &catalog(&[SONNET]), fetched, true);
    assert_eq!(manager.acquire_context(Some(SONNET)).await.unwrap().id, 2);
}

#[tokio::test]
async fn identity_region_profile_and_endpoint_changes_discard_authorization() {
    for field in ["key", "region", "profile", "endpoint"] {
        let manager = manager("priority", "KIRO PRO");
        let old_key = manager.model_catalog_key(1).unwrap();
        let fetched = publish(&manager, 1, &[]);
        {
            let mut entries = manager.entries.lock();
            let credentials = &mut entries[0].credentials;
            match field {
                "key" => credentials.kiro_api_key = Some("changed-identity".into()),
                "region" => credentials.api_region = Some("eu-west-1".into()),
                "profile" => credentials.profile_arn = Some("changed-profile".into()),
                "endpoint" => credentials.endpoint = Some("q".into()),
                _ => unreachable!(),
            }
        }
        // Neither stored nor late responses for the previous scope apply.
        manager.update_model_catalog(1, &old_key, &catalog(&[]), fetched, false);
        assert_eq!(
            manager.acquire_context(Some(SONNET)).await.unwrap().id,
            1,
            "{field}"
        );
    }
}

#[tokio::test]
async fn model_specific_queue_bypasses_blocked_waiters_and_preserves_ready_order() {
    for mode in ["priority", "balanced"] {
        let manager = manager(mode, "KIRO PRO");
        publish(&manager, 1, &[SONNET]);
        publish(&manager, 2, &["claude-sonnet-5.5"]);
        let first = manager.acquire_context(Some(SONNET)).await.unwrap();
        let mut blocked = Box::pin(manager.acquire_context(Some(SONNET)));
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        // Same model family, different entitlement: Sonnet 4.6 cannot block 5.5.
        let other = manager
            .acquire_context(Some("claude-sonnet-5.5"))
            .await
            .unwrap();
        assert_eq!(other.id, 2);
        drop(first);
        let mut newcomer = Box::pin(manager.acquire_context(Some(SONNET)));
        assert!(futures::poll!(newcomer.as_mut()).is_pending());
        let oldest = blocked.await.unwrap();
        assert_eq!(oldest.id, 1);
        drop(oldest);
        assert_eq!(newcomer.await.unwrap().id, 1);
        drop(other);
    }
}

#[tokio::test]
async fn publishing_catalog_wakes_waiters_and_rechecks_eligibility() {
    let manager = manager("priority", "KIRO PRO");
    let active = manager.acquire_context(Some(SONNET)).await.unwrap();
    manager.set_disabled(2, true).unwrap();
    let mut waiting = Box::pin(manager.acquire_context(Some(SONNET)));
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    publish(&manager, 1, &[OPUS]);
    assert!(futures::poll!(waiting.as_mut()).is_ready());
    assert_eq!(manager.snapshot().concurrency.waiting, 0);
    drop(active);
}

#[tokio::test]
async fn catalog_expiry_wakes_queued_request_without_other_capacity_changes() {
    let manager = manager("priority", "KIRO PRO");
    publish(&manager, 1, &[SONNET]);
    manager.update_model_catalog(
        2,
        &manager.model_catalog_key(2).unwrap(),
        &catalog(&[]),
        Instant::now() - CATALOG_TTL + StdDuration::from_millis(50),
        false,
    );
    let active = manager.acquire_context(Some(SONNET)).await.unwrap();
    assert_eq!(active.id, 1);
    let mut queued = Box::pin(manager.acquire_context(Some(SONNET)));
    assert!(futures::poll!(queued.as_mut()).is_pending());
    let recovered = tokio::time::timeout(StdDuration::from_secs(1), queued)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.id, 2);
    drop((active, recovered));
}

#[tokio::test]
async fn failed_fetch_invalidates_old_snapshot_but_not_a_more_recent_success() {
    let manager = manager("priority", "KIRO PRO");
    let old = publish(&manager, 1, &[]);
    let key = manager.model_catalog_key(1).unwrap();
    manager.invalidate_model_catalog(1, &key, Instant::now());
    assert_eq!(manager.acquire_context(Some(SONNET)).await.unwrap().id, 1);
    let new = publish(&manager, 1, &[]);
    assert!(new > old);
    manager.invalidate_model_catalog(1, &key, old);
    assert_eq!(manager.acquire_context(Some(SONNET)).await.unwrap().id, 2);
}

fn native_manager(mode: &str) -> MultiTokenManager {
    let mut manager = manager(mode, "KIRO PRO");
    manager
        .config
        .endpoints
        .insert("ide".into(), serde_json::json!({"nativeThinking":true}));
    manager
}

fn controls(fields: serde_json::Value) -> ModelControls {
    ModelControls::from_request(
        &serde_json::json!({"additionalModelRequestFields":fields}).to_string(),
    )
    .unwrap()
}

fn capabilities(manager: &MultiTokenManager, id: u64, limit: u64, effort: &str, thinking: &str) {
    let mut catalog = catalog(&[SONNET]);
    let model = &mut catalog.models[0];
    model.max_output_tokens = Some(limit);
    model.effort_schema_path = Some("output_config.effort".into());
    model.effort_levels = vec![effort.into()];
    model.thinking_types = vec![thinking.into()];
    manager.update_model_catalog(
        id,
        &manager.model_catalog_key(id).unwrap(),
        &catalog,
        Instant::now(),
        false,
    );
}

#[tokio::test]
async fn native_parameters_choose_an_account_with_matching_limits_and_enums() {
    for mode in ["balanced", "priority"] {
        for fields in [
            serde_json::json!({"max_tokens":2000}),
            serde_json::json!({"output_config":{"effort":"high"}}),
            serde_json::json!({"thinking":{"type":"enabled"}}),
        ] {
            let manager = native_manager(mode);
            capabilities(&manager, 1, 1000, "low", "disabled");
            capabilities(&manager, 2, 2000, "high", "adaptive");
            let ctx = manager
                .acquire_context_with_controls(Some(SONNET), None, Some(&controls(fields)))
                .await
                .unwrap();
            assert_eq!(ctx.id, 2);
        }
    }
}

#[tokio::test]
async fn incompatible_parameters_return_typed_error_without_reservations_or_account_failures() {
    for (fields, parameter) in [
        (serde_json::json!({"max_tokens":2001}), "max_tokens"),
        (
            serde_json::json!({"output_config":{"effort":"high"}}),
            "output_config.effort",
        ),
        (
            serde_json::json!({"thinking":{"type":"adaptive"}}),
            "thinking.type",
        ),
    ] {
        let manager = native_manager("priority");
        capabilities(&manager, 1, 1000, "low", "disabled");
        capabilities(&manager, 2, 2000, "low", "disabled");
        let error = manager
            .acquire_context_with_controls(Some(SONNET), None, Some(&controls(fields)))
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.downcast_ref::<ModelParameterError>().unwrap().0,
            parameter
        );
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.concurrency.in_flight, 0);
        assert_eq!(snapshot.concurrency.waiting, 0);
        assert!(snapshot.entries.iter().all(|entry| !entry.disabled
            && entry.failure_count == 0
            && entry.last_used_at.is_none()));
    }
}

#[tokio::test]
async fn missing_stale_expired_and_legacy_capabilities_do_not_reject_parameters() {
    for case in ["missing", "stale", "expired", "legacy", "other-endpoint"] {
        let mut manager = native_manager("priority");
        manager.set_disabled(2, true).unwrap();
        match case {
            "missing" => {
                publish(&manager, 1, &[SONNET]);
            }
            "expired" => {
                let mut data = catalog(&[SONNET]);
                data.models[0].max_output_tokens = Some(1);
                manager.update_model_catalog(
                    1,
                    &manager.model_catalog_key(1).unwrap(),
                    &data,
                    Instant::now() - CATALOG_TTL,
                    false,
                );
            }
            _ => {
                capabilities(&manager, 1, 1, "low", "disabled");
                match case {
                    "stale" => manager.invalidate_model_catalog(
                        1,
                        &manager.model_catalog_key(1).unwrap(),
                        Instant::now(),
                    ),
                    "legacy" => {
                        manager.config.endpoints.clear();
                    }
                    "other-endpoint" => {
                        manager.entries.lock()[0].credentials.endpoint = Some("cli".into());
                    }
                    _ => unreachable!(),
                }
            }
        }
        let fields = controls(
            serde_json::json!({"max_tokens":2000,"output_config":{"effort":"high"},"thinking":{"type":"enabled"}}),
        );
        assert_eq!(
            manager
                .acquire_context_with_controls(Some(SONNET), None, Some(&fields))
                .await
                .unwrap()
                .id,
            1,
            "{case}"
        );
    }
}

#[tokio::test]
async fn unknown_account_can_accept_parameters_that_known_accounts_reject() {
    let manager = native_manager("priority");
    capabilities(&manager, 1, 1000, "low", "disabled");
    let fields = controls(serde_json::json!({"max_tokens":2000}));
    assert_eq!(
        manager
            .acquire_context_with_controls(Some(SONNET), None, Some(&fields))
            .await
            .unwrap()
            .id,
        2
    );
}

#[tokio::test]
async fn parameter_specific_queue_bypasses_blocked_waiters_but_preserves_ready_order() {
    for mode in ["priority", "balanced"] {
        let manager = native_manager(mode);
        capabilities(&manager, 1, 1000, "low", "adaptive");
        capabilities(&manager, 2, 2000, "low", "adaptive");
        let large = controls(serde_json::json!({"max_tokens":2000}));
        let small = controls(serde_json::json!({"max_tokens":1000}));
        let active = manager
            .acquire_context_with_controls(Some(SONNET), None, Some(&large))
            .await
            .unwrap();
        assert_eq!(active.id, 2);
        let mut old =
            Box::pin(manager.acquire_context_with_controls(Some(SONNET), None, Some(&large)));
        assert!(futures::poll!(old.as_mut()).is_pending());
        let small_request = manager
            .acquire_context_with_controls(Some(SONNET), None, Some(&small))
            .await
            .unwrap();
        assert_eq!(small_request.id, 1);
        drop(active);
        let mut newcomer =
            Box::pin(manager.acquire_context_with_controls(Some(SONNET), None, Some(&large)));
        assert!(futures::poll!(newcomer.as_mut()).is_pending());
        drop(old.await.unwrap());
        assert_eq!(newcomer.await.unwrap().id, 2);
        drop(small_request);
    }
}
