//! Kiln company brain.
//!
//! # Layers
//! * **L0 raw** — every run event, redacted, in zstd JSONL segments under
//!   `<data>/l0/YYYY-MM-DD/<run_id>.jsonl.zst`, compacted nightly into
//!   Parquet (feature `parquet`, on by default). Never used in context.
//! * **L1 episodes** — one row per run (`episodes`): goal, outcome, errors,
//!   fixes, files, duration, cost. Searchable by their owner.
//! * **L2 knowledge** — `(subject, relation, object)` items with metadata
//!   (`memory_items`), indexed in Tantivy and a vector index.
//! * **L3 packs** — per owner × project × role markdown context
//!   (`memory_packs`) written into worker checkouts as `AGENTS.md`/`CLAUDE.md`.
//!
//! # Tenancy
//! [`Memory`] is the **only** access path. Every read takes the requesting
//! user and is filtered in SQL by the exact SQL translation of
//! [`MemoryItem::visible_to`](kiln_types::memory::MemoryItem::visible_to),
//! then re-checked in Rust with `visible_to` itself. Indexes (Tantivy,
//! vectors) only *propose* candidates; a stale index can reduce recall but
//! can never leak an item.
//!
//! # Backends
//! | Trait | Default | Optional |
//! |---|---|---|
//! | [`Embedder`] | [`HashEmbedder`] (offline, deterministic) | `FastEmbedder` (feature `fastembed`) |
//! | [`VectorIndex`] | [`SqliteVectorIndex`] (exact cosine over BLOBs) | `LanceVectorIndex` (feature `lance`, needs `protoc`) |
//! | [`TextIndex`] | [`TantivyIndex`] | — |
//! | [`Reranker`] | [`LexicalReranker`] | `FastEmbedReranker` (feature `fastembed`) |
//!
//! SQL over L0 Parquet is available with feature `datafusion`
//! ([`Memory::query_l0`]).

mod db;
mod embed;
mod error;
mod gateway;
mod l0;
mod mcp;
mod nightly;
mod packs;
#[cfg(feature = "parquet")]
mod parquet_io;
mod pipeline;
mod rerank;
mod text;
mod traits;
mod util;
mod vector;

#[cfg(feature = "datafusion")]
mod sql;

#[cfg(feature = "fastembed")]
mod fastembed_backend;
#[cfg(feature = "lance")]
mod lance_backend;

pub use embed::{HASH_DIM, HashEmbedder};
pub use error::{MemoryError, Result};
pub use gateway::{
    FeedbackReport, MULTI_VALUED_RELATIONS, ProposeAction, ProposeOutcome, UseContext,
};
pub use l0::{IngestReport, L0Row};
pub use nightly::{NightlyOptions, NightlyReport};
pub use pipeline::{SalienceInputs, salience};
pub use rerank::LexicalReranker;
pub use text::TantivyIndex;
pub use traits::{Embedder, IndexMeta, Reranker, SearchFilter, TextDoc, TextIndex, VectorIndex};
pub use util::{blob_to_vec, cosine, vec_to_blob};
pub use vector::{Collection, SqliteVectorIndex};

#[cfg(feature = "fastembed")]
pub use fastembed_backend::{FastEmbedReranker, FastEmbedder};
#[cfg(feature = "lance")]
pub use lance_backend::LanceVectorIndex;

use kiln_types::memory::{MemoryStatus, MemoryType};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Statuses that retrieval paths (search, packs, MCP) return. Pending items
/// are only ever visible to their owner (`visible_to`), so they act as
/// private items until approved.
pub const RETRIEVABLE: [MemoryStatus; 2] = [MemoryStatus::Active, MemoryStatus::Pending];

/// Statuses the browse view ([`Memory::list`]) returns. Superseded and
/// rejected items are reachable by id ([`Memory::get`]) only.
pub const LISTABLE: [MemoryStatus; 4] = [
    MemoryStatus::Active,
    MemoryStatus::Pending,
    MemoryStatus::Flagged,
    MemoryStatus::Archived,
];

/// `settings_kv` key recording which embedder produced stored vectors.
const EMBEDDER_KEY: &str = "memory.embedder";

