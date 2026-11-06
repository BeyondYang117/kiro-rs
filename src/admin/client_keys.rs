//! 调用密钥单独管理：列表脱敏、创建时展示一次、保存后重启生效。
use axum::{Json, extract::State, response::IntoResponse};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    error::AdminServiceError,
    extract::{AdminJson, AdminPath},
    middleware::AdminState,
    settings::{read_document, storage_error},
};
use crate::{kiro::token_manager::MultiTokenManager, model::config::Config};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyItem {
    id: String,
    masked_key: String,
    legacy: bool,
    configured: bool,
    active: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct KeysResponse {
    keys: Vec<KeyItem>,
    revision: String,
    can_save: bool,
    restart_required: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct AddKeyRequest {
    revision: String,
    /// 省略时生成随机密钥。空字符串不视为省略，避免静默替换输入。
    key: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeleteKeyRequest {
    revision: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreatedKeyResponse {
    key: String,
    state: KeysResponse,
}

fn invalid(message: &str) -> AdminServiceError {
    AdminServiceError::InvalidSettings(message.into())
}

fn key_id(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

fn revision(document: &Value) -> String {
    key_id(&json!([document.get("apiKey"), document.get("apiKeys")]).to_string())
}

fn saved_config(document: &Value) -> Result<Config, AdminServiceError> {
    // 不将文件中的具体值带入解析错误或日志。
    serde_json::from_value(document.clone()).map_err(|_| storage_error("调用密钥配置格式无效"))
}

fn keys(config: &Config) -> Result<Vec<String>, AdminServiceError> {
    // 空配置允许在管理页面添加首个密钥；服务启动时仍要求至少一个。
    if config.api_key.is_none() && config.api_keys.is_empty() {
        return Ok(vec![]);
    }
    config
        .client_api_keys()
        .map_err(|_| invalid("调用密钥配置无效，请检查 apiKey 和 apiKeys"))
}

fn response(document: &Value, running: &Config) -> Result<KeysResponse, AdminServiceError> {
    let config = saved_config(document)?;
    let saved_keys = keys(&config)?;
    let running_keys = keys(running)?;
    let restart_required = saved_keys.len() != running_keys.len()
        || saved_keys.iter().any(|key| !running_keys.contains(key));
    let mut all_keys = saved_keys.clone();
    for key in &running_keys {
        if !all_keys.contains(key) {
            all_keys.push(key.clone());
        }
    }
    Ok(KeysResponse {
        keys: all_keys
            .iter()
            .map(|key| KeyItem {
                id: key_id(key),
                // 短密钥不展示任何原文，长密钥仅展示首尾片段。
                masked_key: if key.len() > 12 {
                    format!("{}…{}", &key[..4], &key[key.len() - 4..])
                } else {
                    "••••••••".into()
                },
                legacy: config.api_key.as_ref() == Some(key),
                configured: saved_keys.contains(key),
                active: running_keys.contains(key),
            })
            .collect(),
        revision: revision(document),
        can_save: running.config_path().is_some(),
        restart_required,
    })
}

pub(super) fn read(manager: &MultiTokenManager) -> Result<KeysResponse, AdminServiceError> {
    manager.with_configuration(|config, _| {
        let document = if let Some(path) = config.config_path() {
            read_document(path)?
        } else {
            serde_json::to_value(config).map_err(storage_error)?
        };
        response(&document, config)
    })
}

pub(super) fn add(
    manager: &MultiTokenManager,
    request: AddKeyRequest,
) -> Result<CreatedKeyResponse, AdminServiceError> {
    let key = request.key.unwrap_or_else(|| {
        format!(
            "sk-{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        )
    });
    if !crate::common::auth::is_valid_api_key(&key) {
        return Err(invalid(
            "密钥必须为非空的可打印 ASCII 字符，不能包含空白或逗号",
        ));
    }
    manager.with_configuration(|config, _| {
        let path = config
            .config_path()
            .ok_or_else(|| invalid("配置文件路径未知，无法保存"))?;
        let mut document = read_document(path)?;
        if revision(&document) != request.revision {
            return Err(AdminServiceError::ConfigurationConflict);
        }
        let saved = saved_config(&document)?;
        if keys(&saved)?.contains(&key) {
            return Err(invalid("此调用密钥已存在"));
        }
        if config.admin_api_key.as_deref() == Some(&key)
            || saved.admin_api_key.as_deref() == Some(&key)
        {
            return Err(invalid("调用密钥不能与管理密钥相同"));
        }
        let mut api_keys = saved.api_keys;
        api_keys.push(key.clone());
        document["apiKeys"] = json!(api_keys);
        let state = response(&document, config)?;
        config.save_document(&document).map_err(storage_error)?;
        Ok(CreatedKeyResponse { key, state })
    })
}

pub(super) fn delete(
    manager: &MultiTokenManager,
    id: &str,
    request: DeleteKeyRequest,
) -> Result<KeysResponse, AdminServiceError> {
    manager.with_configuration(|config, _| {
        let path = config
            .config_path()
            .ok_or_else(|| invalid("配置文件路径未知，无法保存"))?;
        let mut document = read_document(path)?;
        if revision(&document) != request.revision {
            return Err(AdminServiceError::ConfigurationConflict);
        }
        let saved = saved_config(&document)?;
        let saved_keys = keys(&saved)?;
        if !saved_keys.iter().any(|key| key_id(key) == id) {
            return Err(invalid("调用密钥不存在或已删除，请重新加载"));
        }
        if saved_keys.len() == 1 {
            return Err(invalid("至少保留一个调用密钥，请先添加替代密钥"));
        }
        // 同时移除旧字段和数组中的重复项，确保重启后真正撤销。
        if saved.api_key.as_ref().is_some_and(|key| key_id(key) == id) {
            document.as_object_mut().unwrap().remove("apiKey");
        }
        document["apiKeys"] = json!(
            saved
                .api_keys
                .into_iter()
                .filter(|key| key_id(key) != id)
                .collect::<Vec<_>>()
        );
        let state = response(&document, config)?;
        config.save_document(&document).map_err(storage_error)?;
        Ok(state)
    })
}

pub(super) async fn list_keys(State(state): State<AdminState>) -> impl IntoResponse {
    match state.service.read_client_keys() {
        Ok(response) => Json(response).into_response(),
        Err(error) => (error.status_code(), Json(error.into_response())).into_response(),
    }
}

pub(super) async fn add_key(
    State(state): State<AdminState>,
    AdminJson(request): AdminJson<AddKeyRequest>,
) -> impl IntoResponse {
    match state.service.add_client_key(request) {
        Ok(response) => Json(response).into_response(),
        Err(error) => (error.status_code(), Json(error.into_response())).into_response(),
    }
}

pub(super) async fn delete_key(
    State(state): State<AdminState>,
    AdminPath(id): AdminPath<String>,
    AdminJson(request): AdminJson<DeleteKeyRequest>,
) -> impl IntoResponse {
    match state.service.delete_client_key(&id, request) {
        Ok(response) => Json(response).into_response(),
        Err(error) => (error.status_code(), Json(error.into_response())).into_response(),
    }
}

#[cfg(test)]
mod tests;
