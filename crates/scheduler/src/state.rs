//! In-memory scheduler state: task handles, live-run registry, capacity
//! reservations, caches. Nothing here is needed to resume after a restart —
//! the database and run folders are authoritative.

use crate::config::SchedulerConfig;
use kiln_types::protocol::{DaemonMessage, RunConfig, RunOutcome};
use kiln_types::{MachineStats, ProjectId, RunId, RunState, TaskId, ThreadId, UserId, WorkerRole};
use kiln_vault::Redactor;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

/// Lock a std mutex, recovering from poisoning (state stays usable).
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A run that passed admission and holds container capacity.
#[derive(Clone, Debug)]
pub(crate) struct Reservation {
    pub owner: UserId,
    pub memory_mb: u64,
    pub task_id: Option<TaskId>,
}

/// How a run ended, as seen by whoever awaits it.
#[derive(Clone, Debug)]
pub(crate) struct Finished {
    pub state: RunState,
    pub outcome: RunOutcome,
}

/// A run whose socket server is up (started, or re-attached after a restart).
pub(crate) struct LiveRun {
    pub run_id: RunId,
    pub owner: UserId,
    pub role: WorkerRole,
    pub task_id: Option<TaskId>,
    pub thread_id: Option<ThreadId>,
    pub project_id: Option<ProjectId>,
    /// Sent as `Welcome` to every authenticated connection.
    pub config: RunConfig,
    /// sha256 hex of the per-run token.
    pub token_hash: String,
    /// Scrubs the owner's injected secrets (+ pattern scan) from events.
    pub redactor: Redactor,
    /// Set when the runner delivers `Complete`.
    pub outcome: watch::Sender<Option<RunOutcome>>,
    /// Daemon → runner messages (Stop, Feedback), fanned out to every connection.
    pub control: broadcast::Sender<DaemonMessage>,
    /// Set by the supervisor once the run is finished in the database.
    pub done: watch::Sender<Option<Finished>>,
    /// Why a human/the scheduler asked the run to stop (`None` = not asked).
    pub stop_reason: Mutex<Option<(RunState, String)>>,
    /// Wakes the supervisor when a stop is requested.
    pub stop_notify: tokio::sync::Notify,
    /// Socket accept loop + connection tasks.
    pub tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Events not yet flushed to L0.
    pub l0_buf: Mutex<Vec<kiln_types::event::RunEvent>>,
    pub l0_lock: tokio::sync::Mutex<()>,
}

impl LiveRun {
    pub fn abort(&self) {
        for t in lock(&self.tasks).drain(..) {
            t.abort();
        }
    }
    /// Ask the runner to stop (graceful; the supervisor escalates to a
    /// container stop after the grace period).
    pub fn request_stop(&self, state: RunState, reason: &str) {
        {
            let mut g = lock(&self.stop_reason);
            if g.is_none() {
                *g = Some((state, reason.to_string()));
            }
        }
        let _ = self.control.send(DaemonMessage::Stop {
            reason: reason.to_string(),
        });
        self.stop_notify.notify_one();
    }
}

pub(crate) struct State {
    pub started: AtomicBool,
    pub shutdown: watch::Sender<bool>,
    pub wake: tokio::sync::Notify,
    /// Notified whenever container capacity frees up.
    pub capacity_freed: tokio::sync::Notify,
    pub machine: std::sync::RwLock<MachineStats>,
    pub self_ref: Mutex<std::sync::Weak<crate::Scheduler>>,
    /// One driver per in-flight task. A tokio mutex: the driver exit
    /// protocol re-reads the task state while holding it.
    pub drivers: tokio::sync::Mutex<HashMap<TaskId, JoinHandle<()>>>,
    pub live: Mutex<HashMap<RunId, Arc<LiveRun>>>,
    pub reservations: Mutex<HashMap<RunId, Reservation>>,
    /// Fair-share cursor: the user admitted last.
    pub last_admitted: Mutex<Option<UserId>>,
    /// Loops and other background tasks (aborted on shutdown).
    pub background: Mutex<Vec<JoinHandle<()>>>,
    /// Threads with a planner turn in progress → "another turn is owed".
    pub planner_threads: Mutex<HashMap<ThreadId, bool>>,
    /// Runs currently being ingested (dedupe).
    pub ingesting: Mutex<HashSet<RunId>>,
    /// System alert key → last sent.
    pub alerts: Mutex<HashMap<String, Instant>>,
    /// CPU history for `MachineStats.cpu_history`.
    pub cpu_history: Mutex<std::collections::VecDeque<f32>>,
    /// `limits.ram_share` per user (refreshed by the dispatcher).
    pub user_shares: Mutex<HashMap<UserId, f32>>,
    /// Serializes refreshes of the shared planner checkouts.
    pub planner_checkout: tokio::sync::Mutex<()>,
}

impl State {
    pub fn new(_cfg: &SchedulerConfig) -> Self {
        Self {
            started: AtomicBool::new(false),
            shutdown: watch::channel(false).0,
            wake: tokio::sync::Notify::new(),
            capacity_freed: tokio::sync::Notify::new(),
            machine: Default::default(),
            self_ref: Mutex::new(std::sync::Weak::new()),
            drivers: tokio::sync::Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            reservations: Mutex::new(HashMap::new()),
            last_admitted: Mutex::new(None),
            background: Mutex::new(Vec::new()),
            planner_threads: Mutex::new(HashMap::new()),
            ingesting: Mutex::new(HashSet::new()),
            alerts: Mutex::new(HashMap::new()),
            cpu_history: Mutex::new(Default::default()),
            user_shares: Mutex::new(HashMap::new()),
            planner_checkout: tokio::sync::Mutex::new(()),
        }
    }

    pub fn is_shutdown(&self) -> bool {
        *self.shutdown.borrow()
    }

    /// Abort every loop, driver and run socket. Containers are untouched.
    pub async fn abort_all(&self) {
        for t in lock(&self.background).drain(..) {
            t.abort();
        }
        // Drivers hold this lock only briefly (exit check), never across a run.
        for (_, t) in self.drivers.lock().await.drain() {
            t.abort();
        }
        for (_, r) in lock(&self.live).drain() {
            r.abort();
        }
        lock(&self.reservations).clear();
    }
}
