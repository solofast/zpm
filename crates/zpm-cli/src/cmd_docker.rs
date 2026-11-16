//! `zpm docker`：容器化交付（init 生成三件套 / build 构建镜像 / up / down）。
use clap::{Args, Subcommand};
use std::path::PathBuf;
use zpm_core::docker;
use zpm_core::error::ZpmError;
use zpm_core::project_config::{self, ProjectConfig};
use std::path::Path;

#[derive(Args)]
pub struct DockerArgs {
    #[command(subcommand)]
    pub command: DockerCommands,
}

#[derive(Subcommand)]
pub enum DockerCommands {
    /// 生成 Dockerfile / docker-compose.yml / .dockerignore（薄镜像 + 项目文件挂载）
    Init {
        /// 覆盖已存在的生成文件
        #[arg(long)]
        force: bool,
        /// 生成物的目标平台（linux/amd64 | linux/arm64），缺省取配置或宿主原生
        #[arg(long)]
        platform: Option<String>,
    },
    /// 准备 zpm 二进制并构建镜像（--binary 或 GitHub Release 下载）
    Build {
        /// 构建平台（linux/amd64 | linux/arm64）；缺省取配置或宿主原生
        #[arg(long)]
        platform: Option<String>,
        /// 覆盖镜像 tag
        #[arg(long)]
        tag: Option<String>,
        /// 使用本地二进制/tar.gz（跳过 release 下载）
        #[arg(long)]
        binary: Option<PathBuf>,
        /// 重新下载/重新落位二进制
        #[arg(long)]
        force: bool,
    },
    /// docker compose up -d 启动原型服务
    Up,
    /// docker compose down 停止
    Down,
}

pub fn run(args: DockerArgs) -> Result<(), ZpmError> {
    let cwd = std::env::current_dir()?;
    let root = project_config::find_project_root(&cwd)?;
    let cfg =
        project_config::load_project_config(&root.join(project_config::PROJECT_FILE))?;
    match args.command {
        DockerCommands::Init { force, platform } => {
            init(&root, &cfg, force, platform.as_deref())
        }
        DockerCommands::Build { platform, tag, binary, force } => {
            build(&root, &cfg, platform.as_deref(), tag.as_deref(), binary.as_deref(), force)
        }
        DockerCommands::Up => up(&root, &cfg),
        DockerCommands::Down => down(&root),
    }
}

/// 平台解析：CLI 显式 > 配置 > None（宿主原生）
fn resolve_platform(
    cli: Option<&str>,
    cfg: &project_config::DockerCfg,
) -> Result<Option<docker::Platform>, ZpmError> {
    match cli {
        Some(s) => Ok(Some(docker::Platform::parse(s)?)),
        None => match &cfg.platform {
            Some(s) => Ok(Some(docker::Platform::parse(s)?)),
            None => Ok(None),
        },
    }
}

fn init(
    root: &Path,
    cfg: &ProjectConfig,
    force: bool,
    platform_cli: Option<&str>,
) -> Result<(), ZpmError> {
    let platform = resolve_platform(platform_cli, &cfg.docker)?;
    docker::generate_files(root, cfg, platform, force).map_err(|e| {
        if matches!(e, ZpmError::AlreadyExists(_)) {
            eprintln!("提示：加 --force 覆盖已有文件。");
        }
        e
    })?;
    println!("已生成 Dockerfile / docker-compose.yml / .dockerignore");
    println!("下一步：zpm docker build && zpm docker up");
    Ok(())
}

fn build(
    root: &Path,
    cfg: &ProjectConfig,
    platform_cli: Option<&str>,
    tag_cli: Option<&str>,
    binary: Option<&Path>,
    force: bool,
) -> Result<(), ZpmError> {
    docker::ensure_available("docker")?;
    let platform = resolve_platform(platform_cli, &cfg.docker)?;
    let image = docker::effective_image(cfg);
    let tag =
        tag_cli.map(str::to_string).unwrap_or_else(|| docker::effective_tag(&cfg.docker));
    let target = docker::effective_rust_target(
        &cfg.docker,
        platform.unwrap_or_else(docker::Platform::host_native),
    );
    match binary {
        Some(p) => docker::stage_binary(root, p, &target)?,
        None => docker::download_release_binary(root, &cfg.docker, &target, force)?,
    };
    // build 前预检：Dockerfile 的 COPY 目标须与本次 target 一致，否则 docker 只会裸报 COPY 失败
    docker::check_dockerfile_target(root, &target)?;
    docker::build_image(root, &image, &tag, platform.map(|p| p.docker_name()))?;
    // 平台漂移警告（规格决定 #9）：与磁盘上 docker-compose.yml 的 platform 行比对，不自动回写生成物
    if let Some(p) = platform_cli {
        let disk_platform = std::fs::read_to_string(root.join("docker-compose.yml"))
            .ok()
            .and_then(|t| docker::compose_platform(&t));
        if let Some(disk_p) = disk_platform {
            if p != disk_p.as_str() {
                println!("警告：--platform {p} 与 docker-compose.yml 的 platform（{disk_p}）不一致。");
                println!("请运行 zpm docker init --platform {p} --force 重新生成 compose。");
            }
        }
    }
    println!("镜像已构建：{image}:{tag}");
    println!("下一步：zpm docker up");
    Ok(())
}

fn up(root: &Path, cfg: &ProjectConfig) -> Result<(), ZpmError> {
    for w in docker::drift_warnings(root, cfg) {
        println!("警告：{w}");
    }
    docker::compose_up(root)?;
    // 访问地址端口以磁盘上的 compose 为准（规格决定 #10）
    let port = std::fs::read_to_string(root.join("docker-compose.yml"))
        .ok()
        .and_then(|t| docker::compose_port(&t))
        .map(|p| p.to_string())
        .unwrap_or_else(|| "<未知>".into());
    println!("已启动：http://localhost:{port}");
    Ok(())
}

fn down(root: &Path) -> Result<(), ZpmError> {
    docker::compose_down(root)?;
    println!("已停止。");
    Ok(())
}
