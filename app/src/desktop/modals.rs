//! Overlays: feedback modal, lightbox, create-account and credential modals,
//! toast.

use crate::state::*;
use kiln_types::api::*;
use kiln_types::*;
use leptos::html;
use leptos::prelude::*;

#[component]
pub fn Overlays() -> impl IntoView {
    let app = use_app();
    view! {
        {move || app.feedback_open.get().then(|| view! { <Feedback/> })}
        {move || app.lightbox.get().map(|lb| view! {
            <div class="scrim center dark" on:click=move |_| app.lightbox.set(None)>
                <div class="lightbox" on:click=|ev| ev.stop_propagation()>
                    {match (lb.url.clone(), lb.video) {
                        (Some(u), true) => view! { <video src=u controls=true autoplay=true></video> }.into_any(),
                        (Some(u), false) => view! { <img src=u alt=lb.title.clone()/> }.into_any(),
                        (None, _) => view! {
                            <div class="t">{lb.title.clone()}</div>
                            <div class="s">"QA capture · 1440×900 · drop the real artifact here"</div>
                        }.into_any(),
                    }}
                </div>
            </div>
        })}
        {move || app.modal.get().map(|m| match m {
            Modal::CreateAccount => view! { <CreateAccount/> }.into_any(),
            Modal::AddCredential => view! { <AddCredential/> }.into_any(),
            Modal::RotateCredential(id) => view! { <RotateCredential id=id/> }.into_any(),
        })}
        {move || matches!(app.conn.get(), Conn::Reconnecting).then(|| view! {
            <div class="conn-banner"><span class="dot breathe" style="background:var(--human)"></span>"reconnecting to the Mac mini…"</div>
        })}
    }
}

#[component]
fn Feedback() -> impl IntoView {
    let app = use_app();
    let text = RwSignal::new(String::new());
    let attach_qa = RwSignal::new(true);
    let attach_verdict = RwSignal::new(true);
    let save_memory = RwSignal::new(false);
    let task = Memo::new(move |_| {
        app.current_task_id()
            .and_then(|id| app.data.with(|d| d.task(id).cloned()))
    });
    let area = NodeRef::<html::Textarea>::new();
    Effect::new(move |_| {
        if let Some(el) = area.get() {
            request_animation_frame(move || {
                let _ = el.focus();
            });
        }
    });
    let send = move || {
        let Some(t) = task.get_untracked() else {
            return;
        };
        app.task_action(
            t.id,
            TaskAction::Feedback {
                text: text.get_untracked(),
                attach_qa: attach_qa.get_untracked(),
                attach_verdict: attach_verdict.get_untracked(),
                save_memory: save_memory.get_untracked(),
            },
        );
        app.feedback_open.set(false);
    };
    let chip = move |sig: RwSignal<bool>, label: &'static str| {
        view! {
            <span class=move || if sig.get() { "chip on" } else { "chip" } tabindex="0" on:click=move |_| sig.update(|v| *v = !*v)>
                {move || if sig.get() { format!("✓ {label}") } else { label.to_string() }}
            </span>
        }
    };
    view! {
        <div class="scrim center" on:click=move |_| app.feedback_open.set(false)>
            <div class="modal" on:click=|ev| ev.stop_propagation()>
                <div class="modal-hd">
                    <b>"Send feedback to Builder"</b>
                    <span>{move || task.with(|t| t.as_ref().map(|t| {
                        let next = (t.attempt() + 1).min(t.loop_cap);
                        format!("{} · this starts attempt {} of {} with your note, the QA report and the reviewer verdict.", t.key, next, t.loop_cap)
                    })).unwrap_or_default()}</span>
                </div>
                <textarea class="textarea" rows="5" node_ref=area placeholder="e.g. Make the backoff cap a config value (default 30s). Seed the jitter RNG in tests."
                    prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" && (ev.meta_key() || ev.ctrl_key()) { ev.prevent_default(); send() }></textarea>
                <div class="chips">
                    {chip(attach_qa, "attach QA report")}
                    {chip(attach_verdict, "attach reviewer verdict")}
                    {chip(save_memory, "save as memory")}
                </div>
                <div class="modal-ft">
                    <button class="btn btn-secondary btn-lg" on:click=move |_| app.feedback_open.set(false)>"Cancel"</button>
                    <button class="btn btn-primary btn-lg" on:click=move |_| send()>"Send & restart Builder"</button>
                </div>
            </div>
        </div>
    }
}

fn input(
    label: &'static str,
    sig: RwSignal<String>,
    ty: &'static str,
    ph: &'static str,
) -> impl IntoView {
    view! {
        <label class="field">{label}
            <input class="input" type=ty placeholder=ph prop:value=move || sig.get() on:input=move |ev| sig.set(event_target_value(&ev))/>
        </label>
    }
}

