//! 仓库引用识别与 Local/Git 双后端传输。
//!
//! Git 后端通过 shell-out 系统 `git` 实现（不引入 git2），
//! 任何 git 命令失败时 [`MetaError::Transport`] 消息携带命令 stderr。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use walkdir::WalkDir;

use crate::error::{MetaError, MetaResult};

/// 仓库引用：本地路径或 Git 地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoRef {
    /// 本地目录（保留字面路径，读取时再 canonicalize）
    Local(PathBuf),
    /// Git 地址（URL 或以 .git 结尾的路径）
    Git(String),
}

/// 识别仓库引用：含 `://`、以 `git@` 开头或以 `.git` 结尾 → Git；否则 Local。
/// 以 `-` 开头的引用一律拒绝：防止后续被 shell-out git 当作选项解析。
pub fn parse_ref(s: &str) -> MetaResult<RepoRef> {
    if s.starts_with('-') {
        return Err(MetaError::Format(format!(
            "仓库引用不允许以 - 开头：{s}"
        )));
    }
    if s.contains("://") || s.starts_with("git@") || s.ends_with(".git") {
        Ok(RepoRef::Git(s.to_string()))
    } else {
        Ok(RepoRef::Local(PathBuf::from(s)))
    }
}

/// 从仓库抽取内容到 dst：subpath 为空表示整库，否则只取 `仓库/subpath` 子树（内容拷入 dst）。
pub fn extract_from_repo(src: &RepoRef, subpath: &str, dst: &Path) -> MetaResult<()> {
    validate_subpath(subpath)?;
    match src {
        RepoRef::Local(p) => extract_local(p, subpath, dst),
        RepoRef::Git(url) => extract_git(url, subpath, dst),
    }
}

/// 把 src 内容写入仓库：subpath 为空表示整库，否则替换 `仓库/subpath` 子树。
pub fn copy_into_repo(src: &Path, dst: &RepoRef, subpath: &str) -> MetaResult<()> {
    validate_subpath(subpath)?;
    match dst {
        RepoRef::Local(p) => {
            let root = p.canonicalize()?;
            let target = subtree_root(&root, subpath);
            replace_with(&target, src, subpath.is_empty())
        }
        RepoRef::Git(url) => copy_into_git(src, url, subpath),
    }
}

/// 校验 subpath 的 containment：拒绝绝对路径、`..` 组件、空组件（如 `a//b`、尾随 `/`）。
/// `""`（整库抽取/写入）是合法值。在触碰任何文件系统路径之前调用。
fn validate_subpath(subpath: &str) -> MetaResult<()> {
    if subpath.is_empty() {
        return Ok(());
    }
    if Path::new(subpath).is_absolute() {
        return Err(MetaError::Format(format!(
            "子路径必须是相对路径：{subpath}"
        )));
    }
    for comp in subpath.split('/') {
        if comp.is_empty() {
            return Err(MetaError::Format(format!(
                "子路径不允许包含空组件：{subpath}"
            )));
        }
        if comp == ".." {
            return Err(MetaError::Format(format!(
                "子路径不允许包含 ..：{subpath}"
            )));
        }
    }
    Ok(())
}

/// 计算 subpath 对应的仓库内路径（subpath 为空 → 仓库根）。
fn subtree_root(root: &Path, subpath: &str) -> PathBuf {
    if subpath.is_empty() {
        root.to_path_buf()
    } else {
        root.join(subpath)
    }
}

/// Local 抽取：canonicalize 后按 subpath 拷贝到 dst。
fn extract_local(src: &Path, subpath: &str, dst: &Path) -> MetaResult<()> {
    let src = src.canonicalize()?;
    let root = subtree_root(&src, subpath);
    if !subpath.is_empty() && !root.exists() {
        return Err(MetaError::Transport(format!("仓库中不存在子路径 {subpath}")));
    }
    copy_tree(&root, dst, subpath.is_empty())
}

