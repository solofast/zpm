//! repos 表 CRUD 与 repo 引用解析。

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use crate::error::{MetaError, MetaResult};

/// repos 表一行记录。
#[derive(Debug)]
pub struct Repo {
    pub id: i64,
    pub name: String,
    pub path: PathBuf,
    pub created_at: String,
}

/// 行 → Repo（列序：id, name, path, created_at）。
fn row_to_repo(row: &rusqlite::Row<'_>) -> rusqlite::Result<Repo> {
    Ok(Repo {
        id: row.get(0)?,
        name: row.get(1)?,
        path: PathBuf::from(row.get::<_, String>(2)?),
        created_at: row.get(3)?,
    })
}

/// 登记一个 repo：路径 canonicalize 后入库。
/// 名字或路径已被其他 repo 登记时返回 Duplicate（文案区分两种冲突）。
pub fn create(conn: &Connection, name: &str, path: &Path) -> MetaResult<Repo> {
    let path = path.canonicalize()?;
    // INSERT 前先 SELECT 判重：名字与路径各查一次，分开提示
    let name_taken = conn
        .query_row("SELECT 1 FROM repos WHERE name = ?1", [name], |_| Ok(()))
        .optional()?;
    if name_taken.is_some() {
        return Err(MetaError::Duplicate(format!("repo 名字 {name}")));
    }
    let path_str = path.display().to_string();
    let path_taken = conn
        .query_row("SELECT 1 FROM repos WHERE path = ?1", [&path_str], |_| Ok(()))
        .optional()?;
    if path_taken.is_some() {
        return Err(MetaError::Duplicate(format!("repo 路径 {path_str}")));
    }
    conn.execute(
        "INSERT INTO repos (name, path) VALUES (?1, ?2)",
        rusqlite::params![name, path_str],
    )?;
    let id = conn.last_insert_rowid();
    let created_at: String = conn.query_row(
        "SELECT created_at FROM repos WHERE id = ?1",
        [id],
        |r| r.get(0),
    )?;
    Ok(Repo {
        id,
        name: name.to_string(),
        path,
        created_at,
    })
}

/// 列出全部 repo，按 name 排序。
pub fn list(conn: &Connection) -> MetaResult<Vec<Repo>> {
    let mut stmt = conn.prepare("SELECT id, name, path, created_at FROM repos ORDER BY name")?;
    let rows = stmt.query_map([], row_to_repo)?;
    let mut repos = Vec::new();
    for row in rows {
        repos.push(row?);
    }
    Ok(repos)
}

/// 按 repo 引用（名字或路径）查找：先按 name 精确匹配；
/// 未命中再按路径匹配——输入可 canonicalize 时用 canonicalize 后的值，
/// 否则按字面路径比对。找不到返回 None。
pub fn find(conn: &Connection, name_or_path: &str) -> MetaResult<Option<Repo>> {
    let by_name = conn
        .query_row(
            "SELECT id, name, path, created_at FROM repos WHERE name = ?1",
            [name_or_path],
            row_to_repo,
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
            "SELECT id, name, path, created_at FROM repos WHERE path = ?1",
            [lookup.display().to_string()],
            row_to_repo,
        )
        .optional()?;
    Ok(by_path)
}

/// 按主键删除 repo。
pub fn delete(conn: &Connection, id: i64) -> MetaResult<()> {
    conn.execute("DELETE FROM repos WHERE id = ?1", [id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::db;

    /// 建临时目录并打开元数据库。
    fn open_db() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("创建临时目录失败");
        let conn = db::open(dir.path()).expect("打开元数据库失败");
        (dir, conn)
    }

    /// 在 tempdir 下创建一个真实存在的 repo 目录。
    fn repo_dir_under(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::create_dir_all(&p).expect("创建 repo 目录失败");
        p
    }

    /// 计算 target 相对 base 的相对路径（两者均 canonicalize）。
    fn relative_to(base: &Path, target: &Path) -> PathBuf {
        let base = base.canonicalize().expect("base canonicalize 失败");
        let target = target.canonicalize().expect("target canonicalize 失败");
        let mut base_comps = base.components().peekable();
        let mut tgt_comps = target.components().peekable();
        while base_comps.peek().is_some()
            && tgt_comps.peek().is_some()
            && base_comps.peek() == tgt_comps.peek()
        {
            base_comps.next();
            tgt_comps.next();
        }
        let mut rel = PathBuf::new();
        for _ in base_comps {
            rel.push("..");
        }
        for c in tgt_comps {
            rel.push(c.as_os_str());
        }
        rel
    }

    #[test]
    fn create_list_find_delete_roundtrip() {
        let (dir, conn) = open_db();
        let p = repo_dir_under(&dir, "repo-a");
        let canonical = p.canonicalize().expect("canonicalize 失败");

        // create
        let repo = create(&conn, "alpha", &p).expect("create 失败");
        assert_eq!(repo.name, "alpha");
        assert_eq!(repo.path, canonical);
        assert!(!repo.created_at.is_empty());

        // list 长度 1
        let all = list(&conn).expect("list 失败");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, repo.id);

        // find(name) 命中
        let by_name = find(&conn, "alpha").expect("find(name) 失败");
        assert!(by_name.is_some(), "按名字应命中");
        // find(path) 命中
        let by_path = find(&conn, &canonical.display().to_string()).expect("find(path) 失败");
        assert!(by_path.is_some(), "按路径应命中");
        assert_eq!(by_path.unwrap().id, repo.id);

        // delete → find 为 None
        delete(&conn, repo.id).expect("delete 失败");
        assert!(
            find(&conn, "alpha").expect("delete 后 find 失败").is_none(),
            "delete 后按名字不应命中"
        );
        assert!(list(&conn).expect("delete 后 list 失败").is_empty());
    }

    #[test]
    fn create_canonicalizes_path() {
        let (dir, conn) = open_db();
        let target = repo_dir_under(&dir, "repo-b");
        let rel = relative_to(&std::env::current_dir().expect("读 cwd 失败"), &target);
        assert!(rel.is_relative(), "应传入相对路径，实际 {rel:?}");

        let repo = create(&conn, "beta", &rel).expect("create 失败");
        let canonical = target.canonicalize().expect("canonicalize 失败");
        assert!(repo.path.is_absolute(), "返回值应为绝对路径");
        assert_eq!(repo.path, canonical, "返回值应为 canonicalize 后路径");

        // 入库值同样是 canonicalize 后的绝对路径
        let stored: String = conn
            .query_row("SELECT path FROM repos WHERE name = 'beta'", [], |r| r.get(0))
            .expect("查库失败");
        assert_eq!(stored, canonical.display().to_string());
    }

    #[test]
    fn duplicate_name_and_path_rejected() {
        let (dir, conn) = open_db();
        let p1 = repo_dir_under(&dir, "repo-c1");
        let p2 = repo_dir_under(&dir, "repo-c2");

        create(&conn, "gamma", &p1).expect("首次 create 失败");

        // 同名第二次（不同路径）→ Duplicate，文案指明名字
        let err = create(&conn, "gamma", &p2).expect_err("同名应被拒绝");
        assert!(
            matches!(err, crate::MetaError::Duplicate(_)),
            "应为 Duplicate，实际 {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("名字"), "错误文案应指明名字冲突：{msg}");

        // 同路径不同名 → Duplicate，文案指明路径
        let err = create(&conn, "delta", &p1).expect_err("同路径应被拒绝");
        assert!(
            matches!(err, crate::MetaError::Duplicate(_)),
            "应为 Duplicate，实际 {err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("路径"), "错误文案应指明路径冲突：{msg}");
    }
}
