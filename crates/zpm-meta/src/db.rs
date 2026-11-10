//! zpm.db 元数据库：路径布局、打开与迁移。

use std::path::{Path, PathBuf};

use crate::error::MetaResult;

/// 版本 1 迁移：创建 repos / projects 两张表（schema 见设计规格 §4.1，
/// created_at 由 SQLite 默认值生成为 RFC3339 UTC）。
const MIGRATION_V1: &str = "
CREATE TABLE repos (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL UNIQUE,   -- repo 名字，唯一
  path       TEXT NOT NULL UNIQUE,   -- 绝对路径（canonicalized）
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);

CREATE TABLE projects (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL,
  path       TEXT NOT NULL UNIQUE,   -- 项目绝对路径
  framework  TEXT NOT NULL,          -- 来源框架名
  repo       TEXT NOT NULL,          -- 创建时来源 repo 名（仅记录，不外键约束）
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now'))
);
";

/// 元数据库固定在 `repo_home/db/zpm.db`。
pub fn db_path(repo_home: &Path) -> PathBuf {
    repo_home.join("db").join("zpm.db")
}

/// 打开（必要时创建）元数据库并迁移到最新版本。
pub fn open(repo_home: &Path) -> MetaResult<rusqlite::Connection> {
    let path = db_path(repo_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = rusqlite::Connection::open(&path)?;
    migrate(&conn)?;
    Ok(conn)
}

/// 按 `PRAGMA user_version` 驱动的顺序迁移。
/// 建表与 user_version 更新包在同一事务内：中途失败整体回滚，
/// 不会留下「表已存在但 user_version=0」的半建库（后续 open 永久报错）。
fn migrate(conn: &rusqlite::Connection) -> MetaResult<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version < 1 {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(MIGRATION_V1)?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_path_layout() {
        assert_eq!(db_path(Path::new("/h")).display().to_string(), "/h/db/zpm.db");
    }

    /// 迁移原子性：脚本中途失败（第二条 CREATE 撞已有表）应整体回滚——
    /// 不留 repos 表、user_version 保持 0，后续 open 不至于永久报 table exists。
    #[test]
    fn migrate_failure_is_atomic() {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let path = db_path(dir.path());
        std::fs::create_dir_all(path.parent().expect("db 路径必有父目录")).expect("建 db 目录失败");
        let conn = rusqlite::Connection::open(&path).expect("打开库失败");
        // 预建 projects 表，使 MIGRATION_V1 的第二条 CREATE TABLE 失败
        conn.execute_batch("CREATE TABLE projects (id INTEGER PRIMARY KEY);")
            .expect("预建 projects 表失败");

        migrate(&conn).expect_err("迁移应失败");

        let repos: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'repos'",
                [],
                |r| r.get(0),
            )
            .expect("查询 sqlite_master 失败");
        assert_eq!(repos, 0, "迁移失败应整体回滚，不应留下 repos 半建库");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("读取 user_version 失败");
        assert_eq!(version, 0, "迁移失败后 user_version 应保持 0");
    }

    #[test]
    fn open_creates_tables_and_is_idempotent() {
        // tempdir → open(repo_home) 两次 → 查 sqlite_master 断言 repos、projects 两表存在
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let home = dir.path();

        let _first = open(home).expect("首次 open 失败");
        let conn = open(home).expect("二次 open 失败（迁移应幂等）");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('repos', 'projects')",
                [],
                |r| r.get(0),
            )
            .expect("查询 sqlite_master 失败");
        assert_eq!(count, 2);

        // 再查 PRAGMA user_version == 1
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("读取 user_version 失败");
        assert_eq!(version, 1);
    }
}
