//! zpm.config.yaml 解析：兼容 TS 版 v0.2 schema，datasource/target 支持 sqlite 扩展。
//!
//! 解析分三步（zod 式校验）：
//! 1. `serde_yaml` 读成 `Value` 并做形状预校验（产出带字段路径的错误，如 `sync.tables`、
//!    `datasource.type`——serde 自身的报错不含字段路径）；
//! 2. 反序列化为 `ProjectConfig`（默认值、password 键存在性在此层由 serde 保证）；
//! 3. 语义校验（name 非空、ident 正则、port 1..=65535）。

use crate::error::ZpmError;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// 项目配置文件名（v0.2：由 zpm.toml 迁移为 zpm.config.yaml）
pub const PROJECT_FILE: &str = "zpm.config.yaml";
/// 项目内 zpm 数据目录名
pub const ZPM_DIR: &str = ".zpm";
/// zpm 数据目录下的资产子目录名
pub const ASSETS_SUBDIR: &str = "assets";

/// 合法标识符正则。任务 3 的 store 模块（safe_ident）将复用此常量。
pub const IDENT_RE: &str = "^[A-Za-z_][A-Za-z0-9_]*$";

/// 判断是否匹配 IDENT_RE（字符级等价实现，避免引入 regex 依赖）
pub fn is_safe_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// [server] 节。缺省时整节取默认：4310 / "0.0.0.0" / "default"。
/// 字段级默认与 TS zod 对齐（节存在但缺 host/theme 时分别回落，框架模板常省略 host）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServerCfg {
    #[serde(default = "default_server_port")]
    pub port: u16,
    #[serde(default = "default_server_host")]
    pub host: String,
    #[serde(default = "default_server_theme")]
    pub theme: String,
}

fn default_server_port() -> u16 {
    4310
}

fn default_server_host() -> String {
    "0.0.0.0".into()
}

fn default_server_theme() -> String {
    "default".into()
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            port: default_server_port(),
            host: default_server_host(),
            theme: default_server_theme(),
        }
    }
}

/// postgres 连接参数。port 缺省 5432。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PgConn {
    pub host: String,
    #[serde(default = "default_pg_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    /// 键缺失 → `missing field password`（deserialize_with 语义，与 serde 对 Option 的
    /// 隐式 None 默认不同）；显式 null → None；有值 → Some。
    #[serde(deserialize_with = "deserialize_present")]
    pub password: Option<String>,
}

fn default_pg_port() -> u16 {
    5432
}

/// 连接串 URI 组件（userinfo 的 user/password、路径段的 database）的 percent 编码集合：
/// 控制符、空格、``" < > ` ``、gen-delims（`: / ? # [ ] @`）及 `%` 本身——这些字符在
/// URI 里是结构分隔符或不经编码就会破坏解析（`%` 必须编码，否则解码后语义错乱）。
/// unreserved（`-._~`）与 sub-delims（`!$&'()*+,;=`）在 userinfo/path 段合法，保持
/// 原样可读；非 ASCII 字节一律由 [`utf8_percent_encode`] 编码。
const CONN_COMPONENT_UNSAFE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'<')
    .add(b'>')
    .add(b'`')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'?')
    .add(b'#')
    .add(b'[')
    .add(b']')
    .add(b'@');

impl PgConn {
    /// postgres 连接串：`postgres://user[:password]@host:port/database`
    ///（password 为 None 时不带冒号段）。
    ///
    /// user/password/database 过 [`CONN_COMPONENT_UNSAFE`] percent 编码后再拼：
    /// 密码含 `@ : / ? # %` 等合法字符时不会破坏 URI 结构（postgres 协议侧
    /// 支持 userinfo 与路径段的 percent 解码）。
    pub fn conn_str(&self) -> String {
        let enc = |s: &str| utf8_percent_encode(s, CONN_COMPONENT_UNSAFE).to_string();
        match &self.password {
            Some(p) => format!(
                "postgres://{}:{}@{}:{}/{}",
                enc(&self.user),
                enc(p),
                self.host,
                self.port,
                enc(&self.database)
            ),
            None => format!(
                "postgres://{}@{}:{}/{}",
                enc(&self.user),
                self.host,
                self.port,
                enc(&self.database)
            ),
        }
    }
}