#[component]
fn CreateAccount() -> impl IntoView {
    let app = use_app();
    let name = RwSignal::new(String::new());
    let email = RwSignal::new(String::new());
    let pw = RwSignal::new(String::new());
    let admin = RwSignal::new(false);
    let err = RwSignal::new(String::new());
    let domain = Memo::new(move |_| {
        app.data
            .with(|d| d.server.as_ref().and_then(|s| s.allowed_domain.clone()))
            .unwrap_or_default()
    });
    let new_domain = RwSignal::new(String::new());
    let editing_domain = RwSignal::new(false);
    let submit = move || {
        let (n, e, p) = (
            name.get_untracked(),
            email.get_untracked(),
            pw.get_untracked(),
        );
        if n.trim().is_empty() || !e.contains('@') {
            err.set("Name and a valid email are required.".into());
            return;
        }
        if p.chars().count() < 10 {
            err.set(
                "Temporary password: at least 10 characters. They can change it after signing in."
                    .into(),
            );
            return;
        }
        app.create_user(
            CreateUserRequest {
                email: e.trim().into(),
                name: n.trim().into(),
                password: p,
                role: if admin.get_untracked() {
                    UserRole::Admin
                } else {
                    UserRole::Member
                },
            },
            move || app.modal.set(None),
        );
    };
    view! {
        <div class="scrim center" on:click=move |_| app.modal.set(None)>
            <div class="modal narrow" on:click=|ev| ev.stop_propagation()>
                <div class="modal-hd"><b>"Create account"</b><span>"Teammates with a company email can also sign up themselves. Create accounts here for everyone else."</span></div>
                {input("Name", name, "text", "Jordan Lee")}
                {input("Email", email, "email", "jordan@company.dev")}
                {input("Temporary password", pw, "password", "at least 10 characters")}
                <div class="seg" style="align-self:flex-start">
                    <div class=move || if !admin.get() { "on" } else { "" } on:click=move |_| admin.set(false)>"Member"</div>
                    <div class=move || if admin.get() { "on" } else { "" } on:click=move |_| admin.set(true)>"Admin"</div>
                </div>
                {move || (!err.get().is_empty()).then(|| view! { <div class="login-err">{err.get()}</div> })}
                <div class="note">
                    {move || if editing_domain.get() {
                        view! {
                            <span style="display:flex;gap:6px;align-items:center">"Self-registration domain @"
                                <input class="input mono" style="height:28px;width:160px" prop:value=move || new_domain.get() on:input=move |ev| new_domain.set(event_target_value(&ev))/>
                                <button class="btn btn-secondary btn-sm" on:click=move |_| { app.set_domain(new_domain.get_untracked()); editing_domain.set(false); }>"Save"</button>
                            </span>
                        }.into_any()
                    } else {
                        view! {
                            <span>"Self-registration domain: "<span class="mono c-muted">{move || format!("@{}", domain.get())}</span>" · "
                                <a on:click=move |_| { new_domain.set(domain.get_untracked()); editing_domain.set(true); }>"Change"</a></span>
                        }.into_any()
                    }}
                </div>
                <div class="modal-ft">
                    <button class="btn btn-secondary btn-lg" on:click=move |_| app.modal.set(None)>"Cancel"</button>
                    <button class="btn btn-primary btn-lg" on:click=move |_| submit()>"Create account"</button>
                </div>
            </div>
        </div>
    }
}

const KINDS: [(CredentialKind, &str, &str); 5] = [
    (
        CredentialKind::AnthropicApiKey,
        "Anthropic API key",
        "ANTHROPIC_API_KEY",
    ),
    (
        CredentialKind::OpenaiApiKey,
        "OpenAI API key",
        "OPENAI_API_KEY",
    ),
    (
        CredentialKind::ClaudeOauth,
        "Claude subscription (OAuth token)",
        "CLAUDE_CODE_OAUTH_TOKEN",
    ),
    (
        CredentialKind::Litellm,
        "LiteLLM proxy",
        "LITELLM_BASE_URL + LITELLM_API_KEY",
    ),
    (CredentialKind::Other, "Other harness key", "custom env var"),
];

