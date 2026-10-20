# Kiln — implementation architecture

Companion to `DECISIONS.md` (assumptions) and `docs/SCHEDULER.md`. This file fixes the **crate boundaries and public APIs** so
crates can be built in parallel. If you change a public API listed here,
update this file in the same change.

## Workspace

```
crates/
  types       pure data, wasm-safe. Domain, API DTOs, events, runner protocol.   [done]
  events      tokio broadcast bus.                                              [done]
  store       SQLite (WAL) via sqlx: migrations + typed queries + job queue.
  auth        argon2 passwords, sessions/tokens, domain allowlist, pairing codes.
  vault       ChaCha20-Poly1305 credential encryption, master key, redaction.
  sandbox     OCI runtime via bollard (Docker/Podman/OrbStack) + a local-process runtime for dev/tests.
  harness     Harness trait + adapters: claude_sdk, codex, opencode, mock.
  runner      in-container binary `kiln-runner` (drives a harness, spools, talks to the daemon socket, MCP memory proxy).
  gitops      bare mirrors, worktrees, branch push, PR providers (GitHub App / token / Local), merge polling.
  memory      gateway, L0–L3, Tantivy, vector index, embedder, distiller/extractor, packs, nightly.
  planner     role prompt templates (planner/builder/qa/reviewer/distiller), plan schema helpers.
  notifier    in-app notifications + UnifiedPush/ntfy delivery, throttling.
  scheduler   queue, admission control, fair-share, deps, task state machine, pipeline, crash recovery.
  daemon      `kilnd`: config, wiring, Axum HTTP/WS API, static web app, /download, service install.
  api-client  typed HTTP + WebSocket client; native (reqwest + tokio-tungstenite) and wasm (fetch + web-sys).
  tui         `kiln` Ratatui client.
app/          Leptos CSR UI (desktop + Android layouts), built with trunk; served by kilnd at /app.
app/src-tauri Tauri 2 shell for desktop + Android (needs WebKitGTK/Android SDK; not in default build).
images/worker Dockerfile for the base worker image (runner + node + Claude Agent SDK + codex + playwright).
```

Dependency direction (no cycles):
`types ← events ← store ← {auth, vault, memory, notifier} ← scheduler ← daemon`;
`types ← {sandbox, harness, gitops, planner}`; `harness ← runner`;
`types ← api-client ← {tui, app}`.

## Conventions

- Rust 2024, `thiserror` for library errors, `anyhow` only in binaries/tests.
- Async: tokio. Traits with async methods use native `async fn` in traits where object
  safety isn't needed, `async-trait` where it is (`dyn Trait`).
- Logging: `tracing`. **Never log secrets.**
- IDs/enums/timestamps stored as described at the top of `crates/store/migrations/0001_init.sql`.
- Tests: unit tests next to code; integration tests in `crates/<c>/tests/`. Tests must not need
  network or model credentials. Tests needing Docker are `#[ignore]` unless `KILN_TEST_DOCKER=1`.
- Data dir layout (`<data>` = `$KILN_DATA_DIR`, default `~/Library/Application Support/kiln`
  on macOS, `~/.local/share/kiln` on Linux):
  ```
  <data>/kiln.db            SQLite control store (+ -wal, -shm)
  <data>/master.key         only if no OS keychain is available (0600)
  <data>/repos/<KEY>.git    bare mirrors
  <data>/worktrees/<TASK-KEY>/
  <data>/runs/<run_id>/     run folder, mounted at /run/kiln (spool.jsonl, out/, transcripts/, kiln.sock)
  <data>/l0/                raw event segments (jsonl.zst, parquet)
  <data>/index/tantivy/     full-text index
  <data>/index/lance/       LanceDB (feature `lance`)
  <data>/models/            fastembed model cache
  <data>/web/               built Leptos app (served at /app)
  <data>/downloads/         client builds served at /download
  ```

## Public APIs (contracts between crates)

