//! Kiln app: one Leptos (CSR) crate, two surfaces.
//!
//! - `state/`   shared reactive state, API layer, live events, demo data,
//!   shortcuts. See `state/mod.rs` for how a surface consumes it.
//! - `desktop/` the desktop layout (design/docs/desktop.md).
//! - `android/` the Android v2 layout (design/docs/android.md).
//!
//! The same build runs in the Tauri 2 shell (`src-tauri/`), in any browser
//! at the daemon's `/app`, and under `trunk serve`.

pub mod android;
pub mod desktop;
pub mod state;

use leptos::prelude::*;
use state::{AppState, Route, platform};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    Desktop,
    Android,
}

/// Android when running in the Tauri Android shell, when forced with
/// `?surface=android`, or on a narrow touch viewport (phone browser).
/// `?surface=desktop` forces desktop.
pub fn detect_surface() -> Surface {
    match platform::query_param("surface").as_deref() {
        Some("android") => return Surface::Android,
        Some("desktop") => return Surface::Desktop,
        _ => {}
    }
    if platform::is_tauri() && platform::is_android_ua() {
        return Surface::Android;
    }
    if platform::is_touch() && platform::viewport_width() < 700.0 {
        return Surface::Android;
    }
    Surface::Desktop
}

#[component]
pub fn App() -> impl IntoView {
    let app = AppState::new();
    provide_context(app);
    let surface = detect_surface();
    platform::set_root_attr(
        "data-surface",
        if surface == Surface::Android {
            "android"
        } else {
            "desktop"
        },
    );
    app.boot();

    // Back/forward + manual hash edits.
    let handle = window_event_listener(leptos::ev::hashchange, move |_| {
        if let Some(r) = Route::parse(&platform::get_hash())
            && r != app.route.get_untracked()
        {
            app.route.set(r.clone());
            app.on_route(&r);
        }
    });
    on_cleanup(move || handle.remove());

    match surface {
        Surface::Desktop => view! { <desktop::DesktopApp/> }.into_any(),
        Surface::Android => view! { <android::AndroidApp/> }.into_any(),
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
