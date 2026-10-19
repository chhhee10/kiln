//! The memory gateway: the only access path to L2 (and L1 via search).
//!
//! Every read takes the requesting user and is filtered **in SQL** with
//! [`visible_pred`] (the SQL form of `MemoryItem::visible_to`) and then
//! re-checked in Rust with `visible_to` itself. Every mutation writes
//! `memory_audit` in the same transaction and re-syncs the indexes.

use crate::db::{
    self, Arg, ItemRecord, audit, bind_all, dynq, episode_visible_pred, fetch_episodes,
    fetch_items, link, status_in, visible_pred,
};
use crate::error::{MemoryError, Result};
use crate::traits::{IndexMeta, SearchFilter, TextDoc};
use crate::util::{
    cosine, estimate_tokens, normalize, now_ts, parse_id, parse_ts, placeholders, truncate,
    vec_to_blob, words,
};
use crate::{LISTABLE, Memory, RETRIEVABLE};
use kiln_types::api::{
    ApproveMemoryRequest, MemoryDecision, MemoryDetail, MemoryListResponse, UpdateMemoryRequest,
};
use kiln_types::memory::{
    Episode, MemoryHit, MemoryItem, MemoryProposal, MemoryQuery, MemoryStatus, MemoryType,
    MemoryUse, Visibility,
};
use kiln_types::{
    MemoryId, ProjectId, RunId, TaskId, UserId, UserSummary, WorkerRole, initials_for,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::{BTreeSet, HashMap};

/// Reciprocal-rank-fusion constant (Cormack et al.).
const RRF_K: f32 = 60.0;
/// Field length caps applied to proposals and edits.
const MAX_TITLE: usize = 200;
const MAX_TRIPLE: usize = 500;
const MAX_BODY: usize = 4000;
/// Relations that naturally hold several objects at once ("FP uses tokio"
/// and "FP uses serde" are both true). A different object under one of
/// these is additional knowledge, never a contradiction.
pub const MULTI_VALUED_RELATIONS: [&str; 14] = [
    "uses",
    "has",
    "have",
    "includes",
    "contains",
    "depends_on",
    "requires",
    "supports",
    "related_to",
    "relates_to",
    "mentions",
    "owns",
    "fixed_by",
    "see_also",
];

fn multi_valued(relation_normalized: &str) -> bool {
    let r = relation_normalized.replace(' ', "_");
    MULTI_VALUED_RELATIONS.contains(&r.as_str())
}

/// Prefix for episode ids in the shared text index.
pub(crate) const EPISODE_PREFIX: &str = "ep:";

/// Where a retrieval happened, for the use log (`memory_uses`) that drives
/// feedback and "Recently used by".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UseContext {
    pub run: Option<RunId>,
    pub task: Option<TaskId>,
    pub role: Option<WorkerRole>,
}

/// What [`Memory::propose_detailed`] did with a proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposeAction {
    /// A new item was inserted.
    Created,
    /// The proposal duplicated an existing visible item, which was bumped
    /// instead (`item` is that existing item).
    Merged,
}

/// Result of a proposal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProposeOutcome {
    pub item: MemoryItem,
    pub action: ProposeAction,
    /// Older items of the same owner this one superseded (contradictions).
    /// For a pending item the supersede is deferred until approval.
    pub superseded: Vec<MemoryId>,
}

/// Result of [`Memory::feedback`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FeedbackReport {
    pub boosted: u32,
    pub penalized: u32,
    pub flagged: u32,
}

/// Extra knobs for internal proposals (extractor).
#[derive(Clone, Debug, Default)]
pub(crate) struct ProposeOpts {
    pub salience: Option<f32>,
    pub source_label: Option<String>,
}

// ------------------------------------------------------------ index helpers

pub(crate) fn item_meta(item: &MemoryItem) -> IndexMeta {
    IndexMeta {
        owner: item.owner_id,
        kind: item.kind,
        visibility: item.visibility,
        shared_with: item.shared_with.clone(),
        project: item.project_id,
        status: item.status,
    }
}

pub(crate) fn item_doc(item: &MemoryItem) -> TextDoc {
    TextDoc {
        id: item.id.to_string(),
        title: item.title.clone(),
        body: item.body.clone(),
        subject: item.subject.clone(),
        relation: item.relation.clone(),
        object: item.object.clone(),
        meta: item_meta(item),
    }
}

/// Text that is embedded for an item.
pub(crate) fn item_embed_text(item: &MemoryItem) -> String {
    format!(
        "{}\n{} {} {}\n{}",
        item.title,
        item.subject,
        item.relation.replace('_', " "),
        item.object,
        item.body
    )
}

/// Compact text of an item for reranking and token budgeting.
pub(crate) fn item_render(item: &MemoryItem) -> String {
    let mut s = format!(
        "{}. {} {} {}.",
        item.title,
        item.subject,
        item.relation.replace('_', " "),
        item.object
    );
    if !item.body.is_empty() {
        s.push(' ');
        s.push_str(&item.body);
    }
    s
}

fn vis_rank(v: Visibility) -> u8 {
    match v {
        Visibility::Private => 0,
        Visibility::Shared => 1,
        Visibility::Company => 2,
    }
}

/// Converts an episode into the `MemoryItem` shape used by search hits
/// (`kind = Episode`, `id` = the episode's uuid).
pub(crate) fn episode_as_item(ep: &Episode, salience: f32) -> MemoryItem {
    let mut body = String::new();
    if !ep.errors.is_empty() {
        body.push_str(&format!("Errors: {}. ", ep.errors.join("; ")));
    }
    if !ep.fixes.is_empty() {
        body.push_str(&format!("Fixes: {}. ", ep.fixes.join("; ")));
    }
    if !ep.files.is_empty() {
        body.push_str(&format!("Files: {}.", ep.files.join(", ")));
    }
    MemoryItem {
        id: MemoryId(ep.id.0),
        owner_id: ep.owner_id,
        kind: MemoryType::Episode,
        project_id: ep.project_id,
        visibility: ep.visibility,
        shared_with: vec![],
        requested_visibility: None,
        title: truncate(&format!("{} run: {}", ep.role.label(), ep.goal), MAX_TITLE),
        subject: truncate(&ep.goal, MAX_TRIPLE),
        relation: "outcome".into(),
        object: truncate(&ep.outcome, MAX_TRIPLE),
        body: body.trim().to_string(),
        status: MemoryStatus::Active,
        salience,
        source_run_id: Some(ep.run_id),
        source_label: Some(ep.role.label().to_string()),
        use_count: 0,
        last_used_at: None,
        created_at: ep.created_at,
        updated_at: ep.created_at,
    }
}

