//! 公共认证工具函数

use axum::{
    body::Body,
    http::{Request, header},
};
use subtle::ConstantTimeEq;

/// A key must be usable as one unambiguous HTTP header value.
pub(crate) fn is_valid_api_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b',')
}

fn single_header(request: &Request<Body>, name: header::HeaderName) -> Option<Option<&str>> {
    let mut values = request.headers().get_all(name).iter();
    let value = values.next();
    if values.next().is_some() {
        return None;
    }
    match value {
        Some(value) => Some(Some(value.to_str().ok()?)),
        None => Some(None),
    }
}

/// 从请求中提取 API Key
///
/// 支持两种认证方式：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
fn extract_api_key(request: &Request<Body>) -> Option<&str> {
    let api_key = single_header(request, header::HeaderName::from_static("x-api-key"))?;
    let authorization = single_header(request, header::AUTHORIZATION)?;
    let bearer = match authorization {
        Some(value) => {
            let (scheme, key) = value.split_once(' ')?;
            if !scheme.eq_ignore_ascii_case("Bearer") {
                return None;
            }
            // RFC 9110 credentials allow one or more SP characters after the scheme.
            let key = key.trim_start_matches(' ');
            if !is_valid_api_key(key) {
                return None;
            }
            Some(key)
        }
        None => None,
    };
    if let Some(key) = api_key {
        if !is_valid_api_key(key) {
            return None;
        }
        // Accept both supported headers only when they identify the same key.
        if let Some(bearer) = bearer
            && !constant_time_eq(key, bearer)
        {
            return None;
        }
        Some(key)
    } else {
        bearer
    }
}

pub(crate) fn authenticate(request: &Request<Body>, expected: &str) -> bool {
    is_valid_api_key(expected)
        && extract_api_key(request).is_some_and(|key| constant_time_eq(key, expected))
}

/// 对所有候选密钥执行比较，不因匹配到某个密钥而提前结束。
pub(crate) fn authenticate_any(request: &Request<Body>, expected: &[String]) -> bool {
    let Some(key) = extract_api_key(request) else {
        return false;
    };
    expected.iter().fold(false, |matched, candidate| {
        matched | (is_valid_api_key(candidate) && constant_time_eq(key, candidate))
    })
}