/// Which embedder to use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EmbedderChoice {
    /// [`HashEmbedder`] — offline, deterministic, 384 dims.
    #[default]
    Hash,
    /// fastembed ONNX model (feature `fastembed`), e.g. `"BAAI/bge-small-en-v1.5"`.
    /// `None` picks the library default. Models are cached in `<data>/models`.
    Fastembed { model: Option<String> },
}

/// Which reranker to use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RerankerChoice {
    /// [`LexicalReranker`].
    #[default]
    Lexical,
    /// fastembed cross-encoder (feature `fastembed`), e.g. `"BAAI/bge-reranker-base"`.
    Fastembed { model: Option<String> },
}

/// Which vector index to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VectorBackend {
    /// [`SqliteVectorIndex`].
    #[default]
    Sqlite,
    /// LanceDB at `<data>/index/lance` (feature `lance`).
    Lance,
}

/// Tunables. Every field has a sensible default (`MemoryConfig::default()`),
/// and the struct deserializes with missing fields defaulted, so it can be a
/// `[memory]` table in `kiln.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub embedder: EmbedderChoice,
    pub reranker: RerankerChoice,
    pub vector_backend: VectorBackend,
    /// Cosine at/above which a proposal with the same subject+relation is a
    /// duplicate and is merged into the existing item.
    pub dedupe_threshold: f32,
    /// Cosine at/above which a proposal with the same subject+relation but a
    /// different object contradicts (and supersedes) the older item.
    pub contradiction_threshold: f32,
    /// Vector candidates below this cosine are ignored (keeps unrelated
    /// nearest neighbours out of results; keyword hits are unaffected).
    pub min_vector_score: f32,
    /// Minimum salience for an extracted item to be promoted to L2.
    pub promote_threshold: f32,
    /// Salience given to items proposed directly (API / MCP).
    pub default_salience: f32,
    /// Salience added when an item retrieved for a run that passed.
    pub feedback_boost: f32,
    /// Salience added when the owner approves a pending item (human signal).
    pub approval_boost: f32,
    /// An item is flagged once it has at least this many failed runs and
    /// failures outnumber successes 2:1.
    pub flag_after_failures: u32,
    pub default_search_limit: u32,
    pub max_search_limit: u32,
    /// Token budget of an L3 pack.
    pub pack_token_budget: u32,
    /// Characters per token for budget estimates.
    pub chars_per_token: usize,
    /// Whether [`Memory::search`] also returns the viewer's L1 episodes.
    pub search_episodes: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            embedder: EmbedderChoice::default(),
            reranker: RerankerChoice::default(),
            vector_backend: VectorBackend::default(),
            dedupe_threshold: 0.92,
            contradiction_threshold: 0.4,
            min_vector_score: 0.15,
            promote_threshold: 0.35,
            default_salience: 0.5,
            feedback_boost: 0.05,
            approval_boost: 0.1,
            flag_after_failures: 3,
            default_search_limit: 8,
            max_search_limit: 50,
            pack_token_budget: 1500,
            chars_per_token: 4,
            search_episodes: true,
        }
    }
}

/// The pluggable backends of a [`Memory`]. Use with [`Memory::open_with`]
/// to inject custom implementations (tests, alternative stores).
#[derive(Clone)]
pub struct Components {
    pub embedder: Arc<dyn Embedder>,
    pub reranker: Arc<dyn Reranker>,
    pub text: Arc<dyn TextIndex>,
    pub item_vectors: Arc<dyn VectorIndex>,
    pub episode_vectors: Arc<dyn VectorIndex>,
}

