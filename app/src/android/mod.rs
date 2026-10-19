//! Android layout (Android v2, Material 3 — design/docs/android.md).
//!
//! Placeholder: the Android agent builds the real screens here (Planner feed
//! with *Needs you*, Runs, You, task detail, plan review) on top of the shared
//! state layer in `crate::state` — read the doc comment at the top of
//! `state/mod.rs` first. Everything a surface needs (data, derived
//! view-models, actions, routing, toasts, demo mode) is already there.

use crate::state::{AuthState, derive, use_app};
use leptos::prelude::*;

#[component]
pub fn AndroidApp() -> impl IntoView {
    let app = use_app();
    let running = Memo::new(move |_| app.data.with(derive::running_count));
    let tasks = Memo::new(move |_| app.data.with(|d| d.tasks.len()));
    let signed_in = move || matches!(app.auth.get(), AuthState::SignedIn);
    view! {
        <div class="android-placeholder">
            <div class="android-mark"><span></span></div>
            <div class="android-title">"Android layout coming"</div>
            <div class="android-sub">
                {move || if signed_in() {
                    format!("{} tasks · {} workers running", tasks.get(), running.get())
                } else {
                    "Open with ?demo=1 to load demo data".to_string()
                }}
            </div>
            <a class="android-link" href="?surface=desktop">"Use the desktop layout"</a>
        </div>
    }
}