### store (`kiln_store`)
```rust
pub struct Store { /* SqlitePool */ }
impl Store {
    pub async fn open(path: &Path) -> Result<Store>;      // creates file, WAL, foreign_keys=ON, busy_timeout, runs migrations
    pub async fn open_in_memory() -> Result<Store>;       // tests (shared-cache memory DB)
    pub fn pool(&self) -> &SqlitePool;                    // for crates with their own queries (memory)
}
```
Grouped query methods (all `async`, all return `Result<_, StoreError>`):
- settings_kv: `get_setting<T>(key)`, `set_setting<T>(key, &T)`; keys: `allowed_domain`, `setup_done`.
- users: `create_user(NewUser) -> User`, `get_user(id)`, `get_user_by_email`, `list_users`, `update_user(id, UserPatch)`,
  `user_password_hash(id)`, `set_user_settings(id, &UserSettings)`, `get_user_settings(id) -> Option<UserSettings>`, `count_users`.
- sessions/devices/pairing: `create_session(user, token_hash, device, ttl)`, `session_user(token_hash) -> Option<(User, SessionId)>` (touches last_seen, rejects expired/disabled), `delete_session`, `upsert_device`, `list_devices(user)`, `set_push_endpoint`, `create_pairing_code(code_hash, user, ttl)`, `consume_pairing_code(code_hash) -> Option<UserId>`.
- credentials: `insert_credential(NewCredential{.., ciphertext, nonce, masked})`, `list_credentials(user) -> Vec<CredentialSummary>`, `get_credential_secret(user, id) -> Option<(CredentialKind, ciphertext, nonce, base_url, env_var)>`, `active_credentials_for_dispatch(user) -> Vec<StoredCredential>`, `rotate_credential`, `revoke_credential`, `set_credential_health`.
- projects: `create_project`, `get_project`, `get_project_by_key`, `list_projects`, `update_project`, `set_project_qa(id, status, Option<QaProfile>)`.
- threads/messages/plans: `create_thread`, `get_thread`, `list_threads(owner, project?)`, `update_thread_status`, `add_message`, `list_messages(thread)`,
  `create_plan`, `get_plan`, `get_plan_for_thread`, `update_plan_spec(id, spec, expected_version) -> Plan` (bumps version, `Conflict` on mismatch), `set_plan_status`, `list_plans(owner?, status?)`.
- tasks: `create_tasks_for_plan(plan, Vec<NewTask>) -> Vec<Task>` (one tx; allocates per-project numbers and keys; resolves `depends_on` spec ids → TaskIds),
  `get_task`, `get_task_by_key`, `list_tasks(TaskFilter)`, `transition_task(id, from, to, reason, actor) -> Task` (**compare-and-swap** on `from`, validates `TaskState::can_transition`, writes `task_transitions` row in the same tx; `StoreError::Conflict` if the state moved),
  `update_task_progress(id, progress, meta)`, `set_task_branch`, `set_task_pr`, `increment_loop(id) -> Task`, `push_task_feedback(id, text)`, `take_task_feedback(id) -> Vec<String>`, `list_transitions(task)`.
- runs: `create_run(NewRun) -> Run`, `get_run`, `list_runs_for_task`, `list_runs_in_states(&[RunState])`, `update_run_state(id, state, error?)`, `set_run_container(id, container_id, token_hash)`, `run_token_hash(id)`, `finish_run(id, state, &RunOutcome)`, `mark_ingested(id)`, `add_run_usage`, `set_run_progress`, `month_spend(user) -> f64`.
- run_events: `insert_run_events(&[(RunEvent, WorkerRole)]) -> u64 /*rows inserted*/` (idempotent `INSERT OR IGNORE` on (run_id, seq)), `list_log_lines(task_or_run, LogQuery) -> Vec<LogLine>`, `prune_run_events(older_than)`.
- artifacts: `insert_artifact(NewArtifact) -> Artifact` (idempotent on (run_id,path)), `list_artifacts_for_task`, `get_artifact`.
- pull_requests: `insert_pr`, `list_open_prs`, `set_pr_state`.
- jobs: `enqueue_job(NewJob) -> JobId`, `claim_job(kinds, owner_filter?) -> Option<Job>` (atomically sets running + locked_at), `complete_job`, `fail_job(id, err, retry_after?)`, `cancel_jobs_for(...)`, `requeue_stale_jobs(older_than)`, `queued_jobs(kind?) -> Vec<Job>`.
- notifications: `insert_notification`, `list_notifications(user, limit)`, `mark_read`, `resolve_notification(id, text)`.

