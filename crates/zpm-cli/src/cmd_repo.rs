//! `zpm repo`：仓库生命周期管理（create/list/switch/delete/sync）+ 激活仓库助手。
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use rusqlite::Connection;
use zpm_core::config::GlobalConfig;
use zpm_core::framework;
use zpm_core::ZpmError;
use zpm_meta::repo::{self, Repo};
use zpm_meta::transport::{self, RepoRef};

#[derive(Args)]
pub struct RepoArgs {
    #[command(subcommand)]
    pub command: RepoCommands,
}

#[derive(Subcommand)]
pub enum RepoCommands {
    /// 创建仓库（目录脚手架 + frameworks 索引 + 登记）
    Create {
        name: String,
        /// 仓库目录（缺省 <repo_home>/<name>）
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// 列出全部仓库（* 标记激活仓库）
    List,
    /// 切换激活仓库（接受已登记的名字或路径）
    Switch {
        #[arg(value_name = "NAME_OR_PATH")]
        r#ref: String,
    },
    /// 删除仓库（--keep-files 只删记录保留文件）
    Delete {
        #[arg(value_name = "NAME_OR_PATH")]
        r#ref: String,
        #[arg(long)]
        keep_files: bool,
    },
    /// 从本地路径或 Git 地址导入一份仓库镜像
    Sync {
        /// 来源：本地目录或 Git 地址
        #[arg(long)]
        from: String,
        /// 镜像名（缺省取来源末段 basename，剥 .git 后缀）
        #[arg(long)]
        name: Option<String>,
    },
}

pub fn run(args: RepoArgs) -> Result<(), ZpmError> {
    match args.command {
        RepoCommands::Create { name, path } => cmd_create(&name, path.as_deref()),
        RepoCommands::List => cmd_list(),
        RepoCommands::Switch { r#ref } => cmd_switch(&r#ref),
        RepoCommands::Delete { r#ref, keep_files } => cmd_delete(&r#ref, keep_files),
        RepoCommands::Sync { from, name } => cmd_sync(&from, name.as_deref()),
    }
}

/// 仓库根目录：显式配置 repo_home（~ 前缀展开）优先，否则默认 ~/.zpm。
pub(crate) fn repo_home() -> Result<PathBuf, ZpmError> {
    match GlobalConfig::get("repo_home")?.as_deref() {
        Some(v) if !v.is_empty() => expand_home(v),
        _ => Ok(dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".zpm")),
    }
}

/// 展开 ~ 前缀（~ 与 ~/…）；其余按字面路径。
fn expand_home(v: &str) -> Result<PathBuf, ZpmError> {
    let home = || dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    Ok(match v {
        "~" => home(),
        _ if v.starts_with("~/") => home().join(&v[2..]),
        _ => PathBuf::from(v),
    })
}

/// 打开（必要时创建）repo_home 下的元数据库；返回连接与 repo_home。
pub(crate) fn open_meta() -> Result<(Connection, PathBuf), ZpmError> {
    let home = repo_home()?;
    let conn = zpm_meta::db::open(&home)?;
    Ok((conn, home))
}

/// 激活仓库：读 config active_repo → repo::find；未设置或未命中即报错。
pub(crate) fn active_repo() -> Result<(Connection, Repo), ZpmError> {
    let (conn, _home) = open_meta()?;
    let key = GlobalConfig::get("active_repo")?;
    let found = match key.as_deref() {
        Some(name) if !name.is_empty() => repo::find(&conn, name)?,
        _ => None,
    };
    found.map(|r| (conn, r)).ok_or_else(|| {
        ZpmError::RepoNotFound("未激活仓库或激活仓库不存在；先 zpm repo switch <名字>".into())
    })
}

fn cmd_create(name: &str, path: Option<&Path>) -> Result<(), ZpmError> {
    let (conn, home) = open_meta()?;
    let target = match path {
        Some(p) => p.to_path_buf(),
        None => home.join(name),
    };
    // 名字/路径已登记则拒绝（先于目录检查，文案指明冲突点）
    if repo::find(&conn, name)?.is_some() {
        return Err(ZpmError::Format(format!("repo 名字 {name} 已登记")));
    }
    if target.exists() {
        let canonical = target.canonicalize()?;
        if repo::find(&conn, &canonical.display().to_string())?.is_some() {
            return Err(ZpmError::Format(format!(
                "repo 路径 {} 已登记",
                canonical.display()
            )));
        }
        if dir_non_empty(&target)? {
            return Err(ZpmError::AlreadyExists(target.display().to_string()));
        }
    }
    std::fs::create_dir_all(target.join("frameworks"))?;
    framework::rebuild_index(&target.join("frameworks"))?;
    let registered = repo::create(&conn, name, &target)?;
    println!("已创建仓库 {}：{}", registered.name, registered.path.display());
    Ok(())
}

fn cmd_list() -> Result<(), ZpmError> {
    let (conn, _home) = open_meta()?;
    let active = GlobalConfig::get("active_repo")?;
    let repos = repo::list(&conn)?;
    if repos.is_empty() {
        println!("暂无仓库：先 zpm repo create <名字> 创建");
        return Ok(());
    }
    for r in repos {
        let marker = if active.as_deref() == Some(r.name.as_str()) { "*" } else { " " };
        println!("{marker}{}\t{}\t{}", r.name, r.path.display(), r.created_at);
    }
    Ok(())
}

fn cmd_switch(r#ref: &str) -> Result<(), ZpmError> {
    let (conn, _home) = open_meta()?;
    let target = repo::find(&conn, r#ref)?
        .ok_or_else(|| ZpmError::RepoNotFound(format!("仓库不存在：{}", r#ref)))?;
    GlobalConfig::set("active_repo", &target.name)?;
    println!("已切换激活仓库：{}", target.name);
    Ok(())
}

fn cmd_delete(r#ref: &str, keep_files: bool) -> Result<(), ZpmError> {
    let (conn, _home) = open_meta()?;
    let target = repo::find(&conn, r#ref)?
        .ok_or_else(|| ZpmError::RepoNotFound(format!("仓库不存在：{}", r#ref)))?;

    // 安全阀一：目标是当前工作目录
    let cwd = std::env::current_dir()?;
    let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
    if target.path == cwd || target.path == cwd_canonical {
        return Err(ZpmError::Dangerous(format!(
            "拒绝删除当前工作目录：{}",
            target.path.display()
        )));
    }
    // 安全阀二：仓库内含已登记项目
    let projects = zpm_meta::project::list(&conn)?;
    if projects.iter().any(|p| p.repo == target.name) {
        return Err(ZpmError::Dangerous(format!(
            "仓库内含已登记项目，先删除相关 project：{}",
            target.name
        )));
    }

    repo::delete(&conn, target.id)?;
    if !keep_files {
        std::fs::remove_dir_all(&target.path)?;
    }
    if GlobalConfig::get("active_repo")?.as_deref() == Some(target.name.as_str()) {
        GlobalConfig::set("active_repo", "")?;
    }
    match keep_files {
        true => println!(
            "已删除仓库 {}：{}（文件保留）",
            target.name,
            target.path.display()
        ),
        false => println!("已删除仓库 {}：{}", target.name, target.path.display()),
    }
    Ok(())
}

fn cmd_sync(from: &str, name: Option<&str>) -> Result<(), ZpmError> {
    let parsed = transport::parse_ref(from)?;
    let mirror_name = match name {
        Some(n) => n.to_string(),
        None => derive_name(&parsed).ok_or_else(|| {
            ZpmError::Format(format!("无法从来源推导仓库名：{from}；请用 --name 指定"))
        })?,
    };
    let (conn, home) = open_meta()?;
    let target = home.join(&mirror_name);
    if target.exists() && dir_non_empty(&target)? {
        return Err(ZpmError::AlreadyExists(target.display().to_string()));
    }
    std::fs::create_dir_all(&target)?;
    transport::extract_from_repo(&parsed, "", &target)?;
    framework::rebuild_index(&target.join("frameworks"))?;
    let registered = repo::create(&conn, &mirror_name, &target)?;
    println!("已创建仓库 {}：{}", registered.name, registered.path.display());
    Ok(())
}

/// 从引用推导镜像名：Local 取末段 basename；Git 取 URL 末段并剥 .git 后缀。
fn derive_name(r: &RepoRef) -> Option<String> {
    let last = match r {
        RepoRef::Local(p) => p.file_name()?.to_str()?.to_string(),
        RepoRef::Git(url) => url.trim_end_matches('/').rsplit('/').next()?.to_string(),
    };
    let trimmed = last.strip_suffix(".git").unwrap_or(&last);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn dir_non_empty(dir: &Path) -> Result<bool, ZpmError> {
    Ok(std::fs::read_dir(dir)?.next().is_some())
}
