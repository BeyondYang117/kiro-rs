//! Anthropic API 路由配置

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

use crate::kiro::provider::KiroProvider;

use super::{
    handlers::{
        count_tokens_route, get_model, get_models, post_messages_cc_route, post_messages_route,
    },
    middleware::{AppState, auth_middleware, cors_layer},
};

/// 请求体最大大小限制 (50MB)
const MAX_BODY_SIZE: usize = 50 * 1024 * 1024;

/// 创建 Anthropic API 路由
///
/// # 端点
/// - `GET /v1/models` - 获取可用模型列表
/// - `POST /v1/messages` - 创建消息（对话）
/// - `POST /v1/messages/count_tokens` - 计算 token 数量
///
/// # 认证
/// 所有 `/v1` 路径需要 API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
///
/// # 参数
/// - `api_key`: API 密钥，用于验证客户端请求
/// - `kiro_provider`: 可选的 KiroProvider，用于调用上游 API
/// - `extract_thinking`: 是否从非流式响应中提取 thinking 块
/// - `cc_buffered_stream`: 是否缓冲 Claude Code 流式响应以更正初始 usage
pub fn create_router_with_provider(
    api_key: impl Into<String>,
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    cc_buffered_stream: bool,
) -> Router {
    let limits = kiro_provider
        .as_ref()
        .map(|provider| provider.request_limits().clone())
        .unwrap_or_default();
    let guard = super::middleware::RequestGuard {
        slots: std::sync::Arc::new(tokio::sync::Semaphore::new(limits.max_concurrent_requests)),
        timeout: std::time::Duration::from_secs(limits.total_timeout_seconds),
    };
    let mut state = AppState::new(api_key, extract_thinking);
    state.cc_buffered_stream = cc_buffered_stream;
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }

    // 需要认证的 /v1 路由
    let v1_routes = Router::new()
        .route("/models", get(get_models))
        .route("/models/{model_id}", get(get_model))
        .route("/messages", post(post_messages_route))
        .route("/messages/count_tokens", post(count_tokens_route))
        .layer(middleware::from_fn_with_state(
            guard.clone(),
            super::middleware::limit_request,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 需要认证的 /cc/v1 路由（Claude Code 兼容端点）
    // 与 /v1 的区别：流式响应会等待 contextUsageEvent 后再发送 message_start
    let cc_v1_routes = Router::new()
        .route("/models", get(get_models))
        .route("/models/{model_id}", get(get_model))
        .route("/messages", post(post_messages_cc_route))
        .route("/messages/count_tokens", post(count_tokens_route))
        .layer(middleware::from_fn_with_state(
            guard.clone(),
            super::middleware::limit_request,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .nest("/v1", v1_routes)
        .nest("/cc/v1", cc_v1_routes)
        .layer(cors_layer())
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}