### auth (`kiln_auth`)
```rust
pub fn hash_password(pw: &str) -> Result<String>;  pub fn verify_password(pw: &str, hash: &str) -> bool;
pub fn new_token() -> String /* 32 random bytes, base64url */;  pub fn token_hash(t: &str) -> String /* sha256 hex */;
pub fn email_domain(email: &str) -> Option<&str>;  pub fn domain_allowed(email: &str, allowed: &str) -> bool;
pub struct Auth { store: Store }
impl Auth {
  pub async fn setup_required(&self) -> Result<bool>;
  pub async fn setup(&self, SetupRequest) -> Result<(User, String /*token*/)>;   // only when no users exist
  pub async fn login(&self, LoginRequest) -> Result<(User, String)>;              // generic error on bad email OR password
  pub async fn register(&self, RegisterRequest) -> Result<(User, String)>;        // AuthError::NotAllowlisted
  pub async fn authenticate(&self, token: &str) -> Result<User>;
  pub async fn logout(&self, token: &str) -> Result<()>;
  pub async fn create_user(&self, by: &User, CreateUserRequest) -> Result<User>;  // admin only
  pub async fn start_pairing(&self, user: &User) -> Result<(String /*code*/, Timestamp)>; // 8 chars, 10 min, one-time
  pub async fn complete_pairing(&self, PairCompleteRequest) -> Result<(User, String)>;
}
```
Password policy: ≥ 10 chars. Session TTL 30 days sliding.

### vault (`kiln_vault`)
```rust
pub struct MasterKey(..);  impl MasterKey { pub fn load_or_create(data_dir: &Path) -> Result<Self>; pub fn from_bytes([u8;32]) -> Self }
pub struct Vault { key: MasterKey, store: Store }
impl Vault {
  pub async fn add(&self, user: UserId, CreateCredentialRequest) -> Result<CredentialSummary>;
  pub async fn rotate(&self, user, id, RotateCredentialRequest) -> Result<CredentialSummary>;
  pub async fn revoke(&self, user, id) -> Result<()>;
  pub async fn list(&self, user) -> Result<Vec<CredentialSummary>>;
  pub async fn dispatch_env(&self, user: UserId) -> Result<DispatchSecrets>; // env vars to inject + the raw values for redaction
}
pub struct DispatchSecrets { pub env: Vec<(String, String)>, pub redactor: Redactor }
pub struct Redactor { .. } impl Redactor { pub fn new(secrets) -> Self; pub fn redact(&self, s: &str) -> Cow<str>; pub fn extend(..) }
pub fn mask(secret: &str) -> String;           // "sk-ant-…3f9a"
pub fn scan_for_secrets(s: &str) -> Cow<str>;  // pattern-based redaction (sk-…, ghp_…, AKIA…, bearer tokens, PEM blocks)
```
AEAD associated data = `user_id || credential_id`, so a ciphertext can't be moved between users.
Env mapping: ClaudeOauth→`CLAUDE_CODE_OAUTH_TOKEN`, AnthropicApiKey→`ANTHROPIC_API_KEY`, OpenaiApiKey→`OPENAI_API_KEY`,
Litellm→`LITELLM_BASE_URL`+`LITELLM_API_KEY` (and `OPENAI_BASE_URL`/`OPENAI_API_KEY` for codex when no OpenAI key), Other→`env_var`.

