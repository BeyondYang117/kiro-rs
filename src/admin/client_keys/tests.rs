use super::*;
use crate::admin::{AdminService, create_admin_router};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use std::{path::PathBuf, sync::Arc};
use tower::ServiceExt;

const LEGACY: &str = "private-legacy-client-secret";
const OTHER: &str = "private-other-client-secret";
const ADMIN: &str = "private-admin-secret";

struct Fixture {
    directory: PathBuf,
    manager: Arc<MultiTokenManager>,
    app: Router,
}

impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("kiro-client-keys-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(
            directory.join("config.json"),
            json!({
                "apiKey":LEGACY, "apiKeys":[LEGACY,OTHER,LEGACY], "adminApiKey":ADMIN,
                "extension":{"keep":"private-extension"}, "concurrency":{"extension":42}
            })
            .to_string(),
        )
        .unwrap();
        let config = Config::load(directory.join("config.json")).unwrap();
        let manager =
            Arc::new(MultiTokenManager::new(config.clone(), vec![], None, None, true).unwrap());
        let app = Self::router(config, manager.clone());
        Self {
            directory,
            manager,
            app,
        }
    }

    fn router(config: Config, manager: Arc<MultiTokenManager>) -> Router {
        crate::anthropic::create_router_with_provider(
            config.client_api_keys().unwrap(),
            None,
            true,
            true,
            false,
        )
        .nest(
            "/api/admin",
            create_admin_router(AdminState::new(
                ADMIN,
                AdminService::new(manager, ["ide".into()]),
            )),
        )
    }

    fn path(&self) -> PathBuf {
        self.directory.join("config.json")
    }
    fn document(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.path()).unwrap()).unwrap()
    }
    async fn call(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        call(&self.app, method, path, body.to_string(), Some(ADMIN)).await
    }
    async fn state(&self) -> Value {
        let (status, state) = self
            .call("GET", "/api/admin/client-keys", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        state
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: String,
    key: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("x-api-key", key);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    let status = response.status();
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await.unwrap()).unwrap();
    for secret in [LEGACY, OTHER, ADMIN, "private-extension"] {
        assert!(
            !body.to_string().contains(secret),
            "response leaked an existing secret"
        );
    }
    (status, body)
}

