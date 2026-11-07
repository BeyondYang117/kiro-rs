//! Explicitly selected live smoke test. Normal `cargo test` never contacts Kiro.
use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, ensure};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::{
    http_client::ProxyConfig,
    kiro::{
        endpoint::{CliEndpoint, IdeEndpoint, KiroEndpoint},
        model::credentials::{CredentialsConfig, KiroCredentials},
        models::ModelCatalog,
        provider::KiroProvider,
        token_manager::MultiTokenManager,
    },
    model::config::Config,
};

const LOCAL_KEY: &str = "live-test-local-key";
const BODY_LIMIT: usize = 1024 * 1024;

struct LiveAccount {
    manager: Arc<MultiTokenManager>,
    id: u64,
    endpoint: String,
    region: String,
}

fn settings() -> anyhow::Result<LiveAccount> {
    // An explicit source avoids accidentally testing every account or using CI secrets.
    let path = std::env::var("KIRO_LIVE_CREDENTIALS").ok();
    let api_key = std::env::var("KIRO_API_KEY")
        .ok()
        .filter(|key| !key.is_empty());
    ensure!(
        path.is_some() ^ api_key.is_some(),
        "Set exactly one of KIRO_LIVE_CREDENTIALS or KIRO_API_KEY"
    );
    let mut config = match std::env::var("KIRO_LIVE_CONFIG") {
        Ok(path) => {
            ensure!(
                std::path::Path::new(&path).is_file(),
                "KIRO_LIVE_CONFIG must name an existing file"
            );
            Config::load(path).map_err(|_| anyhow::anyhow!("Cannot parse live config"))?
        }
        Err(_) => Config::default(),
    };
    let rows = if let Some(path) = path {
        ensure!(
            std::path::Path::new(&path).is_file(),
            "KIRO_LIVE_CREDENTIALS must name an existing file"
        );
        CredentialsConfig::load(path)
            .map_err(|_| anyhow::anyhow!("Cannot parse live credentials"))?
            .into_sorted_credentials()
    } else {
        vec![KiroCredentials {
            kiro_api_key: api_key,
            auth_method: Some("api_key".into()),
            ..Default::default()
        }]
    };
    let mut credential =
        select_account(rows, std::env::var("KIRO_LIVE_ACCOUNT_ID").ok().as_deref())?;
    if let Ok(endpoint) = std::env::var("KIRO_LIVE_ENDPOINT") {
        ensure!(
            matches!(endpoint.as_str(), "cli" | "ide"),
            "KIRO_LIVE_ENDPOINT must be cli or ide"
        );
        credential.endpoint = Some(endpoint);
    }
    let endpoint = credential
        .endpoint
        .clone()
        .unwrap_or_else(|| config.default_endpoint.clone());
    ensure!(
        matches!(endpoint.as_str(), "cli" | "ide"),
        "Unsupported live endpoint"
    );
    let region = credential.effective_api_region(&config).to_owned();
    if let Ok(native) = std::env::var("KIRO_LIVE_NATIVE_THINKING") {
        ensure!(
            matches!(native.as_str(), "true" | "false"),
            "KIRO_LIVE_NATIVE_THINKING must be true or false"
        );
        let options = config
            .endpoints
            .entry(endpoint.clone())
            .or_insert_with(|| json!({}));
        ensure!(
            options.is_object(),
            "Live endpoint options must be an object"
        );
        options["nativeThinking"] = (native == "true").into();
    }
    let mut proxy = config.proxy_url.as_ref().map(ProxyConfig::new);
    if let (Some(proxy), Some(user), Some(password)) =
        (&mut proxy, &config.proxy_username, &config.proxy_password)
    {
        *proxy = proxy.clone().with_auth(user, password);
    }
    // Test production paths with bounded retry/cost and no writes to source credentials,
    // statistics, protection state or request logs. OAuth refresh remains in memory.
    config.request_limits.max_upstream_attempts = 1;
    config.request_limits.total_timeout_seconds = 120;
    config.request_limits.first_content_timeout_seconds = 60;
    config.request_limits.idle_timeout_seconds = 30;
    config.tool_output_recovery_attempts = 0;
    config.force_stream = false;
    let manager = Arc::new(
        MultiTokenManager::new(config, vec![credential], proxy, None, true)
            .map_err(|_| anyhow::anyhow!("Cannot initialize live account"))?,
    );
    let id = manager.snapshot().entries[0].id;
    Ok(LiveAccount {
        manager,
        id,
        endpoint,
        region,
    })
}

