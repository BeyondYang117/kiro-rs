//! Bounded, account-scoped model catalog cache. No tokens or raw schemas leave this API.
use std::{
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{Json, extract::State, response::IntoResponse};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

use super::{
    error::AdminServiceError,
    extract::{AdminPath, AdminQuery},
    middleware::AdminState,
};
use crate::kiro::models::ModelCatalog;

const CACHE_TTL: Duration = Duration::from_secs(300);
const STALE_TTL: Duration = Duration::from_secs(3600);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CatalogResponse {
    #[serde(skip)]
    pub scope_key: String,
    pub credential_id: u64,
    #[serde(flatten)]
    pub catalog: ModelCatalog,
    pub updated_at: String,
    pub cached: bool,
    pub stale: bool,
    pub refresh_error: Option<String>,
}

struct CachedCatalog {
    key: String,
    fetched_at: Instant,
    response: CatalogResponse,
}

type CacheSlot = Arc<AsyncMutex<Option<CachedCatalog>>>;

pub(super) struct CatalogCache {
    slots: Mutex<HashMap<u64, CacheSlot>>,
    network_slots: Semaphore,
}

impl Default for CatalogCache {
    fn default() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            network_slots: Semaphore::new(4),
        }
    }
}

fn unavailable() -> AdminServiceError {
    AdminServiceError::UpstreamError(
        "无法查询该账号的可用模型，请检查认证、API 区域、代理或账号权限后重试".into(),
    )
}

impl CatalogCache {
    pub fn remove(&self, id: u64) {
        self.slots.lock().remove(&id);
    }

    pub async fn load<F, Fut>(
        &self,
        id: u64,
        key: String,
        force: bool,
        fetch: F,
    ) -> Result<CatalogResponse, AdminServiceError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<(String, ModelCatalog)>>,
    {
        let slot = {
            let mut slots = self.slots.lock();
            // Evict an idle slot at the bound. Active fetches are shared, never replaced.
            if !slots.contains_key(&id) && slots.len() >= 128 {
                if let Some(idle) = slots
                    .iter()
                    .find(|(_, slot)| Arc::strong_count(slot) == 1)
                    .map(|(id, _)| *id)
                {
                    slots.remove(&idle);
                } else {
                    return Err(unavailable());
                }
            }
            slots
                .entry(id)
                .or_insert_with(|| Arc::new(AsyncMutex::new(None)))
                .clone()
        };
        let requested_at = Instant::now();
        let mut cached = tokio::time::timeout(Duration::from_secs(60), slot.lock())
            .await
            .map_err(|_| unavailable())?;
        if let Some(hit) = cached.as_ref().filter(|hit| hit.key == key) {
            // Simultaneous forced refreshes share the result already fetched since arrival.
            if hit.fetched_at.elapsed() < CACHE_TTL && (!force || hit.fetched_at >= requested_at) {
                let mut response = hit.response.clone();
                response.cached = true;
                return Ok(response);
            }
        }
        let result = tokio::time::timeout(Duration::from_secs(55), async {
            let _permit = self.network_slots.acquire().await?;
            fetch().await
        })
        .await;
        match result {
            Ok(Ok((key, catalog))) => {
                let response = CatalogResponse {
                    scope_key: key.clone(),
                    credential_id: id,
                    catalog,
                    updated_at: chrono::Utc::now().to_rfc3339(),
                    cached: false,
                    stale: false,
                    refresh_error: None,
                };
                *cached = Some(CachedCatalog {
                    key,
                    fetched_at: Instant::now(),
                    response: response.clone(),
                });
                Ok(response)
            }
            _ => {
                if let Some(hit) = cached
                    .as_mut()
                    .filter(|hit| hit.key == key && hit.fetched_at.elapsed() < STALE_TTL)
                {
                    hit.response.stale = true;
                    hit.response.refresh_error =
                        Some("刷新失败，当前展示上次成功查询的模型；不能据此确认当前授权".into());
                    let mut response = hit.response.clone();
                    response.cached = true;
                    Ok(response)
                } else {
                    Err(unavailable())
                }
            }
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelsQuery {
    #[serde(default)]
    force: bool,
}

pub(super) async fn get_models(
    State(state): State<AdminState>,
    AdminPath(id): AdminPath<u64>,
    AdminQuery(query): AdminQuery<ModelsQuery>,
) -> impl IntoResponse {
    match state.service.available_models(id, query.force).await {
        Ok(response) => Json(response).into_response(),
        Err(error) => (error.status_code(), Json(error.into_response())).into_response(),
    }
}

#[cfg(test)]
mod tests;
