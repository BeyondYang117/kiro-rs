//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, Proxy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// Read JSON/text bodies without trusting Content-Length or buffering an unbounded stream.
/// Empty chunks do not extend the first-byte or idle deadline.
pub(crate) async fn read_bounded_text(
    response: reqwest::Response,
    max_bytes: usize,
    first_byte_timeout: Duration,
    idle_timeout: Duration,
) -> anyhow::Result<String> {
    use anyhow::Context;
    use futures::StreamExt;
    use tokio::time::{Instant, timeout_at};

    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        anyhow::bail!("Upstream response byte limit exceeded ({max_bytes} bytes)");
    }
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();
    let mut deadline = Instant::now() + first_byte_timeout.min(idle_timeout);
    loop {
        let next = timeout_at(deadline, stream.next())
            .await
            .context("Upstream response body timeout exceeded")?;
        let Some(chunk) = next else { break };
        let chunk = chunk
            .map_err(|error| error.without_url())
            .context("Failed to read upstream response body")?;
        if chunk.len() > max_bytes.saturating_sub(buffer.len()) {
            anyhow::bail!("Upstream response byte limit exceeded ({max_bytes} bytes)");
        }
        if !chunk.is_empty() {
            buffer.extend_from_slice(&chunk);
            deadline = Instant::now() + idle_timeout;
        }
    }
    // These endpoints return UTF-8 JSON. Match reqwest's lossy text decoding and BOM removal.
    let text = String::from_utf8_lossy(&buffer);
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}

