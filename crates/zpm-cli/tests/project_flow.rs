//! `zpm project` 集成测试：框架解析（激活仓库）、变量优先级、防覆盖、知识清单、
//! mcp 注入、入库登记与删除（含当前工作目录安全阀）。

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use tempfile::{tempdir, TempDir};

fn zpm() -> Command {
    Command::cargo_bin("zpm").unwrap()
}

/// 每条命令都注入 ZPM_CONFIG_HOME（指向 cfg tempdir），进程级隔离
fn zpm_env(cfg: &TempDir) -> Command {
    let mut c = zpm();
    c.env("ZPM_CONFIG_HOME", cfg.path());
    c
}

/// 造一个 fixture 框架：{{name}}/{{port}} 模板、二进制 png、variables{port:4500}、
/// knowledge 基线一条、空 .opencode/opencode.json（供 mcp 注入）、AGENTS.md
fn make_framework(dir: &std::path::Path) {
    fs::create_dir_all(dir.join("knowledge/rules")).unwrap();
    fs::create_dir_all(dir.join(".opencode")).unwrap();
    fs::write(
        dir.join("framework.yaml"),
        "name: fixture-fw\nversion: 0.3.0\nvariables:\n  port: \"4500\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("zpm.config.yaml.tmpl"),
        "name: {{name}}\nserver:\n  port: {{port}}\n  host: 0.0.0.0\n  theme: default\ndatasource:\n  type: sqlite\n  file: data/source.db\ntarget:\n  type: sqlite\n  file: data/app.db\nsync:\n  tables: []\n",
    )
    .unwrap();
    // 二进制直拷：含 {{name}} 字节但必须原样保留
    fs::write(dir.join("logo.png"), b"\x89PNG\r\n{{name}}\x1a\n").unwrap();
    fs::write(dir.join("knowledge/rules/base.md"), "基线规则").unwrap();
    fs::write(dir.join(".opencode/opencode.json"), "{}").unwrap();
    fs::write(dir.join("AGENTS.md"), "# fixture 宪法\n").unwrap();
}

/// 搭建激活仓库环境：独立 config/repo_home → repo create main → switch → load fixture
/// 返回 (cfg, rh) 两个 TempDir 保活至测试结束
fn setup_repo(fw_src: &std::path::Path) -> (TempDir, TempDir) {
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
    zpm_env(&cfg).args(["framework", "load"]).arg(fw_src).assert().success();
    (cfg, rh)
}

fn projects_count(db: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.query_row("SELECT count(*) FROM projects", [], |r| r.get(0)).unwrap()
}

/// ① 创建 + 登记：目录结构、framework.yaml 拷入项目根、AGENTS.md、projects 表四字段
#[test]
fn project_create_requires_framework_and_registers() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fixture-fw");
    make_framework(&fw);
    let (cfg, rh) = setup_repo(&fw);

    let proj = tmp.path().join("demo");
    let mut cmd = zpm_env(&cfg);
    cmd.args(["project", "create"]).arg(&proj).args(["--framework", "fixture-fw"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("已登记项目 demo（framework=fixture-fw，repo=main）"));

    for d in ["pages", "data", ".zpm", "knowledge/rules", "knowledge/lessons", "knowledge/assets"] {
        assert!(proj.join(d).is_dir(), "缺目录 {d}");
    }
    assert!(proj.join("framework.yaml").is_file(), "框架元数据应拷入项目根");
    assert!(proj.join("AGENTS.md").is_file(), "框架 AGENTS.md 应拷入");
    let cfg_yaml =
        zpm_core::project_config::load_project_config(&proj.join("zpm.config.yaml")).unwrap();
    assert_eq!(cfg_yaml.name, "demo");
    assert_eq!(cfg_yaml.server.port, 4500, "manifest variables 应生效");

    // projects 表登记：name/path/framework/repo 四字段（元数据库固定在 repo_home/db/zpm.db）
    let db = rh.path().join("db").join("zpm.db");
    let conn = rusqlite::Connection::open(db).unwrap();
    let (name, path, framework, repo): (String, String, String, String) = conn
        .query_row(
            "SELECT name, path, framework, repo FROM projects",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(name, "demo");
    assert!(path.ends_with("demo"), "入库 path 应为绝对路径：{path}");
    assert_eq!(framework, "fixture-fw");
    assert_eq!(repo, "main");
}

/// ② 框架不存在：报错并列出可用框架清单
#[test]
fn project_create_unknown_framework_lists_available() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fixture-fw");
    make_framework(&fw);
    let (cfg, _rh) = setup_repo(&fw);

    let proj = tmp.path().join("demo");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "nope"])
        .assert()
        .failure()
        .code(predicate::eq(1))
        .stderr(
            predicate::str::contains("可用内置").and(predicate::str::contains("fixture-fw")),
        );
}

