//! Internal event bus (PRD §7.11). Publishers call [`Bus::publish`] once per
//! state change; subscribers (WebSocket fan-out, notifier, L0 spool,
//! distiller trigger) each hold their own [`Subscription`].
//!
//! Backed by `tokio::sync::broadcast`. A slow subscriber that falls more than
//! `capacity` events behind gets [`Recv::Lagged`] and must resync from the
//! store; the bus never blocks publishers.

use kiln_types::event::ServerEvent;
use kiln_types::ids::*;
use kiln_types::status::{TaskState, WorkerRole};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Everything that flows on the bus.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // events are shared behind Arc
pub enum Event {
    /// Forwarded to clients (after per-user filtering).
    Server(ServerEvent),
    /// Internal: a task changed state (already persisted).
    TaskTransitioned {
        task_id: TaskId,
        owner_id: UserId,
        from: TaskState,
        to: TaskState,
    },
    /// Internal: a run finished and its outcome is persisted.
    RunFinished {
        run_id: RunId,
        task_id: Option<TaskId>,
        owner_id: UserId,
        role: WorkerRole,
        success: bool,
    },
    /// Internal: a run's events are fully flushed to L0 (`runs.ingested = 1`).
    RunIngested { run_id: RunId, owner_id: UserId },
    /// Internal: L2 changed for this owner/project; L3 packs need a rebuild.
    MemoryChanged {
        owner_id: UserId,
        project_id: Option<ProjectId>,
    },
    /// Internal: a scheduler slot freed up or new work arrived.
    SchedulerWake,
    /// Internal: daemon is shutting down.
    Shutdown,
}

#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Arc<Event>>,
}

impl Bus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish an event. Returns the number of live subscribers.
    pub fn publish(&self, event: Event) -> usize {
        self.tx.send(Arc::new(event)).unwrap_or(0)
    }

    /// Convenience for client-facing events.
    pub fn server(&self, event: ServerEvent) -> usize {
        self.publish(Event::Server(event))
    }

    pub fn subscribe(&self) -> Subscription {
        Subscription {
            rx: self.tx.subscribe(),
        }
    }

    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new(4096)
    }
}

pub struct Subscription {
    rx: broadcast::Receiver<Arc<Event>>,
}

pub enum Recv {
    Event(Arc<Event>),
    /// This many events were dropped; resync from the store.
    Lagged(u64),
    Closed,
}

impl Subscription {
    pub async fn recv(&mut self) -> Recv {
        match self.rx.recv().await {
            Ok(e) => Recv::Event(e),
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(dropped = n, "event bus subscriber lagged");
                Recv::Lagged(n)
            }
            Err(broadcast::error::RecvError::Closed) => Recv::Closed,
        }
    }

    /// Receive the next event, skipping lag notifications. `None` when closed.
    pub async fn next(&mut self) -> Option<Arc<Event>> {
        loop {
            match self.recv().await {
                Recv::Event(e) => return Some(e),
                Recv::Lagged(_) => continue,
                Recv::Closed => return None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fan_out() {
        let bus = Bus::new(16);
        let mut a = bus.subscribe();
        let mut b = bus.subscribe();
        assert_eq!(bus.publish(Event::SchedulerWake), 2);
        assert!(matches!(*a.next().await.unwrap(), Event::SchedulerWake));
        assert!(matches!(*b.next().await.unwrap(), Event::SchedulerWake));
    }

    #[tokio::test]
    async fn lag_is_reported() {
        let bus = Bus::new(2);
        let mut a = bus.subscribe();
        for _ in 0..5 {
            bus.publish(Event::SchedulerWake);
        }
        assert!(matches!(a.recv().await, Recv::Lagged(_)));
    }
}
