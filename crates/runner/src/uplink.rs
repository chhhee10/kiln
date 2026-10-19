//! Connection to the daemon: handshake, sending, acks, reconnect + resend.
//!
//! One background task owns the socket. Events arrive already spooled; while
//! the link is down they are simply not sent. After a reconnect the task
//! resends every spooled event with `seq > last ack`, every artifact message
//! and a pending `Complete`. Reconnects use exponential backoff (100 ms
//! doubling, capped at 5 s) until the reconnect budget (default 60 s) is
//! spent; then the link is declared dead and the runner keeps spooling and
//! writes `out/outcome.json` for the daemon to recover.

use crate::env::RunnerEnv;
use crate::lines::{LineReader, write_json};
use crate::spool::Spool;
use kiln_types::event::RunEvent;
use kiln_types::protocol::{DaemonMessage, PROTOCOL_VERSION, RunConfig, RunOutcome, RunnerMessage};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(15);
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// After `Complete` is written, wait this long for the daemon to close.
const COMPLETE_LINGER: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("socket error: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon rejected the runner: {0}")]
    Rejected(String),
    #[error("daemon closed the connection during the handshake")]
    Closed,
    #[error("timed out")]
    Timeout,
}

/// An authenticated connection.
pub struct Conn {
    pub reader: LineReader<BufReader<OwnedReadHalf>>,
    pub writer: OwnedWriteHalf,
}

impl Conn {
    pub async fn send(&mut self, msg: &RunnerMessage) -> std::io::Result<()> {
        write_json(&mut self.writer, msg).await
    }
}

/// Connect, send `Hello`, and wait for `Welcome`.
pub async fn handshake(env: &RunnerEnv) -> Result<(Conn, RunConfig), LinkError> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&env.socket))
        .await
        .map_err(|_| LinkError::Timeout)??;
    let (r, w) = stream.into_split();
    let mut conn = Conn {
        reader: LineReader::new(BufReader::new(r)),
        writer: w,
    };
    conn.send(&RunnerMessage::Hello {
        run_id: env.run_id,
        token: env.token.clone(),
        runner_version: env!("CARGO_PKG_VERSION").into(),
        protocol: PROTOCOL_VERSION,
    })
    .await?;
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let line = tokio::time::timeout_at(deadline, conn.reader.next_line())
            .await
            .map_err(|_| LinkError::Timeout)??;
        let Some(line) = line else {
            return Err(LinkError::Closed);
        };
        match serde_json::from_str::<DaemonMessage>(&line) {
            Ok(DaemonMessage::Welcome { config }) => return Ok((conn, *config)),
            Ok(DaemonMessage::Reject { reason }) => return Err(LinkError::Rejected(reason)),
            Ok(_) => continue,
            Err(e) => tracing::warn!(error = %e, "unparseable daemon line during handshake"),
        }
    }
}

