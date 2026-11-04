//! Anthropic API Handler 函数

use std::convert::Infallible;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::interval;
use uuid::Uuid;

use super::converter::{ConversionError, convert_request};
use super::middleware::AppState;
use super::stream::{BufferedStreamContext, SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, Message, MessagesRequest, Model,
    ModelsResponse, OutputConfig, Thinking,
};
use super::upstream::{ResponsePolicy, response_events};
use super::websearch;

/// 将 KiroProvider 错误映射为 HTTP 响应
pub(super) fn map_provider_error(err: Error) -> Response {
    crate::request_log::error_details(format!("{err:#}"));
    let err_str = err.to_string();

    if err.is::<crate::kiro::token_manager::CapacityError>() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            Json(ErrorResponse::new("overloaded_error", err_str)),
        )
            .into_response();
    }

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %err, "上游拒绝请求：上下文窗口已满（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Context window is full. Reduce conversation history, system prompt, or tools.",
            )),
        )
            .into_response();
    }

    // 单次输入太长（请求体本身超出上游限制）
    if err_str.contains("Input is too long") {
        tracing::warn!(error = %err, "上游拒绝请求：输入过长（不应重试）");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(
                "invalid_request_error",
                "Input is too long. Reduce the size of your messages.",
            )),
        )
            .into_response();
    }
    if let Some(upstream) = err.downcast_ref::<crate::kiro::provider::UpstreamApiError>()
        && upstream.status == StatusCode::BAD_REQUEST
    {
        tracing::warn!(error = %err, "Kiro 拒绝请求参数");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", err_str)),
        )
            .into_response();
    }
    tracing::error!("Kiro API 调用失败: {}", err);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models() -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let models = vec![
        Model {
            id: "claude-fable-5-1".to_string(),
            object: "model".to_string(),
            created: 1756684800, // Sep 1, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Fable 5.1".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-fable-5-1"),
        },
        Model {
            id: "claude-fable-5-1-thinking".to_string(),
            object: "model".to_string(),
            created: 1756684800, // Sep 1, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Fable 5.1 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-fable-5-1-thinking"),
        },
        Model {
            id: "claude-opus-5-5".to_string(),
            object: "model".to_string(),
            created: 1790035200, // Sep 22, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-opus-5-5"),
        },
        Model {
            id: "claude-opus-5-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1790035200, // Sep 22, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-opus-5-5-thinking"),
        },
        Model {
            id: "claude-opus-5".to_string(),
            object: "model".to_string(),
            created: 1782777600, // Jun 30, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-opus-5"),
        },
        Model {
            id: "claude-opus-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1782777600, // Jun 30, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-opus-5-thinking"),
        },
        Model {
            id: "claude-sonnet-5".to_string(),
            object: "model".to_string(),
            created: 1782777600, // Jun 30, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-sonnet-5"),
        },
        Model {
            id: "claude-sonnet-5-5".to_string(),
            object: "model".to_string(),
            created: 1790035200,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-sonnet-5-5"),
        },
        Model {
            id: "claude-sonnet-5-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1790035200,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-sonnet-5-5-thinking"),
        },
        Model {
            id: "claude-sonnet-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1782777600, // Jun 30, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-sonnet-5-thinking"),
        },
        Model {
            id: "claude-opus-4-8".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-opus-4-8"),
        },
        Model {
            id: "claude-opus-4-8-thinking".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-opus-4-8-thinking"),
        },
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size("claude-fable-5-1"),
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size("claude-opus-4-7-thinking"),
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size("claude-opus-4-6"),
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size("claude-opus-4-6-thinking"),
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size("claude-sonnet-4-6"),
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size(
                "claude-sonnet-4-6-thinking",
            ),
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size("claude-opus-4-5-20251101"),
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size(
                "claude-opus-4-5-20251101-thinking",
            ),
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size(
                "claude-sonnet-4-5-20250929",
            ),
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size(
                "claude-sonnet-4-5-20250929-thinking",
            ),
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size(
                "claude-haiku-4-5-20251001",
            ),
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
            max_input_tokens: super::converter::get_context_window_size(
                "claude-haiku-4-5-20251001-thinking",
            ),
        },
        Model {
            id: "claude-haiku-5-5".to_string(),
            object: "model".to_string(),
            created: 1793318400,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 5.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-haiku-5-5"),
        },
        Model {
            id: "claude-haiku-5-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1793318400,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 5.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 128_000,
            max_input_tokens: model_input_limit("claude-haiku-5-5-thinking"),
        },
    ];

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// GET /v1/models/{model_id}: expose the mapped context and output limits.
pub async fn get_model(axum::extract::Path(model_id): axum::extract::Path<String>) -> Response {
    let Some(kiro_model) = super::converter::map_model(&model_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse::new("not_found_error", "Model not found")),
        )
            .into_response();
    };
    let context = super::converter::get_context_window_size(&model_id);
    let detail = super::types::ModelDetail {
        id: model_id.clone(),
        object: "model".into(),
        created_at: chrono::Utc::now().to_rfc3339(),
        display_name: kiro_model,
        type_name: "model".into(),
        max_input_tokens: context,
        max_tokens: 128_000,
        capabilities: model_capabilities(&model_id),
    };
    Json(detail).into_response()
}

