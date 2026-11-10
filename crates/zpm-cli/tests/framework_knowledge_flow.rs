//! framework knowledge 集成测试：项目知识库 ↔ 激活仓库框架知识基线的双向同步契约。
//!
//! 两部分：
//! 1. 核心契约（自 knowledge_flow.rs 迁移，机制不变：manifest 双表 + 三方冲突 + scope 子树）
//! 2. CLI 级测试（新命令面 `zpm framework knowledge`：基线解析自激活仓库、--from 覆盖、
//!    非项目目录报错）

use std::fs;
use std::path::Path;
use tempfile::{tempdir, TempDir};
use zpm_core::knowledge::{self, KNOWLEDGE_MANIFEST};

// ---------------------------------------------------------------------------
// 第一部分：核心契约（直调 knowledge::pull/push，基线显式传入——与 CLI 解耦）
// ---------------------------------------------------------------------------

/// sha256 的测试快捷方式（与 hash_file 同源）
fn h(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

fn write(p: &Path, content: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn manifest_files(project_root: &Path) -> std::collections::BTreeMap<String, String> {
    let txt = fs::read_to_string(project_root.join(KNOWLEDGE_MANIFEST)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&txt).unwrap();
    v["files"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, val)| (k.clone(), val.as_str().unwrap().to_string()))
        .collect()
}

/// 简报步骤 1 的主场景：pull 的四条规则 + manifest 终态
#[test]
fn pull_updates_adds_skips_and_deletes() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));

    // 基线 v1
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&base.join("knowledge/lessons/l1.md"), "l1");
    // 项目：a.md 未改、b.md 项目自改、l1.md 未改
    write(&proj.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/b.md"), "local");
    write(&proj.join("knowledge/lessons/l1.md"), "l1");
    // 项目 manifest 记录 v1 哈希
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {
            "knowledge/rules/a.md": h("v1"),
            "knowledge/lessons/l1.md": h("l1"),
        }})
        .to_string(),
    )
    .unwrap();

    // 基线升级：a.md→v2、新增 c.md、删除 l1.md
    write(&base.join("knowledge/rules/a.md"), "v2");
    write(&base.join("knowledge/rules/c.md"), "c");
    fs::remove_file(base.join("knowledge/lessons/l1.md")).unwrap();

    let r = knowledge::pull(&base, &proj, None, false).unwrap();

    assert_eq!(r.updated, vec!["knowledge/rules/a.md"]);
    assert_eq!(r.added, vec!["knowledge/rules/c.md"]);
    assert!(r.skipped.contains(&"knowledge/rules/b.md".to_string()), "skipped={:?}", r.skipped);
    assert_eq!(r.deleted, vec!["knowledge/lessons/l1.md"]);

    // 文件落盘状态
    assert_eq!(fs::read_to_string(proj.join("knowledge/rules/a.md")).unwrap(), "v2");
    assert_eq!(fs::read_to_string(proj.join("knowledge/rules/c.md")).unwrap(), "c");
    assert_eq!(fs::read_to_string(proj.join("knowledge/rules/b.md")).unwrap(), "local", "项目自改文件不得被覆盖");
    assert!(!proj.join("knowledge/lessons/l1.md").exists(), "基线删除且未改动的文件应被删除");

    // manifest 终态：a.md=v2 哈希、c.md 新条目、l1.md 条目消失
    let m = manifest_files(&proj);
    assert_eq!(m.get("knowledge/rules/a.md").unwrap(), &h("v2"));
    assert_eq!(m.get("knowledge/rules/c.md").unwrap(), &h("c"));
    assert!(!m.contains_key("knowledge/lessons/l1.md"));
}

/// 未改动文件不出现在任何报告分类，且重复 pull 幂等（第二次全空）
#[test]
fn pull_is_idempotent_on_second_run() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1")}}).to_string(),
    )
    .unwrap();

    let r1 = knowledge::pull(&base, &proj, None, false).unwrap();
    assert!(r1.updated.is_empty() && r1.added.is_empty() && r1.skipped.is_empty() && r1.deleted.is_empty());

    let r2 = knowledge::pull(&base, &proj, None, false).unwrap();
    assert!(r2.updated.is_empty() && r2.added.is_empty() && r2.skipped.is_empty() && r2.deleted.is_empty());
}

