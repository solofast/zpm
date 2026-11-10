//! Store 抽象：动态 CRUD 的双方言（sqlite / postgres）第一半。
//!
//! 设计意图：
//! - [`Store`] trait 是双方言抽象，方法全 `&mut self`（rusqlite `Connection` 非
//!   Sync，服务器层后续会包 `Mutex`）；
//! - `insert` 回显入参不回读、`update` 回读——TS 版契约，测试锚定；
//! - 所有表/列名入 SQL 前必过 [`safe_ident`]，值绑定经 [`Value`]，SQL 全参数化。

use crate::error::ZpmError;
use crate::project_config::Target;
use std::path::Path;

pub mod postgres;
pub mod sqlite;

pub use postgres::{open_postgres, PgStore};
pub use sqlite::{open_sqlite, SqliteStore};

/// 按目标方言打开 store（zpm.config.yaml 的 target.type 决定 sqlite / postgres）。
pub fn open_store(target: &Target) -> Result<Box<dyn Store>> {
    match target {
        Target::Sqlite(f) => Ok(Box::new(open_sqlite(Path::new(&f.file))?)),
        Target::Postgres(conn) => Ok(Box::new(open_postgres(conn)?)),
    }
}

/// store 层统一 Result 别名。
pub type Result<T> = std::result::Result<T, ZpmError>;

/// 动态值：与 sqlite 列存储类型一一对应。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// 建表时的逻辑列类型（映射 TEXT/INTEGER/REAL/BLOB）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalType {
    Text,
    Integer,
    Real,
    Blob,
}

/// 表元信息（`list_tables` 返回）。
#[derive(Debug, Clone, PartialEq)]
pub struct TableInfo {
    pub name: String,
    pub primary_key: String,
    pub columns: Vec<String>,
}

/// 一行数据：列名 → 值（BTreeMap 保证键序确定）。
pub type Row = std::collections::BTreeMap<String, Value>;

/// 过滤操作符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Lt,
    Like,
    In,
}

/// 单个过滤条件。
///
/// 多值形态：`eq/ne/gt/lt/like` 取 `values[0]`（长度 1 由解析层保证，Store 层
/// 不做长度校验）；`in` 生成 `field IN (?, ..., ?)`，逐个绑定 `values`，
/// 空列表为恒假谓词（不命中任何行）。
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub field: String,
    pub op: Op,
    pub values: Vec<Value>,
}

/// 排序项。
#[derive(Debug, Clone, PartialEq)]
pub struct Sort {
    pub field: String,
    pub desc: bool,
}

/// 分页（page 从 1 起；offset = (page-1) * page_size）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub page: u64,
    pub page_size: u64,
}

/// 双方言动态 CRUD 抽象。
pub trait Store: Send {
    fn list_tables(&mut self) -> Result<Vec<TableInfo>>;
    fn select(&mut self, table: &str, filters: &[Filter], sort: &[Sort], page: Page) -> Result<(Vec<Row>, i64)>;
    fn get(&mut self, table: &str, id: &str) -> Result<Option<Row>>;
    /// 回显入参（不回读）。
    fn insert(&mut self, table: &str, row: &Row) -> Result<Row>;
    /// 回读；空 patch 返回当前行。
    fn update(&mut self, table: &str, id: &str, patch: &Row) -> Result<Option<Row>>;
    fn delete(&mut self, table: &str, id: &str) -> Result<bool>;
    fn create_table(&mut self, table: &str, cols: &[(String, LogicalType)], pk: &str) -> Result<()>;
    /// 批量插入，每 500 行一个事务。
    fn insert_rows(&mut self, table: &str, rows: &[Row]) -> Result<()>;
    /// 事务内 DROP IF EXISTS final; ALTER shadow RENAME TO final。
    fn swap_table(&mut self, final_name: &str, shadow_name: &str) -> Result<()>;
    /// DROP TABLE IF EXISTS（数据同步失败时清理 shadow 表用）。
    fn drop_table(&mut self, table: &str) -> Result<()>;
    /// 单表列类型（名, 逻辑类型），按定义顺序；表不存在 → NotFound。快照导出用。
    fn describe_table(&mut self, table: &str) -> Result<Vec<(String, LogicalType)>>;
    /// 执行原始 SQL（db seed 用；多语句按方言批量执行）。
    fn execute_raw(&mut self, sql: &str) -> Result<()>;
    /// 批量行数（数据字典页一次拉全部表行数，替代 N 次分页请求；顺序与入参一致）。
    fn row_counts(&mut self, tables: &[String]) -> Result<Vec<i64>>;
    /// 事务包裹的脚本执行（db seed，规格 §14）：逐条执行 stmts，任一失败回滚整体，
    /// 错误信息带语句序号（第 N 条）。
    fn execute_script(&mut self, stmts: &[String]) -> Result<()>;
    /// 单表主键列（定向查询，1 次往返）；三级回退：约束第一列 → 第一列 → "id"。
    fn table_pk(&mut self, table: &str) -> Result<String>;
}

