//! `zpm framework`：管理激活仓库内的框架（加载/列表/删除/创建/拉取/推送）
//! 以及项目知识库 ↔ 框架知识基线的同步（knowledge 子命令组）。
//!
//! 所有子命令都作用于激活仓库（`cmd_repo::active_repo()`），
//! 框架根目录 = `<激活仓库>/frameworks`，索引由 core `rebuild_index` 维护。
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};
use zpm_core::error::ZpmError;
use zpm_core::framework::{self, FrameworkManifest};
use zpm_core::{knowledge, project_config};
use zpm_meta::transport;

use crate::cmd_repo::active_repo;

#[derive(Args)]
pub struct FrameworkArgs {
    #[command(subcommand)]
    pub command: FrameworkCommands,
}

#[derive(Subcommand)]
pub enum FrameworkCommands {
    /// 加载框架到激活仓库（目录须含 framework.yaml）
    Load {
        /// 框架源目录
        path: PathBuf,
        /// 框架名（默认取 manifest.name）
        name: Option<String>,
        /// 目标已存在时覆盖
        #[arg(long)]
        force: bool,
    },
    /// 列出激活仓库中已加载框架
    List,
    /// 从激活仓库删除框架
    Delete {
        /// 框架名
        name: String,
    },
    /// 在激活仓库中创建框架脚手架
    Create {
        /// 框架名
        name: String,
    },
    /// 从仓库拉取框架到激活仓库
    Pull {
        /// 框架名
        name: String,
        /// 来源仓库：本地路径或 Git 地址
        #[arg(long)]
        from: String,
        /// 本地已存在时覆盖
        #[arg(long)]
        force: bool,
    },
    /// 推送激活仓库中的框架到仓库
    Push {
        /// 框架名
        name: String,
        /// 目标仓库：本地路径或 Git 地址
        #[arg(long)]
        to: String,
        /// 远端内容不同时覆盖
        #[arg(long)]
        force: bool,
    },
    /// 项目知识库 ↔ 激活仓库框架知识基线（pull/push/rule/lesson）
    Knowledge(KnowledgeArgs),
}

pub fn run(args: FrameworkArgs) -> Result<(), ZpmError> {
    match args.command {
        FrameworkCommands::Load { path, name, force } => load(&path, name.as_deref(), force),
        FrameworkCommands::List => list(),
        FrameworkCommands::Delete { name } => delete(&name),
        FrameworkCommands::Create { name } => create(&name),
        FrameworkCommands::Pull { name, from, force } => pull(&name, &from, force),
        FrameworkCommands::Push { name, to, force } => push(&name, &to, force),
        FrameworkCommands::Knowledge(k) => knowledge_run(k),
    }
}

fn load(path: &Path, name: Option<&str>, force: bool) -> Result<(), ZpmError> {
    let (_conn, repo) = active_repo()?;
    let frameworks = repo.path.join("frameworks");
    let manifest: FrameworkManifest = framework::load_manifest(path)?;
    let name = name.unwrap_or(manifest.name.as_str()).to_string();
    framework::validate_name(&name)?;
    let dest = frameworks.join(&name);
    if dest.exists() {
        if !force {
            return Err(ZpmError::AlreadyExists(format!(
                "framework {name}（{}）；用 --force 覆盖",
                dest.display()
            )));
        }
        std::fs::remove_dir_all(&dest)?;
    }
    // 加载不做变量渲染（{{var}} 保留原样，init 时才渲染）
    let stats = framework::copy_tree(path, &dest, &Default::default())?;
    println!("已加载框架 {name} → {}（{} 个文件）", dest.display(), stats.files);
    for w in &stats.warnings {
        println!("  警告：{w}");
    }
    framework::rebuild_index(&frameworks)?;
    Ok(())
}

fn list() -> Result<(), ZpmError> {
    let (_conn, repo) = active_repo()?;
    let frameworks = repo.path.join("frameworks");
    let mut found = 0;
    if frameworks.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&frameworks)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.join(framework::MANIFEST_FILE).is_file())
            .collect();
        entries.sort();
        for dir in entries {
            let m = framework::load_manifest(&dir)?;
            found += 1;
            match &m.description {
                Some(d) => println!("{}\t{}\t{}", m.name, m.version, d),
                None => println!("{}\t{}", m.name, m.version),
            }
        }
    }
    if found == 0 {
        println!("尚未加载任何框架（用 `zpm framework load <目录>` 加载）");
    }
    Ok(())
}

