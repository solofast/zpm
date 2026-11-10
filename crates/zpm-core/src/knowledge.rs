//! 知识库同步：项目内 knowledge/ ↔ framework 知识基线的双向回路。
//!
//! - `pull`：基线 → 项目。判定规则：manifest 记录的哈希 == 项目当前文件哈希 →
//!   项目侧未改动 → 基线覆盖；哈希不匹配（或无条目）→ 项目侧改过 → skip；
//!   基线新增 → 拷入；基线删除且项目侧未改 → 删除项目文件。
//! - `push`：项目 → 基线。非交互（无 --all 无路径）直接报错；push 只落文件，
//!   基线侧不维护 manifest（pull 判定只依赖项目侧 manifest）。
//!
//! manifest：`.zpm/knowledge-manifest.json`，格式
//! `{"files":{"<项目根相对路径>":"<sha256>"},"base":{...}}`（路径含 knowledge/ 前缀；
//! files 记项目侧哈希，base 记上次同步时基线侧哈希；旧格式无 base 键视为未知）。

use crate::error::ZpmError;
use crate::project_config::ZPM_DIR;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::{DirEntry, WalkDir};

/// 知识库清单文件路径（相对项目根）
pub const KNOWLEDGE_MANIFEST: &str = ".zpm/knowledge-manifest.json";

/// 同步时跳过的目录名
const SKIP_DIRS: &[&str] = &["node_modules", ".git", "target", ".zpm"];

/// 项目知识目录名
const KNOWLEDGE_DIR: &str = "knowledge";

/// 知识库清单
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct KnowledgeManifest {
    /// 项目根相对路径（如 "knowledge/rules/a.md"）→ 项目侧 sha256
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// 上次同步时基线侧内容 sha256（旧 manifest 无此键 → 视为未知）
    #[serde(default)]
    pub base: BTreeMap<String, String>,
}

impl KnowledgeManifest {
    /// 双表登记：同步成功后两侧内容一致，proj/base 同哈希
    fn record(&mut self, full: &str, hash: String) {
        self.files.insert(full.to_string(), hash.clone());
        self.base.insert(full.to_string(), hash);
    }

    /// 双表移除
    fn unrecord(&mut self, full: &str) {
        self.files.remove(full);
        self.base.remove(full);
    }
}

/// 一次 pull/push 的结果报告
#[derive(Debug, Default, Clone)]
pub struct PullReport {
    /// 基线更新且项目侧未改动，已覆盖
    pub updated: Vec<String>,
    /// 基线新增，已拷入
    pub added: Vec<String>,
    /// 项目侧已改动，跳过未覆盖
    pub skipped: Vec<String>,
    /// 基线删除且项目侧未改动，已删除
    pub deleted: Vec<String>,
    /// 两边都改且内容不同：不写入，需人工选择性合并
    pub conflicts: Vec<String>,
    /// 无 base 记录（旧版清单）下覆盖基线的文件：无法检测冲突
    pub compat_pushed: Vec<String>,
}

impl PullReport {
    /// 是否无任何变化
    pub fn is_empty(&self) -> bool {
        self.updated.is_empty() && self.added.is_empty() && self.skipped.is_empty() && self.deleted.is_empty() && self.conflicts.is_empty() && self.compat_pushed.is_empty()
    }
}

/// 清单文件路径：<项目根>/.zpm/knowledge-manifest.json
pub fn manifest_path(project_root: &Path) -> PathBuf {
    project_root.join(ZPM_DIR).join("knowledge-manifest.json")
}

/// 加载清单；不存在时返回空的默认值
pub fn load_knowledge_manifest(project_root: &Path) -> Result<KnowledgeManifest, ZpmError> {
    let p = manifest_path(project_root);
    if !p.exists() {
        return Ok(KnowledgeManifest::default());
    }
    let txt = std::fs::read_to_string(&p)?;
    Ok(serde_json::from_str(&txt)?)
}

/// 保存清单（自动创建目录）
fn save_manifest(project_root: &Path, m: &KnowledgeManifest) -> Result<(), ZpmError> {
    let p = manifest_path(project_root);
    std::fs::create_dir_all(p.parent().expect("manifest 路径必有父目录"))?;
    std::fs::write(p, serde_json::to_string_pretty(m)?)?;
    Ok(())
}

/// 由项目现有 knowledge/ 目录重建清单（init 整树拷入框架基线后调用）。
/// 返回清单条目数（即 knowledge/ 下文件数）。
pub fn seed_manifest(project_root: &Path) -> Result<usize, ZpmError> {
    let dir = project_root.join(KNOWLEDGE_DIR);
    let mut files = BTreeMap::new();
    let mut base = BTreeMap::new();
    for rel in collect_files(&dir)? {
        let full = format!("{KNOWLEDGE_DIR}/{rel}");
        let hash = hash_file(&dir.join(rel))?;
        files.insert(full.clone(), hash.clone());
        base.insert(full, hash);
    }
    let n = files.len();
    save_manifest(project_root, &KnowledgeManifest { files, base })?;
    Ok(n)
}