/// ③ 防覆盖：同目录二次 create → 退出码 1 且既有配置未被改
#[test]
fn project_create_refuses_existing_project() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fixture-fw");
    make_framework(&fw);
    let (cfg, _rh) = setup_repo(&fw);

    let proj = tmp.path().join("demo");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fixture-fw"])
        .assert()
        .success();
    let before = fs::read_to_string(proj.join("zpm.config.yaml")).unwrap();

    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fixture-fw"])
        .assert()
        .failure()
        .code(predicate::eq(1));
    assert_eq!(
        fs::read_to_string(proj.join("zpm.config.yaml")).unwrap(),
        before,
        "防覆盖失败时不得改动既有配置"
    );
}

/// ④ 变量优先级：variables(4500) 覆盖 DEFAULTS(4310)；CLI --port 4600 最高；二进制直拷
#[test]
fn project_create_variable_priority() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fixture-fw");
    make_framework(&fw);
    let (cfg, _rh) = setup_repo(&fw);

    let proj_a = tmp.path().join("proj-a");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj_a)
        .args(["--framework", "fixture-fw", "--name", "demo"])
        .assert()
        .success();
    let cfg_a =
        zpm_core::project_config::load_project_config(&proj_a.join("zpm.config.yaml")).unwrap();
    assert_eq!(cfg_a.server.port, 4500, "variables 应优先于 DEFAULTS");
    assert_eq!(cfg_a.name, "demo");
    let png = fs::read(proj_a.join("logo.png")).unwrap();
    assert!(png.windows(8).any(|w| w == b"{{name}}"), "二进制文件不得被渲染");

    let proj_b = tmp.path().join("proj-b");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj_b)
        .args(["--framework", "fixture-fw", "--name", "demo", "--port", "4600"])
        .assert()
        .success();
    let cfg_b =
        zpm_core::project_config::load_project_config(&proj_b.join("zpm.config.yaml")).unwrap();
    assert_eq!(cfg_b.server.port, 4600, "CLI 显式参数优先级最高");
}

/// codegraph init 运行失败（非零退出）只提示不阻断，且不得误报"已建立"
#[test]
fn project_create_codegraph_failure_prints_hint() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fixture-fw");
    make_framework(&fw);
    let (cfg, _rh) = setup_repo(&fw);

    let bin = tmp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("codegraph");
    fs::write(&fake, "#!/bin/sh\nexit 3\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

    let proj = tmp.path().join("proj");
    let path_var = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut cmd = zpm_env(&cfg);
    cmd.env("PATH", &path_var)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fixture-fw"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("codegraph init 运行失败")
                .and(predicate::str::contains("codegraph 索引已建立").not()),
        );
}

/// ⑤ 知识清单：创建后存在且条目数 = knowledge/ 文件数（含框架基线拷入）
#[test]
fn project_create_seeds_knowledge_manifest() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fixture-fw");
    make_framework(&fw);
    let (cfg, _rh) = setup_repo(&fw);

    let proj = tmp.path().join("proj");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fixture-fw", "--name", "demo"])
        .assert()
        .success();

    let m: Value =
        serde_json::from_str(&fs::read_to_string(proj.join(".zpm/knowledge-manifest.json")).unwrap())
            .unwrap();
    let entries = m["files"].as_object().unwrap();
    assert_eq!(entries.len(), 1, "基线一条规则 → 清单一条目：{entries:?}");
    assert!(entries.contains_key("knowledge/rules/base.md"));
}

