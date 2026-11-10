//! `zpm repo` 五子命令集成测试：每个测试独立 tempdir + ZPM_CONFIG_HOME 隔离，
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

/// 建立隔离环境：cfg 放配置文件，repo_home 指向独立的 rh tempdir。
fn isolate() -> (TempDir, TempDir) {
    let cfg = tempdir().unwrap();
    let rh = tempdir().unwrap();
    zpm_in(&cfg)
        .args(["config", "set", "repo_home"])
        .arg(rh.path())
        .assert()
        .success();
    (cfg, rh)
}

/// create → list → switch（config get 断言）→ delete --keep-files → 再 delete 失败
#[test]
fn repo_create_list_switch_delete_flow() {
    let (cfg, rh) = isolate();
    let repo_dir = rh.path().join("main");

    zpm_in(&cfg)
        .args(["repo", "create", "main", "--path"])
        .arg(&repo_dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("已创建仓库"));
    assert!(repo_dir.join("frameworks").is_dir());
    assert!(repo_dir.join("frameworks/index.yaml").is_file());

    zpm_in(&cfg)
        .args(["repo", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("main"));

    zpm_in(&cfg)
        .args(["repo", "switch", "main"])
        .assert()
        .success()
        .stdout(predicate::str::contains("已切换激活仓库"));
    zpm_in(&cfg)
        .args(["config", "get", "active_repo"])
        .assert()
        .success()
        .stdout(predicate::str::contains("main"));

    // list 中激活项带 * 标记
    zpm_in(&cfg)
        .args(["repo", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("*main"));

    zpm_in(&cfg)
        .args(["repo", "delete", "main", "--keep-files"])
        .assert()
        .success()
        .stdout(predicate::str::contains("（文件保留）"));
    assert!(repo_dir.is_dir(), "--keep-files 应保留目录");

    // 记录已删，再删应 NotFound
    zpm_in(&cfg)
        .args(["repo", "delete", "main"])
        .assert()
        .failure();
}

/// delete 不带 --keep-files：目录物理删除 + 登记记录消失
#[test]
fn repo_delete_without_keep_files_removes_dir() {
    let (cfg, rh) = isolate();
    let repo_dir = rh.path().join("gone");

    zpm_in(&cfg)
        .args(["repo", "create", "gone", "--path"])
        .arg(&repo_dir)
        .assert()
        .success();

    zpm_in(&cfg)
        .args(["repo", "delete", "gone"])
        .assert()
        .success()
        .stdout(predicate::str::contains("已删除仓库"))
        .stdout(predicate::str::contains("文件保留").not());

    assert!(!repo_dir.exists(), "物理删除后目录应消失");

    // 登记记录已删：list 不再显示
    zpm_in(&cfg)
        .args(["repo", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gone").not());
}

/// repo 目录即 cwd 时 delete 被拒绝，记录保留
#[test]
fn repo_delete_refuses_cwd() {
    let (cfg, _rh) = isolate();
    let work = tempdir().unwrap();

    zpm_in(&cfg)
        .args(["repo", "create", "here", "--path"])
        .arg(work.path())
        .assert()
        .success();

    zpm_in(&cfg)
        .current_dir(work.path())
        .args(["repo", "delete", "here"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("当前工作目录"));

    // 拒绝后登记记录仍在
    zpm_in(&cfg)
        .args(["repo", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("here"));
}

/// 相对路径 canonicalize 入库；同名/同路径重复登记被拒绝
#[test]
fn repo_create_canonicalizes_and_dedups() {
    let (cfg, _rh) = isolate();
    let cfg_canon = cfg.path().canonicalize().unwrap();
    let canonical = cfg_canon.join("shared");

    zpm_in(&cfg)
        .current_dir(cfg.path())
        .args(["repo", "create", "one", "--path", "shared"])
        .assert()
        .success();

    // list 显示的是 canonicalize 后的绝对路径
    zpm_in(&cfg)
        .args(["repo", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains(canonical.display().to_string()));

    // 同名第二次（相对路径）→ 拒绝
    zpm_in(&cfg)
        .current_dir(cfg.path())
        .args(["repo", "create", "one", "--path", "shared"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("已登记"));

    // 同路径不同名 → 拒绝
    zpm_in(&cfg)
        .current_dir(cfg.path())
        .args(["repo", "create", "two", "--path", "shared"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("已登记"));
}

/// 从本地分享仓库 sync 镜像：登记 + frameworks 索引重建
#[test]
fn repo_sync_from_local_path() {
    let (cfg, rh) = isolate();

    // 手工造分享仓库目录：frameworks/basecoat-pm/framework.yaml
    let src = cfg.path().join("shared-src");
    fs::create_dir_all(src.join("frameworks/basecoat-pm")).unwrap();
    fs::write(
        src.join("frameworks/basecoat-pm/framework.yaml"),
        "name: basecoat-pm\nversion: 0.1.0\ndescription: 测试框架\n",
    )
    .unwrap();

    zpm_in(&cfg)
        .args(["repo", "sync", "--from"])
        .arg(&src)
        .args(["--name", "mirror"])
        .assert()
        .success()
        .stdout(predicate::str::contains("已创建仓库"));

    zpm_in(&cfg)
        .args(["repo", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("mirror"));

    let index = rh.path().join("mirror/frameworks/index.yaml");
    assert!(index.is_file(), "mirror/frameworks/index.yaml 应存在");
    let txt = fs::read_to_string(index).unwrap();
    assert!(txt.contains("basecoat-pm"), "索引应含 basecoat-pm：{txt}");
}
