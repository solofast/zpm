//! /api 层：通用 CRUD + _meta + 会话 cookie（TS app.ts/store.ts 契约）。
//!
//! 错误统一 `{"error":"<msg>"}`：NotFound→404、Forbidden→403、BadRequest→400、其余→500。
//! Cookie（仅 /api）：无 `zpm_sid` 时生成 uuid v4 并 `Set-Cookie: zpm_sid=<uuid>; Path=/; HttpOnly`；
//! 已有则复用不重设。写路径守卫：表不在 write.tables 白名单 → 403 `table not writable`。
use crate::filter;
use crate::session;
use crate::AppState;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde_json::{json, Value};
use zpm_core::error::ZpmError;
use zpm_core::project_config::WriteMode;
use zpm_core::store::{Row, Store, Value as SValue};

/// /api 子路由（嵌套到根 /api 下）
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/_meta", get(meta))
        .route("/_dict", get(dict))
        .route("/_session/reset", post(session_reset))
        .route("/{table}", get(list).post(create))
        .route("/{table}/{id}", get(get_one).patch(patch).delete(remove_one))
}

/// 会话 cookie 中间件：无 `zpm_sid` 时生成并 Set-Cookie；已有则复用不重设。
/// sid 存入 request extensions 供后续（任务 13 simulated 会话层）使用。
pub async fn ensure_sid(
    State(_state): State<AppState>,
    mut req: Request,
    next: axum::middleware::Next,
) -> Response {
    let existing = req
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(';').find_map(|c| {
                c.trim().strip_prefix("zpm_sid=").map(str::to_string)
            })
        });
    let created = existing.is_none();
    let sid = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    req.extensions_mut().insert(sid.clone());

    let mut res = next.run(req).await;
    // API 响应禁止缓存：浏览器启发式缓存会让切换 target/重新 sync 后仍显示旧表清单
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    if created {
        if let Ok(hv) = HeaderValue::from_str(&format!("zpm_sid={sid}; Path=/; HttpOnly")) {
            res.headers_mut().append(header::SET_COOKIE, hv);
        }
    }
    res
}

/// ZpmError → API 错误响应
pub fn error_response(e: ZpmError) -> Response {
    let (status, msg) = match &e {
        ZpmError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
        ZpmError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
        ZpmError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
        other => (StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    };
    (status, Json(json!({ "error": msg }))).into_response()
}

/// 查询串解析：& 切分 → k=v 对（percent-decode；+ 视作空格，对齐 Hono query 语义）
fn parse_query(q: Option<&str>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(q) = q else { return out };
    for pair in q.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        let dec = |s: &str| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8()
                .map(|d| d.replace('+', " "))
                .unwrap_or_else(|_| s.replace('+', " "))
        };
        out.push((dec(k), dec(v)));
    }
    out
}

fn get_param(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// 表校验：safe_ident 后用 describe_table + table_pk 定向查询（各 1 次往返，
/// 不做 list_tables 全量扫描）；表不存在 → 404 `table not found: <t>`。
fn check_table(state: &AppState, table: &str) -> Result<zpm_core::store::TableInfo, ZpmError> {
    if zpm_core::store::safe_ident(table).is_err() {
        return Err(ZpmError::NotFound(format!("table not found: {table}")));
    }
    let mut s = state.lock_store();
    let columns = s.describe_table(table)?;
    let primary_key = s.table_pk(table)?;
    Ok(zpm_core::store::TableInfo {
        name: table.to_string(),
        primary_key,
        columns: columns.into_iter().map(|(n, _)| n).collect(),
    })
}

/// 写守卫：表必须在 write.tables 白名单内（写路径先于表存在性校验，对齐 TS assertWritable）
fn assert_writable(state: &AppState, table: &str) -> Result<(), ZpmError> {
    if !state.write_enabled || !state.writable_tables.iter().any(|t| t == table) {
        return Err(ZpmError::Forbidden(format!("table not writable: {table}")));
    }
    Ok(())
}

// --- 值/行与 JSON 互转 -------------------------------------------------------

fn svalue_to_json(v: &SValue) -> Value {
    match v {
        SValue::Null => json!(null),
        SValue::Int(i) => json!(i),
        SValue::Real(f) => json!(f),
        SValue::Text(s) => json!(s),
        SValue::Blob(b) => json!(hex::encode(b)),
    }
}

fn row_to_json(r: &Row) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in r {
        m.insert(k.clone(), svalue_to_json(v));
    }
    Value::Object(m)
}

fn json_to_svalue(v: &Value) -> SValue {
    match v {
        Value::Null => SValue::Null,
        Value::Bool(b) => SValue::Int(i64::from(*b)),
        Value::Number(n) => n
            .as_i64()
            .map(SValue::Int)
            .unwrap_or_else(|| SValue::Real(n.as_f64().unwrap_or_default())),
        Value::String(s) => SValue::Text(s.clone()),
        other => SValue::Text(other.to_string()),
    }
}

