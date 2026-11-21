//! zpm 项目容器化交付：`zpm docker init|build|up|down` 的核心逻辑。
//!
//! 职责：平台映射与镜像名安全化（纯函数）、三件套模板渲染与生成、
//! release 二进制下载/`--binary` 落位、docker/compose shell-out、漂移检查。
//! shell-out 失败时错误携带 stderr（沿 git 传输层约定）。

use crate::error::ZpmError;
use crate::project_config::DockerCfg;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Release 下载默认仓库（用户确认：upstream）
pub const RELEASE_REPO: &str = "solofast/zpm";
/// Release 资产版本（release tag = v<workspace 版本>）
pub const RELEASE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 交付平台
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Amd64,
    Arm64,
}

impl Platform {
    /// 解析 `linux/amd64` | `linux/arm64`
    pub fn parse(s: &str) -> Result<Self, ZpmError> {
        match s {
            "linux/amd64" => Ok(Self::Amd64),
            "linux/arm64" => Ok(Self::Arm64),
            other => Err(ZpmError::Format(format!(
                "docker.platform 不支持：{other:?}（期望 linux/amd64|linux/arm64）"
            ))),
        }
    }

    /// `docker build --platform` 取值
    pub fn docker_name(self) -> &'static str {
        match self {
            Self::Amd64 => "linux/amd64",
            Self::Arm64 => "linux/arm64",
        }
    }

    /// release 产物的 musl target 名
    pub fn rust_target(self) -> &'static str {
        match self {
            Self::Amd64 => "x86_64-unknown-linux-musl",
            Self::Arm64 => "aarch64-unknown-linux-musl",
        }
    }

    /// 宿主原生平台（按编译目标架构推断）
    pub fn host_native() -> Self {
        if cfg!(target_arch = "aarch64") { Self::Arm64 } else { Self::Amd64 }
    }
}

/// 生效 rust target：cfg.rust_target 覆盖 > platform 映射
pub fn effective_rust_target(cfg: &DockerCfg, platform: Platform) -> String {
    cfg.rust_target.clone().unwrap_or_else(|| platform.rust_target().to_string())
}

/// 镜像名安全化：取 name 末段（路径名 → 目录名），小写化，非字母数字一律归一为 '-'，
/// 折叠分隔符连跑为单个 '-'（docker 镜像名不允许相邻分隔段：a..b、a._b、a.-b 均非法），
/// 去掉首尾分隔符；空则回落 zpm-app。输出必然是合法 docker 镜像名。
pub fn sanitize_image_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mapped: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let mut collapsed = String::with_capacity(mapped.len());
    let mut prev_sep = false;
    for c in mapped.chars() {
        if c == '-' {
            if !prev_sep {
                collapsed.push('-');
            }
            prev_sep = true;
        } else {
            collapsed.push(c);
            prev_sep = false;
        }
    }
    let trimmed = collapsed.trim_matches('-');
    if trimmed.is_empty() { "zpm-app".to_string() } else { trimmed.to_string() }
}

/// 生效镜像名：cfg.image > name 末段，两者都经安全化
/// （配置节约定：image 安全化后非空；显式值同样可能是 "My CRM!" 这类非法名）
pub fn effective_image_of(name: &str, cfg: &DockerCfg) -> String {
    cfg.image
        .clone()
        .map(|img| sanitize_image_name(&img))
        .unwrap_or_else(|| sanitize_image_name(name))
}

/// 生效 tag：cfg.tag > latest
pub fn effective_tag(cfg: &DockerCfg) -> String {
    cfg.tag.clone().unwrap_or_else(|| "latest".to_string())
}

/// Release 资产 URL；release_mirror 有值时拼前缀（尾斜杠去重）
pub fn release_url(cfg: &DockerCfg, target: &str) -> String {
    let base = format!(
        "https://github.com/{RELEASE_REPO}/releases/download/v{RELEASE_VERSION}/zpm-{target}.tar.gz"
    );
    match cfg.release_mirror.as_deref() {
        Some(m) if !m.trim().is_empty() => {
            format!("{}/{}", m.trim().trim_end_matches('/'), base)
        }
        _ => base,
    }
}

/// 生效镜像名（ProjectConfig 版）
pub fn effective_image(cfg: &crate::project_config::ProjectConfig) -> String {
    effective_image_of(&cfg.name, &cfg.docker)
}

