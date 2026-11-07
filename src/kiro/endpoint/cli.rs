//! Kiro CLI bearer transport: AWS JSON 1.0 operations at the runtime root.
//! CLI 3.1.0 uses the Rust streaming SDK and a separate control-plane client.
use reqwest::RequestBuilder;
use serde_json::Value;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext, ide};
use crate::{kiro::model::credentials::KiroCredentials, model::config::Config};

pub const CLI_ENDPOINT_NAME: &str = "cli";
// Kiro's current CLI surface is distinct from the legacy Amazon Q `CLI` origin.
pub const CLI_ORIGIN: &str = "KIRO_CLI";
pub const GENERATE_TARGET: &str = "AmazonCodeWhispererStreamingService.GenerateAssistantResponse";
pub const MCP_TARGET: &str = "AmazonCodeWhispererStreamingService.InvokeMCP";

pub struct CliEndpoint;

impl CliEndpoint {
    pub fn new() -> Self {
        Self
    }
}

/// Keep the CLI version independent of the IDE version and Node.js settings.
pub(crate) fn user_agent(config: &Config, service: &str) -> String {
    format!(
        "aws-sdk-rust api/{service} os/{} lang/rust md/appVersion-{} app/AmazonQ-For-CLI",
        config.system_version, config.kiro_cli_version
    )
}

pub(crate) fn decorate(
    req: RequestBuilder,
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    service: &str,
) -> RequestBuilder {
    let ua = user_agent(config, service);
    let req = match ua.parse::<reqwest::header::HeaderValue>() {
        Ok(value) => req.headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::USER_AGENT,
            value,
        )])),
        Err(_) => return req.header("user-agent", ua),
    };
    let authorization = format!("Bearer {token}");
    let req = match authorization.parse::<reqwest::header::HeaderValue>() {
        Ok(mut value) => {
            value.set_sensitive(true);
            req.headers(reqwest::header::HeaderMap::from_iter([(
                reqwest::header::AUTHORIZATION,
                value,
            )]))
        }
        Err(_) => return req.bearer_auth(token),
    };
    let mut req = req.headers(reqwest::header::HeaderMap::from_iter([
        (
            reqwest::header::HeaderName::from_static("amz-sdk-invocation-id"),
            Uuid::new_v4().to_string().parse().unwrap(),
        ),
        (
            reqwest::header::HeaderName::from_static("amz-sdk-request"),
            "attempt=1; max=1".parse().unwrap(),
        ),
    ]));
    if credentials.is_api_key_credential() {
        req = req.header("tokentype", "API_KEY");
    }
    req
}

impl KiroEndpoint for CliEndpoint {
    fn name(&self) -> &'static str {
        CLI_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "https://runtime.{}.kiro.dev/",
            ctx.credentials.effective_api_region(ctx.config)
        )
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        self.api_url(ctx)
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        decorate(
            req,
            ctx.credentials,
            ctx.config,
            ctx.token,
            "codewhispererstreaming/0.1.19636",
        )
        .headers(json_headers())
        .header("x-amz-target", GENERATE_TARGET)
        .header("x-amzn-codewhisperer-optout", "true")
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        decorate(
            req,
            ctx.credentials,
            ctx.config,
            ctx.token,
            "codewhispererstreaming/0.1.19636",
        )
        .headers(json_headers())
        .header("x-amz-target", MCP_TARGET)
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        let body = ide::inject_profile_arn(body, &ctx.credentials.profile_arn);
        let body = if super::native_thinking(ctx.credentials, ctx.config) {
            ide::native_model_body(&body)
        } else {
            ide::legacy_thinking_body(&body)
        };
        let Ok(mut request) = serde_json::from_str::<Value>(&body) else {
            return body;
        };
        if let Some(state) = request
            .get_mut("conversationState")
            .and_then(Value::as_object_mut)
        {
            // These continuation fields belong to the IDE agent workflow.
            state.remove("agentContinuationId");
            state.remove("agentTaskType");
            state.insert("chatTriggerType".into(), "MANUAL".into());
            if let Some(message) = state.get_mut("currentMessage") {
                set_origin(message);
            }
            if let Some(history) = state.get_mut("history").and_then(Value::as_array_mut) {
                for message in history {
                    set_origin(message);
                }
            }
        }
        serde_json::to_string(&request).unwrap_or(body)
    }

    fn transform_mcp_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        ide::inject_profile_arn(body, &ctx.credentials.profile_arn)
    }
}

fn json_headers() -> reqwest::header::HeaderMap {
    reqwest::header::HeaderMap::from_iter([(
        reqwest::header::CONTENT_TYPE,
        "application/x-amz-json-1.0".parse().unwrap(),
    )])
}

