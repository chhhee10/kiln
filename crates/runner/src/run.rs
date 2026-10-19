//! `kiln-runner run`: the in-container driver (ARCHITECTURE.md "runner").
//!
//! connect → Hello/Welcome(RunConfig) → write Launch.files → spawn the
//! harness in its own process group → stdout lines → `parse_event` → spool →
//! send → handle Stop/Feedback/timeout → collect outputs → write
//! `out/outcome.json` → wait for acks → `Complete`.
//!
//! Exit codes: 0 success (delivered), 1 run failed (delivered),
//! 3 could not start (no config), 75 outcome not delivered (the daemon
//! recovers it from `out/outcome.json` and the spool).

use crate::collect::{self, HarnessExit};
use crate::emit::Emitter;
use crate::env::RunnerEnv;
use crate::lines::LineReader;
use crate::spool::Spool;
use crate::uplink::{self, Control, LinkState, Uplink};
use kiln_harness::{Harness, Launch};
use kiln_types::event::{LogLevel, RunEventKind};
use kiln_types::protocol::{RunConfig, RunOutcome};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_STARTUP: i32 = 3;
pub const EXIT_UNDELIVERED: i32 = 75;

/// stderr lines forwarded as events before the rest is summarized.
const MAX_STDERR_EVENTS: usize = 400;
/// After the harness exits, wait this long for its pipes to drain.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// Write `out/outcome.json` atomically.
pub fn write_outcome(env: &RunnerEnv, outcome: &RunOutcome) {
    let path = env.outcome_path();
    let res = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(env.out_dir())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(outcome).map_err(std::io::Error::other)?,
        )?;
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = res {
        tracing::error!(error = %e, "failed to write outcome.json");
    }
}

fn signal_group(pid: Option<u32>, sig: i32) {
    let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 0) else {
        return;
    };
    // SAFETY: plain kill(2) on the process group created with process_group(0).
    unsafe {
        libc::kill(-pid, sig);
    }
}

fn append_feedback(path: &Path, text: &str) {
    use std::io::Write;
    let res = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            writeln!(
                f,
                "--- feedback {} ---\n{}\n",
                chrono::Utc::now().to_rfc3339(),
                text.trim_end()
            )
        });
    if let Err(e) = res {
        tracing::warn!(error = %e, "failed to write feedback.txt");
    }
}

fn write_launch_files(env: &RunnerEnv, launch: &Launch) -> std::io::Result<()> {
    for (p, content) in &launch.files {
        let p: PathBuf = if p.is_absolute() {
            p.clone()
        } else {
            env.run_dir.join(p)
        };
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&p, content)?;
    }
    Ok(())
}

fn spawn_harness(env: &RunnerEnv, launch: &Launch, workspace: &Path) -> std::io::Result<Child> {
    let mut cmd = Command::new(&launch.program);
    cmd.args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("KILN_RUN_DIR", &env.run_dir)
        .env("KILN_WORKSPACE", workspace)
        .env(kiln_types::protocol::paths::ENV_SOCKET, &env.socket)
        .stdin(if launch.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Ok(exe) = std::env::current_exe() {
        cmd.env("KILN_RUNNER_BIN", exe);
    }
    if workspace.is_dir() {
        cmd.current_dir(workspace);
    }
    cmd.spawn()
}

fn exit_code(st: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    st.code().or_else(|| st.signal().map(|s| 128 + s))
}

/// Entry point for `kiln-runner run`. Returns the process exit code.
pub async fn run(env: RunnerEnv) -> i32 {
    if let Err(e) = std::fs::create_dir_all(env.artifacts_dir()) {
        tracing::warn!(error = %e, "cannot create out/artifacts");
    }
    let spool = match Spool::open(&env.spool_path()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "cannot open spool");
            write_outcome(&env, &RunOutcome::failed(format!("cannot open spool: {e}")));
            return EXIT_STARTUP;
        }
    };
    let (conn, mut cfg) = match uplink::connect_with_backoff(&env, env.reconnect_budget).await {
        Ok(ok) => ok,
        Err(e) => {
            tracing::error!(error = %e, "could not connect to the daemon");
            let mut o = RunOutcome::failed(format!("runner could not reach the daemon: {e}"));
            o.last_seq = spool.last_seq();
            write_outcome(&env, &o);
            return EXIT_STARTUP;
        }
    };
    if cfg.run_id != env.run_id {
        tracing::warn!(config = %cfg.run_id, env = %env.run_id, "Welcome run_id differs from KILN_RUN_ID");
    }
    let workspace = env
        .workspace
        .clone()
        .unwrap_or_else(|| PathBuf::from(&cfg.workdir));
    cfg.workdir = workspace.to_string_lossy().into_owned();

    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let uplink = Uplink::spawn(env.clone(), conn, control_tx);
    let mut em = Emitter::new(env.run_id, spool, uplink);
    tracing::info!(run_id = %cfg.run_id, role = cfg.role.as_str(), harness = %cfg.harness, "run starting");

    let harness = kiln_harness::registry().get(&cfg.harness);
    let exit = match &harness {
        Some(h) => drive(&env, &cfg, h.clone(), &workspace, &mut em, control_rx).await,
        None => {
            let msg = format!("unknown harness {:?}", cfg.harness);
            em.emit(RunEventKind::Error {
                message: msg.clone(),
            });
            HarnessExit {
                spawn_error: Some(msg),
                ..Default::default()
            }
        }
    };

    let mut outcome =
        collect::collect(&cfg, &env, &workspace, harness.as_deref(), &exit, &mut em).await;
    em.emit(RunEventKind::Done {
        success: outcome.success,
        summary: outcome.summary.clone(),
    });
    outcome.last_seq = em.last_seq();
    em.sync();
    write_outcome(&env, &outcome);

    let up = em.uplink();
    if !up.wait_acked(outcome.last_seq, env.ack_wait).await && up.state() != LinkState::Dead {
        tracing::warn!(
            last_seq = outcome.last_seq,
            acked = up.last_ack(),
            "not every event was acked"
        );
    }
    let complete_timeout = if up.state() == LinkState::Dead {
        Duration::from_secs(10)
    } else {
        env.reconnect_budget + Duration::from_secs(10)
    };
    let delivered = up.complete(outcome.clone(), complete_timeout).await;
    tracing::info!(success = outcome.success, delivered, "run finished");
    match (delivered, outcome.success) {
        (false, _) => EXIT_UNDELIVERED,
        (true, true) => EXIT_OK,
        (true, false) => EXIT_FAILED,
    }
}

