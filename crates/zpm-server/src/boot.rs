//! 服务启动编排：定时数据同步调度（可选）与停止句柄。
//!
//! - `sync.schedule` 未配置 → 不启动任何调度线程（`has_schedule()==false`）
//! - cron 表达式解析失败 → 仅提示，服务照常启动
//! - 调度线程：croner 计算下次触发 → 分片睡眠（可及时响应停止）→ 防重入 →
//!   每轮重建 Source/Target → 失败仅记日志不崩服务
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use zpm_core::dbsync;
use zpm_core::error::ZpmError;
use zpm_core::project_config::ProjectConfig;
use zpm_core::store;

/// 启动句柄：调度状态查询与停止信号。
pub struct BootHandle {
    has_schedule: bool,
    stop: Arc<AtomicBool>,
}

impl BootHandle {
    /// 是否启动了定时同步线程
    pub fn has_schedule(&self) -> bool {
        self.has_schedule
    }

    /// 发出停止信号（调度线程在下一个 500ms 分片内退出）
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// 启动定时同步（若配置了 sync.schedule）；返回停止句柄。
pub fn start_schedule(cfg: &ProjectConfig, project_root: &Path) -> BootHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let Some(expr) = cfg.sync.schedule.clone() else {
        return BootHandle { has_schedule: false, stop };
    };

    let cron = croner::Cron::new(&expr).parse();
    if let Err(e) = cron {
        eprintln!("[schedule] cron 表达式解析失败（{expr}）：{e}");
        return BootHandle { has_schedule: false, stop };
    }
    let cron = cron.unwrap();

    let cfg = cfg.clone();
    let root = project_root.to_path_buf();
    let stop_thread = Arc::clone(&stop);
    let running = Arc::new(AtomicBool::new(false));

    // 独立线程：避免阻塞 tokio 运行时（同步 DB 调用是阻塞式的）
    std::thread::spawn(move || {
        loop {
            if stop_thread.load(Ordering::SeqCst) {
                return;
            }
            let now = chrono::Local::now();
            let next = match cron.find_next_occurrence(&now, false) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("[schedule] 下次触发计算失败：{e}");
                    return;
                }
            };
            let wait = next
                .signed_duration_since(now)
                .to_std()
                .unwrap_or_default();
            // 分片睡眠：停止信号最长 500ms 内生效
            let mut remaining = wait;
            while !stop_thread.load(Ordering::SeqCst) && remaining > Duration::ZERO {
                let chunk = remaining.min(Duration::from_millis(500));
                std::thread::sleep(chunk);
                remaining = remaining.saturating_sub(chunk);
            }
            if stop_thread.load(Ordering::SeqCst) {
                return;
            }
            // 防重入：对齐 TS engine 的 running 语义（单线程顺序循环下不可触发，保留以对齐语义）
            if running.swap(true, Ordering::SeqCst) {
                continue;
            }
            run_sync(&cfg, &root);
            running.store(false, Ordering::SeqCst);
        }
    });

    BootHandle { has_schedule: true, stop }
}

/// 执行一轮数据同步（每轮重建 Source/Target；失败仅记日志不崩服务）
fn run_sync(cfg: &ProjectConfig, root: &Path) {
    let result = (|| -> Result<Vec<dbsync::SyncReport>, ZpmError> {
        let mut source = dbsync::open_source(&cfg.datasource, root)?;
        let mut target = store::open_store_for(&cfg.target, root)?;
        dbsync::sync_tables(source.as_mut(), target.as_mut(), &cfg.sync.tables)
    })();
    match result {
        Ok(reports) => {
            for r in &reports {
                println!("synced {}: {} rows", r.table, r.rows);
            }
        }
        Err(e) => eprintln!("[schedule] sync failed: {e}"),
    }
}
