//! Admin API 业务逻辑服务

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::token_manager::MultiTokenManager;

use super::error::AdminServiceError;
use super::types::{
    AddCredentialRequest, AddCredentialResponse, BalanceResponse, CredentialStatusItem,
    CredentialsStatusResponse, LoadBalancingModeResponse, SetLoadBalancingModeRequest,
};

/// 余额缓存过期时间（秒），5 分钟
const BALANCE_CACHE_TTL_SECS: i64 = 300;

/// 缓存的余额条目（含时间戳）
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedBalance {
    /// 旧缓存没有身份信息时不复用，下一次查询会重新获取。
    #[serde(default)]
    credential_key: Option<String>,
    /// 缓存时间（Unix 秒）
    cached_at: f64,
    /// 缓存的余额数据
    data: BalanceResponse,
}

impl CachedBalance {
    fn is_fresh(&self, now: f64) -> bool {
        let age = now - self.cached_at;
        (0.0..BALANCE_CACHE_TTL_SECS as f64).contains(&age)
    }
}

/// Admin 服务
///
/// 封装所有 Admin API 的业务逻辑
pub struct AdminService {
    token_manager: Arc<MultiTokenManager>,
    balance_cache: Mutex<HashMap<u64, CachedBalance>>,
    cache_path: Option<PathBuf>,
    /// 已注册的端点名称集合（用于 add_credential 校验）
    known_endpoints: HashSet<String>,
}

impl AdminService {
    pub fn new(
        token_manager: Arc<MultiTokenManager>,
        known_endpoints: impl IntoIterator<Item = String>,
    ) -> Self {
        let cache_path = token_manager
            .cache_dir()
            .map(|d| d.join("kiro_balance_cache.json"));

        let balance_cache = Self::load_balance_cache_from(&cache_path);

        Self {
            token_manager,
            balance_cache: Mutex::new(balance_cache),
            cache_path,
            known_endpoints: known_endpoints.into_iter().collect(),
        }
    }

    /// 获取所有凭据状态
    pub fn get_all_credentials(&self) -> CredentialsStatusResponse {
        let snapshot = self.token_manager.snapshot();
        let default_endpoint = self.token_manager.config().default_endpoint.clone();

        let mut credentials: Vec<CredentialStatusItem> = snapshot
            .entries
            .into_iter()
            .map(|entry| CredentialStatusItem {
                id: entry.id,
                priority: entry.priority,
                disabled: entry.disabled,
                failure_count: entry.failure_count,
                is_current: entry.id == snapshot.current_id,
                expires_at: entry.expires_at,
                auth_method: entry.auth_method,
                has_profile_arn: entry.has_profile_arn,
                refresh_token_hash: entry.refresh_token_hash,
                api_key_hash: entry.api_key_hash,
                masked_api_key: entry.masked_api_key,
                email: entry.email,
                success_count: entry.success_count,
                in_flight: entry.in_flight,
                cooldown_remaining_ms: entry.cooldown_remaining_ms,
                effective_concurrency_limit: entry.effective_concurrency_limit,
                last_used_at: entry.last_used_at.clone(),
                has_proxy: entry.has_proxy,
                proxy_url: entry.proxy_url,
                refresh_failure_count: entry.refresh_failure_count,
                disabled_reason: entry.disabled_reason,
                endpoint: entry.endpoint.unwrap_or_else(|| default_endpoint.clone()),
            })
            .collect();

        // 按优先级排序（数字越小优先级越高）
        credentials.sort_by_key(|c| c.priority);

        CredentialsStatusResponse {
            concurrency: snapshot.concurrency,
            total: snapshot.total,
            available: snapshot.available,
            current_id: snapshot.current_id,
            credentials,
        }
    }

