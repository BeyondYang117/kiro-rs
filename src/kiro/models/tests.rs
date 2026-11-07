use super::*;
use axum::{
    Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn cli_catalog_uses_control_plane_operation_and_keeps_pagination() {
    let app = Router::new().route("/", axum::routing::post(
        |Query(query): Query<BTreeMap<String,String>>, headers: HeaderMap, body: String| async move {
            assert_eq!(headers["x-amz-target"], "KiroControlPlaneBearerService.ListAvailableModels");
            assert_eq!(headers["content-type"], "application/x-amz-json-1.0");
            assert_eq!(headers["authorization"], "Bearer test-token");
            assert!(headers["user-agent"].to_str().unwrap().contains("app/AmazonQ-For-CLI"));
            assert!(!headers.contains_key("x-amz-user-agent"));
            assert_eq!(query["origin"], "CLI");
            assert_eq!(query["profileArn"], "profile+/=&");
            assert_eq!(body, "{}");
            axum::Json(if query.contains_key("nextToken") {
                assert_eq!(query["nextToken"], "cursor+/=&");
                serde_json::json!({"models":[model("claude-opus-4.6")]})
            } else {
                serde_json::json!({"models":[model("claude-sonnet-4.6")],"nextToken":"cursor+/=&","defaultModel":{"modelId":"claude-sonnet-4.6"}})
            })
        }
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = Config::default();
    assert_eq!(config.default_endpoint, "cli");
    let credentials = KiroCredentials {
        profile_arn: Some("profile+/=&".into()),
        ..Default::default()
    };
    let client = build_client(None, 5, config.tls_backend).unwrap();
    let catalog = fetch_pages(&client, &url, &credentials, &config, "test-token")
        .await
        .unwrap();
    assert_eq!(catalog.models.len(), 2);
    assert_eq!(
        catalog.default_model_id.as_deref(),
        Some("claude-sonnet-4.6")
    );
    task.abort();
}

fn model(id: &str) -> Value {
    serde_json::json!({"modelId":id, "modelName":format!("Name {id}"), "modelProvider":"provider", "tokenLimits":{"maxInputTokens":1_000_000,"maxOutputTokens":64_000}})
}

#[test]
fn discovers_provider_specific_schemas_without_promoting_unknown_models() {
    for (id, path, supported) in [
        ("claude-sonnet-5.5", "output_config", true),
        ("gpt-5.6-sol", "reasoning", false),
        ("kimi-k3", "reasoning", false),
        ("claude-haiku-6", "output_config", false),
        ("claude-opus-5.9", "output_config", false),
    ] {
        let mut value = model(id);
        value["rateMultiplier"] = 0.5.into();
        value["additionalModelRequestFieldsSchema"] = serde_json::json!({"properties":{
            path:{"properties":{"effort":{"enum":["low","medium","high","low"],"default":"medium"}}},
            "thinking":{"properties":{"type":{"enum":["adaptive","disabled"],"default":"adaptive"}}}
        }});
        let model = normalize(&value).unwrap();
        assert_eq!(model.effort_levels, ["low", "medium", "high"]);
        assert_eq!(
            model.effort_schema_path.as_deref(),
            Some(format!("{path}.effort").as_str())
        );
        assert_eq!(model.default_effort.as_deref(), Some("medium"));
        assert_eq!(model.thinking_types, ["adaptive", "disabled"]);
        assert_eq!(model.default_thinking.as_deref(), Some("adaptive"));
        assert_eq!(model.proxy_supported, supported);
        assert_eq!(model.max_input_tokens, Some(1_000_000));
        assert_eq!(model.max_output_tokens, Some(64_000));
        assert_eq!(model.rate_multiplier, Some(0.5));
    }
    let mut value = model("unknown");
    value["rateMultiplier"] = (-1).into();
    value["tokenLimits"]["maxInputTokens"] = (-100).into();
    value["additionalModelRequestFieldsSchema"] = serde_json::json!({"properties":{
        "reasoning":{"properties":{"effort":{"enum":["low"],"default":"invalid"}}}
    }});
    let model = normalize(&value).unwrap();
    assert_eq!(model.rate_multiplier, None);
    assert_eq!(model.max_input_tokens, None);
    assert_eq!(model.default_effort, None);
    assert!(normalize(&serde_json::json!({"unknown":true})).is_err());
}

#[test]
fn incomplete_or_oversized_enums_do_not_become_constraints() {
    for values in [
        serde_json::json!(["low", 9]),
        serde_json::json!(["low", null]),
        serde_json::json!(["low", ""]),
        serde_json::json!(["low", "x".repeat(65)]),
        serde_json::json!(vec!["low"; 33]),
    ] {
        let mut value = model("claude-sonnet-4.6");
        value["additionalModelRequestFieldsSchema"] = serde_json::json!({"properties":{
            "output_config":{"properties":{"effort":{"enum":values,"default":"low"}}},
            "thinking":{"properties":{"type":{"enum":values,"default":"low"}}}
        }});
        let normalized = normalize(&value).unwrap();
        assert!(normalized.effort_levels.is_empty());
        assert!(normalized.thinking_types.is_empty());
        assert!(normalized.default_effort.is_none());
        assert!(normalized.default_thinking.is_none());
    }
}

#[tokio::test]
async fn paginated_transport_encodes_queries_authenticates_and_deduplicates() {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new().route("/List-Available-Models", get(
        |State(hits): State<Arc<AtomicUsize>>, Query(query): Query<BTreeMap<String,String>>, headers: HeaderMap| async move {
            hits.fetch_add(1, Ordering::SeqCst);
            assert_eq!(headers["authorization"], "Bearer private-test-key");
            assert_eq!(headers["tokentype"], "API_KEY");
            assert_eq!(query["origin"], "AI_EDITOR");
            assert_eq!(query["profileArn"], "arn:aws:codewhisperer:us-east-1:account:profile/a+b&c");
            let body = if let Some(cursor) = query.get("nextToken") {
                assert_eq!(cursor, "next+/=&?");
                serde_json::json!({"models":[model("kimi-k3"),model("claude-sonnet-5.5")]})
            } else {
                serde_json::json!({"models":[model("claude-sonnet-5.5")],"defaultModel":{"modelId":"claude-sonnet-5.5"},"nextToken":"next+/=&?"})
            };
            axum::Json(body)
        }
    )).with_state(hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/List-Available-Models",
        listener.local_addr().unwrap()
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut config = Config::default();
    config.default_endpoint = "ide".into();
    let credentials = KiroCredentials {
        kiro_api_key: Some("private-test-key".into()),
        profile_arn: Some("arn:aws:codewhisperer:us-east-1:account:profile/a+b&c".into()),
        ..Default::default()
    };
    let client = build_client(None, 5, config.tls_backend).unwrap();
    let catalog = fetch_pages(&client, &url, &credentials, &config, "private-test-key")
        .await
        .unwrap();
    assert_eq!(catalog.models.len(), 2);
    assert_eq!(
        catalog.default_model_id.as_deref(),
        Some("claude-sonnet-5.5")
    );
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let serialized = serde_json::to_string(&catalog).unwrap();
    assert!(!serialized.contains("private-test-key"));
    assert!(!serialized.contains("account:profile"));
    task.abort();
}

#[tokio::test]
async fn partial_catalogs_are_rejected_on_pagination_or_protocol_failure() {
    let app = Router::new().route("/{mode}", get(
        |axum::extract::Path(mode): axum::extract::Path<String>, Query(query): Query<BTreeMap<String,String>>| async move {
            use axum::response::IntoResponse;
            let next = query.get("nextToken").map(String::as_str);
            match mode.as_str() {
                "empty" => axum::Json(serde_json::json!({"models":[]})).into_response(),
                "missing" => axum::Json(serde_json::json!({"model":[]})).into_response(),
                "invalid" => axum::Json(serde_json::json!({"models":[{"modelName":"no id"}]})).into_response(),
                "repeat" => axum::Json(serde_json::json!({"models":[model("known")],"nextToken":"same"})).into_response(),
                "cycle" => axum::Json(serde_json::json!({"models":[],"nextToken":if next == Some("a") {"b"} else {"a"}})).into_response(),
                "cap" => axum::Json(serde_json::json!({"models":[],"nextToken":(next.unwrap_or("0").parse::<u32>().unwrap() + 1).to_string()})).into_response(),
                "second-fails" if next.is_none() => axum::Json(serde_json::json!({"models":[model("known")],"nextToken":"next"})).into_response(),
                "large" => "x".repeat(1024 * 1024 + 1).into_response(),
                "redirect" => (StatusCode::TEMPORARY_REDIRECT, [("location", "/empty")], "private-response").into_response(),
                _ => (StatusCode::FORBIDDEN, "private-response").into_response(),
            }
        }
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut config = Config::default();
    config.default_endpoint = "ide".into();
    let credentials = KiroCredentials::default();
    let client = build_client(None, 5, config.tls_backend).unwrap();
    assert!(
        fetch_pages(
            &client,
            &format!("{url}/empty"),
            &credentials,
            &config,
            "test"
        )
        .await
        .unwrap()
        .models
        .is_empty()
    );
    for mode in [
        "missing",
        "invalid",
        "repeat",
        "cycle",
        "cap",
        "second-fails",
        "large",
        "redirect",
    ] {
        let error = fetch_pages(
            &client,
            &format!("{url}/{mode}"),
            &credentials,
            &config,
            "test",
        )
        .await
        .unwrap_err();
        assert!(!error.to_string().contains("private-response"), "{mode}");
    }
    task.abort();
}

#[test]
fn oauth_requests_do_not_include_api_key_token_type_or_body() {
    let mut config = Config::default();
    config.default_endpoint = "ide".into();
    let client = build_client(None, 5, config.tls_backend).unwrap();
    let req = request(
        &client,
        "https://management.us-east-1.kiro.dev/List-Available-Models",
        &KiroCredentials::default(),
        &config,
        "private-token",
        None,
    )
    .build()
    .unwrap();
    assert!(req.body().is_none());
    assert!(!req.headers().contains_key("tokentype"));
    assert_eq!(req.url().query(), Some("origin=AI_EDITOR"));
    for method in ["idc", "IdC", "builder-id", "iam"] {
        let credentials = KiroCredentials {
            auth_method: Some(method.into()),
            ..Default::default()
        };
        let req = request(
            &client,
            "https://management.us-east-1.kiro.dev/List-Available-Models",
            &credentials,
            &config,
            "private-token",
            None,
        )
        .build()
        .unwrap();
        assert_eq!(req.headers()["tokentype"], "SSO_OIDC");
    }
}
