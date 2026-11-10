//! PgStore：[`Store`] trait 的 postgres 方言实现（postgres crate，纯 Rust 无需 libpq）。
//!
//! SQL 安全约束与 [`super::sqlite::SqliteStore`] 对位：所有用户输入的表/列名拼接进
//! SQL 前必过 [`safe_ident`]；所有值经 [`Value`] 参数绑定，语句零插值。
//!
//! 语义对齐要点：
//! - 占位符 `$n`（编号与绑定顺序同步推进，多 filter 组合不错位）；
//! - like 借 sqlite 语义先文本化：`{field}::text ILIKE $n`，值两侧补通配后绑定；
//! - get/update/delete 主键查找用 `{pk}::text = $n`：bigint/text 主键统一按字符串
//!   命中（对位 sqlite 的亲和性转换）；update 走 `RETURNING *` 一次往返回读；
//! - 无 sort 默认 `ORDER BY ctid`（插入序，对位 sqlite 的 rowid）；
//! - `total` 是同 where 的独立 `SELECT COUNT(*)`，与分页无关；
//! - `list_tables` / pk / 列序全部走 information_schema（public + BASE TABLE），
//!   主键三级回退：约束第一列 → 第一列 → 字面量 "id"。

use super::{quote_ident, safe_ident, sql_str_literal, Filter, LogicalType, Op, Page, Result, Row, Sort, Store, TableInfo, Value};
use crate::dbsync;
use crate::error::ZpmError;
use crate::project_config::PgConn;
use postgres::types::{private::BytesMut, IsNull, ToSql, Type};
use postgres::{Client, GenericClient, NoTls};
use std::error::Error as StdError;
use std::sync::Mutex;

/// postgres 方言实现。`Client` 非 Sync，包 `Mutex`（方法全 `&mut self`，无争用）。
pub struct PgStore {
    client: Mutex<Client>,
}

impl PgStore {
    /// 按 PgConn 打开连接（连接串由 [`PgConn::conn_str`] 拼）。
    pub fn connect(conn: &PgConn) -> Result<PgStore> {
        let client = Client::connect(&conn.conn_str(), NoTls)?;
        Ok(PgStore { client: Mutex::new(client) })
    }

    /// 取连接（锁中毒时恢复内部 client，继续工作而非连锁 panic）。
    fn client(&self) -> std::sync::MutexGuard<'_, Client> {
        self.client.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 打开 postgres store（open_store 的 pg 分支用）。
pub fn open_postgres(conn: &PgConn) -> Result<PgStore> {
    PgStore::connect(conn)
}

/// [`Value`] 的 postgres 参数包装。
///
/// `ToSql::accepts` 是静态方法，无法按 Value 变体分派目标列类型，故包一层：
/// `accepts` 恒真，实际兼容性在 [`ToSql::to_sql`] 内按变体校验，不匹配时返回
/// 带类型名的错误（而非静默错编）。create_table 产出的四类列（bigint / text /
/// double precision / bytea）之外的类型会得到明确报错。
#[derive(Debug)]
struct PgParam(Value);

impl ToSql for PgParam {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn StdError + Sync + Send>> {
        match &self.0 {
            Value::Null => Ok(IsNull::Yes),
            Value::Int(v) if *ty == Type::INT8 => v.to_sql(ty, out),
            Value::Real(v) if *ty == Type::FLOAT8 => v.to_sql(ty, out),
            Value::Text(v)
                if *ty == Type::TEXT || *ty == Type::VARCHAR || *ty == Type::BPCHAR || *ty == Type::NAME =>
            {
                v.to_sql(ty, out)
            }
            Value::Blob(v) if *ty == Type::BYTEA => v.to_sql(ty, out),
            other => Err(format!("无法把 {other:?} 编码为 postgres 类型 {ty}").into()),
        }
    }

    fn accepts(_ty: &Type) -> bool {
        true
    }

    postgres::types::to_sql_checked!();
}

/// Value 绑定数组 → PgParam 数组（获得所有权，供 param_refs 取引用）。
fn owned_params(bind: &[Value]) -> Vec<PgParam> {
    bind.iter().map(|v| PgParam(v.clone())).collect()
}

/// PgParam 数组 → `Client::execute` 形参 `&[&(dyn ToSql + Sync)]`。
fn param_refs(params: &[PgParam]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|p| p as &(dyn ToSql + Sync)).collect()
}