/// password 的自定义反序列化：只处理「键存在」的情况（null → None，值 → Some）；
/// 键缺失时 serde derive（见 expr_is_missing：带 deserialize_with 的字段）直接报
/// `missing field password`，不会走到这里。
fn deserialize_present<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?)
}

/// sqlite 目标文件路径
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SqliteFile {
    pub file: String,
}

/// 数据源（内部标签 type：postgres|sqlite）
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Datasource {
    Postgres(PgConn),
    Sqlite(SqliteFile),
}

/// 同步目标（内部标签 type：sqlite|postgres）
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Target {
    Sqlite(SqliteFile),
    Postgres(PgConn),
}

/// 单表同步规格
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TableSpec {
    pub name: String,
    pub primary_key: String,
    #[serde(default)]
    pub columns: Option<Vec<String>>,
}

/// [sync] 节。tables 无默认：缺失即错（错误信息由 validate_shape 产出，含 sync.tables）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SyncCfg {
    pub schedule: Option<String>,
    pub tables: Vec<TableSpec>,
}

/// 写库模式（yaml: simulated|real）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteMode {
    #[default]
    Simulated,
    Real,
}

/// [write] 节。缺省 / `write: {}` 时全部取默认：false / Simulated / []。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WriteCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: WriteMode,
    #[serde(default)]
    pub tables: Vec<String>,
}

impl Default for WriteCfg {
    fn default() -> Self {
        Self { enabled: false, mode: WriteMode::Simulated, tables: Vec::new() }
    }
}

/// [features] 节：页面功能开关（工作台 / 数据字典 / 主题配置）。缺省（整个节、整个文件或
/// 单个字段缺失）一律 true——fail-open，仅在显式 false 时关闭对应页面与菜单入口。
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FeaturesCfg {
    #[serde(default = "default_true")]
    pub workbench: bool,
    #[serde(default = "default_true")]
    pub db_info: bool,
    #[serde(default = "default_true")]
    pub theme_studio: bool,
}

fn default_true() -> bool {
    true
}

impl Default for FeaturesCfg {
    fn default() -> Self {
        Self { workbench: true, db_info: true, theme_studio: true }
    }
}

/// zpm.config.yaml 根结构
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProjectConfig {
    pub name: String,
    #[serde(default)]
    pub server: ServerCfg,
    pub datasource: Datasource,
    pub target: Target,
    pub sync: SyncCfg,
    #[serde(default)]
    pub write: WriteCfg,
    /// 节级缺省：无 features 字段时 serde 用 FeaturesCfg::default()（三 true）
    #[serde(default)]
    pub features: FeaturesCfg,
}

/// 加载并校验 zpm.config.yaml
pub fn load_project_config(path: &Path) -> Result<ProjectConfig, ZpmError> {
    let txt = std::fs::read_to_string(path)?;
    let v: serde_yaml::Value = serde_yaml::from_str(&txt)
        .map_err(|e| ZpmError::Format(format!("zpm.config.yaml 语法错误：{e}")))?;
    validate_shape(&v)?;
    let cfg: ProjectConfig = serde_yaml::from_value(v)
        .map_err(|e| ZpmError::Format(format!("zpm.config.yaml 解析失败：{e}")))?;
    validate_semantics(&cfg)?;
    Ok(cfg)
}

