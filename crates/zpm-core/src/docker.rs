//! zpm 项目容器化交付：`zpm docker init|build|up|down` 的核心逻辑。
//!
//! 职责：平台映射与镜像名安全化（纯函数）、三件套模板渲染与生成、
//! release 二进制下载/`--binary` 落位、docker/compose shell-out、漂移检查。
//! shell-out 失败时错误携带 stderr（沿 git 传输层约定）。

use crate::error::ZpmError;
use crate::project_config::DockerCfg;
use std::collections::BTreeMap;
use std::path::Path;

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

/// 镜像名安全化：取 name 末段（路径名 → 目录名），小写化，非 [a-z0-9_.-] → '-'，
/// 去掉开头/结尾的非字母数字段（docker 镜像名组件须以字母数字结尾）；空则回落 zpm-app。
pub fn sanitize_image_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mapped: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped
        .trim_start_matches(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .trim_end_matches(['-', '.', '_']);
    if trimmed.is_empty() { "zpm-app".to_string() } else { trimmed.to_string() }
}

/// 生效镜像名：cfg.image > name 末段安全化（此处只做纯计算，ProjectConfig 版包装见任务 3）
pub fn effective_image_of(name: &str, cfg: &DockerCfg) -> String {
    cfg.image.clone().unwrap_or_else(|| sanitize_image_name(name))
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

/// 漂移检查：按当前配置重渲染 compose 与磁盘比对；返回警告列表（空 = 一致）
pub fn drift_warnings(root: &Path, cfg: &crate::project_config::ProjectConfig) -> Vec<String> {
    let path = root.join("docker-compose.yml");
    let Ok(disk) = std::fs::read_to_string(&path) else {
        return vec!["未找到 docker-compose.yml（先运行 zpm docker init）".to_string()];
    };
    let rendered = render_compose(&render_ctx(cfg, None));
    if rendered != disk {
        vec!["docker-compose.yml 与当前 zpm.config.yaml 不一致（端口/镜像名/platform 可能改过）；重新生成：zpm docker init --force".to_string()]
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_config::DockerCfg;
    use crate::project_config::ProjectConfig;
    use std::collections::BTreeMap;

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
}