/// 常量时间字符串比较，防止时序攻击
///
/// 对等长字符串执行常量时间比较，不在首个不同字节处提前返回。
///
/// 使用经过安全审计的 `subtle` crate 实现
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        admin::{AdminService, AdminState, create_admin_router},
        kiro::token_manager::MultiTokenManager,
        model::config::Config,
    };
    use axum::{Router, body::to_bytes, http::HeaderValue};
    use std::sync::Arc;
    use tower::ServiceExt;

    fn app(key: &str) -> Router {
        app_with_keys(key, key)
    }

    fn app_with_keys(api_key: &str, admin_key: &str) -> Router {
        app_with_client_keys(&[api_key], admin_key)
    }

    fn app_with_client_keys(api_keys: &[&str], admin_key: &str) -> Router {
        let manager =
            Arc::new(MultiTokenManager::new(Config::default(), vec![], None, None, true).unwrap());
        crate::anthropic::create_router_with_provider(
            api_keys.iter().copied(),
            None,
            true,
            true,
            false,
        )
        .nest(
            "/api/admin",
            create_admin_router(AdminState::new(
                admin_key,
                AdminService::new(manager, ["ide".into()]),
            )),
        )
    }

    #[tokio::test]
    async fn multiple_client_keys_work_on_all_routes_and_keep_admin_separate() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "apiKey":"legacy-key", "apiKeys":["alice-key", "bob-key"]
        }))
        .unwrap();
        let keys = config.client_api_keys().unwrap();
        let keys: Vec<_> = keys.iter().map(String::as_str).collect();
        let app = app_with_client_keys(&keys, "admin-key");
        for prefix in ["/v1", "/cc/v1"] {
            for (method, suffix, accepted_status) in [
                ("GET", "/models", 200),
                ("GET", "/models/claude-opus-5-5", 200),
                ("POST", "/messages", 503),
                ("POST", "/messages/count_tokens", 200),
            ] {
                for key in [
                    "legacy-key",
                    "alice-key",
                    "bob-key",
                    "admin-key",
                    "unknown-key",
                ] {
                    for header in ["x-api-key", "authorization"] {
                        let value = if header == "authorization" {
                            format!("Bearer {key}")
                        } else {
                            key.to_string()
                        };
                        let request = Request::builder()
                            .method(method)
                            .uri(format!("{prefix}{suffix}"))
                            .header(header, value)
                            .header("content-type", "application/json")
                            .body(Body::from(r#"{"model":"claude-opus-5-5","max_tokens":32,"messages":[{"role":"user","content":"hi"}]}"#))
                            .unwrap();
                        let response = app.clone().oneshot(request).await.unwrap();
                        assert_eq!(
                            response.status().as_u16(),
                            if keys.contains(&key) {
                                accepted_status
                            } else {
                                401
                            },
                            "{method} {prefix}{suffix} {key} {header}"
                        );
                    }
                }
            }
        }
        for key in ["legacy-key", "alice-key", "bob-key", "admin-key"] {
            let request = Request::builder()
                .uri("/api/admin/credentials")
                .header("x-api-key", key)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                app.clone()
                    .oneshot(request)
                    .await
                    .unwrap()
                    .status()
                    .as_u16(),
                if key == "admin-key" { 200 } else { 401 }
            );
        }
        for path in ["/v1/models", "/cc/v1/models"] {
            let request = Request::builder()
                .uri(path)
                .header("x-api-key", "alice-key")
                .header("authorization", "Bearer bob-key")
                .body(Body::empty())
                .unwrap();
            assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 401);
        }

        // Removing one key on restart revokes only that key.
        let app = app_with_client_keys(&["alice-key"], "admin-key");
        for (key, status) in [("alice-key", 200), ("bob-key", 401)] {
            let request = Request::builder()
                .uri("/v1/models")
                .header("x-api-key", key)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                app.clone()
                    .oneshot(request)
                    .await
                    .unwrap()
                    .status()
                    .as_u16(),
                status
            );
        }
    }

    #[tokio::test]
    async fn empty_client_key_set_rejects_all_requests() {
        let app = app_with_client_keys(&[], "admin-key");
        for path in ["/v1/models", "/cc/v1/models"] {
            let request = Request::builder()
                .uri(path)
                .header("x-api-key", "any-key")
                .body(Body::empty())
                .unwrap();
            assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 401);
        }
    }

    #[tokio::test]
    async fn all_api_groups_share_strict_authentication_rules() {
        let app = app("valid-key");
        type Headers<'a> = &'a [(&'a str, &'a [u8])];
        let cases: &[(Headers<'_>, bool)] = &[
            (&[], false),
            (&[("x-api-key", b"valid-key")], true),
            (&[("authorization", b"Bearer valid-key")], true),
            (&[("authorization", b"bEaReR   valid-key")], true),
            (
                &[
                    ("x-api-key", b"valid-key"),
                    ("authorization", b"Bearer valid-key"),
                ],
                true,
            ),
            (&[("x-api-key", b"")], false),
            (&[("x-api-key", b" ")], false),
            (&[("x-api-key", b"valid-key ")], false),
            (&[("x-api-key", b"invalid-key")], false),
            (&[("x-api-key", b"valid-key,valid-key")], false),
            (
                &[("x-api-key", b"valid-key"), ("x-api-key", b"valid-key")],
                false,
            ),
            (
                &[("x-api-key", b"wrong"), ("x-api-key", b"valid-key")],
                false,
            ),
            (
                &[
                    ("authorization", b"Bearer valid-key"),
                    ("authorization", b"Bearer valid-key"),
                ],
                false,
            ),
            (
                &[
                    ("x-api-key", b"wrong"),
                    ("authorization", b"Bearer valid-key"),
                ],
                false,
            ),
            (
                &[
                    ("x-api-key", b"valid-key"),
                    ("authorization", b"Bearer wrong"),
                ],
                false,
            ),
            (
                &[
                    ("x-api-key", b"valid-key"),
                    ("authorization", b"Basic valid-key"),
                ],
                false,
            ),
            (
                &[
                    ("x-api-key", b"\xff"),
                    ("authorization", b"Bearer valid-key"),
                ],
                false,
            ),
            (
                &[("x-api-key", b"valid-key"), ("authorization", b"\xff")],
                false,
            ),
            (&[("authorization", b"Bearer")], false),
            (&[("authorization", b"Bearer ")], false),
            (&[("authorization", b"Bearer\tvalid-key")], false),
            (&[("authorization", b"Bearer valid-key extra")], false),
            (
                &[("authorization", b"Bearer valid-key, Bearer valid-key")],
                false,
            ),
        ];
        for path in ["/v1/models", "/cc/v1/models", "/api/admin/credentials"] {
            for (headers, accepted) in cases {
                let mut request = Request::builder().uri(path).body(Body::empty()).unwrap();
                for (name, value) in *headers {
                    request.headers_mut().append(
                        header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                        HeaderValue::from_bytes(value).unwrap(),
                    );
                }
                let response = app.clone().oneshot(request).await.unwrap();
                assert_eq!(response.headers()["cache-control"], "no-store");
                assert_eq!(response.headers()["x-content-type-options"], "nosniff");
                assert_eq!(
                    response.status().as_u16(),
                    if *accepted { 200 } else { 401 },
                    "{path} {headers:?}"
                );
                if !accepted {
                    assert_eq!(response.headers()["www-authenticate"], "Bearer");
                    let body: serde_json::Value = serde_json::from_slice(
                        &to_bytes(response.into_body(), 4096).await.unwrap(),
                    )
                    .unwrap();
                    assert_eq!(body["error"]["type"], "authentication_error");
                    assert!(!body.to_string().contains("valid-key"));
                }
            }
        }
    }

    #[tokio::test]
    async fn invalid_configured_keys_never_authorize_requests() {
        for key in ["", " ", "\t", "key with space", "key,joined", "密钥"] {
            assert!(!is_valid_api_key(key));
            let app = app(key);
            for path in ["/v1/models", "/cc/v1/models", "/api/admin/credentials"] {
                let request = Request::builder()
                    .uri(path)
                    .header("x-api-key", key)
                    .body(Body::empty())
                    .unwrap();
                assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 401);
            }
        }
    }

    #[tokio::test]
    async fn query_parameters_and_cookies_do_not_authenticate() {
        let app = app("valid-key");
        for path in ["/v1/models", "/cc/v1/models", "/api/admin/credentials"] {
            let request = Request::builder()
                .uri(format!("{path}?api_key=valid-key"))
                .header("cookie", "api_key=valid-key")
                .body(Body::empty())
                .unwrap();
            assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 401);
        }
    }

    #[tokio::test]
    async fn admin_and_client_keys_authorize_only_their_own_route_groups() {
        let app = app_with_keys("client-key", "admin-key");
        for (path, expected) in [
            ("/v1/models", "client-key"),
            ("/cc/v1/models", "client-key"),
            ("/api/admin/credentials", "admin-key"),
        ] {
            for key in ["client-key", "admin-key"] {
                let request = Request::builder()
                    .uri(path)
                    .header("authorization", format!("Bearer {key}"))
                    .body(Body::empty())
                    .unwrap();
                assert_eq!(
                    app.clone()
                        .oneshot(request)
                        .await
                        .unwrap()
                        .status()
                        .as_u16(),
                    if key == expected { 200 } else { 401 }
                );
            }
        }
    }
}
