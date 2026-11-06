use super::*;
use axum::{Router, body::to_bytes, middleware::from_fn_with_state, routing::post};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::prelude::*;

#[test]
fn proxy_url_redaction_covers_username_only_and_encoded_authentication() {
    for url in [
        "http://private-user@proxy.test:8080",
        "https://private-user:private-password@proxy.test",
        "socks5://private%40user:private%3Apassword@proxy.test:1080",
        "SOCKS5H://private-user@[::1]:1080",
        "http://:private-password@proxy.test",
    ] {
        let message = format!("proxy connection failed: {url}");
        assert!(!redact(&message).contains("private"));
        assert!(!public_error_message(&message).contains("private"));
        assert!(redact(&message).contains("proxy connection failed"));
    }
    assert_eq!(redact("http://proxy.test:8080"), "http://proxy.test:8080");
}

struct TestStore {
    store: Arc<LogStore>,
    path: std::path::PathBuf,
}
impl TestStore {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("kiro-logs-test-{}.sqlite3", uuid::Uuid::new_v4()));
        Self {
            store: LogStore::open(&path, 30).unwrap(),
            path,
        }
    }
    async fn record(&self, id: &str) -> Record {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let record: Record = {
                    let data: String = self
                        .store
                        .connection
                        .lock()
                        .query_row("SELECT data FROM request_logs WHERE id=?", [id], |r| {
                            r.get(0)
                        })
                        .unwrap();
                    serde_json::from_str(&data).unwrap()
                };
                if record.status != "running" {
                    return record;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("request log must finish")
    }
}
impl Drop for TestStore {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

fn request(path: &str) -> Request<Body> {
    Request::builder().method("POST").uri(path).header("content-type", "application/json")
        .header("x-api-key", "test-api-key").body(Body::from(r#"{"model":"claude-sonnet-4-6","max_tokens":10,"messages":[{"role":"user","content":"private-prompt-sentinel"}]}"#)).unwrap()
}

/// Exercise the real logging middleware, stream polling and SQLite persistence.
pub(crate) async fn capture_request(app: Router, request: Request<Body>) -> Value {
    let fixture = TestStore::new();
    let app = app.layer(from_fn_with_state(fixture.store.clone(), middleware));
    let record = async {
        let response = app.oneshot(request).await.unwrap();
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        to_bytes(response.into_body(), 1_000_000).await.unwrap();
        fixture.record(&id).await
    }
    .with_subscriber(tracing_subscriber::registry().with(DiagnosticLayer))
    .await;
    serde_json::to_value(record).unwrap()
}

#[tokio::test]
async fn body_failures_preserve_http_metadata_and_old_records_remain_readable() {
    let context = Arc::new(Mutex::new(Record::new("POST", "/v1/messages")));
    CURRENT
        .scope(context.clone(), async {
            attempt_started(175, "ide");
            attempt_result(Some(200), None, Some("upstream-request-1"));
            attempt_response_finished(
                UpstreamResponseDiagnostics {
                    outcome: "decode_error".into(),
                    body_bytes: 123,
                    ..Default::default()
                },
                Some("decode failed: Bearer secret-sentinel"),
            );
        })
        .await;
    let record = context.lock().clone();
    let attempt = &record.upstream_attempts[0];
    assert_eq!(attempt.status, Some(200));
    assert_eq!(
        attempt.upstream_request_id.as_deref(),
        Some("upstream-request-1")
    );
    assert!(attempt.error.as_deref().unwrap().contains("[REDACTED]"));
    assert!(
        !serde_json::to_string(&record)
            .unwrap()
            .contains("secret-sentinel")
    );
    let mut old = serde_json::to_value(&record).unwrap();
    old.as_object_mut().unwrap().remove("response");
    old["upstreamAttempts"][0]
        .as_object_mut()
        .unwrap()
        .remove("responseDiagnostics");
    for field in [
        "responseHeadersMs",
        "firstByteMs",
        "durationMs",
        "bodyBytes",
        "bodyComplete",
    ] {
        old["upstreamAttempts"][0]
            .as_object_mut()
            .unwrap()
            .remove(field);
    }
    let decoded: Record = serde_json::from_value(old).unwrap();
    assert!(decoded.upstream_attempts[0].response_diagnostics.is_none());
    assert!(decoded.upstream_attempts[0].response_headers_ms.is_none());
    assert!(decoded.upstream_attempts[0].body_complete.is_none());
    assert!(decoded.response.is_none());
}

#[test]
fn response_diagnostics_read_old_boolean_and_new_unknown_frame_states() {
    for incomplete in [json!(false), json!(true), Value::Null] {
        let diagnostic: UpstreamResponseDiagnostics = serde_json::from_value(json!({
            "outcome":"refusal", "incompleteFrame":incomplete,
        }))
        .unwrap();
        assert_eq!(diagnostic.incomplete_frame, incomplete.as_bool());
        assert_eq!(diagnostic.buffered_bytes, None);
        assert_eq!(diagnostic.refusal_category, None);
        assert_eq!(diagnostic.refusal_explanation, None);
        assert_eq!(
            serde_json::to_value(diagnostic).unwrap()["incompleteFrame"],
            incomplete
        );
    }
}

#[tokio::test]
async fn response_previews_match_json_and_fragmented_sse_and_redact_split_secrets() {
    let message = json!({"type":"message", "content":[
        {"type":"text", "text":"你好\nOK"},
        {"type":"tool_use", "id":"tool-1", "name":"Example", "input":{"password":"secret-sentinel", "query":"hello"}}
    ], "stop_reason":"tool_use"});
    let events = [
        json!({"type":"message_start", "message":{"content":[]}}),
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"你好\n"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":"OK"}}),
        json!({"type":"content_block_start", "index":1, "content_block":{"type":"tool_use", "id":"tool-1", "name":"Example", "input":{}}}),
        json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta", "partial_json":"{\"password\":\"secret-"}}),
        json!({"type":"content_block_delta", "index":1, "delta":{"type":"input_json_delta", "partial_json":"sentinel\",\"query\":\"hello\"}"}}),
        json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}}),
        json!({"type":"message_stop"}),
    ];
    let sse = events
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect::<String>();
    let mut previews = Vec::new();
    for streaming in [false, true] {
        let message = message.clone();
        let sse = sse.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move || async move {
                if streaming {
                    let chunks = sse
                        .as_bytes()
                        .chunks(3)
                        .map(|c| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(c)))
                        .collect::<Vec<_>>();
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(futures::stream::iter(chunks)))
                        .unwrap()
                } else {
                    Json(message).into_response()
                }
            }),
        );
        let record = capture_request(app, request("/v1/messages")).await;
        assert_eq!(record["status"], "success");
        assert_eq!(record["response"]["stopReason"], "tool_use");
        assert_eq!(record["response"]["truncated"], false);
        assert_eq!(record["response"]["content"][0]["text"], "你好\nOK");
        assert_eq!(
            record["response"]["content"][1]["input"]["password"],
            "[REDACTED]"
        );
        assert!(!record.to_string().contains("secret-sentinel"));
        assert!(!record.to_string().contains("private-prompt-sentinel"));
        previews.push(record["response"].clone());
    }
    assert_eq!(previews[0], previews[1]);
}

