//! Human task actions (PRD §4.4, docs/SCHEDULER.md "Human actions").
//!
//! | Action | From | Effect |
//! |---|---|---|
//! | Stop | active / Queued | → Cancelled ("Stopped"), live runner stopped |
//! | Retry | Escalated / Failed / Cancelled | → Queued; loop counter +1, or reset when it reached the cap |
//! | Feedback | Queued … Reviewing, Ready, Escalated | note queued for the Builder; forces a loop (see below) |
//! | Escalate | active / Queued / Failed | → Escalated, run stopped, owner + admins notified |
//!
//! Feedback by state: Queued/Preparing → just queued for the first Builder
//! prompt; Building → the live Builder is restarted at the next attempt
//! (on its last attempt the note is forwarded to the live session instead);
//! Testing/Reviewing → Looping (the driver applies the cap); Ready → Looping
//! → Queued at the next attempt (cap reached → Escalated); Escalated →
//! exactly one more attempt (Looping → Queued).
//!
//! Every transition is CAS; when the state moves under us we re-read and
//! retry (a few times), never panic.

use crate::Scheduler;
use crate::error::{Result, SchedulerError};
use crate::sql::{Ext, LoopChange, bump_loop_with_feedback, transition_ext};
use crate::util::meta;
use kiln_store::NewNotification;
use kiln_types::api::TaskAction;
use kiln_types::memory::{MemoryProposal, MemoryType, Visibility};
use kiln_types::{NotificationKind, RunState, Task, TaskId, TaskState, User, UserRole, WorkerRole};

const CAS_RETRIES: usize = 6;

impl Scheduler {
    pub(crate) async fn do_task_action(
        &self,
        user: &User,
        id: TaskId,
        action: TaskAction,
    ) -> Result<Task> {
        if self.st.is_shutdown() {
            return Err(SchedulerError::ShuttingDown);
        }
        let task = self
            .store
            .get_task(id)
            .await
            .map_err(|_| SchedulerError::NotFound("task".into()))?;
        self.ensure_owner_or_admin(user, task.owner_id)?;
        let mut last = SchedulerError::Conflict("task kept changing state".into());
        for _ in 0..CAS_RETRIES {
            let task = self.store.get_task(id).await?;
            let r = match &action {
                TaskAction::Stop => self.action_stop(user, &task).await,
                TaskAction::Retry => self.action_retry(user, &task).await,
                TaskAction::Escalate { note } => {
                    self.action_escalate(user, &task, note.as_deref()).await
                }
                TaskAction::Feedback {
                    text,
                    attach_qa,
                    attach_verdict,
                    save_memory,
                } => {
                    self.action_feedback(
                        user,
                        &task,
                        text,
                        *attach_qa,
                        *attach_verdict,
                        *save_memory,
                    )
                    .await
                }
            };
            match r {
                Err(e)
                    if e.is_conflict()
                        && !matches!(e, SchedulerError::Conflict(ref m) if m.starts_with("cannot")) =>
                {
                    last = e;
                    tokio::task::yield_now().await;
                    continue;
                }
                other => {
                    if other.is_ok() {
                        self.wake();
                        if let Some(s) = self.arc() {
                            // Re-activated tasks need a driver right away.
                            let t = self.store.get_task(id).await?;
                            if t.state.is_active() {
                                s.ensure_driver(id).await;
                            }
                        }
                    }
                    return other;
                }
            }
        }
        Err(last)
    }

    fn cannot(action: &str, task: &Task) -> SchedulerError {
        SchedulerError::Conflict(format!(
            "cannot {action} a task that is {}",
            task.state.as_str()
        ))
    }

    async fn action_stop(&self, user: &User, task: &Task) -> Result<Task> {
        if !(task.state.is_active() || task.state == TaskState::Queued) {
            return Err(Self::cannot("stop", task));
        }
        let t = self
            .transition(
                task,
                TaskState::Cancelled,
                "stopped by a human",
                Some(user.id),
                Ext {
                    meta: Some(meta::STOPPED.into()),
                    ..Default::default()
                },
            )
            .await?;
        self.stop_task_runs(task.id, RunState::Cancelled, "stopped by a human");
        Ok(t)
    }

