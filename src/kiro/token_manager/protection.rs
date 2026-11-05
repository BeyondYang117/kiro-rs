//! Account admission pacing and serialized authentication recovery.

use super::*;

#[derive(Debug)]
pub(super) struct RefreshServiceError {
    pub status: http::StatusCode,
    pub retry_after: Option<StdDuration>,
    pub message: String,
}
impl fmt::Display for RefreshServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RefreshServiceError {}

#[derive(Debug)]
struct RefreshDeferred;
impl fmt::Display for RefreshDeferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("账号认证服务正在退避，请稍后重试")
    }
}
impl std::error::Error for RefreshDeferred {}

impl MultiTokenManager {
    pub(super) fn protection_remaining_ms(&self, entry: &CredentialEntry) -> u64 {
        let remaining = self.protection_remaining(entry);
        (remaining.as_millis().min(u64::MAX as u128) as u64)
            .saturating_add(u64::from(remaining.subsec_nanos() % 1_000_000 != 0))
    }

    pub(super) fn protection_remaining(&self, entry: &CredentialEntry) -> StdDuration {
        let remaining = |state: Option<(Instant, StdDuration)>| {
            state
                .map(|(started, duration)| duration.saturating_sub(started.elapsed()))
                .unwrap_or_default()
        };
        let limit = self.config.concurrency.requests_per_minute;
        let rpm = if limit > 0 && entry.request_times.len() >= limit {
            StdDuration::from_secs(60)
                .saturating_sub(entry.request_times[entry.request_times.len() - limit].elapsed())
        } else {
            StdDuration::ZERO
        };
        remaining(entry.cooldown)
            .max(remaining(entry.refresh_backoff))
            .max(rpm)
    }

    pub(super) fn protection_ready_at(&self, entry: &CredentialEntry) -> Option<Instant> {
        let remaining = self.protection_remaining(entry);
        (!remaining.is_zero()).then(|| {
            let now = Instant::now();
            // Unrepresentably long Retry-After values must still block admission.
            now.checked_add(remaining)
                .unwrap_or(now + StdDuration::from_secs(86_400))
        })
    }

    pub(super) fn persist_disabled_state(&self, id: u64) {
        let disabled = self
            .entries
            .lock()
            .iter()
            .any(|entry| entry.id == id && entry.disabled);
        if disabled && let Err(error) = self.persist_credentials() {
            tracing::error!(credential_id = id, error = %error,
                "账号已停用，但停用状态持久化失败");
        }
    }

