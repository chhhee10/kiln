//! Login (two-column split) and first-run setup (same visual language).

use crate::state::*;
use kiln_types::api::{CreateProjectRequest, SetupRequest};
use leptos::prelude::*;

pub const ALLOWLIST_ERR: &str =
    "That email isn’t on the allowlist. Ask an admin to create your account.";

/// `alex@company.dev` matches `company.dev` (case-insensitive).
pub fn email_allowed(email: &str, domain: &str) -> bool {
    let email = email.trim().to_lowercase();
    let domain = domain.trim().trim_start_matches('@').to_lowercase();
    !domain.is_empty()
        && email
            .rsplit_once('@')
            .is_some_and(|(user, d)| !user.is_empty() && d == domain)
}

fn left_column(headline: &'static str, sub: &'static str) -> impl IntoView {
    let app = use_app();
    let reach = Memo::new(move |_| app.reach.get());
    let version = Memo::new(move |_| {
        app.data
            .with(|d| d.server.as_ref().map(|s| s.version.clone()))
            .unwrap_or_else(|| kiln_types::VERSION.to_string())
    });
    view! {
        <div class="login-left">
            <div class="login-brand"><div class="brand-mark"></div>{kiln_types::PRODUCT_NAME}</div>
            <div class="login-hero">
                <div class="login-h1">{headline}</div>
                <div class="login-sub">{sub}</div>
            </div>
            <div class="login-foot">
                <div>
                    <span class="dot" style=move || format!("background:var(--{})", if reach.with(|r| r.ok) { "passed" } else { "failed" })></span>
                    {move || reach.with(|r| {
                        let host = if r.host.is_empty() { "daemon".to_string() } else { r.host.trim_start_matches("https://").trim_start_matches("http://").to_string() };
                        match (r.ok, r.latency_ms) {
                            (true, Some(ms)) => format!("{host} · reachable · {ms}ms"),
                            (true, None) => format!("{host} · reachable"),
                            _ => format!("{host} · unreachable"),
                        }
                    })}
                </div>
                <div>{move || format!("v{} · Tailscale private network", version.get())}</div>
            </div>
        </div>
    }
}

