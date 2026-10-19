//! End-to-end: fake daemon + real `kiln-runner run` + mock harness.

mod common;

use common::*;
use kiln_types::WorkerRole;
use kiln_types::event::RunEventKind;
use kiln_types::model::ArtifactKind;
use kiln_types::protocol::DaemonMessage;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread")]
async fn builder_commits_and_reports_head() {
    let d = dirs();
    init_repo(&d.ws);
    let cfg = config(WorkerRole::Builder, &d.ws);
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let s = daemon.state.lock().unwrap();
    let o = s.outcome.clone().expect("Complete received");
    assert!(o.success, "{o:?}");
    assert_eq!(
        o.commit.as_deref(),
        Some(git(&d.ws, &["rev-parse", "HEAD"]).as_str())
    );
    assert_eq!(
        std::fs::read_to_string(d.ws.join("KILN_MOCK.md")).unwrap(),
        "- FP-1 attempt 1\n"
    );
    assert_eq!(
        git(&d.ws, &["log", "-1", "--format=%an <%ae>|%s"]),
        "Kiln Builder <builder@kiln.local>|mock: FP-1 attempt 1"
    );
    assert_eq!(git(&d.ws, &["status", "--porcelain"]), "");
    assert_contiguous(&s.events);
    assert_eq!(o.last_seq, *s.events.keys().last().unwrap());
    assert!(o.usage.tokens_in > 0);
    assert!(
        s.events
            .values()
            .any(|e| matches!(e.kind, RunEventKind::Step { .. }))
    );
    assert!(
        s.events
            .values()
            .any(|e| matches!(e.kind, RunEventKind::Progress { percent: 100, .. }))
    );
    // Spool holds the same events; outcome.json mirrors Complete.
    let spool = std::fs::read_to_string(d.run.join("spool.jsonl")).unwrap();
    assert_eq!(spool.lines().count(), s.events.len());
    assert_eq!(read_outcome_file(&d), o);
}