    /// 403 is a terminal permission failure, not a reason to cycle the account pool.
    pub(crate) fn report_permission_denied(&self, id: u64) -> bool {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id)
                && !entry.disabled
            {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::PermissionDenied);
                entry.last_used_at = Some(Utc::now().to_rfc3339());
            }
        }
        self.select_highest_priority();
        self.persist_disabled_state(id);
        self.capacity_changed.notify_waiters();
        self.available_count() > 0
    }

    fn record_refresh_error(&self, id: u64, error: &anyhow::Error) {
        if error.is::<crate::kiro::retry_budget::RetryBudgetExceeded>() {
            return;
        }
        if error.is::<RefreshTokenInvalidError>() {
            self.report_refresh_token_invalid(id);
            return;
        }
        if let Some(service) = error.downcast_ref::<RefreshServiceError>()
            && service.status.is_client_error()
            && !matches!(service.status.as_u16(), 408 | 429)
        {
            self.report_refresh_failure(id);
            return;
        }
        // Transport errors, malformed successful responses, 408/429 and 5xx are
        // deferred while the refresh mutex is held, so queued callers cannot storm.
        let retry_after = error
            .downcast_ref::<RefreshServiceError>()
            .and_then(|service| service.retry_after)
            .unwrap_or_default();
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
            entry.refresh_backoff_failures = entry.refresh_backoff_failures.saturating_add(1);
            let base = self.config.concurrency.refresh_backoff_seconds;
            let seconds = base
                .saturating_mul(1 << (entry.refresh_backoff_failures - 1).min(6))
                .min(86_400);
            let backoff = StdDuration::from_secs(seconds)
                + StdDuration::from_millis(fastrand::u64(0..=seconds.saturating_mul(250)));
            let remaining = entry
                .refresh_backoff
                .map(|(start, duration)| duration.saturating_sub(start.elapsed()))
                .unwrap_or_default();
            entry.refresh_backoff = Some((Instant::now(), remaining.max(backoff).max(retry_after)));
        }
        drop(entries);
        self.save_protection_or_log(id);
        self.capacity_changed.notify_waiters();
    }

    pub(crate) async fn refresh_invalid_token(&self, context: &CallContext) -> anyhow::Result<()> {
        self.refresh_managed(context.id, Some(&context.token), true)
            .await
            .map(|_| ())
    }

    pub(super) async fn refresh_managed(
        &self,
        id: u64,
        rejected_token: Option<&str>,
        force: bool,
    ) -> anyhow::Result<KiroCredentials> {
        let lock = self.refresh_lock_for(id)?;
        let _guard = lock.lock().await;
        let credentials = {
            let entries = self.entries.lock();
            let entry = entries
                .iter()
                .find(|entry| entry.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {id}"))?;
            anyhow::ensure!(
                !entry.disabled || (force && rejected_token.is_none()),
                "凭据已禁用: {id}"
            );
            if entry
                .refresh_backoff
                .is_some_and(|(start, duration)| start.elapsed() < duration)
            {
                return Err(RefreshDeferred.into());
            }
            entry.credentials.clone()
        };
        anyhow::ensure!(
            !credentials.is_api_key_credential(),
            "API Key 凭据不支持 Token 刷新"
        );
        // A concurrent request may already have replaced the rejected token.
        if rejected_token.is_some_and(|token| credentials.access_token.as_deref() != Some(token))
            || (!force && !is_token_expired(&credentials) && !is_token_expiring_soon(&credentials))
        {
            return Ok(credentials);
        }
        self.persist_protection(id)?;
        if let Err(error) = validate_refresh_token(&credentials) {
            self.report_refresh_failure(id);
            return Err(error);
        }
        let proxy = credentials.effective_proxy(self.proxy.as_ref());
        let result = async {
            let updated = refresh_token(&credentials, &self.config, proxy.as_ref()).await?;
            anyhow::ensure!(
                !is_token_expired(&updated),
                "刷新后的 Token 仍然无效或已过期"
            );
            Ok::<_, anyhow::Error>(updated)
        }
        .await;
        let updated = match result {
            Ok(updated) => updated,
            Err(error) => {
                self.record_refresh_error(id, &error);
                return Err(error);
            }
        };
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) {
                entry.credentials = updated.clone();
                entry.refresh_failure_count = 0;
                entry.refresh_backoff = None;
                entry.refresh_backoff_failures = 0;
            }
        }
        if let Err(error) = self.persist_credentials() {
            tracing::warn!(credential_id = id, error = %error, "Token 刷新后持久化失败");
        }
        self.persist_protection(id)?;
        self.capacity_changed.notify_waiters();
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> MultiTokenManager {
        MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials {
                id: Some(1),
                refresh_token: Some("r".repeat(150)),
                access_token: Some("current-token".into()),
                expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
                ..Default::default()
            }],
            None,
            None,
            false,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn transient_refresh_failures_back_off_without_disabling_or_refreshing_again() {
        let manager = Arc::new(manager());
        for status in [429, 503, 408, 502] {
            let error = RefreshServiceError {
                status: http::StatusCode::from_u16(status).unwrap(),
                retry_after: Some(StdDuration::from_secs(600)),
                message: "busy".into(),
            }
            .into();
            manager.record_refresh_error(1, &error);
        }
        assert_eq!(manager.available_count(), 1);
        assert!(
            manager.protection_remaining(&manager.entries.lock()[0]) > StdDuration::from_secs(599)
        );
        let mut calls = Vec::new();
        for _ in 0..8 {
            let manager = manager.clone();
            calls.push(tokio::spawn(async move {
                let error = manager.force_refresh_token_for(1).await.unwrap_err();
                assert!(error.is::<RefreshDeferred>());
            }));
        }
        for call in calls {
            call.await.unwrap();
        }
        assert_eq!(manager.entries.lock()[0].refresh_failure_count, 0);
        assert_eq!(manager.entries.lock()[0].refresh_backoff_failures, 4);
        assert_eq!(manager.snapshot().concurrency.in_flight, 0);
    }

    #[tokio::test]
    async fn already_replaced_rejected_token_does_not_force_another_refresh() {
        let manager = manager();
        let mut old = manager.acquire_context(None).await.unwrap();
        old.token = "rejected-old-token".into();
        manager.refresh_invalid_token(&old).await.unwrap();
        assert_eq!(
            manager.entries.lock()[0]
                .credentials
                .access_token
                .as_deref(),
            Some("current-token")
        );
    }

    #[test]
    fn transient_errors_do_not_spend_the_permanent_authentication_failure_threshold() {
        let manager = manager();
        for _ in 0..5 {
            manager.record_refresh_error(
                1,
                &RefreshServiceError {
                    status: http::StatusCode::SERVICE_UNAVAILABLE,
                    retry_after: None,
                    message: "busy".into(),
                }
                .into(),
            );
        }
        let invalid_client = RefreshServiceError {
            status: http::StatusCode::BAD_REQUEST,
            retry_after: None,
            message: "invalid_client".into(),
        }
        .into();
        manager.record_refresh_error(1, &invalid_client);
        assert_eq!(manager.available_count(), 1);
        assert_eq!(manager.entries.lock()[0].refresh_failure_count, 1);
        manager.record_refresh_error(1, &invalid_client);
        manager.record_refresh_error(1, &invalid_client);
        assert_eq!(manager.available_count(), 0);
    }

    #[tokio::test]
    async fn refresh_backoff_and_failure_streak_survive_restart_and_token_rotation() {
        let directory =
            std::env::temp_dir().join(format!("kiro-refresh-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("credentials.json");
        let mut first = manager();
        first.credentials_path = Some(path.clone());
        first.protection_store =
            super::super::protection_state::ProtectionStore::open(Some(&path)).unwrap();
        for _ in 0..3 {
            first.record_refresh_error(
                1,
                &RefreshServiceError {
                    status: http::StatusCode::TOO_MANY_REQUESTS,
                    retry_after: Some(StdDuration::from_secs(600)),
                    message: "busy".into(),
                }
                .into(),
            );
        }
        let mut credentials = first.entries.lock()[0].credentials.clone();
        credentials.refresh_token = Some("rotated-token".repeat(15));
        let reloaded =
            MultiTokenManager::new(Config::default(), vec![credentials], None, Some(path), true)
                .unwrap();
        assert_eq!(reloaded.entries.lock()[0].refresh_backoff_failures, 3);
        assert!(reloaded.snapshot().entries[0].cooldown_remaining_ms > 590_000);
        assert_eq!(reloaded.available_count(), 1);
        assert!(
            reloaded
                .force_refresh_token_for(1)
                .await
                .unwrap_err()
                .is::<RefreshDeferred>()
        );
        drop(reloaded);
        drop(first);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_grant_is_disabled_immediately_and_survives_restart_with_reason() {
        let directory =
            std::env::temp_dir().join(format!("kiro-protection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("credentials.json");
        let mut manager = manager();
        manager.credentials_path = Some(path.clone());
        manager.record_refresh_error(
            1,
            &RefreshTokenInvalidError {
                message: "revoked".into(),
            }
            .into(),
        );
        assert_eq!(manager.available_count(), 0);
        let credentials = crate::kiro::model::credentials::CredentialsConfig::load(&path)
            .unwrap()
            .into_sorted_credentials();
        let reloaded =
            MultiTokenManager::new(Config::default(), credentials, None, None, false).unwrap();
        assert_eq!(
            reloaded.entries.lock()[0].disabled_reason,
            Some(DisabledReason::InvalidRefreshToken)
        );
        reloaded.set_disabled(1, false).unwrap();
        assert_eq!(reloaded.available_count(), 1);
        drop(manager);
        drop(reloaded);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn permission_failure_is_terminal_and_late_success_cannot_reenable_it() {
        let manager = manager();
        assert!(!manager.report_permission_denied(1));
        manager.report_success(1);
        assert_eq!(
            manager.entries.lock()[0].disabled_reason,
            Some(DisabledReason::PermissionDenied)
        );
        assert_eq!(manager.available_count(), 0);
    }
}
