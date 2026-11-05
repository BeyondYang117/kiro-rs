//! Anthropic API 中间件

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use crate::common::auth;
use crate::kiro::provider::KiroProvider;

use super::types::ErrorResponse;

/// 应用共享状态
#[derive(Clone)]
pub struct AppState {
    /// API 密钥
    pub api_key: String,
    /// Kiro Provider（可选，用于实际 API 调用）
    /// 内部使用 MultiTokenManager，已支持线程安全的多凭据管理
    pub kiro_provider: Option<Arc<KiroProvider>>,
    /// 是否开启非流式响应的 thinking 块提取
    pub extract_thinking: bool,
    /// 缓冲 Claude Code SSE，默认保持准确初始 usage 的行为。
    pub cc_buffered_stream: bool,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new(api_key: impl Into<String>, extract_thinking: bool) -> Self {
        Self {
            api_key: api_key.into(),
            kiro_provider: None,
            extract_thinking,
            cc_buffered_stream: true,
        }
    }

    /// 设置 KiroProvider
    pub fn with_kiro_provider(mut self, provider: KiroProvider) -> Self {
        self.kiro_provider = Some(Arc::new(provider));
        self
    }
}

/// API Key 认证中间件
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    match auth::extract_api_key(&request) {
        Some(key) if auth::constant_time_eq(&key, &state.api_key) => next.run(request).await,
        _ => {
            let error = ErrorResponse::authentication_error();
            (StatusCode::UNAUTHORIZED, Json(error)).into_response()
        }
    }
}

/// CORS 中间件层
///
/// **安全说明**：当前配置允许所有来源（Any），这是为了支持公开 API 服务。
/// 如果需要更严格的安全控制，请根据实际需求配置具体的允许来源、方法和头信息。
///
/// # 配置说明
/// - `allow_origin(Any)`: 允许任何来源的请求
/// - `allow_methods(Any)`: 允许任何 HTTP 方法
/// - `allow_headers(Any)`: 允许任何请求头
pub fn cors_layer() -> tower_http::cors::CorsLayer {
    use tower_http::cors::{Any, CorsLayer};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers([axum::http::HeaderName::from_static("x-request-id")])
}

/// A slot spans JSON parsing, conversion, all retries and the complete response body.
#[derive(Clone)]
pub(super) struct RequestGuard {
    pub slots: Arc<tokio::sync::Semaphore>,
    pub timeout: std::time::Duration,
}

pub(super) async fn limit_request(
    State(guard): State<RequestGuard>,
    request: Request<Body>,
    next: Next,
) -> Response {
    use futures::StreamExt;
    use tokio::time::{Instant, timeout_at};
    let permit = match guard.slots.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "1")],
                Json(ErrorResponse::new(
                    "rate_limit_error",
                    "Request admission limit reached",
                )),
            )
                .into_response();
        }
    };
    let deadline = Instant::now() + guard.timeout;
    let response = match timeout_at(deadline, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(ErrorResponse::new(
                    "api_error",
                    "Request total deadline exceeded",
                )),
            )
                .into_response();
        }
    };
    let sse = response
        .headers()
        .get("content-type")
        .is_some_and(|value| value.as_bytes().starts_with(b"text/event-stream"));
    let (parts, body) = response.into_parts();
    let stream = futures::stream::unfold(
        (body.into_data_stream().boxed(), Some(permit), false),
        move |(mut body, mut permit, done)| async move {
            if done {
                return None;
            }
            let result = match timeout_at(deadline, body.next()).await {
                Ok(Some(chunk)) => return Some((chunk, (body, permit, false))),
                Ok(None) => return None,
                Err(_) if sse => Ok(bytes::Bytes::from(
                    super::stream::SseEvent::new(
                        "error",
                        serde_json::json!({"type":"error", "error":{
                            "type":"api_error", "message":"Request total deadline exceeded"
                        }}),
                    )
                    .to_sse_string(),
                )),
                Err(_) => Err(axum::Error::new(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Request total deadline exceeded",
                ))),
            };
            // Cancel upstream immediately; do not retain capacity until the next poll.
            permit.take();
            let empty = futures::stream::empty::<Result<bytes::Bytes, axum::Error>>().boxed();
            Some((result, (empty, permit, true)))
        },
    );
    Response::from_parts(parts, Body::from_stream(stream))
}
