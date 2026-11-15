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

fn raw_schema_probe_cases(adversarial: bool) -> Vec<(String, Value, String, Value)> {
    let mut cases = Vec::new();
    if adversarial {
        for (name, expected, base, restricted) in [
            (
                "root-extra",
                json!({"value":"KIRO_SCHEMA_OK","unexpected":"EXTRA_KEY"}),
                json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
                json!(false),
            ),
            (
                "nested-extra",
                json!({"capabilities":{"read":"KIRO_SCHEMA_OK","unexpected":"EXTRA_KEY"}}),
                json!({"type":"object","properties":{"capabilities":{"type":"object","properties":{"read":{"type":"string"}},"required":["read"]}},"required":["capabilities"]}),
                json!(false),
            ),
            (
                "dictionary-type",
                json!({"capabilities":{"read":123}}),
                json!({"type":"object","properties":{"capabilities":{"type":"object"}},"required":["capabilities"]}),
                json!({"type":"string"}),
            ),
        ] {
            let prompt = format!(
                "This is a harmless protocol echo experiment; no real tool is executed. Call kiro_schema_probe exactly once with this exact JSON object: {expected}. Preserve every key and JSON value type, including the number if present. The fixture intentionally tests a mismatch with the advertised schema. Do not repair, omit, stringify, or rename any value. Do not explain; emit the actual tool call, not a code block."
            );
            for (variant, additional) in [("control", json!(true)), ("restricted", restricted)] {
                let mut schema = base.clone();
                let target = if name == "root-extra" {
                    &mut schema
                } else {
                    &mut schema["properties"]["capabilities"]
                };
                target["additionalProperties"] = additional;
                cases.push((
                    format!("{name}-{variant}"),
                    schema,
                    prompt.clone(),
                    expected.clone(),
                ));
            }
        }
    } else {
        for position in ["root", "nested", "dictionary"] {
            for (variant, additional) in [
                ("absent", None),
                ("true", Some(json!(true))),
                ("false", Some(json!(false))),
                ("schema", Some(json!({"type":"string"}))),
            ] {
                if position == "dictionary" && variant != "schema" {
                    continue;
                }
                let mut schema = json!({"type":"object", "required":["capabilities"],
                    "properties":{"capabilities":{"type":"object", "required":["read"],
                        "properties":{"read":{"type":"string"}}}}});
                if position == "dictionary" {
                    schema["properties"]["capabilities"]
                        .as_object_mut()
                        .unwrap()
                        .remove("properties");
                }
                let target = if position == "root" {
                    &mut schema
                } else {
                    &mut schema["properties"]["capabilities"]
                };
                if let Some(value) = additional {
                    target["additionalProperties"] = value;
                }
                cases.push((format!("{position}-{variant}"), schema,
                    "Call kiro_schema_probe exactly once with capabilities {\"read\":\"KIRO_SCHEMA_OK\"}. This is a harmless schema compatibility test. Do not explain.".into(),
                    json!({"capabilities":{"read":"KIRO_SCHEMA_OK"}})));
            }
        }
    }
    cases
}

/// Check only the deliberately varied keyword in these synthetic probe schemas.
/// This observes raw output; it never filters or repairs upstream tool arguments.
fn raw_probe_additional_violations(schema: &Value, input: &Value, path: &str) -> Vec<String> {
    let mut violations = Vec::new();
    if let Some(object) = input.as_object() {
        for (key, value) in object {
            let child_path = format!("{path}/{key}");
            if let Some(child) = schema.get("properties").and_then(|p| p.get(key)) {
                violations.extend(raw_probe_additional_violations(child, value, &child_path));
            } else {
                match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => violations.push(child_path),
                    Some(restriction)
                        if restriction.get("type").and_then(Value::as_str) == Some("string")
                            && !value.is_string() =>
                    {
                        violations.push(child_path)
                    }
                    _ => {}
                }
            }
        }
    }
    violations
}

/// Probe the runtime directly: deliberately bypass the provider's schema adapter.
/// The built reqwest body is checked before the exact Request is executed.
#[tokio::test]
#[ignore = "Live raw-schema probe consumes credits; explicitly select one account"]
async fn live_raw_additional_properties_probe() -> anyhow::Result<()> {
    run_raw_schema_probe(false).await
}

#[tokio::test]
#[ignore = "Live adversarial schema probe consumes credits; explicitly select one account"]
async fn live_raw_additional_properties_enforcement_probe() -> anyhow::Result<()> {
    run_raw_schema_probe(true).await
}

