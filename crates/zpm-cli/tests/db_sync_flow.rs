//! `zpm db sync` 集成测试：默认配置 / --tables 白名单 / --config 指定路径 / --from 覆盖。

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use tempfile::{tempdir, TempDir};

fn zpm() -> Command {
    Command::cargo_bin("zpm").unwrap()
}

fn zpm_env(cfg: &TempDir) -> Command {
    let mut c = zpm();
    c.env("ZPM_CONFIG_HOME", cfg.path());
    c
}

/// fixture 框架：framework.yaml + 项目配置模板（含空 sync.tables）
fn make_framework(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("framework.yaml"), "name: fixture-fw\nversion: 0.1.0\n").unwrap();
    fs::write(
        dir.join("zpm.config.yaml.tmpl"),
        "name: {{name}}\nserver:\n  port: 4310\n  host: 0.0.0.0\n  theme: default\ndatasource:\n  type: sqlite\n  file: data/source.db\ntarget:\n  type: sqlite\n  file: data/app.db\nsync:\n  tables: []\n",
    )
    .unwrap();
}

/// repo 环境 + project create demo；返回 (tmp, cfg, rh, proj)
/// （tmp 承载 fixture 与项目目录，必须保活到测试结束）
fn setup() -> (TempDir, TempDir, TempDir, std::path::PathBuf) {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fw-src");
    make_framework(&fw);
    let cfg = tempdir().unwrap();
    let rh = tempdir().unwrap();
    zpm_env(&cfg)
        .args(["config", "set", "repo_home"])
        .arg(rh.path())
        .assert()
        .success();
    zpm_env(&cfg)
        .args(["repo", "create", "main", "--path"])
        .arg(rh.path().join("main"))
        .assert()
        .success();
    zpm_env(&cfg).args(["repo", "switch", "main"]).assert().success();
    zpm_env(&cfg).args(["framework", "load"]).arg(&fw).assert().success();
    let proj = tmp.path().join("demo");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fixture-fw"])
        .assert()
        .success();
    (tmp, cfg, rh, proj)
}

/// 覆盖项目配置为 users 表声明（源 data/src.db → 目标 data/app.db）+ 写源库两行
fn write_users_config(proj: &Path) {
    fs::write(
        proj.join("zpm.config.yaml"),
        "name: demo\ndatasource: { type: sqlite, file: data/src.db }\ntarget: { type: sqlite, file: data/app.db }\nsync:\n  tables:\n    - { name: users, primary_key: id }\n",
    )
    .unwrap();
    fs::create_dir_all(proj.join("data")).unwrap();
    let conn = Connection::open(proj.join("data/src.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\nINSERT INTO users VALUES (1,'a'),(2,'b');",
    )
    .unwrap();
}

fn users_count(db: &Path) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT count(*) FROM users", [], |r| r.get(0)).unwrap()
}

/// ① 默认（当前目录向上找配置）+ --tables 白名单（未声明表名 → 非零）
#[test]
fn db_sync_by_default_config_and_tables_whitelist() {
    let (_tmp, cfg, _rh, proj) = setup();
    write_users_config(&proj);

    zpm_env(&cfg)
        .current_dir(&proj)
        .args(["db", "sync"])
        .assert()
        .success()
        .stdout(predicate::str::contains("synced users: 2 rows"));
    assert_eq!(users_count(&proj.join("data/app.db")), 2, "目标库应有两行");

    // 未声明的表名 → NotFound 非零
    zpm_env(&cfg)
        .current_dir(&proj)
        .args(["db", "sync", "--tables", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("表未在配置中声明"));
}

/// ② --config 指定配置路径：在项目外运行，sqlite 相对路径按配置文件父目录解析
#[test]
fn db_sync_explicit_config_path() {
    let (_tmp, cfg, _rh, proj) = setup();
    write_users_config(&proj);

    let mut cmd = zpm_env(&cfg);
    cmd.current_dir(proj.parent().unwrap())
        .args(["db", "sync", "--config"])
        .arg(proj.join("zpm.config.yaml"));
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("synced users: 2 rows"));
    assert_eq!(users_count(&proj.join("data/app.db")), 2);
}

/// ③ --from 临时覆盖数据源：目标库内容来自指定 sqlite 文件
#[test]
fn db_sync_from_sqlite_override() {
    let (_tmp, cfg, _rh, proj) = setup();
    write_users_config(&proj);

    let other = proj.join("data").join("other.db");
    let conn = Connection::open(&other).unwrap();
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\nINSERT INTO users VALUES (1,'a'),(2,'b'),(3,'c');",
    )
    .unwrap();

    zpm_env(&cfg)
        .current_dir(&proj)
        .args(["db", "sync", "--from"])
        .arg(&other)
        .assert()
        .success()
        .stdout(predicate::str::contains("synced users: 3 rows"));
    assert_eq!(users_count(&proj.join("data/app.db")), 3, "目标库应来自 --from 源");
}