impl Memory {
    /// Push the current state of an item to the text and vector indexes.
    /// SQLite is the source of truth, so failures are logged, not returned
    /// (`reindex` repairs them; reads never trust the index anyway).
    pub(crate) async fn sync_item(&self, item: &MemoryItem, embedding: Option<&[f32]>) {
        if let Err(e) = self.c.text.upsert(&[item_doc(item)]).await {
            tracing::warn!(item = %item.id, error = %e, "text index update failed");
        }
        let owned;
        let emb = match embedding {
            Some(e) => Some(e),
            None => {
                owned = db::fetch_item_unchecked(&self.pool, item.id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|r| r.embedding);
                owned.as_deref()
            }
        };
        if let Some(v) = emb
            && let Err(e) = self
                .c
                .item_vectors
                .upsert(&item.id.to_string(), v, &item_meta(item))
                .await
        {
            tracing::warn!(item = %item.id, error = %e, "vector index update failed");
        }
    }

    /// Reload an item (no visibility check) and sync it to the indexes.
    pub(crate) async fn reload_and_sync(&self, id: MemoryId) -> Result<MemoryItem> {
        let rec = db::fetch_item_unchecked(&self.pool, id)
            .await?
            .ok_or(MemoryError::NotFound)?;
        self.sync_item(&rec.item, rec.embedding.as_deref()).await;
        Ok(rec.item)
    }

    /// Load an item if (and only if) `user` may see it.
    pub(crate) async fn load_visible(&self, user: UserId, id: MemoryId) -> Result<ItemRecord> {
        let w = format!("m.id = ? AND {}", visible_pred("m", user));
        let rec = fetch_items(&self.pool, &w, vec![Arg::Text(id.to_string())], true)
            .await?
            .pop()
            .ok_or(MemoryError::NotFound)?;
        if !rec.item.visible_to(user) {
            tracing::error!(item = %id, "SQL visibility predicate disagreed with visible_to; denying");
            return Err(MemoryError::NotFound);
        }
        Ok(rec)
    }

    /// Load an item the user owns: `NotFound` if invisible, `Forbidden` if
    /// visible but owned by someone else.
    async fn load_owned(&self, user: UserId, id: MemoryId) -> Result<ItemRecord> {
        let rec = self.load_visible(user, id).await?;
        if rec.item.owner_id != user {
            return Err(MemoryError::Forbidden(
                "only the item owner can do this".into(),
            ));
        }
        Ok(rec)
    }

    // ------------------------------------------------------------ search

    /// Hybrid retrieval for `user` (records uses without a run context).
    /// See [`Memory::search_ctx`].
    pub async fn search(&self, user: UserId, q: &MemoryQuery) -> Result<Vec<MemoryHit>> {
        self.search_ctx(user, q, &UseContext::default()).await
    }

