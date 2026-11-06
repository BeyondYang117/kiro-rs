use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    #[default]
    Rustls,
    NativeTls,
}

/// 单进程内上游调用的容量与等待策略。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConcurrencyConfig {
    pub max_in_flight: usize,
    pub max_in_flight_per_credential: u64,
    pub max_waiting: usize,
    pub wait_timeout_seconds: u64,
    pub rate_limit_cooldown_seconds: u64,
    /// 429 后减小账号并发，并在响应体完整读取后逐步恢复。
    pub adaptive_concurrency: bool,
    /// 每账号滚动 60 秒内的上游调用预留数；0 关闭主动限速。
    pub requests_per_minute: usize,
    /// 认证服务瞬态故障的初始退避秒数。
    pub refresh_backoff_seconds: u64,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 64,
            max_in_flight_per_credential: 8,
            max_waiting: 128,
            wait_timeout_seconds: 30,
            rate_limit_cooldown_seconds: 30,
            adaptive_concurrency: true,
            requests_per_minute: 60,
            refresh_backoff_seconds: 30,
        }
    }
}

impl ConcurrencyConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.requests_per_minute <= 1_000_000,
            "concurrency.requestsPerMinute 必须在 0..=1000000 之间"
        );
        anyhow::ensure!(
            (1..=86_400).contains(&self.refresh_backoff_seconds),
            "concurrency.refreshBackoffSeconds 必须在 1..=86400 之间"
        );
        anyhow::ensure!(
            (1..=tokio::sync::Semaphore::MAX_PERMITS).contains(&self.max_in_flight),
            "concurrency.maxInFlight 必须在 1..={} 之间",
            tokio::sync::Semaphore::MAX_PERMITS
        );
        anyhow::ensure!(
            self.max_in_flight_per_credential > 0,
            "concurrency.maxInFlightPerCredential 必须大于 0"
        );
        anyhow::ensure!(
            self.max_waiting <= tokio::sync::Semaphore::MAX_PERMITS,
            "concurrency.maxWaiting 超过允许上限"
        );
        anyhow::ensure!(
            (1..=86_400).contains(&self.wait_timeout_seconds),
            "concurrency.waitTimeoutSeconds 必须在 1..=86400 之间"
        );
        anyhow::ensure!(
            (1..=86_400).contains(&self.rate_limit_cooldown_seconds),
            "concurrency.rateLimitCooldownSeconds 必须在 1..=86400 之间"
        );
        Ok(())
    }
}

/// 会话软亲和：只有成功响应才建立/刷新绑定。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SessionAffinityConfig {
    pub enabled: bool,
    pub ttl_seconds: u64,
    pub max_entries: usize,
}

impl Default for SessionAffinityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_seconds: 1800,
            max_entries: 10_000,
        }
    }
}

impl SessionAffinityConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=86_400).contains(&self.ttl_seconds),
            "sessionAffinity.ttlSeconds 必须在 1..=86400 之间"
        );
        anyhow::ensure!(
            (1..=1_000_000).contains(&self.max_entries),
            "sessionAffinity.maxEntries 必须在 1..=1000000 之间"
        );
        Ok(())
    }
}

/// Bounds cover admission, retries, compaction and downstream body consumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RequestLimits {
    /// 一个客户端请求所有上游调用（含压缩、搜索和恢复）共用的尝试上限。
    pub max_upstream_attempts: usize,
    pub max_concurrent_requests: usize,
    pub total_timeout_seconds: u64,
    pub first_content_timeout_seconds: u64,
    pub idle_timeout_seconds: u64,
    pub max_response_bytes: usize,
    pub max_response_events: usize,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_upstream_attempts: 9,
            max_concurrent_requests: 64,
            total_timeout_seconds: 900,
            first_content_timeout_seconds: 180,
            idle_timeout_seconds: 120,
            max_response_bytes: 32 * 1024 * 1024,
            max_response_events: 100_000,
        }
    }
}

