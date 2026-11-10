//! 框架（framework）：manifest 解析、目录解析（resolve）与渲染拷贝（copy_tree）。
//!
//! 行为契约对齐 TS 版 src/framework/{manifest,resolve,copy}.ts：
//! - manifest：framework.yaml 四字段（name/version/description/variables），中文错误；
//! - resolve_in：指定框架根目录内双路解析（名字形态优先，其次本地路径，
//!   含 framework.yaml 才算命中，禁 `..` 路径组件）；
//! - copy_tree：非二进制文本做 `{{var}}` 渲染（未知占位保留原样并记 warnings），`.tmpl`
//!   渲染后去后缀；二进制与 >2MB 文件原样直拷；中断时只删本次新增的 dest 顶层条目。
//!
//! 变量优先级 CLI > manifest.variables > DEFAULT_VARS 由调用方合成，本模块只做纯渲染。

use crate::error::ZpmError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 框架清单文件名
pub const MANIFEST_FILE: &str = "framework.yaml";
/// 框架索引文件名（位于 frameworks 根目录，规格 §4.3）
pub const INDEX_FILE: &str = "index.yaml";

/// 变量默认值（优先级：CLI > manifest.variables > 本常量，由调用方合成）
pub const DEFAULT_VARS: &[(&str, &str)] = &[
    ("port", "4310"),
    ("host", "localhost"),
    ("dbport", "6543"),
    ("database", "rain-source"),
    ("user", "postgres"),
    ("password", "abc123"),
];

/// 模板后缀：渲染后从目标文件名去掉
const TMPL_SUFFIX: &str = ".tmpl";
/// 超过此大小的文件按原样拷贝、不做替换（避免在大体积产物里做无谓扫描）
const TEXT_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// 二进制扩展名（不带点，比较时忽略大小写）：这些文件不做 `{{var}}` 渲染
const BINARY_EXTS: &[&str] = &[
    "woff", "woff2", "ttf", "eot", "otf", "png", "jpg", "jpeg", "gif", "webp", "ico", "zip", "db",
];

/// framework.yaml 解析结果
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FrameworkManifest {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
}

/// 加载并校验 `<dir>/framework.yaml`：文件缺失 / 字段类型与空值校验均为中文错误。
pub fn load_manifest(dir: &Path) -> Result<FrameworkManifest, ZpmError> {
    let file = dir.join(MANIFEST_FILE);
    let txt = std::fs::read_to_string(&file)
        .map_err(|_| ZpmError::NotFound(format!("framework.yaml 不存在: {}", file.display())))?;
    let v: serde_yaml::Value = serde_yaml::from_str(&txt)
        .map_err(|e| ZpmError::Format(format!("framework.yaml 语法错误：{e}")))?;
    let m: FrameworkManifest = serde_yaml::from_value(v).map_err(|e| {
        ZpmError::Format(format!("framework.yaml 字段非法 ({}): {e}", file.display()))
    })?;
    if m.name.trim().is_empty() || m.version.trim().is_empty() {
        return Err(ZpmError::Format(format!(
            "framework.yaml 字段非法 ({}): name/version 不能为空",
            file.display()
        )));
    }
    Ok(m)
}

/// 列出框架根目录下的可用框架：含 framework.yaml 的子目录名，排序。
/// 根目录不存在 / 不可读时视为空。
fn list_available(home: &Path) -> Vec<String> {
    let mut out: Vec<String> = match std::fs::read_dir(home) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(MANIFEST_FILE).is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// 指定根目录版助手（repo 多仓结构用）：resolve_in / validate_name /
// rebuild_index / dir_hash。旧家目录入口（resolve / frameworks_home /
// ENV_FRAMEWORKS_HOME）已删除，全部调用方统一走 resolve_in。
// ---------------------------------------------------------------------------

/// 框架名安全校验（自 cmd_framework.rs::validate_framework_name 平移）：
/// 非空、不含 `..`、不含 `/` 与 `\`、首字符 ASCII 字母数字、其余仅 `[A-Za-z0-9._-]`。
/// 名字直接参与 `frameworks_dir.join()`，恶意名会造成路径穿越。
pub fn validate_name(name: &str) -> Result<(), ZpmError> {
    let ok = !name.is_empty()
        && !name.contains("..")
        && !name.contains('/')
        && !name.contains('\\')
        && name.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(ZpmError::Format(format!(
            "非法框架名：{name:?}（仅允许字母数字与 . _ -，不得含路径分隔符）"
        )))
    }
}