fn set_origin(message: &mut Value) {
    if let Some(user) = message
        .get_mut("userInputMessage")
        .and_then(Value::as_object_mut)
    {
        user.insert("origin".into(), CLI_ORIGIN.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cli_wire_preserves_tools_images_and_history_with_cli_origins() {
        let credentials = KiroCredentials {
            profile_arn: Some("test-profile".into()),
            api_region: Some("eu-central-1".into()),
            ..Default::default()
        };
        for native in [false, true] {
            let mut config = Config::default();
            config
                .endpoints
                .insert("cli".into(), json!({"nativeThinking":native}));
            let ctx = RequestContext {
                credentials: &credentials,
                config: &config,
                token: "test-token",
                machine_id: "test-machine",
            };
            let endpoint = CliEndpoint::new();
            let body = json!({"conversationState":{
                "conversationId":"conversation", "agentTaskType":"vibe", "agentContinuationId":"ide-id", "chatTriggerType":"AUTO",
                "currentMessage":{"userInputMessage":{"content":"continue", "modelId":"claude-sonnet-4.6", "origin":"AI_EDITOR",
                    "images":[{"format":"png","source":{"bytes":"image"}}],
                    "userInputMessageContext":{"tools":[{"toolSpecification":{"name":"read"}}],"toolResults":[{"toolUseId":"tool-1"}]}}},
                "history":[{"userInputMessage":{"content":"earlier", "origin":"AI_EDITOR"}},
                    {"assistantResponseMessage":{"content":"answer", "toolUses":[{"toolUseId":"tool-1","name":"read","input":{"path":"test"}}]}}]
            },"additionalModelRequestFields":{"max_tokens":4000,"thinking":{"type":"enabled","budget_tokens":1000},"output_config":{"effort":"high"}}});
            let result: Value =
                serde_json::from_str(&endpoint.transform_api_body(&body.to_string(), &ctx))
                    .unwrap();
            let state = &result["conversationState"];
            assert_eq!(result["profileArn"], "test-profile");
            assert_eq!(state["conversationId"], "conversation");
            assert_eq!(state["chatTriggerType"], "MANUAL");
            assert!(state.get("agentTaskType").is_none());
            assert!(state.get("agentContinuationId").is_none());
            let mut current = body["conversationState"]["currentMessage"].clone();
            current["userInputMessage"]["origin"] = "KIRO_CLI".into();
            assert_eq!(state["currentMessage"], current);
            let history = state["history"].as_array().unwrap();
            for message in history {
                if let Some(user) = message.get("userInputMessage") {
                    assert_eq!(user["origin"], "KIRO_CLI");
                }
            }
            assert_eq!(
                history.last().unwrap(),
                body["conversationState"]["history"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()
            );
            if native {
                assert_eq!(
                    result["additionalModelRequestFields"]["thinking"],
                    json!({"type":"adaptive"})
                );
                assert_eq!(result["additionalModelRequestFields"]["max_tokens"], 4000);
            } else {
                assert!(result.get("additionalModelRequestFields").is_none());
                assert!(
                    history[0]["userInputMessage"]["content"]
                        .as_str()
                        .unwrap()
                        .contains("<thinking_effort>high")
                );
            }
            let client = reqwest::Client::new();
            let req = endpoint
                .decorate_api(
                    client
                        .post(endpoint.api_url(&ctx))
                        .header("content-type", "application/json"),
                    &ctx,
                )
                .build()
                .unwrap();
            assert_eq!(req.url().as_str(), "https://runtime.eu-central-1.kiro.dev/");
            assert_eq!(req.headers()["content-type"], "application/x-amz-json-1.0");
            assert_eq!(req.headers().get_all("content-type").iter().count(), 1);
            assert_eq!(req.headers()["x-amz-target"], GENERATE_TARGET);
            assert_eq!(req.headers()["authorization"], "Bearer test-token");
            let ua = req.headers()["user-agent"].to_str().unwrap();
            assert!(ua.contains("appVersion-3.1.0 app/AmazonQ-For-CLI"));
            assert!(!ua.contains("KiroIDE"));
            let mcp = endpoint
                .decorate_mcp(client.post(endpoint.mcp_url(&ctx)), &ctx)
                .build()
                .unwrap();
            assert_eq!(mcp.headers()["x-amz-target"], MCP_TARGET);
            let mcp_body: Value = serde_json::from_str(&endpoint.transform_mcp_body(r#"{"jsonrpc":"2.0","id":"1","method":"tools/call","params":{"name":"web_search"}}"#, &ctx)).unwrap();
            assert_eq!(mcp_body["profileArn"], "test-profile");
            assert_eq!(mcp_body["params"]["name"], "web_search");
        }
    }
}
