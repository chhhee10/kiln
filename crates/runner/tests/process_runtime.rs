//! The daemon-side flow under the sandbox's ProcessRuntime: container paths
//! (`/run/kiln/kiln.sock`, `/workspace`) are mapped to host paths and the
//! real runner drives the mock harness.

mod common;

use common::*;
use kiln_sandbox::{ContainerSpec, ProcessRuntime, Runtime, mounts_for, prepare_run_dir};
use kiln_types::WorkerRole;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn runner_under_process_runtime() {
    let d = dirs();
    init_repo(&d.ws);
    prepare_run_dir(&d.run).unwrap();
    let mut cfg = config(WorkerRole::Builder, &d.ws);
    cfg.workdir = "/workspace".into(); // container path, as the daemon sends it
    // The daemon listens on <run_dir>/kiln.sock (mounted at /run/kiln/kiln.sock).
    let sock = d.run.join("kiln.sock");
    let daemon = FakeDaemon::start(&sock, cfg.clone(), Behavior::default());

    let rt = ProcessRuntime::new();
    let mut spec = ContainerSpec::new("kiln-run-test", "kiln/worker:dev");
    spec.cmd = vec![env!("CARGO_BIN_EXE_kiln-runner").into(), "run".into()];
    spec.mounts = mounts_for(WorkerRole::Builder, Some(&d.ws), None, &d.run);
    spec.env = vec![
        ("KILN_SOCKET".into(), "/run/kiln/kiln.sock".into()),
        ("KILN_RUN_TOKEN".into(), TOKEN.into()),
        ("KILN_RUN_ID".into(), cfg.run_id.to_string()),
    ];
    spec.workdir = Some("/workspace".into());
    let id = rt.create(&spec).await.unwrap();
    rt.start(&id).await.unwrap();
    let code = tokio::time::timeout(Duration::from_secs(60), rt.wait(&id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(code, 0, "{}", rt.logs_tail(&id, 50).await.unwrap());
    let o = daemon.state.lock().unwrap().outcome.clone().unwrap();
    assert!(o.success);
    assert_eq!(o.commit.unwrap(), git(&d.ws, &["rev-parse", "HEAD"]));
    rt.remove(&id).await.unwrap();
}
