//! Pipeline tests: happy path, loops, review feedback, human actions,
//! timeouts, merge polling and secret redaction. ProcessRuntime + mock
//! harness + LocalPrs + a temp local git remote.

mod common;

use common::*;
use kiln_types::api::{CreateCredentialRequest, TaskAction};
use kiln_types::event::ServerEvent;
use kiln_types::plan::WorkerChoice;
use kiln_types::*;
use std::time::Duration;

const T: Duration = Duration::from_secs(60);

async fn assert_contiguous_events(env: &Env, task: TaskId) {
    for r in env
        .runs(task)
        .await
        .into_iter()
        .filter(|r| r.started_at.is_some())
    {
        let q = kiln_types::api::LogQuery {
            limit: Some(5000),
            ..Default::default()
        };
        let lines = env
            .store
            .list_log_lines(kiln_store::LogTarget::Run(r.id), &q)
            .await
            .unwrap();
        let mut seqs: Vec<u64> = lines.iter().map(|l| l.seq).collect();
        seqs.sort();
        let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
        assert_eq!(
            seqs, expected,
            "run {} ({:?}) events must be contiguous",
            r.id, r.role
        );
        let spool = std::fs::read_to_string(
            env.data
                .join("runs")
                .join(r.id.to_string())
                .join("spool.jsonl"),
        )
        .unwrap_or_default();
        assert_eq!(
            spool.lines().filter(|l| !l.trim().is_empty()).count(),
            seqs.len(),
            "spool and run_events agree for {}",
            r.id
        );
    }
}

