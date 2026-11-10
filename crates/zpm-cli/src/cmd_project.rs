//! `zpm project`：项目创建（从激活仓库的框架初始化）与删除（物理删除 + 入库移除）。
//!
//! 流程（沿 init 规格 §7.1）：防覆盖 → name → mkdir → 激活仓库解析 framework →
//! 变量合成（CLI > manifest.variables > DEFAULTS；name 在 variables 后、CLI 前）→
//! copy_tree → knowledge 基线清单 → mcp 注入 → codegraph init（best-effort）→
//! 入库登记 → 摘要。
use clap::{Args, Subcommand};
use std::collections::BTreeMap;
use std::path::PathBuf;
use zpm_core::error::ZpmError;
use zpm_core::framework::{self, DEFAULT_VARS};
use zpm_core::knowledge;
use zpm_core::mcp;
use zpm_core::project_config;
use zpm_meta::project::NewProject;

#[derive(Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommands,
}

#[derive(Subcommand)]
pub enum ProjectCommands {
    /// 从激活仓库的框架创建项目（目录结构 + zpm.config.yaml + 入库登记）
    Create {
        /// 目标目录（默认当前目录）
        path: Option<PathBuf>,
        /// 来源框架（须已在激活仓库的 frameworks 中）
        #[arg(long)]
        framework: String,
        /// 项目名（默认取目录名）
        #[arg(long)]
        name: Option<String>,
        /// 跳过 opencode mcp 注入
        #[arg(long)]
        no_mcp: bool,
        /// 只注入这些 mcp 能力（逗号分隔；默认 codegraph,dbx,playwright）
        #[arg(long, value_delimiter = ',')]
        mcp: Vec<String>,
        /// 模板变量：服务端口（覆盖 DEFAULTS/manifest.variables）
        #[arg(long)]
        port: Option<u16>,
        /// 模板变量：数据源地址（sqlite 数据源无 host 概念；server.host 走 schema 默认）
        #[arg(long)]
        host: Option<String>,
        /// 模板变量：数据源端口
        #[arg(long = "dbport")]
        dbport: Option<u16>,
        /// 模板变量：数据库名
        #[arg(long)]
        database: Option<String>,
        /// 模板变量：数据库用户
        #[arg(long)]
        user: Option<String>,
        /// 模板变量：数据库密码
        #[arg(long)]
        password: Option<String>,
    },
    /// 删除已登记项目（物理删除目录 + 移除登记记录）
    Delete {
        /// 项目名字或绝对路径（须已登记）
        name_or_path: String,
    },
}

pub fn run(args: ProjectArgs) -> Result<(), ZpmError> {
    match args.command {
        ProjectCommands::Create {
            path,
            framework,
            name,
            no_mcp,
            mcp,
            port,
            host,
            dbport,
            database,
            user,
            password,
        } => create(CreateArgs {
            path,
            framework,
            name,
            no_mcp,
            mcp,
            port,
            host,
            dbport,
            database,
            user,
            password,
        }),
        ProjectCommands::Delete { name_or_path } => delete(&name_or_path),
    }
}

#[derive(Debug)]
struct CreateArgs {
    path: Option<PathBuf>,
    framework: String,
    name: Option<String>,
    no_mcp: bool,
    mcp: Vec<String>,
    port: Option<u16>,
    host: Option<String>,
    dbport: Option<u16>,
    database: Option<String>,
    user: Option<String>,
    password: Option<String>,
}