#[component]
pub fn Login() -> impl IntoView {
    let app = use_app();
    let demo = app.is_demo();
    let email = RwSignal::new(if demo {
        "alex@company.dev".to_string()
    } else {
        String::new()
    });
    let name = RwSignal::new(String::new());
    let pw = RwSignal::new(String::new());
    let err = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let register = RwSignal::new(false);
    let domain = Memo::new(move |_| {
        app.data
            .with(|d| d.server.as_ref().and_then(|s| s.allowed_domain.clone()))
            .unwrap_or_default()
    });
    let unreachable = Memo::new(move |_| match app.auth.get() {
        AuthState::Unreachable { base, error } => Some((base, error)),
        _ => None,
    });
    let server = RwSignal::new(platform::local_get(STORAGE_SERVER).unwrap_or_default());

    let submit = move || {
        if busy.get_untracked() {
            return;
        }
        let e = email.get_untracked();
        let dom = domain.get_untracked();
        if !dom.is_empty() && !email_allowed(&e, &dom) {
            err.set(ALLOWLIST_ERR.into());
            return;
        }
        if pw.get_untracked().is_empty() && !demo {
            err.set("Enter your password.".into());
            return;
        }
        err.set(String::new());
        busy.set(true);
        let on_err = move |m: String| {
            busy.set(false);
            err.set(m);
        };
        if register.get_untracked() {
            app.register(e, name.get_untracked(), pw.get_untracked(), on_err);
        } else {
            app.login(e, pw.get_untracked(), on_err);
        }
    };

    let connect = move || {
        let s = server
            .get_untracked()
            .trim()
            .trim_end_matches('/')
            .to_string();
        platform::local_set(STORAGE_SERVER, &s);
        app.connect_to(s);
    };

    view! {
        <div class="login">
            {left_column("Plan it. Dispatch it. Review the PR.", "Builder, QA and Reviewer agents run in isolated containers on the team Mac mini. Only you decide what ships.")}
            <div class="login-right">
                <div class="login-form">
                    {move || match unreachable.get() {
                        Some((base, error)) => view! {
                            <div style="display:flex;flex-direction:column;gap:6px">
                                <div class="login-title">"Connect to your Mac mini"</div>
                                <div class="login-help">
                                    {if base.is_empty() { "No Kiln daemon answered on this address.".to_string() } else { format!("No Kiln daemon answered at {base}.") }}
                                    " Enter its Tailscale URL, or explore with demo data."
                                </div>
                            </div>
                            <div class="unreach">
                                <label class="field">"Server URL"
                                    <input class="input" placeholder="https://mac-mini.tailnet-abc.ts.net" prop:value=move || server.get()
                                        on:input=move |ev| server.set(event_target_value(&ev))
                                        on:keydown=move |ev| if ev.key() == "Enter" { connect() } />
                                </label>
                                <div class="login-err">{error}</div>
                                <div class="row">
                                    <button class="btn btn-primary btn-md" on:click=move |_| connect()>"Connect"<span class="k">"↵"</span></button>
                                    <button class="btn btn-secondary btn-md" on:click=move |_| app.start_demo(false)>"Try demo"</button>
                                </div>
                            </div>
                        }.into_any(),
                        None => view! {
                            <div style="display:flex;flex-direction:column;gap:6px">
                                <div class="login-title">{move || if register.get() { "Create your account" } else { "Sign in" }}</div>
                                <div class="login-help">{move || if register.get() {
                                    "Use your company email. Your account is ready as soon as you sign up."
                                } else {
                                    "Use your company email. If you don't have one, ask an admin to create your account."
                                }}</div>
                            </div>
                            {move || register.get().then(|| view! {
                                <label class="field">"Name"
                                    <input class="input" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev))/>
                                </label>
                            })}
                            <label class="field">"Email"
                                <input class="input" type="email" autocomplete="username" prop:value=move || email.get() on:input=move |ev| email.set(event_target_value(&ev))/>
                            </label>
                            <label class="field">
                                <span style="display:flex">"Password"<span style="margin-left:auto;color:var(--dim)">"Forgot? Ask an admin"</span></span>
                                <input class="input" type="password" autocomplete="current-password" prop:value=move || pw.get()
                                    on:input=move |ev| pw.set(event_target_value(&ev))
                                    on:keydown=move |ev| if ev.key() == "Enter" { submit() } />
                            </label>
                            {move || (!err.get().is_empty()).then(|| view! { <div class="login-err">{err.get()}</div> })}
                            <button class="btn btn-primary btn-xl" style="border-radius:7px" disabled=move || busy.get() on:click=move |_| submit()>
                                {move || if register.get() { "Create account" } else { "Sign in" }}<span class="k" style="margin-left:-4px">"↵"</span>
                            </button>
                            <div class="login-note">
                                "Allowed domain: "<span class="mono">{move || format!("@{}", domain.get())}</span>". Other accounts are created by an admin."
                            </div>
                            <span class="login-alt" on:click=move |_| { register.update(|r| *r = !*r); err.set(String::new()); }>
                                {move || if register.get() { "Have an account? Sign in" } else { "New here? Create an account with your company email" }}
                            </span>
                        }.into_any(),
                    }}
                </div>
            </div>
        </div>
    }
}

