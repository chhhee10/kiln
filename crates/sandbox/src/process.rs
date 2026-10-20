//! Host-process "runtime" for development and tests. No isolation at all.
//!
//! `cmd[0]` is spawned directly on the host in its own process group. Mount
//! targets are translated back to host paths:
//!
//! * `KILN_RUN_DIR` is set to the host path mounted at `/run/kiln`, and
//!   `KILN_WORKSPACE` to the one mounted at `/workspace`;
//! * every env value, argument and the workdir that starts with a mount
//!   target (e.g. `KILN_SOCKET=/run/kiln/kiln.sock`) is rewritten to the
//!   corresponding host path.

use crate::Runtime;
use crate::error::{Result, SandboxError};
use crate::spec::*;
use async_trait::async_trait;
use kiln_types::protocol::paths;
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

/// Lines of combined output kept per process for `logs_tail`.
const LOG_LINES: usize = 2000;

#[derive(Debug)]
struct Proc {
    spec: ContainerSpec,
    pid: Option<u32>,
    started: Option<Instant>,
    created: i64,
    exit_tx: watch::Sender<Option<i64>>,
    logs: Arc<Mutex<VecDeque<String>>>,
}

/// Runs container specs as host subprocesses (dev/test only).
#[derive(Debug, Clone, Default)]
pub struct ProcessRuntime {
    procs: Arc<Mutex<HashMap<String, Proc>>>,
    counter: Arc<AtomicU64>,
}

impl ProcessRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_proc<T>(&self, id: &str, f: impl FnOnce(&mut Proc) -> T) -> Result<T> {
        let mut g = self.procs.lock().unwrap_or_else(|e| e.into_inner());
        g.get_mut(id)
            .map(f)
            .ok_or_else(|| SandboxError::NotFound(id.to_string()))
    }

    fn exit_rx(&self, id: &str) -> Result<watch::Receiver<Option<i64>>> {
        self.with_proc(id, |p| p.exit_tx.subscribe())
    }
}

/// Rewrite `s` if it lies under a mount target.
pub(crate) fn map_path(s: &str, mounts: &[Mount]) -> String {
    let best = mounts
        .iter()
        .filter(|m| {
            let t = m.container.trim_end_matches('/');
            s == t || s.strip_prefix(t).is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|m| m.container.len());
    match best {
        Some(m) => {
            let rest = &s[m.container.trim_end_matches('/').len()..];
            format!("{}{}", m.host.to_string_lossy(), rest)
        }
        None => s.to_string(),
    }
}

fn host_of(mounts: &[Mount], target: &str) -> Option<String> {
    mounts
        .iter()
        .find(|m| m.container == target)
        .map(|m| m.host.to_string_lossy().into_owned())
}

fn exit_code(status: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(c), _) => c as i64,
        (None, Some(sig)) => 128 + sig as i64,
        _ => -1,
    }
}

fn signal_group(pid: u32, sig: i32) {
    let Ok(pid) = i32::try_from(pid) else { return };
    if pid <= 0 {
        return;
    }
    // SAFETY: plain syscall; a negative pid targets the process group we
    // created with `process_group(0)`.
    unsafe {
        libc::kill(-pid, sig);
    }
}

fn pump<R: AsyncRead + Unpin + Send + 'static>(r: R, logs: Arc<Mutex<VecDeque<String>>>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(r).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(l)) => {
                    let mut g = logs.lock().unwrap_or_else(|e| e.into_inner());
                    if g.len() >= LOG_LINES {
                        g.pop_front();
                    }
                    g.push_back(l);
                }
                Ok(None) => break,
                // Invalid UTF-8 or read error: keep going is not possible
                // with `lines`, so stop pumping this stream.
                Err(_) => break,
            }
        }
    });
}

