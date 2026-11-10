//! simulated 会话覆盖层集成测试（规格 §10.3 / TS session.ts 契约）。

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tower::ServiceExt; // oneshot
use zpm_core::project_config::{SqliteFile, Target, WriteMode};
use zpm_core::store::{open_store_for, LogicalType, Row, Value as SValue};
use zpm_server::{build_router, AppState};

type App = axum::Router;

async fn send(
    app: &App,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", c);
    }
    builder = builder.header("content-type", "application/json");
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

/// 捕获响应的 zpm_sid cookie（如 "zpm_sid=<uuid>; Path=/; HttpOnly"）
fn sid_cookie(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .expect("首次请求应种 zpm_sid cookie")
        .to_string()
}

fn row(id: i64, name: &str, amount: f64) -> Row {
    let mut r = Row::new();
    r.insert("id".into(), SValue::Int(id));
    r.insert("name".into(), SValue::Text(name.into()));
    r.insert("amount".into(), SValue::Real(amount));
    r
}

/// 造 simulated app：write_enabled=true + mode=Simulated + tables=[users]；种子 (1,aa,5)(2,bb,15)
fn make_app(tmp: &std::path::Path) -> App {
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
        .create_table(
            "audit",
            &[("id".into(), LogicalType::Integer)],
            "id",
        )
        .unwrap();
    store
        .insert_rows("users", &[row(1, "aa", 5.0), row(2, "bb", 15.0)])
        .unwrap();
    build_router(AppState {
        project_root: tmp.to_path_buf(),
        name: "smoke".into(),
        theme: "default".into(),
        store: Arc::new(Mutex::new(store)),
        write_enabled: true,
        write_mode: WriteMode::Simulated,
        writable_tables: Arc::new(vec!["users".into()]),
        features: Default::default(),
        session: Arc::new(zpm_server::session::SessionLayer::new()),
    })
}

/// ① 审查重点 1：simulated insert 只写内存会话，目标库不变；同会话可见 +1
#[tokio::test]
async fn simulated_insert_leaves_db_unchanged() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path());

    let (_, _, v0) = send(&app, "GET", "/api/users", None, None).await;
    assert_eq!(v0["total"], 2);

    // POST（新会话 A）
    let (st, headers, v) = send(
        &app,
        "POST",
        "/api/users",
        None,
        Some(json!({"id": 9, "name": "ins", "amount": 1.5})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(v["id"], 9, "POST 201 回显入参");
    let cookie_a = sid_cookie(&headers);

    // 无 cookie：目标库 total 不变（模拟写入永不落库）
    let (_, _, v_db) = send(&app, "GET", "/api/users", None, None).await;
    assert_eq!(v_db["total"], 2, "simulated 写入不得改变目标库");

    // 同会话（cookie A）：合并后 total+1
    let (_, _, v_a) = send(&app, "GET", "/api/users", Some(&cookie_a), None).await;
    assert_eq!(v_a["total"], 3);
}

/// ② 会话 inserts 不参与 where 过滤（尾部追加）；仅 sort[0] 生效；total=合并后总数
#[tokio::test]
async fn session_inserts_skip_where_filter() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path());

    // 会话 A：插入 name=ins（where=name:eq:zzz 不匹配它，但它仍应出现在 data 中）
    let (st, headers, _) = send(
        &app,
        "POST",
        "/api/users",
        None,
        Some(json!({"id": 9, "name": "ins", "amount": 9.0})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let cookie_a = sid_cookie(&headers);

    let (_, _, v) = send(
        &app,
        "GET",
        "/api/users?where=name:eq:zzz&sort=-id",
        Some(&cookie_a),
        None,
    )
    .await;
    // TS 契约：where 只作用于库内行（aa/bb 不命中 → 0），会话 inserts 绕过滤追加 → total 1
    assert_eq!(v["total"], 1, "total = 库内命中(0) + 会话 inserts(1)");
    let data = v["data"].as_array().unwrap();
    assert!(data.iter().any(|r| r["name"] == "ins"), "会话 inserts 不参与 where 过滤，仍出现在 data 中");
    assert_eq!(data[0]["id"], 9, "仅 sort[0]（-id）生效");
}

/// ③ insert 缺主键 → 400 missing primary key value
#[tokio::test]
async fn insert_missing_pk_400() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path());
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/users",
        None,
        Some(json!({"name": "no-pk"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "missing primary key value: id");
}

/// ④ 会话隔离：两个 sid 各自 insert 互不可见
#[tokio::test]
async fn sessions_are_isolated() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path());

    // 会话 A
    let (_, h_a, _) = send(&app, "POST", "/api/users", None, Some(json!({"id": 10, "name": "a10", "amount": 0.0}))).await;
    let cookie_a = sid_cookie(&h_a);
    // 会话 B（显式指定另一个 sid）
    let cookie_b = "zpm_sid=session-b";
    let (_, _, _) = send(&app, "POST", "/api/users", Some(cookie_b), Some(json!({"id": 11, "name": "b11", "amount": 0.0}))).await;

    let (_, _, va) = send(&app, "GET", "/api/users", Some(&cookie_a), None).await;
    let (_, _, vb) = send(&app, "GET", "/api/users", Some(cookie_b), None).await;
    let ids_a: Vec<i64> = va["data"].as_array().unwrap().iter().map(|r| r["id"].as_i64().unwrap()).collect();
    let ids_b: Vec<i64> = vb["data"].as_array().unwrap().iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert!(ids_a.contains(&10) && !ids_a.contains(&11), "A 不见 B 的写入：{ids_a:?}");
    assert!(ids_b.contains(&11) && !ids_b.contains(&10), "B 不见 A 的写入：{ids_b:?}");
}

