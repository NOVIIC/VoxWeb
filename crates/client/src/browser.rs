use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::HtmlCanvasElement;

/// 信令服务 URL meta tag 名称。
const SIGNALING_META_NAME: &str = "signaling-url";

pub(super) fn sync_canvas_size(canvas: &HtmlCanvasElement) -> (u32, u32) {
    let w = (canvas.client_width().max(1)) as u32;
    let h = (canvas.client_height().max(1)) as u32;
    if canvas.width() != w {
        canvas.set_width(w);
    }
    if canvas.height() != h {
        canvas.set_height(h);
    }
    (w, h)
}

pub(super) fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| p.now())
        .unwrap_or(0.0)
}

/// 用 getrandom 生成一个 u64 随机种子。失败时退化为 0。
pub(super) fn random_seed() -> u64 {
    let mut buf = [0u8; 8];
    let _ = getrandom::getrandom(&mut buf);
    u64::from_le_bytes(buf)
}

/// 从 `<meta name="signaling-url">` 读取信令服务 URL；
/// 若 URL 携带 `?signaling=...` query 参数，则优先用 query（方便本地开发切换地址）。
/// 未配置时返回 None。
pub(super) fn signaling_url() -> Option<String> {
    if let Some(from_query) = read_query_param("signaling")
        && !from_query.is_empty()
    {
        return Some(from_query);
    }

    let window = web_sys::window()?;
    let document = window.document()?;
    let selector = format!("meta[name=\"{SIGNALING_META_NAME}\"]");
    let el = document.query_selector(&selector).ok()??;
    let meta = el.dyn_into::<web_sys::HtmlMetaElement>().ok()?;
    let content = meta.content();
    if content.is_empty() {
        None
    } else {
        Some(content)
    }
}

/// 读取 `window.location.search` 中的一个 query 参数（已 URL 解码）。
pub(super) fn read_query_param(key: &str) -> Option<String> {
    let window = web_sys::window()?;
    let search = window.location().search().ok()?;
    let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
    params.get(key)
}

/// 用 history.replaceState 在不刷新页面的情况下更新 URL 上的 `?room=` 参数，
/// 保留其它已有 query（如 ?signaling=）。失败静默，不影响功能。
pub(super) fn set_room_in_url(room_id: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Ok(history) = window.history() else {
        return;
    };
    let search = window.location().search().unwrap_or_default();
    let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) else {
        return;
    };
    params.set("room", room_id);
    let new_search: String = params.to_string().into();
    let new_url = if new_search.is_empty() {
        "?".to_string()
    } else {
        format!("?{new_search}")
    };
    let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&new_url));
}
