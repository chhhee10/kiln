//! L0 raw capture: redacted, normalized run events in zstd JSONL segments.
//!
//! Layout: `<data>/l0/YYYY-MM-DD/<run_id>.jsonl.zst` (date of the event's
//! timestamp, UTC). Each ingest batch appends **one zstd frame**;
//! concatenated frames decode as one stream. Nightly compaction rewrites a
//! day's segment as `<run_id>.parquet` (feature `parquet`). Segments are
//! registered in `l0_segments` (path relative to `<data>`).
//!
//! The files are the source of truth for idempotency: the set of sequence
//! numbers already stored for a run is read back from its segments on first
//! touch and cached, so (run_id, seq) is ingested at most once even across
//! daemon restarts or crashes between the append and the segment-index update.

use crate::Memory;
use crate::error::{MemoryError, Result};
use crate::util::{now_ts, parse_ts, ts};
use chrono::SubsecRound;
use kiln_types::event::{RunEvent, RunEventKind};
use kiln_types::memory::Visibility;
use kiln_types::{ProjectId, Run, RunId, Timestamp, UserId, WorkerRole};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// How many runs' seq sets are cached before the cache is reset.
const SEEN_CACHE_RUNS: usize = 256;
/// zstd level for L0 frames.
const ZSTD_LEVEL: i32 = 3;

/// One normalized L0 row (PRD §7.9: every row carries access tags).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct L0Row {
    pub run_id: RunId,
    pub seq: u64,
    pub ts: Timestamp,
    pub owner: UserId,
    /// Always `private` for L0.
    pub visibility: Visibility,
    pub project: Option<ProjectId>,
    pub role: WorkerRole,
    /// The `RunEventKind` tag (`log`, `command`, …).
    #[serde(rename = "type")]
    pub kind: String,
    /// The redacted `RunEventKind` as JSON (including its `type` tag).
    pub payload: serde_json::Value,
}

impl L0Row {
    /// Back to a [`RunEvent`]; `None` if the payload no longer parses.
    pub fn to_event(&self) -> Option<RunEvent> {
        let kind: RunEventKind = serde_json::from_value(self.payload.clone()).ok()?;
        Some(RunEvent {
            run_id: self.run_id,
            seq: self.seq,
            ts: self.ts,
            kind,
        })
    }
}

/// Result of [`Memory::ingest_l0`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestReport {
    /// Rows appended.
    pub appended: u64,
    /// Events skipped because (run_id, seq) was already stored.
    pub duplicates: u64,
}

/// In-process L0 state, behind `Memory::l0` (a mutex that also serializes
/// appends and compaction).
#[derive(Debug, Default)]
pub(crate) struct L0State {
    seen: HashMap<RunId, HashSet<u64>>,
}

/// Segment file format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SegFormat {
    Jsonl,
    #[cfg_attr(not(feature = "parquet"), allow(dead_code))]
    Parquet,
}

impl SegFormat {
    pub(crate) fn ext(self) -> &'static str {
        match self {
            SegFormat::Jsonl => "jsonl.zst",
            SegFormat::Parquet => "parquet",
        }
    }
}

/// A segment file found on disk.
#[derive(Clone, Debug)]
pub(crate) struct SegFile {
    pub path: PathBuf,
    pub date: chrono::NaiveDate,
    pub run: RunId,
    pub format: SegFormat,
}

/// Redact every string leaf of a JSON value (keys are left alone). Applying
/// the redactor to leaves, not to serialized JSON text, guarantees secrets
/// containing quotes or backslashes are still caught.
fn redact_value(v: &mut serde_json::Value, redact: &(dyn Fn(&str) -> String + Sync)) {
    match v {
        serde_json::Value::String(s) => *s = redact(s),
        serde_json::Value::Array(a) => a.iter_mut().for_each(|x| redact_value(x, redact)),
        serde_json::Value::Object(m) => m.values_mut().for_each(|x| redact_value(x, redact)),
        _ => {}
    }
}

/// Decode a zstd JSONL segment, tolerating a torn trailing frame.
pub(crate) fn read_jsonl(path: &Path) -> Result<Vec<L0Row>> {
    Ok(read_jsonl_checked(path)?.0)
}

/// Like [`read_jsonl`], also reporting whether the file decoded cleanly
/// (`false` after a crash mid-append left a torn frame).
pub(crate) fn read_jsonl_checked(path: &Path) -> Result<(Vec<L0Row>, bool)> {
    let file = std::fs::File::open(path)?;
    let dec = zstd::stream::read::Decoder::new(file)?;
    let mut out = vec![];
    let mut clean = true;
    for line in BufReader::new(dec).lines() {
        match line {
            Ok(l) if l.trim().is_empty() => {}
            Ok(l) => match serde_json::from_str::<L0Row>(&l) {
                Ok(r) => out.push(r),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping corrupt L0 line")
                }
            },
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "L0 segment truncated; keeping rows read so far");
                clean = false;
                break;
            }
        }
    }
    Ok((out, clean))
}