impl RequestLimits {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=100).contains(&self.max_upstream_attempts),
            "requestLimits.maxUpstreamAttempts 必须在 1..=100 之间"
        );
        anyhow::ensure!(
            (1..=tokio::sync::Semaphore::MAX_PERMITS).contains(&self.max_concurrent_requests),
            "requestLimits.maxConcurrentRequests 超出允许范围"
        );
        for (name, value) in [
            ("totalTimeoutSeconds", self.total_timeout_seconds),
            (
                "firstContentTimeoutSeconds",
                self.first_content_timeout_seconds,
            ),
            ("idleTimeoutSeconds", self.idle_timeout_seconds),
        ] {
            anyhow::ensure!(
                (1..=86_400).contains(&value),
                "requestLimits.{name} 必须在 1..=86400 之间"
            );
        }
        anyhow::ensure!(
            (1024..=1024 * 1024 * 1024).contains(&self.max_response_bytes),
            "requestLimits.maxResponseBytes 必须在 1024..=1073741824 之间"
        );
        anyhow::ensure!(
            (1..=1_000_000).contains(&self.max_response_events),
            "requestLimits.maxResponseEvents 必须在 1..=1000000 之间"
        );
        Ok(())
    }
}

/// KNA 应用配置
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// 远程 token 计数超时（秒），失败后回退本地估算。
    #[serde(default = "default_count_tokens_timeout_seconds")]
    pub count_tokens_timeout_seconds: u64,

    /// 强制客户端消息请求使用 SSE，包括 stream=false 或未指定 stream 的请求。
    #[serde(default)]
    pub force_stream: bool,

    /// Claude Code 端点是否缓冲完整响应以更正初始 usage。
    #[serde(default = "default_cc_buffered_stream")]
    pub cc_buffered_stream: bool,

    /// 未发出响应内容时，截断工具调用的缩块恢复次数（0..=2）。
    #[serde(default = "default_tool_output_recovery_attempts")]
    pub tool_output_recovery_attempts: u8,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 调用日志数据库；默认在 credentials.json 同目录。
    #[serde(default)]
    pub request_log_path: Option<String>,

    /// 调用日志保留天数（至少 1 天），同时最多保留 100000 条。
    #[serde(default = "default_request_log_retention_days")]
    pub request_log_retention_days: u32,

    /// 负载均衡模式（"priority" 或 "balanced"）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    #[serde(default)]
    pub concurrency: ConcurrencyConfig,

    #[serde(default)]
    pub request_limits: RequestLimits,

    #[serde(default)]
    pub session_affinity: SessionAffinityConfig,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 保留扩展配置，管理 API 更新配置时避免丢失未知字段。
    #[serde(flatten)]
    extra_fields: HashMap<String, serde_json::Value>,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("region", &self.region)
            .field("tls_backend", &self.tls_backend)
            .field("load_balancing_mode", &self.load_balancing_mode)
            .field("concurrency", &self.concurrency)
            .field("session_affinity", &self.session_affinity)
            .field("cc_buffered_stream", &self.cc_buffered_stream)
            .field("force_stream", &self.force_stream)
            .field("default_endpoint", &self.default_endpoint)
            .finish_non_exhaustive()
    }
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_request_log_retention_days() -> u32 {
    30
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "0.11.107".to_string()
}

fn default_system_version() -> String {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        platform => platform,
    }
    .to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_count_tokens_timeout_seconds() -> u64 {
    10
}

fn default_cc_buffered_stream() -> bool {
    true
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    "balanced".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_tool_output_recovery_attempts() -> u8 {
    1
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            count_tokens_timeout_seconds: default_count_tokens_timeout_seconds(),
            force_stream: false,
            cc_buffered_stream: default_cc_buffered_stream(),
            tool_output_recovery_attempts: default_tool_output_recovery_attempts(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            request_log_path: None,
            request_log_retention_days: default_request_log_retention_days(),
            load_balancing_mode: default_load_balancing_mode(),
            concurrency: ConcurrencyConfig::default(),
            request_limits: RequestLimits::default(),
            session_affinity: SessionAffinityConfig::default(),
            extract_thinking: default_extract_thinking(),
            default_endpoint: default_endpoint(),
            endpoints: HashMap::new(),
            extra_fields: HashMap::new(),
            config_path: None,
        }
    }
}

