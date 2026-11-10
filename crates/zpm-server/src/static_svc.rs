//! 静态服务：pages 根映射 + vendor 严格 404（无 SPA 兜底）。
//!
//! - pages：URL path decode 后，`/` → `pages/index.html`；其余剥前导 `/` 拼到 pagesRoot。
//!   **只有根映射，无 `/pages/` 别名**（`/pages/x.html` 会落 SPA 兜底——保留 TS 行为，勿"修复"）。
//!   SPA 兜底：文件未命中且 pages/index.html 存在 → 200 返回 index.html；否则 404 文本。
//!   穿越防护：canonicalize 后必须仍在 pagesRoot 内，否则视同未命中（走兜底）。
//! - vendor：无 SPA 兜底——未命中/空路径/穿越 → 404 JSON `{"error":"not found"}`；
//!   vendor 目录不存在时跳过 vendor 语义（落 pages 兜底，TS 现存行为）。
//!   实现说明：vendor 与 pages 都由 `dispatch_unmatched`（fallback）统一分发，
//!   等价于 TS 的 `app.use('/vendor') → app.use(pages)` 挂载顺序。
//! - MIME 10 项 + 未知扩展 octet-stream；无缓存头（与 TS 对齐）。

use crate::AppState;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::path::PathBuf;

/// MIME 表（10 项；未知扩展 → application/octet-stream）
fn mime_for(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

fn json_404() -> Response {
    (StatusCode::NOT_FOUND, axum::Json(json!({"error": "not found"}))).into_response()
}

fn file_response(p: &PathBuf) -> Response {
    match std::fs::read(p) {
        Ok(bytes) => {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
            // 原型工具：页面迭代频繁且无版本化 URL，禁缓存避免浏览器启发式缓存旧页
            // （无该头时 Chrome 按 Last-Modified 启发式缓存，页面更新后用户看到旧版）
            (
                [
                    (header::CONTENT_TYPE, mime_for(&ext)),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("read error: {e}")).into_response(),
    }
}

/// 在 root 下解析 URI 路径：decode → 剥前导 `/`（空 → index.html）→
/// canonicalize 且必须仍在 root 内。任何失败返回 None（视同未命中）。
fn resolve_under(root: &PathBuf, uri_path: &str) -> Option<PathBuf> {
    let decoded = percent_encoding::percent_decode_str(uri_path).decode_utf8().ok()?.into_owned();
    let rel = decoded.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let root_canon = root.canonicalize().ok()?;
    let canon = root.join(rel).canonicalize().ok()?;
    if canon.starts_with(&root_canon) {
        Some(canon)
    } else {
        None
    }
}

/// 未命中路由的统一分发（fallback）：vendor 语义优先于 pages 兜底。
pub async fn dispatch_unmatched(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let path = uri.path().to_string();
    let vendor_dir = state.project_root.join("vendor");
    if is_vendor_path(&path) && vendor_dir.is_dir() {
        return serve_vendor(&state, &path);
    }
    serve_pages(&state, &path)
}

/// /vendor 段边界判定：精确 `/vendor` 或 `/vendor/` 前缀。
/// `/vendorfoo.js` 等同段前缀的路径不属于 vendor 语义，应落 pages 兜底。
fn is_vendor_path(path: &str) -> bool {
    path == "/vendor" || path.starts_with("/vendor/")
}

/// vendor 语义：命中 → 文件；未命中/空路径/穿越 → 404 JSON（无 SPA 兜底）。
fn serve_vendor(state: &AppState, path: &str) -> Response {
    let vendor_root = state.project_root.join("vendor");
    // decode（与 pages 层一致；TS 在 vendor 分发前同样 decode）；失败 → 404 JSON
    let decoded = match percent_encoding::percent_decode_str(path).decode_utf8() {
        Ok(d) => d.into_owned(),
        Err(_) => return json_404(),
    };
    let rel = decoded.strip_prefix("/vendor").unwrap_or("").trim_start_matches('/');
    if rel.is_empty() {
        return json_404();
    }
    let root_canon = match vendor_root.canonicalize() {
        Ok(c) => c,
        Err(_) => return json_404(),
    };
    let canon = match vendor_root.join(rel).canonicalize() {
        Ok(c) => c,
        Err(_) => return json_404(),
    };
    if !canon.starts_with(&root_canon) || !canon.is_file() {
        return json_404();
    }
    file_response(&canon)
}

/// pages 兜底：根映射 → SPA 兜底 → 404 文本。
fn serve_pages(state: &AppState, path: &str) -> Response {
    let pages_root = state.project_root.join("pages");
    let mut served = resolve_under(&pages_root, path);
    if served.is_none() {
        // SPA 兜底：pages/index.html 存在 → 返回它
        let index = pages_root.join("index.html");
        if index.is_file() {
            served = Some(index);
        }
    }
    match served {
        Some(p) => file_response(&p),
        None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
    }
}
