//! Token 管理模块
//!
//! 负责 Token 过期检测和刷新，支持 Social 和 IdC 认证方式
//! 支持多凭据 (MultiTokenManager) 管理

use anyhow::bail;
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as TokioMutex, Notify, OwnedSemaphorePermit, Semaphore};

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration as StdDuration, Instant};

use crate::common::persistence::atomic_write as atomic_write_credentials;
use crate::http_client::{ProxyConfig, build_client};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::token_refresh::{
    IdcRefreshRequest, IdcRefreshResponse, RefreshRequest, RefreshResponse,
};
use crate::kiro::model::usage_limits::UsageLimitsResponse;
use crate::model::config::Config;

mod concurrency;
pub use concurrency::ConcurrencySnapshot;
use concurrency::{ConcurrencyMetrics, Waiter};

/// 检查 Token 是否在指定时间内过期
pub(crate) fn is_token_expiring_within(
    credentials: &KiroCredentials,
    minutes: i64,
) -> Option<bool> {
    credentials
        .expires_at
        .as_ref()
        .and_then(|expires_at| DateTime::parse_from_rfc3339(expires_at).ok())
        .map(|expires| expires <= Utc::now() + Duration::minutes(minutes))
}

/// 检查 Token 是否已过期（提前 5 分钟判断）
pub(crate) fn is_token_expired(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 5).unwrap_or(true)
}

/// 检查 Token 是否即将过期（10分钟内）
pub(crate) fn is_token_expiring_soon(credentials: &KiroCredentials) -> bool {
    is_token_expiring_within(credentials, 10).unwrap_or(false)
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// 生成 API Key 脱敏展示(前 4 + ... + 后 4,长度不足或非 ASCII 回退 ***)
fn mask_api_key(key: &str) -> String {
    if key.is_ascii() && key.len() > 16 {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    } else {
        "***".to_string()
    }
}

/// 验证 refreshToken 的基本有效性
pub(crate) fn validate_refresh_token(credentials: &KiroCredentials) -> anyhow::Result<()> {
    let refresh_token = credentials
        .refresh_token
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;

    if refresh_token.is_empty() {
        bail!("refreshToken 为空");
    }

    if refresh_token.len() < 100 || refresh_token.ends_with("...") || refresh_token.contains("...")
    {
        bail!(
            "refreshToken 已被截断（长度: {} 字符）。\n\
             这通常是 Kiro IDE 为了防止凭证被第三方工具使用而故意截断的。",
            refresh_token.len()
        );
    }

    Ok(())
}

/// Refresh Token 永久失效错误
///
/// 当服务端返回 400 + `invalid_grant` 时，表示 refreshToken 已被撤销或过期，
/// 不应重试，需立即禁用对应凭据。
#[derive(Debug)]
pub(crate) struct RefreshTokenInvalidError {
    pub message: String,
}

impl fmt::Display for RefreshTokenInvalidError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RefreshTokenInvalidError {}

/// 刷新 Token
pub(crate) async fn refresh_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    // API Key 凭据不支持 Token 刷新：底层契约级拦截
    // 其他调用点（try_ensure_token / 活跃路径 / add_credential）在调用前已显式分流 API Key；
    // 仅 force_refresh_token_for 未分流，此处 bail 让错误自然传播为 400 BAD_REQUEST。
    if credentials.is_api_key_credential() {
        bail!("API Key 凭据不支持刷新 Token");
    }

    validate_refresh_token(credentials)?;

    // 根据 auth_method 选择刷新方式
    // 如果未指定 auth_method，根据是否有 clientId/clientSecret 自动判断
    let auth_method = credentials.auth_method.as_deref().unwrap_or_else(|| {
        if credentials.client_id.is_some() && credentials.client_secret.is_some() {
            "idc"
        } else {
            "social"
        }
    });

    if auth_method.eq_ignore_ascii_case("idc")
        || auth_method.eq_ignore_ascii_case("builder-id")
        || auth_method.eq_ignore_ascii_case("iam")
    {
        refresh_idc_token(credentials, config, proxy).await
    } else {
        refresh_social_token(credentials, config, proxy).await
    }
}

/// 刷新 Social Token
async fn refresh_social_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 Social Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);

    let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);
    let refresh_domain = format!("prod.{}.auth.desktop.kiro.dev", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = &config.kiro_version;

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = RefreshRequest {
        refresh_token: refresh_token.to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header(
            "User-Agent",
            format!("KiroIDE-{}-{}", kiro_version, machine_id),
        )
        .header("Accept-Encoding", "gzip, compress, deflate, br")
        .header("host", &refresh_domain)
        .header("Connection", "close")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("Social refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "OAuth 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OAuth 服务暂时不可用",
            _ => "Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: RefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    Ok(new_credentials)
}

/// 刷新 IdC Token (AWS SSO OIDC)
async fn refresh_idc_token(
    credentials: &KiroCredentials,
    config: &Config,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<KiroCredentials> {
    tracing::info!("正在刷新 IdC Token...");

    let refresh_token = credentials.refresh_token.as_ref().unwrap();
    let client_id = credentials
        .client_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientId"))?;
    let client_secret = credentials
        .client_secret
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("IdC 刷新需要 clientSecret"))?;

    // 优先级：凭据.auth_region > 凭据.region > config.auth_region > config.region
    let region = credentials.effective_auth_region(config);
    let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    let x_amz_user_agent = "aws-sdk-js/3.980.0 KiroIDE";
    let user_agent = format!(
        "aws-sdk-js/3.980.0 ua/2.1 os/{} lang/js md/nodejs#{} api/sso-oidc#3.980.0 m/E KiroIDE",
        os_name, node_version
    );

    let client = build_client(proxy, 60, config.tls_backend)?;
    let body = IdcRefreshRequest {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        refresh_token: refresh_token.to_string(),
        grant_type: "refresh_token".to_string(),
    };

    let response = client
        .post(&refresh_url)
        .header("content-type", "application/json")
        .header("x-amz-user-agent", x_amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", format!("oidc.{}.amazonaws.com", region))
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=4")
        .header("Connection", "close")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();

        // 400 + invalid_grant + Invalid refresh token provided → refreshToken 永久失效
        if status.as_u16() == 400
            && body_text.contains("\"invalid_grant\"")
            && body_text.contains("Invalid refresh token provided")
        {
            return Err(RefreshTokenInvalidError {
                message: format!("IdC refreshToken 已失效 (invalid_grant): {}", body_text),
            }
            .into());
        }

        let error_msg = match status.as_u16() {
            401 => "IdC 凭证已过期或无效，需要重新认证",
            403 => "权限不足，无法刷新 Token",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS OIDC 服务暂时不可用",
            _ => "IdC Token 刷新失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: IdcRefreshResponse = response.json().await?;

    let mut new_credentials = credentials.clone();
    new_credentials.access_token = Some(data.access_token);

    if let Some(new_refresh_token) = data.refresh_token {
        new_credentials.refresh_token = Some(new_refresh_token);
    }

    if let Some(expires_in) = data.expires_in {
        let expires_at = Utc::now() + Duration::seconds(expires_in);
        new_credentials.expires_at = Some(expires_at.to_rfc3339());
    }

    // 同步更新 profile_arn（如果 IdC 响应中包含）
    if let Some(profile_arn) = data.profile_arn {
        new_credentials.profile_arn = Some(profile_arn);
    }

    Ok(new_credentials)
}

/// 获取使用额度信息
pub(crate) async fn get_usage_limits(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<UsageLimitsResponse> {
    tracing::debug!("正在获取使用额度信息...");

    // 优先级：凭据.api_region > config.api_region > config.region
    let region = credentials.effective_api_region(config);
    let host = format!("q.{}.amazonaws.com", region);
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let kiro_version = &config.kiro_version;
    let os_name = &config.system_version;
    let node_version = &config.node_version;

    // 构建 URL
    let mut url = format!(
        "https://{}/getUsageLimits?origin=AI_EDITOR&resourceType=AGENTIC_REQUEST",
        host
    );

    // profileArn 是可选的
    if let Some(profile_arn) = &credentials.profile_arn {
        url.push_str(&format!("&profileArn={}", urlencoding::encode(profile_arn)));
    }

    // 构建 User-Agent headers
    let user_agent = format!(
        "aws-sdk-js/1.0.0 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererruntime#1.0.0 m/N,E KiroIDE-{}-{}",
        os_name, node_version, kiro_version, machine_id
    );
    let amz_user_agent = format!("aws-sdk-js/1.0.0 KiroIDE-{}-{}", kiro_version, machine_id);

    let client = build_client(proxy, 60, config.tls_backend)?;

    let mut request = client
        .get(&url)
        .header("x-amz-user-agent", &amz_user_agent)
        .header("user-agent", &user_agent)
        .header("host", &host)
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1")
        .header("Authorization", format!("Bearer {}", token))
        .header("Connection", "close");

    if credentials.is_api_key_credential() {
        request = request.header("tokentype", "API_KEY");
    }

    let response = request.send().await?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().await.unwrap_or_default();
        let error_msg = match status.as_u16() {
            401 => "认证失败，Token 无效或已过期",
            403 => "权限不足，无法获取使用额度",
            429 => "请求过于频繁，已被限流",
            500..=599 => "服务器错误，AWS 服务暂时不可用",
            _ => "获取使用额度失败",
        };
        bail!("{}: {} {}", error_msg, status, body_text);
    }

    let data: UsageLimitsResponse = response.json().await?;
    Ok(data)
}

// ============================================================================
// 多凭据 Token 管理器
// ============================================================================

/// 单个凭据条目的状态
struct CredentialEntry {
    /// 凭据唯一 ID
    id: u64,
    /// 凭据信息
    credentials: KiroCredentials,
    /// API 调用连续失败次数
    failure_count: u32,
    /// Token 刷新连续失败次数
    refresh_failure_count: u32,
    /// 是否已禁用
    disabled: bool,
    /// 禁用原因（用于区分手动禁用 vs 自动禁用，便于自愈）
    disabled_reason: Option<DisabledReason>,
    /// API 调用成功次数
    success_count: u64,
    /// 当前尚未消费完响应的调用数。
    in_flight: Arc<AtomicU64>,
    refresh_lock: Arc<TokioMutex<()>>,
    cooldown: Option<(Instant, StdDuration)>,
    adaptive_limit: concurrency::AdaptiveLimit,
    /// 最后一次 API 调用时间（RFC3339 格式）
    last_used_at: Option<String>,
}

/// 禁用原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledReason {
    /// Admin API 手动禁用
    Manual,
    /// 连续失败达到阈值后自动禁用
    TooManyFailures,
    /// Token 刷新连续失败达到阈值后自动禁用
    TooManyRefreshFailures,
    /// 额度已用尽（如 MONTHLY_REQUEST_COUNT）
    QuotaExceeded,
    /// Refresh Token 永久失效（服务端返回 invalid_grant）
    InvalidRefreshToken,
    /// 凭据配置无效（如 authMethod=api_key 但缺少 kiroApiKey）
    InvalidConfig,
}

/// 统计数据持久化条目
#[derive(Serialize, Deserialize)]
struct StatsEntry {
    success_count: u64,
    last_used_at: Option<String>,
}

// ============================================================================
// Admin API 公开结构
// ============================================================================

/// 凭据条目快照（用于 Admin API 读取）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialEntrySnapshot {
    /// 凭据唯一 ID
    pub id: u64,
    /// 优先级
    pub priority: u32,
    /// 是否被禁用
    pub disabled: bool,
    /// 连续失败次数
    pub failure_count: u32,
    /// 认证方式
    pub auth_method: Option<String>,
    /// 是否有 Profile ARN
    pub has_profile_arn: bool,
    /// Token 过期时间
    pub expires_at: Option<String>,
    /// refreshToken 的 SHA-256 哈希（仅 OAuth 凭据，用于前端去重）
    pub refresh_token_hash: Option<String>,
    /// kiroApiKey 的 SHA-256 哈希（仅 API Key 凭据，用于前端去重）
    pub api_key_hash: Option<String>,
    /// kiroApiKey 的脱敏展示（仅 API Key 凭据，用于前端显示）
    pub masked_api_key: Option<String>,
    /// 用户邮箱（用于前端显示）
    pub email: Option<String>,
    /// API 调用成功次数
    pub success_count: u64,
    pub in_flight: u64,
    pub cooldown_remaining_ms: u64,
    pub effective_concurrency_limit: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adaptive_probe_target: Option<u64>,
    /// 最后一次 API 调用时间（RFC3339 格式）
    pub last_used_at: Option<String>,
    /// 是否配置了凭据级代理
    pub has_proxy: bool,
    /// 代理 URL（用于前端展示）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    /// Token 刷新连续失败次数
    pub refresh_failure_count: u32,
    /// 禁用原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// 端点名称（未显式配置时返回 None，由 Admin 层回退到默认值）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

/// 凭据管理器状态快照
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSnapshot {
    pub concurrency: ConcurrencySnapshot,
    /// 凭据条目列表
    pub entries: Vec<CredentialEntrySnapshot>,
    /// 当前活跃凭据 ID
    pub current_id: u64,
    /// 总凭据数量
    pub total: usize,
    /// 可用凭据数量
    pub available: usize,
}

/// 多凭据 Token 管理器
///
/// 支持多个凭据的管理，实现固定优先级 + 故障转移策略
/// 故障统计基于 API 调用结果，而非 Token 刷新结果
pub struct MultiTokenManager {
    config: Config,
    proxy: Option<ProxyConfig>,
    /// 凭据条目列表
    entries: Mutex<Vec<CredentialEntry>>,
    /// 进程内已分配的最大 ID，删除凭据后也不回退。
    last_assigned_id: AtomicU64,
    /// 当前活动凭据 ID
    current_id: Mutex<u64>,
    /// 在途名额由调用上下文持有，包含 Token 获取和响应体消费。
    in_flight_slots: Arc<Semaphore>,
    waiting_slots: Semaphore,
    /// 锁顺序：wait_queue -> entries -> current_id/load_balancing_mode。
    wait_queue: Mutex<Vec<Waiter>>,
    concurrency_metrics: ConcurrencyMetrics,
    capacity_changed: Arc<Notify>,
    /// 凭据文件路径（用于回写）
    credentials_path: Option<PathBuf>,
    /// 是否使用数组格式；单对象添加第二个凭据后升级为数组。
    is_multiple_format: AtomicBool,
    /// 串行化快照和文件替换，避免旧快照覆盖较新的持久化结果。
    persistence_lock: Mutex<()>,
    /// 负载均衡模式（运行时可修改）
    load_balancing_mode: Mutex<String>,
    /// 最近一次统计持久化时间（用于 debounce）
    last_stats_save_at: Mutex<Option<Instant>>,
    /// 串行化统计快照与文件替换，避免旧快照覆盖新数据。
    stats_persistence_lock: Mutex<()>,
    /// 统计数据是否有未落盘更新
    stats_dirty: AtomicBool,
}

/// 每个凭据最大 API 调用失败次数
const MAX_FAILURES_PER_CREDENTIAL: u32 = 3;
/// 统计数据持久化防抖间隔
const STATS_SAVE_DEBOUNCE: StdDuration = StdDuration::from_secs(30);

/// 选择凭据时持锁预留，最后一个调用上下文销毁时释放。
/// Arc 使克隆上下文不会重复计数；取消 future 或丢弃响应同样会释放。
struct InFlightReservation {
    in_flight: Arc<AtomicU64>,
    global_slot: Option<OwnedSemaphorePermit>,
    capacity_changed: Arc<Notify>,
    adaptive_epoch: Arc<()>,
    transport_success_reported: AtomicBool,
}

impl InFlightReservation {
    fn reserve(
        entry: &CredentialEntry,
        global_slot: OwnedSemaphorePermit,
        capacity_changed: Arc<Notify>,
    ) -> Arc<Self> {
        entry.in_flight.fetch_add(1, Ordering::Relaxed);
        Arc::new(Self {
            in_flight: Arc::clone(&entry.in_flight),
            global_slot: Some(global_slot),
            capacity_changed,
            adaptive_epoch: entry.adaptive_limit.epoch.clone(),
            transport_success_reported: AtomicBool::new(false),
        })
    }
}

impl Drop for InFlightReservation {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        // 先释放全局名额再唤醒，避免等待者醒来后再次看到全局容量已满。
        self.global_slot.take();
        self.capacity_changed.notify_waiters();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityError {
    QueueFull,
    WaitTimeout,
}

impl fmt::Display for CapacityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::QueueFull => "上游调用等待队列已满，请稍后重试",
            Self::WaitTimeout => "等待可用上游调用名额超时，请稍后重试",
        })
    }
}