fn delete(name: &str) -> Result<(), ZpmError> {
    let (conn, repo) = active_repo()?;
    framework::validate_name(name)?;
    let frameworks = repo.path.join("frameworks");
    let dest = frameworks.join(name);
    if !dest.exists() {
        return Err(ZpmError::NotFound(format!("框架不存在：{name}（{}）", dest.display())));
    }
    std::fs::remove_dir_all(&dest)?;
    // 引用检查只提示不阻断：已 init 的项目产物不受影响
    let users: Vec<String> = zpm_meta::project::list(&conn)?
        .into_iter()
        .filter(|p| p.framework == name)
        .map(|p| p.name)
        .collect();
    if !users.is_empty() {
        println!(
            "警告：以下项目仍登记使用框架 {name}：{}（其已生成产物不受影响）",
            users.join(", ")
        );
    }
    framework::rebuild_index(&frameworks)?;
    println!("已删除框架 {name}");
    Ok(())
}

fn create(name: &str) -> Result<(), ZpmError> {
    let (_conn, repo) = active_repo()?;
    framework::validate_name(name)?;
    let frameworks = repo.path.join("frameworks");
    let dest = frameworks.join(name);
    if dest.exists() {
        return Err(ZpmError::AlreadyExists(format!(
            "framework {name}（{}）",
            dest.display()
        )));
    }
    std::fs::create_dir_all(dest.join("knowledge/rules"))?;
    std::fs::create_dir_all(dest.join("knowledge/lessons"))?;
    std::fs::create_dir_all(dest.join("knowledge/assets"))?;
    std::fs::create_dir_all(dest.join("pages"))?;
    std::fs::write(
        dest.join(framework::MANIFEST_FILE),
        format!("name: {name}\nversion: 0.1.0\ndescription: \"\"\nvariables: {{}}\n"),
    )?;
    framework::rebuild_index(&frameworks)?;
    println!("已创建框架 {name} → {}", dest.display());
    Ok(())
}

fn pull(name: &str, from: &str, force: bool) -> Result<(), ZpmError> {
    let (_conn, repo) = active_repo()?;
    framework::validate_name(name)?;
    let frameworks = repo.path.join("frameworks");
    let src = transport::parse_ref(from)?;
    let tmp = tempfile::tempdir()?;
    transport::extract_from_repo(&src, &format!("frameworks/{name}"), tmp.path())?;
    if !tmp.path().join(framework::MANIFEST_FILE).is_file() {
        return Err(ZpmError::NotFound(format!("源仓库不存在框架 {name}")));
    }
    let dest = frameworks.join(name);
    if dest.exists() {
        if !force {
            return Err(ZpmError::AlreadyExists(format!(
                "framework {name}（{}）；用 --force 覆盖",
                dest.display()
            )));
        }
        std::fs::remove_dir_all(&dest)?;
    }
    let stats = framework::copy_tree(tmp.path(), &dest, &Default::default())?;
    println!("已拉取框架 {name} → {}（{} 个文件）", dest.display(), stats.files);
    framework::rebuild_index(&frameworks)?;
    Ok(())
}

fn push(name: &str, to: &str, force: bool) -> Result<(), ZpmError> {
    let (_conn, repo) = active_repo()?;
    framework::validate_name(name)?;
    let src = repo.path.join("frameworks").join(name);
    if !src.join(framework::MANIFEST_FILE).is_file() {
        return Err(ZpmError::NotFound(format!(
            "激活仓库不存在框架 {name}（先 zpm framework load 或 create）"
        )));
    }
    let dst = transport::parse_ref(to)?;
    let subpath = format!("frameworks/{name}");
    // 与远端比较：抽取到临时目录后比对目录哈希。
    // 远端无此框架（抽取失败或无 manifest）视为首次推送，无需 --force。
    let tmp = tempfile::tempdir()?;
    let remote_state = match transport::extract_from_repo(&dst, &subpath, tmp.path()) {
        Ok(()) if tmp.path().join(framework::MANIFEST_FILE).is_file() => {
            Some(framework::dir_hash(tmp.path())? == framework::dir_hash(&src)?)
        }
        _ => None,
    };
    match remote_state {
        Some(true) => {
            println!("已是最新：框架 {name} 与远端内容一致");
            return Ok(());
        }
        Some(false) if !force => {
            return Err(ZpmError::BadRequest(format!(
                "远端框架 {name} 与本地内容不同；用 --force 覆盖远端"
            )));
        }
        _ => {}
    }
    transport::copy_into_repo(&src, &dst, &subpath)?;
    println!("已推送框架 {name} → {to}");
    Ok(())
}