fn select_account(
    rows: Vec<KiroCredentials>,
    requested: Option<&str>,
) -> anyhow::Result<KiroCredentials> {
    let mut enabled: Vec<_> = rows.into_iter().filter(|row| !row.disabled).collect();
    if let Some(requested) = requested {
        let id: u64 = requested
            .parse()
            .context("KIRO_LIVE_ACCOUNT_ID must be an integer")?;
        ensure!(
            enabled.iter().filter(|row| row.id == Some(id)).count() == 1,
            "Selected enabled account must exist exactly once"
        );
        return Ok(enabled.remove(enabled.iter().position(|row| row.id == Some(id)).unwrap()));
    }
    ensure!(
        enabled.len() == 1,
        "Multiple enabled accounts require KIRO_LIVE_ACCOUNT_ID; no enabled account is an error"
    );
    Ok(enabled.remove(0))
}

fn choose_models(catalog: &ModelCatalog, requested: Option<&str>) -> anyhow::Result<Vec<String>> {
    let available: Vec<_> = catalog
        .models
        .iter()
        .filter(|model| model.proxy_supported)
        .collect();
    if let Some(requested) = requested {
        let ids: Vec<_> = requested.split(',').map(str::trim).collect();
        ensure!(
            !ids.is_empty() && ids.len() <= 4 && ids.iter().all(|id| !id.is_empty()),
            "KIRO_LIVE_MODELS requires 1 to 4 exact IDs"
        );
        let mut selected = Vec::new();
        for id in ids {
            ensure!(
                available.iter().any(|model| model.model_id == id),
                "Requested live model must be supported and authorized by the catalog"
            );
            ensure!(
                !selected.iter().any(|selected| selected == id),
                "Duplicate live models are not allowed"
            );
            selected.push(id.to_owned());
        }
        return Ok(selected);
    }
    // One representative per provider; never guess authorization from a model alias.
    let mut selected = Vec::new();
    for family in ["claude-", "gpt-", "kimi-"] {
        if let Some(model) = available
            .iter()
            .filter(|model| model.model_id.starts_with(family))
            .min_by(|a, b| {
                a.rate_multiplier
                    .unwrap_or(f64::MAX)
                    .total_cmp(&b.rate_multiplier.unwrap_or(f64::MAX))
                    .then_with(|| a.model_id.cmp(&b.model_id))
            })
        {
            selected.push(model.model_id.clone());
        }
    }
    ensure!(
        !selected.is_empty(),
        "Account catalog contains no supported models"
    );
    Ok(selected)
}

fn app(account: &LiveAccount, buffered: bool) -> Router {
    let endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::from([
        (
            "cli".into(),
            Arc::new(CliEndpoint::new()) as Arc<dyn KiroEndpoint>,
        ),
        (
            "ide".into(),
            Arc::new(IdeEndpoint::new()) as Arc<dyn KiroEndpoint>,
        ),
    ]);
    let config = account.manager.config();
    let mut proxy = config.proxy_url.as_ref().map(ProxyConfig::new);
    if let (Some(proxy), Some(user), Some(password)) =
        (&mut proxy, &config.proxy_username, &config.proxy_password)
    {
        *proxy = proxy.clone().with_auth(user, password);
    }
    let provider = KiroProvider::with_proxy(
        account.manager.clone(),
        proxy,
        endpoints,
        config.default_endpoint.clone(),
    );
    super::create_router_with_provider([LOCAL_KEY], Some(provider), true, buffered, false)
}

