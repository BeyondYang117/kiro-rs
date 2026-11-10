//! Anthropic API Handler 函数

use std::convert::Infallible;

use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Query, State, rejection::QueryRejection},
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
    ModelsResponse, OutputConfig, SystemMessage, Thinking,
};
use super::upstream::{ResponsePolicy, response_events};
use super::websearch;

/// 将 KiroProvider 错误映射为 HTTP 响应
pub(super) fn map_provider_error(err: Error) -> Response {
    crate::request_log::error_details(format!("{err:#}"));
    let err_str = err.to_string();
    let log_str = crate::request_log::redact(&err_str);

    if err.is::<crate::kiro::model_controls::ModelParameterError>() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", err_str)),
        )
            .into_response();
    }

    if err.is::<crate::kiro::token_manager::CapacityError>()
        || err.is::<crate::kiro::retry_budget::RetryBudgetExceeded>()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "1")],
            Json(ErrorResponse::new("overloaded_error", err_str)),
        )
            .into_response();
    }

    if err
        .downcast_ref::<crate::kiro::provider::UpstreamApiError>()
        .is_some_and(|upstream| upstream.status.is_redirection())
    {
        tracing::warn!(error = %log_str, "拒绝上游重定向");
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "api_error",
                "Upstream redirect rejected. Configure the final endpoint URL.",
            )),
        )
            .into_response();
    }

    // 上下文窗口满了（对话历史累积超出模型上下文窗口限制）
    if err_str.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        tracing::warn!(error = %log_str, "上游拒绝请求：上下文窗口已满（不应重试）");
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
        tracing::warn!(error = %log_str, "上游拒绝请求：输入过长（不应重试）");
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
        tracing::warn!(error = %log_str, "Kiro 拒绝请求参数");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", err_str)),
        )
            .into_response();
    }
    tracing::error!("Kiro API 调用失败: {}", log_str);
    (
        StatusCode::BAD_GATEWAY,
        Json(ErrorResponse::new(
            "api_error",
            format!("上游 API 调用失败: {}", err),
        )),
    )
        .into_response()
}

/// Pagination over the local compatibility list; no upstream discovery is performed.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ModelsQuery {
    limit: Option<usize>,
    after_id: Option<String>,
    before_id: Option<String>,
}

/// GET /v1/models and /cc/v1/models, preserving the existing order and full-list default.
pub async fn get_models(query: Result<Query<ModelsQuery>, QueryRejection>) -> Response {
    let query = match query {
        Ok(Query(query)) => query,
        Err(_) => return model_list_error("Invalid model pagination parameters"),
    };
    let limit = query.limit.unwrap_or(1000);
    if !(1..=1000).contains(&limit) {
        return model_list_error("limit must be between 1 and 1000");
    }
    if query.after_id.is_some() && query.before_id.is_some() {
        return model_list_error("after_id and before_id cannot be used together");
    }
    let models = local_models();
    let (start, end, has_more) = if let Some(cursor) = &query.before_id {
        let Some(end) = models.iter().position(|model| &model.id == cursor) else {
            return model_list_error("before_id must refer to a listed model ID");
        };
        let start = end.saturating_sub(limit);
        (start, end, start > 0)
    } else {
        let start = if let Some(cursor) = &query.after_id {
            let Some(position) = models.iter().position(|model| &model.id == cursor) else {
                return model_list_error("after_id must refer to a listed model ID");
            };
            position + 1
        } else {
            0
        };
        let end = (start + limit).min(models.len());
        (start, end, end < models.len())
    };
    let data: Vec<_> = models.into_iter().skip(start).take(end - start).collect();
    Json(ModelsResponse {
        object: "list".into(),
        has_more,
        first_id: data.first().map(|model| model.id.clone()),
        last_id: data.last().map(|model| model.id.clone()),
        data,
    })
    .into_response()
}

fn model_list_error(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse::new("invalid_request_error", message)),
    )
        .into_response()
}

