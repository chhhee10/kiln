//! Release acceptance: one user can never read another user's
//! private memory. Every gateway read path is checked against the oracle
//! `MemoryItem::visible_to` over every visibility × status × kind × project
//! combination, including with stale and actively lying indexes.

mod common;

use async_trait::async_trait;
use common::*;
use kiln_memory::{
    Components, HashEmbedder, IndexMeta, LISTABLE, LexicalReranker, Memory, MemoryConfig,
    RETRIEVABLE, SearchFilter, TextDoc, TextIndex, UseContext, VectorIndex,
};
use kiln_types::api::{ApproveMemoryRequest, MemoryDecision, UpdateMemoryRequest};
use kiln_types::memory::{MemoryItem, MemoryQuery, MemoryStatus, MemoryType, Visibility};
use kiln_types::{MemoryId, ProjectId, UserId, WorkerRole};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

const STATUSES: [MemoryStatus; 6] = [
    MemoryStatus::Active,
    MemoryStatus::Pending,
    MemoryStatus::Flagged,
    MemoryStatus::Superseded,
    MemoryStatus::Archived,
    MemoryStatus::Rejected,
];

fn cfg() -> MemoryConfig {
    MemoryConfig {
        max_search_limit: 1000,
        pack_token_budget: 1_000_000,
        search_episodes: false,
        ..Default::default()
    }
}

struct Fixture {
    env: Env,
    users: [UserId; 3],
    p1: ProjectId,
    /// Oracle copies of every inserted item, with `token` = a unique word.
    items: Vec<(MemoryItem, String)>,
}

