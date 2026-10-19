//! Thin browser/WebView helpers. Every function has a native fallback so the
//! pure state layer (reducers, demo simulation, shortcut parsing) can be unit
//! tested with plain `cargo test`.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

/// Milliseconds since the epoch (JS `Date.now()` in the browser).
pub fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() * 1000.0)
            .unwrap_or(0.0)
    }
}

/// High-resolution timer (for latency measurement).
pub fn perf_now() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.performance())
            .map(|p| p.now())
            .unwrap_or_else(now_ms)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        now_ms()
    }
}

/// A query-string parameter of the current page (`?demo=1`).
pub fn query_param(name: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        let search = web_sys::window()?.location().search().ok()?;
        let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
        params.get(name)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = name;
        None
    }
}

pub fn local_get(key: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()?
            .local_storage()
            .ok()??
            .get_item(key)
            .ok()?
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = key;
        None
    }
}

pub fn local_set(key: &str, value: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(Ok(Some(s))) = web_sys::window().map(|w| w.local_storage()) {
            let _ = s.set_item(key, value);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (key, value);
    }
}

pub fn local_remove(key: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(Ok(Some(s))) = web_sys::window().map(|w| w.local_storage()) {
            let _ = s.remove_item(key);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = key;
    }
}

/// `location.hash` without the leading `#`.
pub fn get_hash() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().hash().ok())
            .map(|h| h.trim_start_matches('#').to_string())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        String::new()
    }
}

/// Push a new hash entry (keeps the query string, e.g. `?demo=1`).
pub fn set_hash(hash: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(w) = web_sys::window() {
            if get_hash() == hash {
                return;
            }
            let loc = w.location();
            let base = format!(
                "{}{}",
                loc.pathname().unwrap_or_default(),
                loc.search().unwrap_or_default()
            );
            let url = format!("{base}#{hash}");
            if let Ok(h) = w.history() {
                let _ = h.push_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&url));
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = hash;
    }
}

/// Page origin (`https://mac-mini.tailnet.ts.net`), empty natively.
pub fn origin() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.location().origin().ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        String::new()
    }
}

fn user_agent() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.navigator().user_agent().ok())
            .unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        String::new()
    }
}

/// macOS shows ⌘ in key hints; everything else shows Ctrl.
pub fn is_mac() -> bool {
    let ua = user_agent();
    ua.contains("Mac OS") || ua.contains("Macintosh")
}

pub fn is_android_ua() -> bool {
    user_agent().contains("Android")
}

/// True inside the Tauri 2 WebView (desktop or Android shell).
pub fn is_tauri() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .map(|w| js_sys::Reflect::has(&w, &"__TAURI_INTERNALS__".into()).unwrap_or(false))
            .unwrap_or(false)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

pub fn viewport_width() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.inner_width().ok())
            .and_then(|v| v.as_f64())
            .unwrap_or(1440.0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        1440.0
    }
}

pub fn is_touch() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.match_media("(pointer: coarse)").ok().flatten())
            .map(|m| m.matches())
            .unwrap_or(false)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

/// Set an attribute on `<html>` (`data-theme`, `data-surface`).
pub fn set_root_attr(name: &str, value: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.document_element())
        {
            let _ = el.set_attribute(name, value);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (name, value);
    }
}

/// Open a URL outside the app (new tab in a browser; the Tauri shell routes
/// `window.open` to the system browser via the opener plugin).
pub fn open_url(url: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(w) = web_sys::window() {
            // Tauri (withGlobalTauri): window.__TAURI__.opener.openUrl(url).
            let opener = js_sys::Reflect::get(&w, &"__TAURI__".into())
                .ok()
                .filter(|v| !v.is_undefined())
                .and_then(|t| js_sys::Reflect::get(&t, &"opener".into()).ok())
                .filter(|v| !v.is_undefined());
            if let Some(op) = opener
                && let Ok(f) = js_sys::Reflect::get(&op, &"openUrl".into())
                && let Some(f) = f.dyn_ref::<js_sys::Function>()
            {
                let _ = f.call1(&op, &url.into());
                return;
            }
            let _ = w.open_with_url_and_target(url, "_blank");
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = url;
    }
}

/// Focus an element by id and select its text.
pub fn focus_id(id: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(id))
            && let Some(h) = el.dyn_ref::<web_sys::HtmlElement>()
        {
            let _ = h.focus();
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = id;
    }
}

/// Blur whatever has focus (used when Esc leaves an input).
pub fn blur_active() {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(el) = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.active_element())
            && let Some(h) = el.dyn_ref::<web_sys::HtmlElement>()
        {
            let _ = h.blur();
        }
    }
}

/// Copy text to the clipboard (best effort).
pub fn copy_text(text: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(w) = web_sys::window() {
            let _ = w.navigator().clipboard().write_text(text);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
    }
}

pub fn log(msg: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::console::log_1(&msg.into());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = msg;
    }
}
