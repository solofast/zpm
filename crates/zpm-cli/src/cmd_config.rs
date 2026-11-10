//! `zpm config`：查看/设置全局配置（~/.config/zpm/config.toml）。
use clap::{Args, Subcommand};
use zpm_core::config::GlobalConfig;
use zpm_core::ZpmError;

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommands,
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// 读取配置项
    Get { key: String },
    /// 设置配置项（value 为空字符串时清除该项）
    Set { key: String, value: String },
    /// 列出全部配置项
    List,
}

pub fn run(args: ConfigArgs) -> Result<(), ZpmError> {
    match args.command {
        ConfigCommands::Get { key } => {
            match GlobalConfig::get(&key)?.as_deref() {
                Some(v) => println!("{key} = {v}"),
                None => println!("{key} 未设置"),
            }
            Ok(())
        }
        ConfigCommands::Set { key, value } => {
            GlobalConfig::set(&key, &value)?;
            match value.is_empty() {
                true => println!("已清除 {key}"),
                false => println!("已设置 {key} = {value}"),
            }
            Ok(())
        }
        ConfigCommands::List => {
            for (k, v) in GlobalConfig::list()? {
                match v {
                    Some(v) => println!("{k} = {v}"),
                    None => println!("{k} 未设置"),
                }
            }
            Ok(())
        }
    }
}
