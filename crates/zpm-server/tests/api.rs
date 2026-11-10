//! API 层集成测试（real 模式 + 错误契约 + _meta + cookie）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tower::ServiceExt; // oneshot
use zpm_core::project_config::{FeaturesCfg, SqliteFile, Target, WriteMode};
use zpm_core::store::{open_store_for, LogicalType, Row, Value as SValue};
use zpm_server::{build_router, AppState};

type App = axum::Router;

async fn send(
    app: &App,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    let req = match body {
        Some(v) => builder.body(Body::from(v.to_string())).unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, v)
}

fn row(id: i64, name: &str, amount: f64) -> Row {
    let mut r = Row::new();
    r.insert("id".into(), SValue::Int(id));
    r.insert("name".into(), SValue::Text(name.into()));
    r.insert("amount".into(), SValue::Real(amount));
    r
}

/// 造 app：users(id INTEGER pk, name TEXT, amount REAL) 种子行 (1,'aa',5) (2,'bb',15) (3,'cc',25)
fn make_app(
    tmp: &std::path::Path,
    write_enabled: bool,
    writable: &[&str],
    features: FeaturesCfg,
) -> App {
    let mut store = open_store_for(
        &Target::Sqlite(SqliteFile { file: "app.db".into() }),
        tmp,
    )
    .unwrap();
    store
        .create_table(
            "users",
            &[
                ("id".into(), LogicalType::Integer),
                ("name".into(), LogicalType::Text),
                ("amount".into(), LogicalType::Real),
            ],
            "id",
        )
        .unwrap();
    store
        .insert_rows(
            "users",
            &[row(1, "aa", 5.0), row(2, "bb", 15.0), row(3, "cc", 25.0)],
        )
        .unwrap();
    build_router(AppState {
        project_root: tmp.to_path_buf(),
        name: "smoke".into(),
        theme: "default".into(),
        store: Arc::new(Mutex::new(store)),
        write_enabled,
        write_mode: WriteMode::Real,
        writable_tables: Arc::new(writable.iter().map(|s| s.to_string()).collect()),
        features,
        session: Arc::new(zpm_server::session::SessionLayer::new()),
    })
}

/// meta：write 关闭 → disabled；write 开启+白名单 → real + writableTables
#[tokio::test]
async fn meta_write_modes() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path(), false, &[], FeaturesCfg::default());
    let (st, _, v) = send(&app, "GET", "/api/_meta", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "smoke");
    assert_eq!(v["tables"][0]["name"], "users");
    assert_eq!(v["tables"][0]["primaryKey"], "id");
    assert_eq!(v["tables"][0]["columns"], json!(["id", "name", "amount"]));
    assert_eq!(v["writableTables"], json!([]));
    assert_eq!(v["writeMode"], "disabled");
    // features 缺省（默认构造）→ 三 true
    assert_eq!(v["features"]["workbench"], true);
    assert_eq!(v["features"]["db_info"], true);
    assert_eq!(v["features"]["theme_studio"], true);

    let tmp2 = tempdir().unwrap();
    let app = make_app(tmp2.path(), true, &["users"], FeaturesCfg::default());
    let (st, _, v) = send(&app, "GET", "/api/_meta", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["writableTables"], json!(["users"]));
    assert_eq!(v["writeMode"], "real");
}

/// _dict：单请求返回全部表结构 + 实时行数（数据字典页聚合端点，替代 _meta + N 次行数请求）
#[tokio::test]
async fn dict_returns_tables_with_row_counts() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path(), false, &[], FeaturesCfg::default());
    let (st, _, v) = send(&app, "GET", "/api/_dict", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "smoke");
    assert_eq!(v["tables"][0]["name"], "users");
    assert_eq!(v["tables"][0]["primaryKey"], "id");
    assert_eq!(v["tables"][0]["columns"], json!(["id", "name", "amount"]));
    assert_eq!(v["tables"][0]["rows"], 3, "行数应为实时 COUNT 结果");
    // features 缺省（默认构造）→ 三 true
    assert_eq!(v["features"]["workbench"], true);
    assert_eq!(v["features"]["db_info"], true);
    assert_eq!(v["features"]["theme_studio"], true);
}

/// features 显式关闭 → /api/_meta 透出 false（页面/菜单据此隐藏对应入口）
#[tokio::test]
async fn meta_features_disabled() {
    let tmp = tempdir().unwrap();
    let app = make_app(
        tmp.path(),
        false,
        &[],
        FeaturesCfg { workbench: false, db_info: false, theme_studio: true },
    );
    let (st, _, v) = send(&app, "GET", "/api/_meta", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["features"]["workbench"], false);
    assert_eq!(v["features"]["db_info"], false);
    assert_eq!(v["features"]["theme_studio"], true);
}