/// 在指定框架根目录内解析 reference（双路语义，对齐旧 resolve）：
/// - 安全红线：含 `..` 路径组件的一律拒绝（Format），无论名字形态还是路径形态；
/// - 名字形态：validate_name 通过且 `<frameworks_dir>/<ref>/framework.yaml` 存在 → 命中；
/// - 本地路径形态：reference 是存在的目录且含 framework.yaml → 命中（允许分隔符，`..` 已被上面排除）；
/// - 都未命中 → NotFound，错误信息带按名排序的可用内置清单。
pub fn resolve_in(frameworks_dir: &Path, reference: &str) -> Result<PathBuf, ZpmError> {
    if reference.split(['/', '\\']).any(|c| c == "..") {
        return Err(ZpmError::Format(format!("非法引用：{reference:?}（不得含 .. 路径组件）")));
    }
    if validate_name(reference).is_ok() {
        let in_dir = frameworks_dir.join(reference);
        if in_dir.join(MANIFEST_FILE).is_file() {
            return Ok(in_dir);
        }
    }
    let local = PathBuf::from(reference);
    if local.is_dir() && local.join(MANIFEST_FILE).is_file() {
        return Ok(local);
    }
    let list = list_available(frameworks_dir);
    Err(ZpmError::NotFound(format!(
        "framework 不存在: {reference}（可用内置: {}；也可传含 framework.yaml 的本地路径）",
        if list.is_empty() { "无".to_string() } else { list.join(", ") }
    )))
}

/// frameworks/index.yaml 文档结构（规格 §4.3：generated_at + 按 name 排序的 frameworks）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDoc {
    /// 索引生成时间（RFC3339 UTC）
    pub generated_at: String,
    /// 框架条目，按 name 排序
    pub frameworks: Vec<IndexEntry>,
}

/// 索引中的单个框架条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    /// 相对 frameworks/ 的目录名
    pub dir: String,
}

/// 重建 `<frameworks_dir>/index.yaml`：扫描含 framework.yaml 的子目录并加载 manifest，
/// 按 name 排序写出 generated_at + frameworks 数组。空目录也写出 `frameworks: []`。
/// 返回索引文件路径。
pub fn rebuild_index(frameworks_dir: &Path) -> Result<PathBuf, ZpmError> {
    let mut entries: Vec<IndexEntry> = match std::fs::read_dir(frameworks_dir) {
        Ok(rd) => {
            let mut v = Vec::new();
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if !p.join(MANIFEST_FILE).is_file() {
                    continue;
                }
                let m = load_manifest(&p)?;
                let dir = e.file_name().into_string().map_err(|_| {
                    ZpmError::Format(format!("framework 目录名非 UTF-8: {}", p.display()))
                })?;
                v.push(IndexEntry { name: m.name, version: m.version, description: m.description, dir });
            }
            v
        }
        Err(_) => Vec::new(),
    };
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let doc = IndexDoc { generated_at: zpm_meta::rfc3339::rfc3339_now(), frameworks: entries };
    let path = frameworks_dir.join(INDEX_FILE);
    let yaml = serde_yaml::to_string(&doc)
        .map_err(|e| ZpmError::Format(format!("index.yaml 生成失败：{e}")))?;
    std::fs::write(&path, yaml)?;
    Ok(path)
}

