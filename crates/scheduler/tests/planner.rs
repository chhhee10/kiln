//! Planner sessions: thread → planner run → plan v1 + reply; another
//! message → v2; approve → tasks.

mod common;

use common::*;
use kiln_types::api::{
    ApprovePlanRequest, CreateThreadRequest, SendMessageRequest, UpdatePlanRequest,
};
use kiln_types::event::ServerEvent;
use kiln_types::*;
use std::time::Duration;

const T: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_to_plan_versions_to_tasks() {
    let env = Env::new(Opts::default()).await;
    let u = env.member("ada@acme.dev").await;
    let other = env.member("eve@acme.dev").await;
    let d = env
        .sched
        .create_thread(
            &u,
            CreateThreadRequest {
                project_id: env.project.id,
                message: "Add retry backoff to the uploader".into(),
                voice: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(d.thread.title, "Add retry backoff to the uploader");
    assert_eq!(d.messages.len(), 1);
    let thread = d.thread.id;

    env.wait_until("plan v1", T, || async {
        env.store
            .get_plan_for_thread(thread)
            .await
            .unwrap()
            .is_some_and(|p| p.version == 1)
            && env
                .store
                .list_messages(thread)
                .await
                .unwrap()
                .iter()
                .any(|m| m.author == MessageAuthor::Planner)
    })
    .await;
    let plan = env
        .store
        .get_plan_for_thread(thread)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.status, PlanStatus::Drafting);
    assert_eq!(plan.spec.tasks.len(), 2);
    let msgs = env.store.list_messages(thread).await.unwrap();
    let reply = msgs
        .iter()
        .find(|m| m.author == MessageAuthor::Planner)
        .unwrap();
    assert_eq!(reply.plan_version, Some(1));
    assert!(reply.body.contains("plan"), "{}", reply.body);
    let n = env.notifications(u.id).await;
    let pr = n
        .iter()
        .find(|n| n.kind == NotificationKind::PlanReady)
        .expect("PlanReady");
    assert!(pr.title.starts_with("Approve plan: "));
    assert_eq!(pr.plan_id, Some(plan.id));

    // Only the owner may write.
    let e = env
        .sched
        .planner_message(
            &other,
            thread,
            SendMessageRequest {
                body: "hi".into(),
                voice: false,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.status(), 403);

    // Second message → v2.
    env.sched
        .planner_message(
            &u,
            thread,
            SendMessageRequest {
                body: "Also log each retry".into(),
                voice: true,
            },
        )
        .await
        .unwrap();
    env.wait_until("plan v2", T, || async {
        env.store
            .get_plan_for_thread(thread)
            .await
            .unwrap()
            .is_some_and(|p| p.version == 2)
            && env
                .store
                .list_messages(thread)
                .await
                .unwrap()
                .last()
                .is_some_and(|m| m.author == MessageAuthor::Planner)
    })
    .await;
    let msgs = env.store.list_messages(thread).await.unwrap();
    assert_eq!(msgs.last().unwrap().plan_version, Some(2));
    // The second planner run saw the whole conversation and the v1 draft.
    let runs = env.store.list_runs_for_thread(thread).await.unwrap();
    assert_eq!(runs.len(), 2);
    let prompt = env.run_config(runs[1].id).await.prompt;
    assert!(
        prompt.contains("Also log each retry") && prompt.contains("Current plan draft"),
        "{prompt}"
    );

    // Editing with a stale version conflicts.
    let plan = env
        .store
        .get_plan_for_thread(thread)
        .await
        .unwrap()
        .unwrap();
    let e = env
        .sched
        .update_plan(
            &u,
            plan.id,
            UpdatePlanRequest {
                spec: plan.spec.clone(),
                version: 1,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(e.status(), 409);
    let plan = env
        .sched
        .update_plan(
            &u,
            plan.id,
            UpdatePlanRequest {
                spec: plan.spec.clone(),
                version: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(plan.version, 3);

    // Approve → tasks (t2 waits for t1).
    let r = env
        .sched
        .approve_plan(&u, plan.id, ApprovePlanRequest { version: Some(3) })
        .await
        .unwrap();
    assert_eq!(r.tasks.len(), 2);
    assert_eq!(r.plan.status, PlanStatus::Dispatched);
    assert!(
        r.summary
            .starts_with("2 tasks dispatched · 1 building, 1 queued"),
        "{}",
        r.summary
    );
    assert_eq!(
        env.store.get_thread(thread).await.unwrap().status,
        PlanStatus::Dispatched
    );
    let e = env
        .sched
        .approve_plan(&u, plan.id, ApprovePlanRequest::default())
        .await
        .unwrap_err();
    assert_eq!(e.status(), 409);
    // Workers resolved from the defaults.
    assert_eq!(
        r.tasks[0]
            .workers
            .get(&WorkerRole::Builder)
            .unwrap()
            .harness,
        "mock"
    );

    let ev = env.events.lock().unwrap().clone();
    assert!(ev.iter().any(|e| matches!(e, ServerEvent::PlannerStatus { thread_id, pending: true, .. } if *thread_id == thread)));
    assert!(ev.iter().any(|e| matches!(e, ServerEvent::PlannerStatus { thread_id, pending: false, .. } if *thread_id == thread)));
    assert!(ev.iter().any(|e| matches!(e, ServerEvent::Message { message } if message.thread_id == thread && message.author == MessageAuthor::Planner)));
    for t in &r.tasks {
        env.wait_state(t.id, TaskState::Ready, T).await;
    }
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_plan_gets_an_explaining_reply() {
    // The mock planner writes no plan.json with no_output: reply only.
    let env = Env::new(Opts::default().mock("mock.no_output", "1")).await;
    let u = env.member("ada@acme.dev").await;
    let d = env
        .sched
        .create_thread(
            &u,
            CreateThreadRequest {
                project_id: env.project.id,
                message: "Do a thing".into(),
                voice: false,
            },
        )
        .await
        .unwrap();
    env.wait_until("reply", T, || async {
        env.store
            .list_messages(d.thread.id)
            .await
            .unwrap()
            .iter()
            .any(|m| m.author != MessageAuthor::User)
    })
    .await;
    assert!(
        env.store
            .get_plan_for_thread(d.thread.id)
            .await
            .unwrap()
            .is_none()
    );
    env.sched.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_project_parses_qa_yaml() {
    let env = Env::new(Opts::default()).await;
    assert_eq!(env.project.qa_status, QaProfileStatus::Valid);
    let p = env
        .store
        .get_project_qa_profile(env.project.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.checks.unit.as_deref(), Some("cargo test"));
    // Break qa.yaml on the remote's main.
    let work = env.tmp.path().join("w2");
    git(
        env.tmp.path(),
        &[
            "clone",
            "-q",
            env.remote.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    std::fs::write(work.join("qa.yaml"), "version: 9\n").unwrap();
    git(&work, &["commit", "-qam", "break"]);
    git(&work, &["push", "-q", "origin", "main"]);
    let p = env.sched.refresh_project(env.project.id).await.unwrap();
    assert!(
        matches!(p.qa_status, QaProfileStatus::Invalid { ref error } if error.contains("version")),
        "{:?}",
        p.qa_status
    );
    // The last good profile is kept.
    assert!(
        env.store
            .get_project_qa_profile(env.project.id)
            .await
            .unwrap()
            .is_some()
    );
    env.sched.shutdown().await;
}
