//! Test-side fake daemon speaking the runner protocol, plus helpers to run
//! the real `kiln-runner` binary against it with the mock harness.

#![allow(dead_code)]

use kiln_types::event::{RunEvent, RunEventKind};
use kiln_types::protocol::*;
use kiln_types::{RunId, WorkerRole};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;

pub const TOKEN: &str = "test-token-not-a-secret";

#[derive(Default)]
pub struct State {
    pub hellos: usize,
    pub connections: usize,
    /// Events by seq (dedupes resends, like the real daemon).
    pub events: BTreeMap<u64, RunEvent>,
    pub event_msgs: usize,
    pub artifacts: Vec<(String, kiln_types::model::ArtifactKind, u64)>,
    pub outcome: Option<RunOutcome>,
    pub memory_calls: Vec<MemoryCall>,
}

/// Knobs for the fake daemon.
#[derive(Clone, Default)]
pub struct Behavior {
    /// Close the first connection after this many event messages.
    pub drop_after: Option<usize>,
    /// Stop accepting new connections (and drop the current one) after this many events.
    pub vanish_after: Option<usize>,
    /// Send these messages once this many events have been received.
    pub send_after: Vec<(usize, DaemonMessage)>,
    /// Don't ack events.
    pub no_acks: bool,
}

pub struct FakeDaemon {
    pub state: Arc<Mutex<State>>,
    pub socket: PathBuf,
    pub completed: Arc<Notify>,
}

impl FakeDaemon {
    pub fn start(socket: &Path, cfg: RunConfig, behavior: Behavior) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let completed = Arc::new(Notify::new());
        let (st, done) = (state.clone(), completed.clone());
        tokio::spawn(async move {
            let vanished = Arc::new(Notify::new());
            loop {
                let accept = tokio::select! {
                    a = listener.accept() => a,
                    _ = vanished.notified() => break,
                };
                let Ok((stream, _)) = accept else { break };
                let first = {
                    let mut s = st.lock().unwrap();
                    s.connections += 1;
                    s.connections == 1
                };
                tokio::spawn(handle(
                    stream,
                    cfg.clone(),
                    behavior.clone(),
                    st.clone(),
                    done.clone(),
                    vanished.clone(),
                    first,
                ));
            }
            // Listener dropped here: new connects fail.
        });
        Self {
            state,
            socket: socket.to_path_buf(),
            completed,
        }
    }
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, m: &DaemonMessage) -> std::io::Result<()> {
    let mut s = serde_json::to_string(m).unwrap();
    s.push('\n');
    w.write_all(s.as_bytes()).await
}

async fn handle(
    stream: UnixStream,
    cfg: RunConfig,
    b: Behavior,
    st: Arc<Mutex<State>>,
    done: Arc<Notify>,
    vanished: Arc<Notify>,
    first: bool,
) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    let mut sent_triggers = vec![false; b.send_after.len()];
    while let Ok(Some(line)) = lines.next_line().await {
        let msg: RunnerMessage = serde_json::from_str(&line).expect("runner sent invalid JSON");
        match msg {
            RunnerMessage::Hello {
                token, protocol, ..
            } => {
                assert_eq!(protocol, PROTOCOL_VERSION);
                st.lock().unwrap().hellos += 1;
                if token != TOKEN {
                    let _ = send(
                        &mut w,
                        &DaemonMessage::Reject {
                            reason: "bad token".into(),
                        },
                    )
                    .await;
                    return;
                }
                if send(
                    &mut w,
                    &DaemonMessage::Welcome {
                        config: Box::new(cfg.clone()),
                    },
                )
                .await
                .is_err()
                {
                    return;
                }
            }
            RunnerMessage::Event { event } => {
                let seq = event.seq;
                let n = {
                    let mut s = st.lock().unwrap();
                    s.event_msgs += 1;
                    s.events.insert(seq, event);
                    s.events.len()
                };
                if !b.no_acks && send(&mut w, &DaemonMessage::Ack { seq }).await.is_err() {
                    return;
                }
                for (i, (after, m)) in b.send_after.iter().enumerate() {
                    if !sent_triggers[i] && n >= *after {
                        sent_triggers[i] = true;
                        let _ = send(&mut w, m).await;
                    }
                }
                if first && b.drop_after.is_some_and(|d| n >= d) {
                    return; // drop the connection
                }
                if b.vanish_after.is_some_and(|d| n >= d) {
                    vanished.notify_one();
                    return;
                }
            }
            RunnerMessage::Artifact {
                path, kind, size, ..
            } => st.lock().unwrap().artifacts.push((path, kind, size)),
            RunnerMessage::MemoryRequest { req_id, call } => {
                st.lock().unwrap().memory_calls.push(call.clone());
                let result = match call {
                    MemoryCall::SearchMemory { query, .. } => Ok(
                        serde_json::json!({"hits": [{"title": format!("about {query}"), "score": 0.9}]}),
                    ),
                    MemoryCall::ProposeMemory { proposal } => Ok(
                        serde_json::json!({"id": "m1", "status": "active", "title": proposal.title}),
                    ),
                };
                let _ = send(&mut w, &DaemonMessage::MemoryResponse { req_id, result }).await;
            }
            RunnerMessage::Heartbeat => {}
            RunnerMessage::Complete { outcome } => {
                st.lock().unwrap().outcome = Some(outcome);
                done.notify_waiters();
                return; // close, as the real daemon does
            }
        }
    }
}

