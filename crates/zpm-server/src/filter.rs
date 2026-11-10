//! filter 解析：where/sort/page → Store 查询参数（TS filter.ts 契约）。
//!
//! - where：可重复 `where=字段:操作符:值`（AND）；值可含 `:`（splitn(3)）；操作符恰 6 个
//!   eq/ne/gt/lt/like/in；like 双侧 `%` 由 Store 层包装；in 按 `,` 切分**不去空格**（多值）
//! - 值强转：数字形态字符串（i64/f64 可解析）→ 数值，否则 Text
//! - 错误契约（BadRequest，API 层映射 400）：片段<3 `invalid where`；未知操作符 `invalid op`；
//!   字段不在列集 `unknown field`
use zpm_core::error::ZpmError;
use zpm_core::store::{Filter, Op, Page, Sort, Value};

/// 解析 where 参数集（每条 `字段:操作符:值`，多条 AND）
pub fn parse_where(pairs: &[String], columns: &[String]) -> Result<Vec<Filter>, ZpmError> {
    let mut out = Vec::new();
    for raw in pairs {
        let parts: Vec<&str> = raw.splitn(3, ':').collect();
        if parts.len() < 3 {
            return Err(ZpmError::BadRequest(format!("invalid where: {raw}")));
        }
        let (field, op_s, value_s) = (parts[0], parts[1], parts[2]);
        let op = match op_s {
            "eq" => Op::Eq,
            "ne" => Op::Ne,
            "gt" => Op::Gt,
            "lt" => Op::Lt,
            "like" => Op::Like,
            "in" => Op::In,
            _ => {
                return Err(ZpmError::BadRequest(format!("invalid op: {op_s}")));
            }
        };
        if !columns.iter().any(|c| c == field) {
            return Err(ZpmError::BadRequest(format!("unknown field: {field}")));
        }
        // like 值不做强转（TS filter.ts 豁免：`%${v}%` 用原始字符串）；
        // in 按 `,` 切分不去空格，逐值强转；其余单值强转
        let values: Vec<Value> = if matches!(op, Op::Like) {
            vec![Value::Text(value_s.to_string())]
        } else if matches!(op, Op::In) {
            value_s.split(',').map(coerce_value).collect()
        } else {
            vec![coerce_value(value_s)]
        };
        out.push(Filter { field: field.to_string(), op, values });
    }
    Ok(out)
}

/// TS 正则 `/^-?\d+(\.\d+)?$/` 的字符级等价实现：
/// 可选负号 + 至少一位整数 + 可选 `.数字`（`.5`、`4.`、`1e3`、`+42` 均不匹配）
fn ts_number_shape(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    let (int_part, frac_part) = match body.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (body, None),
    };
    if int_part.is_empty() || !int_part.bytes().all(|c| c.is_ascii_digit()) {
        return false;
    }
    match frac_part {
        None => true,
        Some(frac) => !frac.is_empty() && frac.bytes().all(|c| c.is_ascii_digit()),
    }
}

/// 值强转（TS coerce 语义）：匹配 `/^-?\d+(\.\d+)?$/` → 数值（含 `.` 走 Real，
/// 整数溢出 i64 也落 Real 对齐 TS number），否则保留 Text
pub fn coerce_value(s: &str) -> Value {
    if !ts_number_shape(s) {
        return Value::Text(s.to_string());
    }
    if !s.contains('.') {
        if let Ok(i) = s.parse::<i64>() {
            return Value::Int(i);
        }
    }
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => Value::Real(f),
        _ => Value::Text(s.to_string()),
    }
}

/// 解析 sort：逗号分隔多键，前缀 `-` 为 desc；字段须存在
pub fn parse_sort(raw: &str, columns: &[String]) -> Result<Vec<Sort>, ZpmError> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let (field, desc) = match part.strip_prefix('-') {
            Some(f) => (f, true),
            None => (part, false),
        };
        if !columns.iter().any(|c| c == field) {
            return Err(ZpmError::BadRequest(format!("unknown field: {field}")));
        }
        out.push(Sort { field: field.to_string(), desc });
    }
    Ok(out)
}

