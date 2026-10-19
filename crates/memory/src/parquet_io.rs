//! L0 Parquet (zstd) segments (feature `parquet`).
//!
//! Schema (all non-null except `project`):
//! `run_id: utf8, seq: uint64, ts: utf8 (RFC 3339 UTC, µs), owner: utf8,
//! visibility: utf8, project: utf8?, role: utf8, type: utf8, payload: utf8 (JSON)`.

use crate::error::{MemoryError, Result};
use crate::l0::L0Row;
use crate::util::{parse_id, parse_ts, ts};
use arrow_array::{Array, ArrayRef, RecordBatch, StringArray, UInt64Array};
use arrow_schema::{DataType, Field, Schema};
use kiln_types::WorkerRole;
use kiln_types::memory::Visibility;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use std::path::Path;
use std::sync::Arc;

fn pq_err(e: impl std::fmt::Display) -> MemoryError {
    MemoryError::Index(format!("parquet: {e}"))
}

/// The Arrow schema of an L0 Parquet file.
pub(crate) fn schema() -> Arc<Schema> {
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

/// Write rows as one zstd-compressed Parquet file (overwrites `path`).
pub(crate) fn write_rows(path: &Path, rows: &[L0Row]) -> Result<()> {
    let s = |f: &dyn Fn(&L0Row) -> String| -> ArrayRef {
        Arc::new(StringArray::from_iter_values(rows.iter().map(f)))
    };
    let cols: Vec<ArrayRef> = vec![
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
    ];
    let batch = RecordBatch::try_new(schema(), cols).map_err(pq_err)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3).map_err(pq_err)?))
        .build();
    let file = std::fs::File::create(path)?;
    let mut w = ArrowWriter::try_new(file, schema(), Some(props)).map_err(pq_err)?;
    w.write(&batch).map_err(pq_err)?;
    let file = w.into_inner().map_err(pq_err)?;
    file.sync_all()?;
    Ok(())
}

fn col<'a>(b: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    b.column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .ok_or_else(|| pq_err(format!("missing utf8 column {name}")))
}

/// Read every row of an L0 Parquet file.
pub(crate) fn read_rows(path: &Path) -> Result<Vec<L0Row>> {
    let file = std::fs::File::open(path)?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(pq_err)?
        .build()
        .map_err(pq_err)?;
    let mut out = vec![];
    for batch in reader {
        let b = batch.map_err(pq_err)?;
        let seq = b
            .column_by_name("seq")
            .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
            .ok_or_else(|| pq_err("missing uint64 column seq"))?;
        let (run, tsc, owner, vis, project, role, kind, payload) = (
            col(&b, "run_id")?,
            col(&b, "ts")?,
            col(&b, "owner")?,
            col(&b, "visibility")?,
            col(&b, "project")?,
            col(&b, "role")?,
            col(&b, "type")?,
            col(&b, "payload")?,
        );
        for i in 0..b.num_rows() {
            out.push(L0Row {
                run_id: parse_id(run.value(i))?,
                seq: seq.value(i),
                ts: parse_ts(tsc.value(i))?,
                owner: parse_id(owner.value(i))?,
                visibility: Visibility::parse(vis.value(i)).unwrap_or(Visibility::Private),
                project: if project.is_null(i) {
                    None
                } else {
                    Some(parse_id(project.value(i))?)
                },
                role: WorkerRole::parse(role.value(i)).ok_or_else(|| pq_err("bad role"))?,
                kind: kind.value(i).to_string(),
                payload: serde_json::from_str(payload.value(i))?,
            });
        }
    }
    Ok(out)
}
