//! Voice mode: "LISTENING · PLANNER", the lime orb with waveform bars that
//! follow the voice level, a live transcript with a blinking caret, ✕ and
//! the 96dp send FAB. Sending posts a user bubble marked "◉ voice".
//!
//! Engines (`bridge::Engine`): Android SpeechRecognizer via the kiln-mobile
//! Tauri plugin; the Web Speech API in browsers; a scripted transcript in
//! demo mode; otherwise an editable transcript for keyboard dictation.

use super::bridge::{self, Engine, WebSession};
use super::use_ui;
use crate::state::platform;
use leptos::prelude::*;
use std::time::Duration;

/// The prototype's dictated sentence (demo mode).
pub const DEMO_TRANSCRIPT: &str =
    "Also log retries at debug level, not info, and keep them silent in json mode.";

/// Animation frame interval.
const FRAME_MS: u64 = 150;

/// Demo: words revealed after `frames` frames (one word per 675 ms, like
/// the prototype's 450 ms tick / 1.5).
pub fn demo_words(frames: u64) -> String {
    let n = (frames * FRAME_MS / 675) as usize;
    DEMO_TRANSCRIPT
        .split(' ')
        .take(n)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Bar heights (% of the 90dp max) for `t` beats at voice `level` 0..1.
pub fn wave(t: f64, level: f64) -> Vec<f64> {
    (0..32)
        .map(|i| {
            let i = i as f64;
            let base =
                18.0 + (i * 1.1 + t * 0.9).sin().abs() * 62.0 + ((i * 7.0 + t * 11.0) % 18.0);
            8.0 + (base - 8.0) * level.clamp(0.0, 1.0)
        })
        .collect()
}

type Sink = Box<dyn FnMut(serde_json::Value)>;

thread_local! {
    static SPEECH_SINK: std::cell::RefCell<Option<Sink>> = const { std::cell::RefCell::new(None) };
    static SPEECH_LISTENING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Route the plugin's `speech` events to the open overlay (one listener for
/// the app's lifetime; the overlay swaps the sink).
fn set_speech_sink(f: impl FnMut(serde_json::Value) + 'static) {
    if !SPEECH_LISTENING.with(|l| l.replace(true)) {
        bridge::on_plugin_event("speech", |v| {
            SPEECH_SINK.with(|s| {
                if let Some(f) = s.borrow_mut().as_mut() {
                    f(v)
                }
            })
        });
    }
    SPEECH_SINK.with(|s| *s.borrow_mut() = Some(Box::new(f)));
}

fn lang() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|w| w.navigator().language())
            .unwrap_or_else(|| "en-US".into())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        "en-US".into()
    }
}