/// Git 抽取：clone 到临时目录 → 取 subpath 内容拷入 dst（.git 不会进入 dst）。
fn extract_git(url: &str, subpath: &str, dst: &Path) -> MetaResult<()> {
    let temp = new_temp_repo()?;
    run_git(temp.path(), &["clone", url, "."], None)?;
    let root = subtree_root(temp.path(), subpath);
    if !subpath.is_empty() && !root.exists() {
        return Err(MetaError::Transport(format!(
            "远端仓库中不存在子路径 {subpath}"
        )));
    }
    copy_tree(&root, dst, subpath.is_empty())
}

/// Git 写入：clone 到临时目录 → 替换 subpath 子树 → add/commit/push。
/// commit 出现 "nothing to commit"（无变更）视为成功。
fn copy_into_git(src: &Path, url: &str, subpath: &str) -> MetaResult<()> {
    let temp = new_temp_repo()?;
    run_git(temp.path(), &["clone", url, "."], None)?;
    ensure_git_identity(temp.path())?;
    let target = subtree_root(temp.path(), subpath);
    replace_with(&target, src, subpath.is_empty())?;
    let message = if subpath.is_empty() {
        "zpm push".to_string()
    } else {
        format!("zpm push {subpath}")
    };
    run_git(temp.path(), &["add", "-A"], None)?;
    run_git(
        temp.path(),
        &["commit", "-m", message.as_str()],
        Some("nothing to commit"),
    )?;
    run_git(temp.path(), &["push", "origin", "HEAD"], None)?;
    Ok(())
}

/// 建临时目录供 clone 使用；失败归为 Transport（传输过程的环境错误）。
fn new_temp_repo() -> MetaResult<tempfile::TempDir> {
    tempfile::tempdir().map_err(|e| MetaError::Transport(format!("创建临时目录失败：{e}")))
}

/// 运行 git 子命令（cwd=dir）；非零退出 → Transport（消息含命令与 stderr）。
/// tolerate 给定时（如 commit 的 "nothing to commit"），输出含该片段视为成功。
fn run_git(dir: &Path, args: &[&str], tolerate: Option<&str>) -> MetaResult<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| MetaError::Transport(format!("执行 git 命令失败：{e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if let Some(needle) = tolerate {
        if stdout.contains(needle) || stderr.contains(needle) {
            return Ok(());
        }
    }
    Err(MetaError::Transport(format!(
        "git {} 失败：{}",
        args.join(" "),
        stderr.trim()
    )))
}

/// 确保临时仓库能 commit：全局已有 user.email 时沿用，否则写入仓库级兜底身份。
fn ensure_git_identity(dir: &Path) -> MetaResult<()> {
    let out = Command::new("git")
        .args(["config", "user.email"])
        .current_dir(dir)
        .output()
        .map_err(|e| MetaError::Transport(format!("执行 git 命令失败：{e}")))?;
    if out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty() {
        return Ok(());
    }
    run_git(dir, &["config", "user.name", "zpm"], None)?;
    run_git(dir, &["config", "user.email", "zpm@localhost"], None)?;
    Ok(())
}