impl Config {
    pub(crate) fn validate_regions(&self) -> anyhow::Result<()> {
        crate::common::region::validate("region", &self.region)?;
        if let Some(region) = &self.auth_region {
            crate::common::region::validate("authRegion", region)?;
        }
        if let Some(region) = &self.api_region {
            crate::common::region::validate("apiRegion", region)?;
        }
        Ok(())
    }

    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    config_path: Some(path.to_path_buf()),
                    ..Default::default()
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut config: Config = serde_json::from_str(&content)?;
        config.validate_regions()?;
        config.config_path = Some(path.to_path_buf());
        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将合并后的配置文档写回原始路径，调用方保留未管理的参数。
    pub fn save_document(&self, document: &serde_json::Value) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(document).context("序列化配置失败")?;
        crate::common::persistence::atomic_write(path, content.as_bytes())
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_regions_fail_startup_including_disabled_credentials() {
        use crate::kiro::{model::credentials::KiroCredentials, token_manager::MultiTokenManager};
        for field in ["region", "authRegion", "apiRegion"] {
            let invalid = serde_json::json!({field:"private.invalid/path?key=value"});
            let config: Config = serde_json::from_value(invalid.clone()).unwrap();
            let error = MultiTokenManager::new(config, vec![], None, None, false)
                .err()
                .unwrap();
            assert!(error.is::<crate::common::region::InvalidRegion>());
            assert!(!error.to_string().contains("private.invalid"));

            let mut credential: KiroCredentials = serde_json::from_value(invalid).unwrap();
            credential.disabled = true;
            let error =
                MultiTokenManager::new(Config::default(), vec![credential], None, None, false)
                    .err()
                    .unwrap();
            assert!(error.is::<crate::common::region::InvalidRegion>());
        }
    }

