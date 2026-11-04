//! Token 计算模块
//!
//! 提供文本 token 数量计算功能。
//!
//! # 计算规则
//! - 非西文字符：每个计 4.5 个字符单位
//! - 西文字符：每个计 1 个字符单位
//! - 4 个字符单位 = 1 token（四舍五入）

use crate::anthropic::types::{CountTokensResponse, Message, SystemMessage, Tool};
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;
use std::sync::OnceLock;

/// Count Tokens API 配置
#[derive(Clone)]
pub struct CountTokensConfig {
    /// 外部 count_tokens API 地址
    pub api_url: Option<String>,
    /// count_tokens API 密钥
    pub api_key: Option<String>,
    /// count_tokens API 认证类型（"x-api-key" 或 "bearer"）
    pub auth_type: String,
    /// 代理配置
    pub proxy: Option<ProxyConfig>,

    pub tls_backend: TlsBackend,
    /// 远程计数总超时（秒）。
    pub timeout_secs: u64,
}

impl Default for CountTokensConfig {
    fn default() -> Self {
        Self {
            api_url: None,
            api_key: None,
            auth_type: "x-api-key".into(),
            proxy: None,
            tls_backend: TlsBackend::default(),
            timeout_secs: 10,
        }
    }
}

/// 全局计数服务；共享连接池，避免每个请求重新构建客户端。
static COUNT_TOKENS_SERVICE: OnceLock<CountTokensService> = OnceLock::new();

struct CountTokensService {
    config: CountTokensConfig,
    client: Option<reqwest::Client>,
}

impl CountTokensService {
    fn new(config: CountTokensConfig) -> Self {
        let client = config.api_url.as_ref().and_then(|_| {
            match build_client(
                config.proxy.as_ref(),
                config.timeout_secs.max(1),
                config.tls_backend,
            ) {
                Ok(client) => Some(client),
                Err(error) => {
                    tracing::warn!("无法初始化远程 token 计数客户端，将使用本地估算: {error:#}");
                    None
                }
            }
        });
        Self { config, client }
    }

    async fn count(&self, request: &TokenCountRequest<'_>) -> u64 {
        if let (Some(url), Some(client)) = (&self.config.api_url, &self.client) {
            match call_remote_count_tokens(client, url, &self.config, request).await {
                Ok(tokens) => return tokens,
                Err(error) => {
                    tracing::warn!("远程 count_tokens API 调用失败，回退到本地计算: {error:#}")
                }
            }
        }
        count_all_tokens_local(
            request.system.as_deref(),
            request.messages,
            request.tools.as_deref(),
        )
    }
}

/// 应在应用启动时调用一次。
pub fn init_config(config: CountTokensConfig) {
    let _ = COUNT_TOKENS_SERVICE.set(CountTokensService::new(config));
}

/// 判断字符是否为非西文字符
///
/// 西文字符包括：
/// - ASCII 字符 (U+0000..U+007F)
/// - 拉丁字母扩展 (U+0080..U+024F)
/// - 拉丁字母扩展附加 (U+1E00..U+1EFF)
///
/// 返回 true 表示该字符是非西文字符（如中文、日文、韩文、阿拉伯文等）
fn is_non_western_char(c: char) -> bool {
    !matches!(c,
        // 基本 ASCII
        '\u{0000}'..='\u{007F}' |
        // 拉丁字母扩展-A (Latin Extended-A)
        '\u{0080}'..='\u{00FF}' |
        // 拉丁字母扩展-B (Latin Extended-B)
        '\u{0100}'..='\u{024F}' |
        // 拉丁字母扩展附加 (Latin Extended Additional)
        '\u{1E00}'..='\u{1EFF}' |
        // 拉丁字母扩展-C/D/E
        '\u{2C60}'..='\u{2C7F}' |
        '\u{A720}'..='\u{A7FF}' |
        '\u{AB30}'..='\u{AB6F}'
    )
}

/// 计算文本的 token 数量
///
/// # 计算规则
/// - 非西文字符：每个计 4.5 个字符单位
/// - 西文字符：每个计 1 个字符单位
/// - 4 个字符单位 = 1 token（四舍五入）
/// ```
pub fn count_tokens(text: &str) -> u64 {
    // println!("text: {}", text);

    let char_units: f64 = text
        .chars()
        .map(|c| if is_non_western_char(c) { 4.0 } else { 1.0 })
        .sum();

    let tokens = char_units / 4.0;

    let acc_token = if tokens < 100.0 {
        tokens * 1.5
    } else if tokens < 200.0 {
        tokens * 1.3
    } else if tokens < 300.0 {
        tokens * 1.25
    } else if tokens < 800.0 {
        tokens * 1.2
    } else {
        tokens * 1.0
    } as u64;

    // println!("tokens: {}, acc_tokens: {}", tokens, acc_token);
    acc_token
}