fn local_models() -> Vec<Model> {
    let mut models = vec![
        Model {
            id: "claude-fable-5-1".to_string(),
            object: "model".to_string(),
            created: 1756684800, // Sep 1, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Fable 5.1".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-fable-5-1"),
            max_input_tokens: model_input_limit("claude-fable-5-1"),
        },
        Model {
            id: "claude-fable-5-1-thinking".to_string(),
            object: "model".to_string(),
            created: 1756684800, // Sep 1, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Fable 5.1 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-fable-5-1-thinking"),
            max_input_tokens: model_input_limit("claude-fable-5-1-thinking"),
        },
        Model {
            id: "claude-opus-5-5".to_string(),
            object: "model".to_string(),
            created: 1790035200, // Sep 22, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-5-5"),
            max_input_tokens: model_input_limit("claude-opus-5-5"),
        },
        Model {
            id: "claude-opus-5-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1790035200, // Sep 22, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-5-5-thinking"),
            max_input_tokens: model_input_limit("claude-opus-5-5-thinking"),
        },
        Model {
            id: "claude-opus-5".to_string(),
            object: "model".to_string(),
            created: 1782777600, // Jun 30, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-5"),
            max_input_tokens: model_input_limit("claude-opus-5"),
        },
        Model {
            id: "claude-opus-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1782777600, // Jun 30, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-5-thinking"),
            max_input_tokens: model_input_limit("claude-opus-5-thinking"),
        },
        Model {
            id: "claude-sonnet-5".to_string(),
            object: "model".to_string(),
            created: 1782777600, // Jun 30, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-sonnet-5"),
            max_input_tokens: model_input_limit("claude-sonnet-5"),
        },
        Model {
            id: "claude-sonnet-5-5".to_string(),
            object: "model".to_string(),
            created: 1790035200,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-sonnet-5-5"),
            max_input_tokens: model_input_limit("claude-sonnet-5-5"),
        },
        Model {
            id: "claude-sonnet-5-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1790035200,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-sonnet-5-5-thinking"),
            max_input_tokens: model_input_limit("claude-sonnet-5-5-thinking"),
        },
        Model {
            id: "claude-sonnet-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1782777600, // Jun 30, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-sonnet-5-thinking"),
            max_input_tokens: model_input_limit("claude-sonnet-5-thinking"),
        },
        Model {
            id: "claude-opus-4-8".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-4-8"),
            max_input_tokens: model_input_limit("claude-opus-4-8"),
        },
        Model {
            id: "claude-opus-4-8-thinking".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-4-8-thinking"),
            max_input_tokens: model_input_limit("claude-opus-4-8-thinking"),
        },
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-4-7"),
            max_input_tokens: super::converter::get_context_window_size("claude-fable-5-1"),
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-4-7-thinking"),
            max_input_tokens: super::converter::get_context_window_size("claude-opus-4-7-thinking"),
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-4-6"),
            max_input_tokens: super::converter::get_context_window_size("claude-opus-4-6"),
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-opus-4-6-thinking"),
            max_input_tokens: super::converter::get_context_window_size("claude-opus-4-6-thinking"),
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-sonnet-4-6"),
            max_input_tokens: super::converter::get_context_window_size("claude-sonnet-4-6"),
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-sonnet-4-6-thinking"),
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
            max_tokens: super::converter::get_max_output_tokens("claude-opus-4-5-20251101"),
            max_input_tokens: super::converter::get_context_window_size("claude-opus-4-5-20251101"),
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens(
                "claude-opus-4-5-20251101-thinking",
            ),
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
            max_tokens: super::converter::get_max_output_tokens("claude-sonnet-4-5-20250929"),
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
            max_tokens: super::converter::get_max_output_tokens(
                "claude-sonnet-4-5-20250929-thinking",
            ),
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
            max_tokens: super::converter::get_max_output_tokens("claude-haiku-4-5-20251001"),
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
            max_tokens: super::converter::get_max_output_tokens(
                "claude-haiku-4-5-20251001-thinking",
            ),
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
            max_tokens: super::converter::get_max_output_tokens("claude-haiku-5-5"),
            max_input_tokens: model_input_limit("claude-haiku-5-5"),
        },
        Model {
            id: "claude-haiku-5-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1793318400,
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 5.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: super::converter::get_max_output_tokens("claude-haiku-5-5-thinking"),
            max_input_tokens: model_input_limit("claude-haiku-5-5-thinking"),
        },
    ];

    for (id, name, provider) in [
        ("gpt-5.6-sol", "GPT-5.6 Sol", "openai"),
        ("gpt-5.6-terra", "GPT-5.6 Terra", "openai"),
        ("gpt-5.6-luna", "GPT-5.6 Luna", "openai"),
        ("kimi-k3", "Kimi K3", "moonshot"),
    ] {
        models.push(Model {
            id: id.into(),
            object: "model".into(),
            created: 1793318400,
            owned_by: provider.into(),
            display_name: name.into(),
            model_type: "chat".into(),
            max_tokens: super::converter::get_max_output_tokens(id),
            max_input_tokens: model_input_limit(id),
        });
    }

    models
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
    // Resolve aliases to the same static metadata as the model list. A retrieval
    // must not report the current time as the model's creation date.
    let metadata = local_models()
        .into_iter()
        .find(|model| super::converter::map_model(&model.id).as_deref() == Some(&kiro_model))
        .expect("mapped models have local metadata");
    let context = super::converter::get_context_window_size(&model_id);
    let detail = super::types::ModelDetail {
        id: model_id.clone(),
        object: "model".into(),
        created_at: chrono::DateTime::from_timestamp(metadata.created, 0)
            .expect("local model timestamp is valid")
            .to_rfc3339(),
        display_name: kiro_model,
        type_name: "model".into(),
        max_input_tokens: context,
        max_tokens: super::converter::get_max_output_tokens(&model_id),
        capabilities: model_capabilities(&model_id),
    };
    Json(detail).into_response()
}