#[tokio::test(flavor = "multi_thread")]
async fn builder_leftovers_are_committed_by_runner() {
    let d = dirs();
    init_repo(&d.ws);
    let mut cfg = config(WorkerRole::Builder, &d.ws);
    cfg.attempt = 2;
    cfg.options
        .insert("mock.leave_uncommitted".into(), "1".into());
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    assert!(o.success);
    assert_eq!(
        git(&d.ws, &["log", "-1", "--format=%an <%ae>|%s"]),
        "Kiln Builder <builder@kiln.local>|kiln: FP-1 attempt 2"
    );
    assert_eq!(o.commit.unwrap(), git(&d.ws, &["rev-parse", "HEAD"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn builder_without_commits_fails() {
    let d = dirs();
    init_repo(&d.ws);
    let mut cfg = config(WorkerRole::Builder, &d.ws);
    cfg.options.insert("mock.no_output".into(), "1".into());
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    assert!(!o.success);
    assert!(o.error.unwrap().contains("no commits"));
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_harness_fails() {
    let d = dirs();
    let mut cfg = config(WorkerRole::Qa, &d.ws);
    cfg.harness = "does-not-exist".into();
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    assert!(!o.success);
    assert!(o.error.unwrap().contains("unknown harness"));
}

#[tokio::test(flavor = "multi_thread")]
async fn qa_fails_first_attempt_then_passes_with_screenshot() {
    for (attempt, expect_pass) in [(1, false), (2, true)] {
        let d = dirs();
        let mut cfg = config(WorkerRole::Qa, &d.ws);
        cfg.attempt = attempt;
        cfg.options.insert("mock.fail_qa_times".into(), "1".into());
        let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
        let r = run_runner(&d, &cfg, &[], TOKEN).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        let s = daemon.state.lock().unwrap();
        let o = s.outcome.clone().unwrap();
        assert!(o.success, "the QA run itself completed: {o:?}");
        let report = o.qa_report.expect("qa report");
        assert_eq!(report.passed(), expect_pass, "attempt {attempt}");
        assert!(
            o.summary.contains(if expect_pass {
                "QA passed"
            } else {
                "QA failed"
            }),
            "{}",
            o.summary
        );
        let shot = format!("out/artifacts/mock-screenshot-attempt-{attempt}.png");
        assert!(
            s.artifacts
                .iter()
                .any(|(p, k, size)| *p == shot && *k == ArtifactKind::Screenshot && *size > 0),
            "{:?}",
            s.artifacts
        );
        assert!(
            s.artifacts
                .iter()
                .any(|(p, k, _)| p == "out/qa_report.json" && *k == ArtifactKind::QaReport)
        );
        assert!(
            s.events
                .values()
                .any(|e| matches!(&e.kind, RunEventKind::Artifact { path, .. } if *path == shot))
        );
        assert_contiguous(&s.events);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn qa_missing_report_is_a_failure() {
    let d = dirs();
    let mut cfg = config(WorkerRole::Qa, &d.ws);
    cfg.options.insert("mock.no_output".into(), "1".into());
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    assert!(!o.success);
    assert!(
        o.error.as_deref().unwrap().contains("qa_report.json"),
        "{o:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn qa_invalid_report_is_a_failure() {
    let d = dirs();
    let mut cfg = config(WorkerRole::Qa, &d.ws);
    cfg.options.insert("mock.no_output".into(), "1".into());
    std::fs::write(d.run.join("out/qa_report.json"), "{\"criteria\": 7}").unwrap();
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    assert!(
        o.error
            .as_deref()
            .unwrap()
            .contains("invalid qa_report.json"),
        "{o:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_verdicts() {
    for (attempt, approved) in [(1, false), (2, true)] {
        let d = dirs();
        let mut cfg = config(WorkerRole::Reviewer, &d.ws);
        cfg.attempt = attempt;
        cfg.options
            .insert("mock.fail_review_times".into(), "1".into());
        let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
        let r = run_runner(&d, &cfg, &[], TOKEN).await;
        assert_eq!(r.code, 0, "{}", r.stderr);
        let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
        let v = o.verdict.expect("verdict");
        assert_eq!(v.approved(), approved);
        if !approved {
            assert!(!v.findings.is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_writes_plan_and_reply() {
    let d = dirs();
    let cfg = config(WorkerRole::Planner, &d.ws);
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    let plan = o.plan.expect("plan");
    plan.validate().unwrap();
    assert_eq!(plan.feature, "Retry with jittered backoff");
    assert_eq!(plan.tasks.len(), 2);
    assert_eq!(plan.tasks[1].depends_on, vec!["t1".to_string()]);
    assert_eq!(plan.topo_order().unwrap(), vec!["t1", "t2"]);
    assert!(o.reply.unwrap().contains("Retry with jittered backoff"));
}

#[tokio::test(flavor = "multi_thread")]
async fn distiller_summary() {
    let d = dirs();
    let cfg = config(WorkerRole::Distiller, &d.ws);
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    assert!(o.reply.unwrap().contains("jittered backoff"));
}

#[tokio::test(flavor = "multi_thread")]
async fn reconnects_and_resends_after_drop() {
    let d = dirs();
    init_repo(&d.ws);
    let mut cfg = config(WorkerRole::Builder, &d.ws);
    cfg.options.insert("mock.sleep_ms".into(), "100".into());
    let daemon = FakeDaemon::start(
        &d.socket,
        cfg.clone(),
        Behavior {
            drop_after: Some(3),
            ..Default::default()
        },
    );
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let s = daemon.state.lock().unwrap();
    assert!(
        s.connections >= 2,
        "runner must reconnect (connections = {})",
        s.connections
    );
    assert!(s.hellos >= 2);
    let o = s.outcome.clone().expect("Complete after reconnect");
    assert!(o.success);
    assert_contiguous(&s.events);
    assert_eq!(o.last_seq as usize, s.events.len());
}

#[tokio::test(flavor = "multi_thread")]
async fn unacked_events_are_resent_after_reconnect() {
    let d = dirs();
    init_repo(&d.ws);
    let mut cfg = config(WorkerRole::Builder, &d.ws);
    cfg.options.insert("mock.sleep_ms".into(), "100".into());
    // First connection never acks and drops after 2 events: everything must
    // be resent from seq 1 on the second connection.
    let daemon = FakeDaemon::start(
        &d.socket,
        cfg.clone(),
        Behavior {
            drop_after: Some(2),
            no_acks: true,
            ..Default::default()
        },
    );
    let r = run_runner(&d, &cfg, &[("KILN_ACK_WAIT_MS", "200")], TOKEN).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let s = daemon.state.lock().unwrap();
    assert!(
        s.event_msgs > s.events.len(),
        "expected duplicates from the resend"
    );
    assert_contiguous(&s.events);
    assert!(s.outcome.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn daemon_gone_keeps_spooling_and_writes_outcome() {
    let d = dirs();
    init_repo(&d.ws);
    let mut cfg = config(WorkerRole::Builder, &d.ws);
    cfg.options.insert("mock.sleep_ms".into(), "100".into());
    let daemon = FakeDaemon::start(
        &d.socket,
        cfg.clone(),
        Behavior {
            vanish_after: Some(2),
            ..Default::default()
        },
    );
    let r = run_runner(&d, &cfg, &[("KILN_RECONNECT_BUDGET_MS", "400")], TOKEN).await;
    assert_eq!(r.code, 75, "{}", r.stderr);
    assert!(daemon.state.lock().unwrap().outcome.is_none());
    let o = read_outcome_file(&d);
    assert!(o.success, "{o:?}");
    assert!(o.commit.is_some());
    let spooled = std::fs::read_to_string(d.run.join("spool.jsonl")).unwrap();
    assert_eq!(spooled.lines().count() as u64, o.last_seq);
    assert!(daemon.state.lock().unwrap().events.len() < o.last_seq as usize);
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_terminates_harness() {
    let d = dirs();
    let mut cfg = config(WorkerRole::Qa, &d.ws);
    cfg.options.insert("mock.hang".into(), "1".into());
    let stop = DaemonMessage::Stop {
        reason: "user pressed stop".into(),
    };
    let daemon = FakeDaemon::start(
        &d.socket,
        cfg.clone(),
        Behavior {
            send_after: vec![(3, stop)],
            ..Default::default()
        },
    );
    let t = Instant::now();
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert!(t.elapsed() < Duration::from_secs(10));
    assert_eq!(r.code, 1, "{}", r.stderr);
    let s = daemon.state.lock().unwrap();
    let o = s.outcome.clone().unwrap();
    assert!(!o.success);
    assert!(
        o.error
            .as_deref()
            .unwrap()
            .contains("stopped: user pressed stop"),
        "{o:?}"
    );
    assert!(has_log(&s.events, "stop requested"));
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_escalates_to_sigkill() {
    let d = dirs();
    let mut cfg = config(WorkerRole::Qa, &d.ws);
    cfg.options.insert("mock.hang".into(), "1".into());
    cfg.options.insert("mock.ignore_sigterm".into(), "1".into());
    let stop = DaemonMessage::Stop {
        reason: "cancel".into(),
    };
    let daemon = FakeDaemon::start(
        &d.socket,
        cfg.clone(),
        Behavior {
            send_after: vec![(3, stop)],
            ..Default::default()
        },
    );
    let t = Instant::now();
    let r = run_runner(&d, &cfg, &[("KILN_STOP_GRACE_MS", "300")], TOKEN).await;
    assert!(t.elapsed() < Duration::from_secs(10));
    assert_eq!(r.code, 1, "{}", r.stderr);
    let s = daemon.state.lock().unwrap();
    assert!(has_log(&s.events, "SIGKILL"));
    assert_eq!(s.outcome.clone().unwrap().exit_code, Some(128 + 9));
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_is_enforced() {
    let d = dirs();
    let mut cfg = config(WorkerRole::Reviewer, &d.ws);
    cfg.timeout_secs = 1;
    cfg.options.insert("mock.hang".into(), "1".into());
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let t = Instant::now();
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert!(t.elapsed() < Duration::from_secs(10));
    assert_eq!(r.code, 1, "{}", r.stderr);
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    assert!(
        o.error.as_deref().unwrap().contains("timed out after 1s"),
        "{o:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn feedback_is_recorded_and_forwarded() {
    let d = dirs();
    init_repo(&d.ws);
    let mut cfg = config(WorkerRole::Builder, &d.ws);
    cfg.options.insert("mock.sleep_ms".into(), "300".into());
    let fb = DaemonMessage::Feedback {
        text: "please also handle 429".into(),
    };
    let daemon = FakeDaemon::start(
        &d.socket,
        cfg.clone(),
        Behavior {
            send_after: vec![(2, fb)],
            ..Default::default()
        },
    );
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 0, "{}", r.stderr);
    let fb = std::fs::read_to_string(d.run.join("feedback.txt")).unwrap();
    assert!(fb.contains("please also handle 429"));
    let s = daemon.state.lock().unwrap();
    assert!(has_log(
        &s.events,
        "feedback received: please also handle 429"
    ));
    assert!(has_log(
        &s.events,
        "mock received feedback: please also handle 429"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn harness_exit_code_fails_run() {
    let d = dirs();
    let mut cfg = config(WorkerRole::Reviewer, &d.ws);
    cfg.options.insert("mock.exit_code".into(), "4".into());
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], TOKEN).await;
    assert_eq!(r.code, 1, "{}", r.stderr);
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    assert_eq!(o.exit_code, Some(4));
    assert!(o.verdict.is_some(), "outputs are still collected");
    assert!(o.error.unwrap().contains("harness exited with 4"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_token_exits_with_startup_error() {
    let d = dirs();
    let cfg = config(WorkerRole::Qa, &d.ws);
    let _daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let r = run_runner(&d, &cfg, &[], "wrong-token").await;
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert!(
        !r.stderr.contains("wrong-token"),
        "token must not be logged"
    );
    let o = read_outcome_file(&d);
    assert!(o.error.unwrap().contains("bad token"));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_daemon_at_all() {
    let d = dirs();
    let cfg = config(WorkerRole::Qa, &d.ws);
    let r = run_runner(&d, &cfg, &[("KILN_RECONNECT_BUDGET_MS", "300")], TOKEN).await;
    assert_eq!(r.code, 3, "{}", r.stderr);
    assert!(!read_outcome_file(&d).success);
}

#[tokio::test(flavor = "multi_thread")]
async fn spool_resumes_sequence_after_runner_restart() {
    let d = dirs();
    let cfg = config(WorkerRole::Distiller, &d.ws);
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    assert_eq!(run_runner(&d, &cfg, &[], TOKEN).await.code, 0);
    let first = daemon
        .state
        .lock()
        .unwrap()
        .outcome
        .clone()
        .unwrap()
        .last_seq;
    drop(daemon);
    std::fs::remove_file(&d.socket).unwrap();
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    assert_eq!(run_runner(&d, &cfg, &[], TOKEN).await.code, 0);
    let s = daemon.state.lock().unwrap();
    assert_eq!(
        *s.events.keys().next().unwrap(),
        first + 1,
        "seqs continue after the existing spool"
    );
}