#[tokio::test]
async fn key_routes_require_admin_authentication_before_parsing_and_lists_are_masked() {
    let fixture = Fixture::new();
    for (method, path) in [
        ("GET", "/api/admin/client-keys".into()),
        ("POST", "/api/admin/client-keys".into()),
        (
            "DELETE",
            format!("/api/admin/client-keys/{}", key_id(LEGACY)),
        ),
    ] {
        for key in [None, Some(LEGACY), Some(OTHER), Some("wrong-key")] {
            assert_eq!(
                call(&fixture.app, method, &path, "invalid json".into(), key)
                    .await
                    .0,
                StatusCode::UNAUTHORIZED
            );
        }
    }
    let state = fixture.state().await;
    assert_eq!(state["keys"].as_array().unwrap().len(), 2);
    assert_eq!(state["keys"][0]["legacy"], true);
    assert_eq!(state["keys"][0]["id"], key_id(LEGACY));
    assert_eq!(state["keys"][0]["active"], true);
    assert_eq!(state["restartRequired"], false);
    assert_eq!(state["canSave"], true);
    assert_eq!(
        fixture
            .call(
                "POST",
                "/api/admin/client-keys",
                json!({"revision":state["revision"],"key":42})
            )
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn creation_is_persisted_shown_once_and_authentication_changes_only_after_restart() {
    let fixture = Fixture::new();
    let before = fixture.document();
    #[cfg(unix)]
    let before_permissions = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(fixture.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    let state = fixture.state().await;
    let (status, created) = fixture
        .call(
            "POST",
            "/api/admin/client-keys",
            json!({"revision":state["revision"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let key = created["key"].as_str().unwrap();
    assert!(key.starts_with("sk-"));
    assert_eq!(key.len(), 67);
    assert!(crate::common::auth::is_valid_api_key(key));
    let new_item = created["state"]["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == key_id(key))
        .unwrap();
    assert_eq!(new_item["configured"], true);
    assert_eq!(new_item["active"], false);
    assert_eq!(created["state"]["restartRequired"], true);
    let saved = fixture.document();
    for field in ["apiKey", "adminApiKey", "extension", "concurrency"] {
        assert_eq!(saved[field], before[field]);
    }
    assert!(saved["apiKeys"].as_array().unwrap().contains(&json!(key)));
    assert!(!fixture.state().await.to_string().contains(key));
    assert_eq!(
        call(&fixture.app, "GET", "/v1/models", String::new(), Some(key))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    let config = Config::load(fixture.path()).unwrap();
    let manager =
        Arc::new(MultiTokenManager::new(config.clone(), vec![], None, None, true).unwrap());
    assert!(!read(&manager).unwrap().restart_required);
    let restarted = Fixture::router(config, manager);
    assert_eq!(
        call(&restarted, "GET", "/cc/v1/models", String::new(), Some(key))
            .await
            .0,
        StatusCode::OK
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(fixture.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            before_permissions
        );
    }
}

#[tokio::test]
async fn deletion_removes_legacy_and_array_duplicates_and_cannot_remove_last_key() {
    let fixture = Fixture::new();
    let state = fixture.state().await;
    let (status, deleted) = fixture
        .call(
            "DELETE",
            &format!("/api/admin/client-keys/{}", key_id(LEGACY)),
            json!({"revision":state["revision"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let saved = fixture.document();
    assert!(saved.get("apiKey").is_none());
    assert_eq!(saved["apiKeys"], json!([OTHER]));
    let pending = deleted["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == key_id(LEGACY))
        .unwrap();
    assert_eq!(pending["configured"], false);
    assert_eq!(pending["active"], true);
    assert_eq!(
        call(
            &fixture.app,
            "GET",
            "/v1/models",
            String::new(),
            Some(LEGACY)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        fixture
            .call(
                "DELETE",
                &format!("/api/admin/client-keys/{}", key_id(OTHER)),
                json!({"revision":deleted["revision"]})
            )
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fixture.document(), saved);
    let config = Config::load(fixture.path()).unwrap();
    let manager =
        Arc::new(MultiTokenManager::new(config.clone(), vec![], None, None, true).unwrap());
    let restarted = Fixture::router(config, manager);
    assert_eq!(
        call(&restarted, "GET", "/v1/models", String::new(), Some(LEGACY))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&restarted, "GET", "/v1/models", String::new(), Some(OTHER))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn invalid_duplicate_and_admin_keys_are_rejected_without_changes_or_secret_echoes() {
    let fixture = Fixture::new();
    let state = fixture.state().await;
    let original = std::fs::read(fixture.path()).unwrap();
    for key in [
        "",
        "private-invalid secret",
        "private,joined",
        "非ASCII私钥值",
        LEGACY,
        OTHER,
        ADMIN,
    ] {
        let (status, body) = fixture
            .call(
                "POST",
                "/api/admin/client-keys",
                json!({"revision":state["revision"],"key":key}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        if key.len() > 1 {
            assert!(!body.to_string().contains(key));
        }
        assert_eq!(std::fs::read(fixture.path()).unwrap(), original);
    }
    let (status, created) = fixture
        .call(
            "POST",
            "/api/admin/client-keys",
            json!({"revision":state["revision"],"key":"short"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["key"], "short");
    assert!(!created["state"].to_string().contains("short"));
}

#[tokio::test]
async fn key_conflicts_do_not_overwrite_concurrent_edits_and_settings_preserve_keys() {
    let fixture = Fixture::new();
    let state = fixture.state().await;
    let (_, settings) = fixture
        .call("GET", "/api/admin/settings", Value::Null)
        .await;
    // Unrelated file edits do not invalidate a key revision and must be preserved.
    fixture
        .manager
        .set_load_balancing_mode("priority".into())
        .unwrap();
    let (status, created) = fixture
        .call(
            "POST",
            "/api/admin/client-keys",
            json!({"revision":state["revision"],"key":"new-manual-secret"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fixture.document()["loadBalancingMode"], "priority");
    assert_eq!(
        fixture
            .call(
                "POST",
                "/api/admin/client-keys",
                json!({"revision":state["revision"]})
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        fixture
            .call(
                "DELETE",
                &format!("/api/admin/client-keys/{}", key_id(LEGACY)),
                json!({"revision":state["revision"]})
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut document = fixture.document();
    document["apiKeys"]
        .as_array_mut()
        .unwrap()
        .push(json!("external-key"));
    std::fs::write(fixture.path(), document.to_string()).unwrap();
    assert_eq!(
        fixture
            .call(
                "POST",
                "/api/admin/client-keys",
                json!({"revision":created["state"]["revision"]})
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    // Saving the current settings merges into the latest document without losing keys.
    let (_, current) = fixture
        .call("GET", "/api/admin/settings", Value::Null)
        .await;
    assert_ne!(current["revision"], settings["revision"]);
    let mut changed = current["settings"].clone();
    changed["forceStream"] = json!(true);
    assert_eq!(
        fixture
            .call(
                "PUT",
                "/api/admin/settings",
                json!({"revision":current["revision"],"settings":changed})
            )
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(fixture.document()["apiKeys"], document["apiKeys"]);
}

#[test]
fn unknown_config_path_is_read_only_and_invalid_files_return_sanitized_errors() {
    let config: Config = serde_json::from_value(json!({"apiKey":"short"})).unwrap();
    let manager = MultiTokenManager::new(config, vec![], None, None, true).unwrap();
    let state = read(&manager).unwrap();
    assert!(!state.can_save);
    assert_eq!(state.keys[0].masked_key, "••••••••");
    assert!(
        add(
            &manager,
            AddKeyRequest {
                revision: state.revision,
                key: None
            }
        )
        .is_err()
    );
    let fixture = Fixture::new();
    std::fs::write(
        fixture.path(),
        json!({"apiKeys":"private-malformed-secret"}).to_string(),
    )
    .unwrap();
    let error = read(&fixture.manager).err().unwrap();
    assert_eq!(error.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!error.to_string().contains("private-malformed-secret"));
}
