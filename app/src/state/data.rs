//! The client-side data model and the pure reducer that applies daemon
//! events to it. No Leptos, no DOM: everything here is unit tested natively.
//!
//! `Data` is held in one `RwSignal<Data>` (see `state::AppState::data`);
//! surfaces read it through `Memo`s so only the parts that changed re-render.

use kiln_types::api::*;
use kiln_types::event::{LogLine, ServerEvent};
use kiln_types::ids::*;
use kiln_types::memory::*;
use kiln_types::*;
use std::collections::BTreeMap;

/// Max log lines kept per task (the panes show the last 14).
pub const LOG_CAP: usize = 400;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Data {
    pub server: Option<ServerInfo>,
    pub me: Option<User>,
    pub settings: Option<UserSettings>,
    pub projects: Vec<Project>,
    pub users: Vec<UserSummary>,
    /// Board tasks, newest first.
    pub tasks: Vec<Task>,
    /// Task detail cache (runs, artifacts, QA report, verdict, diff).
    pub details: BTreeMap<TaskId, TaskDetail>,
    pub logs: BTreeMap<TaskId, Vec<LogLine>>,
    /// Planner threads (own), newest first.
    pub threads: Vec<Thread>,
    /// Loaded threads: messages + context. The plan lives in `plans`.
    pub thread_details: BTreeMap<ThreadId, ThreadDetail>,
    /// Every plan we know about. Drafting plans are the board's Idea column.
    pub plans: BTreeMap<PlanId, Plan>,
    /// Threads where the planner is working → status label.
    pub planner_status: BTreeMap<ThreadId, String>,
    pub memory: Vec<MemoryItem>,
    pub memory_total: u32,
    pub memory_runs_week: u32,
    pub memory_details: BTreeMap<MemoryId, MemoryDetail>,
    pub credentials: Vec<CredentialSummary>,
    pub harnesses: Option<HarnessCatalog>,
    pub notifications: Vec<Notification>,
    pub machine: MachineStats,
    pub admin: Option<AdminOverview>,
}

/// UI side effects produced by an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Toast { message: String, ok: bool },
}

impl Data {
    // ------------------------------------------------------------ lookups

