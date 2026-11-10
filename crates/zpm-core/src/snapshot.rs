//! 快照：目标库 → JSON（`zpm-snapshot@1`）→ 目标库。格式契约对齐 TS 版 sync/snapshot.ts：
//! `{ format, exportedAt, tables: [{ name, columns: [{name,type,pk}], rows }] }`。
//! 导入复用数据同步的 shadow-rename 通道（dbsync::load_into_target），格式不符报
//! `unsupported snapshot format`。Blob 列序列化为 hex 字符串（v1 约定）。

use crate::dbsync::load_into_target;
use crate::error::ZpmError;
use crate::store::{LogicalType, Page, Row, Store, Value};
use serde_json::{json, Value as Json};

pub const SNAPSHOT_FORMAT: &str = "zpm-snapshot@1";

fn logical_to_str(t: LogicalType) -> &'static str {
    match t {
        LogicalType::Text => "text",
        LogicalType::Integer => "integer",
        LogicalType::Real => "real",
        LogicalType::Blob => "blob",
    }
}

fn logical_from_str(s: &str) -> LogicalType {
    match s {
        "integer" => LogicalType::Integer,
        "real" => LogicalType::Real,
        "blob" => LogicalType::Blob,
        _ => LogicalType::Text,
    }
}

fn value_to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Int(i) => json!(i),
        Value::Real(f) => json!(f),
        Value::Text(s) => json!(s),
        Value::Blob(b) => json!(hex::encode(b)),
    }
}

