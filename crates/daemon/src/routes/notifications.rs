//! Notification inbox and Android inline actions.
//!
//! An action performs the real command through the scheduler / memory
//! gateway **as the notification's user**, then resolves the card with the
//! "✓ …" line from the Android design (e.g. "✓ Retried · attempt 3/3
//! running", "✓ Approved · FP-160 is building, 2 tasks queued").

use crate::error::{ApiResult, AppError};
use crate::extract::{Body, P};
use crate::session::CurrentUser;
use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use kiln_types::api::*;
use kiln_types::memory::Visibility;
use kiln_types::*;

pub async fn list(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<Vec<Notification>>> {
    Ok(Json(st.store.list_notifications(me.user.id, 200).await?))
}

async fn own(st: &AppState, me: &User, id: NotificationId) -> ApiResult<Notification> {
    match st.store.get_notification(id).await {
        Ok(n) if n.user_id == me.id => Ok(n),
        Ok(_) | Err(kiln_store::StoreError::NotFound) => Err(AppError::not_found("notification")),
        Err(e) => Err(e.into()),
    }
}

pub async fn read(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<NotificationId>,
) -> ApiResult<StatusCode> {
    own(&st, &me.user, id).await?;
    st.store.mark_read(me.user.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// "Approved · FP-160 is building, 2 tasks queued".
pub fn approved_text(tasks: &[Task]) -> String {
    let building: Vec<&Task> = tasks.iter().filter(|t| t.state.is_active()).collect();
    let queued = tasks.len() - building.len();
    match building.first() {
        Some(b) if queued > 0 => format!(
            "Approved · {} is building, {} queued",
            b.key,
            plural(queued, "task")
        ),
        Some(b) => format!("Approved · {} is building", b.key),
        None => format!("Approved · {} queued", plural(tasks.len(), "task")),
    }
}

/// "Retried · attempt 3/3 running".
pub fn attempt_text(prefix: &str, t: &Task) -> String {
    let what = if t.state.is_active() {
        "running"
    } else {
        "queued"
    };
    format!(
        "{prefix} · attempt {}/{} {what}",
        t.attempt().min(t.loop_cap.max(1)),
        t.loop_cap.max(1)
    )
}

pub async fn action(
    State(st): State<AppState>,
    me: CurrentUser,
    P(id): P<NotificationId>,
    Body(req): Body<NotificationActionRequest>,
) -> ApiResult<Json<Notification>> {
    let n = own(&st, &me.user, id).await?;
    let action = req.action.trim().to_ascii_lowercase();
    // Navigation-only actions just mark the card read.
    if matches!(action.as_str(), "open" | "review" | "decide" | "view") {
        st.store.mark_read(me.user.id, id).await?;
        return Ok(Json(st.store.get_notification(id).await?));
    }
    if n.resolved.is_some() {
        return Err(AppError::conflict("this notification was already acted on"));
    }
    let need_task = || {
        n.task_id.ok_or_else(|| {
            AppError::invalid(format!("action {action:?} needs a task notification"))
        })
    };
    let text = match action.as_str() {
        "approve" => {
            if let Some(plan) = n.plan_id {
                let r = st
                    .scheduler
                    .approve_plan(&me.user, plan, ApprovePlanRequest::default())
                    .await?;
                approved_text(&r.tasks)
            } else if let Some(mem) = n.memory_id {
                let item = st
                    .memory
                    .decide(
                        me.user.id,
                        mem,
                        ApproveMemoryRequest {
                            decision: MemoryDecision::Approve,
                            edit: None,
                        },
                    )
                    .await?;
                crate::routes::brain::publish_memory(&st, &item);
                match item.visibility {
                    Visibility::Company => {
                        "Approved · now visible to the whole company".to_string()
                    }
                    Visibility::Shared => format!(
                        "Approved · shared with {}",
                        plural(item.shared_with.len(), "person")
                    ),
                    Visibility::Private => "Approved · kept private".to_string(),
                }
            } else {
                return Err(AppError::invalid("nothing to approve on this notification"));
            }
        }
        "keep_shared" | "keep_private" | "reject" => {
            let mem = n
                .memory_id
                .ok_or_else(|| AppError::invalid("not a memory notification"))?;
            let decision = match action.as_str() {
                "keep_shared" => MemoryDecision::KeepShared,
                "keep_private" => MemoryDecision::KeepPrivate,
                _ => MemoryDecision::Reject,
            };
            let item = st
                .memory
                .decide(
                    me.user.id,
                    mem,
                    ApproveMemoryRequest {
                        decision,
                        edit: None,
                    },
                )
                .await?;
            crate::routes::brain::publish_memory(&st, &item);
            match decision {
                MemoryDecision::KeepShared => "Kept shared".into(),
                MemoryDecision::KeepPrivate => "Kept private".into(),
                _ => "Rejected".into(),
            }
        }
        "retry" => {
            let t = st
                .scheduler
                .task_action(&me.user, need_task()?, TaskAction::Retry)
                .await?;
            attempt_text("Retried", &t)
        }
        "stop" => {
            st.scheduler
                .task_action(&me.user, need_task()?, TaskAction::Stop)
                .await?;
            "Stopped".into()
        }
        "escalate" => {
            st.scheduler
                .task_action(
                    &me.user,
                    need_task()?,
                    TaskAction::Escalate {
                        note: req.text.clone(),
                    },
                )
                .await?;
            "Escalated · team notified".into()
        }
        "reply" | "feedback" => {
            let text = req
                .text
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| AppError::invalid("reply text is required"))?;
            if text.len() > 20_000 {
                return Err(AppError::invalid("reply is too long"));
            }
            if let Some(task) = n.task_id {
                let t = st
                    .scheduler
                    .task_action(
                        &me.user,
                        task,
                        TaskAction::Feedback {
                            text: text.to_string(),
                            attach_qa: true,
                            attach_verdict: true,
                            save_memory: false,
                        },
                    )
                    .await?;
                attempt_text("Sent to Builder", &t)
            } else if let Some(plan) = n.plan_id {
                let p = st.store.get_plan(plan).await?;
                st.scheduler
                    .planner_message(
                        &me.user,
                        p.thread_id,
                        SendMessageRequest {
                            body: text.to_string(),
                            voice: false,
                        },
                    )
                    .await?;
                "Replied · planner is revising the plan".into()
            } else {
                return Err(AppError::invalid(
                    "nothing to reply to on this notification",
                ));
            }
        }
        other => return Err(AppError::invalid(format!("unknown action {other:?}"))),
    };
    st.store.mark_read(me.user.id, id).await?;
    st.notifier.resolve(id, &format!("✓ {text}")).await?;
    Ok(Json(st.store.get_notification(id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(key: &str, state: TaskState, loops: u32) -> Task {
        Task {
            id: TaskId::new(),
            plan_id: PlanId::new(),
            project_id: ProjectId::new(),
            owner_id: UserId::new(),
            number: 1,
            key: key.into(),
            spec_id: "t1".into(),
            title: "t".into(),
            description: String::new(),
            acceptance: vec![],
            files_hint: vec![],
            depends_on: vec![],
            workers: Default::default(),
            state,
            last_active: None,
            loop_count: loops,
            loop_cap: 3,
            branch: None,
            pr_url: None,
            pr_number: None,
            progress: 0,
            meta: String::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn resolution_texts() {
        let ts = [
            task("FP-160", TaskState::Building, 0),
            task("FP-161", TaskState::Queued, 0),
            task("FP-162", TaskState::Queued, 0),
        ];
        assert_eq!(
            approved_text(&ts),
            "Approved · FP-160 is building, 2 tasks queued"
        );
        assert_eq!(approved_text(&ts[1..2]), "Approved · 1 task queued");
        assert_eq!(
            attempt_text("Retried", &task("FP-1", TaskState::Building, 2)),
            "Retried · attempt 3/3 running"
        );
    }
}
