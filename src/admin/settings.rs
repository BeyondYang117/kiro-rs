//! 白名单设置视图：密钥不进入响应，非热更新项与运行中配置分别展示。
use std::{collections::HashSet, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::error::AdminServiceError;
use crate::{
    kiro::token_manager::MultiTokenManager,
    model::config::{ConcurrencyConfig, Config, RequestLimits, SessionAffinityConfig, TlsBackend},
};

macro_rules! settings {
    ($($field:ident: $type:ty),+ $(,)?) => {
        #[derive(Clone, Serialize, Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub struct SystemSettings { $(pub $field: $type),+ }
        impl From<&Config> for SystemSettings {
            fn from(config: &Config) -> Self {
                Self { $($field: config.$field.clone()),+ }
            }
        }
    };
}

settings! {
    kiro_version: String,
    kiro_cli_version: String,
    region: String,
    auth_region: Option<String>,
    api_region: Option<String>,
    tls_backend: TlsBackend,
    default_endpoint: String,
    load_balancing_mode: String,
    concurrency: ConcurrencyConfig,
    request_limits: RequestLimits,
    session_affinity: SessionAffinityConfig,
    extract_thinking: bool,
    cc_buffered_stream: bool,
    force_stream: bool,
    tool_output_recovery_attempts: u8,
    count_tokens_timeout_seconds: u64,
    request_log_retention_days: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SaveSettingsRequest {
    revision: String,
    settings: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsResponse {
    settings: SystemSettings,
    running: SystemSettings,
    defaults: SystemSettings,
    revision: String,
    restart_required: Vec<String>,
    can_save: bool,
    endpoints: Vec<String>,
}

fn invalid(message: impl Into<String>) -> AdminServiceError {
    AdminServiceError::InvalidSettings(message.into())
}

// 不把包含原始文件内容的解析错误传给客户端。
pub(super) fn storage_error(error: impl std::fmt::Display) -> AdminServiceError {
    tracing::error!("系统设置文件操作失败: {error}");
    AdminServiceError::InternalError(
        "无法读取或保存配置文件，请检查文件格式、路径及写入权限".into(),
    )
}

pub(super) fn read_document(path: &Path) -> Result<Value, AdminServiceError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let document: Value = serde_json::from_slice(&bytes).map_err(storage_error)?;
            if !document.is_object() {
                return Err(storage_error("配置文件必须是 JSON 对象"));
            }
            Ok(document)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(error) => Err(storage_error(error)),
    }
}

fn settings_from_document(document: &Value) -> Result<SystemSettings, AdminServiceError> {
    let config: Config = serde_json::from_value(document.clone()).map_err(storage_error)?;
    Ok(SystemSettings::from(&config))
}

fn revision(settings: &SystemSettings) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(settings).expect("serializable settings"),
    ))
}

/// 校验完整白名单结构，包括嵌套对象，避免拼错字段被 serde 默认值吞掉。
fn validate_shape(value: &Value, template: &Value, prefix: &str) -> Result<(), AdminServiceError> {
    if let Some(fields) = template.as_object() {
        let actual = value
            .as_object()
            .ok_or_else(|| invalid(format!("{prefix} 必须为对象")))?;
        for key in actual.keys() {
            if !fields.contains_key(key) {
                return Err(invalid("设置包含不支持的参数"));
            }
        }
        for (key, expected) in fields {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            let field = actual
                .get(key)
                .ok_or_else(|| invalid(format!("缺少参数 {path}")))?;
            validate_shape(field, expected, &path)?;
        }
    }
    Ok(())
}

