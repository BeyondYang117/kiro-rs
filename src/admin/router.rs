//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post},
};

use super::{
    handlers::{
        add_credential, delete_credential, force_refresh_token, get_all_credentials,
        get_credential_balance, get_load_balancing_mode, reset_failure_count,
        set_credential_disabled, set_credential_priority, set_load_balancing_mode,
    },
    middleware::{AdminState, admin_auth_middleware},
};

/// 创建 Admin API 路由
///
/// # 端点
/// - `GET /credentials` - 获取所有凭据状态
/// - `POST /credentials` - 添加新凭据
/// - `DELETE /credentials/:id` - 删除凭据
/// - `POST /credentials/:id/disabled` - 设置凭据禁用状态
/// - `POST /credentials/:id/priority` - 设置凭据优先级
/// - `POST /credentials/:id/reset` - 重置失败计数
/// - `POST /credentials/:id/refresh` - 强制刷新 Token
/// - `GET /credentials/:id/balance` - 获取凭据余额
/// - `GET /config/load-balancing` - 获取负载均衡模式
/// - `PUT /config/load-balancing` - 设置负载均衡模式
///
/// # 认证
/// 需要 Admin API Key 认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
pub fn create_admin_router(state: AdminState) -> Router {
    Router::new()
        .route("/logs", get(crate::request_log::list))
        .route("/logs/{id}", get(crate::request_log::detail))
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route("/credentials/{id}", delete(delete_credential))
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        .route(
            "/config/load-balancing",
            get(get_load_balancing_mode).put(set_load_balancing_mode),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::AdminService;
    use crate::kiro::{model::credentials::KiroCredentials, token_manager::MultiTokenManager};
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    #[tokio::test]
    async fn invalid_regions_are_rejected_before_adding_or_refreshing_credentials() {
        let manager =
            Arc::new(MultiTokenManager::new(Default::default(), vec![], None, None, true).unwrap());
        let app = create_admin_router(AdminState::new(
            "admin-key",
            AdminService::new(manager.clone(), ["ide".into()]),
        ));
        for field in ["region", "authRegion", "apiRegion"] {
            for method in ["api_key", "social", "idc"] {
                let body = serde_json::json!({
                    "authMethod":method, "kiroApiKey": if method == "api_key" { Some("ksk_test") } else { None },
                    "refreshToken":"test-refresh", field:"private-host.invalid/path?secret=value"
                });
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/credentials")
                            .header("x-api-key", "admin-key")
                            .header("content-type", "application/json")
                            .body(Body::from(body.to_string()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    StatusCode::BAD_REQUEST,
                    "{field} {method}"
                );
                let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["error"]["type"], "invalid_request");
                assert!(value["error"]["message"].as_str().unwrap().contains(field));
                assert!(!String::from_utf8_lossy(&bytes).contains("private-host"));
                assert_eq!(manager.total_count(), 0);
            }
        }
    }

    #[tokio::test]
    async fn extractor_errors_are_json_and_authentication_runs_before_parsing() {
        let manager =
            Arc::new(MultiTokenManager::new(Default::default(), vec![], None, None, true).unwrap());
        let app = create_admin_router(AdminState::new(
            "admin-key",
            AdminService::new(manager.clone(), ["ide".into()]),
        ))
        .layer(axum::extract::DefaultBodyLimit::max(64));
        for (path, content_type, body, expected) in [
            ("/credentials", "application/json", "{", 400),
            (
                "/credentials",
                "application/json",
                r#"{"authMethod":123}"#,
                422,
            ),
            ("/credentials", "text/plain", "{}", 415),
            (
                "/credentials/1/disabled",
                "application/json",
                r#"{"disabled":"private-invalid-value"}"#,
                422,
            ),
            (
                "/credentials/private-path-value/priority",
                "application/json",
                r#"{"priority":1}"#,
                400,
            ),
            (
                "/credentials/18446744073709551616/disabled",
                "application/json",
                r#"{"disabled":true}"#,
                400,
            ),
            (
                "/credentials/1/balance?force=private-query-value",
                "application/json",
                "",
                400,
            ),
            ("/credentials", "application/json", &"x".repeat(65), 413),
        ] {
            let method = if path.contains("/balance") || path.starts_with("/logs") {
                "GET"
            } else {
                "POST"
            };
            for authenticated in [false, true] {
                let mut request = Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", content_type);
                if authenticated {
                    request = request.header("x-api-key", "admin-key");
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::from(body.to_owned())).unwrap())
                    .await
                    .unwrap();
                assert_eq!(
                    response.status().as_u16(),
                    if authenticated { expected } else { 401 },
                    "{path}"
                );
                assert_eq!(response.headers()["content-type"], "application/json");
                let bytes = to_bytes(response.into_body(), 4096).await.unwrap();
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    value["error"]["type"],
                    if authenticated {
                        "invalid_request"
                    } else {
                        "authentication_error"
                    }
                );
                assert!(
                    !String::from_utf8(bytes.to_vec())
                        .unwrap()
                        .contains("private-")
                );
            }
        }
        assert_eq!(manager.total_count(), 0);
    }

    #[tokio::test]
    async fn credential_capacity_metrics_require_admin_auth_and_include_live_account_state() {
        let manager = Arc::new(
            MultiTokenManager::new(
                serde_json::from_value(serde_json::json!({"concurrency":{
                "maxInFlight":2,"maxInFlightPerCredential":1,"adaptiveConcurrency":false}}))
                .unwrap(),
                (1..=2)
                    .map(|id| KiroCredentials {
                        id: Some(id),
                        kiro_api_key: Some(format!("private-account-key-{id}")),
                        ..Default::default()
                    })
                    .collect(),
                None,
                None,
                true,
            )
            .unwrap(),
        );
        let active = manager.acquire_context(None).await.unwrap();
        manager.report_rate_limited(2, None);
        let mut pending = Box::pin(manager.acquire_context(None));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        let app = create_admin_router(AdminState::new(
            "admin-test-key",
            AdminService::new(manager.clone(), ["ide".into()]),
        ));
        for key in [None, Some("wrong-key")] {
            let mut request = Request::builder().uri("/credentials");
            if let Some(key) = key {
                request = request.header("x-api-key", key);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/credentials")
                    .header("x-api-key", "admin-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["available"], 2);
        assert_eq!(value["concurrency"]["maxInFlight"], 2);
        assert_eq!(value["concurrency"]["inFlight"], 1);
        assert_eq!(value["concurrency"]["globalAvailableSlots"], 1);
        assert_eq!(value["concurrency"]["availableSlots"], 0);
        assert_eq!(value["concurrency"]["availableOpusSlots"], 0);
        assert_eq!(value["concurrency"]["waiting"], 1);
        assert_eq!(value["concurrency"]["waitingOpus"], 0);
        assert_eq!(value["concurrency"]["coolingCredentials"], 1);
        assert_eq!(value["concurrency"]["saturatedCredentials"], 1);
        assert_eq!(value["concurrency"]["rateLimitedTotal"], 1);
        assert_eq!(value["concurrency"]["adaptiveConcurrency"], false);
        assert_eq!(value["concurrency"]["recoveringCredentials"], 0);
        assert_eq!(value["concurrency"]["probingCredentials"], 0);
        assert_eq!(value["concurrency"]["adaptiveReductionTotal"], 0);
        assert_eq!(value["credentials"][0]["inFlight"], 1);
        assert_eq!(value["credentials"][0]["effectiveConcurrencyLimit"], 1);
        assert!(value["credentials"][0].get("adaptiveProbeTarget").is_none());
        assert!(
            value["credentials"][1]["cooldownRemainingMs"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(
            !String::from_utf8(body.to_vec())
                .unwrap()
                .contains("private-account-key")
        );
        drop((pending, active));
        assert_eq!(manager.snapshot().concurrency.cancelled_wait_total, 1);
    }

    #[tokio::test]
    async fn credential_status_exposes_adaptive_probe_and_reduction_metrics() {
        let manager = Arc::new(
            MultiTokenManager::new(
                serde_json::from_value(serde_json::json!({"concurrency": {
                    "adaptiveConcurrency": true, "maxInFlightPerCredential": 4
                }}))
                .unwrap(),
                vec![KiroCredentials {
                    id: Some(1),
                    kiro_api_key: Some("adaptive-admin-test".into()),
                    ..Default::default()
                }],
                None,
                None,
                true,
            )
            .unwrap(),
        );
        manager.report_rate_limited(1, None);
        let app = create_admin_router(AdminState::new(
            "admin-test-key",
            AdminService::new(manager, ["ide".into()]),
        ));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/credentials")
                    .header("x-api-key", "admin-test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["concurrency"]["probingCredentials"], 1);
        assert_eq!(value["concurrency"]["adaptiveReductionTotal"], 1);
        assert_eq!(value["credentials"][0]["effectiveConcurrencyLimit"], 1);
        assert_eq!(value["credentials"][0]["adaptiveProbeTarget"], 2);
    }
}
