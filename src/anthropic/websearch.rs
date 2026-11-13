//! WebSearch 工具处理模块
//!
//! 实现 Anthropic WebSearch 请求到 Kiro MCP 的转换和响应生成

use anyhow::{Context, bail};
use axum::{
    body::Body,
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::stream::SseEvent;
use super::types::{ErrorResponse, MessagesRequest};

/// MCP 请求
#[derive(Debug, Serialize)]
pub struct McpRequest {
    pub id: String,
    pub jsonrpc: String,
    pub method: String,
    pub params: McpParams,
}

/// MCP 请求参数
#[derive(Debug, Serialize)]
pub struct McpParams {
    pub name: String,
    pub arguments: McpArguments,
}

/// MCP 参数
#[derive(Debug, Serialize)]
pub struct McpArguments {
    pub query: String,
}

/// MCP 响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResponse {
    pub error: Option<McpError>,
    pub id: String,
    pub jsonrpc: String,
    pub result: Option<McpResult>,
}

/// MCP 错误
#[derive(Debug, Deserialize)]
pub struct McpError {
    pub code: Option<i32>,
    pub message: Option<String>,
}

/// MCP 结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResult {
    pub content: Vec<McpContent>,
    #[serde(default, rename = "isError")]
    pub is_error: bool,
}

/// MCP 内容
#[derive(Debug, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: Option<String>,
}

/// WebSearch 搜索结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WebSearchResults {
    pub results: Vec<WebSearchResult>,
    #[serde(rename = "totalResults")]
    pub total_results: Option<i32>,
    pub query: Option<String>,
    pub error: Option<String>,
}

/// 单个搜索结果
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<i64>,
    pub id: Option<String>,
    pub domain: Option<String>,
    #[serde(rename = "maxVerbatimWordLimit")]
    pub max_verbatim_word_limit: Option<i32>,
    #[serde(rename = "publicDomain")]
    pub public_domain: Option<bool>,
}

/// 检查请求是否为纯 WebSearch 请求
///
/// Only an explicitly typed native search tool can use the server MCP route.
pub fn has_web_search_tool(req: &MessagesRequest) -> bool {
    req.tools.as_ref().is_some_and(|tools| {
        tools.len() == 1
            && tools.first().is_some_and(|t| {
                t.name == "web_search"
                    && t.tool_type.as_deref() == Some("web_search_20250305")
                    && t.input_schema.is_empty()
            })
    })
}

/// 使用最近一条用户消息中的文本块作为搜索词，忽略历史消息及非文本内容。
pub fn extract_search_query(req: &MessagesRequest) -> Option<String> {
    let message = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")?;
    let text = match &message.content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let query = text
        .trim()
        .strip_prefix("Perform a web search for the query:")
        .unwrap_or(text.trim())
        .trim();
    if query.is_empty() {
        None
    } else {
        Some(query.to_string())
    }
}

/// 生成22位大小写字母和数字的随机字符串
fn generate_random_id_22() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..22)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 生成8位小写字母和数字的随机字符串
fn generate_random_id_8() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..8)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 创建 MCP 请求
///
/// ID 格式: web_search_tooluse_{22位随机}_{毫秒时间戳}_{8位随机}
pub fn create_mcp_request(query: &str) -> (String, McpRequest) {
    let random_22 = generate_random_id_22();
    let timestamp = chrono::Utc::now().timestamp_millis();
    let random_8 = generate_random_id_8();

    let request_id = format!(
        "web_search_tooluse_{}_{}_{}",
        random_22, timestamp, random_8
    );

    // tool_use_id 使用相同格式
    let tool_use_id = format!(
        "srvtoolu_{}",
        &Uuid::new_v4().to_string().replace('-', "")[..32]
    );

    let request = McpRequest {
        id: request_id,
        jsonrpc: "2.0".to_string(),
        method: "tools/call".to_string(),
        params: McpParams {
            name: "web_search".to_string(),
            arguments: McpArguments {
                query: query.to_string(),
            },
        },
    };

    (tool_use_id, request)
}

/// 拒绝上游协议错误和工具错误，合法的零结果搜索仍然返回成功。
fn parse_search_results(response: &McpResponse) -> anyhow::Result<WebSearchResults> {
    if let Some(error) = &response.error {
        bail!(
            "MCP error: {} - {}",
            error.code.unwrap_or(-1),
            error.message.as_deref().unwrap_or("Unknown error")
        );
    }
    let result = response.result.as_ref().context("Missing MCP result")?;
    if result.is_error {
        bail!("MCP web_search tool reported an error");
    }
    let content = result
        .content
        .iter()
        .filter(|content| content.content_type == "text")
        .find_map(|content| content.text.as_deref())
        .context("Missing MCP search result text")?;
    let results: WebSearchResults =
        serde_json::from_str(content).context("Invalid MCP search results")?;
    if let Some(error) = &results.error {
        bail!("Web search failed: {error}");
    }
    Ok(results)
}