/// push：无参（非交互）→ BadRequest；--all 把项目知识文件推入基线且哈希一致
#[test]
fn push_requires_all_or_paths_then_pushes_all() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&proj.join("knowledge/rules/b.md"), "local");
    write(&proj.join("knowledge/lessons/note.md"), "note");

    // 无参 → BadRequest
    let err = knowledge::push(&proj, &base, &[], false, None, false).unwrap_err();
    assert!(
        err.to_string().contains("--all") || err.to_string().contains("路径"),
        "实际错误：{err}"
    );

    // --all → 基线出现项目文件，内容一致
    let r = knowledge::push(&proj, &base, &[], true, None, false).unwrap();
    assert_eq!(fs::read_to_string(base.join("knowledge/rules/b.md")).unwrap(), "local");
    assert_eq!(fs::read_to_string(base.join("knowledge/lessons/note.md")).unwrap(), "note");
    assert_eq!(r.added.len() + r.updated.len(), 2, "added={:?} updated={:?}", r.added, r.updated);

    // 显式路径：只推一个
    let base2 = tmp.path().join("base2");
    write(&proj.join("knowledge/rules/x.md"), "x");
    let r2 = knowledge::push(&proj, &base2, &["knowledge/rules/x.md".into()], false, None, false).unwrap();
    assert_eq!(fs::read_to_string(base2.join("knowledge/rules/x.md")).unwrap(), "x");
    assert_eq!(r2.added, vec!["knowledge/rules/x.md"]);
    assert!(!base2.join("knowledge/lessons/note.md").exists(), "显式路径不得夹带其他文件");
}

/// push 幂等：重复推同一文件不产生 added/updated
#[test]
fn push_is_idempotent() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&proj.join("knowledge/rules/b.md"), "local");
    knowledge::push(&proj, &base, &[], true, None, false).unwrap();
    let r2 = knowledge::push(&proj, &base, &[], true, None, false).unwrap();
    assert!(r2.added.is_empty() && r2.updated.is_empty());
}

/// manifest 升级：旧格式（仅 files）可加载；pull 后 files 保留、base 表记录 added/updated 条目
#[test]
fn pull_records_base_hash_and_upgrades_legacy_manifest() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&base.join("knowledge/rules/n.md"), "n1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    // 旧格式 manifest：仅 files 键
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1")}}).to_string(),
    )
    .unwrap();

    write(&base.join("knowledge/rules/a.md"), "v2"); // updated 分支
    let r = knowledge::pull(&base, &proj, None, false).unwrap();
    assert_eq!(r.updated, vec!["knowledge/rules/a.md"]);

    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.join(KNOWLEDGE_MANIFEST)).unwrap()).unwrap();
    assert_eq!(v["files"]["knowledge/rules/a.md"], h("v2"), "files 条目保留");
    assert_eq!(v["base"]["knowledge/rules/a.md"], h("v2"), "updated 记 base");

    // added 分支也记 base
    knowledge::pull(&base, &proj, None, false).unwrap();
    write(&base.join("knowledge/rules/c.md"), "c1");
    let r2 = knowledge::pull(&base, &proj, None, false).unwrap();
    assert_eq!(r2.added, vec!["knowledge/rules/c.md"]);
    let v2: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.join(KNOWLEDGE_MANIFEST)).unwrap()).unwrap();
    assert_eq!(v2["base"]["knowledge/rules/c.md"], h("c1"), "added 记 base");
    assert_eq!(v2["files"]["knowledge/rules/a.md"], h("v2"), "files 不丢条目");
}

/// 项目自改但内容与基线一致（补登记分支）也记 base
#[test]
fn pull_records_base_when_project_matches_baseline() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    // 项目无 manifest（哈希不匹配视为改过 → 内容与基线一致 → 补登记分支）
    let r = knowledge::pull(&base, &proj, None, false).unwrap();
    assert!(r.updated.is_empty() && r.added.is_empty() && r.skipped.is_empty());
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.join(KNOWLEDGE_MANIFEST)).unwrap()).unwrap();
    assert_eq!(v["base"]["knowledge/rules/a.md"], h("v1"), "补登记记 base");
    assert_eq!(v["files"]["knowledge/rules/a.md"], h("v1"));
}

