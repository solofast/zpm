//! `zpm serve`：原型服务器（静态托管 + 通用 CRUD API + simulated 会话 + 定时同步）。
use clap::Args;
use std::sync::{Arc, Mutex};
use zpm_core::error::ZpmError;
use zpm_core::project_config;
use zpm_core::store;
use zpm_server::session::SessionLayer;
use zpm_server::{boot, AppState};

#[derive(Args)]
pub struct ServeArgs {
    /// 覆盖 zpm.config.yaml 的 server.port
    #[arg(long)]
    pub port: Option<u16>,
}

/// 异步入口：由 main.rs 的 tokio runtime block_on 驱动。
/// sqlite 目标库文件缺失时自动创建（open_store_for 内建 WAL）。
pub async fn run(args: ServeArgs) -> Result<(), ZpmError> {
    let cwd = std::env::current_dir()?;
    let root = project_config::find_project_root(&cwd)?;
    let cfg = project_config::load_project_config(&root.join(project_config::PROJECT_FILE))?;
    let port = args.port.unwrap_or(cfg.server.port);
    let host = cfg.server.host.clone();

    let store = store::open_store_for(&cfg.target, &root)?;
    let app_state = AppState {
        project_root: root.clone(),
        name: cfg.name.clone(),
        theme: cfg.server.theme.clone(),
        store: Arc::new(Mutex::new(store)),
        write_enabled: cfg.write.enabled,
        write_mode: cfg.write.mode,
        writable_tables: Arc::new(cfg.write.tables.clone()),
        features: cfg.features.clone(),
        session: Arc::new(SessionLayer::new()),
    };
    let router = zpm_server::build_router(app_state.clone());

    // 定时同步（sync.schedule 有值才启动）
    let boot = boot::start_schedule(&cfg, &root);

    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|e| ZpmError::Format(format!("端口绑定失败 {host}:{port}：{e}")))?;
    println!("zpm serve: http://{host}:{port}（Ctrl-C 停止）");

    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    boot.stop();
    result.map_err(|e| ZpmError::Format(format!("服务器错误：{e}")))?;
    println!("服务已停止。");
    Ok(())
}