/// JSON 与 SSE 使用同一条完整消息，保持工具关联和 usage 一致。
pub(super) fn create_websearch_message(
    model: &str,
    query: &str,
    tool_use_id: &str,
    results: &WebSearchResults,
    input_tokens: i32,
) -> Value {
    let search_content: Vec<_> = results
        .results
        .iter()
        .map(|result| {
            let page_age = result.published_date.and_then(|ms| {
                chrono::DateTime::from_timestamp_millis(ms)
                    .map(|date| date.format("%B %-d, %Y").to_string())
            });
            json!({
                "type":"web_search_result", "title":result.title, "url":result.url,
                "encrypted_content":result.snippet.as_deref().unwrap_or_default(),
                "page_age":page_age
            })
        })
        .collect();
    let content = vec![
        json!({"type":"text", "text":format!("I'll search for \"{query}\".")}),
        json!({"type":"server_tool_use", "id":tool_use_id, "name":"web_search", "input":{"query":query}}),
        json!({"type":"web_search_tool_result", "tool_use_id":tool_use_id, "content":search_content}),
        json!({"type":"text", "text":generate_search_summary(query, results)}),
    ];
    let output_tokens = crate::token::estimate_output_tokens(&content);
    json!({
        "id":format!("msg_{}", Uuid::new_v4().simple()),
        "type":"message", "role":"assistant", "model":model, "content":content,
        "stop_reason":"end_turn", "stop_sequence":null,
        "usage":{"input_tokens":input_tokens, "output_tokens":output_tokens,
            "server_tool_use":{"web_search_requests":1}}
    })
}

fn generate_websearch_events(message: &Value) -> Vec<SseEvent> {
    let mut initial = message.clone();
    initial["content"] = json!([]);
    initial["stop_reason"] = Value::Null;
    initial["usage"]["output_tokens"] = json!(0);
    let mut events = vec![SseEvent::new(
        "message_start",
        json!({"type":"message_start", "message":initial}),
    )];
    if let Some(blocks) = message["content"].as_array() {
        for (index, block) in blocks.iter().enumerate() {
            let mut initial_block = block.clone();
            let text = block.get("text").and_then(Value::as_str);
            if text.is_some() {
                initial_block["text"] = json!("");
            }
            events.push(SseEvent::new(
                "content_block_start",
                json!({
                    "type":"content_block_start", "index":index, "content_block":initial_block
                }),
            ));
            if let Some(text) = text {
                // 按 Unicode 字符边界分块，避免额外构建整段字符数组。
                let mut chars = text.chars().peekable();
                while chars.peek().is_some() {
                    let chunk: String = chars.by_ref().take(100).collect();
                    events.push(SseEvent::new(
                        "content_block_delta",
                        json!({
                            "type":"content_block_delta", "index":index,
                            "delta":{"type":"text_delta", "text":chunk}
                        }),
                    ));
                }
            }
            events.push(SseEvent::new(
                "content_block_stop",
                json!({"type":"content_block_stop", "index":index}),
            ));
        }
    }
    events.push(SseEvent::new("message_delta", json!({
        "type":"message_delta", "delta":{"stop_reason":message["stop_reason"], "stop_sequence":message["stop_sequence"]},
        "usage":message["usage"]
    })));
    events.push(SseEvent::new(
        "message_stop",
        json!({"type":"message_stop"}),
    ));
    events
}

/// 生成搜索结果摘要
fn generate_search_summary(query: &str, results: &WebSearchResults) -> String {
    let mut summary = format!("Here are the search results for \"{}\":\n\n", query);

    if !results.results.is_empty() {
        for (i, result) in results.results.iter().enumerate() {
            summary.push_str(&format!("{}. **{}**\n", i + 1, result.title));
            if let Some(ref snippet) = result.snippet {
                // 截断过长的摘要（安全处理 UTF-8 多字节字符）
                let truncated = match snippet.char_indices().nth(200) {
                    Some((idx, _)) => format!("{}...", &snippet[..idx]),
                    None => snippet.clone(),
                };
                summary.push_str(&format!("   {}\n", truncated));
            }
            summary.push_str(&format!("   Source: {}\n\n", result.url));
        }
    } else {
        summary.push_str("No results found.\n");
    }

    summary.push_str("\nPlease note that these are web search results and may not be fully accurate or up-to-date.");

    summary
}

