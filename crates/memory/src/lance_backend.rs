//! LanceDB vector index at `<data>/index/lance` (feature `lance`).
//!
//! One table per [`Collection`] with the vector plus the access tags as
//! columns, so the visibility pre-filter runs inside Lance (`only_if`).
//! Results are still re-checked by the gateway against SQLite.
//!
//! Building needs `protoc` (lance's build scripts use prost-build) and
//! lancedb's `remote` feature (lancedb ≤ 0.40 does not compile without it).

use crate::error::{MemoryError, Result};
use crate::traits::{IndexMeta, SearchFilter, VectorIndex};
use crate::vector::Collection;
use async_trait::async_trait;
use futures::TryStreamExt;
use kiln_types::memory::{MemoryStatus, MemoryType, Visibility};
use lancedb::arrow::arrow_array::types::Float32Type;
use lancedb::arrow::arrow_array::{
    Array, FixedSizeListArray, Float32Array, RecordBatch, StringArray,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{DistanceType, Table};
use std::path::Path;
use std::sync::Arc;

const NO_PROJECT: &str = "_none_";

fn lerr(e: impl std::fmt::Display) -> MemoryError {
    MemoryError::Index(format!("lance: {e}"))
}

/// LanceDB-backed [`VectorIndex`].
pub struct LanceVectorIndex {
    table: Table,
    schema: Arc<Schema>,
    dim: usize,
    write: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for LanceVectorIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanceVectorIndex")
            .field("table", &self.table.name())
            .field("dim", &self.dim)
            .finish()
    }
}

fn schema(dim: usize) -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("owner", DataType::Utf8, false),
        Field::new("visibility", DataType::Utf8, false),
        Field::new("shared", DataType::Utf8, false),
        Field::new("project", DataType::Utf8, false),
        Field::new("status", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            true,
        ),
    ]))
}

impl LanceVectorIndex {
    /// Open (or create) the table for `collection` under `dir`. A table
    /// whose vector width differs from `dim` (embedder changed) is dropped
    /// and recreated empty; `Memory::open` then re-fills it.
    pub async fn open(dir: &Path, collection: Collection, dim: usize) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let uri = dir.to_string_lossy().to_string();
        let db = lancedb::connect(&uri).execute().await.map_err(lerr)?;
        let name = collection.name();
        let schema = schema(dim);
        let names = db.table_names().execute().await.map_err(lerr)?;
        let table = if names.iter().any(|n| n == name) {
            let t = db.open_table(name).execute().await.map_err(lerr)?;
            let existing = t.schema().await.map_err(lerr)?;
            let width = existing
                .field_with_name("vector")
                .ok()
                .and_then(|f| match f.data_type() {
                    DataType::FixedSizeList(_, n) => Some(*n as usize),
                    _ => None,
                });
            if width == Some(dim) {
                t
            } else {
                tracing::warn!(
                    table = name,
                    ?width,
                    dim,
                    "lance table has a different vector width; recreating"
                );
                db.drop_table(name, &[]).await.map_err(lerr)?;
                db.create_empty_table(name, schema.clone())
                    .execute()
                    .await
                    .map_err(lerr)?
            }
        } else {
            db.create_empty_table(name, schema.clone())
                .execute()
                .await
                .map_err(lerr)?
        };
        Ok(Self {
            table,
            schema,
            dim,
            write: tokio::sync::Mutex::new(()),
        })
    }

    fn batch(&self, id: &str, v: &[f32], m: &IndexMeta) -> Result<RecordBatch> {
        let s = |x: String| Arc::new(StringArray::from(vec![x])) as Arc<dyn Array>;
        let shared = m
            .shared_with
            .iter()
            .map(|u| u.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let vector = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            std::iter::once(Some(v.iter().map(|x| Some(*x)).collect::<Vec<_>>())),
            self.dim as i32,
        );
        RecordBatch::try_new(
            self.schema.clone(),
            vec![
                s(id.to_string()),
                s(m.owner.to_string()),
                s(m.visibility.as_str().into()),
                s(shared),
                s(m.project
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| NO_PROJECT.into())),
                s(m.status.as_str().into()),
                s(m.kind.as_str().into()),
                Arc::new(vector),
            ],
        )
        .map_err(lerr)
    }

    /// SQL pre-filter mirroring `visible_to` (all values are uuids or
    /// static enum strings, so inlining is injection-safe).
    fn filter_sql(f: &SearchFilter) -> String {
        let u = f.viewer;
        let mut w = vec![format!(
            "(owner = '{u}' OR (status NOT IN ('pending','rejected') AND kind <> 'preference' AND \
             (visibility = 'company' OR (visibility = 'shared' AND shared LIKE '%{u}%'))))"
        )];
        if !f.statuses.is_empty() {
            let s = f
                .statuses
                .iter()
                .map(|s| format!("'{}'", s.as_str()))
                .collect::<Vec<_>>()
                .join(",");
            w.push(format!("status IN ({s})"));
        }
        if let Some(kinds) = &f.kinds {
            let k = kinds
                .iter()
                .map(|k| format!("'{}'", k.as_str()))
                .collect::<Vec<_>>()
                .join(",");
            w.push(format!("kind IN ({k})"));
        }
        if let Some(p) = f.project {
            w.push(format!("(project = '{p}' OR project = '{NO_PROJECT}')"));
        }
        w.join(" AND ")
    }
}

