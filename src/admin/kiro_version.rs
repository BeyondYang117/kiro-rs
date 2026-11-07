//! Query the stable release feed for the running default transport.
use std::time::Duration;

use axum::{Json, extract::State, response::IntoResponse};
use serde::Serialize;
use serde_json::Value;

use super::{error::AdminServiceError, middleware::AdminState};
use crate::{
    http_client::{ProxyConfig, build_client, read_bounded_text},
    model::config::Config,
};

const SOURCE_URL: &str =
    "https://prod.download.desktop.kiro.dev/stable/metadata-linux-x64-stable.json";

const CLI_SOURCE_URL: &str = "https://prod.download.cli.kiro.dev/stable/latest/manifest.json";

pub(super) fn valid_version(version: &str) -> bool {
    let parts: Vec<_> = version.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
                && part.parse::<u32>().is_ok()
        })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LatestVersion {
    latest_version: String,
    published_at: Option<String>,
    checked_at: String,
    source_url: &'static str,
    endpoint: &'static str,
}

fn parse_metadata(body: &str) -> anyhow::Result<LatestVersion> {
    let document: Value = serde_json::from_str(body)?;
    let version = document["currentRelease"]
        .as_str()
        .filter(|version| valid_version(version))
        .ok_or_else(|| anyhow::anyhow!("invalid currentRelease"))?;
    let release = document["releases"]
        .as_array()
        .and_then(|releases| {
            releases.iter().find(|release| {
                release["version"].as_str() == Some(version)
                    && release["updateTo"]["version"].as_str() == Some(version)
            })
        })
        .ok_or_else(|| anyhow::anyhow!("current release metadata missing"))?;
    let published_at = release["updateTo"]["pub_date"]
        .as_str()
        .and_then(|date| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok())
        .map(|date| date.to_string());
    Ok(LatestVersion {
        latest_version: version.to_owned(),
        published_at,
        checked_at: chrono::Utc::now().to_rfc3339(),
        source_url: SOURCE_URL,
        endpoint: "ide",
    })
}

fn parse_cli_metadata(body: &str) -> anyhow::Result<LatestVersion> {
    let document: Value = serde_json::from_str(body)?;
    let version = document["version"]
        .as_str()
        .filter(|version| valid_version(version))
        .ok_or_else(|| anyhow::anyhow!("invalid CLI version"))?;
    anyhow::ensure!(
        document["packages"]
            .as_array()
            .is_some_and(|packages| packages.iter().any(|package| package["download"]
                .as_str()
                .is_some_and(|path| path.starts_with(&format!("{version}/"))))),
        "CLI release packages missing"
    );
    Ok(LatestVersion {
        latest_version: version.to_owned(),
        published_at: None,
        checked_at: chrono::Utc::now().to_rfc3339(),
        source_url: CLI_SOURCE_URL,
        endpoint: "cli",
    })
}

async fn fetch(client: &reqwest::Client, url: &str, cli: bool) -> anyhow::Result<LatestVersion> {
    let response = client.get(url).send().await?.error_for_status()?;
    // The shared HTTP client does not follow redirects. A non-JSON response fails closed.
    anyhow::ensure!(response.status().is_success(), "unexpected metadata status");
    let body = read_bounded_text(
        response,
        256 * 1024,
        Duration::from_secs(10),
        Duration::from_secs(10),
    )
    .await?;
    if cli {
        parse_cli_metadata(&body)
    } else {
        parse_metadata(&body)
    }
}

pub(super) async fn latest(config: &Config) -> Result<LatestVersion, AdminServiceError> {
    let proxy = config.proxy_url.as_ref().map(|url| ProxyConfig {
        url: url.clone(),
        username: config.proxy_username.clone(),
        password: config.proxy_password.clone(),
    });
    let result = async {
        let client = build_client(proxy.as_ref(), 15, config.tls_backend)?;
        let cli = config.default_endpoint == "cli";
        fetch(&client, if cli { CLI_SOURCE_URL } else { SOURCE_URL }, cli).await
    }
    .await;
    result.map_err(|_| {
        // Do not return network errors containing proxy credentials or response bodies.
        AdminServiceError::UpstreamError(
            "无法检查 Kiro 最新版本，请检查网络或全局代理后重试".into(),
        )
    })
}

pub(super) async fn check(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.latest_kiro_version().await {
        Ok(response) => Json(response).into_response(),
        Err(error) => (error.status_code(), Json(error.into_response())).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::TlsBackend;
    use axum::{Router, routing::get};

    #[test]
    fn cli_release_feed_requires_version_and_matching_packages() {
        let parsed = parse_cli_metadata(
            r#"{"version":"3.1.0","packages":[{"download":"3.1.0/kirocli-x86_64-linux.zip"}]}"#,
        )
        .unwrap();
        assert_eq!(parsed.latest_version, "3.1.0");
        assert_eq!(parsed.endpoint, "cli");
        assert_eq!(parsed.source_url, CLI_SOURCE_URL);
        for body in [
            "{}",
            r#"{"version":"3.1.0","packages":[]}"#,
            r#"{"version":"3.1.0","packages":[{"download":"1.4.121/other.zip"}]}"#,
        ] {
            assert!(parse_cli_metadata(body).is_err());
        }
    }

    #[test]
    fn metadata_requires_a_matching_stable_release() {
        let body = serde_json::json!({
            "currentRelease": "1.4.121",
            "releases": [
                {"version":"1.4.86", "updateTo":{"version":"1.4.86"}},
                {"version":"1.4.121", "updateTo":{"version":"1.4.121", "pub_date":"2026-11-06"}}
            ]
        });
        let parsed = parse_metadata(&body.to_string()).unwrap();
        assert_eq!(parsed.latest_version, "1.4.121");
        assert_eq!(parsed.published_at.as_deref(), Some("2026-11-06"));
        for version in [
            "",
            "1.2",
            "1.2.3.4",
            "1.2.3-beta",
            "01.2.3",
            "1.2.3\r\nx: y",
            "1.2.4294967296",
        ] {
            assert!(!valid_version(version), "{version}");
        }
        for body in [
            "{}",
            r#"{"currentRelease":"1.4.121","releases":[]}"#,
            r#"{"currentRelease":"1.4.121","releases":[{"version":"1.4.121","updateTo":{"version":"1.4.86"}}]}"#,
            "<html>error</html>",
        ] {
            assert!(parse_metadata(body).is_err());
        }
    }

    #[tokio::test]
    async fn metadata_fetch_rejects_http_errors_redirects_and_oversized_responses() {
        let app = Router::new()
            .route("/ok", get(|| async { r#"{"currentRelease":"1.4.121","releases":[{"version":"1.4.121","updateTo":{"version":"1.4.121"}}]}"# }))
            .route("/error", get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }))
            .route("/redirect", get(|| async { axum::response::Redirect::temporary("/ok") }))
            .route("/large", get(|| async { "x".repeat(256 * 1024 + 1) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = build_client(None, 5, TlsBackend::Rustls).unwrap();
        assert_eq!(
            fetch(&client, &format!("http://{address}/ok"), false)
                .await
                .unwrap()
                .latest_version,
            "1.4.121"
        );
        for path in ["error", "redirect", "large"] {
            assert!(
                fetch(&client, &format!("http://{address}/{path}"), false)
                    .await
                    .is_err(),
                "{path}"
            );
        }
        task.abort();
    }
}
