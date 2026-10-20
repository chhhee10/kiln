//! ProcessRuntime behaviour (no Docker needed).

use kiln_sandbox::*;
use std::time::{Duration, Instant};

fn sh(name: &str, script: &str, dir: &std::path::Path) -> ContainerSpec {
    let mut s = ContainerSpec::new(name, "unused");
    s.cmd = vec!["sh".into(), "-c".into(), script.into()];
    s.mounts = mounts_for(
        kiln_types::WorkerRole::Builder,
        Some(&dir.join("ws")),
        None,
        &dir.join("run"),
    );
    s.env
        .push(("KILN_SOCKET".into(), "/run/kiln/kiln.sock".into()));
    s.labels.insert(LABEL_RUN_ID.into(), "r1".into());
    s
}

fn tmp() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("ws")).unwrap();
    prepare_run_dir(&d.path().join("run")).unwrap();
    d
}

#[tokio::test]
async fn runs_and_maps_paths() {
    let d = tmp();
    let rt = ProcessRuntime::new();
    assert_eq!(rt.ping().await.unwrap().name, "process");
    let spec = sh(
        "p1",
        "echo run=$KILN_RUN_DIR; echo ws=$KILN_WORKSPACE; echo sock=$KILN_SOCKET; pwd; exit 3",
        d.path(),
    );
    let id = rt.create(&spec).await.unwrap();
    assert!(!rt.is_running(&id).await.unwrap());
    rt.start(&id).await.unwrap();
    assert_eq!(rt.wait(&id).await.unwrap(), 3);
    // Give the log pumps a moment to drain.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let logs = rt.logs_tail(&id, 10).await.unwrap();
    let run = d.path().join("run");
    assert!(logs.contains(&format!("run={}", run.display())), "{logs}");
    assert!(
        logs.contains(&format!("sock={}/kiln.sock", run.display())),
        "{logs}"
    );
    assert!(
        logs.contains(&format!("ws={}", d.path().join("ws").display())),
        "{logs}"
    );
    assert_eq!(rt.logs_tail(&id, 1).await.unwrap().lines().count(), 1);
    let managed = rt.list_managed().await.unwrap();
    assert_eq!(managed.len(), 1);
    assert_eq!(managed[0].state, "exited");
    assert_eq!(
        managed[0].labels.get(LABEL_MANAGED).map(String::as_str),
        Some("1")
    );
    assert_eq!(
        managed[0].labels.get(LABEL_RUN_ID).map(String::as_str),
        Some("r1")
    );
    rt.remove(&id).await.unwrap();
    assert!(rt.list_managed().await.unwrap().is_empty());
    assert!(rt.wait(&id).await.unwrap_err().is_not_found());
}

#[tokio::test]
async fn stop_terminates_group() {
    let d = tmp();
    let rt = ProcessRuntime::new();
    let id = rt
        .create(&sh("p2", "sleep 30 & sleep 30; wait", d.path()))
        .await
        .unwrap();
    rt.start(&id).await.unwrap();
    assert!(rt.is_running(&id).await.unwrap());
    let st = rt.stats(&id).await.unwrap();
    assert!(st.memory_bytes > 0 || cfg!(not(target_os = "linux")));
    let t = Instant::now();
    rt.stop(&id, Duration::from_secs(5)).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(4));
    assert!(!rt.is_running(&id).await.unwrap());
    assert_eq!(rt.wait(&id).await.unwrap(), 128 + 15);
    // Idempotent.
    rt.stop(&id, Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn stop_escalates_to_sigkill() {
    let d = tmp();
    let rt = ProcessRuntime::new();
    let id = rt
        .create(&sh(
            "p3",
            "trap '' TERM; while true; do sleep 0.05; done",
            d.path(),
        ))
        .await
        .unwrap();
    rt.start(&id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = Instant::now();
    rt.stop(&id, Duration::from_millis(300)).await.unwrap();
    assert!(t.elapsed() >= Duration::from_millis(250));
    assert_eq!(rt.wait(&id).await.unwrap(), 128 + 9);
}

#[tokio::test]
async fn rejects_bad_specs() {
    let rt = ProcessRuntime::new();
    let empty = ContainerSpec::new("p4", "img");
    assert!(rt.create(&empty).await.is_err());
    let d = tmp();
    rt.create(&sh("dup", "true", d.path())).await.unwrap();
    assert!(rt.create(&sh("dup", "true", d.path())).await.is_err());
    assert!(rt.start("nope").await.unwrap_err().is_not_found());
    assert!(!rt.is_running("nope").await.unwrap());
    rt.create_network("n").await.unwrap();
    rt.remove_network("n").await.unwrap();
}

#[tokio::test]
async fn exec_runs_in_the_container_context() {
    let d = tmp();
    let rt = ProcessRuntime::new();
    let id = rt
        .create(&sh("p-exec", "sleep 30", d.path()))
        .await
        .unwrap();
    rt.start(&id).await.unwrap();
    let cmd = |s: &str| vec!["sh".to_string(), "-c".to_string(), s.to_string()];
    let ok = rt.exec(&id, &cmd("pwd; echo $KILN_SOCKET")).await.unwrap();
    assert_eq!(ok.exit_code, 0);
    assert!(
        ok.output
            .contains(&d.path().join("ws").display().to_string()),
        "{}",
        ok.output
    );
    assert!(ok.output.contains("run/kiln.sock"), "{}", ok.output);
    let bad = rt.exec(&id, &cmd("echo nope >&2; exit 4")).await.unwrap();
    assert_eq!(bad.exit_code, 4);
    assert!(bad.output.contains("nope"));
    assert!(rt.exec("missing", &cmd("true")).await.is_err());
    rt.remove(&id).await.unwrap();
}