/// 代理配置
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("url", &"[REDACTED]")
            .field(
                "has_auth",
                &(self.username.is_some() || self.password.is_some()),
            )
            .finish()
    }
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// Admin status only needs the proxy address, never URL authentication or extras.
pub(crate) fn proxy_display_url(value: &str) -> String {
    if value.eq_ignore_ascii_case("direct") {
        return "direct".into();
    }
    let Ok(mut url) = reqwest::Url::parse(value) else {
        return "[REDACTED]".into();
    };
    if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h") || url.host_str().is_none()
    {
        return "[REDACTED]".into();
    }
    if url.set_username("").is_err() || url.set_password(None).is_err() {
        return "[REDACTED]".into();
    }
    url.set_path("");
    url.set_query(None);
    url.set_fragment(None);
    url.as_str().trim_end_matches('/').to_owned()
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    let mut builder = Client::builder()
        // Proxy routing is controlled exclusively by config. In particular,
        // credential-level `direct` must also bypass OS and environment proxies.
        .no_proxy()
        // Requests carry refresh tokens, client secrets or custom API-key headers.
        // A redirect must never forward them to a different endpoint.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(15))
        // Reuse upstream TCP/TLS connections to reduce first-byte latency.
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(16)
        .tcp_keepalive(Duration::from_secs(30));

    match tls_backend {
        TlsBackend::Rustls => {
            builder = builder.use_rustls_tls();
        }
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            {
                anyhow::bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
            }
        }
    }

    if let Some(proxy_config) = proxy {
        let mut proxy = Proxy::all(&proxy_config.url)?;

        // 设置代理认证
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(username, password);
        }

        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 已启用代理");
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn proxy_debug_omits_url_credentials_and_password() {
        let proxy = ProxyConfig::new("http://hidden-user:hidden-password@localhost:8080")
            .with_auth("hidden-user", "hidden-password");
        let debug = format!("{proxy:?}");
        assert!(!debug.contains("hidden-user"));
        assert!(!debug.contains("hidden-password"));
    }

    #[test]
    fn proxy_display_keeps_address_and_discards_all_other_url_components() {
        for (value, expected) in [
            (
                "http://private-user:private-password@proxy.test:8080/private-path?private-query#private-fragment",
                "http://proxy.test:8080",
            ),
            ("https://private-user@proxy.test/", "https://proxy.test"),
            (
                "socks5://private%40user:private%3Apassword@[::1]:1080",
                "socks5://[::1]:1080",
            ),
            ("socks5h://proxy.test:1080", "socks5h://proxy.test:1080"),
            ("DIRECT", "direct"),
            ("private-malformed-value", "[REDACTED]"),
            ("file:///private-path", "[REDACTED]"),
            ("data:text/plain,private-content", "[REDACTED]"),
        ] {
            assert_eq!(proxy_display_url(value), expected);
        }
    }

    #[tokio::test]
    async fn configured_routing_ignores_environment_proxies() {
        const TARGET: &str = "KIRO_PROXY_TEST_TARGET_URL";
        const PROXY: &str = "KIRO_PROXY_TEST_EXPLICIT_URL";
        if let Ok(target_url) = std::env::var(TARGET) {
            // This branch runs in an isolated process: never mutate global env
            // while other tests or HTTP clients are running.
            let environment_client = Client::builder()
                .use_rustls_tls()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap();
            assert_eq!(
                environment_client
                    .get(&target_url)
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap(),
                "environment-proxy"
            );
            for backend in [TlsBackend::Rustls, TlsBackend::NativeTls] {
                if backend == TlsBackend::NativeTls && !cfg!(feature = "native-tls") {
                    continue;
                }
                let credentials = crate::kiro::model::credentials::KiroCredentials {
                    proxy_url: Some("direct".into()),
                    ..Default::default()
                };
                let global = ProxyConfig::new("http://127.0.0.1:1");
                let direct = credentials.effective_proxy(Some(&global));
                let client = build_client(direct.as_ref(), 5, backend).unwrap();
                assert_eq!(
                    client
                        .get(&target_url)
                        .send()
                        .await
                        .unwrap()
                        .text()
                        .await
                        .unwrap(),
                    "direct-target"
                );

                let explicit = ProxyConfig::new(std::env::var(PROXY).unwrap());
                let client = build_client(Some(&explicit), 5, backend).unwrap();
                assert_eq!(
                    client
                        .get(&target_url)
                        .send()
                        .await
                        .unwrap()
                        .text()
                        .await
                        .unwrap(),
                    "environment-proxy"
                );
            }
            return;
        }

        use axum::{Router, routing::any};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let target_hits = Arc::new(AtomicUsize::new(0));
        let proxy_hits = Arc::new(AtomicUsize::new(0));
        let target_app = {
            let hits = target_hits.clone();
            Router::new().fallback(any(move || {
                hits.fetch_add(1, Ordering::SeqCst);
                async { "direct-target" }
            }))
        };
        let proxy_app = {
            let hits = proxy_hits.clone();
            Router::new().fallback(any(move || {
                hits.fetch_add(1, Ordering::SeqCst);
                async { "environment-proxy" }
            }))
        };
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_url = format!("http://{}/", target_listener.local_addr().unwrap());
        let proxy_url = format!("http://{}", proxy_listener.local_addr().unwrap());
        let target_task =
            tokio::spawn(async move { axum::serve(target_listener, target_app).await.unwrap() });
        let proxy_task =
            tokio::spawn(async move { axum::serve(proxy_listener, proxy_app).await.unwrap() });
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .kill_on_drop(true)
            .args([
                "--exact",
                "http_client::tests::configured_routing_ignores_environment_proxies",
                "--nocapture",
            ])
            .env(TARGET, target_url)
            .env(PROXY, &proxy_url);
        for name in [
            "HTTP_PROXY",
            "http_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "ALL_PROXY",
            "all_proxy",
        ] {
            command.env(name, &proxy_url);
        }
        for name in ["NO_PROXY", "no_proxy"] {
            command.env(name, "");
        }
        let result = tokio::time::timeout(Duration::from_secs(30), command.output()).await;
        target_task.abort();
        proxy_task.abort();
        let output = result.expect("proxy subprocess timed out").unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let backends = if cfg!(feature = "native-tls") { 2 } else { 1 };
        assert_eq!(target_hits.load(Ordering::SeqCst), backends);
        assert_eq!(proxy_hits.load(Ordering::SeqCst), backends + 1);
    }

    #[tokio::test]
    async fn redirects_never_forward_auth_headers_or_secret_bodies() {
        use axum::{Router, extract::Path, response::IntoResponse, routing::any};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let reached = Arc::new(AtomicUsize::new(0));
        let target = {
            let reached = reached.clone();
            Router::new().fallback(any(move || {
                reached.fetch_add(1, Ordering::SeqCst);
                async { "should never receive credentials" }
            }))
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other_origin = format!("http://{}/target", listener.local_addr().unwrap());
        let source = target.clone().route(
            "/{status}/{scope}",
            any(move |Path((status, scope)): Path<(u16, String)>| {
                let location = if scope == "cross" {
                    other_origin.clone()
                } else {
                    "/target".into()
                };
                async move {
                    (
                        http::StatusCode::from_u16(status).unwrap(),
                        [("location", location)],
                        "redirect",
                    )
                        .into_response()
                }
            }),
        );
        let target_task = tokio::spawn(async move { axum::serve(listener, target).await.unwrap() });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_url = format!("http://{}", listener.local_addr().unwrap());
        let source_task = tokio::spawn(async move { axum::serve(listener, source).await.unwrap() });
        let client = build_client(None, 5, TlsBackend::Rustls).unwrap();
        for status in [301, 302, 303, 307, 308] {
            for scope in ["same", "cross"] {
                let response = client.post(format!("{source_url}/{status}/{scope}"))
                    .bearer_auth("private-access-token")
                    .header("x-api-key", "private-api-key")
                    .json(&serde_json::json!({"refreshToken":"private-refresh-token", "clientSecret":"private-secret"}))
                    .send().await.unwrap();
                assert_eq!(response.status().as_u16(), status, "{scope}");
                assert!(response.headers().contains_key("location"));
                assert_eq!(reached.load(Ordering::SeqCst), 0);
            }
        }
        source_task.abort();
        target_task.abort();
    }
}

