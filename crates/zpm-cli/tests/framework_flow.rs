//! `zpm framework` 六子命令集成测试：每个测试独立 tempdir + ZPM_CONFIG_HOME 隔离，
//! repo_home 显式指向独立 tempdir，绝不触碰真实 ~/.zpm。

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::{tempdir, TempDir};

fn zpm() -> Command {
    Command::cargo_bin("zpm").unwrap()
}

/// 带隔离环境的 zpm 命令：ZPM_CONFIG_HOME 指向 cfg tempdir。
fn zpm_in(cfg: &TempDir) -> Command {
    let mut cmd = zpm();
    cmd.env("ZPM_CONFIG_HOME", cfg.path());
    cmd
}

/// 建立隔离环境：repo_home 指向独立 rh tempdir + 创建并切换激活仓库 main（<rh>/main）。
fn isolate_with_repo() -> (TempDir, TempDir) {
    let cfg = tempdir().unwrap();
    let rh = tempdir().unwrap();
    zpm_in(&cfg)
        .args(["config", "set", "repo_home"])
        .arg(rh.path())
        .assert()
        .success();
    zpm_in(&cfg)
        .args(["repo", "create", "main", "--path"])
        .arg(rh.path().join("main"))
        .assert()
        .success();
    zpm_in(&cfg)
        .args(["repo", "switch", "main"])
        .assert()
        .success();
    (cfg, rh)
}

/// 造一个最小框架目录：framework.yaml（name/version/description）+ 一个内容文件。
fn make_framework(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let fw = dir.join(name);
    fs::create_dir_all(&fw).unwrap();
    fs::write(
        fw.join("framework.yaml"),
        format!("name: {name}\nversion: 0.1.0\ndescription: 测试框架\n"),
    )
    .unwrap();
    fs::write(fw.join("note.txt"), format!("{name} 内容")).unwrap();
    fw
}