/// 三件套渲染上下文
struct RenderCtx {
    image: String,
    tag: String,
    port: u16,
    rust_target: String,
    /// Some(平台名) 时 compose 渲染 platform 行
    compose_platform: Option<String>,
}

/// 模板渲染：替换 `{{key}}`（仅 vars 中存在的键；未知占位保留原样）
fn render(tpl: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = tpl.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{{{k}}}}}"), v);
    }
    out
}

fn platform_line(p: &Option<String>) -> String {
    match p {
        Some(name) => format!("    platform: {name}\n"),
        None => String::new(),
    }
}

fn render_ctx(cfg: &crate::project_config::ProjectConfig, platform: Option<Platform>) -> RenderCtx {
    RenderCtx {
        image: effective_image(cfg),
        tag: effective_tag(&cfg.docker),
        port: cfg.server.port,
        rust_target: effective_rust_target(
            &cfg.docker,
            platform.unwrap_or_else(Platform::host_native),
        ),
        compose_platform: platform
            .map(|p| p.docker_name().to_string())
            .or_else(|| cfg.docker.platform.clone()),
    }
}

fn render_dockerfile(ctx: &RenderCtx) -> String {
    render(
        include_str!("../assets/docker/Dockerfile.tmpl"),
        &BTreeMap::from([
            ("target".to_string(), ctx.rust_target.clone()),
            ("port".to_string(), ctx.port.to_string()),
        ]),
    )
}

fn render_compose(ctx: &RenderCtx) -> String {
    render(
        include_str!("../assets/docker/docker-compose.yml.tmpl"),
        &BTreeMap::from([
            ("image".to_string(), ctx.image.clone()),
            ("tag".to_string(), ctx.tag.clone()),
            ("port".to_string(), ctx.port.to_string()),
            ("platform_line".to_string(), platform_line(&ctx.compose_platform)),
        ]),
    )
}

/// 生成的三件套文件名
pub const GENERATED_FILES: [&str; 3] = ["Dockerfile", "docker-compose.yml", ".dockerignore"];

/// 在项目根生成三件套；任一已存在且 !force → AlreadyExists（payload 列出全部已存在文件）
pub fn generate_files(
    root: &Path,
    cfg: &crate::project_config::ProjectConfig,
    platform_override: Option<Platform>,
    force: bool,
) -> Result<(), ZpmError> {
    let ctx = render_ctx(cfg, platform_override);
    let existing: Vec<&str> =
        GENERATED_FILES.iter().copied().filter(|f| root.join(f).exists()).collect();
    if !existing.is_empty() && !force {
        return Err(ZpmError::AlreadyExists(format!("docker 配置 {}", existing.join("、"))));
    }
    std::fs::write(root.join("Dockerfile"), render_dockerfile(&ctx))?;
    std::fs::write(root.join("docker-compose.yml"), render_compose(&ctx))?;
    std::fs::write(
        root.join(".dockerignore"),
        include_str!("../assets/docker/dockerignore.tmpl"),
    )?;
    Ok(())
}

/// 从生成的 compose 文本解析宿主端口（flow 风格 `ports: ["P:P"]` → P）
pub fn compose_port(compose_txt: &str) -> Option<u16> {
    let line = compose_txt.lines().find(|l| l.trim_start().starts_with("ports:"))?;
    let inner = line.split('"').nth(1)?;
    let (_, host) = inner.split_once(':')?;
    host.trim().parse().ok()
}

/// 从 compose 文本解析 platform 行（`platform: linux/arm64` → 值）；无该行 → None
pub fn compose_platform(compose_txt: &str) -> Option<String> {
    let line = compose_txt.lines().find(|l| l.trim_start().starts_with("platform:"))?;
    let (_, val) = line.split_once(':')?;
    let v = val.trim();
    if v.is_empty() { None } else { Some(v.to_string()) }
}

/// 平台解析：CLI 显式 > 配置 `docker.platform` > None（宿主原生）
pub fn resolve_platform(
    cli: Option<&str>,
    cfg: &DockerCfg,
) -> Result<Option<Platform>, ZpmError> {
    match cli {
        Some(s) => Ok(Some(Platform::parse(s)?)),
        None => match &cfg.platform {
            Some(s) => Ok(Some(Platform::parse(s)?)),
            None => Ok(None),
        },
    }
}