fn json_obj_to_row(obj: &serde_json::Map<String, Value>) -> Row {
    let mut row = Row::new();
    for (k, v) in obj {
        row.insert(k.clone(), json_to_svalue(v));
    }
    row
}

// --- 处理器 -----------------------------------------------------------------

/// GET /api/_meta：{name, tables:[{name,primaryKey,columns}], writableTables, writeMode}
async fn meta(State(state): State<AppState>) -> Response {
    let tables = match state.lock_store().list_tables() {
        Ok(t) => t,
        Err(e) => return error_response(e),
    };
    let writable: Vec<&String> = if state.write_enabled {
        state.writable_tables.iter().collect()
    } else {
        Vec::new()
    };
    let mode = if writable.is_empty() {
        "disabled"
    } else {
        match state.write_mode {
            zpm_core::project_config::WriteMode::Real => "real",
            zpm_core::project_config::WriteMode::Simulated => "simulated",
        }
    };
    let tjson: Vec<Value> = tables
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "primaryKey": t.primary_key,
                "columns": t.columns,
            })
        })
        .collect();
    (
        Json(json!({
            "name": state.name,
            "tables": tjson,
            "writableTables": writable,
            "writeMode": mode,
            "features": {
                "workbench": state.features.workbench,
                "db_info": state.features.db_info,
                "theme_studio": state.features.theme_studio,
            },
        })),
    )
        .into_response()
}

/// GET /api/_dict：数据字典聚合端点——单请求返回全部表的列/主键/实时行数。
/// 数据字典页此前 = /api/_meta + N 次行数请求（Mutex 串行化，demo2 实测 18 并发请求 ≈ 700ms）；
/// 收敛为一次请求后，页面加载 = 一次锁内遍历（list_tables + row_counts，表数 × ~1ms）。
async fn dict(State(state): State<AppState>) -> Response {
    let mut store = state.lock_store();
    let tables = match store.list_tables() {
        Ok(t) => t,
        Err(e) => return error_response(e),
    };
    let names: Vec<String> = tables.iter().map(|t| t.name.clone()).collect();
    let counts = match store.row_counts(&names) {
        Ok(c) => c,
        Err(e) => return error_response(e),
    };
    let tjson: Vec<Value> = tables
        .into_iter()
        .zip(counts)
        .map(|(t, n)| {
            json!({
                "name": t.name,
                "primaryKey": t.primary_key,
                "columns": t.columns,
                "rows": n,
            })
        })
        .collect();
    (
        Json(json!({
            "name": state.name,
            "tables": tjson,
            "features": {
                "workbench": state.features.workbench,
                "db_info": state.features.db_info,
                "theme_studio": state.features.theme_studio,
            },
        })),
    )
        .into_response()
}

/// POST /api/_session/reset：清当前会话覆盖层（real 模式 no-op，恒 {ok:true}）
async fn session_reset(
    State(state): State<AppState>,
    Extension(sid): Extension<String>,
) -> Response {
    state.session.reset(&sid);
    (Json(json!({"ok": true}))).into_response()
}

/// GET /api/:table：列表 {data, total}。simulated 模式走会话覆盖视图（合并 inserts/updates/deletes）
async fn list(
    State(state): State<AppState>,
    Extension(sid): Extension<String>,
    Path(table): Path<String>,
    RawQuery(q): RawQuery,
) -> Response {
    let info = match check_table(&state, &table) {
        Ok(i) => i,
        Err(e) => return error_response(e),
    };
    let columns = info.columns.clone();
    let pairs = parse_query(q.as_deref());
    let where_pairs: Vec<String> =
        pairs.iter().filter(|(k, _)| k == "where").map(|(_, v)| v.clone()).collect();
    let filters = match filter::parse_where(&where_pairs, &columns) {
        Ok(f) => f,
        Err(e) => return error_response(e),
    };
    let sort = match get_param(&pairs, "sort") {
        Some(v) => match filter::parse_sort(&v, &columns) {
            Ok(s) => s,
            Err(e) => return error_response(e),
        },
        None => Vec::new(),
    };
    let page = filter::parse_page(
        get_param(&pairs, "page").as_deref(),
        get_param(&pairs, "pageSize").as_deref(),
    );

    let result = if state.write_mode == WriteMode::Simulated {
        let mut base = state.lock_store();
        let base: &mut dyn Store = &mut **base;
        let mut view =
            session::SessionView::read(base, &state.session, &sid, &table, &info.primary_key);
        view.list(&filters, &sort, page)
    } else {
        state.lock_store().select(&table, &filters, &sort, page)
    };
    match result {
        Ok((data, total)) => {
            let data: Vec<Value> = data.iter().map(row_to_json).collect();
            (Json(json!({ "data": data, "total": total }))).into_response()
        }
        Err(e) => error_response(e),
    }
}

