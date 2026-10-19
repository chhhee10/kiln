//! Kiln shell. All UI and logic live in the Leptos app (`../src`, built by
//! trunk into `../dist`); the shell only hosts the WebView, opens external
//! links (PRs, artifacts) in the system browser and, on Android, provides the
//! mobile entry point plus the native pieces the phone needs:
//!
//! - `kiln-mobile` plugin (`plugins/kiln-mobile`): UnifiedPush registration,
//!   native notifications with inline actions, SpeechRecognizer, taps.
//! - deep links (`kiln://pair?url=…&code=…`, `kiln://task/FP-139`, …) via
//!   the official deep-link plugin, forwarded to the WebView as the
//!   `kiln://deep-link` event (and buffered for a cold start).
//! - the barcode scanner for "Scan pairing QR".
//!
//! The app talks to the daemon over HTTPS/WSS on the tailnet directly from
//! the WebView. See `ANDROID.md` for the Android build.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Deep links that arrived before the WebView was listening (cold start).
#[derive(Default)]
struct PendingLinks(Mutex<Vec<(Instant, String)>>);

impl PendingLinks {
    #[allow(dead_code)]
    fn push(&self, urls: impl IntoIterator<Item = String>) {
        if let Ok(mut v) = self.0.lock() {
            let now = Instant::now();
            v.extend(urls.into_iter().map(|u| (now, u)));
        }
    }
}

/// The WebView calls this once at startup; stale links (already delivered
/// as events to a running WebView) are dropped.
#[tauri::command]
fn take_pending_links(state: tauri::State<'_, PendingLinks>) -> Vec<String> {
    let Ok(mut v) = state.0.lock() else {
        return vec![];
    };
    std::mem::take(&mut *v)
        .into_iter()
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(30))
        .map(|(_, u)| u)
        .collect()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_kiln_mobile::init())
        .manage(PendingLinks::default());
    #[cfg(mobile)]
    {
        builder = builder.plugin(tauri_plugin_deep_link::init());
    }
    #[cfg(target_os = "android")]
    {
        builder = builder.plugin(tauri_plugin_barcode_scanner::init());
    }
    builder
        .invoke_handler(tauri::generate_handler![take_pending_links])
        .setup(|_app| {
            #[cfg(mobile)]
            {
                use tauri::{Emitter, Manager};
                use tauri_plugin_deep_link::DeepLinkExt;
                let handle = _app.handle().clone();
                if let Ok(Some(urls)) = _app.deep_link().get_current() {
                    _app.state::<PendingLinks>()
                        .push(urls.iter().map(|u| u.to_string()));
                }
                _app.deep_link().on_open_url(move |event| {
                    let urls: Vec<String> = event.urls().iter().map(|u| u.to_string()).collect();
                    handle.state::<PendingLinks>().push(urls.clone());
                    let _ = handle.emit("kiln://deep-link", urls);
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Kiln shell");
}
