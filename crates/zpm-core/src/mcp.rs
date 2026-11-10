//! opencode 能力注入：把 codegraph/dbx/playwright 三个 MCP server 写进项目的
//! `.opencode/opencode.json`。
//!
//! v1（opencode 1.x：server 名直接在 `mcp` 下，`enabled: true` 控制）与
//! v2（opencode 2.x：挪入 `mcp.servers`，缺省即启用、`disabled` 控制）格式不同，
//! 按本机 `opencode --version` 探测；探测失败保守按 v1。
//! 合并语义：未提及键原样保留，同名条目覆盖（注入为准）。

use crate::error::ZpmError;
use serde_json::{json, Value};
use std::path::Path;

/// opencode 配置格式版本
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFormat {
    /// opencode 1.x：`mcp.<名称>` + `enabled`
    V1,
    /// opencode 2.x：`mcp.servers.<名称>` + `disabled`
    V2,
}

/// 解析 opencode 版本串首段 semver：2.* → V2，其余（含无法解析）→ V1
pub fn parse_version(s: &str) -> McpFormat {
    let first = s.trim().split('.').next().unwrap_or("");
    if first == "2" {
        McpFormat::V2
    } else {
        McpFormat::V1
    }
}

/// 探测本机 opencode 的配置格式；命令不存在或执行失败 → V1（保守回退）
pub fn detect_format() -> McpFormat {
    match std::process::Command::new("opencode").arg("--version").output() {
        Ok(o) if o.status.success() => parse_version(&String::from_utf8_lossy(&o.stdout)),
        _ => McpFormat::V1,
    }
}

/// 内置能力预设（名 → 启动命令）
pub const PRESETS: &[(&str, &[&str])] = &[
    ("codegraph", &["codegraph", "serve", "--mcp"]),
    ("dbx", &["npx", "-y", "@dbx-app/mcp-server"]),
    ("playwright", &["npx", "@playwright/mcp@latest"]),
];