fn create(args: CreateArgs) -> Result<(), ZpmError> {
    // ① 防覆盖
    let path = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
    if !path.exists() {
        std::fs::create_dir_all(&path)?;
    }
    let root = path.canonicalize()?;
    let cfg_path = root.join(project_config::PROJECT_FILE);
    if cfg_path.exists() {
        return Err(ZpmError::AlreadyExists(project_config::PROJECT_FILE.into()));
    }

    // ② name（CLI 显式 > 目录名）
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| root.file_name().unwrap_or_default().to_string_lossy().into_owned());

    // ③ 目录结构
    for d in ["pages", "data", ".zpm", "knowledge/rules", "knowledge/lessons", "knowledge/assets"] {
        std::fs::create_dir_all(root.join(d))?;
    }

    // ④ 激活仓库解析框架（--framework 必填；NotFound 自带"可用内置"清单）
    let (conn, repo) = crate::cmd_repo::active_repo()?;
    let frameworks_dir = repo.path.join("frameworks");
    let fw_dir = framework::resolve_in(&frameworks_dir, &args.framework)?;
    let manifest = framework::load_manifest(&fw_dir)?;

    // ⑤ 变量合成（CLI 显式 > manifest.variables > DEFAULTS；name 在 variables 后、CLI 前）
    let mut vars: BTreeMap<String, String> =
        DEFAULT_VARS.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    for (k, v) in &manifest.variables {
        vars.insert(k.clone(), v.clone());
    }
    vars.insert("name".into(), name.clone());
    apply_cli_overrides(&mut vars, &args);

    // ⑤ 整树渲染拷贝（knowledge/ 一并拷入，随后由 seed_manifest 建清单）
    let stats = framework::copy_tree(&fw_dir, &root, &vars)?;
    let summary_framework = format!("{} v{}", manifest.name, manifest.version);
    let copied = stats.files;
    let warn_count = stats.warnings.len();

    // 知识清单：由拷入后的 knowledge/ 目录生成（含框架基线；空目录则空清单）
    let knowledge_count = knowledge::seed_manifest(&root)?;

    // ⑥ opencode 能力注入（探测本机 opencode 版本格式）
    let mut injected: Vec<String> = Vec::new();
    if !args.no_mcp {
        let fmt = mcp::detect_format();
        let names: Vec<String> = if args.mcp.is_empty() {
            mcp::PRESETS.iter().map(|(n, _)| n.to_string()).collect()
        } else {
            args.mcp.clone()
        };
        std::fs::create_dir_all(root.join(".opencode"))?;
        let json_path = root.join(".opencode").join("opencode.json");
        let n = mcp::inject(&json_path, fmt, &names.iter().map(String::as_str).collect::<Vec<_>>())?;
        injected = names;
        let _ = n;
    }

    // ⑦ codegraph init（best-effort：spawn 失败或非零退出都只提示，不阻断）
    match std::process::Command::new("codegraph").arg("init").current_dir(&root).output() {
        Ok(out) if out.status.success() => println!("codegraph 索引已建立。"),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            println!(
                "提示：codegraph init 运行失败（{}）。可选重跑 `codegraph init` 建立代码索引。",
                stderr.trim()
            );
        }
        Err(_) => println!(
            "提示：未检测到 codegraph CLI。可选安装后在本目录重跑 `codegraph init` 建立代码索引。"
        ),
    }

    // ⑧ 入库登记
    let record = zpm_meta::project::insert(
        &conn,
        &NewProject { name: &name, path: &root, framework: &args.framework, repo: &repo.name },
    )?;
    let _ = record;

    // ⑨ 摘要
    println!("已创建项目：{}", root.display());
    println!("配置文件：{}", cfg_path.display());
    println!("框架：{summary_framework}（{copied} 个文件{}）", if warn_count > 0 { format!("，{warn_count} 条警告") } else { String::new() });
    println!("知识条目：{knowledge_count} 个（knowledge/rules|lessons|assets）");
    if injected.is_empty() {
        println!("opencode mcp：未注入（--no-mcp 或空集）");
    } else {
        println!("opencode mcp：{}", injected.join(", "));
    }
    println!("已登记项目 {name}（framework={}，repo={}）", args.framework, repo.name);
    println!("下一步：cd {} && zpm db init && zpm serve", path.display());
    Ok(())
}

fn delete(name_or_path: &str) -> Result<(), ZpmError> {
    let (conn, _home) = crate::cmd_repo::open_meta()?;
    let record = zpm_meta::project::find(&conn, name_or_path)?
        .ok_or_else(|| ZpmError::ProjectNotFound(format!("{name_or_path} 未登记")))?;
    // 安全阀：拒绝删除当前工作目录（双侧 canonicalize 比对，符号链接环境同样成立）
    let cwd = std::env::current_dir()?.canonicalize()?;
    if record.path == cwd {
        return Err(ZpmError::Dangerous("拒绝删除当前工作目录".into()));
    }
    std::fs::remove_dir_all(&record.path)?;
    zpm_meta::project::delete(&conn, record.id)?;
    println!("已删除项目 {}", record.name);
    Ok(())
}

fn apply_cli_overrides(vars: &mut BTreeMap<String, String>, args: &CreateArgs) {
    if let Some(p) = args.port {
        vars.insert("port".into(), p.to_string());
    }
    if let Some(h) = args.host.clone() {
        vars.insert("host".into(), h);
    }
    if let Some(d) = args.dbport {
        vars.insert("dbport".into(), d.to_string());
    }
    if let Some(d) = args.database.clone() {
        vars.insert("database".into(), d);
    }
    if let Some(u) = args.user.clone() {
        vars.insert("user".into(), u);
    }
    if let Some(p) = args.password.clone() {
        vars.insert("password".into(), p);
    }
}
