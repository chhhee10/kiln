//! Error type for every memory operation.

/// Errors returned by [`crate::Memory`] and the index/embedder traits.
#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    /// The item does not exist **or is not visible to the caller**. The two
    /// cases are deliberately indistinguishable so that existence of another
    /// user's private item never leaks.
    #[error("not found")]
    NotFound,
    /// The caller can see the item but may not perform this action (only the
    /// owner may edit, change visibility or decide on approval).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// The request breaks a memory rule (e.g. sharing a preference).
    #[error("invalid: {0}")]
    Invalid(String),
    /// The item is not in a state that allows the action (e.g. deciding on
    /// an item that is not pending).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Underlying database error.
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    /// Filesystem error (L0 segments, index directories).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON column or payload could not be (de)serialized.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// A text or vector index failed.
    #[error("index error: {0}")]
    Index(String),
    /// The embedder or reranker failed.
    #[error("embedding error: {0}")]
    Embed(String),
    /// A stored row could not be mapped back to a domain type.
    #[error("corrupt row: {0}")]
    Corrupt(String),
    /// The requested backend was not compiled in (missing cargo feature).
    #[error("unsupported: {0}")]
    Unsupported(String),
}

impl MemoryError {
    /// True for [`MemoryError::NotFound`].
    pub fn is_not_found(&self) -> bool {
        matches!(self, MemoryError::NotFound)
    }
    /// True for [`MemoryError::Forbidden`].
    pub fn is_forbidden(&self) -> bool {
        matches!(self, MemoryError::Forbidden(_))
    }
    /// True for [`MemoryError::Invalid`].
    pub fn is_invalid(&self) -> bool {
        matches!(self, MemoryError::Invalid(_))
    }
    /// True for [`MemoryError::Conflict`].
    pub fn is_conflict(&self) -> bool {
        matches!(self, MemoryError::Conflict(_))
    }

    pub(crate) fn index(e: impl std::fmt::Display) -> Self {
        MemoryError::Index(e.to_string())
    }
}

impl From<tantivy::TantivyError> for MemoryError {
    fn from(e: tantivy::TantivyError) -> Self {
        MemoryError::Index(e.to_string())
    }
}

/// Result alias for memory operations.
pub type Result<T, E = MemoryError> = std::result::Result<T, E>;