fn json_to_value(v: &Json) -> Value {
    match v {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Int(i64::from(*b)),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Real(n.as_f64().unwrap_or_default()),
        },
        Json::String(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

fn row_to_json(r: &Row) -> Json {
    let mut m = serde_json::Map::new();
    for (k, v) in r {
        m.insert(k.clone(), value_to_json(v));
    }
    Json::Object(m)
}

/// 目标库全量导出为快照 JSON（所有表，含列类型与主键标记）。
pub fn export(target: &mut dyn Store) -> Result<Json, ZpmError> {
    let mut tables = Vec::new();
    for t in target.list_tables()? {
        let cols = target.describe_table(&t.name)?;
        let columns: Vec<Json> = cols
            .iter()
            .map(|(n, ty)| {
                json!({
                    "name": n,
                    "type": logical_to_str(*ty),
                    "pk": n == &t.primary_key,
                })
            })
            .collect();
        // 全量分页拉取（pageSize 上限 1000）
        let mut rows_json = Vec::new();
        let mut page = 1u64;
        loop {
            let (rows, _) =
                target.select(&t.name, &[], &[], Page { page, page_size: 1000 })?;
            let got = rows.len();
            for r in rows {
                rows_json.push(row_to_json(&r));
            }
            if got < 1000 {
                break;
            }
            page += 1;
        }
        tables.push(json!({
            "name": t.name,
            "columns": columns,
            "rows": rows_json,
        }));
    }
    Ok(json!({
        "format": SNAPSHOT_FORMAT,
        "exportedAt": chrono::Utc::now().to_rfc3339(),
        "tables": tables,
    }))
}

/// 快照 JSON → 目标库（shadow-rename 全量替换，幂等）。
pub fn import(
    target: &mut dyn Store,
    snap: &Json,
) -> Result<Vec<crate::dbsync::SyncReport>, ZpmError> {
    let fmt = snap.get("format").and_then(|v| v.as_str()).unwrap_or("");
    if fmt != SNAPSHOT_FORMAT {
        return Err(ZpmError::Format(format!("unsupported snapshot format: {fmt}")));
    }
    let mut reports = Vec::new();
    let tables = snap.get("tables").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for t in tables {
        let name = t
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ZpmError::Format("snapshot 表缺少 name".into()))?
            .to_string();
        let mut cols: Vec<(String, LogicalType)> = Vec::new();
        let mut pk = String::new();
        for c in t.get("columns").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let n = c
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ZpmError::Format(format!("snapshot {name} 列缺少 name")))?
                .to_string();
            let ty = logical_from_str(c.get("type").and_then(|v| v.as_str()).unwrap_or("text"));
            if c.get("pk").and_then(|v| v.as_bool()).unwrap_or(false) {
                pk = n.clone();
            }
            cols.push((n, ty));
        }
        if pk.is_empty() {
            pk = cols
                .first()
                .map(|(n, _)| n.clone())
                .ok_or_else(|| ZpmError::Format(format!("snapshot {name} 缺少列定义")))?;
        }
        let mut rows = Vec::new();
        let types: std::collections::BTreeMap<&str, LogicalType> =
            cols.iter().map(|(n, ty)| (n.as_str(), *ty)).collect();
        for r in t.get("rows").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let obj = r
                .as_object()
                .ok_or_else(|| ZpmError::Format(format!("snapshot {name} 行必须是对象")))?;
            let mut row = Row::new();
            for (k, v) in obj {
                let mut val = json_to_value(v);
                // Blob 列的 hex 约定在导入方向解码（导出把字节编码为 hex 字符串；
                // 解码失败回退 Text 以兼容外来源）
                if matches!(types.get(k.as_str()), Some(LogicalType::Blob)) {
                    if let Value::Text(s) = &val {
                        if let Ok(bytes) = hex::decode(s) {
                            val = Value::Blob(bytes);
                        }
                    }
                }
                row.insert(k.clone(), val);
            }
            rows.push(row);
        }
        let n = load_into_target(target, &name, &cols, &pk, rows)?;
        reports.push(crate::dbsync::SyncReport { table: name, rows: n });
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{open_sqlite, SqliteStore, TableInfo};

    /// 造一个含两表若干行的目标库（scores 表带 Blob 列，锚定 hex 往返）
    fn seeded(file: &std::path::Path) -> SqliteStore {
        let mut s = open_sqlite(file).unwrap();
        s.create_table(
            "users",
            &[("id".into(), LogicalType::Integer), ("name".into(), LogicalType::Text)],
            "id",
        )
        .unwrap();
        s.create_table(
            "scores",
            &[
                ("uid".into(), LogicalType::Integer),
                ("val".into(), LogicalType::Real),
                ("doc".into(), LogicalType::Blob),
            ],
            "uid",
        )
        .unwrap();
        let mut r1 = Row::new();
        r1.insert("id".into(), Value::Int(1));
        r1.insert("name".into(), Value::Text("张三".into()));
        s.insert_rows("users", &[r1]).unwrap();
        let mut r2 = Row::new();
        r2.insert("uid".into(), Value::Int(1));
        r2.insert("val".into(), Value::Real(2.5));
        r2.insert("doc".into(), Value::Blob(vec![0xDE, 0xAD]));
        s.insert_rows("scores", &[r2]).unwrap();
        s
    }

    /// ③ 导出格式契约 + 导入往返 + 幂等 + 格式校验
    #[test]
    fn export_import_roundtrip_and_format_check() {
        let tmp = tempfile::tempdir().unwrap();
        let mut src = seeded(&tmp.path().join("a.db"));
        let snap = export(&mut src).unwrap();

        // 首字段与格式
        assert_eq!(snap["format"], SNAPSHOT_FORMAT);
        assert!(snap["exportedAt"].as_str().is_some());
        let tables = snap["tables"].as_array().unwrap();
        assert_eq!(tables.len(), 2);
        let users = tables.iter().find(|t| t["name"] == "users").unwrap();
        assert_eq!(users["rows"].as_array().unwrap().len(), 1);
        assert_eq!(users["columns"][0]["name"], "id");
        assert_eq!(users["columns"][0]["pk"], true);

        // 导入到空库
        let mut dst = open_sqlite(&tmp.path().join("b.db")).unwrap();
        let reports = import(&mut dst, &snap).unwrap();
        assert_eq!(reports.len(), 2);
        let (_, total) =
            dst.select("users", &[], &[], Page { page: 1, page_size: 100 }).unwrap();
        assert_eq!(total, 1);
        let got_users = dst.get("users", "1").unwrap().unwrap();
        assert_eq!(got_users["name"], Value::Text("张三".into()));
        let got = dst.get("scores", "1").unwrap().unwrap();
        assert_eq!(got["val"], Value::Real(2.5));
        assert_eq!(got["doc"], Value::Blob(vec![0xDE, 0xAD]), "Blob hex 往返必须还原原始字节");

        // 幂等：再导两次行数不变
        import(&mut dst, &snap).unwrap();
        let (_, total2) =
            dst.select("users", &[], &[], Page { page: 1, page_size: 100 }).unwrap();
        assert_eq!(total2, 1);
        let names: Vec<String> =
            dst.list_tables().unwrap().into_iter().map(|t: TableInfo| t.name).collect();
        assert!(!names.iter().any(|n| n.starts_with("__shadow_")));

        // 格式不符
        let bad = json!({"format": "nope"});
        let err = import(&mut dst, &bad).unwrap_err();
        assert!(err.to_string().contains("unsupported snapshot format"), "实际错误：{err}");
    }
}
