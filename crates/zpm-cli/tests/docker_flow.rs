//! `zpm docker` 集成测试：init 生成/拒绝覆盖/force；build/up/down 经假 docker 记录 argv。
//! 假 docker 是真实可执行脚本（shell-out 边界的真实行为），不做 mock。
// 假 docker 依赖 /bin/sh 脚本与 unix 权限位：非 unix 平台整文件跳过编译
#![cfg(unix)]
use assert_cmd::Command;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::TempDir;

/// 最小项目 fixture：zpm.config.yaml + pages/index.html
fn fixture(root: &Path) {
    fs::create_dir_all(root.join("pages")).unwrap();
    fs::write(
        root.join("zpm.config.yaml"),
        r#"
name: demo
server: { port: 4599 }
datasource: { type: sqlite, file: data/app.db }
target: { type: sqlite, file: data/app.db }
sync: { tables: [] }
docker: { image: demo-img, tag: t1, platform: "linux/arm64" }
"#,
    )
    .unwrap();
    fs::write(root.join("pages/index.html"), "hi").unwrap();
}

fn zpm() -> Command {
    Command::cargo_bin("zpm").unwrap()
}

/// 假 docker：argv 追加写入 $DOCKER_LOG，永远成功
fn fake_docker() -> (TempDir, TempDir) {
    let bin = TempDir::new().unwrap();
    let log = TempDir::new().unwrap();
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}/argv.log\nexit 0\n",
        log.path().display()
    );
    let p = bin.path().join("docker");
    fs::write(&p, script).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

/// PATH = 假 docker 目录前置（保留原 PATH 供 tar/curl 等使用）
fn cmd_with_fake(bin: &TempDir) -> Command {
    let orig = std::env::var("PATH").unwrap();
    let mut cmd = zpm();
    cmd.env("PATH", format!("{}:{orig}", bin.path().display()));
    cmd
}

