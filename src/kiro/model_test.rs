//! A single account-scoped generation probe, with no retry or account fallback.
use futures::StreamExt;

use super::{
    endpoint::{CliEndpoint, IdeEndpoint, KiroEndpoint, RequestContext},
    machine_id,
    model::{credentials::KiroCredentials, events::Event},
    models::AvailableModel,
};
use crate::{
    http_client::{ProxyConfig, build_client},
    model::config::Config,
};

#[derive(Debug)]
pub(crate) enum ModelTestError {
    Authentication,
    Configuration,
    Network,
    Http(u16),
    InvalidResponse,
}

impl std::fmt::Display for ModelTestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authentication => f.write_str("无法获取该凭据的认证信息，请检查或刷新凭据"),
            Self::Configuration => f.write_str("模型测试配置无效，请检查端点、区域或代理设置"),
            Self::Network => f.write_str("模型测试连接失败，请检查网络或代理设置"),
            Self::Http(status) => write!(f, "模型测试上游返回 HTTP {status}"),
            Self::InvalidResponse => f.write_str("模型测试未收到完整有效的文本回复"),
        }
    }
}

pub(crate) async fn test(
    credentials: &KiroCredentials,
    token: &str,
    config: &Config,
    proxy: Option<&ProxyConfig>,
    model: &AvailableModel,
) -> Result<String, ModelTestError> {
    let endpoint: &dyn KiroEndpoint = match credentials
        .endpoint
        .as_deref()
        .unwrap_or(&config.default_endpoint)
    {
        "ide" => &IdeEndpoint::new(),
        "cli" => &CliEndpoint::new(),
        _ => return Err(ModelTestError::Configuration),
    };
    let machine_id = machine_id::generate_from_credentials(credentials, config);
    let ctx = RequestContext {
        credentials,
        token,
        config,
        machine_id: &machine_id,
    };
    let client =
        build_client(proxy, 15, config.tls_backend).map_err(|_| ModelTestError::Configuration)?;
    send_test(&client, endpoint, &ctx, model).await
}

fn request_body(model: &AvailableModel, ctx: &RequestContext<'_>) -> String {
    let mut body = serde_json::json!({
        "conversationState": {
            "conversationId": uuid::Uuid::new_v4().to_string(),
            "agentTaskType": "vibe",
            "chatTriggerType": "MANUAL",
            "currentMessage": {"userInputMessage": {
                "content": "Reply with OK only.",
                "modelId": model.model_id,
                "origin": super::endpoint::origin(ctx.credentials, ctx.config),
                "userInputMessageContext": {}
            }}
        }
    });
    if model.native_max_tokens == Some(true) {
        let limit = 64
            .max(model.min_output_tokens.unwrap_or(1))
            .min(model.max_output_tokens.unwrap_or(u64::MAX));
        body["additionalModelRequestFields"] = serde_json::json!({"max_tokens": limit});
    }
    body.to_string()
}

async fn send_test(
    client: &reqwest::Client,
    endpoint: &dyn KiroEndpoint,
    ctx: &RequestContext<'_>,
    model: &AvailableModel,
) -> Result<String, ModelTestError> {
    let body = endpoint.transform_api_body(&request_body(model, ctx), ctx);
    let request = client
        .post(endpoint.api_url(ctx))
        .timeout(std::time::Duration::from_secs(60))
        .header("content-type", "application/json")
        .body(body);
    let response = endpoint
        .decorate_api(request, ctx)
        .send()
        .await
        .map_err(|_| ModelTestError::Network)?;
    if !response.status().is_success() {
        return Err(ModelTestError::Http(response.status().as_u16()));
    }
    read_response(response).await
}

async fn read_response(response: reqwest::Response) -> Result<String, ModelTestError> {
    let mut stream = response.bytes_stream();
    let mut decoder = crate::anthropic::UpstreamResponseDecoder::default();
    let mut bytes = 0usize;
    let mut preview = String::new();
    let mut remaining = 512usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ModelTestError::Network)?;
        bytes = bytes.saturating_add(chunk.len());
        if bytes > 1024 * 1024 {
            return Err(ModelTestError::InvalidResponse);
        }
        let events = decoder
            .feed(&chunk)
            .map_err(|_| ModelTestError::InvalidResponse)?;
        for event in events {
            if let Event::AssistantResponse(response) = event {
                let text: String = response.content.chars().take(remaining).collect();
                remaining -= text.chars().count();
                preview.push_str(&text);
            }
        }
    }
    decoder
        .finish()
        .map_err(|_| ModelTestError::InvalidResponse)?;
    if preview.trim().is_empty() {
        return Err(ModelTestError::InvalidResponse);
    }
    Ok(crate::request_log::redact(&preview))
}

#[cfg(test)]
mod tests;
