//! Admin UI 路由配置

use axum::{
    Router,
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::IntoResponse,
    routing::get,
};
use rust_embed::Embed;

/// 嵌入前端构建产物
#[derive(Embed)]
#[folder = "admin-ui/dist"]
struct Asset;

/// 创建 Admin UI 路由
pub fn create_admin_ui_router() -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/{*file}", get(static_handler))
        .layer(axum::middleware::from_fn(
            crate::common::security::admin_ui_headers,
        ))
}

/// 处理首页请求
async fn index_handler() -> impl IntoResponse {
    serve_index()
}

/// 处理静态文件请求
async fn static_handler(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');

    // 安全检查：拒绝包含 .. 的路径
    if path.contains("..") {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Invalid path"))
            .expect("Failed to build response");
    }

    // 尝试获取请求的文件
    if let Some(content) = Asset::get(path) {
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();

        // 根据文件类型设置不同的缓存策略
        let cache_control = get_cache_control(path);

        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, cache_control)
            .body(Body::from(content.data.into_owned()))
            .expect("Failed to build response");
    }

    // SPA fallback: 如果文件不存在且不是资源文件，返回 index.html
    if !is_asset_path(path) {
        return serve_index();
    }

    // 404
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("Not found"))
        .expect("Failed to build response")
}

/// 提供 index.html
fn serve_index() -> Response<Body> {
    match Asset::get("index.html") {
        Some(content) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(content.data.into_owned()))
            .expect("Failed to build response"),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from(
                "Admin UI not built. Run 'pnpm build' in admin-ui directory.",
            ))
            .expect("Failed to build response"),
    }
}

/// 根据文件类型返回合适的缓存策略
fn get_cache_control(path: &str) -> &'static str {
    if path.ends_with(".html") {
        // HTML 文件不缓存，确保用户获取最新版本
        "no-cache"
    } else if path.starts_with("assets/") {
        // assets/ 目录下的文件带有内容哈希，可以长期缓存
        "public, max-age=31536000, immutable"
    } else {
        // 其他文件（如 favicon）使用较短的缓存
        "public, max-age=3600"
    }
}

/// 判断是否为资源文件路径（有扩展名的文件）
fn is_asset_path(path: &str) -> bool {
    // 检查最后一个路径段是否包含扩展名
    path.rsplit('/')
        .next()
        .map(|filename| filename.contains('.'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn browser_security_headers_cover_html_assets_and_errors_without_breaking_caching() {
        let app = Router::new().nest("/admin", create_admin_ui_router());
        let asset = Asset::iter()
            .find(|path| path.starts_with("assets/") && path.ends_with(".js"))
            .expect("built JS asset")
            .into_owned();
        for (method, path, status, cache) in [
            ("GET", "/".to_owned(), StatusCode::OK, Some("no-cache")),
            (
                "GET",
                "/credentials".to_owned(),
                StatusCode::OK,
                Some("no-cache"),
            ),
            (
                "GET",
                format!("/{asset}"),
                StatusCode::OK,
                Some("public, max-age=31536000, immutable"),
            ),
            (
                "GET",
                "/vite.svg".to_owned(),
                StatusCode::OK,
                Some("public, max-age=3600"),
            ),
            ("GET", "/missing.js".to_owned(), StatusCode::NOT_FOUND, None),
            (
                "GET",
                "/../index.html".to_owned(),
                StatusCode::BAD_REQUEST,
                None,
            ),
            ("POST", "/".to_owned(), StatusCode::METHOD_NOT_ALLOWED, None),
        ] {
            let path = if path == "/" {
                "/admin".to_owned()
            } else {
                format!("/admin{path}")
            };
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(&path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{method} {path}");
            let headers = response.headers();
            assert_eq!(headers["x-frame-options"], "DENY");
            assert_eq!(headers["x-content-type-options"], "nosniff");
            assert_eq!(headers["referrer-policy"], "no-referrer");
            let csp = headers["content-security-policy"].to_str().unwrap();
            for directive in [
                "default-src 'none'",
                "script-src 'self'",
                "connect-src 'self'",
                "frame-ancestors 'none'",
                "base-uri 'none'",
                "form-action 'none'",
            ] {
                assert!(
                    csp.split(';')
                        .map(str::trim)
                        .any(|value| value == directive)
                );
            }
            assert!(!csp.contains("unsafe-eval"));
            assert!(
                !csp.split(';')
                    .find(|part| part.trim().starts_with("script-src "))
                    .unwrap()
                    .contains("unsafe-inline")
            );
            assert_eq!(
                headers
                    .get(header::CACHE_CONTROL)
                    .map(|value| value.to_str().unwrap()),
                cache
            );
            if status == StatusCode::OK {
                assert!(
                    !to_bytes(response.into_body(), 8 * 1024 * 1024)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
        }
    }
}