// ---------------------------------------------------------------------------
// knowledge 子命令组：项目知识库 ↔ 激活仓库框架知识基线（沿 zpm knowledge 机制，
// 基线解析从"全局 assets_dir"换为"激活仓库中项目来源框架的 knowledge/"）。
// ---------------------------------------------------------------------------

#[derive(Args)]
pub struct KnowledgeArgs {
    #[command(subcommand)]
    pub command: KnowledgeCommands,
}

#[derive(Subcommand)]
pub enum KnowledgeCommands {
    /// 从框架知识基线拉取更新到当前项目
    Pull {
        /// 基线目录（默认激活仓库中对应框架的 knowledge 基线）
        #[arg(long)]
        from: Option<PathBuf>,
        /// 只预览同步结果，不写入
        #[arg(long)]
        dry_run: bool,
    },
    /// 把项目知识条目提升到基线（非交互需 --all 或显式路径）
    Push {
        /// 基线目录（默认激活仓库中对应框架的 knowledge 基线）
        #[arg(long)]
        from: Option<PathBuf>,
        /// 推送全部项目知识文件
        #[arg(long)]
        all: bool,
        /// 只预览推送结果，不写入；存在冲突时非零退出
        #[arg(long)]
        dry_run: bool,
        /// 显式条目路径（项目根相对，如 knowledge/rules/x.md）
        paths: Vec<String>,
    },
    /// 规则子树（knowledge/rules/）定向同步
    Rule {
        #[command(subcommand)]
        cmd: ScopedCommands,
    },
    /// 教训子树（knowledge/lessons/）定向同步
    Lesson {
        #[command(subcommand)]
        cmd: ScopedCommands,
    },
}

#[derive(Subcommand)]
pub enum ScopedCommands {
    /// 从基线拉取该子树
    Pull {
        /// 基线目录（默认激活仓库中对应框架的 knowledge 基线）
        #[arg(long)]
        from: Option<PathBuf>,
        /// 只预览同步结果，不写入
        #[arg(long)]
        dry_run: bool,
    },
    /// 把该子树推送到基线（无参默认推送整个子树）
    Push {
        /// 基线目录（默认激活仓库中对应框架的 knowledge 基线）
        #[arg(long)]
        from: Option<PathBuf>,
        /// 推送该子树全部文件（无参默认即全子树，兼容保留）
        #[arg(long)]
        all: bool,
        /// 只预览推送结果，不写入；存在冲突时非零退出
        #[arg(long)]
        dry_run: bool,
        /// 显式条目路径（项目根相对，如 knowledge/rules/x.md）
        paths: Vec<String>,
    },
}

const SCOPE_RULES: &str = "rules/";
const SCOPE_LESSONS: &str = "lessons/";

fn knowledge_run(args: KnowledgeArgs) -> Result<(), ZpmError> {
    match args.command {
        KnowledgeCommands::Pull { from, dry_run } => do_pull(from.as_deref(), None, dry_run),
        KnowledgeCommands::Push { from, all, dry_run, paths } => {
            do_push(from.as_deref(), all, dry_run, None, &paths)
        }
        KnowledgeCommands::Rule { cmd } => match cmd {
            ScopedCommands::Pull { from, dry_run } => {
                do_pull(from.as_deref(), Some(SCOPE_RULES), dry_run)
            }
            ScopedCommands::Push { from, all, dry_run, paths } => {
                do_push(from.as_deref(), all, dry_run, Some(SCOPE_RULES), &paths)
            }
        },
        KnowledgeCommands::Lesson { cmd } => match cmd {
            ScopedCommands::Pull { from, dry_run } => {
                do_pull(from.as_deref(), Some(SCOPE_LESSONS), dry_run)
            }
            ScopedCommands::Push { from, all, dry_run, paths } => {
                do_push(from.as_deref(), all, dry_run, Some(SCOPE_LESSONS), &paths)
            }
        },
    }
}