/// 从 start 向上逐级查找 zpm.config.yaml，返回项目根目录
pub fn find_project_root(start: &Path) -> Result<PathBuf, ZpmError> {
    let start = if start.is_dir() {
        start
    } else {
        start.parent().unwrap_or(Path::new("."))
    };
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(PROJECT_FILE).is_file() {
            return Ok(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    Err(ZpmError::NotFound("不在 zpm 项目内".into()))
}

/// 形状预校验：只为 serde 报不出的字段路径错误（sync.tables、*.type）
fn validate_shape(v: &serde_yaml::Value) -> Result<(), ZpmError> {
    // sync.tables 必须显式存在（可为空数组）
    match v.get("sync") {
        None => return Err(missing("sync.tables")),
        Some(s) if s.get("tables").is_none() => return Err(missing("sync.tables")),
        Some(_) => {}
    }
    check_tag(v, "datasource")?;
    check_tag(v, "target")
}

fn missing(field: &str) -> ZpmError {
    ZpmError::Format(format!("缺少 {field}"))
}

/// 校验内部标签枚举的 type 值（postgres|sqlite），错误信息带 `<节>.type` 路径。
/// 整节缺失时跳过（交给 serde 报 missing field）。
fn check_tag(v: &serde_yaml::Value, section: &str) -> Result<(), ZpmError> {
    let node = match v.get(section) {
        None => return Ok(()),
        Some(n) => n,
    };
    match node.get("type") {
        None => Err(ZpmError::Format(format!(
            "缺少 {section}.type（期望 postgres|sqlite）"
        ))),
        Some(tag) => match tag.as_str() {
            Some("postgres") | Some("sqlite") => Ok(()),
            other => Err(ZpmError::Format(format!(
                "{section}.type 不支持：{other:?}（期望 postgres|sqlite）"
            ))),
        },
    }
}

/// 语义校验：name 非空、ident 正则、port 1..=65535
fn validate_semantics(cfg: &ProjectConfig) -> Result<(), ZpmError> {
    if cfg.name.trim().is_empty() {
        return Err(ZpmError::Format("name 不能为空".into()));
    }
    check_port(cfg.server.port, "server.port")?;
    if let Datasource::Postgres(p) = &cfg.datasource {
        check_port(p.port, "datasource.port")?;
    }
    if let Target::Postgres(p) = &cfg.target {
        check_port(p.port, "target.port")?;
    }
    for t in &cfg.sync.tables {
        if !is_safe_ident(&t.name) {
            return Err(ZpmError::Format(format!(
                "invalid identifier: sync.tables[].name = {t:?}（须匹配 {IDENT_RE}）"
            )));
        }
        if !is_safe_ident(&t.primary_key) {
            return Err(ZpmError::Format(format!(
                "invalid identifier: sync.tables[].primary_key = {t:?}（须匹配 {IDENT_RE}）"
            )));
        }
    }
    Ok(())
}

fn check_port(port: u16, field: &str) -> Result<(), ZpmError> {
    if port == 0 {
        return Err(ZpmError::Format(format!("{field} 必须在 1..=65535")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ZpmError;

    /// 写入临时文件并加载
    fn load_str(yaml: &str) -> Result<ProjectConfig, ZpmError> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(PROJECT_FILE);
        std::fs::write(&p, yaml).unwrap();
        load_project_config(&p)
    }

    /// 最小合法配置：datasource/target 为 sqlite，server/write 缺省
    const MINIMAL: &str = r#"
name: minimal
datasource:
  type: sqlite
  file: a.db
target:
  type: sqlite
  file: b.db
sync:
  tables:
    - name: t1
      primary_key: id
"#;

    /// ① 全字段 YAML 解析
    #[test]
    fn full_fields_parse() {
        let cfg = load_str(
            r#"
name: demo
server:
  port: 4310
  host: 127.0.0.1
  theme: dark
datasource:
  type: postgres
  host: db.local
  port: 6543
  database: appdb
  user: app
  password: secret
target:
  type: sqlite
  file: data/app.db
sync:
  schedule: "0 3 * * *"
  tables:
    - name: users
      primary_key: id
      columns: [id, email]
write:
  enabled: true
  mode: simulated
  tables: [users]
"#,
        )
        .unwrap();
        assert_eq!(cfg.name, "demo");
        assert_eq!(cfg.server.port, 4310);
        assert_eq!(
            cfg.datasource,
            Datasource::Postgres(PgConn {
                host: "db.local".into(),
                port: 6543,
                database: "appdb".into(),
                user: "app".into(),
                password: Some("secret".into()),
            })
        );
        assert_eq!(cfg.target, Target::Sqlite(SqliteFile { file: "data/app.db".into() }));
        assert_eq!(cfg.sync.schedule.as_deref(), Some("0 3 * * *"));
        assert_eq!(
            cfg.sync.tables,
            vec![TableSpec {
                name: "users".into(),
                primary_key: "id".into(),
                columns: Some(vec!["id".into(), "email".into()]),
            }]
        );
        assert_eq!(cfg.write.mode, WriteMode::Simulated);
    }

    /// ② 缺 server 节时默认值生效
    #[test]
    fn server_defaults_apply_when_section_missing() {
        let cfg = load_str(MINIMAL).unwrap();
        assert_eq!(cfg.server.port, 4310);
        assert_eq!(cfg.server.host, "0.0.0.0");
        assert_eq!(cfg.server.theme, "default");
    }

    /// 回归（E2E 冒烟抓到的真实缺陷）：节存在但缺 host/theme 字段时，
    /// 字段级默认必须生效（框架模板常省略 server.host）
    #[test]
    fn server_field_defaults_apply_when_fields_missing() {
        let cfg = load_str(
            r#"
name: demo
server: { port: 4310, theme: ./themes/default.css }
datasource: { type: sqlite, file: a.db }
target: { type: sqlite, file: b.db }
sync: { tables: [] }
"#,
        )
        .unwrap();
        assert_eq!(cfg.server.port, 4310);
        assert_eq!(cfg.server.host, "0.0.0.0");
    }

    /// ③ 缺 sync.tables 报错（含 "sync.tables"）
    #[test]
    fn missing_sync_tables_errors() {
        // sync 节存在但缺 tables
        let err = load_str(
            r#"
name: demo
datasource: { type: sqlite, file: a.db }
target: { type: sqlite, file: b.db }
sync:
  schedule: "* * * * *"
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("sync.tables"), "实际错误：{err}");
        // sync 节整体缺失
        let err = load_str(
            r#"
name: demo
datasource: { type: sqlite, file: a.db }
target: { type: sqlite, file: b.db }
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("sync.tables"), "实际错误：{err}");
    }

    /// ④ datasource.type: mysql 报错（含 "datasource.type"）
    #[test]
    fn unknown_datasource_type_errors() {
        let err = load_str(
            r#"
name: demo
datasource:
  type: mysql
  host: h
  database: d
  user: u
  password: p
target: { type: sqlite, file: b.db }
sync: { tables: [] }
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("datasource.type"), "实际错误：{err}");
    }

    /// ⑤ 表名/主键非法标识符报错（含 "invalid identifier"）
    #[test]
    fn invalid_identifiers_error() {
        let bad_name = r#"
name: demo
datasource: { type: sqlite, file: a.db }
target: { type: sqlite, file: b.db }
sync:
  tables:
    - name: 9bad
      primary_key: id
"#;
        let err = load_str(bad_name).unwrap_err();
        assert!(err.to_string().contains("invalid identifier"), "实际错误：{err}");

        let bad_pk = r#"
name: demo
datasource: { type: sqlite, file: a.db }
target: { type: sqlite, file: b.db }
sync:
  tables:
    - name: users
      primary_key: with-dash
"#;
        let err = load_str(bad_pk).unwrap_err();
        assert!(err.to_string().contains("invalid identifier"), "实际错误：{err}");
    }

    /// ⑥ write: {} 通过且默认 false/Simulated/[]
    #[test]
    fn empty_write_section_uses_defaults() {
        let cfg = load_str(
            r#"
name: demo
datasource: { type: sqlite, file: a.db }
target: { type: sqlite, file: b.db }
sync: { tables: [] }
write: {}
"#,
        )
        .unwrap();
        assert!(!cfg.write.enabled);
        assert_eq!(cfg.write.mode, WriteMode::Simulated);
        assert!(cfg.write.tables.is_empty());
    }

    /// ⑦ password：显式空值 → None；键缺失 → 报错含 "password"
    #[test]
    fn password_null_ok_but_missing_key_errors() {
        let cfg = load_str(
            r#"
name: demo
datasource:
  type: postgres
  host: h
  database: d
  user: u
  password:
target: { type: sqlite, file: b.db }
sync: { tables: [] }
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.datasource,
            Datasource::Postgres(PgConn {
                host: "h".into(),
                port: 5432,
                database: "d".into(),
                user: "u".into(),
                password: None,
            })
        );

        let err = load_str(
            r#"
name: demo
datasource:
  type: postgres
  host: h
  database: d
  user: u
target: { type: sqlite, file: b.db }
sync: { tables: [] }
"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("password"), "实际错误：{err}");
    }

    /// ⑧ 未知顶层字段静默忽略
    #[test]
    fn unknown_top_level_fields_ignored() {
        let cfg = load_str(
            r#"
name: demo
future_field: 42
another: { nested: true }
datasource: { type: sqlite, file: a.db }
target: { type: sqlite, file: b.db }
sync: { tables: [] }
"#,
        )
        .unwrap();
        assert_eq!(cfg.name, "demo");
    }

    /// ⑨ features 节整体缺失（或整个文件为最小配置）→ 三 true（fail-open）
    #[test]
    fn features_section_missing_defaults_true() {
        let cfg = load_str(MINIMAL).unwrap();
        assert_eq!(
            cfg.features,
            FeaturesCfg { workbench: true, db_info: true, theme_studio: true }
        );
        assert_eq!(cfg.features, FeaturesCfg::default());
    }

    /// ⑩ 显式 features 字段解析生效：false 为关；单字段缺省仍 true
    #[test]
    fn features_explicit_values_parse() {
        let cfg = load_str(
            r#"
name: demo
features: { workbench: false, db_info: false, theme_studio: false }
datasource: { type: sqlite, file: a.db }
target: { type: sqlite, file: b.db }
sync: { tables: [] }
"#,
        )
        .unwrap();
        assert!(!cfg.features.workbench);
        assert!(!cfg.features.db_info);
        assert!(!cfg.features.theme_studio);

        // features 节存在但只写了 db_info → workbench/theme_studio 字段级缺省 true
        let cfg = load_str(
            r#"
name: demo
features: { db_info: false }
datasource: { type: sqlite, file: a.db }
target: { type: sqlite, file: b.db }
sync: { tables: [] }
"#,
        )
        .unwrap();
        assert!(!cfg.features.db_info);
        assert!(cfg.features.workbench);
        assert!(cfg.features.theme_studio);
    }

    /// PgConn::conn_str：带密码 / 无密码（不带冒号段）
    #[test]
    fn conn_str_builds_url_with_and_without_password() {
        let with = PgConn {
            host: "db.local".into(),
            port: 6543,
            database: "appdb".into(),
            user: "app".into(),
            password: Some("secret".into()),
        };
        assert_eq!(with.conn_str(), "postgres://app:secret@db.local:6543/appdb");
        let without = PgConn {
            host: "h".into(),
            port: 5432,
            database: "d".into(),
            user: "u".into(),
            password: None,
        };
        assert_eq!(without.conn_str(), "postgres://u@h:5432/d");
    }

    /// conn_str：user/password/database 含 URI 特殊字符（`@ : / ? # %` 等）时按
    /// percent 编码——URI 结构分隔符（`://`、`@`、`:` 端口、`/` database）保持唯一
    /// 可解析，编码可逆（解码还原原组件）。
    #[test]
    fn conn_str_percent_encodes_uri_specials() {
        use percent_encoding::percent_decode_str;
        let conn = PgConn {
            host: "db.local".into(),
            port: 6543,
            database: "app/db?name".into(),
            user: "us@er".into(),
            password: Some("p@ss:word?#%/1".into()),
        };
        assert_eq!(
            conn.conn_str(),
            "postgres://us%40er:p%40ss%3Aword%3F%23%25%2F1@db.local:6543/app%2Fdb%3Fname"
        );
        // 按连接串解析规则反解（rsplit '@' → split ':' → split '/'），解码应还原原组件
        let s = conn.conn_str();
        let s = s.strip_prefix("postgres://").unwrap();
        let (userinfo, host_db) = s.rsplit_once('@').unwrap();
        let (user, password) = userinfo.split_once(':').unwrap();
        let (host_port, database) = host_db.split_once('/').unwrap();
        let dec = |x: &str| percent_decode_str(x).decode_utf8().unwrap().into_owned();
        assert_eq!(dec(user), "us@er");
        assert_eq!(dec(password), "p@ss:word?#%/1");
        assert_eq!(dec(database), "app/db?name");
        assert_eq!(host_port, "db.local:6543");
    }

    /// find_project_root：向上逐级查找
    #[test]
    fn find_project_root_walks_up() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(PROJECT_FILE), MINIMAL).unwrap();
        let sub = root.join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_project_root(&sub).unwrap(), root);
    }

    /// find_project_root：项目外报 NotFound
    #[test]
    fn find_project_root_errors_outside_project() {
        let tmp = tempfile::tempdir().unwrap();
        let err = find_project_root(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("不在 zpm 项目内"), "实际错误：{err}");
    }
}
