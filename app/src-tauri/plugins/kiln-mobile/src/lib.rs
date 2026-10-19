//! `kiln-mobile`: the Kiln Android shell plugin.
//!
//! Everything happens in Kotlin (`android/`); the WebView talks to it
//! directly (`invoke("plugin:kiln-mobile|register_push", …)`,
//! `addPluginListener("kiln-mobile", "pushEndpoint" | "notificationTap" |
//! "speech" | "pushFailed", …)`). On desktop the plugin is inert.

use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "dev.kiln.mobile";

/// Keeps the native plugin handle alive for the app's lifetime.
#[cfg(target_os = "android")]
pub struct KilnMobile<R: Runtime>(pub tauri::plugin::PluginHandle<R>);

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("kiln-mobile")
        .setup(|_app, _api| {
            #[cfg(target_os = "android")]
            {
                use tauri::Manager;
                let handle = _api.register_android_plugin(PLUGIN_IDENTIFIER, "KilnMobilePlugin")?;
                _app.manage(KilnMobile(handle));
            }
            Ok(())
        })
        .build()
}
