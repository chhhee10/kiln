//! SQL helpers: row mapping, the visibility predicate and dynamic binds.
//!
//! All queries are runtime-checked `sqlx::query` (no `DATABASE_URL`).

use crate::error::{MemoryError, Result};
use crate::util::{blob_to_vec, parse_id, parse_ts, placeholders};
use kiln_types::memory::{Episode, MemoryItem, MemoryStatus, MemoryType, Visibility};
use kiln_types::{MemoryId, UserId, WorkerRole};
use sqlx::sqlite::{SqliteArguments, SqliteRow};
use sqlx::{Row, Sqlite, SqlitePool};
use std::collections::HashMap;

/// Columns selected for an L2 item (table alias `m`).
pub(crate) const ITEM_COLS: &str = "m.id, m.owner_id, m.type, m.project_id, m.visibility, \
    m.requested_visibility, m.title, m.subject, m.relation, m.object, m.body, m.status, \
    m.salience, m.source_run_id, m.source_label, m.superseded_by, m.use_count, \
    m.success_count, m.failure_count, m.last_used_at, m.created_at, m.updated_at";

/// Columns selected for an L1 episode (table alias `e`).
pub(crate) const EPISODE_COLS: &str = "e.id, e.run_id, e.task_id, e.owner_id, e.project_id, \
    e.role, e.visibility, e.goal, e.outcome, e.errors_json, e.fixes_json, e.files_json, \
    e.duration_secs, e.cost_usd, e.status, e.salience, e.created_at";

/// The SQL form of [`MemoryItem::visible_to`] for table alias `a`.
///
/// Mirrors the Rust rule exactly:
/// owner sees everything; others never see pending/rejected items or
/// preferences; private → nobody else; shared → listed users; company → all.
///
/// The viewer id is inlined rather than bound so the fragment composes with
/// any positional binds. This is injection-safe: `UserId` wraps a parsed
/// `Uuid` whose `Display` is always 36 hex-and-dash characters.
pub(crate) fn visible_pred(a: &str, viewer: UserId) -> String {
    format!(
        "({a}.owner_id = '{viewer}' OR ({a}.status NOT IN ('pending','rejected') \
         AND {a}.type <> 'preference' AND ({a}.visibility = 'company' OR \
         ({a}.visibility = 'shared' AND EXISTS (SELECT 1 FROM memory_shares s \
         WHERE s.item_id = {a}.id AND s.user_id = '{viewer}')))))"
    )
}

/// Visibility of an L1 episode: the owner, or everyone when it was promoted
/// to company visibility. Episodes have no share list.
pub(crate) fn episode_visible_pred(a: &str, viewer: UserId) -> String {
    format!("({a}.owner_id = '{viewer}' OR ({a}.visibility = 'company' AND {a}.status = 'active'))")
}

/// `col IN ('a','b')` for a set of statuses (static strings, no binds).
pub(crate) fn status_in(col: &str, statuses: &[MemoryStatus]) -> String {
    let list = statuses
        .iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(",");
    format!("{col} IN ({list})")
}

/// A dynamically bound SQL argument.
#[derive(Clone, Debug)]
pub(crate) enum Arg {
    Text(String),
}

impl From<String> for Arg {
    fn from(s: String) -> Self {
        Arg::Text(s)
    }
}
impl From<&str> for Arg {
    fn from(s: &str) -> Self {
        Arg::Text(s.to_string())
    }
}

/// A query over a dynamically built SQL string. Every caller builds its SQL
/// from static fragments plus [`visible_pred`]/[`status_in`] (typed, never
/// user text); user-supplied values are always bound.
pub(crate) fn dynq(sql: String) -> sqlx::query::Query<'static, Sqlite, SqliteArguments> {
    sqlx::query(sqlx::AssertSqlSafe(sql))
}

/// Bind `args` positionally onto a query.
pub(crate) fn bind_all<'q>(
    mut q: sqlx::query::Query<'q, Sqlite, SqliteArguments>,
    args: Vec<Arg>,
) -> sqlx::query::Query<'q, Sqlite, SqliteArguments> {
    for a in args {
        q = match a {
            Arg::Text(s) => q.bind(s),
        };
    }
    q
}