/// Rewrite a torn JSONL segment with its readable rows (one frame), so
/// frames appended later stay readable.
fn repair_jsonl(path: &Path, rows: &[L0Row]) -> Result<()> {
    let mut buf = Vec::new();
    for r in rows {
        serde_json::to_writer(&mut buf, r)?;
        buf.push(b'\n');
    }
    let tmp = path.with_extension("zst.repair");
    std::fs::write(&tmp, zstd::stream::encode_all(&buf[..], ZSTD_LEVEL)?)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, path)?;
    tracing::warn!(path = %path.display(), rows = rows.len(), "repaired torn L0 segment");
    Ok(())
}

/// Read any segment file.
pub(crate) fn read_segment(seg: &SegFile) -> Result<Vec<L0Row>> {
    match seg.format {
        SegFormat::Jsonl => read_jsonl(&seg.path),
        #[cfg(feature = "parquet")]
        SegFormat::Parquet => crate::parquet_io::read_rows(&seg.path),
        #[cfg(not(feature = "parquet"))]
        SegFormat::Parquet => {
            tracing::warn!(path = %seg.path.display(), "parquet segment present but feature `parquet` is off");
            Ok(vec![])
        }
    }
}

impl Memory {
    pub(crate) fn l0_dir(&self) -> PathBuf {
        self.data_dir.join("l0")
    }

