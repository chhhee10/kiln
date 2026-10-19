//! Permission-aware projections (docs/SCHEDULER.md "Permission rules").
//!
//! * Task cards are team-visible, but the task *content* written in the
//!   owner's private planner session (description, acceptance criteria,
//!   file hints) is owner-only, like logs, artifacts, transcripts, QA report
//!   bodies, verdicts and diffs ("never read another user's artifacts",
//!   PRD §13).
//! * Admins see metadata (runs, artifact listings) but never content.

use crate::error::ApiResult;
use crate::state::AppState;
use kiln_types::qa::{PassFail, QaReport, ReviewVerdict};
use kiln_types::{
    CriterionStatus, Run, Status, Task, TaskDetail, TaskState, User, UserSummary, WorkerRole,
};

/// What `viewer` may see of a task card.
pub fn card_view(task: &Task, viewer: &User) -> Task {
    if task.owner_id == viewer.id {
        return task.clone();
    }
    let mut t = task.clone();
    t.description = String::new();
    t.acceptance = Vec::new();
    t.files_hint = Vec::new();
    t
}

/// What `viewer` may see of a run row (errors can quote run output).
pub fn run_view(run: &Run, viewer: &User) -> Run {
    if run.owner_id == viewer.id {
        return run.clone();
    }
    let mut r = run.clone();
    r.error = r.error.as_ref().map(|_| "run failed".to_string());
    r
}

/// Latest QA report and review verdict of a task, from run outcomes.
pub async fn latest_outputs(
    state: &AppState,
    runs: &[Run],
) -> ApiResult<(Option<QaReport>, Option<ReviewVerdict>)> {
    let mut ordered: Vec<&Run> = runs.iter().collect();
    // Newest first: by attempt, then start time.
    ordered.sort_by_key(|r| std::cmp::Reverse((r.attempt, r.started_at)));
    let mut qa = None;
    let mut verdict = None;
    for r in ordered {
        if qa.is_some() && verdict.is_some() {
            break;
        }
        let wants = match r.role {
            WorkerRole::Qa => qa.is_none(),
            WorkerRole::Reviewer => verdict.is_none(),
            _ => false,
        };
        if !wants || !r.state.is_finished() {
            continue;
        }
        if let Some(out) = state.store.get_run_outcome(r.id).await? {
            match r.role {
                WorkerRole::Qa => qa = out.qa_report,
                WorkerRole::Reviewer => verdict = out.verdict,
                _ => {}
            }
        }
    }
    Ok((qa, verdict))
}

/// Acceptance criteria with their QA status.
pub fn criteria(task: &Task, qa: Option<&QaReport>) -> Vec<CriterionStatus> {
    task.acceptance
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let res = qa.and_then(|r| {
                r.criteria
                    .iter()
                    .find(|x| x.criterion.trim() == c.trim())
                    .or_else(|| r.criteria.get(i))
            });
            let status = match res.map(|r| r.status) {
                Some(PassFail::Pass) => Status::Passed,
                Some(PassFail::Fail) => Status::Failed,
                None if task.state == TaskState::Testing => Status::Running,
                None => Status::Queued,
            };
            CriterionStatus {
                criterion: c.clone(),
                status,
            }
        })
        .collect()
}

/// Diff of the task branch: live from the worktree while it exists, else
/// from a `diff` artifact holding a serialized `DiffSummary`.
async fn task_diff(
    state: &AppState,
    task: &Task,
    project: &kiln_types::Project,
    artifacts: &[kiln_types::Artifact],
) -> Option<kiln_types::DiffSummary> {
    let wt = state.gitops.worktree_path(&task.key);
    if wt.join(".git").exists()
        && let Ok(d) = state
            .gitops
            .diff_summary(&wt, &project.default_branch)
            .await
    {
        return Some(d);
    }
    let a = artifacts
        .iter()
        .rev()
        .find(|a| a.kind == kiln_types::ArtifactKind::Diff && a.path.ends_with(".json"))?;
    let path = crate::routes::artifact_path(state, a.run_id, &a.path).ok()?;
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Full task detail as `viewer` may see it.
pub async fn task_detail(state: &AppState, viewer: &User, task: Task) -> ApiResult<TaskDetail> {
    let project = state.store.get_project(task.project_id).await?;
    let owner_user = state.store.get_user(task.owner_id).await?;
    let owner = UserSummary::from(&owner_user);
    let runs = state.store.list_runs_for_task(task.id).await?;
    let is_owner = viewer.id == task.owner_id;
    if is_owner {
        let artifacts = state.store.list_artifacts_for_task(task.id).await?;
        let (qa_report, verdict) = latest_outputs(state, &runs).await?;
        let diff = task_diff(state, &task, &project, &artifacts).await;
        let criteria = criteria(&task, qa_report.as_ref());
        return Ok(TaskDetail {
            task,
            project,
            owner,
            runs,
            artifacts,
            qa_report,
            verdict,
            diff,
            criteria,
        });
    }
    // Criterion pass/fail is card-level metadata; texts are owner-only.
    let (qa, _) = latest_outputs(state, &runs).await?;
    let crit = criteria(&task, qa.as_ref())
        .into_iter()
        .enumerate()
        .map(|(i, mut c)| {
            c.criterion = format!("Criterion {}", i + 1);
            c
        })
        .collect();
    let artifacts = if viewer.is_admin() {
        // Admins see the listing (kind/size) but not captions or content.
        state
            .store
            .list_artifacts_for_task(task.id)
            .await?
            .into_iter()
            .map(|mut a| {
                a.caption = None;
                a
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(TaskDetail {
        task: card_view(&task, viewer),
        project,
        owner,
        runs: runs.iter().map(|r| run_view(r, viewer)).collect(),
        artifacts,
        qa_report: None,
        verdict: None,
        diff: None,
        criteria: crit,
    })
}
