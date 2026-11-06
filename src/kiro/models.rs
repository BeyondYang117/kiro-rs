//! Account-scoped discovery from the current Kiro control plane.
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{machine_id, model::credentials::KiroCredentials};
use crate::{
    http_client::{ProxyConfig, build_client, read_bounded_text},
    model::config::Config,
};

const MAX_PAGES: usize = 10;
const MAX_MODELS: usize = 1000;
const MAX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const CATALOG_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AvailableModel {
    pub model_id: String,
    pub model_name: String,
    pub provider: Option<String>,
    pub rate_multiplier: Option<f64>,
    pub rate_unit: Option<String>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub effort_levels: Vec<String>,
    pub effort_schema_path: Option<String>,
    pub default_effort: Option<String>,
    pub thinking_types: Vec<String>,
    pub default_thinking: Option<String>,
    pub proxy_supported: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelCatalog {
    pub models: Vec<AvailableModel>,
    pub default_model_id: Option<String>,
    pub region: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    // Missing models is a protocol error, not an authoritative empty catalog.
    models: Vec<Value>,
    default_model: Option<Value>,
    next_token: Option<String>,
}

fn text(value: &Value, max_chars: usize) -> Option<String> {
    value
        .as_str()
        .filter(|s| !s.is_empty() && s.chars().count() <= max_chars)
        .map(str::to_owned)
}

fn choices(value: &Value) -> Vec<String> {
    let Some(values) = value.as_array().filter(|values| values.len() <= 32) else {
        return Vec::new();
    };
    // A truncated or partially parsed enum is unsafe as an authorization constraint.
    let Some(values) = values
        .iter()
        .map(|value| text(value, 64))
        .collect::<Option<Vec<_>>>()
    else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn normalize(model: &Value) -> anyhow::Result<AvailableModel> {
    let model_id = text(&model["modelId"], 256).context("model ID missing or too long")?;
    let schema = &model["additionalModelRequestFieldsSchema"];
    let mut effort_levels = Vec::new();
    let mut effort_schema_path = None;
    let mut default_effort = None;
    for path in ["output_config", "reasoning"] {
        let effort = &schema["properties"][path]["properties"]["effort"];
        let levels = choices(&effort["enum"]);
        if !levels.is_empty() {
            default_effort = text(&effort["default"], 64).filter(|level| levels.contains(level));
            effort_levels = levels;
            effort_schema_path = Some(format!("{path}.effort"));
            break;
        }
    }
    let thinking = &schema["properties"]["thinking"]["properties"]["type"];
    let thinking_types = choices(&thinking["enum"]);
    let default_thinking =
        text(&thinking["default"], 64).filter(|kind| thinking_types.contains(kind));
    // The proxy's alias matcher is deliberately permissive. Require an exact upstream
    // ID match here so a future haiku/opus/sonnet version isn't advertised as supported.
    let proxy_supported = crate::anthropic::map_model(&model_id).as_deref() == Some(&model_id);
    Ok(AvailableModel {
        model_name: text(&model["modelName"], 256).unwrap_or_else(|| model_id.clone()),
        model_id,
        provider: text(&model["modelProvider"], 128),
        rate_multiplier: model["rateMultiplier"]
            .as_f64()
            .filter(|v| v.is_finite() && *v >= 0.0),
        rate_unit: text(&model["rateUnit"], 128),
        max_input_tokens: model["tokenLimits"]["maxInputTokens"]
            .as_u64()
            .filter(|v| *v > 0),
        max_output_tokens: model["tokenLimits"]["maxOutputTokens"]
            .as_u64()
            .filter(|v| *v > 0),
        effort_levels,
        effort_schema_path,
        default_effort,
        thinking_types,
        default_thinking,
        proxy_supported,
    })
}

fn request(
    client: &reqwest::Client,
    url: &str,
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    next_token: Option<&str>,
) -> reqwest::RequestBuilder {
    let machine = machine_id::generate_from_credentials(credentials, config);
    let mut req = client
        .get(url)
        .query(&[("origin", "AI_EDITOR")])
        .bearer_auth(token)
        .header("accept", "application/json")
        .header(
            "user-agent",
            format!("KiroIDE-{}-{}", config.kiro_version, machine),
        )
        .header(
            "x-amz-user-agent",
            format!("KiroIDE-{}-{}", config.kiro_version, machine),
        )
        .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=1");
    if let Some(arn) = &credentials.profile_arn {
        req = req.query(&[("profileArn", arn)]);
    }
    if let Some(cursor) = next_token {
        req = req.query(&[("nextToken", cursor)]);
    }
    if credentials.is_api_key_credential() {
        req = req.header("tokentype", "API_KEY");
    } else if credentials.auth_method.as_deref().is_some_and(|method| {
        ["idc", "builder-id", "iam"]
            .iter()
            .any(|kind| method.eq_ignore_ascii_case(kind))
    }) || (credentials.auth_method.is_none()
        && credentials.client_id.is_some()
        && credentials.client_secret.is_some())
    {
        req = req.header("tokentype", "SSO_OIDC");
    }
    req
}

async fn fetch_pages(
    client: &reqwest::Client,
    url: &str,
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
) -> anyhow::Result<ModelCatalog> {
    let mut models = BTreeMap::new();
    let mut default_model_id = None;
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut bytes_left = MAX_BYTES;
    let mut total_models = 0;
    for page_number in 0..MAX_PAGES {
        let response = request(client, url, credentials, config, token, cursor.as_deref())
            .send()
            .await?;
        // No response body is included in errors; it may contain account information.
        ensure!(
            response.status().is_success(),
            "model discovery returned HTTP {}",
            response.status()
        );
        let body = read_bounded_text(
            response,
            bytes_left.min(1024 * 1024),
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
        .await?;
        bytes_left = bytes_left
            .checked_sub(body.len())
            .context("catalog response too large")?;
        let page: Page = serde_json::from_str(&body).context("invalid model catalog")?;
        total_models += page.models.len();
        ensure!(total_models <= MAX_MODELS, "too many models");
        for model in page.models {
            let model = normalize(&model)?;
            models.entry(model.model_id.clone()).or_insert(model);
        }
        if default_model_id.is_none() {
            default_model_id = page
                .default_model
                .as_ref()
                .and_then(|m| text(&m["modelId"], 256));
        }
        cursor = page.next_token.filter(|token| !token.is_empty());
        let Some(next) = &cursor else {
            return Ok(ModelCatalog {
                default_model_id: default_model_id.filter(|id| models.contains_key(id)),
                models: models.into_values().collect(),
                region: credentials.effective_api_region(config).to_owned(),
            });
        };
        ensure!(
            next.len() <= 8192 && cursors.insert(next.clone()),
            "model pagination cursor repeated or invalid"
        );
        ensure!(
            page_number + 1 < MAX_PAGES,
            "model pagination limit reached"
        );
    }
    unreachable!("pagination always returns or fails at MAX_PAGES")
}

pub(crate) async fn discover(
    credentials: &KiroCredentials,
    config: &Config,
    token: &str,
    proxy: Option<&ProxyConfig>,
) -> anyhow::Result<ModelCatalog> {
    config.validate_regions()?;
    credentials.validate_regions()?;
    let endpoint = credentials
        .endpoint
        .as_deref()
        .unwrap_or(&config.default_endpoint);
    ensure!(
        endpoint == "ide",
        "model discovery requires an IDE credential"
    );
    let region = credentials.effective_api_region(config);
    let url = format!("https://management.{region}.kiro.dev/List-Available-Models");
    let client = build_client(proxy, 15, config.tls_backend)?;
    tokio::time::timeout(
        Duration::from_secs(45),
        fetch_pages(&client, &url, credentials, config, token),
    )
    .await
    .context("model discovery timed out")?
}

#[cfg(test)]
mod tests;