    pub fn task(&self, id: TaskId) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
    pub fn task_by_key(&self, key: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.key.eq_ignore_ascii_case(key))
    }
    pub fn project(&self, id: ProjectId) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }
    pub fn user(&self, id: UserId) -> Option<&UserSummary> {
        self.users.iter().find(|u| u.id == id)
    }
    pub fn user_name(&self, id: UserId) -> String {
        self.user(id)
            .map(|u| u.name.clone())
            .unwrap_or_else(|| "Unknown".into())
    }
    pub fn user_initials(&self, id: UserId) -> String {
        self.user(id)
            .map(|u| u.initials.clone())
            .unwrap_or_else(|| "?".into())
    }
    pub fn plan_for_thread(&self, thread: ThreadId) -> Option<&Plan> {
        self.plans
            .values()
            .filter(|p| p.thread_id == thread)
            .max_by_key(|p| p.updated_at)
    }
    pub fn thread(&self, id: ThreadId) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }
    pub fn memory_item(&self, id: MemoryId) -> Option<&MemoryItem> {
        self.memory.iter().find(|m| m.id == id)
    }
    pub fn is_admin(&self) -> bool {
        self.me.as_ref().is_some_and(|u| u.is_admin())
    }
    pub fn loop_cap_for(&self, project: ProjectId) -> u32 {
        self.project(project)
            .map(|p| p.loop_cap)
            .unwrap_or(DEFAULT_LOOP_CAP)
    }

    // ------------------------------------------------------------ loaders (HTTP responses)

    pub fn load_board(&mut self, b: BoardResponse) {
        let mut tasks = b.tasks;
        tasks.sort_by_key(|t| std::cmp::Reverse(t.created_at));
        for t in &tasks {
            if let Some(d) = self.details.get_mut(&t.id) {
                d.task = t.clone();
            }
        }
        self.tasks = tasks;
        // Replace drafting plans with the server's view; keep others.
        self.plans.retain(|_, p| p.status != PlanStatus::Drafting);
        for p in b.ideas {
            self.plans.insert(p.id, p);
        }
        if !b.projects.is_empty() {
            self.projects = b.projects;
        }
        if !b.users.is_empty() {
            self.users = b.users;
        }
    }

    pub fn load_task_detail(&mut self, d: TaskDetail) {
        self.upsert_task(d.task.clone());
        if !self.users.iter().any(|u| u.id == d.owner.id) {
            self.users.push(d.owner.clone());
        }
        self.details.insert(d.task.id, d);
    }

    pub fn load_logs(&mut self, task: TaskId, lines: Vec<LogLine>) {
        let entry = self.logs.entry(task).or_default();
        for l in lines {
            push_log(entry, l);
        }
    }

    pub fn load_threads(&mut self, mut threads: Vec<Thread>) {
        threads.sort_by_key(|t| std::cmp::Reverse(t.updated_at));
        self.threads = threads;
    }

    pub fn load_thread_detail(&mut self, mut d: ThreadDetail) {
        if let Some(p) = d.plan.take() {
            self.upsert_plan(p);
        }
        self.upsert_thread(d.thread.clone());
        d.messages.sort_by_key(|a| a.created_at);
        self.thread_details.insert(d.thread.id, d);
    }

    pub fn load_memory(&mut self, r: MemoryListResponse) {
        self.memory = r.items;
        self.memory_total = r.total;
        self.memory_runs_week = r.runs_this_week;
    }

    pub fn load_memory_detail(&mut self, d: MemoryDetail) {
        self.upsert_memory(d.item.clone());
        self.memory_details.insert(d.item.id, d);
    }

    pub fn load_me(&mut self, me: MeResponse) {
        if !self.users.iter().any(|u| u.id == me.user.id) {
            self.users.push(UserSummary::from(&me.user));
        }
        self.me = Some(me.user);
        self.settings = Some(me.settings);
        self.server = Some(me.server);
    }

    pub fn load_admin(&mut self, a: AdminOverview) {
        self.machine = a.machine.clone();
        for row in &a.users {
            if !self.users.iter().any(|u| u.id == row.user.id) {
                self.users.push(UserSummary::from(&row.user));
            }
        }
        if !a.projects.is_empty() {
            self.projects = a.projects.clone();
        }
        self.admin = Some(a);
    }

    // ------------------------------------------------------------ upserts

    pub fn upsert_task(&mut self, t: Task) {
        if let Some(d) = self.details.get_mut(&t.id) {
            d.task = t.clone();
        }
        match self.tasks.iter_mut().find(|x| x.id == t.id) {
            Some(x) => *x = t,
            None => self.tasks.insert(0, t),
        }
    }

    pub fn upsert_plan(&mut self, p: Plan) {
        match self.plans.get(&p.id) {
            // Never go backwards (a stale HTTP response racing a WS event).
            Some(old) if old.version > p.version => {}
            _ => {
                if let Some(t) = self.threads.iter_mut().find(|t| t.id == p.thread_id) {
                    t.status = p.status;
                    t.plan_id = Some(p.id);
                }
                self.plans.insert(p.id, p);
            }
        }
    }

    pub fn upsert_thread(&mut self, t: Thread) {
        if let Some(d) = self.thread_details.get_mut(&t.id) {
            d.thread = t.clone();
        }
        match self.threads.iter_mut().find(|x| x.id == t.id) {
            Some(x) => *x = t,
            None => self.threads.insert(0, t),
        }
    }

    pub fn upsert_memory(&mut self, m: MemoryItem) {
        if let Some(d) = self.memory_details.get_mut(&m.id) {
            d.item = m.clone();
        }
        match self.memory.iter_mut().find(|x| x.id == m.id) {
            Some(x) => *x = m,
            None => {
                self.memory.insert(0, m);
                self.memory_total = self.memory_total.max(self.memory.len() as u32);
            }
        }
    }

    pub fn upsert_credential(&mut self, c: CredentialSummary) {
        match self.credentials.iter_mut().find(|x| x.id == c.id) {
            Some(x) => *x = c,
            None => self.credentials.push(c),
        }
    }

    pub fn upsert_run(&mut self, run: Run) {
        let Some(task_id) = run.task_id else { return };
        if let Some(d) = self.details.get_mut(&task_id) {
            match d.runs.iter_mut().find(|r| r.id == run.id) {
                Some(r) => *r = run,
                None => d.runs.push(run),
            }
        }
    }

    pub fn push_message(&mut self, m: Message) {
        let thread_id = m.thread_id;
        if !self.thread_details.contains_key(&thread_id) {
            let Some(thread) = self.thread(thread_id).cloned() else {
                return;
            };
            self.thread_details.insert(
                thread_id,
                ThreadDetail {
                    thread,
                    messages: vec![],
                    plan: None,
                    memories_in_context: 0,
                    planner_model: String::new(),
                },
            );
        }
        if let Some(d) = self.thread_details.get_mut(&thread_id)
            && !d.messages.iter().any(|x| x.id == m.id)
        {
            d.messages.push(m);
        }
    }

    // ------------------------------------------------------------ the reducer

    /// Apply one live event. Returns UI effects (toasts) for the caller.
    pub fn apply(&mut self, ev: &ServerEvent) -> Vec<Effect> {
        let mut fx = Vec::new();
        match ev {
            ServerEvent::Hello { user, .. } => {
                if !self.users.iter().any(|u| u.id == user.id) {
                    self.users.push(user.clone());
                }
            }
            ServerEvent::TaskUpserted { task } => self.upsert_task(task.clone()),
            ServerEvent::TaskRemoved { task_id } => {
                self.tasks.retain(|t| t.id != *task_id);
                self.details.remove(task_id);
                self.logs.remove(task_id);
            }
            ServerEvent::RunUpserted { run } => self.upsert_run(run.clone()),
            ServerEvent::Log { task_id, line, .. } => {
                if let Some(t) = task_id {
                    push_log(self.logs.entry(*t).or_default(), line.clone());
                }
            }
            ServerEvent::PlanUpserted { plan } => self.upsert_plan(plan.clone()),
            ServerEvent::ThreadUpserted { thread } => self.upsert_thread(thread.clone()),
            ServerEvent::Message { message } => self.push_message(message.clone()),
            ServerEvent::PlannerStatus {
                thread_id,
                pending,
                label,
            } => {
                if *pending {
                    self.planner_status.insert(*thread_id, label.clone());
                } else {
                    self.planner_status.remove(thread_id);
                }
            }
            ServerEvent::Notification { notification } => {
                let fresh = !self.notifications.iter().any(|n| n.id == notification.id);
                match self
                    .notifications
                    .iter_mut()
                    .find(|n| n.id == notification.id)
                {
                    Some(n) => *n = notification.clone(),
                    None => self.notifications.insert(0, notification.clone()),
                }
                if fresh && !notification.read {
                    fx.push(Effect::Toast {
                        message: notification.title.clone(),
                        ok: true,
                    });
                }
            }
            ServerEvent::MemoryUpserted { item } => self.upsert_memory(item.clone()),
            ServerEvent::Machine { stats } => {
                self.machine = stats.clone();
                if let Some(a) = self.admin.as_mut() {
                    a.machine = stats.clone();
                }
            }
            ServerEvent::Toast { message, ok } => fx.push(Effect::Toast {
                message: message.clone(),
                ok: *ok,
            }),
        }
        fx
    }
}