    async fn action_retry(&self, user: &User, task: &Task) -> Result<Task> {
        if !matches!(
            task.state,
            TaskState::Escalated | TaskState::Failed | TaskState::Cancelled
        ) {
            return Err(Self::cannot("retry", task));
        }
        let cap = task.loop_cap.max(1);
        let change = if task.loop_count + 1 >= cap {
            LoopChange::Set(0)
        } else {
            LoopChange::Increment
        };
        let t = self
            .transition(
                task,
                TaskState::Queued,
                "retried by a human",
                Some(user.id),
                Ext {
                    loop_change: Some(change),
                    meta: Some(meta::QUEUED.into()),
                    ..Default::default()
                },
            )
            .await?;
        self.resolve_task_notifications(
            &t,
            &format!("✓ Retried · attempt {}/{} queued", t.attempt(), cap),
        )
        .await;
        Ok(t)
    }

    async fn action_escalate(&self, user: &User, task: &Task, note: Option<&str>) -> Result<Task> {
        if !(task.state.is_active() || matches!(task.state, TaskState::Queued | TaskState::Failed))
        {
            return Err(Self::cannot("escalate", task));
        }
        let fb: Vec<String> = note
            .filter(|n| !n.trim().is_empty())
            .map(|n| format!("## Note from {}\n\n{}", user.name, n.trim()))
            .into_iter()
            .collect();
        let t = self
            .transition(
                task,
                TaskState::Escalated,
                "escalated by a human",
                Some(user.id),
                Ext {
                    feedback: fb,
                    meta: Some(meta::NEEDS_YOU.into()),
                    ..Default::default()
                },
            )
            .await?;
        self.stop_task_runs(task.id, RunState::Cancelled, "escalated by a human");
        let body = match note {
            Some(n) if !n.trim().is_empty() => {
                format!("{} · {}", t.key, crate::util::first_line(n, 160))
            }
            _ => format!("{} · escalated · team notified", t.key),
        };
        let mut recipients = vec![t.owner_id];
        if let Ok(users) = self.store.list_users().await {
            recipients.extend(
                users
                    .iter()
                    .filter(|u| {
                        u.role == UserRole::Admin && u.status == kiln_types::UserStatus::Active
                    })
                    .map(|u| u.id),
            );
        }
        recipients.sort();
        recipients.dedup();
        for r in recipients {
            let mut n = NewNotification::new(
                r,
                NotificationKind::TaskEscalated,
                format!("Escalated: {}", t.title),
                body.clone(),
            );
            n.deep_link = format!("task/{}", t.key);
            n.task_id = Some(t.id);
            n.plan_id = Some(t.plan_id);
            n.actions = vec![
                kiln_types::NotificationAction {
                    id: "retry".into(),
                    label: "Retry".into(),
                },
                kiln_types::NotificationAction {
                    id: "feedback".into(),
                    label: "Send back to Builder".into(),
                },
            ];
            if let Err(e) = self.notifier.notify(n).await {
                tracing::warn!(error = %e, "escalation notification failed");
            }
        }
        Ok(t)
    }

