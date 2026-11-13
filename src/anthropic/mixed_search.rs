//! Execute native search inside a generation turn while leaving client tools pending.

use std::{collections::HashSet, convert::Infallible, time::Duration};

use axum::{
    Json,
    body::{Body, to_bytes},
    http::header,
    response::{IntoResponse, Response},
};
use futures::stream;
use serde_json::{Value, json};

use super::{
    compaction, converter, handlers,
    middleware::AppState,
    stream::SseEvent,
    types::{Message, MessagesRequest, Tool},
    websearch,
};
use crate::kiro::model::requests::kiro::KiroRequest;

const DEFAULT_MAX_USES: usize = 5;
const MAX_USES: usize = 100;

pub(super) fn is_mixed_search(request: &MessagesRequest) -> bool {
    request
        .tools
        .as_ref()
        .is_some_and(|tools| tools.len() > 1 && tools.iter().any(is_native_search))
}

fn is_native_search(tool: &Tool) -> bool {
    tool.tool_type.as_deref() == Some("web_search_20250305")
}

pub(super) async fn handle(state: AppState, mut request: MessagesRequest) -> Response {
    let search = match prepare(&mut request) {
        Ok(search) => search,
        Err(response) => return *response,
    };
    if !request.stream {
        return match run(state, request, search).await {
            Ok(message) => Json(message).into_response(),
            Err(response) => *response,
        };
    }
    // Buffer internal rounds so no native search is exposed as an executable client
    // call. Keep long MCP/generation rounds alive using the existing SSE heartbeat.
    let events = stream::unfold(
        (Box::pin(run(state, request, search)), false),
        |(mut future, done)| async move {
            if done {
                return None;
            }
            tokio::select! {
                result = &mut future => {
                    let text = match result {
                        Ok(message) => compaction::message_events(&message),
                        Err(response) => {
                            let error = to_bytes(response.into_body(), 1024 * 1024).await.ok()
                                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                                .unwrap_or_else(|| json!({"error":{"type":"api_error","message":"Mixed search failed"}}));
                            SseEvent::new("error", json!({"type":"error","error":error["error"],"usage":error["usage"]})).to_sse_string()
                        }
                    };
                    Some((Ok::<_, Infallible>(text), (future, true)))
                }
                _ = tokio::time::sleep(Duration::from_secs(25)) => {
                    Some((Ok(SseEvent::new("ping", json!({"type":"ping"})).to_sse_string()), (future, false)))
                }
            }
        },
    );
    Response::builder()
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(events))
        .unwrap()
}

fn invalid(message: impl Into<String>) -> Box<Response> {
    Box::new(compaction::invalid_request(message))
}

fn native_options(tool: &Tool) -> Result<usize, Box<Response>> {
    if tool.name != "web_search" || !tool.input_schema.is_empty() {
        return Err(invalid(
            "Native web search requires name web_search and no input_schema",
        ));
    }
    for key in ["allowed_domains", "blocked_domains"] {
        if let Some(value) = tool.options.get(key) {
            if !value.as_array().is_some_and(|items| {
                items.iter().all(|domain| {
                    domain.as_str().is_some_and(|domain| {
                        !domain.is_empty() && !domain.contains(['/', ' ', ':'])
                    })
                })
            }) {
                return Err(invalid(format!(
                    "web_search.{key} must contain domain names"
                )));
            }
        }
    }
    if tool
        .options
        .get("allowed_domains")
        .and_then(Value::as_array)
        .is_some_and(|v| !v.is_empty())
        && tool
            .options
            .get("blocked_domains")
            .and_then(Value::as_array)
            .is_some_and(|v| !v.is_empty())
    {
        return Err(invalid(
            "web_search cannot combine allowed_domains and blocked_domains",
        ));
    }
    match tool.max_uses {
        Some(uses) if uses <= 0 || uses as usize > MAX_USES => {
            Err(invalid("web_search.max_uses must be between 1 and 100"))
        }
        Some(uses) => Ok(uses as usize),
        None => Ok(DEFAULT_MAX_USES),
    }
}

fn matches_domain(url: &str, domains: &Value) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    domains
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|domain| {
            host.eq_ignore_ascii_case(domain)
                || host
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", domain.to_ascii_lowercase()))
        })
}

struct Search {
    native: Tool,
    alias: String,
    max_uses: usize,
}