/// Append a log line, dropping duplicates (same run + seq) and keeping order.
pub fn push_log(lines: &mut Vec<LogLine>, l: LogLine) {
    if lines
        .iter()
        .rev()
        .take(64)
        .any(|x| x.run_id == l.run_id && x.seq == l.seq)
    {
        return;
    }
    let pos = lines
        .iter()
        .rposition(|x| (x.ts, x.seq) <= (l.ts, l.seq))
        .map(|p| p + 1)
        .unwrap_or(0);
    lines.insert(pos, l);
    if lines.len() > LOG_CAP {
        let drop = lines.len() - LOG_CAP;
        lines.drain(..drop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::demo;
    use chrono::Utc;
    use kiln_types::event::LogLevel;

    fn seeded() -> Data {
        demo::seed(Utc::now())
    }

    #[test]
    fn task_upsert_inserts_and_replaces() {
        let mut d = seeded();
        let n = d.tasks.len();
        let mut t = d.tasks[3].clone();
        t.state = TaskState::Escalated;
        d.apply(&ServerEvent::TaskUpserted { task: t.clone() });
        assert_eq!(d.tasks.len(), n);
        assert_eq!(d.task(t.id).unwrap().state, TaskState::Escalated);

        let mut new = t.clone();
        new.id = TaskId::new();
        new.key = "FP-999".into();
        d.apply(&ServerEvent::TaskUpserted { task: new.clone() });
        assert_eq!(d.tasks.len(), n + 1);
        assert_eq!(d.tasks[0].key, "FP-999", "new tasks go first");

        d.apply(&ServerEvent::TaskRemoved { task_id: new.id });
        assert_eq!(d.tasks.len(), n);
    }

    #[test]
    fn task_upsert_updates_detail_cache() {
        let mut d = seeded();
        let t = d.task_by_key("FP-142").unwrap().clone();
        assert!(d.details.contains_key(&t.id));
        let mut t2 = t.clone();
        t2.progress = 77;
        d.apply(&ServerEvent::TaskUpserted { task: t2 });
        assert_eq!(d.details[&t.id].task.progress, 77);
    }

    #[test]
    fn logs_dedupe_and_cap() {
        let mut d = seeded();
        let t = d.tasks[0].id;
        d.logs.clear();
        let run = RunId::new();
        let mk = |seq| LogLine {
            run_id: run,
            seq,
            ts: Utc::now(),
            role: WorkerRole::Builder,
            level: LogLevel::Info,
            message: format!("line {seq}"),
        };
        d.apply(&ServerEvent::Log {
            task_id: Some(t),
            thread_id: None,
            line: mk(1),
        });
        d.apply(&ServerEvent::Log {
            task_id: Some(t),
            thread_id: None,
            line: mk(1),
        });
        assert_eq!(d.logs[&t].len(), 1);
        for s in 2..(LOG_CAP as u64 + 50) {
            d.apply(&ServerEvent::Log {
                task_id: Some(t),
                thread_id: None,
                line: mk(s),
            });
        }
        assert_eq!(d.logs[&t].len(), LOG_CAP);
        assert_eq!(d.logs[&t].last().unwrap().seq, LOG_CAP as u64 + 49);
    }

    #[test]
    fn plan_versions_never_go_backwards() {
        let mut d = seeded();
        let p = d
            .plans
            .values()
            .find(|p| p.status == PlanStatus::Drafting)
            .unwrap()
            .clone();
        let mut newer = p.clone();
        newer.version += 2;
        d.apply(&ServerEvent::PlanUpserted {
            plan: newer.clone(),
        });
        d.apply(&ServerEvent::PlanUpserted { plan: p.clone() });
        assert_eq!(d.plans[&p.id].version, newer.version);
    }

    #[test]
    fn planner_status_and_messages() {
        let mut d = seeded();
        let th = d.threads[0].id;
        d.apply(&ServerEvent::PlannerStatus {
            thread_id: th,
            pending: true,
            label: "drafting".into(),
        });
        assert!(d.planner_status.contains_key(&th));
        let before = d.thread_details[&th].messages.len();
        let m = Message {
            id: MessageId::new(),
            thread_id: th,
            author: MessageAuthor::Planner,
            body: "hi".into(),
            voice: false,
            plan_version: None,
            created_at: Utc::now(),
        };
        d.apply(&ServerEvent::Message { message: m.clone() });
        d.apply(&ServerEvent::Message { message: m });
        assert_eq!(d.thread_details[&th].messages.len(), before + 1);
        d.apply(&ServerEvent::PlannerStatus {
            thread_id: th,
            pending: false,
            label: String::new(),
        });
        assert!(!d.planner_status.contains_key(&th));
    }

    #[test]
    fn notifications_toast_once() {
        let mut d = seeded();
        let me = d.me.clone().unwrap();
        let n = Notification {
            id: NotificationId::new(),
            user_id: me.id,
            kind: NotificationKind::TaskFailed,
            title: "QA failed: filter resets on refresh".into(),
            body: String::new(),
            deep_link: "task/FP-139".into(),
            actions: vec![],
            task_id: None,
            plan_id: None,
            memory_id: None,
            read: false,
            resolved: None,
            created_at: Utc::now(),
        };
        let fx = d.apply(&ServerEvent::Notification {
            notification: n.clone(),
        });
        assert_eq!(fx.len(), 1);
        let fx = d.apply(&ServerEvent::Notification { notification: n });
        assert!(fx.is_empty());
    }

    #[test]
    fn machine_and_toast_events() {
        let mut d = seeded();
        let stats = MachineStats {
            cpu_percent: 91.0,
            ..Default::default()
        };
        d.apply(&ServerEvent::Machine { stats });
        assert_eq!(d.machine.cpu_percent, 91.0);
        let fx = d.apply(&ServerEvent::Toast {
            message: "hey".into(),
            ok: false,
        });
        assert_eq!(
            fx,
            vec![Effect::Toast {
                message: "hey".into(),
                ok: false
            }]
        );
    }

    #[test]
    fn memory_upsert() {
        let mut d = seeded();
        let mut m = d.memory[1].clone();
        m.status = MemoryStatus::Active;
        m.visibility = Visibility::Company;
        d.apply(&ServerEvent::MemoryUpserted { item: m.clone() });
        assert_eq!(d.memory_item(m.id).unwrap().status, MemoryStatus::Active);
    }
}
