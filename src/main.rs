mod admin;
mod admin_ui;
mod anthropic;
mod common;
mod http_client;
mod kiro;
mod model;
mod request_log;
pub mod token;

use std::collections::HashMap;
use std::sync::Arc;

use clap::Parser;
use kiro::endpoint::{CliEndpoint, IdeEndpoint, KiroEndpoint};
use kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use kiro::provider::KiroProvider;
use kiro::token_manager::MultiTokenManager;
use model::arg::Args;
use model::config::Config;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer().with_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            ),
        )
        .with(
            request_log::DiagnosticLayer.with_filter(tracing_subscriber::filter::LevelFilter::WARN),
        )
        .init();

    // 加载配置
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());
    let config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });
    let api_keys = config.client_api_keys().unwrap_or_else(|error| {
        tracing::error!("客户端密钥配置无效: {error}");
        std::process::exit(1);
    });
    tracing::info!("已加载 {} 个客户端 API 密钥", api_keys.len());

    // 加载凭证（支持单对象或数组格式）
    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());
    let credentials_config = CredentialsConfig::load(&credentials_path).unwrap_or_else(|e| {
        tracing::error!("加载凭证失败: {}", e);
        std::process::exit(1);
    });

    // 记录原始凭据格式，回写时尽量保留
    let is_multiple_format = credentials_config.is_multiple();

    // 转换为按优先级排序的凭据列表
    let mut credentials_list = credentials_config.into_sorted_credentials();

    // 检查 KIRO_API_KEY 环境变量，自动创建 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        if kiro_api_key.is_empty() {
            tracing::warn!("KIRO_API_KEY 环境变量已设置但为空，视为未配置");
        } else {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    // 获取第一个凭据用于日志显示
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!("主凭证: {:?}", first_credentials);

    // 构建代理配置
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理");
    }

    // 构建端点注册表
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    {
        let ide = IdeEndpoint::new();
        endpoints.insert(ide.name().to_string(), Arc::new(ide));
        let cli = CliEndpoint::new();
        endpoints.insert(cli.name().to_string(), Arc::new(cli));
    }

    // 校验默认端点存在
    if !endpoints.contains_key(&config.default_endpoint) {
        tracing::error!("默认端点 \"{}\" 未注册", config.default_endpoint);
        std::process::exit(1);
    }

    // 校验所有凭据声明的端点都已注册
    for cred in &credentials_list {
        let name = cred.endpoint.as_deref().unwrap_or(&config.default_endpoint);
        if !endpoints.contains_key(name) {
            tracing::error!(
                "凭据 id={:?} 指定了未知端点 \"{}\"（已注册: {:?}）",
                cred.id,
                name,
                endpoints.keys().collect::<Vec<_>>()
            );
            std::process::exit(1);
        }
    }

    let endpoint_names: Vec<String> = endpoints.keys().cloned().collect();

    // 创建 MultiTokenManager 和 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(credentials_path.clone().into()),
        is_multiple_format,
    )
    .unwrap_or_else(|e| {
        tracing::error!("创建 Token 管理器失败: {}", e);
        std::process::exit(1);
    });
    let token_manager = Arc::new(token_manager);
    let log_path = config
        .request_log_path
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::Path::new(&credentials_path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("kiro_requests.sqlite3")
        });
    let request_logs = request_log::LogStore::open(&log_path, config.request_log_retention_days)
        .unwrap_or_else(|error| {
            tracing::error!("初始化调用日志失败: {error:#}");
            std::process::exit(1);
        });
    tracing::info!(path = %log_path.display(), "调用日志持久化已启用");
    let kiro_provider = KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
    );

    // 初始化 count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config,
        tls_backend: config.tls_backend,
        timeout_secs: config.count_tokens_timeout_seconds,
    });

    // 构建 Anthropic API 路由（profile_arn 由 provider 层根据实际凭据动态注入）
    let anthropic_app = anthropic::create_router_with_provider(
        api_keys,
        Some(kiro_provider),
        config.extract_thinking,
        config.cc_buffered_stream,
        config.force_stream,
    );

    // 仅在管理密钥可用作单个 HTTP 认证值时启用管理路由。
    let admin_key_valid = config
        .admin_api_key
        .as_ref()
        .map(|k| common::auth::is_valid_api_key(k))
        .unwrap_or(false);

    let app = if let Some(admin_key) = &config.admin_api_key {
        if !common::auth::is_valid_api_key(admin_key) {
            tracing::warn!("adminApiKey 为空或包含无效字符，Admin API 未启用");
            anthropic_app
        } else {
            let admin_service =
                admin::AdminService::new(token_manager.clone(), endpoint_names.clone());
            let admin_state = admin::AdminState::new(admin_key, admin_service);
            let admin_app = admin::create_admin_router(admin_state)
                .layer(axum::Extension(request_logs.clone()));

            // 创建 Admin UI 路由
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                .nest("/admin", admin_ui_app)
        }
    } else {
        anthropic_app
    };

    let app = app.layer(axum::middleware::from_fn_with_state(
        request_logs.clone(),
        request_log::middleware,
    ));

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    tracing::info!("API Key 认证已启用");
    tracing::info!("可用 API:");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    if admin_key_valid {
        tracing::info!("Admin API:");
        tracing::info!("  GET  /api/admin/credentials");
        tracing::info!("  POST /api/admin/credentials/:index/disabled");
        tracing::info!("  POST /api/admin/credentials/:index/priority");
        tracing::info!("  POST /api/admin/credentials/:index/reset");
        tracing::info!("  GET  /api/admin/credentials/:index/balance");
        tracing::info!("Admin UI:");
        tracing::info!("  GET  /admin");
    }

    let app = app
        .route(
            "/healthz",
            axum::routing::get(|| async { axum::http::StatusCode::OK }),
        )
        .route(
            "/readyz",
            axum::routing::get(move || {
                let manager = token_manager.clone();
                async move {
                    if manager.available_count() > 0 {
                        axum::http::StatusCode::OK
                    } else {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.changed().await;
    });
    let server = std::future::IntoFuture::into_future(server);
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => { result.expect("HTTP server failed"); }
        () = shutdown_signal() => {
            tracing::info!("收到停机信号，停止接收连接，最多等待 30 秒完成在途请求");
            let _ = shutdown_tx.send(true);
            match tokio::time::timeout(std::time::Duration::from_secs(30), &mut server).await {
                Ok(result) => result.expect("HTTP server failed during shutdown"),
                Err(_) => tracing::warn!("停机等待超时，取消剩余请求"),
            }
        }
    }
    // Tokio waits for the final spawn_blocking log writes when the runtime exits.
}

async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("安装 Ctrl-C 处理器失败");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("安装 SIGTERM 处理器失败")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { () = interrupt => {}, () = terminate => {} }
}
