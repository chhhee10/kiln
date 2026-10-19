//! Ad-hoc SQL over L0 via DataFusion (feature `datafusion`).
//!
//! The query sees exactly one table, `l0`, with the Parquet schema of
//! [`crate::parquet_io`] (`run_id, seq, ts, owner, visibility, project,
//! role, type, payload`): compacted Parquet segments **plus** fresh JSONL
//! segments not compacted yet. With `owner = Some(u)` the table is
//! pre-filtered to that user's rows and the raw sources are not reachable.
//! Only queries are allowed (no DDL/DML/statements), so SQL can't write files.

use crate::Memory;
use crate::error::{MemoryError, Result};
use crate::l0::{L0Row, SegFormat, read_jsonl};
use crate::util::ts;
use datafusion::arrow::array::{ArrayRef, RecordBatch, StringArray, UInt64Array};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use datafusion::execution::context::{SQLOptions, SessionContext};
use datafusion::prelude::{ParquetReadOptions, col, lit};
use kiln_types::UserId;
use serde_json::{Map, Value};
use std::sync::Arc;

fn dferr(e: impl std::fmt::Display) -> MemoryError {
    MemoryError::Index(format!("datafusion: {e}"))
}

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("run_id", DataType::Utf8, false),
        Field::new("seq", DataType::UInt64, false),
        Field::new("ts", DataType::Utf8, false),
        Field::new("owner", DataType::Utf8, false),
        Field::new("visibility", DataType::Utf8, false),
        Field::new("project", DataType::Utf8, true),
        Field::new("role", DataType::Utf8, false),
        Field::new("type", DataType::Utf8, false),
        Field::new("payload", DataType::Utf8, false),
    ]))
}

fn batch(rows: &[L0Row]) -> Result<RecordBatch> {
    let s = |f: &dyn Fn(&L0Row) -> String| -> ArrayRef {
        Arc::new(StringArray::from_iter_values(rows.iter().map(f)))
    };
    RecordBatch::try_new(
        schema(),
        vec![
            s(&|r| r.run_id.to_string()),
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.seq))),
            s(&|r| ts(r.ts)),
            s(&|r| r.owner.to_string()),
            s(&|r| r.visibility.as_str().to_string()),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|r| r.project.map(|p| p.to_string()))
                    .collect::<Vec<_>>(),
            )),
            s(&|r| r.role.as_str().to_string()),
            s(&|r| r.kind.clone()),
            s(&|r| r.payload.to_string()),
        ],
    )
    .map_err(dferr)
}

/// One JSON object per row; numbers/bools/strings native, other types as
/// their display string.
fn to_json(batches: &[RecordBatch]) -> Result<Vec<Value>> {
    use datafusion::arrow::array::*;
    let mut out = vec![];
    let opts = FormatOptions::default();
    for b in batches {
        let schema = b.schema();
        let formatters: Vec<ArrayFormatter> = b
            .columns()
            .iter()
            .map(|c| ArrayFormatter::try_new(c.as_ref(), &opts))
            .collect::<Result<_, _>>()
            .map_err(dferr)?;
        for row in 0..b.num_rows() {
            let mut obj = Map::new();
            for (i, column) in b.columns().iter().enumerate() {
                let name = schema.field(i).name().clone();
                let a = column.as_ref();
                let v = if a.is_null(row) {
                    Value::Null
                } else if let Some(x) = a.as_any().downcast_ref::<StringArray>() {
                    Value::String(x.value(row).to_string())
                } else if let Some(x) = a.as_any().downcast_ref::<LargeStringArray>() {
                    Value::String(x.value(row).to_string())
                } else if let Some(x) = a.as_any().downcast_ref::<StringViewArray>() {
                    Value::String(x.value(row).to_string())
                } else if let Some(x) = a.as_any().downcast_ref::<BooleanArray>() {
                    Value::Bool(x.value(row))
                } else if let Some(x) = a.as_any().downcast_ref::<Int64Array>() {
                    x.value(row).into()
                } else if let Some(x) = a.as_any().downcast_ref::<Int32Array>() {
                    x.value(row).into()
                } else if let Some(x) = a.as_any().downcast_ref::<UInt64Array>() {
                    x.value(row).into()
                } else if let Some(x) = a.as_any().downcast_ref::<UInt32Array>() {
                    x.value(row).into()
                } else if let Some(x) = a.as_any().downcast_ref::<Float64Array>() {
                    serde_json::Number::from_f64(x.value(row))
                        .map(Value::Number)
                        .unwrap_or(Value::Null)
                } else if let Some(x) = a.as_any().downcast_ref::<Float32Array>() {
                    serde_json::Number::from_f64(f64::from(x.value(row)))
                        .map(Value::Number)
                        .unwrap_or(Value::Null)
                } else {
                    Value::String(formatters[i].value(row).to_string())
                };
                obj.insert(name, v);
            }
            out.push(Value::Object(obj));
        }
    }
    Ok(out)
}

impl Memory {
    /// Run a read-only SQL query over L0 (`SELECT … FROM l0 …`) and return
    /// the rows as JSON objects. `owner` restricts the table to one user's
    /// rows; pass `None` only for admin/maintenance callers.
    pub async fn query_l0(&self, owner: Option<UserId>, sql: &str) -> Result<Vec<Value>> {
        // Fresh JSONL rows (not yet compacted) — read under the L0 lock.
        let (recent, parquet_files) = {
            let _g = self.l0.lock().await;
            let segs = self.scan_segments(None)?;
            let mut rows = vec![];
            for s in segs.iter().filter(|s| s.format == SegFormat::Jsonl) {
                rows.extend(read_jsonl(&s.path)?);
            }
            let files: Vec<String> = segs
                .iter()
                .filter(|s| s.format == SegFormat::Parquet)
                .map(|s| s.path.to_string_lossy().to_string())
                .collect();
            (rows, files)
        };
        let ctx = SessionContext::new();
        let mut df = ctx.read_batch(batch(&recent)?).map_err(dferr)?;
        if !parquet_files.is_empty() {
            let schema = schema();
            let pq = ctx
                .read_parquet(
                    parquet_files,
                    ParquetReadOptions::default()
                        .schema(&schema)
                        .file_extension(".parquet"),
                )
                .await
                .map_err(dferr)?;
            df = pq.union(df).map_err(dferr)?;
        }
        if let Some(u) = owner {
            df = df
                .filter(col("owner").eq(lit(u.to_string())))
                .map_err(dferr)?;
        }
        ctx.register_table("l0", df.into_view()).map_err(dferr)?;
        let opts = SQLOptions::new()
            .with_allow_ddl(false)
            .with_allow_dml(false)
            .with_allow_statements(false);
        let batches = ctx
            .sql_with_options(sql, opts)
            .await
            .map_err(dferr)?
            .collect()
            .await
            .map_err(dferr)?;
        to_json(&batches)
    }
}