/// ⑤ reset 后该会话 insert 消失、库仍不变
#[tokio::test]
async fn reset_clears_session_only() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path());

    let (_, h_a, _) = send(&app, "POST", "/api/users", None, Some(json!({"id": 10, "name": "a10", "amount": 0.0}))).await;
    let cookie_a = sid_cookie(&h_a);

    // reset（带会话 cookie）
    let (_, _, v) = send(&app, "POST", "/api/_session/reset", Some(&cookie_a), None).await;
    assert_eq!(v["ok"], true);

    let (_, _, va) = send(&app, "GET", "/api/users", Some(&cookie_a), None).await;
    assert_eq!(va["total"], 2, "reset 后会话 insert 消失");
    let (_, _, vdb) = send(&app, "GET", "/api/users", None, None).await;
    assert_eq!(vdb["total"], 2, "目标库始终未变");
}

/// ⑥ update/delete 走覆盖层：库内行 PATCH/DELETE 会话内可见、库不变；库内不存在 → 404
#[tokio::test]
async fn update_and_delete_via_overlay() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path());

    let (_, h_a, _) = send(&app, "GET", "/api/users", None, None).await;
    let cookie_a = sid_cookie(&h_a);

    // PATCH 库内行：会话内可见，库不变
    let (st, _, v) = send(&app, "PATCH", "/api/users/1", Some(&cookie_a), Some(json!({"name": "mut"}))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "mut", "PATCH 回读合并后的完整行");
    let (_, _, v) = send(&app, "GET", "/api/users/1", Some(&cookie_a), None).await;
    assert_eq!(v["name"], "mut");
    let (_, _, v) = send(&app, "GET", "/api/users/1", None, None).await;
    assert_eq!(v["name"], "aa", "库内行未被真正修改");

    // DELETE 库内行：会话内隐藏，库不变
    let (st, _, _) = send(&app, "DELETE", "/api/users/1", Some(&cookie_a), None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, _) = send(&app, "GET", "/api/users/1", Some(&cookie_a), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, v) = send(&app, "GET", "/api/users/1", None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "aa");

    // 库内不存在的行 → 404
    let (st, _, _) = send(&app, "PATCH", "/api/users/999", Some(&cookie_a), Some(json!({"name": "x"}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, _) = send(&app, "DELETE", "/api/users/999", Some(&cookie_a), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // 删除后 PATCH → 404（deletes 检查；已删除行不得被 PATCH 复活）
    let (st, _, _) = send(
        &app,
        "PATCH",
        "/api/users/1",
        Some(&cookie_a),
        Some(json!({"name": "zombie"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // 会话 insert 的 update/delete 直接作用于会话行
    let (_, _, _) = send(&app, "POST", "/api/users", Some(&cookie_a), Some(json!({"id": 20, "name": "s20", "amount": 0.0}))).await;
    let (st, _, v) = send(&app, "PATCH", "/api/users/20", Some(&cookie_a), Some(json!({"name": "s20b"}))).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "s20b");
    let (st, _, _) = send(&app, "DELETE", "/api/users/20", Some(&cookie_a), None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _, _) = send(&app, "GET", "/api/users/20", Some(&cookie_a), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "会话 insert 被 delete 真移除");
}

/// ⑦ simulated 纯读不铸造会话状态（两次纯读行为等价 + reset 前后无差异 + 无条目铸造）
#[tokio::test]
async fn pure_read_does_not_mint_session() {
    let tmp = tempdir().unwrap();
    let layer = Arc::new(zpm_server::session::SessionLayer::new());
    let mut store = open_store_for(
        &Target::Sqlite(SqliteFile { file: "app.db".into() }),
        tmp.path(),
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
        .insert_rows("users", &[row(1, "aa", 5.0), row(2, "bb", 15.0)])
        .unwrap();
    let app = build_router(AppState {
        project_root: tmp.path().to_path_buf(),
        name: "smoke".into(),
        theme: "default".into(),
        store: Arc::new(Mutex::new(store)),
        write_enabled: true,
        write_mode: WriteMode::Simulated,
        writable_tables: Arc::new(vec!["users".into()]),
        features: Default::default(),
        session: layer.clone(),
    });

    // 显式新 cookie 纯读两次
    let cookie = "zpm_sid=pure-read-fresh";
    let (_, _, v1) = send(&app, "GET", "/api/users", Some(cookie), None).await;
    let (_, _, v2) = send(&app, "GET", "/api/users", Some(cookie), None).await;
    assert_eq!(v1, v2, "纯读不产生会话状态，两次结果完全一致");

    // 防铸造回归检测：纯读后该 sid 不得有会话状态
    assert!(!layer.has_state("pure-read-fresh"), "纯读铸造了会话条目（内存泄漏回归）");

    // reset 前后等价
    let (_, _, _) = send(&app, "POST", "/api/_session/reset", Some(cookie), None).await;
    let (_, _, v3) = send(&app, "GET", "/api/users", Some(cookie), None).await;
    assert_eq!(v1, v3);
}

/// ⑧ 白名单外表 → 403（表存在但未列入 write.tables）；不存在的表 → 404（findTable 先行）
#[tokio::test]
async fn non_whitelisted_table_403() {
    let tmp = tempdir().unwrap();
    let app = make_app(tmp.path());
    // audit 表存在于库内（make_app 种子），但不在 write.tables 白名单 → 403
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/audit",
        None,
        Some(json!({"id": 5})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(v["error"], "table not writable: audit");

    // 不存在的表 → 404（表存在性校验先于写守卫，对齐 TS findTable 顺序）
    let (st, _, v) = send(
        &app,
        "POST",
        "/api/orders",
        None,
        Some(json!({"id": 5})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "table not found: orders");
}
