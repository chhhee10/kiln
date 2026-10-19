//! Crash recovery: a daemon restart mid-run loses no task state, events or
//! transcripts (PRD §13).

mod common;

use common::*;
use kiln_sandbox::{ProcessRuntime, Runtime};
use kiln_types::*;
use std::sync::Arc;
use std::time::Duration;

const T: Duration = Duration::from_secs(90);

async fn running_builder(env: &Env, task: TaskId, min_events: usize) -> Run {
    env.wait_until("builder running with events", T, || async {
        let runs = env.runs(task).await;
        let Some(r) = runs
            .iter()
            .find(|r| r.role == WorkerRole::Builder && r.state == RunState::Running)
        else {
            return false;
        };
        let q = kiln_types::api::LogQuery {
            limit: Some(5000),
            ..Default::default()
        };
        env.store
            .list_log_lines(kiln_store::LogTarget::Run(r.id), &q)
            .await
            .unwrap()
            .len()
            >= min_events
    })
    .await;
    env.runs(task)
        .await
        .into_iter()
        .find(|r| r.role == WorkerRole::Builder && r.state == RunState::Running)
        .unwrap()
}

async fn assert_no_lost_events(env: &Env, run: RunId) {
    let r = env.store.get_run(run).await.unwrap();
    let q = kiln_types::api::LogQuery {
        limit: Some(5000),
        ..Default::default()
    };
    let lines = env
        .store
        .list_log_lines(kiln_store::LogTarget::Run(run), &q)
        .await
        .unwrap();
    let mut seqs: Vec<u64> = lines.iter().map(|l| l.seq).collect();
    seqs.sort();
    let spool = std::fs::read_to_string(
        env.data
            .join("runs")
            .join(run.to_string())
            .join("spool.jsonl"),
    )
    .unwrap();
    let n = spool.lines().filter(|l| !l.trim().is_empty()).count() as u64;
    assert!(n > 0);
    assert_eq!(
        seqs,
        (1..=n).collect::<Vec<_>>(),
        "run {run}: stored seqs must equal the spool 1..={n}"
    );
    let o = env.store.get_run_outcome(run).await.unwrap().unwrap();
    assert_eq!(o.last_seq, n, "outcome.last_seq");
    assert!(r.ingested);
    // L0 has every spooled event too.
    let l0 = env.memory.read_l0(run).await.unwrap();
    let l0_seqs: std::collections::BTreeSet<u64> =
        l0.iter().map(|e| e.seq).filter(|s| *s <= n).collect();
    assert_eq!(l0_seqs.len() as u64, n, "L0 has every event");
    // The mock's native transcript survived in the run folder.
    assert!(
        env.data
            .join("runs")
            .join(run.to_string())
            .join("transcripts/mock/session.jsonl")
            .is_file()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reattach_to_live_container_after_restart() {
    let mut env = Env::new(
        Opts::default()
            .mock("mock.sleep_ms", "400")
            .mock("mock.steps", "8"),
    )
    .await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let id = tasks[0].id;
    let run = running_builder(&env, id, 3).await;

    // "Kill" the daemon: every loop, driver and socket stops; the runner
    // keeps running (same ProcessRuntime = container still alive).
    env.sched.shutdown().await;
    let before = env.store.list_transitions(id).await.unwrap().len();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        env.task(id).await.state,
        TaskState::Building,
        "state survives"
    );

    env.sched = env.make_scheduler(env.runtime.clone()).await;
    env.sched.start().await.unwrap();
    let t = env.wait_state(id, TaskState::Ready, T).await;
    let tr = env.store.list_transitions(t.id).await.unwrap();
    assert!(tr.len() > before);
    // The same Builder run finished (re-attached, not retried).
    let builders: Vec<_> = env
        .runs(id)
        .await
        .into_iter()
        .filter(|r| r.role == WorkerRole::Builder)
        .collect();
    assert_eq!(builders.len(), 1, "{builders:?}");
    assert_eq!(builders[0].id, run.id);
    assert_eq!(builders[0].state, RunState::Succeeded);
    env.wait_all_ingested(T).await;
    assert_no_lost_events(&env, run.id).await;
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outcome_recovered_from_run_folder_when_container_is_gone() {
    let mut env = Env::new(Opts {
        reconnect_budget: Duration::from_secs(2),
        ..Opts::default()
            .mock("mock.sleep_ms", "300")
            .mock("mock.steps", "4")
    })
    .await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let id = tasks[0].id;
    let run = running_builder(&env, id, 2).await;
    env.sched.shutdown().await;
    // The runner finishes while the daemon is down and writes outcome.json.
    let outcome = env
        .data
        .join("runs")
        .join(run.id.to_string())
        .join("out/outcome.json");
    env.wait_until("outcome.json", T, || async { outcome.is_file() })
        .await;
    let cid = env
        .store
        .get_run(run.id)
        .await
        .unwrap()
        .container_id
        .unwrap();
    env.wait_until("runner exited", T, || async {
        !env.runtime.is_running(&cid).await.unwrap()
    })
    .await;

    // New daemon with a fresh runtime: the old "container" is gone.
    env.sched = env.make_scheduler(Arc::new(ProcessRuntime::new())).await;
    env.sched.start().await.unwrap();
    let t = env.wait_state(id, TaskState::Ready, T).await;
    let builders: Vec<_> = env
        .runs(t.id)
        .await
        .into_iter()
        .filter(|r| r.role == WorkerRole::Builder)
        .collect();
    assert_eq!(builders.len(), 1, "{builders:?}");
    assert_eq!(builders[0].state, RunState::Succeeded);
    env.wait_all_ingested(T).await;
    assert_no_lost_events(&env, run.id).await;
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lost_run_is_retried_once() {
    let mut env = Env::new(
        Opts::default()
            .mock("mock.sleep_ms", "400")
            .mock("mock.steps", "6"),
    )
    .await;
    let u = env.member("ada@acme.dev").await;
    let (_, tasks) = env.dispatch(&u, spec(&[("t1", &[])])).await;
    let id = tasks[0].id;
    let run = running_builder(&env, id, 2).await;
    env.sched.shutdown().await;
    // The container dies with the daemon and leaves no outcome.
    let cid = env
        .store
        .get_run(run.id)
        .await
        .unwrap()
        .container_id
        .unwrap();
    env.runtime
        .stop(&cid, Duration::from_millis(100))
        .await
        .unwrap();
    let _ = std::fs::remove_file(
        env.data
            .join("runs")
            .join(run.id.to_string())
            .join("out/outcome.json"),
    );

    env.sched = env.make_scheduler(Arc::new(ProcessRuntime::new())).await;
    env.sched.start().await.unwrap();
    let t = env.wait_state(id, TaskState::Ready, T).await;
    let builders: Vec<_> = env
        .runs(t.id)
        .await
        .into_iter()
        .filter(|r| r.role == WorkerRole::Builder)
        .collect();
    assert_eq!(builders.len(), 2, "{builders:?}");
    assert_eq!(builders[0].state, RunState::Lost);
    assert_eq!(builders[0].retries, 0);
    assert_eq!(builders[1].state, RunState::Succeeded);
    assert_eq!(builders[1].retries, 1);
    assert_eq!(builders[1].attempt, 1, "a retry is the same attempt");
    // Events the lost run spooled before dying were still ingested.
    env.wait_all_ingested(T).await;
    let q = kiln_types::api::LogQuery {
        limit: Some(5000),
        ..Default::default()
    };
    assert!(
        !env.store
            .list_log_lines(kiln_store::LogTarget::Run(run.id), &q)
            .await
            .unwrap()
            .is_empty()
    );
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preparing_tasks_are_requeued() {
    let mut env = Env::new(Opts::default()).await;
    let u = env.member("ada@acme.dev").await;
    env.sched.shutdown().await;
    // A task stuck in Preparing (daemon died mid-preparation).
    let s = spec(&[("t1", &[])]);
    let thread = env
        .store
        .create_thread(kiln_store::NewThread {
            project_id: env.project.id,
            owner_id: u.id,
            title: "x".into(),
        })
        .await
        .unwrap();
    let plan = env
        .store
        .create_plan(kiln_store::NewPlan {
            project_id: env.project.id,
            owner_id: u.id,
            thread_id: thread.id,
            spec: s,
        })
        .await
        .unwrap();
    let t = env
        .store
        .create_tasks_for_plan(
            plan.id,
            vec![kiln_store::NewTask {
                spec_id: "t1".into(),
                title: "Task t1".into(),
                description: String::new(),
                acceptance: vec!["works".into()],
                files_hint: vec![],
                depends_on: vec![],
                workers: workers(),
            }],
        )
        .await
        .unwrap()
        .remove(0);
    env.store
        .transition_task(
            t.id,
            TaskState::Queued,
            TaskState::Preparing,
            "admitted",
            None,
        )
        .await
        .unwrap();
    env.sched = env.make_scheduler(env.runtime.clone()).await;
    env.sched.start().await.unwrap();
    env.wait_state(t.id, TaskState::Ready, T).await;
    let tr = env.transitions(t.id).await;
    assert!(
        tr.contains(&(TaskState::Preparing, TaskState::Queued)),
        "{tr:?}"
    );
    env.sched.shutdown().await;
}