/// 从 start 起生成 `$n` 占位符串（IN 列表用）。
fn numbered_placeholders(start: usize, count: usize) -> String {
    (start..start + count).map(|i| format!("${i}")).collect::<Vec<_>>().join(", ")
}

/// 读一列：按列的实际类型严格读取（postgres crate 的 FromSql 逐一接受，错型会报错
/// 而非 panic）。NULL → Value::Null；create_table 产出的四类列全覆盖。
fn cell(row: &postgres::Row, i: usize) -> Result<Value> {
    let ty = row.columns()[i].type_();
    Ok(if *ty == Type::INT8 {
        row.try_get::<_, Option<i64>>(i)?.map(Value::Int).unwrap_or(Value::Null)
    } else if *ty == Type::INT4 {
        row.try_get::<_, Option<i32>>(i)?
            .map(|v| Value::Int(i64::from(v)))
            .unwrap_or(Value::Null)
    } else if *ty == Type::INT2 {
        row.try_get::<_, Option<i16>>(i)?
            .map(|v| Value::Int(i64::from(v)))
            .unwrap_or(Value::Null)
    } else if *ty == Type::FLOAT8 {
        row.try_get::<_, Option<f64>>(i)?.map(Value::Real).unwrap_or(Value::Null)
    } else if *ty == Type::FLOAT4 {
        row.try_get::<_, Option<f32>>(i)?
            .map(|v| Value::Real(f64::from(v)))
            .unwrap_or(Value::Null)
    } else if *ty == Type::BYTEA {
        row.try_get::<_, Option<Vec<u8>>>(i)?.map(Value::Blob).unwrap_or(Value::Null)
    } else {
        // text 家族（TEXT/VARCHAR/BPCHAR/NAME）及其余可文本化类型
        match row.try_get::<_, Option<String>>(i)? {
            Some(s) => Value::Text(s),
            None => Value::Null,
        }
    })
}

/// information_schema.columns：列名/data_type/udt_name（定义序）。
fn raw_columns(
    client: &mut Client,
    table: &str,
) -> Result<Vec<(String, String, String)>> {
    let rows = client.query(
        "SELECT column_name, data_type, udt_name FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position",
        &[&table],
    )?;
    Ok(rows
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1), r.get::<_, String>(2)))
        .collect())
}

/// 结果集 → Row（列名 → 值）。
fn row_to_row(row: &postgres::Row) -> Result<Row> {
    let mut out = Row::new();
    for (i, col) in row.columns().iter().enumerate() {
        out.insert(col.name().to_string(), cell(row, i)?);
    }
    Ok(out)
}

/// information_schema.columns：列名（定义序）。
fn table_columns(client: &mut Client, table: &str) -> Result<Vec<String>> {
    let rows = client.query(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position",
        &[&table],
    )?;
    Ok(rows.iter().map(|r| r.get::<_, String>(0)).collect())
}

/// 主键三级回退：约束第一列（复合主键取 ordinal_position=1）→ 第一列 → 字面量 "id"。
fn table_pk(client: &mut Client, table: &str) -> Result<String> {
    let rows = client.query(
        "SELECT kcu.column_name FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
           ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema \
         WHERE tc.table_schema = 'public' AND tc.table_name = $1 \
           AND tc.constraint_type = 'PRIMARY KEY' \
         ORDER BY kcu.ordinal_position LIMIT 1",
        &[&table],
    )?;
    if let Some(r) = rows.first() {
        return Ok(r.get::<_, String>(0));
    }
    let cols = table_columns(client, table)?;
    Ok(cols.first().cloned().unwrap_or_else(|| "id".to_string()))
}

/// 按 pk 查一行。pk 列 `::text` 化后与字符串 id 比较：bigint/text 主键统一按字符串
/// 命中（对位 sqlite 的类型亲和转换）。
fn fetch_by_pk(client: &mut Client, table: &str, pk: &str, id: &str) -> Result<Option<Row>> {
    let t = quote_ident(table)?;
    let p = quote_ident(pk)?;
    let rows =
        client.query(format!("SELECT * FROM {t} WHERE {p}::text = $1 LIMIT 1").as_str(), &[&id])?;
    rows.first().map(row_to_row).transpose()
}

/// 单行 INSERT（insert / insert_rows 共用；Transaction 同样实现 GenericClient）。
fn exec_insert<C: GenericClient>(client: &mut C, table: &str, row: &Row) -> Result<()> {
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
    let placeholders = numbered_placeholders(1, cols.len());
    let sql =
        format!("INSERT INTO {t} ({}) VALUES ({placeholders})", cols.join(", "));
    let bind: Vec<Value> = row.values().cloned().collect();
    let params = owned_params(&bind);
    client.execute(sql.as_str(), &param_refs(&params))?;
    Ok(())
}