    async fn action_feedback(
        &self,
        user: &User,
        task: &Task,
        text: &str,
        attach_qa: bool,
        attach_verdict: bool,
        save_memory: bool,
    ) -> Result<Task> {
        if text.trim().is_empty() {
            return Err(SchedulerError::Invalid("feedback text is empty".into()));
        }
        if matches!(
            task.state,
            TaskState::Merged | TaskState::Failed | TaskState::Cancelled
        ) {
            return Err(Self::cannot("send feedback to", task));
        }
        // Compose: the human note, plus the latest QA report / verdict.
        let qa = if attach_qa {
            self.latest_role_outcome(task, WorkerRole::Qa)
                .await
                .and_then(|o| o.qa_report)
        } else {
            None
        };
        let verdict = if attach_verdict {
            self.latest_role_outcome(task, WorkerRole::Reviewer)
                .await
                .and_then(|o| o.verdict)
        } else {
            None
        };
        let note = kiln_planner::feedback_from(
            qa.as_ref().filter(|r| !r.passed()),
            verdict.as_ref().filter(|v| !v.approved()),
            Some(&format!("{} ({}): {}", user.name, user.email, text.trim())),
        );
        let cap = task.loop_cap.max(1);
        let reason = "feedback from a human";
        let t = match task.state {
            TaskState::Queued | TaskState::Preparing => {
                self.store.push_task_feedback(task.id, &note).await?;
                self.store.get_task(task.id).await?
            }
            TaskState::Building => {
                if task.loop_count + 1 < cap {
                    let t =
                        bump_loop_with_feedback(&self.store, task.id, TaskState::Building, &note)
                            .await?;
                    let t = self
                        .store
                        .update_task_progress(t.id, 0, &meta::building(t.attempt(), cap))
                        .await?;
                    self.publish_task(&t);
                    self.stop_task_runs(
                        task.id,
                        RunState::Cancelled,
                        "restarting with human feedback",
                    );
                    t
                } else {
                    // Last attempt: forward to the live session, keep the note.
                    self.store.push_task_feedback(task.id, &note).await?;
                    let lives: Vec<_> = crate::state::lock(&self.st.live)
                        .values()
                        .filter(|l| l.task_id == Some(task.id))
                        .cloned()
                        .collect();
                    for l in lives {
                        let _ = l
                            .control
                            .send(kiln_types::protocol::DaemonMessage::Feedback {
                                text: note.clone(),
                            });
                    }
                    self.store.get_task(task.id).await?
                }
            }
            TaskState::Testing | TaskState::Reviewing | TaskState::Looping => {
                let t = if task.state == TaskState::Looping {
                    self.store.push_task_feedback(task.id, &note).await?;
                    self.store.get_task(task.id).await?
                } else {
                    self.transition(
                        task,
                        TaskState::Looping,
                        reason,
                        Some(user.id),
                        Ext {
                            feedback: vec![note.clone()],
                            meta: Some(meta::looping(task.attempt(), cap)),
                            ..Default::default()
                        },
                    )
                    .await?
                };
                self.stop_task_runs(task.id, RunState::Cancelled, "looping with human feedback");
                t
            }
            TaskState::Ready | TaskState::Escalated => {
                let from = task.state;
                if from == TaskState::Ready && task.loop_count + 1 >= cap {
                    // "cap+1 escalates": no silent extra attempt past the cap.
                    let t = self
                        .transition(
                            task,
                            TaskState::Looping,
                            reason,
                            Some(user.id),
                            Ext {
                                feedback: vec![note.clone()],
                                ..Default::default()
                            },
                        )
                        .await?;
                    return self
                        .escalate(
                            &t,
                            NotificationKind::TaskEscalated,
                            "feedback on the last attempt — needs your call",
                        )
                        .await;
                }
                let t = self
                    .transition(
                        task,
                        TaskState::Looping,
                        reason,
                        Some(user.id),
                        Ext {
                            feedback: vec![note.clone()],
                            ..Default::default()
                        },
                    )
                    .await?;
                // Back through admission (the worktree was cleaned up at Ready).
                let t = transition_ext(
                    &self.store,
                    t.id,
                    TaskState::Looping,
                    TaskState::Queued,
                    "requeued for the next attempt",
                    Some(user.id),
                    &Ext {
                        loop_change: Some(LoopChange::Increment),
                        meta: Some(meta::QUEUED.into()),
                        ..Default::default()
                    },
                )
                .await?;
                self.published_transition(&t, TaskState::Looping);
                if from == TaskState::Escalated {
                    self.resolve_task_notifications(
                        &t,
                        &format!("✓ Sent back · attempt {} queued", t.attempt()),
                    )
                    .await;
                }
                t
            }
            _ => return Err(Self::cannot("send feedback to", task)),
        };
        if save_memory {
            let p = MemoryProposal {
                kind: MemoryType::Procedure,
                title: crate::util::truncate(text.trim(), 80),
                subject: task.key.clone(),
                relation: "feedback".into(),
                object: crate::util::truncate(text.trim(), 200),
                body: text.trim().to_string(),
                visibility: Visibility::Private,
                shared_with: vec![],
                project_id: Some(task.project_id),
            };
            match self.memory.propose(user.id, None, p).await {
                Ok(item) => {
                    self.bus
                        .server(kiln_types::event::ServerEvent::MemoryUpserted { item });
                    self.bus.publish(kiln_events::Event::MemoryChanged {
                        owner_id: user.id,
                        project_id: Some(task.project_id),
                    });
                }
                Err(e) => tracing::warn!(error = %e, "saving feedback to memory failed"),
            }
        }
        Ok(t)
    }

    async fn latest_role_outcome(
        &self,
        task: &Task,
        role: WorkerRole,
    ) -> Option<kiln_types::protocol::RunOutcome> {
        let r = self
            .store
            .latest_run_for_role(task.id, role)
            .await
            .ok()
            .flatten()?;
        self.store.get_run_outcome(r.id).await.ok().flatten()
    }

    /// Mark the owner's open escalation/failure cards for this task resolved.
    async fn resolve_task_notifications(&self, task: &Task, text: &str) {
        let Ok(list) = self.store.list_notifications(task.owner_id, 50).await else {
            return;
        };
        for n in list
            .into_iter()
            .filter(|n| n.task_id == Some(task.id) && n.resolved.is_none())
        {
            if matches!(
                n.kind,
                NotificationKind::TaskEscalated | NotificationKind::TaskFailed
            ) {
                let _ = self.notifier.resolve(n.id, text).await;
            }
        }
    }
}
