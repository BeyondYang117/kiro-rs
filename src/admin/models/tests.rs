use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn catalog() -> ModelCatalog {
    ModelCatalog {
        models: vec![],
        default_model_id: None,
        region: "us-east-1".into(),
    }
}

async fn seed(cache: &CatalogCache, key: &str) {
    cache
        .load(1, key.into(), false, || async {
            Ok((key.into(), catalog()))
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn cache_is_scoped_bounded_and_preserves_stale_records_without_extending_freshness() {
    let cache = CatalogCache::default();
    seed(&cache, "account-a").await;
    let cached = cache
        .load(1, "account-a".into(), false, || async {
            panic!("cache hit must not fetch")
        })
        .await
        .unwrap();
    assert!(cached.cached);
    assert!(!cached.stale);
    let before = cached.updated_at;
    let stale = cache
        .load(1, "account-a".into(), true, || async {
            anyhow::bail!("private-network-error")
        })
        .await
        .unwrap();
    assert!(stale.stale);
    assert!(stale.refresh_error.is_some());
    assert_eq!(stale.updated_at, before);
    let reopened = cache
        .load(1, "account-a".into(), false, || async {
            panic!("fresh cache hit")
        })
        .await
        .unwrap();
    assert!(reopened.stale);
    assert!(reopened.refresh_error.is_some());
    assert!(
        !serde_json::to_string(&stale)
            .unwrap()
            .contains("private-network-error")
    );
    assert!(
        cache
            .load(1, "account-b".into(), false, || async {
                anyhow::bail!("different account")
            })
            .await
            .is_err()
    );
    let slot = cache.slots.lock()[&1].clone();
    slot.lock().await.as_mut().unwrap().fetched_at = Instant::now() - STALE_TTL;
    assert!(
        cache
            .load(1, "account-a".into(), false, || async {
                anyhow::bail!("expired")
            })
            .await
            .is_err()
    );
    cache.remove(1);
    assert!(cache.slots.lock().is_empty());
    for id in 0..140 {
        cache
            .load(id, id.to_string(), false, || async {
                Ok((id.to_string(), catalog()))
            })
            .await
            .unwrap();
    }
    assert_eq!(cache.slots.lock().len(), 128);
}

#[tokio::test]
async fn concurrent_refreshes_share_one_upstream_request_and_cancellation_releases_the_slot() {
    let cache = Arc::new(CatalogCache::default());
    let hits = Arc::new(AtomicUsize::new(0));
    let start = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let (cache, hits, start) = (cache.clone(), hits.clone(), start.clone());
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            cache
                .load(1, "same".into(), true, || async {
                    hits.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(("same".into(), catalog()))
                })
                .await
                .unwrap()
        }));
    }
    for task in tasks {
        assert!(!task.await.unwrap().stale);
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let stalled = {
        let cache = cache.clone();
        tokio::spawn(async move {
            cache
                .load(2, "same".into(), false, || async {
                    entered_tx.send(()).unwrap();
                    std::future::pending::<anyhow::Result<(String, ModelCatalog)>>().await
                })
                .await
        })
    };
    entered_rx.await.unwrap();
    stalled.abort();
    let _ = stalled.await;
    assert!(
        cache
            .load(2, "same".into(), false, || async {
                Ok(("same".into(), catalog()))
            })
            .await
            .is_ok()
    );
    assert_eq!(cache.network_slots.available_permits(), 4);
}

#[tokio::test]
async fn admin_catalog_route_authenticates_before_parsing_and_hides_credential_errors() {
    use crate::{
        admin::{AdminService, AdminState, create_admin_router},
        kiro::{model::credentials::KiroCredentials, token_manager::MultiTokenManager},
        model::config::Config,
    };
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let manager = Arc::new(
        MultiTokenManager::new(
            Config::default(),
            vec![KiroCredentials {
                id: Some(1),
                auth_method: Some("api_key".into()),
                ..Default::default()
            }],
            None,
            None,
            true,
        )
        .unwrap(),
    );
    let app = create_admin_router(AdminState::new(
        "admin",
        AdminService::new(manager, ["ide".into()]),
    ));
    for (path, authorized, status) in [
        (
            "/credentials/invalid/models?force=bad",
            false,
            StatusCode::UNAUTHORIZED,
        ),
        ("/credentials/999/models", true, StatusCode::NOT_FOUND),
        (
            "/credentials/1/models?force=bad",
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/credentials/1/models?unknown=true",
            true,
            StatusCode::BAD_REQUEST,
        ),
        ("/credentials/1/models", true, StatusCode::BAD_GATEWAY),
    ] {
        let mut req = Request::builder().uri(path);
        if authorized {
            req = req.header("x-api-key", "admin");
        }
        let response = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{path}");
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("kiroApiKey"));
    }
}
