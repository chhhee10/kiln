//! Browser / Tauri bridges for the Android surface: the Tauri IPC (the
//! `kiln-mobile` plugin in `src-tauri/plugins/kiln-mobile` for push, speech
//! and notification taps; the shell's deep-link event), the Web Speech API
//! and a microphone level meter (getUserMedia + AnalyserNode).
//!
//! The JS lives in a wasm-bindgen inline snippet (trunk bundles it); every
//! function feature-detects, so the same build runs in Chrome, Android
//! WebView and the Tauri shell. Native (`cargo test`) builds get no-op stubs.

#[cfg(target_arch = "wasm32")]
mod imp {
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen(inline_js = r#"
const T = () => (window.__TAURI__ && window.__TAURI__.core) ? window.__TAURI__ : null;
export function tauri_available() { return !!T(); }
export function tauri_invoke(cmd, args) {
  const t = T(); if (!t) return Promise.reject('no tauri');
  return t.core.invoke(cmd, args || {});
}
export function tauri_plugin_listen(plugin, event, cb) {
  const t = T(); if (!t || !t.core.addPluginListener) return;
  t.core.addPluginListener(plugin, event, (p) => cb(JSON.stringify(p || {}))).catch(() => {});
}
export function tauri_listen(event, cb) {
  const t = T(); if (!t || !t.event) return;
  t.event.listen(event, (e) => cb(JSON.stringify(e.payload === undefined ? null : e.payload))).catch(() => {});
}
export function speech_supported() { return !!(window.SpeechRecognition || window.webkitSpeechRecognition); }
export function speech_start(lang, onText, onEnd, onError) {
  const R = window.SpeechRecognition || window.webkitSpeechRecognition;
  if (!R) return null;
  const r = new R();
  r.lang = lang; r.continuous = true; r.interimResults = true;
  r.onresult = (ev) => {
    let fin = '', int = '';
    for (let i = 0; i < ev.results.length; i++) {
      const res = ev.results[i];
      if (res.isFinal) fin += res[0].transcript; else int += res[0].transcript;
    }
    onText(fin, int);
  };
  r.onerror = (e) => onError(String(e.error || 'error'));
  r.onend = () => onEnd();
  try { r.start(); } catch (e) { onError(String(e)); }
  return r;
}
export function speech_stop(r) { if (!r) return; try { r.onend = null; r.onresult = null; r.onerror = null; r.stop(); } catch (e) {} }
export function level_start() {
  const st = { level: 0, stop: false };
  if (!navigator.mediaDevices || !navigator.mediaDevices.getUserMedia) return st;
  navigator.mediaDevices.getUserMedia({ audio: true }).then((stream) => {
    if (st.stop) { stream.getTracks().forEach((t) => t.stop()); return; }
    const AC = window.AudioContext || window.webkitAudioContext;
    const ctx = new AC();
    const an = ctx.createAnalyser(); an.fftSize = 512;
    ctx.createMediaStreamSource(stream).connect(an);
    const buf = new Uint8Array(an.fftSize);
    st.ctx = ctx; st.stream = stream; st.live = true;
    const loop = () => {
      if (st.stop) return;
      an.getByteTimeDomainData(buf);
      let s = 0; for (let i = 0; i < buf.length; i++) { const x = (buf[i] - 128) / 128; s += x * x; }
      st.level = Math.min(1, Math.sqrt(s / buf.length) * 5);
      requestAnimationFrame(loop);
    };
    loop();
  }).catch(() => {});
  return st;
}
export function level_read(st) { return (st && st.level) || 0; }
export function level_live(st) { return !!(st && st.live); }
export function level_stop(st) {
  if (!st) return; st.stop = true;
  if (st.stream) st.stream.getTracks().forEach((t) => t.stop());
  if (st.ctx) st.ctx.close().catch(() => {});
}
export function guard_push() { history.pushState({ kiln: 'android' }, ''); }
export function state_is_guard(s) { return !!(s && s.kiln === 'android'); }
"#)]
    extern "C" {
        pub fn tauri_available() -> bool;
        pub fn tauri_invoke(cmd: &str, args: JsValue) -> js_sys::Promise;
        pub fn tauri_plugin_listen(plugin: &str, event: &str, cb: &Closure<dyn FnMut(String)>);
        pub fn tauri_listen(event: &str, cb: &Closure<dyn FnMut(String)>);
        pub fn speech_supported() -> bool;
        pub fn speech_start(
            lang: &str,
            on_text: &Closure<dyn FnMut(String, String)>,
            on_end: &Closure<dyn FnMut()>,
            on_error: &Closure<dyn FnMut(String)>,
        ) -> JsValue;
        pub fn speech_stop(r: &JsValue);
        pub fn level_start() -> JsValue;
        pub fn level_read(st: &JsValue) -> f64;
        pub fn level_live(st: &JsValue) -> bool;
        pub fn level_stop(st: &JsValue);
        pub fn guard_push();
        pub fn state_is_guard(s: &JsValue) -> bool;
    }
}

#[cfg(target_arch = "wasm32")]
pub use imp::*;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// The Tauri plugin that carries push, speech and notification taps.
pub const PLUGIN: &str = "kiln-mobile";

/// True inside the Tauri shell (desktop or Android).
pub fn in_tauri() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        tauri_available()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

/// Fire-and-forget Tauri command; `done` gets the JSON result or an error.
pub fn invoke(
    cmd: &str,
    args: serde_json::Value,
    done: impl FnOnce(Result<String, String>) + 'static,
) {
    #[cfg(target_arch = "wasm32")]
    {
        let args = js_sys::JSON::parse(&args.to_string()).unwrap_or(JsValue::NULL);
        let p = tauri_invoke(cmd, args);
        wasm_bindgen_futures::spawn_local(async move {
            match wasm_bindgen_futures::JsFuture::from(p).await {
                Ok(v) => done(Ok(js_sys::JSON::stringify(&v)
                    .ok()
                    .and_then(|s| s.as_string())
                    .unwrap_or_else(|| "null".into()))),
                Err(e) => done(Err(e.as_string().unwrap_or_else(|| format!("{e:?}")))),
            }
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (cmd, args);
        done(Err("not in tauri".into()));
    }
}

/// Listen to a `kiln-mobile` plugin event (payload as JSON). Lives forever.
pub fn on_plugin_event(event: &str, f: impl FnMut(serde_json::Value) + 'static) {
    #[cfg(target_arch = "wasm32")]
    {
        if !in_tauri() {
            return;
        }
        let mut f = f;
        let cb = Closure::<dyn FnMut(String)>::new(move |s: String| {
            f(serde_json::from_str(&s).unwrap_or(serde_json::Value::Null))
        });
        tauri_plugin_listen(PLUGIN, event, &cb);
        cb.forget();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (event, f);
    }
}

/// Listen to an app event emitted by the shell (`kiln://deep-link`).
pub fn on_app_event(event: &str, f: impl FnMut(serde_json::Value) + 'static) {
    #[cfg(target_arch = "wasm32")]
    {
        if !in_tauri() {
            return;
        }
        let mut f = f;
        let cb = Closure::<dyn FnMut(String)>::new(move |s: String| {
            f(serde_json::from_str(&s).unwrap_or(serde_json::Value::Null))
        });
        tauri_listen(event, &cb);
        cb.forget();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (event, f);
    }
}

// ---------------------------------------------------------------- speech

/// Which recognizer voice mode uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// Offline demo: a scripted transcript like the prototype.
    Demo,
    /// Android SpeechRecognizer through the kiln-mobile plugin.
    Native,
    /// The WebView/browser Web Speech API.
    Web,
    /// Nothing available: an editable transcript (keyboard dictation).
    Keyboard,
}

pub fn pick_engine(demo: bool, android_shell: bool) -> Engine {
    if demo {
        return Engine::Demo;
    }
    if android_shell {
        return Engine::Native;
    }
    #[cfg(target_arch = "wasm32")]
    {
        if speech_supported() {
            return Engine::Web;
        }
    }
    Engine::Keyboard
}

/// A running Web Speech session plus its callbacks and level meter.
#[derive(Default)]
pub struct WebSession {
    #[cfg(target_arch = "wasm32")]
    rec: Option<JsValue>,
    #[cfg(target_arch = "wasm32")]
    meter: Option<JsValue>,
    #[cfg(target_arch = "wasm32")]
    #[allow(clippy::type_complexity)]
    closures: Option<(
        Closure<dyn FnMut(String, String)>,
        Closure<dyn FnMut()>,
        Closure<dyn FnMut(String)>,
    )>,
}

impl WebSession {
    /// Start recognition (+ the mic level meter). `on_text(final, interim)`.
    pub fn start(
        lang: &str,
        on_text: impl FnMut(String, String) + 'static,
        on_end: impl FnMut() + 'static,
        on_error: impl FnMut(String) + 'static,
    ) -> Self {
        #[cfg(target_arch = "wasm32")]
        {
            let a = Closure::<dyn FnMut(String, String)>::new(on_text);
            let b = Closure::<dyn FnMut()>::new(on_end);
            let c = Closure::<dyn FnMut(String)>::new(on_error);
            let rec = speech_start(lang, &a, &b, &c);
            Self {
                rec: Some(rec),
                meter: Some(level_start()),
                closures: Some((a, b, c)),
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (lang, on_text, on_end, on_error);
            Self::default()
        }
    }

    /// Mic level 0..1, or None when no meter is running.
    pub fn level(&self) -> Option<f64> {
        #[cfg(target_arch = "wasm32")]
        {
            self.meter
                .as_ref()
                .filter(|m| level_live(m))
                .map(level_read)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            None
        }
    }

    pub fn stop(&mut self) {
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(r) = self.rec.take() {
                speech_stop(&r);
            }
            if let Some(m) = self.meter.take() {
                level_stop(&m);
            }
            // speech_stop detached the handlers, so JS can't call these any more.
            self.closures = None;
        }
    }
}

/// History guard entry for the back gesture (see `mod.rs`).
pub fn push_guard() {
    #[cfg(target_arch = "wasm32")]
    guard_push();
}

pub fn history_back() {
    #[cfg(target_arch = "wasm32")]
    if let Some(h) = web_sys::window().and_then(|w| w.history().ok()) {
        let _ = h.back();
    }
}
