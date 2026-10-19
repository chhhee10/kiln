//! Live updates over `/api/v1/ws` (wasm only): connect, apply every
//! `ServerEvent` to `app.data`, reconnect with capped exponential backoff and
//! resync over HTTP after each reconnect (events missed while offline are
//! recovered by the full reload).

use super::*;
use kiln_api_client::ws;
use kiln_types::event::{ClientMessage, ServerEvent};
use leptos::task::spawn_local;

use super::util::backoff_ms;

pub fn connect(app: AppState) {
    let Some(client) = app.client() else { return };
    if !matches!(app.auth.get_untracked(), AuthState::SignedIn) {
        return;
    }
    let reconnecting = app.ws_attempt.get_value() > 0;
    app.conn.set(if reconnecting {
        Conn::Reconnecting
    } else {
        Conn::Connecting
    });
    let on_event = move |ev: ServerEvent| {
        if let ServerEvent::Hello { .. } = &ev {
            let was_reconnect = app.ws_attempt.get_value() > 0;
            app.ws_attempt.set_value(0);
            app.conn.set(Conn::Live);
            // Re-follow what's on screen.
            let followed = app.followed.get_value();
            app.ws.with_value(|w| {
                if let Some(w) = w {
                    for t in followed {
                        w.send(&ClientMessage::FollowTask { task_id: t });
                    }
                }
            });
            if was_reconnect {
                spawn_local(async move { app.load_all().await });
            }
        }
        app.apply_events(vec![ev]);
    };
    let on_close = move || {
        // Never drop the socket from inside its own callback: defer.
        set_timeout(
            move || {
                app.ws.set_value(None);
                if !matches!(app.auth.get_untracked(), AuthState::SignedIn) || app.is_demo() {
                    app.conn.set(Conn::Offline);
                    return;
                }
                let n = app.ws_attempt.get_value();
                app.ws_attempt.set_value(n + 1);
                app.conn.set(Conn::Reconnecting);
                set_timeout(
                    move || connect(app),
                    std::time::Duration::from_millis(backoff_ms(n) as u64),
                );
            },
            std::time::Duration::from_millis(0),
        );
    };
    match ws::connect(&client, on_event, on_close) {
        Ok(conn) => app.ws.set_value(Some(conn)),
        Err(_) => {
            let n = app.ws_attempt.get_value();
            app.ws_attempt.set_value(n + 1);
            set_timeout(
                move || connect(app),
                std::time::Duration::from_millis(backoff_ms(n) as u64),
            );
        }
    }
}
