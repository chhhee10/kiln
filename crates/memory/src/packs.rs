//! L3 packs: per owner × project × role markdown context, written into
//! worker checkouts as `AGENTS.md` / `CLAUDE.md`.
//!
//! A pack contains the items the **owner** can see for the project plus
//! company-wide (project-less) items, `Active` or the owner's own `Pending`
//! ones, ranked by `salience × recency × role emphasis`, grouped by kind in
//! a role-specific order and capped at `pack_token_budget` (≈1500 tokens).
//!
//! Packs are cached in `memory_packs`, but a cached pack is only served if
//! it is still exact: every item in it is still visible to the owner and no
//! visible item changed since it was built. Otherwise it is rebuilt — so a
//! pack can never leak an item whose visibility was narrowed.

use crate::Memory;
use crate::db::{Arg, bind_all, dynq, status_in, visible_pred};
use crate::error::Result;
use crate::util::{estimate_tokens, now_ts, parse_ts, placeholders, truncate};
use crate::{PACK_KINDS, RETRIEVABLE};
use kiln_types::memory::{MemoryItem, MemoryType};
use kiln_types::{MemoryId, ProjectId, Timestamp, UserId, WorkerRole};
use sqlx::Row;

/// Roles a pack is precomputed for.
pub const PACK_ROLES: [WorkerRole; 4] = [
    WorkerRole::Planner,
    WorkerRole::Builder,
    WorkerRole::Qa,
    WorkerRole::Reviewer,
];
/// Cached packs older than this are rebuilt (recency weights drift).
const MAX_PACK_AGE_HOURS: i64 = 24;
/// Recency half-life-ish constant (days) for `exp(-age/τ)`.
const RECENCY_TAU_DAYS: f32 = 90.0;

/// Kind order (= emphasis) per role.
fn kind_order(role: WorkerRole) -> [MemoryType; 5] {
    use MemoryType::*;
    match role {
        WorkerRole::Builder => [Convention, Procedure, Decision, Preference, Fact],
        WorkerRole::Qa => [Procedure, Convention, Fact, Decision, Preference],
        WorkerRole::Reviewer => [Convention, Decision, Procedure, Fact, Preference],
        WorkerRole::Planner => [Decision, Fact, Convention, Procedure, Preference],
        WorkerRole::Distiller => [Fact, Convention, Procedure, Decision, Preference],
    }
}

fn kind_heading(k: MemoryType) -> &'static str {
    match k {
        MemoryType::Convention => "Conventions",
        MemoryType::Procedure => "Procedures",
        MemoryType::Decision => "Decisions",
        MemoryType::Fact => "Facts",
        MemoryType::Preference => "Your preferences",
        MemoryType::Episode => "Past runs",
    }
}

/// `salience × recency × emphasis`.
fn pack_score(item: &MemoryItem, role: WorkerRole, now: Timestamp) -> f32 {
    let last = item
        .last_used_at
        .map_or(item.updated_at, |u| u.max(item.updated_at));
    let age_days = ((now - last).num_seconds().max(0) as f32) / 86_400.0;
    let recency = (-age_days / RECENCY_TAU_DAYS).exp();
    let pos = kind_order(role)
        .iter()
        .position(|k| *k == item.kind)
        .unwrap_or(4);
    let mut emphasis = [1.5, 1.3, 1.1, 1.0, 1.0][pos];
    if role == WorkerRole::Qa {
        let t = format!("{} {}", item.relation, item.title).to_lowercase();
        if t.contains("test") {
            emphasis *= 1.3;
        }
    }
    item.salience.max(0.05) * recency * emphasis
}

fn render_line(item: &MemoryItem) -> String {
    let fact = format!(
        "{} {} {}",
        item.subject,
        item.relation.replace('_', " "),
        item.object
    );
    let mut line = format!("- **{}** — {}", item.title.trim(), fact.trim());
    if !item.body.trim().is_empty() {
        let body = item.body.split_whitespace().collect::<Vec<_>>().join(" ");
        line.push_str(&format!(". {}", truncate(&body, 240)));
    }
    line
}