/// build 平台漂移警告（规格决定 #9）：与磁盘上 compose 的 platform 行比对，
/// 不一致时返回警告文案（含重新生成指引）；一致、无 platform 行或文件缺失 → None
pub fn platform_drift_warning(root: &Path, platform: Platform) -> Option<String> {
    let disk = std::fs::read_to_string(root.join("docker-compose.yml")).ok()?;
    let disk_platform = compose_platform(&disk)?;
    if disk_platform == platform.docker_name() {
        return None;
    }
    Some(format!(
        "--platform {} 与 docker-compose.yml 的 platform（{disk_platform}）不一致；请运行 zpm docker init --platform {} --force 重新生成 compose",
        platform.docker_name(),
        platform.docker_name()
    ))
}

/// up 访问端口（规格决定 #10）：以磁盘上的 compose 文件为准；文件缺失或解析失败 → None
pub fn compose_host_port_from_disk(root: &Path) -> Option<u16> {
    compose_port(&std::fs::read_to_string(root.join("docker-compose.yml")).ok()?)
}

/// build 前预检：磁盘 Dockerfile 必须存在，且 COPY 目标与本次构建 target 一致
/// （否则落位的归档名与 COPY 对不上，docker 只会裸报 COPY 失败）。
pub fn check_dockerfile_target(root: &Path, target: &str) -> Result<(), ZpmError> {
    let txt = std::fs::read_to_string(root.join("Dockerfile")).map_err(|_| {
        ZpmError::NotFound("未找到 Dockerfile（先运行 zpm docker init）".to_string())
    })?;
    let copy_line = format!("COPY zpm-{target}.tar.gz");
    if txt.lines().any(|l| l.contains(&copy_line)) {
        return Ok(());
    }
    let platform = match target {
        "aarch64-unknown-linux-musl" => "linux/arm64",
        "x86_64-unknown-linux-musl" => "linux/amd64",
        _ => "<platform>",
    };
    Err(ZpmError::Format(format!(
        "Dockerfile 的 COPY 目标与本次构建 target（{target}）不一致；请运行 zpm docker init --platform {platform} --force 重新生成 Dockerfile"
    )))
}

/// 漂移检查：按当前配置重渲染 compose，提取 `ports:`/`image:`/`platform:`
/// 三类关键行与磁盘比对（规格决定 #10：只看关键行，手改注释等不告警）；
/// 任一关键行不在磁盘文本中 → 单条警告。
pub fn drift_warnings(root: &Path, cfg: &crate::project_config::ProjectConfig) -> Vec<String> {
    let path = root.join("docker-compose.yml");
    let Ok(disk) = std::fs::read_to_string(&path) else {
        return vec!["未找到 docker-compose.yml（先运行 zpm docker init）".to_string()];
    };
    let rendered = render_compose(&render_ctx(cfg, None));
    let disk_lines: HashSet<&str> = disk.lines().map(str::trim).collect();
    let drifted = rendered
        .lines()
        .map(str::trim)
        .filter(|l| {
            l.starts_with("ports:") || l.starts_with("image:") || l.starts_with("platform:")
        })
        .any(|l| !disk_lines.contains(l));
    if drifted {
        vec!["docker-compose.yml 与当前 zpm.config.yaml 不一致（端口/镜像名/platform 可能改过）；重新生成：zpm docker init --force".to_string()]
    } else {
        vec![]
    }
}