fn str_col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| lerr(format!("no column {name}")))
}

#[async_trait]
impl VectorIndex for LanceVectorIndex {
    fn name(&self) -> &'static str {
        "lance"
    }

    async fn upsert(&self, id: &str, vector: &[f32], meta: &IndexMeta) -> Result<()> {
        if vector.len() != self.dim {
            return Err(lerr(format!(
                "vector has {} dims, table has {}",
                vector.len(),
                self.dim
            )));
        }
        let batch = self.batch(id, vector, meta)?;
        let _g = self.write.lock().await;
        self.table
            .delete(format!("id = '{}'", id.replace('\'', "''")).as_str())
            .await
            .map_err(lerr)?;
        self.table.add(batch).execute().await.map_err(lerr)?;
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<()> {
        let _g = self.write.lock().await;
        self.table
            .delete(format!("id = '{}'", id.replace('\'', "''")).as_str())
            .await
            .map_err(lerr)?;
        Ok(())
    }

    async fn search(
        &self,
        vector: &[f32],
        k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<(String, f32)>> {
        if k == 0 || vector.len() != self.dim || vector.iter().all(|x| *x == 0.0) {
            return Ok(vec![]);
        }
        let batches: Vec<RecordBatch> = self
            .table
            .query()
            .nearest_to(vector)
            .map_err(lerr)?
            .distance_type(DistanceType::Cosine)
            .only_if(Self::filter_sql(filter))
            .limit(k)
            .execute()
            .await
            .map_err(lerr)?
            .try_collect()
            .await
            .map_err(lerr)?;
        let mut out = vec![];
        for b in &batches {
            let (ids, owner, vis, shared, project, status, kind) = (
                str_col(b, "id")?,
                str_col(b, "owner")?,
                str_col(b, "visibility")?,
                str_col(b, "shared")?,
                str_col(b, "project")?,
                str_col(b, "status")?,
                str_col(b, "kind")?,
            );
            let dist = b
                .column_by_name("_distance")
                .and_then(|c| c.as_any().downcast_ref::<Float32Array>())
                .ok_or_else(|| lerr("no _distance column"))?;
            for i in 0..b.num_rows() {
                let meta = IndexMeta {
                    owner: owner.value(i).parse().map_err(lerr)?,
                    kind: MemoryType::parse(kind.value(i)).unwrap_or(MemoryType::Fact),
                    visibility: Visibility::parse(vis.value(i)).unwrap_or(Visibility::Private),
                    shared_with: shared
                        .value(i)
                        .split(',')
                        .filter_map(|s| s.parse().ok())
                        .collect(),
                    project: Some(project.value(i))
                        .filter(|p| *p != NO_PROJECT)
                        .and_then(|p| p.parse().ok()),
                    status: MemoryStatus::parse(status.value(i)).unwrap_or(MemoryStatus::Archived),
                };
                if filter.admits(&meta) {
                    out.push((ids.value(i).to_string(), 1.0 - dist.value(i)));
                }
            }
        }
        out.sort_by(|a, b| b.1.total_cmp(&a.1));
        out.truncate(k);
        Ok(out)
    }

    async fn count(&self) -> Result<u64> {
        Ok(self.table.count_rows(None).await.map_err(lerr)? as u64)
    }
}