fn read_log(log: &TempDir) -> Vec<String> {
    fs::read_to_string(log.path().join("argv.log"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn init_generates_three_files_refuses_then_forces() {
    let proj = TempDir::new().unwrap();
    fixture(proj.path());
    zpm().current_dir(proj.path()).args(["docker", "init"]).assert().success();
    let dockerfile = fs::read_to_string(proj.path().join("Dockerfile")).unwrap();
    assert!(dockerfile.contains("FROM debian:bookworm-slim"));
    assert!(dockerfile.contains("COPY zpm-aarch64-unknown-linux-musl.tar.gz")); // 配置 platform=linux/arm64
    assert!(dockerfile.contains("EXPOSE 4599"));
    let compose = fs::read_to_string(proj.path().join("docker-compose.yml")).unwrap();
    assert!(compose.contains("image: demo-img:t1"));
    assert!(compose.contains("platform: linux/arm64"));
    assert!(compose.contains("[\"4599:4599\"]"));
    assert!(proj.path().join(".dockerignore").exists());

    // 再次 init → 拒绝（含 --force 提示）
    let out = zpm().current_dir(proj.path()).args(["docker", "init"]).assert().failure();
    let err = String::from_utf8_lossy(&out.get_output().stderr);
    assert!(err.contains("已存在"), "实际：{err}");
    assert!(err.contains("--force"), "应有 --force 提示：{err}");

    // init --force 成功
    zpm().current_dir(proj.path()).args(["docker", "init", "--force"]).assert().success();
}

#[test]
fn init_project_outside_errors() {
    let dir = TempDir::new().unwrap();
    let out = zpm().current_dir(dir.path()).args(["docker", "init"]).assert().failure();
    let err = String::from_utf8_lossy(&out.get_output().stderr);
    assert!(err.contains("不在 zpm 项目内"), "实际：{err}");
}

#[test]
fn build_stages_binary_and_invokes_fake_docker() {
    let proj = TempDir::new().unwrap();
    fixture(proj.path());
    let (bin, log) = fake_docker();
    // build 前预检要求 Dockerfile 已生成（I-3：先 init 再 build）
    zpm().current_dir(proj.path()).args(["docker", "init"]).assert().success();
    // 假二进制
    let fake_bin = proj.path().join("fake-zpm-bin");
    fs::write(&fake_bin, b"fake-elf").unwrap();
    cmd_with_fake(&bin)
        .current_dir(proj.path())
        .args(["docker", "build", "--binary"])
        .arg(&fake_bin)
        .assert()
        .success();
    // 归档按配置平台命名
    assert!(proj.path().join("zpm-aarch64-unknown-linux-musl.tar.gz").exists());
    // 假 docker 收到 build argv（ensure_available 的 version 调用也入日志，用 contains 断言）
    let logv = read_log(&log);
    assert!(logv.contains(&"build".to_string()), "日志：{:?}", logv);
    let pos = logv.iter().position(|l| l == "build").unwrap();
    assert_eq!(&logv[pos + 1..pos + 6], &[
        "--platform".to_string(),
        "linux/arm64".to_string(),
        "-t".to_string(),
        "demo-img:t1".to_string(),
        ".".to_string()
    ]);
}

#[test]
fn up_down_invoke_compose_and_drift_warning() {
    let proj = TempDir::new().unwrap();
    fixture(proj.path());
    let (bin, log) = fake_docker();
    let mut cmd = cmd_with_fake(&bin);
    cmd.current_dir(proj.path()).args(["docker", "init"]).assert().success();
    // up：compose up -d
    cmd_with_fake(&bin).current_dir(proj.path()).args(["docker", "up"]).assert().success();
    let logv = read_log(&log);
    // 假 docker 会记录 ensure_available 与 compose 探测的 version 调用，用连续窗口匹配 up 段
    assert!(
        logv.windows(3).any(|w| w == ["compose", "up", "-d"]),
        "日志：{:?}",
        logv
    );
    // 改端口 → up 打印漂移警告但仍执行
    let cfg_path = proj.path().join("zpm.config.yaml");
    let cfg = fs::read_to_string(&cfg_path).unwrap().replace("4599", "5000");
    fs::write(&cfg_path, cfg).unwrap();
    let out = cmd_with_fake(&bin)
        .current_dir(proj.path())
        .args(["docker", "up"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(stdout.contains("警告"), "实际：{stdout}");
    // down
    cmd_with_fake(&bin).current_dir(proj.path()).args(["docker", "down"]).assert().success();
    let logv = read_log(&log);
    assert!(logv.contains(&"down".to_string()));
}

#[test]
fn build_without_docker_errors_chinese() {
    let proj = TempDir::new().unwrap();
    fixture(proj.path());
    let empty_path = TempDir::new().unwrap();
    let out = zpm()
        .env("PATH", empty_path.path())
        .current_dir(proj.path())
        .args(["docker", "build", "--binary", "/dev/null"])
        .assert()
        .failure();
    let err = String::from_utf8_lossy(&out.get_output().stderr);
    assert!(err.contains("未检测到 docker"), "实际：{err}");
}

#[test]
fn build_warns_when_cli_platform_differs_from_disk_compose() {
    let proj = TempDir::new().unwrap();
    fixture(proj.path());
    let (bin, _log) = fake_docker();
    // init：配置 platform=linux/arm64 → 磁盘 compose 的 platform 行是 linux/arm64
    zpm().current_dir(proj.path()).args(["docker", "init"]).assert().success();
    // 手改 Dockerfile 的 COPY 目标为 amd64，模拟「Dockerfile 已按 amd64 重建而 compose 未重建」，
    // 使 I-3 预检放行、单独暴露 I-2 的 compose 漂移警告路径
    let df = proj.path().join("Dockerfile");
    let txt = fs::read_to_string(&df)
        .unwrap()
        .replace("aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl");
    fs::write(&df, txt).unwrap();
    let fake_bin = proj.path().join("fake-zpm-bin");
    fs::write(&fake_bin, b"fake-elf").unwrap();
    let out = cmd_with_fake(&bin)
        .current_dir(proj.path())
        .args(["docker", "build", "--platform", "linux/amd64", "--binary"])
        .arg(&fake_bin)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout);
    assert!(stdout.contains("不一致"), "实际：{stdout}");
    assert!(stdout.contains("docker-compose.yml 的 platform（linux/arm64）"), "实际：{stdout}");
    assert!(stdout.contains("zpm docker init --platform linux/amd64 --force"), "实际：{stdout}");
}
