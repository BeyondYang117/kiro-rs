//! Fresh account catalogs refine routing without network I/O on the generation path.
use std::collections::HashMap;

use super::*;
use crate::kiro::{
    model_controls::{ModelControls, ModelParameterError},
    models::{AvailableModel, CATALOG_TTL, ModelCatalog},
};

pub(super) struct RoutingCatalog {
    scope_key: String,
    fetched_at: Instant,
    stale: bool,
    models: HashMap<String, AvailableModel>,
}

impl MultiTokenManager {
    pub(crate) fn invalidate_model_catalog(&self, id: u64, scope_key: &str, requested_at: Instant) {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.id == id)
            && let Some(current) = &entry.model_catalog
            && current.scope_key == scope_key
            && current.fetched_at <= requested_at
        {
            entry.model_catalog = Some(Arc::new(RoutingCatalog {
                scope_key: scope_key.to_owned(),
                fetched_at: current.fetched_at,
                stale: true,
                models: HashMap::new(),
            }));
        }
        drop(entries);
        self.capacity_changed.notify_waiters();
    }

    /// Publish the original fetch time, never the time a cached response was read.
    /// A stale response invalidates the same snapshot; a delayed fresh cache hit
    /// cannot reactivate it. A later successful fetch can replace it.
    pub(crate) fn update_model_catalog(
        &self,
        id: u64,
        scope_key: &str,
        catalog: &ModelCatalog,
        fetched_at: Instant,
        stale: bool,
    ) {
        let mut entries = self.entries.lock();
        let Some(entry) = entries.iter_mut().find(|entry| entry.id == id) else {
            return;
        };
        if self.model_catalog_key_for(&entry.credentials) != scope_key {
            return;
        }
        if let Some(current) = &entry.model_catalog
            && current.scope_key == scope_key
            && (current.fetched_at > fetched_at
                || (current.fetched_at == fetched_at && (current.stale || !stale)))
        {
            return;
        }
        entry.model_catalog = Some(Arc::new(RoutingCatalog {
            scope_key: scope_key.to_owned(),
            fetched_at,
            stale,
            models: if stale {
                HashMap::new()
            } else {
                catalog
                    .models
                    .iter()
                    .map(|model| (model.model_id.clone(), model.clone()))
                    .collect()
            },
        }));
        // Match the admin cache bound, including snapshots for recently evicted slots.
        if entries
            .iter()
            .filter(|entry| entry.model_catalog.is_some())
            .count()
            > 128
            && let Some(oldest) = entries
                .iter_mut()
                .filter(|entry| entry.model_catalog.is_some())
                .min_by_key(|entry| entry.model_catalog.as_ref().unwrap().fetched_at)
        {
            oldest.model_catalog = None;
        }
        drop(entries);
        self.capacity_changed.notify_waiters();
    }

    fn fresh_catalog<'a>(&self, entry: &'a CredentialEntry) -> Option<&'a RoutingCatalog> {
        entry.model_catalog.as_deref().filter(|catalog| {
            !catalog.stale
                && catalog.fetched_at.elapsed() < CATALOG_TTL
                && catalog.scope_key == self.model_catalog_key_for(&entry.credentials)
        })
    }

    /// None means authorization is unknown and the existing tier rule applies.
    pub(super) fn catalog_support(
        &self,
        entry: &CredentialEntry,
        model: Option<&str>,
    ) -> Option<bool> {
        let model = model?;
        let catalog = self.fresh_catalog(entry)?;
        if catalog.models.contains_key(model) {
            return Some(true);
        }
        // Match the same canonical ID that conversion sends upstream.
        let canonical =
            crate::anthropic::map_model(model).unwrap_or_else(|| model.to_ascii_lowercase());
        Some(catalog.models.contains_key(&canonical))
    }

    pub(super) fn catalog_supports_opus(&self, entry: &CredentialEntry) -> bool {
        self.fresh_catalog(entry).map_or_else(
            || entry.credentials.supports_opus(),
            |catalog| {
                catalog.models.keys().any(|id| {
                    Self::requires_opus(Some(id))
                        && crate::anthropic::map_model(id).as_deref() == Some(id.as_str())
                })
            },
        )
    }

    pub(super) fn catalog_expiry(&self, entry: &CredentialEntry) -> Option<Instant> {
        self.fresh_catalog(entry)
            .map(|catalog| catalog.fetched_at + CATALOG_TTL)
    }

    /// Shape controls using the selected account's fresh schema. Verified GPT/Kimi
    /// schemas reject max_tokens; unknown catalogs use the same provider default.
    pub(crate) fn model_request_body(&self, id: u64, body: String) -> String {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&body) else {
            return body;
        };
        let Some(model) = value
            .pointer("/conversationState/currentMessage/userInputMessage/modelId")
            .and_then(serde_json::Value::as_str)
        else {
            return body;
        };
        let canonical = crate::anthropic::map_model(model).unwrap_or_else(|| model.to_owned());
        let supported = {
            let entries = self.entries.lock();
            entries
                .iter()
                .find(|entry| entry.id == id)
                .and_then(|entry| self.fresh_catalog(entry))
                .and_then(|catalog| catalog.models.get(&canonical))
                .and_then(|model| model.native_max_tokens)
        };
        let omit = supported == Some(false)
            || (supported.is_none() && crate::anthropic::is_non_claude(&canonical));
        if omit
            && let Some(fields) = value
                .get_mut("additionalModelRequestFields")
                .and_then(serde_json::Value::as_object_mut)
        {
            fields.remove("max_tokens");
            if fields.is_empty() {
                value
                    .as_object_mut()
                    .unwrap()
                    .remove("additionalModelRequestFields");
            }
            return serde_json::to_string(&value).unwrap_or(body);
        }
        body
    }

    pub(super) fn validate_model_controls(
        &self,
        entry: &CredentialEntry,
        model: Option<&str>,
        controls: Option<&ModelControls>,
    ) -> Result<(), ModelParameterError> {
        if !crate::kiro::endpoint::native_thinking(&entry.credentials, &self.config)
            && !model.is_some_and(crate::anthropic::is_non_claude)
        {
            return Ok(());
        }
        if let (Some(model), Some(controls), Some(catalog)) =
            (model, controls, self.fresh_catalog(entry))
        {
            let canonical =
                crate::anthropic::map_model(model).unwrap_or_else(|| model.to_ascii_lowercase());
            if let Some(capabilities) = catalog.models.get(&canonical) {
                controls.validate(capabilities)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