impl std::error::Error for CapacityError {}

struct ReservedCredential {
    id: u64,
    credentials: KiroCredentials,
    reservation: Arc<InFlightReservation>,
}

/// API 调用上下文
///
/// 绑定特定凭据的调用上下文，确保 token、credentials 和 id 的一致性
/// 用于解决并发调用时 current_id 竞态问题
#[derive(Clone)]
pub struct CallContext {
    /// 凭据 ID（用于 report_success/report_failure）
    pub id: u64,
    /// 凭据信息（用于构建请求头）
    pub credentials: KiroCredentials,
    /// 访问 Token
    pub token: String,
    reservation: Option<Arc<InFlightReservation>>,
}

impl MultiTokenManager {
    /// 创建多凭据 Token 管理器
    ///
    /// # Arguments
    /// * `config` - 应用配置
    /// * `credentials` - 凭据列表
    /// * `proxy` - 可选的代理配置
    /// * `credentials_path` - 凭据文件路径（用于回写）
    /// * `is_multiple_format` - 是否为多凭据数组格式
    pub fn new(
        config: Config,
        credentials: Vec<KiroCredentials>,
        proxy: Option<ProxyConfig>,
        credentials_path: Option<PathBuf>,
        is_multiple_format: bool,
    ) -> anyhow::Result<Self> {
        config.concurrency.validate()?;
        anyhow::ensure!(
            config.tool_output_recovery_attempts <= 2,
            "toolOutputRecoveryAttempts 必须在 0..=2 之间"
        );
        // 计算当前最大 ID，为没有 ID 的凭据分配新 ID
        let max_existing_id = credentials.iter().filter_map(|c| c.id).max().unwrap_or(0);
        let mut last_assigned_id = max_existing_id;
        let mut has_new_ids = false;
        let mut has_new_machine_ids = false;
        let config_ref = &config;

        let entries: Vec<CredentialEntry> = credentials
            .into_iter()
            .map(|mut cred| {
                cred.canonicalize_auth_method();
                let id = match cred.id {
                    Some(id) => id,
                    None => {
                        last_assigned_id = last_assigned_id
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("凭据 ID 已耗尽"))?;
                        cred.id = Some(last_assigned_id);
                        has_new_ids = true;
                        last_assigned_id
                    }
                };
                if cred.machine_id.is_none() {
                    cred.machine_id =
                        Some(machine_id::generate_from_credentials(&cred, config_ref));
                    has_new_machine_ids = true;
                }
                Ok(CredentialEntry {
                    id,
                    credentials: cred.clone(),
                    failure_count: 0,
                    refresh_failure_count: 0,
                    disabled: cred.disabled, // 从配置文件读取 disabled 状态
                    disabled_reason: if cred.disabled {
                        Some(DisabledReason::Manual)
                    } else {
                        None
                    },
                    success_count: 0,
                    in_flight: Arc::new(AtomicU64::new(0)),
                    refresh_lock: Arc::new(TokioMutex::new(())),
                    cooldown: None,
                    adaptive_limit: concurrency::AdaptiveLimit::new(
                        config.concurrency.max_in_flight_per_credential,
                    ),
                    last_used_at: None,
                })
            })
            .collect::<anyhow::Result<_>>()?;

