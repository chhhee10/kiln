//! You: profile, machine card, notification channels, credential status
//! (read-only on the phone) and sign out.

use super::{PushState, use_ui};
use kiln_types::*;
use leptos::prelude::*;

/// (settings key, label, description) — keys match the daemon's
/// `UserSettings.notification_channels` (missing = enabled).
pub const CHANNELS: [(&str, &str, &str); 4] = [
    (
        "decisions",
        "Decisions",
        "Plans and memories waiting on you",
    ),
    ("failures", "Failures", "QA or reviewer failures"),
    (
        "ready_for_review",
        "Ready for review",
        "PRs ready for human review",
    ),
    ("live_workers", "Live workers", "Ongoing worker progress"),
];

/// Short credential status for the phone: "Valid", "Expires in 6d", …
pub fn credential_short(c: &CredentialSummary) -> (String, Status) {
    match &c.status {
        CredentialHealth::Untested => ("Untested".into(), Status::Queued),
        CredentialHealth::Valid => ("Valid".into(), Status::Passed),
        CredentialHealth::Reachable { .. } => ("Reachable".into(), Status::Passed),
        CredentialHealth::ExpiresSoon { days } => {
            (format!("Expires in {days}d"), Status::NeedsHuman)
        }
        CredentialHealth::Invalid { .. } => ("Invalid".into(), Status::Failed),
    }
}

#[component]
pub fn You() -> impl IntoView {
    let ui = use_ui();
    let app = ui.app;
    let me = Memo::new(move |_| app.data.with(|d| d.me.clone()));
    let machine = Memo::new(move |_| app.data.with(|d| d.machine.clone()));
    let running = Memo::new(move |_| {
        app.data
            .with(|d| d.tasks.iter().filter(|t| t.state.is_active()).count())
    });
    let host = Memo::new(move |_| {
        app.data.with(|d| {
            let h = d.machine.hostname.clone();
            if h.is_empty() {
                d.server
                    .as_ref()
                    .map(|s| s.hostname.clone())
                    .unwrap_or_default()
            } else {
                h
            }
        })
    });
    let creds = Memo::new(move |_| app.data.with(|d| d.credentials.clone()));
    let channel_on = move |k: &'static str| {
        app.data.with(|d| {
            d.settings
                .as_ref()
                .and_then(|s| s.notification_channels.get(k).copied())
                .unwrap_or(true)
        })
    };
    view! {
        <div class="a-page a-scroll a-you a-fade">
            <div class="a-profile">
                <div class="a-profile-av">{move || me.with(|m| m.as_ref().map(|u| u.initials.clone()).unwrap_or_default())}</div>
                <div class="a-profile-text">
                    <div class="n">{move || me.with(|m| m.as_ref().map(|u| u.name.clone()).unwrap_or_default())}</div>
                    <div class="e">{move || me.with(|m| m.as_ref().map(|u| format!("{} · {}", u.email, if u.is_admin() { "Admin" } else { "Member" })).unwrap_or_default())}</div>
                </div>
            </div>
            <div class="a-card a-machine">
                <div class="a-machine-head">
                    <span class="a-mdot" style=move || format!("background:{}", if machine.with(|m| m.runtime_ok || m.hostname.is_empty()) { Status::Passed.hex() } else { Status::Failed.hex() })></span>
                    {move || format!("{} · {}", host.get(), if machine.with(|m| m.tailnet_name.is_some()) || app.is_demo() { "Tailscale" } else { "LAN" })}
                    <span class="up">{move || {
                        let s = machine.with(|m| m.uptime_secs);
                        if s >= 86_400 { format!("up {} days", s / 86_400) } else if s >= 3600 { format!("up {} hours", s / 3600) } else { "just started".into() }
                    }}</span>
                </div>
                <div class="a-tiles">
                    <div class="a-tile"><span class="l">"CPU"</span><span class="v">{move || format!("{}%", machine.with(|m| m.cpu_percent.round() as i64))}</span></div>
                    <div class="a-tile"><span class="l">"Slots"</span><span class="v">{move || format!("{}/{}", running.get(), machine.with(|m| if m.container_slots > 0 { m.container_slots } else { 8 }))}</span></div>
                    <div class="a-tile"><span class="l">"Queue"</span><span class="v">{move || machine.with(|m| m.queue_depth.to_string())}</span></div>
                </div>
            </div>
            <div class="a-card a-section">
                <div class="a-section-title">"Notifications"</div>
                {CHANNELS.into_iter().map(|(k, n, desc)| {
                    let on = move || channel_on(k);
                    view! {
                        <div class="a-row-switch" role="switch" aria-checked=move || on().to_string()
                            on:click=move |_| {
                                let v = !channel_on(k);
                                app.update_settings(|s| { s.notification_channels.insert(k.to_string(), v); });
                            }>
                            <div class="tx"><span class="n">{n}</span><span class="d">{desc}</span></div>
                            <span class="a-switch" class:on=on><span class="knob"></span></span>
                        </div>
                    }
                }).collect_view()}
                {move || {
                    let line = match ui.push.get() {
                        PushState::InApp => None,
                        PushState::Registering => Some("Push · registering with your UnifiedPush distributor…".to_string()),
                        PushState::Registered(_) => Some("Push · registered with UnifiedPush".to_string()),
                        PushState::Unavailable(why) => Some(format!("Push · {why}")),
                    };
                    line.map(|l| view! { <div class="a-note">{l}</div> })
                }}
            </div>
            <div class="a-card a-section">
                <div class="a-section-title">"Credentials"</div>
                <For each=move || creds.get() key=|c| (c.id, format!("{:?}", c.status)) let:c>
                    {
                        let (st, s) = credential_short(&c);
                        view! {
                            <div class="a-row-cred"><span class="n">{c.name.clone()}</span><span class="s" style=format!("color:{}", s.hex())>{st}</span></div>
                        }
                    }
                </For>
                <Show when=move || creds.with(|c| c.is_empty())>
                    <div class="a-row-cred"><span class="n dim">"No credentials yet"</span></div>
                </Show>
                <div class="a-note">"Add or rotate keys on desktop or in the TUI."</div>
            </div>
            <button class="a-btn outlined wide" on:click=move |_| app.logout()>"Sign out"</button>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::demo;

    #[test]
    fn credential_labels() {
        let d = demo::seed(chrono::Utc::now());
        let labels: Vec<String> = d
            .credentials
            .iter()
            .map(|c| credential_short(c).0)
            .collect();
        assert_eq!(labels, vec!["Valid", "Valid", "Expires in 6d", "Reachable"]);
        assert_eq!(
            CHANNELS.map(|c| c.0),
            ["decisions", "failures", "ready_for_review", "live_workers"]
        );
    }
}