/// 目录内容哈希：收集 dir 下全部文件的相对路径（按字节排序），逐个复用
/// [`crate::knowledge::hash_file`] 取文件 sha256，再把「相对路径 + 文件哈希」
/// 拼接为一个 sha256 hex。内容不变 → 哈希稳定；任一文件变化 → 哈希不同。
pub fn dir_hash(dir: &Path) -> Result<String, ZpmError> {
    // 遍历错误（如不可读子树）必须传播：静默跳过会让哈希误判「未变更」
    let entries: Vec<walkdir::DirEntry> = WalkDir::new(dir)
        .into_iter()
        .map(|e| e.map_err(|err| ZpmError::Other(format!("遍历目录失败：{err}"))))
        .collect::<Result<Vec<_>, _>>()?;
    let mut rels: Vec<String> = entries
        .iter()
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            e.path()
                .strip_prefix(dir)
                .expect("walkdir 子路径必有前缀")
                .to_string_lossy()
                .to_string()
        })
        .collect();
    rels.sort();
    use std::io::Write as _;
    let mut h = Sha256::new();
    for rel in &rels {
        let fh = crate::knowledge::hash_file(&dir.join(rel))?;
        h.write_all(rel.as_bytes())?;
        h.write_all(b"\0")?;
        h.write_all(fh.as_bytes())?;
        h.write_all(b"\n")?;
    }
    Ok(hex::encode(h.finalize()))
}

/// copy_tree 结果统计
#[derive(Debug, Default, Clone)]
pub struct CopyStats {
    /// 已写入的目标文件数
    pub files: usize,
    /// 未知 `{{占位}}` 等警告
    pub warnings: Vec<String>,
}

/// 渲染拷贝 `<src>` 整棵树到 `<dest>`。
pub fn copy_tree(
    src: &Path,
    dest: &Path,
    vars: &BTreeMap<String, String>,
) -> Result<CopyStats, ZpmError> {
    copy_tree_impl(src, dest, vars, None)
}

/// 测试专用注入点：第 fail_at 个文件（0 起，按排序后写入顺序）写入前返回错误，
/// 模拟拷贝中断。不进入公共 API。
#[cfg(test)]
pub(crate) fn copy_tree_with_fail_at(
    src: &Path,
    dest: &Path,
    vars: &BTreeMap<String, String>,
    fail_at: usize,
) -> Result<CopyStats, ZpmError> {
    copy_tree_impl(src, dest, vars, Some(fail_at))
}

fn copy_tree_impl(
    src: &Path,
    dest: &Path,
    vars: &BTreeMap<String, String>,
    fail_at: Option<usize>,
) -> Result<CopyStats, ZpmError> {
    std::fs::create_dir_all(dest)?;
    // 快照拷贝前 dest 已有的顶层条目：失败清理时只删新增、不碰既有（保护用户文件）
    let pre_existing: BTreeSet<std::ffi::OsString> = match std::fs::read_dir(dest) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect(),
        Err(_) => BTreeSet::new(),
    };

    let run = || -> Result<CopyStats, ZpmError> {
        let mut stats = CopyStats::default();
        // sort_by_file_name：写入顺序确定，中断清理测试可复现
        for entry in WalkDir::new(src).sort_by_file_name() {
            let e = entry.map_err(|err| ZpmError::Other(format!("遍历框架目录失败：{err}")))?;
            if e.path() == src {
                continue; // 源目录本身
            }
            let rel = e.path().strip_prefix(src).expect("walkdir 子路径必有前缀");
            let mut target = dest.join(rel);
            if e.file_type().is_dir() {
                std::fs::create_dir_all(&target)?;
                continue;
            }
            // .tmpl 渲染后去后缀（扩展名判定用源文件：x.db.tmpl 仍是文本模板）
            if let Some(stem) = e
                .file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(TMPL_SUFFIX))
            {
                target = target.with_file_name(stem);
            }
            let rel_display = target
                .strip_prefix(dest)
                .unwrap_or(&target)
                .to_string_lossy()
                .to_string();
            let bytes = std::fs::read(e.path())?;
            // pages/**/*.html 是浏览器端 mustache 模板（数据字典等页的 {{name}} 为表名）：
            // init 不做 zpm 变量渲染，避免与页面运行时 {{...}} 撞车（如 {{name}} 被烘焙成项目名）
            let rel_str = rel.to_string_lossy().to_string();
            let skip_render = rel_str.starts_with("pages/") && rel_str.ends_with(".html");
            let content = if skip_render || is_binary(e.path(), bytes.len() as u64) {
                bytes
            } else {
                render_text(
                    &String::from_utf8_lossy(&bytes),
                    vars,
                    &rel_display,
                    &mut stats.warnings,
                )
                .into_bytes()
            };
            if fail_at == Some(stats.files) {
                return Err(ZpmError::Other("injected copy failure".into()));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, &content)?;
            stats.files += 1;
        }
        Ok(stats)
    };

    match run() {
        Ok(stats) => Ok(stats),
        Err(err) => {
            // 中断清理：逐项只删本次新增的 dest 顶层条目（含新增子目录整棵）。
            // 边界：与框架同名的既有文件可能已被覆盖，不恢复其内容（与 TS 版一致）；
            // 既有子目录内部的新文件失败后会残留（顶层快照语义，与 TS 一致）。
            if let Ok(rd) = std::fs::read_dir(dest) {
                for e in rd.filter_map(|e| e.ok()) {
                    if pre_existing.contains(&e.file_name()) {
                        continue;
                    }
                    let p = e.path();
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    let _ = if is_dir { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                }
            }
            Err(err)
        }
    }
}

