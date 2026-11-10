//! zpm 的错误类型。模仿 tos 的 KbaseError：thiserror 枚举 + exit_code() 映射。

use zpm_meta::error::MetaError;

/// zpm 错误。exit_code 遵循约定：0 成功 / 用法错误 2 由 clap 处理 / 其他 1。
#[derive(Debug, thiserror::Error)]
pub enum ZpmError {
    /// 目标已存在（如目录已有 zpm.toml）→ 退出码 1
    #[error("{0} 已存在")]
    AlreadyExists(String),
    /// 路径/键不存在 → 退出码 1
    #[error("{0}")]
    NotFound(String),
    /// 当前目录不在 zpm 项目内 → 退出码 1
    #[error("当前目录不在 zpm 项目内：请先运行 `zpm init`")]
    NotAProject,
    /// 未知配置键 → 退出码 1
    #[error("未知配置项：{0}")]
    UnknownKey(String),
    /// （server）请求参数错误 → 退出码 1
    #[error("{0}")]
    BadRequest(String),
    /// （server）禁止访问 → 退出码 1
    #[error("{0}")]
    Forbidden(String),
    /// （server）不安全标识 → 退出码 1
    #[error("{0}")]
    UnsafeIdent(String),
    /// （server）格式错误 → 退出码 1
    #[error("{0}")]
    Format(String),
    /// 未配置任何资产源 → 退出码 1
    #[error("未找到资产源：请用 --from 指定，或在 zpm.toml [sync] source / 全局 assets_dir 中配置")]
    NoAssetSource,
    /// repo 不存在（按名字或路径查找失败）→ 退出码 1
    #[error("{0}")]
    RepoNotFound(String),
    /// project 不存在（按名字或路径查找失败）→ 退出码 1
    #[error("{0}")]
    ProjectNotFound(String),
    /// framework 不存在 → 退出码 1
    #[error("{0}")]
    FrameworkNotFound(String),
    /// 传输失败（git 命令带 stderr）→ 退出码 1
    #[error("{0}")]
    TransportError(String),
    /// 元数据库（zpm.db）错误 → 退出码 1
    #[error("{0}")]
    MetaDbError(String),
    /// 危险操作被拒绝（如删除当前工作目录）→ 退出码 1
    #[error("{0}")]
    Dangerous(String),
    /// IO 错误 → 退出码 1
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// TOML 反序列化错误 → 退出码 1
    #[error("zpm.toml 解析失败：{0}")]
    TomlDe(#[from] toml::de::Error),
    /// TOML 序列化错误 → 退出码 1
    #[error("zpm.toml 生成失败：{0}")]
    TomlSer(String),
    /// JSON（manifest）错误 → 退出码 1
    #[error("manifest.json 解析失败：{0}")]
    Json(#[from] serde_json::Error),
    /// 其他内部错误 → 退出码 1
    #[error("{0}")]
    Other(String),
    /// SQLite 错误 → 退出码 1
    #[error("sqlite 错误：{0}")]
    Sqlite(#[from] rusqlite::Error),
    /// postgres 错误 → 退出码 1
    #[error("postgres 错误：{0}")]
    Postgres(#[from] postgres::Error),
}

impl ZpmError {
    /// 映射到进程退出码：成功 0 / 用法错误 2 由 clap 处理 / v0.1 业务与内部错误统一为 1，
    /// 保留 match 结构便于后续细分。
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::AlreadyExists(_)
            | Self::NotFound(_)
            | Self::NotAProject
            | Self::UnknownKey(_)
            | Self::NoAssetSource => 1,
            Self::RepoNotFound(_)
            | Self::ProjectNotFound(_)
            | Self::FrameworkNotFound(_)
            | Self::TransportError(_)
            | Self::MetaDbError(_)
            | Self::Dangerous(_) => 1,
            Self::BadRequest(_)
            | Self::Forbidden(_)
            | Self::UnsafeIdent(_)
            | Self::Format(_) => 1,
            Self::Io(_) | Self::TomlDe(_) | Self::TomlSer(_) | Self::Json(_) | Self::Other(_) => 1,
            Self::Sqlite(_) => 1,
            Self::Postgres(_) => 1,
        }
    }
}

/// MetaError → ZpmError：CLI 收到 zpm-meta 的错误时经 `?` 自动转为统一出口。
/// Db/Duplicate 需中文包装，其余 payload 直透；Io 复用既有 Io 变体。
impl From<MetaError> for ZpmError {
    fn from(e: MetaError) -> Self {
        match e {
            MetaError::Db(err) => Self::MetaDbError(format!("元数据库错误：{err}")),
            MetaError::RepoNotFound(name) => Self::RepoNotFound(name),
            MetaError::ProjectNotFound(name) => Self::ProjectNotFound(name),
            MetaError::Duplicate(msg) => Self::Format(format!("{msg} 已存在")),
            MetaError::Transport(msg) => Self::TransportError(msg),
            MetaError::Io(err) => Self::Io(err),
            MetaError::Format(msg) => Self::Format(msg),
            MetaError::Dangerous(msg) => Self::Dangerous(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zpm_meta::error::MetaError;

    #[test]
    fn precondition_errors_exit_one() {
        assert_eq!(ZpmError::AlreadyExists("zpm.toml".into()).exit_code(), 1);
        assert_eq!(ZpmError::NotFound("x".into()).exit_code(), 1);
        assert_eq!(ZpmError::NotAProject.exit_code(), 1);
        assert_eq!(ZpmError::UnknownKey("foo".into()).exit_code(), 1);
        assert_eq!(ZpmError::NoAssetSource.exit_code(), 1);
    }

    #[test]
    fn internal_errors_exit_one() {
        let io_err = std::io::Error::other("boom");
        assert_eq!(ZpmError::Io(io_err).exit_code(), 1);
        assert_eq!(ZpmError::Other("oops".into()).exit_code(), 1);
        assert_eq!(ZpmError::Json(serde_json::from_str::<()>("bad").unwrap_err()).exit_code(), 1);
    }

    #[test]
    fn new_variants_exit_code_1() {
        assert_eq!(ZpmError::RepoNotFound("x".into()).exit_code(), 1);
        assert_eq!(ZpmError::ProjectNotFound("x".into()).exit_code(), 1);
        assert_eq!(ZpmError::FrameworkNotFound("x".into()).exit_code(), 1);
        assert_eq!(ZpmError::TransportError("x".into()).exit_code(), 1);
        assert_eq!(ZpmError::MetaDbError("x".into()).exit_code(), 1);
        assert_eq!(ZpmError::Dangerous("x".into()).exit_code(), 1);
    }

    #[test]
    fn from_meta_error_maps_variants() {
        // RepoNotFound → RepoNotFound（payload 直透）
        let mapped = ZpmError::from(MetaError::RepoNotFound("x".into()));
        assert!(matches!(mapped, ZpmError::RepoNotFound(ref s) if s == "x"));
        // ProjectNotFound → ProjectNotFound
        let mapped = ZpmError::from(MetaError::ProjectNotFound("x".into()));
        assert!(matches!(mapped, ZpmError::ProjectNotFound(ref s) if s == "x"));
        // Duplicate → Format（中文包装，对齐 MetaError::Duplicate 的 Display）
        let mapped = ZpmError::from(MetaError::Duplicate("repo x".into()));
        assert!(matches!(mapped, ZpmError::Format(ref s) if s == "repo x 已存在"));
        // Db → MetaDbError（中文包装）
        let db_err = rusqlite::Error::InvalidColumnName("c".into());
        let mapped = ZpmError::from(MetaError::Db(db_err));
        assert!(
            matches!(mapped, ZpmError::MetaDbError(ref s)
                if s.starts_with("元数据库错误：") && s.len() > "元数据库错误：".len()),
            "应带中文包装前缀：{mapped}"
        );
        // Transport → TransportError
        let mapped = ZpmError::from(MetaError::Transport("boom".into()));
        assert!(matches!(mapped, ZpmError::TransportError(ref s) if s == "boom"));
        // Dangerous → Dangerous
        let mapped = ZpmError::from(MetaError::Dangerous("boom".into()));
        assert!(matches!(mapped, ZpmError::Dangerous(ref s) if s == "boom"));
        // Io → 既有 Io
        let mapped = ZpmError::from(MetaError::Io(std::io::Error::other("io boom")));
        assert!(matches!(mapped, ZpmError::Io(_)));
        // Format → Format（payload 直透）
        let mapped = ZpmError::from(MetaError::Format("bad yaml".into()));
        assert!(matches!(mapped, ZpmError::Format(ref s) if s == "bad yaml"));
    }

    #[test]
    fn server_variant_errors_exit_one_and_display_raw() {
        assert_eq!(ZpmError::BadRequest("x".into()).exit_code(), 1);
        let err = ZpmError::BadRequest("x".into());
        assert_eq!(format!("{err}"), "x");
        assert_eq!(ZpmError::Forbidden("y".into()).exit_code(), 1);
        assert_eq!(format!("{}", ZpmError::Forbidden("y".into())), "y");
        assert_eq!(ZpmError::UnsafeIdent("z".into()).exit_code(), 1);
        assert_eq!(format!("{}", ZpmError::UnsafeIdent("z".into())), "z");
        assert_eq!(ZpmError::Format("w".into()).exit_code(), 1);
        assert_eq!(format!("{}", ZpmError::Format("w".into())), "w");
    }
}