/// 两边都改且不同 → conflict，绝不写入基线，manifest 不动
#[test]
fn push_detects_conflict_and_never_overwrites() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1")},
                           "base": {"knowledge/rules/a.md": h("v1")}}).to_string(),
    ).unwrap();

    write(&proj.join("knowledge/rules/a.md"), "v2"); // 项目改
    write(&base.join("knowledge/rules/a.md"), "v3"); // 基线改

    let r = knowledge::push(&proj, &base, &["knowledge/rules/a.md".into()], false, None, false).unwrap();
    assert_eq!(r.conflicts, vec!["knowledge/rules/a.md"]);
    assert_eq!(fs::read_to_string(base.join("knowledge/rules/a.md")).unwrap(), "v3", "基线不得被覆盖");
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.join(KNOWLEDGE_MANIFEST)).unwrap()).unwrap();
    assert_eq!(v["files"]["knowledge/rules/a.md"], h("v1"), "冲突时 manifest 不动");
}

/// 基线未变（theirs == base）→ 干净推送，manifest 双表更新为推送内容
#[test]
fn push_clean_when_baseline_unchanged_records_base() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1")},
                           "base": {"knowledge/rules/a.md": h("v1")}}).to_string(),
    ).unwrap();

    write(&proj.join("knowledge/rules/a.md"), "v2");
    let r = knowledge::push(&proj, &base, &["knowledge/rules/a.md".into()], false, None, false).unwrap();
    assert_eq!(r.updated, vec!["knowledge/rules/a.md"]);
    assert!(r.conflicts.is_empty());
    assert_eq!(fs::read_to_string(base.join("knowledge/rules/a.md")).unwrap(), "v2");
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.join(KNOWLEDGE_MANIFEST)).unwrap()).unwrap();
    assert_eq!(v["base"]["knowledge/rules/a.md"], h("v2"));
    assert_eq!(v["files"]["knowledge/rules/a.md"], h("v2"));
}

/// 项目未变（ours == 上次 proj 哈希）而基线已更新 → skip 等 pull
#[test]
fn push_skips_when_project_unchanged_baseline_ahead() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1")},
                           "base": {"knowledge/rules/a.md": h("v1")}}).to_string(),
    ).unwrap();

    write(&base.join("knowledge/rules/a.md"), "v3");
    let r = knowledge::push(&proj, &base, &["knowledge/rules/a.md".into()], false, None, false).unwrap();
    assert!(r.skipped.contains(&"knowledge/rules/a.md".to_string()), "skipped={:?}", r.skipped);
    assert!(r.conflicts.is_empty());
    assert_eq!(fs::read_to_string(base.join("knowledge/rules/a.md")).unwrap(), "v3", "基线保持更新版");
}

/// 旧 manifest（无 base 键）→ 兼容现状按干净推送
#[test]
fn push_legacy_manifest_without_base_overwrites_compat() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/a.md"), "v2");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1")}}).to_string(),
    ).unwrap();

    let r = knowledge::push(&proj, &base, &["knowledge/rules/a.md".into()], false, None, false).unwrap();
    assert_eq!(r.updated, vec!["knowledge/rules/a.md"]);
    assert!(r.conflicts.is_empty());
    assert_eq!(fs::read_to_string(base.join("knowledge/rules/a.md")).unwrap(), "v2");
    assert_eq!(r.compat_pushed, vec!["knowledge/rules/a.md"], "无 base 记录的覆盖应上报 compat_pushed");
}

/// dry-run：只报告（含冲突）不落盘、不改 manifest
#[test]
fn push_dry_run_reports_without_writing() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&base.join("knowledge/rules/b.md"), "b1");
    write(&proj.join("knowledge/rules/a.md"), "v2");
    write(&proj.join("knowledge/rules/b.md"), "b1");
    write(&proj.join("knowledge/rules/new.md"), "n1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1"),
                                     "knowledge/rules/b.md": h("b1")},
                           "base": {"knowledge/rules/a.md": h("v1"),
                                    "knowledge/rules/b.md": h("b1")}}).to_string(),
    ).unwrap();
    write(&base.join("knowledge/rules/a.md"), "v3"); // 基线也改了 a.md → 冲突

    let r = knowledge::push(&proj, &base, &[], true, None, true).unwrap();
    assert_eq!(r.conflicts, vec!["knowledge/rules/a.md"]);
    assert_eq!(r.added, vec!["knowledge/rules/new.md"]);
    assert!(r.updated.is_empty(), "b.md 幂等，无 updated");
    assert_eq!(fs::read_to_string(base.join("knowledge/rules/a.md")).unwrap(), "v3", "dry-run 不写基线");
    assert!(!base.join("knowledge/rules/new.md").exists(), "dry-run 不落盘");
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.join(KNOWLEDGE_MANIFEST)).unwrap()).unwrap();
    assert!(v["base"].get("knowledge/rules/new.md").is_none(), "dry-run 不改 manifest");
}