/// 基线解析：--from 显式 > 激活仓库中项目来源框架的 knowledge 基线
/// （项目根 framework.yaml 的 name 字段确定框架，项目根按 zpm.config.yaml 向上查找）
fn resolve_baseline(from: Option<&Path>) -> Result<PathBuf, ZpmError> {
    match from {
        Some(p) => Ok(p.to_path_buf()),
        None => {
            let cwd = std::env::current_dir()?;
            let root = project_config::find_project_root(&cwd)?;
            let fw_name = framework::load_manifest(&root)?.name;
            let (_conn, repo) = active_repo()?;
            Ok(repo.path.join("frameworks").join(fw_name))
        }
    }
}

fn do_pull(from: Option<&Path>, scope: Option<&str>, dry_run: bool) -> Result<(), ZpmError> {
    let cwd = std::env::current_dir()?;
    let root = project_config::find_project_root(&cwd)?;
    let baseline = resolve_baseline(from)?;
    if !baseline.is_dir() {
        return Err(ZpmError::NotFound(format!("知识基线目录不存在：{}", baseline.display())));
    }
    let r = knowledge::pull(&baseline, &root, scope, dry_run)?;
    if dry_run {
        println!("（dry-run 预览，未写入任何文件）");
    }
    print_report(&r);
    Ok(())
}

fn do_push(
    from: Option<&Path>,
    all: bool,
    dry_run: bool,
    scope: Option<&str>,
    paths: &[String],
) -> Result<(), ZpmError> {
    let cwd = std::env::current_dir()?;
    let root = project_config::find_project_root(&cwd)?;
    let baseline = resolve_baseline(from)?;
    if !baseline.is_dir() {
        return Err(ZpmError::NotFound(format!("知识基线目录不存在：{}", baseline.display())));
    }
    let r = knowledge::push(&root, &baseline, paths, all, scope, dry_run)?;
    if dry_run {
        println!("（dry-run 预览，未写入任何文件）");
    }
    if r.is_empty() {
        println!("基线已是最新（无差异条目）。");
    } else {
        println!("已提升：{} 新增 / {} 更新", r.added.len(), r.updated.len());
        for p in r.added.iter().chain(r.updated.iter()) {
            println!("  - {p}");
        }
        for c in &r.conflicts {
            println!("  冲突（两边都已修改，未写入）：{c}");
        }
        for s in &r.skipped {
            println!("  跳过（项目未改动，基线更新待 pull）：{s}");
        }
        if !r.compat_pushed.is_empty() {
            println!(
                "  ⚠ {} 个文件在无基线记录（旧版清单）下覆盖基线，无法检测冲突；建议先 `zpm framework knowledge pull` 升级清单：",
                r.compat_pushed.len()
            );
            for p in &r.compat_pushed {
                println!("    - {p}");
            }
        }
    }
    if !r.conflicts.is_empty() {
        return Err(ZpmError::BadRequest(format!(
            "存在 {} 个冲突条目未写入基线；请逐个解决后重试（对话中可用 zpm-rule-sync 编排选择性合并）",
            r.conflicts.len()
        )));
    }
    Ok(())
}

fn print_report(r: &knowledge::PullReport) {
    if r.updated.is_empty() && r.added.is_empty() && r.deleted.is_empty() {
        println!("知识已是最新的。");
        for s in &r.skipped {
            println!("  跳过（本地已修改或项目本地新文件，未覆盖）：{s}");
        }
        return;
    }
    println!(
        "知识同步：{} 更新 / {} 新增 / {} 跳过 / {} 删除",
        r.updated.len(),
        r.added.len(),
        r.skipped.len(),
        r.deleted.len()
    );
    for s in &r.skipped {
        println!("  跳过（本地已修改或项目本地新文件，未覆盖）：{s}");
    }
}
