# Kiln for Android (Tauri 2)

The Android app is the same Leptos UI as desktop (`app/`, built by trunk),
running in the Tauri 2 shell. The UI picks the **Android v2** surface
(`app/src/android/`) automatically inside the
Android shell (also `?surface=android` in any browser, or a narrow touch
viewport). Everything native lives in one local plugin:

```
app/src-tauri/
  src/lib.rs                     shell: plugins, kiln:// deep links → WebView
  capabilities/mobile.json       kiln-mobile, deep-link, barcode-scanner permissions
  plugins/kiln-mobile/           Tauri 2 mobile plugin (Rust + Kotlin)
    src/lib.rs                   registers dev.kiln.mobile.KilnMobilePlugin
    build.rs                     command list → allow-* permissions
    android/build.gradle.kts     org.unifiedpush.android:connector:3.3.5
    android/src/main/AndroidManifest.xml   push service, action receiver, permissions
    android/src/main/java/dev/kiln/mobile/
      KilnMobilePlugin.kt        WebView commands + events, SpeechRecognizer
      KilnPushService.kt         UnifiedPush PushService (endpoint, messages)
      KilnNotifier.kt            channels, notifications, actions, deep links
      NotificationActionReceiver.kt   inline actions → daemon, no app launch
      DaemonClient.kt            POST /devices/push, /notifications/{id}/action
      KilnStore.kt               server URL, session token, endpoint (private prefs)
```

## Build

Prerequisites: Rust (`rustup target add aarch64-linux-android
armv7-linux-androideabi x86_64-linux-android i686-linux-android`), JDK 17+,
Android SDK + NDK (Android Studio, or `sdkmanager "platforms;android-36"
"build-tools;36.0.0" "ndk;27.2.12479018"`), `ANDROID_HOME` and `NDK_HOME`
set, the Tauri CLI (`cargo install tauri-cli --version "^2"`), and trunk.

```sh
cd app
cargo tauri android init          # once: generates src-tauri/gen/android (not committed)
cargo tauri android dev           # device/emulator, live reload via trunk serve
cargo tauri android build --apk   # release APK(s) in src-tauri/gen/android/app/build/outputs/apk/
cargo tauri android build --aab   # Play Store bundle
```

`cargo tauri android init` writes the Gradle project. You don't edit it by
hand: plugins plug in automatically. Each plugin crate with an
`android_path` (ours: `plugins/kiln-mobile/android`, plus the official
deep-link and barcode-scanner plugins) is added to
`gen/android/tauri.settings.gradle` / `tauri.build.gradle.kts` during the
build, and its library manifest is merged into the app manifest (the
UnifiedPush service, the action receiver, `POST_NOTIFICATIONS`,
`RECORD_AUDIO`). The deep-link plugin adds the `kiln://` intent filter to
`MainActivity` from `tauri.conf.json › plugins › deep-link › mobile`.

`minSdkVersion` is 26 (`tauri.conf.json › bundle › android`); the plugin
library itself targets compileSdk 36, minSdk 24.

### Signing

Release builds must be signed. Create a keystore once and keep it out of
git:

```sh
keytool -genkey -v -keystore ~/kiln-release.jks -keyalg RSA -keysize 4096 \
  -validity 10000 -alias kiln
```

Then add `gen/android/keystore.properties`
(`storeFile=/home/you/kiln-release.jks`, `storePassword=…`, `keyAlias=kiln`,
`keyPassword=…`) and the `signingConfigs { release { … } }` block to
`gen/android/app/build.gradle.kts` as described in the Tauri guide
"Android code signing". For CI, pass the keystore as a base64 secret and
write the file in the job. `apksigner verify --print-certs app-release.apk`
checks the result.

### F-Droid / sideloading

- Everything is FOSS-friendly: no Google Play Services, no Firebase. Push
  goes through **UnifiedPush**, so the user picks a distributor (ntfy,
  NextPush, …). The connector is Apache-2.0 on Maven Central.
- For F-Droid, build from source with the steps above (their build server
  provides the SDK/NDK); declare `cargo`, `trunk` and the Rust Android
  targets as build prerequisites, and keep `gen/android` generated at build
  time. Reproducible builds need a pinned toolchain (`rust-toolchain.toml`)
  and `SOURCE_DATE_EPOCH`.
- Sideloading: share the signed APK from the daemon's `/download` page.

## Pairing (QR → `kiln://pair`)

Desktop (Settings › Devices) and the TUI show a QR encoding
`kiln://pair?url=<server_url>&code=<one-time code>` (`POST /pair/start`).