/// 计算单个文件的 SHA-256（hex 编码）
pub fn hash_file(p: &Path) -> Result<String, ZpmError> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// 是否跳过该目录（仅目录命中 SKIP_DIRS 时跳过）
fn is_skipped(e: &DirEntry) -> bool {
    e.file_type().is_dir() && e.file_name().to_str().is_some_and(|n| SKIP_DIRS.contains(&n))
}

/// 遍历目录，返回排序后的相对文件路径列表
fn collect_files(root: &Path) -> Result<Vec<String>, ZpmError> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    for entry in WalkDir::new(root).into_iter().filter_entry(|e| !is_skipped(e)) {
        let e = entry.map_err(|err| ZpmError::Other(format!("遍历目录失败：{err}")))?;
        if !e.file_type().is_file() {
            continue;
        }
        let rel = e
            .path()
            .strip_prefix(root)
            .map_err(|err| ZpmError::Other(format!("相对路径解析失败：{err}")))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        out.push(rel.to_string_lossy().into_owned());
    }
    out.sort();
    Ok(out)
}

/// 类别归组：路径剥掉 knowledge/ 前缀后的首段目录名；顶层散文件归 other。
/// 供 `zpm status` 统计用（沿用原 asset.rs categorize 语义）。
pub fn categorize(rel: &str) -> &str {
    let rest = rel.strip_prefix(&format!("{KNOWLEDGE_DIR}/")).unwrap_or(rel);
    match rest.split_once('/') {
        Some((first, _)) if !first.is_empty() => first,
        _ => "other",
    }
}

/// 基线 → 项目：按 pull 四条规则同步并更新项目侧 manifest。
pub fn pull(baseline: &Path, project_root: &Path, scope: Option<&str>, dry_run: bool) -> Result<PullReport, ZpmError> {
    let base_knowledge = baseline.join(KNOWLEDGE_DIR);
    let proj_knowledge = project_root.join(KNOWLEDGE_DIR);
    let mut manifest = load_knowledge_manifest(project_root)?;
    let mut report = PullReport::default();

    let base_files: Vec<String> = collect_files(&base_knowledge)?
        .into_iter()
        .filter(|rel| scope.map_or(true, |s| rel.starts_with(s)))
        .collect();
    for rel in &base_files {
        let full = format!("{KNOWLEDGE_DIR}/{rel}");
        let b_hash = hash_file(&base_knowledge.join(rel))?;
        let p_path = proj_knowledge.join(rel);
        let m_hash = manifest.files.get(&full).cloned();
        let p_hash = if p_path.is_file() { Some(hash_file(&p_path)?) } else { None };

        match (p_hash.as_deref(), m_hash.as_deref()) {
            // 项目侧不存在 → 新增
            (None, _) => {
                if !dry_run {
                    copy_into(&base_knowledge.join(rel), &p_path)?;
                    manifest.record(&full, b_hash);
                }
                report.added.push(full);
            }
            // 项目侧存在且与 manifest 一致（未改动）→ 基线不同则覆盖，相同则补记 base（升级清单）
            (Some(ph), Some(mh)) if ph == mh => {
                if b_hash != ph {
                    if !dry_run {
                        copy_into(&base_knowledge.join(rel), &p_path)?;
                        manifest.record(&full, b_hash);
                    }
                    report.updated.push(full);
                } else if !dry_run {
                    manifest.record(&full, b_hash);
                }
            }
            // 项目侧改过（无条目或哈希不匹配）→ 内容与基线一致则补登记，否则 skip
            (Some(ph), _) => {
                if ph == b_hash {
                    if !dry_run {
                        manifest.record(&full, b_hash);
                    }
                } else {
                    report.skipped.push(full);
                }
            }
        }
    }

    // 基线删除的文件：项目侧未改 → 删除；已改 → 保留
    let base_set: std::collections::HashSet<&String> = base_files.iter().collect();
    for (full, h) in manifest.files.clone() {
        let Some(rel) = full.strip_prefix(&format!("{KNOWLEDGE_DIR}/")) else { continue };
        if scope.is_some_and(|s| !rel.starts_with(s)) {
            continue;
        }
        if base_set.contains(&rel.to_string()) {
            continue;
        }
        let p_path = proj_knowledge.join(rel);
        if !p_path.exists() {
            // 文件已不在项目里：静默清掉陈旧条目
            if !dry_run {
                manifest.unrecord(&full);
            }
            continue;
        }
        if hash_file(&p_path)? == h {
            if !dry_run {
                std::fs::remove_file(&p_path)?;
                if let Some(parent) = p_path.parent() {
                    prune_empty_parents(parent, &proj_knowledge);
                }
                manifest.unrecord(&full);
            }
            report.deleted.push(full);
        }
        // 已改动 → 保留文件与条目（项目本地演化）
    }

    // 项目本地文件（基线没有、manifest 也无条目）→ 报告 skipped：
    // 让用户知道这些文件未被本次同步触及（简报契约：b.md 应出现在 skipped）
    for rel in collect_files(&proj_knowledge)?
        .into_iter()
        .filter(|rel| scope.map_or(true, |s| rel.starts_with(s)))
    {
        let full = format!("{KNOWLEDGE_DIR}/{rel}");
        if !base_set.contains(&rel) && !manifest.files.contains_key(&full) {
            report.skipped.push(full);
        }
    }

    if !dry_run {
        save_manifest(project_root, &manifest)?;
    }
    Ok(report)
}

