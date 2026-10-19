//! Admission control & fair-share: capacity for 2 containers, 3 users.

mod common;

use common::*;
use kiln_store::TaskFilter;
use kiln_types::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capacity_fair_share_and_user_caps() {
    let env = Env::new(Opts {
        capacity: kiln_scheduler::Capacity {
            ram_mb: 64 * 1024,
            reserve_mb: 0,
            max_containers: 2,
            max_tasks: 2,
        },
        ..Opts::default().mock("mock.sleep_ms", "100")
    })
    .await;
    let limits = UserLimits {
        concurrency: 1,
        ram_share: 1.0,
        monthly_budget_usd: 10.0,
    };
    let users = [
        env.user("a@acme.dev", UserRole::Member, limits.clone())
            .await,
        env.user("b@acme.dev", UserRole::Member, limits.clone())
            .await,
        env.user("c@acme.dev", UserRole::Member, limits.clone())
            .await,
    ];
    // Queue everything while no scheduler runs, so the FIFO order
    // (a, a, b, b, c, c) is fixed before the first pass.
    env.sched.shutdown().await;
    let mut all = Vec::new();
    for u in &users {
        let s = spec(&[("t1", &[]), ("t2", &[])]);
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
                spec: s.clone(),
            })
            .await
            .unwrap();
        let new = s
            .tasks
            .iter()
            .map(|t| kiln_store::NewTask {
                spec_id: t.id.clone(),
                title: t.title.clone(),
                description: t.description.clone(),
                acceptance: t.acceptance_criteria.clone(),
                files_hint: vec![],
                depends_on: vec![],
                workers: workers(),
            })
            .collect();
        all.extend(env.store.create_tasks_for_plan(plan.id, new).await.unwrap());
        env.store
            .set_plan_status(plan.id, PlanStatus::Dispatched)
            .await
            .unwrap();
    }

    // Sample concurrency while the real scheduler runs.
    let max_live = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let (store, max_live, stop) = (env.store.clone(), max_live.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let n = store
                    .list_runs_in_states(&[RunState::Starting, RunState::Running])
                    .await
                    .unwrap()
                    .len() as u64;
                max_live.fetch_max(n, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
    }
    let sched = env.make_scheduler(env.runtime.clone()).await;
    sched.start().await.unwrap();
    for t in &all {
        env.wait_state(t.id, TaskState::Ready, Duration::from_secs(120))
            .await;
    }
    stop.store(true, Ordering::Relaxed);
    assert!(
        max_live.load(Ordering::Relaxed) <= 2,
        "max concurrent containers {}",
        max_live.load(Ordering::Relaxed)
    );
    assert!(max_live.load(Ordering::Relaxed) >= 1);

    // Admission order and per-user intervals from the persisted transitions.
    let mut admits = Vec::new(); // (at, owner)
    let mut intervals: HashMap<UserId, Vec<(String, String)>> = HashMap::new();
    for t in env.store.list_tasks(TaskFilter::default()).await.unwrap() {
        let tr = env.store.list_transitions(t.id).await.unwrap();
        let start = tr.iter().find(|x| x.to == TaskState::Preparing).unwrap().at;
        let end = tr.iter().find(|x| x.to == TaskState::Ready).unwrap().at;
        admits.push((start, t.owner_id));
        intervals
            .entry(t.owner_id)
            .or_default()
            .push((start.to_rfc3339(), end.to_rfc3339()));
    }
    admits.sort();
    // Fair share: the first three admissions are three different users
    // (FIFO alone would have admitted a's two tasks first).
    let first: std::collections::HashSet<UserId> = admits.iter().take(3).map(|a| a.1).collect();
    assert_eq!(first.len(), 3, "round-robin across users: {admits:?}");
    // Per-user concurrency cap of 1: a user's tasks never overlap.
    for (u, mut iv) in intervals {
        iv.sort();
        for w in iv.windows(2) {
            assert!(w[0].1 <= w[1].0, "user {u} had two tasks in flight: {iv:?}");
        }
    }
    // Never more than max_tasks in flight at once.
    let mut points: Vec<(chrono::DateTime<chrono::Utc>, i32)> = Vec::new();
    for t in env.store.list_tasks(TaskFilter::default()).await.unwrap() {
        let tr = env.store.list_transitions(t.id).await.unwrap();
        points.push((
            tr.iter().find(|x| x.to == TaskState::Preparing).unwrap().at,
            1,
        ));
        points.push((tr.iter().find(|x| x.to == TaskState::Ready).unwrap().at, -1));
    }
    points.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut cur = 0;
    for (_, d) in points {
        cur += d;
        assert!(cur <= 2, "more than max_tasks in flight");
    }
    let q = sched.queue().await.unwrap();
    assert!(q.is_empty());
    sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_and_machine_views() {
    let env = Env::new(Opts {
        capacity: kiln_scheduler::Capacity {
            ram_mb: 64 * 1024,
            reserve_mb: 0,
            max_containers: 1,
            max_tasks: 1,
        },
        ..Opts::default()
            .mock("mock.sleep_ms", "300")
            .mock("mock.steps", "3")
    })
    .await;
    let u = env.member("a@acme.dev").await;
    let (_, tasks) = env
        .dispatch(&u, spec(&[("t1", &[]), ("t2", &[]), ("t3", &[])]))
        .await;
    env.wait_until("a running container", Duration::from_secs(30), || async {
        !env.sched.running_containers().await.unwrap().is_empty()
    })
    .await;
    let rc = env.sched.running_containers().await.unwrap();
    assert_eq!(rc.len(), 1);
    assert_eq!(rc[0].owner.id, u.id);
    let q = env.sched.queue().await.unwrap();
    assert_eq!(q.len(), 2);
    assert_eq!(q[0].position, 1);
    assert_eq!(q[0].task_id, tasks[1].id);
    env.wait_until("machine sample", Duration::from_secs(10), || async {
        env.sched.machine().ram_total_gb > 0.0
    })
    .await;
    let m = env.sched.machine();
    assert_eq!(m.container_slots, 1);
    assert!(!m.cpu_history.is_empty());
    assert!(m.runtime_ok);
    env.sched.shutdown().await;
}