#[component]
pub fn Setup() -> impl IntoView {
    let app = use_app();
    let name = RwSignal::new(String::new());
    let email = RwSignal::new(String::new());
    let pw = RwSignal::new(String::new());
    let domain = RwSignal::new(String::new());
    let p_key = RwSignal::new(String::new());
    let p_name = RwSignal::new(String::new());
    let p_repo = RwSignal::new(String::new());
    let p_branch = RwSignal::new("main".to_string());
    let err = RwSignal::new(String::new());
    let busy = RwSignal::new(false);

    // Suggest the domain from the admin's email.
    let on_email = move |v: String| {
        if (domain.get_untracked().is_empty()
            || email.get_untracked().split('@').nth(1) == Some(domain.get_untracked().as_str()))
            && let Some((_, d)) = v.rsplit_once('@')
        {
            domain.set(d.to_string());
        }
        email.set(v);
    };

    let submit = move || {
        if busy.get_untracked() {
            return;
        }
        let (n, e, p, d) = (
            name.get_untracked(),
            email.get_untracked(),
            pw.get_untracked(),
            domain.get_untracked(),
        );
        let d = d.trim().trim_start_matches('@').to_string();
        if n.trim().is_empty() || e.trim().is_empty() || d.is_empty() {
            err.set("Name, email and allowed domain are required.".into());
            return;
        }
        if !email_allowed(&e, &d) {
            err.set(format!("The admin email must be on @{d}."));
            return;
        }
        if p.chars().count() < 10 {
            err.set("Use at least 10 characters for the password.".into());
            return;
        }
        let project = (!p_repo.get_untracked().trim().is_empty()).then(|| {
            let pname = p_name.get_untracked();
            let key = p_key.get_untracked();
            CreateProjectRequest {
                key: if key.trim().is_empty() {
                    pname
                        .chars()
                        .filter(|c| c.is_ascii_alphabetic())
                        .take(3)
                        .collect::<String>()
                        .to_uppercase()
                } else {
                    key.trim().to_uppercase()
                },
                name: if pname.trim().is_empty() {
                    "First project".into()
                } else {
                    pname
                },
                repo_url: p_repo.get_untracked().trim().to_string(),
                default_branch: p_branch.get_untracked(),
                qa_path: "qa.yaml".into(),
            }
        });
        err.set(String::new());
        busy.set(true);
        app.setup(
            SetupRequest {
                admin_email: e.trim().to_string(),
                admin_name: n.trim().to_string(),
                password: p,
                allowed_domain: d,
            },
            project,
            move |m| {
                busy.set(false);
                err.set(m);
            },
        );
    };
    let field = move |label: &'static str,
                      sig: RwSignal<String>,
                      ph: &'static str,
                      ty: &'static str| {
        view! {
            <label class="field">{label}
                <input class="input" type=ty placeholder=ph prop:value=move || sig.get() on:input=move |ev| sig.set(event_target_value(&ev))/>
            </label>
        }
    };

    view! {
        <div class="login">
            {left_column("Set up Kiln on this machine.", "Create the admin account, choose who can sign up, and register the first repository. Teammates join with their company email.")}
            <div class="login-right">
                <div class="login-form wide">
                    <div style="display:flex;flex-direction:column;gap:6px">
                        <div class="login-title">"First-run setup"</div>
                        <div class="login-help">"This runs once. The account you create here is the admin."</div>
                    </div>
                    <div class="form-section">"ADMIN ACCOUNT"</div>
                    {field("Name", name, "Alex Rivera", "text")}
                    <label class="field">"Email"
                        <input class="input" type="email" placeholder="alex@company.dev" prop:value=move || email.get() on:input=move |ev| on_email(event_target_value(&ev))/>
                    </label>
                    {field("Password", pw, "at least 10 characters", "password")}
                    <label class="field">"Allowed email domain"
                        <input class="input mono" placeholder="company.dev" prop:value=move || domain.get() on:input=move |ev| domain.set(event_target_value(&ev))/>
                    </label>
                    <div class="form-section">"FIRST PROJECT · OPTIONAL"</div>
                    <div class="form-row">
                        {field("Name", p_name, "Acme CLI/SDK", "text")}
                        {field("Key", p_key, "FP", "text")}
                    </div>
                    {field("Repository URL", p_repo, "git@github.com:company/cli.git", "text")}
                    {field("Default branch", p_branch, "main", "text")}
                    {move || (!err.get().is_empty()).then(|| view! { <div class="login-err">{err.get()}</div> })}
                    <button class="btn btn-primary btn-xl" style="border-radius:7px" disabled=move || busy.get() on:click=move |_| submit()>
                        "Create admin & continue"<span class="k">"↵"</span>
                    </button>
                    <div class="login-note">"Install the GitHub App from Admin later. "<span class="mono">"qa.yaml"</span>" is read from the repository root by default."</div>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist() {
        assert!(email_allowed("alex@company.dev", "company.dev"));
        assert!(email_allowed(" Alex@Company.dev ", "@company.dev"));
        assert!(!email_allowed("alex@gmail.com", "company.dev"));
        assert!(!email_allowed("alex@evilcompany.dev", "company.dev"));
        assert!(!email_allowed("@company.dev", "company.dev"));
        assert!(!email_allowed("alex@company.dev", ""));
    }
}
