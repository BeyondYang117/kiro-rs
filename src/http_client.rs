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
        let chunk = chunk.context("Failed to read upstream response body")?;
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