/// scoped push：只推指定子树，不夹带其他子树
#[test]
fn scoped_push_only_pushes_subtree() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&proj.join("knowledge/rules/x.md"), "x");
    write(&proj.join("knowledge/lessons/y.md"), "y");

    let r = knowledge::push(&proj, &base, &[], true, Some("rules/"), false).unwrap();
    assert_eq!(r.added, vec!["knowledge/rules/x.md"]);
    assert!(!base.join("knowledge/lessons/y.md").exists(), "不得夹带 lessons");
}

/// scoped push 无参（非交互）→ 默认推送整个子树（"rule push 即合并全部规则"）
#[test]
fn scoped_push_defaults_to_subtree_without_all() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&proj.join("knowledge/rules/x.md"), "x");
    write(&proj.join("knowledge/lessons/y.md"), "y");

    let r = knowledge::push(&proj, &base, &[], false, Some("rules/"), false).unwrap();
    assert_eq!(r.added, vec!["knowledge/rules/x.md"]);
    assert!(!base.join("knowledge/lessons/y.md").exists(), "不得夹带 lessons");
}

/// scoped push：显式路径越界 → BadRequest
#[test]
fn scoped_push_rejects_out_of_scope_paths() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&proj.join("knowledge/lessons/y.md"), "y");
    let err = knowledge::push(&proj, &base, &["knowledge/lessons/y.md".into()], false, Some("rules/"), false).unwrap_err();
    assert!(err.to_string().contains("rules/") || err.to_string().contains("范围"), "实际错误：{err}");
}

/// scoped pull：只拉指定子树，删除/本地文件 sweep 同样限定子树
#[test]
fn scoped_pull_only_pulls_subtree() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&base.join("knowledge/lessons/l.md"), "l1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/lessons/l.md"), "l1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1"),
                                     "knowledge/lessons/l.md": h("l1")},
                           "base": {"knowledge/rules/a.md": h("v1"),
                                    "knowledge/lessons/l.md": h("l1")}}).to_string(),
    ).unwrap();

    write(&base.join("knowledge/rules/a.md"), "v2");
    write(&base.join("knowledge/lessons/l.md"), "l2");
    let r = knowledge::pull(&base, &proj, Some("rules/"), false).unwrap();
    assert_eq!(r.updated, vec!["knowledge/rules/a.md"]);
    assert_eq!(fs::read_to_string(proj.join("knowledge/lessons/l.md")).unwrap(), "l1", "lessons 不得被 scoped pull 触及");
}

/// scoped pull：基线删除 scope 内文件且项目未改 → 删除项目文件（删除 sweep 在 scope 内生效）
#[test]
fn scoped_pull_deletes_removed_rules_within_scope() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&base.join("knowledge/rules/gone.md"), "g");
    write(&base.join("knowledge/lessons/l.md"), "l1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/gone.md"), "g");
    write(&proj.join("knowledge/lessons/l.md"), "l1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1"),
                                     "knowledge/rules/gone.md": h("g"),
                                     "knowledge/lessons/l.md": h("l1")},
                           "base": {"knowledge/rules/a.md": h("v1"),
                                    "knowledge/rules/gone.md": h("g"),
                                    "knowledge/lessons/l.md": h("l1")}}).to_string(),
    ).unwrap();

    fs::remove_file(base.join("knowledge/rules/gone.md")).unwrap();
    let r = knowledge::pull(&base, &proj, Some("rules/"), false).unwrap();
    assert_eq!(r.deleted, vec!["knowledge/rules/gone.md"]);
    assert!(!proj.join("knowledge/rules/gone.md").exists(), "scope 内基线删除应删项目文件");
    assert!(proj.join("knowledge/lessons/l.md").exists(), "scope 外不得触碰");
}