        // 校验 API Key 凭据配置完整性：authMethod=api_key 时必须提供 kiroApiKey
        let mut entries = entries;
        for entry in &mut entries {
            if entry.credentials.kiro_api_key.is_none()
                && entry
                    .credentials
                    .auth_method
                    .as_deref()
                    .map(|m| m.eq_ignore_ascii_case("api_key") || m.eq_ignore_ascii_case("apikey"))
                    .unwrap_or(false)
            {
                tracing::warn!(
                    "凭据 #{} 配置了 authMethod=api_key 但缺少 kiroApiKey 字段，已自动禁用",
                    entry.id
                );
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::InvalidConfig);
            }
        }

        // 检测重复 ID
        let mut seen_ids = std::collections::HashSet::new();
        let mut duplicate_ids = Vec::new();
        for entry in &entries {
            if !seen_ids.insert(entry.id) {
                duplicate_ids.push(entry.id);
            }
        }
        if !duplicate_ids.is_empty() {
            anyhow::bail!("检测到重复的凭据 ID: {:?}", duplicate_ids);
        }

        // 选择初始凭据：优先级最高（priority 最小）的可用凭据，无可用凭据时为 0
        let initial_id = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| e.credentials.priority)
            .map(|e| e.id)
            .unwrap_or(0);

        let load_balancing_mode = config.load_balancing_mode.clone();
        let in_flight_slots = Arc::new(Semaphore::new(config.concurrency.max_in_flight));
        let waiting_slots = Semaphore::new(config.concurrency.max_waiting);
        let manager = Self {
            config,
            proxy,
            entries: Mutex::new(entries),
            last_assigned_id: AtomicU64::new(last_assigned_id),
            current_id: Mutex::new(initial_id),
            in_flight_slots,
            waiting_slots,
            wait_queue: Mutex::new(Vec::new()),
            concurrency_metrics: ConcurrencyMetrics::default(),
            capacity_changed: Arc::new(Notify::new()),
            credentials_path,
            is_multiple_format: AtomicBool::new(is_multiple_format),
            persistence_lock: Mutex::new(()),
            load_balancing_mode: Mutex::new(load_balancing_mode),
            last_stats_save_at: Mutex::new(None),
            stats_persistence_lock: Mutex::new(()),
            stats_dirty: AtomicBool::new(false),
        };

        // 如果有新分配的 ID 或新生成的 machineId，立即持久化到配置文件
        if has_new_ids || has_new_machine_ids {
            if let Err(e) = manager.persist_credentials() {
                tracing::warn!("补全凭据 ID/machineId 后持久化失败: {}", e);
            } else {
                tracing::info!("已补全凭据 ID/machineId 并写回配置文件");
            }
        }

        // 加载持久化的统计数据（success_count, last_used_at）
        manager.load_stats();

        Ok(manager)
    }

    /// 获取配置的引用
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 获取凭据总数
    pub fn total_count(&self) -> usize {
        self.entries.lock().len()
    }

    /// 余额缓存绑定到实际认证凭据，避免重启后复用 ID 时命中旧账户。
    /// 优先使用 refreshToken；仅有 accessToken 时使用其哈希，不暴露原始密钥。
    pub fn credential_cache_key(&self, id: u64) -> Option<String> {
        let entries = self.entries.lock();
        let credentials = &entries.iter().find(|entry| entry.id == id)?.credentials;
        if credentials.is_api_key_credential() {
            Some(format!(
                "api_key:{}",
                sha256_hex(credentials.kiro_api_key.as_deref().unwrap_or_default())
            ))
        } else if let Some(token) = credentials
            .refresh_token
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            Some(format!("oauth:{}", sha256_hex(token)))
        } else {
            Some(format!(
                "oauth_access:{}",
                sha256_hex(credentials.access_token.as_deref().unwrap_or_default())
            ))
        }
    }

    /// 获取可用凭据数量
    pub fn available_count(&self) -> usize {
        self.entries.lock().iter().filter(|e| !e.disabled).count()
    }

    fn requires_opus(model: Option<&str>) -> bool {
        model.is_some_and(|name| name.to_ascii_lowercase().contains("opus"))
    }

    /// 复用当前凭据和重新选择凭据时使用相同的模型资格检查。
    fn eligible_for_model(entry: &CredentialEntry, model: Option<&str>) -> bool {
        !entry.disabled && (!Self::requires_opus(model) || entry.credentials.supports_opus())
    }

    fn dispatchable(&self, entry: &CredentialEntry, model: Option<&str>, now: Instant) -> bool {
        Self::eligible_for_model(entry, model)
            && entry.in_flight.load(Ordering::Relaxed) < self.effective_concurrency_limit(entry)
            && entry.cooldown.is_none_or(|(started, duration)| {
                now.saturating_duration_since(started) >= duration
            })
    }

    /// 检查资格、容量、冷却并预留名额都在同一临界区内完成。
    fn try_reserve_credential(
        &self,
        model: Option<&str>,
        earlier_waiters: &[Waiter],
    ) -> anyhow::Result<(Option<ReservedCredential>, Option<Instant>)> {
        let mut entries = self.entries.lock();
        if !entries.iter().any(|e| Self::eligible_for_model(e, model)) {
            // 仅在没有合格账号时自愈，容量已满/冷却不会重启失败账号。
            for entry in entries
                .iter_mut()
                .filter(|e| e.disabled_reason == Some(DisabledReason::TooManyFailures))
            {
                entry.disabled = false;
                entry.disabled_reason = None;
                entry.failure_count = 0;
            }
        }
        let eligible = entries
            .iter()
            .filter(|e| Self::eligible_for_model(e, model));
        if eligible.clone().next().is_none() {
            let available = entries.iter().filter(|e| !e.disabled).count();
            if available > 0 {
                anyhow::bail!(
                    "没有支持所请求模型的可用凭据（{}/{}）",
                    available,
                    entries.len()
                );
            }
            anyhow::bail!("所有凭据均已禁用（0/{}）", entries.len());
        }

        let now = Instant::now();
        let ready_at = eligible
            .clone()
            .filter_map(|e| {
                e.cooldown.and_then(|(started, duration)| {
                    (now.duration_since(started) < duration)
                        .then(|| started.checked_add(duration))
                        .flatten()
                })
            })
            .min();
        // 按到达顺序服务可调度的等待者；模型不匹配/账号满载/冷却不阻塞后续请求。
        if self.in_flight_slots.available_permits() == 0 {
            return Ok((None, ready_at));
        }
        if !earlier_waiters.is_empty() {
            let has_general_waiter = earlier_waiters.iter().any(|waiter| !waiter.requires_opus);
            if entries.iter().any(|entry| {
                self.dispatchable(entry, None, now)
                    && (has_general_waiter || entry.credentials.supports_opus())
            }) {
                return Ok((None, ready_at));
            }
        }
        let balanced = self.load_balancing_mode.lock().as_str() == "balanced";
        let current_id = *self.current_id.lock();
        let best = eligible
            .filter(|e| self.dispatchable(e, model, now))
            .min_by_key(|e| {
                if balanced {
                    (
                        0,
                        e.in_flight.load(Ordering::Relaxed),
                        e.success_count,
                        e.credentials.priority,
                    )
                } else {
                    // 保留 priority 对当前账号的偏好；满载/冷却时使用其他账号。
                    (u8::from(e.id != current_id), 0, 0, e.credentials.priority)
                }
            });
        let Some(entry) = best else {
            return Ok((None, ready_at));
        };
        let Ok(global_slot) = self.in_flight_slots.clone().try_acquire_owned() else {
            return Ok((None, ready_at));
        };
        *self.current_id.lock() = entry.id;
        Ok((
            Some(ReservedCredential {
                id: entry.id,
                credentials: entry.credentials.clone(),
                reservation: InFlightReservation::reserve(
                    entry,
                    global_slot,
                    self.capacity_changed.clone(),
                ),
            }),
            ready_at,
        ))
    }

    /// 供无实际调用上下文的调度测试使用。
    #[cfg(test)]
    pub fn report_rate_limited(&self, id: u64, retry_after: Option<StdDuration>) {
        self.report_rate_limited_inner(id, None, retry_after);
    }

    /// 429 触发账号冷却；同一批在途调用只执行一次自适应降档。
    pub(crate) fn report_rate_limited_for_context(
        &self,
        context: &CallContext,
        retry_after: Option<StdDuration>,
    ) {
        let epoch = context
            .reservation
            .as_ref()
            .map(|reservation| &reservation.adaptive_epoch);
        self.report_rate_limited_inner(context.id, epoch, retry_after);
    }

    fn report_rate_limited_inner(
        &self,
        id: u64,
        epoch: Option<&Arc<()>>,
        retry_after: Option<StdDuration>,
    ) {
        let duration = retry_after.unwrap_or_default().max(StdDuration::from_secs(
            self.config.concurrency.rate_limit_cooldown_seconds,
        ));
        self.concurrency_metrics
            .rate_limited_total
            .fetch_add(1, Ordering::Relaxed);
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
            if self.config.concurrency.adaptive_concurrency
                && epoch.is_none_or(|epoch| Arc::ptr_eq(epoch, &entry.adaptive_limit.epoch))
            {
                if entry.adaptive_limit.rate_limited() {
                    self.concurrency_metrics
                        .adaptive_reduction_total
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            let remaining = entry
                .cooldown
                .map(|(started, duration)| duration.saturating_sub(started.elapsed()))
                .unwrap_or_default();
            entry.cooldown = Some((Instant::now(), duration.max(remaining)));
        }
        drop(entries);
        self.capacity_changed.notify_waiters();
    }

    fn refresh_lock_for(&self, id: u64) -> anyhow::Result<Arc<TokioMutex<()>>> {
        self.entries
            .lock()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.refresh_lock.clone())
            .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))
    }

    /// 获取 API 调用上下文
    ///
    /// 返回绑定了 id、credentials 和 token 的调用上下文
    /// 确保整个 API 调用过程中使用一致的凭据信息
    ///
    /// 如果 Token 过期或即将过期，会自动刷新
    /// Token 刷新失败会累计到当前凭据，达到阈值后禁用并切换
    ///
    /// # 参数
    /// - `model`: 可选的模型名称，用于过滤支持该模型的凭据（如 opus 模型需要付费订阅）
    pub async fn acquire_context(&self, model: Option<&str>) -> anyhow::Result<CallContext> {
        let total = self.total_count();
        let max_attempts = (total * MAX_FAILURES_PER_CREDENTIAL as usize).max(1);
        let mut attempt_count = 0;

        loop {
            if attempt_count >= max_attempts {
                anyhow::bail!(
                    "所有凭据均无法获取有效 Token（可用: {}/{}）",
                    self.available_count(),
                    total
                );
            }

            let ReservedCredential {
                id,
                credentials,
                reservation,
            } = self.reserve_credential(model).await?;

            // 尝试获取/刷新 Token
            match self.try_ensure_token(id, &credentials).await {
                Ok(mut ctx) => {
                    ctx.reservation = Some(reservation);
                    return Ok(ctx);
                }
                Err(e) => {
                    // refreshToken 永久失效 → 立即禁用，不累计重试
                    let has_available = if e.downcast_ref::<RefreshTokenInvalidError>().is_some() {
                        tracing::warn!("凭据 #{} refreshToken 永久失效: {}", id, e);
                        self.report_refresh_token_invalid(id)
                    } else {
                        tracing::warn!("凭据 #{} Token 刷新失败: {}", id, e);
                        self.report_refresh_failure(id)
                    };
                    attempt_count += 1;
                    if !has_available {
                        anyhow::bail!("所有凭据均已禁用（0/{}）", total);
                    }
                }
            }
        }
    }

    /// 选择优先级最高的未禁用凭据作为当前凭据（内部方法）
    ///
    /// 纯粹按优先级选择，不排除当前凭据，用于优先级变更后立即生效
    fn select_highest_priority(&self) {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（不排除当前凭据）
        if let Some(best) = entries
            .iter()
            .filter(|e| !e.disabled)
            .min_by_key(|e| e.credentials.priority)
            && best.id != *current_id
        {
            tracing::info!(
                "优先级变更后切换凭据: #{} -> #{}（优先级 {}）",
                *current_id,
                best.id,
                best.credentials.priority
            );
            *current_id = best.id;
        }
    }

    /// 尝试使用指定凭据获取有效 Token
    ///
    /// 使用双重检查锁定模式，确保同一时间只有一个刷新操作
    ///
    /// # Arguments
    /// * `id` - 凭据 ID，用于更新正确的条目
    /// * `credentials` - 凭据信息
    async fn try_ensure_token(
        &self,
        id: u64,
        credentials: &KiroCredentials,
    ) -> anyhow::Result<CallContext> {
        // API Key 凭据直接使用 kiro_api_key 作为 Bearer Token，无需刷新
        if credentials.is_api_key_credential() {
            let token = credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            return Ok(CallContext {
                id,
                credentials: credentials.clone(),
                token,
                reservation: None,
            });
        }

        // 第一次检查（无锁）：快速判断是否需要刷新
        let needs_refresh = is_token_expired(credentials) || is_token_expiring_soon(credentials);

        let creds = if needs_refresh {
            // 获取刷新锁，确保同一时间只有一个刷新操作
            let refresh_lock = self.refresh_lock_for(id)?;
            let _guard = refresh_lock.lock().await;

            // 第二次检查：获取锁后重新读取凭据，因为其他请求可能已经完成刷新
            let current_creds = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.credentials.clone())
                    .ok_or_else(|| anyhow::anyhow!("凭据 #{} 不存在", id))?
            };

            if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                // 确实需要刷新
                let effective_proxy = current_creds.effective_proxy(self.proxy.as_ref());
                let new_creds =
                    refresh_token(&current_creds, &self.config, effective_proxy.as_ref()).await?;

                if is_token_expired(&new_creds) {
                    anyhow::bail!("刷新后的 Token 仍然无效或已过期");
                }

                // 更新凭据
                {
                    let mut entries = self.entries.lock();
                    if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                        entry.credentials = new_creds.clone();
                    }
                }

                // 回写凭据到文件，失败只记录警告
                if let Err(e) = self.persist_credentials() {
                    tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                }

                new_creds
            } else {
                // 其他请求已经完成刷新，直接使用新凭据
                tracing::debug!("Token 已被其他请求刷新，跳过刷新");
                current_creds
            }
        } else {
            credentials.clone()
        };

        let token = creds
            .access_token
            .clone()
            .ok_or_else(|| anyhow::anyhow!("没有可用的 accessToken"))?;

        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.refresh_failure_count = 0;
            }
        }

        Ok(CallContext {
            id,
            credentials: creds,
            token,
            reservation: None,
        })
    }

    /// 回写凭据，保留单对象格式；新增或删空时升级为数组格式。
    /// 无文件路径时返回 Ok(false)。快照和原子替换在同一写锁内完成。
    fn persist_credentials(&self) -> anyhow::Result<bool> {
        let Some(path) = &self.credentials_path else {
            return Ok(false);
        };
        let persist = || {
            let _guard = self.persistence_lock.lock();
            let credentials: Vec<KiroCredentials> = {
                let entries = self.entries.lock();
                entries
                    .iter()
                    .map(|entry| {
                        let mut credential = entry.credentials.clone();
                        credential.canonicalize_auth_method();
                        credential.disabled = entry.disabled;
                        credential
                    })
                    .collect()
            };
            let multiple =
                self.is_multiple_format.load(Ordering::Relaxed) || credentials.len() != 1;
            let json = if multiple {
                serde_json::to_vec_pretty(&credentials)?
            } else {
                serde_json::to_vec_pretty(&credentials[0])?
            };
            atomic_write_credentials(path, &json)?;
            self.is_multiple_format.store(multiple, Ordering::Relaxed);
            tracing::debug!("已回写凭据到文件: {:?}", path);
            Ok(true)
        };
        // block_in_place 仅适用于多线程 runtime；同步和单线程调用也应能保存。
        if tokio::runtime::Handle::try_current().is_ok_and(|handle| {
            handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        }) {
            tokio::task::block_in_place(persist)
        } else {
            persist()
        }
    }

    /// 获取缓存目录（凭据文件所在目录）
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.credentials_path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    /// 统计数据文件路径
    fn stats_path(&self) -> Option<PathBuf> {
        self.cache_dir().map(|d| d.join("kiro_stats.json"))
    }

    /// 从磁盘加载统计数据并应用到当前条目
    fn load_stats(&self) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return, // 首次运行时文件不存在
        };

        let stats: HashMap<String, StatsEntry> = match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("解析统计缓存失败，将忽略: {}", e);
                return;
            }
        };

        let mut entries = self.entries.lock();
        for entry in entries.iter_mut() {
            if let Some(s) = stats.get(&entry.id.to_string()) {
                entry.success_count = s.success_count;
                entry.last_used_at = s.last_used_at.clone();
            }
        }
        *self.last_stats_save_at.lock() = Some(Instant::now());
        self.stats_dirty.store(false, Ordering::Relaxed);
        tracing::info!("已从缓存加载 {} 条统计数据", stats.len());
    }

    /// 将当前统计数据持久化到磁盘
    fn save_stats(&self) {
        self.save_stats_inner(false);
    }

    fn save_stats_inner(&self, debounced: bool) {
        let path = match self.stats_path() {
            Some(p) => p,
            None => return,
        };

        let _guard = self.stats_persistence_lock.lock();
        if debounced
            && self
                .last_stats_save_at
                .lock()
                .is_some_and(|last| last.elapsed() < STATS_SAVE_DEBOUNCE)
        {
            return;
        }
        // 必须在快照之前清理标记；写入期间产生的新更新继续保留 dirty。
        self.stats_dirty.store(false, Ordering::Relaxed);

        let stats: HashMap<String, StatsEntry> = {
            let entries = self.entries.lock();
            entries
                .iter()
                .map(|e| {
                    (
                        e.id.to_string(),
                        StatsEntry {
                            success_count: e.success_count,
                            last_used_at: e.last_used_at.clone(),
                        },
                    )
                })
                .collect()
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                if let Err(e) = atomic_write_credentials(&path, json.as_bytes()) {
                    self.stats_dirty.store(true, Ordering::Relaxed);
                    tracing::warn!("保存统计缓存失败: {}", e);
                } else {
                    *self.last_stats_save_at.lock() = Some(Instant::now());
                }
            }
            Err(e) => {
                self.stats_dirty.store(true, Ordering::Relaxed);
                tracing::warn!("序列化统计数据失败: {}", e);
            }
        }
    }

    /// 标记统计数据已更新，并按 debounce 策略决定是否立即落盘
    fn save_stats_debounced(&self) {
        self.stats_dirty.store(true, Ordering::Relaxed);

        self.save_stats_inner(true);
    }

    /// 报告指定凭据 API 调用成功
    ///
    /// 重置该凭据的失败计数
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_success(&self, id: u64) {
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.success_count = entry.success_count.saturating_add(1);
                entry.last_used_at = Some(Utc::now().to_rfc3339());
                tracing::debug!(
                    "凭据 #{} API 调用成功（累计 {} 次）",
                    id,
                    entry.success_count
                );
            }
        }
        self.save_stats_debounced();
    }

    /// 报告指定凭据 API 调用失败
    ///
    /// 增加失败计数，达到阈值时禁用凭据并切换到优先级最高的可用凭据
    /// 返回是否还有可用凭据可以重试
    ///
    /// # Arguments
    /// * `id` - 凭据 ID（来自 CallContext）
    pub fn report_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.failure_count += 1;
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            let failure_count = entry.failure_count;

            tracing::warn!(
                "凭据 #{} API 调用失败（{}/{}）",
                id,
                failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if failure_count >= MAX_FAILURES_PER_CREDENTIAL {
                entry.disabled = true;
                entry.disabled_reason = Some(DisabledReason::TooManyFailures);
                tracing::error!("凭据 #{} 已连续失败 {} 次，已被禁用", id, failure_count);

                // 切换到优先级最高的可用凭据
                if let Some(next) = entries
                    .iter()
                    .filter(|e| !e.disabled)
                    .min_by_key(|e| e.credentials.priority)
                {
                    *current_id = next.id;
                    tracing::info!(
                        "已切换到凭据 #{}（优先级 {}）",
                        next.id,
                        next.credentials.priority
                    );
                } else {
                    tracing::error!("所有凭据均已禁用！");
                }
            }

            entries.iter().any(|e| !e.disabled)
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据额度已用尽
    ///
    /// 用于处理 402 Payment Required 且 reason 为 `MONTHLY_REQUEST_COUNT` 的场景：
    /// - 立即禁用该凭据（不等待连续失败阈值）
    /// - 切换到下一个可用凭据继续重试
    /// - 返回是否还有可用凭据
    pub fn report_quota_exhausted(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::QuotaExceeded);
            entry.last_used_at = Some(Utc::now().to_rfc3339());
            // 设为阈值，便于在管理面板中直观看到该凭据已不可用
            entry.failure_count = MAX_FAILURES_PER_CREDENTIAL;

            tracing::error!("凭据 #{} 额度已用尽（MONTHLY_REQUEST_COUNT），已被禁用", id);

            // 切换到优先级最高的可用凭据
            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据刷新 Token 失败。
    ///
    /// 连续刷新失败达到阈值后禁用凭据并切换，阈值内保持当前凭据不切换，
    /// 与 API 401/403 的累计失败策略保持一致。
    pub fn report_refresh_failure(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.refresh_failure_count += 1;
            let refresh_failure_count = entry.refresh_failure_count;

            tracing::warn!(
                "凭据 #{} Token 刷新失败（{}/{}）",
                id,
                refresh_failure_count,
                MAX_FAILURES_PER_CREDENTIAL
            );

            if refresh_failure_count < MAX_FAILURES_PER_CREDENTIAL {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::TooManyRefreshFailures);

            tracing::error!(
                "凭据 #{} Token 已连续刷新失败 {} 次，已被禁用",
                id,
                refresh_failure_count
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 报告指定凭据的 refreshToken 永久失效（invalid_grant）。
    ///
    /// 立即禁用凭据，不累计、不重试。
    /// 返回是否还有可用凭据。
    pub fn report_refresh_token_invalid(&self, id: u64) -> bool {
        let result = {
            let mut entries = self.entries.lock();
            let mut current_id = self.current_id.lock();

            let entry = match entries.iter_mut().find(|e| e.id == id) {
                Some(e) => e,
                None => return entries.iter().any(|e| !e.disabled),
            };

            if entry.disabled {
                return entries.iter().any(|e| !e.disabled);
            }

            entry.last_used_at = Some(Utc::now().to_rfc3339());
            entry.disabled = true;
            entry.disabled_reason = Some(DisabledReason::InvalidRefreshToken);

            tracing::error!(
                "凭据 #{} refreshToken 已失效 (invalid_grant)，已立即禁用",
                id
            );

            if let Some(next) = entries
                .iter()
                .filter(|e| !e.disabled)
                .min_by_key(|e| e.credentials.priority)
            {
                *current_id = next.id;
                tracing::info!(
                    "已切换到凭据 #{}（优先级 {}）",
                    next.id,
                    next.credentials.priority
                );
                true
            } else {
                tracing::error!("所有凭据均已禁用！");
                false
            }
        };
        self.save_stats_debounced();
        result
    }

    /// 切换到优先级最高的可用凭据
    ///
    /// 返回是否成功切换
    pub fn switch_to_next(&self) -> bool {
        let entries = self.entries.lock();
        let mut current_id = self.current_id.lock();

        // 选择优先级最高的未禁用凭据（排除当前凭据）
        if let Some(next) = entries
            .iter()
            .filter(|e| !e.disabled && e.id != *current_id)
            .min_by_key(|e| e.credentials.priority)
        {
            *current_id = next.id;
            tracing::info!(
                "已切换到凭据 #{}（优先级 {}）",
                next.id,
                next.credentials.priority
            );
            true
        } else {
            // 没有其他可用凭据，检查当前凭据是否可用
            entries.iter().any(|e| e.id == *current_id && !e.disabled)
        }
    }

    // ========================================================================
    // Admin API 方法
    // ========================================================================

    /// 获取管理器状态快照（用于 Admin API）
    pub fn snapshot(&self) -> ManagerSnapshot {
        let queue = self.wait_queue.lock();
        let entries = self.entries.lock();
        let current_id = *self.current_id.lock();
        let available = entries.iter().filter(|e| !e.disabled).count();

        ManagerSnapshot {
            concurrency: self.concurrency_snapshot(&queue, &entries),
            entries: entries
                .iter()
                .map(|e| CredentialEntrySnapshot {
                    id: e.id,
                    priority: e.credentials.priority,
                    disabled: e.disabled,
                    failure_count: e.failure_count,
                    auth_method: if e.credentials.is_api_key_credential() {
                        Some("api_key".to_string())
                    } else {
                        e.credentials.auth_method.as_deref().map(|m| {
                            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam")
                            {
                                "idc".to_string()
                            } else {
                                m.to_string()
                            }
                        })
                    },
                    has_profile_arn: e.credentials.profile_arn.is_some(),
                    expires_at: if e.credentials.is_api_key_credential() {
                        None // API Key 凭据本地不维护过期时间（服务端策略未知）
                    } else {
                        e.credentials.expires_at.clone()
                    },
                    refresh_token_hash: if e.credentials.is_api_key_credential() {
                        None
                    } else {
                        e.credentials.refresh_token.as_deref().map(sha256_hex)
                    },
                    api_key_hash: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(sha256_hex)
                    } else {
                        None
                    },
                    masked_api_key: if e.credentials.is_api_key_credential() {
                        e.credentials.kiro_api_key.as_deref().map(mask_api_key)
                    } else {
                        None
                    },
                    email: e.credentials.email.clone(),
                    success_count: e.success_count,
                    in_flight: e.in_flight.load(Ordering::Relaxed),
                    cooldown_remaining_ms: concurrency::cooldown_remaining_ms(e),
                    effective_concurrency_limit: self.effective_concurrency_limit(e),
                    adaptive_probe_target: self
                        .config
                        .concurrency
                        .adaptive_concurrency
                        .then(|| e.adaptive_limit.probe_target())
                        .flatten(),
                    last_used_at: e.last_used_at.clone(),
                    has_proxy: e.credentials.proxy_url.is_some(),
                    proxy_url: e.credentials.proxy_url.clone(),
                    refresh_failure_count: e.refresh_failure_count,
                    disabled_reason: e.disabled_reason.map(|r| {
                        match r {
                            DisabledReason::Manual => "Manual",
                            DisabledReason::TooManyFailures => "TooManyFailures",
                            DisabledReason::TooManyRefreshFailures => "TooManyRefreshFailures",
                            DisabledReason::QuotaExceeded => "QuotaExceeded",
                            DisabledReason::InvalidRefreshToken => "InvalidRefreshToken",
                            DisabledReason::InvalidConfig => "InvalidConfig",
                        }
                        .to_string()
                    }),
                    endpoint: e.credentials.endpoint.clone(),
                })
                .collect(),
            current_id,
            total: entries.len(),
            available,
        }
    }

    /// 设置凭据禁用状态（Admin API）
    pub fn set_disabled(&self, id: u64, disabled: bool) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.disabled = disabled;
            if !disabled {
                // 启用时重置失败计数
                entry.failure_count = 0;
                entry.refresh_failure_count = 0;
                entry.disabled_reason = None;
            } else {
                entry.disabled_reason = Some(DisabledReason::Manual);
            }
        }
        self.capacity_changed.notify_waiters();
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 设置凭据优先级（Admin API）
    ///
    /// 修改优先级后会立即按新优先级重新选择当前凭据。
    /// 即使持久化失败，内存中的优先级和当前凭据选择也会生效。
    pub fn set_priority(&self, id: u64, priority: u32) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            entry.credentials.priority = priority;
        }
        // 立即按新优先级重新选择当前凭据（无论持久化是否成功）
        self.select_highest_priority();
        self.capacity_changed.notify_waiters();
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 重置凭据失败计数并重新启用（Admin API）
    pub fn reset_and_enable(&self, id: u64) -> anyhow::Result<()> {
        {
            let mut entries = self.entries.lock();
            let entry = entries
                .iter_mut()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;
            if entry.disabled_reason == Some(DisabledReason::InvalidConfig) {
                anyhow::bail!("凭据 #{} 因配置无效被禁用，请修正配置后重启服务", id);
            }
            entry.failure_count = 0;
            entry.refresh_failure_count = 0;
            entry.disabled = false;
            entry.disabled_reason = None;
        }
        self.capacity_changed.notify_waiters();
        // 持久化更改
        self.persist_credentials()?;
        Ok(())
    }

    /// 获取指定凭据的使用额度（Admin API）
    pub async fn get_usage_limits_for(&self, id: u64) -> anyhow::Result<UsageLimitsResponse> {
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // API Key 凭据直接使用 kiro_api_key，无需刷新
        let token = if credentials.is_api_key_credential() {
            credentials
                .kiro_api_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?
        } else {
            // 检查是否需要刷新 token
            let needs_refresh =
                is_token_expired(&credentials) || is_token_expiring_soon(&credentials);

            if needs_refresh {
                let refresh_lock = self.refresh_lock_for(id)?;
                let _guard = refresh_lock.lock().await;
                let current_creds = {
                    let entries = self.entries.lock();
                    entries
                        .iter()
                        .find(|e| e.id == id)
                        .map(|e| e.credentials.clone())
                        .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
                };

                if is_token_expired(&current_creds) || is_token_expiring_soon(&current_creds) {
                    let effective_proxy = current_creds.effective_proxy(self.proxy.as_ref());
                    let new_creds =
                        refresh_token(&current_creds, &self.config, effective_proxy.as_ref())
                            .await?;
                    {
                        let mut entries = self.entries.lock();
                        if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                            entry.credentials = new_creds.clone();
                        }
                    }
                    // 持久化失败只记录警告，不影响本次请求
                    if let Err(e) = self.persist_credentials() {
                        tracing::warn!("Token 刷新后持久化失败（不影响本次请求）: {}", e);
                    }
                    new_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("刷新后无 access_token"))?
                } else {
                    current_creds
                        .access_token
                        .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
                }
            } else {
                credentials
                    .access_token
                    .ok_or_else(|| anyhow::anyhow!("凭据无 access_token"))?
            }
        };

        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let usage_limits =
            get_usage_limits(&credentials, &self.config, &token, effective_proxy.as_ref()).await?;

        // 更新订阅等级到凭据（仅在发生变化时持久化）
        if let Some(subscription_title) = usage_limits.subscription_title() {
            let changed = {
                let mut entries = self.entries.lock();
                if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                    let old_title = entry.credentials.subscription_title.clone();
                    if old_title.as_deref() != Some(subscription_title) {
                        entry.credentials.subscription_title = Some(subscription_title.to_string());
                        tracing::info!(
                            "凭据 #{} 订阅等级已更新: {:?} -> {}",
                            id,
                            old_title,
                            subscription_title
                        );
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if changed && let Err(e) = self.persist_credentials() {
                tracing::warn!("订阅等级更新后持久化失败（不影响本次请求）: {}", e);
            }
        }

        Ok(usage_limits)
    }

    /// 添加新凭据（Admin API）
    ///
    /// # 流程
    /// 1. 验证凭据基本字段（API Key: kiroApiKey 不为空; OAuth: refreshToken 不为空）
    /// 2. 基于 kiroApiKey 或 refreshToken 的 SHA-256 哈希检测重复
    /// 3. OAuth: 尝试刷新 Token 验证凭据有效性; API Key: 跳过
    /// 4. 分配新 ID（当前最大 ID + 1）
    /// 5. 添加到 entries 列表
    /// 6. 持久化到配置文件
    ///
    /// # 返回
    /// - `Ok(u64)` - 新凭据 ID
    /// - `Err(_)` - 验证失败或添加失败
    pub async fn add_credential(&self, new_cred: KiroCredentials) -> anyhow::Result<u64> {
        // 1. 基本验证
        if new_cred.is_api_key_credential() {
            let api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少 kiroApiKey"))?;
            if api_key.is_empty() {
                anyhow::bail!("kiroApiKey 为空");
            }
        } else {
            validate_refresh_token(&new_cred)?;
        }

        // 2. 基于哈希检测重复
        if new_cred.is_api_key_credential() {
            let new_api_key = new_cred
                .kiro_api_key
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 kiroApiKey"))?;
            let new_api_key_hash = sha256_hex(new_api_key);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .kiro_api_key
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_api_key_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（kiroApiKey 重复）");
            }
        } else {
            let new_refresh_token = new_cred
                .refresh_token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("缺少 refreshToken"))?;
            let new_refresh_token_hash = sha256_hex(new_refresh_token);
            let duplicate_exists = {
                let entries = self.entries.lock();
                entries.iter().any(|entry| {
                    entry
                        .credentials
                        .refresh_token
                        .as_deref()
                        .map(sha256_hex)
                        .as_deref()
                        == Some(new_refresh_token_hash.as_str())
                })
            };
            if duplicate_exists {
                anyhow::bail!("凭据已存在（refreshToken 重复）");
            }
        }

        // 3. 验证凭据有效性（API Key 无需网络刷新）
        let mut validated_cred = if new_cred.is_api_key_credential() {
            new_cred.clone()
        } else {
            let effective_proxy = new_cred.effective_proxy(self.proxy.as_ref());
            refresh_token(&new_cred, &self.config, effective_proxy.as_ref()).await?
        };

        // 保留用户输入的元数据，ID 在插入时分配。
        validated_cred.priority = new_cred.priority;
        validated_cred.auth_method = new_cred.auth_method.map(|m| {
            if m.eq_ignore_ascii_case("builder-id") || m.eq_ignore_ascii_case("iam") {
                "idc".to_string()
            } else {
                m
            }
        });
        validated_cred.client_id = new_cred.client_id;
        validated_cred.client_secret = new_cred.client_secret;
        validated_cred.region = new_cred.region;
        validated_cred.auth_region = new_cred.auth_region;
        validated_cred.api_region = new_cred.api_region;
        validated_cred.machine_id = new_cred.machine_id;
        validated_cred.email = new_cred.email;
        validated_cred.proxy_url = new_cred.proxy_url;
        validated_cred.proxy_username = new_cred.proxy_username;
        validated_cred.proxy_password = new_cred.proxy_password;
        validated_cred.kiro_api_key = new_cred.kiro_api_key;

        let new_id = {
            let mut entries = self.entries.lock();
            // 网络验证期间其他请求可能已添加相同凭据，插入前再次检查。
            let duplicate = entries.iter().any(|entry| {
                if validated_cred.is_api_key_credential() {
                    entry.credentials.kiro_api_key == validated_cred.kiro_api_key
                } else {
                    entry.credentials.refresh_token == validated_cred.refresh_token
                }
            });
            if duplicate {
                anyhow::bail!("凭据已存在");
            }
            let new_id = self
                .last_assigned_id
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| anyhow::anyhow!("凭据 ID 已耗尽"))?
                + 1;
            validated_cred.id = Some(new_id);
            entries.push(CredentialEntry {
                id: new_id,
                credentials: validated_cred,
                failure_count: 0,
                refresh_failure_count: 0,
                disabled: false,
                disabled_reason: None,
                success_count: 0,
                in_flight: Arc::new(AtomicU64::new(0)),
                refresh_lock: Arc::new(TokioMutex::new(())),
                cooldown: None,
                adaptive_limit: concurrency::AdaptiveLimit::new(
                    self.config.concurrency.max_in_flight_per_credential,
                ),
                last_used_at: None,
            });
            new_id
        };

        self.capacity_changed.notify_waiters();
        // 6. 持久化
        self.persist_credentials()?;

        tracing::info!("成功添加凭据 #{}", new_id);
        Ok(new_id)
    }

    /// 删除凭据（Admin API）
    ///
    /// # 前置条件
    /// - 凭据必须已禁用（disabled = true）
    ///
    /// # 行为
    /// 1. 验证凭据存在
    /// 2. 验证凭据已禁用
    /// 3. 从 entries 移除
    /// 4. 如果删除的是当前凭据，切换到优先级最高的可用凭据
    /// 5. 如果删除后没有凭据，将 current_id 重置为 0
    /// 6. 持久化到文件
    ///
    /// # 返回
    /// - `Ok(())` - 删除成功
    /// - `Err(_)` - 凭据不存在、未禁用或持久化失败
    pub fn delete_credential(&self, id: u64) -> anyhow::Result<()> {
        let was_current = {
            let mut entries = self.entries.lock();

            // 查找凭据
            let entry = entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?;

            // 检查是否已禁用
            if !entry.disabled {
                anyhow::bail!("只能删除已禁用的凭据（请先禁用凭据 #{}）", id);
            }

            // 记录是否是当前凭据
            let current_id = *self.current_id.lock();
            let was_current = current_id == id;

            // 删除凭据
            entries.retain(|e| e.id != id);

            was_current
        };

        // 如果删除的是当前凭据，切换到优先级最高的可用凭据
        if was_current {
            self.select_highest_priority();
        }

        // 如果删除后没有任何凭据，将 current_id 重置为 0（与初始化行为保持一致）
        {
            let entries = self.entries.lock();
            if entries.is_empty() {
                let mut current_id = self.current_id.lock();
                *current_id = 0;
                tracing::info!("所有凭据已删除，current_id 已重置为 0");
            }
        }

        self.capacity_changed.notify_waiters();
        // 持久化更改
        self.persist_credentials()?;

        // 立即回写统计数据，清除已删除凭据的残留条目
        self.save_stats();

        tracing::info!("已删除凭据 #{}", id);
        Ok(())
    }

    /// 强制刷新指定凭据的 Token（Admin API）
    ///
    /// 无条件调用上游 API 重新获取 access token，不检查是否过期。
    /// 适用于排查问题、Token 异常但未过期、主动更新凭据状态等场景。
    pub async fn force_refresh_token_for(&self, id: u64) -> anyhow::Result<()> {
        let refresh_lock = self.refresh_lock_for(id)?;
        let _guard = refresh_lock.lock().await;
        let credentials = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|e| e.id == id)
                .map(|e| e.credentials.clone())
                .ok_or_else(|| anyhow::anyhow!("凭据不存在: {}", id))?
        };

        // 无条件调用 refresh_token
        let effective_proxy = credentials.effective_proxy(self.proxy.as_ref());
        let new_creds = refresh_token(&credentials, &self.config, effective_proxy.as_ref()).await?;

        // 更新 entries 中对应凭据
        {
            let mut entries = self.entries.lock();
            if let Some(entry) = entries.iter_mut().find(|e| e.id == id) {
                entry.credentials = new_creds;
                entry.refresh_failure_count = 0;
            }
        }

        // 持久化
        if let Err(e) = self.persist_credentials() {
            tracing::warn!("强制刷新 Token 后持久化失败: {}", e);
        }

        tracing::info!("凭据 #{} Token 已强制刷新", id);
        Ok(())
    }

    /// 获取负载均衡模式（Admin API）
    pub fn get_load_balancing_mode(&self) -> String {
        self.load_balancing_mode.lock().clone()
    }

    fn persist_load_balancing_mode(&self, mode: &str) -> anyhow::Result<()> {
        use anyhow::Context;

        let config_path = match self.config.config_path() {
            Some(path) => path.to_path_buf(),
            None => {
                tracing::warn!("配置文件路径未知，负载均衡模式仅在当前进程生效: {}", mode);
                return Ok(());
            }
        };

        let mut config = Config::load(&config_path)
            .with_context(|| format!("重新加载配置失败: {}", config_path.display()))?;
        config.load_balancing_mode = mode.to_string();
        config
            .save()
            .with_context(|| format!("持久化负载均衡模式失败: {}", config_path.display()))?;

        Ok(())
    }

    /// 设置负载均衡模式（Admin API）
    pub fn set_load_balancing_mode(&self, mode: String) -> anyhow::Result<()> {
        // 验证模式值
        if mode != "priority" && mode != "balanced" {
            anyhow::bail!("无效的负载均衡模式: {}", mode);
        }

        // 串行完成持久化和内存提交；写入失败时保留原运行模式。
        let mut current_mode = self.load_balancing_mode.lock();
        if *current_mode == mode {
            return Ok(());
        }

        self.persist_load_balancing_mode(&mode)?;
        *current_mode = mode.clone();

        tracing::info!("负载均衡模式已设置为: {}", mode);
        Ok(())
    }
}