impl SystemSettings {
    fn validate(&self, endpoints: &HashSet<String>) -> Result<(), AdminServiceError> {
        let check = || -> anyhow::Result<()> {
            anyhow::ensure!(
                super::kiro_version::valid_version(&self.kiro_version),
                "kiroVersion 必须是 major.minor.patch 格式的版本号"
            );
            anyhow::ensure!(
                super::kiro_version::valid_version(&self.kiro_cli_version),
                "kiroCliVersion 必须是 major.minor.patch 格式的版本号"
            );
            crate::common::region::validate("region", &self.region)?;
            for (name, region) in [
                ("authRegion", &self.auth_region),
                ("apiRegion", &self.api_region),
            ] {
                if let Some(region) = region {
                    crate::common::region::validate(name, region)?;
                }
            }
            self.concurrency.validate()?;
            self.request_limits.validate()?;
            self.session_affinity.validate()?;
            anyhow::ensure!(
                ["priority", "balanced"].contains(&self.load_balancing_mode.as_str()),
                "loadBalancingMode 必须是 priority 或 balanced"
            );
            anyhow::ensure!(
                endpoints.contains(&self.default_endpoint),
                "defaultEndpoint 必须是已注册的端点"
            );
            anyhow::ensure!(
                self.tool_output_recovery_attempts <= 2,
                "toolOutputRecoveryAttempts 必须在 0..=2 之间"
            );
            anyhow::ensure!(
                (1..=300).contains(&self.count_tokens_timeout_seconds),
                "countTokensTimeoutSeconds 必须在 1..=300 之间"
            );
            anyhow::ensure!(
                (1..=3650).contains(&self.request_log_retention_days),
                "requestLogRetentionDays 必须在 1..=3650 之间"
            );
            for (name, value) in [
                (
                    "concurrency.maxInFlight",
                    self.concurrency.max_in_flight as u64,
                ),
                (
                    "concurrency.maxInFlightPerCredential",
                    self.concurrency.max_in_flight_per_credential,
                ),
                (
                    "requestLimits.maxConcurrentRequests",
                    self.request_limits.max_concurrent_requests as u64,
                ),
            ] {
                anyhow::ensure!(
                    (1..=1_000_000).contains(&value),
                    "{name} 必须在 1..=1000000 之间"
                );
            }
            anyhow::ensure!(
                self.concurrency.max_waiting <= 1_000_000,
                "concurrency.maxWaiting 必须在 0..=1000000 之间"
            );
            #[cfg(not(feature = "native-tls"))]
            anyhow::ensure!(
                self.tls_backend != TlsBackend::NativeTls,
                "当前构建未启用 native-tls"
            );
            Ok(())
        };
        check().map_err(|error| invalid(error.to_string()))
    }
}

fn changed_paths(saved: &Value, running: &Value, prefix: &str, paths: &mut Vec<String>) {
    if let Some(fields) = saved.as_object() {
        for (key, value) in fields {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            changed_paths(value, &running[key], &path, paths);
        }
    } else if saved != running {
        paths.push(prefix.to_owned());
    }
}

// 按叶子合并，保留文件中未纳入管理的参数、嵌套扩展字段和密钥。
fn merge(document: &mut Value, settings: Value) {
    if let Value::Object(fields) = settings {
        if !document.is_object() {
            *document = serde_json::json!({});
        }
        for (key, value) in fields {
            merge(&mut document[key], value);
        }
    } else {
        *document = settings;
    }
}

fn response(
    settings: SystemSettings,
    config: &Config,
    mode: &str,
    endpoints: &HashSet<String>,
) -> SettingsResponse {
    let mut running = SystemSettings::from(config);
    running.load_balancing_mode = mode.to_owned();
    let mut restart_required = Vec::new();
    changed_paths(
        &serde_json::to_value(&settings).unwrap(),
        &serde_json::to_value(&running).unwrap(),
        "",
        &mut restart_required,
    );
    let mut endpoints: Vec<_> = endpoints.iter().cloned().collect();
    endpoints.sort();
    SettingsResponse {
        revision: revision(&settings),
        settings,
        running,
        defaults: SystemSettings::from(&Config::default()),
        restart_required,
        can_save: config.config_path().is_some(),
        endpoints,
    }
}

pub(super) fn read(
    manager: &MultiTokenManager,
    endpoints: &HashSet<String>,
) -> Result<SettingsResponse, AdminServiceError> {
    manager.with_configuration(|config, mode| {
        let settings = if let Some(path) = config.config_path() {
            settings_from_document(&read_document(path)?)?
        } else {
            let mut settings = SystemSettings::from(config);
            settings.load_balancing_mode = mode.clone();
            settings
        };
        Ok(response(settings, config, mode, endpoints))
    })
}