/// 解析分页：page 默认 1；pageSize 默认 20、上限 1000；非法值回落默认（TS Number(x)||default 语义）
pub fn parse_page(page: Option<&str>, page_size: Option<&str>) -> Page {
    let p = page
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(1);
    let ps = page_size
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(20)
        .min(1000);
    Page { page: p, page_size: ps }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<String> {
        vec!["id".into(), "name".into(), "ts".into(), "tag".into()]
    }

    #[test]
    fn parse_where_basic_eq() {
        let out = parse_where(&["name:eq:tom".into()], &cols()).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].field, "name");
        assert!(matches!(out[0].op, Op::Eq));
        assert_eq!(out[0].values, vec![Value::Text("tom".into())]);
    }

    #[test]
    fn value_may_contain_colon() {
        let out = parse_where(&["ts:eq:2026-01-01:00:00".into()], &cols()).unwrap();
        assert_eq!(out[0].values, vec![Value::Text("2026-01-01:00:00".into())]);
    }

    #[test]
    fn in_keeps_inner_spaces() {
        let out = parse_where(&["tag:in:a, b".into()], &cols()).unwrap();
        assert_eq!(
            out[0].values,
            vec![Value::Text("a".into()), Value::Text(" b".into())]
        );
    }

    #[test]
    fn unknown_op_and_short_fragment_and_unknown_field() {
        let e = parse_where(&["name:zz:1".into()], &cols()).unwrap_err();
        assert!(matches!(&e, ZpmError::BadRequest(m) if m.starts_with("invalid op: zz")), "实际：{e}");
        let e = parse_where(&["name:eq".into()], &cols()).unwrap_err();
        assert!(matches!(&e, ZpmError::BadRequest(m) if m.starts_with("invalid where: name:eq")), "实际：{e}");
        let e = parse_where(&["nope:eq:1".into()], &cols()).unwrap_err();
        assert!(matches!(&e, ZpmError::BadRequest(m) if m.starts_with("unknown field: nope")), "实际：{e}");
    }

    /// like 值不做强转（TS 豁免：原始字符串直接进 %..%）
    #[test]
    fn like_value_not_coerced() {
        let out = parse_where(&["name:like:042".into()], &cols()).unwrap();
        assert_eq!(out[0].values, vec![Value::Text("042".into())], "like 的 042 不得转成 42");
    }

    /// coerce：严格 TS 正则形态（.5 / 4. / 1e3 / +42 均为 Text；大整数溢出 i64 落 Real）
    #[test]
    fn coerce_value_strict_ts_number_shape() {
        assert_eq!(coerce_value("42"), Value::Int(42));
        assert_eq!(coerce_value("-3"), Value::Int(-3));
        assert_eq!(coerce_value("4.5"), Value::Real(4.5));
        assert_eq!(coerce_value("x"), Value::Text("x".into()));
        assert_eq!(coerce_value("+42"), Value::Text("+42".into()));
        assert_eq!(coerce_value("1e3"), Value::Text("1e3".into()));
        assert_eq!(coerce_value("4."), Value::Text("4.".into()));
        assert_eq!(coerce_value(".5"), Value::Text(".5".into()));
        assert_eq!(coerce_value("99999999999999999999"), Value::Real(1e20), "整数溢出 i64 落 Real 对齐 TS number");
    }

    #[test]
    fn parse_page_defaults_and_cap() {
        let p = parse_page(None, None);
        assert_eq!((p.page, p.page_size), (1, 20));
        let p = parse_page(Some("3"), Some("2000"));
        assert_eq!((p.page, p.page_size), (3, 1000));
        let p = parse_page(Some("abc"), Some("0"));
        assert_eq!((p.page, p.page_size), (1, 20));
    }

    #[test]
    fn parse_sort_desc_prefix() {
        let out = parse_sort("-a,b", &["a".into(), "b".into()]).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].desc);
        assert!(!out[1].desc);
        assert_eq!(out[0].field, "a");
        let e = parse_sort("-a,zz", &["a".into()]).unwrap_err();
        assert!(matches!(&e, ZpmError::BadRequest(m) if m.starts_with("unknown field: zz")), "实际：{e}");
    }
}