/// 处理 WebSearch 请求
pub async fn handle_websearch_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    payload: &MessagesRequest,
    input_tokens: i32,
) -> Response {
    // 1. 提取搜索查询
    let query = match extract_search_query(payload) {
        Some(q) => q,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "无法从消息中提取搜索查询",
                )),
            )
                .into_response();
        }
    };

    tracing::info!(query_chars = query.chars().count(), "处理 WebSearch 请求");

    // 2. 创建 MCP 请求
    let (tool_use_id, mcp_request) = create_mcp_request(&query);

    // 3. 调用 Kiro MCP API
    let results = match call_mcp_api(&provider, &mcp_request).await {
        Ok(results) => results,
        Err(error) => return super::handlers::map_provider_error(error),
    };
    let message =
        create_websearch_message(&payload.model, &query, &tool_use_id, &results, input_tokens);
    if !payload.stream {
        crate::request_log::response_usage(&message);
        return Json(message).into_response();
    }
    let stream = stream::iter(
        generate_websearch_events(&message)
            .into_iter()
            .map(|event| Ok::<_, std::convert::Infallible>(Bytes::from(event.to_sse_string()))),
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 调用 Kiro MCP API
pub(super) async fn call_mcp_api(
    provider: &crate::kiro::provider::KiroProvider,
    request: &McpRequest,
) -> anyhow::Result<WebSearchResults> {
    let request_body = serde_json::to_string(request)?;

    tracing::debug!(request_bytes = request_body.len(), "MCP request prepared");

    let response = provider.call_mcp(&request_body).await?;
    let credential_id = response.credential_id();

    let body = response.text().await?;
    tracing::debug!(response_bytes = body.len(), "MCP response received");

    let mcp_response: McpResponse = serde_json::from_str(&body)?;

    if mcp_response.jsonrpc != "2.0" || mcp_response.id != request.id {
        bail!("Invalid MCP response version or request ID");
    }
    let results = parse_search_results(&mcp_response)?;
    provider.report_response_success(credential_id, None);
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_web_search_tool_only_one() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: String::new(),
                input_schema: Default::default(),
                max_uses: Some(8),
                options: Default::default(),
            }]),
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: None,
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };

        assert!(has_web_search_tool(&req));
    }

    #[test]
    fn test_has_web_search_tool_multiple_tools() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![
                Tool {
                    tool_type: Some("web_search_20250305".to_string()),
                    name: "web_search".to_string(),
                    description: String::new(),
                    input_schema: Default::default(),
                    max_uses: Some(8),
                    options: Default::default(),
                },
                Tool {
                    tool_type: None,
                    name: "other_tool".to_string(),
                    description: "Other tool".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                    options: Default::default(),
                },
            ]),
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: None,
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };

        // 多个工具时不应该被识别为纯 websearch 请求
        assert!(!has_web_search_tool(&req));
    }

    #[test]
    fn test_extract_search_query_with_prefix() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": "Perform a web search for the query: rust latest version 2026"
                }]),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: None,
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };

        let query = extract_search_query(&req);
        // 前缀应该被去除
        assert_eq!(query, Some("rust latest version 2026".to_string()));
    }

    #[test]
    fn test_extract_search_query_plain_text() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("What is the weather today?"),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            extra_controls: Default::default(),
            thinking: None,
            output_config: None,
            metadata: None,
            compaction: None,
            context_management: None,
        };

        let query = extract_search_query(&req);
        assert_eq!(query, Some("What is the weather today?".to_string()));
    }

    #[test]
    fn test_create_mcp_request() {
        let (tool_use_id, request) = create_mcp_request("test query");

        assert!(tool_use_id.starts_with("srvtoolu_"));
        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(request.method, "tools/call");
        assert_eq!(request.params.name, "web_search");
        assert_eq!(request.params.arguments.query, "test query");

        // 验证 ID 格式: web_search_tooluse_{22位}_{时间戳}_{8位}
        assert!(request.id.starts_with("web_search_tooluse_"));
    }

    #[test]
    fn test_mcp_request_id_format() {
        let (_, request) = create_mcp_request("test");

        // 格式: web_search_tooluse_{22位}_{毫秒时间戳}_{8位}
        let id = &request.id;
        assert!(id.starts_with("web_search_tooluse_"));

        let suffix = &id["web_search_tooluse_".len()..];
        let parts: Vec<&str> = suffix.split('_').collect();
        assert_eq!(parts.len(), 3, "应该有3个部分: 22位随机_时间戳_8位随机");

        // 第一部分: 22位大小写字母和数字
        assert_eq!(parts[0].len(), 22);
        assert!(parts[0].chars().all(|c| c.is_ascii_alphanumeric()));

        // 第二部分: 毫秒时间戳
        assert!(parts[1].parse::<i64>().is_ok());

        // 第三部分: 8位小写字母和数字
        assert_eq!(parts[2].len(), 8);
        assert!(
            parts[2]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn test_parse_search_results() {
        let response = McpResponse {
            error: None,
            id: "test_id".to_string(),
            jsonrpc: "2.0".to_string(),
            result: Some(McpResult {
                content: vec![McpContent {
                    content_type: "text".to_string(),
                    text: Some(r#"{"results":[{"title":"Test","url":"https://example.com","snippet":"Test snippet"}],"totalResults":1}"#.to_string()),
                }],
                is_error: false,
            }),
        };

        let results = parse_search_results(&response);
        assert!(results.is_ok());
        let results = results.unwrap();
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].title, "Test");
    }

    #[test]
    fn test_generate_search_summary() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Test Result".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("This is a test snippet".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("test".to_string()),
            error: None,
        };

        let summary = generate_search_summary("test", &results);

        assert!(summary.contains("Test Result"));
        assert!(summary.contains("https://example.com"));
        assert!(summary.contains("This is a test snippet"));
    }

    #[test]
    fn search_query_uses_latest_user_text_and_rejects_blank_queries() {
        let mut req: MessagesRequest = serde_json::from_value(json!({
            "model":"claude-sonnet-5", "max_tokens":1024,
            "messages":[
                {"role":"user", "content":"old search"},
                {"role":"assistant", "content":"old answer"},
                {"role":"user", "content":[
                    {"type":"image", "source":{"type":"url", "url":"https://example.com"}},
                    {"type":"text", "text":" Perform a web search for the query: 最新 Rust"},
                    {"type":"text", "text":"与 Tokio 版本  "}
                ]},
                {"role":"assistant", "content":"ignored"}
            ]
        }))
        .unwrap();
        assert_eq!(
            extract_search_query(&req).as_deref(),
            Some("最新 Rust\n与 Tokio 版本")
        );
        for text in ["", " \n\t ", "Perform a web search for the query: "] {
            req.messages[2].content = json!(text);
            assert!(extract_search_query(&req).is_none());
        }
        req.messages[2].content = json!([{"type":"image"}]);
        assert!(extract_search_query(&req).is_none());
    }

    #[test]
    fn search_sse_reconstructs_json_message_with_unicode_and_tool_links() {
        let results: WebSearchResults = serde_json::from_value(json!({"results":[{
            "title":"Rust 文档🦀", "url":"https://example.com/rust",
            "snippet":"中文🦀".repeat(120), "publishedDate":0
        }]}))
        .unwrap();
        let message = create_websearch_message(
            "claude-sonnet-5",
            "最新版本🦀",
            "srvtoolu_test",
            &results,
            42,
        );
        let events = generate_websearch_events(&message);
        let mut reconstructed = events[0].data["message"].clone();
        let mut blocks = Vec::new();
        let mut active_block = None;
        for event in events.iter().skip(1) {
            match event.event.as_str() {
                "content_block_start" => {
                    assert!(active_block.is_none());
                    assert_eq!(event.data["index"], blocks.len());
                    active_block = Some(blocks.len());
                    blocks.push(event.data["content_block"].clone());
                }
                "content_block_delta" => {
                    let index = event.data["index"].as_u64().unwrap() as usize;
                    assert_eq!(Some(index), active_block);
                    let text = format!(
                        "{}{}",
                        blocks[index]["text"].as_str().unwrap(),
                        event.data["delta"]["text"].as_str().unwrap()
                    );
                    blocks[index]["text"] = json!(text);
                }
                "content_block_stop" => {
                    assert_eq!(event.data["index"], active_block.take().unwrap());
                }
                "message_delta" => {
                    reconstructed["usage"] = event.data["usage"].clone();
                    reconstructed["stop_reason"] = event.data["delta"]["stop_reason"].clone();
                }
                "message_stop" => assert!(active_block.is_none()),
                kind => panic!("Unexpected event {kind}"),
            }
        }
        reconstructed["content"] = json!(blocks);
        assert_eq!(reconstructed, message);
        assert_eq!(
            message["content"][2]["tool_use_id"],
            message["content"][1]["id"]
        );
        assert_eq!(
            message["usage"]["server_tool_use"]["web_search_requests"],
            1
        );
        assert!(
            message["content"][3]["text"]
                .as_str()
                .unwrap()
                .contains("...")
        );
    }
}
