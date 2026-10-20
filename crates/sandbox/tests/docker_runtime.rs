//! DockerRuntime against a real daemon. Skipped unless `KILN_TEST_DOCKER=1`.

use kiln_sandbox::*;
use std::time::Duration;

fn enabled() -> bool {
    let on = std::env::var("KILN_TEST_DOCKER").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipping: set KILN_TEST_DOCKER=1 to run Docker tests");
    }
    on
}

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        kiln_types::RunId::new().0.simple()
    )
}

#[tokio::test]
async fn lifecycle_mounts_and_limits() {
    if !enabled() {
        return;
    }
    let rt = DockerRuntime::connect().await.expect("docker");
    let info = rt.ping().await.unwrap();
    assert!(!info.version.is_empty());
    rt.ensure_image("alpine:latest").await.unwrap();

    let d = tempfile::tempdir().unwrap();
    let ws = d.path().join("ws");
    let run = d.path().join("run");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("hello.txt"), "hi").unwrap();
    prepare_run_dir(&run).unwrap();
    // Let the container user write into the run dir regardless of host uid.
    use std::os::unix::fs::PermissionsExt;
    // (root in the container has no CAP_DAC_OVERRIDE: caps are dropped).
    for p in [run.clone(), run.join("out")] {
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o777)).unwrap();
    }

    let name = unique("kiln-test");
    let mut spec = ContainerSpec::new(&name, "alpine:latest");
    spec.cmd = vec![
        "sh".into(),
        "-c".into(),
        // RO workspace must reject writes; run dir must accept them.
        "cat /workspace/hello.txt; if touch /workspace/x 2>/dev/null; then echo RW; else echo RO; fi; \
         echo ok > /run/kiln/out/probe.txt; echo caps=$(grep CapEff /proc/self/status | awk '{print $2}'); exit 7"
            .into(),
    ];
    spec.mounts = mounts_for(kiln_types::WorkerRole::Qa, Some(&ws), None, &run);
    spec.memory_mb = 256;
    spec.cpus = 0.5;
    spec.labels
        .insert(LABEL_RUN_ID.into(), kiln_types::RunId::new().to_string());
    spec.labels.insert(LABEL_ROLE.into(), "qa".into());

    let id = rt.create(&spec).await.unwrap();
    let listed = rt.list_managed().await.unwrap();
    let me = listed.iter().find(|c| c.id == id).expect("listed");
    assert_eq!(me.name, name);
    assert_eq!(me.role(), Some("qa"));
    assert!(me.run_id().is_some());

    rt.start(&id).await.unwrap();
    assert_eq!(rt.wait(&id).await.unwrap(), 7);
    let logs = rt.logs_tail(&id, 50).await.unwrap();
    assert!(logs.contains("hi"), "{logs}");
    assert!(logs.contains("RO"), "{logs}");
    assert!(logs.contains("caps=0000000000000000"), "{logs}");
    assert_eq!(
        std::fs::read_to_string(run.join("out/probe.txt"))
            .unwrap()
            .trim(),
        "ok"
    );
    assert!(!rt.is_running(&id).await.unwrap());
    // Not auto-removed.
    assert!(rt.list_managed().await.unwrap().iter().any(|c| c.id == id));
    rt.remove(&id).await.unwrap();
    rt.remove(&id).await.unwrap();
    assert!(!rt.list_managed().await.unwrap().iter().any(|c| c.id == id));
}

#[tokio::test]
async fn stop_stats_and_networks() {
    if !enabled() {
        return;
    }
    let rt = DockerRuntime::connect().await.expect("docker");
    rt.ensure_image("alpine:latest").await.unwrap();
    let net = unique("kiln-net");
    rt.create_network(&net).await.unwrap();
    rt.create_network(&net).await.unwrap();

    let d = tempfile::tempdir().unwrap();
    prepare_run_dir(d.path()).unwrap();
    let mut spec = ContainerSpec::new(unique("kiln-test"), "alpine:latest");
    spec.cmd = vec!["sleep".into(), "300".into()];
    spec.mounts = mounts_for(kiln_types::WorkerRole::Builder, None, None, d.path());
    spec.network = NetworkMode::Named(net.clone());
    spec.aliases = vec!["worker".into()];
    let id = rt.create(&spec).await.unwrap();
    rt.start(&id).await.unwrap();
    assert!(rt.is_running(&id).await.unwrap());
    let st = rt.stats(&id).await.unwrap();
    assert!(st.pids >= 1, "{st:?}");
    rt.stop(&id, Duration::from_secs(1)).await.unwrap();
    assert!(!rt.is_running(&id).await.unwrap());
    rt.remove(&id).await.unwrap();
    rt.remove_network(&net).await.unwrap();
    rt.remove_network(&net).await.unwrap();
}

#[tokio::test]
async fn missing_mount_source_is_reported() {
    if !enabled() {
        return;
    }
    let rt = DockerRuntime::connect().await.expect("docker");
    let mut spec = ContainerSpec::new(unique("kiln-test"), "alpine:latest");
    spec.mounts = vec![Mount::ro("/definitely/not/here/kiln", "/x")];
    assert!(matches!(
        rt.create(&spec).await,
        Err(SandboxError::MissingMountSource(_))
    ));
}

#[tokio::test]
async fn exec_reports_exit_code_and_output() {
    if !enabled() {
        return;
    }
    let rt = DockerRuntime::connect().await.expect("docker");
    rt.ensure_image("alpine:latest").await.unwrap();
    let name = unique("kiln-exec");
    let mut spec = ContainerSpec::new(&name, "alpine:latest");
    spec.cmd = vec!["sleep".into(), "30".into()];
    let id = rt.create(&spec).await.unwrap();
    rt.start(&id).await.unwrap();
    let cmd = |s: &str| vec!["sh".to_string(), "-c".to_string(), s.to_string()];
    let ok = rt.exec(&id, &cmd("echo hello")).await.unwrap();
    assert_eq!(ok.exit_code, 0);
    assert!(ok.output.contains("hello"), "{}", ok.output);
    let bad = rt.exec(&id, &cmd("echo nope >&2; exit 4")).await.unwrap();
    assert_eq!(bad.exit_code, 4);
    assert!(bad.output.contains("nope"), "{}", bad.output);
    rt.stop(&id, Duration::from_secs(1)).await.unwrap();
    rt.remove(&id).await.unwrap();
}