/// 递归复制 src 的内容到 dst；whole_repo 为 true 时只跳过 src 顶层（depth 1）的
/// db/ 与 .git/——深层同名目录（如 frameworks/x/db/）正常复制。
fn copy_tree(src: &Path, dst: &Path, whole_repo: bool) -> MetaResult<()> {
    std::fs::create_dir_all(dst)?;
    let walker = WalkDir::new(src)
        .min_depth(1)
        .into_iter()
        .filter_entry(|e| {
            !whole_repo
                || e.depth() != 1
                || (e.file_name() != OsStr::new("db") && e.file_name() != OsStr::new(".git"))
        });
    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let io = match e.io_error() {
                    Some(io) => std::io::Error::new(io.kind(), io.to_string()),
                    None => std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
                };
                return Err(MetaError::Io(io));
            }
        };
        // 符号链接（含目录符号链接）一律拒绝：解引用复制可能带出仓库外内容，
        // 静默跳过又会让复制结果残缺——响亮失败优于两者。
        if entry.file_type().is_symlink() {
            return Err(MetaError::Format(format!(
                "源包含符号链接，暂不支持复制：{}",
                entry.path().display()
            )));
        }
        let rel = entry
            .path()
            .strip_prefix(src)
            .map_err(|e| MetaError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
        let target = dst.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
        } else {
            // 普通文件
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// 用 src 内容替换 target 的旧内容；whole_repo 为 true 时 target 是仓库根：
/// 跳过 src 顶层的 db/ 与 .git/，并保留 target 已有的 .git/。
fn replace_with(target: &Path, src: &Path, whole_repo: bool) -> MetaResult<()> {
    if whole_repo {
        // 整库替换：清空仓库根旧内容（保留 .git/），再拷入 src（跳过 db/、.git/）
        for child in std::fs::read_dir(target)? {
            let child = child?.path();
            if child.file_name() == Some(OsStr::new(".git")) {
                continue;
            }
            let md = std::fs::symlink_metadata(&child)?;
            if md.is_dir() {
                std::fs::remove_dir_all(&child)?;
            } else {
                std::fs::remove_file(&child)?;
            }
        }
    } else if target.exists() {
        // 子树替换：整目录删掉重拷
        std::fs::remove_dir_all(target)?;
    }
    copy_tree(src, target, whole_repo)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;
    use crate::MetaError;

    /// 在 path 写文件（自动建父目录）。
    fn write_file(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("建父目录失败");
        }
        std::fs::write(path, content).expect("写文件失败");
    }

    #[test]
    fn parse_ref_rules() {
        // 含 :// → Git
        assert!(matches!(parse_ref("https://x.com/a.git").unwrap(), RepoRef::Git(_)));
        // git@ 前缀 → Git
        assert!(matches!(parse_ref("git@github.com:a/b.git").unwrap(), RepoRef::Git(_)));
        // .git 后缀 → Git
        assert!(matches!(parse_ref("/tmp/x.git").unwrap(), RepoRef::Git(_)));
        // 其余 → Local（保留字面路径）
        assert_eq!(
            parse_ref("/tmp/x").unwrap(),
            RepoRef::Local(PathBuf::from("/tmp/x"))
        );
        assert_eq!(
            parse_ref("relative/path").unwrap(),
            RepoRef::Local(PathBuf::from("relative/path"))
        );
    }

    /// 以 `-` 开头的引用被拒绝（防止被 shell-out git 当作选项解析）
    #[test]
    fn parse_ref_rejects_leading_dash() {
        let err = parse_ref("-evil").expect_err("以 - 开头的引用应被拒绝");
        assert!(matches!(err, MetaError::Format(_)), "应为 Format，实际 {err}");
        assert!(err.to_string().contains("-evil"), "错误消息应含该引用：{err}");
        // 即使形如合法 Git 地址，`-` 开头也拒绝
        assert!(parse_ref("-x.git").is_err());
    }

    #[test]
    fn local_extract_skips_db_and_git() {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let src = dir.path().join("repo");
        write_file(&src.join("db/zpm.db"), "db");
        write_file(&src.join(".git/HEAD"), "git");
        write_file(&src.join("frameworks/a/f.txt"), "hello");

        let dst = dir.path().join("dst");
        extract_from_repo(
            &parse_ref(src.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"),
            "",
            &dst,
        )
        .expect("整库抽取失败");

        assert!(!dst.join("db").exists(), "dst 不应含 db/");
        assert!(!dst.join(".git").exists(), "dst 不应含 .git/");
        assert_eq!(
            std::fs::read_to_string(dst.join("frameworks/a/f.txt")).expect("读 f.txt 失败"),
            "hello"
        );
    }

    /// 整库抽取只剪枝顶层 db/ 与 .git/（depth 1）：深层同名目录（如 frameworks/x/db/）正常复制
    #[test]
    fn whole_repo_prune_keeps_nested_db_dir() {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let src = dir.path().join("repo");
        write_file(&src.join("db/zpm.db"), "db");
        write_file(&src.join(".git/HEAD"), "git");
        write_file(&src.join("frameworks/x/db/keep.txt"), "nested");

        let dst = dir.path().join("dst");
        extract_from_repo(
            &parse_ref(src.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"),
            "",
            &dst,
        )
        .expect("整库抽取失败");

        assert!(
            dst.join("frameworks/x/db/keep.txt").is_file(),
            "深层 db/ 目录不应被剪枝"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("frameworks/x/db/keep.txt"))
                .expect("读 keep.txt 失败"),
            "nested"
        );
        assert!(!dst.join("db").exists(), "顶层 db/ 仍应被剪枝");
        assert!(!dst.join(".git").exists(), "顶层 .git/ 仍应被剪枝");
    }

    /// 源中含文件符号链接 → 响亮失败（Format），不得解引用复制仓库外内容
    #[test]
    fn local_extract_rejects_symlink() {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let src = dir.path().join("repo");
        write_file(&src.join("frameworks/a/f.txt"), "a");
        // 指向仓库外的符号链接
        let outside = dir.path().join("outside.txt");
        write_file(&outside, "secret");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, src.join("frameworks/a/link.txt"))
            .expect("建符号链接失败");

        let dst = dir.path().join("dst");
        let err = extract_from_repo(
            &parse_ref(src.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"),
            "",
            &dst,
        )
        .expect_err("含符号链接的源应失败");
        assert!(matches!(err, MetaError::Format(_)), "应为 Format，实际 {err}");
        let msg = err.to_string();
        assert!(msg.contains("符号链接"), "错误消息应说明符号链接：{msg}");
        assert!(
            msg.contains("link.txt"),
            "错误消息应含符号链接路径：{msg}"
        );
        assert!(!dst.join("frameworks/a/link.txt").exists(), "不应复制符号链接目标内容");
    }

    #[test]
    fn local_extract_subpath() {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let src = dir.path().join("repo");
        write_file(&src.join("frameworks/a/f.txt"), "a");
        write_file(&src.join("frameworks/b/g.txt"), "b");
        write_file(&src.join("README.md"), "readme");

        let dst = dir.path().join("dst");
        extract_from_repo(
            &parse_ref(src.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"),
            "frameworks/a",
            &dst,
        )
        .expect("子树抽取失败");

        // dst 只含 frameworks/a 子树的内容
        assert!(dst.join("f.txt").exists(), "dst 应含子树内容");
        assert!(!dst.join("g.txt").exists(), "dst 不应含相邻子树内容");
        assert!(!dst.join("README.md").exists(), "dst 不应含子树外内容");
    }

    #[test]
    fn local_copy_into_replaces_subtree() {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let remote = dir.path().join("remote");
        write_file(&remote.join("frameworks/a/old.txt"), "old");
        write_file(&remote.join("frameworks/b/keep.txt"), "keep");

        let src = dir.path().join("fw");
        write_file(&src.join("f.txt"), "new");

        copy_into_repo(
            &src,
            &parse_ref(remote.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"),
            "frameworks/a",
        )
        .expect("子树写入失败");

        // frameworks/a 被整体替换为 src；相邻子树不受影响
        assert!(!remote.join("frameworks/a/old.txt").exists(), "旧内容应被替换");
        assert_eq!(
            std::fs::read_to_string(remote.join("frameworks/a/f.txt")).expect("读 f.txt 失败"),
            "new"
        );
        assert!(
            remote.join("frameworks/b/keep.txt").exists(),
            "相邻子树不应受影响"
        );
    }

    #[test]
    fn git_roundtrip_via_bare_remote() {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let remote = dir.path().join("remote.git");
        let out = Command::new("git")
            .args(["init", "--bare"])
            .arg(&remote)
            .output()
            .expect("运行 git init 失败");
        assert!(
            out.status.success(),
            "git init --bare 失败：{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let src = dir.path().join("fw");
        write_file(&src.join("framework.yaml"), "name: a\nversion: 0.1.0\n");

        let remote_ref =
            parse_ref(remote.to_str().expect("非 UTF-8 路径")).expect("解析引用失败");
        copy_into_repo(&src, &remote_ref, "frameworks/a").expect("写入 git 远端失败");

        let dst = dir.path().join("dst");
        extract_from_repo(&remote_ref, "frameworks/a", &dst).expect("从 git 远端抽取失败");
        assert_eq!(
            std::fs::read_to_string(dst.join("framework.yaml")).expect("读 framework.yaml 失败"),
            "name: a\nversion: 0.1.0\n"
        );
    }

    #[test]
    fn git_fetch_bad_url_returns_transport_error() {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let dst = dir.path().join("dst");
        let err = extract_from_repo(
            &RepoRef::Git("file:///nonexistent-nope.git".to_string()),
            "",
            &dst,
        )
        .expect_err("坏 URL 应失败");
        assert!(
            matches!(err, MetaError::Transport(_)),
            "应为 Transport，实际 {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("fatal"), "错误消息应含 git stderr 片段：{msg}");
    }

    #[test]
    fn local_extract_missing_subpath_errors() {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let src = dir.path().join("repo");
        write_file(&src.join("frameworks/a/f.txt"), "a");

        let dst = dir.path().join("dst");
        let err = extract_from_repo(
            &parse_ref(src.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"),
            "frameworks/missing",
            &dst,
        )
        .expect_err("不存在的子路径应失败");
        assert!(
            matches!(err, MetaError::Transport(_)),
            "应为 Transport，实际 {err}"
        );
        assert!(
            err.to_string().contains("frameworks/missing"),
            "错误消息应指明子路径：{err}"
        );
    }

    #[test]
    fn subpath_rejects_traversal_and_absolute() {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let repo = dir.path().join("repo");
        write_file(&repo.join("frameworks/a/f.txt"), "a");
        let src = dir.path().join("fw");
        write_file(&src.join("f.txt"), "new");
        let dst = dir.path().join("dst");

        // extract 拒绝含 .. 的子路径
        let err = extract_from_repo(
            &parse_ref(repo.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"),
            "../x",
            &dst,
        )
        .expect_err("含 .. 的子路径应被拒绝");
        assert!(matches!(err, MetaError::Format(_)), "应为 Format，实际 {err}");

        // copy_into 拒绝含 .. 的子路径（不得对仓库外目录做任何写操作）
        let err = copy_into_repo(
            &src,
            &parse_ref(repo.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"),
            "../x",
        )
        .expect_err("含 .. 的子路径应被拒绝");
        assert!(matches!(err, MetaError::Format(_)), "应为 Format，实际 {err}");

        // copy_into 拒绝绝对路径子路径
        let err = copy_into_repo(
            &src,
            &parse_ref(repo.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"),
            "/abs",
        )
        .expect_err("绝对路径子路径应被拒绝");
        assert!(matches!(err, MetaError::Format(_)), "应为 Format，实际 {err}");

        // 越界写入不应发生：仓库外不应出现 x 目录
        assert!(!dir.path().join("x").exists(), "仓库外不应被写入");

        // ""（整库抽取）仍合法，行为不受影响
        write_file(&repo.join("db/zpm.db"), "db");
        write_file(&repo.join(".git/HEAD"), "git");
        extract_from_repo(&parse_ref(repo.to_str().expect("非 UTF-8 路径")).expect("解析引用失败"), "", &dst)
            .expect("整库抽取应合法");
        assert!(dst.join("frameworks/a/f.txt").exists());
        assert!(!dst.join("db").exists(), "整库抽取仍应跳过 db/");
        assert!(!dst.join(".git").exists(), "整库抽取仍应跳过 .git/");
    }
}