/// `--binary` 落位：把用户提供的二进制/tar.gz 整理为项目根 `zpm-<target>.tar.gz`。
/// 裸二进制打包为根级单条目 `zpm` 的 tar.gz（与 release 产物布局一致）；
/// tar.gz 直接按名落位；dest 已存在也无条件重新落位
/// （--binary 显式提供时不得静默复用旧归档，否则镜像里可能是旧二进制）。
pub fn stage_binary(root: &Path, src: &Path, target: &str) -> Result<PathBuf, ZpmError> {
    let dest = root.join(format!("zpm-{target}.tar.gz"));
    // 先判存在性再 canonicalize：源缺失时 canonicalize 只给裸 io 错误，报错须带 --binary 语境
    if !src.is_file() {
        return Err(ZpmError::NotFound(format!(
            "--binary 不存在或不是文件：{}",
            src.display()
        )));
    }
    let src = src.canonicalize()?;
    let name = src.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
    if name.ends_with(".tar.gz") {
        std::fs::copy(&src, &dest)?;
        return Ok(dest);
    }
    // 裸二进制 → 打包
    let stage_dir = root.join(".zpm-docker-stage");
    std::fs::create_dir_all(&stage_dir)?;
    std::fs::copy(&src, stage_dir.join("zpm"))?;
    let out = Command::new("tar")
        .args(["czf"])
        .arg(&dest)
        .arg("-C")
        .arg(&stage_dir)
        .arg("zpm")
        .output();
    let _ = std::fs::remove_dir_all(&stage_dir);
    let out = out?;
    if !out.status.success() {
        return Err(ZpmError::TransportError(format!(
            "打包 --binary 失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(dest)
}

/// 从 GitHub Release 下载二进制到项目根 `zpm-<target>.tar.gz`（curl shell-out）。
/// 已存在且 !force → 复用。成功后尽力校验 `.sha256`（工具/网络不可得 → 警告跳过；
/// 摘要不匹配 → 报错并删除产物）。
pub fn download_release_binary(
    root: &Path,
    cfg: &DockerCfg,
    target: &str,
    force: bool,
) -> Result<PathBuf, ZpmError> {
    let dest = root.join(format!("zpm-{target}.tar.gz"));
    if dest.exists() && !force {
        println!("复用已有 zpm-{target}.tar.gz（--force 重新下载）");
        return Ok(dest);
    }
    let url = release_url(cfg, target);
    let out = Command::new("curl")
        .args(["-fSL", "--retry", "2", "-o"])
        .arg(&dest)
        .arg(&url)
        .output()
        .map_err(|_| {
            ZpmError::NotFound(
                "未检测到 curl：下载 release 二进制需要 curl，或用 --binary 提供本地文件".into(),
            )
        })?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&dest);
        return Err(ZpmError::TransportError(format!(
            "下载 release 二进制失败：{url}\n{}（提示：国内网络可配置 zpm.config.yaml 的 docker.release_mirror，或改用 --binary）",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    if let Err(e) = verify_sha256(root, &dest, &url) {
        let _ = std::fs::remove_file(&dest);
        return Err(e);
    }
    Ok(dest)
}

/// 尽力校验 `<url>.sha256`：获取/计算任一步不可行 → stderr 警告并跳过；不匹配 → 报错。
fn verify_sha256(root: &Path, dest: &Path, url: &str) -> Result<(), ZpmError> {
    let sum_file = root.join(".zpm-docker-download.sha256");
    let fetched = Command::new("curl")
        .args(["-fSL", "--retry", "1", "-o"])
        .arg(&sum_file)
        .arg(format!("{url}.sha256"))
        .output();
    let expected = match fetched {
        Ok(o) if o.status.success() => {
            let first = std::fs::read_to_string(&sum_file)
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            let _ = std::fs::remove_file(&sum_file);
            first
        }
        _ => {
            eprintln!("提示：未能获取 .sha256（跳过校验）");
            return Ok(());
        }
    };
    if expected.is_empty() {
        eprintln!("提示：.sha256 内容为空（跳过校验）");
        return Ok(());
    }
    // 本地摘要：sha256sum（linux）优先，shasum -a 256（mac）兜底
    let local = Command::new("sha256sum")
        .arg(dest)
        .output()
        .or_else(|_| Command::new("shasum").args(["-a", "256"]).arg(dest).output())?;
    if !local.status.success() {
        eprintln!("提示：本地摘要命令执行失败（跳过校验）");
        return Ok(());
    }
    let actual = String::from_utf8_lossy(&local.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    if actual.is_empty() {
        eprintln!("提示：无法计算本地摘要（跳过校验）");
        return Ok(());
    }
    if actual != expected {
        return Err(ZpmError::TransportError(format!(
            "release 二进制校验失败：期望 {expected}，实际 {actual}"
        )));
    }
    Ok(())
}

/// 纯函数：docker build 参数（不含程序名）
pub fn docker_build_args(image: &str, tag: &str, platform: Option<&str>) -> Vec<String> {
    let mut v = vec!["build".to_string()];
    if let Some(p) = platform {
        v.push("--platform".into());
        v.push(p.to_string());
    }
    v.push("-t".into());
    v.push(format!("{image}:{tag}"));
    v.push(".".into());
    v
}

/// 纯函数：docker compose 子命令参数（仅 up/down 两用，透传分支已删）
pub fn compose_args(action: &str) -> Vec<String> {
    match action {
        "up" => vec!["compose".into(), "up".into(), "-d".into()],
        _ => vec!["compose".into(), "down".into()],
    }
}

/// 探测 `<prog> version` 可用性；未安装/不可用给中文错误
pub fn ensure_available(prog: &str) -> Result<(), ZpmError> {
    match Command::new(prog).arg("version").output() {
        Err(_) => Err(ZpmError::NotFound(
            "未检测到 docker：请先安装并启动 Docker Desktop（或 linux docker 服务）".into(),
        )),
        Ok(out) if !out.status.success() => Err(ZpmError::Format(format!(
            "docker 不可用（daemon 未运行？）：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
        Ok(_) => Ok(()),
    }
}

/// shell-out 执行（沿 git 传输层约定：失败携带 stderr）
fn run_prog(prog: &str, args: &[String], cwd: &Path) -> Result<(), ZpmError> {
    let out = Command::new(prog).args(args).current_dir(cwd).output()?;
    if !out.status.success() {
        return Err(ZpmError::TransportError(format!(
            "docker {} 失败：{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// docker build（在项目根执行）
pub fn build_image(
    root: &Path,
    image: &str,
    tag: &str,
    platform: Option<&str>,
) -> Result<(), ZpmError> {
    ensure_available("docker")?;
    run_prog("docker", &docker_build_args(image, tag, platform), root)
}

/// 探测 docker compose 插件（up/down 都依赖）
fn ensure_compose_plugin() -> Result<(), ZpmError> {
    let probe = Command::new("docker").args(["compose", "version"]).output()?;
    if !probe.status.success() {
        return Err(ZpmError::NotFound(
            "docker compose 插件不可用：请升级 Docker Desktop 或安装 docker-compose-plugin".into(),
        ));
    }
    Ok(())
}

/// docker compose up -d（在项目根执行；先探测 compose 插件）
pub fn compose_up(root: &Path) -> Result<(), ZpmError> {
    ensure_available("docker")?;
    ensure_compose_plugin()?;
    run_prog("docker", &compose_args("up"), root)
}

/// docker compose down（在项目根执行；与 up 一致探测可用性与 compose 插件）
pub fn compose_down(root: &Path) -> Result<(), ZpmError> {
    ensure_available("docker")?;
    ensure_compose_plugin()?;
    run_prog("docker", &compose_args("down"), root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_config::DockerCfg;
    use crate::project_config::ProjectConfig;

    /// 最小合法项目配置（写临时文件加载；server.port = 4599）
    fn min_cfg() -> ProjectConfig {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("zpm.config.yaml");
        std::fs::write(
            &p,
            r#"
name: demo
server: { port: 4599 }
datasource: { type: sqlite, file: data/app.db }
target: { type: sqlite, file: data/app.db }
sync: { tables: [] }
"#,
        )
        .unwrap();
        crate::project_config::load_project_config(&p).unwrap()
    }

    #[test]
    fn platform_parse_and_mappings() {
        let p = Platform::parse("linux/arm64").unwrap();
        assert_eq!(p, Platform::Arm64);
        assert_eq!(p.docker_name(), "linux/arm64");
        assert_eq!(p.rust_target(), "aarch64-unknown-linux-musl");
        let p = Platform::parse("linux/amd64").unwrap();
        assert_eq!(p.docker_name(), "linux/amd64");
        assert_eq!(p.rust_target(), "x86_64-unknown-linux-musl");
        let err = Platform::parse("linux/riscv").unwrap_err();
        assert!(err.to_string().contains("docker.platform"), "实际错误：{err}");
    }

    #[test]
    fn host_native_matches_build_target_arch() {
        let expect = if cfg!(target_arch = "aarch64") { Platform::Arm64 } else { Platform::Amd64 };
        assert_eq!(Platform::host_native(), expect);
    }

    #[test]
    fn sanitize_image_name_rules() {
        // 路径名取末段（参考项目 name 是绝对路径）
        assert_eq!(sanitize_image_name("/Users/nutsjian/.rain/zpm-examples/zpm-new-crm"), "zpm-new-crm");
        assert_eq!(sanitize_image_name("demo"), "demo");
        assert_eq!(sanitize_image_name("My CRM!"), "my-crm");
        assert_eq!(sanitize_image_name("app."), "app");
        // 开头必须字母数字
        assert_eq!(sanitize_image_name("-app"), "app");
        assert_eq!(sanitize_image_name("!!!"), "zpm-app");
        assert_eq!(sanitize_image_name(""), "zpm-app");
    }

    #[test]
    fn effective_image_and_tag_prefer_config() {
        let mut cfg = DockerCfg::default();
        assert_eq!(effective_image_of("demo", &cfg), "demo");
        cfg.image = Some("my-img".into());
        cfg.tag = Some("v9".into());
        assert_eq!(effective_image_of("demo", &cfg), "my-img");
        assert_eq!(effective_tag(&cfg), "v9");
        cfg.tag = None;
        assert_eq!(effective_tag(&cfg), "latest");
        // 显式值也过安全化（配置节约定：image 安全化后非空）
        cfg.image = Some("My CRM!".into());
        assert_eq!(effective_image_of("demo", &cfg), "my-crm");
    }

    #[test]
    fn release_url_with_and_without_mirror() {
        let mut cfg = DockerCfg::default();
        let url = release_url(&cfg, "x86_64-unknown-linux-musl");
        assert_eq!(
            url,
            format!(
                "https://github.com/{RELEASE_REPO}/releases/download/v{RELEASE_VERSION}/zpm-x86_64-unknown-linux-musl.tar.gz"
            )
        );
        cfg.release_mirror = Some("https://gh-proxy.com/".into());
        let url = release_url(&cfg, "x86_64-unknown-linux-musl");
        assert!(url.starts_with("https://gh-proxy.com/https://github.com/"), "实际：{url}");
        // 尾斜杠去重
        cfg.release_mirror = Some("https://gh-proxy.com".into());
        assert!(release_url(&cfg, "t").starts_with("https://gh-proxy.com/https://"));
    }

    #[test]
    fn effective_rust_target_override() {
        let mut cfg = DockerCfg::default();
        assert_eq!(
            effective_rust_target(&cfg, Platform::Arm64),
            "aarch64-unknown-linux-musl"
        );
        cfg.rust_target = Some("x86_64-unknown-linux-musl".into());
        assert_eq!(
            effective_rust_target(&cfg, Platform::Arm64),
            "x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn render_substitutes_known_and_keeps_unknown() {
        let vars = BTreeMap::from([("port".to_string(), "4310".to_string())]);
        assert_eq!(render("a {{port}} b {{nope}}", &vars), "a 4310 b {{nope}}");
    }

    #[test]
    fn generate_files_writes_three_and_refuses_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = min_cfg();
        generate_files(dir.path(), &cfg, None, false).unwrap();
        let dockerfile = std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap();
        assert!(dockerfile.contains("FROM debian:bookworm-slim"), "实际：{dockerfile}");
        assert!(dockerfile.contains(&format!(
            "COPY zpm-{}.tar.gz",
            Platform::host_native().rust_target()
        ))); // 无平台覆盖时按宿主原生
        let compose = std::fs::read_to_string(dir.path().join("docker-compose.yml")).unwrap();
        assert!(compose.contains("image: demo:t1") || compose.contains("image: demo:latest"));
        assert!(compose.contains("\"4599:4599\""));
        assert!(std::fs::read_to_string(dir.path().join(".dockerignore")).is_ok());
        // 拒绝覆盖
        let err = generate_files(dir.path(), &cfg, None, false).unwrap_err();
        assert!(matches!(err, crate::error::ZpmError::AlreadyExists(_)), "实际：{err}");
        assert!(err.to_string().contains("Dockerfile"));
        // force 覆盖成功
        generate_files(dir.path(), &cfg, Some(Platform::Arm64), true).unwrap();
        let compose2 = std::fs::read_to_string(dir.path().join("docker-compose.yml")).unwrap();
        assert!(compose2.contains("platform: linux/arm64"), "实际：{compose2}");
    }

    #[test]
    fn compose_platform_line_comes_from_override_then_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = min_cfg();
        cfg.docker.platform = Some("linux/amd64".into());
        generate_files(dir.path(), &cfg, None, false).unwrap();
        let compose = std::fs::read_to_string(dir.path().join("docker-compose.yml")).unwrap();
        assert!(compose.contains("platform: linux/amd64"));
        // override 优先于配置
        generate_files(dir.path(), &cfg, Some(Platform::Arm64), true).unwrap();
        let compose = std::fs::read_to_string(dir.path().join("docker-compose.yml")).unwrap();
        assert!(compose.contains("platform: linux/arm64"));
    }

    #[test]
    fn compose_port_parses_host_port() {
        let txt = "services:\n  zpm:\n    ports: [\"4310:4310\"]\n";
        assert_eq!(compose_port(txt), Some(4310));
        assert_eq!(compose_port("no ports here"), None);
    }

    #[test]
    fn compose_platform_parses_line_or_none() {
        let txt = "services:\n  zpm:\n    platform: linux/arm64\n    ports: [\"1:1\"]\n";
        assert_eq!(compose_platform(txt), Some("linux/arm64".to_string()));
        assert_eq!(compose_platform("no platform here"), None);
    }

    #[test]
    fn check_dockerfile_target_matches_and_mismatches() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = min_cfg();
        generate_files(dir.path(), &cfg, None, false).unwrap();
        let target = Platform::host_native().rust_target();
        // 一致 → Ok
        check_dockerfile_target(dir.path(), target).unwrap();
        // 不一致 → Err 含「不一致」与指引
        let other = if target == "aarch64-unknown-linux-musl" {
            "x86_64-unknown-linux-musl"
        } else {
            "aarch64-unknown-linux-musl"
        };
        let err = check_dockerfile_target(dir.path(), other).unwrap_err();
        assert!(err.to_string().contains("不一致"), "实际：{err}");
        assert!(err.to_string().contains("zpm docker init --platform"), "实际：{err}");
    }

    #[test]
    fn check_dockerfile_target_missing_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_dockerfile_target(dir.path(), "x86_64-unknown-linux-musl").unwrap_err();
        assert!(err.to_string().contains("未找到 Dockerfile"), "实际：{err}");
    }

    #[test]
    fn drift_warnings_detect_config_change() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = min_cfg();
        generate_files(dir.path(), &cfg, None, false).unwrap();
        assert!(drift_warnings(dir.path(), &cfg).is_empty());
        // 改端口后漂移
        let mut cfg2 = min_cfg();
        cfg2.server.port = 5000;
        let ws = drift_warnings(dir.path(), &cfg2);
        assert_eq!(ws.len(), 1);
        assert!(ws[0].contains("不一致"), "实际：{:?}", ws);
        // compose 缺失
        std::fs::remove_file(dir.path().join("docker-compose.yml")).unwrap();
        let ws = drift_warnings(dir.path(), &cfg);
        assert!(ws[0].contains("zpm docker init"));
    }

    #[test]
    fn stage_binary_tars_plain_binary_as_flat_zpm_entry() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("zpm-debug-build");
        std::fs::write(&bin, b"fake-elf").unwrap();
        let dest = stage_binary(dir.path(), &bin, "x86_64-unknown-linux-musl").unwrap();
        assert_eq!(
            dest.file_name().unwrap().to_str().unwrap(),
            "zpm-x86_64-unknown-linux-musl.tar.gz"
        );
        // 归档内是根级单条目 zpm（与 release 产物布局一致）
        let list = Command::new("tar").args(["-tzf"]).arg(&dest).output().unwrap();
        assert!(list.status.success());
        let names = String::from_utf8_lossy(&list.stdout);
        assert!(names.lines().any(|l| l.trim() == "zpm"), "归档内容：{names}");
        // 重复落位返回同路径（裁定：--binary 显式提供时无条件重新落位，不再静默复用旧归档）
        let dest2 = stage_binary(dir.path(), &bin, "x86_64-unknown-linux-musl").unwrap();
        assert_eq!(dest, dest2);
    }

    #[test]
    fn stage_binary_passes_through_tar_gz_and_overwrites_unconditionally() {
        let dir = tempfile::tempdir().unwrap();
        // 预制一个 tar.gz
        let src_bin = dir.path().join("zpm");
        std::fs::write(&src_bin, b"payload").unwrap();
        let src_tgz = dir.path().join("custom.tar.gz");
        let o = Command::new("tar")
            .args(["czf"]).arg(&src_tgz).arg("-C").arg(dir.path()).arg("zpm")
            .output().unwrap();
        assert!(o.status.success());
        let dest = stage_binary(dir.path(), &src_tgz, "aarch64-unknown-linux-musl").unwrap();
        assert!(dest.exists());
        // 直通覆盖语义（裁定：--binary 显式提供时无条件重新落位，无需 force）
        let other = dir.path().join("other.tar.gz");
        std::fs::write(dir.path().join("marker"), b"marker").unwrap();
        let o = Command::new("tar")
            .args(["czf"]).arg(&other).arg("-C").arg(dir.path()).arg("marker")
            .output().unwrap();
        assert!(o.status.success());
        stage_binary(dir.path(), &other, "aarch64-unknown-linux-musl").unwrap();
        let list = Command::new("tar").args(["-tzf"]).arg(&dest).output().unwrap();
        assert!(String::from_utf8_lossy(&list.stdout).contains("marker"), "未覆盖");
    }

    #[test]
    fn stage_binary_missing_source_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = stage_binary(dir.path(), &dir.path().join("nope"), "t").unwrap_err();
        assert!(err.to_string().contains("--binary"), "实际：{err}");
    }

    /// 在 dir 下写可执行脚本（unix）
    #[cfg(unix)]
    fn fake_prog(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn docker_build_args_shape() {
        assert_eq!(
            docker_build_args("img", "t1", Some("linux/arm64")),
            vec![
                "build".to_string(),
                "--platform".to_string(),
                "linux/arm64".to_string(),
                "-t".to_string(),
                "img:t1".to_string(),
                ".".to_string()
            ]
        );
        assert_eq!(
            docker_build_args("img", "t1", None),
            vec!["build".to_string(), "-t".to_string(), "img:t1".to_string(), ".".to_string()]
        );
    }

    #[test]
    fn compose_args_shape() {
        assert_eq!(compose_args("up"), vec!["compose".to_string(), "up".to_string(), "-d".to_string()]);
        assert_eq!(compose_args("down"), vec!["compose".to_string(), "down".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_available_reports_missing_and_broken() {
        let dir = tempfile::tempdir().unwrap();
        // 假 docker：成功
        let good = fake_prog(dir.path(), "docker-ok", "#!/bin/sh\nexit 0\n");
        let prog = good.to_str().unwrap();
        ensure_available(prog).unwrap();
        // 假 docker：退出非零（daemon 未运行）
        let bad = fake_prog(dir.path(), "docker-bad", "#!/bin/sh\necho cannot connect >&2\nexit 1\n");
        let err = ensure_available(bad.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("不可用"), "实际：{err}");
        assert!(err.to_string().contains("cannot connect"));
        // 不存在的程序
        let err = ensure_available(dir.path().join("no-such-docker").to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("未检测到"), "实际：{err}");
    }

    #[cfg(unix)]
    #[test]
    fn run_prog_carries_stderr_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let bad = fake_prog(dir.path(), "docker", "#!/bin/sh\necho boom >&2\nexit 7\n");
        let args = vec!["build".to_string()];
        let err = run_prog(bad.to_str().unwrap(), &args, dir.path()).unwrap_err();
        assert!(err.to_string().contains("boom"), "实际：{err}");
        assert!(err.to_string().contains("build"), "错误应含子命令：{err}");
    }

    #[test]
    fn sanitize_collapses_separator_runs() {
        assert_eq!(sanitize_image_name("app..data"), "app-data");
        assert_eq!(sanitize_image_name("app._x"), "app-x");
        assert_eq!(sanitize_image_name("a-._-b"), "a-b");
        assert_eq!(sanitize_image_name("my.app"), "my-app");
    }

    #[test]
    fn resolve_platform_prefers_cli_then_config() {
        let mut cfg = DockerCfg::default();
        cfg.platform = Some("linux/arm64".into());
        assert_eq!(
            resolve_platform(Some("linux/amd64"), &cfg).unwrap(),
            Some(Platform::Amd64)
        );
        assert_eq!(resolve_platform(None, &cfg).unwrap(), Some(Platform::Arm64));
        cfg.platform = None;
        assert_eq!(resolve_platform(None, &cfg).unwrap(), None);
    }

    #[test]
    fn platform_drift_warning_compares_disk_compose() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  zpm:\n    platform: linux/arm64\n",
        )
        .unwrap();
        // 与磁盘一致 → 无警告
        assert_eq!(platform_drift_warning(dir.path(), Platform::Arm64), None);
        // 不一致 → 警告含「不一致」与重新生成指引
        let w = platform_drift_warning(dir.path(), Platform::Amd64).unwrap();
        assert!(w.contains("不一致"), "实际：{w}");
        assert!(w.contains("init --platform"), "实际：{w}");
        // compose 缺失 → None（无 platform 行可比）
        std::fs::remove_file(dir.path().join("docker-compose.yml")).unwrap();
        assert_eq!(platform_drift_warning(dir.path(), Platform::Amd64), None);
    }

    #[test]
    fn compose_host_port_from_disk_reads_disk_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  zpm:\n    ports: [\"4310:4310\"]\n",
        )
        .unwrap();
        assert_eq!(compose_host_port_from_disk(dir.path()), Some(4310));
        std::fs::remove_file(dir.path().join("docker-compose.yml")).unwrap();
        assert_eq!(compose_host_port_from_disk(dir.path()), None);
    }
}
