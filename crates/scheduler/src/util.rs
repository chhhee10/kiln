//! Small helpers: spawning, publishing, redaction, copy for meta lines.

use crate::Scheduler;
use crate::state::lock;
use kiln_events::Event;
use kiln_types::event::{RunEvent, ServerEvent};
use kiln_types::{Plan, Run, Task, TaskState, Thread, User, UserId};
use kiln_vault::Redactor;
use std::future::Future;
use std::sync::Arc;

impl Scheduler {
    /// A strong handle to ourselves (for spawning from `&self` methods).
    pub(crate) fn arc(&self) -> Option<Arc<Scheduler>> {
        lock(&self.st.self_ref).upgrade()
    }

    /// Spawn a background task that is aborted on shutdown.
    pub(crate) fn spawn_bg<F>(&self, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.st.is_shutdown() {
            return;
        }
        let h = tokio::spawn(fut);
        let mut bg = lock(&self.st.background);
        bg.retain(|t| !t.is_finished());
        bg.push(h);
    }

    pub(crate) fn publish_task(&self, task: &Task) {
        self.bus
            .server(ServerEvent::TaskUpserted { task: task.clone() });
    }

    pub(crate) fn publish_run(&self, run: &Run) {
        self.bus
            .server(ServerEvent::RunUpserted { run: run.clone() });
    }

    pub(crate) fn publish_plan(&self, plan: &Plan) {
        self.bus
            .server(ServerEvent::PlanUpserted { plan: plan.clone() });
    }

    pub(crate) fn publish_thread(&self, thread: &Thread) {
        self.bus.server(ServerEvent::ThreadUpserted {
            thread: thread.clone(),
        });
    }

    /// Publish a persisted transition (client upsert + internal event).
    pub(crate) fn published_transition(&self, task: &Task, from: TaskState) {
        self.publish_task(task);
        self.bus.publish(Event::TaskTransitioned {
            task_id: task.id,
            owner_id: task.owner_id,
            from,
            to: task.state,
        });
        if task.state.is_terminal() || task.state == TaskState::Queued {
            self.wake();
        }
    }

    /// Owner or admin may act on someone's object.
    pub(crate) fn ensure_owner_or_admin(&self, user: &User, owner: UserId) -> crate::Result<()> {
        if user.id == owner || user.is_admin() {
            Ok(())
        } else {
            Err(crate::SchedulerError::Forbidden(
                "only the owner or an admin may do this".into(),
            ))
        }
    }
}

/// Redact every string inside an event (exact secrets + pattern scan).
pub(crate) fn redact_event(ev: &RunEvent, redactor: &Redactor) -> RunEvent {
    let Ok(mut v) = serde_json::to_value(ev) else {
        return ev.clone();
    };
    let mut changed = false;
    scrub_value(&mut v, redactor, &mut changed);
    if !changed {
        return ev.clone();
    }
    serde_json::from_value(v).unwrap_or_else(|_| ev.clone())
}

fn scrub_value(v: &mut serde_json::Value, r: &Redactor, changed: &mut bool) {
    match v {
        serde_json::Value::String(s) => {
            let clean = r.scrub(s);
            if clean != *s {
                *s = clean;
                *changed = true;
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| scrub_value(x, r, changed)),
        serde_json::Value::Object(m) => m.values_mut().for_each(|x| scrub_value(x, r, changed)),
        _ => {}
    }
}

/// Thread title from the first message: first non-empty line, ≤ 60 chars.
pub(crate) fn thread_title(message: &str) -> String {
    let line = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("New thread");
    truncate(line, 60)
}

/// Truncate to `max` chars with an ellipsis.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// First line of `s`, trimmed and truncated (for meta lines / errors).
pub(crate) fn first_line(s: &str, max: usize) -> String {
    truncate(
        s.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(""),
        max,
    )
}

/// Design copy for task meta lines (docs/SCHEDULER.md "Progress & meta").
pub(crate) mod meta {
    pub fn building(attempt: u32, cap: u32) -> String {
        format!("Building · attempt {attempt}/{}", cap.max(attempt))
    }
    pub fn testing(attempt: u32, cap: u32) -> String {
        format!("QA testing · {attempt}/{}", cap.max(attempt))
    }
    pub const PREPARING: &str = "Preparing";
    pub const REVIEWING: &str = "Reviewing";
    pub const QUEUED: &str = "Queued";
    pub const WAITING: &str = "waiting for capacity";
    pub fn ready(pr: Option<u64>) -> String {
        match pr {
            Some(n) => format!("PR #{n} ready"),
            None => "Ready".into(),
        }
    }
    pub const NEEDS_YOU: &str = "Needs you";
    pub const MERGED: &str = "Merged";
    pub fn failed(reason: &str) -> String {
        format!("Failed · {}", super::first_line(reason, 80))
    }
    pub const STOPPED: &str = "Stopped";
    pub fn looping(attempt: u32, cap: u32) -> String {
        format!("Looping · attempt {attempt}/{}", cap.max(attempt))
    }
}
