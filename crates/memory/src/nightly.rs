//! Nightly maintenance (PRD §7.9): compact L0, enforce retention, merge
//! duplicates, flag stale items, expire low-salience episodes.

use crate::Memory;
use crate::db::{ItemRecord, audit, fetch_items, link};
use crate::error::Result;
use crate::gateway::EPISODE_PREFIX;
use crate::util::{cosine, normalize, now_ts, ts};
use kiln_types::memory::MemoryStatus;
use kiln_types::{MemoryId, ProjectId, Timestamp};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Retention bounds from the PRD ("retention 60–90 days").
pub const MIN_RETENTION_DAYS: u32 = 60;
pub const MAX_RETENTION_DAYS: u32 = 90;

/// Options for [`Memory::nightly`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NightlyOptions {
    /// L0 retention in days, clamped to 60–90 (default 75). Episodes older
    /// than this with salience below `promote_threshold` expire too.
    pub retention_days: u32,
    /// Active items unused for this many days are flagged stale (default 60).
    pub stale_after_days: u32,
    /// Override "now" (tests, backfills). Defaults to the current time.
    pub now: Option<Timestamp>,
}

impl Default for NightlyOptions {
    fn default() -> Self {
        Self {
            retention_days: 75,
            stale_after_days: 60,
            now: None,
        }
    }
}

/// What a nightly pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NightlyReport {
    /// JSONL segments rewritten as Parquet (0 without feature `parquet`).
    pub segments_compacted: u32,
    pub parquet_files_written: u32,
    pub rows_compacted: u64,
    /// L0 segment files deleted by retention.
    pub segments_expired: u32,
    pub bytes_freed: u64,
    /// Items merged into an equivalent item (status `Superseded`, link `duplicate_of`).
    pub duplicates_merged: u32,
    /// Items flagged for being unused longer than `stale_after_days`.
    pub flagged_unused: u32,
    /// Items flagged for referencing files that no longer exist.
    pub flagged_missing_files: u32,
    /// Low-salience L1 episodes deleted by retention.
    pub episodes_expired: u32,
    /// Effective retention after clamping.
    pub retention_days: u32,
}