/// 项目 → 基线：把项目知识文件写入基线对应位置。
/// 非交互（无 --all、无显式路径且无 scope）→ BadRequest；带 scope 时默认推送整个子树。
/// paths 为项目根相对路径。
pub fn push(
    project_root: &Path,
    baseline: &Path,
    paths: &[String],
    all: bool,
    scope: Option<&str>,
    dry_run: bool,
) -> Result<PullReport, ZpmError> {
    if !all && paths.is_empty() && scope.is_none() {
        return Err(ZpmError::BadRequest(
            "非交互模式下需要 --all 或显式路径（如 knowledge/rules/b.md）".into(),
        ));
    }
    let proj_knowledge = project_root.join(KNOWLEDGE_DIR);
    let base_knowledge = baseline.join(KNOWLEDGE_DIR);
    let mut report = PullReport::default();
    let mut manifest = load_knowledge_manifest(project_root)?;
    let mut touched = false;

    let rels: Vec<String> = if all || (paths.is_empty() && scope.is_some()) {
        collect_files(&proj_knowledge)?
            .into_iter()
            .filter(|rel| scope.map_or(true, |s| rel.starts_with(s)))
            .collect()
    } else {
        let stripped: Vec<String> = paths
            .iter()
            .map(|p| p.strip_prefix(&format!("{KNOWLEDGE_DIR}/")).unwrap_or(p).to_string())
            .collect();
        for rel in &stripped {
            if let Some(s) = scope {
                let escapes = Path::new(rel)
                    .components()
                    .any(|c| c == std::path::Component::ParentDir);
                if escapes || !rel.starts_with(s) {
                    return Err(ZpmError::BadRequest(format!(
                        "路径超出同步范围（{}）：knowledge/{rel}",
                        s
                    )));
                }
            }
        }
        stripped
    };

    for rel in &rels {
        let src = proj_knowledge.join(rel);
        if !src.is_file() {
            return Err(ZpmError::NotFound(format!("项目知识文件不存在：{KNOWLEDGE_DIR}/{rel}")));
        }
        let dest = base_knowledge.join(rel);
        let full = format!("{KNOWLEDGE_DIR}/{rel}");
        let s_hash = hash_file(&src)?;
        let d_hash = if dest.is_file() { Some(hash_file(&dest)?) } else { None };
        if d_hash.as_deref() == Some(s_hash.as_str()) {
            if !dry_run && !manifest.base.contains_key(&full) {
                // 幂等命中：旧 manifest 从未流动的文件在此补记 base，一次 push 完成升级
                manifest.record(&full, s_hash);
                touched = true;
            }
            continue; // 幂等：内容一致不推
        }
        // 三方判定：theirs(基线现值) 相对 base(上次同步基线) 变了吗？项目相对上次同步变了吗？
        if let Some(dh) = d_hash.as_deref() {
            if let Some(bh) = manifest.base.get(&full).map(String::as_str) {
                if dh != bh {
                    if manifest.files.get(&full).map(String::as_str) == Some(s_hash.as_str()) {
                        report.skipped.push(full); // 项目未变、基线更新 → 等 pull
                        continue;
                    }
                    report.conflicts.push(full); // 两边都改 → 冲突，不写入
                    continue;
                }
            }
            // base 未知（旧 manifest）→ 兼容现状按干净推送
        }
        let base_unknown = d_hash.is_some() && !manifest.base.contains_key(&full);
        if !dry_run {
            if base_unknown {
                // 旧版清单无基线记录：无法检测冲突，上报供 CLI 警告
                report.compat_pushed.push(full.clone());
            }
            copy_into(&src, &dest)?;
            manifest.record(&full, s_hash);
            touched = true;
        }
        if d_hash.is_some() {
            report.updated.push(full);
        } else {
            report.added.push(full);
        }
    }
    if touched {
        save_manifest(project_root, &manifest)?;
    }
    Ok(report)
}

/// 复制文件并创建父目录
fn copy_into(src: &Path, dest: &Path) -> Result<(), ZpmError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dest)?;
    Ok(())
}

/// 删除文件后，尽力清理因此变空的父目录（不越过 knowledge 根，忽略失败）
fn prune_empty_parents(mut cur: &Path, stop: &Path) {
    while cur.starts_with(stop) && cur != stop {
        if cur.is_dir() {
            let _ = std::fs::remove_dir(cur); // 非空时会失败，忽略
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已知 sha256 向量：防 hash_file 与测试自证循环（终审 A/T6②）
    #[test]
    fn hash_file_known_sha256() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("abc.txt");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            hash_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
