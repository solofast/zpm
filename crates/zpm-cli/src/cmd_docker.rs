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
        /// 强制重新下载 release 二进制（--binary 本就无条件重新落位，与此参数无关）
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
    // down 不依赖项目配置：配置文件损坏时也应能停容器
    if let DockerCommands::Down = args.command {
        return down(&root);
    }
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
        DockerCommands::Down => Ok(()), // 上方 if 已处理，此处仅为穷尽性
    }
}

fn init(
    root: &Path,
    cfg: &ProjectConfig,
    force: bool,
    platform_cli: Option<&str>,
) -> Result<(), ZpmError> {
    let platform = docker::resolve_platform(platform_cli, &cfg.docker)?;
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
    let platform = docker::resolve_platform(platform_cli, &cfg.docker)?;
    let image = docker::effective_image(cfg);
    let cfg_tag = docker::effective_tag(&cfg.docker);
    let tag = tag_cli.map(str::to_string).unwrap_or_else(|| cfg_tag.clone());
    let target = platform.unwrap_or_else(docker::Platform::host_native).rust_target().to_string();
    // build 前预检先快速失败：避免白下载/白落位后才发现 Dockerfile 平台不匹配
    docker::check_dockerfile_target(root, &target)?;
    docker::validate_tag(&tag)?;
    // tag 漂移提醒：--tag 构建的镜像 tag 与 compose 渲染的不一致时，up 用的是 compose 的 tag，
    // compose 的 build: . 可能拿本次落位归档重建出 tag/架构混淆的镜像
    if let Some(w) = docker::tag_drift_warning(&tag, &cfg_tag) {
        println!("警告：{w}");
    }
    match binary {
        Some(p) => docker::stage_binary(root, p, &target)?,
        None => docker::download_release_binary(root, &cfg.docker, &target, force)?,
    };
    docker::build_image(root, &image, &tag, platform.map(|p| p.docker_name()))?;
    // 平台漂移警告（规格决定 #9）：与磁盘上 docker-compose.yml 的 platform 行比对，不自动回写生成物
    if let Some(p) = platform {
        if let Some(w) = docker::platform_drift_warning(root, p) {
            println!("警告：{w}");
        }
    }
    println!("镜像已构建：{image}:{tag}");
    println!("下一步：zpm docker up");
    Ok(())
}

fn up(root: &Path, cfg: &ProjectConfig) -> Result<(), ZpmError> {
    // compose 缺失时中文早退，而不是等 docker compose up 裸报错
    if !root.join("docker-compose.yml").is_file() {
        return Err(ZpmError::NotFound(
            "未找到 docker-compose.yml（先运行 zpm docker init）".into(),
        ));
    }
    for w in docker::drift_warnings(root, cfg) {
        println!("警告：{w}");
    }
    docker::compose_up(root)?;
    // 访问地址端口以磁盘上的 compose 为准（规格决定 #10）
    let port = docker::compose_host_port_from_disk(root)
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
