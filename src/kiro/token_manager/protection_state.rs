//! Durable per-account admission state. SQLite transactions are committed before
//! returning a reservation; snapshots are taken under the store lock so an older
//! caller cannot overwrite a newer snapshot. This is not a distributed limiter.

use super::*;
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub(super) struct ProtectionStore {
    connection: Option<Mutex<Connection>>,
}

impl ProtectionStore {
    pub(super) fn open(credentials_path: Option<&Path>) -> anyhow::Result<Self> {
        let Some(credentials_path) = credentials_path else {
            return Ok(Self { connection: None });
        };
        let mut filename = credentials_path.as_os_str().to_os_string();
        filename.push(".protection.sqlite3");
        let path = PathBuf::from(filename);
        // Restrict newly created files, including on systems with a loose umask.
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)
            .with_context(|| format!("无法创建账号保护状态文件: {}", path.display()))?;
        let connection = Connection::open(&path)?;
        connection.busy_timeout(StdDuration::from_secs(1))?;
        connection.execute_batch(
            "PRAGMA journal_mode=DELETE;
             PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS protection (
                 credential_id TEXT PRIMARY KEY,
                 state TEXT NOT NULL
             );",
        )?;
        Ok(Self {
            connection: Some(Mutex::new(connection)),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct ProtectionState {
    version: u32,
    // Bind state to the stable device identity, not rotating OAuth tokens.
    identity: String,
    saved_at_ms: i64,
    cooldown_ms: u64,
    refresh_backoff_ms: u64,
    refresh_backoff_failures: u32,
    request_ages_ms: Vec<u64>,
    adaptive_limit: concurrency::AdaptiveLimit,
}

fn identity(entry: &CredentialEntry) -> String {
    sha256_hex(&format!(
        "{}:{}:{}",
        entry.credentials.is_api_key_credential(),
        entry.credentials.machine_id.as_deref().unwrap_or_default(),
        // API keys do not rotate through OAuth; distinguish replaced keys even
        // when a manually configured global machineId is reused.
        if entry.credentials.is_api_key_credential() {
            entry
                .credentials
                .kiro_api_key
                .as_deref()
                .unwrap_or_default()
        } else {
            ""
        },
    ))
}

fn remaining_ms(state: Option<(Instant, StdDuration)>, now: Instant) -> u64 {
    let duration = state
        .map(|(start, duration)| duration.saturating_sub(now.saturating_duration_since(start)))
        .unwrap_or_default();
    // Round deadlines up so a restart cannot shorten a protection interval.
    (duration.as_millis().min(u64::MAX as u128) as u64)
        .saturating_add(u64::from(duration.subsec_nanos() % 1_000_000 != 0))
}

impl ProtectionState {
    fn capture(entry: &CredentialEntry) -> Self {
        let now = Instant::now();
        Self {
            version: 1,
            identity: identity(entry),
            saved_at_ms: Utc::now().timestamp_millis(),
            cooldown_ms: remaining_ms(entry.cooldown, now),
            refresh_backoff_ms: remaining_ms(entry.refresh_backoff, now),
            refresh_backoff_failures: entry.refresh_backoff_failures,
            request_ages_ms: entry
                .request_times
                .iter()
                .map(|time| now.saturating_duration_since(*time).as_millis() as u64)
                .filter(|age| *age < 60_000)
                .collect(),
            adaptive_limit: entry.adaptive_limit.clone(),
        }
    }

    fn apply(self, entry: &mut CredentialEntry, config: &Config) -> anyhow::Result<()> {
        anyhow::ensure!(self.version == 1, "不支持的账号保护状态版本");
        if self.identity != identity(entry) {
            return Ok(());
        }
        // Backwards clock movement keeps the saved wait rather than clearing it
        // or extending it by the magnitude of the clock jump.
        let elapsed_ms = Utc::now()
            .timestamp_millis()
            .saturating_sub(self.saved_at_ms)
            .max(0) as u64;
        let now = Instant::now();
        let restore = |remaining: u64| {
            let remaining = remaining.saturating_sub(elapsed_ms);
            (remaining > 0).then(|| (now, StdDuration::from_millis(remaining)))
        };
        entry.cooldown = restore(self.cooldown_ms);
        entry.refresh_backoff = restore(self.refresh_backoff_ms);
        entry.refresh_backoff_failures = self.refresh_backoff_failures;
        entry.request_times = if config.concurrency.requests_per_minute == 0 {
            VecDeque::new()
        } else {
            let mut ages: Vec<_> = self
                .request_ages_ms
                .into_iter()
                .map(|age| age.saturating_add(elapsed_ms))
                .filter(|age| *age < 60_000)
                .collect();
            // Keep oldest -> newest order even if the file was edited.
            ages.sort_unstable_by(|a, b| b.cmp(a));
            ages.into_iter()
                .filter_map(|age| now.checked_sub(StdDuration::from_millis(age)))
                .collect()
        };
        entry.adaptive_limit = self
            .adaptive_limit
            .after_restart(config.concurrency.max_in_flight_per_credential);
        Ok(())
    }
}

impl MultiTokenManager {
    pub(super) fn restore_protection(&self) -> anyhow::Result<()> {
        let Some(connection) = &self.protection_store.connection else {
            return Ok(());
        };
        let mut connection = connection.lock();
        let transaction = connection.transaction()?;
        let mut entries = self.entries.lock();
        for entry in entries.iter_mut() {
            let state: Option<String> = transaction
                .query_row(
                    "SELECT state FROM protection WHERE credential_id = ?1",
                    [entry.id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(state) = state {
                serde_json::from_str::<ProtectionState>(&state)
                    .with_context(|| format!("账号 #{} 保护状态损坏", entry.id))?
                    .apply(entry, &self.config)?;
            }
        }
        // Remove deleted accounts on startup; state never contains credentials.
        let ids: Vec<String> = {
            let mut statement = transaction.prepare("SELECT credential_id FROM protection")?;
            statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?
        };
        for id in ids {
            if !entries.iter().any(|entry| entry.id.to_string() == id) {
                transaction.execute("DELETE FROM protection WHERE credential_id = ?1", [id])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub(super) fn persist_protection(&self, id: u64) -> anyhow::Result<()> {
        let Some(connection) = &self.protection_store.connection else {
            return Ok(());
        };
        let persist = || -> anyhow::Result<()> {
            let connection = connection.lock();
            let state = self
                .entries
                .lock()
                .iter()
                .find(|entry| entry.id == id)
                .map(ProtectionState::capture);
            if let Some(state) = state {
                connection.execute(
                    "INSERT INTO protection (credential_id, state) VALUES (?1, ?2)
                     ON CONFLICT(credential_id) DO UPDATE SET state = excluded.state",
                    params![id.to_string(), serde_json::to_string(&state)?],
                )?;
            } else {
                connection.execute(
                    "DELETE FROM protection WHERE credential_id = ?1",
                    [id.to_string()],
                )?;
            }
            Ok(())
        };
        let result = if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        }) {
            tokio::task::block_in_place(persist)
        } else {
            persist()
        };
        result.map_err(|error| error.context(CapacityError::ProtectionUnavailable))
    }

    pub(super) fn save_protection_or_log(&self, id: u64) {
        if let Err(error) = self.persist_protection(id) {
            tracing::error!(credential_id = id, error = %error, "账号保护状态持久化失败");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("kiro-state-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn credentials(&self) -> PathBuf {
            self.0.join("credentials.json")
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn credential() -> KiroCredentials {
        KiroCredentials {
            id: Some(1),
            kiro_api_key: Some("state-test-secret".into()),
            ..Default::default()
        }
    }

    fn manager(directory: &Directory, credentials: Vec<KiroCredentials>) -> MultiTokenManager {
        MultiTokenManager::new(
            serde_json::from_value(serde_json::json!({"concurrency": {
                "requestsPerMinute": 2, "maxWaiting": 0
            }}))
            .unwrap(),
            credentials,
            None,
            Some(directory.credentials()),
            true,
        )
        .unwrap()
    }

    fn saved(manager: &MultiTokenManager) -> ProtectionState {
        let connection = manager.protection_store.connection.as_ref().unwrap().lock();
        let state: String = connection
            .query_row(
                "SELECT state FROM protection WHERE credential_id = '1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&state).unwrap()
    }

    fn write_saved(manager: &MultiTokenManager, state: &ProtectionState) {
        manager
            .protection_store
            .connection
            .as_ref()
            .unwrap()
            .lock()
            .execute(
                "UPDATE protection SET state = ?1 WHERE credential_id = '1'",
                [serde_json::to_string(state).unwrap()],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn rpm_is_durable_before_returning_and_survives_restart_without_drop_flush() {
        let directory = Directory::new();
        let first = manager(&directory, vec![credential()]);
        drop(first.acquire_context(None).await.unwrap());
        drop(first.acquire_context(None).await.unwrap());
        assert_eq!(saved(&first).request_ages_ms.len(), 2);
        // Open while the old manager is still alive: persistence cannot rely on Drop.
        let reloaded = manager(&directory, vec![credential()]);
        assert_eq!(reloaded.snapshot().concurrency.available_slots, 0);
        assert_eq!(reloaded.snapshot().concurrency.in_flight, 0);
        let error = reloaded.acquire_context(None).await.err().unwrap();
        assert_eq!(
            error.downcast_ref::<CapacityError>(),
            Some(&CapacityError::QueueFull)
        );
        let state = serde_json::to_string(&saved(&first)).unwrap();
        assert!(!state.contains("state-test-secret"));
    }

    #[test]
    fn cooldown_and_adaptive_reduction_survive_restart_with_downtime_deducted() {
        let directory = Directory::new();
        let first = manager(&directory, vec![credential()]);
        first.report_rate_limited(1, Some(StdDuration::from_secs(120)));
        let mut state = saved(&first);
        state.saved_at_ms -= 30_000;
        write_saved(&first, &state);
        let reloaded = manager(&directory, vec![credential()]);
        let snapshot = reloaded.snapshot();
        assert!((85_000..=90_000).contains(&snapshot.entries[0].cooldown_remaining_ms));
        assert_eq!(snapshot.entries[0].effective_concurrency_limit, 1);
        assert_eq!(snapshot.entries[0].adaptive_probe_target, Some(4));
        assert_eq!(snapshot.concurrency.cooling_credentials, 1);
    }

    #[tokio::test]
    async fn expired_windows_clear_but_adaptive_recovery_remains() {
        let directory = Directory::new();
        let first = manager(&directory, vec![credential()]);
        drop(first.acquire_context(None).await.unwrap());
        drop(first.acquire_context(None).await.unwrap());
        first.report_rate_limited(1, Some(StdDuration::from_secs(120)));
        let mut state = saved(&first);
        state.saved_at_ms -= 180_000;
        write_saved(&first, &state);
        let reloaded = manager(&directory, vec![credential()]);
        assert_eq!(reloaded.snapshot().entries[0].cooldown_remaining_ms, 0);
        let context = reloaded.acquire_context(None).await.unwrap();
        reloaded.report_transport_success(&context);
        drop(context);
        let again = manager(&directory, vec![credential()]);
        assert_eq!(again.snapshot().entries[0].effective_concurrency_limit, 4);
        assert_eq!(again.snapshot().entries[0].adaptive_probe_target, None);
        assert_eq!(again.entries.lock()[0].request_times.len(), 1);
    }

    #[test]
    fn backwards_clock_retains_saved_wait_without_extending_it() {
        let directory = Directory::new();
        let first = manager(&directory, vec![credential()]);
        first.report_rate_limited(1, Some(StdDuration::from_secs(120)));
        let mut state = saved(&first);
        state.saved_at_ms += 3_600_000;
        write_saved(&first, &state);
        let reloaded = manager(&directory, vec![credential()]);
        assert!(
            (115_000..=120_000).contains(&reloaded.snapshot().entries[0].cooldown_remaining_ms)
        );
    }

    #[test]
    fn changed_account_identity_does_not_inherit_old_state_or_ignore_other_accounts() {
        let directory = Directory::new();
        let first = manager(&directory, vec![credential()]);
        first.report_rate_limited(1, Some(StdDuration::from_secs(120)));
        let mut replacement = credential();
        replacement.kiro_api_key = Some("replacement-secret".into());
        replacement.machine_id = first.entries.lock()[0].credentials.machine_id.clone();
        let reloaded = manager(&directory, vec![replacement]);
        assert_eq!(reloaded.snapshot().entries[0].cooldown_remaining_ms, 0);
        assert_eq!(
            reloaded.snapshot().entries[0].effective_concurrency_limit,
            8
        );
    }

    #[tokio::test]
    async fn write_failure_stops_admission_and_releases_capacity() {
        let directory = Directory::new();
        let manager = manager(&directory, vec![credential()]);
        manager
            .protection_store
            .connection
            .as_ref()
            .unwrap()
            .lock()
            .execute_batch("PRAGMA query_only = ON")
            .unwrap();
        let error = manager.acquire_context(None).await.err().unwrap();
        assert_eq!(
            error.downcast_ref::<CapacityError>(),
            Some(&CapacityError::ProtectionUnavailable)
        );
        assert_eq!(manager.snapshot().concurrency.in_flight, 0);
        assert_eq!(manager.entries.lock()[0].request_times.len(), 1);
    }

    #[test]
    fn corrupt_state_refuses_startup_instead_of_silently_clearing_protection() {
        let directory = Directory::new();
        let first = manager(&directory, vec![credential()]);
        first.report_rate_limited(1, None);
        first
            .protection_store
            .connection
            .as_ref()
            .unwrap()
            .lock()
            .execute("UPDATE protection SET state = 'broken'", [])
            .unwrap();
        let error = MultiTokenManager::new(
            Config::default(),
            vec![credential()],
            None,
            Some(directory.credentials()),
            true,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("保护状态损坏"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reservations_cannot_overwrite_newer_rpm_history() {
        let directory = Directory::new();
        let mut config = Config::default();
        config.concurrency.requests_per_minute = 60;
        let manager = Arc::new(
            MultiTokenManager::new(
                config,
                vec![credential()],
                None,
                Some(directory.credentials()),
                true,
            )
            .unwrap(),
        );
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let manager = manager.clone();
            tasks.push(tokio::spawn(async move {
                drop(manager.acquire_context(None).await.unwrap());
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(saved(&manager).request_ages_ms.len(), 32);
    }
}