#[component]
fn AddCredential() -> impl IntoView {
    let app = use_app();
    let kind = RwSignal::new(0usize);
    let name = RwSignal::new(KINDS[0].1.to_string());
    let secret = RwSignal::new(String::new());
    let base = RwSignal::new(String::new());
    let env = RwSignal::new(String::new());
    let err = RwSignal::new(String::new());
    let submit = move || {
        let (k, _, _) = KINDS[kind.get_untracked()];
        if secret.get_untracked().trim().is_empty() {
            err.set("Paste the secret value.".into());
            return;
        }
        if k == CredentialKind::Litellm && base.get_untracked().trim().is_empty() {
            err.set("LiteLLM needs a base URL.".into());
            return;
        }
        if k == CredentialKind::Other && env.get_untracked().trim().is_empty() {
            err.set("Name the environment variable workers should receive.".into());
            return;
        }
        app.add_credential(
            CreateCredentialRequest {
                kind: k,
                name: name.get_untracked().trim().to_string(),
                secret: secret.get_untracked().trim().to_string(),
                base_url: (k == CredentialKind::Litellm)
                    .then(|| base.get_untracked().trim().to_string()),
                env_var: (k == CredentialKind::Other)
                    .then(|| env.get_untracked().trim().to_string()),
            },
            move || app.modal.set(None),
        );
    };
    view! {
        <div class="scrim center" on:click=move |_| app.modal.set(None)>
            <div class="modal narrow" on:click=|ev| ev.stop_propagation()>
                <div class="modal-hd"><b>"Add credential"</b><span>"Add an API key, OAuth token or LiteLLM endpoint. It's encrypted on the Mac mini and never shown again."</span></div>
                <label class="field">"Type"
                    <select class="select lg" on:change=move |ev| {
                        let i: usize = event_target_value(&ev).parse().unwrap_or(0);
                        kind.set(i);
                        name.set(KINDS[i].1.to_string());
                    }>
                        {KINDS.iter().enumerate().map(|(i, (_, l, _))| view! { <option value=i.to_string() selected=move || kind.get() == i>{*l}</option> }).collect_view()}
                    </select>
                </label>
                {input("Name", name, "text", "")}
                {move || (KINDS[kind.get()].0 == CredentialKind::Litellm).then(|| input("Base URL", base, "url", "https://litellm.tailnet:4000"))}
                {move || (KINDS[kind.get()].0 == CredentialKind::Other).then(|| input("Environment variable", env, "text", "GEMINI_API_KEY"))}
                {input("Secret", secret, "password", "paste the key or token")}
                <div class="note mono">{move || format!("injected as {}", KINDS[kind.get()].2)}</div>
                {move || (KINDS[kind.get()].0 == CredentialKind::ClaudeOauth).then(|| view! {
                    <div class="note c-human">"Provider terms apply: subscription OAuth tokens used in automation may be restricted. API keys are the safe default."</div>
                })}
                {move || (!err.get().is_empty()).then(|| view! { <div class="login-err">{err.get()}</div> })}
                <div class="modal-ft">
                    <button class="btn btn-secondary btn-lg" on:click=move |_| app.modal.set(None)>"Cancel"</button>
                    <button class="btn btn-primary btn-lg" on:click=move |_| submit()>"Save to vault"</button>
                </div>
            </div>
        </div>
    }
}

#[component]
fn RotateCredential(id: CredentialId) -> impl IntoView {
    let app = use_app();
    let cred = Memo::new(move |_| {
        app.data
            .with(|d| d.credentials.iter().find(|c| c.id == id).cloned())
    });
    let secret = RwSignal::new(String::new());
    let base = RwSignal::new(
        cred.get_untracked()
            .and_then(|c| c.base_url)
            .unwrap_or_default(),
    );
    let confirm_revoke = RwSignal::new(false);
    let submit = move || {
        if secret.get_untracked().trim().is_empty() {
            app.toast("Paste a new value to rotate", false);
            return;
        }
        let is_llm = cred.with_untracked(|c| {
            c.as_ref()
                .is_some_and(|c| c.kind == CredentialKind::Litellm)
        });
        app.rotate_credential(
            id,
            RotateCredentialRequest {
                secret: secret.get_untracked().trim().into(),
                base_url: is_llm.then(|| base.get_untracked()),
            },
            move || app.modal.set(None),
        );
    };
    view! {
        <div class="scrim center" on:click=move |_| app.modal.set(None)>
            <div class="modal narrow" on:click=|ev| ev.stop_propagation()>
                <div class="modal-hd">
                    <b>{move || format!("Rotate {}", cred.with(|c| c.as_ref().map(|c| c.name.clone()).unwrap_or_default()))}</b>
                    <span>{move || format!("Current value {}. Paste a new value; running workers keep the old one until they finish.", cred.with(|c| c.as_ref().map(|c| c.masked.clone()).unwrap_or_default()))}</span>
                </div>
                {move || cred.with(|c| c.as_ref().is_some_and(|c| c.kind == CredentialKind::Litellm)).then(|| input("Base URL", base, "url", ""))}
                {input("New secret", secret, "password", "paste the new key or token")}
                <div class="modal-ft">
                    <button class=move || if confirm_revoke.get() { "btn btn-secondary btn-lg btn-danger left" } else { "btn btn-secondary btn-lg left" }
                        on:click=move |_| if confirm_revoke.get_untracked() { app.revoke_credential(id, move || app.modal.set(None)) } else { confirm_revoke.set(true) }>
                        {move || if confirm_revoke.get() { "Click again to revoke" } else { "Revoke" }}
                    </button>
                    <button class="btn btn-secondary btn-lg" on:click=move |_| app.modal.set(None)>"Cancel"</button>
                    <button class="btn btn-primary btn-lg" on:click=move |_| submit()>"Rotate"</button>
                </div>
            </div>
        </div>
    }
}

#[component]
pub fn ToastView() -> impl IntoView {
    let app = use_app();
    move || {
        app.toast.get().map(|t| {
            view! {
                <div class="toast">
                    <span class="dot" style=format!("background:var(--{})", if t.ok { "passed" } else { "failed" })></span>
                    {t.message}
                </div>
            }
        })
    }
}