/// Best-effort /proc sample for one pid.
fn proc_stats(pid: u32, started: Option<Instant>) -> ContainerStats {
    let mut s = ContainerStats::default();
    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        for line in status.lines() {
            if let Some(v) = line.strip_prefix("VmRSS:") {
                let kb: u64 = v.trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
                s.memory_bytes = kb * 1024;
            } else if let Some(v) = line.strip_prefix("Threads:") {
                s.pids = v.trim().parse().unwrap_or(0);
            }
        }
    }
    if let (Ok(stat), Some(started)) = (
        std::fs::read_to_string(format!("/proc/{pid}/stat")),
        started,
    ) {
        // Fields after the `(comm)`: state is field 3; utime=14, stime=15.
        if let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) {
            let f: Vec<&str> = rest.split_whitespace().collect();
            let ticks = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            let cpu_ticks = ticks(11) + ticks(12);
            // SAFETY: sysconf has no preconditions.
            let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            let elapsed = started.elapsed().as_secs_f64();
            if hz > 0 && elapsed > 0.0 {
                s.cpu_percent = ((cpu_ticks as f64 / hz as f64) / elapsed * 100.0) as f32;
            }
        }
    }
    s
}

#[async_trait]
impl Runtime for ProcessRuntime {
    async fn ping(&self) -> Result<RuntimeInfo> {
        Ok(RuntimeInfo {
            name: "process".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            api_version: None,
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            endpoint: "local".into(),
        })
    }

    async fn ensure_image(&self, _image: &str) -> Result<()> {
        Ok(())
    }

