# zpm

zpm 是一个 Rust 原型工具链：以「仓库（repo）」管理框架集合，从激活仓库的框架初始化项目
（带宪法、知识库与 opencode 能力），提供通用 CRUD API 与静态托管的原型服务器，并沉淀
「经验反哺框架」的闭环。

核心闭环：**repo create → framework load → project create → 建库 → opencode 生成原型 →
浏览器即时查看 → 经验沉淀 → framework knowledge push 反哺框架**。

## 安装

```sh
git clone <本仓库> && cd zpm
cargo install --path crates/zpm-cli        # 编译 release 并安装到 ~/.cargo/bin/zpm
# 后续更新代码后重新安装：cargo install --path crates/zpm-cli --force
```

## 核心概念

- **仓库（repo）**：框架集合的管理单位。所有框架/项目操作都作用于**激活仓库**；
  仓库登记与项目登记存于 CLI 元数据库 `~/.zpm/db/zpm.db`（由 `zpm repo list` 等命令查询）。
- **框架（framework）**：一个含 `framework.yaml`（name/version/description/variables）+
  `knowledge/{rules,lessons,assets}` + `pages/` 的目录，存放在仓库的 `frameworks/` 下。
- **项目（project）**：从激活仓库的某个框架初始化的产物（渲染拷贝 + 知识基线 + AGENTS.md +
  opencode 能力），创建时登记进元数据库。
- **激活仓库**：全局状态，存于配置键 `active_repo`，由 `zpm repo switch` 维护；
  未激活时框架/项目类命令会提示先 switch。

目录布局（`repo_home` 默认 `~/.zpm`，可配置）：

```
~/.zpm/
  db/zpm.db                     ← CLI 元数据库（repos/projects 两表；与项目数据同步的 Store 无关）
  <各 repo 目录>/
    frameworks/
      <name>/framework.yaml     ← 框架元数据
      index.yaml                ← 索引（每次框架变更后自动全量重建，仅快速浏览）
    projects 登记…               ← 项目本体可在任意目录，路径入库
```

## 快速开始

```sh
zpm repo create main                      # 创建仓库（默认 <repo_home>/main）
zpm repo switch main                      # 切换激活仓库
zpm framework load ~/.zpm/frameworks/basecoat   # 从本地目录加载框架
zpm project create demo --framework basecoat   # 从激活仓库的框架创建项目（必填 --framework）
cd demo
zpm db init                               # 建目标库（sqlite 自动创建；pg 验证连接）
zpm db seed seed.sql                      # 导入初始数据（可选）
zpm db sync                               # 数据同步：数据源 → 目标库
zpm serve                                 # 启动原型服务器（默认 0.0.0.0:4310）
# 浏览器打开 http://localhost:4310/index.html，opencode 打开目录开始生成页面
# 经验反哺：zpm framework knowledge pull | push（项目知识 ↔ 仓库框架基线）
```

## 命令