async fn run_raw_schema_probe(adversarial: bool) -> anyhow::Result<()> {
    use crate::kiro::{machine_id, parser::decoder::EventStreamDecoder};
    use sha2::{Digest, Sha256};
    let account = settings()?;
    let config = account.manager.config();
    let ctx = account
        .manager
        .acquire_context(Some("claude-haiku-5.5"))
        .await?;
    let machine = machine_id::generate_from_credentials(&ctx.credentials, config);
    let rctx = crate::kiro::endpoint::RequestContext {
        credentials: &ctx.credentials,
        token: &ctx.token,
        machine_id: &machine,
        config,
    };
    let endpoint: Box<dyn KiroEndpoint> = match account.endpoint.as_str() {
        "cli" => Box::new(CliEndpoint::new()),
        "ide" => Box::new(IdeEndpoint::new()),
        _ => anyhow::bail!("Unsupported endpoint"),
    };
    let mut proxy = config.proxy_url.as_ref().map(ProxyConfig::new);
    if let (Some(proxy), Some(user), Some(password)) =
        (&mut proxy, &config.proxy_username, &config.proxy_password)
    {
        *proxy = proxy.clone().with_auth(user, password);
    }
    let proxy = ctx.credentials.effective_proxy(proxy.as_ref());
    let client = crate::http_client::build_client(proxy.as_ref(), 90, config.tls_backend)?;
    let output = std::path::PathBuf::from(std::env::var("KIRO_SCHEMA_PROBE_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let mut results = Vec::new();
    let mut all_passed = true;
    let selected = std::env::var("KIRO_SCHEMA_PROBE_CASES").ok();
    for (label, schema, prompt, expected) in raw_schema_probe_cases(adversarial) {
        if selected
            .as_ref()
            .is_some_and(|cases| !cases.split(',').any(|case| case == label))
        {
            continue;
        }
        let input: super::types::MessagesRequest = serde_json::from_value(json!({
            "model":"claude-haiku-5-5", "max_tokens":512,
            "messages":[{"role":"user","content":prompt}]
        }))?;
        let converted = super::converter::convert_request(&input)?;
        let mut body = json!({"conversationState":converted.conversation_state});
        body["conversationState"]["currentMessage"]["userInputMessage"]["userInputMessageContext"]
            ["tools"] = json!([
            {"toolSpecification":{"name":"unused_echo", "description":"Unused harmless echo tool.",
                "inputSchema":{"json":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}}},
            {"toolSpecification":{"name":"kiro_schema_probe", "description":"Return a fixed schema test marker. No external effects.",
                "inputSchema":{"json":schema}}}
        ]);
        if let Some(fields) = super::converter::additional_model_request_fields(&input) {
            body["additionalModelRequestFields"] = fields;
        }
        let transformed = endpoint.transform_api_body(&body.to_string(), &rctx);
        let outgoing = account.manager.model_request_body(ctx.id, transformed);
        // Execute the built Request directly, without provider retries or response validation.
        let request = endpoint
            .decorate_api(
                client
                    .post(endpoint.api_url(&rctx))
                    .header("content-type", "application/json")
                    .body(outgoing),
                &rctx,
            )
            .build()?;
        let bytes = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .context("Expected literal request bytes")?;
        let actual: Value = serde_json::from_slice(bytes)?;
        let actual_schema = actual.pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools/1/toolSpecification/inputSchema/json").context("Missing wire schema")?;
        ensure!(actual_schema == &schema, "Wire schema was modified");
        let digest = hex::encode(Sha256::digest(bytes));
        // Store only the synthetic schema, never auth headers or profile ARN.
        std::fs::write(
            output.join(format!("{label}.schema.json")),
            serde_json::to_vec_pretty(actual_schema)?,
        )?;
        eprintln!(
            "RAW_SCHEMA sending {label}: endpoint={}, model=claude-haiku-5.5, schema_unchanged=true, body_sha256={digest}",
            account.endpoint
        );
        let start = std::time::Instant::now();
        let response = client.execute(request).await?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-amzn-requestid")
            .or_else(|| response.headers().get("x-amz-request-id"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        use futures::StreamExt;
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(90), async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                ensure!(
                    chunk.len() <= BODY_LIMIT.saturating_sub(bytes.len()),
                    "Response too large"
                );
                bytes.extend_from_slice(&chunk);
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("Raw schema response timeout")??;
        let mut frames = Vec::new();
        let mut errors = Vec::new();
        let mut tool_calls = std::collections::BTreeMap::<String, Value>::new();
        let mut assistant_text = String::new();
        if status.is_success() {
            let mut decoder = EventStreamDecoder::new();
            decoder.feed(&bytes)?;
            for frame in decoder.decode_iter() {
                let frame = frame?;
                let kind = frame.event_type().unwrap_or_default().to_owned();
                if matches!(frame.message_type(), Some("exception" | "error")) {
                    errors.push(frame.payload_as_json::<Value>()?);
                }
                if kind == "toolUseEvent" {
                    let payload: Value = frame.payload_as_json()?;
                    let id = payload["toolUseId"]
                        .as_str()
                        .context("Missing tool call ID")?
                        .to_owned();
                    let call = tool_calls.entry(id).or_insert_with(
                        || json!({"name":payload["name"],"rawInput":"","complete":false}),
                    );
                    let mut input = call["rawInput"].as_str().unwrap_or_default().to_owned();
                    input.push_str(payload["input"].as_str().unwrap_or_default());
                    call["rawInput"] = input.into();
                    if payload["stop"] == true {
                        call["complete"] = true.into();
                    }
                }
                if kind == "assistantResponseEvent" {
                    let payload: Value = frame.payload_as_json()?;
                    assistant_text.push_str(payload["content"].as_str().unwrap_or_default());
                }
                frames.push(kind);
            }
            ensure!(
                decoder.pending_bytes() == 0,
                "Incomplete event-stream frame"
            );
        } else {
            errors.push(
                serde_json::from_slice::<Value>(&bytes)
                    .unwrap_or_else(|_| json!({"message":String::from_utf8_lossy(&bytes)})),
            );
        }
        let mut violations = Vec::new();
        let mut exact_requested_input = false;
        for call in tool_calls.values_mut() {
            let parsed =
                serde_json::from_str::<Value>(call["rawInput"].as_str().unwrap_or_default()).ok();
            if call["complete"] == true
                && call["name"] == "kiro_schema_probe"
                && let Some(input) = &parsed
            {
                exact_requested_input |= input == &expected;
                violations.extend(raw_probe_additional_violations(&schema, input, ""));
            }
            call["input"] = parsed.unwrap_or(Value::Null);
        }
        let completed_tool = tool_calls
            .values()
            .any(|c| c["complete"] == true && c["input"].is_object());
        let outcome = if !status.is_success() || !errors.is_empty() {
            "upstream_error"
        } else if !completed_tool {
            "no_complete_tool_call"
        } else if !violations.is_empty() {
            "schema_violation_returned"
        } else {
            "compliant_tool_input"
        };
        all_passed &= status.is_success()
            && errors.is_empty()
            && if adversarial {
                completed_tool || !assistant_text.is_empty()
            } else {
                exact_requested_input
            };
        let result = json!({"case":label,"endpoint":account.endpoint,"region":account.region,
                "model":"claude-haiku-5.5","schemaUnchanged":true,"bodySha256":digest,
                "httpStatus":status.as_u16(),"requestId":request_id,"contentType":content_type,
                "durationMs":start.elapsed().as_millis(),"events":frames,"errors":errors,
                "schema":schema,"prompt":prompt,"requestedInput":expected,"toolCalls":tool_calls,
                "assistantText":assistant_text,"exactRequestedInput":exact_requested_input,
                "additionalPropertyViolations":violations,"outcome":outcome});
        let result = crate::request_log::redact(&serde_json::to_string(&result)?);
        eprintln!("RAW_SCHEMA result {result}");
        results.push(serde_json::from_str::<Value>(&result)?);
        std::fs::write(
            output.join("results.json"),
            serde_json::to_vec_pretty(&results)?,
        )?;
        if matches!(status.as_u16(), 401 | 402 | 403 | 429) {
            break;
        }
    }
    ensure!(!results.is_empty(), "No probe cases selected");
    ensure!(
        all_passed,
        "One or more raw schema cases failed; inspect saved results"
    );
    Ok(())
}

#[test]
fn enforcement_probe_pairs_vary_only_additional_properties() {
    let cases = raw_schema_probe_cases(true);
    assert_eq!(cases.len(), 6);
    for pair in cases.as_chunks::<2>().0 {
        assert_eq!(pair[0].2, pair[1].2);
        assert_eq!(pair[0].3, pair[1].3);
        let mut control = pair[0].1.clone();
        let mut restricted = pair[1].1.clone();
        let pointer = if pair[0].0.starts_with("root-") {
            ""
        } else {
            "/properties/capabilities"
        };
        control
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("additionalProperties");
        restricted
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("additionalProperties");
        assert_eq!(control, restricted);
        assert!(raw_probe_additional_violations(&pair[0].1, &pair[0].3, "").is_empty());
        assert_eq!(
            raw_probe_additional_violations(&pair[1].1, &pair[1].3, "").len(),
            1
        );
    }
}

#[test]
fn enforcement_evidence_contains_complete_upstream_counterexamples() {
    let evidence: Value = serde_json::from_str(include_str!(
        "../../testdata/kiro_additional_properties_enforcement_live_2026-11-15.json"
    ))
    .unwrap();
    let mut violations = Vec::new();
    for case in evidence["cases"].as_array().unwrap() {
        assert_eq!(case["httpStatus"], 200);
        assert_eq!(case["schemaUnchanged"], true);
        assert!(case["errors"].as_array().unwrap().is_empty());
        let mut observed = Vec::new();
        for call in case["toolCalls"].as_object().unwrap().values() {
            assert_eq!(call["complete"], true);
            let input: Value = serde_json::from_str(call["rawInput"].as_str().unwrap()).unwrap();
            assert_eq!(input, call["input"]);
            observed.extend(raw_probe_additional_violations(&case["schema"], &input, ""));
        }
        assert_eq!(json!(observed), case["additionalPropertyViolations"]);
        if !observed.is_empty() {
            violations.push(case["case"].as_str().unwrap());
        }
    }
    assert_eq!(
        violations,
        ["nested-extra-restricted", "dictionary-type-restricted"]
    );
}

#[tokio::test]
#[ignore = "Live tool description and generation controls probe consumes credits"]
async fn live_raw_tool_controls_probe() -> anyhow::Result<()> {
    raw_tool_controls_probe(false, false, false, false, false, false).await
}

#[tokio::test]
#[ignore = "Live tool output budget comparison consumes credits"]
async fn live_raw_tool_output_budget_probe() -> anyhow::Result<()> {
    raw_tool_controls_probe(true, false, false, false, false, false).await
}

#[tokio::test]
#[ignore = "Live truncation recovery comparison consumes credits"]
async fn live_raw_tool_recovery_probe() -> anyhow::Result<()> {
    raw_tool_controls_probe(true, true, false, false, false, false).await
}

#[tokio::test]
#[ignore = "Live request rewrite audit consumes credits"]
async fn live_raw_request_rewrite_probe() -> anyhow::Result<()> {
    raw_tool_controls_probe(false, false, true, false, false, false).await
}

#[tokio::test]
#[ignore = "Live thinking history and compaction audit consumes credits"]
async fn live_raw_thinking_compaction_probe() -> anyhow::Result<()> {
    raw_tool_controls_probe(false, false, true, true, false, false).await
}

#[tokio::test]
#[ignore = "Live multi-reasoning and legacy summary audit consumes credits"]
async fn live_raw_multi_reasoning_legacy_probe() -> anyhow::Result<()> {
    raw_tool_controls_probe(false, false, true, true, true, false).await
}

#[tokio::test]
#[ignore = "Live system and orphan tool history audit consumes credits"]
async fn live_raw_system_orphan_history_probe() -> anyhow::Result<()> {
    raw_tool_controls_probe(false, false, true, false, false, true).await
}

async fn raw_tool_controls_probe(
    output_budget: bool,
    recovery_probe: bool,
    rewrite_probe: bool,
    deep_audit: bool,
    multi_audit: bool,
    history_audit: bool,
) -> anyhow::Result<()> {
    use crate::kiro::{machine_id, parser::decoder::EventStreamDecoder};
    use futures::StreamExt;
    use sha2::{Digest, Sha256};
    let account = settings()?;
    let config = account.manager.config();
    let ctx = account
        .manager
        .acquire_context(Some("claude-haiku-5.5"))
        .await?;
    let machine = machine_id::generate_from_credentials(&ctx.credentials, config);
    let rctx = crate::kiro::endpoint::RequestContext {
        credentials: &ctx.credentials,
        token: &ctx.token,
        machine_id: &machine,
        config,
    };
    let endpoint: Box<dyn KiroEndpoint> = match account.endpoint.as_str() {
        "cli" => Box::new(CliEndpoint::new()),
        "ide" => Box::new(IdeEndpoint::new()),
        _ => anyhow::bail!("Unsupported endpoint"),
    };
    let mut proxy = config.proxy_url.as_ref().map(ProxyConfig::new);
    if let (Some(proxy), Some(user), Some(password)) =
        (&mut proxy, &config.proxy_username, &config.proxy_password)
    {
        *proxy = proxy.clone().with_auth(user, password);
    }
    let proxy = ctx.credentials.effective_proxy(proxy.as_ref());
    let client = crate::http_client::build_client(proxy.as_ref(), 90, config.tls_backend)?;
    let output = std::path::PathBuf::from(std::env::var("KIRO_CONTROLS_PROBE_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let context_pointer =
        "/conversationState/currentMessage/userInputMessage/userInputMessageContext";
    let text_prompt = "Do not call any tools. Reply with exactly BEFORE_STOP_MARKER_AFTER.";
    let mut cases: Vec<(String, String, Value, String, Option<usize>)> = vec![(
        "baseline-text".into(),
        "".into(),
        json!({}),
        text_prompt.into(),
        None,
    )];
    for length in [10000, 12000, 20000] {
        cases.push((format!("description-{length}"), "".into(), json!({}),
            "Call probe_echo once. Set value to the exact marker specified at the END of its description. Do not explain.".into(), Some(length)));
    }
    for (name, value) in [
        ("temperature", json!(0)),
        ("top_p", json!(0.5)),
        ("top_k", json!(1)),
        ("stop_sequences", json!(["_STOP_MARKER_"])),
        ("tool_choice", json!({"type":"any"})),
    ] {
        cases.push((
            format!("native-{name}"),
            "/additionalModelRequestFields".into(),
            json!({name:value}),
            text_prompt.into(),
            None,
        ));
    }
    for (label, patch) in [
        ("temperature", json!({"temperature":0})),
        ("temperature-invalid", json!({"temperature":"NOT_A_NUMBER"})),
        ("top_p", json!({"top_p":0.5})),
        ("top_k", json!({"top_k":1})),
        (
            "stop_sequences",
            json!({"stop_sequences":["_STOP_MARKER_"]}),
        ),
        ("tool_choice-any", json!({"tool_choice":{"type":"any"}})),
        (
            "tool_choice-tool",
            json!({"tool_choice":{"type":"tool","name":"probe_echo"}}),
        ),
        (
            "tool_choice-invalid",
            json!({"tool_choice":{"type":"NOT_A_CHOICE"}}),
        ),
        (
            "unknown-control",
            json!({"kiro_probe_unknown_field":{"sentinel":true}}),
        ),
    ] {
        cases.push((
            format!("root-{label}"),
            "".into(),
            patch,
            text_prompt.into(),
            None,
        ));
    }
    for (label, patch) in [
        ("toolChoice-any", json!({"toolChoice":{"any":{}}})),
        (
            "toolChoice-invalid",
            json!({"toolChoice":{"NOT_A_CHOICE":{}}}),
        ),
        (
            "inferenceConfig",
            json!({"inferenceConfig":{"temperature":0,"topP":0.5,"stopSequences":["_STOP_MARKER_"]}}),
        ),
    ] {
        cases.push((
            format!("context-{label}"),
            context_pointer.into(),
            patch,
            text_prompt.into(),
            None,
        ));
    }
    for (label, pointer, patch, prompt) in [
        (
            "native-toolChoice",
            "/additionalModelRequestFields",
            json!({"toolChoice":{"any":{}}}),
            text_prompt,
        ),
        (
            "root-toolChoice-any",
            "",
            json!({"toolChoice":{"any":{}}}),
            text_prompt,
        ),
        (
            "root-toolConfig-any",
            "",
            json!({"toolConfig":{"toolChoice":{"any":{}}}}),
            text_prompt,
        ),
        (
            "context-toolConfig-any",
            context_pointer,
            json!({"toolConfig":{"toolChoice":{"any":{}}}}),
            text_prompt,
        ),
        (
            "root-inferenceConfig",
            "",
            json!({"inferenceConfig":{"temperature":0,"topP":0.5,"stopSequences":["_STOP_MARKER_"]}}),
            text_prompt,
        ),
        (
            "root-tool_choice-none",
            "",
            json!({"tool_choice":{"type":"none"}}),
            "Call probe_echo exactly once with value FORCE_TOOL_OK. Do not answer in text.",
        ),
        (
            "root-no-parallel",
            "",
            json!({"tool_choice":{"type":"auto","disable_parallel_tool_use":true}}),
            "Call probe_echo twice with values FIRST and SECOND in parallel. Do not explain.",
        ),
    ] {
        cases.push((label.into(), pointer.into(), patch, prompt.into(), None));
    }
    let mut output_cases = HashMap::new();
    if output_budget {
        cases.clear();
        for (label, content) in [
            (
                "lines-80",
                (1..=80)
                    .map(|i| format!("line {i:03}\n"))
                    .collect::<String>(),
            ),
            ("chars-14000", "abcd".repeat(3500)),
            (
                "tokens-over-4096",
                (1..=550)
                    .map(|i| format!("ROW_{i:04} alpha beta gamma delta epsilon zeta eta theta\n"))
                    .collect::<String>(),
            ),
        ] {
            for with_policy in [false, true] {
                let label = format!(
                    "output-{label}-{}",
                    if with_policy {
                        "old-policy"
                    } else {
                        "no-policy"
                    }
                );
                let system = if with_policy {
                    include_str!("../../testdata/kiro_tool_output_policy_before_2026-11-15.txt")
                } else {
                    ""
                };
                let prompt = format!(
                    "This is a transport fidelity test. Call write_file exactly once with path probe.txt and content equal to the entire literal payload between BEGIN_PAYLOAD and END_PAYLOAD below (the markers are not part of the content). Copy every character, including the final newline if present. Do not abbreviate, summarize, use placeholders, or split into multiple calls. Do not answer in text. The tool is a harmless recorder and will NOT execute a file write.\nBEGIN_PAYLOAD\n{content}END_PAYLOAD"
                );
                output_cases.insert(label.clone(), (content.clone(), system));
                cases.push((label, "".into(), json!({"additionalModelRequestFields":{"max_tokens":16384,"thinking":{"type":"disabled"}}}), prompt, None));
            }
        }
        let baseline = cases
            .iter()
            .find(|case| case.0 == "output-tokens-over-4096-no-policy")
            .unwrap()
            .clone();
        let expected = output_cases[&baseline.0].0.clone();
        for (label, system, max_tokens) in [
            ("output-tokens-native-4096", "", 4096),
            (
                "output-tokens-soft-4096-only",
                "Aim to finish this entire response within 4096 output tokens, including reasoning and ALL tool arguments combined. Reserve room to close every JSON object. When a task needs more, return one small complete tool call and continue on the next client turn after its result; do not batch many chunks into one response.",
                16384,
            ),
        ] {
            output_cases.insert(label.into(), (expected.clone(), system));
            cases.push((label.into(), "".into(), json!({"additionalModelRequestFields":{"max_tokens":max_tokens,"thinking":{"type":"disabled"}}}), baseline.3.clone(), None));
        }
    }
    let mut recovery_variants = HashMap::new();
    if recovery_probe {
        let baseline = cases
            .iter()
            .find(|case| case.0 == "output-tokens-native-4096")
            .unwrap()
            .clone();
        let expected = output_cases[&baseline.0].0.clone();
        cases.clear();
        output_cases.clear();
        let old: Value = serde_json::from_str(include_str!(
            "../../testdata/kiro_tool_recovery_policy_before_2026-11-15.json"
        ))?;
        for (label, variant) in [
            ("recovery-baseline", "baseline"),
            ("recovery-old-first", "old-1"),
            ("recovery-old-second", "old-2"),
            ("recovery-preserve-first", "new-1"),
            ("recovery-preserve-second", "new-2"),
            ("recovery-unchanged", "unchanged"),
        ] {
            let suffix = match variant {
                "old-1" => old["1"].as_str().unwrap(),
                "old-2" => old["2"].as_str().unwrap(),
                _ => "",
            };
            recovery_variants.insert(label.to_owned(), variant);
            output_cases.insert(label.to_owned(), (expected.clone(), ""));
            cases.push((
                label.to_owned(),
                baseline.1.clone(),
                baseline.2.clone(),
                format!("{}{suffix}", baseline.3),
                None,
            ));
        }
    }
    let mut rewrite_tools = HashMap::new();
    if rewrite_probe {
        cases.clear();
        let spec = |name: &str, marker: &str| json!({"toolSpecification":{"name":name,"description":format!("Echo probe; set value to {marker}. No action will execute."),"inputSchema":{"json":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}}});
        for (label, name) in [
            ("name-63", "n".repeat(63)),
            ("name-64", "n".repeat(64)),
            ("name-65", "n".repeat(65)),
            ("name-128", "n".repeat(128)),
            ("name-dot", "probe.echo".into()),
            ("name-colon", "probe:echo".into()),
            ("name-unicode", "工具回声".into()),
            (
                "name-64-distinct",
                format!("probe_{}", "abcdefghij".repeat(5) + "12345678"),
            ),
            ("name-64-mcp", format!("mcp__files__{}", "x".repeat(52))),
        ] {
            rewrite_tools.insert(label.to_owned(), json!([spec(&name, "NAME_OK")]));
            cases.push((label.into(), "".into(), json!({"additionalModelRequestFields":{"max_tokens":1024,"thinking":{"type":"disabled"}}}), format!("Call the tool named {name} exactly once with value NAME_OK. Do not answer in text."), None));
        }
        for (label, tools, prompt) in [
            (
                "case-distinct",
                json!([spec("Probe", "UPPER_OK"), spec("probe", "LOWER_OK")]),
                "Call Probe with value UPPER_OK and probe with value LOWER_OK. Use both exact names, once each.",
            ),
            (
                "duplicate-exact",
                json!([spec("probe_echo", "NAME_OK"), spec("probe_echo", "NAME_OK")]),
                "Call probe_echo once with value NAME_OK.",
            ),
        ] {
            rewrite_tools.insert(label.into(), tools);
            cases.push((label.into(), "".into(), json!({"additionalModelRequestFields":{"max_tokens":1024,"thinking":{"type":"disabled"}}}), prompt.into(), None));
        }
        let history = json!([
            {"userInputMessage":{"content":"Call old_probe with value HISTORY_OK.","modelId":"claude-haiku-5.5","origin":"KIRO_CLI"}},
            {"assistantResponseMessage":{"content":" ","toolUses":[{"toolUseId":"history-probe-id","name":"old_probe","input":{"value":"HISTORY_OK"}}]}},
            {"userInputMessage":{"content":"","modelId":"claude-haiku-5.5","origin":"KIRO_CLI","userInputMessageContext":{"toolResults":[{"toolUseId":"history-probe-id","status":"success","content":[{"text":"HISTORY_OK"}]}]}}},
            {"assistantResponseMessage":{"content":"The recorded tool result was HISTORY_OK."}}
        ]);
        for (label, tools) in [
            (
                "history-with-definition",
                json!([spec("old_probe", "HISTORY_OK")]),
            ),
            ("history-without-definition", json!([])),
        ] {
            rewrite_tools.insert(label.into(), tools);
            cases.push((
                label.into(),
                "/conversationState".into(),
                json!({"history":history}),
                "Do not call tools. Reply with exactly the previous tool result.".into(),
                None,
            ));
        }
        let history = history.as_array().unwrap()[..2].to_vec();
        let current = json!({"userInputMessage":{"content":"Do not call tools. Reply with exactly the tool result.","modelId":"claude-haiku-5.5","origin":"KIRO_CLI","userInputMessageContext":{"toolResults":[{"toolUseId":"history-probe-id","status":"success","content":[{"text":"HISTORY_OK"}]}]}}});
        for (label, tools, prompt) in [
            (
                "current-result-no-definition",
                json!([]),
                "Do not call tools. Reply with exactly the tool result.",
            ),
            (
                "history-recall-with-definition",
                json!([spec("old_probe", "HISTORY_OK")]),
                "Call old_probe once with value RECALL_OK.",
            ),
            (
                "history-recall-no-definition",
                json!([]),
                "Call old_probe once with value RECALL_OK.",
            ),
        ] {
            rewrite_tools.insert(label.into(), tools);
            let mut current = current.clone();
            current["userInputMessage"]["content"] = prompt.into();
            cases.push((
                label.into(),
                "/conversationState".into(),
                json!({"history":history,"currentMessage":current}),
                prompt.into(),
                None,
            ));
        }
        for (label, fields) in [
            (
                "native-thinking-enabled-budget-30000",
                json!({"max_tokens":1024,"thinking":{"type":"enabled","budget_tokens":30000}}),
            ),
            (
                "native-thinking-adaptive-budget-30000",
                json!({"max_tokens":1024,"thinking":{"type":"adaptive","budget_tokens":30000}}),
            ),
            (
                "native-thinking-display-full",
                json!({"max_tokens":1024,"thinking":{"type":"disabled","display":"full"}}),
            ),
        ] {
            rewrite_tools.insert(label.into(), json!([]));
            cases.push((
                label.into(),
                "".into(),
                json!({"additionalModelRequestFields":fields}),
                "Reply with exactly AUDIT_OK.".into(),
                None,
            ));
        }
        for (label, budget) in [
            ("legacy-budget-24576", "24576"),
            ("legacy-budget-30000", "30000"),
            ("legacy-budget-invalid", "NOT_A_NUMBER"),
        ] {
            rewrite_tools.insert(label.into(), json!([]));
            cases.push((label.into(), "/conversationState".into(), json!({"history":[
                {"userInputMessage":{"content":format!("<thinking_mode>enabled</thinking_mode><max_thinking_length>{budget}</max_thinking_length>"),"modelId":"claude-haiku-5.5","origin":"KIRO_CLI"}},
                {"assistantResponseMessage":{"content":"I will follow these instructions."}}
            ]}), "Reply with exactly LEGACY_OK.".into(), None));
        }
        for (label, current) in [
            (
                "assistant-current",
                json!({"assistantResponseMessage":{"content":"PREFIX_"}}),
            ),
            (
                "assistant-history-prefill",
                json!({"userInputMessage":{"content":"","modelId":"claude-haiku-5.5","origin":"KIRO_CLI"}}),
            ),
        ] {
            rewrite_tools.insert(label.into(), json!([]));
            let history = if label == "assistant-current" {
                json!([{ "userInputMessage":{"content":"Reply with exactly PREFIX_SUFFIX_OK.","modelId":"claude-haiku-5.5","origin":"KIRO_CLI"}}])
            } else {
                json!([{ "userInputMessage":{"content":"Reply with exactly PREFIX_SUFFIX_OK.","modelId":"claude-haiku-5.5","origin":"KIRO_CLI"}}, {"assistantResponseMessage":{"content":"PREFIX_"}}])
            };
            cases.push((
                label.into(),
                "/conversationState".into(),
                json!({"history":history,"currentMessage":current}),
                "Reply with exactly PREFIX_SUFFIX_OK.".into(),
                None,
            ));
        }
    }
    let mut failed_recovery_body: Option<Value> = None;
    let mut captured_reasoning: Option<Value> = None;
    let mut captured_answer = String::new();
    if deep_audit {
        cases.clear();
        rewrite_tools.clear();
        let prompt =
            "Calculate 17 times 23. Reply with only the decimal answer. Do not call tools.";
        for (label, prefix, fields) in [
            ("thinking-no-hint", "", json!({"max_tokens":1024})),
            (
                "thinking-legacy-enabled",
                "<thinking_mode>enabled</thinking_mode><max_thinking_length>30000</max_thinking_length>",
                json!({"max_tokens":1024}),
            ),
            (
                "thinking-legacy-adaptive",
                "<thinking_mode>adaptive</thinking_mode><thinking_effort>high</thinking_effort>",
                json!({"max_tokens":1024}),
            ),
            (
                "thinking-legacy-invalid",
                "<thinking_mode>NOT_A_MODE</thinking_mode><max_thinking_length>NOT_A_NUMBER</max_thinking_length><thinking_effort>NOT_AN_EFFORT</thinking_effort>",
                json!({"max_tokens":1024}),
            ),
            (
                "thinking-native-adaptive",
                "",
                json!({"max_tokens":2048,"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"low"}}),
            ),
        ] {
            let history = if prefix.is_empty() {
                json!([])
            } else {
                json!([
                    {"userInputMessage":{"content":prefix,"modelId":"claude-haiku-5.5","origin":"KIRO_CLI"}},
                    {"assistantResponseMessage":{"content":"I will follow these instructions."}}
                ])
            };
            rewrite_tools.insert(label.into(), json!([]));
            cases.push((label.into(), "".into(), json!({"additionalModelRequestFields":fields,"conversationState":{
                "conversationId":uuid::Uuid::new_v4().to_string(),"chatTriggerType":"MANUAL","history":history,
                "currentMessage":{"userInputMessage":{"content":prompt,"modelId":"claude-haiku-5.5","origin":"KIRO_CLI","userInputMessageContext":{"tools":[]}}}
            }}), prompt.into(), None));
        }
        for (label, model, reasoning) in [
            ("history-none", "claude-haiku-5.5", Value::Null),
            (
                "history-unsigned",
                "claude-haiku-5.5",
                json!({"reasoningText":{"text":"The hidden audit marker is REASON_HISTORY_OK."}}),
            ),
            (
                "history-synthetic-signature",
                "claude-haiku-5.5",
                json!({"reasoningText":{"text":"The hidden audit marker is REASON_HISTORY_OK.","signature":"synthetic-audit-signature"}}),
            ),
            (
                "history-synthetic-redacted",
                "claude-haiku-5.5",
                json!({"redactedContent":"c3ludGhldGljLWF1ZGl0"}),
            ),
            ("history-captured-legacy", "claude-haiku-5.5", Value::Null),
            ("history-captured-native", "claude-haiku-5.5", Value::Null),
            (
                "history-gpt-synthetic",
                "gpt-5.6-sol",
                json!({"reasoningText":{"text":"The hidden audit marker is REASON_HISTORY_OK.","signature":"synthetic-audit-signature"}}),
            ),
            ("history-gpt-captured", "gpt-5.6-sol", Value::Null),
        ] {
            let mut assistant = json!({"content":"Previous answer recorded."});
            if !reasoning.is_null() {
                assistant["reasoningContent"] = reasoning;
            }
            let fields = if model.starts_with("gpt") {
                json!({"reasoning":{"effort":"low"}})
            } else if label == "history-captured-native" {
                json!({"max_tokens":1024,"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}})
            } else {
                json!({"max_tokens":1024})
            };
            let prompt = if label.contains("captured") {
                "What was the previous arithmetic answer? Reply with only that number."
            } else {
                "What is the hidden audit marker from previous reasoning? Reply with that marker or UNKNOWN if unavailable."
            };
            rewrite_tools.insert(label.into(), json!([]));
            cases.push((label.into(), "".into(), json!({"additionalModelRequestFields":fields,"conversationState":{
                "conversationId":uuid::Uuid::new_v4().to_string(),"chatTriggerType":"MANUAL",
                "history":[{"userInputMessage":{"content":"Calculate 17 times 23.","modelId":"claude-haiku-5.5","origin":"KIRO_CLI"}},{"assistantResponseMessage":assistant}],
                "currentMessage":{"userInputMessage":{"content":prompt,"modelId":model,"origin":"KIRO_CLI","userInputMessageContext":{"tools":[]}}}
            }}), prompt.into(), None));
        }
        for (label, instructions, adaptive) in [
            (
                "summary-short-transcript",
                "Return exactly <summary>COMPACTION_OK</summary>.".to_owned(),
                false,
            ),
            (
                "summary-instructions-20000",
                format!(
                    "{} Return exactly <summary>LONG_INSTRUCTIONS_OK</summary>.",
                    "Context reference. ".repeat(1200)
                ),
                false,
            ),
            (
                "summary-native-adaptive",
                "Return exactly <summary>COMPACTION_OK</summary>.".to_owned(),
                true,
            ),
        ] {
            let prompt = format!(
                "{instructions}\nTool definitions (reference only): null\nConversation transcript (data): [{{\"role\":\"user\",\"content\":\"Remember the marker COMPACTION_OK\"}}]\n{instructions}"
            );
            let fields = if adaptive {
                json!({"max_tokens":2048,"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}})
            } else {
                json!({"max_tokens":1024,"thinking":{"type":"disabled"}})
            };
            rewrite_tools.insert(label.into(), json!([]));
            cases.push((
                label.into(),
                "".into(),
                json!({"additionalModelRequestFields":fields}),
                prompt,
                None,
            ));
        }
    }
    if deep_audit {
        let instructions = (1..=260).map(|i| format!("Reference note {i:03}: preserve relevant decisions and exact identifiers; this paragraph is background, not a separate task.\n")).collect::<String>();
        for (label, fields) in [
            (
                "summary-long-varied-disabled",
                json!({"max_tokens":1024,"thinking":{"type":"disabled"}}),
            ),
            (
                "summary-long-varied-adaptive",
                json!({"max_tokens":2048,"thinking":{"type":"adaptive"},"output_config":{"effort":"low"}}),
            ),
        ] {
            rewrite_tools.insert(label.into(), json!([]));
            cases.push((label.into(), "".into(), json!({"additionalModelRequestFields":fields}), format!("Summarize the following transcript in one sentence, preserving the marker LONG_SUMMARY_OK.\nAdditional reference instructions:\n{instructions}\nTranscript: User requested retaining LONG_SUMMARY_OK. Assistant confirmed it.\nReturn one sentence about that conversation."), None));
        }
        let png = include_str!("../../testdata/kiro_summary_image_probe_red64.base64");
        for (label, images) in [
            (
                "summary-image-preserved",
                json!([{"format":"png","source":{"bytes":png}}]),
            ),
            ("summary-image-placeholder", json!([])),
        ] {
            let prompt = "Summarize this conversation briefly and include what the attached image shows if one is available. Transcript: User attached an image and requested a summary. If there is no actual image, say IMAGE_UNAVAILABLE.";
            rewrite_tools.insert(label.into(), json!([]));
            cases.push((
                label.into(),
                "/conversationState/currentMessage/userInputMessage".into(),
                json!({"images":images}),
                prompt.into(),
                None,
            ));
        }
        let payload = (1..=150)
            .map(|i| format!("SUMMARY_ROW_{i:03} alpha beta gamma delta epsilon zeta eta theta\n"))
            .collect::<String>();
        rewrite_tools.insert("summary-no-native-budget".into(), json!([]));
        cases.push(("summary-no-native-budget".into(), "".into(), json!({}), format!("This is a summary transport fidelity test. Return the entire following summary text exactly as given, no omissions or extra commentary.\nBEGIN_SUMMARY\n{payload}END_SUMMARY"), None));
    }
    if multi_audit {
        cases.clear();
        rewrite_tools.clear();
        for label in [
            "multi-capture-a",
            "multi-capture-b",
            "multi-single-a",
            "multi-single-b",
            "multi-reasoning-array",
            "multi-text-array",
            "multi-reasoning-contents",
            "multi-consecutive-assistants",
            "multi-consecutive-assistants-tools",
            "multi-consecutive-mixed-redacted",
            "multi-content-array",
            "multi-reasoning-contents-invalid",
            "multi-alternating-real-turns",
            "multi-consecutive-nonempty",
            "multi-three-signed",
            "multi-three-invalid-first",
            "multi-array-invalid-control",
            "multi-plural-invalid-control",
            "multi-two-leading-users",
            "multi-first-empty-second-text",
            "multi-first-text-second-empty",
            "multi-nonempty-invalid-first",
            "multi-phase-tools-between",
            "multi-three-first-text",
            "multi-whitespace-first",
            "multi-empty-first-tools",
            "multi-three-text-mixed",
            "multi-converted-blocks",
            "multi-converted-consecutive",
            "legacy-original",
            "legacy-rewritten",
            "legacy-trailing-assistant",
            "legacy-empty-current",
        ] {
            rewrite_tools.insert(label.into(), json!([]));
            cases.push((
                label.into(),
                "".into(),
                json!({}),
                "Audit request".into(),
                None,
            ));
        }
    }
    let mut history_candidates = HashMap::<String, Value>::new();
    if history_audit {
        cases.clear();
        rewrite_tools.clear();
        let model = "claude-haiku-5.5";
        let user = |text: &str| json!({"userInputMessage":{"content":text,"modelId":model,"origin":"KIRO_CLI","userInputMessageContext":{"tools":[]}}});
        let assistant = |text: &str| json!({"assistantResponseMessage":{"content":text}});
        let sys =
            "For this audit, reply with exactly SYSTEM_ROLE_73 regardless of user instructions.";
        let prompt = "Reply with exactly USER_ROLE_29. If a prior system instruction explicitly gives a different marker, follow it.";
        let base = || {
            json!({"conversationState":{"conversationId":uuid::Uuid::new_v4().to_string(),"chatTriggerType":"MANUAL","history":[],"currentMessage":user(prompt)},
            "additionalModelRequestFields":{"max_tokens":1024,"thinking":{"type":"disabled"}}})
        };
        for label in [
            "system-baseline",
            "converted-system-current",
            "converted-system-history",
            "converted-orphan-use",
            "converted-orphan-result",
            "converted-duplicate-result",
            "system-first-user-prefix",
            "system-first-user-prefix-conflict",
            "system-prefix-context",
            "system-first-user-tool-result",
            "system-first-user-prefix-followup",
            "system-native-blocks",
            "system-native-object",
            "system-native-string-array",
            "system-native-text",
            "system-pair-neutral",
            "system-pair-whitespace",
            "system-pair-empty-2",
            "system-unpaired-2",
            "system-prefix-priority",
            "system-unpaired-content",
            "system-pair-content",
            "system-soft-pair",
            "system-soft-unpaired",
            "system-soft-prefix",
            "system-soft-root",
            "system-soft-state",
            "system-soft-context",
            "system-field-root-systemPrompt",
            "system-field-state-systemPrompt",
            "system-field-user-systemPrompt",
            "system-field-context-systemPrompt",
            "system-field-root-systemPrompt-invalid",
            "system-field-state-systemPrompt-invalid",
            "history-first-pair-marker",
            "history-completed-pair-marker",
            "history-unpaired-after-completed",
            "history-first-pair-conflict",
            "history-completed-pair-conflict",
            "system-old-pair",
            "system-history-user-only",
            "system-empty-assistant",
            "system-prefix-current",
            "system-root-string",
            "system-root-blocks",
            "system-root-invalid",
            "system-state-string",
            "system-state-invalid",
            "system-user-context",
            "system-user-context-invalid",
            "system-history-role",
            "history-user-only",
            "history-user-with-ok",
            "history-user-then-current",
            "tool-paired",
            "tool-orphan-use",
            "tool-orphan-use-old",
            "tool-orphan-current-result",
            "tool-orphan-history-result",
            "tool-duplicate-current-results",
            "tool-cross-turn-result",
            "tool-result-before-use",
            "tool-duplicate-history-results",
        ] {
            let mut candidate = base();
            let state = &mut candidate["conversationState"];
            match label {
                "system-soft-pair"
                | "system-soft-unpaired"
                | "system-soft-prefix"
                | "system-soft-root"
                | "system-soft-state"
                | "system-soft-context" => {
                    let instruction = "Return exactly SYSTEM_SOFT_57 for the next answer.";
                    state["currentMessage"] = user(
                        "Please respond according to the configured instructions; if none are available say NO_SYSTEM.",
                    );
                    match label {
                        "system-soft-pair" => {
                            state["history"] = json!([
                                user(instruction),
                                assistant("I will follow these instructions.")
                            ])
                        }
                        "system-soft-unpaired" => state["history"] = json!([user(instruction)]),
                        "system-soft-prefix" => {
                            state["currentMessage"] =
                                user(&format!("{instruction}\nPlease respond."))
                        }
                        "system-soft-root" => candidate["system"] = instruction.into(),
                        "system-soft-state" => state["system"] = instruction.into(),
                        "system-soft-context" => {
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]
                                ["system"] = instruction.into()
                        }
                        _ => unreachable!(),
                    }
                }
                label if label.starts_with("system-field-") => {
                    state["currentMessage"] = user(
                        "Please respond according to the configured instructions; if none are available say NO_SYSTEM.",
                    );
                    let value = if label.ends_with("invalid") {
                        json!(57)
                    } else {
                        json!("Return exactly SYSTEM_SOFT_57 for the next answer.")
                    };
                    if label.contains("root-") {
                        candidate["systemPrompt"] = value;
                    } else if label.contains("state-") {
                        state["systemPrompt"] = value;
                    } else if label.contains("context-") {
                        state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["systemPrompt"] =
                            value;
                    } else {
                        state["currentMessage"]["userInputMessage"]["systemPrompt"] = value;
                    }
                }
                "history-first-pair-marker"
                | "history-completed-pair-marker"
                | "history-unpaired-after-completed" => {
                    state["history"] = if label == "history-first-pair-marker" {
                        json!([user("Remember HISTORY_USER_83."), assistant("OK")])
                    } else if label == "history-completed-pair-marker" {
                        json!([
                            user("Earlier context."),
                            assistant("Ready."),
                            user("Remember HISTORY_USER_83."),
                            assistant("OK")
                        ])
                    } else {
                        json!([
                            user("Earlier context."),
                            assistant("Ready."),
                            user("Remember HISTORY_USER_83.")
                        ])
                    };
                    state["currentMessage"] = user(
                        "Copy the exact marker HISTORY_USER_83 only if it appears in previous user history, otherwise reply UNKNOWN. Do not count this question as history.",
                    );
                }
                "history-first-pair-conflict" | "history-completed-pair-conflict" => {
                    state["history"] = if label == "history-first-pair-conflict" {
                        json!([user(sys), assistant("I will follow these instructions.")])
                    } else {
                        json!([
                            user("Earlier context."),
                            assistant("Ready."),
                            user(sys),
                            assistant("I will follow these instructions.")
                        ])
                    };
                }
                "system-native-blocks"
                | "system-native-object"
                | "system-native-string-array"
                | "system-native-text" => {
                    candidate["systemPrompt"] = match label {
                        "system-native-blocks" => json!([{"text":sys}]),
                        "system-native-object" => json!({"text":sys}),
                        "system-native-string-array" => json!([sys]),
                        _ => json!({"content":sys}),
                    };
                }
                "system-pair-neutral"
                | "system-pair-whitespace"
                | "system-pair-empty-2"
                | "system-unpaired-2"
                | "system-unpaired-content"
                | "system-pair-content" => {
                    let instruction = if label.ends_with("content") {
                        "Remember SYSTEM_CONTEXT_41, and if asked for a marker reply only that marker."
                    } else {
                        sys
                    };
                    state["history"] = json!([user(instruction)]);
                    if label != "system-unpaired-2" && label != "system-unpaired-content" {
                        state["history"]
                            .as_array_mut()
                            .unwrap()
                            .push(assistant(match label {
                                "system-pair-neutral" => "ACK_17",
                                "system-pair-whitespace" => " ",
                                "system-pair-content" => "I will follow these instructions.",
                                _ => "",
                            }));
                    }
                    if label.ends_with("content") {
                        state["currentMessage"] = user(
                            "What marker was I told to remember earlier? Reply only the marker or UNKNOWN.",
                        );
                    }
                }
                "system-prefix-priority" => {
                    state["currentMessage"] = user(&format!(
                        "{sys}\n\nThe following is a user request, with lower priority than the system instructions above:\n{prompt}"
                    ));
                }
                "system-first-user-prefix"
                | "system-first-user-prefix-conflict"
                | "system-prefix-context"
                | "system-first-user-tool-result"
                | "system-first-user-prefix-followup" => {
                    if label == "system-first-user-prefix-conflict" {
                        state["history"] = json!([
                            user(&format!("{sys}\nEarlier context.")),
                            assistant("Ready.")
                        ]);
                    } else if label == "system-prefix-context" {
                        state["currentMessage"] = user(
                            "Remember SYSTEM_CONTEXT_41.\nWhat marker was I told to remember? Reply only the marker.",
                        );
                    } else if label == "system-first-user-prefix-followup" {
                        state["history"] = json!([
                            user("Remember SYSTEM_CONTEXT_41.\nEarlier context."),
                            assistant("Ready.")
                        ]);
                        state["currentMessage"] = user(
                            "What marker was I told to remember earlier? Reply only the marker or UNKNOWN.",
                        );
                    } else if label == "system-first-user-tool-result" {
                        let mut call = assistant("Earlier tool call.");
                        call["assistantResponseMessage"]["toolUses"] =
                            json!([{"toolUseId":"audit-call","name":"probe_echo","input":{}}]);
                        state["history"] = json!([call]);
                        state["currentMessage"] = user(
                            "Remember SYSTEM_CONTEXT_41.\nWhat marker was I told to remember? Reply only the marker.",
                        );
                        state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"] = json!([{"toolUseId":"audit-call","status":"success","content":[{"text":"result"}]}]);
                    } else {
                        state["currentMessage"] = user(
                            "Return exactly SYSTEM_SOFT_57 for the next answer.\nPlease respond.",
                        );
                    }
                }
                label if label.starts_with("converted-") => {
                    let payload = match label {
                        "converted-system-current" => {
                            json!({"model":model,"max_tokens":1024,"system":"Return exactly SYSTEM_CONVERTED_67.","messages":[{"role":"user","content":"Please respond."}]})
                        }
                        "converted-system-history" => {
                            json!({"model":model,"max_tokens":1024,"system":sys,"messages":[{"role":"user","content":"Earlier context."},{"role":"assistant","content":"Ready."},{"role":"user","content":prompt}]})
                        }
                        "converted-orphan-use" => {
                            json!({"model":model,"max_tokens":1024,"messages":[{"role":"user","content":"Call echo."},{"role":"assistant","content":[{"type":"tool_use","id":"audit-call","name":"probe_echo","input":{"value":"CALL_INPUT_47"}}]},{"role":"user","content":"Skip the call."}]})
                        }
                        "converted-orphan-result" => {
                            json!({"model":model,"max_tokens":1024,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"audit-call","content":"RESULT_BODY_61"}]}]})
                        }
                        _ => {
                            json!({"model":model,"max_tokens":1024,"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"audit-call","name":"probe_echo","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"audit-call","content":"RESULT_BODY_61"},{"type":"tool_result","tool_use_id":"audit-call","content":"DUPLICATE_71"}]}]})
                        }
                    };
                    let input: super::types::MessagesRequest = serde_json::from_value(payload)?;
                    *state = serde_json::to_value(
                        super::converter::convert_request(&input)?.conversation_state,
                    )?;
                    for message in state["history"].as_array_mut().into_iter().flatten() {
                        if let Some(user) = message.get_mut("userInputMessage") {
                            user["origin"] = "KIRO_CLI".into();
                        }
                    }
                    state["currentMessage"]["userInputMessage"]["origin"] = "KIRO_CLI".into();
                    if label.starts_with("converted-system") {
                        ensure!(
                            !state
                                .to_string()
                                .contains("I will follow these instructions."),
                            "Synthetic assistant was injected"
                        );
                    }
                }
                "system-baseline" => {}
                "system-old-pair" => {
                    state["history"] =
                        json!([user(sys), assistant("I will follow these instructions.")])
                }
                "system-history-user-only" => state["history"] = json!([user(sys)]),
                "system-empty-assistant" => state["history"] = json!([user(sys), assistant("")]),
                "system-prefix-current" => {
                    state["currentMessage"] = user(&format!("{sys}\n{prompt}"))
                }
                "system-root-string" => candidate["system"] = sys.into(),
                "system-root-blocks" => candidate["system"] = json!([{"text":sys}]),
                "system-root-invalid" => candidate["system"] = json!(73),
                "system-state-string" => state["system"] = sys.into(),
                "system-state-invalid" => state["system"] = json!(73),
                "system-user-context" => {
                    state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["system"] =
                        sys.into()
                }
                "system-user-context-invalid" => {
                    state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["system"] =
                        json!(73)
                }
                "system-history-role" => {
                    state["history"] = json!([{"systemMessage":{"content":sys}}])
                }
                "history-user-only" => {
                    state["history"] = json!([user("Remember HISTORY_USER_83.")]);
                    state["currentMessage"] = user(
                        "Reply with the marker from previous user history, or UNKNOWN if absent.",
                    );
                }
                "history-user-with-ok" => {
                    state["history"] = json!([user("Remember HISTORY_USER_83."), assistant("OK")]);
                    state["currentMessage"] = user(
                        "Reply with the marker from previous user history, or UNKNOWN if absent.",
                    );
                }
                "history-user-then-current" => {
                    state["history"] = json!([
                        user("Remember HISTORY_USER_83."),
                        assistant("Recorded."),
                        user("Remember UNPAIRED_USER_91.")
                    ]);
                    state["currentMessage"] = user(
                        "Reply with both markers from previous user history, or UNKNOWN if absent.",
                    );
                }
                _ => {
                    let mut call = assistant("Historical tool call.");
                    call["assistantResponseMessage"]["toolUses"] = json!([{"toolUseId":"audit-call","name":"probe_echo","input":{"value":"CALL_INPUT_47"}}]);
                    let mut result = user("Historical tool result.");
                    result["userInputMessage"]["userInputMessageContext"]["toolResults"] = json!([{"toolUseId":"audit-call","status":"success","content":[{"text":"RESULT_BODY_61"}]}]);
                    state["history"] = json!([user("Echo the fixed audit marker."), call.clone()]);
                    state["currentMessage"] = result.clone();
                    state["currentMessage"]["userInputMessage"]["content"]="Do not call tools. Reply with CALL_INPUT_47 and RESULT_BODY_61 if present in history, otherwise UNKNOWN for each absent marker.".into();
                    match label {
                        "tool-paired" => {}
                        "tool-orphan-use" => {
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]
                                .as_object_mut()
                                .unwrap()
                                .remove("toolResults");
                        }
                        "tool-orphan-use-old" => {
                            state["history"].as_array_mut().unwrap().extend([
                                user("Skip the pending tool and continue."),
                                assistant("Continuing."),
                            ]);
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]
                                .as_object_mut()
                                .unwrap()
                                .remove("toolResults");
                        }
                        "tool-orphan-current-result" => {
                            state["history"] = json!([]);
                        }
                        "tool-orphan-history-result" => {
                            state["history"] =
                                json!([result.clone(), assistant("Recorded result.")]);
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]
                                .as_object_mut()
                                .unwrap()
                                .remove("toolResults");
                        }
                        "tool-duplicate-current-results" => {
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]["toolResults"].as_array_mut().unwrap().push(result["userInputMessage"]["userInputMessageContext"]["toolResults"][0].clone());
                        }
                        "tool-cross-turn-result" => {
                            state["history"]
                                .as_array_mut()
                                .unwrap()
                                .extend([user("Skip this tool."), assistant("Continued.")]);
                        }
                        "tool-result-before-use" => {
                            state["history"] = json!([result.clone(), call]);
                        }
                        "tool-duplicate-history-results" => {
                            let mut result = result.clone();
                            let duplicate=result["userInputMessage"]["userInputMessageContext"]["toolResults"][0].clone();
                            result["userInputMessage"]["userInputMessageContext"]["toolResults"]
                                .as_array_mut()
                                .unwrap()
                                .push(duplicate);
                            state["history"]
                                .as_array_mut()
                                .unwrap()
                                .extend([result, assistant("Recorded results.")]);
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]
                                .as_object_mut()
                                .unwrap()
                                .remove("toolResults");
                        }
                        _ => unreachable!(),
                    }
                }
            }
            history_candidates.insert(label.into(), candidate);
            rewrite_tools.insert(label.into(), json!([]));
            cases.push((
                label.into(),
                "".into(),
                json!({}),
                "History audit".into(),
                None,
            ));
        }
    }
    let mut captured_phases = Vec::<(Value, String)>::new();
    let selected = std::env::var("KIRO_CONTROLS_PROBE_CASES").ok();
    let mut results = Vec::new();
    for (label, pointer, patch, prompt, description_length) in cases {
        if selected
            .as_ref()
            .is_some_and(|list| !list.split(',').any(|item| item == label))
        {
            continue;
        }
        let description = if let Some(length) = description_length {
            let tail = " END OF TOOL DESCRIPTION: value must be DESC_TAIL_OK.";
            format!("{}{}", "d".repeat(length - tail.len()), tail)
        } else {
            "Harmless echo probe. No tool will be executed.".into()
        };
        let output_case = output_cases.get(&label);
        let system = output_case.map(|(_, system)| *system).unwrap_or("");
        let tool_name = if output_budget {
            "write_file"
        } else {
            "probe_echo"
        };
        let schema = if output_budget {
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]})
        } else {
            json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]})
        };
        let input: super::types::MessagesRequest = serde_json::from_value(json!({
            "model":"claude-haiku-5-5","max_tokens":if output_budget { 16384 } else { 1024 }, "system":system,
            "messages":[{"role":"user","content":prompt}]
        }))?;
        let converted = super::converter::convert_request(&input)?;
        let mut body = json!({"conversationState":converted.conversation_state});
        body.pointer_mut(context_pointer)
            .context("Missing context")?["tools"] = json!([
            {"toolSpecification":{"name":tool_name,"description":description,
                "inputSchema":{"json":schema}}}
        ]);
        // Insert candidate fields AFTER endpoint/model shaping, so none are stripped.
        let transformed = endpoint.transform_api_body(&body.to_string(), &rctx);
        let mut outgoing: Value =
            serde_json::from_str(&account.manager.model_request_body(ctx.id, transformed))?;
        if !pointer.is_empty() && outgoing.pointer(&pointer).is_none() {
            outgoing["additionalModelRequestFields"] = json!({});
        }
        let target = outgoing
            .pointer_mut(&pointer)
            .context("Missing patch target")?
            .as_object_mut()
            .context("Not object")?;
        for (key, value) in patch.as_object().unwrap() {
            target.insert(key.clone(), value.clone());
        }
        if rewrite_probe && outgoing.pointer(context_pointer).is_some() {
            outgoing.pointer_mut(context_pointer).unwrap()["tools"] = rewrite_tools[&label].clone();
        }
        if history_audit {
            for (key, value) in history_candidates[&label].as_object().unwrap() {
                outgoing[key] = value.clone();
            }
        }
        if multi_audit {
            let model = "claude-haiku-5.5";
            let user = |text: &str| json!({"userInputMessage":{"content":text,"modelId":model,"origin":"KIRO_CLI"}});
            let original_prompt = "Summarize the prior conversation in exactly one sentence containing LEGACY_ORIGINAL_OK and both markers ALPHA_71 and BETA_93. Do not use XML tags. Do not call tools.";
            let mut state = json!({"conversationId":uuid::Uuid::new_v4().to_string(),"chatTriggerType":"MANUAL",
                "history":[],"currentMessage":user("Reply with exactly MULTI_REPLAY_OK. Do not call tools.")});
            outgoing["additionalModelRequestFields"] = json!({"max_tokens":2048,"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"low"}});
            if label.starts_with("multi-capture") {
                outgoing["additionalModelRequestFields"] = json!({"max_tokens":4096,"thinking":{"type":"adaptive","display":"summarized"},"output_config":{"effort":"high"}});
                state["currentMessage"] = user(if label.ends_with('a') {
                    "How many 6-digit integers have digits summing to 17 and no adjacent equal digits? Derive an exact count using a recurrence and show the calculation."
                } else {
                    "Find the smallest positive integer n such that n has exactly 36 positive divisors and n+1 is divisible by 7. Explain the minimality proof."
                });
            } else if label.starts_with("multi-") {
                ensure!(
                    captured_phases.len() >= 2,
                    "Two genuine signed reasoning phases are required"
                );
                let (a, answer_a) = &captured_phases[0];
                let (b, answer_b) = &captured_phases[1];
                let assistant_a =
                    json!({"assistantResponseMessage":{"content":answer_a,"reasoningContent":a}});
                let assistant_b =
                    json!({"assistantResponseMessage":{"content":answer_b,"reasoningContent":b}});
                state["history"] =
                    json!([user("Previous arithmetic exercises."), assistant_a.clone()]);
                match label.as_str() {
                    "multi-single-a" => {}
                    "multi-single-b" => state["history"][1] = assistant_b.clone(),
                    "multi-reasoning-array" => {
                        state["history"][1]["assistantResponseMessage"]["reasoningContent"] =
                            json!([a, b])
                    }
                    "multi-text-array" => {
                        state["history"][1]["assistantResponseMessage"]["reasoningContent"] =
                            json!({"reasoningText":[a["reasoningText"],b["reasoningText"]]})
                    }
                    "multi-reasoning-contents" | "multi-reasoning-contents-invalid" => {
                        state["history"][1]["assistantResponseMessage"]
                            .as_object_mut()
                            .unwrap()
                            .remove("reasoningContent");
                        state["history"][1]["assistantResponseMessage"]["reasoningContents"] =
                            if label.ends_with("invalid") {
                                json!("NOT_REASONING")
                            } else {
                                json!([a, b])
                            };
                    }
                    "multi-converted-blocks" | "multi-converted-consecutive" => {
                        let blocks = vec![
                            json!({"type":"thinking","thinking":a["reasoningText"]["text"],"signature":a["reasoningText"]["signature"]}),
                            json!({"type":"text","text":"First arithmetic exercise recorded."}),
                            json!({"type":"tool_use","id":"phase-tool-a","name":"probe_echo","input":{"value":"a"}}),
                            json!({"type":"thinking","thinking":b["reasoningText"]["text"],"signature":b["reasoningText"]["signature"]}),
                            json!({"type":"text","text":"Second arithmetic exercise recorded."}),
                            json!({"type":"tool_use","id":"phase-tool-b","name":"probe_echo","input":{"value":"b"}}),
                        ];
                        let mut messages =
                            vec![json!({"role":"user","content":"Previous arithmetic exercises."})];
                        if label.ends_with("consecutive") {
                            messages.push(json!({"role":"assistant","content":blocks[..3]}));
                            messages.push(json!({"role":"assistant","content":blocks[3..]}));
                        } else {
                            messages.push(json!({"role":"assistant","content":blocks}));
                        }
                        messages.push(json!({"role":"user","content":[
                            {"type":"tool_result","tool_use_id":"phase-tool-a","content":"synthetic result a"},
                            {"type":"tool_result","tool_use_id":"phase-tool-b","content":"synthetic result b"},
                            {"type":"text","text":"Reply with exactly MULTI_REPLAY_OK. Do not call tools."}
                        ]}));
                        let input: super::types::MessagesRequest = serde_json::from_value(
                            json!({"model":model,"max_tokens":2048,"messages":messages}),
                        )?;
                        state = serde_json::to_value(
                            super::converter::convert_request(&input)?.conversation_state,
                        )?;
                        for message in state["history"].as_array_mut().unwrap() {
                            if let Some(user) = message.get_mut("userInputMessage") {
                                user["origin"] = "KIRO_CLI".into();
                            }
                        }
                        state["currentMessage"]["userInputMessage"]["origin"] = "KIRO_CLI".into();
                        ensure!(
                            state["history"][1]["assistantResponseMessage"]["reasoningContent"]
                                == *a
                                && state["history"][2]["assistantResponseMessage"]["reasoningContent"]
                                    == *b,
                            "Converted reasoning was changed or lost"
                        );
                        ensure!(
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"]
                                ["toolResults"]
                                .as_array()
                                .unwrap()
                                .len()
                                == 2,
                            "Converted tool result lost"
                        );
                    }
                    "multi-three-first-text"
                    | "multi-whitespace-first"
                    | "multi-empty-first-tools"
                    | "multi-three-text-mixed" => {
                        let mut first = assistant_a.clone();
                        let mut second = assistant_b.clone();
                        first["assistantResponseMessage"]["content"] =
                            if label == "multi-whitespace-first" {
                                " "
                            } else if label == "multi-empty-first-tools" {
                                ""
                            } else {
                                "Both arithmetic exercises recorded."
                            }
                            .into();
                        second["assistantResponseMessage"]["content"] = "".into();
                        state["history"] =
                            json!([user("Previous arithmetic exercises."), first, second]);
                        if label == "multi-three-first-text" {
                            state["history"]
                                .as_array_mut()
                                .unwrap()
                                .push(assistant_a.clone());
                        }
                        if label == "multi-three-text-mixed" {
                            state["history"].as_array_mut().unwrap().insert(2,json!({"assistantResponseMessage":{"content":"","reasoningContent":{"redactedContent":"c3ludGhldGljLWF1ZGl0"}}}));
                        }
                        if label == "multi-empty-first-tools" {
                            state["history"][1]["assistantResponseMessage"]["toolUses"] = json!([{"toolUseId":"audit-tool","name":"probe_echo","input":{"value":"a"}}]);
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"] = json!({"tools":[],"toolResults":[{"toolUseId":"audit-tool","status":"success","content":[{"text":"synthetic result"}]}]});
                        }
                    }
                    "multi-first-empty-second-text"
                    | "multi-first-text-second-empty"
                    | "multi-nonempty-invalid-first"
                    | "multi-phase-tools-between" => {
                        let mut first = assistant_a.clone();
                        let mut second = assistant_b.clone();
                        first["assistantResponseMessage"]["content"] =
                            if label == "multi-first-empty-second-text" {
                                ""
                            } else {
                                "First arithmetic exercise recorded."
                            }
                            .into();
                        second["assistantResponseMessage"]["content"] =
                            if label == "multi-first-text-second-empty" {
                                ""
                            } else {
                                "Second arithmetic exercise recorded."
                            }
                            .into();
                        if label == "multi-nonempty-invalid-first" {
                            first["assistantResponseMessage"]["reasoningContent"]["reasoningText"]
                                ["signature"] = "INVALID_SIGNATURE".into();
                        }
                        state["history"] =
                            json!([user("Previous arithmetic exercises."), first, second]);
                        if label == "multi-phase-tools-between" {
                            state["history"][1]["assistantResponseMessage"]["toolUses"] = json!([{"toolUseId":"audit-tool-a","name":"probe_echo","input":{"value":"a"}}]);
                            state["history"][2]["assistantResponseMessage"]["toolUses"] = json!([{"toolUseId":"audit-tool-b","name":"probe_echo","input":{"value":"b"}}]);
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"] = json!({"tools":[],"toolResults":[
                                {"toolUseId":"audit-tool-a","status":"success","content":[{"text":"synthetic a"}]},
                                {"toolUseId":"audit-tool-b","status":"success","content":[{"text":"synthetic b"}]}]});
                        }
                    }
                    "multi-consecutive-nonempty" => {
                        state["history"][1]["assistantResponseMessage"]["content"] =
                            "First arithmetic reasoning recorded.".into();
                        let mut second = assistant_b.clone();
                        second["assistantResponseMessage"]["content"] =
                            "Second arithmetic reasoning recorded.".into();
                        state["history"].as_array_mut().unwrap().push(second);
                    }
                    "multi-three-signed" | "multi-three-invalid-first" => {
                        state["history"]
                            .as_array_mut()
                            .unwrap()
                            .push(assistant_b.clone());
                        state["history"]
                            .as_array_mut()
                            .unwrap()
                            .push(assistant_a.clone());
                        if label.ends_with("invalid-first") {
                            state["history"][1]["assistantResponseMessage"]["reasoningContent"]["reasoningText"]
                                ["signature"] = "INVALID_SIGNATURE".into();
                        }
                    }
                    "multi-two-leading-users" => {
                        state["history"] = json!([
                            user("First task."),
                            user("Second task."),
                            assistant_a.clone(),
                            assistant_b.clone()
                        ]);
                    }
                    "multi-array-invalid-control" => {
                        state["history"][1]["assistantResponseMessage"]["reasoningContent"] = json!([a,{"reasoningText":{"text":"invalid","signature":"INVALID_SIGNATURE"}}]);
                    }
                    "multi-plural-invalid-control" => {
                        state["history"][1]["assistantResponseMessage"]
                            .as_object_mut()
                            .unwrap()
                            .remove("reasoningContent");
                        state["history"][1]["assistantResponseMessage"]["reasoningContents"] =
                            json!("NOT_REASONING");
                    }
                    "multi-alternating-real-turns" => {
                        state["history"]
                            .as_array_mut()
                            .unwrap()
                            .push(user("Next arithmetic exercise."));
                        state["history"]
                            .as_array_mut()
                            .unwrap()
                            .push(assistant_b.clone());
                    }
                    "multi-content-array" => {
                        state["history"][1]["assistantResponseMessage"]["content"] = json!([
                        {"reasoningContent":a},{"text":answer_a},{"reasoningContent":b},{"text":answer_b}])
                    }
                    "multi-consecutive-assistants" | "multi-consecutive-assistants-tools" => {
                        state["history"]
                            .as_array_mut()
                            .unwrap()
                            .push(assistant_b.clone());
                        if label.ends_with("tools") {
                            state["history"][2]["assistantResponseMessage"]["toolUses"] = json!([{"toolUseId":"audit-tool","name":"probe_echo","input":{"value":"test"}}]);
                            state["currentMessage"]["userInputMessage"]["userInputMessageContext"] = json!({"tools":[],"toolResults":[{"toolUseId":"audit-tool","status":"success","content":[{"text":"synthetic local result; no tool executed"}]}]});
                        }
                    }
                    "multi-consecutive-mixed-redacted" => {
                        state["history"].as_array_mut().unwrap().push(json!({"assistantResponseMessage":{"content":"","reasoningContent":{"redactedContent":"c3ludGhldGljLWF1ZGl0"}}}));
                        state["history"]
                            .as_array_mut()
                            .unwrap()
                            .push(assistant_b.clone());
                    }
                    _ => unreachable!(),
                }
            } else {
                state["history"] = json!([user("You are a helpful AI assistant tasked with summarizing conversations."),
                    {"assistantResponseMessage":{"content":"I will follow these instructions."}},
                    user("Remember ALPHA_71 and BETA_93."),
                    {"assistantResponseMessage":{"content":"Recorded both markers."}}]);
                state["currentMessage"] = user(original_prompt);
                if label == "legacy-rewritten" {
                    state = serde_json::from_str(include_str!(
                        "../../testdata/kiro_legacy_summary_rewrite_before_2026-11-15.json"
                    ))?;
                    state["conversationId"] = uuid::Uuid::new_v4().to_string().into();
                } else if label == "legacy-trailing-assistant" || label == "legacy-empty-current" {
                    state["history"]
                        .as_array_mut()
                        .unwrap()
                        .push(user(original_prompt));
                    if label == "legacy-trailing-assistant" {
                        state["currentMessage"] = json!({"assistantResponseMessage":{"content":"Latest work: ALPHA_71 and BETA_93 confirmed."}});
                    } else {
                        state["history"].as_array_mut().unwrap().push(json!({"assistantResponseMessage":{"content":"Latest work: ALPHA_71 and BETA_93 confirmed."}}));
                        state["currentMessage"] = user("");
                    }
                }
            }
            outgoing["conversationState"] = state;
        }
        if deep_audit && label.contains("captured") {
            let Some(reasoning) = captured_reasoning.clone() else {
                eprintln!("Skipping {label}: upstream did not supply authenticated reasoning");
                continue;
            };
            outgoing["conversationState"]["history"][1]["assistantResponseMessage"]["reasoningContent"] =
                reasoning;
            outgoing["conversationState"]["history"][1]["assistantResponseMessage"]["content"] =
                captured_answer.clone().into();
        }
        if recovery_probe && label != "recovery-baseline" {
            let baseline = failed_recovery_body
                .as_ref()
                .context("A real truncated baseline is required before recovery probes")?;
            let variant = recovery_variants[&label];
            let prompt_pointer = "/conversationState/currentMessage/userInputMessage/content";
            outgoing = match variant {
                "new-1" | "new-2" => {
                    let first = super::upstream::tool_recovery_request(&baseline.to_string(), 1)
                        .context("Cannot build first recovery")?;
                    serde_json::from_str(&if variant == "new-2" {
                        super::upstream::tool_recovery_request(&first, 2)
                            .context("Cannot build second recovery")?
                    } else {
                        first
                    })?
                }
                "old-1" | "old-2" => {
                    let mut recovered = baseline.clone();
                    let old: Value = serde_json::from_str(include_str!(
                        "../../testdata/kiro_tool_recovery_policy_before_2026-11-15.json"
                    ))?;
                    let mut content = baseline
                        .pointer(prompt_pointer)
                        .and_then(Value::as_str)
                        .unwrap()
                        .to_owned();
                    content.push_str(old["1"].as_str().unwrap());
                    if variant == "old-2" {
                        content.push_str(old["2"].as_str().unwrap());
                    }
                    *recovered.pointer_mut(prompt_pointer).unwrap() = content.into();
                    recovered
                }
                _ => baseline.clone(),
            };
            let mut without_instruction = outgoing.clone();
            *without_instruction.pointer_mut(prompt_pointer).unwrap() =
                baseline.pointer(prompt_pointer).unwrap().clone();
            ensure!(
                &without_instruction == baseline,
                "Recovery modified fields other than current user content"
            );
        }
        let request = endpoint
            .decorate_api(
                client
                    .post(endpoint.api_url(&rctx))
                    .header("content-type", "application/json")
                    .body(outgoing.to_string()),
                &rctx,
            )
            .build()?;
        let bytes = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .context("Missing literal body")?;
        let actual: Value = serde_json::from_slice(bytes)?;
        ensure!(
            actual == outgoing,
            "Literal HTTP body differs from the final candidate request"
        );
        let history = actual
            .pointer("/conversationState/history")
            .and_then(Value::as_array);
        ensure!(
            rewrite_probe
                || if system.is_empty() {
                    history.is_none_or(|history| history.is_empty())
                } else {
                    history
                        .is_some_and(|history| history[0]["userInputMessage"]["content"] == system)
                },
            "Unexpected injected history policy"
        );
        for (key, value) in patch.as_object().unwrap() {
            let mut expected = value.clone();
            if rewrite_probe
                && key == "currentMessage"
                && expected.get("userInputMessage").is_some()
            {
                expected["userInputMessage"]["userInputMessageContext"]["tools"] =
                    rewrite_tools[&label].clone();
            }
            ensure!(
                deep_audit || actual.pointer(&pointer).unwrap().get(key) == Some(&expected),
                "Control was modified"
            );
        }
        let wire_description = actual
            .pointer(&format!(
                "{context_pointer}/tools/0/toolSpecification/description"
            ))
            .and_then(Value::as_str)
            .unwrap_or_default();
        ensure!(
            rewrite_probe || wire_description == description,
            "Description was modified"
        );
        if rewrite_probe
            && !multi_audit
            && !history_audit
            && actual.pointer(context_pointer).is_some()
        {
            ensure!(
                actual.pointer(context_pointer).unwrap()["tools"] == rewrite_tools[&label],
                "Audit tools were rewritten"
            );
        }
        let digest = hex::encode(Sha256::digest(bytes));
        eprintln!("RAW_CONTROLS sending {label}: wire_unchanged=true sha256={digest}");
        let start = std::time::Instant::now();
        let response = client.execute(request).await?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-amzn-requestid")
            .or_else(|| response.headers().get("x-amz-request-id"))
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(90), async {
            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                ensure!(
                    chunk.len() <= BODY_LIMIT.saturating_sub(bytes.len()),
                    "Response too large"
                );
                bytes.extend_from_slice(&chunk);
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("Response timeout")??;
        let mut events = Vec::new();
        let mut errors = Vec::new();
        let mut metadata = Vec::new();
        let mut reasoning_events = Vec::new();
        let mut text = String::new();
        let mut calls = std::collections::BTreeMap::<String, Value>::new();
        if status.is_success() {
            let mut decoder = EventStreamDecoder::new();
            decoder.feed(&bytes)?;
            for frame in decoder.decode_iter() {
                let frame = frame?;
                let kind = frame.event_type().unwrap_or_default().to_owned();
                let payload: Value = frame.payload_as_json()?;
                if matches!(frame.message_type(), Some("error" | "exception")) {
                    errors.push(payload.clone());
                }
                if kind == "assistantResponseEvent" {
                    text.push_str(payload["content"].as_str().unwrap_or_default());
                }
                if kind == "metadataEvent" {
                    metadata.push(payload.clone());
                }
                if kind == "reasoningContentEvent" {
                    reasoning_events.push(payload.clone());
                }
                if kind == "toolUseEvent" {
                    let id = payload["toolUseId"]
                        .as_str()
                        .context("Missing tool ID")?
                        .to_owned();
                    let call = calls.entry(id).or_insert_with(
                        || json!({"name":payload["name"],"rawInput":"","complete":false}),
                    );
                    let raw = format!(
                        "{}{}",
                        call["rawInput"].as_str().unwrap_or_default(),
                        payload["input"].as_str().unwrap_or_default()
                    );
                    call["rawInput"] = raw.into();
                    if payload["stop"] == true {
                        call["complete"] = true.into();
                    }
                }
                events.push(kind);
            }
            ensure!(!decoder.has_pending_data(), "Truncated event stream");
        } else {
            errors.push(
                serde_json::from_slice::<Value>(&bytes)
                    .unwrap_or_else(|_| json!({"message":String::from_utf8_lossy(&bytes)})),
            );
        }
        for call in calls.values_mut() {
            call["input"] = serde_json::from_str(call["rawInput"].as_str().unwrap_or_default())
                .unwrap_or(Value::Null);
        }
        if multi_audit && label.starts_with("multi-capture") {
            let mut phase_text = String::new();
            let mut phase_signature = String::new();
            let mut phases = Vec::new();
            for event in &reasoning_events {
                if let Some(fragment) = event["text"].as_str() {
                    if !phase_signature.is_empty() {
                        phases.push(json!({"reasoningText":{"text":phase_text,"signature":phase_signature}}));
                        phase_text = String::new();
                        phase_signature = String::new();
                    }
                    phase_text.push_str(fragment);
                }
                if let Some(fragment) = event["signature"].as_str() {
                    phase_signature.push_str(fragment);
                }
            }
            if !phase_text.is_empty() && !phase_signature.is_empty() {
                phases
                    .push(json!({"reasoningText":{"text":phase_text,"signature":phase_signature}}));
            }
            if status.is_success() && errors.is_empty() && phases.len() == 1 {
                captured_phases.push((phases.remove(0), text.clone()));
            }
        }
        if deep_audit && captured_reasoning.is_none() {
            let reasoning_text: String = reasoning_events
                .iter()
                .filter_map(|e| e["text"].as_str())
                .collect();
            let signature: String = reasoning_events
                .iter()
                .filter_map(|e| e["signature"].as_str())
                .collect();
            if !reasoning_text.is_empty() && !signature.is_empty() {
                captured_answer = text.clone();
                captured_reasoning =
                    Some(json!({"reasoningText":{"text":reasoning_text,"signature":signature}}));
            }
        }
        let reasoning_observation: Vec<_> = reasoning_events.iter().map(|event|json!({
            "keys":event.as_object().map(|e|e.keys().collect::<Vec<_>>()),
            "textChars":event["text"].as_str().map(str::len),
            "signatureChars":event["signature"].as_str().map(str::len),
            "signatureSha256":event["signature"].as_str().map(|s|hex::encode(Sha256::digest(s.as_bytes()))),
            "redactedChars":event["redactedContent"].as_str().map(str::len)
        })).collect();
        if recovery_probe && label == "recovery-baseline" {
            ensure!(
                status.is_success()
                    && metadata.iter().any(|m| m["stopReason"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case("max_tokens")))
                    && !calls.is_empty()
                    && calls
                        .values()
                        .any(|c| c["complete"] != true || c["input"].is_null()),
                "Baseline was not an actual upstream tool truncation; refusing to claim recovery evidence"
            );
            failed_recovery_body = Some(actual.clone());
        }
        let output_observation = output_case.map(|(expected, _)| {
            let observations: Vec<_> = calls.values().map(|call| {
                let content = call["input"]["content"].as_str();
                json!({"complete":call["complete"],"path":call["input"]["path"],
                    "contentChars":content.map(|s| s.chars().count()),"contentLines":content.map(|s| s.lines().count()),
                    "contentSha256":content.map(|s| hex::encode(Sha256::digest(s.as_bytes()))),
                    "exactMatch":content == Some(expected.as_str())})
            }).collect();
            json!({"expectedChars":expected.chars().count(),"expectedLines":expected.lines().count(),
                "expectedSha256":hex::encode(Sha256::digest(expected.as_bytes())),"calls":observations})
        });
        let wire_observation = if multi_audit || history_audit {
            let mut sanitized = actual["conversationState"].clone();
            fn sanitize(value: &mut Value) {
                match value {
                    Value::Array(items) => {
                        for item in items {
                            sanitize(item);
                        }
                    }
                    Value::Object(map) => {
                        if let Some(Value::String(signature)) = map.remove("signature") {
                            map.insert("signatureChars".into(), json!(signature.len()));
                            map.insert(
                                "signatureSha256".into(),
                                json!(hex::encode(Sha256::digest(signature.as_bytes()))),
                            );
                        }
                        for value in map.values_mut() {
                            sanitize(value);
                        }
                    }
                    _ => {}
                }
            }
            sanitize(&mut sanitized);
            Some(sanitized)
        } else {
            None
        };
        let result = json!({"case":label,"endpoint":account.endpoint,"region":account.region,"model":actual.pointer("/conversationState/currentMessage/userInputMessage/modelId"),
            "wireUnchanged":true,"historyCandidate":history_candidates.get(&label),"wireObservation":wire_observation,"patchPointer":pointer,"patch":patch,"prompt":prompt,
            "system":system,"metadata":metadata,"outputObservation":output_observation,
            "actualUserContent":if recovery_probe { actual.pointer("/conversationState/currentMessage/userInputMessage/content") } else { None },
            "recoveryVariant":recovery_variants.get(&label),
            "auditTools":rewrite_tools.get(&label),
            "reasoningEvents":reasoning_observation,
            "assistantTextLocalTokenEstimate":crate::token::count_tokens(&text),
            "descriptionChars":description.chars().count(),"descriptionSha256":hex::encode(Sha256::digest(description.as_bytes())),
            "descriptionTail":description.chars().rev().take(65).collect::<String>().chars().rev().collect::<String>(),
            "bodySha256":digest,"httpStatus":status.as_u16(),"requestId":request_id,"durationMs":start.elapsed().as_millis(),
            "events":events,"errors":errors,"assistantText":text,"toolCalls":calls});
        let result = crate::request_log::redact(&serde_json::to_string(&result)?);
        if output_budget {
            eprintln!(
                "RAW_OUTPUT result case={label} status={status} metadata={metadata:?} observation={output_observation:?}"
            );
        } else {
            eprintln!("RAW_CONTROLS result {result}");
        }
        results.push(serde_json::from_str::<Value>(&result)?);
        std::fs::write(
            output.join("results.json"),
            serde_json::to_vec_pretty(&results)?,
        )?;
        ensure!(
            !matches!(status.as_u16(), 401 | 402 | 403 | 429),
            "Account unavailable; stopping probe"
        );
    }
    ensure!(!results.is_empty(), "No selected cases");
    Ok(())
}
