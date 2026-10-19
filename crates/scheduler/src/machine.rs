//! Machine sampler (docs/SCHEDULER.md "machine sampler"): host CPU, RAM and
//! disk, runtime and queue → `MachineStats` (32-sample CPU history) → bus
//! `Machine` every `sample_interval`, plus health checks that raise
//! `SystemAlert` notifications to admins (deduplicated, at most hourly).

use crate::Scheduler;
use crate::state::lock;
use kiln_store::{NewNotification, TaskFilter};
use kiln_types::event::ServerEvent;
use kiln_types::{MachineStats, NotificationKind, TaskState, UserRole, UserStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CPU_HISTORY: usize = 32;
const ALERT_EVERY: Duration = Duration::from_secs(3600);
const PING_EVERY: u32 = 5;
const BACKLOG_ALERT: u32 = 20;
const OLDEST_ALERT_SECS: u32 = 30 * 60;
const DISK_FREE_ALERT: f32 = 0.10;

const GB: f32 = 1024.0 * 1024.0 * 1024.0;

impl Scheduler {
    pub(crate) async fn machine_loop(self: Arc<Self>) {
        let mut shutdown = self.st.shutdown.subscribe();
        let mut sys = sysinfo::System::new();
        let mut disks = sysinfo::Disks::new_with_refreshed_list();
        let mut runtime_ok = self.runtime.ping().await.is_ok();
        let mut n: u32 = 0;
        loop {
            n = n.wrapping_add(1);
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            disks.refresh(true);
            if n.is_multiple_of(PING_EVERY) {
                runtime_ok = self.runtime.ping().await.is_ok();
            }
            let stats = self.sample(&sys, &disks, runtime_ok).await;
            *self.st.machine.write().unwrap_or_else(|e| e.into_inner()) = stats.clone();
            self.bus.server(ServerEvent::Machine {
                stats: stats.clone(),
            });
            self.health_checks(&stats).await;
            tokio::select! {
                _ = tokio::time::sleep(self.cfg.sample_interval) => {}
                _ = shutdown.changed() => return,
            }
        }
    }

    async fn sample(
        &self,
        sys: &sysinfo::System,
        disks: &sysinfo::Disks,
        runtime_ok: bool,
    ) -> MachineStats {
        let cpu = sys.global_cpu_usage();
        let history: Vec<f32> = {
            let mut h = lock(&self.st.cpu_history);
            h.push_back(cpu);
            while h.len() > CPU_HISTORY {
                h.pop_front();
            }
            h.iter().copied().collect()
        };
        let data =
            std::fs::canonicalize(&self.cfg.data_dir).unwrap_or_else(|_| self.cfg.data_dir.clone());
        let disk = disks
            .list()
            .iter()
            .filter(|d| data.starts_with(d.mount_point()))
            .max_by_key(|d| d.mount_point().as_os_str().len());
        let (disk_total, disk_free) = disk
            .map(|d| (d.total_space(), d.available_space()))
            .unwrap_or((0, 0));
        let queued = self
            .store
            .list_tasks(TaskFilter {
                states: vec![TaskState::Queued],
                ..Default::default()
            })
            .await
            .unwrap_or_default();
        let oldest = queued
            .iter()
            .map(|t| (chrono::Utc::now() - t.created_at).num_seconds().max(0) as u32)
            .max()
            .unwrap_or(0);
        let containers = lock(&self.st.reservations).len() as u32;
        let hostname = sysinfo::System::host_name().unwrap_or_else(|| "kiln".into());
        let cores = sys.cpus().len();
        let brand = sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_default();
        let ram_total = sys.total_memory() as f32 / GB;
        let uptime = sysinfo::System::uptime();
        let mut spec = vec![hostname.clone()];
        if !brand.is_empty() {
            spec.push(brand);
        }
        spec.push(format!("{cores} cores"));
        spec.push(format!("{:.0} GB", ram_total.round()));
        spec.push(format!("uptime {}d", uptime / 86_400));
        let slots = self.capacity.max_tasks.max(1);
        let est = if queued.is_empty() {
            0
        } else {
            (queued.len() as u32).div_ceil(slots) * 600
        };
        MachineStats {
            hostname,
            spec_line: spec.join(" · "),
            cpu_percent: cpu,
            cpu_history: history,
            ram_used_gb: sys.used_memory() as f32 / GB,
            ram_total_gb: ram_total,
            disk_used_gb: disk_total.saturating_sub(disk_free) as f32 / GB,
            disk_total_gb: disk_total as f32 / GB,
            containers_running: containers,
            container_slots: self.capacity.max_containers,
            queue_depth: queued.len() as u32,
            est_wait_secs: est,
            oldest_queued_secs: oldest,
            uptime_secs: uptime,
            runtime_ok,
            tailnet_name: None,
            latency_ms: None,
        }
    }

    async fn health_checks(&self, s: &MachineStats) {
        if !s.runtime_ok {
            self.alert(
                "runtime",
                "Container runtime unreachable",
                "Kiln can't reach the container runtime; no runs can start.",
            )
            .await;
        }
        if s.disk_total_gb > 0.0 {
            let free = 1.0 - s.disk_used_gb / s.disk_total_gb;
            if free < DISK_FREE_ALERT {
                self.alert(
                    "disk",
                    "Disk almost full",
                    &format!("{:.0}% free on the data volume", free * 100.0),
                )
                .await;
            }
        }
        if s.queue_depth > BACKLOG_ALERT || s.oldest_queued_secs > OLDEST_ALERT_SECS {
            self.alert(
                "backlog",
                "Queue backlog",
                &format!(
                    "{} task(s) queued, oldest waiting {} min",
                    s.queue_depth,
                    s.oldest_queued_secs / 60
                ),
            )
            .await;
        }
    }

    /// Notify every active admin, at most once per hour per `key`.
    pub(crate) async fn alert(&self, key: &str, title: &str, body: &str) {
        {
            let mut a = lock(&self.st.alerts);
            if a.get(key).is_some_and(|t| t.elapsed() < ALERT_EVERY) {
                return;
            }
            a.insert(key.to_string(), Instant::now());
        }
        tracing::warn!(alert = key, "{title}: {body}");
        let Ok(users) = self.store.list_users().await else {
            return;
        };
        for u in users
            .iter()
            .filter(|u| u.role == UserRole::Admin && u.status == UserStatus::Active)
        {
            let mut n = NewNotification::new(u.id, NotificationKind::SystemAlert, title, body);
            n.deep_link = "admin".into();
            if let Err(e) = self.notifier.notify(n).await {
                tracing::warn!(error = %e, "system alert notification failed");
            }
        }
    }
}