fn model_capabilities(model: &str) -> serde_json::Value {
    let mapped = super::converter::map_model(model).unwrap_or_default();
    // Haiku 4.5 has no native thinking support; Haiku 5.5 supports adaptive thinking and effort.
    let thinking = !mapped.contains("haiku") || mapped == "claude-haiku-5.5";
    let generation_5 = matches!(
        mapped.as_str(),
        "claude-sonnet-5"
            | "claude-sonnet-5.5"
            | "claude-opus-5"
            | "claude-opus-5.5"
            | "claude-fable-5.1"
            | "claude-haiku-5.5"
    );
    let adaptive = generation_5
        || mapped.contains("fable")
        || mapped == "claude-opus-4.6"
        || mapped == "claude-sonnet-4.6";
    let modern_effort = generation_5
        || mapped.contains("fable")
        || mapped == "claude-opus-4.7"
        || mapped == "claude-opus-4.8";
    let effort: Vec<&str> = if mapped.contains("haiku") && mapped != "claude-haiku-5.5" {
        vec![]
    } else if modern_effort {
        vec!["low", "medium", "high", "xhigh"]
    } else {
        vec!["low", "medium", "high"]
    };
    let max_effort = modern_effort || mapped == "claude-opus-4.6" || mapped == "claude-sonnet-4.6";
    let mut capabilities = serde_json::Map::new();
    capabilities.insert(
        "thinking".into(),
        json!({"supported":thinking, "types": {
            "adaptive":adaptive, "enabled":thinking, "disabled":true
        }}),
    );
    if !effort.is_empty() || max_effort {
        let mut levels = serde_json::Map::new();
        for level in effort.into_iter().chain(max_effort.then_some("max")) {
            levels.insert(level.into(), json!({"supported":true}));
        }
        levels.insert("supported".into(), json!(true));
        capabilities.insert("effort".into(), serde_json::Value::Object(levels));
    } else {
        capabilities.insert("effort".into(), json!({"supported":false}));
    }
    // These endpoints are proxy implemented; model retrieval follows the Anthropic shape.
    capabilities.insert(
        "context_management".into(),
        json!({"supported":true, "compact_20260112":{"supported":true}}),
    );
    serde_json::Value::Object(capabilities)
}

fn model_input_limit(model: &str) -> i32 {
    super::converter::get_context_window_size(model)
}

/// 两个入口以及压缩后续请求共享相同的校验规则。
pub(super) fn validate_request(payload: &MessagesRequest) -> Result<(), &'static str> {
    if payload.max_tokens <= 0 {
        return Err("max_tokens must be greater than zero");
    }
    validate_message_input(&payload.model, &payload.messages)?;
    if let Some(thinking) = &payload.thinking
        && !matches!(
            thinking.thinking_type.as_str(),
            "enabled" | "adaptive" | "disabled"
        )
    {
        return Err("thinking.type must be enabled, adaptive, or disabled");
    }
    if let Some(config) = &payload.output_config {
        let allowed = model_capabilities(&payload.model)["effort"][&config.effort]["supported"]
            .as_bool()
            == Some(true);
        if !config.effort.is_empty() && !allowed {
            return Err("output_config.effort is unsupported for the requested model");
        }
    }
    Ok(())
}

