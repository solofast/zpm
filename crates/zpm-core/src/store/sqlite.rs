//! SqliteStore：[`Store`] trait 的 sqlite 方言实现（rusqlite）。
//!
//! SQL 安全约束：所有表/列名拼接进 SQL 前必过 [`safe_ident`]；所有值经
//! [`Value`] 参数绑定，语句零插值（PRAGMA table_info 的表名同样先过校验）。

use super::{quote_ident, safe_ident, sql_str_literal, sqlite_decl_to_logical, Filter, LogicalType, Op, Page, Result, Row, Sort, Store, TableInfo, Value};
use crate::error::ZpmError;
use rusqlite::{params, params_from_iter, Connection, ToSql};
use std::path::Path;

use rusqlite::types::Value as RValue;

/// 打开 sqlite 文件库：建父目录、WAL、busy_timeout=5000。
pub fn open_sqlite(file: &Path) -> Result<SqliteStore> {
    if let Some(parent) = file.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let conn = Connection::open(file)?;
    init_pragmas(&conn)?;
    Ok(SqliteStore { conn })
}

/// sqlite 方言实现。`Connection` 是 Send 但非 Sync，故 trait 方法全 `&mut self`。
pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    /// 测试专用构造：内存库。
    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<SqliteStore> {
        let conn = Connection::open_in_memory()?;
        init_pragmas(&conn)?;
        Ok(SqliteStore { conn })
    }

    /// 测试支持：读 PRAGMA 字符串值（如 journal_mode）。
    #[cfg(test)]
    pub(crate) fn journal_mode(&self) -> Result<String> {
        Ok(self.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?)
    }
}

fn init_pragmas(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(())
}

impl ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::ToSqlOutput;
        Ok(match self {
            Value::Null => ToSqlOutput::Owned(RValue::Null),
            Value::Int(i) => ToSqlOutput::Owned(RValue::Integer(*i)),
            Value::Real(f) => ToSqlOutput::Owned(RValue::Real(*f)),
            Value::Text(s) => ToSqlOutput::Owned(RValue::Text(s.clone())),
            Value::Blob(b) => ToSqlOutput::Owned(RValue::Blob(b.clone())),
        })
    }
}

impl From<RValue> for Value {
    fn from(v: RValue) -> Self {
        match v {
            RValue::Null => Value::Null,
            RValue::Integer(i) => Value::Int(i),
            RValue::Real(f) => Value::Real(f),
            RValue::Text(s) => Value::Text(s),
            RValue::Blob(b) => Value::Blob(b),
        }
    }
}

