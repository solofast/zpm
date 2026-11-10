//! CLI 冒烟测试：子命令（framework/project/db/sync）的 help 与核心流程。

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::tempdir;

fn zpm() -> Command {
    Command::cargo_bin("zpm").unwrap()
}

/// 造一个最小 zpm 项目：zpm.config.yaml（sqlite 源+目标，users 表）+ 源库两行数据
fn write_project(dir: &std::path::Path) -> std::path::PathBuf {
    let proj = dir.join("proj");
    fs::create_dir_all(proj.join("data")).unwrap();
    fs::write(
        proj.join("zpm.config.yaml"),
        r#"
name: smoke
datasource: { type: sqlite, file: data/src.db }
target: { type: sqlite, file: data/app.db }
sync:
  tables:
    - { name: users, primary_key: id }
write: {}
"#,
    )
    .unwrap();
    let conn = rusqlite::Connection::open(proj.join("data/src.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
         INSERT INTO users VALUES (1, 'a'), (2, 'b');",
    )
    .unwrap();
    proj
}

/// 各新子命令 --help 退出 0 且含中文说明
#[test]
fn new_subcommands_help() {
    for (args, keyword) in [
        (vec!["framework", "--help"], "加载框架"),
        (vec!["project", "--help"], "项目"),
        (vec!["db", "--help"], "目标库"),
        (vec!["serve", "--help"], "原型服务器"),
    ] {
        zpm()
            .args(&args)
            .assert()
            .success()
            .stdout(predicate::str::contains(keyword));
    }
}

/// framework load → 激活仓库落盘；list 输出；重复加载报错；--force 覆盖
#[test]
fn framework_import_and_list() {
    let cfg = tempdir().unwrap();
    let rh = tempdir().unwrap();
    // repo 环境搭建：repo_home 指向独立 tempdir → repo create main → repo switch main
    zpm()
        .env("ZPM_CONFIG_HOME", cfg.path())
        .args(["config", "set", "repo_home"])
        .arg(rh.path())
        .assert()
        .success();
    zpm()
        .env("ZPM_CONFIG_HOME", cfg.path())
        .args(["repo", "create", "main", "--path"])
        .arg(rh.path().join("main"))
        .assert()
        .success();
    zpm()
        .env("ZPM_CONFIG_HOME", cfg.path())
        .args(["repo", "switch", "main"])
        .assert()
        .success();
    let with_env = |cmd: &mut Command| {
        cmd.env("ZPM_CONFIG_HOME", cfg.path());
    };

    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fw-src");
    fs::create_dir_all(&fw).unwrap();
    fs::write(
        fw.join("framework.yaml"),
        "name: fixture-fw\nversion: 0.1.0\ndescription: 测试框架\n",
    )
    .unwrap();
    let home = rh.path().join("main").join("frameworks");

    let mut load_cmd = zpm();
    load_cmd.args(["framework", "load"]).arg(&fw);
    with_env(&mut load_cmd);
    load_cmd.assert().success();
    assert!(home.join("fixture-fw").join("framework.yaml").is_file());

    let mut list_cmd = zpm();
    list_cmd.args(["framework", "list"]);
    with_env(&mut list_cmd);
    list_cmd
        .assert()
        .success()
        .stdout(predicate::str::contains("fixture-fw"));

    // 重复加载报错
    let mut load_cmd = zpm();
    load_cmd.args(["framework", "load"]).arg(&fw);
    with_env(&mut load_cmd);
    load_cmd.assert().failure();
    // --force 覆盖成功
    let mut load_cmd = zpm();
    load_cmd.args(["framework", "load", "--force"]).arg(&fw);
    with_env(&mut load_cmd);
    load_cmd.assert().success();

    // 路径穿越名拒绝（--force 也不得删除仓库目录之外的目标）
    let evil = tmp.path().join("evil-marker");
    fs::create_dir_all(&evil).unwrap();
    let mut load_cmd = zpm();
    load_cmd
        .args(["framework", "load", "--force"])
        .arg(&fw)
        .arg("../evil-marker");
    with_env(&mut load_cmd);
    load_cmd
        .assert()
        .failure()
        .stderr(predicate::str::contains("非法框架名"));
    assert!(evil.is_dir(), "穿越目标目录不得被删除");
}

/// db init → sync → seed → export → import 全流程（sqlite→sqlite）
#[test]
fn db_and_sync_flow() {
    let tmp = tempdir().unwrap();
    let proj = write_project(tmp.path());

    // db init 建目标文件
    zpm().current_dir(&proj).args(["db", "init"]).assert().success();
    assert!(proj.join("data/app.db").is_file());

    // 数据同步：源库 users 两行 → 目标
    zpm()
        .current_dir(&proj)
        .args(["db", "sync"])
        .assert()
        .success()
        .stdout(predicate::str::contains("synced users: 2 rows"));

    // 未声明的表报错
    zpm()
        .current_dir(&proj)
        .args(["db", "sync", "--tables", "nope"])
        .assert()
        .failure();

    // seed：分号切分逐条执行
    fs::write(
        proj.join("seed.sql"),
        "CREATE TABLE t (id INTEGER PRIMARY KEY);\nINSERT INTO t VALUES (9);",
    )
    .unwrap();
    zpm()
        .current_dir(&proj)
        .args(["db", "seed"])
        .arg(proj.join("seed.sql"))
        .assert()
        .success();
    let conn = rusqlite::Connection::open(proj.join("data/app.db")).unwrap();
    let n: i64 = conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);

    // export → import 往返（幂等）
    zpm().current_dir(&proj).args(["db", "export"]).assert().success();
    assert!(proj.join("zpm-snapshot.json").is_file());
    zpm()
        .current_dir(&proj)
        .args(["db", "import", "zpm-snapshot.json"])
        .assert()
        .success();
    let (_, total) = {
        let conn = rusqlite::Connection::open(proj.join("data/app.db")).unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM users", [], |r| r.get(0)).unwrap();
        ((), n)
    };
    assert_eq!(total, 2, "导入后 users 行数不变");
}

/// 不在项目内时 db sync 退出码 1
#[test]
fn sync_outside_project_fails() {
    let tmp = tempdir().unwrap();
    zpm()
        .current_dir(tmp.path())
        .args(["db", "sync"])
        .assert()
        .failure()
        .code(predicate::eq(1));
}