impl Components {
    /// Build the backends selected by `cfg`.
    pub async fn from_config(
        pool: &SqlitePool,
        data_dir: &Path,
        cfg: &MemoryConfig,
    ) -> Result<Self> {
        let embedder: Arc<dyn Embedder> = match &cfg.embedder {
            EmbedderChoice::Hash => Arc::new(HashEmbedder::new()),
            #[cfg(feature = "fastembed")]
            EmbedderChoice::Fastembed { model } => {
                Arc::new(FastEmbedder::new(model.as_deref(), &data_dir.join("models")).await?)
            }
            #[cfg(not(feature = "fastembed"))]
            EmbedderChoice::Fastembed { .. } => {
                return Err(MemoryError::Unsupported(
                    "built without feature `fastembed`".into(),
                ));
            }
        };
        let reranker: Arc<dyn Reranker> = match &cfg.reranker {
            RerankerChoice::Lexical => Arc::new(LexicalReranker),
            #[cfg(feature = "fastembed")]
            RerankerChoice::Fastembed { model } => {
                Arc::new(FastEmbedReranker::new(model.as_deref(), &data_dir.join("models")).await?)
            }
            #[cfg(not(feature = "fastembed"))]
            RerankerChoice::Fastembed { .. } => {
                return Err(MemoryError::Unsupported(
                    "built without feature `fastembed`".into(),
                ));
            }
        };
        let (item_vectors, episode_vectors): (Arc<dyn VectorIndex>, Arc<dyn VectorIndex>) =
            match cfg.vector_backend {
                VectorBackend::Sqlite => (
                    Arc::new(SqliteVectorIndex::new(pool.clone(), Collection::Items)),
                    Arc::new(SqliteVectorIndex::new(pool.clone(), Collection::Episodes)),
                ),
                #[cfg(feature = "lance")]
                VectorBackend::Lance => {
                    let dir = data_dir.join("index").join("lance");
                    (
                        Arc::new(
                            LanceVectorIndex::open(&dir, Collection::Items, embedder.dim()).await?,
                        ),
                        Arc::new(
                            LanceVectorIndex::open(&dir, Collection::Episodes, embedder.dim())
                                .await?,
                        ),
                    )
                }
                #[cfg(not(feature = "lance"))]
                VectorBackend::Lance => {
                    return Err(MemoryError::Unsupported(
                        "built without feature `lance`".into(),
                    ));
                }
            };
        let text: Arc<dyn TextIndex> =
            Arc::new(TantivyIndex::open(&data_dir.join("index").join("tantivy"))?);
        Ok(Self {
            embedder,
            reranker,
            text,
            item_vectors,
            episode_vectors,
        })
    }
}

/// The company brain. Construct once per daemon and share (`Arc<Memory>`).
pub struct Memory {
    pub(crate) pool: SqlitePool,
    pub(crate) data_dir: PathBuf,
    pub(crate) cfg: MemoryConfig,
    pub(crate) c: Components,
    pub(crate) l0: tokio::sync::Mutex<l0::L0State>,
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("data_dir", &self.data_dir)
            .field("embedder", &self.c.embedder.id())
            .finish()
    }
}

impl Memory {
    /// Open the memory subsystem over an already-migrated control-store
    /// pool (`kiln_store::Store::pool()`), with backends chosen by `cfg`.
    ///
    /// Creates `<data>/l0`, `<data>/index/…` as needed. If the embedder
    /// changed since the last open, every stored vector is recomputed; if the
    /// text index is missing rows (fresh or wiped directory), it is rebuilt.
    pub async fn open(pool: SqlitePool, data_dir: &Path, cfg: MemoryConfig) -> Result<Memory> {
        let c = Components::from_config(&pool, data_dir, &cfg).await?;
        Self::open_with(pool, data_dir, cfg, c).await
    }

    /// Like [`Memory::open`] with explicit backends.
    pub async fn open_with(
        pool: SqlitePool,
        data_dir: &Path,
        cfg: MemoryConfig,
        c: Components,
    ) -> Result<Memory> {
        std::fs::create_dir_all(data_dir.join("l0"))?;
        let m = Memory {
            pool,
            data_dir: data_dir.to_path_buf(),
            cfg,
            c,
            l0: tokio::sync::Mutex::new(l0::L0State::default()),
        };
        m.startup_sync().await?;
        Ok(m)
    }

    /// The configuration in effect.
    pub fn config(&self) -> &MemoryConfig {
        &self.cfg
    }