#[tokio::test]
async fn large_response_previews_are_bounded_without_changing_client_content_or_usage() {
    let fixture = TestStore::new();
    let text = "你好".repeat(MAX_CAPTURE / 3);
    let message = json!({"type":"message", "content":[{"type":"text", "text":text}],
        "stop_reason":"max_tokens", "usage":{"input_tokens":100,"output_tokens":200}});
    let expected = message.clone();
    let app = Router::new()
        .route(
            "/cc/v1/messages",
            post(move || async move {
                response_usage(&message);
                Json(message)
            }),
        )
        .layer(from_fn_with_state(fixture.store.clone(), middleware));
    let response = app.oneshot(request("/cc/v1/messages")).await.unwrap();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = to_bytes(response.into_body(), 4 * MAX_CAPTURE)
        .await
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), expected);
    let record = fixture.record(&id).await;
    let preview = record.response.as_ref().unwrap();
    assert!(preview.truncated);
    assert!(preview.content[0]["text"].as_str().unwrap().len() <= response::MAX_PREVIEW_BYTES);
    assert_eq!(preview.stop_reason.as_deref(), Some("max_tokens"));
    assert_eq!(record.output_tokens, Some(200));
    let listing = fixture.store.query(LogQuery::default()).unwrap();
    assert!(listing["items"][0].get("response").is_none());
}