    #[test]
    fn config_loading_rejects_url_fragments_in_regions() {
        let path = std::env::temp_dir().join(format!("kiro-region-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, r#"{"authRegion":"private.invalid/"}"#).unwrap();
        let result = Config::load(&path);
        std::fs::remove_file(path).unwrap();
        assert!(
            result
                .unwrap_err()
                .is::<crate::common::region::InvalidRegion>()
        );
    }

    #[test]
    fn tool_output_recovery_defaults_and_roundtrip() {
        let defaults: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.tool_output_recovery_attempts, 1);
        for attempts in [0, 1, 2] {
            let config: Config =
                serde_json::from_value(serde_json::json!({"toolOutputRecoveryAttempts":attempts}))
                    .unwrap();
            assert_eq!(config.tool_output_recovery_attempts, attempts);
            assert_eq!(
                serde_json::to_value(config).unwrap()["toolOutputRecoveryAttempts"],
                attempts
            );
        }
        let invalid: Config = serde_json::from_str(r#"{"toolOutputRecoveryAttempts":3}"#).unwrap();
        // Constructor validates settings before requiring any credentials.
        assert!(
            crate::kiro::token_manager::MultiTokenManager::new(invalid, vec![], None, None, false)
                .err()
                .unwrap()
                .to_string()
                .contains("toolOutputRecoveryAttempts")
        );
    }

    #[test]
    fn session_affinity_defaults_roundtrip_and_validation() {
        let defaults: Config = serde_json::from_str("{}").unwrap();
        assert!(defaults.session_affinity.enabled);
        assert_eq!(defaults.session_affinity.ttl_seconds, 1800);
        assert_eq!(defaults.session_affinity.max_entries, 10_000);
        let partial: Config = serde_json::from_value(serde_json::json!({
            "sessionAffinity":{"enabled":false,"ttlSeconds":60}
        }))
        .unwrap();
        partial.session_affinity.validate().unwrap();
        let encoded = serde_json::to_value(partial).unwrap();
        assert_eq!(encoded["sessionAffinity"]["enabled"], false);
        assert_eq!(encoded["sessionAffinity"]["maxEntries"], 10_000);
        for value in [
            serde_json::json!({"ttlSeconds":0}),
            serde_json::json!({"ttlSeconds":u64::MAX}),
            serde_json::json!({"maxEntries":0}),
            serde_json::json!({"maxEntries":usize::MAX}),
        ] {
            let settings: SessionAffinityConfig = serde_json::from_value(value).unwrap();
            assert!(settings.validate().is_err());
        }
    }

    #[test]
    fn concurrency_defaults_partial_settings_and_roundtrip() {
        let defaults: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.load_balancing_mode, "balanced");
        assert_eq!(defaults.concurrency.max_in_flight, 64);
        assert_eq!(defaults.concurrency.max_in_flight_per_credential, 8);
        assert_eq!(defaults.concurrency.max_waiting, 128);
        assert!(defaults.concurrency.adaptive_concurrency);
        assert_eq!(defaults.concurrency.requests_per_minute, 60);
        assert_eq!(defaults.concurrency.refresh_backoff_seconds, 30);
        assert_eq!(defaults.request_limits.max_upstream_attempts, 9);
        let config: Config = serde_json::from_str(
            r#"{"loadBalancingMode":"priority","concurrency":{"maxWaiting":0,"maxInFlight":2}}"#,
        )
        .unwrap();
        config.concurrency.validate().unwrap();
        assert_eq!(config.load_balancing_mode, "priority");
        assert_eq!(config.concurrency.max_waiting, 0);
        assert_eq!(config.concurrency.max_in_flight_per_credential, 8);
        assert!(config.concurrency.adaptive_concurrency);
        assert_eq!(config.concurrency.wait_timeout_seconds, 30);
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(encoded["concurrency"]["maxInFlight"], 2);
        assert_eq!(encoded["concurrency"]["maxWaiting"], 0);
        assert_eq!(encoded["concurrency"]["adaptiveConcurrency"], true);
        let explicit: Config = serde_json::from_value(serde_json::json!({
            "concurrency": {"maxInFlightPerCredential": 4, "adaptiveConcurrency": false}
        }))
        .unwrap();
        assert_eq!(explicit.concurrency.max_in_flight_per_credential, 4);
        assert!(!explicit.concurrency.adaptive_concurrency);
    }

    #[test]
    fn invalid_concurrency_settings_fail_validation_instead_of_panicking_or_hanging() {
        for settings in [
            serde_json::json!({"requestsPerMinute":1_000_001}),
            serde_json::json!({"refreshBackoffSeconds":0}),
            serde_json::json!({"refreshBackoffSeconds":86_401}),
            serde_json::json!({"maxInFlight":0}),
            serde_json::json!({"maxInFlight":usize::MAX}),
            serde_json::json!({"maxInFlightPerCredential":0}),
            serde_json::json!({"maxWaiting":usize::MAX}),
            serde_json::json!({"waitTimeoutSeconds":0}),
            serde_json::json!({"waitTimeoutSeconds":u64::MAX}),
            serde_json::json!({"rateLimitCooldownSeconds":0}),
            serde_json::json!({"rateLimitCooldownSeconds":u64::MAX}),
        ] {
            let config: ConcurrencyConfig = serde_json::from_value(settings).unwrap();
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn streaming_and_token_timeout_settings_preserve_legacy_defaults_and_roundtrip() {
        let defaults: Config = serde_json::from_str("{}").unwrap();
        assert!(defaults.cc_buffered_stream);
        assert!(!defaults.force_stream);
        assert_eq!(defaults.count_tokens_timeout_seconds, 10);
        let config: Config = serde_json::from_str(
            r#"{"ccBufferedStream":false,"forceStream":true,"countTokensTimeoutSeconds":3}"#,
        )
        .unwrap();
        assert!(!config.cc_buffered_stream);
        assert!(config.force_stream);
        assert_eq!(config.count_tokens_timeout_seconds, 3);
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(encoded["ccBufferedStream"], false);
        assert_eq!(encoded["forceStream"], true);
        assert_eq!(encoded["countTokensTimeoutSeconds"], 3);
    }

    #[test]
    fn config_debug_omits_keys_proxy_auth_and_custom_endpoint_secrets() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "apiKey":"sensitive-sentinel", "adminApiKey":"sensitive-sentinel",
            "countTokensApiKey":"sensitive-sentinel",
            "proxyUrl":"http://user:sensitive-sentinel@localhost",
            "proxyPassword":"sensitive-sentinel", "endpoints":{"test":{"key":"sensitive-sentinel"}}
        }))
        .unwrap();
        assert!(!format!("{config:?}").contains("sensitive-sentinel"));
    }

    #[test]
    fn missing_config_can_be_created_with_private_permissions() {
        let path = std::env::temp_dir().join(format!("kiro-config-{}.json", uuid::Uuid::new_v4()));
        let config = Config::load(&path).unwrap();
        config
            .save_document(&serde_json::to_value(&config).unwrap())
            .unwrap();
        assert_eq!(Config::load(&path).unwrap().port, config.port);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_file(path).unwrap();
    }
}
