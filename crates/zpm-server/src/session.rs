//! simulated 演示写会话覆盖层：目标库只读，写入落内存会话（TS session.ts 契约）。
//!
//! - 按 `zpm_sid` 会话隔离；进程重启即全部消失
//! - list：库内全量（where 仍作用于库内行）→ 过滤 deletes → 合并 updates →
//!   尾部追加会话 inserts（**不参与 where 过滤**）→ 仅 `sort[0]` 内存排序 → 内存分页；
//!   `total` = 合并后总行数（与 real 模式"where 命中数"语义不同）
//! - get：会话 inserts 命中优先 → deletes 命中返回 None → 库内行合并 updates
//! - update：会话 insert 直接合并；库内行按 id 累积合并 updates；不存在 → None
//! - delete：会话 insert 真移除；库内存在 → 记 deletes 并清该行 updates
//! - 纯读不铸造会话条目（防内存泄漏）：读方法对无写入历史的 sid 视为空覆盖

use zpm_core::error::ZpmError;
use zpm_core::store::{Filter, Page, Row, Sort, Store, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

/// 单会话覆盖层（按表组织）
#[derive(Default)]
pub struct SessionState {
    /// 表名 → 会话插入行（尾部追加；不过滤 where）
    inserts: HashMap<String, Vec<Row>>,
    /// 表名 → id → 累积合并的更新
    updates: HashMap<String, HashMap<String, Row>>,
    /// 表名 → 已删除的库内行 id 集合
    deletes: HashMap<String, HashSet<String>>,
}

/// 全部会话（key=sid）
#[derive(Default)]
pub struct SessionLayer {
    sessions: Mutex<HashMap<String, SessionState>>,
}

impl SessionLayer {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, SessionState>> {
        self.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 清除该 sid 的整个覆盖层（POST /api/_session/reset）
    pub fn reset(&self, sid: &str) {
        self.lock().remove(sid);
    }

    /// 该 sid 是否已有覆盖状态（测试/诊断用）
    pub fn has_state(&self, sid: &str) -> bool {
        self.lock().contains_key(sid)
    }
}

/// 覆盖层视图：base 为库句柄；写方法（insert/update/delete）首次访问时铸造会话条目，
/// 读方法不铸造（纯读零痕迹）。
pub struct SessionView<'a> {
    base: &'a mut dyn Store,
    layer: &'a SessionLayer,
    sid: String,
    table: String,
    pk: String,
}

impl<'a> SessionView<'a> {
    /// 读视图（不铸造会话条目）
    pub fn read(
        base: &'a mut dyn Store,
        layer: &'a SessionLayer,
        sid: &str,
        table: &str,
        pk: &str,
    ) -> Self {
        Self {
            base,
            layer,
            sid: sid.to_string(),
            table: table.to_string(),
            pk: pk.to_string(),
        }
    }

    /// 写视图（首次写操作铸造会话条目）
    pub fn write(
        base: &'a mut dyn Store,
        layer: &'a SessionLayer,
        sid: &str,
        table: &str,
        pk: &str,
    ) -> Self {
        Self {
            base,
            layer,
            sid: sid.to_string(),
            table: table.to_string(),
            pk: pk.to_string(),
        }
    }

    fn with_state<R>(&self, f: impl FnOnce(&SessionState) -> R) -> R {
        let g = self.layer.lock();
        let empty = SessionState::default();
        f(g.get(&self.sid).unwrap_or(&empty))
    }

    /// 列表：库内全量（where 作用）→ 过滤 deletes → 合并 updates → 追加 inserts →
    /// 仅 sort[0] 排序 → 内存分页；total = 合并后总数
    pub fn list(
        &mut self,
        filters: &[Filter],
        sort: &[Sort],
        page: Page,
    ) -> Result<(Vec<Row>, i64), ZpmError> {
        let full = Page { page: 1, page_size: u64::MAX };
        let (mut rows, _) = self.base.select(&self.table, filters, &[], full)?;

        let dels: HashSet<String> = self.with_state(|s| {
            s.deletes.get(&self.table).map(|d| d.iter().cloned().collect()).unwrap_or_default()
        });
        rows.retain(|r| !dels.contains(&skey(r.get(&self.pk))));

        let ups: HashMap<String, Row> = self.with_state(|s| {
            s.updates.get(&self.table).cloned().unwrap_or_default()
        });
        for r in rows.iter_mut() {
            let id = skey(r.get(&self.pk));
            if let Some(u) = ups.get(&id) {
                for (k, v) in u {
                    r.insert(k.clone(), v.clone());
                }
            }
        }
        let inserts: Vec<Row> = self.with_state(|s| {
            s.inserts.get(&self.table).cloned().unwrap_or_default()
        });
        rows.extend(inserts);

        if let Some(s) = sort.first() {
            rows.sort_by(|a, b| cmp_values(a.get(&s.field), b.get(&s.field)));
            if s.desc {
                rows.reverse();
            }
        }

        let total = rows.len() as i64;
        let start = (page.page.saturating_sub(1)).saturating_mul(page.page_size) as usize;
        let data: Vec<Row> =
            rows.into_iter().skip(start).take(page.page_size as usize).collect();
        Ok((data, total))
    }

    /// 单条：inserts 优先 → deletes → 库内行合并 updates
    pub fn get(&mut self, id: &str) -> Result<Option<Row>, ZpmError> {
        let ins: Vec<Row> = self.with_state(|s| {
            s.inserts.get(&self.table).cloned().unwrap_or_default()
        });
        if let Some(r) = ins.iter().find(|r| skey(r.get(&self.pk)) == id) {
            return Ok(Some(r.clone()));
        }
        let deleted = self.with_state(|s| {
            s.deletes.get(&self.table).is_some_and(|d| d.contains(id))
        });
        if deleted {
            return Ok(None);
        }
        match self.base.get(&self.table, id)? {
            Some(mut r) => {
                let u = self.with_state(|s| {
                    s.updates.get(&self.table).and_then(|m| m.get(id)).cloned()
                });
                if let Some(u) = u {
                    for (k, v) in &u {
                        r.insert(k.clone(), v.clone());
                    }
                }
                Ok(Some(r))
            }
            None => Ok(None),
        }
    }

    /// 插入：body 必须携带主键值（缺失 400）；201 回显入参
    pub fn insert(&mut self, row: &Row) -> Result<Row, ZpmError> {
        let missing = matches!(row.get(&self.pk), None | Some(Value::Null));
        if missing {
            return Err(ZpmError::BadRequest(format!(
                "missing primary key value: {}",
                self.pk
            )));
        }
        let mut g = self.layer.lock();
        let ins = g
            .entry(self.sid.clone())
            .or_default()
            .inserts
            .entry(self.table.clone())
            .or_default();
        ins.push(row.clone());
        Ok(row.clone())
    }

    /// 更新：会话 insert 直接合并；库内行累积 updates；不存在 → None
    pub fn update(&mut self, id: &str, patch: &Row) -> Result<Option<Row>, ZpmError> {
        {
            let mut g = self.layer.lock();
            let ins = g
                .entry(self.sid.clone())
                .or_default()
                .inserts
                .entry(self.table.clone())
                .or_default();
            if let Some(r) = ins.iter_mut().find(|r| skey(r.get(&self.pk)) == id) {
                for (k, v) in patch {
                    r.insert(k.clone(), v.clone());
                }
                return Ok(Some(r.clone()));
            }
        }
        // 已删除的行 → None（TS session.ts:105）；防止 delete 后 PATCH 复活脏 updates
        let deleted = self
            .with_state(|s| s.deletes.get(&self.table).is_some_and(|d| d.contains(id)));
        if deleted {
            return Ok(None);
        }
        let Some(mut current) = self.base.get(&self.table, id)? else {
            return Ok(None);
        };
        let mut g = self.layer.lock();
        let u = g
            .entry(self.sid.clone())
            .or_default()
            .updates
            .entry(self.table.clone())
            .or_default()
            .entry(id.to_string())
            .or_default();
        for (k, v) in patch {
            u.insert(k.clone(), v.clone());
        }
        for (k, v) in u {
            current.insert(k.clone(), v.clone());
        }
        Ok(Some(current))
    }

    /// 删除：会话 insert 真移除；库内存在 → 记 deletes 并清该行 updates；不存在 → false
    pub fn delete(&mut self, id: &str) -> Result<bool, ZpmError> {
        {
            let mut g = self.layer.lock();
            let ins = g
                .entry(self.sid.clone())
                .or_default()
                .inserts
                .entry(self.table.clone())
                .or_default();
            let before = ins.len();
            ins.retain(|r| skey(r.get(&self.pk)) != id);
            if ins.len() < before {
                return Ok(true);
            }
        }
        if self.base.get(&self.table, id)?.is_some() {
            let mut g = self.layer.lock();
            let st = g.entry(self.sid.clone()).or_default();
            st.deletes
                .entry(self.table.clone())
                .or_default()
                .insert(id.to_string());
            if let Some(u) = st.updates.get_mut(&self.table) {
                u.remove(id);
            }
            return Ok(true);
        }
        Ok(false)
    }
}

/// 主键值的字符串化（id 键控统一口径，对齐 TS String(id)）
fn skey(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::Int(i)) => i.to_string(),
        Some(Value::Real(f)) => f.to_string(),
        Some(Value::Text(s)) => s.clone(),
        Some(Value::Blob(b)) => hex::encode(b),
    }
}

/// sort[0] 内存排序比较（混合类型：数值互通、文本字典序、Null 最小）
fn cmp_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Int(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
        }
    }
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(Value::Int(x)), Some(Value::Int(y))) => x.cmp(y),
        (Some(Value::Text(x)), Some(Value::Text(y))) => x.cmp(y),
        (Some(Value::Blob(x)), Some(Value::Blob(y))) => x.cmp(y),
        (Some(x), Some(y)) => {
            let xf = match x {
                Value::Int(i) => *i as f64,
                Value::Real(f) => *f,
                _ => return rank(x).cmp(&rank(y)),
            };
            let yf = match y {
                Value::Int(i) => *i as f64,
                Value::Real(f) => *f,
                _ => return rank(x).cmp(&rank(y)),
            };
            xf.partial_cmp(&yf).unwrap_or(Ordering::Equal)
        }
    }
}