/// Plausible relative file paths mentioned in free text
/// (`src/lib.rs`, `crates/memory/Cargo.toml`, `` `qa.yaml` ``).
pub(crate) fn file_refs(text: &str) -> Vec<String> {
    const EXTS: [&str; 30] = [
        "rs", "ts", "tsx", "js", "jsx", "mjs", "py", "go", "java", "kt", "rb", "toml", "yaml",
        "yml", "json", "md", "css", "scss", "html", "sql", "sh", "c", "h", "cpp", "hpp", "swift",
        "vue", "svelte", "proto", "lock",
    ];
    let mut out: Vec<String> = vec![];
    for raw in text.split_whitespace() {
        let t = raw
            .trim_matches(|c: char| "`\"'()[]{}<>,;:!?".contains(c))
            .trim_end_matches('.');
        let t = t.strip_prefix("./").unwrap_or(t);
        if t.len() < 3
            || t.len() > 200
            || t.contains("://")
            || t.starts_with(['-', '/', '~', '$', '@'])
            || t.contains('*')
        {
            continue;
        }
        let Some((stem, ext)) = t.rsplit_once('.') else {
            continue;
        };
        let file_stem = stem.rsplit('/').next().unwrap_or(stem);
        if file_stem.is_empty() || !EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
            continue;
        }
        if !t.chars().all(|c| c.is_alphanumeric() || "/._-".contains(c)) {
            continue;
        }
        if !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

impl Memory {
    /// Run the nightly job. `file_exists(project, path)` tells whether a
    /// repository-relative path still exists in the project's default
    /// branch (the daemon answers it from the bare mirror).
    pub async fn nightly(
        &self,
        opts: &NightlyOptions,
        file_exists: &(dyn Fn(ProjectId, &str) -> bool + Sync),
    ) -> Result<NightlyReport> {
        let now = opts.now.unwrap_or_else(chrono::Utc::now);
        let retention = opts
            .retention_days
            .clamp(MIN_RETENTION_DAYS, MAX_RETENTION_DAYS);
        let mut r = NightlyReport {
            retention_days: retention,
            ..Default::default()
        };

        #[cfg(feature = "parquet")]
        {
            let (s, f, rows) = self.compact_l0(now).await?;
            r.segments_compacted = s;
            r.parquet_files_written = f;
            r.rows_compacted = rows;
        }
        let (n, bytes) = self.expire_l0(now, retention).await?;
        r.segments_expired = n;
        r.bytes_freed = bytes;
        r.duplicates_merged = self.merge_duplicates().await?;
        let (unused, missing) = self
            .flag_stale(now, opts.stale_after_days, file_exists)
            .await?;
        r.flagged_unused = unused;
        r.flagged_missing_files = missing;
        r.episodes_expired = self.expire_episodes(now, retention).await?;
        tracing::info!(report = ?r, "memory nightly finished");
        Ok(r)
    }

    /// Merge active items that say the same thing with the same audience:
    /// same owner, project, kind, visibility, share list, subject and
    /// relation, and the same object or cosine ≥ `dedupe_threshold`.
    /// The keeper is the more salient (then older) item; it absorbs the
    /// loser's use/success/failure counts.
    async fn merge_duplicates(&self) -> Result<u32> {
        let recs = fetch_items(&self.pool, "m.status = 'active'", vec![], true).await?;
        let mut groups: BTreeMap<String, Vec<ItemRecord>> = BTreeMap::new();
        for r in recs {
            let i = &r.item;
            let mut shares: Vec<String> = i.shared_with.iter().map(|u| u.to_string()).collect();
            shares.sort();
            let key = format!(
                "{}|{:?}|{}|{}|{}|{}|{}",
                i.owner_id,
                i.project_id,
                i.kind.as_str(),
                i.visibility.as_str(),
                shares.join(","),
                normalize(&i.subject),
                normalize(&i.relation)
            );
            groups.entry(key).or_default().push(r);
        }
        let mut merged = 0;
        for (_, mut g) in groups {
            if g.len() < 2 {
                continue;
            }
            g.sort_by(|a, b| {
                b.item
                    .salience
                    .total_cmp(&a.item.salience)
                    .then_with(|| a.item.created_at.cmp(&b.item.created_at))
            });
            let mut gone = vec![false; g.len()];
            for i in 0..g.len() {
                if gone[i] {
                    continue;
                }
                for j in i + 1..g.len() {
                    if gone[j] {
                        continue;
                    }
                    let (a, b) = (&g[i], &g[j]);
                    let same_obj = normalize(&a.item.object) == normalize(&b.item.object);
                    let sim = match (&a.embedding, &b.embedding) {
                        (Some(x), Some(y)) => cosine(x, y),
                        _ => 0.0,
                    };
                    if !(same_obj || sim >= self.cfg.dedupe_threshold) {
                        continue;
                    }
                    self.merge_pair(a, b).await?;
                    gone[j] = true;
                    merged += 1;
                }
            }
        }
        Ok(merged)
    }

    async fn merge_pair(&self, keep: &ItemRecord, lose: &ItemRecord) -> Result<()> {
        let (k, l) = (keep.item.id.to_string(), lose.item.id.to_string());
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE memory_items SET use_count = use_count + ?, success_count = success_count + ?, \
             failure_count = failure_count + ?, salience = MAX(salience, ?), updated_at = ? WHERE id = ?",
        )
        .bind(i64::from(lose.item.use_count))
        .bind(i64::from(lose.success_count))
        .bind(i64::from(lose.failure_count))
        .bind(f64::from(lose.item.salience))
        .bind(now_ts())
        .bind(&k)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE memory_items SET status = 'superseded', superseded_by = ?, updated_at = ? WHERE id = ?")
            .bind(&k)
            .bind(now_ts())
            .bind(&l)
            .execute(&mut *tx)
            .await?;
        link(&mut *tx, &l, &k, "duplicate_of").await?;
        audit(
            &mut *tx,
            &l,
            None,
            "merged",
            &format!("duplicate of {k} (nightly)"),
        )
        .await?;
        audit(
            &mut *tx,
            &k,
            None,
            "merged",
            &format!("absorbed {l} (nightly)"),
        )
        .await?;
        tx.commit().await?;
        self.reload_and_sync(keep.item.id).await?;
        self.reload_and_sync(lose.item.id).await?;
        Ok(())
    }

    /// Flag active items unused for `stale_days`, or whose text references
    /// a file that no longer exists in their project.
    async fn flag_stale(
        &self,
        now: Timestamp,
        stale_days: u32,
        file_exists: &(dyn Fn(ProjectId, &str) -> bool + Sync),
    ) -> Result<(u32, u32)> {
        let cutoff = now - chrono::Duration::days(i64::from(stale_days.max(1)));
        let recs = fetch_items(&self.pool, "m.status = 'active'", vec![], false).await?;
        let (mut unused, mut missing) = (0u32, 0u32);
        for r in recs {
            let i = &r.item;
            let last = i.last_used_at.unwrap_or(i.created_at).max(i.created_at);
            let reason = if last < cutoff {
                unused += 1;
                Some(format!("stale: unused since {}", ts(last)))
            } else if let Some(project) = i.project_id {
                let text = format!("{} {} {} {}", i.title, i.subject, i.object, i.body);
                match file_refs(&text)
                    .into_iter()
                    .find(|p| !file_exists(project, p))
                {
                    Some(p) => {
                        missing += 1;
                        Some(format!("stale: references missing file {p}"))
                    }
                    None => None,
                }
            } else {
                None
            };
            if let Some(reason) = reason {
                self.set_flagged(i.id, &reason).await?;
            }
        }
        Ok((unused, missing))
    }

    async fn set_flagged(&self, id: MemoryId, reason: &str) -> Result<()> {
        let sid = id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE memory_items SET status = ?, updated_at = ? WHERE id = ? AND status = 'active'",
        )
        .bind(MemoryStatus::Flagged.as_str())
        .bind(now_ts())
        .bind(&sid)
        .execute(&mut *tx)
        .await?;
        audit(&mut *tx, &sid, None, "flagged", reason).await?;
        tx.commit().await?;
        self.reload_and_sync(id).await?;
        Ok(())
    }

    /// Delete L1 episodes older than the retention window whose salience is
    /// below the promotion threshold ("low scorers stay in L0/L1 and expire").
    async fn expire_episodes(&self, now: Timestamp, retention: u32) -> Result<u32> {
        let cutoff = ts(now - chrono::Duration::days(i64::from(retention)));
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM episodes WHERE created_at < ? AND salience < ?")
                .bind(&cutoff)
                .bind(f64::from(self.cfg.promote_threshold))
                .fetch_all(&self.pool)
                .await?;
        for id in &ids {
            sqlx::query("DELETE FROM episodes WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await?;
            if let Err(e) = self.c.text.delete(&[format!("{EPISODE_PREFIX}{id}")]).await {
                tracing::warn!(error = %e, "episode text index delete failed");
            }
            if let Err(e) = self.c.episode_vectors.remove(id).await {
                tracing::warn!(error = %e, "episode vector delete failed");
            }
        }
        Ok(ids.len() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_file_references() {
        let refs = file_refs(
            "Edit `src/upload/retry.rs` and crates/memory/Cargo.toml, see https://x.io/a.rs or v1.2 or e.g.",
        );
        assert_eq!(
            refs,
            vec!["src/upload/retry.rs", "crates/memory/Cargo.toml"]
        );
        assert_eq!(file_refs("run ./scripts/dev.sh."), vec!["scripts/dev.sh"]);
        assert!(file_refs("version 0.1.0 and foo.bar").is_empty());
    }
}
