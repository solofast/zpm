# AGENTS.md — zpm 项目指南

本文件面向在本仓库工作的 AI 代理与人类协作者，概述项目结构、构建/测试方式、命令树与代码约定。

## 项目结构

Rust workspace（edition 2021），四个 crate。依赖方向：`zpm-cli → zpm-core → zpm-meta`，
另有 `zpm-cli → zpm-server`（`zpm serve` 命令本体在 zpm-server crate）。

| crate | 职责 |
|---|---|
| `crates/zpm-cli` | 命令行入口：clap 4 derive，**一命令一 `cmd_*.rs` 薄层文件**（解析参数 → 调 core/meta → 打印），`main.rs` 只做命令表分发 + `report()` 退出码映射 |
| `crates/zpm-server` | axum 原型服务器：静态三层、`/api` 通用 CRUD、simulated 会话层、定时调度（由 `zpm serve` 挂载） |
| `crates/zpm-core` | 领域逻辑：全局配置（`~/.config/zpm/config.toml`）、framework 渲染拷贝（resolve_in/validate_name/rebuild_index/dir_hash/copy_tree）、knowledge 回路（manifest 双表 + 三方冲突）、Store 抽象（sqlite/pg 双方言）、数据同步（shadow-rename）、快照、mcp 注入 |
| `crates/zpm-meta` | CLI 元数据库（`<repo_home>/db/zpm.db`：repos/projects 两表，rusqlite 直连 + `PRAGMA user_version` 迁移）与传输抽象（`transport.rs`：Local walkdir 复制 / Git shell-out 系统 `git`，禁止 git2） |

`~/.zpm` 目录布局（`repo_home` 默认值，可在配置中改）：

```
~/.zpm/
  db/zpm.db                ← CLI 元数据库（zpm-meta 管理，与项目数据同步的 Store 无关）
  <各 repo 目录>/
    frameworks/
      <name>/framework.yaml   ← 框架元数据（name/version/description/variables）
      index.yaml              ← 索引（每次框架变更后全量重建，仅快速浏览，list 以目录扫描为准）
```

激活 repo 存于全局配置 `active_repo` 键；所有框架/项目操作都作用于激活 repo。

## 构建与测试

```sh
cargo build && cargo test     # 全套件（含 static/api/session 集成测试；server 测试不动全局状态）
bash scripts/e2e-smoke.sh     # 端到端冒烟：repo→framework→project→db→serve→HTTP 断言（需先 cargo build；全程临时目录隔离）
ZPM_TEST_PG="postgres://..." cargo test -p zpm-core store::postgres   # pg 门控测试（需真库）
```

- 集成测试（`crates/zpm-cli/tests/*.rs`）用 assert_cmd + 独立 tempdir + `ZPM_CONFIG_HOME` 环境变量隔离；承载 fixture/项目目录的 TempDir 必须保活到测试结束（返回或绑定）
- 涉及环境变量的 core 测试用 `crate::testing::ENV_LOCK` 互斥
- 规格与实现计划存于 `docs/superpowers/{specs,plans}/`

## 命令树

```
zpm repo      create <name> [--path <dir>] | list | switch <name-or-path>
              | delete <name-or-path> [--keep-files] | sync --from <路径|git-url> [--name <new>]
zpm framework list | load <path> [name] [--force] | delete <name> | create <name>
              | pull <name> --from <repo-ref> [--force] | push <name> --to <repo-ref> [--force]
              | knowledge pull|push|rule pull|push|lesson pull|push [--from <path>] [--dry-run] [--all] [paths...]
zpm project   create [path] --framework <name> [init 选项] | delete <name-or-path>
zpm db        sync [--config <path>] [--tables <a,b>] [--from <sqlite>]
              | init | seed <file> | export [file] | import <file>
zpm serve     [--port <p>]
zpm config    get|set|list        （键：repo_home / active_repo / assets_dir）
zpm status | zpm version
```

## 代码约定

- **分层**：薄 CLI + core/meta 纯函数。core 不依赖 clap；复杂逻辑不进 `cmd_*.rs`
- **错误**：一律中文文案。双层模型：`zpm_meta::MetaError`（元数据库/传输层）经 `From` 自动转 `zpm_core::ZpmError`；CLI 里直接 `?`
- **退出码**：业务错误 = 1，clap 用法错误 = 2；`ZpmError::exit_code()` 的 match 结构保留以便细分
- **传输**：Git 后端只能 shell-out 系统 `git`（复用用户凭据/代理），失败时 `TransportError` 带 stderr；Local 复制跳过 `db/` 与 `.git/`
- **安全红线**：框架名过 `validate_name`（禁 `..` 与路径分隔符）；transport subpath 拒绝 `..` 与绝对路径；删除类命令拒绝当前工作目录
- **破坏性操作**：物理删除前先查登记（repo 内含 projects、框架被项目引用等场景给警告或拒绝）
- **提交**：conventional commits + 中文描述（如 `feat(cli): ...`、`fix(meta): ...`、`docs(spec): ...`）
- **测试卫生**：测试断言真实行为（文件系统/数据库回读），不做 mock；测试输出必须干净（无告警噪声）
