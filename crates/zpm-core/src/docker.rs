//! zpm 项目容器化交付：`zpm docker init|build|up|down` 的核心逻辑。
//!
//! 职责：平台映射与镜像名安全化（纯函数）、三件套模板渲染与生成、
//! release 二进制下载/`--binary` 落位、docker/compose shell-out、漂移检查。
//! shell-out 失败时错误携带 stderr（沿 git 传输层约定）。

use crate::error::ZpmError;
use crate::project_config::DockerCfg;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_config::DockerCfg;

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
}