fn prepare(request: &mut MessagesRequest) -> Result<Search, Box<Response>> {
    let tools = request.tools.as_mut().unwrap();
    if tools.iter().filter(|tool| is_native_search(tool)).count() != 1 {
        return Err(invalid("Only one native web search tool is allowed"));
    }
    let index = tools.iter().position(is_native_search).unwrap();
    let native = tools[index].clone();
    let max_uses = native_options(&native)?;
    // Use a private, unambiguous name even when a client defines web_search too.
    let mut alias = "kiro_native_web_search".to_owned();
    while tools
        .iter()
        .any(|tool| tool.name.eq_ignore_ascii_case(&alias))
        || request.messages.iter().any(|message| {
            message.content.as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    block["name"]
                        .as_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case(&alias))
                })
            })
        })
    {
        alias.push('_');
    }
    tools[index] = serde_json::from_value(json!({"name":alias,
        "description":"Search the web for current information. Executed by the server; use other tools for client operations.",
        "input_schema":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}})).unwrap();
    // Validate the entire request before committing HTTP/SSE headers or calling MCP.
    converter::convert_request(request).map_err(|error| invalid(error.to_string()))?;
    Ok(Search {
        native,
        alias,
        max_uses,
    })
}

async fn run(
    state: AppState,
    request: MessagesRequest,
    search: Search,
) -> Result<Value, Box<Response>> {
    let mut usage = json!({});
    match run_rounds(state, request, search, &mut usage).await {
        Ok(message) => Ok(message),
        Err(response) => {
            // Completed generation/search rounds are billable even when a later
            // round fails. Preserve evidence on both JSON and SSE error paths.
            let status = response.status();
            let mut headers = response.headers().clone();
            headers.remove(header::CONTENT_LENGTH);
            let bytes = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap_or_default();
            let mut error: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                json!({
                    "error":{"type":"api_error","message":"Mixed search failed"}
                })
            });
            if error["usage"].is_object() {
                let mut iteration = error["usage"].clone();
                add_usage(&mut usage, &iteration);
                iteration["type"] = json!("message");
                if !usage["iterations"].is_array() {
                    usage["iterations"] = json!([]);
                }
                usage["iterations"].as_array_mut().unwrap().push(iteration);
            }
            if usage.as_object().is_some_and(|usage| !usage.is_empty()) {
                error["usage"] = usage;
                crate::request_log::response_usage(&error);
            }
            Err(Box::new((status, headers, Json(error)).into_response()))
        }
    }
}

