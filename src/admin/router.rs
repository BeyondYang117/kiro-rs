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
    async fn credential_capacity_metrics_require_admin_auth_and_include_live_account_state() {
        let manager = Arc::new(
            MultiTokenManager::new(
                serde_json::from_value(serde_json::json!({"concurrency":{
                "maxInFlight":2,"maxInFlightPerCredential":1}}))
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
        assert_eq!(value["concurrency"]["waiting"], 1);
        assert_eq!(value["concurrency"]["coolingCredentials"], 1);
        assert_eq!(value["concurrency"]["saturatedCredentials"], 1);
        assert_eq!(value["concurrency"]["rateLimitedTotal"], 1);
        assert_eq!(value["concurrency"]["adaptiveConcurrency"], false);
        assert_eq!(value["concurrency"]["recoveringCredentials"], 0);
        assert_eq!(value["credentials"][0]["inFlight"], 1);
        assert_eq!(value["credentials"][0]["effectiveConcurrencyLimit"], 1);
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
}