/// An L2 row plus the columns `MemoryItem` does not carry.
#[derive(Clone, Debug)]
pub(crate) struct ItemRecord {
    pub item: MemoryItem,
    pub success_count: u32,
    pub failure_count: u32,
    #[allow(dead_code)]
    pub superseded_by: Option<String>,
    pub embedding: Option<Vec<f32>>,
}

fn opt_str(row: &SqliteRow, col: &str) -> Result<Option<String>> {
    Ok(row.try_get::<Option<String>, _>(col)?)
}

fn parse_enum<T>(s: &str, f: impl Fn(&str) -> Option<T>, what: &str) -> Result<T> {
    f(s).ok_or_else(|| MemoryError::Corrupt(format!("bad {what} {s:?}")))
}

pub(crate) fn map_item(row: &SqliteRow, with_embedding: bool) -> Result<ItemRecord> {
    let id: String = row.try_get("id")?;
    let item = MemoryItem {
        id: parse_id(&id)?,
        owner_id: parse_id(&row.try_get::<String, _>("owner_id")?)?,
        kind: parse_enum(
            &row.try_get::<String, _>("type")?,
            MemoryType::parse,
            "type",
        )?,
        project_id: opt_str(row, "project_id")?
            .map(|s| parse_id(&s))
            .transpose()?,
        visibility: parse_enum(
            &row.try_get::<String, _>("visibility")?,
            Visibility::parse,
            "visibility",
        )?,
        shared_with: vec![],
        requested_visibility: opt_str(row, "requested_visibility")?
            .map(|s| parse_enum(&s, Visibility::parse, "visibility"))
            .transpose()?,
        title: row.try_get("title")?,
        subject: row.try_get("subject")?,
        relation: row.try_get("relation")?,
        object: row.try_get("object")?,
        body: row.try_get("body")?,
        status: parse_enum(
            &row.try_get::<String, _>("status")?,
            MemoryStatus::parse,
            "status",
        )?,
        salience: row.try_get::<f64, _>("salience")? as f32,
        source_run_id: opt_str(row, "source_run_id")?
            .map(|s| parse_id(&s))
            .transpose()?,
        source_label: opt_str(row, "source_label")?,
        use_count: row.try_get::<i64, _>("use_count")?.max(0) as u32,
        last_used_at: opt_str(row, "last_used_at")?
            .map(|s| parse_ts(&s))
            .transpose()?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        updated_at: parse_ts(&row.try_get::<String, _>("updated_at")?)?,
    };
    let embedding = if with_embedding {
        row.try_get::<Option<Vec<u8>>, _>("embedding")?
            .map(|b| blob_to_vec(&b))
    } else {
        None
    };
    Ok(ItemRecord {
        item,
        success_count: row.try_get::<i64, _>("success_count")?.max(0) as u32,
        failure_count: row.try_get::<i64, _>("failure_count")?.max(0) as u32,
        superseded_by: opt_str(row, "superseded_by")?,
        embedding,
    })
}

/// Fill `shared_with` for every record from `memory_shares`.
pub(crate) async fn attach_shares(pool: &SqlitePool, recs: &mut [ItemRecord]) -> Result<()> {
    if recs.is_empty() {
        return Ok(());
    }
    let mut by_id: HashMap<String, Vec<UserId>> = HashMap::new();
    for chunk in recs.chunks(500) {
        let sql = format!(
            "SELECT item_id, user_id FROM memory_shares WHERE item_id IN ({}) ORDER BY user_id",
            placeholders(chunk.len())
        );
        let mut q = dynq(sql);
        for r in chunk {
            q = q.bind(r.item.id.to_string());
        }
        for row in q.fetch_all(pool).await? {
            let item: String = row.try_get("item_id")?;
            let user: String = row.try_get("user_id")?;
            by_id.entry(item).or_default().push(parse_id(&user)?);
        }
    }
    for r in recs.iter_mut() {
        r.item.shared_with = by_id.remove(&r.item.id.to_string()).unwrap_or_default();
    }
    Ok(())
}