    async fn create(&self, spec: &ContainerSpec) -> Result<String> {
        spec.validate()?;
        if spec.cmd.is_empty() {
            return Err(SandboxError::InvalidSpec(
                "ProcessRuntime needs an explicit cmd".into(),
            ));
        }
        let mut g = self.procs.lock().unwrap_or_else(|e| e.into_inner());
        if g.values().any(|p| p.spec.name == spec.name) {
            return Err(SandboxError::InvalidSpec(format!(
                "name {} already in use",
                spec.name
            )));
        }
        let id = format!(
            "proc-{}-{}",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed) + 1
        );
        let (exit_tx, _) = watch::channel(None);
        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        g.insert(
            id.clone(),
            Proc {
                spec: spec.clone(),
                pid: None,
                started: None,
                created,
                exit_tx,
                logs: Default::default(),
            },
        );
        Ok(id)
    }

    async fn start(&self, id: &str) -> Result<()> {
        let (spec, logs, exit_tx) = self.with_proc(id, |p| {
            (
                p.started.is_none().then(|| p.spec.clone()),
                p.logs.clone(),
                p.exit_tx.clone(),
            )
        })?;
        let Some(spec) = spec else { return Ok(()) }; // already started
        let mounts = &spec.mounts;
        let mut cmd = Command::new(map_path(&spec.cmd[0], mounts));
        cmd.args(spec.cmd[1..].iter().map(|a| map_path(a, mounts)));
        for (k, v) in &spec.env {
            cmd.env(k, map_path(v, mounts));
        }
        if let Some(run) = host_of(mounts, paths::RUN_DIR) {
            cmd.env("KILN_RUN_DIR", run);
        }
        let ws = host_of(mounts, paths::WORKSPACE);
        if let Some(ws) = &ws {
            cmd.env("KILN_WORKSPACE", ws);
        }
        if let Some(wd) = spec.workdir.as_deref().map(|w| map_path(w, mounts)).or(ws) {
            cmd.current_dir(wd);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = cmd.spawn()?;
        let pid = child.id();
        if let Some(o) = child.stdout.take() {
            pump(o, logs.clone());
        }
        if let Some(e) = child.stderr.take() {
            pump(e, logs);
        }
        self.with_proc(id, |p| {
            p.pid = pid;
            p.started = Some(Instant::now());
        })?;
        tokio::spawn(async move {
            let code = match child.wait().await {
                Ok(st) => exit_code(st),
                Err(_) => -1,
            };
            exit_tx.send_replace(Some(code));
        });
        Ok(())
    }

    async fn wait(&self, id: &str) -> Result<i64> {
        let mut rx = self.exit_rx(id)?;
        let started = self.with_proc(id, |p| p.started.is_some())?;
        if !started {
            return Err(SandboxError::InvalidSpec(format!("{id} was never started")));
        }
        let code = rx
            .wait_for(Option::is_some)
            .await
            .map_err(|_| SandboxError::NotFound(id.to_string()))?
            .unwrap_or(-1);
        Ok(code)
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<()> {
        let Ok((pid, mut rx)) = self.with_proc(id, |p| (p.pid, p.exit_tx.subscribe())) else {
            return Ok(());
        };
        let Some(pid) = pid else { return Ok(()) };
        if rx.borrow().is_some() {
            return Ok(());
        }
        signal_group(pid, libc::SIGTERM);
        if tokio::time::timeout(grace, rx.wait_for(Option::is_some))
            .await
            .is_err()
        {
            signal_group(pid, libc::SIGKILL);
            let _ =
                tokio::time::timeout(Duration::from_secs(5), rx.wait_for(Option::is_some)).await;
        }
        // Also reap anything left in the group (the leader may have exited first).
        signal_group(pid, libc::SIGKILL);
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<()> {
        if self.is_running(id).await.unwrap_or(false) {
            self.stop(id, Duration::from_millis(100)).await?;
        }
        self.procs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        Ok(())
    }

    async fn is_running(&self, id: &str) -> Result<bool> {
        match self.with_proc(id, |p| p.started.is_some() && p.exit_tx.borrow().is_none()) {
            Ok(r) => Ok(r),
            Err(SandboxError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn list_managed(&self) -> Result<Vec<ManagedContainer>> {
        let g = self.procs.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<ManagedContainer> = g
            .iter()
            .map(|(id, p)| {
                let exited = p.exit_tx.borrow().is_some();
                let state = match (p.started.is_some(), exited) {
                    (false, _) => "created",
                    (true, false) => "running",
                    (true, true) => "exited",
                };
                ManagedContainer {
                    id: id.clone(),
                    name: p.spec.name.clone(),
                    image: p.spec.image.clone(),
                    labels: p.spec.effective_labels(),
                    running: state == "running",
                    state: state.into(),
                    created: Some(p.created),
                }
            })
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(v)
    }

    async fn stats(&self, id: &str) -> Result<ContainerStats> {
        let (pid, started, running) =
            self.with_proc(id, |p| (p.pid, p.started, p.exit_tx.borrow().is_none()))?;
        match pid {
            Some(pid) if running => Ok(proc_stats(pid, started)),
            _ => Ok(ContainerStats::default()),
        }
    }

    async fn create_network(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    async fn remove_network(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    async fn logs_tail(&self, id: &str, lines: usize) -> Result<String> {
        let logs = self.with_proc(id, |p| p.logs.clone())?;
        let g = logs.lock().unwrap_or_else(|e| e.into_inner());
        let skip = g.len().saturating_sub(lines);
        let mut out = g.iter().skip(skip).cloned().collect::<Vec<_>>().join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        Ok(out)
    }

    async fn exec(&self, id: &str, cmd: &[String]) -> Result<ExecOutput> {
        let spec = self.with_proc(id, |p| p.spec.clone())?;
        let Some(prog) = cmd.first() else {
            return Err(SandboxError::InvalidSpec("empty exec command".into()));
        };
        let mounts = &spec.mounts;
        let mut c = Command::new(map_path(prog, mounts));
        c.args(cmd[1..].iter().map(|a| map_path(a, mounts)));
        for (k, v) in &spec.env {
            c.env(k, map_path(v, mounts));
        }
        let ws = host_of(mounts, paths::WORKSPACE);
        if let Some(wd) = spec.workdir.as_deref().map(|w| map_path(w, mounts)).or(ws) {
            c.current_dir(wd);
        }
        c.stdin(Stdio::null());
        let out = c.output().await?;
        let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
        output.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok(ExecOutput {
            exit_code: exit_code(out.status),
            output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn path_mapping() {
        let m = vec![
            Mount::rw("/data/runs/r1", "/run/kiln"),
            Mount::rw("/data/wt/FP-1", "/workspace"),
            Mount::rw("/data/runs/r1/transcripts/claude", "/run/kiln/x"),
        ];
        assert_eq!(
            map_path("/run/kiln/kiln.sock", &m),
            "/data/runs/r1/kiln.sock"
        );
        assert_eq!(map_path("/run/kiln", &m), "/data/runs/r1");
        assert_eq!(map_path("/run/kilnx", &m), "/run/kilnx");
        assert_eq!(
            map_path("/run/kiln/x/a", &m),
            "/data/runs/r1/transcripts/claude/a"
        );
        assert_eq!(map_path("/workspace/src", &m), "/data/wt/FP-1/src");
        assert_eq!(map_path("plain", &m), "plain");
        assert_eq!(
            host_of(&m, "/workspace"),
            Some(
                PathBuf::from("/data/wt/FP-1")
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }
}