fn model_capabilities(model: &str) -> serde_json::Value {
    if super::converter::is_non_claude(model) {
        let gpt = super::converter::is_gpt(model);
        return json!({
            "thinking":{"supported":false,"types":{"enabled":false,"adaptive":false,"disabled":true,"between_tools":false}},
            "effort": if gpt { json!({"supported":true,"schema_path":"reasoning.effort","default":"high",
                "none":{"supported":true},"low":{"supported":true},"medium":{"supported":true},
                "high":{"supported":true},"xhigh":{"supported":true},"max":{"supported":true}}) }
                else { json!({"supported":false}) },
            "context_management":{"supported":true,"compact_20260112":{"supported":true}}
        });
    }
    let mapped = super::converter::map_model(model).unwrap_or_default();
    // Haiku 4.5 supports budgeted thinking; Haiku 5.5 also supports adaptive thinking.
    let thinking = !mapped.is_empty();
    let generation_5 = matches!(
        mapped.as_str(),
        "claude-sonnet-5"
            | "claude-sonnet-5.5"
            | "claude-opus-5"
            | "claude-opus-5.5"
            | "claude-fable-5.1"
            | "claude-haiku-5.5"
    );
    let modern_effort = generation_5
        || mapped.contains("fable")
        || mapped == "claude-opus-4.7"
        || mapped == "claude-opus-4.8";
    let adaptive = super::converter::supports_adaptive_thinking(model);
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
            "adaptive":adaptive, "enabled":thinking, "disabled":true,
            "between_tools":mapped == "claude-sonnet-5.5"
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
fn validate_and_normalize_request(payload: &mut MessagesRequest) -> Result<(), String> {
    for name in ["temperature", "top_p", "top_k"] {
        if payload
            .extra_controls
            .remove(name)
            .is_some_and(|v| !v.is_null())
        {
            tracing::warn!(
                stage = "request_normalization",
                parameter = name,
                "Unsupported sampling parameter ignored; using Kiro's default sampling behavior. Remove this parameter to suppress the warning."
            );
        }
    }
    match payload.extra_controls.get("stop_sequences") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Array(sequences)) if sequences.is_empty() => {}
        Some(serde_json::Value::Array(sequences))
            if sequences.iter().all(serde_json::Value::is_string) =>
        {
            return Err(
                "Non-empty stop_sequences is not supported by this Kiro proxy; omit it or use []"
                    .into(),
            );
        }
        Some(_) => return Err("stop_sequences must be an array of strings or null".into()),
    }
    // Clients may send [] by default; it has the same semantics as an omitted field.
    payload.extra_controls.remove("stop_sequences");
    if let Some(choice) = &payload.tool_choice {
        let kind = choice.get("type").and_then(serde_json::Value::as_str);
        if choice
            .get("disable_parallel_tool_use")
            .is_some_and(|value| value != false)
        {
            return Err("tool_choice.disable_parallel_tool_use is not supported".into());
        }
        match kind {
            Some("auto") => {}
            Some("none") => payload.tools = None,
            Some("any" | "tool") => {
                return Err("Forced tool_choice is not supported; use auto or none".into());
            }
            _ => return Err("tool_choice.type must be auto or none".into()),
        }
    }
    if payload.max_tokens <= 0 {
        return Err("max_tokens must be greater than zero".into());
    }
    normalize_message_input(&payload.model, &mut payload.system, &mut payload.messages)?;
    let non_claude = super::converter::is_non_claude(&payload.model);
    let gpt = super::converter::is_gpt(&payload.model);
    if let Some(reasoning) = payload
        .extra_controls
        .get("reasoning")
        .filter(|value| !value.is_null())
    {
        if !gpt {
            return Err("reasoning.effort is only supported for GPT-5.6 models".into());
        }
        let Some(object) = reasoning.as_object() else {
            return Err("reasoning must be an object containing effort".into());
        };
        if object.keys().any(|key| key != "effort") {
            return Err("Only reasoning.effort is supported".into());
        }
        let effort = object.get("effort").and_then(serde_json::Value::as_str);
        if !effort.is_some_and(|value| {
            matches!(value, "none" | "low" | "medium" | "high" | "xhigh" | "max")
        }) {
            return Err("reasoning.effort must be none, low, medium, high, xhigh, or max".into());
        }
        if let Some(config) = &payload.output_config
            && !config.effort.is_empty()
            && Some(config.effort.as_str()) != effort
        {
            return Err(
                "reasoning.effort and output_config.effort must agree when both are provided"
                    .into(),
            );
        }
    }
    if non_claude
        && payload.thinking.as_ref().is_some_and(|thinking| {
            thinking.thinking_type != "disabled" || thinking.display.is_some()
        })
    {
        return Err("Claude thinking controls are unsupported for this model; GPT-5.6 accepts reasoning.effort".into());
    }
    if let Some(thinking) = &mut payload.thinking {
        if !matches!(
            thinking.thinking_type.as_str(),
            "enabled" | "adaptive" | "disabled" | "between_tools"
        ) {
            return Err(
                "thinking.type must be enabled, adaptive, disabled, or between_tools".into(),
            );
        }
        if thinking.thinking_type == "between_tools"
            && super::converter::map_model(&payload.model).as_deref() != Some("claude-sonnet-5.5")
        {
            return Err(
                "thinking.type between_tools is only supported for Claude Sonnet 5.5".into(),
            );
        }
        if thinking.thinking_type == "adaptive"
            && !super::converter::supports_adaptive_thinking(&payload.model)
        {
            return Err("thinking.type adaptive is unsupported for the requested model".into());
        }
        if thinking.thinking_type == "enabled" && thinking.budget_tokens <= 0 {
            return Err("thinking.budget_tokens must be greater than zero".into());
        }
        if thinking
            .display
            .as_deref()
            .is_some_and(|display| !matches!(display, "summarized" | "omitted"))
        {
            // Display is an optional presentation hint. New clients may send
            // values outside Kiro's enum; omit the hint rather than rejecting
            // an otherwise valid generation or forwarding an invalid control.
            let received_display = crate::request_log::public_error_message(
                &thinking
                    .display
                    .as_deref()
                    .unwrap_or_default()
                    .chars()
                    .take(128)
                    .collect::<String>(),
            );
            tracing::warn!(
                stage = "request_normalization",
                parameter = "thinking.display",
                received = %received_display,
                "Unsupported thinking.display ignored; using the default display. Use summarized or omitted, or remove thinking.display."
            );
            thinking.display = None;
        }
    }
    if let Some(config) = &payload.output_config {
        let allowed = model_capabilities(&payload.model)["effort"][&config.effort]["supported"]
            .as_bool()
            == Some(true);
        if !config.effort.is_empty() && !allowed {
            return Err("output_config.effort is unsupported for the requested model".into());
        }
        if super::converter::map_model(&payload.model).as_deref() == Some("claude-sonnet-5.5")
            && payload.thinking.as_ref().is_some_and(|thinking| {
                matches!(
                    thinking.thinking_type.as_str(),
                    "disabled" | "between_tools"
                )
            })
            && matches!(config.effort.as_str(), "xhigh" | "max")
        {
            return Err(
                "Claude Sonnet 5.5 between_tools thinking supports low, medium, or high effort"
                    .into(),
            );
        }
    }
    Ok(())
}

