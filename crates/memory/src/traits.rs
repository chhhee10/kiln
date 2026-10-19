//! Pluggable backends ("heavy indexes behind traits", DECISIONS.md).
//!
//! Indexes are **accelerators, never authorities**: the gateway always
//! re-checks every candidate against SQLite with the visibility predicate
//! before returning it, so a stale or buggy index can only cost recall, never
//! leak an item. The [`SearchFilter`] passed to `search` is a *pre-filter*
//! hint so top-k is not wasted on rows the viewer can't see.

use crate::error::Result;
use async_trait::async_trait;
use kiln_types::memory::{MemoryStatus, MemoryType, Visibility};
use kiln_types::{ProjectId, UserId};

/// Access tags stored alongside every indexed row (PRD §7.9: every row in
/// every layer carries owner/visibility/shared_with/project/type/status).
#[derive(Clone, Debug, PartialEq)]
pub struct IndexMeta {
    pub owner: UserId,
    pub kind: MemoryType,
    pub visibility: Visibility,
    pub shared_with: Vec<UserId>,
    pub project: Option<ProjectId>,
    pub status: MemoryStatus,
}

impl IndexMeta {
    /// The same rule as [`kiln_types::memory::MemoryItem::visible_to`],
    /// evaluated on index metadata.
    pub fn visible_to(&self, user: UserId) -> bool {
        if self.owner == user {
            return true;
        }
        if matches!(self.status, MemoryStatus::Pending | MemoryStatus::Rejected) {
            return false;
        }
        if self.kind == MemoryType::Preference {
            return false;
        }
        match self.visibility {
            Visibility::Private => false,
            Visibility::Shared => self.shared_with.contains(&user),
            Visibility::Company => true,
        }
    }
}

/// Pre-filter for index searches.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchFilter {
    /// The requesting user; only rows visible to them should be returned.
    pub viewer: UserId,
    /// Restrict to this project **plus** company-wide rows (`project = None`).
    pub project: Option<ProjectId>,
    /// Allowed statuses (empty = any).
    pub statuses: Vec<MemoryStatus>,
    /// Allowed kinds (`None` = any).
    pub kinds: Option<Vec<MemoryType>>,
}

impl SearchFilter {
    /// A filter that admits everything the viewer can see.
    pub fn for_viewer(viewer: UserId) -> Self {
        Self {
            viewer,
            project: None,
            statuses: vec![],
            kinds: None,
        }
    }

    /// Whether a row with `meta` passes this filter.
    pub fn admits(&self, meta: &IndexMeta) -> bool {
        meta.visible_to(self.viewer)
            && (self.statuses.is_empty() || self.statuses.contains(&meta.status))
            && self.kinds.as_ref().is_none_or(|k| k.contains(&meta.kind))
            && match self.project {
                None => true,
                Some(p) => meta.project.is_none() || meta.project == Some(p),
            }
    }
}

/// Text → dense vectors.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Stable identifier of the model + dimension (e.g. `hash-v1:384`).
    /// Stored embeddings are recomputed when this changes.
    fn id(&self) -> String;
    /// Output dimension.
    fn dim(&self) -> usize;
    /// Embed a batch of texts; returns one vector of length [`Self::dim`] per text.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Approximate or exact nearest-neighbour index over embeddings.
#[async_trait]
pub trait VectorIndex: Send + Sync {
    /// Short backend name for logs (`sqlite`, `lance`).
    fn name(&self) -> &'static str;
    /// Insert or replace the vector and access tags for `id`.
    async fn upsert(&self, id: &str, vector: &[f32], meta: &IndexMeta) -> Result<()>;
    /// Remove `id` (no-op if absent).
    async fn remove(&self, id: &str) -> Result<()>;
    /// Top-`k` ids by cosine similarity (higher is better), pre-filtered.
    async fn search(
        &self,
        vector: &[f32],
        k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<(String, f32)>>;
    /// Number of vectors stored (used to detect an index that needs a rebuild).
    async fn count(&self) -> Result<u64>;
}

/// A document in the full-text index.
#[derive(Clone, Debug, PartialEq)]
pub struct TextDoc {
    pub id: String,
    pub title: String,
    pub body: String,
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub meta: IndexMeta,
}

/// Keyword (BM25) index.
#[async_trait]
pub trait TextIndex: Send + Sync {
    /// Insert or replace documents (by id) and make them searchable.
    async fn upsert(&self, docs: &[TextDoc]) -> Result<()>;
    /// Delete documents by id (no-op for unknown ids).
    async fn delete(&self, ids: &[String]) -> Result<()>;
    /// Top-`k` ids by keyword relevance, pre-filtered. Query syntax errors
    /// are tolerated (lenient parsing), never surfaced to users.
    async fn search(
        &self,
        query: &str,
        k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<(String, f32)>>;
    /// Remove every document (used by full reindex).
    async fn clear(&self) -> Result<()>;
    /// Number of documents currently searchable.
    async fn count(&self) -> Result<u64>;
}

/// Second-stage relevance scoring.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Short backend name for logs.
    fn name(&self) -> &'static str;
    /// One score per candidate, higher is better, ideally in `[0, 1]`.
    async fn rerank(&self, query: &str, candidates: &[String]) -> Result<Vec<f32>>;
}