/// GET /api/:table/:id：单条；无则 404 `row not found`。simulated 走会话覆盖视图
async fn get_one(
    State(state): State<AppState>,
    Extension(sid): Extension<String>,
    Path((table, id)): Path<(String, String)>,
) -> Response {
    let info = match check_table(&state, &table) {
        Ok(i) => i,
        Err(e) => return error_response(e),
    };
    let found = if state.write_mode == WriteMode::Simulated {
        let mut base = state.lock_store();
        let base: &mut dyn Store = &mut **base;
        let mut view = session::SessionView::read(&mut *base, &state.session, &sid, &table, &info.primary_key);
        view.get(&id)
    } else {
        state.lock_store().get(&table, &id)
    };
    match found {
        Ok(Some(row)) => (Json(row_to_json(&row))).into_response(),
        Ok(None) => error_response(ZpmError::NotFound("row not found".into())),
        Err(e) => error_response(e),
    }
}

/// POST /api/:table：插入；**201 回显入参**（不回读）；body 非 JSON 对象 → 400
async fn create(
    State(state): State<AppState>,
    Extension(sid): Extension<String>,
    Path(table): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    // 表存在性 404 先于写守卫 403（对齐 TS app.ts findTable 顺序）
    // 表存在性 404 先于写守卫 403（对齐 TS app.ts findTable 顺序）
    let info = match check_table(&state, &table) {
        Ok(i) => i,
        Err(e) => return error_response(e),
    };
    if let Err(e) = assert_writable(&state, &table) {
        return error_response(e);
    }
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return error_response(ZpmError::BadRequest("json object body required".into()))
        }
    };
    let Some(obj) = v.as_object() else {
        return error_response(ZpmError::BadRequest("json object body required".into()));
    };
    let row = json_obj_to_row(obj);

    let created = if state.write_mode == WriteMode::Simulated {
        let mut base = state.lock_store();
        let base: &mut dyn Store = &mut **base;
        let mut view = session::SessionView::write(&mut *base, &state.session, &sid, &table, &info.primary_key);
        view.insert(&row)
    } else {
        state.lock_store().insert(&table, &row)
    };
    match created {
        // 201 回显入参（不回读）
        Ok(created) => (StatusCode::CREATED, Json(row_to_json(&created))).into_response(),
        Err(e) => error_response(e),
    }
}

/// PATCH /api/:table/:id：部分更新；**200 回读完整行**；空 patch 返回当前行
async fn patch(
    State(state): State<AppState>,
    Extension(sid): Extension<String>,
    Path((table, id)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> Response {
    // 表存在性 404 先于写守卫 403（对齐 TS app.ts findTable 顺序）
    // 表存在性 404 先于写守卫 403（对齐 TS app.ts findTable 顺序）
    let info = match check_table(&state, &table) {
        Ok(i) => i,
        Err(e) => return error_response(e),
    };
    if let Err(e) = assert_writable(&state, &table) {
        return error_response(e);
    }
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return error_response(ZpmError::BadRequest("json object body required".into()))
        }
    };
    let Some(obj) = v.as_object() else {
        return error_response(ZpmError::BadRequest("json object body required".into()));
    };
    let patch_row = json_obj_to_row(obj);

    let updated = if state.write_mode == WriteMode::Simulated {
        let mut base = state.lock_store();
        let base: &mut dyn Store = &mut **base;
        let mut view = session::SessionView::write(&mut *base, &state.session, &sid, &table, &info.primary_key);
        view.update(&id, &patch_row)
    } else {
        state.lock_store().update(&table, &id, &patch_row)
    };
    match updated {
        // 200 回读完整行；空 patch 返回当前行
        Ok(Some(row)) => (Json(row_to_json(&row))).into_response(),
        Ok(None) => error_response(ZpmError::NotFound("row not found".into())),
        Err(e) => error_response(e),
    }
}

/// DELETE /api/:table/:id：成功 `{"ok":true}`；删不到 404
async fn remove_one(
    State(state): State<AppState>,
    Extension(sid): Extension<String>,
    Path((table, id)): Path<(String, String)>,
) -> Response {
    // 表存在性 404 先于写守卫 403（对齐 TS app.ts findTable 顺序）
    let info = match check_table(&state, &table) {
        Ok(i) => i,
        Err(e) => return error_response(e),
    };
    if let Err(e) = assert_writable(&state, &table) {
        return error_response(e);
    }
    let deleted = if state.write_mode == WriteMode::Simulated {
        let mut base = state.lock_store();
        let base: &mut dyn Store = &mut **base;
        let mut view = session::SessionView::write(&mut *base, &state.session, &sid, &table, &info.primary_key);
        view.delete(&id)
    } else {
        state.lock_store().delete(&table, &id)
    };
    match deleted {
        Ok(true) => (Json(json!({"ok": true}))).into_response(),
        Ok(false) => error_response(ZpmError::NotFound("row not found".into())),
        Err(e) => error_response(e),
    }
}
