# Scheduler & daemon design

The scheduler is the heart of Kiln: it turns approved plans into
Builder → QA → Reviewer pipelines, enforces capacity and fairness, survives
restarts, and feeds memory. The daemon (`kilnd`) wires every crate together
and exposes the HTTP/WS API.

## Scheduler public API (`kiln_scheduler`)

```rust
pub struct SchedulerDeps {
    pub store: Store, pub bus: Bus,
    pub runtime: Arc<dyn Runtime>,          // DockerRuntime in prod, ProcessRuntime in dev/tests
    pub gitops: Arc<GitOps>, pub prs: Arc<dyn PrProvider>,
    pub vault: Arc<Vault>, pub memory: Arc<Memory>, pub notifier: Arc<Notifier>,
    pub harnesses: Arc<kiln_harness::Registry>,
    pub config: SchedulerConfig,
}
pub struct SchedulerConfig {
    pub data_dir: PathBuf,
    pub worker_image: String,               // base image, e.g. "ghcr.io/chhhee10/kiln-worker:0.1.2"
    pub runner_bin: Option<PathBuf>,        // ProcessRuntime: host path of kiln-runner
    pub capacity: Capacity { ram_mb, reserve_mb, max_containers, max_tasks },  // RAM-aware, defaults from host RAM
    pub timeouts: RoleTimeouts { planner, builder, qa, reviewer, distiller },  // minutes
    pub tick: Duration,                     // dispatcher fallback tick (2s)
    pub llm_distill: bool,                  // run the Distiller container with the owner's creds (decision #1)
    pub harness_options: BTreeMap<String, BTreeMap<String,String>>, // per-harness options → RunConfig.options
}
impl Scheduler {
    pub async fn new(deps: SchedulerDeps) -> Result<Arc<Scheduler>>;
    pub async fn start(self: &Arc<Self>) -> Result<()>;           // crash recovery, then spawn loops
    pub async fn shutdown(&self);                                 // stop accepting, let runners keep spooling; containers keep running
    // commands (all permission-checked by the caller AND re-checked here for ownership)
    pub async fn create_thread(&self, user: &User, req: CreateThreadRequest) -> Result<ThreadDetail>;
    pub async fn planner_message(&self, user: &User, thread: ThreadId, req: SendMessageRequest) -> Result<Message>;
    pub async fn update_plan(&self, user: &User, plan: PlanId, req: UpdatePlanRequest) -> Result<Plan>;
    pub async fn approve_plan(&self, user: &User, plan: PlanId, req: ApprovePlanRequest) -> Result<ApprovePlanResponse>;
    pub async fn task_action(&self, user: &User, task: TaskId, action: TaskAction) -> Result<Task>;
    pub async fn refresh_project(&self, project: ProjectId) -> Result<Project>;  // fetch mirror, parse qa.yaml → qa_status
    // reads for the API
    pub fn machine(&self) -> MachineStats;
    pub async fn running_containers(&self) -> Result<Vec<RunningContainer>>;
    pub async fn queue(&self) -> Result<Vec<QueueEntry>>;
}
```

## Loops (all tokio tasks, all stop on `Event::Shutdown`)

| Loop | Trigger | Does |
|---|---|---|
| dispatcher | `Event::SchedulerWake` or tick | admit queued tasks (below) and spawn a `TaskDriver` per admitted task |
| run socket server | per run | `UnixListener` at `<run_dir>/kiln.sock`; Hello token check (sha256 vs `runs.token_hash`); Welcome(RunConfig); events → `store.insert_run_events` (idempotent) + `bus Log` + L0 batch (`memory.ingest_l0`, redacted with the run's `Redactor` + `scan_for_secrets`) → `Ack{seq}`; `MemoryRequest` → gateway as the run owner (project-scoped); `Complete` → outcome. Multiple connections per run are allowed (the `mcp` subprocess opens its own). |
| ingest | `Event::RunFinished` | reconcile `<run_dir>/spool.jsonl` vs stored seqs, parse native transcript (`harness.parse_transcript` over `<run_dir>/transcripts/`), scan+redact, flush to L0, `mark_ingested`, publish `RunIngested`, remove container, enqueue `distill` job. Never delete a run folder before `ingested`. |
| jobs | tick | `distill` (memory.distill → optional Distiller container with owner creds → memory.extract → rebuild_packs → MemoryChanged), `nightly` (03:00 local: memory.nightly, prune run_events > 14d, cleanup dead containers / worktrees of terminal tasks / dangling images / run folders past retention (ingested only)), `poll_prs` (every 5 min: Ready tasks → `prs.pr_state` → `Merged` transition + plan status Merged when all tasks merged). |
| machine sampler | 2 s | `sysinfo` CPU/RAM/disk + runtime stats → `MachineStats` (32-sample CPU history) → `bus Machine` every 2s; health checks (runtime ping, disk < 10% free, queue backlog > 20 or oldest > 30 min) → `SystemAlert` notifications to admins (deduped, at most hourly). |

## Admission control & fair-share

A task is *runnable* when `state == Queued`, every `depends_on` task is `Ready` or
`Merged`, and its owner is active. Each dispatcher pass:

1. Build the per-user FIFO of runnable tasks (by `created_at`).
2. Round-robin across users starting after the user who was admitted last.
3. Admit the head task of a user only if **all** hold:
   - user in-flight tasks < `limits.concurrency`;
   - running containers + 1 ≤ `capacity.max_containers` (≈8 on 32 GB) and in-flight tasks < `max_tasks` (≈4);
   - Σ memory limits of running containers + task's `resources.memory_mb` ≤ `capacity.ram_mb − reserve_mb`;
   - Σ this user's running memory + task's ≤ `limits.ram_share × (ram_mb − reserve_mb)` (skipped when the user is the only one with queued work, so a lone user isn't throttled by their share).
4. Stop when nothing more fits. The pass is cheap; it runs on every wake.

The pipeline re-checks admission before *each* role's container starts (a
task holds its "slot" between roles, but a QA container with sidecars needs
more RAM than the Builder). If a later role doesn't fit, it waits (state
unchanged, `meta` "waiting for capacity").