async fn message(app: Router, path: &str, payload: Value) -> anyhow::Result<Vec<u8>> {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("x-api-key", LOCAL_KEY)
        .header("content-type", "application/json")
        .body(Body::from(payload.to_string()))?;
    // Do not attach raw upstream errors, responses or account information to failures.
    tokio::time::timeout(Duration::from_secs(125), async {
        let response = app.oneshot(request).await?;
        if response.status() != StatusCode::OK {
            let status = response.status();
            let bytes = to_bytes(response.into_body(), BODY_LIMIT).await?;
            let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let message = body
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let hints: Vec<_> = [
                "INVALID_MODEL_ID",
                "ValidationException",
                "AccessDeniedException",
                "max_tokens",
                "reasoning.effort",
                "output_config.effort",
                "thinking",
                "not supported",
                "not authorized",
                "fresh model capabilities",
                "Input is too long",
                "CONTENT_LENGTH_EXCEEDS_THRESHOLD",
            ]
            .into_iter()
            .filter(|hint| message.contains(hint))
            .collect();
            anyhow::bail!("Messages returned HTTP {status}; diagnostic tags: {hints:?}");
        }
        let expected = if payload["stream"] == true {
            "text/event-stream"
        } else {
            "application/json"
        };
        ensure!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with(expected)),
            "Unexpected response content type"
        );
        to_bytes(response.into_body(), BODY_LIMIT)
            .await
            .map(|body| body.to_vec())
            .map_err(Into::into)
    })
    .await
    .map_err(|_| anyhow::anyhow!("Live message timeout"))?
}

fn usage(value: &Value) -> anyhow::Result<()> {
    for field in ["input_tokens", "output_tokens"] {
        ensure!(
            value[field].as_u64().is_some(),
            "Missing or invalid usage count"
        );
    }
    Ok(())
}

fn json_message(bytes: &[u8]) -> anyhow::Result<Value> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("Invalid message JSON"))?;
    ensure!(
        value["type"] == "message" && value["role"] == "assistant",
        "Invalid message envelope"
    );
    ensure!(
        value["content"]
            .as_array()
            .is_some_and(|blocks| !blocks.is_empty()),
        "Empty message content"
    );
    ensure!(
        matches!(value["stop_reason"].as_str(), Some("end_turn" | "tool_use")),
        "Unexpected or truncated completion"
    );
    usage(&value["usage"])?;
    Ok(value)
}