/// Insert a row directly (bypassing proposal rules) so that every
/// combination — including ones the rules never produce — is covered.
async fn insert_raw(env: &Env, item: &MemoryItem) {
    let now = ts(item.created_at);
    sqlx::query(
        "INSERT INTO memory_items (id, owner_id, type, project_id, visibility, requested_visibility, title, subject, \
         relation, object, body, status, salience, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(item.id.to_string())
    .bind(item.owner_id.to_string())
    .bind(item.kind.as_str())
    .bind(item.project_id.map(|p| p.to_string()))
    .bind(item.visibility.as_str())
    .bind(item.requested_visibility.map(|v| v.as_str()))
    .bind(&item.title)
    .bind(&item.subject)
    .bind(&item.relation)
    .bind(&item.object)
    .bind(&item.body)
    .bind(item.status.as_str())
    .bind(0.5f64)
    .bind(&now)
    .bind(&now)
    .execute(&env.pool)
    .await
    .unwrap();
    for u in &item.shared_with {
        sqlx::query("INSERT INTO memory_shares (item_id, user_id) VALUES (?, ?)")
            .bind(item.id.to_string())
            .bind(u.to_string())
            .execute(&env.pool)
            .await
            .unwrap();
    }
}

async fn fixture_with(env: Env) -> Fixture {
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let c = add_user(&env.pool, "Cy Moss").await;
    let p1 = add_project(&env.pool, "FP").await;
    let p2 = add_project(&env.pool, "WEB").await;
    let mut items = vec![];
    let mut n = 0;
    for owner in [a, b] {
        let other = if owner == a { b } else { a };
        let shares: [(Visibility, Vec<UserId>); 4] = [
            (Visibility::Private, vec![]),
            (Visibility::Shared, vec![c]),
            (Visibility::Shared, vec![other]),
            (Visibility::Company, vec![]),
        ];
        for kind in [MemoryType::Fact, MemoryType::Preference] {
            for (vis, shared) in &shares {
                for status in STATUSES {
                    for project in [None, Some(p1), Some(p2)] {
                        n += 1;
                        let token = format!("tok{n:04}");
                        let item = MemoryItem {
                            id: MemoryId::new(),
                            owner_id: owner,
                            kind,
                            project_id: project,
                            visibility: *vis,
                            shared_with: shared.clone(),
                            requested_visibility: (status == MemoryStatus::Pending)
                                .then_some(Visibility::Company),
                            title: format!("zebra note {token}"),
                            subject: "zebra".into(),
                            relation: "is".into(),
                            object: format!("striped {token}"),
                            body: "common zebra body".into(),
                            status,
                            salience: 0.5,
                            source_run_id: None,
                            source_label: None,
                            use_count: 0,
                            last_used_at: None,
                            created_at: chrono::Utc::now(),
                            updated_at: chrono::Utc::now(),
                        };
                        insert_raw(&env, &item).await;
                        items.push((item, token));
                    }
                }
            }
        }
    }
    env.mem.reembed_all().await.unwrap();
    Fixture {
        env,
        users: [a, b, c],
        p1,
        items,
    }
}

async fn fixture() -> Fixture {
    fixture_with(env_with(cfg()).await).await
}

fn in_scope(i: &MemoryItem, project: Option<ProjectId>) -> bool {
    project.is_none() || i.project_id.is_none() || i.project_id == project
}

impl Fixture {
    fn oracle(
        &self,
        user: UserId,
        statuses: &[MemoryStatus],
        f: impl Fn(&MemoryItem) -> bool,
    ) -> BTreeSet<MemoryId> {
        self.items
            .iter()
            .map(|(i, _)| i)
            .filter(|i| i.visible_to(user) && statuses.contains(&i.status) && f(i))
            .map(|i| i.id)
            .collect()
    }
    fn mem(&self) -> &Memory {
        &self.env.mem
    }
}

fn ids(items: impl IntoIterator<Item = MemoryId>) -> BTreeSet<MemoryId> {
    items.into_iter().collect()
}

#[tokio::test]
async fn fixture_covers_every_combination() {
    let fx = fixture().await;
    assert_eq!(fx.items.len(), 2 * 2 * 4 * 6 * 3);
    // Sanity: every user sees some items and is denied some.
    for u in fx.users {
        let visible = fx.items.iter().filter(|(i, _)| i.visible_to(u)).count();
        assert!(visible > 0 && visible < fx.items.len(), "{visible}");
    }
}

#[tokio::test]
async fn search_returns_exactly_the_visible_set() {
    let fx = fixture().await;
    for u in fx.users {
        for project in [None, Some(fx.p1)] {
            let q = MemoryQuery {
                q: "zebra".into(),
                project_id: project,
                limit: Some(1000),
                ..Default::default()
            };
            let got = ids(fx
                .mem()
                .search(u, &q)
                .await
                .unwrap()
                .into_iter()
                .map(|h| h.item.id));
            let want = fx.oracle(u, &RETRIEVABLE, |i| in_scope(i, project));
            assert_eq!(got, want, "search user={u} project={project:?}");
        }
        // empty query (browse by salience) uses the same hard filter
        let q = MemoryQuery {
            q: String::new(),
            limit: Some(1000),
            ..Default::default()
        };
        let got = ids(fx
            .mem()
            .search(u, &q)
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.item.id));
        assert_eq!(
            got,
            fx.oracle(u, &RETRIEVABLE, |_| true),
            "empty-query search user={u}"
        );
        // pending_only = the user's own pending items
        let q = MemoryQuery {
            q: "zebra".into(),
            pending_only: true,
            limit: Some(1000),
            ..Default::default()
        };
        let got = ids(fx
            .mem()
            .search(u, &q)
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.item.id));
        assert_eq!(
            got,
            fx.oracle(u, &[MemoryStatus::Pending], |i| i.owner_id == u),
            "pending search user={u}"
        );
        // visibility + kind filters
        for vis in [Visibility::Private, Visibility::Shared, Visibility::Company] {
            let q = MemoryQuery {
                q: "zebra".into(),
                visibility: Some(vis),
                kind: Some(MemoryType::Fact),
                limit: Some(1000),
                ..Default::default()
            };
            let got = ids(fx
                .mem()
                .search(u, &q)
                .await
                .unwrap()
                .into_iter()
                .map(|h| h.item.id));
            let want = fx.oracle(u, &RETRIEVABLE, |i| {
                i.visibility == vis && i.kind == MemoryType::Fact
            });
            assert_eq!(got, want, "search user={u} vis={vis:?}");
        }
    }
}

