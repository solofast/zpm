//! 全局配置（~/.config/zpm/config.toml）读写，支持 get/set/list。

use crate::error::ZpmError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 当前支持的配置键
pub const KEYS: &[&str] = &["assets_dir", "repo_home", "active_repo"];

/// 环境变量：覆盖配置目录（测试隔离用）
pub const ENV_CONFIG_HOME: &str = "ZPM_CONFIG_HOME";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// 资产源目录（同步时的默认来源）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets_dir: Option<String>,
    /// 仓库根目录（zpm.db 所在位置；缺省语义由调用方决定）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_home: Option<String>,
    /// 当前激活的仓库名
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_repo: Option<String>,
}

/// 配置目录：$ZPM_CONFIG_HOME > ~/.config/zpm（经 dirs::config_dir）
pub fn config_dir() -> PathBuf {
    if let Ok(p) = std::env::var(ENV_CONFIG_HOME) {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zpm")
}

/// 全局配置文件路径
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// 校验配置键是否受支持
fn check_key(key: &str) -> Result<(), ZpmError> {
    if KEYS.contains(&key) {
        Ok(())
    } else {
        Err(ZpmError::UnknownKey(key.to_string()))
    }
}

impl GlobalConfig {
    /// 加载配置；文件不存在时返回默认值
    pub fn load() -> Result<Self, ZpmError> {
        let p = config_path();
        if !p.exists() {
            return Ok(Self::default());
        }
        let txt = std::fs::read_to_string(&p)?;
        Ok(toml::from_str(&txt)?)
    }

    /// 保存配置（自动创建目录）
    pub fn save(&self) -> Result<(), ZpmError> {
        let p = config_path();
        std::fs::create_dir_all(p.parent().expect("配置路径必有父目录"))?;
        let txt = toml::to_string_pretty(self).map_err(|e| ZpmError::TomlSer(e.to_string()))?;
        std::fs::write(p, txt)?;
        Ok(())
    }

    /// 读取单个配置项
    pub fn get(key: &str) -> Result<Option<String>, ZpmError> {
        check_key(key)?;
        let cfg = Self::load()?;
        Ok(match key {
            "assets_dir" => cfg.assets_dir,
            "repo_home" => cfg.repo_home,
            "active_repo" => cfg.active_repo,
            _ => None,
        })
    }

    /// 设置单个配置项；value 为空字符串时清除该项
    pub fn set(key: &str, value: &str) -> Result<(), ZpmError> {
        check_key(key)?;
        let mut cfg = Self::load()?;
        let v = if value.is_empty() { None } else { Some(value.to_string()) };
        match key {
            "assets_dir" => cfg.assets_dir = v,
            "repo_home" => cfg.repo_home = v,
            "active_repo" => cfg.active_repo = v,
            _ => {}
        }
        cfg.save()
    }

    /// 列出全部配置项（含未设置的键，值为 None）
    pub fn list() -> Result<Vec<(String, Option<String>)>, ZpmError> {
        let cfg = Self::load()?;
        Ok(vec![
            ("assets_dir".into(), cfg.assets_dir),
            ("repo_home".into(), cfg.repo_home),
            ("active_repo".into(), cfg.active_repo),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_in_isolated_home() {
        // 与其他会读取全局配置的测试互斥
        let _guard = crate::testing::ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // edition 2021 下 set_var/remove_var 是安全 API；仅本测试使用该变量
        std::env::set_var(ENV_CONFIG_HOME, tmp.path());
        // 初始为空
        assert_eq!(GlobalConfig::get("assets_dir").unwrap(), None);
        // set → get
        GlobalConfig::set("assets_dir", "/tmp/assets").unwrap();
        assert_eq!(
            GlobalConfig::get("assets_dir").unwrap().as_deref(),
            Some("/tmp/assets")
        );
        // 落盘且可重新加载
        let raw = std::fs::read_to_string(config_path()).unwrap();
        assert!(raw.contains("/tmp/assets"));
        // 清除（空字符串）
        GlobalConfig::set("assets_dir", "").unwrap();
        assert_eq!(GlobalConfig::get("assets_dir").unwrap(), None);
        // 未知键报错
        assert!(matches!(
            GlobalConfig::get("nope"),
            Err(ZpmError::UnknownKey(_))
        ));
        // list 覆盖全部键
        let all = GlobalConfig::list().unwrap();
        assert_eq!(all.len(), KEYS.len());
        std::env::remove_var(ENV_CONFIG_HOME);
    }

    #[test]
    fn repo_home_default_and_roundtrip() {
        // 与其他会读取全局配置的测试互斥
        let _guard = crate::testing::ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_CONFIG_HOME, tmp.path());
        // 无配置文件时 get("repo_home") == None（默认值由调用方决定，config 层只存字符串）
        assert_eq!(GlobalConfig::get("repo_home").unwrap(), None);
        // set → get 命中
        GlobalConfig::set("repo_home", "/tmp/rh").unwrap();
        assert_eq!(
            GlobalConfig::get("repo_home").unwrap().as_deref(),
            Some("/tmp/rh")
        );
        GlobalConfig::set("active_repo", "main").unwrap();
        assert_eq!(
            GlobalConfig::get("active_repo").unwrap().as_deref(),
            Some("main")
        );
        // 清除（空字符串）
        GlobalConfig::set("active_repo", "").unwrap();
        assert_eq!(GlobalConfig::get("active_repo").unwrap(), None);
        std::env::remove_var(ENV_CONFIG_HOME);
    }

    #[test]
    fn keys_whitelist_extended() {
        // 与其他会读取全局配置的测试互斥
        let _guard = crate::testing::ENV_LOCK.lock().unwrap();
        assert!(KEYS.contains(&"repo_home"));
        assert!(KEYS.contains(&"active_repo"));
    }
}
