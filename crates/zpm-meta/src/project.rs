//! projects 表 CRUD。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use crate::error::{MetaError, MetaResult};

/// projects 表一行记录。
#[derive(Debug)]
pub struct ProjectRecord {
    pub id: i64,
    pub name: String,
    pub path: PathBuf,
    pub framework: String,
    pub repo: String,
    pub created_at: String,
}

/// 新建 project 的输入。
#[derive(Debug)]
pub struct NewProject<'a> {
    pub name: &'a str,
    pub path: &'a Path,
    pub framework: &'a str,
    pub repo: &'a str,
}

/// 行 → ProjectRecord（列序：id, name, path, framework, repo, created_at）。
fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRecord> {
    Ok(ProjectRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        path: PathBuf::from(row.get::<_, String>(2)?),
        framework: row.get(3)?,
        repo: row.get(4)?,
        created_at: row.get(5)?,
    })
}

/// 登记一个 project：路径 canonicalize 后入库。
/// 路径已被其他 project 登记时返回 Duplicate。
pub fn insert(conn: &Connection, p: &NewProject<'_>) -> MetaResult<ProjectRecord> {
    let path = p.path.canonicalize()?;
    let path_str = path.display().to_string();
    // INSERT 前先 SELECT 判重，保证 Duplicate 文案为中文（UNIQUE 约束兜底）
    let path_taken = conn
        .query_row("SELECT 1 FROM projects WHERE path = ?1", [&path_str], |_| Ok(()))
        .optional()?;
    if path_taken.is_some() {
        return Err(MetaError::Duplicate(format!("project 路径 {path_str}")));
    }
    conn.execute(
        "INSERT INTO projects (name, path, framework, repo) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![p.name, path_str, p.framework, p.repo],
    )?;
    let id = conn.last_insert_rowid();
    let created_at: String = conn.query_row(
        "SELECT created_at FROM projects WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?;
    Ok(ProjectRecord {
        id,
        name: p.name.to_string(),
        path,
        framework: p.framework.to_string(),
        repo: p.repo.to_string(),
        created_at,
    })
}

/// 列出全部 project，按 name 排序。
pub fn list(conn: &Connection) -> MetaResult<Vec<ProjectRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, path, framework, repo, created_at FROM projects ORDER BY name",
    )?;
    let rows = stmt.query_map([], row_to_project)?;
    let mut projects = Vec::new();
    for row in rows {
        projects.push(row?);
    }
    Ok(projects)
}

/// 按 project 引用（名字或路径）查找：先按 name 精确匹配；
/// 未命中再按路径匹配——输入可 canonicalize 时用 canonicalize 后的值，
/// 否则按字面路径比对。找不到返回 None。
pub fn find(conn: &Connection, name_or_path: &str) -> MetaResult<Option<ProjectRecord>> {
    let by_name = conn
        .query_row(
            "SELECT id, name, path, framework, repo, created_at FROM projects WHERE name = ?1",
            [name_or_path],
            row_to_project,
        )
        .optional()?;
    if by_name.is_some() {
        return Ok(by_name);
    }
    let candidate = Path::new(name_or_path);
    let lookup = match candidate.canonicalize() {
        Ok(p) => p,
        // 无法 canonicalize 的输入（如不存在/非法路径）按字面路径比对
        Err(_) => candidate.to_path_buf(),
    };
    let by_path = conn
        .query_row(
            "SELECT id, name, path, framework, repo, created_at FROM projects WHERE path = ?1",
            [lookup.display().to_string()],
            row_to_project,
        )
        .optional()?;
    Ok(by_path)
}

/// 按主键删除 project。
pub fn delete(conn: &Connection, id: i64) -> MetaResult<()> {
    conn.execute("DELETE FROM projects WHERE id = ?1", [id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::db;

    /// 建临时目录并打开元数据库。
    fn open_db() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let conn = db::open(dir.path()).expect("打开元数据库失败");
        (dir, conn)
    }

    /// 在 tempdir 下创建一个真实存在的 project 目录（insert 需要 canonicalize 成功）。
    fn project_dir_under(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::create_dir_all(&p).expect("创建 project 目录失败");
        p
    }

    #[test]
    fn insert_find_delete_roundtrip() {
        let (dir, conn) = open_db();
        let p = project_dir_under(&dir, "proj-a");

        // insert{name,path,framework,repo}
        let proj = insert(
            &conn,
            &NewProject {
                name: "app",
                path: &p,
                framework: "tinkera",
                repo: "alpha",
            },
        )
        .expect("insert 失败");
        assert_eq!(proj.name, "app");
        assert_eq!(proj.path, p.canonicalize().expect("canonicalize 失败"));
        assert_eq!(proj.framework, "tinkera");
        assert_eq!(proj.repo, "alpha");
        assert!(!proj.created_at.is_empty());

        // find(name) 命中且 framework/repo 字段一致
        let found = find(&conn, "app")
            .expect("find(name) 失败")
            .expect("按名字应命中");
        assert_eq!(found.id, proj.id);
        assert_eq!(found.framework, "tinkera");
        assert_eq!(found.repo, "alpha");

        // delete → find None
        delete(&conn, proj.id).expect("delete 失败");
        assert!(
            find(&conn, "app").expect("delete 后 find 失败").is_none(),
            "delete 后按名字不应命中"
        );
        assert!(list(&conn).expect("delete 后 list 失败").is_empty());
    }

    #[test]
    fn duplicate_path_rejected() {
        let (dir, conn) = open_db();
        let p = project_dir_under(&dir, "proj-b");

        insert(
            &conn,
            &NewProject {
                name: "first",
                path: &p,
                framework: "tinkera",
                repo: "alpha",
            },
        )
        .expect("首次 insert 失败");

        // 同 path 不同 name → MetaError::Duplicate
        let err = insert(
            &conn,
            &NewProject {
                name: "second",
                path: &p,
                framework: "tinkera",
                repo: "alpha",
            },
        )
        .expect_err("同路径应被拒绝");
        assert!(
            matches!(err, crate::MetaError::Duplicate(_)),
            "应为 Duplicate，实际 {err}"
        );
    }

    #[test]
    fn find_by_path() {
        let (dir, conn) = open_db();
        let p = project_dir_under(&dir, "proj-c");
        let canonical = p.canonicalize().expect("canonicalize 失败");

        let proj = insert(
            &conn,
            &NewProject {
                name: "web",
                path: &p,
                framework: "tinkera",
                repo: "alpha",
            },
        )
        .expect("insert 失败");

        // find(绝对路径字符串) 命中
        let by_path = find(&conn, &canonical.display().to_string())
            .expect("find(path) 失败")
            .expect("按路径应命中");
        assert_eq!(by_path.id, proj.id);
    }
}
