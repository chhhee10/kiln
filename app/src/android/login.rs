//! Sign-in for the phone (built from the app's M3 pieces): pair by scanning the desktop/TUI QR (`kiln://pair?…`), or sign
//! in with server + email + password, or try the offline demo.

use super::bridge;
use super::nav::{DeepLink, parse_deep_link};
use super::use_ui;
use crate::state::*;
use leptos::prelude::*;

#[component]
pub fn Login() -> impl IntoView {
    let ui = use_ui();
    let app = ui.app;
    let unreachable = Memo::new(move |_| match app.auth.get() {
        AuthState::Unreachable { base, error } => Some((base, error)),
        _ => None,
    });
    let server = RwSignal::new(platform::local_get(STORAGE_SERVER).unwrap_or_else(|| {
        let o = platform::origin();
        if o.starts_with("http") {
            o
        } else {
            String::new()
        }
    }));
    let email = RwSignal::new(if app.is_demo() {
        "alex@company.dev".to_string()
    } else {
        String::new()
    });
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let in_shell = bridge::in_tauri();
    let sign_in = move || {
        error.set(None);
        if unreachable.get_untracked().is_some() {
            let s = server
                .get_untracked()
                .trim()
                .trim_end_matches('/')
                .to_string();
            if s.is_empty() {
                error.set(Some("Enter your Mac mini's address".into()));
                return;
            }
            platform::local_set(STORAGE_SERVER, &s);
            app.connect_to(s);
            return;
        }
        busy.set(true);
        app.login(email.get_untracked(), password.get_untracked(), move |e| {
            busy.set(false);
            error.set(Some(e));
        });
    };
    let scan = move |_| {
        bridge::invoke(
            "plugin:barcode-scanner|scan",
            serde_json::json!({ "formats": ["QR_CODE"], "windowed": false }),
            move |r| match r {
                Ok(s) => {
                    let v: serde_json::Value = serde_json::from_str(&s).unwrap_or_default();
                    let content = v
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or("")
                        .to_string();
                    match parse_deep_link(&content) {
                        Some(l @ DeepLink::Pair { .. }) => ui.go_link(l),
                        _ => error.set(Some("That QR isn't a Kiln pairing code".into())),
                    }
                }
                Err(e) => error.set(Some(format!("Scanner unavailable · {e}"))),
            },
        );
    };
    view! {
        <div class="a-login a-fade">
            <div class="a-brand lg"><span></span></div>
            <div class="a-login-title">"Kiln"</div>
            <div class="a-login-sub">{move || match unreachable.get() {
                Some((base, _)) => format!("Can't reach {base}. Check Tailscale, or enter the address."),
                None => "Sign in to your team's Mac mini".to_string(),
            }}</div>
            {in_shell.then(|| view! {
                <button class="a-btn primary wide tall" on:click=scan>"Scan pairing QR"</button>
                <div class="a-login-or">"Desktop › Settings › Devices, or the TUI's pair command, shows the code. Your camera app works too."</div>
            })}
            <Show when=move || unreachable.with(|u| u.is_some())>
                <label class="a-tf">
                    <span>"Server"</span>
                    <input inputmode="url" placeholder="https://mac-mini.tailnet.ts.net" prop:value=move || server.get() on:input=move |ev| server.set(event_target_value(&ev))/>
                </label>
            </Show>
            <Show when=move || unreachable.with(|u| u.is_none())>
                <label class="a-tf">
                    <span>"Email"</span>
                    <input type="email" autocomplete="username" prop:value=move || email.get() on:input=move |ev| email.set(event_target_value(&ev))/>
                </label>
                <label class="a-tf">
                    <span>"Password"</span>
                    <input type="password" autocomplete="current-password" prop:value=move || password.get()
                        on:input=move |ev| password.set(event_target_value(&ev))
                        on:keydown=move |ev| if ev.key() == "Enter" { sign_in() }/>
                </label>
            </Show>
            {move || error.get().map(|e| view! { <div class="a-login-err">{e}</div> })}
            <button class=move || if in_shell { "a-btn tonal wide tall" } else { "a-btn primary wide tall" } disabled=move || busy.get() on:click=move |_| sign_in()>
                {move || if unreachable.with(|u| u.is_some()) { "Connect" } else if busy.get() { "Signing in…" } else { "Sign in" }}
            </button>
            <button class="a-text-btn center" on:click=move |_| app.start_demo(true)>"Try the demo"</button>
        </div>
    }
}

#[component]
pub fn SetupNeeded() -> impl IntoView {
    view! {
        <div class="a-login a-fade">
            <div class="a-brand lg"><span></span></div>
            <div class="a-login-title">"Finish setup on desktop"</div>
            <div class="a-login-sub">"This Mac mini has no admin yet. Create the admin account in the desktop app or with `kiln setup`, then pair this phone."</div>
        </div>
    }
}