Dependencies: when B depends on A and A is `Ready` (PR open, not merged), B's
worktree is created from **A's branch** and B's PR targets A's branch
(stacked). When A is `Merged`, B uses the default branch.

## TaskDriver (one per in-flight task)

Every state transition is persisted (`store.transition_task`, CAS) **before**
its side effect, and the driver is fully restartable from the DB state.

```
Queued ─admit→ Preparing:
   mirror fetch; worktree + branch (gitops); qa.yaml parse at the task branch (fallback: project's last good profile);
   pick image (qa.image | base [+ extras image, built & cached as kiln-extras:<hash>]);
   memory.pack(owner, project, role) → AGENTS.md + CLAUDE.md written into the worktree and excluded via info/exclude
→ Building:  run(Builder)  — success requires a new commit on the branch (HEAD changed vs. before the run); otherwise failure
→ Testing:   run(QA)       — qa.services → sidecars on network kiln-run-<run_id> (healthchecks), QA container joins it; report required
→ Reviewing: run(Reviewer) — prompt includes diff_summary + QA report; verdict required
→ Ready:     push branch; open PR (render_pr_body with artifact links `/app/task/<key>`); notify owner (TaskReady); remove worktree
On QA fail / changes_requested → Looping: feedback = planner::feedback_from(qa, verdict, human notes) pushed to task feedback;
   if loop_count + 1 > loop_cap → Escalated (notify TaskEscalated "needs your call") else increment_loop → Building.
On a run failure (crash/timeout/no report): retry that role once (runs.retries); second failure → Escalated (notify TaskFailed).
```

`run(role)`: create run row (state Pending, token hash) → admission recheck →
socket server up → `ContainerSpec` (mounts via `sandbox::mounts_for`, env:
`KILN_RUN_TOKEN`, `KILN_RUN_ID`, `KILN_SOCKET`, user's `dispatch_env`
secrets, `HOME=/home/kiln`; limits from the project profile; labels) →
create/start (Starting → Running) → await `Complete` or container exit or
timeout (`Stop` → grace 10s → runtime.stop) → `finish_run` → publish
`RunFinished` → progress/meta updates on the task along the way (from
`Progress` events, e.g. "QA testing · 2/3").

Progress & meta lines follow the design: Building "Building · attempt n/3",
Testing "QA testing · n/3", Reviewing "Reviewing", Ready "PR #433 ready",
Escalated "Needs you", Failed "Failed · <reason>", Cancelled "Stopped".

## Human actions (`task_action`)

| Action | Allowed from | Effect |
|---|---|---|
| Stop | any active / Queued | `Stop` to the live runner, container stop, → Cancelled, meta "Stopped" |
| Retry | Escalated, Failed, Cancelled | → Queued with loop_count reset only if it reached the cap (attempt+1 up to cap otherwise) |
| Feedback | any non-terminal or Ready/Escalated | push note (+ attach QA report / verdict per flags, + `save_memory` → `memory.propose` Private procedure); if a Builder run is live → stop it; → Looping → Building at attempt+1 (feedback forces a loop even past the cap? no: cap+1 escalates, but a human feedback on an Escalated task resets the loop counter to cap−1 so exactly one more attempt runs) |
| Escalate | active / Queued / Failed | stop live run, → Escalated, notify team admins + owner ("escalated · team notified on Android") |

Permission: owner or admin may act; others get 403.

## Planner sessions