#[component]
pub fn VoiceOverlay() -> impl IntoView {
    let ui = use_ui();
    let app = ui.app;
    let android_shell = bridge::in_tauri() && platform::is_android_ua();
    let engine = RwSignal::new(bridge::pick_engine(app.is_demo(), android_shell));
    let finals = RwSignal::new(String::new());
    let interim = RwSignal::new(String::new());
    let typed = RwSignal::new(String::new());
    let level = RwSignal::new(0.0f64);
    let frames = RwSignal::new(0u64);
    let note = RwSignal::new(None::<String>);
    let alive = StoredValue::new_local(true);
    let session = StoredValue::new_local(WebSession::default());

    match engine.get_untracked() {
        Engine::Web => {
            let s = WebSession::start(
                &lang(),
                move |f, i| {
                    if alive.get_value() {
                        finals.set(f);
                        interim.set(i);
                    }
                },
                move || {},
                move |e| {
                    if !alive.get_value() {
                        return;
                    }
                    if e == "not-allowed" || e == "service-not-allowed" || e == "audio-capture" {
                        engine.set(Engine::Keyboard);
                        note.set(Some(
                            "Microphone unavailable · dictate with your keyboard's mic".into(),
                        ));
                    }
                },
            );
            session.set_value(s);
        }
        Engine::Native => {
            set_speech_sink(move |v| {
                if !alive.get_value() {
                    return;
                }
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("level") => {
                        level.set(v.get("level").and_then(|l| l.as_f64()).unwrap_or(0.0))
                    }
                    Some("partial") => interim.set(
                        v.get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                    ),
                    Some("final") => {
                        let t = v
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        finals.update(|f| {
                            if !f.is_empty() {
                                f.push(' ');
                            }
                            f.push_str(&t);
                        });
                        interim.set(String::new());
                    }
                    Some("end") => {
                        // Android stops after a pause; keep dictating until ↑ or ✕.
                        bridge::invoke(
                            "plugin:kiln-mobile|start_listening",
                            serde_json::json!({ "lang": lang() }),
                            |_| {},
                        );
                    }
                    Some("error") => {
                        engine.set(Engine::Keyboard);
                        note.set(Some(
                            "Speech recognition unavailable · dictate with your keyboard's mic"
                                .into(),
                        ));
                    }
                    _ => {}
                }
            });
            bridge::invoke(
                "plugin:kiln-mobile|start_listening",
                serde_json::json!({ "lang": lang() }),
                move |r| {
                    if r.is_err() && alive.get_value() {
                        engine.set(Engine::Keyboard);
                        note.set(Some(
                            "Speech recognition unavailable · dictate with your keyboard's mic"
                                .into(),
                        ));
                    }
                },
            );
        }
        _ => {}
    }

    let h = set_interval_with_handle(
        move || {
            frames.update(|f| *f += 1);
            match engine.get_untracked() {
                Engine::Demo => {
                    let t = frames.get_untracked() as f64 * FRAME_MS as f64 / 450.0;
                    level.set(0.55 + (t * 0.8).sin().abs() * 0.45);
                    finals.set(demo_words(frames.get_untracked()));
                }
                Engine::Web => {
                    if let Some(l) = session.with_value(|s| s.level()) {
                        level.set(l);
                    } else {
                        // No meter (permission pending): a gentle idle wave.
                        let t = frames.get_untracked() as f64 * FRAME_MS as f64 / 450.0;
                        level.set(0.35 + (t * 0.8).sin().abs() * 0.2);
                    }
                }
                Engine::Native => {}
                Engine::Keyboard => level.set(0.25),
            }
        },
        Duration::from_millis(FRAME_MS),
    )
    .ok();
    let stop = move || {
        alive.set_value(false);
        SPEECH_SINK.with(|x| *x.borrow_mut() = None);
        session.update_value(|s| s.stop());
        if engine.get_untracked() == Engine::Native || android_shell {
            bridge::invoke(
                "plugin:kiln-mobile|stop_listening",
                serde_json::json!({}),
                |_| {},
            );
        }
    };
    on_cleanup(move || {
        if let Some(h) = h {
            h.clear();
        }
        stop();
    });

    let transcript = move || {
        let f = finals.get();
        let i = interim.get();
        match (f.is_empty(), i.is_empty()) {
            (_, true) => f,
            (true, false) => i,
            _ => format!("{f} {i}"),
        }
    };
    let send = move |_| {
        let text = if engine.get_untracked() == Engine::Keyboard {
            typed.get_untracked()
        } else {
            transcript()
        };
        stop();
        ui.nav.update(|n| n.voice = false);
        if text.trim().is_empty() {
            ui.snack("Didn't catch that");
        } else {
            ui.send(text, true);
        }
    };
    let cancel = move |_| {
        stop();
        ui.nav.update(|n| n.voice = false);
    };
    let t = move || frames.get() as f64 * FRAME_MS as f64 / 450.0;
    let orb = move || {
        format!(
            "transform:scale({:.3})",
            1.0 + level.get() * 0.14 * (0.6 + 0.4 * (t() * 0.8).sin().abs())
        )
    };
    let halo = move || {
        format!(
            "transform:scale({:.3})",
            1.15 + level.get() * 0.25 * (0.5 + 0.5 * (t() * 0.5 + 1.0).sin().abs())
        )
    };

    view! {
        <div class="a-voice">
            <div class="a-voice-label">"LISTENING · PLANNER"</div>
            <div class="a-voice-stage">
                <div class="a-halo" style=halo></div>
                <div class="a-orb" style=orb>
                    {move || wave(t(), level.get()).into_iter().map(|h| view! {
                        <div class="bar" style=format!("height:{h:.1}%")></div>
                    }).collect_view()}
                </div>
            </div>
            {move || if engine.get() == Engine::Keyboard {
                view! {
                    <textarea class="a-voice-typed" rows="3" autofocus placeholder="Tap the keyboard's mic and speak…"
                        prop:value=move || typed.get()
                        on:input=move |ev| typed.set(event_target_value(&ev))></textarea>
                }.into_any()
            } else {
                view! { <div class="a-transcript">{transcript}<span class="caret">"▍"</span></div> }.into_any()
            }}
            {move || note.get().map(|n| view! { <div class="a-voice-note">{n}</div> })}
            <div class="a-voice-actions">
                <button class="a-voice-cancel" aria-label="Cancel" on:click=cancel>"✕"</button>
                <button class="a-voice-send" aria-label="Send" on:click=send>"↑"</button>
                <div class="a-voice-hint">"tap ↑ to send"</div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_transcript_reveals_word_by_word() {
        assert_eq!(demo_words(0), "");
        assert_eq!(demo_words(5), "Also");
        assert_eq!(demo_words(1000), DEMO_TRANSCRIPT);
        let w = wave(3.0, 1.0);
        assert_eq!(w.len(), 32);
        assert!(w.iter().all(|h| (8.0..=100.0).contains(h)));
        assert!(wave(3.0, 0.0).iter().all(|h| (*h - 8.0).abs() < 1e-9));
    }
}
