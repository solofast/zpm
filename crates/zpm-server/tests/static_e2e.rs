//! 静态三层行为测试（对齐 TS 契约 + 规格 §11 验收标准）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::fs;
use tempfile::tempdir;
use tower::ServiceExt; // oneshot
use zpm_server::{build_router, AppState};

type App = axum::Router;

async fn get(app: App, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn app(dir: &std::path::Path, theme: &str, with_vendor: bool) -> App {
    seed_project(dir, with_vendor);
    build_router(AppState {
        project_root: dir.to_path_buf(),
        name: "smoke".into(),
        theme: theme.into(),
        store: std::sync::Arc::new(std::sync::Mutex::new(
            zpm_core::store::open_store_for(
                &zpm_core::project_config::Target::Sqlite(zpm_core::project_config::SqliteFile {
                    file: "app.db".into(),
                }),
                dir,
            )
            .unwrap(),
        )),
        write_enabled: false,
        write_mode: zpm_core::project_config::WriteMode::Simulated,
        writable_tables: std::sync::Arc::new(vec![]),
        features: Default::default(),
        session: std::sync::Arc::new(zpm_server::session::SessionLayer::new()),
    })
}

fn seed_project(dir: &std::path::Path, with_vendor: bool) {
    fs::create_dir_all(dir.join("pages/layout")).unwrap();
    fs::write(dir.join("pages/index.html"), "INDEX-MARKER").unwrap();
    fs::write(dir.join("pages/orders.html"), "ORDERS-MARKER").unwrap();
    fs::write(dir.join("pages/layout/layout.js"), "LAYOUT-JS").unwrap();
    fs::write(dir.join("pages/font.woff2"), "WW").unwrap();
    fs::write(dir.join("pages/blob.bin"), "BIN").unwrap();
    if with_vendor {
        fs::create_dir_all(dir.join("vendor")).unwrap();
        fs::write(dir.join("vendor/x.js"), "VENDOR-X").unwrap();
    }
    fs::create_dir_all(dir.join("themes")).unwrap();
    fs::write(dir.join("themes/lyra.css"), "LYRA-TOKENS").unwrap();
    fs::write(dir.join("secret.txt"), "SECRET-MARKER").unwrap();
}

/// ① pages 根映射：/ → index.html；子路径映射；只有根映射（无 /pages/ 别名）
#[tokio::test]
async fn pages_root_mapping_and_spa_fallback() {
    let tmp = tempdir().unwrap();
    let app = app(tmp.path(), "default", true);

    let (st, headers, body) = get(app.clone(), "/").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "INDEX-MARKER");
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");

    let (st, _, body) = get(app.clone(), "/layout/layout.js").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "LAYOUT-JS");

    // /pages/orders.html 无别名 → 落 SPA 兜底返回 index.html（保留 TS 行为，勿"修复"）
    let (st, _, body) = get(app, "/pages/orders.html").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "INDEX-MARKER", "/pages/ 路径应落 SPA 兜底（返回 index.html）");
}

/// ② SPA 兜底：未命中的 html 也返回 index.html；全无 → 404 文本
#[tokio::test]
async fn spa_fallback_and_404() {
    let tmp = tempdir().unwrap();
    let app = app(tmp.path(), "default", true);
    let (st, _, body) = get(app, "/missing-page.html").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "INDEX-MARKER", "SPA 兜底应返回 index.html");
}

/// 审查重点 5 + 规格：/vendor 未命中必须 404 JSON，不得落 SPA 兜底
#[tokio::test]
async fn vendor_miss_returns_json_404_no_spa_fallback() {
    let tmp = tempdir().unwrap();
    let app = app(tmp.path(), "default", true);

    let (st, headers, body) = get(app.clone(), "/vendor/missing.js").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body, r#"{"error":"not found"}"#);
    assert_eq!(headers["content-type"], "application/json");
    assert!(!body.contains("INDEX"), "vendor 未命中不得落 SPA 兜底");

    // /vendor 与 /vendor/（空路径）同样 404 JSON
    for uri in ["/vendor", "/vendor/"] {
        let (st, _, body) = get(app.clone(), uri).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "uri={uri}");
        assert_eq!(body, r#"{"error":"not found"}"#, "uri={uri}");
    }

    // vendor 命中正常服务
    let (st, _, body) = get(app.clone(), "/vendor/x.js").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "VENDOR-X");

    // 段边界：/vendorfoo.js 不属于 vendor 语义 → 落 pages 兜底（不返回 404 JSON）
    let (st, _, body) = get(app.clone(), "/vendorfoo.js").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "INDEX-MARKER", "/vendorfoo.js 应落 pages 兜底");
    let (st, _, body) = get(app, "/vendor-config.json").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "INDEX-MARKER", "/vendor-config.json 应落 pages 兜底");
}

