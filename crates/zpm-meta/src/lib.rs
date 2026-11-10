//! zpm-meta：CLI 元数据库（zpm.db）与传输抽象。

pub mod db;
pub mod error;
pub mod project;
pub mod rfc3339;
pub mod repo;
pub mod transport;

pub use error::{MetaError, MetaResult};