#[test]
fn streaming_previews_bound_content_and_redact_after_assembling_deltas() {
    let mut preview = StreamResponsePreview::default();
    preview.observe(&json!({"type":"content_block_start", "index":0, "content_block":{"type":"text", "text":""}}));
    for text in [
        "Bearer ",
        "split-secret-sentinel",
        &"界".repeat(response::MAX_PREVIEW_BYTES),
    ] {
        preview.observe(&json!({"type":"content_block_delta", "index":0, "delta":{"type":"text_delta", "text":text}}));
    }
    preview.observe(&json!({"type":"message_delta", "delta":{"stop_reason":"end_turn"}}));
    let preview = preview.finish().unwrap();
    assert!(preview.truncated);
    assert!(
        !serde_json::to_string(&preview)
            .unwrap()
            .contains("split-secret-sentinel")
    );
    assert!(preview.content[0]["text"].as_str().unwrap().len() <= response::MAX_PREVIEW_BYTES);
}

#[tokio::test]
async fn actual_api_records_auth_validation_and_provider_errors_without_conversation_bodies() {
    let fixture = TestStore::new();
    let app = crate::anthropic::create_router_with_provider("test-api-key", None, true, true)
        .layer(from_fn_with_state(fixture.store.clone(), middleware));
    for (kind, expected) in [("auth", 401), ("json", 400), ("provider", 503)] {
        let mut req = request("/v1/messages");
        if kind == "auth" {
            req.headers_mut().remove("x-api-key");
        }
        if kind == "json" {
            *req.body_mut() = Body::from("{");
        }
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status().as_u16(), expected);
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        to_bytes(response.into_body(), 100_000).await.unwrap();
        let record = fixture.record(&id).await;
        assert_eq!(record.status, "error");
        assert_eq!(record.http_status, expected);
        assert!(record.error.as_deref().is_some_and(|e| !e.is_empty()));
        let encoded = serde_json::to_string(&record).unwrap();
        assert!(!encoded.contains("private-prompt-sentinel"));
        assert!(!encoded.contains("test-api-key"));
        if kind == "provider" {
            assert_eq!(record.model.as_deref(), Some("claude-sonnet-4-6"));
        }
    }
}

#[tokio::test]
async fn fragmented_sse_preserves_usage_and_detects_http_200_errors_and_missing_stop() {
    let fixture = TestStore::new();
    let success = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":20,\"output_tokens\":9}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n"
    );
    let failed = "data: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"CRC校验失败\"}}\n\n";
    for (body, status) in [
        (success, "success"),
        (failed, "error"),
        ("data: {\"type\":\"ping\"}\n\n", "interrupted"),
    ] {
        let app = Router::new()
            .route(
                "/v1/messages",
                post(move || async move {
                    request_metadata("test-model", true);
                    let chunks = body
                        .as_bytes()
                        .chunks(3)
                        .map(|c| Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(c)))
                        .collect::<Vec<_>>();
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(futures::stream::iter(chunks)))
                        .unwrap()
                }),
            )
            .layer(from_fn_with_state(fixture.store.clone(), middleware));
        let response = app.oneshot(request("/v1/messages")).await.unwrap();
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(response.status(), StatusCode::OK);
        let original = to_bytes(response.into_body(), 100_000).await.unwrap();
        assert_eq!(original.as_ref(), body.as_bytes());
        let record = fixture.record(&id).await;
        assert_eq!(record.status, status);
        assert!(record.stream);
        if status == "success" {
            assert_eq!(record.input_tokens, Some(20));
            assert_eq!(record.output_tokens, Some(9));
        }
        if status == "error" {
            assert!(record.error.unwrap().contains("CRC校验失败"));
        }
    }
}

