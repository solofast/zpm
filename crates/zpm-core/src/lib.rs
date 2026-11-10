//! zpm-core：项目脚手架与知识库同步的核心逻辑。

pub mod config;
pub mod dbsync;
pub mod error;
pub mod framework;
pub mod knowledge;
pub mod mcp;
pub mod project_config;
pub mod snapshot;
pub mod store;

pub use error::ZpmError;

/// 测试支持：串行化依赖 ZPM_CONFIG_HOME 环境变量的测试，避免并发互扰。
#[cfg(test)]
pub(crate) mod testing {
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