#[tokio::test]
async fn targeted_search_and_mcp_search_never_leak() {
    let fx = fixture().await;
    for u in fx.users {
        let allowed = fx.oracle(u, &RETRIEVABLE, |_| true);
        for (item, token) in &fx.items {
            let q = MemoryQuery {
                q: token.clone(),
                limit: Some(5),
                ..Default::default()
            };
            let got = ids(fx
                .mem()
                .search(u, &q)
                .await
                .unwrap()
                .into_iter()
                .map(|h| h.item.id));
            assert!(got.is_subset(&allowed), "search leaked for {u}");
            assert_eq!(
                got.contains(&item.id),
                allowed.contains(&item.id),
                "search {token} for {u}"
            );

            let v = fx
                .mem()
                .mcp_search(u, None, token, Some(5), &UseContext::default())
                .await
                .unwrap();
            let got: BTreeSet<MemoryId> = v["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap().parse().unwrap())
                .collect();
            assert!(got.is_subset(&allowed), "mcp_search leaked for {u}");
            assert_eq!(
                got.contains(&item.id),
                allowed.contains(&item.id),
                "mcp_search {token} for {u}"
            );
        }
    }
}

#[tokio::test]
async fn list_returns_exactly_the_visible_set() {
    let fx = fixture().await;
    for u in fx.users {
        let base = MemoryQuery {
            limit: Some(1000),
            ..Default::default()
        };
        let r = fx.mem().list(u, &base).await.unwrap();
        let want = fx.oracle(u, &LISTABLE, |_| true);
        assert_eq!(ids(r.items.iter().map(|i| i.id)), want, "list all user={u}");
        assert_eq!(r.total as usize, want.len());
        let pending = fx.oracle(u, &[MemoryStatus::Pending], |i| i.owner_id == u);
        assert_eq!(r.pending_approvals as usize, pending.len());

        for vis in [Visibility::Private, Visibility::Shared, Visibility::Company] {
            let q = MemoryQuery {
                visibility: Some(vis),
                ..base.clone()
            };
            let got = ids(fx
                .mem()
                .list(u, &q)
                .await
                .unwrap()
                .items
                .iter()
                .map(|i| i.id));
            assert_eq!(
                got,
                fx.oracle(u, &LISTABLE, |i| i.visibility == vis),
                "list vis={vis:?} user={u}"
            );
        }
        let q = MemoryQuery {
            pending_only: true,
            ..base.clone()
        };
        let got = ids(fx
            .mem()
            .list(u, &q)
            .await
            .unwrap()
            .items
            .iter()
            .map(|i| i.id));
        assert_eq!(got, pending, "approval queue user={u}");

        let q = MemoryQuery {
            project_id: Some(fx.p1),
            kind: Some(MemoryType::Preference),
            ..base.clone()
        };
        let got = ids(fx
            .mem()
            .list(u, &q)
            .await
            .unwrap()
            .items
            .iter()
            .map(|i| i.id));
        assert_eq!(
            got,
            fx.oracle(u, &LISTABLE, |i| i.kind == MemoryType::Preference
                && in_scope(i, Some(fx.p1))),
            "list project+kind user={u}"
        );
        // free-text filter
        let (item, token) = &fx.items[5];
        let q = MemoryQuery {
            q: token.clone(),
            ..base.clone()
        };
        let got = ids(fx
            .mem()
            .list(u, &q)
            .await
            .unwrap()
            .items
            .iter()
            .map(|i| i.id));
        let expect = if item.visible_to(u) && LISTABLE.contains(&item.status) {
            ids([item.id])
        } else {
            ids([])
        };
        assert_eq!(got, expect);
    }
}

#[tokio::test]
async fn get_matches_visible_to_for_every_item() {
    let fx = fixture().await;
    for u in fx.users {
        for (item, _) in &fx.items {
            match fx.mem().get(u, item.id).await {
                Ok(d) => {
                    assert!(item.visible_to(u), "get leaked {} to {u}", item.id);
                    assert_eq!(d.item.id, item.id);
                    assert_eq!(d.owner.id, item.owner_id);
                }
                Err(e) => {
                    assert!(!item.visible_to(u), "get denied a visible item: {e}");
                    assert!(e.is_not_found(), "denial must look like absence, got {e}");
                }
            }
        }
    }
}

#[tokio::test]
async fn packs_contain_exactly_the_visible_set() {
    let fx = fixture().await;
    for u in fx.users {
        let want = fx.oracle(u, &RETRIEVABLE, |i| {
            in_scope(i, Some(fx.p1)) && i.kind != MemoryType::Episode
        });
        for role in [
            WorkerRole::Builder,
            WorkerRole::Qa,
            WorkerRole::Reviewer,
            WorkerRole::Planner,
        ] {
            let (text, got) = fx.mem().pack_with_items(u, fx.p1, role).await.unwrap();
            assert_eq!(ids(got), want, "pack {role:?} user={u}");
            for (item, token) in &fx.items {
                if !want.contains(&item.id) {
                    assert!(!text.contains(token.as_str()), "pack text leaked {token}");
                }
            }
            // cached second read is identical
            let (text2, _) = fx.mem().pack_with_items(u, fx.p1, role).await.unwrap();
            assert_eq!(text, text2);
        }
    }
}

#[tokio::test]
async fn stale_text_index_cannot_leak_an_item_made_private() {
    let env = env_with(cfg()).await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let p = add_project(&env.pool, "FP").await;
    let mut pr = prop(
        MemoryType::Fact,
        "canary",
        "is",
        "stalecanary secret",
        Visibility::Private,
    );
    pr.project_id = Some(p);
    let item = env.mem.propose(a, None, pr).await.unwrap();
    // Owner publishes it company-wide (pending → approve).
    env.mem
        .update(
            a,
            item.id,
            UpdateMemoryRequest {
                visibility: Some(Visibility::Company),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    env.mem
        .decide(
            a,
            item.id,
            ApproveMemoryRequest {
                decision: MemoryDecision::Approve,
                edit: None,
            },
        )
        .await
        .unwrap();
    let q = MemoryQuery {
        q: "stalecanary".into(),
        ..Default::default()
    };
    assert_eq!(
        env.mem.search(b, &q).await.unwrap().len(),
        1,
        "B sees the company item"
    );
    let (_, pack_ids) = env
        .mem
        .pack_with_items(b, p, WorkerRole::Builder)
        .await
        .unwrap();
    assert!(pack_ids.contains(&item.id));

    // Owner makes it private again.
    let private = env
        .mem
        .update(
            a,
            item.id,
            UpdateMemoryRequest {
                visibility: Some(Visibility::Private),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(private.visibility, Visibility::Private);

    // Make the text index deliberately stale: it still claims "company".
    let mut stale = private.clone();
    stale.visibility = Visibility::Company;
    let doc = TextDoc {
        id: stale.id.to_string(),
        title: stale.title.clone(),
        body: stale.body.clone(),
        subject: stale.subject.clone(),
        relation: stale.relation.clone(),
        object: stale.object.clone(),
        meta: IndexMeta {
            owner: a,
            kind: stale.kind,
            visibility: Visibility::Company,
            shared_with: vec![],
            project: Some(p),
            status: MemoryStatus::Active,
        },
    };
    env.mem.text_index().upsert(&[doc]).await.unwrap();
    let raw = env
        .mem
        .text_index()
        .search("stalecanary", 10, &SearchFilter::for_viewer(b))
        .await
        .unwrap();
    assert_eq!(raw.len(), 1, "the index itself is stale and would leak");

    assert!(
        env.mem.search(b, &q).await.unwrap().is_empty(),
        "search leaked a private item"
    );
    let v = env
        .mem
        .mcp_search(b, Some(p), "stalecanary", None, &UseContext::default())
        .await
        .unwrap();
    assert_eq!(v["count"], 0, "mcp_search leaked a private item");
    let l = env
        .mem
        .list(
            b,
            &MemoryQuery {
                q: "stalecanary".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(l.items.is_empty());
    assert!(env.mem.get(b, item.id).await.unwrap_err().is_not_found());
    let (text, pack_ids) = env
        .mem
        .pack_with_items(b, p, WorkerRole::Builder)
        .await
        .unwrap();
    assert!(!pack_ids.contains(&item.id), "stale cached pack leaked");
    assert!(!text.contains("stalecanary"));
    // The owner still finds it.
    assert_eq!(env.mem.search(a, &q).await.unwrap().len(), 1);

    // Out-of-band SQL change (no index sync at all) is also safe.
    let other = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Fact,
                "otter",
                "is",
                "oobcanary",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE memory_items SET visibility = 'company' WHERE id = ?")
        .bind(other.id.to_string())
        .execute(&env.pool)
        .await
        .unwrap();
    env.mem.reindex().await.unwrap();
    let q2 = MemoryQuery {
        q: "oobcanary".into(),
        ..Default::default()
    };
    assert_eq!(env.mem.search(b, &q2).await.unwrap().len(), 1);
    sqlx::query("UPDATE memory_items SET visibility = 'private' WHERE id = ?")
        .bind(other.id.to_string())
        .execute(&env.pool)
        .await
        .unwrap();
    assert!(
        env.mem.search(b, &q2).await.unwrap().is_empty(),
        "index still says company; SQL must win"
    );
}

/// An index that ignores every filter and returns every id it has seen.
#[derive(Default)]
struct LyingIndex {
    ids: Mutex<Vec<String>>,
}

#[async_trait]
impl TextIndex for LyingIndex {
    async fn upsert(&self, docs: &[TextDoc]) -> kiln_memory::Result<()> {
        let mut ids = self.ids.lock().unwrap();
        for d in docs {
            if !ids.contains(&d.id) {
                ids.push(d.id.clone());
            }
        }
        Ok(())
    }
    async fn delete(&self, _ids: &[String]) -> kiln_memory::Result<()> {
        Ok(())
    }
    async fn search(
        &self,
        _q: &str,
        _k: usize,
        _f: &SearchFilter,
    ) -> kiln_memory::Result<Vec<(String, f32)>> {
        Ok(self
            .ids
            .lock()
            .unwrap()
            .iter()
            .map(|i| (i.clone(), 1.0))
            .collect())
    }
    async fn clear(&self) -> kiln_memory::Result<()> {
        Ok(())
    }
    async fn count(&self) -> kiln_memory::Result<u64> {
        Ok(self.ids.lock().unwrap().len() as u64)
    }
}

#[async_trait]
impl VectorIndex for LyingIndex {
    fn name(&self) -> &'static str {
        "lying"
    }
    async fn upsert(&self, id: &str, _v: &[f32], _m: &IndexMeta) -> kiln_memory::Result<()> {
        let mut ids = self.ids.lock().unwrap();
        if !ids.iter().any(|i| i == id) {
            ids.push(id.to_string());
        }
        Ok(())
    }
    async fn remove(&self, _id: &str) -> kiln_memory::Result<()> {
        Ok(())
    }
    async fn search(
        &self,
        _v: &[f32],
        _k: usize,
        _f: &SearchFilter,
    ) -> kiln_memory::Result<Vec<(String, f32)>> {
        Ok(self
            .ids
            .lock()
            .unwrap()
            .iter()
            .map(|i| (i.clone(), 1.0))
            .collect())
    }
    async fn count(&self) -> kiln_memory::Result<u64> {
        Ok(self.ids.lock().unwrap().len() as u64)
    }
}

#[tokio::test]
async fn lying_indexes_cannot_leak() {
    let dir = tempfile::tempdir().unwrap();
    let pool = pool_at(&dir.path().join("kiln.db")).await;
    let data = dir.path().join("data");
    let comps = Components {
        embedder: Arc::new(HashEmbedder::new()),
        reranker: Arc::new(LexicalReranker),
        text: Arc::new(LyingIndex::default()),
        item_vectors: Arc::new(LyingIndex::default()),
        episode_vectors: Arc::new(LyingIndex::default()),
    };
    let mem = Memory::open_with(pool.clone(), &data, cfg(), comps)
        .await
        .unwrap();
    let env = Env {
        mem,
        pool,
        data,
        _dir: dir,
    };
    let fx = fixture_with(env).await;
    for u in fx.users {
        let q = MemoryQuery {
            q: "anything at all".into(),
            limit: Some(1000),
            ..Default::default()
        };
        let got = ids(fx
            .mem()
            .search(u, &q)
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.item.id));
        assert_eq!(
            got,
            fx.oracle(u, &RETRIEVABLE, |_| true),
            "lying index leaked to {u}"
        );
        let v = fx
            .mem()
            .mcp_search(u, None, "anything", Some(20), &UseContext::default())
            .await
            .unwrap();
        let allowed = fx.oracle(u, &RETRIEVABLE, |_| true);
        for r in v["results"].as_array().unwrap() {
            assert!(allowed.contains(&r["id"].as_str().unwrap().parse().unwrap()));
        }
    }
}

#[tokio::test]
async fn non_owners_cannot_mutate_and_uses_need_visibility() {
    let env = env_with(cfg()).await;
    let a = add_user(&env.pool, "Ada Rao").await;
    let b = add_user(&env.pool, "Ben Ito").await;
    let private = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Fact,
                "x",
                "is",
                "private one",
                Visibility::Private,
            ),
        )
        .await
        .unwrap();
    let company = env
        .mem
        .propose(
            a,
            None,
            prop(
                MemoryType::Fact,
                "y",
                "is",
                "company one",
                Visibility::Company,
            ),
        )
        .await
        .unwrap();
    env.mem
        .decide(
            a,
            company.id,
            ApproveMemoryRequest {
                decision: MemoryDecision::Approve,
                edit: None,
            },
        )
        .await
        .unwrap();

    let edit = UpdateMemoryRequest {
        title: Some("hijack".into()),
        ..Default::default()
    };
    assert!(
        env.mem
            .update(b, private.id, edit.clone())
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        env.mem
            .update(b, company.id, edit)
            .await
            .unwrap_err()
            .is_forbidden()
    );
    let dec = ApproveMemoryRequest {
        decision: MemoryDecision::Reject,
        edit: None,
    };
    assert!(
        env.mem
            .decide(b, private.id, dec.clone())
            .await
            .unwrap_err()
            .is_not_found()
    );
    assert!(
        env.mem
            .decide(b, company.id, dec)
            .await
            .unwrap_err()
            .is_forbidden()
    );

    let n = env
        .mem
        .record_use(b, &[private.id, company.id], &UseContext::default())
        .await
        .unwrap();
    assert_eq!(n, 1, "only the visible item is logged");
    let d = env.mem.get(a, private.id).await.unwrap();
    assert_eq!(d.item.use_count, 0);
}
