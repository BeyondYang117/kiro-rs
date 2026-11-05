use super::*;
use futures::{StreamExt, stream};

fn response(status: u16, body: reqwest::Body) -> reqwest::Response {
    reqwest::Response::from(http::Response::builder().status(status).body(body).unwrap())
}

fn credentials() -> KiroCredentials {
    KiroCredentials {
        id: Some(7),
        access_token: Some("original-access".into()),
        refresh_token: Some("original-refresh".into()),
        profile_arn: Some("original-profile".into()),
        expires_at: Some((Utc::now() + Duration::hours(1)).to_rfc3339()),
        ..Default::default()
    }
}

#[tokio::test]
async fn refresh_errors_preserve_status_and_retry_after_for_shared_backoff() {
    for status in [429, 503] {
        let mut response = response(status, "busy".into());
        response
            .headers_mut()
            .insert("retry-after", "600".parse().unwrap());
        let error =
            read_refresh_response::<RefreshResponse>(response, &Config::default(), "Social")
                .await
                .unwrap_err();
        let service = error
            .downcast_ref::<protection::RefreshServiceError>()
            .unwrap();
        assert_eq!(service.status.as_u16(), status);
        assert_eq!(service.retry_after, Some(StdDuration::from_secs(600)));
    }
}

#[tokio::test]
async fn standalone_account_calls_validate_regions_before_network_or_token_processing() {
    for field in ["region", "authRegion", "apiRegion"] {
        let invalid = serde_json::json!({field:"private.invalid/"});
        let invalid_config: Config = serde_json::from_value(invalid.clone()).unwrap();
        let invalid_credentials: KiroCredentials = serde_json::from_value(invalid).unwrap();
        let config = Config::default();
        let credential = credentials();
        for (config, credential) in [
            (&invalid_config, &credential),
            (&config, &invalid_credentials),
        ] {
            let error = refresh_token(credential, config, None).await.unwrap_err();
            assert!(error.is::<crate::common::region::InvalidRegion>());
            let error = get_usage_limits(credential, config, "private-token", None)
                .await
                .unwrap_err();
            assert!(error.is::<crate::common::region::InvalidRegion>());
        }
    }
}

#[tokio::test]
async fn oauth_redirect_bodies_never_replace_tokens_or_invalidate_credentials() {
    for status in [301, 302, 303, 307, 308] {
        for method in ["Social", "IdC"] {
            let error = read_refresh_response::<RefreshResponse>(
                response(
                    status,
                    r#"{"accessToken":"attacker-token","expiresIn":3600,"error":"invalid_grant"}"#
                        .into(),
                ),
                &Config::default(),
                method,
            )
            .await
            .unwrap_err();
            assert!(!error.is::<RefreshTokenInvalidError>());
            assert!(error.to_string().contains(&status.to_string()));
            assert!(!error.to_string().contains("attacker-token"));
        }
    }
}

