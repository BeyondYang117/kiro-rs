use super::*;
use crate::kiro::parser::crc::crc32;

fn frame(kind: &str, payload: serde_json::Value) -> Vec<u8> {
    let mut headers = Vec::new();
    for (key, value) in [(":message-type", "event"), (":event-type", kind)] {
        headers.push(key.len() as u8);
        headers.extend_from_slice(key.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = payload.to_string();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    bytes.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&crc32(&bytes).to_be_bytes());
    bytes.extend(headers);
    bytes.extend_from_slice(payload.as_bytes());
    bytes.extend_from_slice(&crc32(&bytes).to_be_bytes());
    bytes
}

fn response(bytes: Vec<u8>) -> reqwest::Response {
    http::Response::builder()
        .body(reqwest::Body::from(bytes))
        .unwrap()
        .into()
}

#[tokio::test]
async fn response_requires_complete_nonempty_text_and_rejects_late_errors() {
    let valid = frame(
        "assistantResponseEvent",
        serde_json::json!({"content":"OK"}),
    );
    assert_eq!(read_response(response(valid.clone())).await.unwrap(), "OK");
    let refusal = frame("metadataEvent", serde_json::json!({"stopReason":"refusal"}));
    let mut late_refusal = valid.clone();
    late_refusal.extend(refusal);
    let mut incomplete = valid.clone();
    incomplete.extend_from_slice(&[0, 0]);
    let mut corrupt = valid.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    for bytes in [
        vec![],
        frame(
            "assistantResponseEvent",
            serde_json::json!({"content":"  "}),
        ),
        frame(
            "metadataEvent",
            serde_json::json!({"stopReason":"end_turn"}),
        ),
        late_refusal,
        incomplete,
        corrupt,
        vec![0; 1024 * 1024 + 1],
    ] {
        assert!(matches!(
            read_response(response(bytes)).await,
            Err(ModelTestError::InvalidResponse)
        ));
    }
    let preview = read_response(response(frame(
        "assistantResponseEvent",
        serde_json::json!({"content":"好".repeat(600)}),
    )))
    .await
    .unwrap();
    assert_eq!(preview.chars().count(), 512);
}

fn model() -> AvailableModel {
    AvailableModel {
        model_id: "gpt-5.6-sol".into(),
        model_name: "GPT".into(),
        provider: None,
        rate_multiplier: None,
        rate_unit: None,
        max_input_tokens: None,
        native_max_tokens: Some(true),
        min_output_tokens: Some(128),
        max_output_tokens: Some(1024),
        effort_levels: vec![],
        effort_schema_path: None,
        default_effort: None,
        thinking_types: vec![],
        default_thinking: None,
        thinking_displays: vec![],
        proxy_supported: true,
    }
}

#[tokio::test]
async fn single_probe_uses_selected_account_model_and_does_not_retry_http_errors() {
    use axum::{Router, extract::State, http::HeaderMap, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/",
            post(
                |State(hits): State<Arc<AtomicUsize>>,
                 headers: HeaderMap,
                 body: String| async move {
                    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
                    hits.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(headers["authorization"], "Bearer selected-account-token");
                    assert_eq!(
                        headers["x-amz-target"],
                        super::super::endpoint::cli::GENERATE_TARGET
                    );
                    assert_eq!(headers["tokentype"], "API_KEY");
                    assert_eq!(
                        body["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
                        "gpt-5.6-sol"
                    );
                    assert_eq!(
                        body["conversationState"]["currentMessage"]["userInputMessage"]["origin"],
                        "KIRO_CLI"
                    );
                    assert_eq!(body["profileArn"], "selected-profile");
                    assert_eq!(body["additionalModelRequestFields"]["max_tokens"], 128);
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        "private-token-in-error",
                    )
                },
            ),
        )
        .with_state(hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = LocalEndpoint(format!("http://{}/", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = Config::default();
    let credentials = KiroCredentials {
        kiro_api_key: Some("selected-account-token".into()),
        profile_arn: Some("selected-profile".into()),
        ..Default::default()
    };
    let ctx = RequestContext {
        credentials: &credentials,
        token: "selected-account-token",
        machine_id: "test",
        config: &config,
    };
    let client = build_client(None, 5, config.tls_backend).unwrap();
    let error = send_test(&client, &endpoint, &ctx, &model())
        .await
        .unwrap_err();
    assert!(matches!(error, ModelTestError::Http(429)));
    assert!(!error.to_string().contains("private-token"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

struct LocalEndpoint(String);
impl KiroEndpoint for LocalEndpoint {
    fn name(&self) -> &'static str {
        "cli"
    }
    fn api_url(&self, _: &RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        self.api_url(ctx)
    }
    fn decorate_api(
        &self,
        req: reqwest::RequestBuilder,
        ctx: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        CliEndpoint::new().decorate_api(req, ctx)
    }
    fn decorate_mcp(
        &self,
        req: reqwest::RequestBuilder,
        ctx: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        self.decorate_api(req, ctx)
    }
    fn transform_api_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        CliEndpoint::new().transform_api_body(body, ctx)
    }
}
