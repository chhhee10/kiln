//! Built-in exact cosine vector index over the `embedding` BLOB columns.
//!
//! Fine for a small company brain (≈10⁵ rows: one sequential scan of
//! 384-dim f32 vectors per query). The access tags are the table's own
//! columns, so the pre-filter is the real visibility predicate.

use crate::db::{Arg, bind_all, dynq, episode_visible_pred, status_in, visible_pred};
use crate::error::Result;
use crate::traits::{IndexMeta, SearchFilter, VectorIndex};
use crate::util::{blob_to_vec, cosine, vec_to_blob};
use async_trait::async_trait;
use sqlx::{Row, SqlitePool};

/// Which table a [`SqliteVectorIndex`] covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collection {
    /// L2 `memory_items`.
    Items,
    /// L1 `episodes`.
    Episodes,
}

impl Collection {
    /// Table / collection name.
    pub fn name(self) -> &'static str {
        match self {
            Collection::Items => "items",
            Collection::Episodes => "episodes",
        }
    }
}

/// Exact cosine search over `memory_items.embedding` or `episodes.embedding`.
#[derive(Clone, Debug)]
pub struct SqliteVectorIndex {
    pool: SqlitePool,
    collection: Collection,
}

impl SqliteVectorIndex {
    /// An index over `collection` in the given database.
    pub fn new(pool: SqlitePool, collection: Collection) -> Self {
        Self { pool, collection }
    }

    fn table(&self) -> &'static str {
        match self.collection {
            Collection::Items => "memory_items",
            Collection::Episodes => "episodes",
        }
    }
}

#[async_trait]
impl VectorIndex for SqliteVectorIndex {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    async fn upsert(&self, id: &str, vector: &[f32], _meta: &IndexMeta) -> Result<()> {
        // The access tags live in the row itself; only the vector is written.
        dynq(format!(
            "UPDATE {} SET embedding = ? WHERE id = ?",
            self.table()
        ))
        .bind(vec_to_blob(vector))
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<()> {
        dynq(format!(
            "UPDATE {} SET embedding = NULL WHERE id = ?",
            self.table()
        ))
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn search(
        &self,
        vector: &[f32],
        k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<(String, f32)>> {
        if k == 0 || vector.iter().all(|x| *x == 0.0) {
            return Ok(vec![]);
        }
        let mut args = vec![];
        let sql = match self.collection {
            Collection::Items => {
                let mut w = vec![
                    "m.embedding IS NOT NULL".to_string(),
                    visible_pred("m", filter.viewer),
                ];
                if !filter.statuses.is_empty() {
                    w.push(status_in("m.status", &filter.statuses));
                }
                if let Some(kinds) = &filter.kinds {
                    let list = kinds
                        .iter()
                        .map(|k| format!("'{}'", k.as_str()))
                        .collect::<Vec<_>>()
                        .join(",");
                    w.push(format!("m.type IN ({list})"));
                }
                if let Some(p) = filter.project {
                    w.push("(m.project_id IS NULL OR m.project_id = ?)".into());
                    args.push(Arg::Text(p.to_string()));
                }
                format!(
                    "SELECT m.id, m.embedding FROM memory_items m WHERE {}",
                    w.join(" AND ")
                )
            }
            Collection::Episodes => {
                let mut w = vec![
                    "e.embedding IS NOT NULL".to_string(),
                    episode_visible_pred("e", filter.viewer),
                ];
                if let Some(p) = filter.project {
                    w.push("(e.project_id IS NULL OR e.project_id = ?)".into());
                    args.push(Arg::Text(p.to_string()));
                }
                format!(
                    "SELECT e.id, e.embedding FROM episodes e WHERE {}",
                    w.join(" AND ")
                )
            }
        };
        let rows = bind_all(dynq(sql), args).fetch_all(&self.pool).await?;
        let mut scored = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            let blob: Vec<u8> = row.try_get("embedding")?;
            scored.push((id, cosine(vector, &blob_to_vec(&blob))));
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        Ok(scored)
    }

    async fn count(&self) -> Result<u64> {
        let n: i64 = dynq(format!(
            "SELECT COUNT(*) FROM {} WHERE embedding IS NOT NULL",
            self.table()
        ))
        .try_map(|r: sqlx::sqlite::SqliteRow| r.try_get::<i64, _>(0))
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u64)
    }
}