#[tokio::test]
async fn social_and_idc_responses_rotate_credentials_and_expiration() {
    let config = Config::default();
    let original = credentials();
    let before = Utc::now();
    for method in ["Social", "IdC"] {
        let body = serde_json::json!({
            "accessToken": "new-access", "refreshToken": "new-refresh",
            "profileArn": "new-profile", "expiresIn": 3600
        })
        .to_string();
        // Both OAuth endpoints share the same fields, but retain their own model.
        let (access, refresh, profile, expiration) = if method == "Social" {
            let data: RefreshResponse =
                read_refresh_response(response(200, body.into()), &config, method)
                    .await
                    .unwrap();
            (
                data.access_token,
                data.refresh_token,
                data.profile_arn,
                data.expires_in,
            )
        } else {
            let data: IdcRefreshResponse =
                read_refresh_response(response(200, body.into()), &config, method)
                    .await
                    .unwrap();
            (
                data.access_token,
                data.refresh_token,
                data.profile_arn,
                data.expires_in,
            )
        };
        let updated =
            apply_refresh_response(&original, access, refresh, profile, expiration).unwrap();
        assert_eq!(updated.id, original.id);
        assert_eq!(updated.access_token.as_deref(), Some("new-access"));
        assert_eq!(updated.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(updated.profile_arn.as_deref(), Some("new-profile"));
        let expiration =
            DateTime::parse_from_rfc3339(updated.expires_at.as_ref().unwrap()).unwrap();
        assert!(expiration >= before + Duration::hours(1));
        assert!(expiration <= Utc::now() + Duration::hours(1));
    }
    assert_eq!(original.access_token.as_deref(), Some("original-access"));
}

#[test]
fn invalid_refresh_fields_are_rejected_without_changing_original() {
    let original = credentials();
    let snapshot = serde_json::to_value(&original).unwrap();
    for (access, refresh, expiration) in [
        ("", None, Some(3600)),
        (" \t", None, Some(3600)),
        ("new", Some(""), Some(3600)),
        ("new", Some(" \t"), Some(3600)),
        ("new", None, Some(0)),
        ("new", None, Some(-1)),
        ("new", None, Some(i64::MAX)),
        // Fits chrono::Duration but overflows chrono::DateTime.
        ("new", None, Some(i64::MAX / 1000)),
    ] {
        assert!(
            apply_refresh_response(
                &original,
                access.into(),
                refresh.map(String::from),
                Some("new-profile".into()),
                expiration,
            )
            .is_err()
        );
        assert_eq!(serde_json::to_value(&original).unwrap(), snapshot);
    }
}

#[test]
fn omitted_optional_fields_preserve_existing_credential_metadata() {
    let original = credentials();
    let updated = apply_refresh_response(&original, "new".into(), None, None, None).unwrap();
    assert_eq!(updated.access_token.as_deref(), Some("new"));
    assert_eq!(updated.refresh_token, original.refresh_token);
    assert_eq!(updated.expires_at, original.expires_at);
    assert_eq!(updated.profile_arn, original.profile_arn);
}

#[tokio::test]
async fn permanent_refresh_failure_requires_oauth_error_field_and_bad_request() {
    let config = Config::default();
    for method in ["Social", "IdC"] {
        for (status, body, permanent) in [
            (400, r#"{"error":"invalid_grant"}"#, true),
            (
                400,
                r#"{"error":"invalid_grant","error_description":"token revoked"}"#,
                true,
            ),
            (
                400,
                r#"{"error":"invalid_grant","error_description":"Invalid refresh token provided"}"#,
                true,
            ),
            (500, r#"{"error":"invalid_grant"}"#, false),
            (429, r#"{"error":"invalid_grant"}"#, false),
            (
                400,
                r#"{"error":"invalid_client","description":"invalid_grant: Invalid refresh token provided"}"#,
                false,
            ),
            (
                400,
                r#"{"error":"invalid_client","values":["invalid_grant","Invalid refresh token provided"]}"#,
                false,
            ),
            (
                400,
                r#"not JSON "invalid_grant" Invalid refresh token provided"#,
                false,
            ),
        ] {
            let error = read_refresh_response::<RefreshResponse>(
                response(status, body.into()),
                &config,
                method,
            )
            .await
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<RefreshTokenInvalidError>().is_some(),
                permanent,
                "{method}: {status} {body}",
            );
        }
    }
}

#[tokio::test]
async fn account_body_limits_cover_known_and_chunked_bodies_and_configured_cap() {
    for configured_limit in [1024, 2 * 1024 * 1024] {
        let config: Config = serde_json::from_value(serde_json::json!({
            "requestLimits": {"maxResponseBytes": configured_limit}
        }))
        .unwrap();
        let limit = configured_limit.min(1024 * 1024);
        for chunked in [false, true] {
            let data = "x".repeat(limit + 1);
            let body = if chunked {
                reqwest::Body::wrap_stream(
                    stream::once(async move { Ok::<_, std::io::Error>(data) })
                        .chain(stream::pending()),
                )
            } else {
                data.into()
            };
            let mut response = response(200, body);
            if !chunked {
                response
                    .headers_mut()
                    .insert("content-length", (limit + 1).into());
            }
            let result = tokio::time::timeout(
                StdDuration::from_secs(1),
                read_account_response(response, &config),
            )
            .await
            .unwrap();
            assert!(result.unwrap_err().to_string().contains("byte limit"));
        }
    }
}

#[tokio::test]
async fn oversized_error_bodies_preserve_status_without_disabling_credentials() {
    let config = Config::default();
    for status in [400, 401, 429, 503] {
        let body = format!(
            r#"{{"error":"invalid_grant","padding":"{}"}}"#,
            "x".repeat(64 * 1024)
        );
        let error = read_refresh_response::<RefreshResponse>(
            response(status, body.into()),
            &config,
            "Social",
        )
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<RefreshTokenInvalidError>().is_none());
        let message = error.to_string();
        assert!(message.contains(&status.to_string()));
        assert!(message.contains("error body unavailable"));
        assert!(message.len() < 1024);
    }
}

#[tokio::test]
async fn refresh_errors_redact_upstream_credentials_before_they_can_be_logged() {
    let body = r#"{"error":"invalid_client","accessToken":"private-access","kiroApiKey":"private-key","clientSecret":"private-secret"}"#;
    for method in ["Social", "IdC"] {
        let error = read_refresh_response::<RefreshResponse>(
            response(401, body.into()),
            &Config::default(),
            method,
        )
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("401"));
        assert!(!message.contains("private-"));
        assert!(message.contains("invalid_client"));
    }
}

#[tokio::test]
async fn account_idle_timeout_releases_the_upstream_body() {
    struct OnDrop(Arc<AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = OnDrop(dropped.clone());
    let pending = stream::once(async move {
        let _guard = guard;
        std::future::pending::<Result<String, std::io::Error>>().await
    });
    let body = reqwest::Body::wrap_stream(
        stream::once(async { Ok::<_, std::io::Error>("{".to_owned()) }).chain(pending),
    );
    let config: Config = serde_json::from_value(serde_json::json!({
        "requestLimits": {"idleTimeoutSeconds": 1}
    }))
    .unwrap();
    let error = tokio::time::timeout(
        StdDuration::from_secs(3),
        read_account_response(response(200, body), &config),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("timeout"));
    assert!(dropped.load(Ordering::SeqCst));
}
