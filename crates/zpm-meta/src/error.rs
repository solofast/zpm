//! zpm-meta 的错误类型。手写 Display + std::error::Error（不依赖 thiserror），文案风格对齐 zpm-core。

/// zpm-meta 错误。退出码映射由 CLI 层完成，此处只负责错误语义与展示文案。
#[derive(Debug)]
pub enum MetaError {
    /// SQLite 错误
    Db(rusqlite::Error),
    /// repo 不存在（按名字或路径查找失败）
    RepoNotFound(String),
    /// project 不存在（按名字或路径查找失败）
    ProjectNotFound(String),
    /// 目标已存在（如 repo / project 重名）
    Duplicate(String),
    /// 传输失败（git 命令带 stderr）
    Transport(String),
    /// IO 错误
    Io(std::io::Error),
    /// 格式错误（如 framework.yaml 解析失败）
    Format(String),
    /// 危险操作被拒绝（如删除当前工作目录）
    Dangerous(String),
}

pub type MetaResult<T> = Result<T, MetaError>;

impl std::fmt::Display for MetaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(e) => write!(f, "sqlite 错误：{e}"),
            Self::RepoNotFound(name) => write!(f, "repo 不存在：{name}"),
            Self::ProjectNotFound(name) => write!(f, "project 不存在：{name}"),
            Self::Duplicate(msg) => write!(f, "{msg} 已存在"),
            Self::Transport(msg) => write!(f, "传输失败：{msg}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Format(msg) => write!(f, "格式错误：{msg}"),
            Self::Dangerous(msg) => write!(f, "危险操作被拒绝：{msg}"),
        }
    }
}

impl std::error::Error for MetaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for MetaError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

impl From<std::io::Error> for MetaError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