### sandbox (`kiln_sandbox`)
```rust
#[async_trait] pub trait Runtime: Send + Sync {
  async fn ping(&self) -> Result<RuntimeInfo>;
  async fn ensure_image(&self, image: &str) -> Result<()>;
  async fn create(&self, spec: &ContainerSpec) -> Result<String /*id*/>;
  async fn start(&self, id: &str) -> Result<()>;
  async fn wait(&self, id: &str) -> Result<i64 /*exit code*/>;
  async fn stop(&self, id: &str, grace: Duration) -> Result<()>;
  async fn remove(&self, id: &str) -> Result<()>;
  async fn is_running(&self, id: &str) -> Result<bool>;
  async fn list_managed(&self) -> Result<Vec<ManagedContainer>>;  // label kiln.managed=1
  async fn stats(&self, id: &str) -> Result<ContainerStats>;
  async fn create_network(&self, name: &str) -> Result<()>;  async fn remove_network(&self, name: &str) -> Result<()>;
  async fn logs_tail(&self, id: &str, lines: usize) -> Result<String>;
}
pub struct ContainerSpec { name, image, cmd: Vec<String>, env: Vec<(String,String)>, mounts: Vec<Mount{host, container, read_only}>,
  cpus: f32, memory_mb: u64, network: NetworkMode /*None | Bridge | Named(String)*/, labels: BTreeMap, workdir, user: Option<String>, aliases: Vec<String> }
pub struct DockerRuntime;   // bollard; connects via DOCKER_HOST or default socket (also OrbStack/Podman sockets)
pub struct ProcessRuntime;  // dev/test: runs `cmd` as a host subprocess, maps mounts via env (KILN_RUN_DIR, KILN_WORKSPACE); no isolation
pub fn mounts_for(role, worktree, git_common_dir, run_dir) -> Vec<Mount>; // decision #3: common git dir RO at same abs path
```
Labels: `kiln.managed=1`, `kiln.run_id`, `kiln.role`, `kiln.owner`, `kiln.task`.

### harness (`kiln_harness`)
```rust
pub trait Harness: Send + Sync {
  fn id(&self) -> &'static str;
  fn prepare(&self, cfg: &RunConfig) -> Result<Launch>;               // command, env, files to write
  fn parse_event(&self, line: &str) -> Vec<RunEventKind>;            // stdout line → normalized events (0..n)
  fn parse_transcript(&self, path: &Path) -> Vec<RunEventKind>;      // post-run ingest of the native transcript
  fn transcript_dir(&self) -> Option<&'static str>;                  // "~/.claude/projects" | "~/.codex/sessions"
}
pub struct Launch { pub program: String, pub args: Vec<String>, pub env: Vec<(String,String)>, pub files: Vec<(PathBuf, String)>, pub stdin: Option<String> }
pub fn registry() -> Registry;  // by id: "claude_sdk", "codex", "opencode", "mock"
```
The `mock` harness is `kiln-runner mock-harness` (a subcommand of the runner) driven by `RunConfig.options`
(`mock.fail_qa_times`, `mock.fail_review_times`, `mock.sleep_ms`, …). It edits a file, commits, writes
`qa_report.json`/`verdict.json`/`plan.json` as the role requires. It is what all pipeline tests use.

### runner (`kiln-runner` binary)
`kiln-runner run` (default): connect `$KILN_SOCKET` → Hello(token) → Welcome(RunConfig) → write Launch.files →
spawn harness → for each stdout line: `parse_event` → append to spool → send Event → on Ack trim nothing (spool is kept) →
handle Stop/Feedback → on exit: collect `/run/kiln/out/*`, `git rev-parse HEAD` (Builder; also commits leftovers with
message `kiln: <task> attempt n` if the harness left changes), `parse_transcript` for usage → send Complete.
Reconnect with backoff; resend spool entries with seq > last Ack.
`kiln-runner mcp`: stdio MCP server exposing `search_memory`, `propose_memory`, proxied over a second socket connection (Hello carries the same token).
`kiln-runner mock-harness`: see harness.
All paths overridable by env (`KILN_RUN_DIR`, `KILN_WORKSPACE`) so ProcessRuntime works.