    /// The data directory (`<data>`).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The full-text index. Exposed for maintenance tooling and tests; it is
    /// **not** an access path (it holds no item content beyond what search
    /// re-validates against SQL).
    pub fn text_index(&self) -> &Arc<dyn TextIndex> {
        &self.c.text
    }

    /// The L2 vector index (maintenance/tests; not an access path).
    pub fn item_vector_index(&self) -> &Arc<dyn VectorIndex> {
        &self.c.item_vectors
    }

    /// The embedder in use.
    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.c.embedder
    }

    async fn startup_sync(&self) -> Result<()> {
        let stored: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings_kv WHERE key = ?")
                .bind(EMBEDDER_KEY)
                .fetch_optional(&self.pool)
                .await?;
        let stored = stored.and_then(|v| serde_json::from_str::<String>(&v).ok());
        let current = self.c.embedder.id();
        if stored.as_deref() != Some(current.as_str()) {
            let rows: i64 = sqlx::query_scalar(
                "SELECT (SELECT COUNT(*) FROM memory_items) + (SELECT COUNT(*) FROM episodes)",
            )
            .fetch_one(&self.pool)
            .await?;
            if rows > 0 {
                tracing::info!(from = ?stored, to = %current, rows, "embedder changed; re-embedding memory");
                self.reembed_all().await?;
            }
            sqlx::query("INSERT INTO settings_kv (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
                .bind(EMBEDDER_KEY)
                .bind(serde_json::to_string(&current)?)
                .execute(&self.pool)
                .await?;
        }
        let rows: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM memory_items) + (SELECT COUNT(*) FROM episodes)",
        )
        .fetch_one(&self.pool)
        .await?;
        let vec_rows: i64 = sqlx::query_scalar(
            "SELECT (SELECT COUNT(*) FROM memory_items WHERE embedding IS NOT NULL) \
             + (SELECT COUNT(*) FROM episodes WHERE embedding IS NOT NULL)",
        )
        .fetch_one(&self.pool)
        .await?;
        let indexed_vecs =
            self.c.item_vectors.count().await? + self.c.episode_vectors.count().await?;
        if self.c.text.count().await? != rows as u64 || indexed_vecs != vec_rows as u64 {
            tracing::info!(
                rows,
                vec_rows,
                "indexes out of sync with SQLite; rebuilding"
            );
            self.reindex().await?;
        }
        Ok(())
    }

    /// Recompute every stored embedding with the current embedder and push
    /// them to the vector indexes.
    pub async fn reembed_all(&self) -> Result<()> {
        for rec in db::fetch_items(&self.pool, "1 = 1", vec![], false).await? {
            let v = self.embed_one(&gateway::item_embed_text(&rec.item)).await?;
            sqlx::query("UPDATE memory_items SET embedding = ? WHERE id = ?")
                .bind(vec_to_blob(&v))
                .bind(rec.item.id.to_string())
                .execute(&self.pool)
                .await?;
        }
        for (ep, _) in db::fetch_episodes(&self.pool, "1 = 1", vec![]).await? {
            let v = self.embed_one(&pipeline::episode_embed_text(&ep)).await?;
            sqlx::query("UPDATE episodes SET embedding = ? WHERE id = ?")
                .bind(vec_to_blob(&v))
                .bind(ep.id.to_string())
                .execute(&self.pool)
                .await?;
        }
        self.reindex().await
    }

    /// Rebuild the text index and vector indexes from SQLite (the source of
    /// truth). Safe to run at any time.
    pub async fn reindex(&self) -> Result<()> {
        self.c.text.clear().await?;
        let items = db::fetch_items(&self.pool, "1 = 1", vec![], true).await?;
        let docs: Vec<TextDoc> = items.iter().map(|r| gateway::item_doc(&r.item)).collect();
        for chunk in docs.chunks(500) {
            self.c.text.upsert(chunk).await?;
        }
        for r in &items {
            if let Some(v) = &r.embedding {
                self.c
                    .item_vectors
                    .upsert(&r.item.id.to_string(), v, &gateway::item_meta(&r.item))
                    .await?;
            }
        }
        let rows = db::dynq("SELECT e.id, e.embedding FROM episodes e".into())
            .fetch_all(&self.pool)
            .await?;
        let mut emb = std::collections::HashMap::new();
        for row in rows {
            use sqlx::Row;
            let id: String = row.try_get("id")?;
            if let Some(b) = row.try_get::<Option<Vec<u8>>, _>("embedding")? {
                emb.insert(id, blob_to_vec(&b));
            }
        }
        let eps = db::fetch_episodes(&self.pool, "1 = 1", vec![]).await?;
        let ep_docs: Vec<TextDoc> = eps.iter().map(|(e, _)| pipeline::episode_doc(e)).collect();
        for chunk in ep_docs.chunks(500) {
            self.c.text.upsert(chunk).await?;
        }
        for (e, _) in &eps {
            if let Some(v) = emb.get(&e.id.to_string()) {
                self.c
                    .episode_vectors
                    .upsert(&e.id.to_string(), v, &pipeline::episode_meta(e))
                    .await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.c.embedder.embed(&[text.to_string()]).await?;
        v.pop()
            .ok_or_else(|| MemoryError::Embed("embedder returned no vector".into()))
    }
}

/// Kinds a pack never includes (episodes are L1, not packed knowledge).
pub(crate) const PACK_KINDS: [MemoryType; 5] = [
    MemoryType::Convention,
    MemoryType::Procedure,
    MemoryType::Decision,
    MemoryType::Fact,
    MemoryType::Preference,
];

#[cfg(test)]
mod send_tests {
    use super::*;
    use kiln_types::api::{ApproveMemoryRequest, MemoryDecision, UpdateMemoryRequest};
    use kiln_types::memory::{MemoryProposal, MemoryQuery, Visibility};
    use kiln_types::{MemoryId, ProjectId, Run, RunId, UserId, WorkerRole};

    fn assert_send<T: Send>(_: &T) {}

    /// The daemon drives `Memory` from multi-threaded axum handlers and
    /// jobs, so every public future must be `Send` (compile-time check).
    #[allow(dead_code, clippy::too_many_arguments)]
    fn public_futures_are_send(
        m: &Memory,
        u: UserId,
        id: MemoryId,
        run: &Run,
        p: ProjectId,
        prop: MemoryProposal,
    ) {
        let q = MemoryQuery::default();
        let ctx = UseContext::default();
        assert_send(&m.search(u, &q));
        assert_send(&m.search_ctx(u, &q, &ctx));
        assert_send(&m.list(u, &q));
        assert_send(&m.get(u, id));
        assert_send(&m.propose(u, None, prop.clone()));
        assert_send(&m.update(u, id, UpdateMemoryRequest::default()));
        assert_send(&m.decide(
            u,
            id,
            ApproveMemoryRequest {
                decision: MemoryDecision::Approve,
                edit: None,
            },
        ));
        assert_send(&m.record_use(u, &[id], &ctx));
        assert_send(&m.feedback(RunId::new(), true));
        let redact = |s: &str| s.to_string();
        assert_send(&m.ingest_l0(run, None, &[], &redact));
        assert_send(&m.read_l0(run.id));
        assert_send(&m.distill(run, None));
        assert_send(&m.pack(u, p, WorkerRole::Builder));
        assert_send(&m.rebuild_packs(u, p));
        let exists = |_: ProjectId, _: &str| true;
        assert_send(&m.nightly(&NightlyOptions::default(), &exists));
        assert_send(&m.mcp_search(u, None, "q", None, &ctx));
        assert_send(&m.mcp_propose(u, None, prop));
        assert_send(&m.reindex());
        let _ = Visibility::Private;
    }

    #[test]
    fn default_config_roundtrips_through_serde() {
        let cfg: MemoryConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, MemoryConfig::default());
        let cfg: MemoryConfig =
            serde_json::from_str(r#"{"embedder": {"kind": "fastembed", "model": "BAAI/bge-small-en-v1.5"}, "vector_backend": "lance"}"#)
                .unwrap();
        assert_eq!(cfg.vector_backend, VectorBackend::Lance);
        assert!(matches!(cfg.embedder, EmbedderChoice::Fastembed { .. }));
    }
}