/// Handshake with exponential backoff, giving up after `budget`.
pub async fn connect_with_backoff(
    env: &RunnerEnv,
    budget: Duration,
) -> Result<(Conn, RunConfig), LinkError> {
    let start = Instant::now();
    let mut delay = BACKOFF_START;
    loop {
        match handshake(env).await {
            Ok(ok) => return Ok(ok),
            Err(e @ LinkError::Rejected(_)) => return Err(e),
            Err(e) => {
                if start.elapsed() + delay > budget {
                    return Err(e);
                }
                tracing::info!(error = %e, retry_in_ms = delay.as_millis() as u64, "daemon not reachable yet");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// Daemon → runner commands surfaced to the run loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    Stop(String),
    Feedback(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    Connected,
    Reconnecting,
    /// Reconnect budget exhausted (or rejected). Events are only spooled.
    Dead,
}

enum Out {
    Event(RunEvent),
    Artifact(Box<RunnerMessage>),
    Complete(Box<RunOutcome>, oneshot::Sender<bool>),
}

/// Handle to the background link task.
pub struct Uplink {
    tx: mpsc::UnboundedSender<Out>,
    acked: watch::Receiver<u64>,
    state: watch::Receiver<LinkState>,
}

impl Uplink {
    pub fn spawn(env: RunnerEnv, conn: Conn, control: mpsc::UnboundedSender<Control>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (acked_tx, acked) = watch::channel(0);
        let (state_tx, state) = watch::channel(LinkState::Connected);
        let link = Link {
            env,
            conn: Some(conn),
            acked_tx,
            state_tx,
            control,
            max_sent: 0,
            artifacts: Vec::new(),
            complete: None,
            complete_written_at: None,
            down_since: None,
            next_retry: None,
            delay: BACKOFF_START,
        };
        tokio::spawn(link.run(rx));
        Self { tx, acked, state }
    }

    pub fn send_event(&self, e: RunEvent) {
        let _ = self.tx.send(Out::Event(e));
    }

    pub fn send_artifact(&self, m: RunnerMessage) {
        let _ = self.tx.send(Out::Artifact(Box::new(m)));
    }

    pub fn last_ack(&self) -> u64 {
        *self.acked.borrow()
    }

    pub fn state(&self) -> LinkState {
        *self.state.borrow()
    }

    /// Wait until `seq` is acked, the link dies, or `timeout` passes.
    pub async fn wait_acked(&self, seq: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.last_ack() >= seq {
                return true;
            }
            if self.state() == LinkState::Dead || Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Send `Complete` (reconnecting if needed). True when it was written to
    /// a live connection.
    pub async fn complete(&self, outcome: RunOutcome, timeout: Duration) -> bool {
        let (done_tx, done_rx) = oneshot::channel();
        if self
            .tx
            .send(Out::Complete(Box::new(outcome), done_tx))
            .is_err()
        {
            return false;
        }
        matches!(tokio::time::timeout(timeout, done_rx).await, Ok(Ok(true)))
    }
}

struct Link {
    env: RunnerEnv,
    conn: Option<Conn>,
    acked_tx: watch::Sender<u64>,
    state_tx: watch::Sender<LinkState>,
    control: mpsc::UnboundedSender<Control>,
    max_sent: u64,
    artifacts: Vec<RunnerMessage>,
    complete: Option<(RunnerMessage, Option<oneshot::Sender<bool>>)>,
    complete_written_at: Option<Instant>,
    down_since: Option<Instant>,
    next_retry: Option<Instant>,
    delay: Duration,
}

enum Ev {
    Out(Option<Out>),
    Line(std::io::Result<Option<String>>),
    Retry,
    Heartbeat,
    Linger,
}

async fn read_line(conn: &mut Option<Conn>) -> std::io::Result<Option<String>> {
    match conn {
        Some(c) => c.reader.next_line().await,
        None => std::future::pending().await,
    }
}

impl Link {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Out>) {
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.tick().await;
        let mut inputs_open = true;
        loop {
            let reading = self.conn.is_some();
            let retry_at = self.next_retry;
            let linger_at = self.complete_written_at.map(|t| t + COMPLETE_LINGER);
            let ev = tokio::select! {
                biased;
                o = rx.recv(), if inputs_open => Ev::Out(o),
                l = read_line(&mut self.conn), if reading => Ev::Line(l),
                _ = tokio::time::sleep_until(retry_at.unwrap_or_else(Instant::now)), if retry_at.is_some() => Ev::Retry,
                _ = tokio::time::sleep_until(linger_at.unwrap_or_else(Instant::now)), if linger_at.is_some() => Ev::Linger,
                _ = heartbeat.tick(), if reading && linger_at.is_none() => Ev::Heartbeat,
            };
            let finished = match ev {
                Ev::Out(Some(o)) => self.on_out(o).await,
                Ev::Out(None) => {
                    inputs_open = false;
                    // The run loop is gone without a Complete: nothing more to do.
                    self.complete.is_none()
                }
                Ev::Line(l) => self.on_line(l).await,
                Ev::Retry => self.try_reconnect().await,
                Ev::Heartbeat => {
                    self.write(&RunnerMessage::Heartbeat).await;
                    false
                }
                Ev::Linger => self.finish(true),
            };
            if finished {
                return;
            }
        }
    }

    fn set_state(&self, s: LinkState) {
        self.state_tx.send_if_modified(|cur| {
            let changed = *cur != s;
            *cur = s;
            changed
        });
    }

    fn dead(&self) -> bool {
        *self.state_tx.borrow() == LinkState::Dead
    }

    /// Write one message; on failure drop the connection and schedule a reconnect.
    async fn write(&mut self, msg: &RunnerMessage) -> bool {
        let Some(c) = self.conn.as_mut() else {
            return false;
        };
        match c.send(msg).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "daemon connection lost while sending");
                self.disconnect();
                false
            }
        }
    }

    fn disconnect(&mut self) {
        self.conn = None;
        self.complete_written_at = None;
        if self.dead() {
            return;
        }
        self.set_state(LinkState::Reconnecting);
        let now = Instant::now();
        self.down_since.get_or_insert(now);
        self.delay = BACKOFF_START;
        self.next_retry = Some(now + self.delay);
    }

    fn finish(&mut self, delivered: bool) -> bool {
        if let Some((_, Some(done))) = self.complete.take() {
            let _ = done.send(delivered);
        }
        true
    }

    async fn on_out(&mut self, o: Out) -> bool {
        match o {
            Out::Event(e) => {
                if e.seq > self.max_sent && self.conn.is_some() {
                    let seq = e.seq;
                    if self.write(&RunnerMessage::Event { event: e }).await {
                        self.max_sent = seq;
                    }
                }
                false
            }
            Out::Artifact(m) => {
                if self.conn.is_some() {
                    self.write(&m).await;
                }
                self.artifacts.push(*m);
                false
            }
            Out::Complete(outcome, done) => {
                self.complete = Some((RunnerMessage::Complete { outcome: *outcome }, Some(done)));
                if self.conn.is_some() {
                    self.send_complete().await;
                    false
                } else if self.dead() {
                    // One last attempt before giving up for good.
                    self.reconnect_once().await;
                    if self.conn.is_some() {
                        false
                    } else {
                        self.finish(false)
                    }
                } else {
                    false // reconnect loop will deliver it
                }
            }
        }
    }

    async fn send_complete(&mut self) {
        let Some((msg, _)) = &self.complete else {
            return;
        };
        let msg = msg.clone();
        if self.write(&msg).await {
            if let Some(c) = self.conn.as_mut() {
                let _ = c.writer.shutdown().await;
            }
            self.complete_written_at = Some(Instant::now());
        }
    }

    async fn on_line(&mut self, l: std::io::Result<Option<String>>) -> bool {
        let line = match l {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => {
                if self.complete_written_at.is_some() {
                    return self.finish(true);
                }
                tracing::warn!("daemon connection closed");
                self.disconnect();
                return false;
            }
        };
        match serde_json::from_str::<DaemonMessage>(&line) {
            Ok(DaemonMessage::Ack { seq }) => {
                self.acked_tx.send_if_modified(|a| {
                    let up = seq > *a;
                    if up {
                        *a = seq;
                    }
                    up
                });
            }
            Ok(DaemonMessage::Stop { reason }) => {
                let _ = self.control.send(Control::Stop(reason));
            }
            Ok(DaemonMessage::Feedback { text }) => {
                let _ = self.control.send(Control::Feedback(text));
            }
            Ok(DaemonMessage::Reject { reason }) => {
                tracing::error!(%reason, "daemon rejected the runner");
                let _ = self
                    .control
                    .send(Control::Stop(format!("rejected by daemon: {reason}")));
                self.conn = None;
                self.next_retry = None;
                self.set_state(LinkState::Dead);
                if self.complete.is_some() {
                    return self.finish(false);
                }
            }
            Ok(DaemonMessage::Welcome { .. }) | Ok(DaemonMessage::MemoryResponse { .. }) => {}
            Err(e) => tracing::warn!(error = %e, "unparseable daemon message"),
        }
        false
    }

    /// One handshake attempt; on success resend everything outstanding.
    async fn reconnect_once(&mut self) -> Option<LinkError> {
        match handshake(&self.env).await {
            Ok((conn, _cfg)) => {
                tracing::info!("reconnected to daemon");
                self.conn = Some(conn);
                self.down_since = None;
                self.next_retry = None;
                self.delay = BACKOFF_START;
                self.set_state(LinkState::Connected);
                self.resend().await;
                None
            }
            Err(e) => Some(e),
        }
    }

    async fn resend(&mut self) {
        let last_ack = *self.acked_tx.borrow();
        for e in Spool::read_after(&self.env.spool_path(), last_ack) {
            let seq = e.seq;
            if !self.write(&RunnerMessage::Event { event: e }).await {
                return;
            }
            self.max_sent = self.max_sent.max(seq);
        }
        for m in self.artifacts.clone() {
            if !self.write(&m).await {
                return;
            }
        }
        if self.complete.is_some() {
            self.send_complete().await;
        }
    }

    async fn try_reconnect(&mut self) -> bool {
        self.next_retry = None;
        match self.reconnect_once().await {
            None => false,
            Some(LinkError::Rejected(reason)) => {
                let _ = self
                    .control
                    .send(Control::Stop(format!("rejected by daemon: {reason}")));
                self.set_state(LinkState::Dead);
                self.complete.is_some() && self.finish(false)
            }
            Some(e) => {
                let down = self.down_since.map(|t| t.elapsed()).unwrap_or_default();
                if down >= self.env.reconnect_budget {
                    tracing::error!(error = %e, "giving up reconnecting; events stay in the spool");
                    self.set_state(LinkState::Dead);
                    return self.complete.is_some() && self.finish(false);
                }
                self.delay = (self.delay * 2).min(BACKOFF_MAX);
                self.next_retry = Some(Instant::now() + self.delay);
                false
            }
        }
    }
}