| 命令 | 说明 |
|------|------|
| `zpm repo create <name> [--path <dir>]` / `list` / `switch <名或路径>` | 创建仓库（默认 `<repo_home>/<name>`）/ 列出（激活项标 `*`）/ 切换激活仓库（本质是 `config set active_repo`） |
| `zpm repo delete <名或路径> [--keep-files]` | 删登记 + 默认物理删除目录（`--keep-files` 只删记录）；repo 内含已登记项目或目标是当前目录时拒绝 |
| `zpm repo sync --from <路径\|git-url> [--name <new>]` | 把分享仓库整体导入为新本地 repo（本地路径或 git URL，走 Transport） |
| `zpm framework list` / `load <path> [name] [--force]` | 列出激活仓库的框架 / 从本地目录加载框架（`--force` 覆盖同名） |
| `zpm framework delete <name>` / `create <name>` | 删除框架（物理删除 + 索引重建）/ 初始化框架脚手架（framework.yaml + knowledge 三目录 + pages/） |
| `zpm framework pull <name> --from <repo-ref>` / `push <name> --to <repo-ref>` | 从分享仓库拉取框架（本地已有同名需 `--force`）/ 推送框架到分享仓库（远端内容不同需 `--force`）；来源/目标为本地路径或 git URL |
| `zpm framework knowledge pull\|push\|rule pull\|push\|lesson pull\|push` | 项目知识库 ↔ 激活仓库框架知识基线（基线由项目根 framework.yaml 确定框架；`--from` 可临时指定其它基线；`--dry-run` 预览） |
| `zpm project create [path] --framework <名> [init 选项]` | 从激活仓库的框架创建项目：框架整树渲染拷贝 + 知识基线 + AGENTS.md 宪法 + opencode mcp 注入 + codegraph 索引（best-effort）+ 入库登记（projects 表）。变量优先级：CLI 参数 > framework.yaml > 内置默认 |
| `zpm project delete <名或路径>` | 删除已登记项目（物理删除目录 + 移除记录；当前工作目录拒绝） |
| `zpm db sync [--config <path>] [--tables a,b] [--from <sqlite>]` | 数据同步：zpm.config.yaml 的数据源 → 目标库，shadow-rename 全量替换；`--config` 指定任意位置的配置文件 |
| `zpm db init` / `seed <file>` / `export` / `import <file>` | 目标库初始化 / SQL 导入（分号切分逐条）/ 快照导出 / 快照导入 |
| `zpm serve [--port <p>]` | 原型服务器：静态三层 + `/api` 通用 CRUD + simulated 演示写 + 定时同步（`sync.schedule`）+ Ctrl-C 优雅关闭 |
| `zpm config get/set/list` | 全局配置（`repo_home` / `active_repo` / `assets_dir`） |
| `zpm status` / `zpm version` | 项目状态概览 / 版本号 |

## 典型工作流

### 知识反哺

```
激活仓库 <repo_home>/<repo>/frameworks/<name>/knowledge/   # 基线（framework load/create/pull 维护）
    ▲ zpm framework knowledge push        # 项目里验证过的经验 → 框架基线
    │
项目 knowledge/{rules,lessons,assets}     ← project create 时从框架基线拷入
    └ zpm framework knowledge pull        # 基线更新增量合入项目（基线由项目根 framework.yaml 确定框架；--from 可指定其它基线）
```

全程 markdown，Obsidian 直接打开项目目录即可阅读；清单与 SHA-256 存于 `.zpm/knowledge-manifest.json`
（双表设计：项目侧哈希 + 基线侧哈希，push 三方冲突检测，冲突不写入、`--dry-run` 非零退出）。
子树定向同步：`framework knowledge rule pull|push`、`lesson pull|push`。

### 框架分享与同步

```sh
# 整仓导入：把别人分享的 zpm 仓库变成本地一个新 repo（自动剔除源里的 db/ 与 .git/）
zpm repo sync --from ~/shared/zpm-repo --name borrowed
zpm repo sync --from https://github.com/someone/zpm-repo.git   # 名字默认取 URL 末段

# 单框架流动：只取/只推 frameworks/<name> 子树
zpm framework pull basecoat --from ~/shared/zpm-repo          # 本地同名已存在时需 --force
zpm framework push basecoat --to git@github.com:me/repo.git   # 远端内容不同时需 --force
```

引用识别规则：含 `://`、`git@` 前缀或 `.git` 后缀 → Git 后端（shell-out 系统 `git`，
完整复用你机器上的凭据与代理配置）；否则按本地路径处理。所有删除类命令自带安全阀
（登记引用检查、当前工作目录拒绝），框架名与仓库引用拒绝 `..` 与路径穿越。

## 服务器（zpm serve）

- **静态三层**：`pages/` 根映射（`/x.html` ↔ `pages/x.html`，SPA 兜底）、`/vendor/`（严格 404）、`/theme.css`（主题 tokens）
- **通用 CRUD**：`/api/<表>` 列表（`where=字段:操作符:值`、`sort`、分页）+ 单条 + POST/PATCH/DELETE；`/api/_meta` 元信息；写路径受 `write.tables` 白名单约束
- **simulated 演示写**：`write.mode: simulated` 时写操作落内存会话（按 `zpm_sid` 隔离），目标库只读，`/api/_session/reset` 清空
- **双方言**：目标库与数据源均支持 sqlite / postgres，按 `zpm.config.yaml` 切换

