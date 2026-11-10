//! `zpm db`：目标库初始化、seed、快照导入导出与数据同步。
use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};
use zpm_core::error::ZpmError;
use zpm_core::{dbsync, project_config, snapshot, store};

#[derive(Args)]
pub struct DbArgs {
    #[command(subcommand)]
    pub command: DbCommands,
}

#[derive(Subcommand)]
pub enum DbCommands {
    /// 初始化目标库（sqlite 建文件 + WAL；pg 验证连接可达）
    Init,
    /// 对目标库执行 SQL 脚本（分号分隔，逐条执行，错误带语句序号）
    Seed { file: PathBuf },
    /// 导出目标库为快照 JSON（默认 zpm-snapshot.json）
    Export { file: Option<PathBuf> },
    /// 从快照 JSON 导入目标库（shadow-rename 全量替换，幂等）
    Import { file: PathBuf },
    /// 数据同步（源库 → 目标库，shadow-rename 全量替换）
    Sync {
        /// 指定 zpm.config.yaml 路径（默认当前目录向上查找）
        #[arg(long)]
        config: Option<PathBuf>,
        /// 只同步这些表（逗号分隔；须已在 zpm.config.yaml 声明）
        #[arg(long)]
        tables: Option<String>,
        /// 临时用指定 sqlite 文件作为数据源
        #[arg(long)]
        from: Option<PathBuf>,
    },
}

pub fn run(args: DbArgs) -> Result<(), ZpmError> {
    match args.command {
        DbCommands::Init => init(),
        DbCommands::Seed { file } => seed(&file),
        DbCommands::Export { file } => export(file.as_deref()),
        DbCommands::Import { file } => import(&file),
        DbCommands::Sync { config, tables, from } => {
            sync(config.as_deref(), tables.as_deref(), from.as_deref())
        }
    }
}

fn project_root() -> Result<std::path::PathBuf, ZpmError> {
    let cwd = std::env::current_dir()?;
    project_config::find_project_root(&cwd)
}

fn load_config(root: &std::path::Path) -> Result<project_config::ProjectConfig, ZpmError> {
    project_config::load_project_config(&root.join(project_config::PROJECT_FILE))
}

fn init() -> Result<(), ZpmError> {
    let root = project_root()?;
    let cfg = load_config(&root)?;
    let mut s = store::open_store_for(&cfg.target, &root)?;
    let tables = s.list_tables()?;
    let desc = match &cfg.target {
        project_config::Target::Sqlite(f) => {
            format!("sqlite: {}", root.join(&f.file).display())
        }
        project_config::Target::Postgres(c) => {
            format!("postgres: {}/{}", c.host, c.database)
        }
    };
    println!("目标库就绪：{desc}（{} 张表）", tables.len());
    Ok(())
}

fn seed(file: &PathBuf) -> Result<(), ZpmError> {
    let root = project_root()?;
    let cfg = load_config(&root)?;
    let mut s = store::open_store_for(&cfg.target, &root)?;
    let sql = std::fs::read_to_string(file)?;
    // 规格 §14：按 `;` 切分逐条、事务包裹（任一条失败回滚整体），错误带语句序号
    let stmts: Vec<String> =
        sql.split(';').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    s.execute_script(&stmts)?;
    println!("seed 完成：{} 条语句（{}）", stmts.len(), file.display());
    Ok(())
}

fn export(file: Option<&Path>) -> Result<(), ZpmError> {
    let root = project_root()?;
    let cfg = load_config(&root)?;
    let mut s = store::open_store_for(&cfg.target, &root)?;
    let snap = snapshot::export(s.as_mut())?;
    // 路径约定：显式路径按 cwd（CLI 惯例）；无参默认落在项目根
    let cwd = std::env::current_dir()?;
    let out = file
        .map(|p| if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) })
        .unwrap_or_else(|| root.join("zpm-snapshot.json"));
    std::fs::write(&out, serde_json::to_string_pretty(&snap)?)?;
    let count = snap["tables"].as_array().map(|a| a.len()).unwrap_or(0);
    println!("已导出快照（{} 张表）：{}", count, out.display());
    Ok(())
}

fn import(file: &PathBuf) -> Result<(), ZpmError> {
    let root = project_root()?;
    let cfg = load_config(&root)?;
    let mut s = store::open_store_for(&cfg.target, &root)?;
    let txt = std::fs::read_to_string(file)?;
    let snap: serde_json::Value = serde_json::from_str(&txt)
        .map_err(|e| ZpmError::Format(format!("快照 JSON 解析失败：{e}")))?;
    let reports = snapshot::import(s.as_mut(), &snap)?;
    for r in &reports {
        println!("synced {}: {} rows", r.table, r.rows);
    }
    Ok(())
}

/// 数据同步（源库 → 目标库，shadow-rename 全量替换；接管原 `zpm sync`）。
/// root 推导：--config 显式指定时取配置文件所在目录（配置内 sqlite 相对路径按它解析）；
/// 否则从当前目录向上查找项目根。
fn sync(
    config: Option<&Path>,
    tables: Option<&str>,
    from: Option<&Path>,
) -> Result<(), ZpmError> {
    let (root, cfg) = match config {
        Some(p) => {
            let cfg = project_config::load_project_config(p)?;
            let root = p
                .canonicalize()?
                .parent()
                .expect("配置文件必有父目录")
                .to_path_buf();
            (root, cfg)
        }
        None => {
            let cwd = std::env::current_dir()?;
            let root = project_config::find_project_root(&cwd)?;
            let cfg = project_config::load_project_config(&root.join(project_config::PROJECT_FILE))?;
            (root, cfg)
        }
    };
    if cfg.sync.tables.is_empty() {
        println!("sync.tables 为空，无表可同步。");
        return Ok(());
    }

    // 表子集：逗号分隔的名字须都在配置里声明
    let specs: Vec<project_config::TableSpec> = match tables {
        None => cfg.sync.tables.clone(),
        Some(s) => {
            let mut picked = Vec::new();
            for w in s.split(',').map(str::trim).filter(|x| !x.is_empty()) {
                match cfg.sync.tables.iter().find(|t| t.name == w) {
                    Some(t) => picked.push(t.clone()),
                    None => {
                        return Err(ZpmError::NotFound(format!(
                            "表未在配置中声明：{w}"
                        )))
                    }
                }
            }
            picked
        }
    };

    let mut source: Box<dyn dbsync::Source> = match from {
        Some(f) => Box::new(dbsync::SqliteSource::open(f)?),
        None => dbsync::open_source(&cfg.datasource, &root)?,
    };
    let mut target = store::open_store_for(&cfg.target, &root)?;
    let reports = dbsync::sync_tables(source.as_mut(), target.as_mut(), &specs)?;
    for r in &reports {
        println!("synced {}: {} rows", r.table, r.rows);
    }
    Ok(())
}
