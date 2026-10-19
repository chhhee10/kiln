//! The API layer: every write a surface can make, for both backends.
//! Live calls go through `kiln_api_client::Client`; demo calls go through the
//! pure functions in `demo.rs`. Both end in `Data::apply`, so the UI can't
//! tell them apart.

use super::*;
use kiln_api_client::ClientError;
use kiln_types::api::*;
use kiln_types::event::{ClientMessage, ServerEvent};
use kiln_types::memory::*;
use kiln_types::plan::PlanSpec;
use kiln_types::*;
use leptos::task::spawn_local;

fn cross_origin(base: &str) -> bool {
    !base.is_empty() && base.trim_end_matches('/') != platform::origin()
}

impl AppState {
    // ------------------------------------------------------------ plumbing

    pub(crate) fn apply_events(&self, evs: Vec<ServerEvent>) {
        let mut fx = Vec::new();
        self.data.update(|d| {
            for e in &evs {
                fx.extend(d.apply(e));
            }
        });
        for data::Effect::Toast { message, ok } in fx {
            self.toast(message, ok);
        }
    }

    fn demo_outcome(&self, o: demo::Outcome) {
        self.apply_events(o.events);
        if let Some(t) = o.toast {
            self.toast(t, true);
        }
    }

    fn with_sim<R>(&self, f: impl FnOnce(&mut demo::DemoSim, &data::Data) -> R) -> Option<R> {
        self.sim
            .try_update_value(|s| s.as_mut().map(|s| self.data.with_untracked(|d| f(s, d))))
            .flatten()
    }

    /// Show an API error; a 401 drops back to the login screen.
    pub(crate) fn fail(&self, e: ClientError) {
        if e.is_unauthorized() {
            self.auth.set(AuthState::SignedOut);
            return;
        }
        self.toast(e.to_string(), false);
    }

    // ------------------------------------------------------------ connect & auth

    /// Probe a daemon at `base` ("" = same origin) and restore the session.
    pub fn connect_to(&self, base: String) {
        let app = *self;
        let client = Client::new(base.clone());
        if cross_origin(&base)
            && let Some(t) = platform::local_get(STORAGE_TOKEN)
        {
            client.set_token(Some(t));
        }
        self.backend.set_value(Backend::Live(client.clone()));
        self.auth.set(AuthState::Booting);
        spawn_local(async move {
            let t0 = platform::perf_now();
            match client.check_compatible().await {
                Ok(info) => {
                    let latency = (platform::perf_now() - t0).round() as u32;
                    let host = info
                        .tailnet_host
                        .clone()
                        .unwrap_or_else(|| info.hostname.clone());
                    app.reach.set(Reach {
                        host,
                        latency_ms: Some(latency),
                        ok: true,
                    });
                    let setup = info.setup_required;
                    app.data.update(|d| d.server = Some(info));
                    if setup {
                        app.auth.set(AuthState::Setup);
                        return;
                    }
                    match client.me().await {
                        Ok(me) => {
                            app.data.update(|d| d.load_me(me));
                            app.signed_in().await;
                        }
                        Err(_) => app.auth.set(AuthState::SignedOut),
                    }
                }
                Err(e) => {
                    let host = if base.is_empty() {
                        platform::origin()
                    } else {
                        base.clone()
                    };
                    app.reach.set(Reach {
                        host,
                        latency_ms: None,
                        ok: false,
                    });
                    app.auth.set(AuthState::Unreachable {
                        base,
                        error: e.to_string(),
                    });
                }
            }
        });
    }

    async fn signed_in(&self) {
        self.auth.set(AuthState::SignedIn);
        self.load_all().await;
        #[cfg(target_arch = "wasm32")]
        live::connect(*self);
        let r = self.route.get_untracked();
        self.on_route(&r);
        // Machine card fallback poll (events usually keep it fresh).
        static TICKER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if TICKER.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let app = *self;
        set_interval(
            move || {
                app.tick.update(|t| *t += 1);
                if app.tick.get_untracked().is_multiple_of(10)
                    && matches!(app.auth.get_untracked(), AuthState::SignedIn)
                    && let Some(c) = app.client()
                {
                    spawn_local(async move {
                        if let Ok(m) = c.machine().await {
                            app.data.update(|d| d.machine = m);
                        }
                    });
                }
                if app.current_screen() == Screen::Admin
                    && app.tick.get_untracked().is_multiple_of(4)
                {
                    app.refresh_admin();
                }
            },
            std::time::Duration::from_millis(1000),
        );
    }