/// 打开目标库；sqlite 的文件路径相对项目根解析（zpm.config.yaml 的约定）。
pub fn open_store_for(
    target: &crate::project_config::Target,
    project_root: &std::path::Path,
) -> Result<Box<dyn Store>> {
    match target {
        crate::project_config::Target::Sqlite(f) => {
            Ok(Box::new(open_sqlite(&project_root.join(&f.file))?))
        }
        crate::project_config::Target::Postgres(c) => Ok(Box::new(open_postgres(c)?)),
    }
}

/// sqlite 声明类型 → 逻辑类型（亲和规则；store 内省与数据同步源共用）
pub(crate) fn sqlite_decl_to_logical(decl: &str) -> LogicalType {
    let d = decl.to_ascii_uppercase();
    if d.contains("INT") {
        LogicalType::Integer
    } else if d.contains("CHAR") || d.contains("CLOB") || d.contains("TEXT") {
        LogicalType::Text
    } else if d.contains("BLOB") {
        LogicalType::Blob
    } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
        LogicalType::Real
    } else {
        LogicalType::Text
    }
}

/// 标识符安全校验：不合 IDENT_RE → Err(UnsafeIdent)。
pub fn safe_ident(name: &str) -> Result<()> {
    if crate::project_config::is_safe_ident(name) {
        Ok(())
    } else {
        Err(ZpmError::UnsafeIdent(format!(
            "不安全的标识符：{name:?}（须匹配 {}）",
            crate::project_config::IDENT_RE
        )))
    }
}

/// 校验并**双引号包裹**标识符后返回（§12：防保留字冲突与大小写折叠；
/// IDENT_RE 已排除 `"`，包裹无需转义）。所有 SQL 标识符插值点必须使用本函数的返回值。
pub fn quote_ident(name: &str) -> Result<String> {
    safe_ident(name)?;
    Ok(format!("\"{name}\""))
}