/// 估算请求的输入 tokens
///
/// 优先调用远程 API，失败时回退到本地计算
pub(crate) async fn count_all_tokens(
    model: &str,
    system: &Option<Vec<SystemMessage>>,
    messages: &[Message],
    tools: &Option<Vec<Tool>>,
) -> u64 {
    let request = TokenCountRequest {
        model,
        system,
        messages,
        tools,
    };
    match COUNT_TOKENS_SERVICE.get() {
        Some(service) => service.count(&request).await,
        None => count_all_tokens_local(system.as_deref(), messages, tools.as_deref()),
    }
}

/// 直接序列化借用的数据，避免克隆整个会话。
#[derive(serde::Serialize)]
struct TokenCountRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: &'a Option<Vec<SystemMessage>>,
    messages: &'a [Message],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: &'a Option<Vec<Tool>>,
}

async fn call_remote_count_tokens(
    client: &reqwest::Client,
    api_url: &str,
    config: &CountTokensConfig,
    request: &TokenCountRequest<'_>,
) -> anyhow::Result<u64> {
    let mut builder = client.post(api_url);
    if let Some(key) = &config.api_key {
        builder = if config.auth_type == "bearer" {
            builder.bearer_auth(key)
        } else {
            builder.header("x-api-key", key)
        };
    }
    let response = builder
        .json(request)
        .send()
        .await
        .map_err(|error| error.without_url())?;
    let response = response
        .error_for_status()
        .map_err(|error| error.without_url())?;
    let result: CountTokensResponse = response.json().await.map_err(|error| error.without_url())?;
    anyhow::ensure!(result.input_tokens > 0, "远程计数必须为正整数");
    Ok(result.input_tokens as u64)
}

/// 本地计算请求的输入 tokens
fn count_all_tokens_local(
    system: Option<&[SystemMessage]>,
    messages: &[Message],
    tools: Option<&[Tool]>,
) -> u64 {
    let mut total = 0;

    // 系统消息
    if let Some(system) = system {
        for msg in system {
            total += count_tokens(&msg.text);
        }
    }

    // 用户消息
    for msg in messages {
        if let serde_json::Value::String(s) = &msg.content {
            total += count_tokens(s);
        } else if let serde_json::Value::Array(arr) = &msg.content {
            total += arr.iter().map(count_content_tokens).sum::<u64>();
        }
    }

    // 工具定义
    if let Some(tools) = tools {
        for tool in tools {
            total += count_tokens(&tool.name);
            total += count_tokens(&tool.description);
            let input_schema_json = serde_json::to_string(&tool.input_schema).unwrap_or_default();
            total += count_tokens(&input_schema_json);
        }
    }

    total.max(1)
}

fn count_content_tokens(block: &serde_json::Value) -> u64 {
    match block.get("type").and_then(|value| value.as_str()) {
        Some("text") => block
            .get("text")
            .and_then(|v| v.as_str())
            .map(count_tokens)
            .unwrap_or(0),
        Some("thinking") => block
            .get("thinking")
            .and_then(|v| v.as_str())
            .map(count_tokens)
            .unwrap_or(0),
        Some("tool_use") => block
            .get("input")
            .map(|v| count_tokens(&v.to_string()))
            .unwrap_or(0),
        Some("tool_result") | Some("compaction") => match block.get("content") {
            Some(serde_json::Value::String(text)) => count_tokens(text),
            Some(serde_json::Value::Array(blocks)) => blocks.iter().map(count_content_tokens).sum(),
            _ => 0,
        },
        _ => 0,
    }
}

