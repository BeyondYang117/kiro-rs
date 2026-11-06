//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::time::sleep;

use crate::http_client::{ProxyConfig, build_client, read_bounded_text_observed};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::{
    AffinityKey, CallContext, CapacityError, MultiTokenManager, session_key,
};
use crate::model::config::TlsBackend;
use parking_lot::Mutex;

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 总重试次数硬上限（避免无限重试）
const MAX_TOTAL_RETRIES: usize = 9;

/// 不提前重试要求更长等待的响应，也不让单次退避无限挂起。
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Preserve the upstream status so a rejected request is not reported as 502.
#[derive(Debug)]
pub struct UpstreamApiError {
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl std::fmt::Display for UpstreamApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Kiro API 请求失败: {} {}",
            self.status,
            crate::request_log::redact(&self.body)
        )
    }
}

impl std::error::Error for UpstreamApiError {}

/// 上游响应持有凭据预留，直到响应读取完毕或被丢弃。
pub struct UpstreamResponse {
    response: reqwest::Response,
    affinity_key: Option<AffinityKey>,
    context: CallContext,
    token_manager: Arc<MultiTokenManager>,
}

impl UpstreamResponse {
    pub(crate) fn affinity_key(&self) -> Option<AffinityKey> {
        self.affinity_key
    }
    pub(crate) fn headers(&self) -> &reqwest::header::HeaderMap {
        self.response.headers()
    }

    pub(crate) fn credential_id(&self) -> u64 {
        self.context.id
    }

    pub fn bytes_stream(
        self,
    ) -> futures::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>> {
        use futures::StreamExt;
        futures::stream::unfold(
            (
                self.response.bytes_stream(),
                Some(self.context),
                self.token_manager,
            ),
            |(mut stream, mut context, manager)| async move {
                context.as_ref()?;
                let Some(chunk) = stream.next().await else {
                    crate::request_log::attempt_body_finished(None);
                    manager.report_transport_success(context.as_ref().unwrap());
                    return None;
                };
                match &chunk {
                    Ok(bytes) => crate::request_log::attempt_body_chunk(bytes),
                    Err(error) => {
                        crate::request_log::attempt_body_finished(Some(
                            &crate::request_log::error_chain(error),
                        ));
                        // An errored body cannot make further progress; release immediately.
                        context.take();
                    }
                }
                Some((chunk, (stream, context, manager)))
            },
        )
        .boxed()
    }

    pub async fn text(self) -> anyhow::Result<String> {
        let limits = &self.token_manager.config().request_limits;
        let result = read_bounded_text_observed(
            self.response,
            limits.max_response_bytes,
            Duration::from_secs(limits.first_content_timeout_seconds),
            Duration::from_secs(limits.idle_timeout_seconds),
            crate::request_log::attempt_body_chunk,
        )
        .await;
        crate::request_log::attempt_body_finished(
            result
                .as_ref()
                .err()
                .map(|error| format!("{error:#}"))
                .as_deref(),
        );
        if result.is_ok() {
            self.token_manager.report_transport_success(&self.context);
        }
        drop(self.context);
        result
    }
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// 全局代理配置（用于凭据无自定义代理时的回退）
    global_proxy: Option<ProxyConfig>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client
    client_cache: Mutex<HashMap<Option<ProxyConfig>, Client>>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
}

impl KiroProvider {
    pub(crate) fn request_limits(&self) -> &crate::model::config::RequestLimits {
        &self.token_manager.config().request_limits
    }

    pub(crate) fn tool_output_recovery_attempts(&self) -> u8 {
        self.token_manager.config().tool_output_recovery_attempts
    }

    /// Called only after the Messages or MCP response has been decoded and validated.
    pub(crate) fn report_response_success(
        &self,
        credential_id: u64,
        affinity_key: Option<AffinityKey>,
    ) {
        self.token_manager.report_success(credential_id);
        self.token_manager
            .report_session_success(affinity_key, credential_id);
    }

    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 预热：构建全局代理对应的 Client
        let initial_client =
            build_client(proxy.as_ref(), 720, tls_backend).expect("创建 HTTP 客户端失败");
        let mut cache = HashMap::new();
        cache.insert(proxy.clone(), initial_client);