fn branch_files(env: &Env, branch: &str) -> Vec<String> {
    git(&env.remote, &["ls-tree", "-r", "--name-only", branch])
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn happy_path_two_stacked_tasks() {
    let env = Env::new(Opts {
        llm_distill: true,
        ..Default::default()
    })
    .await;
    let u = env.member("ada@acme.dev").await;
    let (plan, tasks) = env
        .dispatch(&u, spec(&[("t1", &[]), ("t2", &["t1"])]))
        .await;
    assert_eq!(tasks.len(), 2);
    let t1 = env.wait_state(tasks[0].id, TaskState::Ready, T).await;
    let t2 = env.wait_state(tasks[1].id, TaskState::Ready, T).await;

    use TaskState::*;
    let path = vec![
        (Queued, Preparing),
        (Preparing, Building),
        (Building, Testing),
        (Testing, Reviewing),
        (Reviewing, Ready),
    ];
    assert_eq!(env.transitions(t1.id).await, path);
    assert_eq!(env.transitions(t2.id).await, path);
    assert_eq!(t1.meta, format!("PR #{} ready", t1.pr_number.unwrap()));
    assert_eq!(t1.progress, 100);

    // Stacked: t2 branches from t1's branch and its PR targets it.
    let prs = env.prs.list().await.unwrap();
    assert_eq!(prs.len(), 2);
    let b1 = t1.branch.clone().unwrap();
    let b2 = t2.branch.clone().unwrap();
    let pr2 = prs.iter().find(|p| p.head == b2).unwrap();
    assert_eq!(pr2.base, b1);
    assert_eq!(prs.iter().find(|p| p.head == b1).unwrap().base, "main");
    assert!(pr2.body.contains(&t2.key));

    // Commits are on the pushed branches; generated files are not.
    for b in [&b1, &b2] {
        let files = branch_files(&env, b);
        assert!(files.contains(&"KILN_MOCK.md".to_string()), "{files:?}");
        assert!(
            !files.iter().any(|f| f == "AGENTS.md" || f == "CLAUDE.md"),
            "{files:?}"
        );
    }
    // …but they were written into the worktree for the workers.
    let runs = env.runs(t1.id).await;
    assert_eq!(
        runs.iter()
            .filter(|r| r.role == WorkerRole::Builder)
            .count(),
        1
    );
    assert!(
        runs.iter().all(|r| r.state == RunState::Succeeded),
        "{runs:?}"
    );

    // Events: upserts for every stage, run upserts and log lines.
    let ev = env.events.lock().unwrap().clone();
    for want in [Preparing, Building, Testing, Reviewing, Ready] {
        assert!(ev.iter().any(|e| matches!(e, ServerEvent::TaskUpserted { task } if task.id == t1.id && task.state == want)), "{want:?}");
    }
    assert!(
        ev.iter()
            .any(|e| matches!(e, ServerEvent::RunUpserted { run } if run.task_id == Some(t1.id)))
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, ServerEvent::Log { task_id: Some(t), .. } if *t == t1.id))
    );
    assert!(ev.iter().any(|e| matches!(e, ServerEvent::PlanUpserted { plan: p } if p.id == plan.id && p.status == PlanStatus::Dispatched)));

    // Owner notified per task; runs ingested; containers cleaned up.
    let notes = env.notifications(u.id).await;
    assert_eq!(
        notes
            .iter()
            .filter(|n| n.kind == NotificationKind::TaskReady)
            .count(),
        2
    );
    env.wait_all_ingested(T).await;
    assert_contiguous_events(&env, t1.id).await;
    assert_contiguous_events(&env, t2.id).await;
    // Distiller containers ran with the owner's (mock) harness.
    env.wait_until("distill jobs done", T, || async {
        env.store
            .queued_jobs(Some("distill"))
            .await
            .unwrap()
            .is_empty()
            && env
                .store
                .jobs_in_state(kiln_store::JobState::Running, Some("distill"))
                .await
                .unwrap()
                .is_empty()
    })
    .await;
    let all = env.runs(t1.id).await;
    assert!(
        all.iter().any(|r| r.role == WorkerRole::Distiller),
        "{all:?}"
    );
    // Worktree removed at Ready.
    assert!(!env.data.join("worktrees").join(&t1.key).exists());
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tracked_agents_md_is_kept_out_of_commits() {
    let env = Env::new(Opts {
        tracked_agents_md: true,
        ..Default::default()
    })
    .await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let t = env.wait_state(tasks[0].id, TaskState::Ready, T).await;
    let b = t.branch.unwrap();
    assert_eq!(
        git(&env.remote, &["show", &format!("{b}:AGENTS.md")]),
        "# Repo's own agent guide"
    );
    assert!(!branch_files(&env, &b).contains(&"CLAUDE.md".to_string()));
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qa_fails_twice_then_passes() {
    let env = Env::new(Opts::default().mock("mock.fail_qa_times", "2")).await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let t = env.wait_state(tasks[0].id, TaskState::Ready, T).await;
    assert_eq!(t.loop_count, 2);
    let tr = env.transitions(t.id).await;
    assert_eq!(
        tr.iter()
            .filter(|x| **x == (TaskState::Testing, TaskState::Looping))
            .count(),
        2
    );
    assert_eq!(
        tr.iter()
            .filter(|x| **x == (TaskState::Looping, TaskState::Building))
            .count(),
        2
    );
    let runs = env.runs(t.id).await;
    let builders: Vec<_> = runs
        .iter()
        .filter(|r| r.role == WorkerRole::Builder)
        .collect();
    assert_eq!(
        builders.iter().map(|r| r.attempt).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    // Attempt 2's prompt carries QA's failure.
    let cfg = env.run_config(builders[1].id).await;
    assert!(
        cfg.prompt.contains("filter resets on refresh"),
        "{}",
        cfg.prompt
    );
    assert!(cfg.prompt.contains("Attempt **2 of 3**"));
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qa_fails_three_times_escalates() {
    let env = Env::new(Opts::default().mock("mock.fail_qa_times", "3")).await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let t = env.wait_state(tasks[0].id, TaskState::Escalated, T).await;
    assert_eq!(t.meta, "Needs you");
    assert_eq!(t.last_active, Some(TaskState::Looping));
    let builders = env
        .runs(t.id)
        .await
        .into_iter()
        .filter(|r| r.role == WorkerRole::Builder)
        .count();
    assert_eq!(builders, 3);
    let notes = env.notifications(u.id).await;
    let n = notes
        .iter()
        .find(|n| n.kind == NotificationKind::TaskEscalated)
        .expect("escalation notification");
    assert_eq!(n.task_id, Some(t.id));
    assert!(n.actions.iter().any(|a| a.id == "retry"));
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reviewer_changes_requested_loops_with_findings() {
    let env = Env::new(Opts::default().mock("mock.fail_review_times", "1")).await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let t = env.wait_state(tasks[0].id, TaskState::Ready, T).await;
    assert_eq!(t.loop_count, 1);
    assert!(
        env.transitions(t.id)
            .await
            .contains(&(TaskState::Reviewing, TaskState::Looping))
    );
    let b2 = env
        .runs(t.id)
        .await
        .into_iter()
        .find(|r| r.role == WorkerRole::Builder && r.attempt == 2)
        .unwrap();
    let prompt = env.run_config(b2.id).await.prompt;
    assert!(prompt.contains("Reviewer requested changes"), "{prompt}");
    assert!(prompt.contains("mock: please add a test"), "{prompt}");
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn human_stop_retry_feedback_escalate() {
    let env = Env::new(
        Opts::default()
            .mock("mock.sleep_ms", "400")
            .mock("mock.steps", "4"),
    )
    .await;
    let u = env.member("ada@acme.dev").await;
    let admin = env
        .user("boss@acme.dev", UserRole::Admin, UserLimits::default())
        .await;
    let other = env.member("eve@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let id = tasks[0].id;
    env.wait_state(id, TaskState::Building, T).await;
    env.wait_until("builder running", T, || async {
        env.runs(id)
            .await
            .iter()
            .any(|r| r.role == WorkerRole::Builder && r.state == RunState::Running)
    })
    .await;

    // Others may not act.
    let e = env
        .sched
        .task_action(&other, id, TaskAction::Stop)
        .await
        .unwrap_err();
    assert_eq!(e.status(), 403);

    // Stop mid-Building.
    let t = env
        .sched
        .task_action(&u, id, TaskAction::Stop)
        .await
        .unwrap();
    assert_eq!(t.state, TaskState::Cancelled);
    assert_eq!(t.meta, "Stopped");
    env.wait_until("builder cancelled", T, || async {
        env.runs(id).await.iter().all(|r| r.state.is_finished())
    })
    .await;
    assert!(
        env.runs(id)
            .await
            .iter()
            .any(|r| r.state == RunState::Cancelled)
    );

    // Retry → runs again (attempt 2) to Ready.
    let t = env
        .sched
        .task_action(&u, id, TaskAction::Retry)
        .await
        .unwrap();
    assert_eq!(t.state, TaskState::Queued);
    let t = env.wait_state(id, TaskState::Ready, T).await;
    assert_eq!(t.attempt(), 2);

    // Feedback on Ready → Looping → (re-admitted) Building with the note.
    let note = "Please also handle the 429 case";
    let t2 = env
        .sched
        .task_action(
            &u,
            id,
            TaskAction::Feedback {
                text: note.into(),
                attach_qa: true,
                attach_verdict: true,
                save_memory: true,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(
            t2.state,
            TaskState::Queued | TaskState::Preparing | TaskState::Building
        ),
        "{:?}",
        t2.state
    );
    let tr = env.transitions(id).await;
    assert!(tr.contains(&(TaskState::Ready, TaskState::Looping)));
    env.wait_state(id, TaskState::Building, T).await;
    env.wait_until("attempt-3 builder", T, || async {
        env.runs(id)
            .await
            .iter()
            .any(|r| r.role == WorkerRole::Builder && r.attempt == 3)
    })
    .await;
    let b3 = env
        .runs(id)
        .await
        .into_iter()
        .find(|r| r.role == WorkerRole::Builder && r.attempt == 3)
        .unwrap();
    let prompt = env.run_config(b3.id).await.prompt;
    assert!(prompt.contains(note), "{prompt}");

    // Escalate while it builds: admins and the owner are told.
    let t = env
        .sched
        .task_action(
            &admin,
            id,
            TaskAction::Escalate {
                note: Some("blocked on API keys".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(t.state, TaskState::Escalated);
    for who in [u.id, admin.id] {
        let n = env.notifications(who).await;
        assert!(
            n.iter()
                .any(|n| n.kind == NotificationKind::TaskEscalated && n.task_id == Some(id)),
            "{who:?}"
        );
    }
    env.wait_until("runs stopped", T, || async {
        env.runs(id).await.iter().all(|r| r.state.is_finished())
    })
    .await;
    // Feedback on Escalated → exactly one more attempt.
    let t = env
        .sched
        .task_action(
            &u,
            id,
            TaskAction::Feedback {
                text: "try the other client".into(),
                attach_qa: false,
                attach_verdict: false,
                save_memory: false,
            },
        )
        .await
        .unwrap();
    assert!(t.state != TaskState::Escalated);
    env.wait_state(id, TaskState::Ready, T).await;
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hung_run_times_out_retries_once_then_escalates() {
    let env = Env::new(Opts {
        timeout: Some(Duration::from_secs(2)),
        ..Opts::default().mock("mock.hang", "1")
    })
    .await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let t = env.wait_state(tasks[0].id, TaskState::Escalated, T).await;
    let runs = env.runs(t.id).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert!(
        runs.iter()
            .all(|r| r.state == RunState::TimedOut && r.role == WorkerRole::Builder),
        "{runs:?}"
    );
    assert_eq!(
        runs.iter().map(|r| r.retries).collect::<Vec<_>>(),
        vec![0, 1]
    );
    let notes = env.notifications(u.id).await;
    assert!(
        notes
            .iter()
            .any(|n| n.kind == NotificationKind::TaskFailed && n.task_id == Some(t.id))
    );
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merge_polling_marks_merged() {
    let env = Env::new(Opts::default()).await;
    let u = env.member("ada@acme.dev").await;
    let (plan, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let t = env.wait_state(tasks[0].id, TaskState::Ready, T).await;
    assert_eq!(env.sched.poll_prs().await.unwrap(), 0);
    // Merge on the "remote" (fast-forward main to the task branch).
    let b = t.branch.unwrap();
    git(
        &env.remote,
        &["update-ref", "refs/heads/main", &format!("refs/heads/{b}")],
    );
    assert_eq!(env.sched.poll_prs().await.unwrap(), 1);
    let t = env.task(t.id).await;
    assert_eq!(t.state, TaskState::Merged);
    assert_eq!(
        env.store.get_plan(plan.id).await.unwrap().status,
        PlanStatus::Merged
    );
    assert_eq!(
        env.store
            .get_pr_for_task(t.id)
            .await
            .unwrap()
            .unwrap()
            .state,
        kiln_store::PullRequestState::Merged
    );
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn secrets_are_redacted_in_run_events_and_l0() {
    let env = Env::new(Opts::default()).await;
    let u = env.member("ada@acme.dev").await;
    let secret = "sk-ant-api03-SuperSecretValue-1234567890abcdef";
    env.vault
        .add(
            u.id,
            CreateCredentialRequest {
                kind: CredentialKind::AnthropicApiKey,
                name: "anthropic".into(),
                secret: secret.into(),
                base_url: None,
                env_var: None,
            },
        )
        .await
        .unwrap();
    // The mock logs its model name, so a "model" equal to the secret puts
    // the secret value into a log line.
    let mut s = spec(&[("t1", &[])]);
    s.tasks[0].workers.builder = Some(WorkerChoice::new("mock", secret));
    let (_, tasks) = env.dispatch(&u, s).await;
    let t = env.wait_state(tasks[0].id, TaskState::Ready, T).await;
    env.wait_all_ingested(T).await;
    let builder = env
        .runs(t.id)
        .await
        .into_iter()
        .find(|r| r.role == WorkerRole::Builder)
        .unwrap();
    let q = kiln_types::api::LogQuery {
        limit: Some(5000),
        ..Default::default()
    };
    let lines = env
        .store
        .list_log_lines(kiln_store::LogTarget::Run(builder.id), &q)
        .await
        .unwrap();
    assert!(!lines.is_empty());
    assert!(
        lines.iter().all(|l| !l.message.contains(secret)),
        "secret leaked into run_events"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.message.contains(kiln_vault::REDACTED)),
        "{lines:#?}"
    );
    let l0 = env.memory.read_l0(builder.id).await.unwrap();
    assert!(!l0.is_empty());
    let text = serde_json::to_string(&l0).unwrap();
    assert!(!text.contains(secret), "secret leaked into L0");
    assert!(text.contains(kiln_vault::REDACTED));
    // Nor in published log events.
    let logs: Vec<ServerEvent> = env
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, ServerEvent::Log { .. }))
        .cloned()
        .collect();
    assert!(!logs.is_empty());
    assert!(
        !serde_json::to_string(&logs)
            .unwrap()
            .contains("SuperSecretValue")
    );
    env.sched.shutdown().await;
}

fn qa_with_service(command: &str, healthcheck: &str) -> Opts {
    Opts {
        qa_yaml: Some(format!(
            "version: 1\nresources: {{ cpus: 1, memory: 1g }}\nchecks:\n  unit: cargo test\n\
             services:\n  db:\n    image: postgres:16\n    command: {command}\n    healthcheck: \"{healthcheck}\"\n"
        )),
        ..Opts::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qa_waits_for_healthy_services() {
    // The service becomes healthy once its marker file exists.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("up");
    let cmd = format!(
        "[\"sh\", \"-c\", \"sleep 1; touch {}; sleep 60\"]",
        marker.display()
    );
    let env = Env::new(qa_with_service(
        &cmd,
        &format!("test -f {}", marker.display()),
    ))
    .await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let t = env.wait_state(tasks[0].id, TaskState::Ready, T).await;
    assert!(marker.exists());
    let qa = env
        .runs(t.id)
        .await
        .into_iter()
        .find(|r| r.role == WorkerRole::Qa)
        .unwrap();
    assert_eq!(qa.error, None);
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qa_fails_when_a_service_dies_unhealthy() {
    let env = Env::new(qa_with_service("[\"false\"]", "false")).await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let id = tasks[0].id;
    let deadline = std::time::Instant::now() + T;
    let qa = loop {
        let failed = env
            .runs(id)
            .await
            .into_iter()
            .find(|r| r.role == WorkerRole::Qa && r.state == RunState::Failed);
        if let Some(r) = failed {
            break r;
        }
        assert!(std::time::Instant::now() < deadline, "QA run never failed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let err = qa.error.unwrap_or_default();
    assert!(err.contains("`db` exited before becoming healthy"), "{err}");
    env.sched.shutdown().await;
}