/// pg data_type → 逻辑类型（与 dbsync 的读取口径一致：boolean/整型族→Integer，
/// 浮点族/numeric→Real，bytea→Blob，其余→Text）
fn pg_data_type_to_logical(data_type: &str) -> LogicalType {
    match data_type {
        "boolean"
        | "smallint"
        | "integer"
        | "bigint"
        | "serial"
        | "smallserial"
        | "bigserial"
        | "int2"
        | "int4"
        | "int8" => LogicalType::Integer,
        "real" | "double precision" | "float4" | "float8" | "numeric" | "decimal" => {
            LogicalType::Real
        }
        "bytea" => LogicalType::Blob,
        _ => LogicalType::Text,
    }
}

impl Store for PgStore {
    fn list_tables(&mut self) -> Result<Vec<TableInfo>> {
        // 3 次往返替代原 N+1（1 + 2N 次）：表名 / 全库列清单 / 全库主键首列。
        // 数据字典页每次刷新都经此路径（demo2 实测 16 表 ≈ 44ms → 3 次往返 <2ms）。
        let mut c = self.client();
        let rows = c.query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_type = 'BASE TABLE' ORDER BY table_name",
            &[],
        )?;
        let names: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();

        let mut cols_by_table: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let rows = c.query(
            "SELECT table_name, column_name FROM information_schema.columns \
             WHERE table_schema = 'public' ORDER BY table_name, ordinal_position",
            &[],
        )?;
        for r in &rows {
            cols_by_table
                .entry(r.get::<_, String>(0))
                .or_default()
                .push(r.get::<_, String>(1));
        }

        // 每表取 kcu.ordinal_position 最小的一条（与单表版 ORDER BY ... LIMIT 1 同语义）
        let mut pk_by_table: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let rows = c.query(
            "SELECT DISTINCT ON (tc.table_name) tc.table_name, kcu.column_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema \
             WHERE tc.table_schema = 'public' AND tc.constraint_type = 'PRIMARY KEY' \
             ORDER BY tc.table_name, kcu.ordinal_position",
            &[],
        )?;
        for r in &rows {
            pk_by_table.insert(r.get::<_, String>(0), r.get::<_, String>(1));
        }