/// 先完整校验，再把兼容客户端内嵌的指令移入顶层 system。
/// 普通消息仍然保留 user/assistant 角色，避免转换器静默丢弃未知角色。
fn normalize_message_input(
    model: &str,
    system: &mut Option<Vec<SystemMessage>>,
    messages: &mut Vec<Message>,
) -> Result<(), String> {
    if model.trim().is_empty() {
        return Err("model must not be empty".into());
    }
    if messages.is_empty() {
        return Err("messages must contain at least one message".into());
    }
    for (index, message) in messages.iter().enumerate() {
        let instruction = matches!(message.role.as_str(), "system" | "developer");
        if !instruction && !matches!(message.role.as_str(), "user" | "assistant") {
            let role: String = message.role.chars().take(64).collect();
            return Err(format!(
                "messages[{index}].role is unsupported: {}; expected user or assistant (system/developer text instructions are also accepted)",
                json!(role)
            ));
        }
        match &message.content {
            serde_json::Value::String(_) => {}
            serde_json::Value::Array(blocks) => {
                for (block_index, block) in blocks.iter().enumerate() {
                    let Some(kind) = block
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .filter(|kind| !kind.trim().is_empty())
                    else {
                        return Err(format!(
                            "messages[{index}].content[{block_index}] must be an object with a non-empty type string"
                        ));
                    };
                    if instruction && kind != "text" {
                        return Err(format!(
                            "messages[{index}].content[{block_index}]: system/developer messages only support text blocks"
                        ));
                    }
                    if kind == "text"
                        && !block.get("text").is_some_and(serde_json::Value::is_string)
                    {
                        return Err(format!(
                            "messages[{index}].content[{block_index}] must contain a text string"
                        ));
                    }
                }
            }
            _ => {
                return Err(format!(
                    "messages[{index}].content must be a string or an array of content blocks"
                ));
            }
        }
    }
    if !messages
        .iter()
        .any(|message| matches!(message.role.as_str(), "user" | "assistant"))
    {
        return Err("messages must contain at least one user or assistant message in addition to system/developer instructions".into());
    }
    messages.retain(|message| {
        if !matches!(message.role.as_str(), "system" | "developer") {
            return true;
        }
        let instructions = system.get_or_insert_with(Vec::new);
        match &message.content {
            serde_json::Value::String(text) => {
                instructions.push(SystemMessage { text: text.clone() })
            }
            serde_json::Value::Array(blocks) => {
                instructions.extend(blocks.iter().map(|block| {
                    SystemMessage {
                        text: block["text"]
                            .as_str()
                            .expect("validated text block")
                            .to_owned(),
                    }
                }));
            }
            _ => unreachable!("validated message content"),
        }
        false
    });
    Ok(())
}