    /// 设置凭据禁用状态
    pub fn set_disabled(&self, id: u64, disabled: bool) -> Result<(), AdminServiceError> {
        // 先获取当前凭据 ID，用于判断是否需要切换
        let snapshot = self.token_manager.snapshot();
        let current_id = snapshot.current_id;

        self.token_manager
            .set_disabled(id, disabled)
            .map_err(|e| self.classify_error(e, id))?;

        // 只有禁用的是当前凭据时才尝试切换到下一个
        if disabled && id == current_id {
            let _ = self.token_manager.switch_to_next();
        }
        Ok(())
    }

    /// 设置凭据优先级
    pub fn set_priority(&self, id: u64, priority: u32) -> Result<(), AdminServiceError> {
        self.token_manager
            .set_priority(id, priority)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 重置失败计数并重新启用
    pub fn reset_and_enable(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .reset_and_enable(id)
            .map_err(|e| self.classify_error(e, id))
    }

    /// 获取凭据余额（带缓存）
    pub async fn get_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let credential_key = self
            .token_manager
            .credential_cache_key(id)
            .ok_or(AdminServiceError::NotFound { id })?;
        // 先查缓存
        {
            let cache = self.balance_cache.lock();
            if let Some(cached) = cache.get(&id) {
                let now = Utc::now().timestamp() as f64;
                if cached.is_fresh(now)
                    && cached.data.id == id
                    && cached.credential_key.as_deref() == Some(credential_key.as_str())
                {
                    tracing::debug!("凭据 #{} 余额命中缓存", id);
                    return Ok(cached.data.clone());
                }
            }
        }

        // 缓存未命中或已过期，从上游获取
        let balance = self.fetch_balance(id).await?;
        // 刷新可能轮换 refreshToken；使用查询完成后的身份，并重新检查凭据存在。
        let credential_key = self
            .token_manager
            .credential_cache_key(id)
            .ok_or(AdminServiceError::NotFound { id })?;

        // 更新缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.insert(
                id,
                CachedBalance {
                    credential_key: Some(credential_key),
                    cached_at: Utc::now().timestamp() as f64,
                    data: balance.clone(),
                },
            );
        }
        self.save_balance_cache();

        Ok(balance)
    }

    /// 从上游获取余额（无缓存）
    async fn fetch_balance(&self, id: u64) -> Result<BalanceResponse, AdminServiceError> {
        let usage = self
            .token_manager
            .get_usage_limits_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))?;

        let current_usage = usage.current_usage();
        let usage_limit = usage.usage_limit();
        let remaining = (usage_limit - current_usage).max(0.0);
        let usage_percentage = if usage_limit > 0.0 {
            (current_usage / usage_limit * 100.0).min(100.0)
        } else {
            0.0
        };

        Ok(BalanceResponse {
            id,
            subscription_title: usage.subscription_title().map(|s| s.to_string()),
            current_usage,
            usage_limit,
            remaining,
            usage_percentage,
            next_reset_at: usage.next_date_reset,
        })
    }

    /// 添加新凭据
    pub async fn add_credential(
        &self,
        req: AddCredentialRequest,
    ) -> Result<AddCredentialResponse, AdminServiceError> {
        // 校验端点名：未指定则默认合法，指定则必须已注册
        if let Some(ref name) = req.endpoint
            && !self.known_endpoints.contains(name)
        {
            let mut known: Vec<&str> = self.known_endpoints.iter().map(|s| s.as_str()).collect();
            known.sort();
            return Err(AdminServiceError::InvalidCredential(format!(
                "未知端点 \"{}\"，已注册端点: {:?}",
                name, known
            )));
        }

        // 构建凭据对象
        let email = req.email.clone();
        let new_cred = KiroCredentials {
            id: None,
            access_token: None,
            refresh_token: req.refresh_token,
            profile_arn: None,
            expires_at: None,
            auth_method: Some(req.auth_method),
            client_id: req.client_id,
            client_secret: req.client_secret,
            priority: req.priority,
            region: req.region,
            auth_region: req.auth_region,
            api_region: req.api_region,
            machine_id: req.machine_id,
            email: req.email,
            subscription_title: None, // 将在首次获取使用额度时自动更新
            proxy_url: req.proxy_url,
            proxy_username: req.proxy_username,
            proxy_password: req.proxy_password,
            disabled: false, // 新添加的凭据默认启用
            kiro_api_key: req.kiro_api_key,
            endpoint: req.endpoint,
        };

        // 调用 token_manager 添加凭据
        let credential_id = self
            .token_manager
            .add_credential(new_cred)
            .await
            .map_err(|e| self.classify_add_error(e))?;

        // 主动获取订阅等级，避免首次请求时 Free 账号绕过 Opus 模型过滤
        if let Err(e) = self.token_manager.get_usage_limits_for(credential_id).await {
            tracing::warn!("添加凭据后获取订阅等级失败（不影响凭据添加）: {}", e);
        }

        Ok(AddCredentialResponse {
            success: true,
            message: format!("凭据添加成功，ID: {}", credential_id),
            credential_id,
            email,
        })
    }

    /// 删除凭据
    pub fn delete_credential(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .delete_credential(id)
            .map_err(|e| self.classify_delete_error(e, id))?;

        // 清理已删除凭据的余额缓存
        {
            let mut cache = self.balance_cache.lock();
            cache.remove(&id);
        }
        self.save_balance_cache();

        Ok(())
    }

    /// 获取负载均衡模式
    pub fn get_load_balancing_mode(&self) -> LoadBalancingModeResponse {
        LoadBalancingModeResponse {
            mode: self.token_manager.get_load_balancing_mode(),
        }
    }

    /// 设置负载均衡模式
    pub fn set_load_balancing_mode(
        &self,
        req: SetLoadBalancingModeRequest,
    ) -> Result<LoadBalancingModeResponse, AdminServiceError> {
        // 验证模式值
        if req.mode != "priority" && req.mode != "balanced" {
            return Err(AdminServiceError::InvalidCredential(
                "mode 必须是 'priority' 或 'balanced'".to_string(),
            ));
        }

        self.token_manager
            .set_load_balancing_mode(req.mode.clone())
            .map_err(|e| AdminServiceError::InternalError(e.to_string()))?;

        Ok(LoadBalancingModeResponse { mode: req.mode })
    }

    /// 强制刷新指定凭据的 Token
    pub async fn force_refresh_token(&self, id: u64) -> Result<(), AdminServiceError> {
        self.token_manager
            .force_refresh_token_for(id)
            .await
            .map_err(|e| self.classify_balance_error(e, id))
    }

    // ============ 余额缓存持久化 ============

    fn load_balance_cache_from(cache_path: &Option<PathBuf>) -> HashMap<u64, CachedBalance> {
        let path = match cache_path {
            Some(p) => p,
            None => return HashMap::new(),
        };

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return HashMap::new(),
        };

        // 文件中使用字符串 key 以兼容 JSON 格式
        let map: HashMap<String, CachedBalance> = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("解析余额缓存失败，将忽略: {}", e);
                return HashMap::new();
            }
        };

        let now = Utc::now().timestamp() as f64;
        map.into_iter()
            .filter_map(|(k, v)| {
                let id = k.parse::<u64>().ok()?;
                // 丢弃超过 TTL 的条目
                if v.is_fresh(now) && v.data.id == id && v.credential_key.is_some() {
                    Some((id, v))
                } else {
                    None
                }
            })
            .collect()
    }

    fn save_balance_cache(&self) {
        let path = match &self.cache_path {
            Some(p) => p,
            None => return,
        };

        // 持有锁期间完成序列化和写入，防止并发损坏
        let cache = self.balance_cache.lock();
        let map: HashMap<String, &CachedBalance> =
            cache.iter().map(|(k, v)| (k.to_string(), v)).collect();

        match serde_json::to_string_pretty(&map) {
            Ok(json) => {
                if let Err(e) = crate::common::persistence::atomic_write(path, json.as_bytes()) {
                    tracing::warn!("保存余额缓存失败: {}", e);
                }
            }
            Err(e) => tracing::warn!("序列化余额缓存失败: {}", e),
        }
    }

    // ============ 错误分类 ============

    /// 分类简单操作错误（set_disabled, set_priority, reset_and_enable）
    fn classify_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类余额查询错误（可能涉及上游 API 调用）
    fn classify_balance_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();

        // 1. 凭据不存在
        if msg.contains("不存在") {
            return AdminServiceError::NotFound { id };
        }

        // 2. API Key 凭据不支持刷新：客户端请求错误，映射为 400
        if msg.contains("API Key 凭据不支持刷新") {
            return AdminServiceError::InvalidCredential(msg);
        }

        // 3. 上游服务错误特征：HTTP 响应错误或网络错误
        let is_upstream_error =
            // HTTP 响应错误（来自 refresh_*_token 的错误消息）
            msg.contains("凭证已过期或无效") ||
            msg.contains("权限不足") ||
            msg.contains("已被限流") ||
            msg.contains("服务器错误") ||
            msg.contains("Token 刷新失败") ||
            msg.contains("暂时不可用") ||
            // 网络错误（reqwest 错误）
            msg.contains("error trying to connect") ||
            msg.contains("connection") ||
            msg.contains("timeout") ||
            msg.contains("timed out");

        if is_upstream_error {
            AdminServiceError::UpstreamError(msg)
        } else {
            // 4. 默认归类为内部错误（本地验证失败、配置错误等）
            // 包括：缺少 refreshToken、refreshToken 已被截断、无法生成 machineId 等
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类添加凭据错误
    fn classify_add_error(&self, e: anyhow::Error) -> AdminServiceError {
        let msg = e.to_string();

        // 凭据验证失败（refreshToken 无效、格式错误等）
        let is_invalid_credential = msg.contains("缺少 refreshToken")
            || msg.contains("refreshToken 为空")
            || msg.contains("refreshToken 已被截断")
            || msg.contains("凭据已存在")
            || msg.contains("refreshToken 重复")
            || msg.contains("kiroApiKey 重复")
            || msg.contains("缺少 kiroApiKey")
            || msg.contains("kiroApiKey 为空")
            || msg.contains("凭证已过期或无效")
            || msg.contains("权限不足")
            || msg.contains("已被限流");

        if is_invalid_credential {
            AdminServiceError::InvalidCredential(msg)
        } else if msg.contains("error trying to connect")
            || msg.contains("connection")
            || msg.contains("timeout")
        {
            AdminServiceError::UpstreamError(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }

    /// 分类删除凭据错误
    fn classify_delete_error(&self, e: anyhow::Error, id: u64) -> AdminServiceError {
        let msg = e.to_string();
        if msg.contains("不存在") {
            AdminServiceError::NotFound { id }
        } else if msg.contains("只能删除已禁用的凭据") || msg.contains("请先禁用凭据")
        {
            AdminServiceError::InvalidCredential(msg)
        } else {
            AdminServiceError::InternalError(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::config::Config;

    struct CacheTestDirectory(PathBuf);

    impl CacheTestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("kiro-balance-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn service(&self, refresh_token: &str) -> AdminService {
            let manager = MultiTokenManager::new(
                Config::default(),
                vec![KiroCredentials {
                    id: Some(1),
                    refresh_token: Some(refresh_token.into()),
                    ..Default::default()
                }],
                None,
                Some(self.0.join("credentials.json")),
                false,
            )
            .unwrap();
            AdminService::new(Arc::new(manager), ["ide".into()])
        }
    }

    impl Drop for CacheTestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cached_balance(service: &AdminService) -> CachedBalance {
        CachedBalance {
            credential_key: service.token_manager.credential_cache_key(1),
            cached_at: Utc::now().timestamp() as f64,
            data: BalanceResponse {
                id: 1,
                subscription_title: Some("cached-account".into()),
                current_usage: 10.0,
                usage_limit: 100.0,
                remaining: 90.0,
                usage_percentage: 10.0,
                next_reset_at: None,
            },
        }
    }

    #[tokio::test]
    async fn balance_cache_survives_restart_but_rejects_reused_ids_and_deleted_credentials() {
        let directory = CacheTestDirectory::new();
        // 使用短 refreshToken，缓存未命中时会在本地校验失败，不访问外网。
        let service = directory.service("old-account-secret");
        service
            .balance_cache
            .lock()
            .insert(1, cached_balance(&service));
        service.save_balance_cache();
        let content = std::fs::read_to_string(service.cache_path.as_ref().unwrap()).unwrap();
        assert!(!content.contains("old-account-secret"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(service.cache_path.as_ref().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        drop(service);
        let same_account = directory.service("old-account-secret");
        assert_eq!(same_account.get_balance(1).await.unwrap().remaining, 90.0);
        drop(same_account);
        let changed_account = directory.service("new-account-secret");
        let error = changed_account.get_balance(1).await.unwrap_err();
        assert!(error.to_string().contains("refreshToken 已被截断"));
        changed_account.token_manager.set_disabled(1, true).unwrap();
        // 模拟已删除账户仍有缓存的情况（例如删除落盘失败）。
        changed_account.token_manager.delete_credential(1).unwrap();
        assert!(matches!(
            changed_account.get_balance(1).await,
            Err(AdminServiceError::NotFound { id: 1 })
        ));
    }

    #[test]
    fn balance_cache_rejects_expired_future_legacy_and_mismatched_entries() {
        let directory = CacheTestDirectory::new();
        let service = directory.service("account-secret");
        let now = Utc::now().timestamp() as f64;
        let mut valid = cached_balance(&service);
        valid.cached_at = now - 10.0;
        let mut entries = serde_json::Map::new();
        entries.insert("1".into(), serde_json::to_value(&valid).unwrap());
        for (id, age) in [(2, 301.0), (3, -60.0)] {
            let mut invalid = valid.clone();
            invalid.data.id = id;
            invalid.cached_at = now - age;
            entries.insert(id.to_string(), serde_json::to_value(invalid).unwrap());
        }
        let mut legacy = serde_json::to_value(&valid).unwrap();
        legacy.as_object_mut().unwrap().remove("credential_key");
        legacy["data"]["id"] = 4.into();
        entries.insert("4".into(), legacy);
        entries.insert("5".into(), serde_json::to_value(&valid).unwrap());
        entries.insert("invalid-id".into(), serde_json::to_value(&valid).unwrap());
        std::fs::write(
            service.cache_path.as_ref().unwrap(),
            serde_json::to_vec(&entries).unwrap(),
        )
        .unwrap();
        let loaded = AdminService::load_balance_cache_from(&service.cache_path);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains_key(&1));
        assert!(!valid.is_fresh(f64::NAN));
        assert!(!valid.is_fresh(f64::INFINITY));
        assert!(!valid.is_fresh(valid.cached_at + BALANCE_CACHE_TTL_SECS as f64));
    }

    #[test]
    fn failed_balance_save_preserves_original_and_recovers() {
        let directory = CacheTestDirectory::new();
        let service = directory.service("account-secret");
        let path = service.cache_path.as_ref().unwrap();
        std::fs::create_dir(path).unwrap();
        std::fs::write(path.join("keep"), b"original").unwrap();
        service
            .balance_cache
            .lock()
            .insert(1, cached_balance(&service));
        service.save_balance_cache();
        assert_eq!(std::fs::read(path.join("keep")).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 2);
        std::fs::remove_dir_all(path).unwrap();
        service.save_balance_cache();
        assert_eq!(
            AdminService::load_balance_cache_from(&service.cache_path).len(),
            1
        );
    }
}