    /// Hybrid retrieval: Tantivy keyword top-k ∪ vector top-k fused by
    /// reciprocal rank → **hard SQL visibility/project/status filter** →
    /// rerank → limit and token budget → record uses under `ctx`.
    ///
    /// Final score = `0.55·rerank + 0.35·rrf/max_rrf + 0.10·salience`.
    /// An empty query returns the most salient visible items.
    /// Only `Active` and (the owner's own) `Pending` items are returned;
    /// with `pending_only`, only the viewer's pending items.
    pub async fn search_ctx(
        &self,
        user: UserId,
        q: &MemoryQuery,
        ctx: &UseContext,
    ) -> Result<Vec<MemoryHit>> {
        let limit = q
            .limit
            .unwrap_or(self.cfg.default_search_limit)
            .clamp(1, self.cfg.max_search_limit) as usize;
        let k = (limit * 4).max(20);
        let statuses: Vec<MemoryStatus> = if q.pending_only {
            vec![MemoryStatus::Pending]
        } else {
            RETRIEVABLE.to_vec()
        };
        let want_items = q.kind != Some(MemoryType::Episode);
        let want_episodes = !q.pending_only
            && q.visibility.is_none()
            && match q.kind {
                None => self.cfg.search_episodes,
                Some(k) => k == MemoryType::Episode,
            };
        let filter = SearchFilter {
            viewer: user,
            project: q.project_id,
            statuses: statuses.clone(),
            kinds: q.kind.map(|k| vec![k]),
        };
        let query = q.q.trim();

        // 1. candidates
        let mut rrf: HashMap<String, f32> = HashMap::new();
        if !query.is_empty() {
            let mut lists: Vec<Vec<String>> = vec![];
            match self.c.text.search(query, k * 2, &filter).await {
                Ok(hits) => lists.push(hits.into_iter().map(|(id, _)| id).collect()),
                Err(e) => tracing::warn!(error = %e, "text search failed; continuing with vectors"),
            }
            let qv = self.embed_one(query).await?;
            if want_items {
                match self.c.item_vectors.search(&qv, k, &filter).await {
                    Ok(hits) => lists.push(
                        hits.into_iter()
                            .filter(|(_, s)| *s >= self.cfg.min_vector_score)
                            .map(|(id, _)| id)
                            .collect(),
                    ),
                    Err(e) => tracing::warn!(error = %e, "vector search failed"),
                }
            }
            if want_episodes {
                match self.c.episode_vectors.search(&qv, k, &filter).await {
                    Ok(hits) => lists.push(
                        hits.into_iter()
                            .filter(|(_, s)| *s >= self.cfg.min_vector_score)
                            .map(|(id, _)| format!("{EPISODE_PREFIX}{id}"))
                            .collect(),
                    ),
                    Err(e) => tracing::warn!(error = %e, "episode vector search failed"),
                }
            }
            for list in lists {
                for (rank, id) in list.into_iter().enumerate() {
                    *rrf.entry(id).or_default() += 1.0 / (RRF_K + rank as f32 + 1.0);
                }
            }
        }

        // 2. hard filter in SQL (never trust the indexes)
        let mut where_ = vec![visible_pred("m", user), status_in("m.status", &statuses)];
        let mut args: Vec<Arg> = vec![];
        if let Some(kind) = q.kind {
            where_.push("m.type = ?".into());
            args.push(Arg::Text(kind.as_str().into()));
        }
        if let Some(p) = q.project_id {
            where_.push("(m.project_id IS NULL OR m.project_id = ?)".into());
            args.push(Arg::Text(p.to_string()));
        }
        if let Some(v) = q.visibility {
            where_.push("m.visibility = ?".into());
            args.push(Arg::Text(v.as_str().into()));
        }
        if q.pending_only {
            where_.push(format!("m.owner_id = '{user}'"));
        }
        let mut items: Vec<(MemoryItem, f32)> = vec![];
        if want_items {
            if query.is_empty() {
                let w = format!(
                    "{} ORDER BY m.salience DESC, m.updated_at DESC LIMIT {}",
                    where_.join(" AND "),
                    k
                );
                for r in fetch_items(&self.pool, &w, args.clone(), false).await? {
                    items.push((r.item, 0.0));
                }
            } else {
                let ids: Vec<String> = rrf
                    .keys()
                    .filter(|id| !id.starts_with(EPISODE_PREFIX))
                    .cloned()
                    .collect();
                for chunk in ids.chunks(400) {
                    let w = format!(
                        "m.id IN ({}) AND {}",
                        placeholders(chunk.len()),
                        where_.join(" AND ")
                    );
                    let mut a: Vec<Arg> = chunk.iter().map(|s| Arg::Text(s.clone())).collect();
                    a.extend(args.clone());
                    for r in fetch_items(&self.pool, &w, a, false).await? {
                        let s = rrf.get(&r.item.id.to_string()).copied().unwrap_or(0.0);
                        items.push((r.item, s));
                    }
                }
            }
        }
        if want_episodes && !query.is_empty() {
            let ids: Vec<String> = rrf
                .keys()
                .filter_map(|id| id.strip_prefix(EPISODE_PREFIX))
                .map(str::to_string)
                .collect();
            for chunk in ids.chunks(400) {
                let mut w = format!(
                    "e.id IN ({}) AND {} AND e.status = 'active'",
                    placeholders(chunk.len()),
                    episode_visible_pred("e", user)
                );
                let mut a: Vec<Arg> = chunk.iter().map(|s| Arg::Text(s.clone())).collect();
                if let Some(p) = q.project_id {
                    w.push_str(" AND (e.project_id IS NULL OR e.project_id = ?)");
                    a.push(Arg::Text(p.to_string()));
                }
                for (ep, sal) in fetch_episodes(&self.pool, &w, a).await? {
                    let s = rrf
                        .get(&format!("{EPISODE_PREFIX}{}", ep.id))
                        .copied()
                        .unwrap_or(0.0);
                    items.push((episode_as_item(&ep, sal), s));
                }
            }
        }
        // 3. defense in depth: the Rust rule itself
        items.retain(|(item, _)| {
            let ok = item.visible_to(user);
            if !ok {
                tracing::error!(item = %item.id, "visibility mismatch between SQL and visible_to; dropped");
            }
            ok
        });

        // 4. rerank + score
        let mut hits: Vec<MemoryHit> = if query.is_empty() {
            items
                .into_iter()
                .map(|(item, _)| MemoryHit {
                    score: item.salience,
                    item,
                })
                .collect()
        } else {
            let texts: Vec<String> = items.iter().map(|(i, _)| item_render(i)).collect();
            let rr = match self.c.reranker.rerank(query, &texts).await {
                Ok(s) if s.len() == texts.len() => s,
                Ok(_) | Err(_) => {
                    tracing::warn!("reranker failed; using fusion scores only");
                    vec![0.0; texts.len()]
                }
            };
            let max_rrf = items
                .iter()
                .map(|(_, s)| *s)
                .fold(0.0f32, f32::max)
                .max(f32::EPSILON);
            items
                .into_iter()
                .zip(rr)
                .map(|((item, fused), r)| {
                    let score = 0.55 * r + 0.35 * (fused / max_rrf) + 0.10 * item.salience;
                    MemoryHit { item, score }
                })
                .collect()
        };
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| b.item.updated_at.cmp(&a.item.updated_at))
        });

        // 5. limit + token budget
        let mut out = Vec::with_capacity(limit);
        let mut used = 0usize;
        for h in hits {
            if out.len() >= limit {
                break;
            }
            if let Some(budget) = q.token_budget {
                let t = estimate_tokens(&item_render(&h.item), self.cfg.chars_per_token);
                if used + t > budget as usize {
                    continue;
                }
                used += t;
            }
            out.push(h);
        }

        // 6. record uses (L2 items only; episodes are not in memory_items)
        let used_ids: Vec<MemoryId> = out
            .iter()
            .filter(|h| h.item.kind != MemoryType::Episode)
            .map(|h| h.item.id)
            .collect();
        if !used_ids.is_empty()
            && let Err(e) = self.record_use(user, &used_ids, ctx).await
        {
            tracing::warn!(error = %e, "recording memory uses failed");
        }
        Ok(out)
    }

    // ------------------------------------------------------------ list / get

    /// The browse view: items visible to `user` with filters
    /// (`visibility` = Private / Shared / Company, `pending_only` = the
    /// approval queue of items **owned by** `user`), optional free-text
    /// filter (every word must appear in title/subject/relation/object/body)
    /// and counts. Statuses shown: see [`LISTABLE`]. Newest first.
    pub async fn list(&self, user: UserId, q: &MemoryQuery) -> Result<MemoryListResponse> {
        let mut where_ = vec![visible_pred("m", user)];
        let mut args: Vec<Arg> = vec![];
        if q.pending_only {
            where_.push(format!("m.owner_id = '{user}' AND m.status = 'pending'"));
        } else {
            where_.push(status_in("m.status", &LISTABLE));
        }
        if let Some(v) = q.visibility {
            where_.push("m.visibility = ?".into());
            args.push(Arg::Text(v.as_str().into()));
        }
        if let Some(kind) = q.kind {
            where_.push("m.type = ?".into());
            args.push(Arg::Text(kind.as_str().into()));
        }
        if let Some(p) = q.project_id {
            where_.push("(m.project_id IS NULL OR m.project_id = ?)".into());
            args.push(Arg::Text(p.to_string()));
        }
        for w in words(&q.q).into_iter().take(8) {
            where_.push(
                "(lower(m.title) LIKE ? OR lower(m.subject) LIKE ? OR lower(m.relation) LIKE ? \
                 OR lower(m.object) LIKE ? OR lower(m.body) LIKE ?)"
                    .into(),
            );
            let pat = format!("%{w}%");
            for _ in 0..5 {
                args.push(Arg::Text(pat.clone()));
            }
        }
        let where_sql = where_.join(" AND ");
        let total: i64 = bind_all(
            dynq(format!(
                "SELECT COUNT(*) AS n FROM memory_items m WHERE {where_sql}"
            )),
            args.clone(),
        )
        .fetch_one(&self.pool)
        .await?
        .try_get("n")?;
        let limit = q.limit.unwrap_or(200).clamp(1, 1000);
        let mut recs = fetch_items(
            &self.pool,
            &format!("{where_sql} ORDER BY m.updated_at DESC, m.id DESC LIMIT {limit}"),
            args,
            false,
        )
        .await?;
        recs.retain(|r| r.item.visible_to(user));
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_items WHERE owner_id = ? AND status = 'pending'",
        )
        .bind(user.to_string())
        .fetch_one(&self.pool)
        .await?;
        let week_ago = crate::util::ts(chrono::Utc::now() - chrono::Duration::days(7));
        let runs: i64 = dynq(format!(
            "SELECT COUNT(DISTINCT u.run_id) AS n FROM memory_uses u JOIN memory_items m ON m.id = u.item_id \
             WHERE u.run_id IS NOT NULL AND u.at >= ? AND {}",
            visible_pred("m", user)
        ))
        .bind(week_ago)
        .fetch_one(&self.pool)
        .await?
        .try_get("n")?;
        Ok(MemoryListResponse {
            items: recs.into_iter().map(|r| r.item).collect(),
            total: total as u32,
            pending_approvals: pending as u32,
            runs_this_week: runs as u32,
        })
    }

    /// One item with owner, share list and recent uses. `NotFound` unless
    /// visible to `user` (any status). Episode ids (from search hits) are
    /// resolved too. Non-owners only see their own entries in
    /// `recently_used`.
    pub async fn get(&self, user: UserId, id: MemoryId) -> Result<MemoryDetail> {
        let item = match self.load_visible(user, id).await {
            Ok(rec) => rec.item,
            Err(MemoryError::NotFound) => {
                let w = format!("e.id = ? AND {}", episode_visible_pred("e", user));
                let (ep, sal) = fetch_episodes(&self.pool, &w, vec![Arg::Text(id.to_string())])
                    .await?
                    .pop()
                    .ok_or(MemoryError::NotFound)?;
                let item = episode_as_item(&ep, sal);
                if !item.visible_to(user) {
                    return Err(MemoryError::NotFound);
                }
                item
            }
            Err(e) => return Err(e),
        };
        let mut sql = "SELECT u.role, u.at, u.run_id, t.key AS task_key FROM memory_uses u \
                       LEFT JOIN tasks t ON t.id = u.task_id WHERE u.item_id = ? AND u.role IS NOT NULL"
            .to_string();
        if item.owner_id != user {
            sql.push_str(&format!(" AND u.user_id = '{user}'"));
        }
        sql.push_str(" ORDER BY u.id DESC LIMIT 10");
        let mut recently_used = vec![];
        for row in dynq(sql).bind(id.to_string()).fetch_all(&self.pool).await? {
            let role: String = row.try_get("role")?;
            let Some(role) = WorkerRole::parse(&role) else {
                continue;
            };
            let task_key: Option<String> = row.try_get("task_key")?;
            let run: Option<String> = row.try_get("run_id")?;
            let task_key = task_key
                .or_else(|| run.map(|r| format!("run {}", &r[r.len().saturating_sub(6)..])))
                .unwrap_or_else(|| "—".into());
            recently_used.push(MemoryUse {
                task_key,
                role,
                at: parse_ts(&row.try_get::<String, _>("at")?)?,
            });
        }
        let mut ids = vec![item.owner_id];
        ids.extend(item.shared_with.iter().copied());
        let users = self.user_summaries(&ids).await?;
        let owner = users
            .get(&item.owner_id)
            .cloned()
            .unwrap_or_else(|| UserSummary {
                id: item.owner_id,
                name: "Unknown".into(),
                email: String::new(),
                initials: "?".into(),
            });
        let shared_with = item
            .shared_with
            .iter()
            .filter_map(|u| users.get(u).cloned())
            .collect();
        Ok(MemoryDetail {
            item,
            recently_used,
            owner,
            shared_with,
        })
    }

    pub(crate) async fn user_summaries(
        &self,
        ids: &[UserId],
    ) -> Result<HashMap<UserId, UserSummary>> {
        let mut out = HashMap::new();
        if ids.is_empty() {
            return Ok(out);
        }
        let sql = format!(
            "SELECT id, name, email FROM users WHERE id IN ({})",
            placeholders(ids.len())
        );
        let mut q = dynq(sql);
        for id in ids {
            q = q.bind(id.to_string());
        }
        for row in q.fetch_all(&self.pool).await? {
            let id: UserId = parse_id(&row.try_get::<String, _>("id")?)?;
            let name: String = row.try_get("name")?;
            out.insert(
                id,
                UserSummary {
                    id,
                    initials: initials_for(&name),
                    name,
                    email: row.try_get("email")?,
                },
            );
        }
        Ok(out)
    }

    async fn existing_users(&self, ids: &[UserId]) -> Result<BTreeSet<UserId>> {
        Ok(self.user_summaries(ids).await?.into_keys().collect())
    }

    // ------------------------------------------------------------ propose

    /// Propose an item as `owner` (API, MCP `propose_memory`, extractor).
    /// Returns the created item, or the existing item it was merged into.
    /// See [`Memory::propose_detailed`] for the rules.
    pub async fn propose(
        &self,
        owner: UserId,
        source_run: Option<RunId>,
        p: MemoryProposal,
    ) -> Result<MemoryItem> {
        Ok(self.propose_detailed(owner, source_run, p).await?.item)
    }

    /// Propose with full outcome. Rules (PRD §4.5, §5.4):
    /// * `Preference` ⇒ forced `Private`.
    /// * `Private` ⇒ `Active` immediately.
    /// * `Shared`/`Company` ⇒ `Pending` with `requested_visibility`; the
    ///   stored visibility stays `Private` until the owner approves.
    /// * **Dedupe:** an existing item visible to the owner with the same
    ///   subject+relation and (same object or cosine ≥ `dedupe_threshold`)
    ///   is bumped (salience +0.05, use +1, links `source_run` and
    ///   `duplicate_of`) instead of inserting.
    /// * **Contradiction:** same subject+relation, different object, cosine ≥
    ///   `contradiction_threshold`, for a single-valued relation (not in
    ///   [`MULTI_VALUED_RELATIONS`]) ⇒ the owner's older item is superseded
    ///   (status `Superseded`, link `supersedes`) once the new one is active
    ///   and at least as visible. Other users' items are only linked `related`.
    pub async fn propose_detailed(
        &self,
        owner: UserId,
        source_run: Option<RunId>,
        p: MemoryProposal,
    ) -> Result<ProposeOutcome> {
        self.propose_inner(owner, source_run, p, ProposeOpts::default())
            .await
    }

    pub(crate) async fn propose_inner(
        &self,
        owner: UserId,
        source_run: Option<RunId>,
        mut p: MemoryProposal,
        opts: ProposeOpts,
    ) -> Result<ProposeOutcome> {
        // ---- validate
        if p.kind == MemoryType::Episode {
            return Err(MemoryError::Invalid(
                "episodes are produced by distillation, not proposed".into(),
            ));
        }
        p.subject = truncate(p.subject.trim(), MAX_TRIPLE);
        p.relation = truncate(p.relation.trim(), MAX_TRIPLE);
        p.object = truncate(p.object.trim(), MAX_TRIPLE);
        p.body = truncate(p.body.trim(), MAX_BODY);
        if p.subject.is_empty() || p.object.is_empty() {
            return Err(MemoryError::Invalid(
                "subject and object are required".into(),
            ));
        }
        if p.relation.is_empty() {
            p.relation = "is".into();
        }
        p.title = truncate(p.title.trim(), MAX_TITLE);
        if p.title.is_empty() {
            p.title = truncate(
                &format!(
                    "{} {} {}",
                    p.subject,
                    p.relation.replace('_', " "),
                    p.object
                ),
                MAX_TITLE,
            );
        }
        if self.existing_users(&[owner]).await?.is_empty() {
            return Err(MemoryError::Invalid("unknown owner".into()));
        }
        if let Some(run) = source_run {
            let run_owner: Option<String> =
                sqlx::query_scalar("SELECT owner_id FROM runs WHERE id = ?")
                    .bind(run.to_string())
                    .fetch_optional(&self.pool)
                    .await?;
            match run_owner {
                None => return Err(MemoryError::Invalid("unknown source run".into())),
                Some(o) if o != owner.to_string() => {
                    return Err(MemoryError::Forbidden(
                        "source run belongs to another user".into(),
                    ));
                }
                Some(_) => {}
            }
        }
        if let Some(project) = p.project_id {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE id = ?")
                .bind(project.to_string())
                .fetch_one(&self.pool)
                .await?;
            if n == 0 {
                return Err(MemoryError::Invalid("unknown project".into()));
            }
        }
        if p.kind == MemoryType::Preference {
            p.visibility = Visibility::Private;
        }
        let mut shares: Vec<UserId> = p
            .shared_with
            .iter()
            .copied()
            .filter(|u| *u != owner)
            .collect();
        shares.sort();
        shares.dedup();
        if p.visibility == Visibility::Shared {
            if shares.is_empty() {
                return Err(MemoryError::Invalid(
                    "shared items need at least one other user".into(),
                ));
            }
            if self.existing_users(&shares).await?.len() != shares.len() {
                return Err(MemoryError::Invalid(
                    "shared_with contains an unknown user".into(),
                ));
            }
        } else {
            shares.clear();
        }

        // ---- embed + dedupe / contradiction candidates
        let draft = MemoryItem {
            id: MemoryId::new(),
            owner_id: owner,
            kind: p.kind,
            project_id: p.project_id,
            visibility: Visibility::Private,
            shared_with: shares.clone(),
            requested_visibility: None,
            title: p.title.clone(),
            subject: p.subject.clone(),
            relation: p.relation.clone(),
            object: p.object.clone(),
            body: p.body.clone(),
            status: MemoryStatus::Active,
            salience: opts
                .salience
                .unwrap_or(self.cfg.default_salience)
                .clamp(0.0, 1.0),
            source_run_id: source_run,
            source_label: opts.source_label.clone(),
            use_count: 0,
            last_used_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let emb = self.embed_one(&item_embed_text(&draft)).await?;
        let (subj, rel, obj) = (
            normalize(&p.subject),
            normalize(&p.relation),
            normalize(&p.object),
        );
        // Scope: a project proposal matches that project and company-wide
        // items; a project-less proposal matches items of any project.
        let mut w = format!(
            "{} AND {} AND lower(trim(m.relation)) = ?",
            visible_pred("m", owner),
            status_in(
                "m.status",
                &[
                    MemoryStatus::Active,
                    MemoryStatus::Pending,
                    MemoryStatus::Flagged
                ]
            ),
        );
        let mut cand_args = vec![Arg::Text(p.relation.trim().to_lowercase())];
        if let Some(project) = p.project_id {
            w.push_str(" AND (m.project_id IS NULL OR m.project_id = ?)");
            cand_args.push(Arg::Text(project.to_string()));
        }
        let cands = fetch_items(&self.pool, &w, cand_args, true).await?;
        let mut dup: Option<(ItemRecord, f32)> = None;
        let mut contradicted: Vec<ItemRecord> = vec![];
        for c in cands {
            if !c.item.visible_to(owner)
                || normalize(&c.item.subject) != subj
                || normalize(&c.item.relation) != rel
            {
                continue;
            }
            let sim = c
                .embedding
                .as_deref()
                .map(|v| cosine(&emb, v))
                .unwrap_or(0.0);
            let same_obj = normalize(&c.item.object) == obj;
            if same_obj || sim >= self.cfg.dedupe_threshold {
                // prefer the owner's own item, then the most similar
                let better = match &dup {
                    None => true,
                    Some((d, ds)) => {
                        let own = |r: &ItemRecord| r.item.owner_id == owner;
                        (own(&c), sim) > (own(d), *ds)
                    }
                };
                if better {
                    dup = Some((c, sim));
                }
            } else if !multi_valued(&rel) && sim >= self.cfg.contradiction_threshold {
                contradicted.push(c);
            }
        }
        if let Some((existing, sim)) = dup {
            return self
                .merge_into(owner, source_run, existing, sim, &p, draft.salience)
                .await;
        }

        // ---- insert
        let requested = match p.visibility {
            Visibility::Private => None,
            v => Some(v),
        };
        let status = if requested.is_some() {
            MemoryStatus::Pending
        } else {
            MemoryStatus::Active
        };
        let now = now_ts();
        let id = draft.id.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO memory_items (id, owner_id, type, project_id, visibility, requested_visibility, title, \
             subject, relation, object, body, status, salience, source_run_id, source_label, embedding, \
             created_at, updated_at) VALUES (?, ?, ?, ?, 'private', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(owner.to_string())
        .bind(p.kind.as_str())
        .bind(p.project_id.map(|x| x.to_string()))
        .bind(requested.map(|v| v.as_str()))
        .bind(&p.title)
        .bind(&p.subject)
        .bind(&p.relation)
        .bind(&p.object)
        .bind(&p.body)
        .bind(status.as_str())
        .bind(f64::from(draft.salience))
        .bind(source_run.map(|r| r.to_string()))
        .bind(&opts.source_label)
        .bind(vec_to_blob(&emb))
        .bind(&now)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        for u in &shares {
            sqlx::query("INSERT INTO memory_shares (item_id, user_id) VALUES (?, ?)")
                .bind(&id)
                .bind(u.to_string())
                .execute(&mut *tx)
                .await?;
        }
        if let Some(run) = source_run {
            link(&mut *tx, &id, &run.to_string(), "source_run").await?;
        }
        let detail = match requested {
            Some(v) => format!("status=pending requested={}", v.as_str()),
            None => "status=active visibility=private".into(),
        };
        audit(&mut *tx, &id, Some(owner), "created", &detail).await?;
        for old in &contradicted {
            let kind = if old.item.owner_id == owner {
                "supersedes"
            } else {
                "related"
            };
            link(&mut *tx, &id, &old.item.id.to_string(), kind).await?;
        }
        tx.commit().await?;

        let superseded = if status == MemoryStatus::Active {
            self.apply_supersedes(draft.id, owner).await?
        } else {
            vec![]
        };
        let item = self.reload_and_sync(draft.id).await?;
        Ok(ProposeOutcome {
            item,
            action: ProposeAction::Created,
            superseded: if status == MemoryStatus::Active {
                superseded
            } else {
                contradicted
                    .iter()
                    .filter(|c| c.item.owner_id == owner)
                    .map(|c| c.item.id)
                    .collect()
            },
        })
    }

    /// Merge a duplicate proposal into `existing`.
    async fn merge_into(
        &self,
        owner: UserId,
        source_run: Option<RunId>,
        existing: ItemRecord,
        sim: f32,
        p: &MemoryProposal,
        salience: f32,
    ) -> Result<ProposeOutcome> {
        let e = &existing.item;
        let id = e.id.to_string();
        let new_sal = (e.salience.max(salience) + 0.05).min(1.0);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE memory_items SET salience = ?, use_count = use_count + 1, updated_at = ? WHERE id = ?")
            .bind(f64::from(new_sal))
            .bind(now_ts())
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        // A wider visibility request on the owner's own settled item turns it
        // into a pending approval, exactly like proposing it fresh would.
        let wants = p.visibility;
        if e.owner_id == owner
            && e.kind != MemoryType::Preference
            && e.status == MemoryStatus::Active
            && vis_rank(wants) > vis_rank(e.visibility)
            && (wants != Visibility::Shared || !p.shared_with.is_empty())
        {
            sqlx::query(
                "UPDATE memory_items SET status = 'pending', requested_visibility = ? WHERE id = ?",
            )
            .bind(wants.as_str())
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            if wants == Visibility::Shared {
                let valid = self.existing_users(&p.shared_with).await?;
                for u in valid.iter().filter(|u| **u != owner) {
                    sqlx::query(
                        "INSERT OR IGNORE INTO memory_shares (item_id, user_id) VALUES (?, ?)",
                    )
                    .bind(&id)
                    .bind(u.to_string())
                    .execute(&mut *tx)
                    .await?;
                }
            }
            audit(
                &mut *tx,
                &id,
                Some(owner),
                "visibility",
                &format!("requested={} (via duplicate proposal)", wants.as_str()),
            )
            .await?;
        }
        if let Some(run) = source_run {
            link(&mut *tx, &id, &run.to_string(), "source_run").await?;
            link(&mut *tx, &run.to_string(), &id, "duplicate_of").await?;
        }
        audit(
            &mut *tx,
            &id,
            Some(owner),
            "merged",
            &format!("duplicate proposal (cosine {sim:.3})"),
        )
        .await?;
        tx.commit().await?;
        let item = self.reload_and_sync(e.id).await?;
        Ok(ProposeOutcome {
            item,
            action: ProposeAction::Merged,
            superseded: vec![],
        })
    }

    /// Supersede the older items `new_id` links to with `supersedes`, if the
    /// new item is active and at least as visible as each old one.
    pub(crate) async fn apply_supersedes(
        &self,
        new_id: MemoryId,
        owner: UserId,
    ) -> Result<Vec<MemoryId>> {
        let Some(new) = db::fetch_item_unchecked(&self.pool, new_id).await? else {
            return Ok(vec![]);
        };
        if new.item.status != MemoryStatus::Active {
            return Ok(vec![]);
        }
        let olds: Vec<String> = sqlx::query_scalar(
            "SELECT to_id FROM memory_links WHERE from_id = ? AND kind = 'supersedes'",
        )
        .bind(new_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut done = vec![];
        for old in olds {
            let Ok(old_id) = old.parse::<MemoryId>() else {
                continue;
            };
            let Some(o) = db::fetch_item_unchecked(&self.pool, old_id).await? else {
                continue;
            };
            let wide_enough = vis_rank(new.item.visibility) >= vis_rank(o.item.visibility)
                && (o.item.visibility != Visibility::Shared
                    || new.item.visibility == Visibility::Company
                    || o.item
                        .shared_with
                        .iter()
                        .all(|u| new.item.shared_with.contains(u)));
            if o.item.owner_id != owner
                || !matches!(
                    o.item.status,
                    MemoryStatus::Active | MemoryStatus::Flagged | MemoryStatus::Pending
                )
                || !wide_enough
            {
                continue;
            }
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            sqlx::query(
                "UPDATE memory_items SET status = 'superseded', superseded_by = ?, requested_visibility = NULL, \
                 updated_at = ? WHERE id = ?",
            )
            .bind(new_id.to_string())
            .bind(now_ts())
            .bind(&old)
            .execute(&mut *tx)
            .await?;
            audit(
                &mut *tx,
                &old,
                Some(owner),
                "superseded",
                &format!("by {new_id}"),
            )
            .await?;
            tx.commit().await?;
            self.reload_and_sync(old_id).await?;
            done.push(old_id);
        }
        Ok(done)
    }

    // ------------------------------------------------------------ update / decide

    /// Edit an item (owner only). Visibility rules:
    /// * → `Private`: immediate (pending requests are dropped, shares cleared).
    /// * Narrowing (Company → Shared, or removing people from a share list)
    ///   on a settled item: immediate.
    /// * Widening (→ Shared/Company, adding people): status `Pending` with
    ///   `requested_visibility`; visibility is unchanged until approved.
    /// * A `Preference` can never leave `Private`.
    /// * `archived: true/false` archives / restores.
    pub async fn update(
        &self,
        user: UserId,
        id: MemoryId,
        req: UpdateMemoryRequest,
    ) -> Result<MemoryItem> {
        let rec = self.load_owned(user, id).await?;
        self.apply_update(user, rec, &req).await?;
        self.reload_and_sync(id).await
    }

    async fn apply_update(
        &self,
        user: UserId,
        rec: ItemRecord,
        req: &UpdateMemoryRequest,
    ) -> Result<()> {
        let item = &rec.item;
        let sid = item.id.to_string();
        let mut title = item.title.clone();
        let mut body = item.body.clone();
        let mut status = item.status;
        let mut visibility = item.visibility;
        let mut requested = item.requested_visibility;
        let mut shares: Vec<UserId> = item.shared_with.clone();
        let mut audits: Vec<(&str, String)> = vec![];

        if let Some(t) = &req.title {
            let t = truncate(t.trim(), MAX_TITLE);
            if t.is_empty() {
                return Err(MemoryError::Invalid("title cannot be empty".into()));
            }
            if t != title {
                title = t;
                audits.push(("edited", "title".into()));
            }
        }
        if let Some(b) = &req.body {
            let b = truncate(b.trim(), MAX_BODY);
            if b != body {
                body = b;
                audits.push(("edited", "body".into()));
            }
        }
        match req.archived {
            Some(true) if status != MemoryStatus::Archived => {
                if matches!(status, MemoryStatus::Superseded | MemoryStatus::Rejected) {
                    return Err(MemoryError::Conflict(format!(
                        "item is {}",
                        status.as_str()
                    )));
                }
                status = MemoryStatus::Archived;
                requested = None;
                audits.push(("archived", String::new()));
            }
            Some(false) if status == MemoryStatus::Archived => {
                status = MemoryStatus::Active;
                audits.push(("archived", "restored".into()));
            }
            _ => {}
        }

        let target = match (req.visibility, &req.shared_with) {
            (Some(v), _) => Some(v),
            (None, Some(_)) => {
                let t = requested.unwrap_or(visibility);
                if t != Visibility::Shared {
                    return Err(MemoryError::Invalid(
                        "shared_with requires shared visibility".into(),
                    ));
                }
                Some(t)
            }
            (None, None) => None,
        };
        if let Some(target) = target {
            if item.kind == MemoryType::Preference && target != Visibility::Private {
                return Err(MemoryError::Invalid(
                    "preferences are always private".into(),
                ));
            }
            if target != Visibility::Private
                && matches!(
                    status,
                    MemoryStatus::Superseded | MemoryStatus::Rejected | MemoryStatus::Archived
                )
            {
                return Err(MemoryError::Conflict(format!(
                    "cannot share an item that is {}",
                    status.as_str()
                )));
            }
            match target {
                Visibility::Private => {
                    visibility = Visibility::Private;
                    requested = None;
                    shares.clear();
                    if status == MemoryStatus::Pending {
                        status = MemoryStatus::Active;
                    }
                    audits.push(("visibility", "private".into()));
                }
                Visibility::Shared | Visibility::Company => {
                    let new_shares: Vec<UserId> = if target == Visibility::Shared {
                        let mut s: Vec<UserId> = req
                            .shared_with
                            .clone()
                            .unwrap_or_else(|| shares.clone())
                            .into_iter()
                            .filter(|u| *u != user)
                            .collect();
                        s.sort();
                        s.dedup();
                        if s.is_empty() {
                            return Err(MemoryError::Invalid(
                                "shared items need at least one other user".into(),
                            ));
                        }
                        if self.existing_users(&s).await?.len() != s.len() {
                            return Err(MemoryError::Invalid(
                                "shared_with contains an unknown user".into(),
                            ));
                        }
                        s
                    } else {
                        vec![]
                    };
                    let settled = status != MemoryStatus::Pending;
                    let narrowing = settled
                        && match (visibility, target) {
                            (Visibility::Company, Visibility::Company) => true,
                            (Visibility::Company, Visibility::Shared) => true,
                            (Visibility::Shared, Visibility::Shared) => {
                                new_shares.iter().all(|u| shares.contains(u))
                            }
                            _ => false,
                        };
                    if narrowing {
                        visibility = target;
                        requested = None;
                        audits.push(("visibility", target.as_str().into()));
                    } else {
                        status = MemoryStatus::Pending;
                        requested = Some(target);
                        audits.push(("visibility", format!("requested={}", target.as_str())));
                    }
                    shares = new_shares;
                }
            }
        }

        let text_changed = title != item.title || body != item.body;
        let emb = if text_changed {
            let mut draft = item.clone();
            draft.title = title.clone();
            draft.body = body.clone();
            Some(self.embed_one(&item_embed_text(&draft)).await?)
        } else {
            None
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE memory_items SET title = ?, body = ?, status = ?, visibility = ?, requested_visibility = ?, \
             embedding = COALESCE(?, embedding), updated_at = ? WHERE id = ?",
        )
        .bind(&title)
        .bind(&body)
        .bind(status.as_str())
        .bind(visibility.as_str())
        .bind(requested.map(|v| v.as_str()))
        .bind(emb.as_deref().map(vec_to_blob))
        .bind(now_ts())
        .bind(&sid)
        .execute(&mut *tx)
        .await?;
        if shares != item.shared_with {
            sqlx::query("DELETE FROM memory_shares WHERE item_id = ?")
                .bind(&sid)
                .execute(&mut *tx)
                .await?;
            for u in &shares {
                sqlx::query("INSERT INTO memory_shares (item_id, user_id) VALUES (?, ?)")
                    .bind(&sid)
                    .bind(u.to_string())
                    .execute(&mut *tx)
                    .await?;
            }
        }
        for (action, detail) in audits {
            audit(&mut *tx, &sid, Some(user), action, &detail).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The owner's decision on a pending item (approval inbox), optionally
    /// with edits applied first (title/body/share list).
    /// * `Approve` → the requested visibility, `Active`, salience +`approval_boost`.
    /// * `KeepShared` → `Shared` with the current share list, `Active`.
    /// * `KeepPrivate` → `Private`, `Active`.
    /// * `Reject` → `Rejected` (stays private to the owner).
    pub async fn decide(
        &self,
        user: UserId,
        id: MemoryId,
        req: ApproveMemoryRequest,
    ) -> Result<MemoryItem> {
        let rec = self.load_owned(user, id).await?;
        if rec.item.status != MemoryStatus::Pending {
            return Err(MemoryError::Conflict("item is not pending approval".into()));
        }
        let sid = id.to_string();
        let mut shares = rec.item.shared_with.clone();
        if let Some(edit) = &req.edit {
            // Only content and the share list; visibility is the decision itself.
            let content = UpdateMemoryRequest {
                title: edit.title.clone(),
                body: edit.body.clone(),
                ..Default::default()
            };
            if content.title.is_some() || content.body.is_some() {
                self.apply_update(user, rec.clone(), &content).await?;
            }
            if let Some(s) = &edit.shared_with {
                let mut s: Vec<UserId> = s.iter().copied().filter(|u| *u != user).collect();
                s.sort();
                s.dedup();
                if self.existing_users(&s).await?.len() != s.len() {
                    return Err(MemoryError::Invalid(
                        "shared_with contains an unknown user".into(),
                    ));
                }
                shares = s;
            }
        }
        let requested = rec.item.requested_visibility.unwrap_or(rec.item.visibility);
        let (status, visibility, action) = match req.decision {
            MemoryDecision::Approve => (MemoryStatus::Active, requested, "approved"),
            MemoryDecision::KeepShared => (MemoryStatus::Active, Visibility::Shared, "approved"),
            MemoryDecision::KeepPrivate => (MemoryStatus::Active, Visibility::Private, "approved"),
            MemoryDecision::Reject => (MemoryStatus::Rejected, Visibility::Private, "rejected"),
        };
        let visibility = if rec.item.kind == MemoryType::Preference {
            Visibility::Private
        } else {
            visibility
        };
        if visibility == Visibility::Shared && shares.is_empty() {
            return Err(MemoryError::Invalid(
                "keeping it shared needs at least one other user".into(),
            ));
        }
        if visibility != Visibility::Shared {
            shares.clear();
        }
        let boost = if req.decision == MemoryDecision::Reject {
            0.0
        } else {
            self.cfg.approval_boost
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "UPDATE memory_items SET status = ?, visibility = ?, requested_visibility = NULL, \
             salience = MIN(1.0, salience + ?), updated_at = ? WHERE id = ?",
        )
        .bind(status.as_str())
        .bind(visibility.as_str())
        .bind(f64::from(boost))
        .bind(now_ts())
        .bind(&sid)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM memory_shares WHERE item_id = ?")
            .bind(&sid)
            .execute(&mut *tx)
            .await?;
        for u in &shares {
            sqlx::query("INSERT INTO memory_shares (item_id, user_id) VALUES (?, ?)")
                .bind(&sid)
                .bind(u.to_string())
                .execute(&mut *tx)
                .await?;
        }
        let detail = format!(
            "decision={} visibility={}",
            serde_json::to_value(req.decision)?.as_str().unwrap_or(""),
            visibility.as_str()
        );
        audit(&mut *tx, &sid, Some(user), action, &detail).await?;
        tx.commit().await?;
        if status == MemoryStatus::Active {
            self.apply_supersedes(id, user).await?;
        }
        self.reload_and_sync(id).await
    }

    // ------------------------------------------------------------ uses & feedback

    /// Log that `user` used `items` (only items visible to them are logged).
    /// Bumps `use_count`/`last_used_at` and writes a `used` audit row.
    /// Returns how many were recorded.
    pub async fn record_use(
        &self,
        user: UserId,
        items: &[MemoryId],
        ctx: &UseContext,
    ) -> Result<u32> {
        if items.is_empty() {
            return Ok(0);
        }
        let ids: Vec<String> = items
            .iter()
            .map(|i| i.to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let sql = format!(
            "SELECT m.id FROM memory_items m WHERE m.id IN ({}) AND {}",
            placeholders(ids.len()),
            visible_pred("m", user)
        );
        let mut q = dynq(sql);
        for id in &ids {
            q = q.bind(id.clone());
        }
        let visible: Vec<String> = q
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| r.get::<String, _>("id"))
            .collect();
        if visible.is_empty() {
            return Ok(0);
        }
        let now = now_ts();
        let detail = match ctx.run {
            Some(r) => format!("run={r}"),
            None => String::new(),
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for id in &visible {
            sqlx::query("INSERT INTO memory_uses (item_id, run_id, task_id, role, user_id, at) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(id)
                .bind(ctx.run.map(|r| r.to_string()))
                .bind(ctx.task.map(|t| t.to_string()))
                .bind(ctx.role.map(|r| r.as_str()))
                .bind(user.to_string())
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE memory_items SET use_count = use_count + 1, last_used_at = ? WHERE id = ?",
            )
            .bind(&now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            audit(&mut *tx, id, Some(user), "used", &detail).await?;
        }
        tx.commit().await?;
        Ok(visible.len() as u32)
    }

    /// Outcome feedback for the items retrieved during `run`: passed ⇒
    /// salience += `feedback_boost` and `success_count++`; failed ⇒
    /// `failure_count++`, and an active item is `Flagged` once it has
    /// ≥ `flag_after_failures` failures and failures ≥ 2 × successes.
    /// Idempotent per (item, run).
    pub async fn feedback(&self, run: RunId, passed: bool) -> Result<FeedbackReport> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT item_id FROM memory_uses WHERE run_id = ? ORDER BY item_id",
        )
        .bind(run.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut report = FeedbackReport::default();
        let tag = format!("run={run}");
        for id in ids {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            let seen: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM memory_audit WHERE item_id = ? AND action = 'feedback' AND detail LIKE ?",
            )
            .bind(&id)
            .bind(format!("{tag} %"))
            .fetch_one(&mut *tx)
            .await?;
            if seen > 0 {
                continue;
            }
            let mut flagged = false;
            if passed {
                sqlx::query(
                    "UPDATE memory_items SET salience = MIN(1.0, salience + ?), success_count = success_count + 1, \
                     updated_at = ? WHERE id = ?",
                )
                .bind(f64::from(self.cfg.feedback_boost))
                .bind(now_ts())
                .bind(&id)
                .execute(&mut *tx)
                .await?;
                report.boosted += 1;
            } else {
                sqlx::query("UPDATE memory_items SET failure_count = failure_count + 1, updated_at = ? WHERE id = ?")
                    .bind(now_ts())
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                report.penalized += 1;
                let row = sqlx::query(
                    "SELECT status, success_count, failure_count FROM memory_items WHERE id = ?",
                )
                .bind(&id)
                .fetch_one(&mut *tx)
                .await?;
                let (st, s, f): (String, i64, i64) = (
                    row.try_get("status")?,
                    row.try_get("success_count")?,
                    row.try_get("failure_count")?,
                );
                if st == "active" && f >= i64::from(self.cfg.flag_after_failures) && f >= 2 * s {
                    sqlx::query("UPDATE memory_items SET status = 'flagged' WHERE id = ?")
                        .bind(&id)
                        .execute(&mut *tx)
                        .await?;
                    audit(
                        &mut *tx,
                        &id,
                        None,
                        "flagged",
                        &format!("failures dominate ({f} failed / {s} passed)"),
                    )
                    .await?;
                    flagged = true;
                    report.flagged += 1;
                }
            }
            audit(
                &mut *tx,
                &id,
                None,
                "feedback",
                &format!("{tag} {}", if passed { "passed" } else { "failed" }),
            )
            .await?;
            tx.commit().await?;
            if let Ok(mid) = id.parse::<MemoryId>()
                && (flagged || passed)
            {
                let _ = self.reload_and_sync(mid).await;
            }
        }
        Ok(report)
    }

    /// Items `user` can see for a project scope, in a status set — the
    /// shared base query of packs and tests.
    pub(crate) async fn visible_in_scope(
        &self,
        user: UserId,
        project: Option<ProjectId>,
        statuses: &[MemoryStatus],
        kinds: &[MemoryType],
    ) -> Result<Vec<MemoryItem>> {
        let kinds_sql = kinds
            .iter()
            .map(|k| format!("'{}'", k.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        let mut w = format!(
            "{} AND {} AND m.type IN ({kinds_sql})",
            visible_pred("m", user),
            status_in("m.status", statuses)
        );
        let mut args = vec![];
        if let Some(p) = project {
            w.push_str(" AND (m.project_id IS NULL OR m.project_id = ?)");
            args.push(Arg::Text(p.to_string()));
        }
        let recs = fetch_items(&self.pool, &w, args, false).await?;
        Ok(recs
            .into_iter()
            .map(|r| r.item)
            .filter(|i| i.visible_to(user))
            .collect())
    }
}