/// vendor 路径 percent-decode（与 pages 层/TS 一致）：编码文件名可命中
#[tokio::test]
async fn vendor_percent_decoded_path() {
    let tmp = tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("vendor")).unwrap();
    fs::write(tmp.path().join("vendor/my lib.js"), "DECODED").unwrap();
    fs::create_dir_all(tmp.path().join("pages")).unwrap();
    fs::write(tmp.path().join("pages/index.html"), "INDEX-MARKER").unwrap();
    let router = build_router(AppState {
        project_root: tmp.path().to_path_buf(),
        name: "s".into(),
        theme: "default".into(),
        store: std::sync::Arc::new(std::sync::Mutex::new(
            zpm_core::store::open_store_for(
                &zpm_core::project_config::Target::Sqlite(zpm_core::project_config::SqliteFile {
                    file: "app.db".into(),
                }),
                tmp.path(),
            )
            .unwrap(),
        )),
        write_enabled: false,
        write_mode: zpm_core::project_config::WriteMode::Simulated,
        writable_tables: std::sync::Arc::new(vec![]),
        features: Default::default(),
        session: std::sync::Arc::new(zpm_server::session::SessionLayer::new()),
    });
    let (st, _, body) = get(router, "/vendor/my%20lib.js").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "DECODED");
}

/// vendor 目录不存在时放行给 pages 兜底（TS 现存行为）
#[tokio::test]
async fn vendor_missing_dir_falls_to_pages() {
    let tmp = tempdir().unwrap();
    let app = app(tmp.path(), "default", false);
    let (st, _, body) = get(app, "/vendor/missing.js").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "INDEX-MARKER", "vendor 目录不存在时落 SPA 兜底");
}

/// ④ theme：default 内嵌 / 相对路径按工程目录 / 未命中 404 文本
#[tokio::test]
async fn theme_three_states() {
    let tmp = tempdir().unwrap();
    let router = app(tmp.path(), "default", true);
    let (st, headers, body) = get(router, "/theme.css").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/css; charset=utf-8");
    assert!(!body.is_empty(), "内置 default 主题应为非空 tokens.css");

    // 相对路径按工程目录解析（有意修正 TS 按 cwd 的缺陷）
    let router = app(tmp.path(), "themes/lyra.css", true);
    let (st, _, body) = get(router, "/theme.css").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body, "LYRA-TOKENS");

    // 未命中 → 404 纯文本
    let router = app(tmp.path(), "themes/nope.css", true);
    let (st, _, body) = get(router, "/theme.css").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(body.contains("theme not found: themes/nope.css"), "实际：{body}");
}

/// ⑤ MIME 表：woff2 → font/woff2；未知扩展 → application/octet-stream
#[tokio::test]
async fn mime_types() {
    let tmp = tempdir().unwrap();
    let app = app(tmp.path(), "default", true);
    let (_, headers, _) = get(app.clone(), "/font.woff2").await;
    assert_eq!(headers["content-type"], "font/woff2");
    let (_, headers, _) = get(app, "/blob.bin").await;
    assert_eq!(headers["content-type"], "application/octet-stream");
}

/// 穿越防护：/../secret 不得泄漏项目外文件
#[tokio::test]
async fn traversal_does_not_leak() {
    let tmp = tempdir().unwrap();
    let app = app(tmp.path(), "default", true);
    for uri in ["/../secret.txt", "/%2e%2e/secret.txt"] {
        let (st, _, body) = get(app.clone(), uri).await;
        assert!(!body.contains("SECRET-MARKER"), "uri={uri} 泄漏：{body}");
        // 要么 SPA 兜底（index）要么 404
        assert!(
            st == StatusCode::OK || st == StatusCode::NOT_FOUND,
            "uri={uri} status={st}"
        );
    }
}

/// 静态文件响应带 Cache-Control: no-store（原型迭代频繁且无版本化 URL，
/// 缺头时浏览器启发式缓存会长期展示旧页面——demo4 实测踩坑）
#[tokio::test]
async fn pages_have_no_store_cache_control() {
    let tmp = tempdir().unwrap();
    seed_project(&tmp.path(), false);
    let app = app(&tmp.path(), "default", false);
    let (_, h, _) = get(app.clone(), "/index.html").await;
    assert_eq!(h.get("cache-control").map(|v| v.to_str().unwrap()), Some("no-store"));
    let (_, h, _) = get(app, "/layout/layout.js").await;
    assert_eq!(h.get("cache-control").map(|v| v.to_str().unwrap()), Some("no-store"));
}