    /// Relative path stored in `l0_segments`.
    pub(crate) fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.data_dir)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Every segment file on disk, optionally only for one run.
    pub(crate) fn scan_segments(&self, run: Option<RunId>) -> Result<Vec<SegFile>> {
        let mut out = vec![];
        let root = self.l0_dir();
        let Ok(days) = std::fs::read_dir(&root) else {
            return Ok(out);
        };
        for day in days.flatten() {
            let Some(date) = day
                .file_name()
                .to_str()
                .and_then(|n| chrono::NaiveDate::parse_from_str(n, "%Y-%m-%d").ok())
            else {
                continue;
            };
            let candidates: Vec<PathBuf> = match run {
                Some(r) => [SegFormat::Jsonl, SegFormat::Parquet]
                    .iter()
                    .map(|f| day.path().join(format!("{r}.{}", f.ext())))
                    .filter(|p| p.exists())
                    .collect(),
                None => std::fs::read_dir(day.path())?
                    .flatten()
                    .map(|e| e.path())
                    .collect(),
            };
            for path in candidates {
                let Some(name) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                let (stem, format) = if let Some(s) = name.strip_suffix(".jsonl.zst") {
                    (s, SegFormat::Jsonl)
                } else if let Some(s) = name.strip_suffix(".parquet") {
                    (s, SegFormat::Parquet)
                } else {
                    continue;
                };
                let Ok(run_id) = stem.parse::<RunId>() else {
                    continue;
                };
                out.push(SegFile {
                    path: path.clone(),
                    date,
                    run: run_id,
                    format,
                });
            }
        }
        out.sort_by(|a, b| (a.date, a.run, a.format.ext()).cmp(&(b.date, b.run, b.format.ext())));
        Ok(out)
    }

    /// Append run events to L0 (normalized, redacted, access-tagged).
    ///
    /// `redact` is applied to every string in each event payload (use the
    /// vault's `Redactor` plus pattern scanning). Idempotent on
    /// `(run_id, seq)`: re-sending a spool is safe. Every event must belong
    /// to `run`.
    pub async fn ingest_l0(
        &self,
        run: &Run,
        project: Option<ProjectId>,
        events: &[RunEvent],
        redact: &(dyn Fn(&str) -> String + Sync),
    ) -> Result<IngestReport> {
        if let Some(e) = events.iter().find(|e| e.run_id != run.id) {
            return Err(MemoryError::Invalid(format!(
                "event seq {} belongs to run {}, not {}",
                e.seq, e.run_id, run.id
            )));
        }
        let mut st = self.l0.lock().await;
        if !st.seen.contains_key(&run.id) {
            if st.seen.len() >= SEEN_CACHE_RUNS {
                st.seen.clear();
            }
            let mut set = HashSet::new();
            for seg in self.scan_segments(Some(run.id))? {
                let rows = if seg.format == SegFormat::Jsonl {
                    let (rows, clean) = read_jsonl_checked(&seg.path)?;
                    if !clean {
                        repair_jsonl(&seg.path, &rows)?;
                    }
                    rows
                } else {
                    read_segment(&seg)?
                };
                set.extend(rows.into_iter().map(|r| r.seq));
            }
            st.seen.insert(run.id, set);
        }
        let seen = st.seen.get_mut(&run.id).expect("inserted above");
        let mut report = IngestReport::default();
        let mut by_day: BTreeMap<chrono::NaiveDate, Vec<L0Row>> = BTreeMap::new();
        for e in events {
            if !seen.insert(e.seq) {
                report.duplicates += 1;
                continue;
            }
            let mut payload = serde_json::to_value(&e.kind)?;
            redact_value(&mut payload, redact);
            let kind = payload
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("unknown")
                .to_string();
            // Store convention: microsecond precision (same in JSONL and Parquet).
            let at = e.ts.trunc_subsecs(6);
            by_day.entry(at.date_naive()).or_default().push(L0Row {
                run_id: run.id,
                seq: e.seq,
                ts: at,
                owner: run.owner_id,
                visibility: Visibility::Private,
                project,
                role: run.role,
                kind,
                payload,
            });
        }
        for (day, rows) in by_day {
            let dir = self.l0_dir().join(day.format("%Y-%m-%d").to_string());
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!("{}.{}", run.id, SegFormat::Jsonl.ext()));
            let mut buf = Vec::new();
            for r in &rows {
                serde_json::to_writer(&mut buf, r)?;
                buf.push(b'\n');
            }
            let frame = zstd::stream::encode_all(&buf[..], ZSTD_LEVEL)?;
            let res = (|| -> std::io::Result<u64> {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                let before = f.metadata()?.len();
                if let Err(e) = f.write_all(&frame).and_then(|_| f.sync_data()) {
                    // Never leave a torn frame behind a healthy file.
                    let _ = f.set_len(before);
                    return Err(e);
                }
                Ok(f.metadata()?.len())
            })();
            let bytes = match res {
                Ok(b) => b,
                Err(e) => {
                    // Not stored: forget these seqs so a retry can append them.
                    for r in &rows {
                        seen.remove(&r.seq);
                    }
                    return Err(e.into());
                }
            };
            let min = rows.iter().map(|r| r.ts).min().expect("non-empty");
            let max = rows.iter().map(|r| r.ts).max().expect("non-empty");
            self.register_segment(
                &path,
                SegFormat::Jsonl,
                min,
                max,
                rows.len() as i64,
                bytes as i64,
                true,
            )
            .await?;
            report.appended += rows.len() as u64;
        }
        Ok(report)
    }

    /// Upsert an `l0_segments` row. With `accumulate`, `rows` is added to
    /// the existing count (append); otherwise it replaces it (rewrite).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn register_segment(
        &self,
        path: &Path,
        format: SegFormat,
        min: Timestamp,
        max: Timestamp,
        rows: i64,
        bytes: i64,
        accumulate: bool,
    ) -> Result<()> {
        let rows_sql = if accumulate {
            "l0_segments.rows + excluded.rows"
        } else {
            "excluded.rows"
        };
        let sql = format!(
            "INSERT INTO l0_segments (path, format, min_ts, max_ts, rows, bytes, created_at) VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(path) DO UPDATE SET min_ts = MIN(l0_segments.min_ts, excluded.min_ts), \
             max_ts = MAX(l0_segments.max_ts, excluded.max_ts), rows = {rows_sql}, bytes = excluded.bytes"
        );
        crate::db::dynq(sql)
            .bind(self.rel(path))
            .bind(format.ext())
            .bind(ts(min))
            .bind(ts(max))
            .bind(rows)
            .bind(bytes)
            .bind(now_ts())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// All L0 rows of a run (JSONL and Parquet segments), deduplicated by
    /// seq and sorted. Internal pipeline access — L0 is never served to
    /// clients or put into model context.
    pub async fn read_l0_rows(&self, run_id: RunId) -> Result<Vec<L0Row>> {
        let _guard = self.l0.lock().await;
        let mut by_seq: BTreeMap<u64, L0Row> = BTreeMap::new();
        // Parquet first: it is the older, compacted copy; JSONL rows that
        // arrived later for the same day fill in the rest.
        let mut segs = self.scan_segments(Some(run_id))?;
        segs.sort_by_key(|s| (s.format != SegFormat::Parquet, s.date));
        for seg in segs {
            for r in read_segment(&seg)? {
                by_seq.entry(r.seq).or_insert(r);
            }
        }
        Ok(by_seq.into_values().collect())
    }

    /// The run's L0 events, sorted by seq.
    pub async fn read_l0(&self, run_id: RunId) -> Result<Vec<RunEvent>> {
        Ok(self
            .read_l0_rows(run_id)
            .await?
            .iter()
            .filter_map(L0Row::to_event)
            .collect())
    }

    /// Nightly step 1: compact JSONL segments whose newest row is older
    /// than 24 h into Parquet. Returns (segments compacted, parquet files
    /// written, rows compacted).
    #[cfg(feature = "parquet")]
    pub(crate) async fn compact_l0(&self, now: Timestamp) -> Result<(u32, u32, u64)> {
        let _guard = self.l0.lock().await;
        let cutoff = now - chrono::Duration::hours(24);
        let (mut segs, mut files, mut rows_total) = (0u32, 0u32, 0u64);
        for seg in self
            .scan_segments(None)?
            .into_iter()
            .filter(|s| s.format == SegFormat::Jsonl)
        {
            let rows = read_jsonl(&seg.path)?;
            if rows.iter().any(|r| r.ts >= cutoff) {
                continue;
            }
            let pq = seg
                .path
                .with_file_name(format!("{}.{}", seg.run, SegFormat::Parquet.ext()));
            let mut by_seq: BTreeMap<u64, L0Row> = BTreeMap::new();
            if pq.exists() {
                for r in crate::parquet_io::read_rows(&pq)? {
                    by_seq.insert(r.seq, r);
                }
            }
            for r in rows {
                by_seq.entry(r.seq).or_insert(r);
            }
            let merged: Vec<L0Row> = by_seq.into_values().collect();
            if !merged.is_empty() {
                let tmp = pq.with_extension("parquet.tmp");
                crate::parquet_io::write_rows(&tmp, &merged)?;
                std::fs::rename(&tmp, &pq)?;
                let bytes = std::fs::metadata(&pq)?.len() as i64;
                let min = merged.iter().map(|r| r.ts).min().expect("non-empty");
                let max = merged.iter().map(|r| r.ts).max().expect("non-empty");
                self.register_segment(
                    &pq,
                    SegFormat::Parquet,
                    min,
                    max,
                    merged.len() as i64,
                    bytes,
                    false,
                )
                .await?;
                files += 1;
                rows_total += merged.len() as u64;
            }
            std::fs::remove_file(&seg.path)?;
            sqlx::query("DELETE FROM l0_segments WHERE path = ?")
                .bind(self.rel(&seg.path))
                .execute(&self.pool)
                .await?;
            segs += 1;
        }
        Ok((segs, files, rows_total))
    }

    /// Nightly step 2: delete whole days older than `retention_days`.
    /// Returns (segments deleted, bytes freed).
    pub(crate) async fn expire_l0(
        &self,
        now: Timestamp,
        retention_days: u32,
    ) -> Result<(u32, u64)> {
        let _guard = self.l0.lock().await;
        let cutoff = (now - chrono::Duration::days(i64::from(retention_days))).date_naive();
        let (mut n, mut bytes) = (0u32, 0u64);
        let mut dirs = HashSet::new();
        for seg in self
            .scan_segments(None)?
            .into_iter()
            .filter(|s| s.date < cutoff)
        {
            bytes += std::fs::metadata(&seg.path).map(|m| m.len()).unwrap_or(0);
            std::fs::remove_file(&seg.path)?;
            sqlx::query("DELETE FROM l0_segments WHERE path = ?")
                .bind(self.rel(&seg.path))
                .execute(&self.pool)
                .await?;
            if let Some(d) = seg.path.parent() {
                dirs.insert(d.to_path_buf());
            }
            n += 1;
        }
        for d in dirs {
            // Only removes empty directories.
            let _ = std::fs::remove_dir(&d);
        }
        // Rows whose files vanished out-of-band.
        let stale: Vec<String> = sqlx::query_scalar("SELECT path FROM l0_segments")
            .fetch_all(&self.pool)
            .await?;
        for p in stale {
            if !self.data_dir.join(&p).exists() {
                sqlx::query("DELETE FROM l0_segments WHERE path = ?")
                    .bind(&p)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok((n, bytes))
    }

    /// Oldest and newest L0 timestamps in the segment index, if any.
    pub async fn l0_span(&self) -> Result<Option<(Timestamp, Timestamp)>> {
        let row: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT MIN(min_ts), MAX(max_ts) FROM l0_segments")
                .fetch_one(&self.pool)
                .await?;
        match row {
            (Some(a), Some(b)) => Ok(Some((parse_ts(&a)?, parse_ts(&b)?))),
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_reaches_nested_strings() {
        let mut v =
            serde_json::json!({"a": "token sk-123", "b": ["sk-123", {"c": "x sk-123 y"}], "n": 1});
        redact_value(&mut v, &|s: &str| s.replace("sk-123", "[REDACTED]"));
        assert_eq!(
            v,
            serde_json::json!({"a": "token [REDACTED]", "b": ["[REDACTED]", {"c": "x [REDACTED] y"}], "n": 1})
        );
    }
}