## 配置

项目配置 `zpm.config.yaml`（字段与 TS 版兼容，datasource/target 支持 sqlite 与 postgres）：

```yaml
name: demo
server: { port: 4310, host: 0.0.0.0, theme: default }
datasource: { type: postgres, host: localhost, port: 5432, database: app, user: u, password: p }
target: { type: sqlite, file: data/app.db }
sync: { schedule: "0 3 * * *", tables: [{ name: users, primary_key: id }] }
write: { enabled: true, mode: simulated, tables: [users] }
# features: { db_info: true, theme_studio: true }  # 页面功能开关（可选，缺省全 true）
```

`features` 为可选节：`db_info` 关闭数据字典页与菜单入口，`theme_studio` 关闭主题配置页与菜单入口；
整节/字段缺省一律 `true`（fail-open），显式 `false` 后经 `/api/_meta` 与 `/api/_dict` 的
`features` 字段透出，由外壳隐藏对应入口。

全局配置 `~/.config/zpm/config.toml`（macOS：`~/Library/Application Support/zpm/config.toml`），
可用 `ZPM_CONFIG_HOME` 覆盖：

| 键 | 默认 | 说明 |
|---|---|---|
| `repo_home` | `~/.zpm` | repo 主目录，元数据库位于 `<repo_home>/db/zpm.db` |
| `active_repo` | （空） | 当前激活仓库，由 `zpm repo switch` 维护 |
| `assets_dir` | （空） | 资产源目录 |

## 从旧版本迁移

| 旧命令 / 旧约定 | 新命令 / 新约定 |
|---|---|
| `zpm init --framework X demo` | `zpm project create demo --framework X`（`--framework` 必填；`--name --no-mcp --mcp --port --host --dbport --database --user --password` 沿用） |
| `zpm sync [--tables a,b] [--from <sqlite>]` | `zpm db sync`（同名参数沿用；新增 `--config <path>` 指定任意位置的配置文件） |
| `zpm knowledge pull\|push` | `zpm framework knowledge pull\|push`（基线推导从 assets_dir 改为激活仓库框架） |
| `zpm knowledge rule\|lesson pull\|push` | `zpm framework knowledge rule\|lesson pull\|push`（机制不变） |
| `zpm framework import <path>` | `zpm framework load <path>` |
| 基线固定在 `~/.zpm/frameworks/<名>/` | 基线在激活仓库 `<repo_home>/<repo>/frameworks/<名>/`；旧目录用 `zpm framework load ~/.zpm/frameworks/<名>` 一次性迁入 |
| 全局配置仅 `assets_dir` | 新增 `repo_home` / `active_repo` 两键 |

## 构建与测试

```sh
cargo build && cargo test     # 全套件（含 static/api/session 集成测试）
bash scripts/e2e-smoke.sh     # 端到端冒烟（repo→framework→project→db→serve→HTTP 断言，需先 cargo build）
ZPM_TEST_PG="postgres://..." cargo test -p zpm-core store::postgres   # pg 门控测试（需真库）
```

## 结构

- `crates/zpm-meta`：CLI 元数据库（`<repo_home>/db/zpm.db`：repos/projects）与传输抽象（Local / Git shell-out）
- `crates/zpm-core`：配置、framework 渲染、knowledge 回路、Store 抽象（sqlite/pg）、数据同步、快照、mcp 注入
- `crates/zpm-server`：axum 服务器（静态三层、通用 CRUD、simulated 会话层、定时调度）
- `crates/zpm-cli`：命令行入口（一命令一 `cmd_*.rs` 薄层）

## 路线图

git 命令族（commit/PR/tag）、`zpm docker`（镜像/部署/离线包）、旧 TS 项目迁移助手、`project list`、复合主键同步。