`create_thread` / `planner_message` persist the user message, publish
`PlannerStatus{pending:true,label:"reading company brain · drafting changes"}`,
and enqueue a planner run (respecting admission; planner runs don't count
toward `max_tasks`). Planner container: read-only worktree of the default
branch at `<data>/worktrees/_planner/<project>` (refreshed per run), prompt =
`planner_prompt(project, history, memory pack + top search hits, current plan)`.
On Complete: validate `outcome.plan` (`PlanSpec::validate`); store as a new
plan version (create on first draft, `update_plan_spec` after); add the
planner reply message (with `plan_version`); thread status Drafting; notify
`PlanReady` ("Approve plan: <feature>") with actions Approve · Review.
Invalid plan → reply explains the validation error, no new version.

`approve_plan`: validate; resolve workers per task (`PlanWorkers::resolve`
with the user's defaults); `create_tasks_for_plan`; tasks start Queued (the
dispatcher immediately admits what fits → first one Building, rest Planned);
plan Dispatched; thread Dispatched; wake dispatcher. Summary:
"3 tasks dispatched · 1 building, 2 queued".

## Crash recovery (`start`)

1. `requeue_stale_jobs`.
2. Runs in Starting/Running: container alive → **re-attach** (recreate socket listener at the same path; the runner reconnects and resends from its spool; restart the TaskDriver from the task's DB state awaiting that run). Container gone → read `<run_dir>/out/outcome.json` if the runner wrote one (complete normally), else mark `Lost`; retry once (state back to the role's start), else escalate.
3. Tasks in Preparing → Queued (preparation is idempotent).
4. Managed containers with no live run row → remove (orphans). Networks `kiln-run-*` without live runs → remove.
5. Runs finished but not ingested → enqueue ingest.

A daemon restart mid-run must lose no task state, events or transcripts.

## Daemon (`kilnd`)

- **Config** `kiln.toml` (path: `--config`, `$KILN_CONFIG`, `<data>/kiln.toml`); env overrides `KILN_*`.
  ```toml
  data_dir = "~/Library/Application Support/kiln"
  [server]  bind = "tailscale"  # or "127.0.0.1:7420" ; port = 7420 ; public_url = "https://mac-mini.tailnet.ts.net"
  [runtime] kind = "docker"     # docker | process ; worker_image = "kiln-worker:0.1.2"
  [capacity] ram_mb = 0 (auto) ; reserve_mb = 6144 ; max_containers = 8 ; max_tasks = 4
  [github] mode = "app" | "token" | "local" ; app_id ; private_key_path ; token_env = "GITHUB_TOKEN"
  [harnesses.claude_sdk] label="Claude Agent SDK" models=["claude-opus-4-1", "claude-sonnet-4-5", ...]
  [harnesses.codex] ... ; [harnesses.opencode] experimental = true ; [harnesses.mock] (dev only)
  [defaults] builder = "claude_sdk:claude-sonnet-4-5" ; qa = "codex:gpt-5-codex" ; reviewer = "codex:gpt-5"
  [memory] llm_distill = true ; retention_days = 75
  [push] ntfy_url = "https://ntfy.mac-mini.ts.net" (optional)
  ```
- **CLI**: `kilnd serve [--dev] [--demo-seed]`, `kilnd install` (launchd plist `~/Library/LaunchAgents/dev.kiln.kilnd.plist` with KeepAlive/RunAtLoad, or systemd `--user` unit; prints host-prep tips: `sudo pmset -a sleep 0 autorestart 1`, dedicated non-admin user), `kilnd doctor` (runtime, keychain, tailscale, disk, image present, GitHub auth), `kilnd version`.
- **Binding**: `bind = "tailscale"` → `tailscale ip -4` (or interface `utun*`/`tailscale0` address); refuse to start if missing unless `--dev` (127.0.0.1). Never 0.0.0.0.
- **HTTP**: every route in `kiln_types::api` docs. Auth extractor: `Authorization: Bearer` or `kiln_session` cookie (Secure, HttpOnly, SameSite=Strict); WS also accepts `?token=`. Permission rules:
  - board/task cards: visible to every active user (team board, with user filter);
  - task logs, artifacts, transcripts, QA report bodies, verdicts: **owner only** (admins see metadata, not content) — "never read another user's artifacts";
  - memory: gateway enforces visibility on every read;
  - credentials: owner only, never returned decrypted;
  - admin routes: admin only.
- **WS fan-out**: per connection, filter `ServerEvent`s: Task/Run upserts to everyone (cards are team-visible) but strip `meta`-level details only; `Log` only to the task owner and only if followed; Plan/Thread/Message/PlannerStatus to the owner; Notification to its user; Memory upserts only if `visible_to`; Machine to all; Toast to its originator.
- **Static**: `/app/*` from `web_dir` (default `<data>/web`, dev fallback `app/dist`), SPA fallback to index.html; `/download` HTML page listing `<data>/downloads/<version>/*`; `/healthz`.
- **Seeding** (`--demo-seed`, dev only): creates admin `admin@<domain>` + 3 members, 3 projects backed by local repos, mock harness defaults — for the end-to-end demo and tests.