/// 列表过滤/排序/分页 + total 独立性 + 错误契约 + 写路径全链
#[tokio::test]
async fn crud_query_and_error_contract() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path(), true, &["users"], FeaturesCfg::default());

    // 过滤+排序+分页：total 独立于分页
    let (st, _, v) = send(
        &app,
        "GET",
        "/api/users?where=amount:gt:10&sort=-amount&page=1&pageSize=1",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["total"], 2, "where 命中数与分页无关");
    assert_eq!(v["data"].as_array().unwrap().len(), 1);
    assert_eq!(v["data"][0]["name"], "cc", "sort=-amount 首行应为最大值");

    // like 双侧通配
    let (_, _, v) = send(&app, "GET", "/api/users?where=name:like:a", None).await;
    assert_eq!(v["total"], 1);
    assert_eq!(v["data"][0]["name"], "aa");

    // in 多值（含空格保留语义）
    let (_, _, v) = send(&app, "GET", "/api/users?where=id:in:1,3", None).await;
    assert_eq!(v["total"], 2);
    assert_eq!(v["data"][0]["id"], 1);
    assert_eq!(v["data"][1]["id"], 3);

    // 单条：命中 / 未命中
    let (st, _, v) = send(&app, "GET", "/api/users/1", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "aa");
    let (st, _, v) = send(&app, "GET", "/api/users/999", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "row not found");

    // POST 201 回显入参（不回读）
    let body = json!({"id": 9, "name": "xx", "amount": 1.5});
    let (st, _, v) = send(&app, "POST", "/api/users", Some(body.clone())).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(v, body, "POST 必须回显入参");

    // PATCH 回读完整行；空 patch 返回当前行
    let (st, _, v) = send(&app, "PATCH", "/api/users/9", Some(json!({"name": "yy"}))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "yy");
    assert_eq!(v["amount"], 1.5, "PATCH 应回读完整行");
    let (st, _, v) = send(&app, "PATCH", "/api/users/9", Some(json!({}))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "yy", "空 patch 返回当前行");

    // DELETE → {"ok":true}，再查 404
    let (st, _, v) = send(&app, "DELETE", "/api/users/9", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["ok"], true);
    let (st, _, _) = send(&app, "GET", "/api/users/9", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // body 数组 → 400
    let (st, _, v) = send(&app, "POST", "/api/users", Some(json!([1, 2]))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "json object body required");

    // 畸形表名 → 404（不是 500/SQL 错误）
    let (st, _, v) = send(&app, "GET", "/api/users;drop", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "table not found: users;drop");

    // filter 校验：未知字段 / 未知操作符（TS 消息带明细后缀）
    let (st, _, v) = send(&app, "GET", "/api/users?where=nope:eq:1", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().starts_with("unknown field: nope"));
    let (st, _, v) = send(&app, "GET", "/api/users?where=name:zz:1", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().starts_with("invalid op: zz"));
    let (st, _, v) = send(&app, "GET", "/api/users?where=name:eq", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["error"].as_str().unwrap().starts_with("invalid where: name:eq"));

    // 两个 where 参数 AND（契约：where 可重复）
    let (_, _, v) = send(
        &app,
        "GET",
        "/api/users?where=id:in:1,2,3&where=name:like:bb",
        None,
    )
    .await;
    assert_eq!(v["total"], 1);
    assert_eq!(v["data"][0]["id"], 2);
}

/// 写守卫：write 关闭 → 403 table not writable；白名单外表 → 404 表不存在（先于守卫，对齐 TS）
#[tokio::test]
async fn write_guard_403() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path(), false, &[], FeaturesCfg::default());
    let (st, _, v) = send(&app, "POST", "/api/users", Some(json!({"id": 5}))).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(v["error"], "table not writable: users");

    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path(), true, &["users"], FeaturesCfg::default());
    // 表不存在时 404 先于守卫 403（TS app.ts findTable 顺序）
    let (st, _, v) = send(&app, "POST", "/api/orders", Some(json!({"id": 5}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "table not found: orders");
}

/// 会话 cookie：首次 /api 请求种 zpm_sid（HttpOnly）；已有则复用不重设
#[tokio::test]
async fn session_cookie_set_and_reuse() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path(), false, &[], FeaturesCfg::default());
    let (st, headers, _) = send(&app, "GET", "/api/_meta", None).await;
    assert_eq!(st, StatusCode::OK);
    let cookie = headers
        .get("set-cookie")
        .map(|v| v.to_str().unwrap().to_string())
        .expect("首次请求应种 cookie");
    assert!(cookie.starts_with("zpm_sid="));
    assert!(cookie.contains("HttpOnly"));

    // 带 cookie 复用：不重设 Set-Cookie
    let sid = cookie.split(';').next().unwrap().to_string();
    let (_, headers2, _) = send_with_cookie(&app, "GET", "/api/_meta", &sid, None).await;
    assert!(
        headers2.get("set-cookie").is_none(),
        "已有 zpm_sid 时不得重设 cookie"
    );
}

async fn send_with_cookie(
    app: &App,
    method: &str,
    uri: &str,
    cookie: &str,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("cookie", cookie);
    let req = match body {
        Some(v) => builder.body(Body::from(v.to_string())).unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, v)
}

/// 会话重置端点（real 模式 no-op，恒 {ok:true}）
#[tokio::test]
async fn session_reset_noop() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path(), true, &["users"], FeaturesCfg::default());
    let (st, _, v) = send(&app, "POST", "/api/_session/reset", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["ok"], true);
}