/// 旧 manifest 升级完整性：内容三处一致的文件经一次 pull 也补记 base（不变式：base.keys() == files.keys()）
#[test]
fn pull_upgrades_all_base_entries_in_one_pass() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "a1");
    write(&base.join("knowledge/rules/b.md"), "b1");
    write(&base.join("knowledge/lessons/l.md"), "l1");
    write(&proj.join("knowledge/rules/a.md"), "a1");
    write(&proj.join("knowledge/rules/b.md"), "b1");
    write(&proj.join("knowledge/lessons/l.md"), "l1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("a1"),
                                     "knowledge/rules/b.md": h("b1"),
                                     "knowledge/lessons/l.md": h("l1")}}).to_string(),
    ).unwrap();

    write(&base.join("knowledge/rules/b.md"), "b2"); // 仅 b 有流动
    let r = knowledge::pull(&base, &proj, None, false).unwrap();
    assert_eq!(r.updated, vec!["knowledge/rules/b.md"]);

    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.join(KNOWLEDGE_MANIFEST)).unwrap()).unwrap();
    let files: Vec<&String> = v["files"].as_object().unwrap().keys().collect();
    let base_keys: Vec<&String> = v["base"].as_object().unwrap().keys().collect();
    assert_eq!(files.len(), 3, "files 条目不得丢失");
    for k in &files {
        assert!(base_keys.contains(k), "文件 {} 未升级出 base 记录", k);
    }
}

/// push 幂等命中也补记 base（旧 manifest 从未流动的文件可经一次幂等 push 升级）
#[test]
fn push_records_base_on_idempotent_hit_with_legacy_manifest() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1")}}).to_string(),
    ).unwrap();

    let r = knowledge::push(&proj, &base, &[], false, Some("rules/"), false).unwrap();
    assert!(r.added.is_empty() && r.updated.is_empty() && r.conflicts.is_empty(), "内容一致应幂等");
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.join(KNOWLEDGE_MANIFEST)).unwrap()).unwrap();
    assert_eq!(v["base"]["knowledge/rules/a.md"], h("v1"), "幂等命中应补记 base");
}

/// scoped push：.. 绕过 scope 前缀校验 → BadRequest（断言只认范围拒绝，防 NotFound 假阳性）
#[test]
fn scoped_push_rejects_parent_dir_traversal() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&proj.join("knowledge/rules/ok.md"), "ok");
    write(&proj.join("knowledge/lessons/y.md"), "y");
    let err = knowledge::push(&proj, &base, &["knowledge/rules/../lessons/y.md".into()], false, Some("rules/"), false).unwrap_err();
    assert!(err.to_string().contains("路径超出同步范围"), "实际错误：{err}");
}

/// scoped pull dry-run：只报告不落盘、不改 manifest（规格 §7.3）
#[test]
fn scoped_pull_dry_run_reports_without_writing() {
    let tmp = tempdir().unwrap();
    let (base, proj) = (tmp.path().join("base"), tmp.path().join("proj"));
    write(&base.join("knowledge/rules/a.md"), "v1");
    write(&proj.join("knowledge/rules/a.md"), "v1");
    fs::create_dir_all(proj.join(".zpm")).unwrap();
    fs::write(
        proj.join(KNOWLEDGE_MANIFEST),
        serde_json::json!({"files": {"knowledge/rules/a.md": h("v1")},
                           "base": {"knowledge/rules/a.md": h("v1")}}).to_string(),
    ).unwrap();

    write(&base.join("knowledge/rules/a.md"), "v2");
    let r = knowledge::pull(&base, &proj, Some("rules/"), true).unwrap();
    assert_eq!(r.updated, vec!["knowledge/rules/a.md"]);
    assert_eq!(fs::read_to_string(proj.join("knowledge/rules/a.md")).unwrap(), "v1", "dry-run 不改项目文件");
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(proj.join(KNOWLEDGE_MANIFEST)).unwrap()).unwrap();
    assert_eq!(v["files"]["knowledge/rules/a.md"], h("v1"), "dry-run 不改 manifest");
    assert_eq!(v["base"]["knowledge/rules/a.md"], h("v1"), "dry-run 不改 manifest");
}

// ---------------------------------------------------------------------------
// 第二部分：CLI 级测试（新命令面 `zpm framework knowledge`）
// ---------------------------------------------------------------------------

use assert_cmd::Command;
use predicates::prelude::*;

fn zpm() -> Command {
    Command::cargo_bin("zpm").unwrap()
}

fn zpm_env(cfg: &TempDir) -> Command {
    let mut c = zpm();
    c.env("ZPM_CONFIG_HOME", cfg.path());
    c
}

/// 在项目目录内运行的命令
fn zpm_in(cfg: &TempDir, proj: &Path) -> Command {
    let mut c = zpm_env(cfg);
    c.current_dir(proj);
    c
}

