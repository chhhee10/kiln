//! projects, threads/messages/plans, tasks, runs, run_events, artifacts, PRs.

mod common;

use chrono::{Duration, Utc};
use common::*;
use kiln_store::*;
use kiln_types::api::{LogQuery, UpdateProjectRequest};
use kiln_types::event::{LogLevel, RunEvent, RunEventKind};
use kiln_types::protocol::{RunOutcome, Usage};
use kiln_types::*;

#[tokio::test]
async fn projects_crud() {
    let s = store().await;
    let p = project(&s, "fp").await;
    assert_eq!(p.key, "FP");
    assert_eq!(p.qa_status, QaProfileStatus::Unknown);
    assert_eq!(p.loop_cap, 3);
    assert_eq!(s.get_project_by_key("fp").await.unwrap().id, p.id);
    assert!(
        s.create_project(NewProject::new("FP", "dup", "x"))
            .await
            .unwrap_err()
            .is_conflict()
    );
    assert!(matches!(
        s.create_project(NewProject::new("1X", "bad", "x")).await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        s.create_project(NewProject::new("A-B", "bad", "x")).await,
        Err(StoreError::Invalid(_))
    ));

    let p2 = s
        .update_project(
            p.id,
            UpdateProjectRequest {
                name: Some("Acme".into()),
                image: Some("ghcr.io/x/worker:1".into()),
                loop_cap: Some(5),
                resources: Some(ResourceProfile {
                    cpus: 4.0,
                    memory_mb: 8192,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(p2.name, "Acme");
    assert_eq!(p2.image.as_deref(), Some("ghcr.io/x/worker:1"));
    assert_eq!(p2.loop_cap, 5);
    assert_eq!(p2.resources.memory_mb, 8192);
    assert_eq!(p2.default_branch, "main");
    // Untouched image stays; empty string clears it.
    let p3 = s
        .update_project(p.id, UpdateProjectRequest::default())
        .await
        .unwrap();
    assert_eq!(p3.image.as_deref(), Some("ghcr.io/x/worker:1"));
    let p4 = s
        .update_project(
            p.id,
            UpdateProjectRequest {
                image: Some(String::new()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(p4.image, None);
    assert!(matches!(
        s.update_project(
            p.id,
            UpdateProjectRequest {
                loop_cap: Some(0),
                ..Default::default()
            }
        )
        .await,
        Err(StoreError::Invalid(_))
    ));

    let profile: kiln_types::qa::QaProfile =
        serde_json::from_value(serde_json::json!({"version": 1, "boot": ["make up"]})).unwrap();
    s.set_project_qa(p.id, &QaProfileStatus::Valid, Some(&profile))
        .await
        .unwrap();
    assert_eq!(
        s.get_project(p.id).await.unwrap().qa_status,
        QaProfileStatus::Valid
    );
    assert_eq!(s.get_project_qa_profile(p.id).await.unwrap(), Some(profile));
    s.set_project_qa(
        p.id,
        &QaProfileStatus::Invalid {
            error: "bad yaml".into(),
        },
        None,
    )
    .await
    .unwrap();
    assert_eq!(s.get_project_qa_profile(p.id).await.unwrap(), None);
    project(&s, "WEB").await;
    let keys: Vec<_> = s
        .list_projects()
        .await
        .unwrap()
        .into_iter()
        .map(|p| p.key)
        .collect();
    assert_eq!(keys, vec!["FP", "WEB"]);
}

#[tokio::test]
async fn threads_messages_plans() {
    let f = fixture().await;
    let s = &f.store;
    assert_eq!(f.thread.status, PlanStatus::Drafting);
    assert_eq!(
        s.get_thread(f.thread.id).await.unwrap().plan_id,
        Some(f.plan.id)
    );
    assert_eq!(
        s.get_plan_for_thread(f.thread.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        f.plan.id
    );
    assert_eq!(f.plan.version, 1);

    // A second plan for the same thread conflicts.
    let again = s
        .create_plan(NewPlan {
            project_id: f.project.id,
            owner_id: f.user.id,
            thread_id: f.thread.id,
            spec: spec(&[("a", &[])]),
        })
        .await;
    assert!(again.unwrap_err().is_conflict());

    for (i, body) in ["first", "second", "third"].iter().enumerate() {
        s.add_message(NewMessage {
            thread_id: f.thread.id,
            author: if i % 2 == 0 {
                MessageAuthor::User
            } else {
                MessageAuthor::Planner
            },
            body: body.to_string(),
            voice: i == 0,
            plan_version: (i == 1).then_some(1),
        })
        .await
        .unwrap();
    }
    let msgs = s.list_messages(f.thread.id).await.unwrap();
    assert_eq!(
        msgs.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    assert!(msgs[0].voice);
    assert_eq!(msgs[1].plan_version, Some(1));
    assert!(
        s.add_message(NewMessage {
            thread_id: ThreadId::new(),
            author: MessageAuthor::User,
            body: "x".into(),
            voice: false,
            plan_version: None
        })
        .await
        .unwrap_err()
        .is_not_found()
    );

    // Optimistic concurrency on plan edits.
    let v2 = s
        .update_plan_spec(f.plan.id, &spec(&[("t1", &[]), ("t2", &["t1"])]), 1)
        .await
        .unwrap();
    assert_eq!(v2.version, 2);
    assert_eq!(v2.spec.tasks.len(), 2);
    let stale = s
        .update_plan_spec(f.plan.id, &spec(&[("t1", &[])]), 1)
        .await
        .unwrap_err();
    assert!(stale.is_conflict());
    assert_eq!(s.get_plan(f.plan.id).await.unwrap().version, 2);

    let p = s
        .set_plan_status(f.plan.id, PlanStatus::Dispatched)
        .await
        .unwrap();
    assert_eq!(p.status, PlanStatus::Dispatched);
    assert_eq!(
        s.get_thread(f.thread.id).await.unwrap().status,
        PlanStatus::Dispatched
    );
    // Dispatched plans are frozen.
    assert!(
        s.update_plan_spec(f.plan.id, &spec(&[("t1", &[])]), 2)
            .await
            .unwrap_err()
            .is_conflict()
    );

    assert_eq!(s.list_plans(Some(f.user.id), None).await.unwrap().len(), 1);
    assert_eq!(
        s.list_plans(None, Some(PlanStatus::Drafting))
            .await
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        s.list_plans(None, Some(PlanStatus::Dispatched))
            .await
            .unwrap()
            .len(),
        1
    );

    let t2 = s
        .create_thread(NewThread {
            project_id: f.project.id,
            owner_id: f.user.id,
            title: "Second".into(),
        })
        .await
        .unwrap();
    s.update_thread_status(t2.id, PlanStatus::Archived)
        .await
        .unwrap();
    s.set_thread_title(t2.id, "Renamed").await.unwrap();
    let threads = s.list_threads(f.user.id, Some(f.project.id)).await.unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].title, "Renamed", "most recently updated first");
    assert!(
        s.list_threads(f.user.id, Some(ProjectId::new()))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn tasks_get_sequential_keys_and_resolved_deps() {
    let f = fixture().await;
    let s = &f.store;
    let sp = spec(&[("t1", &[]), ("t2", &["t1"]), ("t3", &["t1", "t2"])]);
    let tasks = s
        .create_tasks_for_plan(f.plan.id, new_tasks(&sp))
        .await
        .unwrap();
    let keys: Vec<_> = tasks.iter().map(|t| t.key.as_str()).collect();
    assert_eq!(keys, ["FP-1", "FP-2", "FP-3"]);
    assert_eq!(tasks[2].depends_on, vec![tasks[0].id, tasks[1].id]);
    assert!(
        tasks
            .iter()
            .all(|t| t.state == TaskState::Queued && t.loop_cap == 3 && t.owner_id == f.user.id)
    );
    assert_eq!(
        tasks[0].workers.get(&WorkerRole::Builder).unwrap().harness,
        "mock"
    );

    // A second plan in the same project continues numbering.
    let th = s
        .create_thread(NewThread {
            project_id: f.project.id,
            owner_id: f.user.id,
            title: "b".into(),
        })
        .await
        .unwrap();
    let plan2 = s
        .create_plan(NewPlan {
            project_id: f.project.id,
            owner_id: f.user.id,
            thread_id: th.id,
            spec: spec(&[("x", &[])]),
        })
        .await
        .unwrap();
    let more = s
        .create_tasks_for_plan(plan2.id, new_tasks(&spec(&[("x", &[])])))
        .await
        .unwrap();
    assert_eq!(more[0].key, "FP-4");
    assert_eq!(more[0].number, 4);

    // Unknown dependency rolls back the whole batch (no numbers consumed).
    let bad = s
        .create_tasks_for_plan(plan2.id, new_tasks(&spec(&[("y", &["nope"])])))
        .await
        .unwrap_err();
    assert!(matches!(bad, StoreError::Invalid(_)));
    // Duplicate spec id within a plan conflicts.
    assert!(
        s.create_tasks_for_plan(plan2.id, new_tasks(&spec(&[("x", &[])])))
            .await
            .unwrap_err()
            .is_conflict()
    );
    // A later batch may depend on an already-created task of the plan.
    let z = s
        .create_tasks_for_plan(plan2.id, new_tasks(&spec(&[("z", &["x"])])))
        .await
        .unwrap();
    assert_eq!(z[0].key, "FP-5");
    assert_eq!(z[0].depends_on, vec![more[0].id]);

    assert_eq!(s.get_task_by_key("fp-2").await.unwrap().id, tasks[1].id);
    assert!(s.get_task_by_key("FP-99").await.unwrap_err().is_not_found());
    assert_eq!(
        s.list_tasks(TaskFilter {
            plan: Some(f.plan.id),
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        3
    );
    assert_eq!(
        s.list_tasks(TaskFilter {
            project: Some(f.project.id),
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        5
    );
    assert_eq!(
        s.list_tasks(TaskFilter {
            owners: vec![UserId::new()],
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        0
    );
    assert_eq!(
        s.list_tasks(TaskFilter {
            limit: Some(2),
            ..Default::default()
        })
        .await
        .unwrap()
        .len(),
        2
    );
}

#[tokio::test]
async fn concurrent_task_creation_never_reuses_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("kiln.db")).await.unwrap();
    let u = user(&s, "a@example.com").await;
    let p = project(&s, "FP").await;
    let mut handles = Vec::new();
    for i in 0..8 {
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            let th = s
                .create_thread(NewThread {
                    project_id: p.id,
                    owner_id: u.id,
                    title: format!("t{i}"),
                })
                .await
                .unwrap();
            let plan = s
                .create_plan(NewPlan {
                    project_id: p.id,
                    owner_id: u.id,
                    thread_id: th.id,
                    spec: spec(&[("a", &[]), ("b", &[])]),
                })
                .await
                .unwrap();
            s.create_tasks_for_plan(plan.id, new_tasks(&spec(&[("a", &[]), ("b", &[])])))
                .await
                .unwrap()
        }));
    }
    let mut numbers = Vec::new();
    for h in handles {
        numbers.extend(h.await.unwrap().into_iter().map(|t| t.number));
    }
    numbers.sort();
    assert_eq!(numbers, (1..=16).collect::<Vec<u64>>());
}

#[tokio::test]
async fn transition_cas_validation_and_history() {
    use TaskState::*;
    let f = fixture().await;
    let s = &f.store;
    let t = one_task(&f).await;

    let t1 = s
        .transition_task(t.id, Queued, Preparing, "admitted", None)
        .await
        .unwrap();
    assert_eq!(t1.state, Preparing);
    assert_eq!(t1.last_active, Some(Preparing));

    // CAS: stale `from` conflicts and changes nothing.
    let err = s
        .transition_task(t.id, Queued, Preparing, "dup", None)
        .await
        .unwrap_err();
    assert!(err.is_conflict(), "{err:?}");
    // Not allowed by the state machine.
    assert!(matches!(
        s.transition_task(t.id, Preparing, Reviewing, "skip", None)
            .await,
        Err(StoreError::Invalid(_))
    ));
    // Unknown task.
    assert!(
        s.transition_task(TaskId::new(), Queued, Preparing, "x", None)
            .await
            .unwrap_err()
            .is_not_found()
    );

    s.update_task_progress(t.id, 40, "Building · 2/5")
        .await
        .unwrap();
    let b = s
        .transition_task(t.id, Preparing, Building, "", None)
        .await
        .unwrap();
    assert_eq!(b.progress, 0, "progress resets per stage");
    s.update_task_progress(t.id, 250, "clamped").await.unwrap();
    assert_eq!(s.get_task(t.id).await.unwrap().progress, 100);
    s.transition_task(t.id, Building, Testing, "", None)
        .await
        .unwrap();
    let e = s
        .transition_task(t.id, Testing, Escalated, "loop cap", Some(f.user.id))
        .await
        .unwrap();
    assert_eq!(e.state, Escalated);
    assert_eq!(e.last_active, Some(Testing));
    assert_eq!(e.column(), BoardColumn::Qa);
    let fl = s
        .transition_task(t.id, Escalated, Failed, "", None)
        .await
        .unwrap();
    assert_eq!(
        fl.last_active,
        Some(Testing),
        "halted → halted keeps the last active stage"
    );
    let q = s
        .transition_task(t.id, Failed, Queued, "retry", Some(f.user.id))
        .await
        .unwrap();
    assert_eq!(q.state, Queued);

    let hist = s.list_transitions(t.id).await.unwrap();
    let pairs: Vec<_> = hist.iter().map(|h| (h.from, h.to)).collect();
    assert_eq!(
        pairs,
        vec![
            (Queued, Preparing),
            (Preparing, Building),
            (Building, Testing),
            (Testing, Escalated),
            (Escalated, Failed),
            (Failed, Queued)
        ]
    );
    assert_eq!(hist[3].reason, "loop cap");
    assert_eq!(hist[3].actor, Some(f.user.id));
    assert_eq!(hist[0].actor, None);
}

#[tokio::test]
async fn concurrent_transitions_exactly_one_wins() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(&dir.path().join("kiln.db")).await.unwrap();
    let u = user(&s, "a@example.com").await;
    let p = project(&s, "FP").await;
    let th = s
        .create_thread(NewThread {
            project_id: p.id,
            owner_id: u.id,
            title: "t".into(),
        })
        .await
        .unwrap();
    let plan = s
        .create_plan(NewPlan {
            project_id: p.id,
            owner_id: u.id,
            thread_id: th.id,
            spec: spec(&[("a", &[])]),
        })
        .await
        .unwrap();
    let t = s
        .create_tasks_for_plan(plan.id, new_tasks(&spec(&[("a", &[])])))
        .await
        .unwrap()
        .remove(0);
    let mut handles = Vec::new();
    for _ in 0..8 {
        let s = s.clone();
        handles.push(tokio::spawn(async move {
            s.transition_task(t.id, TaskState::Queued, TaskState::Preparing, "race", None)
                .await
        }));
    }
    let mut ok = 0;
    for h in handles {
        match h.await.unwrap() {
            Ok(_) => ok += 1,
            Err(e) => assert!(e.is_conflict(), "{e:?}"),
        }
    }
    assert_eq!(ok, 1);
    assert_eq!(s.list_transitions(t.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn task_fields_loops_and_feedback() {
    let f = fixture().await;
    let s = &f.store;
    let t = one_task(&f).await;
    assert_eq!(
        s.set_task_branch(t.id, "kiln/fp-1-retry")
            .await
            .unwrap()
            .branch
            .as_deref(),
        Some("kiln/fp-1-retry")
    );
    let pr = s
        .set_task_pr(t.id, "https://github.com/x/y/pull/7", 7)
        .await
        .unwrap();
    assert_eq!(pr.pr_number, Some(7));
    assert_eq!(s.increment_loop(t.id).await.unwrap().loop_count, 1);
    let t2 = s.increment_loop(t.id).await.unwrap();
    assert_eq!(t2.loop_count, 2);
    assert_eq!(t2.attempt(), 3);
    assert_eq!(s.reset_loops(t.id).await.unwrap().loop_count, 0);
    assert!(
        s.increment_loop(TaskId::new())
            .await
            .unwrap_err()
            .is_not_found()
    );

    assert!(s.take_task_feedback(t.id).await.unwrap().is_empty());
    s.push_task_feedback(t.id, "QA: filter resets on refresh")
        .await
        .unwrap();
    s.push_task_feedback(t.id, "Human: also \"quote\" this")
        .await
        .unwrap();
    assert_eq!(s.peek_task_feedback(t.id).await.unwrap().len(), 2);
    let fb = s.take_task_feedback(t.id).await.unwrap();
    assert_eq!(
        fb,
        vec![
            "QA: filter resets on refresh".to_string(),
            "Human: also \"quote\" this".to_string()
        ]
    );
    assert!(s.take_task_feedback(t.id).await.unwrap().is_empty());
    assert!(
        s.push_task_feedback(TaskId::new(), "x")
            .await
            .unwrap_err()
            .is_not_found()
    );

    let counts = s.count_tasks_by_state(Some(f.user.id)).await.unwrap();
    assert_eq!(counts.get("queued"), Some(&1));
}

#[tokio::test]
async fn runs_lifecycle_usage_and_spend() {
    let f = fixture().await;
    let s = &f.store;
    let t = one_task(&f).await;
    let r = run_for(&f, &t, WorkerRole::Builder).await;
    assert_eq!(r.state, RunState::Pending);
    assert!(r.started_at.is_none());
    assert!(matches!(
        s.create_run(NewRun {
            task_id: None,
            thread_id: None,
            owner_id: f.user.id,
            role: WorkerRole::Builder,
            harness: "mock".into(),
            model: "m".into(),
            attempt: 1
        })
        .await,
        Err(StoreError::Invalid(_))
    ));

    s.set_run_container(r.id, "ctr-1", "tokhash").await.unwrap();
    assert_eq!(
        s.run_token_hash(r.id).await.unwrap().as_deref(),
        Some("tokhash")
    );
    assert!(
        s.run_token_hash(RunId::new())
            .await
            .unwrap_err()
            .is_not_found()
    );
    let running = s
        .update_run_state(r.id, RunState::Running, None)
        .await
        .unwrap();
    assert!(running.started_at.is_some());
    assert_eq!(running.container_id.as_deref(), Some("ctr-1"));
    assert_eq!(
        s.list_runs_in_states(&[RunState::Running, RunState::Starting])
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(s.count_live_runs(Some(f.user.id)).await.unwrap(), 1);

    s.set_run_progress(r.id, 55).await.unwrap();
    s.add_run_usage(r.id, 100, 50, 0.25).await.unwrap();
    s.add_run_usage(r.id, 10, 5, 0.25).await.unwrap();
    let mid = s.get_run(r.id).await.unwrap();
    assert_eq!((mid.tokens_in, mid.tokens_out), (110, 55));
    assert!((mid.cost_usd - 0.5).abs() < 1e-9);
    assert_eq!(mid.progress, 55);

    // Outcome without usage keeps accumulated totals.
    let mut outcome = RunOutcome::failed("boom");
    outcome.last_seq = 12;
    let done = s
        .finish_run(r.id, RunState::Failed, &outcome)
        .await
        .unwrap();
    assert_eq!(done.state, RunState::Failed);
    assert!(done.ended_at.is_some());
    assert_eq!(done.error.as_deref(), Some("boom"));
    assert_eq!(done.tokens_in, 110);
    assert_eq!(s.get_run_outcome(r.id).await.unwrap(), Some(outcome));
    assert!(matches!(
        s.finish_run(r.id, RunState::Running, &RunOutcome::failed("x"))
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(s.list_uningested_runs().await.unwrap().len(), 1);
    s.mark_ingested(r.id).await.unwrap();
    assert!(s.get_run(r.id).await.unwrap().ingested);
    assert!(s.list_uningested_runs().await.unwrap().is_empty());

    // Outcome with usage is authoritative.
    let r2 = run_for(&f, &t, WorkerRole::Qa).await;
    s.add_run_usage(r2.id, 1, 1, 0.01).await.unwrap();
    let ok = RunOutcome {
        success: true,
        summary: "passed".into(),
        exit_code: Some(0),
        usage: Usage {
            tokens_in: 1000,
            tokens_out: 200,
            cost_usd: 1.5,
        },
        commit: None,
        qa_report: None,
        verdict: None,
        plan: None,
        reply: None,
        error: None,
        last_seq: 3,
    };
    let done2 = s.finish_run(r2.id, RunState::Succeeded, &ok).await.unwrap();
    assert_eq!(done2.tokens_in, 1000);
    assert_eq!(done2.progress, 100);
    assert!(done2.started_at.is_some());

    assert_eq!(s.list_runs_for_task(t.id).await.unwrap().len(), 2);
    assert_eq!(
        s.latest_run_for_role(t.id, WorkerRole::Qa)
            .await
            .unwrap()
            .unwrap()
            .id,
        r2.id
    );
    assert!(
        s.latest_run_for_role(t.id, WorkerRole::Reviewer)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(s.increment_run_retries(r.id).await.unwrap(), 1);
    assert_eq!(s.increment_run_retries(r.id).await.unwrap(), 2);

    let spend = s.month_spend(f.user.id).await.unwrap();
    assert!((spend - 2.0).abs() < 1e-9, "{spend}");
    assert_eq!(s.month_spend(UserId::new()).await.unwrap(), 0.0);

    // Planner run on a thread.
    let pr = s
        .create_run(NewRun {
            task_id: None,
            thread_id: Some(f.thread.id),
            owner_id: f.user.id,
            role: WorkerRole::Planner,
            harness: "mock".into(),
            model: "m".into(),
            attempt: 1,
        })
        .await
        .unwrap();
    assert_eq!(
        s.list_runs_for_thread(f.thread.id).await.unwrap()[0].id,
        pr.id
    );
}

fn ev(run: RunId, seq: u64, msg: &str) -> RunEvent {
    RunEvent {
        run_id: run,
        seq,
        ts: Utc::now(),
        kind: RunEventKind::Log {
            level: LogLevel::Info,
            message: msg.into(),
        },
    }
}

#[tokio::test]
async fn run_events_are_idempotent_and_queryable() {
    let f = fixture().await;
    let s = &f.store;
    let t = one_task(&f).await;
    let b = run_for(&f, &t, WorkerRole::Builder).await;
    let q = run_for(&f, &t, WorkerRole::Qa).await;

    let batch: Vec<_> = (1..=5)
        .map(|i| (ev(b.id, i, &format!("build line {i}")), WorkerRole::Builder))
        .collect();
    assert_eq!(s.insert_run_events(&batch).await.unwrap(), 5);
    // Replaying the spool inserts nothing new.
    assert_eq!(s.insert_run_events(&batch).await.unwrap(), 0);
    // Overlapping batch inserts only the new tail.
    let overlap: Vec<_> = (4..=7)
        .map(|i| (ev(b.id, i, &format!("build line {i}")), WorkerRole::Builder))
        .collect();
    assert_eq!(s.insert_run_events(&overlap).await.unwrap(), 2);
    assert_eq!(s.insert_run_events(&[]).await.unwrap(), 0);
    let qa_events = vec![
        (ev(q.id, 1, "QA boot 100%_done"), WorkerRole::Qa),
        (
            RunEvent {
                run_id: q.id,
                seq: 2,
                ts: Utc::now(),
                kind: RunEventKind::Command {
                    command: "cargo test".into(),
                    exit_code: Some(1),
                    output: String::new(),
                },
            },
            WorkerRole::Qa,
        ),
    ];
    assert_eq!(s.insert_run_events(&qa_events).await.unwrap(), 2);
    let last_seq: i64 = sqlx::query_scalar("SELECT last_seq FROM runs WHERE id = ?")
        .bind(b.id.to_string())
        .fetch_one(s.pool())
        .await
        .unwrap();
    assert_eq!(last_seq, 7);

    let all = s
        .list_log_lines(LogTarget::Task(t.id), &LogQuery::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 9);
    let run_only = s
        .list_log_lines(LogTarget::Run(b.id), &LogQuery::default())
        .await
        .unwrap();
    assert_eq!(
        run_only.iter().map(|l| l.seq).collect::<Vec<_>>(),
        (1..=7).collect::<Vec<_>>()
    );
    let qa = s
        .list_log_lines(
            LogTarget::Task(t.id),
            &LogQuery {
                role: Some(WorkerRole::Qa),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(qa.len(), 2);
    assert_eq!(qa[1].level, LogLevel::Fail);
    assert_eq!(qa[1].message, "$ cargo test → exit 1");
    // Tail: last N in chronological order.
    let tail = s
        .list_log_lines(
            LogTarget::Run(b.id),
            &LogQuery {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(tail.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![6, 7]);
    let after = s
        .list_log_lines(
            LogTarget::Run(b.id),
            &LogQuery {
                after_seq: Some(5),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(after.iter().map(|l| l.seq).collect::<Vec<_>>(), vec![6, 7]);
    // Filter is a literal substring (LIKE wildcards escaped), case-insensitive.
    let filt = s
        .list_log_lines(
            LogTarget::Task(t.id),
            &LogQuery {
                filter: Some("100%_D".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(filt.len(), 1);
    let none = s
        .list_log_lines(
            LogTarget::Task(t.id),
            &LogQuery {
                filter: Some("%".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(none.len(), 1, "only the line containing a literal %");

    assert_eq!(
        s.prune_run_events(Utc::now() - Duration::days(14))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        s.prune_run_events(Utc::now() + Duration::seconds(1))
            .await
            .unwrap(),
        9
    );
}

#[tokio::test]
async fn artifacts_idempotent_on_path() {
    let f = fixture().await;
    let s = &f.store;
    let t = one_task(&f).await;
    let r = run_for(&f, &t, WorkerRole::Qa).await;
    let a = s
        .insert_artifact(NewArtifact {
            run_id: r.id,
            kind: ArtifactKind::Screenshot,
            path: "out/artifacts/login.png".into(),
            size: 10,
            mime: "image/png".into(),
            caption: Some("login".into()),
        })
        .await
        .unwrap();
    let again = s
        .insert_artifact(NewArtifact {
            run_id: r.id,
            kind: ArtifactKind::Screenshot,
            path: "out/artifacts/login.png".into(),
            size: 20,
            mime: "image/png".into(),
            caption: None,
        })
        .await
        .unwrap();
    assert_eq!(a.id, again.id);
    assert_eq!(again.size, 20);
    assert_eq!(again.caption.as_deref(), Some("login"));
    assert_eq!(s.get_artifact(a.id).await.unwrap().size, 20);
    s.insert_artifact(NewArtifact {
        run_id: r.id,
        kind: ArtifactKind::QaReport,
        path: "out/qa_report.json".into(),
        size: 1,
        mime: "application/json".into(),
        caption: None,
    })
    .await
    .unwrap();
    assert_eq!(s.list_artifacts_for_task(t.id).await.unwrap().len(), 2);
    assert_eq!(s.list_artifacts_for_run(r.id).await.unwrap().len(), 2);
    for bad in ["/etc/passwd", "../x", "out/../../x", ""] {
        let e = s
            .insert_artifact(NewArtifact {
                run_id: r.id,
                kind: ArtifactKind::Other,
                path: bad.into(),
                size: 0,
                mime: "x".into(),
                caption: None,
            })
            .await;
        assert!(matches!(e, Err(StoreError::Invalid(_))), "{bad}");
    }
}

#[tokio::test]
async fn pull_requests() {
    let f = fixture().await;
    let s = &f.store;
    let t = one_task(&f).await;
    assert_eq!(s.next_pr_number("local").await.unwrap(), 1);
    let pr = s
        .insert_pr(NewPr {
            task_id: t.id,
            provider: "local".into(),
            number: 1,
            url: "local://FP/1".into(),
            branch: "kiln/fp-1".into(),
            title: "FP-1".into(),
            body: "body".into(),
        })
        .await
        .unwrap();
    assert_eq!(pr.state, PullRequestState::Open);
    assert_eq!(s.next_pr_number("local").await.unwrap(), 2);
    assert_eq!(s.list_open_prs().await.unwrap().len(), 1);
    assert_eq!(s.get_pr_for_task(t.id).await.unwrap().unwrap().id, pr.id);
    s.set_pr_state(pr.id, PullRequestState::Merged)
        .await
        .unwrap();
    assert!(s.list_open_prs().await.unwrap().is_empty());
    assert!(
        s.set_pr_state(9999, PullRequestState::Closed)
            .await
            .unwrap_err()
            .is_not_found()
    );
}
