//! `/theme.css`：按 zpm.config.yaml 的 server.theme 服务主题 tokens。
//!
//! - `default` → 编译进二进制的内置 tokens（`assets/default-tokens.css`）
//! - 绝对路径直用；相对路径**按工程目录解析**（TS 按 cwd 解析属缺陷，有意修正）
//! - 未命中 → 404 纯文本 `theme not found: <t>`；每次请求时解析（改主题文件即时生效）

use crate::AppState;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use std::path::PathBuf;

/// 内置默认主题（随包分发，来自 frameworks 的 tokens.css）
const DEFAULT_TOKENS_CSS: &str = include_str!("../assets/default-tokens.css");

pub async fn theme_css(State(state): State<AppState>) -> Response {
    let theme = state.theme.clone();
    if theme == "default" {
        return css_response(DEFAULT_TOKENS_CSS.as_bytes().to_vec());
    }
    let p = PathBuf::from(&theme);
    let p = if p.is_absolute() { p } else { state.project_root.join(p) };
    match std::fs::read(&p) {
        Ok(bytes) => css_response(bytes),
        Err(_) => (StatusCode::NOT_FOUND, format!("theme not found: {theme}")).into_response(),
    }
}

fn css_response(bytes: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], bytes).into_response()
}