/// 在历史规范化、远程计数及工具分流之前校验，避免错误内容被静默丢弃。
fn validate_message_input(model: &str, messages: &[Message]) -> Result<(), &'static str> {
    if model.trim().is_empty() {
        return Err("model must not be empty");
    }
    if messages.is_empty() {
        return Err("messages must contain at least one message");
    }
    for message in messages {
        if !matches!(message.role.as_str(), "user" | "assistant") {
            return Err("message.role must be user or assistant");
        }
        match &message.content {
            serde_json::Value::String(_) => {}
            serde_json::Value::Array(blocks) => {
                for block in blocks {
                    let Some(kind) = block
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .filter(|kind| !kind.trim().is_empty())
                    else {
                        return Err("content blocks must be objects with a non-empty type string");
                    };
                    if kind == "text"
                        && !block.get("text").is_some_and(serde_json::Value::is_string)
                    {
                        return Err("text blocks must contain a text string");
                    }
                }
            }
            _ => return Err("message.content must be a string or an array of content blocks"),
        }
    }
    Ok(())
}

/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    crate::request_log::request_metadata(&payload.model, payload.stream);
    if let Err(message) = validate_request(&payload) {
        return super::compaction::invalid_request(message);
    }

    if let Err(message) = super::compaction::normalize_history(&mut payload.messages) {
        return super::compaction::invalid_request(message);
    }
    if payload.compaction.is_some() || payload.context_management.is_some() {
        return super::compaction::handle_request(state, payload, false).await;
    }
    super::compaction::prepare_legacy_request(&mut payload);
    post_messages_inner(State(state), JsonExtractor(payload)).await
}

pub(super) async fn post_messages_inner(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    crate::request_log::request_metadata(&payload.model, payload.stream);
    if let Err(message) = validate_request(&payload) {
        return super::compaction::invalid_request(message);
    }

    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages request"
    );
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // Model aliases provide defaults; explicit request parameters take precedence.
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            &payload.model,
            &payload.system,
            &payload.messages,
            &payload.tools,
        )
        .await as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: super::converter::additional_model_request_fields(
            &payload,
        ),
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!(request_bytes = request_body.len(), "Kiro request prepared");

    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        &payload.model,
        &payload.system,
        &payload.messages,
        &payload.tools,
    )
    .await as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        // 流式响应
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            extract_thinking,
            tool_name_map,
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建流处理上下文
    let mut ctx =
        StreamContext::new_with_thinking(model, input_tokens, thinking_enabled, tool_name_map);

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流
    let events = response_events(
        provider,
        request_body.to_owned(),
        true,
        response,
        ResponsePolicy::Conversation { thinking_enabled },
    );
    let stream = create_sse_stream(events, ctx, initial_events);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

fn upstream_error_event(error: impl std::fmt::Display) -> SseEvent {
    crate::request_log::error_details(&error);
    tracing::error!("Invalid upstream response: {error}");
    SseEvent::new(
        "error",
        json!({
            "type": "error",
            "error": {"type": "api_error", "message": format!("Upstream response failed: {error}")}
        }),
    )
}

fn upstream_error_response(error: Error) -> Response {
    if error.is::<crate::kiro::token_manager::CapacityError>() {
        return map_provider_error(error);
    }
    crate::request_log::error_details(format!("{error:#}"));
    tracing::error!("Invalid upstream response: {error:#}");
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("Upstream response failed: {error:#}"),
        )),
    )
        .into_response()
}

