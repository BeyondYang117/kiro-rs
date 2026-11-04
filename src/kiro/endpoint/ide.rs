//! Kiro IDE 端点
//!
//! 对应 Kiro IDE 客户端目前使用的 AWS CodeWhisperer 端点：
//! - API: `https://q.{api_region}.amazonaws.com/generateAssistantResponse`
//! - MCP: `https://q.{api_region}.amazonaws.com/mcp`
//!
//! 请求头使用 aws-sdk-js User-Agent 标识。请求体会在根对象上注入 `profileArn`。

use reqwest::RequestBuilder;
use uuid::Uuid;

use super::{KiroEndpoint, RequestContext};

/// Kiro IDE 端点名称
pub const IDE_ENDPOINT_NAME: &str = "ide";

/// Kiro IDE 端点
pub struct IdeEndpoint;

impl IdeEndpoint {
    pub fn new() -> Self {
        Self
    }

    fn api_region<'a>(&self, ctx: &'a RequestContext<'_>) -> &'a str {
        ctx.credentials.effective_api_region(ctx.config)
    }

    fn host(&self, ctx: &RequestContext<'_>) -> String {
        format!("q.{}.amazonaws.com", self.api_region(ctx))
    }

    fn x_amz_user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 KiroIDE-{}-{}",
            ctx.config.kiro_version, ctx.machine_id
        )
    }

    fn user_agent(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "aws-sdk-js/1.0.34 ua/2.1 os/{} lang/js md/nodejs#{} api/codewhispererstreaming#1.0.34 m/E KiroIDE-{}-{}",
            ctx.config.system_version,
            ctx.config.node_version,
            ctx.config.kiro_version,
            ctx.machine_id
        )
    }
}

impl Default for IdeEndpoint {
    fn default() -> Self {
        Self::new()
    }
}

impl KiroEndpoint for IdeEndpoint {
    fn name(&self) -> &'static str {
        IDE_ENDPOINT_NAME
    }

    fn api_url(&self, ctx: &RequestContext<'_>) -> String {
        format!(
            "https://q.{}.amazonaws.com/generateAssistantResponse",
            self.api_region(ctx)
        )
    }

    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        format!("https://q.{}.amazonaws.com/mcp", self.api_region(ctx))
    }

    fn decorate_api(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amzn-codewhisperer-optout", "true")
            .header("x-amzn-kiro-agent-mode", "vibe")
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        }
        req
    }

    fn decorate_mcp(&self, req: RequestBuilder, ctx: &RequestContext<'_>) -> RequestBuilder {
        let mut req = req
            .header("x-amz-user-agent", self.x_amz_user_agent(ctx))
            .header("user-agent", self.user_agent(ctx))
            .header("host", self.host(ctx))
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=3")
            .header("Authorization", format!("Bearer {}", ctx.token));

        if let Some(ref arn) = ctx.credentials.profile_arn {
            req = req.header("x-amzn-kiro-profile-arn", arn);
        }
        if ctx.credentials.is_api_key_credential() {
            req = req.header("tokentype", "API_KEY");
        }
        req
    }

    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        let body = inject_profile_arn(body, &ctx.credentials.profile_arn);
        let native_thinking = ctx
            .config
            .endpoints
            .get(IDE_ENDPOINT_NAME)
            .and_then(|options| options.get("nativeThinking"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if native_thinking {
            body
        } else {
            legacy_thinking_body(&body)
        }
    }
}

/// Keep the established q.amazonaws.com wire format by default. Native model
/// controls are opt-in until the selected account/region has been verified.
fn legacy_thinking_body(body: &str) -> String {
    use serde_json::{Value, json};

    let Ok(mut request) = serde_json::from_str::<Value>(body) else {
        return body.to_owned();
    };
    let fields = request
        .as_object_mut()
        .and_then(|root| root.remove("additionalModelRequestFields"));
    let prefix = fields.as_ref().and_then(|fields| {
        match fields.pointer("/thinking/type").and_then(Value::as_str) {
            Some("enabled") => Some(format!(
                "<thinking_mode>enabled</thinking_mode><max_thinking_length>{}</max_thinking_length>",
                fields.pointer("/thinking/budget_tokens").and_then(Value::as_i64)
                    .unwrap_or(20_000).min(24_576)
            )),
            Some("adaptive") => Some(format!(
                "<thinking_mode>adaptive</thinking_mode><thinking_effort>{}</thinking_effort>",
                fields.pointer("/output_config/effort").and_then(Value::as_str).unwrap_or("high")
            )),
            _ => None,
        }
    });
    if let Some(state) = request.get_mut("conversationState") {
        let model = state
            .pointer("/currentMessage/userInputMessage/modelId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if let Some(history) = state.get_mut("history").and_then(Value::as_array_mut) {
            for message in history.iter_mut() {
                if let Some(assistant) = message
                    .get_mut("assistantResponseMessage")
                    .and_then(Value::as_object_mut)
                    && let Some(reasoning) = assistant.remove("reasoningContent")
                    && let Some(text) = reasoning
                        .pointer("/reasoningText/text")
                        .and_then(Value::as_str)
                {
                    let content = assistant
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    assistant.insert(
                        "content".into(),
                        json!(format!("<thinking>{text}</thinking>\n\n{content}")),
                    );
                }
            }
        }
        if let Some(prefix) = prefix {
            // Use a separate instruction pair, preserving user text and tools.
            let Some(state) = state.as_object_mut() else {
                return body.to_owned();
            };
            let history = state.entry("history").or_insert_with(|| json!([]));
            if let Some(history) = history.as_array_mut() {
                history.splice(0..0, [
                    json!({"userInputMessage":{"content":prefix,"modelId":model,"origin":"AI_EDITOR"}}),
                    json!({"assistantResponseMessage":{"content":"I will follow these instructions."}}),
                ]);
            }
        }
    }
    serde_json::to_string(&request).unwrap_or_else(|_| body.to_owned())
}