    fn remember_token(&self, client: &Client, token: &str) {
        // Same-origin browsers use the HTTP-only cookie; only cross-origin
        // clients (the Tauri WebView) keep the bearer token.
        if cross_origin(client.base()) {
            platform::local_set(STORAGE_TOKEN, token);
        }
    }

    /// Sign in. `on_error` receives the message for the form's error line.
    pub fn login(&self, email: String, password: String, on_error: impl Fn(String) + 'static) {
        let app = *self;
        if self.is_demo() {
            self.auth.set(AuthState::SignedIn);
            let r = self.route.get_untracked();
            self.on_route(&r);
            return;
        }
        let Some(client) = self.client() else { return };
        spawn_local(async move {
            let req = LoginRequest {
                email,
                password,
                client: Some(if platform::is_tauri() {
                    ClientKind::Desktop
                } else {
                    ClientKind::Web
                }),
                device_name: Some(if platform::is_tauri() {
                    "Kiln desktop".into()
                } else {
                    "Browser".into()
                }),
            };
            match client.login(&req).await {
                Ok(r) => {
                    app.remember_token(&client, &r.token);
                    match client.me().await {
                        Ok(me) => app.data.update(|d| d.load_me(me)),
                        Err(_) => app.data.update(|d| d.me = Some(r.user)),
                    }
                    app.signed_in().await;
                }
                Err(e) => on_error(e.to_string()),
            }
        });
    }

    pub fn register(
        &self,
        email: String,
        name: String,
        password: String,
        on_error: impl Fn(String) + 'static,
    ) {
        if self.is_demo() {
            self.login(email, password, on_error);
            return;
        }
        let app = *self;
        let Some(client) = self.client() else { return };
        spawn_local(async move {
            match client
                .register(&RegisterRequest {
                    email,
                    name,
                    password,
                })
                .await
            {
                Ok(r) => {
                    app.remember_token(&client, &r.token);
                    if let Ok(me) = client.me().await {
                        app.data.update(|d| d.load_me(me));
                    }
                    app.signed_in().await;
                }
                Err(e) => on_error(e.to_string()),
            }
        });
    }

    /// First-run setup: admin account + allowed domain, then the first project.
    pub fn setup(
        &self,
        req: SetupRequest,
        project: Option<CreateProjectRequest>,
        on_error: impl Fn(String) + 'static,
    ) {
        let app = *self;
        if self.is_demo() {
            self.data.update(|d| {
                if let Some(s) = d.server.as_mut() {
                    s.allowed_domain = Some(req.allowed_domain.clone());
                }
            });
            self.auth.set(AuthState::SignedIn);
            self.toast("Admin account created · welcome to Kiln", true);
            return;
        }
        let Some(client) = self.client() else { return };
        spawn_local(async move {
            match client.setup(&req).await {
                Ok(r) => {
                    app.remember_token(&client, &r.token);
                    if let Some(p) = project
                        && let Err(e) = client.create_project(&p).await
                    {
                        app.toast(format!("Project not created: {e}"), false);
                    }
                    if let Ok(me) = client.me().await {
                        app.data.update(|d| d.load_me(me));
                    }
                    app.signed_in().await;
                    app.toast("Admin account created · welcome to Kiln", true);
                }
                Err(e) => on_error(e.to_string()),
            }
        });
    }

    pub fn logout(&self) {
        self.palette_open.set(false);
        self.peek.set(None);
        if self.is_demo() {
            self.auth.set(AuthState::SignedOut);
            return;
        }
        platform::local_remove(STORAGE_TOKEN);
        self.ws.set_value(None);
        self.conn.set(Conn::Offline);
        let app = *self;
        if let Some(c) = self.client() {
            spawn_local(async move {
                let _ = c.logout().await;
                app.data.update(|d| {
                    let server = d.server.take();
                    *d = data::Data {
                        server,
                        ..Default::default()
                    };
                });
                app.auth.set(AuthState::SignedOut);
            });
        }
    }