/// ⑥ mcp 注入：framework 自带 .opencode/opencode.json 时注入三条能力；--no-mcp 不注入
#[test]
fn project_create_injects_mcp_entries() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fixture-fw");
    make_framework(&fw);
    let (cfg, _rh) = setup_repo(&fw);

    let proj = tmp.path().join("proj");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fixture-fw", "--name", "demo"])
        .assert()
        .success();

    let v: Value =
        serde_json::from_str(&fs::read_to_string(proj.join(".opencode/opencode.json")).unwrap())
            .unwrap();
    // 兼容 v1（mcp.<名>）与 v2（mcp.servers.<名>）：按本机探测结果取节点
    let mcp_node = v["mcp"]
        .as_object()
        .filter(|m| !m.contains_key("servers"))
        .map(|_| &v["mcp"])
        .unwrap_or(&v["mcp"]["servers"]);
    for name in ["codegraph", "dbx", "playwright"] {
        assert_eq!(
            mcp_node[name]["type"], "local",
            "{name} 条目形状不对：{}",
            mcp_node[name]
        );
        assert!(mcp_node[name]["command"].as_array().is_some(), "{name} 缺 command");
    }

    // --no-mcp：不注入
    let proj2 = tmp.path().join("proj-nomcp");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj2)
        .args(["--framework", "fixture-fw", "--name", "demo", "--no-mcp"])
        .assert()
        .success();
    let v2: Value =
        serde_json::from_str(&fs::read_to_string(proj2.join(".opencode/opencode.json")).unwrap())
            .unwrap();
    let no_codegraph = v2["mcp"].get("codegraph").is_none()
        && v2["mcp"]["servers"].get("codegraph").is_none();
    assert!(no_codegraph, "--no-mcp 时不得注入条目");
}

/// ⑦ 删除：按名字与按路径删除（目录+记录消失）；在项目目录内删除本项目被拒
#[test]
fn project_delete_by_name_and_by_path_refuses_cwd() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fixture-fw");
    make_framework(&fw);
    let (cfg, rh) = setup_repo(&fw);
    let db = rh.path().join("db").join("zpm.db");

    // 创建 → 按名字删除
    let proj = tmp.path().join("demo");
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fixture-fw"])
        .assert()
        .success();
    zpm_env(&cfg).args(["project", "delete", "demo"]).assert().success();
    assert!(!proj.exists(), "按名字删除后目录应消失");
    assert_eq!(projects_count(&db), 0, "按名字删除后登记应消失");

    // 再创建 → 按绝对路径删除
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fixture-fw"])
        .assert()
        .success();
    let mut cmd = zpm_env(&cfg);
    cmd.args(["project", "delete"]).arg(&proj);
    cmd.assert().success();
    assert!(!proj.exists(), "按路径删除后目录应消失");
    assert_eq!(projects_count(&db), 0, "按路径删除后登记应消失");

    // 再创建 → 站在项目目录内删除本项目 → 拒绝
    zpm_env(&cfg)
        .args(["project", "create"])
        .arg(&proj)
        .args(["--framework", "fixture-fw"])
        .assert()
        .success();
    zpm()
        .env("ZPM_CONFIG_HOME", cfg.path())
        .current_dir(&proj)
        .args(["project", "delete", "demo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("当前工作目录"));
    assert!(proj.exists(), "被拒后目录不得删除");
}

/// ⑧ 删除未登记路径：报错含"未登记"
#[test]
fn project_delete_unregistered_path_errors() {
    let tmp = tempdir().unwrap();
    let fw = tmp.path().join("fixture-fw");
    make_framework(&fw);
    let (cfg, _rh) = setup_repo(&fw);

    let nowhere = tmp.path().join("nowhere");
    let mut cmd = zpm_env(&cfg);
    cmd.args(["project", "delete"]).arg(&nowhere);
    cmd.assert().failure().stderr(predicate::str::contains("未登记"));
}