/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    payload.stream |= state.force_stream;
    crate::request_log::request_metadata(&payload.model, payload.stream);
    if let Err(message) = validate_and_normalize_request(&mut payload) {
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
    if let Err(message) = validate_and_normalize_request(&mut payload) {
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

    apply_thinking_defaults(&mut payload);

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
                ConversionError::InvalidToolFormat(_) => ("invalid_request_error", e.to_string()),
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

fn upstream_error_event(error: Error) -> SseEvent {
    crate::request_log::error_details(format!("{error:#}"));
    let message =
        crate::request_log::public_error_message(&format!("Upstream response failed: {error:#}"));
    tracing::error!("{message}");
    let error_type = if error.is::<super::upstream::ToolCallError>()
        || error.is::<super::upstream::UpstreamRefusalError>()
        || error.is::<crate::kiro::model_controls::ModelParameterError>()
    {
        "invalid_request_error"
    } else {
        "api_error"
    };
    SseEvent::new(
        "error",
        json!({
            "type": "error",
            "error": {"type": error_type, "message": message}
        }),
    )
}

pub(super) fn upstream_error_response(error: Error) -> Response {
    if error.is::<crate::kiro::token_manager::CapacityError>()
        || error.is::<crate::kiro::model_controls::ModelParameterError>()
        || error.is::<crate::kiro::retry_budget::RetryBudgetExceeded>()
    {
        return map_provider_error(error);
    }
    // Replaying a malformed/truncated tool call or an explicit refusal will not repair it.
    // Avoid SDK retries intended for transient 5xx errors.
    let (status, error_type) = if error.is::<super::upstream::ToolCallError>()
        || error.is::<super::upstream::UpstreamRefusalError>()
    {
        (StatusCode::BAD_REQUEST, "invalid_request_error")
    } else {
        (StatusCode::BAD_GATEWAY, "api_error")
    };
    crate::request_log::error_details(format!("{error:#}"));
    tracing::error!(
        "Invalid upstream response: {}",
        crate::request_log::redact(&format!("{error:#}"))
    );
    (
        status,
        Json(ErrorResponse::new(
            error_type,
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
                    Some(Err(error)) => (vec![upstream_error_event(error)], true),
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
    let mut reported_usage = crate::kiro::model::events::ReportedUsage::default();

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
                Event::Metadata(metadata) => {
                    reported_usage.observe(&metadata);
                    if metadata.output_limited() {
                        stop_reason = "max_tokens".to_string();
                    }
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
    let mut response_body = json!({
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

    reported_usage.apply(&mut response_body["usage"]);

    crate::request_log::response_usage(&response_body);
    (StatusCode::OK, Json(response_body)).into_response()
}

/// Model aliases supply thinking defaults; explicit client controls take priority.
/// Adaptive requests share model-specific effort defaults, with or without an alias.
fn apply_thinking_defaults(payload: &mut MessagesRequest) {
    if super::converter::is_non_claude(&payload.model) {
        return;
    }
    if payload.thinking.is_none() && payload.model.to_lowercase().contains("thinking") {
        let adaptive = model_capabilities(&payload.model)["thinking"]["types"]["adaptive"]
            .as_bool()
            == Some(true);
        payload.thinking = Some(Thinking {
            thinking_type: if adaptive { "adaptive" } else { "enabled" }.to_string(),
            budget_tokens: 20000,
            display: None,
        });
    }

    if payload
        .thinking
        .as_ref()
        .is_some_and(|thinking| thinking.thinking_type == "adaptive")
    {
        let config = payload.output_config.get_or_insert_with(|| OutputConfig {
            effort: String::new(),
        });
        if config.effort.is_empty() {
            config.effort = super::converter::default_effort(&payload.model).to_string();
        }
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    JsonExtractor(mut payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    crate::request_log::request_metadata(&payload.model, false);
    if let Err(message) =
        normalize_message_input(&payload.model, &mut payload.system, &mut payload.messages)
    {
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
    payload.stream |= state.force_stream;
    crate::request_log::request_metadata(&payload.model, payload.stream);
    if let Err(message) = validate_and_normalize_request(&mut payload) {
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
    if let Err(message) = validate_and_normalize_request(&mut payload) {
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

    apply_thinking_defaults(&mut payload);

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
                ConversionError::InvalidToolFormat(_) => ("invalid_request_error", e.to_string()),
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
    )
    .with_limits(provider.request_limits().clone());

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
                                if let Err(error) = ctx.process_and_buffer(&event) {
                                    let bytes = Bytes::from(upstream_error_event(error).to_sse_string());
                                    return Some((stream::iter(vec![Ok(bytes)]),
                                        (event_stream, ctx, true, ping_interval)));
                                }
                            }
                            continue;
                        }
                        Some(Err(error)) => vec![upstream_error_event(error)],
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

fn json_rejection(error: axum::extract::rejection::JsonRejection) -> Response {
    let response = ErrorResponse::new("invalid_request_error", error.body_text());
    crate::request_log::error_details(&response.error.message);
    tracing::warn!(stage = "request_deserialization", error = %response.error.message,
        "Request body rejected locally; no upstream request was sent");
    (error.status(), Json(response)).into_response()
}

pub(super) async fn post_messages_route(
    state: State<AppState>,
    payload: Result<JsonExtractor<MessagesRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    match payload {
        Ok(payload) => post_messages(state, payload).await,
        Err(error) => json_rejection(error),
    }
}

pub(super) async fn post_messages_cc_route(
    state: State<AppState>,
    payload: Result<JsonExtractor<MessagesRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    match payload {
        Ok(payload) => post_messages_cc(state, payload).await,
        Err(error) => json_rejection(error),
    }
}

pub(super) async fn count_tokens_route(
    payload: Result<JsonExtractor<CountTokensRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    match payload {
        Ok(payload) => count_tokens(payload).await.into_response(),
        Err(error) => json_rejection(error),
    }
}

#[cfg(test)]
mod model_capability_tests {
    use super::model_capabilities;
    use serde_json::json;

    #[test]
    fn gpt_and_kimi_controls_are_validated_before_dispatch() {
        for (model, controls, valid) in [
            (
                "gpt-5.6-sol",
                serde_json::json!({"reasoning":{"effort":"none"}}),
                true,
            ),
            (
                "gpt-5.6-terra",
                serde_json::json!({"output_config":{"effort":"max"}}),
                true,
            ),
            (
                "gpt-5.6-luna",
                serde_json::json!({"reasoning":{"effort":"low"},"output_config":{"effort":"low"}}),
                true,
            ),
            (
                "gpt-5.6-sol",
                serde_json::json!({"reasoning":{"effort":"low"},"output_config":{"effort":"high"}}),
                false,
            ),
            (
                "gpt-5.6-sol",
                serde_json::json!({"reasoning":{"effort":"ultra"}}),
                false,
            ),
            (
                "gpt-5.6-sol",
                serde_json::json!({"reasoning":{"effort":null}}),
                false,
            ),
            (
                "gpt-5.6-sol",
                serde_json::json!({"reasoning":"high"}),
                false,
            ),
            (
                "gpt-5.6-sol",
                serde_json::json!({"reasoning":{"effort":"high","summary":"auto"}}),
                false,
            ),
            (
                "gpt-5.6-sol",
                serde_json::json!({"thinking":{"type":"enabled"}}),
                false,
            ),
            (
                "gpt-5.6-sol",
                serde_json::json!({"thinking":{"type":"disabled"}}),
                true,
            ),
            ("kimi-k3", serde_json::json!({}), true),
            (
                "kimi-k3",
                serde_json::json!({"reasoning":{"effort":"high"}}),
                false,
            ),
            (
                "kimi-k3",
                serde_json::json!({"output_config":{"effort":"high"}}),
                false,
            ),
            (
                "kimi-k3",
                serde_json::json!({"thinking":{"type":"adaptive"}}),
                false,
            ),
            (
                "claude-opus-5.5",
                serde_json::json!({"reasoning":{"effort":"high"}}),
                false,
            ),
        ] {
            let mut value = serde_json::json!({"model":model,"max_tokens":4096,"messages":[{"role":"user","content":"hi"}]});
            value
                .as_object_mut()
                .unwrap()
                .extend(controls.as_object().unwrap().clone());
            let mut request = serde_json::from_value(value).unwrap();
            assert_eq!(
                super::validate_and_normalize_request(&mut request).is_ok(),
                valid,
                "{model}: {controls}"
            );
            if valid {
                super::apply_thinking_defaults(&mut request);
                assert!(
                    !request
                        .thinking
                        .as_ref()
                        .is_some_and(|thinking| thinking.is_enabled())
                );
            }
        }
        assert_eq!(
            model_capabilities("gpt-5.6-sol")["effort"]["schema_path"],
            "reasoning.effort"
        );
        assert_eq!(model_capabilities("kimi-k3")["effort"]["supported"], false);
    }

    #[test]
    fn adaptive_effort_defaults_match_claude_code_and_preserve_explicit_values() {
        for (model, default) in [
            ("claude-haiku-5-5", "medium"),
            ("claude-opus-5-5", "medium"),
            ("claude-sonnet-5-5", "medium"),
            ("claude-opus-4-7", "xhigh"),
            ("claude-opus-4-8", "high"),
            ("claude-sonnet-5", "high"),
            ("claude-fable-5-1", "high"),
            ("claude-opus-4-6", "high"),
        ] {
            assert_eq!(
                model_capabilities(model)["thinking"]["types"]["adaptive"],
                true
            );
            for alias in [false, true] {
                for effort in [None, Some(""), Some("low")] {
                    let mut value = serde_json::json!({
                        "model":if alias {format!("{model}-thinking")} else {model.to_owned()},
                        "max_tokens":1024, "messages":[{"role":"user", "content":"hi"}]
                    });
                    if !alias {
                        value["thinking"] = serde_json::json!({"type":"adaptive"});
                    }
                    if let Some(effort) = effort {
                        value["output_config"] = serde_json::json!({"effort":effort});
                    }
                    let mut payload = serde_json::from_value(value).unwrap();
                    for _ in 0..2 {
                        super::apply_thinking_defaults(&mut payload);
                        assert_eq!(payload.thinking.as_ref().unwrap().thinking_type, "adaptive");
                        assert_eq!(
                            payload.output_config.as_ref().unwrap().effort,
                            effort.filter(|value| !value.is_empty()).unwrap_or(default)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn effort_does_not_enable_thinking_or_replace_explicit_budgets() {
        for (model, kind) in [
            ("claude-sonnet-5-5", None),
            ("claude-sonnet-5-5-thinking", Some("disabled")),
            ("claude-sonnet-5-5-thinking", Some("enabled")),
        ] {
            let mut value = serde_json::json!({"model":model, "max_tokens":1024,
                "messages":[{"role":"user", "content":"hi"}], "output_config":{"effort":"low"}});
            if let Some(kind) = kind {
                value["thinking"] = serde_json::json!({"type":kind, "budget_tokens":4096});
            }
            let mut payload = serde_json::from_value(value).unwrap();
            super::apply_thinking_defaults(&mut payload);
            assert_eq!(
                payload
                    .thinking
                    .as_ref()
                    .map(|thinking| thinking.thinking_type.as_str()),
                kind
            );
            if let Some(thinking) = payload.thinking {
                assert_eq!(thinking.budget_tokens, 4096);
            }
            assert_eq!(payload.output_config.unwrap().effort, "low");
        }
        let mut payload = serde_json::from_value(serde_json::json!({
            "model":"claude-haiku-5-5", "max_tokens":1024, "messages":[{"role":"user", "content":"hi"}]
        })).unwrap();
        super::apply_thinking_defaults(&mut payload);
        assert!(payload.thinking.is_none());
        assert!(payload.output_config.is_none());
    }

    #[test]
    fn sampling_controls_are_ignored_without_changing_other_controls() {
        for name in ["temperature", "top_p", "top_k"] {
            for value in [
                serde_json::Value::Null,
                json!(0),
                json!(1),
                json!(0.7),
                json!(-1),
                json!("future-value"),
                json!({"unknown":true}),
                json!([]),
            ] {
                let mut request = json!({"model":"claude-opus-4-5-20251101", "max_tokens":4096,
                    "messages":[{"role":"user", "content":"hi"}],
                    "thinking":{"type":"enabled", "budget_tokens":2048},
                    "output_config":{"effort":"high"}, "future_control":true});
                request[name] = value;
                let mut payload: super::MessagesRequest = serde_json::from_value(request).unwrap();
                // Outer and inner handlers both normalize the same request.
                for _ in 0..2 {
                    super::validate_and_normalize_request(&mut payload).unwrap();
                    assert!(!payload.extra_controls.contains_key(name));
                    assert_eq!(payload.extra_controls["future_control"], true);
                    assert_eq!(payload.max_tokens, 4096);
                    let thinking = payload.thinking.as_ref().unwrap();
                    assert_eq!(thinking.thinking_type, "enabled");
                    assert_eq!(thinking.budget_tokens, 2048);
                    assert_eq!(payload.output_config.as_ref().unwrap().effort, "high");
                }
            }
        }
    }

    #[test]
    fn thinking_display_falls_back_without_changing_reasoning_controls() {
        for kind in ["enabled", "adaptive", "disabled"] {
            for display in [
                None,
                Some("summarized"),
                Some("omitted"),
                Some(""),
                Some("full"),
                Some("future-value"),
            ] {
                let mut value = serde_json::json!({"model":"claude-haiku-5-5", "max_tokens":4096,
                    "messages":[{"role":"user", "content":"hi"}],
                    "thinking":{"type":kind, "budget_tokens":2048},
                    "output_config":{"effort":"high"}});
                if let Some(display) = display {
                    value["thinking"]["display"] = display.into();
                }
                let mut payload: super::MessagesRequest = serde_json::from_value(value).unwrap();
                // Both outer and inner handlers normalize; the fallback must be idempotent.
                for _ in 0..2 {
                    super::validate_and_normalize_request(&mut payload).unwrap();
                    let thinking = payload.thinking.as_ref().unwrap();
                    assert_eq!(thinking.thinking_type, kind);
                    assert_eq!(thinking.budget_tokens, 2048);
                    assert_eq!(
                        thinking.display.as_deref(),
                        display.filter(|v| matches!(*v, "summarized" | "omitted"))
                    );
                    assert_eq!(payload.output_config.as_ref().unwrap().effort, "high");
                    let fields =
                        super::super::converter::additional_model_request_fields(&payload).unwrap();
                    assert_eq!(
                        fields["thinking"]
                            .get("display")
                            .and_then(serde_json::Value::as_str),
                        thinking.display.as_deref()
                    );
                }
            }
        }
    }

    #[test]
    fn instruction_normalization_is_idempotent_and_keeps_conversation_blocks() {
        let mut payload: super::MessagesRequest = serde_json::from_value(serde_json::json!({
            "model":"claude-sonnet-5-5", "max_tokens":1024, "system":"top-level",
            "messages":[
                {"role":"system", "content":"system instructions"},
                {"role":"user", "content":"question"},
                {"role":"developer", "content":[{"type":"text", "text":"developer instructions"}]},
                {"role":"assistant", "content":[{"type":"tool_use", "id":"t1", "name":"read", "input":{}}]},
                {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"t1", "content":"done"}]}
            ]
        })).unwrap();
        let original = payload.messages.clone();
        for _ in 0..2 {
            super::validate_and_normalize_request(&mut payload).unwrap();
            assert_eq!(
                payload
                    .system
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|part| part.text.as_str())
                    .collect::<Vec<_>>(),
                ["top-level", "system instructions", "developer instructions"]
            );
            assert_eq!(
                serde_json::to_value(&payload.messages).unwrap(),
                serde_json::to_value([&original[1], &original[3], &original[4]]).unwrap()
            );
        }
    }

    #[test]
    fn role_errors_identify_the_message_without_copying_its_content() {
        for role in ["tool", "function", "unknown", ""] {
            let mut payload: super::MessagesRequest = serde_json::from_value(serde_json::json!({
                "model":"claude-sonnet-5-5", "max_tokens":1024,
                "messages":[{"role":"user", "content":"hi"}, {"role":role, "content":"private-body-sentinel"}]
            })).unwrap();
            let error = super::validate_and_normalize_request(&mut payload).unwrap_err();
            assert!(error.contains("messages[1].role"), "{error}");
            assert!(
                error.contains(&serde_json::json!(role).to_string()),
                "{error}"
            );
            assert!(!error.contains("private-body-sentinel"));
        }
    }

    #[tokio::test]
    async fn catalog_parameter_errors_keep_400_for_initial_and_buffered_responses() {
        for response in [
            super::map_provider_error(
                anyhow::Error::new(crate::kiro::model_controls::ModelParameterError(
                    "max_tokens",
                ))
                .context("provider"),
            ),
            super::upstream_error_response(
                crate::kiro::model_controls::ModelParameterError("output_config.effort").into(),
            ),
        ] {
            assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
            assert!(!response.headers().contains_key("retry-after"));
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], "invalid_request_error");
        }
        let event = super::upstream_error_event(
            crate::kiro::model_controls::ModelParameterError("thinking.type").into(),
        );
        assert!(event.to_sse_string().contains("invalid_request_error"));
    }

    #[tokio::test]
    async fn overload_errors_keep_503_and_retry_after_for_initial_and_buffered_responses() {
        use crate::kiro::token_manager::CapacityError;
        use axum::{body::to_bytes, http::StatusCode};
        for error in [
            CapacityError::QueueFull,
            CapacityError::WaitTimeout,
            CapacityError::ProtectionUnavailable,
        ] {
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

    #[tokio::test]
    async fn redirect_errors_keep_502_regardless_of_body_or_error_context() {
        use axum::http::StatusCode;
        for status in [301, 302, 303, 307, 308] {
            let error = crate::kiro::provider::UpstreamApiError {
                status: StatusCode::from_u16(status).unwrap(),
                body: "CONTENT_LENGTH_EXCEEDS_THRESHOLD Input is too long private-value".into(),
            };
            let response = super::map_provider_error(anyhow::Error::new(error).context("provider"));
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["error"]["type"], "api_error");
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("redirect")
            );
            assert!(!String::from_utf8_lossy(&bytes).contains("private-value"));
        }
    }

    #[tokio::test]
    async fn provider_and_stream_errors_redact_credentials_and_bound_messages() {
        use axum::body::to_bytes;
        let message = format!(
            "upstream failed: {{\"accessToken\":\"private-access\",\"kiroApiKey\":\"private-kiro\"}} Bearer private-bearer https://user:private-password@proxy.test ksk_private-key {}",
            "中".repeat(6000),
        );
        let errors = [
            super::map_provider_error(anyhow::anyhow!(message.clone())),
            super::upstream_error_response(anyhow::anyhow!(message.clone())),
            super::map_provider_error(
                crate::kiro::provider::UpstreamApiError {
                    status: axum::http::StatusCode::BAD_REQUEST,
                    body: message.clone(),
                }
                .into(),
            ),
        ];
        for response in errors {
            let body: serde_json::Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 32 * 1024).await.unwrap())
                    .unwrap();
            let error = body["error"]["message"].as_str().unwrap();
            assert!(!error.contains("private-"), "{error}");
            assert!(!error.contains("ksk_private"));
            assert!(error.chars().count() <= 4096);
            assert!(error.contains("upstream failed"));
        }
        let event = super::upstream_error_event(anyhow::anyhow!(message));
        let encoded = event.to_sse_string();
        assert!(encoded.starts_with("event: error\n"));
        assert!(!encoded.contains("private-"));
        assert!(!encoded.contains("ksk_private"));
        assert!(encoded.len() < 16 * 1024);
    }

    #[test]
    fn haiku_45_reports_budgeted_thinking_without_adaptive_or_effort() {
        let capabilities = model_capabilities("claude-haiku-4-5-20251001");
        assert_eq!(capabilities["thinking"]["supported"], true);
        assert_eq!(capabilities["thinking"]["types"]["enabled"], true);
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

    #[tokio::test]
    async fn model_list_and_detail_expose_sonnet_55_and_opus_output_caps() {
        use axum::{body::to_bytes, response::IntoResponse};
        let list = super::get_models(Ok(axum::extract::Query(super::ModelsQuery::default())))
            .await
            .into_response();
        let list: serde_json::Value =
            serde_json::from_slice(&to_bytes(list.into_body(), 100_000).await.unwrap()).unwrap();
        for (model, expected) in [
            ("claude-sonnet-5-5", 64_000),
            ("claude-opus-4-7", 64_000),
            ("claude-opus-4-8", 128_000),
        ] {
            let entry = list["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["id"] == model)
                .unwrap();
            assert_eq!(entry["max_tokens"], expected);
            let detail = super::get_model(axum::extract::Path(model.to_owned())).await;
            let detail: serde_json::Value =
                serde_json::from_slice(&to_bytes(detail.into_body(), 100_000).await.unwrap())
                    .unwrap();
            assert_eq!(detail["max_tokens"], expected);
            if model == "claude-sonnet-5-5" {
                assert_eq!(
                    detail["capabilities"]["thinking"]["types"]["between_tools"],
                    true
                );
            }
        }
    }
}
