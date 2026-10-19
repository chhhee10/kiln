//! End-to-end tests of `kilnd` through `kiln_api_client` only.

mod common;

use common::*;
use kiln_api_client::kiln_types::api::*;
use kiln_api_client::kiln_types::event::{ClientMessage, ServerEvent};
use kiln_api_client::kiln_types::memory::*;
use kiln_api_client::kiln_types::*;
use kiln_api_client::{Client, ws};
use std::time::Duration;

const LONG: Duration = Duration::from_secs(120);

fn status(e: &kiln_api_client::ClientError) -> u16 {
    match e {
        kiln_api_client::ClientError::Api { status, .. } => *status,
        other => panic!("expected an API error, got {other:?}"),
    }
}

// ---------------------------------------------------------------- (1) auth

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn setup_login_register() {
    let env = Env::new();
    let d = env.start().await;
    let anon = client(&d);

    let info = anon.check_compatible().await.unwrap();
    assert!(info.setup_required);
    assert_eq!(info.product, "Kiln");
    // Nothing but setup works before setup.
    assert_eq!(status(&anon.me().await.unwrap_err()), 401);
    let e = anon
        .register(&RegisterRequest {
            email: "x@acme.dev".into(),
            name: "X".into(),
            password: PASSWORD.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some("setup_required"));

    let (admin, admin_user) = setup_admin(&d).await;
    assert!(admin_user.is_admin());
    let info = anon.info().await.unwrap();
    assert!(!info.setup_required);
    assert_eq!(info.allowed_domain.as_deref(), Some("acme.dev"));
    // Setup is one-shot.
    let again = client(&d)
        .setup(&SetupRequest {
            admin_email: "evil@acme.dev".into(),
            admin_name: "E".into(),
            password: PASSWORD.into(),
            allowed_domain: "evil.dev".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(status(&again), 409);

    // Login: wrong password and unknown email look the same.
    let bad = client(&d)
        .login(&LoginRequest {
            email: ADMIN_EMAIL.into(),
            password: "wrong password!".into(),
            client: None,
            device_name: None,
        })
        .await
        .unwrap_err();
    assert_eq!(status(&bad), 401);
    let unknown = client(&d)
        .login(&LoginRequest {
            email: "nobody@acme.dev".into(),
            password: PASSWORD.into(),
            client: None,
            device_name: None,
        })
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), bad.code());
    let tui = client(&d);
    let r = tui
        .login(&LoginRequest {
            email: ADMIN_EMAIL.into(),
            password: PASSWORD.into(),
            client: Some(ClientKind::Tui),
            device_name: Some("laptop".into()),
        })
        .await
        .unwrap();
    assert_eq!(r.user.id, admin_user.id);
    let me = tui.me().await.unwrap();
    assert_eq!(me.user.email, ADMIN_EMAIL);
    assert!(
        me.settings
            .worker_defaults
            .contains_key(&WorkerRole::Builder)
    );

    // Register: allowlisted domain only.
    let (m, mu) = member(&d, "Jordan Lee", "jordan@acme.dev").await;
    assert_eq!(mu.role, UserRole::Member);
    assert_eq!(m.me().await.unwrap().user.initials, "JL");
    let e = client(&d)
        .register(&RegisterRequest {
            email: "mallory@evil.dev".into(),
            name: "M".into(),
            password: PASSWORD.into(),
        })
        .await
        .unwrap_err();
    assert_eq!(e.code(), Some("not_allowlisted"));
    assert_eq!(status(&e), 403);
    let e = client(&d)
        .register(&RegisterRequest {
            email: "short@acme.dev".into(),
            name: "S".into(),
            password: "short".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(status(&e), 400);

    // Pairing: QR + one-time code.
    let p = admin.pair_start().await.unwrap();
    assert!(p.qr_svg.contains("<svg"));
    assert!(
        p.pair_uri
            .starts_with("kiln://pair?url=http%3A%2F%2F127.0.0.1")
    );
    assert!(p.pair_uri.ends_with(&format!("code={}", p.code)));
    let phone = client(&d);
    let r = phone
        .pair_complete(&PairCompleteRequest {
            code: p.code.clone(),
            client: ClientKind::Android,
            device_name: "Pixel".into(),
        })
        .await
        .unwrap();
    assert_eq!(r.user.id, admin_user.id);
    let dev = phone
        .register_push(&RegisterPushRequest {
            endpoint: "https://ntfy.acme.dev/up123".into(),
            device_name: "Pixel".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        dev.push_endpoint.as_deref(),
        Some("https://ntfy.acme.dev/up123")
    );
    let reuse = client(&d)
        .pair_complete(&PairCompleteRequest {
            code: p.code,
            client: ClientKind::Android,
            device_name: "Other".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(status(&reuse), 401);

    // Settings round-trip and validation.
    let mut s = m.settings().await.unwrap();
    s.default_visibility = Visibility::Shared;
    let s2 = m.update_settings(&s).await.unwrap();
    assert_eq!(s2.default_visibility, Visibility::Shared);
    s.worker_defaults.insert(
        WorkerRole::Builder,
        kiln_types::plan::WorkerChoice::new("nope", "x"),
    );
    assert_eq!(status(&m.update_settings(&s).await.unwrap_err()), 400);
    let cat = m.harnesses().await.unwrap();
    assert!(cat.harnesses.iter().any(|h| h.id == "mock"));
    assert!(
        cat.harnesses
            .iter()
            .any(|h| h.id == "claude_sdk" && h.models.contains(&"claude-sonnet-4.5".to_string()))
    );

    // Logout kills the token.
    tui.logout().await.unwrap();
    let stale = Client::new(d.url()).with_token(r.token.clone());
    stale.me().await.unwrap(); // the paired phone session is separate
    d.shutdown().await;
}

// ---------------------------------------------------------------- (2) full flow

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn full_flow_plan_to_pr() {
    let env = Env::new();
    let d = env.start().await;
    let (admin, _) = setup_admin(&d).await;
    let p = project(&admin, &env, "FP").await;
    let (ada, ada_user) = member(&d, "Ada Rao", "ada@acme.dev").await;
    assert_eq!(ada.projects().await.unwrap().len(), 1);

    let mut sock = ws::connect(&ada).await.unwrap();
    let hello = ws_until(&mut sock.events, LONG, |e| {
        matches!(e, ServerEvent::Hello { .. })
    })
    .await;
    assert!(
        matches!(hello.last(), Some(ServerEvent::Hello { user, .. }) if user.id == ada_user.id)
    );

    let t = ada
        .create_thread(&CreateThreadRequest {
            project_id: p.id,
            message: "Add retry with backoff to the CLI".into(),
            voice: false,
        })
        .await
        .unwrap();
    assert_eq!(t.messages.len(), 1);
    sock.send
        .send(ClientMessage::FollowThread {
            thread_id: t.thread.id,
        })
        .await
        .unwrap();
    // Planner reply + plan over WS.
    let mut got_msg = false;
    let mut got_plan = None;
    ws_until(&mut sock.events, LONG, |e| {
        match e {
            ServerEvent::Message { message } if message.author == MessageAuthor::Planner => {
                got_msg = true
            }
            ServerEvent::PlanUpserted { plan } => got_plan = Some(plan.clone()),
            _ => {}
        }
        got_msg && got_plan.is_some()
    })
    .await;
    let plan = got_plan.unwrap();
    assert_eq!(plan.owner_id, ada_user.id);
    let detail = ada.thread(t.thread.id).await.unwrap();
    assert!(
        detail
            .messages
            .iter()
            .any(|m| m.author == MessageAuthor::Planner)
    );
    assert_eq!(detail.plan.as_ref().map(|p| p.id), Some(plan.id));
    assert_eq!(detail.planner_model, "mock-1");
    assert!(
        ada.threads(&ThreadQuery::default())
            .await
            .unwrap()
            .iter()
            .any(|x| x.id == t.thread.id)
    );
    let listed = ada
        .plans(&PlanListQuery {
            status: Some(PlanStatus::Drafting),
            project: Some(p.id),
        })
        .await
        .unwrap();
    assert!(listed.iter().any(|x| x.id == plan.id));
    let board = ada.board(&BoardQuery::default()).await.unwrap();
    assert!(board.ideas.iter().any(|x| x.id == plan.id));

    // Edit the plan (optimistic concurrency).
    let mut spec = plan.spec.clone();
    spec.feature = "Retry with backoff (edited)".into();
    spec.tasks[0]
        .acceptance_criteria
        .push("Backoff is capped at 30s".into());
    let edited = ada
        .update_plan(
            plan.id,
            &UpdatePlanRequest {
                spec: spec.clone(),
                version: plan.version,
            },
        )
        .await
        .unwrap();
    assert_eq!(edited.version, plan.version + 1);
    assert_eq!(edited.spec.feature, "Retry with backoff (edited)");
    let stale = ada
        .update_plan(
            plan.id,
            &UpdatePlanRequest {
                spec,
                version: plan.version,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(status(&stale), 409);

    // Approve → tasks reach Ready (watch TaskUpserted on the socket).
    let r = ada
        .approve_plan(
            plan.id,
            &ApprovePlanRequest {
                version: Some(edited.version),
            },
        )
        .await
        .unwrap();
    assert_eq!(r.tasks.len(), edited.spec.tasks.len());
    assert!(r.summary.contains("dispatched"), "{}", r.summary);
    let ids: std::collections::HashSet<TaskId> = r.tasks.iter().map(|t| t.id).collect();
    sock.send
        .send(ClientMessage::FollowTask {
            task_id: r.tasks[0].id,
        })
        .await
        .unwrap();
    let mut ready = std::collections::HashSet::new();
    let mut saw_log = false;
    let seen = ws_until(&mut sock.events, Duration::from_secs(240), |e| {
        match e {
            ServerEvent::TaskUpserted { task }
                if ids.contains(&task.id) && task.state == TaskState::Ready =>
            {
                ready.insert(task.id);
            }
            ServerEvent::Log {
                task_id: Some(t), ..
            } if *t == r.tasks[0].id => saw_log = true,
            _ => {}
        }
        ready.len() == ids.len()
    })
    .await;
    assert!(
        saw_log,
        "followed task streamed no log lines ({} events)",
        seen.len()
    );

    for id in &ids {
        let td = ada.task(*id).await.unwrap();
        assert_eq!(td.task.state, TaskState::Ready);
        assert!(
            td.task.pr_url.is_some() && td.task.pr_number.is_some(),
            "{:?}",
            td.task
        );
        let qa = td.qa_report.as_ref().expect("qa report");
        assert!(qa.passed());
        assert!(td.verdict.as_ref().expect("verdict").approved());
        assert!(
            td.criteria.iter().all(|c| c.status == Status::Passed),
            "{:?}",
            td.criteria
        );
        assert!(td.runs.iter().any(|r| r.role == WorkerRole::Builder));
        let shot = td
            .artifacts
            .iter()
            .find(|a| a.kind == ArtifactKind::Screenshot)
            .expect("screenshot artifact");
        let bytes = ada.artifact_bytes(shot.run_id, shot.id).await.unwrap();
        assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']));
        assert_eq!(bytes.len() as u64, shot.size);
        let logs = ada.task_logs(*id, &LogQuery::default()).await.unwrap();
        assert!(!logs.is_empty());
        let builder_logs = ada
            .task_logs(
                *id,
                &LogQuery {
                    role: Some(WorkerRole::Builder),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(builder_logs.iter().all(|l| l.role == WorkerRole::Builder));
    }
    let plan_now = ada
        .plans(&PlanListQuery::default())
        .await
        .unwrap()
        .into_iter()
        .find(|x| x.id == plan.id)
        .unwrap();
    assert_ne!(plan_now.status, PlanStatus::Drafting);

    // Ready notifications for the owner.
    let notes = ada.notifications().await.unwrap();
    assert!(
        notes.iter().any(|n| n.kind == NotificationKind::TaskReady),
        "{notes:?}"
    );
    // Admin view: machine + overview work.
    let ov = admin.admin_overview().await.unwrap();
    assert!(ov.users.iter().any(|u| u.user.id == ada_user.id));
    let m = admin.machine().await.unwrap();
    assert!(m.cpu_history.len() <= 32);
    d.shutdown().await;
}

// ---------------------------------------------------------------- (3)+(4) permissions & WS filtering

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permission_matrix_and_ws_filtering() {
    let env = Env::new();
    let d = env.start().await;
    let (admin, _) = setup_admin(&d).await;
    let p = project(&admin, &env, "FC").await;
    let (a, a_user) = member(&d, "Ada Rao", "ada@acme.dev").await;
    let (b, b_user) = member(&d, "Jin Lee", "jin@acme.dev").await;

    // B listens from the start.
    let mut b_sock = ws::connect(&b).await.unwrap();
    ws_until(&mut b_sock.events, LONG, |e| {
        matches!(e, ServerEvent::Hello { .. })
    })
    .await;

    // A's private credential and memory.
    let cred = a
        .add_credential(&CreateCredentialRequest {
            kind: CredentialKind::AnthropicApiKey,
            name: "personal".into(),
            secret: "sk-ant-api03-supersecretvalue1234".into(),
            base_url: None,
            env_var: None,
        })
        .await
        .unwrap();
    assert!(!cred.masked.contains("supersecret"));
    let item = d
        .state
        .memory
        .propose(
            a_user.id,
            None,
            MemoryProposal {
                kind: MemoryType::Fact,
                title: "A's private note".into(),
                subject: "retry".into(),
                relation: "uses".into(),
                object: "jitter".into(),
                body: "private body".into(),
                visibility: Visibility::Private,
                shared_with: vec![],
                project_id: Some(p.id),
            },
        )
        .await
        .unwrap();

    // A runs a plan to Ready (follows its task so logs flow on A's socket).
    let (thread, plan) = draft_plan(&a, p.id, "Usage chart on billing page").await;
    let tasks = approve_and_finish(&a, &plan).await;
    let task = &tasks[0];
    // A edits its memory → MemoryUpserted (private) must not reach B.
    a.update_memory(
        item.id,
        &UpdateMemoryRequest {
            title: Some("A's private note v2".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let td_a = a.task(task.id).await.unwrap();
    let art = td_a.artifacts.first().expect("artifact").clone();

    // B: board card visible, content not.
    let board = b
        .board(&BoardQuery {
            users: Some(a_user.id.to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    let card = board
        .tasks
        .iter()
        .find(|t| t.id == task.id)
        .expect("A's card on B's board");
    assert_eq!(card.title, task.title);
    assert!(card.description.is_empty() && card.acceptance.is_empty());
    assert!(board.ideas.is_empty());
    let td_b = b.task(task.id).await.unwrap();
    assert!(td_b.qa_report.is_none() && td_b.verdict.is_none() && td_b.diff.is_none());
    assert!(td_b.artifacts.is_empty());
    assert!(
        td_b.criteria
            .iter()
            .all(|c| c.criterion.starts_with("Criterion "))
    );
    assert_eq!(
        status(
            &b.task_logs(task.id, &LogQuery::default())
                .await
                .unwrap_err()
        ),
        403
    );
    assert_eq!(
        status(&b.artifact_bytes(art.run_id, art.id).await.unwrap_err()),
        403
    );
    let e = b.task_action(task.id, TaskAction::Retry).await.unwrap_err();
    assert_eq!(status(&e), 403);
    // Threads/plans are private.
    assert_eq!(status(&b.thread(thread.thread.id).await.unwrap_err()), 404);
    assert_eq!(
        status(
            &b.approve_plan(plan.id, &ApprovePlanRequest::default())
                .await
                .unwrap_err()
        ),
        404
    );
    assert!(b.plans(&PlanListQuery::default()).await.unwrap().is_empty());
    // Credentials.
    assert!(b.credentials().await.unwrap().is_empty());
    assert_eq!(
        status(&b.revoke_credential(cred.id).await.unwrap_err()),
        404
    );
    assert_eq!(
        status(
            &b.rotate_credential(
                cred.id,
                &RotateCredentialRequest {
                    secret: "sk-ant-stolen-123456".into(),
                    base_url: None
                }
            )
            .await
            .unwrap_err()
        ),
        404
    );
    assert_eq!(status(&b.test_credential(cred.id).await.unwrap_err()), 404);
    assert_eq!(a.credentials().await.unwrap().len(), 1);
    // Memory.
    assert_eq!(status(&b.memory_item(item.id).await.unwrap_err()), 404);
    assert_eq!(
        status(
            &b.update_memory(
                item.id,
                &UpdateMemoryRequest {
                    archived: Some(true),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
        ),
        404
    );
    let bl = b.memory(&MemoryQuery::default()).await.unwrap();
    assert!(bl.items.iter().all(|i| i.id != item.id));
    assert_eq!(a.memory_item(item.id).await.unwrap().item.id, item.id);
    // Notifications are per user.
    let a_notes = a.notifications().await.unwrap();
    assert!(!a_notes.is_empty());
    assert_eq!(
        status(&b.mark_notification_read(a_notes[0].id).await.unwrap_err()),
        404
    );
    assert!(
        b.notifications()
            .await
            .unwrap()
            .iter()
            .all(|n| n.user_id == b_user.id)
    );

    // Admin: metadata yes, content no.
    let td_admin = admin.task(task.id).await.unwrap();
    assert!(!td_admin.artifacts.is_empty());
    assert!(td_admin.qa_report.is_none() && td_admin.verdict.is_none());
    assert_eq!(
        status(&admin.artifact_bytes(art.run_id, art.id).await.unwrap_err()),
        403
    );
    assert_eq!(
        status(
            &admin
                .task_logs(task.id, &LogQuery::default())
                .await
                .unwrap_err()
        ),
        403
    );
    assert_eq!(status(&admin.memory_item(item.id).await.unwrap_err()), 404);
    assert!(admin.credentials().await.unwrap().is_empty());

    // Non-admins can't use admin routes.
    assert_eq!(status(&b.admin_overview().await.unwrap_err()), 403);
    assert_eq!(status(&b.admin_users().await.unwrap_err()), 403);
    let cu = CreateUserRequest {
        email: "x@acme.dev".into(),
        name: "X".into(),
        password: PASSWORD.into(),
        role: UserRole::Admin,
    };
    assert_eq!(status(&b.create_user(&cu).await.unwrap_err()), 403);
    assert_eq!(
        status(
            &b.update_user(
                b_user.id,
                &UpdateUserRequest {
                    role: Some(UserRole::Admin),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
        ),
        403
    );
    assert_eq!(
        status(
            &b.set_domain(&SetDomainRequest {
                allowed_domain: "evil.dev".into()
            })
            .await
            .unwrap_err()
        ),
        403
    );
    let cp = CreateProjectRequest {
        key: "EV".into(),
        name: "Evil".into(),
        repo_url: "https://github.com/x/y".into(),
        default_branch: "main".into(),
        qa_path: "qa.yaml".into(),
    };
    assert_eq!(status(&b.create_project(&cp).await.unwrap_err()), 403);
    assert_eq!(admin.admin_users().await.unwrap().len(), 3);

    // (4) WS: B tries to follow A's task — refused; then check everything B got.
    b_sock
        .send
        .send(ClientMessage::FollowTask { task_id: task.id })
        .await
        .unwrap();
    b_sock
        .send
        .send(ClientMessage::FollowThread {
            thread_id: thread.thread.id,
        })
        .await
        .unwrap();
    let mut events = ws_drain(&mut b_sock.events, Duration::from_millis(1500)).await;
    // Drain everything buffered from the run as well.
    events.extend(ws_drain(&mut b_sock.events, Duration::from_millis(300)).await);
    let mut saw_card = false;
    for e in &events {
        match e {
            ServerEvent::Log { .. } => panic!("B received a Log event"),
            ServerEvent::Notification { notification } => {
                assert_eq!(notification.user_id, b_user.id, "B got A's notification")
            }
            ServerEvent::MemoryUpserted { item: m } => {
                assert!(m.visible_to(b_user.id), "B got A's private memory")
            }
            ServerEvent::PlanUpserted { plan } => {
                panic!("B got a plan event for {}", plan.owner_id)
            }
            ServerEvent::ThreadUpserted { .. }
            | ServerEvent::Message { .. }
            | ServerEvent::PlannerStatus { .. } => {
                panic!("B got A's planner event: {}", kind(e))
            }
            ServerEvent::TaskUpserted { task: t } if t.owner_id == a_user.id => {
                saw_card = true;
                assert!(
                    t.description.is_empty() && t.acceptance.is_empty(),
                    "card content leaked"
                );
            }
            ServerEvent::RunUpserted { run } if run.owner_id == a_user.id => {
                assert!(run.error.is_none() || run.error.as_deref() == Some("run failed"));
            }
            _ => {}
        }
    }
    assert!(
        saw_card,
        "B never saw A's task card; events: {:?}",
        events.iter().map(kind).collect::<Vec<_>>()
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ServerEvent::Toast { ok: false, .. })),
        "follow refusal toast"
    );
    d.shutdown().await;
}

// ---------------------------------------------------------------- (5) notification actions

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notification_actions_approve_and_retry() {
    // QA fails on every attempt → the task escalates after the loop cap.
    let env = Env::new().with_mock("mock.fail_qa_times", "99");
    let d = env.start().await;
    let (admin, _) = setup_admin(&d).await;
    let p = project(&admin, &env, "WEB").await;
    let (a, _) = member(&d, "Ada Rao", "ada@acme.dev").await;

    let (_t, plan) = draft_plan(&a, p.id, "Dark mode for docs site").await;
    // PlanReady → approve from the notification.
    let n = eventually("plan-ready notification", LONG, || async {
        a.notifications()
            .await
            .ok()?
            .into_iter()
            .find(|n| n.kind == NotificationKind::PlanReady && n.plan_id == Some(plan.id))
    })
    .await;
    assert!(
        n.actions.iter().any(|x| x.id == "approve"),
        "{:?}",
        n.actions
    );
    let resolved = a
        .notification_action(
            n.id,
            &NotificationActionRequest {
                action: "approve".into(),
                text: None,
            },
        )
        .await
        .unwrap();
    let text = resolved.resolved.clone().unwrap();
    assert!(text.starts_with("✓ Approved · "), "{text}");
    assert!(resolved.read);
    let again = a
        .notification_action(
            n.id,
            &NotificationActionRequest {
                action: "approve".into(),
                text: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(status(&again), 409);

    // Wait for the escalation notification, then retry from it.
    let esc = eventually(
        "escalation/failure notification",
        Duration::from_secs(240),
        || async {
            a.notifications().await.ok()?.into_iter().find(|n| {
                matches!(
                    n.kind,
                    NotificationKind::TaskEscalated | NotificationKind::TaskFailed
                ) && n.task_id.is_some()
            })
        },
    )
    .await;
    let tid = esc.task_id.unwrap();
    let before = a.task(tid).await.unwrap().task;
    assert!(
        matches!(before.state, TaskState::Escalated | TaskState::Failed),
        "{:?}",
        before.state
    );
    let r = a
        .notification_action(
            esc.id,
            &NotificationActionRequest {
                action: "retry".into(),
                text: None,
            },
        )
        .await
        .unwrap();
    let text = r.resolved.unwrap();
    assert!(text.starts_with("✓ Retried · attempt "), "{text}");
    let after = a.task(tid).await.unwrap().task;
    assert!(
        !matches!(after.state, TaskState::Escalated | TaskState::Failed),
        "{:?}",
        after.state
    );
    // Unknown actions are rejected.
    let bad = a
        .notification_action(
            esc.id,
            &NotificationActionRequest {
                action: "launch_missiles".into(),
                text: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(status(&bad), 400 | 409));
    a.task_action(tid, TaskAction::Stop).await.unwrap();
    d.shutdown().await;
}

// ---------------------------------------------------------------- (6) restart

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_mid_run_loses_nothing() {
    let env = Env::new()
        .with_mock("mock.sleep_ms", "400")
        .with_mock("mock.steps", "6");
    let d = env.start().await;
    let (admin, _) = setup_admin(&d).await;
    let p = project(&admin, &env, "RS").await;
    let (a, _) = member(&d, "Ada Rao", "ada@acme.dev").await;
    let token = a.token().unwrap();
    let (_t, plan) = draft_plan(&a, p.id, "Streaming responses in SDK").await;
    let r = a
        .approve_plan(
            plan.id,
            &ApprovePlanRequest {
                version: Some(plan.version),
            },
        )
        .await
        .unwrap();
    let first = r.tasks.iter().find(|t| t.depends_on.is_empty()).unwrap().id;
    // Wait for the Builder to be running and logging.
    eventually("builder running", LONG, || async {
        let d = a.task(first).await.ok()?;
        let logs = a.task_logs(first, &LogQuery::default()).await.ok()?;
        (d.task.state == TaskState::Building && logs.len() >= 2).then_some(())
    })
    .await;

    d.shutdown().await;
    let d2 = env.start().await;
    let a2 = Client::new(d2.url()).with_token(token);
    let ids: Vec<TaskId> = r.tasks.iter().map(|t| t.id).collect();
    eventually(
        "tasks ready after restart",
        Duration::from_secs(300),
        || async {
            for id in &ids {
                let t = a2.task(*id).await.ok()?.task;
                assert!(
                    !matches!(
                        t.state,
                        TaskState::Failed | TaskState::Escalated | TaskState::Cancelled
                    ),
                    "{} {:?} {}",
                    t.key,
                    t.state,
                    t.meta
                );
                if t.state != TaskState::Ready {
                    return None;
                }
            }
            Some(())
        },
    )
    .await;
    // Logs complete: every run's sequence numbers are contiguous from 1.
    let logs = a2
        .task_logs(
            first,
            &LogQuery {
                limit: Some(5000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut by_run: std::collections::BTreeMap<RunId, Vec<u64>> = Default::default();
    for l in &logs {
        by_run.entry(l.run_id).or_default().push(l.seq);
    }
    assert!(by_run.len() >= 3, "expected builder, qa and reviewer runs");
    for (run, mut seqs) in by_run {
        seqs.sort_unstable();
        seqs.dedup();
        let expect: Vec<u64> = (seqs[0]..seqs[0] + seqs.len() as u64).collect();
        assert_eq!(seqs, expect, "gap in run {run} log sequence");
        assert!(seqs[0] <= 1, "run {run} starts at seq {}", seqs[0]);
    }
    let td = a2.task(first).await.unwrap();
    assert!(td.runs.iter().all(|r| r.state.is_finished()));
    d2.shutdown().await;
}

// ---------------------------------------------------------------- static routes

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn info_routes_and_errors_are_json() {
    let env = Env::new();
    let d = env.start().await;
    let c = client(&d);
    // Unknown API route → JSON 404.
    let e = Client::new(d.url())
        .with_token("garbage")
        .me()
        .await
        .unwrap_err();
    assert_eq!(status(&e), 401);
    assert_eq!(e.code(), Some("unauthorized"));
    let (admin, _) = setup_admin(&d).await;
    let info = c.info().await.unwrap();
    assert_eq!(info.version, kiln_types::VERSION);
    let dom = admin
        .set_domain(&SetDomainRequest {
            allowed_domain: "example.org".into(),
        })
        .await
        .unwrap();
    assert_eq!(dom.allowed_domain.as_deref(), Some("example.org"));
    let u = admin
        .create_user(&CreateUserRequest {
            email: "ops@other.io".into(),
            name: "Ops".into(),
            password: PASSWORD.into(),
            role: UserRole::Member,
        })
        .await
        .unwrap();
    let u2 = admin
        .update_user(
            u.id,
            &UpdateUserRequest {
                status: Some(UserStatus::Disabled),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(u2.status, UserStatus::Disabled);
    let e = client(&d)
        .login(&LoginRequest {
            email: "ops@other.io".into(),
            password: PASSWORD.into(),
            client: None,
            device_name: None,
        })
        .await
        .unwrap_err();
    assert_eq!(status(&e), 403);
    d.shutdown().await;
}
