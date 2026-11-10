//! 数据同步引擎：源库 → 目标库，shadow-rename 全量替换（对齐 TS 版 sync/engine.ts）。
//!
//! 算法（每表独立，逐表执行，前表成功不回滚）：
//! 1. 读源 schema（pg: information_schema；sqlite: PRAGMA table_info）
//! 2. `columns` 白名单过滤后为空 → `no matching columns`
//! 3. 校验 primary_key 在列集中，否则 `primary key "..." not among columns of <t>`
//! 4. 全量读取（无 WHERE/ORDER）
//! 5. 建 `__shadow_<表>` → 批量插入（值规整见 `coerce_logical`）
//! 6. 事务内 `DROP IF EXISTS <final>; ALTER TABLE shadow RENAME TO final`
//! 7. 任何一步失败 → 尽力 DROP shadow（防污染 `/api` 表清单）→ `sync <t> failed: <原因>`
//!
//! 外键/索引/约束不处理；目标表只在同步时由本引擎创建，server 运行时只反射。

use crate::error::ZpmError;
use crate::project_config::{Datasource, PgConn, TableSpec};
use crate::store::{quote_ident, LogicalType, Row, Store, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// 按数据源配置打开 Source；sqlite 的文件路径相对项目根解析。
pub fn open_source(ds: &Datasource, project_root: &Path) -> Result<Box<dyn Source>, ZpmError> {
    match ds {
        Datasource::Postgres(c) => Ok(Box::new(PgSource::connect(c)?)),
        Datasource::Sqlite(f) => Ok(Box::new(SqliteSource::open(&project_root.join(&f.file))?)),
    }
}

/// 单表同步结果
#[derive(Debug, Clone)]
pub struct SyncReport {
    pub table: String,
    pub rows: usize,
}

/// 数据源抽象：pg（真实业务库）或 sqlite（sqlite 源，本设计的方言扩展）。
pub trait Source {
    /// 列清单（名, 逻辑类型），按定义顺序；表不存在 → `source table not found: <t>`
    fn columns(&mut self, table: &str) -> Result<Vec<(String, LogicalType)>, ZpmError>;
    /// 全量读取选中列的行（无 WHERE、无 ORDER，一次性载入）
    fn read_all(&mut self, table: &str, cols: &[String]) -> Result<Vec<Row>, ZpmError>;
}

// ---------------------------------------------------------------------------
// sqlite 源
// ---------------------------------------------------------------------------

/// sqlite 数据源（独立只读连接，与目标 Store 互不共享）。
pub struct SqliteSource {
    conn: rusqlite::Connection,
}

impl SqliteSource {
    pub fn open(file: &Path) -> Result<Self, ZpmError> {
        let conn = rusqlite::Connection::open(file)?;
        Ok(Self { conn })
    }
}

/// sqlite 声明类型 → 逻辑类型（复用 store 模块的亲和规则）
fn sqlite_decl_to_logical(decl: &str) -> LogicalType {
    crate::store::sqlite_decl_to_logical(decl)
}

impl Source for SqliteSource {
    fn columns(&mut self, table: &str) -> Result<Vec<(String, LogicalType)>, ZpmError> {
        let table = quote_ident(table)?;
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let name: String = r.get(1)?;
            let decl: String = r.get(2)?;
            out.push((name, sqlite_decl_to_logical(&decl)));
        }
        if out.is_empty() {
            return Err(ZpmError::NotFound(format!("source table not found: {table}")));
        }
        Ok(out)
    }

    fn read_all(&mut self, table: &str, cols: &[String]) -> Result<Vec<Row>, ZpmError> {
        let table = quote_ident(table)?;
        let quoted: Vec<String> = cols
            .iter()
            .map(|c| quote_ident(c))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let sql = format!("SELECT {} FROM {table}", quoted.join(", "));
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let mut row = Row::new();
            for (i, name) in cols.iter().enumerate() {
                let v = match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(i) => Value::Int(i),
                    rusqlite::types::ValueRef::Real(f) => Value::Real(f),
                    rusqlite::types::ValueRef::Text(t) => {
                        Value::Text(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => Value::Blob(b.to_vec()),
                };
                row.insert(name.clone(), v);
            }
            out.push(row);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// pg 源
// ---------------------------------------------------------------------------

/// 列读取种类：决定 SELECT 表达式与行值解析方式（store/pg select 共用口径）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ColKind {
    Bool,
    I32,
    I64,
    F64,
    /// numeric/decimal：驱动无法原生解析，取 ::text，由 coerce 转数值
    NumericStr,
    Text,
    Bytea,
    /// json/jsonb/数组/时间族：`to_json(col)#>>'{}'` 产出 JSON/ISO 文本形态
    /// （`#>>'{}'` 把 JSON 标量解包为裸文本，时间戳得到无引号 ISO；对象/数组保持 JSON 文本）
    JsonStr,
}

/// pg data_type → ColKind（含读取 cast 策略）

/// data_type + udt_name → 读取种类
pub(crate) fn pg_col_kind(data_type: &str, udt_name: &str) -> ColKind {
    match data_type {
        "boolean" => ColKind::Bool,
        "smallint" | "integer" | "serial" | "smallserial" | "int2" | "int4" => ColKind::I32,
        "bigint" | "bigserial" | "int8" => ColKind::I64,
        "real" | "double precision" | "float4" | "float8" => ColKind::F64,
        "numeric" | "decimal" => ColKind::NumericStr,
        "bytea" => ColKind::Bytea,
        "json" | "jsonb" | "ARRAY" => ColKind::JsonStr,
        t if t.starts_with("timestamp") || t.starts_with("date") || t.starts_with("time") => {
            ColKind::JsonStr
        }
        _ if udt_name.starts_with('_') => ColKind::JsonStr, // 数组类型
        _ => ColKind::Text,
    }
}

fn pg_col_kind_to_logical(kind: ColKind) -> LogicalType {
    match kind {
        ColKind::Bool | ColKind::I32 | ColKind::I64 => LogicalType::Integer,
        ColKind::F64 | ColKind::NumericStr => LogicalType::Real,
        ColKind::Bytea => LogicalType::Blob,
        ColKind::Text | ColKind::JsonStr => LogicalType::Text,
    }
}

/// 列读取表达式（store/pg select 与 PgSource 共用口径；col_quoted 须已过 quote_ident）：
/// bool→::int、numeric→::text、JSON/时间族→to_json 解包、Text 兜底→::text
pub(crate) fn pg_read_expr(col_quoted: &str, kind: ColKind) -> String {
    match kind {
        ColKind::Bool => format!("{col_quoted}::int"),
        ColKind::I32 | ColKind::I64 | ColKind::F64 | ColKind::Bytea => col_quoted.to_string(),
        ColKind::NumericStr => format!("{col_quoted}::text"),
        ColKind::JsonStr => format!("to_json({col_quoted})#>>'{{}}'"),
        ColKind::Text => format!("{col_quoted}::text"),
    }
}

/// pg 数据源。`kinds` 缓存最近一次 `columns()` 的列种类（read_all 决定 cast 用）。
pub struct PgSource {
    client: postgres::Client,
    kinds: BTreeMap<String, Vec<(String, ColKind)>>,
}

impl PgSource {
    pub fn connect(conn: &PgConn) -> Result<Self, ZpmError> {
        let client = postgres::Client::connect(&conn.conn_str(), postgres::NoTls)?;
        Ok(Self { client, kinds: BTreeMap::new() })
    }

    fn load_columns(&mut self, table: &str) -> Result<(), ZpmError> {
        let rows = self
            .client
            .query(
                "SELECT column_name, data_type, udt_name FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position",
                &[&table],
            )
            .map_err(|_| ZpmError::NotFound(format!("source table not found: {table}")))?;
        if rows.is_empty() {
            return Err(ZpmError::NotFound(format!("source table not found: {table}")));
        }
        let mut cols = Vec::new();
        for r in &rows {
            let name: String = r.get(0);
            let data_type: String = r.get(1);
            let udt_name: String = r.get(2);
            cols.push((name, pg_col_kind(&data_type, &udt_name)));
        }
        self.kinds.insert(table.to_string(), cols);
        Ok(())
    }
}

impl Source for PgSource {
    fn columns(&mut self, table: &str) -> Result<Vec<(String, LogicalType)>, ZpmError> {
        // 元数据查询按裸名绑定（information_schema 存裸名；引号形态仅供 FROM 拼接）
        self.load_columns(table)?;
        let kinds = &self.kinds[table];
        Ok(kinds.iter().map(|(n, k)| (n.clone(), pg_col_kind_to_logical(*k))).collect())
    }

    fn read_all(&mut self, table: &str, cols: &[String]) -> Result<Vec<Row>, ZpmError> {
        self.load_columns(table)?;
        let tq = quote_ident(table)?;
        let kinds: BTreeMap<String, ColKind> = self.kinds[table]
            .iter()
            .map(|(n, k)| (n.clone(), *k))
            .collect();

        // 逐列构建 SELECT 表达式（与 store/pg select 共用 pg_read_expr 口径）：
        // bool→::int、numeric→::text、JSON/时间族→to_json 解包、Text 兜底→::text
        let mut exprs = Vec::new();
        let mut kinds_in_order = Vec::new();
        for c in cols {
            let q = quote_ident(c)?;
            let k = kinds.get(c).copied().unwrap_or(ColKind::Text);
            exprs.push(pg_read_expr(&q, k));
            kinds_in_order.push(k);
        }
        let sql = format!("SELECT {} FROM {tq}", exprs.join(", "));
        let pg_rows = self.client.query(sql.as_str(), &[])?;

        let mut out = Vec::with_capacity(pg_rows.len());
        for r in pg_rows {
            let mut row = Row::new();
            for (i, name) in cols.iter().enumerate() {
                let v = match kinds_in_order[i] {
                    ColKind::Bool => match r.try_get::<_, Option<i32>>(i)? {
                        Some(b) => Value::Int(i64::from(b)),
                        None => Value::Null,
                    },
                    ColKind::I32 => match r.try_get::<_, Option<i32>>(i)? {
                        Some(v) => Value::Int(i64::from(v)),
                        None => Value::Null,
                    },
                    ColKind::I64 => match r.try_get::<_, Option<i64>>(i)? {
                        Some(v) => Value::Int(v),
                        None => Value::Null,
                    },
                    ColKind::F64 => match r.try_get::<_, Option<f64>>(i)? {
                        Some(v) => Value::Real(v),
                        None => Value::Null,
                    },
                    ColKind::NumericStr | ColKind::Text | ColKind::JsonStr => {
                        match r.try_get::<_, Option<String>>(i)? {
                            Some(s) => Value::Text(s),
                            None => Value::Null,
                        }
                    }
                    ColKind::Bytea => match r.try_get::<_, Option<Vec<u8>>>(i)? {
                        Some(b) => Value::Blob(b),
                        None => Value::Null,
                    },
                };
                row.insert(name.clone(), v);
            }
            out.push(row);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// 同步引擎
// ---------------------------------------------------------------------------

/// 插入前值规整（对齐 TS coerceLogical）：数值列的数字字符串 → 数值（空串/NaN 保留原样）；
/// null → null；其余原样。bool 在 pg 源读取时已转 1/0。
fn coerce_logical(v: Value, ty: LogicalType) -> Value {
    match (v, ty) {
        (Value::Null, _) => Value::Null,
        (Value::Text(s), LogicalType::Integer) => match s.trim().parse::<i64>() {
            Ok(i) => Value::Int(i),
            Err(_) => Value::Text(s),
        },
        (Value::Text(s), LogicalType::Real) => match s.trim().parse::<f64>() {
            Ok(f) if f.is_finite() => Value::Real(f),
            _ => Value::Text(s),
        },
        (v, _) => v,
    }
}

fn coerce_row(mut row: Row, cols: &[(String, LogicalType)]) -> Row {
    let mut out = Row::new();
    for (name, ty) in cols {
        let v = row.remove(name).unwrap_or(Value::Null);
        out.insert(name.clone(), coerce_logical(v, *ty));
    }
    out
}

/// 共享装载通道：建 shadow → 批量插入 → 事务内 DROP+RENAME。
/// 任何失败尽力 DROP shadow 后报 `sync <t> failed`。
pub(crate) fn load_into_target(
    target: &mut dyn Store,
    table: &str,
    cols: &[(String, LogicalType)],
    pk: &str,
    rows: Vec<Row>,
) -> Result<usize, ZpmError> {
    let shadow = format!("__shadow_{table}");
    // 建表前尽力清掉上次崩溃残留的 shadow（对齐 TS engine.ts），否则残留会让后续同步永久卡死
    let _ = target.drop_table(&shadow);
    if let Err(e) = target.create_table(&shadow, cols, pk) {
        let _ = target.drop_table(&shadow);
        return Err(ZpmError::Format(format!("sync {table} failed: {e}")));
    }
    let coerced: Vec<Row> = rows.into_iter().map(|r| coerce_row(r, cols)).collect();
    let n = coerced.len();
    if let Err(e) = target.insert_rows(&shadow, &coerced) {
        let _ = target.drop_table(&shadow);
        return Err(ZpmError::Format(format!("sync {table} failed: {e}")));
    }
    if let Err(e) = target.swap_table(table, &shadow) {
        let _ = target.drop_table(&shadow);
        return Err(ZpmError::Format(format!("sync {table} failed: {e}")));
    }
    Ok(n)
}

/// 同步单表
pub fn sync_table(
    source: &mut dyn Source,
    target: &mut dyn Store,
    spec: &TableSpec,
) -> Result<SyncReport, ZpmError> {
    let all = source.columns(&spec.name)?;
    let cols = match &spec.columns {
        None => all,
        Some(w) => {
            let filtered: Vec<_> = all.into_iter().filter(|(c, _)| w.contains(c)).collect();
            if filtered.is_empty() {
                return Err(ZpmError::Format(format!(
                    "no matching columns in {}（白名单 {:?}）",
                    spec.name, w
                )));
            }
            filtered
        }
    };
    if !cols.iter().any(|(c, _)| *c == spec.primary_key) {
        return Err(ZpmError::Format(format!(
            "primary key {:?} not among columns of {}",
            spec.primary_key, spec.name
        )));
    }
    let names: Vec<String> = cols.iter().map(|(c, _)| c.clone()).collect();
    let rows = source.read_all(&spec.name, &names)?;
    let n = load_into_target(target, &spec.name, &cols, &spec.primary_key, rows)?;
    Ok(SyncReport { table: spec.name.clone(), rows: n })
}

/// 同步多表：逐表独立，前表成功不回滚；任一表失败即停止并报错。
pub fn sync_tables(
    source: &mut dyn Source,
    target: &mut dyn Store,
    specs: &[TableSpec],
) -> Result<Vec<SyncReport>, ZpmError> {
    let mut reports = Vec::new();
    for spec in specs {
        reports.push(sync_table(source, target, spec)?);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{open_sqlite, Page, SqliteStore};
    use rusqlite::Connection;

    fn source_with_users(file: &Path) -> SqliteSource {
        let conn = Connection::open(file).unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, score REAL);
             INSERT INTO users VALUES (1, 'a', 1.5), (2, 'b', 2.5);",
        )
        .unwrap();
        SqliteSource { conn }
    }

    /// ① sqlite→sqlite 全量替换：表存在、行数一致、pk 约束、幂等
    #[test]
    fn sync_sqlite_to_sqlite_full_replace() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = source_with_users(&tmp.path().join("src.db"));
        let mut target = open_sqlite(&tmp.path().join("dst.db")).unwrap();
        let spec = TableSpec {
            name: "users".into(),
            primary_key: "id".into(),
            columns: None,
        };
        let r = sync_table(&mut source, &mut target, &spec).unwrap();
        assert_eq!(r.rows, 2);

        let tables = target.list_tables().unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "users");
        assert_eq!(tables[0].primary_key, "id");
        assert_eq!(tables[0].columns, vec!["id", "name", "score"]);

        let (rows, total) =
            target.select("users", &[], &[], Page { page: 1, page_size: 100 }).unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows[0]["score"], Value::Real(1.5));

        // 幂等：重跑行数不变
        let r2 = sync_table(&mut source, &mut target, &spec).unwrap();
        assert_eq!(r2.rows, 2);
        assert_eq!(target.select("users", &[], &[], Page { page: 1, page_size: 100 }).unwrap().1, 2);
    }

    /// coerceLogical 纯函数：数值列数字字符串→数值；空串/非数保留；null 保留
    #[test]
    fn coerce_logical_converts_numeric_strings() {
        assert_eq!(coerce_logical(Value::Text("42".into()), LogicalType::Integer), Value::Int(42));
        assert_eq!(coerce_logical(Value::Text(" 3.5 ".into()), LogicalType::Real), Value::Real(3.5));
        assert_eq!(
            coerce_logical(Value::Text("abc".into()), LogicalType::Real),
            Value::Text("abc".into())
        );
        assert_eq!(
            coerce_logical(Value::Text("".into()), LogicalType::Integer),
            Value::Text("".into())
        );
        assert_eq!(coerce_logical(Value::Null, LogicalType::Integer), Value::Null);
        assert_eq!(coerce_logical(Value::Int(7), LogicalType::Text), Value::Int(7));
    }

    /// 错误路径：源表不存在 / pk 不在列集 / 白名单过滤后为空
    #[test]
    fn sync_error_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = SqliteSource::open(&tmp.path().join("empty.db")).unwrap();
        let mut target = open_sqlite(&tmp.path().join("dst.db")).unwrap();

        let missing = TableSpec { name: "nope".into(), primary_key: "id".into(), columns: None };
        let err = sync_table(&mut source, &mut target, &missing).unwrap_err();
        assert!(err.to_string().contains("source table not found"), "实际错误：{err}");

        let mut src2 = source_with_users(&tmp.path().join("src.db"));
        let bad_pk = TableSpec { name: "users".into(), primary_key: "zzz".into(), columns: None };
        let err = sync_table(&mut src2, &mut target, &bad_pk).unwrap_err();
        assert!(err.to_string().contains("not among columns"), "实际错误：{err}");

        let bad_cols = TableSpec {
            name: "users".into(),
            primary_key: "id".into(),
            columns: Some(vec!["nope".into()]),
        };
        let err = sync_table(&mut src2, &mut target, &bad_cols).unwrap_err();
        assert!(err.to_string().contains("no matching columns"), "实际错误：{err}");
    }

    /// 包装 Store：按模式在 create_table / swap_table 注入失败（其余全部委托真实实现）
    #[derive(Clone, Copy, PartialEq)]
    enum FailMode { Create, Swap }
    struct FailingStore { inner: SqliteStore, fail: FailMode }
    impl Store for FailingStore {
        fn list_tables(&mut self) -> crate::store::Result<Vec<crate::store::TableInfo>> {
            self.inner.list_tables()
        }
        fn select(&mut self, t: &str, f: &[crate::store::Filter], s: &[crate::store::Sort], p: Page) -> crate::store::Result<(Vec<Row>, i64)> {
            self.inner.select(t, f, s, p)
        }
        fn get(&mut self, t: &str, id: &str) -> crate::store::Result<Option<Row>> {
            self.inner.get(t, id)
        }
        fn insert(&mut self, t: &str, r: &Row) -> crate::store::Result<Row> {
            self.inner.insert(t, r)
        }
        fn update(&mut self, t: &str, id: &str, patch: &Row) -> crate::store::Result<Option<Row>> {
            self.inner.update(t, id, patch)
        }
        fn delete(&mut self, t: &str, id: &str) -> crate::store::Result<bool> {
            self.inner.delete(t, id)
        }
        fn create_table(&mut self, t: &str, c: &[(String, LogicalType)], pk: &str) -> crate::store::Result<()> {
            if self.fail == FailMode::Create {
                return Err(ZpmError::Other("boom".into()));
            }
            self.inner.create_table(t, c, pk)
        }
        fn insert_rows(&mut self, t: &str, r: &[Row]) -> crate::store::Result<()> {
            self.inner.insert_rows(t, r)
        }
        fn swap_table(&mut self, _f: &str, _s: &str) -> crate::store::Result<()> {
            if self.fail == FailMode::Swap {
                return Err(ZpmError::Other("boom".into()));
            }
            self.inner.swap_table(_f, _s)
        }
        fn drop_table(&mut self, t: &str) -> crate::store::Result<()> {
            self.inner.drop_table(t)
        }
        fn execute_raw(&mut self, sql: &str) -> crate::store::Result<()> {
            self.inner.execute_raw(sql)
        }
        fn execute_script(&mut self, stmts: &[String]) -> crate::store::Result<()> {
            self.inner.execute_script(stmts)
        }
        fn table_pk(&mut self, t: &str) -> crate::store::Result<String> {
            self.inner.table_pk(t)
        }
        fn describe_table(&mut self, t: &str) -> crate::store::Result<Vec<(String, LogicalType)>> {
            self.inner.describe_table(t)
        }
        fn row_counts(&mut self, t: &[String]) -> crate::store::Result<Vec<i64>> {
            self.inner.row_counts(t)
        }
    }

    /// 失败注入通用断言：报 `sync users failed` 且无 `__shadow_` 残留
    fn assert_failure_cleans_shadow(
        source: &mut dyn Source,
        target: &mut FailingStore,
        spec: &TableSpec,
    ) {
        let err = sync_table(source, target, spec).unwrap_err();
        assert!(err.to_string().contains("sync users failed"), "实际错误：{err}");
        let names: Vec<String> = target.list_tables().unwrap().into_iter().map(|t| t.name).collect();
        assert!(
            !names.iter().any(|n| n.starts_with("__shadow_")),
            "shadow 表残留：{names:?}"
        );
    }

    /// 审查重点 4：swap 失败时 shadow 表必须被清理，不得污染 list_tables
    #[test]
    fn failed_swap_cleans_shadow() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = source_with_users(&tmp.path().join("src.db"));
        let target_store = open_sqlite(&tmp.path().join("dst.db")).unwrap();
        let mut target = FailingStore { inner: target_store, fail: FailMode::Swap };
        let spec = TableSpec { name: "users".into(), primary_key: "id".into(), columns: None };
        assert_failure_cleans_shadow(&mut source, &mut target, &spec);
    }

    /// create 失败同样清场并报契约错误（且建表前的预清理让崩溃残留可自愈）
    #[test]
    fn failed_create_cleans_shadow_and_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let mut source = source_with_users(&tmp.path().join("src.db"));
        let target_store = open_sqlite(&tmp.path().join("dst.db")).unwrap();
        let mut target = FailingStore { inner: target_store, fail: FailMode::Create };
        let spec = TableSpec { name: "users".into(), primary_key: "id".into(), columns: None };
        assert_failure_cleans_shadow(&mut source, &mut target, &spec);
    }

    /// ② pg→sqlite（ZPM_TEST_PG 门控）：boolean→1/0、jsonb→JSON 文本、timestamp→ISO 文本
    #[test]
    fn pg_source_to_sqlite_target() {
        let Some(url) = std::env::var("ZPM_TEST_PG").ok().filter(|s| !s.is_empty()) else {
            eprintln!("skip pg test: ZPM_TEST_PG not set");
            return;
        };
        // 卫生 guard：测试产物不残留真库（正常返回或 panic 展开都清理）
        struct Guard { url: String }
        impl Drop for Guard {
            fn drop(&mut self) {
                if let Ok(mut c) = postgres::Client::connect(self.url.as_str(), postgres::NoTls) {
                    let _ = c.execute("DROP TABLE IF EXISTS pg_src_dbsync CASCADE", &[]);
                }
            }
        }
        let conn = PgConn {
            host: "localhost".into(),
            port: 6543,
            database: "rain-source".into(),
            user: "postgres".into(),
            password: Some("abc123".into()),
        };
        let _ = &url;
        let _guard = Guard { url: conn.conn_str() };

        {
            let mut c = postgres::Client::connect(&conn.conn_str(), postgres::NoTls).unwrap();
            c.execute("DROP TABLE IF EXISTS pg_src_dbsync CASCADE", &[]).unwrap();
            c.execute(
                "CREATE TABLE pg_src_dbsync (id int4 PRIMARY KEY, b boolean, j jsonb, ts timestamptz)",
                &[],
            )
            .unwrap();
            c.execute(
                "INSERT INTO pg_src_dbsync VALUES (1, true, '{\"k\":1}', '2026-01-02T03:04:05Z'::timestamptz)",
                &[],
            )
            .unwrap();
        }

        let mut source = PgSource::connect(&conn).unwrap();
        let dst = tempfile::tempdir().unwrap();
        let mut target = open_sqlite(&dst.path().join("dst.db")).unwrap();
        let spec = TableSpec {
            name: "pg_src_dbsync".into(),
            primary_key: "id".into(),
            columns: None,
        };
        let r = sync_table(&mut source, &mut target, &spec).unwrap();
        assert_eq!(r.rows, 1);

        let (rows, total) =
            target.select("pg_src_dbsync", &[], &[], Page { page: 1, page_size: 10 }).unwrap();
        assert_eq!(total, 1);
        let row = &rows[0];
        assert_eq!(row["b"], Value::Int(1), "boolean→1/0");
        assert!(matches!(&row["j"], Value::Text(s) if s.contains("\"k\"")), "jsonb→JSON 文本：{:?}", row["j"]);
        assert!(matches!(&row["ts"], Value::Text(t) if t.starts_with("20")), "timestamp→ISO 文本：{:?}", row["ts"]);
    }
}
