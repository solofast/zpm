//! zpm 命令行入口：clap 4 derive，模仿 tos-cli 的 report() 退出码模式。
use std::process::ExitCode;
use clap::{Parser, Subcommand};
use zpm_core::ZpmError;

mod cmd_config;
mod cmd_db;
mod cmd_framework;
mod cmd_project;
mod cmd_repo;
mod cmd_serve;
mod cmd_status;
mod cmd_version;

#[derive(Parser)]
#[command(name = "zpm", about = "zpm — 原型工具链（仓库/框架/项目/数据同步/原型服务器）", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// 管理 zpm 仓库（create/list/switch/delete/sync）
    Repo(cmd_repo::RepoArgs),
    /// 管理 zpm 仓库中的框架（list/load/delete/create/pull/push/knowledge）
    Framework(cmd_framework::FrameworkArgs),
    /// 项目管理（从激活仓库的框架创建/删除）
    Project(cmd_project::ProjectArgs),
    /// 目标库初始化/seed/快照导入导出/数据同步
    Db(cmd_db::DbArgs),
    /// 原型服务器（静态托管 + 通用 CRUD API）
    Serve(cmd_serve::ServeArgs),
    /// 查看/设置全局配置
    Config(cmd_config::ConfigArgs),
    /// 查看当前项目状态
    Status,
    /// 查看版本
    Version,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => {
            println!("zpm — 项目脚手架与 opencode 资产同步工具\n\n运行 'zpm --help' 查看用法。");
            ExitCode::SUCCESS
        }
        Some(Commands::Repo(args)) => report(cmd_repo::run(args)),
        Some(Commands::Framework(args)) => report(cmd_framework::run(args)),
        Some(Commands::Project(args)) => report(cmd_project::run(args)),
        Some(Commands::Db(args)) => report(cmd_db::run(args)),
        Some(Commands::Serve(args)) => {
            // serve 是长驻服务：独立 tokio runtime 驱动，Ctrl-C 优雅关闭
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime 初始化失败");
            let result = runtime.block_on(cmd_serve::run(args));
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            report(result)
        }
        Some(Commands::Config(args)) => report(cmd_config::run(args)),
        Some(Commands::Status) => report(cmd_status::run()),
        Some(Commands::Version) => {
            cmd_version::run();
            ExitCode::SUCCESS
        }
    }
}

/// 把 Result 映射为进程退出码：错误输出到 stderr，格式 `Error: <msg>`。
fn report(r: Result<(), ZpmError>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::from(e.exit_code() as u8)
        }
    }
}