/// 执行查询并把结果集读成 Row（列名 → 值）。
fn query_rows(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let rows = stmt.query_map(params, |r| {
        let mut row = Row::new();
        for (i, name) in names.iter().enumerate() {
            let v: RValue = r.get(i)?;
            row.insert(name.clone(), v.into());
        }
        Ok(row)
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// PRAGMA table_info：列名（定义序）与其 pk 序号。
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<(String, i64)>> {
    let t = quote_ident(table)?;
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({t})"))?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// 主键回退链：首个 pk 列 → 第一列 → 字面量 "id"。
fn pk_fallback(cols: &[(String, i64)]) -> String {
    cols.iter()
        .find(|(_, pk)| *pk > 0)
        .map(|(c, _)| c.clone())
        .or_else(|| cols.first().map(|(c, _)| c.clone()))
        .unwrap_or_else(|| "id".to_string())
}

fn table_pk(conn: &Connection, table: &str) -> Result<String> {
    Ok(pk_fallback(&table_columns(conn, table)?))
}

/// 单行 INSERT（insert / insert_rows 共用；Transaction 可解引用为 Connection）。
fn exec_insert(conn: &Connection, table: &str, row: &Row) -> Result<()> {
    if row.is_empty() {
        return Err(ZpmError::BadRequest("insert 至少需要一列".into()));
    }
    safe_ident(table)?;
    for col in row.keys() {
        safe_ident(col)?;
    }
    let t = quote_ident(table)?;
    let cols: Vec<String> = row
        .keys()
        .map(|c| quote_ident(c))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let placeholders = vec!["?"; cols.len()].join(", ");
    let sql = format!("INSERT INTO {t} ({}) VALUES ({placeholders})", cols.join(", "));
    conn.execute(&sql, params_from_iter(row.values()))?;
    Ok(())
}

impl Store for SqliteStore {
    fn list_tables(&mut self) -> Result<Vec<TableInfo>> {
        let names: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            safe_ident(&name)?;
            let cols = table_columns(&self.conn, &name)?;
            let columns: Vec<String> = cols.iter().map(|(c, _)| c.clone()).collect();
            let primary_key = pk_fallback(&cols);
            out.push(TableInfo { name, primary_key, columns });
        }
        Ok(out)
    }

    fn select(&mut self, table: &str, filters: &[Filter], sort: &[Sort], page: Page) -> Result<(Vec<Row>, i64)> {
        let tq = quote_ident(table)?;
        let mut where_sql = String::new();
        let mut bind: Vec<Value> = Vec::new();
        for (i, f) in filters.iter().enumerate() {
            let ff = quote_ident(&f.field)?;
            if i > 0 {
                where_sql.push_str(" AND ");
            }
            match f.op {
                // IN：占位符个数 = values.len()，逐个绑定；空列表 → 恒假 1=0（防 `IN ()` 语法错误）
                Op::In => {
                    if f.values.is_empty() {
                        where_sql.push_str("1=0");
                    } else {
                        let placeholders = vec!["?"; f.values.len()].join(", ");
                        where_sql.push_str(&format!("{ff} IN ({placeholders})", ff = ff));
                        bind.extend(f.values.iter().cloned());
                    }
                }
                _ => {
                    let op = match f.op {
                        Op::Eq => "=",
                        Op::Ne => "<>",
                        Op::Gt => ">",
                        Op::Lt => "<",
                        Op::Like => "LIKE",
                        Op::In => unreachable!(),
                    };
                    // 单值操作符取 values[0]（长度 1 由解析层保证，Store 层不校验）
                    let v = f.values.first().cloned().unwrap_or(Value::Null);
                    where_sql.push_str(&format!("{ff} {op} ?", ff = ff));
                    match f.op {
                        // LIKE：文本值两侧补通配后绑定（参数化，不拼 SQL）
                        Op::Like => match &v {
                            Value::Text(s) => bind.push(Value::Text(format!("%{s}%"))),
                            other => bind.push(other.clone()),
                        },
                        _ => bind.push(v),
                    }
                }
            }
        }
        let where_clause =
            if filters.is_empty() { String::new() } else { format!(" WHERE {where_sql}") };

        // total：同 where 的独立计数，与分页无关
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM {tq}{where_clause}"),
            params_from_iter(&bind),
            |r| r.get(0),
        )?;

        let mut sql = format!("SELECT * FROM {tq}{where_clause}");
        if sort.is_empty() {
            sql.push_str(" ORDER BY rowid");
        } else {
            let terms: Vec<String> = sort
                .iter()
                .map(|s| {
                    let f = quote_ident(&s.field)?;
                    Ok(format!("{f} {}", if s.desc { "DESC" } else { "ASC" }))
                })
                .collect::<Result<_>>()?;
            sql.push_str(&format!(" ORDER BY {}", terms.join(", ")));
        }
        let offset = page.page.saturating_sub(1).saturating_mul(page.page_size);
        sql.push_str(" LIMIT ? OFFSET ?");
        bind.push(Value::Int(i64::try_from(page.page_size).unwrap_or(i64::MAX)));
        bind.push(Value::Int(i64::try_from(offset).unwrap_or(i64::MAX)));
        let rows = query_rows(&self.conn, &sql, params_from_iter(&bind))?;
        Ok((rows, total))
    }

    fn get(&mut self, table: &str, id: &str) -> Result<Option<Row>> {
        let tq = quote_ident(table)?;
        let pkq = quote_ident(&table_pk(&self.conn, table)?)?;
        let rows =
            query_rows(&self.conn, &format!("SELECT * FROM {tq} WHERE {pkq} = ? LIMIT 1"), params![id])?;
        Ok(rows.into_iter().next())
    }

    fn insert(&mut self, table: &str, row: &Row) -> Result<Row> {
        exec_insert(&self.conn, table, row)?;
        Ok(row.clone()) // 契约：回显入参，不回读
    }

    fn update(&mut self, table: &str, id: &str, patch: &Row) -> Result<Option<Row>> {
        let tq = quote_ident(table)?;
        let Some(current) = self.get(table, id)? else {
            return Ok(None);
        };
        if patch.is_empty() {
            return Ok(Some(current)); // 空 patch：返回当前行，不发 UPDATE
        }
        let sets: Vec<String> = patch
            .keys()
            .map(|c| Ok::<_, ZpmError>(format!("{} = ?", quote_ident(c)?)))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let pkq = quote_ident(&table_pk(&self.conn, table)?)?;
        let sql = format!("UPDATE {tq} SET {} WHERE {pkq} = ?", sets.join(", "));
        let mut bind: Vec<Value> = patch.values().cloned().collect();
        bind.push(Value::Text(id.to_string()));
        self.conn.execute(&sql, params_from_iter(&bind))?;
        self.get(table, id) // 契约：回读
    }

    fn delete(&mut self, table: &str, id: &str) -> Result<bool> {
        let tq = quote_ident(table)?;
        let pkq = quote_ident(&table_pk(&self.conn, table)?)?;
        let n = self.conn.execute(&format!("DELETE FROM {tq} WHERE {pkq} = ?"), params![id])?;
        Ok(n > 0)
    }

    fn create_table(&mut self, table: &str, cols: &[(String, LogicalType)], pk: &str) -> Result<()> {
        let table = quote_ident(table)?;
        safe_ident(pk)?;
        for (name, _) in cols {
            safe_ident(name)?;
        }
        if !cols.iter().any(|(name, _)| name == pk) {
            return Err(ZpmError::BadRequest(format!("create_table：主键 {pk:?} 不在列定义中")));
        }
        let pkq = quote_ident(pk)?;
        let defs: Vec<String> = cols
            .iter()
            .map(|(name, ty)| -> Result<String> {
                let n = quote_ident(name)?;
                let sql_ty = match ty {
                    LogicalType::Text => "TEXT",
                    LogicalType::Integer => "INTEGER",
                    LogicalType::Real => "REAL",
                    LogicalType::Blob => "BLOB",
                };
                if name == pk {
                    Ok(format!("{pkq} {sql_ty} PRIMARY KEY"))
                } else {
                    Ok(format!("{n} {sql_ty}"))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        self.conn.execute(&format!("CREATE TABLE {table} ({})", defs.join(", ")), [])?;
        Ok(())
    }

    fn insert_rows(&mut self, table: &str, rows: &[Row]) -> Result<()> {
        for chunk in rows.chunks(500) {
            let tx = self.conn.transaction()?;
            for row in chunk {
                exec_insert(&tx, table, row)?;
            }
            tx.commit()?;
        }
        Ok(())
    }

    fn swap_table(&mut self, final_name: &str, shadow_name: &str) -> Result<()> {
        let final_name = quote_ident(final_name)?;
        let shadow_name = quote_ident(shadow_name)?;
        let tx = self.conn.transaction()?;
        tx.execute(&format!("DROP TABLE IF EXISTS {final_name}"), [])?;
        tx.execute(&format!("ALTER TABLE {shadow_name} RENAME TO {final_name}"), [])?;
        tx.commit()?;
        Ok(())
    }

    fn drop_table(&mut self, table: &str) -> Result<()> {
        let table = quote_ident(table)?;
        self.conn.execute(&format!("DROP TABLE IF EXISTS {table}"), [])?;
        Ok(())
    }

    fn describe_table(&mut self, table: &str) -> Result<Vec<(String, LogicalType)>> {
        let raw = table.to_string();
        let table = quote_ident(table)?;
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let cols: Vec<(String, LogicalType)> = rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(name, decl)| (name, sqlite_decl_to_logical(&decl)))
            .collect();
        if cols.is_empty() {
            return Err(ZpmError::NotFound(format!("table not found: {raw}")));
        }
        Ok(cols)
    }

    fn execute_raw(&mut self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    fn row_counts(&mut self, tables: &[String]) -> Result<Vec<i64>> {
        // 单条 UNION ALL 一次往返拿全部行数（每 100 表一段防超长语句）；按名称回映射保证输出顺序
        let mut out = Vec::with_capacity(tables.len());
        for chunk in tables.chunks(100) {
            let sql = chunk
                .iter()
                .map(|t| {
                    let q = quote_ident(t)?;
                    Ok(format!(
                        "SELECT {} AS tbl, COUNT(*) AS n FROM {q}",
                        sql_str_literal(t)
                    ))
                })
                .collect::<std::result::Result<Vec<String>, ZpmError>>()?
                .join(" UNION ALL ");
            let mut stmt = self.conn.prepare(&sql)?;
            let pairs: Vec<(String, i64)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let by_name: std::collections::HashMap<&str, i64> =
                pairs.iter().map(|(n, c)| (n.as_str(), *c)).collect();
            for t in chunk {
                out.push(by_name.get(t.as_str()).copied().unwrap_or(0));
            }
        }
        Ok(out)
    }

    /// 事务包裹的脚本执行（db seed）：任一语句失败回滚整体，错误带语句序号。
    fn execute_script(&mut self, stmts: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (i, stmt) in stmts.iter().enumerate() {
            tx.execute_batch(stmt)
                .map_err(|e| ZpmError::Format(format!("seed 第 {} 条语句失败：{e}", i + 1)))?;
        }
        tx.commit()?;
        Ok(())
    }

    fn table_pk(&mut self, table: &str) -> Result<String> {
        table_pk(&self.conn, table)
    }
}

#[cfg(test)]
mod tests {
    use super::{open_sqlite, LogicalType, Row, Store, Value};
    use crate::store::Page;

    /// §12 保留字列：双引号包裹后保留字列名可正常 CRUD（quote_ident 回归）
    #[test]
    fn reserved_word_columns_work() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open_sqlite(&tmp.path().join("t.db")).unwrap();
        s.create_table(
            "t",
            &[
                ("id".into(), LogicalType::Integer),
                ("order".into(), LogicalType::Text),
                ("desc".into(), LogicalType::Text),
            ],
            "id",
        )
        .unwrap();
        let mut r = Row::new();
        r.insert("id".into(), Value::Int(1));
        r.insert("order".into(), Value::Text("o".into()));
        r.insert("desc".into(), Value::Text("d".into()));
        s.insert("t", &r).unwrap();
        let got = s.get("t", "1").unwrap().unwrap();
        assert_eq!(got["order"], Value::Text("o".into()));
        let (rows, total) =
            s.select("t", &[], &[], Page { page: 1, page_size: 10 }).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0]["order"], Value::Text("o".into()));
    }

    /// §14 seed 语义（execute_script）：第 3 条失败 → 整体回滚（st 表不得存在）
    #[test]
    fn execute_script_rolls_back_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = open_sqlite(&tmp.path().join("t.db")).unwrap();
        let stmts = vec![
            "CREATE TABLE st (id INTEGER PRIMARY KEY)".to_string(),
            "INSERT INTO st VALUES (1)".to_string(),
            "INSERT INTO missing_table VALUES (1)".to_string(),
        ];
        let err = s.execute_script(&stmts).unwrap_err();
        assert!(err.to_string().contains("第 3 条语句失败"), "实际：{err}");
        let names: Vec<String> =
            s.list_tables().unwrap().into_iter().map(|t| t.name).collect();
        assert!(!names.iter().any(|n| n == "st"), "事务回滚后 st 表不得存在：{names:?}");
    }
}