/// SQL 字符串字面量（单引号翻倍转义）；仅用于标签/别名拼接，值一律走参数绑定。
pub fn sql_str_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// 双方言共享语义断言：sqlite / postgres 测试各调一次，保证 Store 契约完全一致。
///
/// 固定建表 `pg_sem_t` / `pg_ord_t` / `pg_like_t`；pg 侧测试运行前需先 DROP 这三张表。
/// 表名带 `pg_` 前缀：pg 测试的清理 DROP（IF EXISTS ... CASCADE）打在真实业务库上，
/// 前缀使 CASCADE 的破坏半径永远不越过 pg_ 命名空间，不会波及同名业务表。
/// 覆盖：list_tables（定义列序+主键）、insert 回显、get、update（空 patch 回当前行 /
/// 不存在→None / 非空回读）、delete bool、无 sort 插入序、total 独立分页、like 两侧通配、
/// in 多值与空值恒假、int64 大数往返、row_counts 批量行数。
#[cfg(test)]
pub(crate) fn assert_store_semantics(s: &mut dyn Store) {
    // ① create_table → list_tables：定义列序 + 主键识别（按表名升序返回；
    //    真库可能还有其他既有表，故按名查找而非全量相等）
    s.create_table(
        "pg_sem_t",
        &[("id".into(), LogicalType::Integer), ("name".into(), LogicalType::Text)],
        "id",
    )
    .unwrap();
    s.create_table(
        "pg_ord_t",
        &[("name".into(), LogicalType::Text), ("n".into(), LogicalType::Integer)],
        "name",
    )
    .unwrap();
    s.create_table("pg_like_t", &[("name".into(), LogicalType::Text)], "name").unwrap();
    let tables = s.list_tables().unwrap();
    let expected = [
        TableInfo {
            name: "pg_like_t".into(),
            primary_key: "name".into(),
            columns: vec!["name".into()],
        },
        TableInfo {
            name: "pg_ord_t".into(),
            primary_key: "name".into(),
            columns: vec!["name".into(), "n".into()],
        },
        TableInfo {
            name: "pg_sem_t".into(),
            primary_key: "id".into(),
            columns: vec!["id".into(), "name".into()],
        },
    ];
    for e in &expected {
        let t = tables
            .iter()
            .find(|t| t.name == e.name)
            .unwrap_or_else(|| panic!("list_tables 应含 {}", e.name));
        assert_eq!(t, e);
    }
    let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "list_tables 应按表名升序");

    // ② insert 回显入参原样（未提供的列不回读补键）
    let mut row = Row::new();
    row.insert("id".into(), Value::Int(1));
    let echoed = s.insert("pg_sem_t", &row).unwrap();
    assert_eq!(echoed, row);

    // ③ get：命中 / 未命中 → None
    let got = s.get("pg_sem_t", "1").unwrap().unwrap();
    assert_eq!(got.get("id"), Some(&Value::Int(1)));
    assert_eq!(s.get("pg_sem_t", "404").unwrap(), None);

    // ④ update：空 patch 返回当前行；不存在 id → None；非空 patch 回读新值
    let cur = s.update("pg_sem_t", "1", &Row::new()).unwrap().unwrap();
    assert_eq!(cur.get("id"), Some(&Value::Int(1)));
    assert_eq!(s.update("pg_sem_t", "404", &Row::new()).unwrap(), None);
    let mut patch = Row::new();
    patch.insert("name".into(), Value::Text("b".into()));
    let upd = s.update("pg_sem_t", "1", &patch).unwrap().unwrap();
    assert_eq!(upd.get("name"), Some(&Value::Text("b".into())));

    // ⑤ delete：存在 → true；不存在 → false
    assert!(s.delete("pg_sem_t", "1").unwrap());
    assert!(!s.delete("pg_sem_t", "1").unwrap());

    // ⑥ 无 sort 按插入序返回（sqlite rowid / pg ctid）；total 是 where 命中数，独立于分页
    for (name, n) in [("e", 5i64), ("a", 1), ("b", 2), ("c", 3), ("d", 4)] {
        let mut r = Row::new();
        r.insert("name".into(), Value::Text(name.into()));
        r.insert("n".into(), Value::Int(n));
        s.insert("pg_ord_t", &r).unwrap();
    }
    let (rows, total) = s.select("pg_ord_t", &[], &[], Page { page: 2, page_size: 2 }).unwrap();
    assert_eq!(total, 5);
    assert_eq!(
        rows.iter().map(|r| r["name"].clone()).collect::<Vec<_>>(),
        vec![Value::Text("b".into()), Value::Text("c".into())]
    );
    let f = Filter { field: "n".into(), op: Op::Gt, values: vec![Value::Int(3)] };
    let (rows, total) = s.select("pg_ord_t", &[f], &[], Page { page: 1, page_size: 1 }).unwrap();
    assert_eq!(total, 2);
    assert_eq!(rows.len(), 1);

    // ⑦ like：两侧通配命中（存 "hello" 查 "ell"）
    let mut r = Row::new();
    r.insert("name".into(), Value::Text("hello".into()));
    s.insert("pg_like_t", &r).unwrap();
    let f = Filter { field: "name".into(), op: Op::Like, values: vec![Value::Text("ell".into())] };
    let (rows, total) = s.select("pg_like_t", &[f], &[], Page { page: 1, page_size: 10 }).unwrap();
    assert_eq!(total, 1);
    assert_eq!(rows[0].get("name"), Some(&Value::Text("hello".into())));

    // ⑧ in：多值 IN 命中任一（按插入序）；空 values → 恒假谓词
    for name in ["a", "c"] {
        let mut r = Row::new();
        r.insert("name".into(), Value::Text(name.into()));
        s.insert("pg_like_t", &r).unwrap();
    }
    let f = Filter {
        field: "name".into(),
        op: Op::In,
        values: vec![Value::Text("a".into()), Value::Text("c".into())],
    };
    let (rows, total) = s.select("pg_like_t", &[f], &[], Page { page: 1, page_size: 10 }).unwrap();
    assert_eq!(total, 2);
    assert_eq!(
        rows.iter().map(|r| r["name"].clone()).collect::<Vec<_>>(),
        vec![Value::Text("a".into()), Value::Text("c".into())]
    );
    let f = Filter { field: "name".into(), op: Op::In, values: vec![] };
    let (rows, total) = s.select("pg_sem_t", &[f], &[], Page { page: 1, page_size: 10 }).unwrap();
    assert_eq!(total, 0);
    assert!(rows.is_empty());

    // ⑨ int64 大数往返（pg 侧锚定 int8）
    let mut big = Row::new();
    big.insert("id".into(), Value::Int(2147483648));
    s.insert("pg_sem_t", &big).unwrap();
    let got = s.get("pg_sem_t", "2147483648").unwrap().unwrap();
    assert_eq!(got.get("id"), Some(&Value::Int(2147483648)));

    // ⑩ row_counts：批量行数按入参顺序返回实时值（/api/_dict 聚合端点用）
    let counts = s
        .row_counts(&["pg_sem_t".into(), "pg_ord_t".into(), "pg_like_t".into()])
        .unwrap();
    assert_eq!(counts, vec![1, 5, 3], "row_counts 顺序与入参一致且为实时值");
    assert!(s.row_counts(&[]).unwrap().is_empty(), "空入参 → 空输出");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_config::SqliteFile;

    /// ① safe_ident：非法 → UnsafeIdent；"_a1" → Ok
    #[test]
    fn safe_ident_rejects_unsafe_and_allows_simple() {
        assert!(matches!(safe_ident("users; DROP"), Err(ZpmError::UnsafeIdent(_))));
        assert!(safe_ident("_a1").is_ok());
    }

    /// ②③④⑤⑥⑦⑨⑩ 方言无关语义契约 → assert_store_semantics（pg 侧共用同一组断言）
    #[test]
    fn sqlite_store_semantics_match_shared_contract() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        assert_store_semantics(&mut s);
    }

    /// ⑧ open_sqlite：WAL 生效 + 父目录自动创建
    #[test]
    fn open_sqlite_enables_wal_and_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("nested").join("data.db");
        let s = open_sqlite(&db).unwrap();
        assert_eq!(s.journal_mode().unwrap(), "wal");
        assert!(db.is_file());
    }

    /// open_store：Target::Sqlite 分派到 open_sqlite（父目录自动创建、CRUD 可用）
    #[test]
    fn open_store_dispatches_sqlite_target() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("nested").join("t.db");
        let mut s = open_store(&Target::Sqlite(SqliteFile { file: file.to_string_lossy().into_owned() }))
            .unwrap();
        s.create_table("t", &[("id".into(), LogicalType::Integer)], "id").unwrap();
        let mut row = Row::new();
        row.insert("id".into(), Value::Int(1));
        s.insert("t", &row).unwrap();
        let (rows, total) = s.select("t", &[], &[], Page { page: 1, page_size: 10 }).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0]["id"], Value::Int(1));
        assert!(s.delete("t", "1").unwrap());
    }
}