#[tokio::test]
async fn dropped_stream_is_interrupted_and_active_metadata_is_queryable() {
    let fixture = TestStore::new();
    let app = Router::new()
        .route(
            "/cc/v1/messages",
            post(|| async {
                request_metadata("active-model", true);
                attempt_started(42, "ide");
                attempt_result(Some(200), None, Some("upstream-42"));
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(futures::stream::pending::<
                        Result<bytes::Bytes, std::io::Error>,
                    >()))
                    .unwrap()
            }),
        )
        .layer(from_fn_with_state(fixture.store.clone(), middleware));
    let response = app.oneshot(request("/cc/v1/messages")).await.unwrap();
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let rows = fixture
        .store
        .query(LogQuery {
            model: Some("active-model".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(rows["summary"]["running"], 1);
    drop(response);
    let record = fixture.record(&id).await;
    assert_eq!(record.status, "interrupted");
    assert_eq!(record.credential_id, Some(42));
    assert_eq!(
        record.upstream_attempts[0].upstream_request_id.as_deref(),
        Some("upstream-42")
    );
}

#[tokio::test]
async fn concurrent_stream_diagnostics_and_attempts_stay_with_their_request() {
    let fixture = TestStore::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let app = Router::new()
        .route(
            "/v1/messages/{id}",
            post(move |AxumPath(id): AxumPath<u64>| {
                let barrier = barrier.clone();
                async move {
                    request_metadata(&format!("model-{id}"), true);
                    let stream = futures::stream::once(async move {
                        barrier.wait().await;
                        attempt_started(id, "ide");
                        attempt_result(Some(429), Some(&format!("specific-error-{id}")), None);
                        tracing::warn!(credential_id = id, "request-specific-warning-{id}");
                        Ok::<_, std::io::Error>(bytes::Bytes::from_static(
                            b"data: {\"type\":\"message_stop\"}\n\n",
                        ))
                    });
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from_stream(stream))
                        .unwrap()
                }
            }),
        )
        .layer(from_fn_with_state(fixture.store.clone(), middleware));
    let subscriber = tracing_subscriber::registry().with(DiagnosticLayer);
    let dispatch = tracing::Dispatch::new(subscriber);
    let run = |id: u64| {
        let app = app.clone();
        let dispatch = dispatch.clone();
        async move {
            let response = app
                .oneshot(request(&format!("/v1/messages/{id}")))
                .await
                .unwrap();
            let request_id = response.headers()["x-request-id"]
                .to_str()
                .unwrap()
                .to_owned();
            to_bytes(response.into_body(), 10000).await.unwrap();
            request_id
        }
        .with_subscriber(dispatch)
    };
    let (a, b) = tokio::join!(run(11), run(22));
    for (id, key, other) in [(&a, 11, 22), (&b, 22, 11)] {
        let record = fixture.record(id).await;
        assert_eq!(
            record.model.as_deref(),
            Some(format!("model-{key}").as_str())
        );
        assert_eq!(record.upstream_attempts.len(), 1);
        assert_eq!(record.upstream_attempts[0].credential_id, key);
        assert_eq!(record.diagnostics.len(), 1);
        assert!(
            record.diagnostics[0]
                .message
                .contains(&format!("request-specific-warning-{key}"))
        );
        assert!(
            !record.diagnostics[0]
                .message
                .contains(&format!("request-specific-warning-{other}"))
        );
    }
}

#[test]
fn database_survives_restart_marks_unfinished_calls_and_keeps_private_permissions() {
    let fixture = TestStore::new();
    let mut completed = Record::new("POST", "/v1/messages");
    completed.status = "success".into();
    fixture.store.save(&completed).unwrap();
    let running = Record::new("POST", "/v1/messages");
    fixture.store.save(&running).unwrap();
    let reopened = LogStore::open(&fixture.path, 30).unwrap();
    let result = reopened.query(LogQuery::default()).unwrap();
    assert_eq!(result["summary"]["success"], 1);
    assert_eq!(result["summary"]["interrupted"], 1);
    assert_eq!(result["summary"]["running"], 0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["", "-wal", "-shm"] {
            let mode = std::fs::metadata(format!("{}{suffix}", fixture.path.display()))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}

#[test]
fn queries_filter_paginate_aggregate_and_treat_search_as_literal() {
    let fixture = TestStore::new();
    for i in 0..25 {
        let mut record = Record::new("POST", "/v1/messages");
        record.status = if i % 2 == 0 { "success" } else { "error" }.into();
        record.model = Some("model-a".into());
        record.credential_id = Some(7);
        record.http_status = if i % 2 == 0 { 200 } else { 502 };
        record.input_tokens = Some(10);
        record.output_tokens = Some(2);
        record.error = Some("literal %_ quote' SQL -- diagnostic".into());
        fixture.store.save(&record).unwrap();
    }
    let query = LogQuery {
        model: Some("model-a".into()),
        status: Some("error".into()),
        credential_id: Some(7),
        http_status: Some(502),
        from: Some(Utc::now().timestamp_millis() - 60000),
        to: Some(Utc::now().timestamp_millis() + 60000),
        search: Some("%_ quote' SQL --".into()),
        page: Some(2),
        page_size: Some(5),
    };
    let rows = fixture.store.query(query).unwrap();
    assert_eq!(rows["items"].as_array().unwrap().len(), 5);
    assert_eq!(rows["summary"]["total"], 12);
    assert_eq!(rows["summary"]["inputTokens"], 120);
    assert_eq!(rows["summary"]["outputTokens"], 24);
    assert!(rows["items"][0].get("diagnostics").is_none());
    let injection = fixture
        .store
        .query(LogQuery {
            search: Some("' OR 1=1 --".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(injection["summary"]["total"], 0);
}

#[test]
fn expired_rows_are_removed_and_compaction_iterations_are_summed() {
    let fixture = TestStore::new();
    let mut record = Record::new("POST", "/v1/messages");
    record.timestamp -= 31 * 86_400_000;
    fixture.store.save(&record).unwrap();
    assert_eq!(
        fixture.store.query(LogQuery::default()).unwrap()["summary"]["total"],
        0
    );
    let count: u64 = fixture
        .store
        .connection
        .lock()
        .query_row("SELECT COUNT(*) FROM request_logs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    apply_usage(
        &mut record,
        &json!({"input_tokens":0,"output_tokens":0,"iterations":[
        {"input_tokens":100,"output_tokens":20},{"input_tokens":30,"output_tokens":40}]}),
    );
    assert_eq!(record.input_tokens, Some(130));
    assert_eq!(record.output_tokens, Some(60));
}

#[test]
fn completed_calls_cannot_be_overwritten_by_late_active_snapshots_and_filter_matches_failover() {
    let fixture = TestStore::new();
    let mut record = Record::new("POST", "/v1/messages");
    let initial = record.clone();
    record.status = "success".into();
    record.credential_id = Some(2);
    record.upstream_attempts = vec![Attempt {
        timestamp: record.timestamp,
        credential_id: 1,
        endpoint: "ide".into(),
        status: Some(402),
        error: Some("quota exhausted".into()),
        upstream_request_id: None,
        response_headers_ms: None,
        first_byte_ms: None,
        duration_ms: None,
        body_bytes: None,
        body_complete: None,
        started: None,
        response_diagnostics: None,
    }];
    fixture.store.save(&record).unwrap();
    fixture.store.save(&initial).unwrap();
    let result = fixture
        .store
        .query(LogQuery {
            credential_id: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(result["summary"]["total"], 1);
    assert_eq!(result["items"][0]["status"], "success");
    assert_eq!(result["items"][0]["credentialId"], 2);
}

#[tokio::test]
async fn log_list_and_detail_require_admin_auth_and_validate_dates() {
    let fixture = TestStore::new();
    let record = Record::new("GET", "/v1/models");
    fixture.store.save(&record).unwrap();
    let manager = crate::kiro::token_manager::MultiTokenManager::new(
        crate::model::config::Config::default(),
        vec![],
        None,
        None,
        false,
    )
    .unwrap();
    let service = crate::admin::AdminService::new(Arc::new(manager), vec!["ide".into()]);
    let app =
        crate::admin::create_admin_router(crate::admin::AdminState::new("admin-test-key", service))
            .layer(Extension(fixture.store.clone()));
    for path in ["/logs".to_owned(), format!("/logs/{}", record.id)] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("x-api-key", "admin-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    for authenticated in [false, true] {
        let mut request = Request::builder().uri("/logs?page=private-query-value");
        if authenticated {
            request = request.header("x-api-key", "admin-test-key");
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            if authenticated { 400 } else { 401 }
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(
            body["error"]["type"],
            if authenticated {
                "invalid_request"
            } else {
                "authentication_error"
            }
        );
        assert!(!body.to_string().contains("private-query-value"));
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/logs?from=100&to=1")
                .header("x-api-key", "admin-test-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/logs/missing")
                .header("x-api-key", "admin-test-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn errors_keep_causes_but_redact_tokens_and_proxy_auth() {
    let message = r#"403 {"refreshToken":"refresh-secret","access_token":"access-secret","clientSecret":"client-secret","apiKey":"key-secret"} Authorization: Bearer header-secret https://proxyuser:proxypass@proxy.test:8080 sk-plain-key eyJheader.eyJpayload.signature root cause: certificate verify failed"#;
    let redacted = redact(message);
    for secret in [
        "refresh-secret",
        "access-secret",
        "client-secret",
        "key-secret",
        "header-secret",
        "proxyuser",
        "proxypass",
        "sk-plain-key",
        "eyJheader",
    ] {
        assert!(
            !redacted.contains(secret),
            "secret leaked: {secret}: {redacted}"
        );
    }
    assert!(redacted.contains("certificate verify failed"));
}

#[test]
fn quoted_and_escaped_secret_values_are_redacted_as_complete_values() {
    let message = r#"root cause {"kiroApiKey":"private kiro value","accessToken":"private\"escaped value","clientSecret":"private\\slash value"} refreshToken='private single quoted value' ksk_private_key ADMIN_API_KEY=private-admin KIRO_API_KEY=private-env Authorization: Basic private-basic"#;
    let safe = redact(message);
    assert!(safe.contains("root cause"));
    assert!(!safe.contains("private"), "{safe}");
    assert!(!safe.contains("escaped value"), "{safe}");
    assert!(!safe.contains("slash value"), "{safe}");
    assert!(!safe.contains("quoted value"), "{safe}");
    let public = public_error_message(&format!("{message}\n\t{}", "中".repeat(6000)));
    assert!(!public.contains("private"));
    assert!(!public.chars().any(char::is_control));
    assert_eq!(public.chars().count(), 4096);
    let admin = crate::admin::types::AdminErrorResponse::api_error(message);
    assert!(!admin.error.message.contains("private"));
    for truncated in [
        r#"{"accessToken":"private unterminated value"#,
        "refreshToken='private unterminated value",
        r#"{"accessToken":"private trailing escape\"#,
    ] {
        assert!(!redact(truncated).contains("private"), "{truncated}");
    }
}

#[tokio::test]
async fn real_provider_records_api_and_search_retries_with_full_upstream_errors() {
    use crate::kiro::{
        endpoint::KiroEndpoint, model::credentials::KiroCredentials, provider::KiroProvider,
        token_manager::MultiTokenManager,
    };
    for (search, streaming) in [(false, false), (false, true), (true, false), (true, true)] {
        let fixture = TestStore::new();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let captured = calls.clone();
        let upstream = Router::new().route("/",post(move || {
            let captured = captured.clone();
            async move {
                let first = captured.fetch_add(1,std::sync::atomic::Ordering::Relaxed) == 0;
                Response::builder().status(if first {429} else {400})
                    .header("x-amzn-requestid",if first {"upstream-1"} else {"upstream-2"})
                    .body(Body::from(if first {r#"{"reason":"temporarily overloaded","accessToken":"hidden-token"}"#}
                        else {r#"{"reason":"UnsupportedConfiguration","message":"exact upstream rejection detail"}"#})).unwrap()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let manager = MultiTokenManager::new(
            serde_json::from_value(serde_json::json!({
                "concurrency":{"rateLimitCooldownSeconds":1}
            }))
            .unwrap(),
            vec![KiroCredentials {
                id: Some(7),
                kiro_api_key: Some("unused-test-key".into()),
                ..Default::default()
            }],
            None,
            None,
            false,
        )
        .unwrap();
        let endpoints: std::collections::HashMap<String, Arc<dyn KiroEndpoint>> = [(
            "ide".into(),
            Arc::new(LogTestEndpoint(format!("http://{address}/"))) as Arc<dyn KiroEndpoint>,
        )]
        .into();
        let provider = KiroProvider::with_proxy(Arc::new(manager), None, endpoints, "ide".into());
        let app = crate::anthropic::create_router_with_provider(
            "test-api-key",
            Some(provider),
            true,
            true,
        )
        .layer(from_fn_with_state(fixture.store.clone(), middleware));
        let mut payload = json!({"model":"claude-sonnet-4-6","max_tokens":10,"stream":streaming,
            "messages":[{"role":"user","content":"private search query"}]});
        if search {
            payload["tools"] = json!([{"type":"web_search_20250305","name":"web_search"}]);
        }
        let mut req = request("/v1/messages");
        *req.body_mut() = Body::from(payload.to_string());
        let subscriber = tracing_subscriber::registry().with(DiagnosticLayer);
        let response = app.oneshot(req).with_subscriber(subscriber).await.unwrap();
        let id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        to_bytes(response.into_body(), 100_000).await.unwrap();
        let record = fixture.record(&id).await;
        assert_eq!(record.status, "error");
        assert_eq!(record.attempts, 2);
        assert_eq!(record.stream, streaming);
        assert_eq!(record.upstream_attempts[0].status, Some(429));
        assert_eq!(record.upstream_attempts[1].status, Some(400));
        for attempt in &record.upstream_attempts {
            assert!(attempt.response_headers_ms.is_some());
            assert!(attempt.first_byte_ms.is_some());
            assert!(attempt.duration_ms.is_some());
            assert!(attempt.body_bytes.unwrap() > 0);
            assert_eq!(attempt.body_complete, Some(true));
        }
        assert_eq!(
            record.upstream_attempts[1].upstream_request_id.as_deref(),
            Some("upstream-2")
        );
        assert!(
            record
                .error
                .as_deref()
                .unwrap()
                .contains("exact upstream rejection detail")
        );
        assert!(
            !serde_json::to_string(&record)
                .unwrap()
                .contains("hidden-token")
        );
        assert!(
            !serde_json::to_string(&record)
                .unwrap()
                .contains("private search query")
        );
        assert!(record.diagnostics.iter().any(|d| d.message.contains("429")));
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
        server.abort();
    }
}

struct LogTestEndpoint(String);
impl crate::kiro::endpoint::KiroEndpoint for LogTestEndpoint {
    fn name(&self) -> &'static str {
        "ide"
    }
    fn api_url(&self, _: &crate::kiro::endpoint::RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn mcp_url(&self, _: &crate::kiro::endpoint::RequestContext<'_>) -> String {
        self.0.clone()
    }
    fn decorate_api(
        &self,
        r: reqwest::RequestBuilder,
        _: &crate::kiro::endpoint::RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        r
    }
    fn decorate_mcp(
        &self,
        r: reqwest::RequestBuilder,
        _: &crate::kiro::endpoint::RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        r
    }
    fn transform_api_body(
        &self,
        body: &str,
        _: &crate::kiro::endpoint::RequestContext<'_>,
    ) -> String {
        body.into()
    }
}

#[tokio::test]
async fn provider_progress_survives_cancellation_before_downstream_headers() {
    use crate::kiro::{
        endpoint::KiroEndpoint, model::credentials::KiroCredentials, provider::KiroProvider,
        token_manager::MultiTokenManager,
    };
    use futures::StreamExt;

    // Exercise both raw event streaming and the bounded MCP/text body reader.
    for stream_body in [false, true] {
        for phase in ["sending", "headers", "partial", "complete"] {
            let fixture = TestStore::new();
            let upstream = Router::new().route(
                "/",
                post(move || async move {
                    if phase == "sending" {
                        return futures::future::pending::<Response>().await;
                    }
                    let prefix = if phase == "headers" { "" } else { "hello" };
                    let body = futures::stream::once(async move {
                        Ok::<_, std::io::Error>(bytes::Bytes::from_static(prefix.as_bytes()))
                    });
                    let body = if phase == "complete" {
                        body.boxed()
                    } else {
                        body.chain(futures::stream::pending()).boxed()
                    };
                    Response::builder()
                        .header("x-request-id", "pending-upstream")
                        .body(Body::from_stream(body))
                        .unwrap()
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server =
                tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
            let manager = Arc::new(
                MultiTokenManager::new(
                    Default::default(),
                    vec![KiroCredentials {
                        id: Some(176),
                        kiro_api_key: Some("test-key".into()),
                        ..Default::default()
                    }],
                    None,
                    None,
                    false,
                )
                .unwrap(),
            );
            let endpoints = std::collections::HashMap::from([(
                "ide".into(),
                Arc::new(LogTestEndpoint(format!("http://{address}/"))) as Arc<dyn KiroEndpoint>,
            )]);
            let provider = Arc::new(KiroProvider::with_proxy(
                manager.clone(),
                None,
                endpoints,
                "ide".into(),
            ));
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let sender = Arc::new(Mutex::new(Some(sender)));
            let app = Router::new()
                .route(
                    "/cc/v1/messages",
                    post(move || {
                        let provider = provider.clone();
                        let sender = sender.clone();
                        async move {
                            request_metadata("test-model", false);
                            let context = CURRENT.with(Clone::clone);
                            let _ = sender.lock().take().unwrap().send(context);
                            let response = provider.call_api("{}").await.unwrap();
                            if stream_body {
                                let mut body = response.bytes_stream();
                                while let Some(chunk) = body.next().await {
                                    chunk.unwrap();
                                }
                            } else {
                                response.text().await.unwrap();
                            }
                            Json(json!({"ok":true}))
                        }
                    }),
                )
                .layer(from_fn_with_state(fixture.store.clone(), middleware));
            let task = tokio::spawn(app.oneshot(request("/cc/v1/messages")));
            let context: RequestContext = receiver.await.unwrap();
            let id = context.lock().id.clone();
            if phase == "complete" {
                let response = task.await.unwrap().unwrap();
                to_bytes(response.into_body(), 100_000).await.unwrap();
            } else {
                tokio::time::timeout(std::time::Duration::from_secs(3), async {
                    loop {
                        let ready =
                            context
                                .lock()
                                .upstream_attempts
                                .last()
                                .is_some_and(|attempt| {
                                    if phase == "sending" {
                                        attempt.status.is_none()
                                    } else {
                                        attempt.status == Some(200)
                                            && (phase != "partial" || attempt.body_bytes == Some(5))
                                    }
                                });
                        if ready {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            }
            let record = fixture.record(&id).await;
            let attempt = &record.upstream_attempts[0];
            assert!(attempt.duration_ms.is_some());
            if phase == "complete" {
                assert_eq!(record.status, "success");
                assert_eq!(attempt.body_complete, Some(true));
                assert!(attempt.error.is_none());
            } else {
                assert_eq!(record.status, "interrupted");
                assert_eq!(record.http_status, 0);
                assert!(record.first_byte_ms.is_none());
                assert!(record.error.as_deref().unwrap().contains("响应头前被取消"));
                assert!(attempt.error.is_some());
            }
            assert_eq!(attempt.response_headers_ms.is_some(), phase != "sending");
            assert_eq!(
                attempt.first_byte_ms.is_some(),
                matches!(phase, "partial" | "complete")
            );
            assert_eq!(
                attempt.body_bytes,
                match phase {
                    "sending" => None,
                    "headers" => Some(0),
                    _ => Some(5),
                }
            );
            if let (Some(headers), Some(first)) =
                (attempt.response_headers_ms, attempt.first_byte_ms)
            {
                assert!(headers <= first && first <= attempt.duration_ms.unwrap());
            }
            assert_eq!(manager.snapshot().concurrency.in_flight, 0);
            server.abort();
        }
    }
}