        let mut out = Vec::with_capacity(names.len());
        for name in names {
            safe_ident(&name)?;
            let columns = cols_by_table.remove(&name).unwrap_or_default();
            // 主键回退链与 table_pk 一致：约束首列 → 第一列 → 字面量 "id"
            let primary_key = pk_by_table
                .remove(&name)
                .unwrap_or_else(|| columns.first().cloned().unwrap_or_else(|| "id".to_string()));
            out.push(TableInfo { name, primary_key, columns });
        }
        Ok(out)
    }

    fn select(&mut self, table: &str, filters: &[Filter], sort: &[Sort], page: Page) -> Result<(Vec<Row>, i64)> {
        let t = quote_ident(table)?;
        let mut c = self.client();
        // A3：富类型列（bool/timestamptz/jsonb/numeric/uuid 等驱动不可原生读），
        // 按列类型生成读取表达式（与 dbsync PgSource 同款口径），别名回原列名
        let raw_cols = raw_columns(&mut c, table)?;
        let select_list: Vec<String> = raw_cols
            .iter()
            .map(|(n, dt, u)| {
                let q = quote_ident(n)?;
                Ok(format!("{} AS {q}", dbsync::pg_read_expr(&q, dbsync::pg_col_kind(dt, u))))
            })
            .collect::<std::result::Result<Vec<String>, ZpmError>>()?;
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
                        let placeholders = numbered_placeholders(bind.len() + 1, f.values.len());
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
                        // like→ILIKE（sqlite LIKE 不分大小写已由通配测试锚定语义），
                        // 列侧 ::text 文本化以对位 sqlite LIKE 的文本匹配
                        Op::Like => "ILIKE",
                        Op::In => unreachable!(),
                    };
                    // 单值操作符取 values[0]（长度 1 由解析层保证，Store 层不校验）
                    let v = f.values.first().cloned().unwrap_or(Value::Null);
                    let col_expr =
                        if f.op == Op::Like { format!("{ff}::text", ff = ff) } else { ff.clone() };
                    where_sql.push_str(&format!("{col_expr} {op} ${}", bind.len() + 1));
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

        let params = owned_params(&bind);
        let refs = param_refs(&params);

        // total：同 where 的独立计数，与分页无关
        let total: i64 = c
            .query_one(format!("SELECT COUNT(*) FROM {t}{where_clause}").as_str(), &refs)?
            .get::<_, i64>(0);

        let mut sql = format!("SELECT {} FROM {t}{where_clause}", select_list.join(", "));
        if sort.is_empty() {
            sql.push_str(" ORDER BY ctid");
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
        sql.push_str(&format!(
            " LIMIT ${} OFFSET ${}",
            bind.len() + 1,
            bind.len() + 2
        ));
        bind.push(Value::Int(i64::try_from(page.page_size).unwrap_or(i64::MAX)));
        bind.push(Value::Int(i64::try_from(offset).unwrap_or(i64::MAX)));
        let params = owned_params(&bind);
        let rows = c.query(sql.as_str(), &param_refs(&params))?;
        let out = rows.iter().map(row_to_row).collect::<Result<Vec<_>>>()?;
        Ok((out, total))
    }

    fn get(&mut self, table: &str, id: &str) -> Result<Option<Row>> {
        safe_ident(table)?;
        let mut c = self.client();
        let pk = table_pk(&mut c, table)?;
        fetch_by_pk(&mut c, table, &pk, id)
    }

    fn insert(&mut self, table: &str, row: &Row) -> Result<Row> {
        let mut c = self.client();
        exec_insert(&mut *c, table, row)?;
        Ok(row.clone()) // 契约：回显入参，不回读
    }

    fn update(&mut self, table: &str, id: &str, patch: &Row) -> Result<Option<Row>> {
        let t = quote_ident(table)?;
        let mut c = self.client();
        let pk = table_pk(&mut c, table)?;
        if patch.is_empty() {
            return fetch_by_pk(&mut c, table, &pk, id); // 空 patch：返回当前行，不发 UPDATE
        }
        // SET $1..$k，WHERE "pk"::text = $k+1；RETURNING * 一次往返完成回读
        let pkq = quote_ident(&pk)?;
        let mut bind: Vec<Value> = patch.values().cloned().collect();
        let sets: Vec<String> = patch
            .keys()
            .enumerate()
            .map(|(i, col)| {
                Ok(format!("{} = ${}", quote_ident(col)?, i + 1))
            })
            .collect::<std::result::Result<Vec<String>, ZpmError>>()?;
        let sql = format!(
            "UPDATE {t} SET {} WHERE {pkq}::text = ${} RETURNING *",
            sets.join(", "),
            bind.len() + 1
        );
        bind.push(Value::Text(id.to_string()));
        let params = owned_params(&bind);
        let rows = c.query(sql.as_str(), &param_refs(&params))?;
        rows.first().map(row_to_row).transpose() // 0 行 → None（id 不存在）
    }

    fn delete(&mut self, table: &str, id: &str) -> Result<bool> {
        let t = quote_ident(table)?;
        let mut c = self.client();
        let p = quote_ident(&table_pk(&mut c, table)?)?;
        let n =
            c.execute(format!("DELETE FROM {t} WHERE {p}::text = $1").as_str(), &[&id])?;
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
                    LogicalType::Text => "text",
                    LogicalType::Integer => "bigint",
                    LogicalType::Real => "double precision",
                    LogicalType::Blob => "bytea",
                };
                if name == pk {
                    Ok(format!("{pkq} {sql_ty} PRIMARY KEY"))
                } else {
                    Ok(format!("{n} {sql_ty}"))
                }
            })
            .collect::<Result<Vec<_>>>()?;
        self.client().execute(format!("CREATE TABLE {table} ({})", defs.join(", ")).as_str(), &[])?;
        Ok(())
    }

    fn insert_rows(&mut self, table: &str, rows: &[Row]) -> Result<()> {
        let mut c = self.client();
        for chunk in rows.chunks(500) {
            let mut tx = c.transaction()?;
            for row in chunk {
                exec_insert(&mut tx, table, row)?;
            }
            tx.commit()?;
        }
        Ok(())
    }

    fn swap_table(&mut self, final_name: &str, shadow_name: &str) -> Result<()> {
        let f = quote_ident(final_name)?;
        let s = quote_ident(shadow_name)?;
        let mut c = self.client();
        let mut tx = c.transaction()?;
        tx.execute(format!("DROP TABLE IF EXISTS {f}").as_str(), &[])?;
        tx.execute(format!("ALTER TABLE {s} RENAME TO {f}").as_str(), &[])?;
        tx.commit()?;
        Ok(())
    }

    fn drop_table(&mut self, table: &str) -> Result<()> {
        let t = quote_ident(table)?;
        let mut c = self.client();
        c.execute(format!("DROP TABLE IF EXISTS {t}").as_str(), &[])?;
        Ok(())
    }

    fn describe_table(&mut self, table: &str) -> Result<Vec<(String, LogicalType)>> {
        safe_ident(table)?;
        let mut c = self.client();
        let rows = c.query(
            "SELECT column_name, data_type FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position",
            &[&table],
        )?;
        if rows.is_empty() {
            return Err(ZpmError::NotFound(format!("table not found: {table}")));
        }
        Ok(rows
            .iter()
            .map(|r| {
                let name: String = r.get(0);
                let data_type: String = r.get(1);
                (name, pg_data_type_to_logical(&data_type))
            })
            .collect())
    }

    fn execute_raw(&mut self, sql: &str) -> Result<()> {
        let mut c = self.client();
        c.batch_execute(sql)?;
        Ok(())
    }

    fn row_counts(&mut self, tables: &[String]) -> Result<Vec<i64>> {
        // 单条 UNION ALL 一次往返拿全部行数（每 100 表一段防超长语句）；按名称回映射保证输出顺序
        let mut c = self.client();
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
            let rows = c.query(sql.as_str(), &[])?;
            let by_name: std::collections::HashMap<String, i64> = rows
                .iter()
                .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)))
                .collect();
            for t in chunk {
                out.push(by_name.get(t.as_str()).copied().unwrap_or(0));
            }
        }
        Ok(out)
    }

    /// 事务包裹的脚本执行（db seed）：任一语句失败回滚整体，错误带语句序号。
    fn execute_script(&mut self, stmts: &[String]) -> Result<()> {
        let mut c = self.client();
        let mut tx = c.transaction()?;
        for (i, stmt) in stmts.iter().enumerate() {
            tx.batch_execute(stmt)
                .map_err(|e| ZpmError::Format(format!("seed 第 {} 条语句失败：{e}", i + 1)))?;
        }
        tx.commit()?;
        Ok(())
    }

    fn table_pk(&mut self, table: &str) -> Result<String> {
        let mut c = self.client();
        table_pk(&mut c, table)
    }
}

