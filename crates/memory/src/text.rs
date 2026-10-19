//! Tantivy full-text index at `<data>/index/tantivy/`.

use crate::error::{MemoryError, Result};
use crate::traits::{IndexMeta, SearchFilter, TextDoc, TextIndex};
use async_trait::async_trait;
use kiln_types::memory::{MemoryStatus, MemoryType, Visibility};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, STORED, STRING, Schema, TEXT, Value};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};

/// Project tag stored for company-wide (project-less) rows.
const NO_PROJECT: &str = "_none_";
/// Writer heap (single indexing thread; memory items are small).
const WRITER_HEAP: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Fields {
    id: Field,
    title: Field,
    body: Field,
    subject: Field,
    relation: Field,
    object: Field,
    owner: Field,
    visibility: Field,
    shared: Field,
    project: Field,
    status: Field,
    kind: Field,
}

fn schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let f = Fields {
        id: b.add_text_field("id", STRING | STORED),
        title: b.add_text_field("title", TEXT),
        body: b.add_text_field("body", TEXT),
        subject: b.add_text_field("subject", TEXT),
        relation: b.add_text_field("relation", TEXT),
        object: b.add_text_field("object", TEXT),
        owner: b.add_text_field("owner", STRING | STORED),
        visibility: b.add_text_field("visibility", STRING | STORED),
        shared: b.add_text_field("shared", STRING | STORED),
        project: b.add_text_field("project", STRING | STORED),
        status: b.add_text_field("status", STRING | STORED),
        kind: b.add_text_field("kind", STRING | STORED),
    };
    (b.build(), f)
}

struct Inner {
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    fields: Fields,
}

/// BM25 keyword index with access tags, backed by Tantivy.
///
/// Writes commit immediately (memory mutations are rare and must be visible
/// to the next search). All Tantivy calls run on the blocking thread pool.
#[derive(Clone)]
pub struct TantivyIndex {
    inner: Arc<Inner>,
    dir: Option<PathBuf>,
}

impl std::fmt::Debug for TantivyIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TantivyIndex")
            .field("dir", &self.dir)
            .finish()
    }
}

impl TantivyIndex {
    /// Open (or create) the index in `dir`. An index with an incompatible
    /// schema or a corrupt directory is wiped and recreated — it is always
    /// rebuildable from SQLite (`Memory::reindex`).
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (schema, fields) = schema();
        let index = match Self::open_dir(dir, &schema) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, dir = %dir.display(), "tantivy index unusable; recreating");
                std::fs::remove_dir_all(dir)?;
                std::fs::create_dir_all(dir)?;
                Self::open_dir(dir, &schema)?
            }
        };
        Self::from_index(index, fields, Some(dir.to_path_buf()))
    }

    /// An in-RAM index (tests, ephemeral tools).
    pub fn in_memory() -> Result<Self> {
        let (schema, fields) = schema();
        Self::from_index(Index::create_in_ram(schema), fields, None)
    }

    fn open_dir(dir: &Path, schema: &Schema) -> Result<Index> {
        let mmap = MmapDirectory::open(dir).map_err(MemoryError::index)?;
        Ok(Index::open_or_create(mmap, schema.clone())?)
    }

    fn from_index(index: Index, fields: Fields, dir: Option<PathBuf>) -> Result<Self> {
        let writer = index.writer_with_num_threads(1, WRITER_HEAP)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            inner: Arc::new(Inner {
                index,
                reader,
                writer: Mutex::new(writer),
                fields,
            }),
            dir,
        })
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Inner) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || f(&inner))
            .await
            .map_err(MemoryError::index)?
    }
}