fn text_answer(value: &Value) -> anyhow::Result<()> {
    ensure!(
        value["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "text"
                && block["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("KIRO_LIVE_OK")))),
        "Missing smoke response marker"
    );
    ensure!(
        value["stop_reason"] == "end_turn",
        "Text response did not finish normally"
    );
    Ok(())
}

fn sse_message(bytes: &[u8]) -> anyhow::Result<()> {
    let text = std::str::from_utf8(bytes)
        .context("SSE is not UTF-8")?
        .replace("\r\n", "\n");
    let mut started = false;
    let mut stopped = false;
    let mut final_delta = false;
    let mut answer = String::new();
    let mut blocks = std::collections::HashSet::new();
    for frame in text.split("\n\n").filter(|frame| !frame.trim().is_empty()) {
        let event = frame.lines().find_map(|line| line.strip_prefix("event: "));
        let data = frame
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_str(&data).map_err(|_| anyhow::anyhow!("Invalid SSE JSON"))?;
        ensure!(event == value["type"].as_str(), "SSE type mismatch");
        ensure!(!stopped, "SSE content after message_stop");
        match event {
            Some("ping") => {}
            Some("message_start") => {
                ensure!(!started, "Duplicate SSE start");
                usage(&value["message"]["usage"])?;
                started = true;
            }
            Some("content_block_start") => {
                ensure!(
                    started
                        && !final_delta
                        && blocks.insert(value["index"].as_u64().context("Missing block index")?),
                    "Invalid SSE block start"
                );
            }
            Some("content_block_delta") => {
                ensure!(
                    !final_delta
                        && blocks
                            .contains(&value["index"].as_u64().context("Missing block index")?),
                    "SSE delta outside a block"
                );
                if value["delta"]["type"] == "text_delta" {
                    answer.push_str(
                        value["delta"]["text"]
                            .as_str()
                            .context("Invalid text delta")?,
                    );
                }
            }
            Some("content_block_stop") => {
                ensure!(
                    blocks.remove(&value["index"].as_u64().context("Missing block index")?),
                    "Unknown SSE block stop"
                );
            }
            Some("message_delta") => {
                ensure!(
                    started && blocks.is_empty() && !final_delta,
                    "Invalid final SSE delta"
                );
                ensure!(
                    value["delta"]["stop_reason"] == "end_turn",
                    "SSE completion truncated"
                );
                ensure!(
                    value["usage"]["output_tokens"].as_u64().is_some(),
                    "Missing SSE usage"
                );
                final_delta = true;
            }
            Some("message_stop") => {
                ensure!(
                    final_delta && blocks.is_empty(),
                    "SSE stopped before completion"
                );
                stopped = true;
            }
            _ => anyhow::bail!("Unexpected SSE event or upstream error"),
        }
    }
    ensure!(
        started && stopped && answer.contains("KIRO_LIVE_OK"),
        "Incomplete SSE response"
    );
    Ok(())
}

fn tool_call(value: &Value) -> anyhow::Result<(String, Value)> {
    ensure!(
        value["stop_reason"] == "tool_use",
        "Model did not request the smoke tool"
    );
    let blocks = value["content"]
        .as_array()
        .context("Missing tool content")?;
    let tools: Vec<_> = blocks
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .collect();
    ensure!(tools.len() == 1, "Expected one smoke tool call");
    let tool = tools[0];
    ensure!(
        tool["name"] == "kiro_live_echo" && tool["input"] == json!({"value":"KIRO_LIVE_OK"}),
        "Invalid smoke tool arguments"
    );
    let id = tool["id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .context("Missing tool ID")?;
    Ok((id.into(), value["content"].clone()))
}

#[tokio::test]
#[ignore = "Live Kiro calls consume credits; select credentials explicitly, see docs/LIVE_TESTING.md"]
async fn live_account_smoke() -> anyhow::Result<()> {
    let account = settings()?;
    let (scope, catalog) = account
        .manager
        .available_models_for(account.id)
        .await
        .map_err(|_| {
            anyhow::anyhow!("Live catalog failed; verify authentication, endpoint and region")
        })?;
    account.manager.update_model_catalog(
        account.id,
        &scope,
        &catalog,
        std::time::Instant::now(),
        false,
    );
    eprintln!(
        "Live catalog passed: endpoint={}, region={}, models={}",
        account.endpoint,
        account.region,
        catalog.models.len()
    );
    let limits = account
        .manager
        .get_usage_limits_for(account.id)
        .await
        .map_err(|_| anyhow::anyhow!("Live usage query failed"))?;
    ensure!(
        !limits.usage_breakdown_list.is_empty()
            && limits.usage_limit().is_finite()
            && limits.current_usage().is_finite(),
        "Invalid live usage response"
    );
    eprintln!("Live usage query passed");
    let requested = std::env::var("KIRO_LIVE_MODELS").ok();
    let models = choose_models(&catalog, requested.as_deref())?;
    for model in models {
        eprintln!("{model}: starting live matrix");
        let capability = catalog
            .models
            .iter()
            .find(|entry| entry.model_id == model)
            .unwrap();
        eprintln!(
            "{model}: native max_tokens={:?}, output bounds {:?}..{:?}, effort path {:?}, levels {:?}",
            capability.native_max_tokens,
            capability.min_output_tokens,
            capability.max_output_tokens,
            capability.effort_schema_path,
            capability.effort_levels
        );
        let max_tokens = capability.min_output_tokens.unwrap_or(1024).max(1024);
        ensure!(
            max_tokens <= 4096
                && capability
                    .max_output_tokens
                    .is_none_or(|max| max >= max_tokens),
            "Model output bounds exceed smoke test budget"
        );
        let mut base = json!({"model":model,"max_tokens":max_tokens,"messages":[{"role":"user","content":"Reply with exactly KIRO_LIVE_OK. Do not explain."}]});
        if super::converter::is_gpt(&model) {
            // Exercise the provider-specific control using a value confirmed by
            // the account schema, while keeping reasoning inexpensive.
            let effort = ["low", "none", "medium", "high", "xhigh", "max"]
                .into_iter()
                .find(|effort| capability.effort_levels.iter().any(|level| level == effort));
            ensure!(
                capability.effort_schema_path.as_deref() == Some("reasoning.effort")
                    && effort.is_some(),
                "GPT catalog did not confirm the supported reasoning schema"
            );
            base["reasoning"] = json!({"effort":effort.unwrap()});
        }
        let bytes = message(app(&account, true), "/v1/messages", base.clone())
            .await
            .context("JSON generation failed")?;
        text_answer(&json_message(&bytes)?)?;
        eprintln!("{model}: JSON passed");
        for (path, buffered) in [
            ("/v1/messages", false),
            ("/cc/v1/messages", false),
            ("/cc/v1/messages", true),
        ] {
            let mut payload = base.clone();
            payload["stream"] = true.into();
            sse_message(
                &message(app(&account, buffered), path, payload)
                    .await
                    .context("SSE generation failed")?,
            )?;
            eprintln!("{model}: {path} buffered={buffered} SSE passed");
        }
        let mut payload = base.clone();
        payload["messages"] = json!([{"role":"user","content":"Call kiro_live_echo exactly once with value KIRO_LIVE_OK. Do not answer before calling the tool."}]);
        payload["tools"] = json!([{"name":"kiro_live_echo","description":"Echo a fixed test marker. No filesystem, network or external side effects.","input_schema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}}]);
        let first = json_message(
            &message(app(&account, true), "/v1/messages", payload.clone())
                .await
                .context("Tool generation failed")?,
        )?;
        let (id, content) = tool_call(&first)?;
        payload["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":content}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"KIRO_LIVE_OK"},{"type":"text","text":"The tool is complete. Reply exactly KIRO_LIVE_OK without calling tools again."}]}),
        ]);
        text_answer(&json_message(
            &message(app(&account, true), "/cc/v1/messages", payload)
                .await
                .context("Tool continuation failed")?,
        )?)?;
        eprintln!("{model}: tool round trip passed");
    }
    let snapshot = account.manager.snapshot();
    ensure!(
        snapshot.concurrency.in_flight == 0 && snapshot.concurrency.waiting == 0,
        "Live test leaked request capacity"
    );
    ensure!(
        snapshot
            .entries
            .iter()
            .all(|entry| !entry.disabled && entry.failure_count == 0),
        "Live account failure detected"
    );
    Ok(())
}