/// 估算输出 tokens
pub(crate) fn estimate_output_tokens(content: &[serde_json::Value]) -> i32 {
    let mut total = 0;

    for block in content {
        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
            total += count_tokens(text) as i32;
        }
        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
            // 工具调用开销
            if let Some(input) = block.get("input") {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                total += count_tokens(&input_str) as i32;
            }
        }
    }

    total.max(1)
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_large_tool_results_for_compaction_threshold() {
        let messages = vec![super::Message {
            role: "user".into(),
            content: serde_json::json!([
                {"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"x".repeat(200_000)}]}
            ]),
        }];
        assert!(super::count_all_tokens_local(None, &messages, None) >= 50_000);
    }

    struct TokenServer {
        url: String,
        requests: std::sync::Arc<
            std::sync::Mutex<
                Vec<(
                    std::net::SocketAddr,
                    axum::http::HeaderMap,
                    serde_json::Value,
                )>,
            >,
        >,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for TokenServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn token_server() -> TokenServer {
        use axum::{
            Json,
            body::Body,
            extract::{ConnectInfo, Path},
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
        };
        use futures::{StreamExt, stream};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = requests.clone();
        let app = axum::Router::new().route(
            "/{mode}",
            axum::routing::post(
                move |Path(mode): Path<String>,
                      ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
                      headers: HeaderMap,
                      Json(body): Json<serde_json::Value>| {
                    captured.lock().unwrap().push((peer, headers, body));
                    async move {
                        match mode.as_str() {
                            "error" => (StatusCode::TOO_MANY_REQUESTS, "busy").into_response(),
                            "invalid" => {
                                Json(serde_json::json!({"input_tokens":-1})).into_response()
                            }
                            "malformed" => "not json".into_response(),
                            "slow" => Body::from_stream(
                                stream::once(async {
                                    Ok::<_, std::io::Error>("{\"input_tokens\":")
                                })
                                .chain(stream::pending::<Result<&str, std::io::Error>>()),
                            )
                            .into_response(),
                            _ => Json(serde_json::json!({"input_tokens":123})).into_response(),
                        }
                    }
                },
            ),
        );
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        TokenServer {
            url,
            requests,
            task,
        }
    }

    fn remote_config(url: String) -> super::CountTokensConfig {
        super::CountTokensConfig {
            api_url: Some(url),
            timeout_secs: 1,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn remote_counting_works_on_current_thread_and_reuses_connections() {
        use super::*;
        let server = token_server().await;
        let messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("hello"),
        }];
        let system = Some(vec![SystemMessage {
            text: "instructions".into(),
        }]);
        let tools = None;
        let request = TokenCountRequest {
            model: "claude-sonnet-5",
            system: &system,
            messages: &messages,
            tools: &tools,
        };
        for auth_type in ["x-api-key", "bearer"] {
            let mut config = remote_config(format!("{}/ok", server.url));
            config.api_key = Some("test-secret".into());
            config.auth_type = auth_type.into();
            let service = CountTokensService::new(config);
            assert_eq!(service.count(&request).await, 123);
            assert_eq!(service.count(&request).await, 123);
        }
        let captured = server.requests.lock().unwrap();
        assert_eq!(captured.len(), 4);
        assert_eq!(
            captured[0].0, captured[1].0,
            "HTTP connection should be reused"
        );
        assert_eq!(captured[2].0, captured[3].0);
        assert_eq!(captured[0].1["x-api-key"], "test-secret");
        assert_eq!(captured[2].1["authorization"], "Bearer test-secret");
        assert_eq!(
            captured[0].2,
            serde_json::json!({"model":"claude-sonnet-5", "system":[{"text":"instructions"}], "messages":[{"role":"user","content":"hello"}]})
        );
    }

    #[tokio::test]
    async fn remote_errors_and_invalid_counts_fall_back_to_local_estimates() {
        use super::*;
        let server = token_server().await;
        let messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("你好 world"),
        }];
        let request = TokenCountRequest {
            model: "claude-sonnet-5",
            system: &None,
            messages: &messages,
            tools: &None,
        };
        let local = count_all_tokens_local(None, &messages, None);
        for mode in ["error", "invalid", "malformed"] {
            let service = CountTokensService::new(remote_config(format!("{}/{mode}", server.url)));
            assert_eq!(service.count(&request).await, local);
        }
        assert_eq!(
            count_all_tokens("test", &None, &messages, &None).await,
            local
        );
    }

    #[tokio::test]
    async fn remote_timeout_covers_response_body_and_falls_back() {
        use super::*;
        let server = token_server().await;
        let service = CountTokensService::new(remote_config(format!("{}/slow", server.url)));
        let request = TokenCountRequest {
            model: "test",
            system: &None,
            messages: &[],
            tools: &None,
        };
        let started = std::time::Instant::now();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(3), service.count(&request))
                .await
                .unwrap();
        assert_eq!(result, 1);
        assert!(started.elapsed() >= std::time::Duration::from_millis(900));
    }
}