/// 将 profile_arn 注入到请求体 JSON 根对象
fn inject_profile_arn(request_body: &str, profile_arn: &Option<String>) -> String {
    if let Some(arn) = profile_arn
        && let Ok(mut json) = serde_json::from_str::<serde_json::Value>(request_body)
    {
        json["profileArn"] = serde_json::Value::String(arn.clone());
        if let Ok(body) = serde_json::to_string(&json) {
            return body;
        }
    }
    request_body.to_string()
}

#[cfg(test)]
mod tests {
    use super::{IdeEndpoint, inject_profile_arn};
    use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
    use crate::kiro::model::credentials::KiroCredentials;
    use crate::model::config::Config;
    use serde_json::{Value, json};

    fn transform(body: Value, native: bool) -> Value {
        let mut config = Config::default();
        if native {
            config
                .endpoints
                .insert("ide".into(), json!({"nativeThinking":true}));
        }
        let credentials = KiroCredentials {
            profile_arn: Some("test-arn".into()),
            ..Default::default()
        };
        let ctx = RequestContext {
            credentials: &credentials,
            token: "test",
            machine_id: "test",
            config: &config,
        };
        serde_json::from_str(&IdeEndpoint::new().transform_api_body(&body.to_string(), &ctx))
            .unwrap()
    }

    #[test]
    fn ordinary_requests_keep_the_existing_wire_format() {
        let body = json!({"conversationState":{
            "conversationId":"test", "currentMessage":{"userInputMessage":{"content":"hello", "modelId":"claude-sonnet-4.6"}}
        }});
        let mut expected = body.clone();
        expected["profileArn"] = json!("test-arn");
        assert_eq!(transform(body, false), expected);
    }

    #[test]
    fn default_endpoint_does_not_send_native_controls_or_reasoning() {
        let body = json!({"conversationState":{
            "currentMessage":{"userInputMessage":{"content":"continue", "modelId":"claude-sonnet-4.6"}},
            "history":[{"assistantResponseMessage":{"content":"answer", "reasoningContent":{"reasoningText":{"text":"thought", "signature":"sig"}}}}]
        }, "additionalModelRequestFields":{"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}}});
        let result = transform(body.clone(), false);
        assert!(result.get("additionalModelRequestFields").is_none());
        let history = result["conversationState"]["history"].as_array().unwrap();
        assert!(
            history[0]["userInputMessage"]["content"]
                .as_str()
                .unwrap()
                .contains("<thinking_effort>low</thinking_effort>")
        );
        assert_eq!(
            history[2]["assistantResponseMessage"]["content"],
            "<thinking>thought</thinking>\n\nanswer"
        );
        assert!(
            history[2]["assistantResponseMessage"]
                .get("reasoningContent")
                .is_none()
        );
        assert_eq!(
            transform(body.clone(), true)["additionalModelRequestFields"],
            body["additionalModelRequestFields"]
        );
        assert_eq!(
            transform(body.clone(), true)["conversationState"],
            body["conversationState"]
        );
    }

    #[test]
    fn explicitly_disabled_thinking_adds_no_prompt_or_native_fields() {
        let body = json!({"conversationState":{"currentMessage":{"userInputMessage":{"content":"hi", "modelId":"claude-opus-5.5"}}},
            "additionalModelRequestFields":{"thinking":{"type":"disabled"},"output_config":{"effort":"medium"}}});
        let result = transform(body, false);
        assert!(result.get("additionalModelRequestFields").is_none());
        assert!(result["conversationState"].get("history").is_none());
    }

    #[test]
    fn test_inject_profile_arn_with_some() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let arn = Some("arn:aws:codewhisperer:us-east-1:123:profile/ABC".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            json["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/ABC"
        );
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_with_none() {
        let body = r#"{"conversationState":{"conversationId":"c1"}}"#;
        let result = inject_profile_arn(body, &None);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert!(json.get("profileArn").is_none());
        assert_eq!(json["conversationState"]["conversationId"], "c1");
    }

    #[test]
    fn test_inject_profile_arn_overwrites_existing() {
        let body = r#"{"conversationState":{},"profileArn":"old-arn"}"#;
        let arn = Some("new-arn".to_string());
        let result = inject_profile_arn(body, &arn);
        let json: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(json["profileArn"], "new-arn");
    }

    #[test]
    fn test_inject_profile_arn_invalid_json() {
        let body = "not-valid-json";
        let arn = Some("arn:test".to_string());
        let result = inject_profile_arn(body, &arn);
        assert_eq!(result, "not-valid-json");
    }
}
