-- Kiln control-store schema (PRD §8.1). SQLite, WAL mode.
-- Conventions:
--   * ids are UUIDv7 strings (TEXT), timestamps are RFC3339 UTC strings (TEXT)
--   * JSON columns are TEXT holding serde_json of the matching kiln-types struct
--   * enums are stored as their snake_case serde string

CREATE TABLE settings_kv (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL            -- JSON
);

CREATE TABLE users (
    id             TEXT PRIMARY KEY,
    email          TEXT NOT NULL UNIQUE COLLATE NOCASE,
    name           TEXT NOT NULL,
    role           TEXT NOT NULL CHECK (role IN ('admin','member')),
    status         TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','disabled')),
    password_hash  TEXT NOT NULL,
    limits_json    TEXT NOT NULL,       -- UserLimits
    settings_json  TEXT,                -- UserSettings (NULL = defaults)
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);

CREATE TABLE sessions (
    id           TEXT PRIMARY KEY,
    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash   TEXT NOT NULL UNIQUE,  -- sha256 hex of the bearer token
    device_id    TEXT,
    created_at   TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    expires_at   TEXT NOT NULL
);
CREATE INDEX sessions_user ON sessions(user_id);

CREATE TABLE devices (
    id             TEXT PRIMARY KEY,
    user_id        TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,       -- ClientKind
    name           TEXT NOT NULL,
    push_endpoint  TEXT,
    created_at     TEXT NOT NULL,
    last_seen_at   TEXT NOT NULL
);
CREATE INDEX devices_user ON devices(user_id);

CREATE TABLE pairing_codes (
    code_hash   TEXT PRIMARY KEY,       -- sha256 hex of the one-time code
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at  TEXT NOT NULL,
    used_at     TEXT
);

CREATE TABLE credentials (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,          -- CredentialKind
    name        TEXT NOT NULL,
    ciphertext  BLOB NOT NULL,
    nonce       BLOB NOT NULL,
    masked      TEXT NOT NULL,
    base_url    TEXT,
    env_var     TEXT,
    health_json TEXT NOT NULL,          -- CredentialHealth
    created_at  TEXT NOT NULL,
    expires_at  TEXT,
    revoked_at  TEXT
);
CREATE INDEX credentials_user ON credentials(user_id);

CREATE TABLE projects (
    id               TEXT PRIMARY KEY,
    key              TEXT NOT NULL UNIQUE,
    name             TEXT NOT NULL,
    repo_url         TEXT NOT NULL,
    default_branch   TEXT NOT NULL,
    qa_path          TEXT NOT NULL,
    qa_status_json   TEXT NOT NULL,     -- QaProfileStatus
    qa_profile_json  TEXT,              -- last parsed QaProfile
    image            TEXT,
    resources_json   TEXT NOT NULL,     -- ResourceProfile
    loop_cap         INTEGER NOT NULL DEFAULT 3,
    next_task_number INTEGER NOT NULL DEFAULT 1,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
);