#[test]
fn account_selection_never_silently_fails_over_or_enables_disabled_accounts() {
    let row = |id, disabled| KiroCredentials {
        id: Some(id),
        disabled,
        ..Default::default()
    };
    assert!(select_account(vec![], None).is_err());
    assert!(select_account(vec![row(1, false), row(2, false)], None).is_err());
    assert!(select_account(vec![row(1, true)], Some("1")).is_err());
    assert!(select_account(vec![row(1, false), row(1, false)], Some("1")).is_err());
    assert_eq!(
        select_account(vec![row(1, true), row(2, false)], None)
            .unwrap()
            .id,
        Some(2)
    );
    assert_eq!(
        select_account(vec![row(1, false), row(2, false)], Some("2"))
            .unwrap()
            .id,
        Some(2)
    );
}

#[test]
fn live_validators_reject_errors_truncation_and_invalid_tool_results() {
    let valid = json!({"type":"message","role":"assistant","content":[{"type":"text","text":"KIRO_LIVE_OK"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}});
    text_answer(&json_message(&serde_json::to_vec(&valid).unwrap()).unwrap()).unwrap();
    for invalid in [
        json!({"type":"error"}),
        {
            let mut value = valid.clone();
            value["stop_reason"] = "max_tokens".into();
            value
        },
        {
            let mut value = valid.clone();
            value["usage"]["output_tokens"] = (-1).into();
            value
        },
    ] {
        assert!(json_message(&serde_json::to_vec(&invalid).unwrap()).is_err());
    }
    let tool = json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"kiro_live_echo","input":{"value":"KIRO_LIVE_OK"}}]});
    assert_eq!(tool_call(&tool).unwrap().0, "t1");
    let mut wrong = tool.clone();
    wrong["content"][0]["input"] = json!({"value":"wrong"});
    assert!(tool_call(&wrong).is_err());
    let mut duplicate = tool.clone();
    duplicate["content"]
        .as_array_mut()
        .unwrap()
        .push(tool["content"][0].clone());
    assert!(tool_call(&duplicate).is_err());
}