pub fn config(role: WorkerRole, workdir: &Path) -> RunConfig {
    RunConfig {
        run_id: RunId::new(),
        task_id: None,
        task_key: Some("FP-1".into()),
        role,
        harness: "mock".into(),
        model: "mock-1".into(),
        prompt: "Retry with jittered backoff\nMore detail.".into(),
        system: "You are a test.".into(),
        workdir: workdir.to_string_lossy().into_owned(),
        branch: Some("kiln/fp-1-retry".into()),
        base_branch: "main".into(),
        attempt: 1,
        timeout_secs: 60,
        qa_profile: None,
        options: BTreeMap::from([("mock.sleep_ms".into(), "10".into())]),
        read_only: !role.writes_code(),
    }
}

pub struct Dirs {
    _root: tempfile::TempDir,
    pub run: PathBuf,
    pub ws: PathBuf,
    pub socket: PathBuf,
}

pub fn dirs() -> Dirs {
    // Short path: Unix socket paths are limited to ~108 bytes.
    let root = tempfile::Builder::new()
        .prefix("kr")
        .tempdir_in("/tmp")
        .unwrap();
    let run = root.path().join("run");
    let ws = root.path().join("ws");
    std::fs::create_dir_all(run.join("out/artifacts")).unwrap();
    std::fs::create_dir_all(&ws).unwrap();
    let socket = root.path().join("k.sock");
    Dirs {
        _root: root,
        run,
        ws,
        socket,
    }
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

pub fn init_repo(ws: &Path) {
    git(ws, &["init", "-q", "-b", "main"]);
    std::fs::write(ws.join("README.md"), "hello\n").unwrap();
    git(ws, &["add", "-A"]);
    git(ws, &["commit", "-q", "-m", "init"]);
    git(ws, &["checkout", "-q", "-b", "kiln/fp-1-retry"]);
}

pub struct RunResult {
    pub code: i32,
    pub stderr: String,
}

pub async fn run_runner(
    d: &Dirs,
    cfg: &RunConfig,
    extra_env: &[(&str, &str)],
    token: &str,
) -> RunResult {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_kiln-runner"));
    cmd.arg("run")
        .env("KILN_RUN_DIR", &d.run)
        .env("KILN_WORKSPACE", &d.ws)
        .env("KILN_SOCKET", &d.socket)
        .env("KILN_RUN_TOKEN", token)
        .env("KILN_RUN_ID", cfg.run_id.to_string())
        .env("KILN_RECONNECT_BUDGET_MS", "5000")
        .env("KILN_ACK_WAIT_MS", "3000")
        .env("KILN_LOG", "debug")
        .env_remove("KILN_RUNNER_BIN")
        .kill_on_drop(true);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .expect("runner timed out")
        .unwrap();
    RunResult {
        code: out.status.code().unwrap_or(-1),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Events must be 1..=n without gaps, ending with Done.
pub fn assert_contiguous(events: &BTreeMap<u64, RunEvent>) {
    let seqs: Vec<u64> = events.keys().copied().collect();
    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "event seqs must be contiguous");
    assert!(matches!(
        events.values().last().map(|e| &e.kind),
        Some(RunEventKind::Done { .. })
    ));
}

pub fn has_log(events: &BTreeMap<u64, RunEvent>, needle: &str) -> bool {
    events
        .values()
        .any(|e| matches!(&e.kind, RunEventKind::Log { message, .. } if message.contains(needle)))
}

pub fn read_outcome_file(d: &Dirs) -> RunOutcome {
    serde_json::from_slice(&std::fs::read(d.run.join("out/outcome.json")).unwrap()).unwrap()
}