/// load → 落盘激活仓库 + index.yaml + list；重复 load 报错；--force 覆盖；delete 后目录与索引均清除
#[test]
fn framework_load_list_delete_flow() {
    let (cfg, rh) = isolate_with_repo();
    let repo_dir = rh.path().join("main");
    let fw = make_framework(cfg.path(), "fw-a");

    zpm_in(&cfg)
        .args(["framework", "load"])
        .arg(&fw)
        .assert()
        .success()
        .stdout(predicate::str::contains("已加载框架 fw-a"));
    let dest = repo_dir.join("frameworks/fw-a");
    assert!(dest.join("framework.yaml").is_file(), "load 后激活仓库应有 fw-a");
    let index = fs::read_to_string(repo_dir.join("frameworks/index.yaml")).unwrap();
    assert!(index.contains("fw-a"), "index.yaml 应含 fw-a：{index}");
    zpm_in(&cfg)
        .args(["framework", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fw-a"));

    // 重复 load 报错（已存在）
    zpm_in(&cfg)
        .args(["framework", "load"])
        .arg(&fw)
        .assert()
        .failure();
    // --force 覆盖成功
    zpm_in(&cfg)
        .args(["framework", "load", "--force"])
        .arg(&fw)
        .assert()
        .success();

    // delete → 目录消失、index.yaml 不再含 fw-a
    zpm_in(&cfg)
        .args(["framework", "delete", "fw-a"])
        .assert()
        .success();
    assert!(!dest.exists(), "delete 后目录应消失");
    let index = fs::read_to_string(repo_dir.join("frameworks/index.yaml")).unwrap();
    assert!(!index.contains("fw-a"), "delete 后索引不应含 fw-a：{index}");

    // 再删 → NotFound（非零退出）
    zpm_in(&cfg)
        .args(["framework", "delete", "fw-a"])
        .assert()
        .failure();
}

/// create fw-b → 脚手架目录与 framework.yaml（name == fw-b）；重复 create 报错
#[test]
fn framework_create_scaffolds() {
    let (cfg, rh) = isolate_with_repo();
    zpm_in(&cfg)
        .args(["framework", "create", "fw-b"])
        .assert()
        .success();
    let dest = rh.path().join("main/frameworks/fw-b");
    assert!(dest.join("framework.yaml").is_file());
    assert!(dest.join("knowledge/rules").is_dir());
    assert!(dest.join("knowledge/lessons").is_dir());
    assert!(dest.join("knowledge/assets").is_dir());
    assert!(dest.join("pages").is_dir());
    let m = fs::read_to_string(dest.join("framework.yaml")).unwrap();
    assert!(m.contains("name: fw-b"), "framework.yaml 应含 name: fw-b：{m}");

    // 重复 create → AlreadyExists（非零退出）
    zpm_in(&cfg)
        .args(["framework", "create", "fw-b"])
        .assert()
        .failure();
}

/// 坏掉的 index.yaml 不影响 list；再 create 后索引恢复可解析且含新旧框架
#[test]
fn framework_list_ignores_broken_index() {
    let (cfg, rh) = isolate_with_repo();
    let fw = make_framework(cfg.path(), "fw-a");
    zpm_in(&cfg)
        .args(["framework", "load"])
        .arg(&fw)
        .assert()
        .success();

    // 手工写坏 index.yaml
    let index = rh.path().join("main/frameworks/index.yaml");
    fs::write(&index, "!!!!").unwrap();
    zpm_in(&cfg)
        .args(["framework", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fw-a"));

    // create fw-c → index.yaml 恢复可解析且含 fw-a、fw-c
    zpm_in(&cfg)
        .args(["framework", "create", "fw-c"])
        .assert()
        .success();
    let txt = fs::read_to_string(&index).unwrap();
    assert!(!txt.contains("!!!!"), "索引应被重建：{txt}");
    assert!(txt.contains("fw-a"), "重建后索引应含 fw-a：{txt}");
    assert!(txt.contains("fw-c"), "重建后索引应含 fw-c：{txt}");
}

/// 手工造本地分享仓库（frameworks/fw-a）：pull 落地激活仓库；本地改动 push 无 --force 失败、
/// --force 更新远端；内容一致时 push 输出"已是最新"且无需 --force
#[test]
fn framework_pull_push_via_local_repo() {
    let (cfg, rh) = isolate_with_repo();
    let repo_dir = rh.path().join("main");

    let share = cfg.path().join("share");
    fs::create_dir_all(share.join("frameworks/fw-a")).unwrap();
    fs::write(
        share.join("frameworks/fw-a/framework.yaml"),
        "name: fw-a\nversion: 0.1.0\ndescription: 分享框架\n",
    )
    .unwrap();
    fs::write(share.join("frameworks/fw-a/page.txt"), "v1").unwrap();

    // pull → 激活仓库出现 fw-a
    zpm_in(&cfg)
        .args(["framework", "pull", "fw-a", "--from"])
        .arg(&share)
        .assert()
        .success();
    let local = repo_dir.join("frameworks/fw-a");
    assert_eq!(
        fs::read_to_string(local.join("page.txt")).unwrap(),
        "v1",
        "pull 后本地应有远端内容"
    );

    // 修改本地一文件 → push 不带 --force 失败（内容不同）
    fs::write(local.join("page.txt"), "v2").unwrap();
    zpm_in(&cfg)
        .args(["framework", "push", "fw-a", "--to"])
        .arg(&share)
        .assert()
        .failure();

    // --force → share 内文件被更新
    zpm_in(&cfg)
        .args(["framework", "push", "fw-a", "--force", "--to"])
        .arg(&share)
        .assert()
        .success();
    assert_eq!(
        fs::read_to_string(share.join("frameworks/fw-a/page.txt")).unwrap(),
        "v2",
        "--force 推送后远端应更新"
    );

    // 内容一致时 push → "已是最新"，无需 --force
    zpm_in(&cfg)
        .args(["framework", "push", "fw-a", "--to"])
        .arg(&share)
        .assert()
        .success()
        .stdout(predicate::str::contains("已是最新"));
}

/// 非法框架名（路径穿越）被拒绝且不产生副作用
#[test]
fn framework_commands_reject_bad_name() {
    let (cfg, _rh) = isolate_with_repo();
    zpm_in(&cfg)
        .args(["framework", "create", "../evil"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("非法框架名"));
}

/// 全新隔离环境（无任何 repo）→ framework 命令非零退出，stderr 指引 zpm repo switch
#[test]
fn commands_fail_without_active_repo() {
    let cfg = tempdir().unwrap();
    let rh = tempdir().unwrap();
    // repo_home 指向独立 tempdir，避免触碰真实 ~/.zpm
    zpm_in(&cfg)
        .args(["config", "set", "repo_home"])
        .arg(rh.path())
        .assert()
        .success();

    zpm_in(&cfg)
        .args(["framework", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("zpm repo switch"));
}