/// `SELECT … FROM memory_items m WHERE <where_sql>` with shares attached.
pub(crate) async fn fetch_items(
    pool: &SqlitePool,
    where_sql: &str,
    args: Vec<Arg>,
    with_embedding: bool,
) -> Result<Vec<ItemRecord>> {
    let cols = if with_embedding {
        format!("{ITEM_COLS}, m.embedding")
    } else {
        ITEM_COLS.to_string()
    };
    let sql = format!("SELECT {cols} FROM memory_items m WHERE {where_sql}");
    let rows = bind_all(dynq(sql), args).fetch_all(pool).await?;
    let mut recs = rows
        .iter()
        .map(|r| map_item(r, with_embedding))
        .collect::<Result<Vec<_>>>()?;
    attach_shares(pool, &mut recs).await?;
    Ok(recs)
}

/// Load one item by id with no visibility check (internal use only).
pub(crate) async fn fetch_item_unchecked(
    pool: &SqlitePool,
    id: MemoryId,
) -> Result<Option<ItemRecord>> {
    Ok(
        fetch_items(pool, "m.id = ?", vec![Arg::Text(id.to_string())], true)
            .await?
            .pop(),
    )
}

pub(crate) fn map_episode(row: &SqliteRow) -> Result<Episode> {
    let json_vec = |col: &str| -> Result<Vec<String>> {
        let s: String = row.try_get(col)?;
        Ok(serde_json::from_str(&s)?)
    };
    Ok(Episode {
        id: parse_id(&row.try_get::<String, _>("id")?)?,
        run_id: parse_id(&row.try_get::<String, _>("run_id")?)?,
        task_id: opt_str(row, "task_id")?.map(|s| parse_id(&s)).transpose()?,
        owner_id: parse_id(&row.try_get::<String, _>("owner_id")?)?,
        project_id: opt_str(row, "project_id")?
            .map(|s| parse_id(&s))
            .transpose()?,
        role: parse_enum(
            &row.try_get::<String, _>("role")?,
            WorkerRole::parse,
            "role",
        )?,
        goal: row.try_get("goal")?,
        outcome: row.try_get("outcome")?,
        errors: json_vec("errors_json")?,
        fixes: json_vec("fixes_json")?,
        files: json_vec("files_json")?,
        duration_secs: row.try_get("duration_secs")?,
        cost_usd: row.try_get("cost_usd")?,
        visibility: parse_enum(
            &row.try_get::<String, _>("visibility")?,
            Visibility::parse,
            "visibility",
        )?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
    })
}

/// `SELECT … FROM episodes e WHERE <where_sql>`, with each episode's salience.
pub(crate) async fn fetch_episodes(
    pool: &SqlitePool,
    where_sql: &str,
    args: Vec<Arg>,
) -> Result<Vec<(Episode, f32)>> {
    let sql = format!("SELECT {EPISODE_COLS} FROM episodes e WHERE {where_sql}");
    let rows = bind_all(dynq(sql), args).fetch_all(pool).await?;
    rows.iter()
        .map(|r| Ok((map_episode(r)?, r.try_get::<f64, _>("salience")? as f32)))
        .collect()
}

/// Write one `memory_audit` row.
pub(crate) async fn audit<'e, E>(
    ex: E,
    item: &str,
    actor: Option<UserId>,
    action: &str,
    detail: &str,
) -> Result<()>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    sqlx::query(
        "INSERT INTO memory_audit (item_id, actor_id, action, detail, at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(item)
    .bind(actor.map(|a| a.to_string()))
    .bind(action)
    .bind(detail)
    .bind(crate::util::now_ts())
    .execute(ex)
    .await?;
    Ok(())
}

/// Insert a `memory_links` row (idempotent).
pub(crate) async fn link<'e, E>(ex: E, from: &str, to: &str, kind: &str) -> Result<()>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    sqlx::query("INSERT OR IGNORE INTO memory_links (from_id, to_id, kind) VALUES (?, ?, ?)")
        .bind(from)
        .bind(to)
        .bind(kind)
        .execute(ex)
        .await?;
    Ok(())
}