/// 把预设条目合并进 opencode.json（未提及键原样保留，同名覆盖）。返回注入条目数。
/// 文件不存在时以 `{}` 为起点创建。
pub fn inject(opencode_json: &Path, format: McpFormat, names: &[&str]) -> Result<usize, ZpmError> {
    let raw = if opencode_json.exists() {
        std::fs::read_to_string(opencode_json)?
    } else {
        "{}".to_string()
    };
    let mut root: Value = serde_json::from_str(&raw)
        .map_err(|e| ZpmError::Format(format!("opencode.json 解析失败：{e}")))?;

    // 根必须是 JSON 对象（合法 JSON 但非对象时 serde_json IndexMut 会 panic，提前报错）
    if !root.is_object() {
        return Err(ZpmError::Format("opencode.json 根必须是 JSON 对象".into()));
    }
    // 容器缺失则补建；存在但非对象 → 报错（保守：不静默替换用户数据）
    let mcp_bad = root.get("mcp").is_some_and(|m| !m.is_object());
    let servers_bad = matches!(format, McpFormat::V2)
        && root.get("mcp").is_some_and(|m| m.get("servers").is_some_and(|s| !s.is_object()));
    if mcp_bad || servers_bad {
        return Err(ZpmError::Format(
            "opencode.json 的 mcp（或 mcp.servers）字段存在但不是对象，请先手工修复".into(),
        ));
    }
    if !root.get("mcp").is_some_and(|m| m.is_object()) {
        root["mcp"] = json!({});
    }
    if matches!(format, McpFormat::V2)
        && !root["mcp"].get("servers").is_some_and(|m| m.is_object())
    {
        root["mcp"]["servers"] = json!({});
    }
    let entries = match format {
        McpFormat::V1 => root["mcp"]
            .as_object_mut()
            .ok_or_else(|| ZpmError::Format("mcp 字段不是对象".into()))?,
        McpFormat::V2 => root["mcp"]["servers"]
            .as_object_mut()
            .ok_or_else(|| ZpmError::Format("mcp.servers 字段不是对象".into()))?,
    };

    let mut injected = 0usize;
    for name in names {
        let Some((_, cmd)) = PRESETS.iter().find(|(n, _)| n == name) else {
            return Err(ZpmError::NotFound(format!(
                "未知 mcp 能力：{name}（可用：codegraph/dbx/playwright）"
            )));
        };
        let mut entry = json!({ "type": "local", "command": cmd });
        if matches!(format, McpFormat::V1) {
            entry["enabled"] = json!(true);
        }
        entries.insert(name.to_string(), entry);
        injected += 1;
    }

    std::fs::write(opencode_json, serde_json::to_string_pretty(&root)?)?;
    Ok(injected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ① 版本串 → 格式
    #[test]
    fn parse_version_maps_major_to_format() {
        assert!(matches!(parse_version("1.17.18"), McpFormat::V1));
        assert!(matches!(parse_version("2.3.0"), McpFormat::V2));
        assert!(matches!(parse_version("2.0"), McpFormat::V2));
        assert!(matches!(parse_version("garbage"), McpFormat::V1));
        assert!(matches!(parse_version(""), McpFormat::V1));
    }

    /// ② V1 注入：三条预设形状精确 + 同名覆盖 + 未提及键保留
    #[test]
    fn inject_v1_merges_preserving_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("opencode.json");
        std::fs::write(
            &p,
            r#"{"$schema": "https://opencode.ai/config.json", "mcp": {"old": {"type":"local","command":["x"],"enabled":false}}, "instructions": ["a.md"]}"#,
        )
        .unwrap();
        let n = inject(&p, McpFormat::V1, &["codegraph", "dbx", "playwright"]).unwrap();
        assert_eq!(n, 3);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            v["mcp"]["codegraph"],
            json!({"type":"local","command":["codegraph","serve","--mcp"],"enabled":true})
        );
        assert_eq!(v["mcp"]["dbx"]["command"], json!(["npx", "-y", "@dbx-app/mcp-server"]));
        assert_eq!(v["mcp"]["playwright"]["command"], json!(["npx", "@playwright/mcp@latest"]));
        assert!(v["mcp"].get("old").is_some(), "同名外的既有条目保留");
        assert_eq!(v["instructions"], json!(["a.md"]));
        assert_eq!(v["$schema"], "https://opencode.ai/config.json");
    }

    /// ③ V2 注入：条目挪入 mcp.servers 且不带 enabled
    #[test]
    fn inject_v2_places_under_servers_without_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("opencode.json");
        std::fs::write(&p, "{}").unwrap();
        inject(&p, McpFormat::V2, &["codegraph"]).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            v["mcp"]["servers"]["codegraph"],
            json!({"type":"local","command":["codegraph","serve","--mcp"]})
        );
        assert!(v["mcp"]["servers"]["codegraph"].get("enabled").is_none());
    }

    /// 未知能力名报错（防拼错静默通过）
    #[test]
    fn inject_unknown_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("opencode.json");
        std::fs::write(&p, "{}").unwrap();
        let err = inject(&p, McpFormat::V1, &["codemap"]).unwrap_err();
        assert!(err.to_string().contains("未知 mcp 能力"), "实际错误：{err}");
    }

    /// 畸形配置拒绝注入：mcp 容器损坏 → 报错且不写盘；根非对象 → 报错
    #[test]
    fn inject_rejects_corrupt_container_and_root() {
        let tmp = tempfile::tempdir().unwrap();
        // mcp 字段是字符串（手改损坏）→ 报错且文件原样保留
        let p = tmp.path().join("a.json");
        std::fs::write(&p, r#"{"mcp": "broken"}"#).unwrap();
        let before = std::fs::read_to_string(&p).unwrap();
        let err = inject(&p, McpFormat::V1, &["dbx"]).unwrap_err();
        assert!(err.to_string().contains("不是对象"), "实际错误：{err}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before, "报错时不得改动文件");

        // 根是数组 → 报错（serde_json IndexMut 对数组会 panic，必须提前拦截）
        let p2 = tmp.path().join("b.json");
        std::fs::write(&p2, "[]").unwrap();
        let err = inject(&p2, McpFormat::V2, &["dbx"]).unwrap_err();
        assert!(err.to_string().contains("根必须"), "实际错误：{err}");
    }

    /// 文件不存在时以 {} 为起点创建
    #[test]
    fn inject_creates_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("opencode.json");
        inject(&p, McpFormat::V1, &["dbx"]).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(v["mcp"]["dbx"]["enabled"].as_bool().unwrap());
    }
}
