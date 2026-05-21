//! AppSettings 的 `localStorage` 持久化（Phase 6）。
//!
//! 仅持久化用户面向字段（FOV / 灵敏度 / 渲染距离 / 插值延迟 / 显示统计），
//! 开发字段（`fly_speed` 等）已在 [`AppSettings`] 上加 `#[serde(skip)]`。
//!
//! 失败策略：读失败回退默认值；写失败仅 log::warn，不阻断游戏流程。
//!
//! 设计取舍：
//! - 单一 STORAGE_KEY + `schema` 头字段，便于未来 migration（schema != 1 → 当作 None）。
//! - 序列化为 JSON 而非 bincode：localStorage 是 UTF-8 文本，便于用户手改/调试。
//! - 抽出 [`serialize`] / [`deserialize`] 纯函数对接单测（非 wasm 目标也可测）。

use serde::{Deserialize, Serialize};

use crate::app::AppSettings;

/// localStorage 中存放设置的键。后缀 `.v1` 是 schema 版本，未来变更结构时换键名。
pub const STORAGE_KEY: &str = "voxweb.settings.v1";

/// 当前 schema 版本（写入文件头）。读取时 schema != 1 → 视作不可读、回退默认。
pub const CURRENT_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredSettings {
    schema: u32,
    settings: AppSettings,
}

/// 序列化为 JSON 字符串。可单测，不依赖浏览器。
pub fn serialize(settings: &AppSettings) -> String {
    let wrapped = StoredSettings {
        schema: CURRENT_SCHEMA,
        settings: settings.clone(),
    };
    // 默认值 + 5 个字段，体积小，失败可能性极低。
    serde_json::to_string(&wrapped).unwrap_or_else(|_| "{}".into())
}

/// 解析 JSON 字符串。可单测。
pub fn deserialize(s: &str) -> Option<AppSettings> {
    let parsed: StoredSettings = serde_json::from_str(s).ok()?;
    if parsed.schema != CURRENT_SCHEMA {
        log::warn!(
            "settings schema mismatch: stored={} expected={}",
            parsed.schema,
            CURRENT_SCHEMA
        );
        return None;
    }
    Some(parsed.settings)
}

/// 从 `window.localStorage` 读取设置。任何错误（无浏览器 / 键不存在 / 解析失败 / schema 不对）
/// 都返回 None，调用方应回退到 [`AppSettings::default`]。
pub fn load() -> Option<AppSettings> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok().flatten()?;
    let raw = storage.get_item(STORAGE_KEY).ok().flatten()?;
    deserialize(&raw)
}

/// 写入 `window.localStorage`。失败仅 log::warn，不返回错误（设置即使丢失也不影响游戏）。
pub fn save(settings: &AppSettings) {
    let Some(window) = web_sys::window() else {
        log::warn!("settings save: no window");
        return;
    };
    let storage = match window.local_storage() {
        Ok(Some(s)) => s,
        _ => {
            log::warn!("settings save: localStorage unavailable");
            return;
        }
    };
    let payload = serialize(settings);
    if let Err(e) = storage.set_item(STORAGE_KEY, &payload) {
        log::warn!("settings save failed: {e:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default_settings() {
        let s = AppSettings::default();
        let encoded = serialize(&s);
        let decoded = deserialize(&encoded).expect("default settings should roundtrip");
        assert_eq!(s, decoded);
    }

    #[test]
    fn roundtrip_modified_settings() {
        let s = AppSettings {
            fov_degrees: 95.0,
            mouse_sensitivity: 2.5,
            render_distance: 8,
            interp_delay_ms: 50.0,
            show_stats: false,
            ..AppSettings::default()
        };
        let encoded = serialize(&s);
        let decoded = deserialize(&encoded).expect("modified settings should roundtrip");
        assert_eq!(s, decoded);
    }

    #[test]
    fn schema_mismatch_returns_none() {
        // schema=99 → None，让调用方回退默认。
        let bad = r#"{"schema": 99, "settings": {"fov_degrees": 90.0, "mouse_sensitivity": 1.0,
            "render_distance": 6, "interp_delay_ms": 100.0, "show_stats": true}}"#;
        assert!(deserialize(bad).is_none());
    }

    #[test]
    fn malformed_json_returns_none() {
        assert!(deserialize("{not json").is_none());
        assert!(deserialize("").is_none());
    }

    #[test]
    fn skipped_fields_default_on_load() {
        // 即便 fly_speed 等不在持久化数据里，反序列化时应取 #[serde(default)] 值。
        let stored = serialize(&AppSettings::default());
        let loaded = deserialize(&stored).expect("default");
        assert_eq!(loaded.fly_speed, 12.0);
        assert_eq!(loaded.mesh_budget_ms, 4.0);
        assert_eq!(loaded.min_action_interval_ms, 100.0);
    }
}