/// 创建 SSE 事件流；上游失败时发送 error，且不发送正常完成事件。
fn create_sse_stream(
    events: futures::stream::BoxStream<'static, anyhow::Result<Vec<Event>>>,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|event| Ok(Bytes::from(event.to_sse_string()))),
    );
    let processing_stream = stream::unfold(
        (
            events,
            ctx,
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
        ),
        |(mut event_stream, mut ctx, finished, mut ping_interval)| async move {
            if finished {
                return None;
            }
            let (events, finished) = tokio::select! {
                batch = event_stream.next() => match batch {
                    Some(Ok(events)) => (
                        events.iter().flat_map(|event| ctx.process_kiro_event(event)).collect(),
                        false,
                    ),
                    Some(Err(error)) => (vec![upstream_error_event(format!("{error:#}"))], true),
                    None => (ctx.generate_final_events(), true),
                },
                _ = ping_interval.tick() => {
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    return Some((stream::iter(bytes), (event_stream, ctx, false, ping_interval)));
                }
            };
            let bytes: Vec<Result<Bytes, Infallible>> = events
                .into_iter()
                .map(|event| Ok(Bytes::from(event.to_sse_string())))
                .collect();
            Some((
                stream::iter(bytes),
                (event_stream, ctx, finished, ping_interval),
            ))
        },
    )
    .flatten();
    initial_stream.chain(processing_stream)
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    let mut event_stream = response_events(
        provider,
        request_body.to_owned(),
        false,
        response,
        ResponsePolicy::Conversation { thinking_enabled },
    );

    let mut text_content = String::new();
    let mut reasoning_text = String::new();
    let mut reasoning_signature: Option<String> = None;
    let mut redacted_reasoning: Vec<String> = Vec::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut has_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // 从 contextUsageEvent 计算的实际输入 tokens
    let mut context_input_tokens: Option<i32> = None;

    // 收集工具调用的增量 JSON
    let mut tool_json_buffers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    while let Some(batch) = event_stream.next().await {
        let events = match batch {
            Ok(events) => events,
            Err(error) => return upstream_error_response(error),
        };
        for event in events {
            match event {
                Event::AssistantResponse(resp) => {
                    text_content.push_str(&resp.content);
                }
                Event::ReasoningContent(reasoning) => {
                    if thinking_enabled {
                        reasoning_text.push_str(&reasoning.text);
                        if reasoning.signature.is_some() {
                            reasoning_signature = reasoning.signature;
                        }
                        if let Some(data) = reasoning.redacted_content {
                            redacted_reasoning.push(data);
                        }
                    }
                }
                Event::ToolUse(tool_use) => {
                    has_tool_use = true;

                    // 累积工具的 JSON 输入
                    let buffer = tool_json_buffers
                        .entry(tool_use.tool_use_id.clone())
                        .or_default();
                    buffer.push_str(&tool_use.input);

                    // 如果是完整的工具调用，添加到列表
                    if tool_use.stop {
                        let input: serde_json::Value = if buffer.is_empty() {
                            serde_json::json!({})
                        } else {
                            match serde_json::from_str(buffer) {
                                Ok(input) => input,
                                Err(error) => return upstream_error_response(error.into()),
                            }
                        };
                        tool_json_buffers.remove(&tool_use.tool_use_id);

                        let original_name = tool_name_map
                            .get(&tool_use.name)
                            .cloned()
                            .unwrap_or_else(|| tool_use.name.clone());

                        tool_uses.push(json!({
                            "type": "tool_use",
                            "id": tool_use.tool_use_id,
                            "name": original_name,
                            "input": input
                        }));
                    }
                }
                Event::ContextUsage(context_usage) => {
                    // 从上下文使用百分比计算实际的 input_tokens
                    let window_size = get_context_window_size(model);
                    let actual_input_tokens = (context_usage.context_usage_percentage
                        * (window_size as f64)
                        / 100.0) as i32;
                    context_input_tokens = Some(actual_input_tokens);
                    // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                    if context_usage.context_usage_percentage >= 100.0 {
                        stop_reason = "model_context_window_exceeded".to_string();
                    }
                    tracing::debug!(
                        "收到 contextUsageEvent: {}%, 计算 input_tokens: {}",
                        context_usage.context_usage_percentage,
                        actual_input_tokens
                    );
                }
                Event::Exception { exception_type, .. }
                    if exception_type == "ContentLengthExceededException" =>
                {
                    stop_reason = "max_tokens".to_string();
                }
                _ => {}
            }
        }
    }
    // 确定 stop_reason
    if has_tool_use && stop_reason == "end_turn" {
        stop_reason = "tool_use".to_string();
    }

    // 构建响应内容
    let mut content: Vec<serde_json::Value> = Vec::new();
    if !reasoning_text.is_empty() {
        let mut block = json!({"type":"thinking", "thinking":reasoning_text});
        if let Some(signature) = reasoning_signature {
            block["signature"] = json!(signature);
        }
        content.push(block);
    }
    content.extend(
        redacted_reasoning
            .into_iter()
            .map(|data| json!({"type":"redacted_thinking", "data":data})),
    );

    if thinking_enabled {
        // 从完整文本中提取 thinking 块
        let (thinking, remaining_text) =
            super::stream::extract_thinking_from_complete_text(&text_content);

        if let Some(thinking_text) = thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": thinking_text
            }));
        }

        if !remaining_text.is_empty() {
            content.push(json!({
                "type": "text",
                "text": remaining_text
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    }

    content.extend(tool_uses);

    // 估算输出 tokens
    let output_tokens = token::estimate_output_tokens(&content);

    // 使用从 contextUsageEvent 计算的 input_tokens，如果没有则使用估算值
    let final_input_tokens = context_input_tokens.unwrap_or(input_tokens);

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": final_input_tokens,
            "output_tokens": output_tokens
        }
    });

    crate::request_log::response_usage(&response_body);
    (StatusCode::OK, Json(response_body)).into_response()
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    if payload.thinking.is_some() {
        return;
    }

    let is_adaptive_thinking = (model_lower.contains("opus")
        && (model_lower.contains("4-6")
            || model_lower.contains("4.6")
            || model_lower.contains("4-7")
            || model_lower.contains("4.7")
            || model_lower.contains("4-8")
            || model_lower.contains("4.8")))
        || (model_lower.contains("sonnet")
            && (model_lower.contains("4-6") || model_lower.contains("4.6")))
        || model_lower.contains("haiku-5")
        || model_lower.contains("sonnet-5")
        || model_lower.contains("opus-5")
        || model_lower.contains("fable");

    let thinking_type = if is_adaptive_thinking {
        "adaptive"
    } else {
        "enabled"
    };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });
    if is_adaptive_thinking && payload.output_config.is_none() {
        payload.output_config = Some(OutputConfig {
            effort: super::converter::default_effort(&payload.model).to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(mut payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    crate::request_log::request_metadata(&payload.model, false);
    if let Err(message) = validate_message_input(&payload.model, &payload.messages) {
        return super::compaction::invalid_request(message);
    }
    if let Err(message) = super::compaction::normalize_history(&mut payload.messages) {
        return super::compaction::invalid_request(message);
    }
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        &payload.model,
        &payload.system,
        &payload.messages,
        &payload.tools,
    )
    .await as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
    .into_response()
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会等待 kiro 端返回 contextUsageEvent 后再发送 message_start
/// - message_start 中的 input_tokens 是从 contextUsageEvent 计算的准确值
pub async fn post_messages_cc(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    crate::request_log::request_metadata(&payload.model, payload.stream);
    if let Err(message) = validate_request(&payload) {
        return super::compaction::invalid_request(message);
    }

    if let Err(message) = super::compaction::normalize_history(&mut payload.messages) {
        return super::compaction::invalid_request(message);
    }
    if payload.compaction.is_some() || payload.context_management.is_some() {
        return super::compaction::handle_request(state, payload, true).await;
    }
    super::compaction::prepare_legacy_request(&mut payload);
    post_messages_cc_inner(State(state), JsonExtractor(payload)).await
}

pub(super) async fn post_messages_cc_inner(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    crate::request_log::request_metadata(&payload.model, payload.stream);
    if let Err(message) = validate_request(&payload) {
        return super::compaction::invalid_request(message);
    }

    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            &payload.model,
            &payload.system,
            &payload.messages,
            &payload.tools,
        )
        .await as i32;

        return websearch::handle_websearch_request(provider, &payload, input_tokens).await;
    }

    // 转换请求
    let conversion_result = match convert_request(&payload) {
        Ok(result) => result,
        Err(e) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // 构建 Kiro 请求（profile_arn 由 provider 层根据实际凭据注入）
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: super::converter::additional_model_request_fields(
            &payload,
        ),
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!(request_bytes = request_body.len(), "Kiro request prepared");

    // 估算输入 tokens
    let input_tokens = token::count_all_tokens(
        &payload.model,
        &payload.system,
        &payload.messages,
        &payload.tools,
    )
    .await as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;

    if payload.stream {
        if !state.cc_buffered_stream {
            return handle_stream_request(
                provider,
                &request_body,
                &payload.model,
                input_tokens,
                thinking_enabled,
                tool_name_map,
            )
            .await;
        }
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            thinking_enabled,
            tool_name_map,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            input_tokens,
            extract_thinking,
            tool_name_map,
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用从 contextUsageEvent 计算的正确 input_tokens 生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    estimated_input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let response = match provider.call_api_stream(request_body).await {
        Ok(resp) => resp,
        Err(e) => return map_provider_error(e),
    };

    // 创建缓冲流处理上下文
    let ctx = BufferedStreamContext::new(
        model,
        estimated_input_tokens,
        thinking_enabled,
        tool_name_map,
    );

    // 创建缓冲 SSE 流
    let events = response_events(
        provider,
        request_body.to_owned(),
        true,
        response,
        ResponsePolicy::Conversation { thinking_enabled },
    );
    let stream = create_buffered_sse_stream(events, ctx);

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 缓冲完整事件并更正 usage；失败时丢弃缓冲内容，仅返回 error。
fn create_buffered_sse_stream(
    events: futures::stream::BoxStream<'static, anyhow::Result<Vec<Event>>>,
    ctx: BufferedStreamContext,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    stream::unfold(
        (
            events,
            ctx,
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
        ),
        |(mut event_stream, mut ctx, finished, mut ping_interval)| async move {
            if finished {
                return None;
            }
            loop {
                let events = tokio::select! {
                    biased;
                    _ = ping_interval.tick() => {
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (event_stream, ctx, false, ping_interval)));
                    }
                    batch = event_stream.next() => match batch {
                        Some(Ok(events)) => {
                            for event in events {
                                ctx.process_and_buffer(&event);
                            }
                            continue;
                        }
                        Some(Err(error)) => vec![upstream_error_event(format!("{error:#}"))],
                        None => ctx.finish_and_get_all_events(),
                    }
                };
                let bytes: Vec<Result<Bytes, Infallible>> = events.into_iter()
                    .map(|event| Ok(Bytes::from(event.to_sse_string())))
                    .collect();
                return Some((stream::iter(bytes), (event_stream, ctx, true, ping_interval)));
            }
        },
    ).flatten()
}

#[cfg(test)]
mod model_capability_tests {
    use super::model_capabilities;

    #[tokio::test]
    async fn overload_errors_keep_503_and_retry_after_for_initial_and_buffered_responses() {
        use crate::kiro::token_manager::CapacityError;
        use axum::{body::to_bytes, http::StatusCode};
        for error in [CapacityError::QueueFull, CapacityError::WaitTimeout] {
            for response in [
                super::map_provider_error(anyhow::Error::new(error).context("provider")),
                super::upstream_error_response(error.into()),
            ] {
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(response.headers()["retry-after"], "1");
                let body = to_bytes(response.into_body(), 4096).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"]["type"], "overloaded_error");
            }
        }
    }

    #[test]
    fn haiku_45_reports_no_thinking_or_effort() {
        let capabilities = model_capabilities("claude-haiku-4-5-20251001");
        assert_eq!(capabilities["thinking"]["supported"], false);
        assert_eq!(capabilities["thinking"]["types"]["enabled"], false);
        assert_eq!(capabilities["thinking"]["types"]["adaptive"], false);
        assert_eq!(capabilities["effort"]["supported"], false);
        assert_eq!(
            super::model_input_limit("claude-haiku-4-5-20251001"),
            200_000
        );
    }

    #[test]
    fn haiku_55_reports_one_million_context_adaptive_thinking_and_effort() {
        let capabilities = model_capabilities("claude-haiku-5-5");
        assert_eq!(capabilities["thinking"]["supported"], true);
        assert_eq!(capabilities["thinking"]["types"]["adaptive"], true);
        assert_eq!(capabilities["effort"]["supported"], true);
        assert_eq!(super::model_input_limit("claude-haiku-5-5"), 1_000_000);
    }
}
