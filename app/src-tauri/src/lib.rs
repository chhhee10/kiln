//! Kiln shell. All UI and logic live in the Leptos app (`../src`, built by
//! trunk into `../dist`); the shell only hosts the WebView, opens external
//! links (PRs, artifacts) in the system browser and, on Android, provides the
//! mobile entry point. The app talks to the daemon over HTTPS/WSS on the
//! tailnet directly from the WebView.

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .run(tauri::generate_context!())
        .expect("error while running the Kiln shell");
}
