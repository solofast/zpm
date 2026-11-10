//! zpm-server：axum 原型服务器（静态三层 + 通用 CRUD API + simulated 会话层）。
//!
//! 挂载顺序即契约（规格 §11）：`/theme.css` → `/api/*`（嵌套，带会话 cookie 中间件）→
//! 其余全部进统一分发（vendor 语义优先于 pages 兜底）。
pub mod api;
pub mod boot;
pub mod filter;
pub mod session;
pub mod static_svc;
pub mod theme;

use axum::Router;
use session::SessionLayer;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zpm_core::project_config::{FeaturesCfg, WriteMode};
use zpm_core::store::Store;

/// 服务器运行上下文。
#[derive(Clone)]
pub struct AppState {
    /// 工程目录（zpm.config.yaml 所在目录）
    pub project_root: PathBuf,
    /// 项目名（/api/_meta 用）
    pub name: String,
    /// zpm.config.yaml 的 server.theme（default → 内置 tokens）
    pub theme: String,
    /// 目标库（/api 共享；axum 并发下用互斥锁串行化）
    pub store: Arc<Mutex<Box<dyn Store>>>,
    /// write.enabled（写入白名单总开关）
    pub write_enabled: bool,
    /// write.mode（simulated/real；simulated 走内存会话覆盖层）
    pub write_mode: WriteMode,
    /// write.tables（白名单）
    pub writable_tables: Arc<Vec<String>>,
    /// features 页面功能开关（数据字典 / 主题配置；/api/_meta 与 /api/_dict 透出）
    pub features: FeaturesCfg,
    /// simulated 会话覆盖层（按 zpm_sid 隔离；进程重启即消失）
    pub session: Arc<SessionLayer>,
}

impl AppState {
    /// 取目标库互斥锁（中毒时恢复内容继续服务）
    pub fn lock_store(&self) -> MutexGuard<'_, Box<dyn Store>> {
        self.store.lock().unwrap_or_else(|p| p.into_inner())
    }
}

use std::sync::MutexGuard;

/// 构建应用路由。
pub fn build_router(state: AppState) -> Router {
    use axum::routing::get;
    let api = api::router().layer(axum::middleware::from_fn_with_state(
        state.clone(),
        api::ensure_sid,
    ));
    Router::new()
        .route("/theme.css", get(theme::theme_css))
        .nest("/api", api)
        .fallback(static_svc::dispatch_unmatched)
        .with_state(state)
}