### gitops (`kiln_gitops`)
```rust
pub struct GitOps { data_dir }
impl GitOps {
  pub async fn ensure_mirror(&self, project: &Project, auth: &GitAuth) -> Result<PathBuf>;  // bare clone/fetch
  pub async fn create_worktree(&self, project, task_key, branch, base) -> Result<Worktree{path, git_common_dir, branch}>;
  pub async fn remove_worktree(&self, project, task_key) -> Result<()>;
  pub async fn read_file_at(&self, project, rev, path) -> Result<Option<String>>;   // qa.yaml
  pub async fn head(&self, worktree) -> Result<String>;
  pub async fn diff_summary(&self, worktree, base) -> Result<DiffSummary>;
  pub async fn push_branch(&self, project, worktree, branch, auth) -> Result<()>;    // refuses protected/non-`kiln/` branches
  pub fn exclude_paths(&self, worktree, &["AGENTS.md","CLAUDE.md"]) -> Result<()>;   // .git/info/exclude
}
pub fn branch_name(task_key, title) -> String;   // "kiln/fp-142-retry-backoff"
#[async_trait] pub trait PrProvider { async fn open_pr(&self, NewPr) -> Result<PrRef{number,url}>; async fn pr_state(&self, repo_url, number) -> Result<PrState>; async fn git_auth(&self, repo_url) -> Result<GitAuth>; }
pub struct GitHubApp { app_id, private_key_pem }  pub struct GitHubToken { token }  pub struct LocalPrs { store }
pub fn render_pr_body(plan, task, qa: Option<&QaReport>, verdict: Option<&ReviewVerdict>, artifact_links) -> String;
```

### planner (`kiln_planner`)
Pure prompt/templating: `planner_system()`, `planner_prompt(project, thread messages, memory pack, plan draft?)`,
`builder_prompt(task, attempt, feedback, memory pack)`, `qa_prompt(task, qa_profile)`, `reviewer_prompt(task, diff, qa_report, conventions)`,
`distiller_prompt(episode, events)`, `plan_json_schema() -> &'static str`, `memory_file(pack) -> String` (AGENTS.md / CLAUDE.md content).

### memory (`kiln_memory`)
```rust
pub struct Memory { pool, data_dir, text: TextIndex, vectors: Box<dyn VectorIndex>, embedder: Box<dyn Embedder> }
impl Memory {
  pub async fn open(store: &Store, data_dir: &Path, cfg: MemoryConfig) -> Result<Self>;
  // gateway: the ONLY access path; every read takes the requesting user
  pub async fn search(&self, user: UserId, q: &MemoryQuery) -> Result<Vec<MemoryHit>>;  // hybrid → visibility/project filter → rerank → budget
  pub async fn list(&self, user, q) -> Result<MemoryListResponse>;  pub async fn get(&self, user, id) -> Result<MemoryDetail>;
  pub async fn propose(&self, owner, source_run: Option<RunId>, MemoryProposal) -> Result<MemoryItem>; // private→Active; shared/company→Pending
  pub async fn update(&self, user, id, UpdateMemoryRequest) -> Result<MemoryItem>;
  pub async fn decide(&self, user, id, ApproveMemoryRequest) -> Result<MemoryItem>;
  pub async fn record_use(&self, items, run, task, role, user);  pub async fn feedback(&self, run: RunId, passed: bool);
  // capture
  pub async fn ingest_l0(&self, run: &Run, project: Option<ProjectId>, events: &[RunEvent], redact: &(dyn Fn(&str)->String + Sync)) -> Result<()>;
  // pipeline
  pub async fn distill(&self, run: &Run, task: Option<&Task>) -> Result<Episode>;          // heuristic L0→L1
  pub async fn extract(&self, episode: &Episode, llm_output: Option<&str>) -> Result<Vec<MemoryItem>>; // heuristic or LLM JSON → L2 (dedupe/supersede/visibility rules)
  pub async fn rebuild_packs(&self, owner, project) -> Result<()>;  pub async fn pack(&self, owner, project, role) -> Result<String>;
  pub async fn nightly(&self) -> Result<NightlyReport>;  // compact L0→parquet, retention, merge dupes, flag stale
}
```

### notifier (`kiln_notifier`)
```rust
pub struct Notifier { store, bus, http }
impl Notifier {
  pub async fn notify(&self, NewNotification) -> Result<Notification>;  // persist → bus ServerEvent::Notification → UnifiedPush to user's devices (respecting channel prefs)
  pub async fn workers_live(&self, user, live: u32, avg_progress: u8);  // throttled ≥10s, ongoing notification
  pub async fn resolve(&self, id, text) -> Result<()>;
}
```
Push payload (JSON POST to the device endpoint): `{ id, kind, channel, title, body, deep_link, actions:[{id,label}], progress? }`.

### scheduler / daemon
Specified in wave 2 once the above exist (see `docs/SCHEDULER.md`).