    /// Fetch everything the shell needs (initial load and WS resync).
    pub async fn load_all(&self) {
        let Some(c) = self.client() else { return };
        let (bq, tq) = (BoardQuery::default(), ThreadQuery::default());
        let mq = MemoryQuery {
            limit: Some(500),
            ..Default::default()
        };
        let board = c.board(&bq);
        let threads = c.threads(&tq);
        let memory = c.memory(&mq);
        let creds = c.credentials();
        let harn = c.harnesses();
        let notifs = c.notifications();
        let machine = c.machine();
        let (board, threads, memory, creds, harn, notifs, machine) =
            futures::join!(board, threads, memory, creds, harn, notifs, machine);
        let mut err = None;
        self.data.update(|d| {
            match board {
                Ok(b) => d.load_board(b),
                Err(e) => err = Some(e),
            }
            if let Ok(t) = threads {
                d.load_threads(t);
            }
            if let Ok(m) = memory {
                d.load_memory(m);
            }
            if let Ok(c) = creds {
                d.credentials = c;
            }
            if let Ok(h) = harn {
                d.harnesses = Some(h);
            }
            if let Ok(n) = notifs {
                d.notifications = n;
            }
            if let Ok(m) = machine {
                d.machine = m;
            }
        });
        if let Some(e) = err {
            self.fail(e);
        }
        if self.is_admin() {
            self.refresh_admin();
        }
        // Re-load what's on screen.
        if let Some(t) = self.selected_task.get_untracked() {
            self.load_task(t);
        }
    }

    pub fn refresh_admin(&self) {
        if self.is_demo() || !self.is_admin() {
            return;
        }
        let app = *self;
        if let Some(c) = self.client() {
            spawn_local(async move {
                match c.admin_overview().await {
                    Ok(a) => app.data.update(|d| d.load_admin(a)),
                    Err(e) => app.fail(e),
                }
            });
        }
    }

    // ------------------------------------------------------------ tasks