impl Inner {
    fn commit(&self, writer: &mut IndexWriter) -> Result<()> {
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    fn to_doc(&self, d: &TextDoc) -> TantivyDocument {
        let f = self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_text(f.id, &d.id);
        doc.add_text(f.title, &d.title);
        doc.add_text(f.body, &d.body);
        doc.add_text(f.subject, &d.subject);
        doc.add_text(f.relation, d.relation.replace('_', " "));
        doc.add_text(f.object, &d.object);
        doc.add_text(f.owner, d.meta.owner.to_string());
        doc.add_text(f.visibility, d.meta.visibility.as_str());
        for u in &d.meta.shared_with {
            doc.add_text(f.shared, u.to_string());
        }
        doc.add_text(
            f.project,
            d.meta
                .project
                .map(|p| p.to_string())
                .unwrap_or_else(|| NO_PROJECT.into()),
        );
        doc.add_text(f.status, d.meta.status.as_str());
        doc.add_text(f.kind, d.meta.kind.as_str());
        doc
    }

    fn term(&self, field: Field, value: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(field, value),
            IndexRecordOption::Basic,
        ))
    }

    fn any_of(&self, field: Field, values: &[&str]) -> Box<dyn Query> {
        Box::new(BooleanQuery::new(
            values
                .iter()
                .map(|v| (Occur::Should, self.term(field, v)))
                .collect(),
        ))
    }

    /// The access pre-filter as a Tantivy query (mirrors `visible_to`).
    fn filter_query(&self, filter: &SearchFilter) -> Box<dyn Query> {
        let f = self.fields;
        let viewer = filter.viewer.to_string();
        let not_hidden = |must: Box<dyn Query>| -> Box<dyn Query> {
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, must),
                (
                    Occur::MustNot,
                    self.any_of(f.status, &["pending", "rejected"]),
                ),
                (
                    Occur::MustNot,
                    self.term(f.kind, MemoryType::Preference.as_str()),
                ),
            ]))
        };
        let shared: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
            (Occur::Must, self.term(f.visibility, "shared")),
            (Occur::Must, self.term(f.shared, &viewer)),
        ]));
        let visible: Box<dyn Query> = Box::new(BooleanQuery::new(vec![
            (Occur::Should, self.term(f.owner, &viewer)),
            (
                Occur::Should,
                not_hidden(self.term(f.visibility, "company")),
            ),
            (Occur::Should, not_hidden(shared)),
        ]));
        let mut clauses = vec![(Occur::Must, visible)];
        if !filter.statuses.is_empty() {
            let s: Vec<&str> = filter.statuses.iter().map(|s| s.as_str()).collect();
            clauses.push((Occur::Must, self.any_of(f.status, &s)));
        }
        if let Some(kinds) = &filter.kinds {
            let k: Vec<&str> = kinds.iter().map(|k| k.as_str()).collect();
            clauses.push((Occur::Must, self.any_of(f.kind, &k)));
        }
        if let Some(p) = filter.project {
            let p = p.to_string();
            clauses.push((
                Occur::Must,
                self.any_of(f.project, &[p.as_str(), NO_PROJECT]),
            ));
        }
        Box::new(BooleanQuery::new(clauses))
    }

    fn stored_meta(&self, doc: &TantivyDocument) -> Option<(String, IndexMeta)> {
        let f = self.fields;
        let get = |field| {
            doc.get_first(field)
                .and_then(|v| v.as_str().map(str::to_string))
        };
        let id = get(f.id)?;
        let project = get(f.project)
            .filter(|p| p != NO_PROJECT)
            .and_then(|p| p.parse().ok());
        let meta = IndexMeta {
            owner: get(f.owner)?.parse().ok()?,
            kind: MemoryType::parse(&get(f.kind)?)?,
            visibility: Visibility::parse(&get(f.visibility)?)?,
            shared_with: doc
                .get_all(f.shared)
                .filter_map(|v| v.as_str().and_then(|s| s.parse().ok()))
                .collect(),
            project,
            status: MemoryStatus::parse(&get(f.status)?)?,
        };
        Some((id, meta))
    }
}

#[async_trait]
impl TextIndex for TantivyIndex {
    async fn upsert(&self, docs: &[TextDoc]) -> Result<()> {
        if docs.is_empty() {
            return Ok(());
        }
        let docs = docs.to_vec();
        self.blocking(move |inner| {
            let mut w = inner.writer.lock().map_err(MemoryError::index)?;
            for d in &docs {
                w.delete_term(Term::from_field_text(inner.fields.id, &d.id));
                w.add_document(inner.to_doc(d))?;
            }
            inner.commit(&mut w)
        })
        .await
    }

    async fn delete(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids = ids.to_vec();
        self.blocking(move |inner| {
            let mut w = inner.writer.lock().map_err(MemoryError::index)?;
            for id in &ids {
                w.delete_term(Term::from_field_text(inner.fields.id, id));
            }
            inner.commit(&mut w)
        })
        .await
    }

    async fn search(
        &self,
        query: &str,
        k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<(String, f32)>> {
        if k == 0 || query.trim().is_empty() {
            return Ok(vec![]);
        }
        let (query, filter) = (query.to_string(), filter.clone());
        self.blocking(move |inner| {
            let f = inner.fields;
            let mut parser = QueryParser::for_index(
                &inner.index,
                vec![f.title, f.body, f.subject, f.relation, f.object],
            );
            parser.set_field_boost(f.title, 2.0);
            parser.set_field_boost(f.subject, 1.5);
            parser.set_field_boost(f.object, 1.5);
            // Lenient: user text with stray quotes/operators never errors.
            let (text_q, _errors) = parser.parse_query_lenient(&query);
            let q = BooleanQuery::new(vec![
                (Occur::Must, text_q),
                (Occur::Must, inner.filter_query(&filter)),
            ]);
            let searcher = inner.reader.searcher();
            let top = searcher.search(&q, &TopDocs::with_limit(k).order_by_score())?;
            let mut out = Vec::with_capacity(top.len());
            for (score, addr) in top {
                let doc: TantivyDocument = searcher.doc(addr)?;
                // Defense in depth: re-check the stored tags.
                if let Some((id, meta)) = inner.stored_meta(&doc)
                    && filter.admits(&meta)
                {
                    out.push((id, score));
                }
            }
            Ok(out)
        })
        .await
    }

    async fn clear(&self) -> Result<()> {
        self.blocking(|inner| {
            let mut w = inner.writer.lock().map_err(MemoryError::index)?;
            w.delete_all_documents()?;
            inner.commit(&mut w)
        })
        .await
    }

    async fn count(&self) -> Result<u64> {
        self.blocking(|inner| Ok(inner.reader.searcher().num_docs()))
            .await
    }
}
