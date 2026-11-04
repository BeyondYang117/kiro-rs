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
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 64,
            max_in_flight_per_credential: 4,
            max_waiting: 128,
            wait_timeout_seconds: 30,
            rate_limit_cooldown_seconds: 30,
            adaptive_concurrency: false,
        }
    }
}

impl ConcurrencyConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
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
            .field("cc_buffered_stream", &self.cc_buffered_stream)
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
    const SYSTEM_VERSIONS: &[&str] = &["darwin#24.6.0", "win32#10.0.22631"];
    SYSTEM_VERSIONS[fastrand::usize(..SYSTEM_VERSIONS.len())].to_string()
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
            extract_thinking: default_extract_thinking(),
            default_endpoint: default_endpoint(),
            endpoints: HashMap::new(),
            extra_fields: HashMap::new(),
            config_path: None,
        }
    }
}

impl Config {
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
        config.config_path = Some(path.to_path_buf());
        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        crate::common::persistence::atomic_write(path, content.as_bytes())
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn concurrency_defaults_partial_settings_and_roundtrip() {
        let defaults: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.load_balancing_mode, "balanced");
        assert_eq!(defaults.concurrency.max_in_flight, 64);
        assert_eq!(defaults.concurrency.max_in_flight_per_credential, 4);
        assert_eq!(defaults.concurrency.max_waiting, 128);
        assert!(!defaults.concurrency.adaptive_concurrency);
        let config: Config = serde_json::from_str(
            r#"{"loadBalancingMode":"priority","concurrency":{"maxWaiting":0,"maxInFlight":2}}"#,
        )
        .unwrap();
        config.concurrency.validate().unwrap();
        assert_eq!(config.load_balancing_mode, "priority");
        assert_eq!(config.concurrency.max_waiting, 0);
        assert_eq!(config.concurrency.wait_timeout_seconds, 30);
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(encoded["concurrency"]["maxInFlight"], 2);
        assert_eq!(encoded["concurrency"]["maxWaiting"], 0);
        assert_eq!(encoded["concurrency"]["adaptiveConcurrency"], false);
        let enabled: Config = serde_json::from_value(serde_json::json!({
            "concurrency": {"adaptiveConcurrency": true}
        }))
        .unwrap();
        assert!(enabled.concurrency.adaptive_concurrency);
    }

    #[test]
    fn invalid_concurrency_settings_fail_validation_instead_of_panicking_or_hanging() {
        for settings in [
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
        assert_eq!(defaults.count_tokens_timeout_seconds, 10);
        let config: Config =
            serde_json::from_str(r#"{"ccBufferedStream":false,"countTokensTimeoutSeconds":3}"#)
                .unwrap();
        assert!(!config.cc_buffered_stream);
        assert_eq!(config.count_tokens_timeout_seconds, 3);
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(encoded["ccBufferedStream"], false);
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
        config.save().unwrap();
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