/// 造 fixture 框架：framework.yaml + knowledge 基线一条 + 项目配置模板
fn make_knowledge_framework(dir: &Path, baseline: &str) {
    fs::create_dir_all(dir.join("knowledge/rules")).unwrap();
    fs::write(
        dir.join("framework.yaml"),
        "name: fw-knowledge\nversion: 0.1.0\n",
    )
    .unwrap();
    fs::write(dir.join("knowledge/rules/base.md"), baseline).unwrap();
    fs::write(
        dir.join("zpm.config.yaml.tmpl"),
        "name: {{name}}\nserver:\n  port: 4310\n  host: 0.0.0.0\n  theme: default\ndatasource:\n  type: sqlite\n  file: data/source.db\ntarget:\n  type: sqlite\n  file: data/app.db\nsync:\n  tables: []\n",
    )
    .unwrap();
}

/// 搭建激活仓库 + 加载框架 + 创建项目；返回 (cfg, rh)
fn setup(fw: &Path) -> (TempDir, TempDir) {
    let cfg = tempdir().unwrap();
    let rh = tempdir().unwrap();
    zpm_env(&cfg)
        .args(["config", "set", "repo_home"])
        .arg(rh.path())
        .assert()
        .success();
    zpm_env(&cfg)
        .args(["repo", "create", "main", "--path"])
        .arg(rh.path().join("main"))
        .assert()
        .success();
    zpm_env(&cfg).args(["repo", "switch", "main"]).assert().success();
    zpm_env(&cfg).args(["framework", "load"]).arg(fw).assert().success();
    let proj = rh.path().join("proj");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fw-knowledge"])
        .assert()
        .success();
    (cfg, rh)
}

/// 基线解析自激活仓库：项目侧改动 push --all → repo 内框架基线更新；
/// 基线再演进 → pull → 项目拿到最新版
#[test]
fn knowledge_baseline_resolves_from_active_repo() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fw-knowledge");
    make_knowledge_framework(&fw, "v1");
    let (cfg, rh) = setup(&fw);
    let proj = rh.path().join("proj");
    let baseline = rh.path().join("main").join("frameworks").join("fw-knowledge");

    // 项目侧改规则 → push --all → 激活仓库内框架基线被更新
    write(&proj.join("knowledge/rules/base.md"), "v2");
    zpm_in(&cfg, &proj)
        .args(["framework", "knowledge", "push", "--all"])
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(baseline.join("knowledge/rules/base.md")).unwrap(),
        "v2",
        "push 应写入激活仓库中的框架基线"
    );

    // 基线再演进（模拟框架侧直接编辑）→ pull → 项目拿到最新版
    write(&baseline.join("knowledge/rules/base.md"), "v3");
    zpm_in(&cfg, &proj)
        .args(["framework", "knowledge", "pull"])
        .assert()
        .success()
        .stdout(predicate::str::contains("知识同步"));
    assert_eq!(
        fs::read_to_string(proj.join("knowledge/rules/base.md")).unwrap(),
        "v3",
        "pull 应从激活仓库基线更新项目"
    );
}

/// --from 临时指定其它基线路径
#[test]
fn knowledge_from_override() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fw-knowledge");
    make_knowledge_framework(&fw, "v1");
    let (cfg, rh) = setup(&fw);
    let proj = rh.path().join("proj");

    let other = tmp.path().join("other-baseline");
    write(&other.join("knowledge/rules/other.md"), "o1");

    zpm_in(&cfg, &proj)
        .args(["framework", "knowledge", "pull", "--from"])
        .arg(&other)
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(proj.join("knowledge/rules/other.md")).unwrap(),
        "o1",
        "--from 基线应生效"
    );
}

/// 有 zpm.config.yaml 但无 framework.yaml 的目录 → 报错含 framework.yaml
#[test]
fn knowledge_requires_framework_yaml() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fw-knowledge");
    make_knowledge_framework(&fw, "v1");
    let (cfg, _rh) = setup(&fw);

    let bare = tmp.path().join("bare");
    fs::create_dir_all(&bare).unwrap();
    fs::write(bare.join("zpm.config.yaml"), "name: bare\n").unwrap();

    zpm_env(&cfg)
        .current_dir(&bare)
        .args(["framework", "knowledge", "pull"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("framework.yaml"));
}