/// Spawn and supervise the harness process.
async fn drive(
    env: &RunnerEnv,
    cfg: &RunConfig,
    harness: Arc<dyn Harness>,
    workspace: &Path,
    em: &mut Emitter,
    mut control: mpsc::UnboundedReceiver<Control>,
) -> HarnessExit {
    let mut exit = HarnessExit::default();
    let launch = match harness.prepare(cfg) {
        Ok(l) => l,
        Err(e) => {
            exit.spawn_error = Some(e.to_string());
            em.emit(RunEventKind::Error {
                message: format!("harness prepare failed: {e}"),
            });
            return exit;
        }
    };
    if let Err(e) = write_launch_files(env, &launch) {
        exit.spawn_error = Some(format!("writing launch files: {e}"));
        em.emit(RunEventKind::Error {
            message: exit.spawn_error.clone().unwrap_or_default(),
        });
        return exit;
    }
    let mut child = match spawn_harness(env, &launch, workspace) {
        Ok(c) => c,
        Err(e) => {
            exit.spawn_error = Some(format!("{}: {e}", launch.program));
            em.emit(RunEventKind::Error {
                message: format!("failed to start {}: {e}", launch.program),
            });
            return exit;
        }
    };
    let pid = child.id();
    em.emit(RunEventKind::Step {
        title: format!(
            "{} started ({})",
            harness.id(),
            if cfg.model.is_empty() {
                "default model"
            } else {
                &cfg.model
            }
        ),
    });

    // stdin: initial payload, then live feedback lines, via a writer task so
    // a slow reader never blocks the supervision loop.
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
    let live = launch.live_input;
    if let (Some(mut stdin), Some(initial)) = (child.stdin.take(), launch.stdin.clone()) {
        tokio::spawn(async move {
            if stdin.write_all(initial.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                return;
            }
            if !live {
                return; // dropping stdin closes it
            }
            while let Some(line) = stdin_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                    return;
                }
            }
        });
    }
    let mut stdout = child
        .stdout
        .take()
        .map(|o| LineReader::new(BufReader::new(o)));
    let mut stderr = child
        .stderr
        .take()
        .map(|e| LineReader::new(BufReader::new(e)));
    let mut stderr_count = 0usize;

    let deadline =
        (cfg.timeout_secs > 0).then(|| Instant::now() + Duration::from_secs(cfg.timeout_secs));
    let mut kill_at: Option<Instant> = None;
    let mut status = None;

    async fn next(
        r: &mut Option<LineReader<BufReader<impl tokio::io::AsyncRead + Unpin>>>,
    ) -> Option<String> {
        match r {
            Some(r) => r.next_line().await.ok().flatten(),
            None => std::future::pending().await,
        }
    }

    enum Ev {
        Out(Option<String>),
        Err(Option<String>),
        Ctl(Option<Control>),
        Timeout,
        Kill,
        Exited(std::io::Result<std::process::ExitStatus>),
    }

    let mut control_open = true;
    while status.is_none() {
        let ev = tokio::select! {
            l = next(&mut stdout), if stdout.is_some() => Ev::Out(l),
            l = next(&mut stderr), if stderr.is_some() => Ev::Err(l),
            c = control.recv(), if control_open => Ev::Ctl(c),
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() && exit.timed_out.is_none() && exit.stopped.is_none() => Ev::Timeout,
            _ = tokio::time::sleep_until(kill_at.unwrap_or_else(Instant::now)), if kill_at.is_some() => Ev::Kill,
            st = child.wait() => Ev::Exited(st),
        };
        match ev {
            Ev::Out(Some(line)) => {
                for k in harness.parse_event(&line) {
                    em.emit(k);
                }
            }
            Ev::Out(None) => stdout = None,
            Ev::Err(Some(line)) => {
                stderr_count += 1;
                if stderr_count <= MAX_STDERR_EVENTS && !line.trim().is_empty() {
                    em.log(
                        LogLevel::Warn,
                        kiln_harness::util::truncate(
                            line.trim_end(),
                            kiln_harness::util::MAX_LOG_CHARS,
                        ),
                    );
                } else if stderr_count == MAX_STDERR_EVENTS + 1 {
                    em.log(LogLevel::Warn, "further harness stderr suppressed");
                }
            }
            Ev::Err(None) => stderr = None,
            Ev::Ctl(None) => control_open = false,
            Ev::Ctl(Some(Control::Stop(reason))) => {
                if exit.stopped.is_none() && exit.timed_out.is_none() {
                    em.log(LogLevel::Warn, format!("stop requested: {reason}"));
                    exit.stopped = Some(reason);
                    signal_group(pid, libc::SIGTERM);
                    kill_at = Some(Instant::now() + env.stop_grace);
                }
            }
            Ev::Ctl(Some(Control::Feedback(text))) => {
                append_feedback(&env.feedback_path(), &text);
                let first = text
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                em.log(
                    LogLevel::Info,
                    format!(
                        "feedback received: {}",
                        kiln_harness::util::truncate(&first, 200)
                    ),
                );
                if live && let Some(line) = harness.feedback_input(&text) {
                    let _ = stdin_tx.send(line);
                }
            }
            Ev::Timeout => {
                em.emit(RunEventKind::Error {
                    message: format!("timed out after {}s; stopping harness", cfg.timeout_secs),
                });
                exit.timed_out = Some(cfg.timeout_secs);
                signal_group(pid, libc::SIGTERM);
                kill_at = Some(Instant::now() + env.stop_grace);
            }
            Ev::Kill => {
                em.log(LogLevel::Warn, "harness ignored SIGTERM; sending SIGKILL");
                signal_group(pid, libc::SIGKILL);
                kill_at = None;
            }
            Ev::Exited(st) => status = Some(st),
        }
    }
    drop(stdin_tx);

    // Drain whatever the harness printed before exiting; background
    // grandchildren holding the pipes open are killed after the timeout.
    let drain_until = Instant::now() + DRAIN_TIMEOUT;
    while stdout.is_some() || stderr.is_some() {
        let ev = tokio::select! {
            l = next(&mut stdout), if stdout.is_some() => Ev::Out(l),
            l = next(&mut stderr), if stderr.is_some() => Ev::Err(l),
            _ = tokio::time::sleep_until(drain_until) => Ev::Kill,
        };
        match ev {
            Ev::Out(Some(line)) => {
                for k in harness.parse_event(&line) {
                    em.emit(k);
                }
            }
            Ev::Out(None) => stdout = None,
            Ev::Err(Some(line)) => {
                stderr_count += 1;
                if stderr_count <= MAX_STDERR_EVENTS && !line.trim().is_empty() {
                    em.log(
                        LogLevel::Warn,
                        kiln_harness::util::truncate(
                            line.trim_end(),
                            kiln_harness::util::MAX_LOG_CHARS,
                        ),
                    );
                }
            }
            Ev::Err(None) => stderr = None,
            _ => break,
        }
    }
    signal_group(pid, libc::SIGKILL);

    match status {
        Some(Ok(st)) => exit.exit_code = exit_code(st),
        Some(Err(e)) => {
            em.emit(RunEventKind::Error {
                message: format!("waiting for harness: {e}"),
            });
        }
        None => {}
    }
    exit
}
