// Commands the WebView may invoke on the native (Kotlin) plugin. Tauri
// generates an `allow-<command>` permission for each; `default.toml` grants
// them to the app.
const COMMANDS: &[&str] = &[
    "register_push",
    "unregister_push",
    "take_pending_link",
    "start_listening",
    "stop_listening",
    "register_listener",
    "remove_listener",
];

fn main() {
    tauri_plugin::Builder::new(COMMANDS)
        .android_path("android")
        .build();
}