#[test]
fn sse_validation_detects_error_events_missing_stops_and_broken_block_order() {
    let events = [
        json!({"type":"message_start","message":{"usage":{"input_tokens":0,"output_tokens":0}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"KIRO_LIVE_OK"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ];
    let encode = |events: &[Value]| {
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>()
    };
    let valid = encode(&events);
    sse_message(valid.as_bytes()).unwrap();
    sse_message(valid.replace('\n', "\r\n").as_bytes()).unwrap();
    let mut wrong_index = events.to_vec();
    wrong_index[2]["index"] = 1.into();
    let mut truncated = events.to_vec();
    truncated[4]["delta"]["stop_reason"] = "max_tokens".into();
    let mut missing_usage = events.to_vec();
    missing_usage[4]["usage"] = json!({});
    for invalid in [
        encode(&events[..5]),
        encode(&events[1..]),
        encode(&wrong_index),
        encode(&truncated),
        encode(&missing_usage),
        format!(
            "{}{}",
            encode(&events[..2]),
            encode(&[json!({"type":"error","error":{"type":"api_error"}})])
        ),
        format!("{valid}{}", encode(&[events[2].clone()])),
    ] {
        assert!(sse_message(invalid.as_bytes()).is_err());
    }
}

#[test]
fn model_selection_uses_exact_catalog_authorization_and_bounds_the_matrix() {
    use crate::kiro::models::AvailableModel;
    let entry = |id: &str, rate, supported| AvailableModel {
        model_id: id.into(),
        model_name: id.into(),
        provider: None,
        rate_multiplier: Some(rate),
        rate_unit: None,
        max_input_tokens: None,
        native_max_tokens: None,
        min_output_tokens: None,
        max_output_tokens: None,
        effort_levels: vec![],
        effort_schema_path: None,
        default_effort: None,
        thinking_types: vec![],
        default_thinking: None,
        thinking_displays: vec![],
        proxy_supported: supported,
    };
    let catalog = ModelCatalog {
        region: "us-east-1".into(),
        default_model_id: None,
        models: vec![
            entry("claude-opus-5.5", 5.0, true),
            entry("claude-haiku-5.5", 0.5, true),
            entry("gpt-5.6-sol", 1.0, true),
            entry("kimi-k3", 1.0, true),
            entry("gpt-future", 0.0, false),
        ],
    };
    assert_eq!(
        choose_models(&catalog, None).unwrap(),
        ["claude-haiku-5.5", "gpt-5.6-sol", "kimi-k3"]
    );
    assert_eq!(
        choose_models(&catalog, Some("kimi-k3, gpt-5.6-sol")).unwrap(),
        ["kimi-k3", "gpt-5.6-sol"]
    );
    for invalid in [
        "",
        "gpt-future",
        "gpt-5.6-sol-thinking",
        "kimi-k3,kimi-k3",
        "kimi-k3,",
        "a,b,c,d,e",
    ] {
        assert!(choose_models(&catalog, Some(invalid)).is_err());
    }
    let empty = ModelCatalog {
        models: vec![],
        ..catalog
    };
    assert!(choose_models(&empty, None).is_err());
}