    pub(crate) fn load_task(&self, id: TaskId) {
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            let lq = LogQuery {
                limit: Some(200),
                ..Default::default()
            };
            let (detail, logs) = futures::join!(c.task(id), c.task_logs(id, &lq));
            app.data.update(|d| {
                if let Ok(det) = detail {
                    d.load_task_detail(det);
                }
                if let Ok(l) = logs {
                    d.load_logs(id, l);
                }
            });
        });
        self.follow(id);
    }

    fn follow(&self, id: TaskId) {
        let prev: Vec<TaskId> = self.followed.with_value(|f| f.clone());
        if prev.contains(&id) {
            return;
        }
        self.followed.set_value(vec![id]);
        #[cfg(target_arch = "wasm32")]
        self.ws.with_value(|ws| {
            if let Some(ws) = ws {
                for p in &prev {
                    ws.send(&ClientMessage::UnfollowTask { task_id: *p });
                }
                ws.send(&ClientMessage::FollowTask { task_id: id });
            }
        });
        #[cfg(not(target_arch = "wasm32"))]
        let _ = (prev, ClientMessage::Ping);
    }

    /// Stop / Retry / Escalate / Feedback on a task (PRD §4.4).
    pub fn task_action(&self, id: TaskId, action: TaskAction) {
        if self.is_demo() {
            let now = chrono::Utc::now();
            if let Some(o) = self.with_sim(|s, d| demo::act_task(s, d, id, &action, now)) {
                self.demo_outcome(o);
            }
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c.task_action(id, action.clone()).await {
                Ok(t) => {
                    let msg = match &action {
                        TaskAction::Retry => format!(
                            "{} restarted · attempt {}/{}",
                            t.key,
                            t.attempt(),
                            t.loop_cap
                        ),
                        TaskAction::Stop => format!("{} stopped · container released", t.key),
                        TaskAction::Escalate { .. } => {
                            format!("{} escalated · team notified on Android", t.key)
                        }
                        TaskAction::Feedback { .. } => {
                            format!("Feedback sent · Builder restarted on {}", t.key)
                        }
                    };
                    app.data.update(|d| d.upsert_task(t));
                    app.toast(msg, true);
                }
                Err(e) => app.fail(e),
            }
        });
    }

    pub fn open_pr(&self, id: TaskId) {
        let url = self
            .data
            .with_untracked(|d| d.task(id).and_then(|t| t.pr_url.clone()));
        match url {
            Some(u) if !self.is_demo() => platform::open_url(&u),
            _ => self.toast("Opens github.com in your browser", true),
        }
    }

    // ------------------------------------------------------------ planner

    pub fn open_thread(&self, id: ThreadId) {
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c.thread(id).await {
                Ok(t) => app.data.update(|d| d.load_thread_detail(t)),
                Err(e) => app.fail(e),
            }
        });
        #[cfg(target_arch = "wasm32")]
        self.ws.with_value(|ws| {
            if let Some(ws) = ws {
                ws.send(&ClientMessage::FollowThread { thread_id: id });
            }
        });
    }

    /// Send a planner message. `thread == None` starts a new thread in
    /// `project`; the route switches to it once created.
    pub fn send_message(&self, thread: Option<ThreadId>, project: ProjectId, body: String) {
        if body.trim().is_empty() {
            return;
        }
        if self.is_demo() {
            let now = chrono::Utc::now();
            let (tid, evs) = self
                .data
                .with_untracked(|d| demo::act_send(d, thread, project, &body, now));
            self.apply_events(evs);
            if thread != Some(tid) {
                platform::set_hash(&Route::Planner(Some(tid)).to_hash());
                self.route.set(Route::Planner(Some(tid)));
            }
            let app = *self;
            set_timeout(
                move || {
                    let now = chrono::Utc::now();
                    if let Some(evs) = app.with_sim(|s, d| demo::planner_reply(s, d, tid, now)) {
                        app.apply_events(evs);
                    }
                },
                std::time::Duration::from_millis(1600),
            );
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match thread {
                Some(t) => match c
                    .send_message(t, &SendMessageRequest { body, voice: false })
                    .await
                {
                    Ok(m) => app.data.update(|d| d.push_message(m)),
                    Err(e) => app.fail(e),
                },
                None => match c
                    .create_thread(&CreateThreadRequest {
                        project_id: project,
                        message: body,
                        voice: false,
                    })
                    .await
                {
                    Ok(det) => {
                        let tid = det.thread.id;
                        app.data.update(|d| d.load_thread_detail(det));
                        app.go(Route::Planner(Some(tid)));
                    }
                    Err(e) => app.fail(e),
                },
            }
        });
    }

    /// Save plan edits (optimistic; the server bumps the version).
    pub fn save_plan(&self, plan: PlanId, spec: PlanSpec, version: u32) {
        // Apply locally right away so other views (board idea card) follow.
        self.data.update(|d| {
            if let Some(p) = d.plans.get_mut(&plan) {
                p.spec = spec.clone();
                p.updated_at = chrono::Utc::now();
            }
        });
        if self.is_demo() {
            let now = chrono::Utc::now();
            let evs = self
                .data
                .with_untracked(|d| demo::act_update_plan(d, plan, spec, now));
            self.apply_events(evs);
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c
                .update_plan(plan, &UpdatePlanRequest { spec, version })
                .await
            {
                Ok(p) => app.data.update(|d| d.upsert_plan(p)),
                Err(e) if e.code() == Some("conflict") => {
                    app.toast("Plan changed elsewhere · reloaded the latest draft", false);
                    let thread = app
                        .data
                        .with_untracked(|d| d.plans.get(&plan).map(|p| p.thread_id));
                    if let Some(t) = thread {
                        app.open_thread(t);
                    }
                }
                Err(e) => app.fail(e),
            }
        });
    }

    /// Approve & Dispatch → tasks on the board, toast the summary, go to Board.
    pub fn approve_plan(&self, plan: PlanId) {
        let finish = move |app: AppState, summary: String| {
            app.board_filter.set(BoardFilter::default());
            app.go(Route::Board);
            app.toast(summary, true);
        };
        if self.is_demo() {
            let now = chrono::Utc::now();
            if let Some(o) = self.with_sim(|s, d| demo::act_approve(s, d, plan, now)) {
                let t = o.toast.clone().unwrap_or_default();
                self.apply_events(o.events);
                finish(*self, t);
            }
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        let version = self
            .data
            .with_untracked(|d| d.plans.get(&plan).map(|p| p.version));
        spawn_local(async move {
            match c.approve_plan(plan, &ApprovePlanRequest { version }).await {
                Ok(r) => {
                    app.data.update(|d| {
                        d.upsert_plan(r.plan);
                        for t in r.tasks.into_iter().rev() {
                            d.upsert_task(t);
                        }
                    });
                    finish(app, r.summary);
                }
                Err(e) => app.fail(e),
            }
        });
    }

    // ------------------------------------------------------------ memory

    pub fn open_memory(&self, id: MemoryId) {
        self.brain_selected.set(Some(id));
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            if let Ok(d) = c.memory_item(id).await {
                app.data.update(|x| x.load_memory_detail(d));
            }
        });
    }

    pub fn update_memory(&self, id: MemoryId, req: UpdateMemoryRequest) {
        if self.is_demo() {
            let o = self
                .data
                .with_untracked(|d| demo::act_update_memory(d, id, &req, chrono::Utc::now()));
            self.demo_outcome(o);
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        let to_company = req.visibility == Some(Visibility::Company);
        spawn_local(async move {
            match c.update_memory(id, &req).await {
                Ok(m) => {
                    let pending = m.status == MemoryStatus::Pending;
                    app.data.update(|d| d.upsert_memory(m));
                    if to_company && pending {
                        app.toast("Submitted for Company approval", true);
                    }
                }
                Err(e) => app.fail(e),
            }
        });
    }

    pub fn decide_memory(&self, id: MemoryId, decision: MemoryDecision) {
        if self.is_demo() {
            let o = self
                .data
                .with_untracked(|d| demo::act_decide_memory(d, id, decision, chrono::Utc::now()));
            self.demo_outcome(o);
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c
                .decide_memory(
                    id,
                    &ApproveMemoryRequest {
                        decision,
                        edit: None,
                    },
                )
                .await
            {
                Ok(m) => {
                    app.data.update(|d| d.upsert_memory(m));
                    app.toast(
                        match decision {
                            MemoryDecision::Approve => {
                                "Approved · now visible to the whole company"
                            }
                            MemoryDecision::KeepShared => "Kept as shared",
                            MemoryDecision::KeepPrivate => "Kept private",
                            MemoryDecision::Reject => "Rejected",
                        },
                        true,
                    );
                }
                Err(e) => app.fail(e),
            }
        });
    }

    // ------------------------------------------------------------ credentials & settings

    pub fn test_credential(&self, id: CredentialId) {
        let name = self
            .data
            .with_untracked(|d| {
                d.credentials
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| c.name.clone())
            })
            .unwrap_or_default();
        if self.is_demo() {
            self.toast(format!("{name} · connection OK · 212ms"), true);
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c.test_credential(id).await {
                Ok(r) => {
                    if r.ok {
                        app.toast(format!("{name} · connection OK · {}ms", r.latency_ms), true);
                    } else {
                        app.toast(format!("{name} · {}", r.message), false);
                    }
                    if let Ok(list) = c.credentials().await {
                        app.data.update(|d| d.credentials = list);
                    }
                }
                Err(e) => app.fail(e),
            }
        });
    }

    pub fn add_credential(&self, req: CreateCredentialRequest, done: impl Fn() + 'static) {
        if self.is_demo() {
            let now = chrono::Utc::now();
            let c = CredentialSummary {
                id: CredentialId::new(),
                kind: req.kind,
                name: req.name.clone(),
                masked: demo::mask(&req.secret),
                base_url: req.base_url.clone(),
                env_var: req.env_var.clone(),
                status: CredentialHealth::Untested,
                created_at: now,
                expires_at: None,
            };
            self.data.update(|d| d.upsert_credential(c));
            self.toast(format!("{} · saved · encrypted at rest", req.name), true);
            done();
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c.add_credential(&req).await {
                Ok(s) => {
                    let n = s.name.clone();
                    app.data.update(|d| d.upsert_credential(s));
                    app.toast(format!("{n} · saved · encrypted at rest"), true);
                    done();
                }
                Err(e) => app.fail(e),
            }
        });
    }

    pub fn rotate_credential(
        &self,
        id: CredentialId,
        req: RotateCredentialRequest,
        done: impl Fn() + 'static,
    ) {
        if self.is_demo() {
            self.data.update(|d| {
                if let Some(c) = d.credentials.iter_mut().find(|c| c.id == id) {
                    c.masked = demo::mask(&req.secret);
                    if req.base_url.is_some() {
                        c.base_url = req.base_url.clone();
                    }
                    c.status = CredentialHealth::Untested;
                }
            });
            let n = self
                .data
                .with_untracked(|d| {
                    d.credentials
                        .iter()
                        .find(|c| c.id == id)
                        .map(|c| c.name.clone())
                })
                .unwrap_or_default();
            self.toast(format!("{n} · rotated"), true);
            done();
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c.rotate_credential(id, &req).await {
                Ok(s) => {
                    let n = s.name.clone();
                    app.data.update(|d| d.upsert_credential(s));
                    app.toast(format!("{n} · rotated"), true);
                    done();
                }
                Err(e) => app.fail(e),
            }
        });
    }

    pub fn revoke_credential(&self, id: CredentialId, done: impl Fn() + 'static) {
        let app = *self;
        let remove = move || app.data.update(|d| d.credentials.retain(|c| c.id != id));
        if self.is_demo() {
            remove();
            self.toast("Credential revoked", true);
            done();
            return;
        }
        let Some(c) = self.client() else { return };
        spawn_local(async move {
            match c.revoke_credential(id).await {
                Ok(()) => {
                    remove();
                    app.toast("Credential revoked", true);
                    done();
                }
                Err(e) => app.fail(e),
            }
        });
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut UserSettings)) {
        let Some(mut s) = self.data.with_untracked(|d| d.settings.clone()) else {
            return;
        };
        f(&mut s);
        self.data.update(|d| d.settings = Some(s.clone()));
        if self.is_demo() {
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c.update_settings(&s).await {
                Ok(s) => app.data.update(|d| d.settings = Some(s)),
                Err(e) => app.fail(e),
            }
        });
    }

    /// Start device pairing; `on_done` gets the QR + code.
    pub fn pair_start(&self, on_done: impl Fn(PairStartResponse) + 'static) {
        if self.is_demo() {
            on_done(demo::demo_pairing(chrono::Utc::now()));
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c.pair_start().await {
                Ok(r) => on_done(r),
                Err(e) => app.fail(e),
            }
        });
    }

    // ------------------------------------------------------------ admin

    pub fn update_user(&self, id: UserId, req: UpdateUserRequest) {
        // Optimistic local update of the admin table.
        self.data.update(|d| {
            if let Some(a) = d.admin.as_mut()
                && let Some(row) = a.users.iter_mut().find(|r| r.user.id == id)
            {
                if let Some(r) = req.role {
                    row.user.role = r;
                }
                if let Some(l) = &req.limits {
                    row.user.limits = l.clone();
                }
            }
        });
        if self.is_demo() {
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            if let Err(e) = c.update_user(id, &req).await {
                app.fail(e);
                app.refresh_admin();
            }
        });
    }

    pub fn create_user(&self, req: CreateUserRequest, done: impl Fn() + 'static) {
        if self.is_demo() {
            let u = User {
                id: UserId::new(),
                email: req.email.clone(),
                name: req.name.clone(),
                role: req.role,
                status: UserStatus::Active,
                limits: UserLimits {
                    concurrency: 2,
                    ram_share: 0.5,
                    monthly_budget_usd: 300.0,
                },
                initials: initials_for(&req.name),
                created_at: chrono::Utc::now(),
            };
            self.data.update(|d| {
                d.users.push(UserSummary::from(&u));
                if let Some(a) = d.admin.as_mut() {
                    a.users.push(AdminUserRow {
                        spend: UserSpend {
                            user_id: u.id,
                            month_usd: 0.0,
                            budget_usd: 300.0,
                        },
                        user: u,
                        running: 0,
                    });
                }
            });
            self.toast("Account created · invite sent to new teammate", true);
            done();
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c.create_user(&req).await {
                Ok(u) => {
                    app.toast(format!("Account created · {}", u.email), true);
                    app.refresh_admin();
                    done();
                }
                Err(e) => app.fail(e),
            }
        });
    }

    pub fn set_domain(&self, domain: String) {
        let domain = domain.trim().trim_start_matches('@').to_string();
        if domain.is_empty() {
            return;
        }
        if self.is_demo() {
            self.data.update(|d| {
                if let Some(s) = d.server.as_mut() {
                    s.allowed_domain = Some(domain.clone());
                }
            });
            self.toast(format!("Allowed domain set to @{domain}"), true);
            return;
        }
        let Some(c) = self.client() else { return };
        let app = *self;
        spawn_local(async move {
            match c
                .set_domain(&SetDomainRequest {
                    allowed_domain: domain.clone(),
                })
                .await
            {
                Ok(info) => {
                    app.data.update(|d| d.server = Some(info));
                    app.toast(format!("Allowed domain set to @{domain}"), true);
                }
                Err(e) => app.fail(e),
            }
        });
    }

    pub fn mark_notifications_read(&self) {
        let unread: Vec<NotificationId> = self.data.with_untracked(|d| {
            d.notifications
                .iter()
                .filter(|n| !n.read)
                .map(|n| n.id)
                .collect()
        });
        self.data
            .update(|d| d.notifications.iter_mut().for_each(|n| n.read = true));
        if self.is_demo() {
            return;
        }
        if let Some(c) = self.client() {
            spawn_local(async move {
                for id in unread {
                    let _ = c.mark_notification_read(id).await;
                }
            });
        }
    }
}