pub(super) fn save(
    manager: &MultiTokenManager,
    endpoints: &HashSet<String>,
    request: SaveSettingsRequest,
) -> Result<SettingsResponse, AdminServiceError> {
    let template = serde_json::to_value(SystemSettings::from(&Config::default())).unwrap();
    validate_shape(&request.settings, &template, "")?;
    let settings: SystemSettings = serde_json::from_value(request.settings)
        .map_err(|_| invalid("参数类型不正确，请检查整数、开关和选项值"))?;
    settings.validate(endpoints)?;
    manager.with_configuration(|config, mode| {
        let path = config
            .config_path()
            .ok_or_else(|| invalid("配置文件路径未知，无法保存"))?;
        let mut document = read_document(path)?;
        if revision(&settings_from_document(&document)?) != request.revision {
            return Err(AdminServiceError::ConfigurationConflict);
        }
        merge(&mut document, serde_json::to_value(&settings).unwrap());
        config.save_document(&document).map_err(storage_error)?;
        // 先持久化再更新热配置，写入失败时运行状态保持不变。
        *mode = settings.load_balancing_mode.clone();
        Ok(response(settings, config, mode, endpoints))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{AdminService, AdminState, create_admin_router};
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    struct Fixture {
        directory: std::path::PathBuf,
        manager: Arc<MultiTokenManager>,
        app: Router,
    }
    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("kiro-settings-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("config.json");
            std::fs::write(
                &path,
                serde_json::json!({
                    "apiKey":"private-api-key", "adminApiKey":"private-admin-key",
                    "apiKeys":["private-alice-key", "private-bob-key"],
                    "countTokensApiKey":"private-count-key", "proxyPassword":"private-password",
                    "proxyUrl":"http://user:private-secret@localhost:8080/path?private-query",
                    "endpoints":{"ide":{"customSecret":"private-endpoint-secret"}},
                    "extension":{"secret":"private-extension-secret"},
                    "concurrency":{"extension":42}
                })
                .to_string(),
            )
            .unwrap();
            let manager = Arc::new(
                MultiTokenManager::new(Config::load(&path).unwrap(), vec![], None, None, true)
                    .unwrap(),
            );
            let app = create_admin_router(AdminState::new(
                "test-admin",
                AdminService::new(manager.clone(), ["ide".into(), "cli".into()]),
            ));
            Self {
                directory,
                manager,
                app,
            }
        }
        fn path(&self) -> std::path::PathBuf {
            self.directory.join("config.json")
        }
        async fn call(
            &self,
            method: &str,
            path: &str,
            body: Value,
            authenticated: bool,
        ) -> (StatusCode, Value) {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json");
            if authenticated {
                request = request.header("x-api-key", "test-admin");
            }
            let response = self
                .app
                .clone()
                .oneshot(request.body(Body::from(body.to_string())).unwrap())
                .await
                .unwrap();
            assert_eq!(response.headers()["cache-control"], "no-store");
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
            assert!(!String::from_utf8_lossy(&bytes).contains("private-"));
            (status, serde_json::from_slice(&bytes).unwrap())
        }
        async fn get(&self) -> Value {
            let (status, body) = self.call("GET", "/settings", Value::Null, true).await;
            assert_eq!(status, StatusCode::OK);
            body
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[tokio::test]
    async fn authentication_precedes_settings_parsing_and_no_secrets_are_exposed() {
        let fixture = Fixture::new();
        for method in ["GET", "PUT"] {
            assert_eq!(
                fixture
                    .call(method, "/settings", Value::Null, false)
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
        let response = fixture.get().await;
        assert_eq!(
            fixture
                .call("GET", "/settings/kiro-version", Value::Null, false)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(response["settings"]["kiroVersion"], "1.4.121");
        assert_eq!(response["settings"]["kiroCliVersion"], "3.1.0");
        assert_eq!(response["settings"]["defaultEndpoint"], "cli");
        assert_eq!(response["settings"]["concurrency"]["maxInFlight"], 64);
        assert_eq!(response["defaults"]["requestLogRetentionDays"], 30);
        assert_eq!(response["canSave"], true);
        assert_eq!(response["restartRequired"], serde_json::json!([]));
        assert!(response["settings"].get("apiKey").is_none());
        assert!(response["settings"].get("apiKeys").is_none());
    }

    #[tokio::test]
    async fn cli_version_is_saved_independently_and_invalid_values_are_rejected() {
        let fixture = Fixture::new();
        let first = fixture.get().await;
        let mut settings = first["settings"].clone();
        settings["kiroCliVersion"] = "3.1.1".into();
        let (status, saved) = fixture
            .call(
                "PUT",
                "/settings",
                serde_json::json!({
                    "revision":first["revision"], "settings":settings
                }),
                true,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["settings"]["kiroCliVersion"], "3.1.1");
        assert_eq!(saved["running"]["kiroCliVersion"], "3.1.0");
        assert_eq!(saved["settings"]["kiroVersion"], "1.4.121");
        assert_eq!(
            saved["restartRequired"],
            serde_json::json!(["kiroCliVersion"])
        );
        assert_eq!(
            Config::load(fixture.path()).unwrap().kiro_cli_version,
            "3.1.1"
        );
        let mut invalid = saved["settings"].clone();
        invalid["kiroCliVersion"] = "3.2".into();
        assert_eq!(
            fixture
                .call(
                    "PUT",
                    "/settings",
                    serde_json::json!({
                        "revision":saved["revision"], "settings":invalid
                    }),
                    true
                )
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn version_updates_preserve_other_settings_and_require_restart() {
        let fixture = Fixture::new();
        let first = fixture.get().await;
        let mut settings = first["settings"].clone();
        settings["kiroVersion"] = "1.4.122".into();
        let (status, saved) = fixture
            .call(
                "PUT",
                "/settings",
                serde_json::json!({
                    "revision":first["revision"], "settings":settings
                }),
                true,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["settings"]["kiroVersion"], "1.4.122");
        assert_eq!(saved["running"]["kiroVersion"], "1.4.121");
        assert_eq!(saved["restartRequired"], serde_json::json!(["kiroVersion"]));
        assert_eq!(
            Config::load(fixture.path()).unwrap().kiro_version,
            "1.4.122"
        );
        let document: Value =
            serde_json::from_slice(&std::fs::read(fixture.path()).unwrap()).unwrap();
        assert_eq!(document["apiKey"], "private-api-key");
        assert_eq!(
            document["endpoints"]["ide"]["customSecret"],
            "private-endpoint-secret"
        );
        assert_eq!(
            saved["settings"]["concurrency"],
            first["settings"]["concurrency"]
        );
        assert_eq!(
            fixture
                .call(
                    "PUT",
                    "/settings",
                    serde_json::json!({
                        "revision":first["revision"], "settings":first["settings"]
                    }),
                    true
                )
                .await
                .0,
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn saving_preserves_all_unmanaged_values_and_distinguishes_runtime_from_saved_settings() {
        let fixture = Fixture::new();
        let original: Value =
            serde_json::from_slice(&std::fs::read(fixture.path()).unwrap()).unwrap();
        let first = fixture.get().await;
        let mut settings = first["settings"].clone();
        settings["concurrency"]["maxInFlight"] = 12.into();
        settings["requestLogRetentionDays"] = 7.into();
        settings["forceStream"] = true.into();
        settings["loadBalancingMode"] = "priority".into();
        let (status, saved) = fixture
            .call(
                "PUT",
                "/settings",
                serde_json::json!({"revision":first["revision"],"settings":settings}),
                true,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["running"]["concurrency"]["maxInFlight"], 64);
        assert_eq!(saved["settings"]["concurrency"]["maxInFlight"], 12);
        assert_eq!(saved["running"]["forceStream"], false);
        assert_eq!(saved["settings"]["forceStream"], true);
        assert_eq!(saved["running"]["loadBalancingMode"], "priority");
        assert_eq!(fixture.manager.get_load_balancing_mode(), "priority");
        assert_eq!(
            saved["restartRequired"],
            serde_json::json!([
                "concurrency.maxInFlight",
                "forceStream",
                "requestLogRetentionDays"
            ])
        );
        let document: Value =
            serde_json::from_slice(&std::fs::read(fixture.path()).unwrap()).unwrap();
        for key in [
            "apiKey",
            "apiKeys",
            "adminApiKey",
            "countTokensApiKey",
            "proxyPassword",
            "proxyUrl",
            "endpoints",
            "extension",
        ] {
            assert_eq!(document[key], original[key], "{key}");
        }
        assert_eq!(document["concurrency"]["extension"], 42);
        assert_eq!(fixture.get().await["settings"], saved["settings"]);
        let restarted = MultiTokenManager::new(
            Config::load(fixture.path()).unwrap(),
            vec![],
            None,
            None,
            true,
        )
        .unwrap();
        let restarted_response =
            read(&restarted, &HashSet::from(["ide".into(), "cli".into()])).unwrap();
        assert!(restarted_response.restart_required.is_empty());
        assert!(restarted_response.running.force_stream);
        // 保存运行值即可取消待重启的修改。
        let (status, restored) = fixture
            .call(
                "PUT",
                "/settings",
                serde_json::json!({"revision":saved["revision"], "settings":saved["running"]}),
                true,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(restored["restartRequired"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn validation_rejects_invalid_unknown_and_incomplete_settings_without_side_effects() {
        let fixture = Fixture::new();
        let first = fixture.get().await;
        let before = std::fs::read(fixture.path()).unwrap();
        let mut candidates = Vec::new();
        for (path, value) in [
            (
                vec!["kiroVersion"],
                Value::from("1.4.121\r\nx-header: value"),
            ),
            (vec!["kiroVersion"], Value::from("")),
            (vec!["kiroVersion"], Value::from("1.2.3-beta")),
            (vec!["region"], Value::from("private-host/path")),
            (vec!["loadBalancingMode"], Value::from("unsupported")),
            (vec!["defaultEndpoint"], Value::from("unregistered")),
            (vec!["concurrency", "maxInFlight"], Value::from(0)),
            (vec!["concurrency", "waitTimeoutSeconds"], Value::from(0)),
            (
                vec!["concurrency", "adaptiveConcurrency"],
                Value::from("true"),
            ),
            (
                vec!["requestLimits", "maxUpstreamAttempts"],
                Value::from(101),
            ),
            (
                vec!["requestLimits", "idleTimeoutSeconds"],
                Value::from(1.5),
            ),
            (vec!["sessionAffinity", "ttlSeconds"], Value::from(0)),
            (vec!["toolOutputRecoveryAttempts"], Value::from(3)),
            (vec!["countTokensTimeoutSeconds"], Value::from(0)),
            (vec!["requestLogRetentionDays"], Value::from(0)),
            (vec!["requestLogRetentionDays"], Value::from(3651)),
            (vec!["concurrency", "unsupported"], Value::from(1)),
            (vec!["apiKey"], Value::from("replacement")),
            (vec!["apiKeys"], serde_json::json!(["replacement"])),
        ] {
            let mut settings = first["settings"].clone();
            let mut node = &mut settings;
            for key in path {
                node = &mut node[key];
            }
            *node = value;
            candidates.push(settings);
        }
        let mut incomplete = first["settings"].clone();
        incomplete["concurrency"]
            .as_object_mut()
            .unwrap()
            .remove("maxWaiting");
        candidates.push(incomplete);
        for settings in candidates {
            let (status, _) = fixture
                .call(
                    "PUT",
                    "/settings",
                    serde_json::json!({"revision":first["revision"],"settings":settings}),
                    true,
                )
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(std::fs::read(fixture.path()).unwrap(), before);
            assert_eq!(fixture.manager.get_load_balancing_mode(), "balanced");
        }
    }

    #[tokio::test]
    async fn old_load_balancing_endpoint_and_external_edits_make_stale_saves_conflict() {
        let fixture = Fixture::new();
        let first = fixture.get().await;
        assert_eq!(
            fixture
                .call(
                    "PUT",
                    "/config/load-balancing",
                    serde_json::json!({"mode":"priority"}),
                    true
                )
                .await
                .0,
            StatusCode::OK
        );
        let body = serde_json::json!({"revision":first["revision"],"settings":first["settings"]});
        assert_eq!(
            fixture.call("PUT", "/settings", body, true).await.0,
            StatusCode::CONFLICT
        );
        let current = fixture.get().await;
        assert_eq!(current["settings"]["loadBalancingMode"], "priority");
        let mut document: Value =
            serde_json::from_slice(&std::fs::read(fixture.path()).unwrap()).unwrap();
        assert_eq!(document["concurrency"]["extension"], 42);
        document["requestLogRetentionDays"] = 10.into();
        std::fs::write(fixture.path(), document.to_string()).unwrap();
        assert_eq!(fixture.call("PUT", "/settings", serde_json::json!({"revision":current["revision"],"settings":current["settings"]}), true).await.0, StatusCode::CONFLICT);
        assert_eq!(
            Config::load(fixture.path())
                .unwrap()
                .request_log_retention_days,
            10
        );
    }

    #[tokio::test]
    async fn storage_failures_do_not_commit_runtime_and_responses_do_not_leak_file_values() {
        let fixture = Fixture::new();
        let first = fixture.get().await;
        let mut settings = first["settings"].clone();
        settings["loadBalancingMode"] = "priority".into();
        std::fs::remove_file(fixture.path()).unwrap();
        std::fs::create_dir(fixture.path()).unwrap();
        let body = serde_json::json!({"revision":first["revision"],"settings":settings});
        assert_eq!(
            fixture.call("PUT", "/settings", body.clone(), true).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(fixture.manager.get_load_balancing_mode(), "balanced");
        std::fs::remove_dir(fixture.path()).unwrap();
        std::fs::write(fixture.path(), r#"{"private-sensitive-content"#).unwrap();
        assert_eq!(
            fixture.call("GET", "/settings", Value::Null, true).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            fixture.call("PUT", "/settings", body, true).await.0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(fixture.manager.get_load_balancing_mode(), "balanced");
    }

    #[test]
    fn settings_without_config_path_are_read_only() {
        let manager = MultiTokenManager::new(Config::default(), vec![], None, None, true).unwrap();
        let endpoints = HashSet::from(["ide".into(), "cli".into()]);
        let response = read(&manager, &endpoints).unwrap();
        assert!(!response.can_save);
        let request = SaveSettingsRequest {
            revision: response.revision,
            settings: serde_json::to_value(response.settings).unwrap(),
        };
        assert!(matches!(
            save(&manager, &endpoints, request),
            Err(AdminServiceError::InvalidSettings(_))
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_atomic_write_preserves_file_and_runtime_mode() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let first = fixture.get().await;
        let before = std::fs::read(fixture.path()).unwrap();
        let mut settings = first["settings"].clone();
        settings["loadBalancingMode"] = "priority".into();
        let original_permissions = std::fs::metadata(&fixture.directory).unwrap().permissions();
        std::fs::set_permissions(&fixture.directory, std::fs::Permissions::from_mode(0o500))
            .unwrap();
        // root 可以绕过目录权限；这种环境由原子替换失败的其他测试覆盖。
        let probe = fixture.directory.join("write-permission-probe");
        if std::fs::write(&probe, []).is_ok() {
            std::fs::set_permissions(&fixture.directory, original_permissions).unwrap();
            std::fs::remove_file(probe).unwrap();
            return;
        }
        let (status, _) = fixture
            .call(
                "PUT",
                "/settings",
                serde_json::json!({
                    "revision":first["revision"], "settings":settings
                }),
                true,
            )
            .await;
        std::fs::set_permissions(&fixture.directory, original_permissions).unwrap();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(std::fs::read(fixture.path()).unwrap(), before);
        assert_eq!(fixture.manager.get_load_balancing_mode(), "balanced");
        assert_eq!(std::fs::read_dir(&fixture.directory).unwrap().count(), 1);
    }
}