impl Memory {
    /// Build the pack text and its item ids (no caching).
    async fn build_pack(
        &self,
        owner: UserId,
        project: ProjectId,
        role: WorkerRole,
    ) -> Result<(String, Vec<MemoryId>)> {
        let items = self
            .visible_in_scope(owner, Some(project), &RETRIEVABLE, &PACK_KINDS)
            .await?;
        let now = chrono::Utc::now();
        let mut ranked: Vec<(f32, MemoryItem)> = items
            .into_iter()
            .map(|i| (pack_score(&i, role, now), i))
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));

        let name = self.project_label(Some(project)).await?;
        let header = format!(
            "# Company brain · {name} · {}\n\n_Assembled by Kiln from memory you can access. Prefer these \
             conventions unless the task says otherwise._\n",
            role.label()
        );
        let cpt = self.cfg.chars_per_token;
        let mut used = estimate_tokens(&header, cpt);
        let budget = self.cfg.pack_token_budget as usize;
        let mut chosen: Vec<(MemoryItem, String)> = vec![];
        for (_, item) in ranked {
            let line = render_line(&item);
            // Each new kind section also costs its heading.
            let heading_cost = if chosen.iter().any(|(c, _)| c.kind == item.kind) {
                0
            } else {
                estimate_tokens(kind_heading(item.kind), cpt) + 2
            };
            let cost = estimate_tokens(&line, cpt) + heading_cost;
            if used + cost > budget {
                continue;
            }
            used += cost;
            chosen.push((item, line));
        }
        let mut out = header;
        let mut ids = vec![];
        for kind in kind_order(role) {
            let lines: Vec<&(MemoryItem, String)> =
                chosen.iter().filter(|(i, _)| i.kind == kind).collect();
            if lines.is_empty() {
                continue;
            }
            out.push_str(&format!("\n## {}\n", kind_heading(kind)));
            for (item, line) in lines {
                out.push_str(line);
                out.push('\n');
                ids.push(item.id);
            }
        }
        if ids.is_empty() {
            out.push_str("\n_No memories yet for this project._\n");
        }
        Ok((out, ids))
    }

    async fn store_pack(
        &self,
        owner: UserId,
        project: ProjectId,
        role: WorkerRole,
        content: &str,
        ids: &[MemoryId],
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO memory_packs (owner_id, project_id, role, content, item_ids, built_at) VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(owner_id, project_id, role) DO UPDATE SET content = excluded.content, \
             item_ids = excluded.item_ids, built_at = excluded.built_at",
        )
        .bind(owner.to_string())
        .bind(project.to_string())
        .bind(role.as_str())
        .bind(content)
        .bind(serde_json::to_string(ids)?)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Recompute and store every role's pack for `owner` × `project`.
    pub async fn rebuild_packs(&self, owner: UserId, project: ProjectId) -> Result<()> {
        for role in PACK_ROLES {
            let (content, ids) = self.build_pack(owner, project, role).await?;
            self.store_pack(owner, project, role, &content, &ids)
                .await?;
        }
        Ok(())
    }

    /// Is the cached pack still exact for the owner? (see module docs)
    async fn cached_pack(
        &self,
        owner: UserId,
        project: ProjectId,
        role: WorkerRole,
    ) -> Result<Option<(String, Vec<MemoryId>)>> {
        let row = sqlx::query("SELECT content, item_ids, built_at FROM memory_packs WHERE owner_id = ? AND project_id = ? AND role = ?")
            .bind(owner.to_string())
            .bind(project.to_string())
            .bind(role.as_str())
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else { return Ok(None) };
        let built_at: String = row.try_get("built_at")?;
        if parse_ts(&built_at)? < chrono::Utc::now() - chrono::Duration::hours(MAX_PACK_AGE_HOURS) {
            return Ok(None);
        }
        let ids: Vec<MemoryId> =
            serde_json::from_str(&row.try_get::<String, _>("item_ids")?).unwrap_or_default();
        let kinds = PACK_KINDS
            .iter()
            .map(|k| format!("'{}'", k.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        let scope = format!(
            "{} AND {} AND m.type IN ({kinds}) AND (m.project_id IS NULL OR m.project_id = ?)",
            visible_pred("m", owner),
            status_in("m.status", &RETRIEVABLE)
        );
        // 1. nothing visible changed since the build
        let newest: Option<String> = bind_all(
            dynq(format!(
                "SELECT MAX(m.updated_at) AS newest FROM memory_items m WHERE {scope}"
            )),
            vec![Arg::Text(project.to_string())],
        )
        .fetch_one(&self.pool)
        .await?
        .try_get("newest")?;
        if newest.is_some_and(|n| n > built_at) {
            return Ok(None);
        }
        // 2. every packed item is still visible + retrievable
        if !ids.is_empty() {
            let mut args: Vec<Arg> = ids.iter().map(|i| Arg::Text(i.to_string())).collect();
            args.push(Arg::Text(project.to_string()));
            let n: i64 = bind_all(
                dynq(format!(
                    "SELECT COUNT(*) AS n FROM memory_items m WHERE m.id IN ({}) AND {scope}",
                    placeholders(ids.len())
                )),
                args,
            )
            .fetch_one(&self.pool)
            .await?
            .try_get("n")?;
            if n as usize != ids.len() {
                return Ok(None);
            }
        }
        Ok(Some((row.try_get("content")?, ids)))
    }

    /// The L3 pack (markdown) for `owner` × `project` × `role`, served from
    /// the cache when still exact, rebuilt otherwise.
    pub async fn pack(
        &self,
        owner: UserId,
        project: ProjectId,
        role: WorkerRole,
    ) -> Result<String> {
        Ok(self.pack_with_items(owner, project, role).await?.0)
    }

    /// The pack and the ids of the items in it — record them with
    /// [`Memory::record_use`] under the run that receives the pack so
    /// [`Memory::feedback`] can credit them.
    pub async fn pack_with_items(
        &self,
        owner: UserId,
        project: ProjectId,
        role: WorkerRole,
    ) -> Result<(String, Vec<MemoryId>)> {
        if let Some(hit) = self.cached_pack(owner, project, role).await? {
            return Ok(hit);
        }
        let (content, ids) = self.build_pack(owner, project, role).await?;
        self.store_pack(owner, project, role, &content, &ids)
            .await?;
        Ok((content, ids))
    }
}