- **Camera app:** scanning opens the link; Android routes `kiln://` to the
  app (deep-link plugin), the shell emits `kiln://deep-link` to the WebView
  (buffered for a cold start, `take_pending_links`), the Android surface
  parses it (`android/nav.rs › parse_deep_link`) and calls
  `AppState::pair_complete(url, code)` → `POST /pair/complete` with
  `client: android` → session token → signed in.
- **In app:** the sign-in screen's "Scan pairing QR" uses the official
  barcode-scanner plugin and follows the same path.

The token is kept by the WebView (cross-origin → `localStorage`) and handed
to the native plugin with `register_push` so background actions can call
the daemon.

## Push notifications (UnifiedPush)

1. After sign-in the WebView calls
   `plugin:kiln-mobile|register_push {server, token, deviceName}`. The
   plugin stores the session, asks for `POST_NOTIFICATIONS` (Android 13+),
   picks the user's distributor (`UnifiedPush.tryUseCurrentOrDefaultDistributor`)
   and registers. No distributor → `{status: "no_distributor"}`; the You
   screen says to install one (ntfy).
2. The distributor answers with an endpoint (`KilnPushService.onNewEndpoint`).
   With the app open, the plugin emits `pushEndpoint` and the WebView calls
   the shared `AppState::register_push` → `POST /api/v1/devices/push`.
   When the endpoint changes in the background, the service posts it itself.
3. The daemon's notifier POSTs `PushPayload` JSON to the endpoint for each
   notification whose channel the user left on (You › Notifications →
   `UserSettings.notification_channels`):
   `{ id, kind, channel, title, body, deep_link, actions:[{id,label}], progress?, resolved? }`.
4. `KilnNotifier` shows it in its channel:

   | Channel | Importance | Notes |
   |---|---|---|
   | Decisions | high (heads-up) | plans, escalations, memory approvals |
   | Failures | high | QA / reviewer failures |
   | Ready for review | default | PRs |
   | Live workers | low, ongoing, silent | progress bar, updated ≤ every 10 s |

   Large icon: a 36dp circle with the status glyph on a dark tint of the
   status color; notifications are grouped (M3 grouped corners in the shade).
5. **Actions.** `retry`, `approve`, `keep_shared`, `keep_private`, `reject`,
   `stop`, `escalate` go to `NotificationActionReceiver`, which POSTs
   `/api/v1/notifications/{id}/action` **without opening the app** and
   replaces the notification with the daemon's "✓ …" line (auto-dismissed).
   `open`, `review`, `view`, `decide`, `reply` open the app on the deep link
   (reply/decide → task + feedback sheet, review → plan review).
6. **Tap** → the app opens on `deep_link` (`notificationTap` event / 
   `take_pending_link` on cold start): plan → chat scrolled to the plan
   card with a 2.2 s primary ring; failed task → task detail; escalation →
   task + feedback sheet; PRs → chat at the PR card; workers live → Runs.
7. A `resolved` payload (the card was handled elsewhere) updates the
   notification to "✓ …"; the in-app card shows the same line.

The payload is sent unencrypted (the daemon doesn't do RFC 8291 yet); the
connector passes it through with `decrypted = false`. Adding Web Push
encryption means sending `endpoint.pubKeySet` with `register_push` and
encrypting in `crates/notifier`.

## Voice mode

`start_listening {lang}` runs Android's `SpeechRecognizer` (partial results
on, RMS → `speech {type: "level"}` for the orb, `partial` / `final` text,
restarted after pauses until the user taps ↑ or ✕). It asks for
`RECORD_AUDIO` on first use. We use our own plugin rather than
`tauri-plugin-stt` (0.2, single maintainer, pulls whisper.cpp + cpal into
the desktop build, drops RMS levels the orb needs). If recognition is
unavailable the overlay falls back to an editable transcript, so the
keyboard's dictation mic works. In a browser the UI uses the Web Speech API
plus `getUserMedia` + `AnalyserNode` for the level; Android WebView's Web
Speech support is unreliable, which is why the shell uses the native path.

## What can't be checked without an SDK/device

This repo's build machine has no Android SDK/NDK, so the Kotlin and the
Android Gradle wiring were written against the Tauri 2.12 plugin API
(`app.tauri.plugin.*`) and the UnifiedPush connector 3.3.5 sources but not
compiled. First build checklist: `cargo tauri android build --debug`,
install ntfy, sign in, confirm "Push · registered" on You, trigger a QA
failure from desktop, tap Retry in the shade with the app closed, and check
the card in chat shows "✓ Retried · attempt n/3 running".
