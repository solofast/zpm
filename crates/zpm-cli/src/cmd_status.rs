//! `zpm status`：查看当前项目状态（配置、数据链路与知识库概览）。
use zpm_core::knowledge;
use zpm_core::project_config as pc;

pub fn run() -> Result<(), zpm_core::ZpmError> {
    let cwd = std::env::current_dir()?;
    let root = match pc::find_project_root(&cwd) {
        Ok(r) => r,
        Err(_) => {
            println!("当前目录不在 zpm 项目内（未找到 {}）", pc::PROJECT_FILE);
            return Ok(());
        }
    };
    println!("项目根：{}", root.display());
    println!("配置文件：{}（存在）", root.join(pc::PROJECT_FILE).display());
    // 知识清单为纯哈希结构（设计 §6，无时间戳字段），故不展示"最近知识同步"
    let cfg = pc::load_project_config(&root.join(pc::PROJECT_FILE))?;
    println!("项目：{}（server :{}，主题 {}）", cfg.name, cfg.server.port, cfg.server.theme);
    println!(
        "数据源：{}",
        match &cfg.datasource {
            zpm_core::project_config::Datasource::Postgres(c) => format!(
                "postgres {}/{}:{}",
                c.database, c.host, c.port
            ),
            zpm_core::project_config::Datasource::Sqlite(f) => {
                format!("sqlite {}", root.join(&f.file).display())
            }
        }
    );
    println!(
        "目标库：{}（write {}：{} 张白名单表）",
        match &cfg.target {
            zpm_core::project_config::Target::Sqlite(f) => {
                format!("sqlite {}", root.join(&f.file).display())
            }
            zpm_core::project_config::Target::Postgres(c) => {
                format!("postgres {}/{}:{}", c.database, c.host, c.port)
            }
        },
        if cfg.write.enabled {
            match cfg.write.mode {
                zpm_core::project_config::WriteMode::Real => "real",
                zpm_core::project_config::WriteMode::Simulated => "simulated",
            }
        } else {
            "disabled"
        },
        cfg.write.tables.len(),
    );
    println!("同步表：{} 张（schedule：{}）", cfg.sync.tables.len(), cfg.sync.schedule.as_deref().unwrap_or("未配置"));

    let m = knowledge::load_knowledge_manifest(&root)?;
    if m.files.is_empty() {
        println!("知识条目：0 个文件");
    } else {
        let cats: std::collections::BTreeSet<&str> =
            m.files.keys().map(|k| knowledge::categorize(k)).collect();
        println!("知识条目：{} 个文件 / {} 个类别", m.files.len(), cats.len());
    }
    println!("知识目录：{}", root.join("knowledge").display());
    Ok(())
}
