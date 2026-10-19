//! Per-run Unix socket server speaking the runner protocol
//! (`kiln_types::protocol`).
//!
//! * The listener lives at `<run_dir>/kiln.sock` (mounted at
//!   `/run/kiln/kiln.sock`). A stale socket file from a previous daemon is
//!   replaced, which is how re-attach after a restart works: the runner keeps
//!   reconnecting and resends everything after its last ack.
//! * Every connection must start with `Hello`; the token's sha256 is
//!   compared with `runs.token_hash`. Several connections per run are
//!   normal (the memory MCP subprocess opens its own).
//! * `Event` → redact → `insert_run_events` (idempotent on seq) → `Ack`.
//!   Side effects (bus `Log`, progress, usage, L0 batch) only happen for
//!   newly inserted rows, so resends are harmless.
//! * `MemoryRequest` → the memory gateway as the run owner.
//! * `Complete` → the outcome is handed to the run supervisor.

use crate::Scheduler;
use crate::state::{LiveRun, lock};
use crate::util::redact_event;
use kiln_events::Event;
use kiln_store::NewArtifact;
use kiln_types::event::{LogLine, RunEvent, RunEventKind, ServerEvent};
use kiln_types::model::ArtifactKind;
use kiln_types::protocol::{DaemonMessage, MemoryCall, PROTOCOL_VERSION, RunnerMessage};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// `sun_path` is 104 bytes on macOS and 108 on Linux (incl. NUL).
const MAX_SOCKET_PATH: usize = 103;
/// Flush buffered events to L0 at least this often.
const L0_FLUSH_EVERY: Duration = Duration::from_secs(1);
/// …or when this many events are buffered.
const L0_FLUSH_BATCH: usize = 64;
/// Longest accepted protocol line (a runner never sends more).
const MAX_LINE: usize = 8 * 1024 * 1024;