impl Drop for MultiTokenManager {
    fn drop(&mut self) {
        if self.stats_dirty.load(Ordering::Relaxed) {
            self.save_stats();
        }
    }
}

#[cfg(test)]
#[path = "token_manager/concurrency_tests.rs"]
mod concurrency_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_token_expired_with_expired_token() {
        let credentials = KiroCredentials {
            expires_at: Some("2020-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_with_valid_token() {
        let mut credentials = KiroCredentials::default();
        let future = Utc::now() + Duration::hours(1);
        credentials.expires_at = Some(future.to_rfc3339());
        assert!(!is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_within_5_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(3);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expired_no_expires_at() {
        let credentials = KiroCredentials::default();
        assert!(is_token_expired(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_within_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(8);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_is_token_expiring_soon_beyond_10_minutes() {
        let mut credentials = KiroCredentials::default();
        let expires = Utc::now() + Duration::minutes(15);
        credentials.expires_at = Some(expires.to_rfc3339());
        assert!(!is_token_expiring_soon(&credentials));
    }

    #[test]
    fn test_validate_refresh_token_missing() {
        let credentials = KiroCredentials::default();
        let result = validate_refresh_token(&credentials);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_refresh_token_valid() {
        let credentials = KiroCredentials {
            refresh_token: Some("a".repeat(150)),
            ..Default::default()
        };
        let result = validate_refresh_token(&credentials);
        assert!(result.is_ok());
    }

    #[test]
    fn test_sha256_hex() {
        let result = sha256_hex("test");
        assert_eq!(
            result,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[tokio::test]
    async fn test_refresh_token_rejects_api_key_credential() {
        let config = Config::default();
        let credentials = KiroCredentials {
            kiro_api_key: Some("ksk_test_key_123".to_string()),
            auth_method: Some("api_key".to_string()),
            ..Default::default()
        };

        let result = refresh_token(&credentials, &config, None).await;

        assert!(result.is_err(), "API Key 凭据应被 refresh_token 拒绝");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("API Key 凭据不支持刷新"),
            "期望错误消息包含 'API Key 凭据不支持刷新'，实际: {}",
            err_msg
        );
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_refresh_token() {
        let config = Config::default();

        let existing = KiroCredentials {
            refresh_token: Some("a".repeat(150)),
            ..Default::default()
        };

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let duplicate = KiroCredentials {
            refresh_token: Some("a".repeat(150)),
            ..Default::default()
        };

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("凭据已存在"));
    }

    #[tokio::test]
    async fn test_add_credential_api_key_success() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let api_key_cred = KiroCredentials {
            kiro_api_key: Some("ksk_test_key_123".to_string()),
            auth_method: Some("api_key".to_string()),
            ..Default::default()
        };

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        let id = result.unwrap();
        assert!(id > 0);
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[tokio::test]
    async fn test_add_credential_reject_duplicate_api_key() {
        let config = Config::default();

        let existing = KiroCredentials {
            kiro_api_key: Some("ksk_existing_key".to_string()),
            auth_method: Some("api_key".to_string()),
            ..Default::default()
        };

        let manager = MultiTokenManager::new(config, vec![existing], None, None, false).unwrap();

        let duplicate = KiroCredentials {
            kiro_api_key: Some("ksk_existing_key".to_string()),
            auth_method: Some("api_key".to_string()),
            ..Default::default()
        };

        let result = manager.add_credential(duplicate).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("kiroApiKey 重复")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_empty_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let cred = KiroCredentials {
            kiro_api_key: Some(String::new()),
            auth_method: Some("api_key".to_string()),
            ..Default::default()
        };

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("kiroApiKey 为空")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_missing_key_rejected() {
        let config = Config::default();
        let manager = MultiTokenManager::new(config, vec![], None, None, false).unwrap();

        let cred = KiroCredentials {
            auth_method: Some("api_key".to_string()),
            ..Default::default()
        };
        // kiro_api_key is None

        let result = manager.add_credential(cred).await;
        assert!(result.is_err());
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("缺少 kiroApiKey")
        );
    }

    #[tokio::test]
    async fn test_add_credential_api_key_and_oauth_coexist() {
        let config = Config::default();

        let oauth_cred = KiroCredentials {
            refresh_token: Some("a".repeat(150)),
            ..Default::default()
        };

        let manager = MultiTokenManager::new(config, vec![oauth_cred], None, None, false).unwrap();

        let api_key_cred = KiroCredentials {
            kiro_api_key: Some("ksk_new_key".to_string()),
            auth_method: Some("api_key".to_string()),
            ..Default::default()
        };

        let result = manager.add_credential(api_key_cred).await;
        assert!(result.is_ok());
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    // MultiTokenManager 测试

    #[test]
    fn test_multi_token_manager_new() {
        let config = Config::default();
        let cred1 = KiroCredentials {
            priority: 0,
            ..Default::default()
        };
        let cred2 = KiroCredentials {
            priority: 1,
            ..Default::default()
        };

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 2);
    }

    #[test]
    fn test_multi_token_manager_empty_credentials() {
        let config = Config::default();
        let result = MultiTokenManager::new(config, vec![], None, None, false);
        // 支持 0 个凭据启动（可通过管理面板添加）
        assert!(result.is_ok());
        let manager = result.unwrap();
        assert_eq!(manager.total_count(), 0);
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_duplicate_ids() {
        let config = Config::default();
        let cred1 = KiroCredentials {
            id: Some(1),
            ..Default::default()
        };
        let cred2 = KiroCredentials {
            id: Some(1),
            ..Default::default()
        }; // 重复 ID

        let result = MultiTokenManager::new(config, vec![cred1, cred2], None, None, false);
        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("重复的凭据 ID"),
            "错误消息应包含 '重复的凭据 ID'，实际: {}",
            err_msg
        );
    }

    #[test]
    fn test_multi_token_manager_api_key_missing_kiro_api_key_auto_disabled() {
        let config = Config::default();

        // auth_method=api_key 但缺少 kiro_api_key → 应被自动禁用
        let bad_cred = KiroCredentials {
            auth_method: Some("api_key".to_string()),
            ..Default::default()
        };
        // kiro_api_key 保持 None

        let good_cred = KiroCredentials {
            refresh_token: Some("valid_token".to_string()),
            ..Default::default()
        };

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 2);
        assert_eq!(manager.available_count(), 1); // bad_cred 被禁用，只剩 1 个可用
    }

    #[test]
    fn test_multi_token_manager_api_key_with_kiro_api_key_not_disabled() {
        let config = Config::default();

        // auth_method=api_key 且有 kiro_api_key → 不应被禁用
        let cred = KiroCredentials {
            auth_method: Some("api_key".to_string()),
            kiro_api_key: Some("ksk_test123".to_string()),
            ..Default::default()
        };

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();
        assert_eq!(manager.total_count(), 1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_report_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        // 前两次失败不会禁用（使用 ID 1）
        assert!(manager.report_failure(1));
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 2);

        // 第三次失败会禁用第一个凭据
        assert!(manager.report_failure(1));
        assert_eq!(manager.available_count(), 1);

        // 继续失败第二个凭据（使用 ID 2）
        assert!(manager.report_failure(2));
        assert!(manager.report_failure(2));
        assert!(!manager.report_failure(2)); // 所有凭据都禁用了
        assert_eq!(manager.available_count(), 0);
    }

    #[test]
    fn test_multi_token_manager_report_success() {
        let config = Config::default();
        let cred = KiroCredentials::default();

        let manager = MultiTokenManager::new(config, vec![cred], None, None, false).unwrap();

        // 失败两次（使用 ID 1）
        manager.report_failure(1);
        manager.report_failure(1);

        // 成功后重置计数（使用 ID 1）
        manager.report_success(1);

        // 再失败两次不会禁用
        manager.report_failure(1);
        manager.report_failure(1);
        assert_eq!(manager.available_count(), 1);
    }

    #[test]
    fn test_multi_token_manager_switch_to_next() {
        let config = Config::default();
        let cred1 = KiroCredentials {
            refresh_token: Some("token1".to_string()),
            ..Default::default()
        };
        let cred2 = KiroCredentials {
            refresh_token: Some("token2".to_string()),
            ..Default::default()
        };

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        let initial_id = manager.snapshot().current_id;

        // 切换到下一个
        assert!(manager.switch_to_next());
        assert_ne!(manager.snapshot().current_id, initial_id);
    }

    #[test]
    fn test_set_load_balancing_mode_persists_to_config_file() {
        let config_path =
            std::env::temp_dir().join(format!("kiro-load-balancing-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&config_path, r#"{"loadBalancingMode":"priority"}"#).unwrap();

        let config = Config::load(&config_path).unwrap();
        let manager =
            MultiTokenManager::new(config, vec![KiroCredentials::default()], None, None, false)
                .unwrap();

        manager
            .set_load_balancing_mode("balanced".to_string())
            .unwrap();

        let persisted = Config::load(&config_path).unwrap();
        assert_eq!(persisted.load_balancing_mode, "balanced");
        assert_eq!(manager.get_load_balancing_mode(), "balanced");

        std::fs::remove_file(&config_path).unwrap();
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_auto_recovers_all_disabled() {
        let config = Config::default();
        let cred1 = KiroCredentials {
            access_token: Some("t1".to_string()),
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
            ..Default::default()
        };
        let cred2 = KiroCredentials {
            access_token: Some("t2".to_string()),
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
            ..Default::default()
        };

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(1);
        }
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(2);
        }

        assert_eq!(manager.available_count(), 0);

        // 应触发自愈：重置失败计数并重新启用，避免必须重启进程
        let ctx = manager.acquire_context(None).await.unwrap();
        assert!(ctx.token == "t1" || ctx.token == "t2");
        assert_eq!(manager.available_count(), 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_acquire_context_balanced_retries_until_bad_credential_disabled()
     {
        let mut config = Config::default();
        config.load_balancing_mode = "balanced".to_string();

        let bad_cred = KiroCredentials {
            priority: 0,
            refresh_token: Some("bad".to_string()),
            ..Default::default()
        };

        let good_cred = KiroCredentials {
            priority: 1,
            access_token: Some("good-token".to_string()),
            expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
            ..Default::default()
        };

        let manager =
            MultiTokenManager::new(config, vec![bad_cred, good_cred], None, None, false).unwrap();

        let ctx = manager.acquire_context(None).await.unwrap();
        assert_eq!(ctx.id, 2);
        assert_eq!(ctx.token, "good-token");
    }

    #[test]
    fn test_multi_token_manager_report_refresh_failure() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        assert_eq!(manager.available_count(), 2);
        for _ in 0..(MAX_FAILURES_PER_CREDENTIAL - 1) {
            assert!(manager.report_refresh_failure(1));
        }
        assert_eq!(manager.available_count(), 2);

        assert!(manager.report_refresh_failure(1));
        assert_eq!(manager.available_count(), 1);

        let snapshot = manager.snapshot();
        let first = snapshot.entries.iter().find(|e| e.id == 1).unwrap();
        assert!(first.disabled);
        assert_eq!(first.refresh_failure_count, MAX_FAILURES_PER_CREDENTIAL);
        assert_eq!(snapshot.current_id, 2);
    }

    #[tokio::test]
    async fn test_multi_token_manager_refresh_failure_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_refresh_failure(1);
            manager.report_refresh_failure(2);
        }
        assert_eq!(manager.available_count(), 0);

        let err = manager
            .acquire_context(None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
    }

    #[test]
    fn test_multi_token_manager_report_quota_exhausted() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        // 凭据会自动分配 ID（从 1 开始）
        assert_eq!(manager.available_count(), 2);
        assert!(manager.report_quota_exhausted(1));
        assert_eq!(manager.available_count(), 1);

        // 再禁用第二个后，无可用凭据
        assert!(!manager.report_quota_exhausted(2));
        assert_eq!(manager.available_count(), 0);
    }

    #[tokio::test]
    async fn test_multi_token_manager_quota_disabled_is_not_auto_recovered() {
        let config = Config::default();
        let cred1 = KiroCredentials::default();
        let cred2 = KiroCredentials::default();

        let manager =
            MultiTokenManager::new(config, vec![cred1, cred2], None, None, false).unwrap();

        manager.report_quota_exhausted(1);
        manager.report_quota_exhausted(2);
        assert_eq!(manager.available_count(), 0);

        let err = manager
            .acquire_context(None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("所有凭据均已禁用"),
            "错误应提示所有凭据禁用，实际: {}",
            err
        );
        assert_eq!(manager.available_count(), 0);
    }

    // ============ 凭据级 Region 优先级测试 ============

    #[test]
    fn test_credential_region_priority_uses_credential_auth_region() {
        // 凭据配置了 auth_region 时，应使用凭据的 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials {
            auth_region: Some("eu-west-1".to_string()),
            ..Default::default()
        };

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-west-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_credential_region() {
        // 凭据未配置 auth_region 但配置了 region 时，应回退到凭据.region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials {
            region: Some("eu-central-1".to_string()),
            ..Default::default()
        };

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "eu-central-1");
    }

    #[test]
    fn test_credential_region_priority_fallback_to_config() {
        // 凭据未配置 auth_region 和 region 时，应回退到 config
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials::default();
        assert!(credentials.auth_region.is_none());
        assert!(credentials.region.is_none());

        let region = credentials.effective_auth_region(&config);
        assert_eq!(region, "us-west-2");
    }

    #[test]
    fn test_multiple_credentials_use_respective_regions() {
        // 多凭据场景下，不同凭据使用各自的 auth_region
        let mut config = Config::default();
        config.region = "ap-northeast-1".to_string();

        let cred1 = KiroCredentials {
            auth_region: Some("us-east-1".to_string()),
            ..Default::default()
        };

        let cred2 = KiroCredentials {
            region: Some("eu-west-1".to_string()),
            ..Default::default()
        };

        let cred3 = KiroCredentials::default(); // 无 region，使用 config

        assert_eq!(cred1.effective_auth_region(&config), "us-east-1");
        assert_eq!(cred2.effective_auth_region(&config), "eu-west-1");
        assert_eq!(cred3.effective_auth_region(&config), "ap-northeast-1");
    }

    #[test]
    fn test_idc_oidc_endpoint_uses_credential_auth_region() {
        // 验证 IdC OIDC endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials {
            auth_region: Some("eu-central-1".to_string()),
            ..Default::default()
        };

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://oidc.{}.amazonaws.com/token", region);

        assert_eq!(refresh_url, "https://oidc.eu-central-1.amazonaws.com/token");
    }

    #[test]
    fn test_social_refresh_endpoint_uses_credential_auth_region() {
        // 验证 Social refresh endpoint URL 使用凭据 auth_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials {
            auth_region: Some("ap-southeast-1".to_string()),
            ..Default::default()
        };

        let region = credentials.effective_auth_region(&config);
        let refresh_url = format!("https://prod.{}.auth.desktop.kiro.dev/refreshToken", region);

        assert_eq!(
            refresh_url,
            "https://prod.ap-southeast-1.auth.desktop.kiro.dev/refreshToken"
        );
    }

    #[test]
    fn test_api_call_uses_effective_api_region() {
        // 验证 API 调用使用 effective_api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials {
            region: Some("eu-west-1".to_string()),
            ..Default::default()
        };

        // 凭据.region 不参与 api_region 回退链
        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.us-west-2.amazonaws.com");
    }

    #[test]
    fn test_api_call_uses_credential_api_region() {
        // 凭据配置了 api_region 时，API 调用应使用凭据的 api_region
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials {
            api_region: Some("eu-central-1".to_string()),
            ..Default::default()
        };

        let api_region = credentials.effective_api_region(&config);
        let api_host = format!("q.{}.amazonaws.com", api_region);

        assert_eq!(api_host, "q.eu-central-1.amazonaws.com");
    }

    #[test]
    fn test_credential_region_empty_string_treated_as_set() {
        // 空字符串 auth_region 被视为已设置（虽然不推荐，但行为应一致）
        let mut config = Config::default();
        config.region = "us-west-2".to_string();

        let credentials = KiroCredentials {
            auth_region: Some("".to_string()),
            ..Default::default()
        };

        let region = credentials.effective_auth_region(&config);
        // 空字符串被视为已设置，不会回退到 config
        assert_eq!(region, "");
    }

    #[test]
    fn test_auth_and_api_region_independent() {
        // auth_region 和 api_region 互不影响
        let mut config = Config::default();
        config.region = "default".to_string();

        let credentials = KiroCredentials {
            auth_region: Some("auth-only".to_string()),
            api_region: Some("api-only".to_string()),
            ..Default::default()
        };

        assert_eq!(credentials.effective_auth_region(&config), "auth-only");
        assert_eq!(credentials.effective_api_region(&config), "api-only");
    }

    fn model_test_credential(id: u64, tier: Option<&str>) -> KiroCredentials {
        KiroCredentials {
            id: Some(id),
            kiro_api_key: Some(format!("test-key-{id}")),
            priority: id as u32,
            subscription_title: tier.map(str::to_string),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn priority_opus_requests_skip_ineligible_current_credentials() {
        let manager = MultiTokenManager::new(
            serde_json::from_value(serde_json::json!({"loadBalancingMode":"priority"})).unwrap(),
            vec![
                model_test_credential(1, Some("KIRO FREE")),
                model_test_credential(2, Some("KIRO PRO")),
            ],
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            manager
                .acquire_context(Some("claude-sonnet-4.6"))
                .await
                .unwrap()
                .id,
            1
        );
        assert_eq!(
            manager
                .acquire_context(Some("CLAUDE-OPUS-4.6"))
                .await
                .unwrap()
                .id,
            2
        );
        // Returning to a FREE credential must not bypass filtering on the next call.
        manager.set_priority(1, 0).unwrap();
        assert_eq!(
            manager
                .acquire_context(Some("claude-opus-4.6"))
                .await
                .unwrap()
                .id,
            2
        );
    }

    #[tokio::test]
    async fn unsupported_models_do_not_disable_otherwise_healthy_credentials() {
        for mode in ["priority", "balanced"] {
            let config: Config = serde_json::from_value(serde_json::json!({
                "loadBalancingMode": mode
            }))
            .unwrap();
            let manager = MultiTokenManager::new(
                config,
                vec![model_test_credential(1, Some("FREE"))],
                None,
                None,
                false,
            )
            .unwrap();
            assert!(
                manager
                    .acquire_context(Some("claude-opus-4.6"))
                    .await
                    .is_err()
            );
            assert_eq!(manager.available_count(), 1);
            assert_eq!(manager.snapshot().entries[0].failure_count, 0);
            assert_eq!(
                manager
                    .acquire_context(Some("claude-sonnet-4.6"))
                    .await
                    .unwrap()
                    .id,
                1
            );
        }
        // Unknown subscription information keeps the existing permissive behavior.
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![model_test_credential(1, None)],
            None,
            None,
            false,
        )
        .unwrap();
        assert!(
            manager
                .acquire_context(Some("claude-opus-4.6"))
                .await
                .is_ok()
        );
    }

    struct CredentialTestDirectory(PathBuf);

    impl CredentialTestDirectory {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("kiro-credentials-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&directory).unwrap();
            Self(directory)
        }
        fn file(&self) -> PathBuf {
            self.0.join("credentials.json")
        }
    }

    impl Drop for CredentialTestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn single_credentials_persist_admin_changes_and_promote_to_array() {
        use crate::kiro::model::credentials::CredentialsConfig;
        let directory = CredentialTestDirectory::new();
        let path = directory.file();
        let credential = model_test_credential(1, Some("PRO"));
        std::fs::write(&path, serde_json::to_vec(&credential).unwrap()).unwrap();
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![credential],
            None,
            Some(path.clone()),
            false,
        )
        .unwrap();
        manager.set_priority(1, 9).unwrap();
        manager.set_disabled(1, true).unwrap();
        let single = CredentialsConfig::load(&path).unwrap();
        assert!(!single.is_multiple());
        let reloaded = single.into_sorted_credentials();
        assert!(reloaded[0].disabled);
        assert_eq!(reloaded[0].priority, 9);
        assert!(reloaded[0].machine_id.is_some());

        let added = manager
            .add_credential(model_test_credential(2, Some("PRO")))
            .await
            .unwrap();
        let multiple = CredentialsConfig::load(&path).unwrap();
        assert!(multiple.is_multiple());
        assert_eq!(multiple.into_sorted_credentials().len(), 2);
        manager.set_disabled(added, true).unwrap();
        manager.delete_credential(added).unwrap();
        assert!(CredentialsConfig::load(&path).unwrap().is_multiple());
        manager.delete_credential(1).unwrap();
        assert!(
            CredentialsConfig::load(&path)
                .unwrap()
                .into_sorted_credentials()
                .is_empty()
        );
    }

    #[test]
    fn concurrent_credential_updates_remain_parseable_and_keep_latest_state() {
        use crate::kiro::model::credentials::CredentialsConfig;
        use std::sync::{Arc, Barrier};
        let directory = CredentialTestDirectory::new();
        let path = directory.file();
        let manager = Arc::new(
            MultiTokenManager::new(
                Config::default(),
                (1..=4)
                    .map(|id| model_test_credential(id, Some("PRO")))
                    .collect(),
                None,
                Some(path.clone()),
                true,
            )
            .unwrap(),
        );
        let barrier = Arc::new(Barrier::new(6));
        let finished = Arc::new(AtomicBool::new(false));
        let reader = {
            let path = path.clone();
            let barrier = barrier.clone();
            let finished = finished.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut reads = 0;
                loop {
                    let config = CredentialsConfig::load(&path).unwrap();
                    assert_eq!(config.into_sorted_credentials().len(), 4);
                    reads += 1;
                    if finished.load(Ordering::Acquire) {
                        return reads;
                    }
                }
            })
        };
        let writers: Vec<_> = (1..=4)
            .map(|id| {
                let manager = manager.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for priority in 10..20 {
                        manager.set_priority(id, priority).unwrap();
                    }
                })
            })
            .collect();
        barrier.wait();
        for writer in writers {
            writer.join().unwrap();
        }
        finished.store(true, Ordering::Release);
        assert!(reader.join().unwrap() > 0);
        let reloaded = CredentialsConfig::load(&path)
            .unwrap()
            .into_sorted_credentials();
        assert!(reloaded.iter().all(|credential| credential.priority == 19));
        assert_eq!(reloaded.len(), manager.total_count());
    }

    #[test]
    fn failed_atomic_replace_keeps_target_and_removes_temporary_files() {
        let directory = CredentialTestDirectory::new();
        let target = directory.file();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), b"original").unwrap();
        assert!(atomic_write_credentials(&target, b"replacement").is_err());
        assert_eq!(std::fs::read(target.join("keep")).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_credentials_write_preserves_symlinks_and_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = CredentialTestDirectory::new();
        let target = directory.0.join("actual.json");
        let link = directory.file();
        std::fs::write(&target, b"old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &link).unwrap();
        atomic_write_credentials(&link, b"updated").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"updated");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let new_file = directory.0.join("new.json");
        atomic_write_credentials(&new_file, b"new").unwrap();
        assert_eq!(
            std::fs::metadata(new_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_additions_persist_unique_ids_and_reject_duplicates() {
        use crate::kiro::model::credentials::CredentialsConfig;
        use std::sync::Arc;
        let directory = CredentialTestDirectory::new();
        let path = directory.file();
        let manager = Arc::new(
            MultiTokenManager::new(Config::default(), vec![], None, Some(path.clone()), true)
                .unwrap(),
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let mut tasks = Vec::new();
        for id in 1..=8 {
            let manager = manager.clone();
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                manager
                    .add_credential(model_test_credential(id, Some("PRO")))
                    .await
                    .unwrap()
            }));
        }
        let mut ids = std::collections::HashSet::new();
        for task in tasks {
            assert!(ids.insert(task.await.unwrap()));
        }
        let config = CredentialsConfig::load(&path).unwrap();
        let restarted = MultiTokenManager::new(
            Config::default(),
            config.into_sorted_credentials(),
            None,
            Some(path),
            true,
        )
        .unwrap();
        assert_eq!(restarted.total_count(), 8);
        assert_eq!(restarted.available_count(), 8);

        let (first, second) = tokio::join!(
            manager.add_credential(model_test_credential(9, Some("PRO"))),
            manager.add_credential(model_test_credential(9, Some("PRO"))),
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        assert_eq!(manager.total_count(), 9);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn balanced_reservations_distribute_concurrent_requests_and_release_clones() {
        let manager = Arc::new(
            MultiTokenManager::new(
                serde_json::from_value::<Config>(
                    serde_json::json!({"loadBalancingMode":"balanced", "concurrency":{"maxInFlightPerCredential":16}}),
                )
                .unwrap(),
                (1..=3).map(|id| model_test_credential(id, None)).collect(),
                None,
                None,
                false,
            )
            .unwrap(),
        );
        let barrier = Arc::new(tokio::sync::Barrier::new(48));
        let tasks = (0..48)
            .map(|_| {
                let manager = manager.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    manager.acquire_context(None).await.unwrap()
                })
            })
            .collect::<Vec<_>>();
        let mut contexts = Vec::new();
        for task in tasks {
            contexts.push(task.await.unwrap());
        }
        assert_eq!(
            manager
                .entries
                .lock()
                .iter()
                .map(|e| e.in_flight.load(Ordering::Relaxed))
                .collect::<Vec<_>>(),
            vec![16, 16, 16]
        );
        let clone = contexts[0].clone();
        let id = clone.id;
        drop(contexts);
        assert_eq!(
            manager
                .entries
                .lock()
                .iter()
                .find(|e| e.id == id)
                .unwrap()
                .in_flight
                .load(Ordering::Relaxed),
            1
        );
        drop(clone);
        assert!(
            manager
                .entries
                .lock()
                .iter()
                .all(|e| e.in_flight.load(Ordering::Relaxed) == 0)
        );
    }

    #[tokio::test]
    async fn cancelling_token_refresh_releases_the_reserved_credential() {
        let credential = KiroCredentials {
            refresh_token: Some("test-refresh".into()),
            ..Default::default()
        };
        let manager = Arc::new(
            MultiTokenManager::new(Config::default(), vec![credential], None, None, false).unwrap(),
        );
        // Keep refresh queued so this test never contacts a real token service.
        let refresh_lock = manager.refresh_lock_for(1).unwrap();
        let refresh = refresh_lock.lock().await;
        let pending = manager.clone();
        let task = tokio::spawn(async move { pending.acquire_context(None).await });
        tokio::time::timeout(StdDuration::from_secs(2), async {
            loop {
                if manager.entries.lock()[0].in_flight.load(Ordering::Relaxed) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        assert_eq!(
            manager.entries.lock()[0].in_flight.load(Ordering::Relaxed),
            0
        );
        drop(refresh);
    }

    #[test]
    fn mode_changes_preserve_extensions_and_leave_runtime_unchanged_on_failure() {
        let directory = CredentialTestDirectory::new();
        let path = directory.0.join("config.json");
        let original =
            r#"{"loadBalancingMode":"priority","custom":{"nested":[1,2]},"apiKey":"keep-me"}"#;
        std::fs::write(&path, original).unwrap();
        let manager =
            MultiTokenManager::new(Config::load(&path).unwrap(), vec![], None, None, false)
                .unwrap();
        // 无效文件不得被默认值覆盖，也不得影响运行中的模式。
        std::fs::write(&path, b"{broken").unwrap();
        assert!(manager.set_load_balancing_mode("balanced".into()).is_err());
        assert_eq!(manager.get_load_balancing_mode(), "priority");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        std::fs::write(&path, original).unwrap();
        manager.set_load_balancing_mode("balanced".into()).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["custom"], serde_json::json!({"nested":[1,2]}));
        assert_eq!(saved["apiKey"], "keep-me");
        assert_eq!(
            saved["loadBalancingMode"],
            manager.get_load_balancing_mode()
        );
    }

    #[test]
    fn concurrent_mode_changes_keep_config_readable_and_match_runtime() {
        let directory = CredentialTestDirectory::new();
        let path = directory.0.join("config.json");
        std::fs::write(&path, r#"{"loadBalancingMode":"priority"}"#).unwrap();
        let manager =
            MultiTokenManager::new(Config::load(&path).unwrap(), vec![], None, None, false)
                .unwrap();
        let barrier = std::sync::Barrier::new(5);
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let manager = &manager;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    for iteration in 0..30 {
                        let mode = if (worker + iteration) % 2 == 0 {
                            "balanced"
                        } else {
                            "priority"
                        };
                        manager.set_load_balancing_mode(mode.into()).unwrap();
                    }
                });
            }
            barrier.wait();
            for _ in 0..200 {
                let config = Config::load(&path).unwrap();
                assert!(matches!(
                    config.load_balancing_mode.as_str(),
                    "priority" | "balanced"
                ));
                std::thread::yield_now();
            }
        });
        assert_eq!(
            Config::load(&path).unwrap().load_balancing_mode,
            manager.get_load_balancing_mode()
        );
    }

    #[tokio::test]
    async fn deleted_ids_are_not_reused_by_in_flight_requests() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![model_test_credential(1, Some("PRO"))],
            None,
            None,
            false,
        )
        .unwrap();
        let old_context = manager.acquire_context(None).await.unwrap();
        manager.set_disabled(1, true).unwrap();
        manager.delete_credential(1).unwrap();
        let new_id = manager
            .add_credential(KiroCredentials {
                kiro_api_key: Some("new-account".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(new_id, 2);
        // 被删除账号的延迟回调不得禁用新账号或污染其统计。
        for _ in 0..MAX_FAILURES_PER_CREDENTIAL {
            manager.report_failure(old_context.id);
        }
        manager.report_success(old_context.id);
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.available, 1);
        assert_eq!(snapshot.entries[0].failure_count, 0);
        assert_eq!(snapshot.entries[0].success_count, 0);
    }

    #[test]
    fn balance_cache_keys_follow_auth_identity_and_redact_secrets() {
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials {
                id: Some(1),
                refresh_token: Some("refresh-secret".into()),
                access_token: Some("access-secret".into()),
                ..Default::default()
            }],
            None,
            None,
            false,
        )
        .unwrap();
        let original = manager.credential_cache_key(1).unwrap();
        assert!(!original.contains("secret"));
        manager.entries.lock()[0].credentials.access_token = Some("rotated-secret".into());
        assert_eq!(manager.credential_cache_key(1).unwrap(), original);
        manager.entries.lock()[0].credentials.refresh_token = None;
        let access_only = manager.credential_cache_key(1).unwrap();
        assert_ne!(access_only, original);
        manager.entries.lock()[0].credentials.access_token = Some("other-account-secret".into());
        assert_ne!(manager.credential_cache_key(1).unwrap(), access_only);
        manager.entries.lock()[0].credentials.kiro_api_key = Some("other-account-secret".into());
        let api_key = manager.credential_cache_key(1).unwrap();
        assert_ne!(api_key, access_only);
        assert!(!api_key.contains("secret"));
        assert!(manager.credential_cache_key(2).is_none());
    }

    #[test]
    fn concurrent_stats_writes_are_atomic_and_keep_pending_updates() {
        let directory = CredentialTestDirectory::new();
        let path = directory.file();
        let manager = Arc::new(
            MultiTokenManager::new(
                Config::default(),
                vec![model_test_credential(1, None)],
                None,
                Some(path),
                false,
            )
            .unwrap(),
        );
        manager.save_stats();
        let stats_path = manager.stats_path().unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let reader_done = done.clone();
        let reader_path = stats_path.clone();
        let reader = std::thread::spawn(move || {
            let mut last = 0;
            let mut reads = 0;
            loop {
                let stats: HashMap<String, StatsEntry> =
                    serde_json::from_slice(&std::fs::read(&reader_path).unwrap()).unwrap();
                let count = stats["1"].success_count;
                assert!(count >= last, "旧统计快照覆盖了较新的计数");
                last = count;
                reads += 1;
                if reader_done.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::yield_now();
            }
            reads
        });
        let writers: Vec<_> = (0..4)
            .map(|_| {
                let manager = manager.clone();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        manager.report_success(1);
                        if i % 10 == 0 {
                            manager.save_stats();
                        }
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        // 最后的增量在防抖窗口内；析构必须将其落盘。
        assert!(manager.stats_dirty.load(Ordering::Relaxed));
        drop(manager);
        done.store(true, Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
        let stats: HashMap<String, StatsEntry> =
            serde_json::from_slice(&std::fs::read(stats_path).unwrap()).unwrap();
        assert_eq!(stats["1"].success_count, 200);
    }

    #[test]
    fn failed_stats_write_keeps_dirty_state_and_retries_on_drop() {
        let directory = CredentialTestDirectory::new();
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![model_test_credential(1, None)],
            None,
            Some(directory.file()),
            false,
        )
        .unwrap();
        let path = manager.stats_path().unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("keep"), b"original").unwrap();
        manager.report_success(1);
        assert!(manager.stats_dirty.load(Ordering::Relaxed));
        assert_eq!(std::fs::read(path.join("keep")).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 2);
        std::fs::remove_dir_all(&path).unwrap();
        drop(manager);
        let stats: HashMap<String, StatsEntry> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(stats["1"].success_count, 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn persisted_maximum_success_count_does_not_overflow() {
        let directory = CredentialTestDirectory::new();
        std::fs::write(
            directory.0.join("kiro_stats.json"),
            format!(
                r#"{{"1":{{"success_count":{},"last_used_at":null}}}}"#,
                u64::MAX
            ),
        )
        .unwrap();
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![model_test_credential(1, None)],
            None,
            Some(directory.file()),
            false,
        )
        .unwrap();
        manager.report_success(1);
        assert_eq!(manager.snapshot().entries[0].success_count, u64::MAX);
    }

    #[tokio::test]
    async fn credential_id_exhaustion_is_reported_without_overflow() {
        let maximum = model_test_credential(u64::MAX, Some("PRO"));
        let manager =
            MultiTokenManager::new(Config::default(), vec![maximum.clone()], None, None, false)
                .unwrap();
        assert_eq!(manager.acquire_context(None).await.unwrap().id, u64::MAX);
        let error = manager
            .add_credential(KiroCredentials {
                kiro_api_key: Some("new-account".into()),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ID 已耗尽"));
        assert_eq!(manager.total_count(), 1);
        let error = MultiTokenManager::new(
            Config::default(),
            vec![maximum, KiroCredentials::default()],
            None,
            None,
            false,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("ID 已耗尽"));
        let manager = MultiTokenManager::new(
            Config::default(),
            vec![
                model_test_credential(u64::MAX - 1, None),
                KiroCredentials::default(),
            ],
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(manager.snapshot().entries[1].id, u64::MAX);
    }
}