#[cfg(test)]
mod tests {
    use crate::project_config::{PgConn, Target};
    use crate::store::{assert_store_semantics, open_store, Filter, LogicalType, Op, Page, Row, Store, Value};
    use postgres::{Client, NoTls};

    use super::PgStore;

    /// 解析 ZPM_TEST_PG 连接串（postgres://user[:pass]@host[:port]/db）→ PgConn，
    /// 顺带让 PgStore::connect 走 PgConn::conn_str 的往返路径。
    fn parse_test_conn(url: &str) -> PgConn {
        let rest = url.strip_prefix("postgres://").expect("连接串须以 postgres:// 开头");
        let (auth, host_db) = rest.split_once('@').expect("缺少 user@host 段");
        let (user, password) = match auth.split_once(':') {
            Some((u, p)) => (u.to_string(), Some(p.to_string())),
            None => (auth.to_string(), None),
        };
        let (host_port, database) = host_db.split_once('/').expect("缺少 /database 段");
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().expect("端口须为数字")),
            None => (host_port.to_string(), 5432),
        };
        PgConn { host, port, database: database.to_string(), user, password }
    }

    /// 测试门控：未设置 ZPM_TEST_PG → None（skip，不算失败）。
    fn gated() -> Option<(PgConn, String)> {
        let url = match std::env::var("ZPM_TEST_PG") {
            Ok(u) if !u.trim().is_empty() => u,
            _ => {
                eprintln!("skip pg test: ZPM_TEST_PG not set");
                return None;
            }
        };
        let conn = parse_test_conn(&url);
        Some((conn, url))
    }

    /// 已门控时建 PgStore；连接失败视为真失败（环境变量已明确指定目标库）。
    fn pg_store(conn: &PgConn) -> PgStore {
        PgStore::connect(conn).unwrap_or_else(|e| panic!("pg 连接失败：{e}"))
    }

    /// 清掉表/视图残留（各测试表名互不相同且带 pg_ 前缀，可并行、CASCADE 的
    /// 破坏半径不越过 pg_ 命名空间）。对同名异型（表/视图互降）的 DROP 报错
    /// 忽略——真失败会在后续 CREATE 时暴露。
    fn drop_all(client: &mut Client, names: &[&str]) {
        for n in names {
            let _ = client.batch_execute(format!("DROP TABLE IF EXISTS {n} CASCADE").as_str());
            let _ = client.batch_execute(format!("DROP VIEW IF EXISTS {n} CASCADE").as_str());
        }
    }

    /// 测试清场 guard：构造时前置 DROP 残留；Drop 时（测试正常返回**或 panic
    /// 展开退出**，如 unwrap 断言失败）再 DROP 一遍本测试的表/视图——测试失败
    /// 也不把产物残留在真库（逐字约束：测试产物不应残留在真库中）。
    /// 持独立 raw 连接（与 PgStore 的连接互不干扰）；DROP 名单就是本测试的
    /// pg_ 前缀专用表名，不触及业务表。
    struct CleanupGuard {
        client: Client,
        names: Vec<String>,
    }

    impl CleanupGuard {
        /// 建连接（失败视为真失败：环境变量已明确指定目标库）并前置清场。
        fn setup(url: &str, names: &[&str]) -> CleanupGuard {
            let mut client =
                Client::connect(url, NoTls).unwrap_or_else(|e| panic!("pg 连接失败：{e}"));
            drop_all(&mut client, names);
            CleanupGuard { client, names: names.iter().map(|s| (*s).to_string()).collect() }
        }

        /// 测试中取 raw 连接（查 information_schema、预置视图等）。
        fn client(&mut self) -> &mut Client {
            &mut self.client
        }
    }

    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let names: Vec<&str> = self.names.iter().map(String::as_str).collect();
            drop_all(&mut self.client, &names);
        }
    }

    /// ① create_table 类型映射：text→text / integer→bigint / real→double precision / blob→bytea
    #[test]
    fn create_table_maps_logical_types_to_pg_types() {
        let Some((conn, url)) = gated() else { return };
        let mut raw = CleanupGuard::setup(&url, &["pg_ty_t"]);
        let mut s = pg_store(&conn);
        s.create_table(
            "pg_ty_t",
            &[
                ("id".into(), LogicalType::Integer),
                ("name".into(), LogicalType::Text),
                ("score".into(), LogicalType::Real),
                ("data".into(), LogicalType::Blob),
            ],
            "id",
        )
        .unwrap();
        let rows = raw
            .client()
            .query(
                "SELECT column_name, data_type FROM information_schema.columns \
                 WHERE table_schema = 'public' AND table_name = 'pg_ty_t' \
                 ORDER BY ordinal_position",
                &[],
            )
            .unwrap();
        let got: Vec<(String, String)> = rows
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
            .collect();
        assert_eq!(
            got,
            vec![
                ("id".into(), "bigint".into()),
                ("name".into(), "text".into()),
                ("score".into(), "double precision".into()),
                ("data".into(), "bytea".into()),
            ]
        );
    }

    /// ③ list_tables 走 information_schema public BASE TABLE：视图不出现；
    /// 主键取约束第一列（复合主键 PRIMARY KEY (b, a) → b），列按定义序。
    #[test]
    fn list_tables_reads_public_base_tables_and_pk_first_column() {
        let Some((conn, url)) = gated() else { return };
        let mut raw = CleanupGuard::setup(&url, &["pg_list_t", "pg_list_v", "pg_pk_t"]);
        raw.client().batch_execute("CREATE VIEW pg_list_v AS SELECT 1 AS x").unwrap();
        raw.client()
            .batch_execute("CREATE TABLE pg_pk_t (a integer, b integer, c text, PRIMARY KEY (b, a))")
            .unwrap();
        let mut s = pg_store(&conn);
        s.create_table(
            "pg_list_t",
            &[("a".into(), LogicalType::Text), ("b".into(), LogicalType::Integer)],
            "a",
        )
        .unwrap();
        let tables = s.list_tables().unwrap();
        let t = tables.iter().find(|t| t.name == "pg_list_t").expect("pg_list_t 应在列表中");
        assert_eq!(t.primary_key, "a");
        assert_eq!(t.columns, vec!["a".to_string(), "b".to_string()]);
        let t = tables.iter().find(|t| t.name == "pg_pk_t").expect("pg_pk_t 应在列表中");
        assert_eq!(t.primary_key, "b", "复合主键取约束第一列");
        assert_eq!(t.columns, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
        assert!(!tables.iter().any(|t| t.name == "pg_list_v"), "视图不是 BASE TABLE，不应出现");
    }

    /// ② insert/update/delete/select 与 sqlite 语义一致（双方言共用同一组断言函数）
    #[test]
    fn store_semantics_match_sqlite_shared_contract() {
        let Some((conn, url)) = gated() else { return };
        let _raw = CleanupGuard::setup(&url, &["pg_sem_t", "pg_ord_t", "pg_like_t"]);
        let mut s = pg_store(&conn);
        assert_store_semantics(&mut s);
    }

    /// ④ 无 sort 默认 ctid 序 = 插入序（分页连续不重不漏）
    ///
    /// 表名 `pg_ctid_t`：共享断言已占用 `pg_ord_t`，各测试表名保持互不相同（可并行）。
    #[test]
    fn select_without_sort_defaults_to_ctid_insertion_order() {
        let Some((conn, url)) = gated() else { return };
        let _raw = CleanupGuard::setup(&url, &["pg_ctid_t"]);
        let mut s = pg_store(&conn);
        s.create_table("pg_ctid_t", &[("name".into(), LogicalType::Text)], "name").unwrap();
        // 乱序值插入（e,a,b,c,d）
        for name in ["e", "a", "b", "c", "d"] {
            let mut r = Row::new();
            r.insert("name".into(), Value::Text(name.into()));
            s.insert("pg_ctid_t", &r).unwrap();
        }
        let (rows, total) = s.select("pg_ctid_t", &[], &[], Page { page: 1, page_size: 100 }).unwrap();
        assert_eq!(total, 5);
        let names: Vec<_> = rows.iter().map(|r| r["name"].clone()).collect();
        assert_eq!(
            names,
            vec![
                Value::Text("e".into()),
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Text("c".into()),
                Value::Text("d".into()),
            ]
        );
        // 分页接续：第 2 页从第 3 行起
        let (rows, total) = s.select("pg_ctid_t", &[], &[], Page { page: 2, page_size: 2 }).unwrap();
        assert_eq!(total, 5);
        let names: Vec<_> = rows.iter().map(|r| r["name"].clone()).collect();
        assert_eq!(names, vec![Value::Text("b".into()), Value::Text("c".into())]);
    }

    /// ⑤ int8 读写 i64：2147483648 存取不损（超出 i32 范围）
    #[test]
    fn int8_values_roundtrip_as_i64() {
        let Some((conn, url)) = gated() else { return };
        let _raw = CleanupGuard::setup(&url, &["pg_i8_t"]);
        let mut s = pg_store(&conn);
        s.create_table("pg_i8_t", &[("n".into(), LogicalType::Integer)], "n").unwrap();
        let mut r = Row::new();
        r.insert("n".into(), Value::Int(2147483648));
        let echoed = s.insert("pg_i8_t", &r).unwrap();
        assert_eq!(echoed, r);
        let got = s.get("pg_i8_t", "2147483648").unwrap().unwrap();
        assert_eq!(got["n"], Value::Int(2147483648));
        let f = Filter { field: "n".into(), op: Op::Eq, values: vec![Value::Int(2147483648)] };
        let (rows, total) = s.select("pg_i8_t", &[f], &[], Page { page: 1, page_size: 10 }).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0]["n"], Value::Int(2147483648));
    }

    /// 多 filter 组合下 $n 编号无错位：Gt($1) AND In($2,$3) / Gt($1) AND ILIKE($2)
    #[test]
    fn select_combines_filters_with_sequential_placeholders() {
        let Some((conn, url)) = gated() else { return };
        let _raw = CleanupGuard::setup(&url, &["pg_ph_t"]);
        let mut s = pg_store(&conn);
        s.create_table(
            "pg_ph_t",
            &[
                ("id".into(), LogicalType::Integer),
                ("name".into(), LogicalType::Text),
                ("n".into(), LogicalType::Integer),
            ],
            "id",
        )
        .unwrap();
        for (id, name, n) in [(1i64, "alpha", 10i64), (2, "beta", 20), (3, "gamma", 30)] {
            let mut r = Row::new();
            r.insert("id".into(), Value::Int(id));
            r.insert("name".into(), Value::Text(name.into()));
            r.insert("n".into(), Value::Int(n));
            s.insert("pg_ph_t", &r).unwrap();
        }
        // n > $1 AND name ILIKE $2：beta/gamma（alpha 的 n=10 不满足 Gt）
        let f1 = Filter { field: "n".into(), op: Op::Gt, values: vec![Value::Int(15)] };
        let f2 = Filter { field: "name".into(), op: Op::Like, values: vec![Value::Text("a".into())] };
        let (rows, total) =
            s.select("pg_ph_t", &[f1, f2], &[], Page { page: 1, page_size: 10 }).unwrap();
        assert_eq!(total, 2);
        let names: Vec<_> = rows.iter().map(|r| r["name"].clone()).collect();
        assert_eq!(names, vec![Value::Text("beta".into()), Value::Text("gamma".into())]);
        // n > $1 AND name IN ($2,$3)：beta/gamma
        let f1 = Filter { field: "n".into(), op: Op::Gt, values: vec![Value::Int(15)] };
        let f2 = Filter {
            field: "name".into(),
            op: Op::In,
            values: vec![Value::Text("beta".into()), Value::Text("gamma".into())],
        };
        let (_, total) =
            s.select("pg_ph_t", &[f1, f2], &[], Page { page: 1, page_size: 10 }).unwrap();
        assert_eq!(total, 2);
        // 三 filter：Gt AND In AND Like —— 编号继续顺延
        let f1 = Filter { field: "id".into(), op: Op::Gt, values: vec![Value::Int(0)] };
        let f2 = Filter { field: "n".into(), op: Op::In, values: vec![Value::Int(10), Value::Int(30)] };
        let f3 = Filter { field: "name".into(), op: Op::Like, values: vec![Value::Text("a".into())] };
        let (rows, total) =
            s.select("pg_ph_t", &[f1, f2, f3], &[], Page { page: 1, page_size: 10 }).unwrap();
        assert_eq!(total, 2);
        let names: Vec<_> = rows.iter().map(|r| r["name"].clone()).collect();
        assert_eq!(names, vec![Value::Text("alpha".into()), Value::Text("gamma".into())]);
    }

    /// open_store：Target::Postgres 分派到 PgStore（Box<dyn Store> 可用）
    #[test]
    fn open_store_dispatches_postgres_target() {
        let Some((conn, url)) = gated() else { return };
        let _raw = CleanupGuard::setup(&url, &["pg_open_t"]);
        let mut s = open_store(&Target::Postgres(conn)).unwrap();
        s.create_table(
            "pg_open_t",
            &[("id".into(), LogicalType::Integer), ("name".into(), LogicalType::Text)],
            "id",
        )
        .unwrap();
        let mut r = Row::new();
        r.insert("id".into(), Value::Int(1));
        r.insert("name".into(), Value::Text("x".into()));
        s.insert("pg_open_t", &r).unwrap();
        let (rows, total) = s.select("pg_open_t", &[], &[], Page { page: 1, page_size: 10 }).unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0]["name"], Value::Text("x".into()));
        assert!(s.delete("pg_open_t", "1").unwrap());
    }

    /// A3：富类型列经 select 读取表达式正常返回（不再整表 500）。
    /// bool→1/0、ts→ISO 文本、uuid/jsonb→文本；numeric 以 ::text 文本形态返回
    /// （Real 转换属 dbsync coerce 路径，select 层不做）
    #[test]
    fn select_rich_types_via_read_exprs() {
        let Some((conn, url)) = gated() else { return };
        let mut raw = CleanupGuard::setup(&url, &["pg_rich_t"]);
        raw.client()
            .batch_execute(
                "CREATE TABLE pg_rich_t (id int4 PRIMARY KEY, b boolean, ts timestamptz, u uuid, j jsonb, n numeric)",
            )
            .unwrap();
        raw.client()
            .batch_execute(
                "INSERT INTO pg_rich_t VALUES (1, true, '2026-01-02T03:04:05Z'::timestamptz, \
                 '550e8400-e29b-41d4-a716-446655440000', '{\"k\":1}', '42.50')",
            )
            .unwrap();
        let mut s = pg_store(&conn);
        let (rows, total) = s
            .select("pg_rich_t", &[], &[], Page { page: 1, page_size: 10 })
            .unwrap();
        assert_eq!(total, 1);
        let r = &rows[0];
        assert_eq!(r["b"], Value::Int(1), "boolean→1/0");
        assert!(matches!(&r["ts"], Value::Text(t) if t.starts_with("20")), "ts→ISO 文本：{:?}", r["ts"]);
        assert!(matches!(&r["u"], Value::Text(_)), "uuid→文本：{:?}", r["u"]);
        assert!(matches!(&r["j"], Value::Text(_)), "jsonb→JSON 文本");
        assert!(matches!(&r["n"], Value::Text(t) if t.starts_with("42")), "numeric→文本形态：{:?}", r["n"]);
    }

}