#[cfg(test)]
mod bounded_body_tests {
    use super::*;
    use futures::{StreamExt, stream};

    fn response(body: reqwest::Body, length: Option<usize>) -> reqwest::Response {
        let mut response = http::Response::builder();
        if let Some(length) = length {
            response = response.header("content-length", length);
        }
        reqwest::Response::from(response.body(body).unwrap())
    }

    #[tokio::test]
    async fn exact_byte_limit_accepts_split_utf8_and_strips_bom() {
        let bytes = "\u{feff}你好".as_bytes();
        let body = reqwest::Body::wrap_stream(stream::iter(
            bytes
                .iter()
                .map(|byte| Ok::<_, std::io::Error>(vec![*byte]))
                .collect::<Vec<_>>(),
        ));
        let text = read_bounded_text(
            response(body, None),
            bytes.len(),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(text, "你好");
    }

    #[tokio::test]
    async fn advertised_and_chunked_oversize_bodies_are_rejected() {
        for advertised in [false, true] {
            let body = if advertised {
                reqwest::Body::from("12345")
            } else {
                reqwest::Body::wrap_stream(
                    stream::iter([Ok::<_, std::io::Error>("1234"), Ok("5")])
                        .chain(stream::pending()),
                )
            };
            // In particular, chunked bodies must be rejected without waiting for EOF.
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                read_bounded_text(
                    response(body, advertised.then_some(5)),
                    4,
                    Duration::from_secs(2),
                    Duration::from_secs(2),
                ),
            )
            .await
            .unwrap();
            assert!(result.unwrap_err().to_string().contains("byte limit"));
        }
    }

    #[tokio::test]
    async fn stalled_bodies_and_empty_keepalives_time_out() {
        for prefix in [false, true] {
            let initial = stream::iter(prefix.then_some(Ok::<_, std::io::Error>("x")));
            let empty_chunks = stream::unfold((), |()| async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Some((Ok::<_, std::io::Error>(""), ()))
            });
            let body = reqwest::Body::wrap_stream(initial.chain(empty_chunks));
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                read_bounded_text(
                    response(body, None),
                    100,
                    Duration::from_millis(25),
                    Duration::from_millis(25),
                ),
            )
            .await
            .unwrap();
            assert!(result.unwrap_err().to_string().contains("timeout"));
        }
    }
}