async fn run_rounds(
    state: AppState,
    mut request: MessagesRequest,
    search: Search,
    aggregate_usage: &mut Value,
) -> Result<Value, Box<Response>> {
    let Search {
        native,
        alias,
        max_uses,
    } = search;
    let provider = state
        .kiro_provider
        .clone()
        .expect("provider checked by handlers");
    request.stream = false;
    let mut conversation_id = None;
    let mut content = Vec::new();
    let mut iterations = Vec::new();
    let mut uses = 0usize;
    let mut response_bytes = 0usize;
    let mut used_ids: HashSet<String> = request
        .messages
        .iter()
        .filter_map(|m| m.content.as_array())
        .flatten()
        .filter_map(|block| block["id"].as_str().map(str::to_owned))
        .collect();
    loop {
        let mut converted =
            converter::convert_request(&request).map_err(|error| invalid(error.to_string()))?;
        let id = conversation_id
            .get_or_insert_with(|| converted.conversation_state.conversation_id.clone());
        converted.conversation_state.conversation_id = id.clone();
        let body = serde_json::to_string(&KiroRequest {
            conversation_state: converted.conversation_state,
            profile_arn: None,
            additional_model_request_fields: converter::additional_model_request_fields(&request),
        })
        .unwrap();
        let input_tokens = crate::token::count_all_tokens(
            &request.model,
            &request.system,
            &request.messages,
            &request.tools,
        )
        .await as i32;
        let thinking =
            request.thinking.as_ref().is_some_and(|t| t.is_enabled()) && state.extract_thinking;
        let response = handlers::handle_non_stream_request(
            provider.clone(),
            &body,
            &request.model,
            input_tokens,
            thinking,
            converted.tool_name_map,
        )
        .await;
        let status = response.status();
        let bytes = to_bytes(
            response.into_body(),
            provider.request_limits().max_response_bytes,
        )
        .await
        .map_err(|error| Box::new(handlers::upstream_error_response(error.into())))?;
        if !status.is_success() {
            return Err(Box::new(
                (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
            ));
        }
        let mut message: Value = serde_json::from_slice(&bytes)
            .map_err(|error| Box::new(handlers::upstream_error_response(error.into())))?;
        let mut blocks = message["content"].as_array().cloned().unwrap_or_default();
        // Model-generated IDs can repeat across internal rounds or client history.
        // Reassign before both emitting and replaying, preserving one-to-one pairs.
        for block in &mut blocks {
            if block["type"] == "tool_use" {
                let id = block["id"].as_str().unwrap_or_default().to_owned();
                if !used_ids.insert(id) {
                    let id = format!("toolu_{}", uuid::Uuid::new_v4().simple());
                    used_ids.insert(id.clone());
                    block["id"] = json!(id);
                }
            }
        }
        let mut iteration = message["usage"].clone();
        iteration["type"] = json!("message");
        iterations.push(iteration);
        add_usage(aggregate_usage, &message["usage"]);
        aggregate_usage["iterations"] = json!(iterations);
        response_bytes = response_bytes.saturating_add(bytes.len());
        if response_bytes > provider.request_limits().max_response_bytes {
            return Err(Box::new(handlers::upstream_error_response(
                anyhow::anyhow!("Mixed search response exceeds buffer limit"),
            )));
        }
        let searches: Vec<_> = blocks
            .iter()
            .filter(|block| is_search_call(block, &alias))
            .collect();
        // Validate every query before executing any of this turn's server calls.
        if searches.iter().any(|block| {
            !block["input"]["query"]
                .as_str()
                .is_some_and(|q| !q.trim().is_empty())
        }) {
            return Err(invalid(
                "Native web search query must be a non-empty string",
            ));
        }
        let client_pending = blocks
            .iter()
            .any(|block| block["type"] == "tool_use" && !is_search_call(block, &alias));
        let mut results = Vec::new();
        let mut limit_reached = false;
        for block in &blocks {
            if !is_search_call(block, &alias) {
                content.push(block.clone());
                continue;
            }
            let query = block["input"]["query"].as_str().unwrap();
            let id = format!("srvtoolu_{}", uuid::Uuid::new_v4().simple());
            let mut call = block.clone();
            call["type"] = json!("server_tool_use");
            call["name"] = json!(native.name);
            call["id"] = json!(id);
            content.push(call);
            let (search_result, result_text, is_error) = if uses >= max_uses {
                limit_reached = true;
                let error =
                    json!({"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"});
                (error.clone(), error.to_string(), true)
            } else {
                let (_, mut mcp) = websearch::create_mcp_request(query);
                for (key, prefix) in [("allowed_domains", "site:"), ("blocked_domains", "-site:")] {
                    if let Some(domains) = native.options.get(key).and_then(Value::as_array) {
                        let terms: Vec<_> = domains
                            .iter()
                            .filter_map(Value::as_str)
                            .map(|domain| format!("{prefix}{domain}"))
                            .collect();
                        if !terms.is_empty() {
                            let separator = if key == "allowed_domains" {
                                " OR "
                            } else {
                                " "
                            };
                            mcp.params
                                .arguments
                                .query
                                .push_str(&format!(" ({})", terms.join(separator)));
                        }
                    }
                }
                if let Some(location) = native.options.get("user_location") {
                    mcp.params
                        .arguments
                        .query
                        .push_str(&format!("\nUser location context: {location}"));
                }
                let mut found = websearch::call_mcp_api(&provider, &mcp)
                    .await
                    .map_err(|error| Box::new(handlers::map_provider_error(error)))?;
                uses += 1;
                aggregate_usage["server_tool_use"] = json!({"web_search_requests":uses});
                found.results.retain(|result| {
                    native
                        .options
                        .get("allowed_domains")
                        .filter(|v| v.as_array().is_some_and(|v| !v.is_empty()))
                        .is_none_or(|domains| matches_domain(&result.url, domains))
                        && native
                            .options
                            .get("blocked_domains")
                            .is_none_or(|domains| !matches_domain(&result.url, domains))
                });
                let summary =
                    websearch::create_websearch_message(&request.model, query, &id, &found, 0);
                let result = summary["content"][2]["content"].clone();
                response_bytes = response_bytes.saturating_add(result.to_string().len());
                if response_bytes > provider.request_limits().max_response_bytes {
                    return Err(Box::new(handlers::upstream_error_response(
                        anyhow::anyhow!("Mixed search response exceeds buffer limit"),
                    )));
                }
                (result.clone(), result.to_string(), false)
            };
            content.push(
                json!({"type":"web_search_tool_result","tool_use_id":id,"content":search_result}),
            );
            results.push(json!({"type":"tool_result","tool_use_id":block["id"],"content":result_text,"is_error":is_error}));
        }
        if searches.is_empty() || client_pending || limit_reached {
            message["content"] = json!(content);
            if limit_reached && !client_pending {
                message["stop_reason"] = json!("pause_turn");
            }
            aggregate_usage["server_tool_use"] = json!({"web_search_requests":uses});
            aggregate_usage["iterations"] = json!(iterations);
            message["usage"] = aggregate_usage.clone();
            crate::request_log::response_usage(&message);
            return Ok(message);
        }
        request.messages.push(Message {
            role: "assistant".into(),
            content: json!(blocks),
        });
        request.messages.push(Message {
            role: "user".into(),
            content: json!(results),
        });
    }
}

fn is_search_call(block: &Value, alias: &str) -> bool {
    block["type"] == "tool_use"
        && block["name"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(alias))
}

fn add_usage(total: &mut Value, usage: &Value) {
    if let Some(number) = usage.as_i64() {
        *total = json!(total.as_i64().unwrap_or(0).saturating_add(number));
    } else if let Some(fields) = usage.as_object() {
        if !total.is_object() {
            *total = json!({});
        }
        for (key, value) in fields {
            add_usage(&mut total[key], value);
        }
    }
}