        Self {
            token_manager,
            global_proxy: proxy,
            client_cache: Mutex::new(cache),
            tls_backend,
            endpoints,
            default_endpoint,
        }
    }

    /// 根据凭据的代理配置获取（或创建并缓存）对应的 reqwest::Client
    fn client_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Client> {
        let effective = credentials.effective_proxy(self.global_proxy.as_ref());
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&effective) {
            return Ok(client.clone());
        }
        let client = build_client(effective.as_ref(), 720, self.tls_backend)?;
        cache.insert(effective, client.clone());
        Ok(client)
    }

    /// 根据凭据选择 endpoint 实现
    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）
    pub async fn call_api(&self, request_body: &str) -> anyhow::Result<UpstreamResponse> {
        self.call_api_with_retry(request_body, false).await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(&self, request_body: &str) -> anyhow::Result<UpstreamResponse> {
        self.call_api_with_retry(request_body, true).await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(&self, request_body: &str) -> anyhow::Result<UpstreamResponse> {
        self.call_mcp_with_retry(request_body).await
    }

    async fn read_error_body(&self, response: reqwest::Response) -> String {
        let limits = self.request_limits();
        let result = read_bounded_text_observed(
            response,
            limits.max_response_bytes.min(64 * 1024),
            Duration::from_secs(limits.first_content_timeout_seconds),
            Duration::from_secs(limits.idle_timeout_seconds),
            crate::request_log::attempt_body_chunk,
        )
        .await;
        crate::request_log::attempt_body_finished(
            result
                .as_ref()
                .err()
                .map(|error| format!("{error:#}"))
                .as_deref(),
        );
        result.unwrap_or_else(|error| format!("读取上游错误响应失败: {error:#}"))
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(&self, request_body: &str) -> anyhow::Result<UpstreamResponse> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();

        for attempt in 0..max_retries {
            // MCP 调用（WebSearch 等工具）不涉及模型选择，无需按模型过滤凭据
            let ctx = match self.token_manager.acquire_context(None).await {
                Ok(c) => c,
                Err(e)
                    if e.is::<CapacityError>()
                        || e.is::<crate::kiro::retry_budget::RetryBudgetExceeded>() =>
                {
                    return Err(e);
                }
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.mcp_url(&rctx);
            let body = endpoint.transform_mcp_body(request_body, &rctx);

            let base = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(body)
                .header("content-type", "application/json");
            let request = endpoint.decorate_mcp(base, &rctx);

            crate::kiro::retry_budget::consume()?;
            crate::request_log::attempt_started(ctx.id, endpoint.name());
            let response = match request
                .timeout(Duration::from_secs(
                    self.request_limits().total_timeout_seconds,
                ))
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    crate::request_log::attempt_result(
                        None,
                        Some(&crate::request_log::error_chain(&e)),
                        None,
                    );
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        crate::request_log::redact(&e.to_string())
                    );
                    last_error = Some(e.into());
                    drop(ctx);
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            let upstream_id = response
                .headers()
                .get("x-amzn-requestid")
                .or_else(|| response.headers().get("x-amz-request-id"))
                .or_else(|| response.headers().get("x-request-id"))
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            crate::request_log::attempt_result(Some(status.as_u16()), None, upstream_id.as_deref());
            let retry_after = Self::retry_after(response.headers(), SystemTime::now());
            if status.as_u16() == 429 {
                self.token_manager
                    .report_rate_limited_for_context(&ctx, retry_after);
            }

            // 成功响应
            if status.is_success() {
                return Ok(UpstreamResponse {
                    response,
                    affinity_key: None,
                    context: ctx,
                    token_manager: self.token_manager.clone(),
                });
            }

            // 失败响应
            let body = self.read_error_body(response).await;
            crate::request_log::attempt_result(
                Some(status.as_u16()),
                Some(&body),
                upstream_id.as_deref(),
            );

            // Redirects are endpoint configuration failures, not credential failures.
            if status.is_redirection() {
                return Err(UpstreamApiError { status, body }.into());
            }

            // 402 额度用尽
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 与对话端点一致保留无须重试的参数错误。
            if status.as_u16() == 400 {
                return Err(UpstreamApiError { status, body }.into());
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if !ctx.credentials.is_api_key_credential()
                    && endpoint.is_bearer_token_invalid(&body)
                    && !force_refreshed.contains(&ctx.id)
                {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    match self.token_manager.refresh_invalid_token(&ctx).await {
                        Ok(_) => {
                            tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                            continue;
                        }
                        Err(error) => {
                            tracing::warn!(credential_id = ctx.id, error = %crate::request_log::redact(&format!("{error:#}")),
                            "Token 强制刷新失败，已应用认证恢复策略");
                            if error.is::<crate::kiro::retry_budget::RetryBudgetExceeded>()
                                || error.is::<CapacityError>()
                            {
                                return Err(error);
                            }
                            last_error = Some(error);
                            continue;
                        }
                    }
                }

                if status.as_u16() == 403 {
                    self.token_manager.report_permission_denied(ctx.id);
                    return Err(UpstreamApiError { status, body }.into());
                }
                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    crate::request_log::redact(&body)
                );
                last_error = Some(UpstreamApiError { status, body }.into());
                drop(ctx);
                if attempt + 1 < max_retries {
                    let Some(delay) = Self::response_retry_delay(attempt, retry_after) else {
                        break;
                    };
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            drop(ctx);
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
    ) -> anyhow::Result<UpstreamResponse> {
        let total_credentials = self.token_manager.total_count();
        let max_retries = (total_credentials * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 一次解析同时提取模型资格和会话亲和信息。
        let (model, affinity_key, controls) = Self::extract_routing_from_request(request_body);

        for attempt in 0..max_retries {
            // 获取调用上下文（绑定 index、credentials、token）
            let ctx = match self
                .token_manager
                .acquire_context_with_controls(
                    model.as_deref(),
                    affinity_key.as_ref(),
                    controls.as_ref(),
                )
                .await
            {
                Ok(c) => c,
                Err(e)
                    if e.is::<CapacityError>()
                        || e.is::<crate::kiro::model_controls::ModelParameterError>()
                        || e.is::<crate::kiro::retry_budget::RetryBudgetExceeded>() =>
                {
                    return Err(e);
                }
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let rctx = RequestContext {
                credentials: &ctx.credentials,
                token: &ctx.token,
                machine_id: &machine_id,
                config,
            };

            let url = endpoint.api_url(&rctx);
            let body = endpoint.transform_api_body(request_body, &rctx);

            let base = self
                .client_for(&ctx.credentials)?
                .post(&url)
                .body(body)
                .header("content-type", "application/json");
            let request = endpoint.decorate_api(base, &rctx);

            crate::kiro::retry_budget::consume()?;
            crate::request_log::attempt_started(ctx.id, endpoint.name());
            let response = match request
                .timeout(Duration::from_secs(
                    self.request_limits().total_timeout_seconds,
                ))
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    crate::request_log::attempt_result(
                        None,
                        Some(&crate::request_log::error_chain(&e)),
                        None,
                    );
                    tracing::warn!(
                        "API 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        crate::request_log::redact(&e.to_string())
                    );
                    // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                    // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                    last_error = Some(e.into());
                    drop(ctx);
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            let upstream_id = response
                .headers()
                .get("x-amzn-requestid")
                .or_else(|| response.headers().get("x-amz-request-id"))
                .or_else(|| response.headers().get("x-request-id"))
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            crate::request_log::attempt_result(Some(status.as_u16()), None, upstream_id.as_deref());
            let retry_after = Self::retry_after(response.headers(), SystemTime::now());
            if status.as_u16() == 429 {
                self.token_manager
                    .report_rate_limited_for_context(&ctx, retry_after);
            }

            // Messages callers validate the body before reporting success.
            if status.is_success() {
                return Ok(UpstreamResponse {
                    response,
                    affinity_key,
                    context: ctx,
                    token_manager: self.token_manager.clone(),
                });
            }

            // 失败响应：读取 body 用于日志/错误信息
            let body = self.read_error_body(response).await;
            crate::request_log::attempt_result(
                Some(status.as_u16()),
                Some(&body),
                upstream_id.as_deref(),
            );

            if status.is_redirection() {
                return Err(UpstreamApiError { status, body }.into());
            }

            // 402 Payment Required 且额度用尽：禁用凭据并故障转移
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    crate::request_log::redact(&body)
                );

                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 400 Bad Request - 请求问题，重试/切换凭据无意义
            if status.as_u16() == 400 {
                return Err(UpstreamApiError { status, body }.into());
            }

            // Token 失效先尝试刷新；403 停用并终止本次调用，401 按失败阈值处理。
            if matches!(status.as_u16(), 401 | 403) {
                tracing::warn!(
                    "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    crate::request_log::redact(&body)
                );

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if !ctx.credentials.is_api_key_credential()
                    && endpoint.is_bearer_token_invalid(&body)
                    && !force_refreshed.contains(&ctx.id)
                {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    match self.token_manager.refresh_invalid_token(&ctx).await {
                        Ok(_) => {
                            tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                            continue;
                        }
                        Err(error) => {
                            tracing::warn!(credential_id = ctx.id, error = %crate::request_log::redact(&format!("{error:#}")),
                            "Token 强制刷新失败，已应用认证恢复策略");
                            if error.is::<crate::kiro::retry_budget::RetryBudgetExceeded>()
                                || error.is::<CapacityError>()
                            {
                                return Err(error);
                            }
                            last_error = Some(error);
                            continue;
                        }
                    }
                }

                if status.as_u16() == 403 {
                    self.token_manager.report_permission_denied(ctx.id);
                    return Err(UpstreamApiError { status, body }.into());
                }
                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 429/408/5xx - 瞬态错误不计入认证失败；429 账号冷却后重新调度
            // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    crate::request_log::redact(&body)
                );
                last_error = Some(UpstreamApiError { status, body }.into());
                drop(ctx);
                if attempt + 1 < max_retries {
                    let Some(delay) = Self::response_retry_delay(attempt, retry_after) else {
                        break;
                    };
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 兜底：当作可重试的瞬态错误处理（不切换凭据）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                crate::request_log::redact(&body)
            );
            last_error = Some(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            ));
            drop(ctx);
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    /// 从请求体中提取模型和会话亲和键。
    ///
    /// 模型和 conversationId 独立解析，缺失会话标识不影响模型资格检查。
    fn extract_routing_from_request(
        request_body: &str,
    ) -> (
        Option<String>,
        Option<AffinityKey>,
        Option<crate::kiro::model_controls::ModelControls>,
    ) {
        use serde_json::Value;

        let Ok(json) = serde_json::from_str::<Value>(request_body) else {
            return (None, None, None);
        };
        let model = json
            .pointer("/conversationState/currentMessage/userInputMessage/modelId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let key = json
            .pointer("/conversationState/conversationId")
            .and_then(Value::as_str)
            .and_then(session_key);
        let controls = crate::kiro::model_controls::ModelControls::from_value(&json);
        (model, key, controls)
    }

    fn retry_after(headers: &reqwest::header::HeaderMap, now: SystemTime) -> Option<Duration> {
        crate::http_client::retry_after(headers, now)
    }

    fn response_retry_delay(attempt: usize, retry_after: Option<Duration>) -> Option<Duration> {
        let delay = retry_after.unwrap_or_default();
        if delay > MAX_RETRY_AFTER {
            return None;
        }
        Some(Self::retry_delay(attempt).max(delay))
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;
    use crate::model::config::Config;
    use futures::{StreamExt, stream};

    async fn response(pending: bool) -> (Arc<MultiTokenManager>, UpstreamResponse) {
        response_with_limit(pending, 64).await
    }

    async fn response_with_limit(
        pending: bool,
        max_in_flight: usize,
    ) -> (Arc<MultiTokenManager>, UpstreamResponse) {
        let manager = Arc::new(
            MultiTokenManager::new(
                serde_json::from_value::<Config>(serde_json::json!({
                    "loadBalancingMode":"balanced",
                    "concurrency":{"maxInFlight":max_in_flight,"maxWaiting":0}
                }))
                .unwrap(),
                (1..=2)
                    .map(|id| KiroCredentials {
                        id: Some(id),
                        kiro_api_key: Some(format!("test-{id}")),
                        ..Default::default()
                    })
                    .collect(),
                None,
                None,
                false,
            )
            .unwrap(),
        );
        let context = manager.acquire_context(None).await.unwrap();
        assert_eq!(context.id, 1);
        // Prefer #1 on historical usage alone; while its response is active #2 must win.
        manager.report_success(2);
        let body = if pending {
            reqwest::Body::wrap_stream(
                stream::once(async { Ok::<_, std::io::Error>("hello") })
                    .chain(stream::pending::<Result<&str, std::io::Error>>()),
            )
        } else {
            reqwest::Body::from("hello")
        };
        let response = reqwest::Response::from(http::Response::new(body));
        (
            manager.clone(),
            UpstreamResponse {
                response,
                affinity_key: None,
                context,
                token_manager: manager,
            },
        )
    }

    #[tokio::test]
    async fn oversized_text_response_releases_reservation_without_recording_success() {
        let manager = Arc::new(
            MultiTokenManager::new(
                serde_json::from_value(serde_json::json!({
                    "requestLimits": {"maxResponseBytes": 1024}
                }))
                .unwrap(),
                vec![KiroCredentials {
                    id: Some(1),
                    kiro_api_key: Some("test-key".into()),
                    ..Default::default()
                }],
                None,
                None,
                true,
            )
            .unwrap(),
        );
        let context = manager.acquire_context(None).await.unwrap();
        let response = UpstreamResponse {
            response: reqwest::Response::from(http::Response::new(reqwest::Body::from(
                "x".repeat(1025),
            ))),
            affinity_key: None,
            context,
            token_manager: manager.clone(),
        };
        assert_eq!(manager.snapshot().concurrency.in_flight, 1);
        assert!(
            response
                .text()
                .await
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        assert_eq!(manager.snapshot().concurrency.in_flight, 0);
        assert_eq!(manager.snapshot().entries[0].success_count, 0);
    }

    #[tokio::test]
    async fn global_limit_is_held_until_stream_eof_drop_or_read_cancellation() {
        for mode in [
            "drop_response",
            "text",
            "stream",
            "drop_stream",
            "cancel_read",
        ] {
            let (manager, response) = response_with_limit(mode == "cancel_read", 1).await;
            assert!(
                manager
                    .acquire_context(None)
                    .await
                    .err()
                    .unwrap()
                    .is::<CapacityError>()
            );
            match mode {
                "text" => {
                    response.text().await.unwrap();
                }
                "stream" | "drop_stream" => {
                    let mut chunks = response.bytes_stream();
                    assert_eq!(chunks.next().await.unwrap().unwrap(), "hello");
                    assert!(
                        manager
                            .acquire_context(None)
                            .await
                            .err()
                            .unwrap()
                            .is::<CapacityError>()
                    );
                    if mode == "stream" {
                        assert!(chunks.next().await.is_none());
                        // EOF must release even while the completed stream is still held.
                        drop(manager.acquire_context(None).await.unwrap());
                    }
                    drop(chunks);
                }
                "cancel_read" => {
                    let task = tokio::spawn(response.text());
                    tokio::task::yield_now().await;
                    task.abort();
                    assert!(task.await.err().unwrap().is_cancelled());
                }
                _ => drop(response),
            }
            drop(manager.acquire_context(None).await.unwrap());
        }
    }

    #[tokio::test]
    async fn response_reservation_lasts_until_body_consumption_or_drop() {
        for mode in ["drop_response", "text", "stream", "drop_stream"] {
            let (manager, response) = response(false).await;
            assert_eq!(manager.acquire_context(None).await.unwrap().id, 2);
            match mode {
                "text" => assert_eq!(response.text().await.unwrap(), "hello"),
                "stream" => {
                    let mut chunks = response.bytes_stream();
                    assert_eq!(chunks.next().await.unwrap().unwrap(), "hello");
                    assert_eq!(manager.acquire_context(None).await.unwrap().id, 2);
                    assert!(chunks.next().await.is_none());
                }
                "drop_stream" => {
                    let mut chunks = response.bytes_stream();
                    chunks.next().await.unwrap().unwrap();
                    assert_eq!(manager.acquire_context(None).await.unwrap().id, 2);
                    drop(chunks);
                }
                _ => drop(response),
            }
            assert_eq!(manager.acquire_context(None).await.unwrap().id, 1, "{mode}");
        }
    }

    #[tokio::test]
    async fn cancelling_body_read_releases_reservation() {
        let (manager, response) = response(true).await;
        let task = tokio::spawn(response.text());
        assert_eq!(manager.acquire_context(None).await.unwrap().id, 2);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(manager.acquire_context(None).await.unwrap().id, 1);
    }

    #[tokio::test]
    async fn body_error_releases_reservation_before_stream_is_dropped() {
        let (manager, mut response) = response(false).await;
        response.response =
            reqwest::Response::from(http::Response::new(reqwest::Body::wrap_stream(
                stream::once(async { Err::<&str, _>(std::io::Error::other("broken body")) }),
            )));
        let mut chunks = response.bytes_stream();
        assert!(chunks.next().await.unwrap().is_err());
        assert_eq!(manager.acquire_context(None).await.unwrap().id, 1);
        assert!(chunks.next().await.is_none());
    }

    #[tokio::test]
    async fn adaptive_recovery_requires_complete_body_consumption() {
        futures::future::join_all(
            [
                "text",
                "stream",
                "drop_response",
                "drop_stream",
                "text_error",
                "stream_error",
                "cancel_read",
            ]
            .into_iter()
            .map(|mode| async move {
                let manager = Arc::new(
                    MultiTokenManager::new(
                        serde_json::from_value(serde_json::json!({"concurrency": {
                            "adaptiveConcurrency": true, "maxInFlightPerCredential": 4,
                            "rateLimitCooldownSeconds": 1
                        }}))
                        .unwrap(),
                        vec![KiroCredentials {
                            id: Some(1),
                            kiro_api_key: Some("adaptive-test".into()),
                            ..Default::default()
                        }],
                        None,
                        None,
                        true,
                    )
                    .unwrap(),
                );
                manager.report_rate_limited(1, None);
                // 调度器的冷却计时器负责放行探测，无需手动操作内部状态。
                let context =
                    tokio::time::timeout(Duration::from_secs(3), manager.acquire_context(None))
                        .await
                        .unwrap()
                        .unwrap();
                let body = if mode.ends_with("error") {
                    reqwest::Body::wrap_stream(stream::once(async {
                        Err::<&str, _>(std::io::Error::other("broken body"))
                    }))
                } else if mode == "cancel_read" {
                    reqwest::Body::wrap_stream(stream::pending::<Result<&str, std::io::Error>>())
                } else {
                    reqwest::Body::from("hello")
                };
                let response = UpstreamResponse {
                    response: reqwest::Response::from(http::Response::new(body)),
                    affinity_key: None,
                    context,
                    token_manager: manager.clone(),
                };
                match mode {
                    "text" => {
                        response.text().await.unwrap();
                    }
                    "stream" => {
                        let mut chunks = response.bytes_stream();
                        assert!(chunks.next().await.unwrap().is_ok());
                        assert_eq!(manager.snapshot().entries[0].effective_concurrency_limit, 1);
                        assert!(chunks.next().await.is_none());
                    }
                    "drop_stream" => {
                        let mut chunks = response.bytes_stream();
                        chunks.next().await.unwrap().unwrap();
                    }
                    "text_error" => {
                        assert!(response.text().await.is_err());
                    }
                    "stream_error" => {
                        let mut chunks = response.bytes_stream();
                        assert!(chunks.next().await.unwrap().is_err());
                        assert!(chunks.next().await.is_none());
                    }
                    "cancel_read" => {
                        assert!(
                            tokio::time::timeout(Duration::from_millis(10), response.text())
                                .await
                                .is_err()
                        );
                    }
                    _ => drop(response),
                }
                assert_eq!(
                    manager.snapshot().entries[0].effective_concurrency_limit,
                    if matches!(mode, "text" | "stream") {
                        2
                    } else {
                        1
                    },
                    "{mode}"
                );
                assert_eq!(manager.snapshot().concurrency.in_flight, 0, "{mode}");
                // 传输完成不计业务成功。
                assert_eq!(manager.snapshot().entries[0].success_count, 0);
            }),
        )
        .await;
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::model::config::Config;
    use reqwest::{
        StatusCode,
        header::{HeaderMap, HeaderValue, RETRY_AFTER},
    };
    use tokio::time::{Instant, timeout};

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn retry_after_supports_seconds_and_all_http_date_formats() {
        let now = httpdate::parse_http_date("Sun, 06 Nov 1994 08:49:00 GMT").unwrap();
        for value in [
            "37",
            " 37 ",
            "Sun, 06 Nov 1994 08:49:37 GMT",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
        ] {
            assert_eq!(
                KiroProvider::retry_after(&headers(value), now),
                Some(Duration::from_secs(37)),
                "{value}"
            );
        }
        assert_eq!(
            KiroProvider::retry_after(&headers("Sun, 06 Nov 1994 08:48:00 GMT"), now),
            Some(Duration::ZERO)
        );
        for value in ["", "-1", "+1", "1.5", "tomorrow", "1, 2"] {
            assert_eq!(
                KiroProvider::retry_after(&headers(value), now),
                None,
                "{value}"
            );
        }
        assert_eq!(KiroProvider::retry_after(&HeaderMap::new(), now), None);
    }

    #[test]
    fn retry_waits_keep_backoff_and_do_not_shorten_long_server_delays() {
        for value in ["0", "invalid"] {
            let delay = KiroProvider::response_retry_delay(
                0,
                KiroProvider::retry_after(&headers(value), SystemTime::now()),
            )
            .unwrap();
            assert!((Duration::from_millis(200)..=Duration::from_millis(250)).contains(&delay));
        }
        assert_eq!(
            KiroProvider::response_retry_delay(8, Some(Duration::from_secs(60))),
            Some(Duration::from_secs(60))
        );
        for value in [
            "61",
            "18446744073709551615",
            "99999999999999999999999999999999999999",
        ] {
            assert!(
                KiroProvider::response_retry_delay(
                    0,
                    KiroProvider::retry_after(&headers(value), SystemTime::now())
                )
                .is_none(),
                "{value}"
            );
        }
    }

    struct MockEndpoint(String);

    impl KiroEndpoint for MockEndpoint {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn api_url(&self, _: &RequestContext<'_>) -> String {
            self.0.clone()
        }
        fn mcp_url(&self, _: &RequestContext<'_>) -> String {
            self.0.clone()
        }
        fn decorate_api(
            &self,
            req: reqwest::RequestBuilder,
            _: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req
        }
        fn decorate_mcp(
            &self,
            req: reqwest::RequestBuilder,
            _: &RequestContext<'_>,
        ) -> reqwest::RequestBuilder {
            req
        }
        fn transform_api_body(&self, body: &str, _: &RequestContext<'_>) -> String {
            body.to_string()
        }
    }

    struct Mock {
        provider: Arc<KiroProvider>,
        calls: Arc<Mutex<Vec<Instant>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn mock(status: StatusCode, retry_after: Option<&'static str>, recover: bool) -> Mock {
        mock_with_body(status, retry_after, recover, "upstream busy").await
    }

    async fn mock_with_body(
        status: StatusCode,
        retry_after: Option<&'static str>,
        recover: bool,
        body: &'static str,
    ) -> Mock {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let captured = captured.clone();
                async move {
                    let mut calls = captured.lock();
                    calls.push(Instant::now());
                    if recover && calls.len() > 1 {
                        return (StatusCode::OK, HeaderMap::new(), "success");
                    }
                    (status, retry_after.map(headers).unwrap_or_default(), body)
                }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let manager = MultiTokenManager::new(
            serde_json::from_value::<Config>(serde_json::json!({"loadBalancingMode":"balanced", "concurrency":{"maxInFlight":1}}))
                .unwrap(),
            (1..=2)
                .map(|id| KiroCredentials {
                    id: Some(id),
                    kiro_api_key: Some(format!("test-{id}")),
                    proxy_url: Some("direct".into()),
                    ..Default::default()
                })
                .collect(),
            None,
            None,
            false,
        )
        .unwrap();
        // #1 should be preferred whenever no request is actively using it.
        manager.report_success(2);
        let endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::from([(
            "mock".into(),
            Arc::new(MockEndpoint(format!("http://{address}/"))) as Arc<dyn KiroEndpoint>,
        )]);
        let provider = Arc::new(KiroProvider::with_proxy(
            Arc::new(manager),
            None,
            endpoints,
            "mock".into(),
        ));
        Mock {
            provider,
            calls,
            task,
        }
    }

    async fn call(provider: &KiroProvider, kind: &str) -> anyhow::Result<UpstreamResponse> {
        match kind {
            "stream" => provider.call_api_stream("{}").await,
            "mcp" => provider.call_mcp("{}").await,
            _ => provider.call_api("{}").await,
        }
    }

    #[tokio::test]
    async fn permission_denial_stops_the_call_without_cycling_accounts_or_reenabling_them() {
        for kind in ["api", "stream", "mcp"] {
            let mock = mock(StatusCode::FORBIDDEN, None, false).await;
            for expected in 1..=2 {
                let error = call(&mock.provider, kind).await.err().unwrap();
                assert_eq!(
                    error.downcast_ref::<UpstreamApiError>().unwrap().status,
                    StatusCode::FORBIDDEN
                );
                assert_eq!(mock.calls.lock().len(), expected);
            }
            assert!(call(&mock.provider, kind).await.is_err());
            assert_eq!(mock.calls.lock().len(), 2);
            assert_eq!(
                mock.provider.token_manager.snapshot().concurrency.in_flight,
                0
            );
            assert!(
                mock.provider
                    .token_manager
                    .snapshot()
                    .entries
                    .iter()
                    .all(|entry| entry.disabled_reason.as_deref() == Some("PermissionDenied"))
            );
        }
    }

    #[tokio::test]
    async fn one_budget_caps_http_retries_and_multiple_provider_calls() {
        use crate::kiro::retry_budget::{CURRENT, RetryBudget, RetryBudgetExceeded};
        for kind in ["api", "stream", "mcp"] {
            let mock = mock(StatusCode::SERVICE_UNAVAILABLE, None, false).await;
            let budget = RetryBudget::new(2, tokio::time::Instant::now() + Duration::from_secs(10));
            let error = CURRENT
                .scope(budget, call(&mock.provider, kind))
                .await
                .err()
                .unwrap();
            assert!(error.is::<RetryBudgetExceeded>());
            assert_eq!(mock.calls.lock().len(), 2);
            assert_eq!(
                mock.provider.token_manager.snapshot().concurrency.in_flight,
                0
            );
        }
        let mock = mock(StatusCode::OK, None, false).await;
        let budget = RetryBudget::new(2, tokio::time::Instant::now() + Duration::from_secs(10));
        CURRENT
            .scope(budget, async {
                for kind in ["api", "mcp"] {
                    call(&mock.provider, kind)
                        .await
                        .unwrap()
                        .text()
                        .await
                        .unwrap();
                }
                assert!(
                    call(&mock.provider, "stream")
                        .await
                        .err()
                        .unwrap()
                        .is::<RetryBudgetExceeded>()
                );
            })
            .await;
        assert_eq!(mock.calls.lock().len(), 2);
    }

    #[tokio::test]
    async fn oversized_error_bodies_preserve_status_and_do_not_retry_parameter_errors() {
        static BODY: [u8; 65_537] = [b'x'; 65_537];
        let mock = mock_with_body(
            StatusCode::BAD_REQUEST,
            None,
            false,
            std::str::from_utf8(&BODY).unwrap(),
        )
        .await;
        for kind in ["api", "stream", "mcp"] {
            let error = call(&mock.provider, kind).await.err().unwrap();
            let error = error.downcast_ref::<UpstreamApiError>().unwrap();
            assert_eq!(error.status, StatusCode::BAD_REQUEST);
            assert!(error.body.contains("byte limit"));
            assert!(error.body.len() < 1024);
            assert_eq!(
                mock.provider.token_manager.snapshot().concurrency.in_flight,
                0
            );
        }
        assert_eq!(mock.calls.lock().len(), 3);
    }

    #[tokio::test]
    async fn mcp_http_success_is_not_counted_before_response_validation() {
        let mock = mock(StatusCode::OK, None, false).await;
        let before = mock.provider.token_manager.snapshot();
        let response = call(&mock.provider, "mcp").await.unwrap();
        response.text().await.unwrap();
        let after = mock.provider.token_manager.snapshot();
        for (before, after) in before.entries.iter().zip(&after.entries) {
            assert_eq!(before.success_count, after.success_count);
        }
    }

    #[tokio::test]
    async fn catalog_parameter_validation_blocks_invalid_requests_before_http_or_refresh() {
        use crate::kiro::{
            model_controls::ModelParameterError,
            models::{AvailableModel, ModelCatalog},
        };
        let mock = mock(StatusCode::OK, None, false).await;
        let manager = Arc::new(
            MultiTokenManager::new(
                serde_json::from_value(
                    serde_json::json!({"endpoints":{"ide":{"nativeThinking":true}}}),
                )
                .unwrap(),
                vec![KiroCredentials {
                    id: Some(1),
                    // Expired OAuth would require a network refresh if validation happened too late.
                    access_token: Some("private-access-token".into()),
                    refresh_token: Some("private-refresh-token".into()),
                    expires_at: Some("2000-01-01T00:00:00Z".into()),
                    ..Default::default()
                }],
                None,
                None,
                true,
            )
            .unwrap(),
        );
        let model = AvailableModel {
            model_id: "claude-sonnet-4.6".into(),
            model_name: "Sonnet".into(),
            provider: None,
            rate_multiplier: None,
            rate_unit: None,
            max_input_tokens: None,
            max_output_tokens: Some(1000),
            effort_levels: vec![],
            effort_schema_path: None,
            default_effort: None,
            thinking_types: vec![],
            default_thinking: None,
            proxy_supported: true,
        };
        manager.update_model_catalog(
            1,
            &manager.model_catalog_key(1).unwrap(),
            &ModelCatalog {
                models: vec![model],
                default_model_id: None,
                region: "us-east-1".into(),
            },
            std::time::Instant::now(),
            false,
        );
        let provider = KiroProvider::with_proxy(
            manager.clone(),
            None,
            mock.provider.endpoints.clone(),
            "mock".into(),
        );
        let body = serde_json::json!({"conversationState":{"currentMessage":{"userInputMessage":{"modelId":"claude-sonnet-4.6"}}},
            "additionalModelRequestFields":{"max_tokens":1001}}).to_string();
        for streaming in [false, true] {
            let error = provider
                .call_api_with_retry(&body, streaming)
                .await
                .err()
                .unwrap();
            assert_eq!(
                error.downcast_ref::<ModelParameterError>().unwrap().0,
                "max_tokens"
            );
        }
        assert!(mock.calls.lock().is_empty());
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.entries[0].refresh_failure_count, 0);
        assert!(!snapshot.entries[0].disabled);
        assert_eq!(snapshot.concurrency.admitted_total, 0);
    }

    #[tokio::test]
    async fn capacity_rejections_do_not_retry_or_reach_the_upstream() {
        let mock = mock(StatusCode::OK, None, true).await;
        for max_waiting in [0, 1] {
            let manager = Arc::new(MultiTokenManager::new(
                serde_json::from_value::<Config>(serde_json::json!({
                    "concurrency":{"maxInFlight":1,"maxWaiting":max_waiting,"waitTimeoutSeconds":1}
                })).unwrap(),
                vec![KiroCredentials {
                    kiro_api_key: Some("capacity-test".into()),
                    ..Default::default()
                }],
                None, None, false,
            ).unwrap());
            let _active = manager.acquire_context(None).await.unwrap();
            let provider = KiroProvider::with_proxy(
                manager,
                None,
                mock.provider.endpoints.clone(),
                "mock".into(),
            );
            for kind in ["api", "stream", "mcp"] {
                let error = timeout(Duration::from_secs(2), call(&provider, kind))
                    .await
                    .expect("capacity errors must bypass HTTP retries")
                    .err()
                    .unwrap();
                let expected = if max_waiting == 0 {
                    CapacityError::QueueFull
                } else {
                    CapacityError::WaitTimeout
                };
                assert_eq!(error.downcast_ref::<CapacityError>(), Some(&expected));
            }
        }
        assert!(mock.calls.lock().is_empty());
    }

    async fn wait_for_backoff(mock: &Mock) {
        timeout(Duration::from_secs(2), async {
            loop {
                if mock.calls.lock().len() == 1 {
                    let context = mock
                        .provider
                        .token_manager
                        .acquire_context(None)
                        .await
                        .unwrap();
                    // 全局上限为 1，能够获取任何账号就证明退避已释放名额。
                    drop(context);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backoff must release the credential reservation");
    }

    #[tokio::test]
    async fn api_and_mcp_respect_retry_after_and_release_reservations_during_backoff() {
        let cases = [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::SERVICE_UNAVAILABLE,
        ]
        .into_iter()
        .flat_map(|status| {
            ["api", "stream", "mcp"]
                .into_iter()
                .map(move |kind| (status, kind))
        });
        // Client construction can synchronously load system TLS roots. Finish
        // setup before starting any 2-second backoff assertion on this runtime.
        let mut prepared = Vec::new();
        for (status, kind) in cases {
            prepared.push((mock(status, Some("3"), true).await, kind));
        }
        futures::future::join_all(prepared.into_iter().map(|(mock, kind)| async move {
            let provider = mock.provider.clone();
            let task =
                tokio::spawn(
                    async move { call(&provider, kind).await.unwrap().text().await.unwrap() },
                );
            wait_for_backoff(&mock).await;
            assert_eq!(
                timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap(),
                "success"
            );
            let calls = mock.calls.lock();
            assert_eq!(calls.len(), 2);
            assert!(calls[1].duration_since(calls[0]) >= Duration::from_secs(3));
            let snapshot = mock.provider.token_manager.snapshot();
            assert_eq!(snapshot.available, 2);
            assert!(
                snapshot
                    .entries
                    .iter()
                    .all(|entry| entry.failure_count == 0)
            );
        }))
        .await;
    }

    #[tokio::test]
    async fn excessive_retry_after_returns_without_early_retry_or_disabling_credentials() {
        for kind in ["api", "stream", "mcp"] {
            let mock = mock(
                StatusCode::TOO_MANY_REQUESTS,
                Some("9999999999999999999999999"),
                false,
            )
            .await;
            let result = timeout(Duration::from_secs(2), call(&mock.provider, kind))
                .await
                .unwrap();
            let error = match result {
                Ok(_) => panic!("expected upstream rejection"),
                Err(error) => error,
            };
            let upstream = error.downcast_ref::<UpstreamApiError>().unwrap();
            assert_eq!(upstream.status, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(upstream.body, "upstream busy");
            assert_eq!(mock.calls.lock().len(), 1);
            assert_eq!(mock.provider.token_manager.snapshot().available, 2);
            assert_eq!(
                mock.provider
                    .token_manager
                    .acquire_context(None)
                    .await
                    .unwrap()
                    .id,
                2
            );
        }
    }

    #[tokio::test]
    async fn redirect_statuses_stop_without_retry_or_penalizing_credentials() {
        for status in [301, 302, 303, 307, 308] {
            let mock = mock(StatusCode::from_u16(status).unwrap(), None, false).await;
            for (index, kind) in ["api", "stream", "mcp"].into_iter().enumerate() {
                let result = timeout(Duration::from_secs(2), call(&mock.provider, kind))
                    .await
                    .unwrap();
                let error = result.err().expect("redirects must fail");
                assert_eq!(
                    error
                        .downcast_ref::<UpstreamApiError>()
                        .unwrap()
                        .status
                        .as_u16(),
                    status
                );
                assert_eq!(mock.calls.lock().len(), index + 1);
                let snapshot = mock.provider.token_manager.snapshot();
                assert_eq!(snapshot.concurrency.in_flight, 0);
                assert_eq!(snapshot.available, 2);
                assert!(
                    snapshot
                        .entries
                        .iter()
                        .all(|entry| entry.failure_count == 0)
                );
            }
        }
    }

    #[tokio::test]
    async fn cancelling_retry_wait_keeps_credentials_available() {
        for kind in ["api", "stream", "mcp"] {
            let mock = mock(StatusCode::SERVICE_UNAVAILABLE, Some("60"), false).await;
            let provider = mock.provider.clone();
            let task = tokio::spawn(async move { call(&provider, kind).await });
            wait_for_backoff(&mock).await;
            task.abort();
            assert!(task.await.is_err_and(|error| error.is_cancelled()));
            assert_eq!(mock.calls.lock().len(), 1);
            assert_eq!(
                mock.provider
                    .token_manager
                    .acquire_context(None)
                    .await
                    .unwrap()
                    .id,
                1
            );
        }
    }
}
