//! Kiln control store (PRD §7.3, §8.1).
//!
//! SQLite in WAL mode via `sqlx`, with the schema in `migrations/` embedded at
//! compile time and applied on [`Store::open`]. Every query is runtime-checked
//! (`sqlx::query` + manual row mapping into `kiln_types` structs), so no
//! `DATABASE_URL` is needed to build.
//!
//! Storage conventions (see the header of `migrations/0001_init.sql`):
//! * ids are UUIDv7 strings,
//! * timestamps are RFC 3339 UTC strings with a fixed microsecond precision
//!   and a `Z` suffix, so lexical comparison in SQL equals time comparison,
//! * enums are their snake_case serde string, structs are serde JSON.
//!
//! Query methods are grouped by table in the submodules; they are all
//! inherent methods on [`Store`].

mod artifacts;
mod credentials;
mod jobs;
mod notifications;
mod planning;
mod projects;
mod pull_requests;
mod row;
mod run_events;
mod runs;
mod sessions;
mod settings;
mod tasks;
mod users;

pub use artifacts::NewArtifact;
pub use credentials::{CredentialSecret, NewCredential, StoredCredential};
pub use jobs::{Job, JobState, NewJob};
pub use notifications::NewNotification;
pub use planning::{NewMessage, NewPlan, NewThread};
pub use projects::NewProject;
pub use pull_requests::{NewPr, PullRequest, PullRequestState};
pub use run_events::LogTarget;
pub use runs::NewRun;
pub use sessions::{NewDevice, PushTarget};
pub use settings::keys as setting_keys;
pub use tasks::{NewTask, TaskFilter, TaskTransition};
pub use users::{NewUser, UserPatch};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::path::Path;
use std::time::Duration;

pub use sqlx;
pub use sqlx::SqlitePool as Pool;

/// Embedded migrations (`crates/store/migrations`).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// How long a connection waits on a locked database before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors returned by every store method.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The requested row does not exist (or is not visible to the caller).
    #[error("not found")]
    NotFound,
    /// A uniqueness or compare-and-swap check failed.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The caller passed something the store refuses to persist.
    #[error("invalid: {0}")]
    Invalid(String),
    /// A row could not be mapped back into a domain type.
    #[error("corrupt row: {0}")]
    Corrupt(String),
    /// Underlying database error.
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    /// Applying migrations failed.
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// A JSON column could not be (de)serialized.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl StoreError {
    /// True for [`StoreError::NotFound`].
    pub fn is_not_found(&self) -> bool {
        matches!(self, StoreError::NotFound)
    }
    /// True for [`StoreError::Conflict`].
    pub fn is_conflict(&self) -> bool {
        matches!(self, StoreError::Conflict(_))
    }
}

/// Result alias for store operations.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Converts `Err(NotFound)` into `Ok(None)`.
pub trait OptionalExt<T> {
    /// `Ok(v) → Ok(Some(v))`, `Err(NotFound) → Ok(None)`, other errors pass through.
    fn optional(self) -> Result<Option<T>>;
}

impl<T> OptionalExt<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(StoreError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Handle to the control store. Cheap to clone (wraps a connection pool).
#[derive(Clone, Debug)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open (creating if needed) the database file at `path`, enable WAL,
    /// `foreign_keys=ON` and a 5 s busy timeout, and run pending migrations.
    pub async fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Db(sqlx::Error::Io(e)))?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(opts)
            .await?;
        Self::from_pool(pool).await
    }

    /// Open a fresh, private in-memory database (for tests).
    ///
    /// Each call gets a uniquely named shared-cache database, so parallel
    /// tests never collide. The pool holds a single connection that is never
    /// recycled: that keeps the database alive for the lifetime of the
    /// `Store` and avoids shared-cache table-lock errors under concurrency.
    pub async fn open_in_memory() -> Result<Store> {
        let name = format!("file:kiln-mem-{}", uuid::Uuid::new_v4().simple());
        let opts = SqliteConnectOptions::new()
            .filename(&name)
            .in_memory(true)
            .shared_cache(true)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(opts)
            .await?;
        Self::from_pool(pool).await
    }

    /// Wrap an existing pool and run migrations on it.
    pub async fn from_pool(pool: SqlitePool) -> Result<Store> {
        MIGRATOR.run(&pool).await?;
        Ok(Store { pool })
    }

    /// The underlying pool, for crates with their own queries (memory).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Close every connection. Further use of the store fails.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Start a write transaction with `BEGIN IMMEDIATE`, so concurrent
    /// writers queue on the busy timeout instead of failing on upgrade.
    pub(crate) async fn begin_write(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }
}

/// Map a unique-constraint violation to [`StoreError::Conflict`].
pub(crate) fn conflict_on_unique(e: sqlx::Error, what: &str) -> StoreError {
    if let sqlx::Error::Database(db) = &e
        && db.is_unique_violation()
    {
        return StoreError::Conflict(format!("{what} already exists"));
    }
    StoreError::Db(e)
}