CREATE TABLE threads (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    owner_id    TEXT NOT NULL REFERENCES users(id),
    title       TEXT NOT NULL,
    plan_id     TEXT,
    status      TEXT NOT NULL,          -- PlanStatus
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX threads_owner ON threads(owner_id, updated_at);

CREATE TABLE messages (
    id            TEXT PRIMARY KEY,
    thread_id     TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    author        TEXT NOT NULL,        -- MessageAuthor
    body          TEXT NOT NULL,
    voice         INTEGER NOT NULL DEFAULT 0,
    plan_version  INTEGER,
    created_at    TEXT NOT NULL
);
CREATE INDEX messages_thread ON messages(thread_id, created_at);

CREATE TABLE plans (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    owner_id    TEXT NOT NULL REFERENCES users(id),
    thread_id   TEXT NOT NULL REFERENCES threads(id),
    status      TEXT NOT NULL,          -- PlanStatus
    version     INTEGER NOT NULL DEFAULT 1,
    spec_json   TEXT NOT NULL,          -- PlanSpec
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX plans_owner ON plans(owner_id, status);

CREATE TABLE tasks (
    id              TEXT PRIMARY KEY,
    plan_id         TEXT NOT NULL REFERENCES plans(id),
    project_id      TEXT NOT NULL REFERENCES projects(id),
    owner_id        TEXT NOT NULL REFERENCES users(id),
    number          INTEGER NOT NULL,
    key             TEXT NOT NULL UNIQUE,   -- "FP-142"
    spec_id         TEXT NOT NULL,          -- "t1"
    title           TEXT NOT NULL,
    description     TEXT NOT NULL DEFAULT '',
    acceptance_json TEXT NOT NULL,          -- Vec<String>
    files_hint_json TEXT NOT NULL DEFAULT '[]',
    depends_on_json TEXT NOT NULL DEFAULT '[]', -- Vec<TaskId>
    workers_json    TEXT NOT NULL,          -- BTreeMap<WorkerRole, WorkerChoice>
    state           TEXT NOT NULL,          -- TaskState
    last_active     TEXT,
    loop_count      INTEGER NOT NULL DEFAULT 0,
    loop_cap        INTEGER NOT NULL DEFAULT 3,
    branch          TEXT,
    pr_url          TEXT,
    pr_number       INTEGER,
    progress        INTEGER NOT NULL DEFAULT 0,
    meta            TEXT NOT NULL DEFAULT '',
    feedback_json   TEXT NOT NULL DEFAULT '[]', -- Vec<String> pending human/QA/review feedback for the next Builder run
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    UNIQUE (project_id, number)
);
CREATE INDEX tasks_state ON tasks(state);
CREATE INDEX tasks_owner ON tasks(owner_id, state);

-- Every task state transition, persisted before its side effect (PRD §7.6).
CREATE TABLE task_transitions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    from_state  TEXT NOT NULL,
    to_state    TEXT NOT NULL,
    reason      TEXT NOT NULL DEFAULT '',
    actor       TEXT,                    -- user id, or NULL for the system
    at          TEXT NOT NULL
);
CREATE INDEX task_transitions_task ON task_transitions(task_id, id);

CREATE TABLE runs (
    id            TEXT PRIMARY KEY,
    task_id       TEXT REFERENCES tasks(id),
    thread_id     TEXT REFERENCES threads(id),
    owner_id      TEXT NOT NULL REFERENCES users(id),
    role          TEXT NOT NULL,          -- WorkerRole
    harness       TEXT NOT NULL,
    model         TEXT NOT NULL,
    attempt       INTEGER NOT NULL DEFAULT 1,
    container_id  TEXT,
    token_hash    TEXT,                   -- sha256 of the per-run socket token
    state         TEXT NOT NULL,          -- RunState
    progress      INTEGER NOT NULL DEFAULT 0,
    started_at    TEXT,
    ended_at      TEXT,
    ingested      INTEGER NOT NULL DEFAULT 0,
    cost_usd      REAL NOT NULL DEFAULT 0,
    tokens_in     INTEGER NOT NULL DEFAULT 0,
    tokens_out    INTEGER NOT NULL DEFAULT 0,
    error         TEXT,
    retries       INTEGER NOT NULL DEFAULT 0,
    outcome_json  TEXT,                   -- RunOutcome
    last_seq      INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL
);
CREATE INDEX runs_task ON runs(task_id, created_at);
CREATE INDEX runs_state ON runs(state);
CREATE INDEX runs_owner_month ON runs(owner_id, created_at);

-- Recent run events for log panes (L0 is the long-term store; this is a
-- bounded hot cache: rows older than 14 days are pruned).
CREATE TABLE run_events (
    run_id   TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    seq      INTEGER NOT NULL,
    ts       TEXT NOT NULL,
    role     TEXT NOT NULL,
    level    TEXT NOT NULL,             -- LogLevel
    message  TEXT NOT NULL,
    kind     TEXT NOT NULL,             -- RunEventKind tag
    PRIMARY KEY (run_id, seq)
) WITHOUT ROWID;

CREATE TABLE artifacts (
    id          TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,          -- ArtifactKind
    path        TEXT NOT NULL,          -- relative to the run dir
    size        INTEGER NOT NULL,
    mime        TEXT NOT NULL,
    caption     TEXT,
    created_at  TEXT NOT NULL,
    UNIQUE (run_id, path)
);

CREATE TABLE pull_requests (           -- used by the Local PR provider and merge polling
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     TEXT NOT NULL REFERENCES tasks(id),
    provider    TEXT NOT NULL,
    number      INTEGER NOT NULL,
    url         TEXT NOT NULL,
    branch      TEXT NOT NULL,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    state       TEXT NOT NULL DEFAULT 'open', -- open | merged | closed
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- Persistent job queue (PRD §7.6).
CREATE TABLE jobs (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,          -- "run_role", "distill", "extract", "rebuild_packs", "nightly", "poll_prs", ...
    payload     TEXT NOT NULL,          -- JSON
    owner_id    TEXT,                   -- for fair-share
    state       TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued','running','done','failed','cancelled')),
    priority    INTEGER NOT NULL DEFAULT 0,
    attempts    INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 2,
    run_after   TEXT NOT NULL,
    locked_at   TEXT,
    last_error  TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX jobs_ready ON jobs(state, run_after, priority);

CREATE TABLE notifications (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    deep_link   TEXT NOT NULL,
    actions_json TEXT NOT NULL DEFAULT '[]',
    task_id     TEXT,
    plan_id     TEXT,
    memory_id   TEXT,
    read        INTEGER NOT NULL DEFAULT 0,
    resolved    TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX notifications_user ON notifications(user_id, created_at);

-- ------------------------------------------------------------- memory (PRD §7.9)
-- Every row carries owner, visibility, shared_with (via memory_shares),
-- project, type, source_run, status, created_at, updated_at.

CREATE TABLE memory_items (                -- L2
    id                    TEXT PRIMARY KEY,
    owner_id              TEXT NOT NULL REFERENCES users(id),
    type                  TEXT NOT NULL,   -- MemoryType
    project_id            TEXT REFERENCES projects(id),
    visibility            TEXT NOT NULL,   -- Visibility
    requested_visibility  TEXT,
    title                 TEXT NOT NULL,
    subject               TEXT NOT NULL,
    relation              TEXT NOT NULL,
    object                TEXT NOT NULL,
    body                  TEXT NOT NULL DEFAULT '',
    status                TEXT NOT NULL,   -- MemoryStatus
    salience              REAL NOT NULL DEFAULT 0.5,
    source_run_id         TEXT REFERENCES runs(id),
    source_label          TEXT,
    superseded_by         TEXT,
    use_count             INTEGER NOT NULL DEFAULT 0,
    success_count         INTEGER NOT NULL DEFAULT 0,
    failure_count         INTEGER NOT NULL DEFAULT 0,
    last_used_at          TEXT,
    embedding             BLOB,            -- f32 little-endian vector (built-in vector index)
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL
);
CREATE INDEX memory_items_owner ON memory_items(owner_id, status);
CREATE INDEX memory_items_vis ON memory_items(visibility, status);
CREATE INDEX memory_items_project ON memory_items(project_id);

CREATE TABLE memory_shares (
    item_id  TEXT NOT NULL REFERENCES memory_items(id) ON DELETE CASCADE,
    user_id  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (item_id, user_id)
) WITHOUT ROWID;

CREATE TABLE memory_links (
    from_id  TEXT NOT NULL,
    to_id    TEXT NOT NULL,
    kind     TEXT NOT NULL,                -- "source_run" | "supersedes" | "duplicate_of" | "related"
    PRIMARY KEY (from_id, to_id, kind)
) WITHOUT ROWID;

CREATE TABLE memory_audit (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id   TEXT NOT NULL,
    actor_id  TEXT,
    action    TEXT NOT NULL,               -- created | approved | rejected | visibility | edited | archived | flagged | used
    detail    TEXT NOT NULL DEFAULT '',
    at        TEXT NOT NULL
);
CREATE INDEX memory_audit_item ON memory_audit(item_id, id);

CREATE TABLE memory_uses (                 -- retrieval log, drives feedback + "Recently used by"
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id   TEXT NOT NULL REFERENCES memory_items(id) ON DELETE CASCADE,
    run_id    TEXT,
    task_id   TEXT,
    role      TEXT,
    user_id   TEXT NOT NULL,
    at        TEXT NOT NULL
);
CREATE INDEX memory_uses_item ON memory_uses(item_id, at);
CREATE INDEX memory_uses_run ON memory_uses(run_id);

CREATE TABLE episodes (                    -- L1
    id            TEXT PRIMARY KEY,
    run_id        TEXT NOT NULL UNIQUE REFERENCES runs(id),
    task_id       TEXT,
    owner_id      TEXT NOT NULL REFERENCES users(id),
    project_id    TEXT,
    role          TEXT NOT NULL,
    visibility    TEXT NOT NULL DEFAULT 'private',
    goal          TEXT NOT NULL,
    outcome       TEXT NOT NULL,
    errors_json   TEXT NOT NULL DEFAULT '[]',
    fixes_json    TEXT NOT NULL DEFAULT '[]',
    files_json    TEXT NOT NULL DEFAULT '[]',
    duration_secs INTEGER NOT NULL DEFAULT 0,
    cost_usd      REAL NOT NULL DEFAULT 0,
    status        TEXT NOT NULL DEFAULT 'active',
    salience      REAL NOT NULL DEFAULT 0.5,
    embedding     BLOB,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX episodes_owner ON episodes(owner_id, created_at);

CREATE TABLE memory_packs (                -- L3: precomputed context per role × project × owner
    owner_id    TEXT NOT NULL,
    project_id  TEXT NOT NULL,
    role        TEXT NOT NULL,
    content     TEXT NOT NULL,             -- markdown written into AGENTS.md / CLAUDE.md
    item_ids    TEXT NOT NULL DEFAULT '[]',
    built_at    TEXT NOT NULL,
    PRIMARY KEY (owner_id, project_id, role)
) WITHOUT ROWID;

-- L0 segment index (segments are files under <data>/l0/)
CREATE TABLE l0_segments (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    path        TEXT NOT NULL UNIQUE,
    format      TEXT NOT NULL,             -- "jsonl.zst" | "parquet"
    min_ts      TEXT NOT NULL,
    max_ts      TEXT NOT NULL,
    rows        INTEGER NOT NULL,
    bytes       INTEGER NOT NULL,
    created_at  TEXT NOT NULL
);
