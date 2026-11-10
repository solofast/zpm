//! boot 调度分支测试（规格：sync.schedule 未配置 → has_schedule()==false；
//! 配置后 → true 且 stop() 可停）。路由全链行为已由 static_e2e/api 覆盖。

use std::fs;
use tempfile::tempdir;
use zpm_core::project_config;
use zpm_server::boot;

fn load_cfg(dir: &std::path::Path, schedule: Option<&str>) -> project_config::ProjectConfig {
    let yaml = match schedule {
        Some(s) => format!(
            r#"
name: smoke
datasource: {{ type: sqlite, file: data/src.db }}
target: {{ type: sqlite, file: data/app.db }}
sync:
  schedule: "{s}"
  tables: []
"#
        ),
        None => r#"
name: smoke
datasource: { type: sqlite, file: data/src.db }
target: { type: sqlite, file: data/app.db }
sync:
  tables: []
"#
        .to_string(),
    };
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("zpm.config.yaml"), yaml).unwrap();
    project_config::load_project_config(&dir.join("zpm.config.yaml")).unwrap()
}

/// sync.schedule 未配置 → 不启动调度线程
#[test]
fn no_schedule_creates_inactive_handle() {
    let tmp = tempdir().unwrap();
    let cfg = load_cfg(tmp.path(), None);
    let handle = boot::start_schedule(&cfg, tmp.path());
    assert!(!handle.has_schedule());
}

/// 配置了 schedule → 调度线程启动；stop() 可停（远期 cron，测试期间不触发同步）
#[test]
fn schedule_configured_activates_handle_and_stops() {
    let tmp = tempdir().unwrap();
    let cfg = load_cfg(tmp.path(), Some("0 0 1 1 *"));
    let handle = boot::start_schedule(&cfg, tmp.path());
    assert!(handle.has_schedule());
    handle.stop();
}

/// cron 表达式解析失败 → 仅提示，不启动调度线程（has_schedule()==false，服务照常）
#[test]
fn invalid_schedule_creates_inactive_handle() {
    let tmp = tempdir().unwrap();
    let cfg = load_cfg(tmp.path(), Some("not-a-cron"));
    let handle = boot::start_schedule(&cfg, tmp.path());
    assert!(!handle.has_schedule());
    handle.stop();
}