fn mime_for(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "webm" => "video/webm",
        "mp4" => "video/mp4",
        "json" => "application/json",
        "jsonl" => "application/x-ndjson",
        "html" | "htm" => "text/html",
        "md" => "text/markdown",
        "txt" | "log" | "out" | "diff" | "patch" => "text/plain",
        "xml" => "application/xml",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

impl Scheduler {
    /// Bind `<run_dir>/kiln.sock` and serve `live`. Replaces a stale file.
    pub(crate) fn serve_run(self: &Arc<Self>, live: Arc<LiveRun>) -> crate::Result<()> {
        let path = self.run_dir(live.run_id).join("kiln.sock");
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(crate::SchedulerError::Invalid(format!(
                "run socket path is {} bytes, over the {MAX_SOCKET_PATH}-byte Unix socket limit; use a shorter data dir",
                path.as_os_str().len()
            )));
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(&path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        let s = self.clone();
        let l = live.clone();
        let accept = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let (s2, l2) = (s.clone(), l.clone());
                        let h = tokio::spawn(async move { s2.handle_conn(l2, stream).await });
                        let mut t = lock(&l.tasks);
                        t.retain(|h| !h.is_finished());
                        t.push(h);
                    }
                    Err(e) => {
                        tracing::warn!(run = %l.run_id, error = %e, "run socket accept failed");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
        let s = self.clone();
        let l = live.clone();
        let flusher = tokio::spawn(async move {
            loop {
                tokio::time::sleep(L0_FLUSH_EVERY).await;
                s.flush_l0(&l).await;
            }
        });
        let mut t = lock(&live.tasks);
        t.push(accept);
        t.push(flusher);
        Ok(())
    }

    async fn handle_conn(self: Arc<Self>, live: Arc<LiveRun>, stream: UnixStream) {
        let (r, mut w) = stream.into_split();
        let mut reader = BufReader::new(r);
        let mut control = live.control.subscribe();
        let mut authed = false;
        let mut buf = Vec::new();
        loop {
            // `read_line` is cancel-safe: partial input stays in `buf`.
            let line = tokio::select! {
                n = read_line(&mut reader, &mut buf) => match n {
                    Ok(true) => {
                        let l = String::from_utf8_lossy(&buf).into_owned();
                        buf.clear();
                        l
                    }
                    Ok(false) => return,
                    Err(e) => {
                        tracing::debug!(run = %live.run_id, error = %e, "run connection closed");
                        return;
                    }
                },
                m = control.recv(), if authed => {
                    match m {
                        Ok(m) => {
                            if send(&mut w, &m).await.is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => return,
                    }
                    continue;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg: RunnerMessage = match serde_json::from_str(line) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(run = %live.run_id, error = %e, "unparseable runner message");
                    continue;
                }
            };
            match msg {
                RunnerMessage::Hello {
                    run_id,
                    token,
                    protocol,
                    ..
                } => {
                    let ok =
                        run_id == live.run_id && kiln_auth::token_hash(&token) == live.token_hash;
                    if !ok {
                        tracing::warn!(run = %live.run_id, "runner handshake rejected (bad token or run id)");
                        let _ = send(
                            &mut w,
                            &DaemonMessage::Reject {
                                reason: "bad token".into(),
                            },
                        )
                        .await;
                        return;
                    }
                    if protocol != PROTOCOL_VERSION {
                        tracing::warn!(run = %live.run_id, protocol, "runner protocol mismatch; continuing");
                    }
                    authed = true;
                    let welcome = DaemonMessage::Welcome {
                        config: Box::new(live.config.clone()),
                    };
                    if send(&mut w, &welcome).await.is_err() {
                        return;
                    }
                    // A stop requested while the runner was disconnected.
                    let pending_stop = lock(&live.stop_reason).clone();
                    if let Some((_, reason)) = pending_stop
                        && send(&mut w, &DaemonMessage::Stop { reason }).await.is_err()
                    {
                        return;
                    }
                }
                _ if !authed => {
                    let _ = send(
                        &mut w,
                        &DaemonMessage::Reject {
                            reason: "hello first".into(),
                        },
                    )
                    .await;
                    return;
                }
                RunnerMessage::Event { event } => {
                    let seq = event.seq;
                    match self.on_event(&live, event).await {
                        Ok(()) => {
                            if send(&mut w, &DaemonMessage::Ack { seq }).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            // Not acked: the runner resends it later.
                            tracing::warn!(run = %live.run_id, seq, error = %e, "could not persist run event");
                        }
                    }
                }
                RunnerMessage::Artifact {
                    path,
                    kind,
                    caption,
                    size,
                } => {
                    self.on_artifact(&live, path, kind, caption, size).await;
                }
                RunnerMessage::MemoryRequest { req_id, call } => {
                    let result = self.memory_call(&live, call).await;
                    if send(&mut w, &DaemonMessage::MemoryResponse { req_id, result })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                RunnerMessage::Heartbeat => {}
                RunnerMessage::Complete { outcome } => {
                    self.flush_l0(&live).await;
                    let mut o = outcome;
                    // Scrub free text the runner produced (summary/errors/reply).
                    o.summary = live.redactor.scrub(&o.summary);
                    o.error = o.error.map(|e| live.redactor.scrub(&e));
                    o.reply = o.reply.map(|e| live.redactor.scrub(&e));
                    live.outcome.send_replace(Some(o));
                    let _ = w.shutdown().await;
                    return;
                }
            }
        }
    }

    /// Persist one event; side effects only for newly inserted rows.
    async fn on_event(&self, live: &LiveRun, event: RunEvent) -> crate::Result<()> {
        if event.run_id != live.run_id {
            tracing::warn!(run = %live.run_id, other = %event.run_id, "event for another run dropped");
            return Ok(());
        }
        let ev = redact_event(&event, &live.redactor);
        let inserted = self
            .store
            .insert_run_events(&[(ev.clone(), live.role)])
            .await?;
        if inserted == 0 {
            return Ok(());
        }
        let (level, message) = ev.kind.log_line();
        self.bus.server(ServerEvent::Log {
            task_id: live.task_id,
            thread_id: live.thread_id,
            line: LogLine {
                run_id: ev.run_id,
                seq: ev.seq,
                ts: ev.ts,
                role: live.role,
                level,
                message,
            },
        });
        match &ev.kind {
            RunEventKind::Progress { percent, .. } => {
                let p = (*percent).min(100);
                let _ = self.store.set_run_progress(live.run_id, p).await;
                if let Some(t) = live.task_id
                    && let Ok(task) = self.store.get_task(t).await
                    && task.state == live.role.task_state().unwrap_or(task.state)
                    && task.state.is_active()
                    && let Ok(task) = self.store.update_task_progress(t, p, &task.meta).await
                {
                    self.publish_task(&task);
                }
            }
            RunEventKind::Usage {
                tokens_in,
                tokens_out,
                cost_usd,
            } => {
                let _ = self
                    .store
                    .add_run_usage(live.run_id, *tokens_in, *tokens_out, *cost_usd)
                    .await;
            }
            _ => {}
        }
        let flush = {
            let mut b = lock(&live.l0_buf);
            b.push(ev);
            b.len() >= L0_FLUSH_BATCH
        };
        if flush {
            self.flush_l0(live).await;
        }
        Ok(())
    }

    async fn on_artifact(
        &self,
        live: &LiveRun,
        path: String,
        kind: ArtifactKind,
        caption: Option<String>,
        size: u64,
    ) {
        let rel = path.trim_start_matches('/').to_string();
        if rel.contains("..") {
            tracing::warn!(run = %live.run_id, "artifact path with '..' ignored");
            return;
        }
        let new = NewArtifact {
            run_id: live.run_id,
            kind,
            mime: mime_for(&rel).into(),
            path: rel,
            size,
            caption: caption.map(|c| live.redactor.scrub(&c)),
        };
        if let Err(e) = self.store.insert_artifact(new).await {
            tracing::warn!(run = %live.run_id, error = %e, "could not index artifact");
        }
    }

    /// Flush buffered events to L0 (redacted again inside, idempotent).
    pub(crate) async fn flush_l0(&self, live: &LiveRun) {
        let _g = live.l0_lock.lock().await;
        let events: Vec<RunEvent> = std::mem::take(&mut *lock(&live.l0_buf));
        if events.is_empty() {
            return;
        }
        let run = match self.store.get_run(live.run_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(run = %live.run_id, error = %e, "L0 flush: run missing");
                return;
            }
        };
        let r = live.redactor.clone();
        let redact = move |s: &str| r.scrub(s);
        if let Err(e) = self
            .memory
            .ingest_l0(&run, live.project_id, &events, &redact)
            .await
        {
            // Not fatal: the ingest loop reconciles L0 from the spool.
            tracing::warn!(run = %live.run_id, error = %e, "L0 flush failed; ingest will reconcile");
        }
    }

    async fn memory_call(
        &self,
        live: &LiveRun,
        call: MemoryCall,
    ) -> Result<serde_json::Value, String> {
        let ctx = kiln_memory::UseContext {
            run: Some(live.run_id),
            task: live.task_id,
            role: Some(live.role),
        };
        let r = match call {
            MemoryCall::SearchMemory { query, limit } => {
                self.memory
                    .mcp_search(live.owner, live.project_id, &query, limit, &ctx)
                    .await
            }
            MemoryCall::ProposeMemory { proposal } => {
                let mut p = proposal;
                if p.project_id.is_none() {
                    p.project_id = live.project_id;
                }
                let r = self
                    .memory
                    .mcp_propose(live.owner, Some(live.run_id), p)
                    .await;
                if r.is_ok() {
                    self.bus.publish(Event::MemoryChanged {
                        owner_id: live.owner,
                        project_id: live.project_id,
                    });
                }
                r
            }
        };
        r.map_err(|e| e.to_string())
    }
}

/// Read one `\n`-terminated line into `buf` (bounded). `Ok(false)` on EOF.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<bool> {
    loop {
        let available = r.fill_buf().await?;
        if available.is_empty() {
            return Ok(!buf.is_empty());
        }
        if let Some(i) = available.iter().position(|b| *b == b'\n') {
            buf.extend_from_slice(&available[..i]);
            r.consume(i + 1);
            return Ok(true);
        }
        let n = available.len();
        buf.extend_from_slice(available);
        r.consume(n);
        if buf.len() > MAX_LINE {
            return Err(std::io::Error::other("runner line too long"));
        }
    }
}

async fn send(w: &mut tokio::net::unix::OwnedWriteHalf, m: &DaemonMessage) -> std::io::Result<()> {
    let mut s = serde_json::to_string(m).map_err(std::io::Error::other)?;
    s.push('\n');
    w.write_all(s.as_bytes()).await?;
    w.flush().await
}
