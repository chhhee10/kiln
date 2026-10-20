//! Dispatcher: admission control, fair-share and dependency gating
//! (docs/SCHEDULER.md "Admission control & fair-share").
//!
//! A task is *runnable* when it is Queued, every dependency is Ready or
//! Merged, and its owner is active. Each pass builds per-user FIFOs of
//! runnable tasks, round-robins across users starting after the user
//! admitted last, and admits a user's head task only when the user is under
//! their concurrency cap, the machine has a container slot and a task slot,
//! the task's RAM fits the budget, and (unless the user is the only one with
//! queued work) the user stays within their RAM share. Admission is the
//! persisted `Queued → Preparing` transition; a TaskDriver takes it from
//! there and re-checks capacity before each container.

use crate::Scheduler;
use crate::error::Result;
use crate::sql::Ext;
use crate::state::lock;
use crate::util::meta;
use kiln_store::TaskFilter;
use kiln_types::{
    QueueEntry, RunState, RunningContainer, Task, TaskId, TaskState, User, UserId, UserStatus,
    UserSummary,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;

const ACTIVE: [TaskState; 5] = [
    TaskState::Preparing,
    TaskState::Building,
    TaskState::Testing,
    TaskState::Reviewing,
    TaskState::Looping,
];

/// Admission inputs gathered once per pass.
struct Snapshot {
    /// Runnable tasks per user, FIFO.
    fifo: BTreeMap<UserId, VecDeque<Task>>,
    users: HashMap<UserId, User>,
    inflight_per_user: HashMap<UserId, u32>,
    inflight: u32,
    /// Containers held or projected (in-flight tasks without a container
    /// count as one, so a pass never over-admits).
    containers: u32,
    memory_mb: u64,
    memory_per_user: HashMap<UserId, u64>,
    task_memory: HashMap<TaskId, u64>,
}

impl Scheduler {
    pub(crate) async fn dispatch_loop(self: Arc<Self>) {
        let mut shutdown = self.st.shutdown.subscribe();
        loop {
            if let Err(e) = self.dispatch_pass().await {
                tracing::warn!(error = %e, "dispatch pass failed");
            }
            tokio::select! {
                _ = self.st.wake.notified() => {}
                _ = tokio::time::sleep(self.cfg.tick) => {}
                _ = shutdown.changed() => return,
            }
        }
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        let queued = self
            .store
            .list_tasks(TaskFilter {
                states: vec![TaskState::Queued],
                ..Default::default()
            })
            .await?;
        let inflight = self
            .store
            .list_tasks(TaskFilter {
                states: ACTIVE.to_vec(),
                ..Default::default()
            })
            .await?;
        let mut users: HashMap<UserId, User> = HashMap::new();
        for t in queued.iter().chain(inflight.iter()) {
            if !users.contains_key(&t.owner_id)
                && let Ok(u) = self.store.get_user(t.owner_id).await
            {
                users.insert(u.id, u);
            }
        }
        {
            let mut shares = lock(&self.st.user_shares);
            for u in users.values() {
                shares.insert(u.id, u.limits.ram_share);
            }
        }
        let mut dep_state: HashMap<TaskId, TaskState> = HashMap::new();
        let mut project_mem: HashMap<kiln_types::ProjectId, u64> = HashMap::new();
        let mut task_memory = HashMap::new();
        let mut fifo: BTreeMap<UserId, VecDeque<Task>> = BTreeMap::new();
        for t in queued {
            let Some(u) = users.get(&t.owner_id) else {
                continue;
            };
            if u.status != UserStatus::Active {
                continue;
            }
            let mut ready = true;
            for d in &t.depends_on {
                let s = match dep_state.get(d) {
                    Some(s) => *s,
                    None => {
                        let s = self
                            .store
                            .get_task(*d)
                            .await
                            .map(|x| x.state)
                            .unwrap_or(TaskState::Failed);
                        dep_state.insert(*d, s);
                        s
                    }
                };
                if !matches!(s, TaskState::Ready | TaskState::Merged) {
                    ready = false;
                }
            }
            if !ready {
                continue;
            }
            let mem = match project_mem.get(&t.project_id) {
                Some(m) => *m,
                None => {
                    let m = self
                        .store
                        .get_project(t.project_id)
                        .await
                        .map(|p| p.resources.memory_mb)
                        .unwrap_or(4096);
                    project_mem.insert(t.project_id, m);
                    m
                }
            };
            task_memory.insert(t.id, mem);
            fifo.entry(t.owner_id).or_default().push_back(t);
        }

        let reservations = lock(&self.st.reservations).clone();
        let mut containers = reservations.len() as u32;
        let mut memory_mb: u64 = reservations.values().map(|r| r.memory_mb).sum();
        let mut memory_per_user: HashMap<UserId, u64> = HashMap::new();
        for r in reservations.values() {
            *memory_per_user.entry(r.owner).or_default() += r.memory_mb;
        }
        let mut inflight_per_user: HashMap<UserId, u32> = HashMap::new();
        for t in &inflight {
            *inflight_per_user.entry(t.owner_id).or_default() += 1;
            let holds = reservations.values().any(|r| r.task_id == Some(t.id));
            if !holds {
                let m = match project_mem.get(&t.project_id) {
                    Some(m) => *m,
                    None => self
                        .store
                        .get_project(t.project_id)
                        .await
                        .map(|p| p.resources.memory_mb)
                        .unwrap_or(4096),
                };
                containers += 1;
                memory_mb += m;
                *memory_per_user.entry(t.owner_id).or_default() += m;
            }
        }
        Ok(Snapshot {
            fifo,
            users,
            inflight_per_user,
            inflight: inflight.len() as u32,
            containers,
            memory_mb,
            memory_per_user,
            task_memory,
        })
    }

    /// Round-robin order of users, starting after `last`.
    fn rotation(users: Vec<UserId>, last: Option<UserId>) -> VecDeque<UserId> {
        let mut v: VecDeque<UserId> = users.into_iter().collect();
        if let Some(l) = last
            && let Some(pos) = v.iter().position(|u| *u > l).or(Some(0))
            && !v.is_empty()
        {
            v.rotate_left(pos % v.len());
        }
        v
    }

    /// One admission pass. Returns how many tasks were admitted.
    pub(crate) async fn dispatch_pass(self: &Arc<Self>) -> Result<u32> {
        if self.st.is_shutdown() {
            return Ok(0);
        }
        self.watchdog().await?;
        let mut s = self.snapshot().await?;
        let cap = &self.capacity;
        let budget = cap.budget_mb();
        let lone = s.fifo.len() <= 1;
        let last = *lock(&self.st.last_admitted);
        let mut order = Self::rotation(s.fifo.keys().copied().collect(), last);
        let mut admitted = 0;
        while let Some(user) = order.pop_front() {
            let Some(q) = s.fifo.get_mut(&user) else {
                continue;
            };
            let Some(task) = q.front().cloned() else {
                continue;
            };
            let Some(u) = s.users.get(&user) else {
                continue;
            };
            // Global slots: nothing more fits for anyone.
            if s.inflight >= cap.max_tasks.max(1) || s.containers + 1 > cap.max_containers.max(1) {
                break;
            }
            let mine = s.inflight_per_user.get(&user).copied().unwrap_or(0);
            if mine >= u.limits.concurrency.max(1) {
                continue; // user at their cap: skip them this pass
            }
            let mem = s
                .task_memory
                .get(&task.id)
                .copied()
                .unwrap_or(4096)
                .min(budget);
            if s.memory_mb + mem > budget {
                continue;
            }
            if !lone {
                let share = (u.limits.ram_share.clamp(0.0, 1.0) as f64 * budget as f64) as u64;
                let used = s.memory_per_user.get(&user).copied().unwrap_or(0);
                if used + mem > share.max(mem) && used > 0 {
                    continue;
                }
            }
            q.pop_front();
            match self
                .transition(
                    &task,
                    TaskState::Preparing,
                    "admitted",
                    None,
                    Ext {
                        meta: Some(meta::PREPARING.into()),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(_) => {
                    admitted += 1;
                    s.inflight += 1;
                    s.containers += 1;
                    s.memory_mb += mem;
                    *s.memory_per_user.entry(user).or_default() += mem;
                    *s.inflight_per_user.entry(user).or_default() += 1;
                    *lock(&self.st.last_admitted) = Some(user);
                    tracing::info!(task = %task.key, user = %user, "task admitted");
                    self.ensure_driver(task.id).await;
                }
                Err(e) if e.is_conflict() => {}
                Err(e) => {
                    tracing::warn!(task = %task.key, error = %e, "admission transition failed")
                }
            }
            // Next user's turn; come back to this one after the others.
            if !s.fifo.get(&user).is_none_or(|q| q.is_empty()) {
                order.push_back(user);
            }
        }
        Ok(admitted)
    }

    /// Every in-flight task has a driver (covers restarts and races).
    async fn watchdog(self: &Arc<Self>) -> Result<()> {
        let active = self
            .store
            .list_tasks(TaskFilter {
                states: ACTIVE.to_vec(),
                ..Default::default()
            })
            .await?;
        for t in active {
            self.ensure_driver(t.id).await;
        }
        Ok(())
    }

    pub(crate) async fn do_queue(&self) -> Result<Vec<QueueEntry>> {
        let s = self.snapshot().await?;
        let queued = self
            .store
            .list_tasks(TaskFilter {
                states: vec![TaskState::Queued],
                ..Default::default()
            })
            .await?;
        // Projected order: round-robin over runnable FIFOs, then blocked tasks.
        let last = *lock(&self.st.last_admitted);
        let mut fifo = s.fifo.clone();
        let mut order = Self::rotation(fifo.keys().copied().collect(), last);
        let mut ordered: Vec<Task> = Vec::new();
        while let Some(u) = order.pop_front() {
            if let Some(q) = fifo.get_mut(&u)
                && let Some(t) = q.pop_front()
            {
                ordered.push(t);
                if !q.is_empty() {
                    order.push_back(u);
                }
            }
        }
        for t in queued {
            if !ordered.iter().any(|o| o.id == t.id) {
                ordered.push(t);
            }
        }
        let avg = crate::sql::avg_run_secs(&self.store)
            .await?
            .unwrap_or(600.0)
            .max(30.0)
            * 3.0;
        let slots = self.capacity.max_tasks.max(1) as f64;
        let mut out = Vec::new();
        for (i, t) in ordered.into_iter().enumerate() {
            let owner = match s.users.get(&t.owner_id) {
                Some(u) => UserSummary::from(u),
                None => match self.store.get_user(t.owner_id).await {
                    Ok(u) => UserSummary::from(&u),
                    Err(_) => continue,
                },
            };
            let ahead = i as f64 + s.inflight as f64;
            out.push(QueueEntry {
                position: i as u32 + 1,
                task_id: t.id,
                task_key: t.key.clone(),
                title: t.title.clone(),
                owner,
                est_wait_secs: ((ahead / slots).floor() * avg) as u32,
            });
        }
        Ok(out)
    }

    pub(crate) async fn do_running_containers(&self) -> Result<Vec<RunningContainer>> {
        let runs = self
            .store
            .list_runs_in_states(&[RunState::Starting, RunState::Running])
            .await?;
        let mut out = Vec::new();
        let mut users: HashMap<UserId, UserSummary> = HashMap::new();
        for r in runs {
            let owner = match users.get(&r.owner_id) {
                Some(u) => u.clone(),
                None => {
                    let Ok(u) = self.store.get_user(r.owner_id).await else {
                        continue;
                    };
                    let s = UserSummary::from(&u);
                    users.insert(u.id, s.clone());
                    s
                }
            };
            let (mem_mb, cpu) = match &r.container_id {
                Some(c) if !c.is_empty() => match self.runtime.stats(c).await {
                    Ok(st) => (st.memory_bytes / (1024 * 1024), st.cpu_percent),
                    Err(_) => (0, 0.0),
                },
                _ => (0, 0.0),
            };
            let task_key = match r.task_id {
                Some(t) => self.store.get_task(t).await.ok().map(|t| t.key),
                None => None,
            };
            out.push(RunningContainer {
                run_id: r.id,
                task_id: r.task_id,
                task_key,
                role: r.role,
                owner,
                mem_mb,
                cpu_percent: cpu,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_starts_after_last() {
        let mut ids: Vec<UserId> = (0..3).map(|_| UserId::new()).collect();
        ids.sort();
        let r = Scheduler::rotation(ids.clone(), Some(ids[0]));
        assert_eq!(
            r.iter().copied().collect::<Vec<_>>(),
            vec![ids[1], ids[2], ids[0]]
        );
        let r = Scheduler::rotation(ids.clone(), Some(ids[2]));
        assert_eq!(
            r.iter().copied().collect::<Vec<_>>(),
            vec![ids[0], ids[1], ids[2]]
        );
        let r = Scheduler::rotation(ids.clone(), None);
        assert_eq!(r.iter().copied().collect::<Vec<_>>(), ids);
    }
}