/// 是否按二进制直拷：源扩展名命中 [`BINARY_EXTS`]（忽略大小写）或体积 > [`TEXT_MAX_BYTES`]。
fn is_binary(path: &Path, size: u64) -> bool {
    if size > TEXT_MAX_BYTES {
        return true;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => BINARY_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// 手写扫描替换 `{{\w+}}`（等价 JS 的 /\{\{(\w+)\}\}/g；\w 按 ASCII 解释，
/// 不引入 regex 依赖）。命中 vars → 替换；未命中 → 保留原样并记入 warnings（每处一条）。
fn render_text(
    content: &str,
    vars: &BTreeMap<String, String>,
    rel_display: &str,
    warnings: &mut Vec<String>,
) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    loop {
        let Some(pos) = rest.find("{{") else {
            out.push_str(rest);
            return out;
        };
        let after = &rest[pos + 2..];
        let key_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        let key = &after[..key_len];
        if !key.is_empty() && after[key_len..].starts_with("}}") {
            out.push_str(&rest[..pos]);
            match vars.get(key) {
                Some(v) => out.push_str(v),
                None => {
                    warnings.push(format!("{rel_display}: 未知占位 {{{{{key}}}}}，保留原样"));
                    out.push_str(&rest[pos..pos + 2 + key_len + 2]);
                }
            }
            rest = &after[key_len + 2..];
        } else {
            // 仅前进 1 字节再重试：与正则逐位扫描语义一致（如 "{{{name}}}" 只替换内层）
            out.push_str(&rest[..pos + 1]);
            rest = &rest[pos + 1..];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// pages/**/*.html 跳过变量渲染：浏览器端 mustache 的 {{name}} 不得被 zpm 变量烘焙；
    /// 非 pages 文件照常渲染
    #[test]
    fn pages_html_skips_var_rendering_but_tmpl_renders() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("fw");
        let dest = tmp.path().join("proj");
        fs::create_dir_all(src.join("pages")).unwrap();
        fs::write(src.join("pages/db-info.html"), "card {{name}} {{port}}").unwrap();
        fs::write(src.join("AGENTS.md"), "hello {{name}} on :{{port}}").unwrap();
        let stats = copy_tree(&src, &dest, &vars(&[("name", "demo2"), ("port", "4310")])).unwrap();
        assert_eq!(stats.files, 2);

        let page = fs::read_to_string(dest.join("pages/db-info.html")).unwrap();
        assert_eq!(page, "card {{name}} {{port}}", "pages html 必须原样保留（mustache 模板）");
        let agents = fs::read_to_string(dest.join("AGENTS.md")).unwrap();
        assert_eq!(agents, "hello demo2 on :4310");
    }

    // ① load_manifest ------------------------------------------------------

    /// 缺 framework.yaml → 中文错误含 "framework.yaml 不存在"
    #[test]
    fn load_manifest_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_manifest(dir.path()).unwrap_err();
        assert!(err.to_string().contains("framework.yaml 不存在"), "实际错误：{err}");
    }

    /// version 为空字符串 → 错误含 "字段非法"
    #[test]
    fn load_manifest_empty_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MANIFEST_FILE), "name: x\nversion: \"\"\n").unwrap();
        let err = load_manifest(dir.path()).unwrap_err();
        assert!(err.to_string().contains("字段非法"), "实际错误：{err}");
    }

    /// 全字段解析（对齐 basecoat-pm 样例形状）
    #[test]
    fn load_manifest_parses_fields() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(MANIFEST_FILE),
            "name: basecoat-pm\nversion: 0.1.0\ndescription: 工作台\nvariables:\n  theme: default\n",
        )
        .unwrap();
        let m = load_manifest(dir.path()).unwrap();
        assert_eq!(m.name, "basecoat-pm");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.description.as_deref(), Some("工作台"));
        assert_eq!(m.variables.get("theme").map(String::as_str), Some("default"));
    }

    // ② resolve_in -----------------------------------------------------------

    /// 根目录内没有该名字 → 回退本地路径（绝对路径与相对路径走同一分支）
    #[test]
    fn resolve_in_falls_back_to_local_path() {
        let tmp = tempfile::tempdir().unwrap();
        let fw = tmp.path().join("local-fw");
        fs::create_dir_all(&fw).unwrap();
        fs::write(fw.join(MANIFEST_FILE), "name: local-fw\nversion: 0.1.0\n").unwrap();
        let empty_dir = tempfile::tempdir().unwrap();
        let got = resolve_in(empty_dir.path(), fw.to_str().unwrap()).unwrap();
        assert_eq!(got, fw);
    }

    /// 都不在 → 错误含 "framework 不存在"、可用内置清单；无 framework.yaml 的目录不进清单
    #[test]
    fn resolve_in_errors_listing_available() {
        let home = tempfile::tempdir().unwrap();
        for n in ["alpha", "beta"] {
            let d = home.path().join(n);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(MANIFEST_FILE), format!("name: {n}\nversion: 0.1.0\n")).unwrap();
        }
        fs::create_dir_all(home.path().join("no-manifest")).unwrap();
        let err = resolve_in(home.path(), "basecoat-pm").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("framework 不存在"), "实际错误：{msg}");
        assert!(msg.contains("可用内置"), "实际错误：{msg}");
        assert!(msg.contains("alpha") && msg.contains("beta"), "实际错误：{msg}");
        assert!(!msg.contains("no-manifest"), "实际错误：{msg}");
    }

    // ③ copy_tree：渲染与直拷 ------------------------------------------------

    /// .tmpl 渲染后去后缀；{{name}}/{{port}} 被替换；未知占位 {{nope}} 原样保留并进 warnings
    #[test]
    fn copy_tree_renders_tmpl_and_replaces_vars() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        fs::write(
            src.path().join("zpm.config.yaml.tmpl"),
            "name: {{name}}\nport: {{port}}\nkeep: {{nope}}\n",
        )
        .unwrap();
        fs::write(src.path().join("readme.txt"), "hi {{name}}").unwrap();

        let stats = copy_tree(src.path(), dest.path(), &vars(&[("name", "demo"), ("port", "5000")]))
            .unwrap();

        let rendered = fs::read_to_string(dest.path().join("zpm.config.yaml")).unwrap();
        assert_eq!(rendered, "name: demo\nport: 5000\nkeep: {{nope}}\n");
        assert!(!dest.path().join("zpm.config.yaml.tmpl").exists());
        assert_eq!(fs::read_to_string(dest.path().join("readme.txt")).unwrap(), "hi demo");
        assert_eq!(stats.files, 2);
        assert_eq!(stats.warnings.len(), 1, "实际警告：{:?}", stats.warnings);
        assert!(stats.warnings[0].contains("未知占位 {{nope}}"), "实际警告：{}", stats.warnings[0]);
        assert!(stats.warnings[0].contains("zpm.config.yaml"), "实际警告：{}", stats.warnings[0]);
    }

    /// .png/.db 二进制与 >2MB 大文件原样直拷：字节逐一相等、不做占位扫描
    #[test]
    fn copy_tree_copies_binary_and_large_files_verbatim() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        // 含非法 UTF-8 字节与占位符：若被误当作文本渲染，字节必然改变
        let mut png = b"{{name}}\xff\xfebin".to_vec();
        png.extend(std::iter::repeat(0xABu8).take(64));
        let mut db = b"\x00\x01{{name}}\xff".to_vec();
        db.extend(std::iter::repeat(0xCDu8).take(64));
        let mut big = b"{{name}}\xff".to_vec();
        big.extend(std::iter::repeat(0x7Fu8).take(2 * 1024 * 1024)); // > 2MB
        fs::write(src.path().join("logo.png"), &png).unwrap();
        fs::write(src.path().join("data.db"), &db).unwrap();
        fs::write(src.path().join("dump.bin"), &big).unwrap();

        let stats = copy_tree(src.path(), dest.path(), &vars(&[("name", "demo")])).unwrap();

        assert_eq!(fs::read(dest.path().join("logo.png")).unwrap(), png);
        assert_eq!(fs::read(dest.path().join("data.db")).unwrap(), db);
        assert_eq!(fs::read(dest.path().join("dump.bin")).unwrap(), big);
        assert_eq!(stats.files, 3);
        assert!(stats.warnings.is_empty(), "实际警告：{:?}", stats.warnings);
    }

    // ④ copy_tree：中断清理 --------------------------------------------------

    /// 第 2 个文件（0 起 fail_at=1）失败：本次新增条目被删（含新增子目录整棵）、
    /// 既有条目保留；与框架同名且已被覆盖的既有文件按既定限制不恢复内容。
    #[test]
    fn copy_tree_failure_removes_new_entries_keeps_existing() {
        let src = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        fs::write(src.path().join("a.txt"), "frame a {{name}}").unwrap();
        fs::create_dir_all(src.path().join("sub")).unwrap();
        fs::write(src.path().join("sub").join("b.txt"), "frame b").unwrap();
        // dest 既有条目：用户文件 keep.txt、与框架同名会被覆盖的 a.txt
        fs::write(dest.path().join("keep.txt"), "user data").unwrap();
        fs::write(dest.path().join("a.txt"), "user original").unwrap();

        let err = copy_tree_with_fail_at(
            src.path(),
            dest.path(),
            &vars(&[("name", "demo")]),
            1, // a.txt(0) 写入后，sub/b.txt(1) 写入前失败
        )
        .unwrap_err();
        assert!(err.to_string().contains("injected"), "实际错误：{err}");

        // 既有条目保留
        assert_eq!(fs::read_to_string(dest.path().join("keep.txt")).unwrap(), "user data");
        // 已被覆盖的既有同名文件：不恢复内容（文件本身仍在）
        assert_eq!(fs::read_to_string(dest.path().join("a.txt")).unwrap(), "frame a demo");
        // 本次新增条目（新增子目录整棵）被删除
        assert!(!dest.path().join("sub").exists());
        assert!(!dest.path().join("sub").join("b.txt").exists());
    }

    /// render_text 边界：与 JS 正则 /\{\{(\w+)\}\}/g 逐位扫描语义一致
    #[test]
    fn render_text_matches_js_regex_semantics() {
        let mut w = Vec::new();
        let v = vars(&[("name", "demo")]);
        // 内层替换：{{{name}}} → {demo}
        assert_eq!(render_text("{{{name}}}", &v, "f", &mut w), "{demo}");
        // 空 key / 非词字符不成占位 → 原样
        assert_eq!(render_text("{{}}", &v, "f", &mut w), "{{}}");
        assert_eq!(render_text("{{na me}}", &v, "f", &mut w), "{{na me}}");
        // 多处替换与多处未知占位各记一条警告
        w.clear();
        assert_eq!(render_text("{{name}}-{{name}}-{{a}}-{{b}}", &v, "f", &mut w), "demo-demo-{{a}}-{{b}}");
        assert_eq!(w.len(), 2);
        // 文件末尾无闭合 → 原样
        assert_eq!(render_text("end {{a", &v, "f", &mut w), "end {{a");
    }

    // ⑤ 新助手：resolve_in / validate_name / rebuild_index / dir_hash ----------

    /// 名字形态命中 `<dir>/<ref>/framework.yaml`；未命中 → 错误信息带按名排序清单
    #[test]
    fn resolve_in_finds_framework_and_lists_available() {
        let dir = tempfile::tempdir().unwrap();
        for (name, version) in [("b", "0.2.0"), ("a", "0.1.0")] {
            let d = dir.path().join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(MANIFEST_FILE), format!("name: {name}\nversion: {version}\n")).unwrap();
        }
        let got = resolve_in(dir.path(), "a").unwrap();
        assert_eq!(got, dir.path().join("a"));
        let err = resolve_in(dir.path(), "missing").unwrap_err();
        assert!(err.to_string().contains("可用内置: a, b"), "实际错误：{err}");
    }

    /// 安全红线：含 `..` 组件一律 Format（无论名字/路径形态）；
    /// 含分隔符但无 `..` 且不存在 → NotFound（文案带本地路径提示）
    #[test]
    fn resolve_in_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        for r in ["a/../b", "../evil"] {
            let err = resolve_in(dir.path(), r).unwrap_err();
            assert!(matches!(err, ZpmError::Format(_)), "输入 {r:?} 实际错误：{err}");
        }
        let err = resolve_in(dir.path(), "nonexistent/path").unwrap_err();
        assert!(matches!(err, ZpmError::NotFound(_)), "实际错误：{err}");
        assert!(
            err.to_string().contains("也可传含 framework.yaml 的本地路径"),
            "实际错误：{err}"
        );
    }

    /// b、a 两框架 → index.yaml 的 frameworks 按 name 排序，四字段与 manifest 一致，
    /// generated_at 以 Z 结尾
    #[test]
    fn rebuild_index_writes_sorted_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str, version: &str, desc: &str| {
            let d = dir.path().join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(
                d.join(MANIFEST_FILE),
                format!("name: {name}\nversion: {version}\ndescription: {desc}\n"),
            )
            .unwrap();
        };
        mk("b-fw", "0.2.0", "B 框架");
        mk("a-fw", "0.1.0", "A 框架");
        let path = rebuild_index(dir.path()).unwrap();
        assert_eq!(path, dir.path().join(INDEX_FILE));
        let doc: IndexDoc = serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc.generated_at.ends_with('Z'), "generated_at：{}", doc.generated_at);
        assert_eq!(doc.frameworks.len(), 2);
        assert_eq!(doc.frameworks[0].name, "a-fw");
        assert_eq!(doc.frameworks[0].version, "0.1.0");
        assert_eq!(doc.frameworks[0].description.as_deref(), Some("A 框架"));
        assert_eq!(doc.frameworks[0].dir, "a-fw");
        assert_eq!(doc.frameworks[1].name, "b-fw");
        assert_eq!(doc.frameworks[1].version, "0.2.0");
        assert_eq!(doc.frameworks[1].description.as_deref(), Some("B 框架"));
        assert_eq!(doc.frameworks[1].dir, "b-fw");
    }

    /// 空 frameworks 目录 → 仍写出合法索引（frameworks 为空数组）
    #[test]
    fn rebuild_index_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = rebuild_index(dir.path()).unwrap();
        assert!(path.is_file());
        let doc: IndexDoc = serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(doc.frameworks.is_empty(), "实际：{:?}", doc.frameworks);
        assert!(doc.generated_at.ends_with('Z'), "generated_at：{}", doc.generated_at);
    }

    /// 同目录两次 hash 相同；改一个文件后 hash 不同
    #[test]
    fn dir_hash_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::write(dir.path().join("sub").join("b.txt"), "world").unwrap();
        let h1 = dir_hash(dir.path()).unwrap();
        let h2 = dir_hash(dir.path()).unwrap();
        assert_eq!(h1, h2, "同内容两次 hash 应相同");
        fs::write(dir.path().join("sub").join("b.txt"), "changed").unwrap();
        let h3 = dir_hash(dir.path()).unwrap();
        assert_ne!(h1, h3, "内容变化后 hash 应不同");
    }

    /// 不可读子树的遍历错误必须传播（而非静默跳过导致哈希误判「未变更」）
    #[test]
    fn dir_hash_propagates_walk_errors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir_all(locked.join("sub")).unwrap();
        fs::write(locked.join("sub").join("f.txt"), "x").unwrap();

        let mut perms = fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&locked, perms).unwrap();

        // 权限模型不生效的环境（如以 root 运行）跳过
        if fs::read_dir(&locked).is_ok() {
            let mut perms = fs::metadata(&locked).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&locked, perms).unwrap();
            return;
        }

        let r = dir_hash(dir.path());
        // 先恢复权限，保证 tempdir 清理不被阻塞
        let mut perms = fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&locked, perms).unwrap();

        let err = r.expect_err("不可读子树的遍历错误应传播");
        assert!(
            err.to_string().contains("遍历目录失败"),
            "错误消息应说明遍历失败：{err}"
        );
    }
}
