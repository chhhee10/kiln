//! fastembed (ONNX) embedder and cross-encoder reranker (feature `fastembed`).
//!
//! Models download from Hugging Face on first use into `<data>/models` and
//! are loaded on the blocking pool. Inference is CPU-bound and runs on
//! `spawn_blocking` behind a mutex (the ONNX session needs `&mut`).

use crate::error::{MemoryError, Result};
use crate::traits::{Embedder, Reranker};
use async_trait::async_trait;
use fastembed::{
    EmbeddingModel, RerankInitOptions, RerankerModel, TextEmbedding, TextInitOptions, TextRerank,
};
use std::path::Path;
use std::sync::{Arc, Mutex};

fn ferr(e: impl std::fmt::Display) -> MemoryError {
    MemoryError::Embed(format!("fastembed: {e}"))
}

/// Resolve an embedding model by Hugging Face code (`BAAI/bge-small-en-v1.5`)
/// or enum name (`BGESmallENV15`); `None` = the library default.
fn embedding_model(name: Option<&str>) -> Result<(EmbeddingModel, usize, String)> {
    let models = TextEmbedding::list_supported_models();
    let info = match name {
        None => {
            let d = EmbeddingModel::default();
            models.into_iter().find(|m| m.model == d)
        }
        Some(n) => models.into_iter().find(|m| {
            m.model_code.eq_ignore_ascii_case(n) || format!("{:?}", m.model).eq_ignore_ascii_case(n)
        }),
    }
    .ok_or_else(|| MemoryError::Invalid(format!("unknown fastembed model {name:?}")))?;
    Ok((info.model, info.dim, info.model_code))
}

fn reranker_model(name: Option<&str>) -> Result<(RerankerModel, String)> {
    let models = TextRerank::list_supported_models();
    let info = match name {
        None => {
            let d = RerankerModel::default();
            models.into_iter().find(|m| m.model == d)
        }
        Some(n) => models.into_iter().find(|m| {
            m.model_code.eq_ignore_ascii_case(n) || format!("{:?}", m.model).eq_ignore_ascii_case(n)
        }),
    }
    .ok_or_else(|| MemoryError::Invalid(format!("unknown fastembed reranker {name:?}")))?;
    Ok((info.model, info.model_code))
}

/// fastembed text embedder.
pub struct FastEmbedder {
    model: Arc<Mutex<TextEmbedding>>,
    dim: usize,
    code: String,
}

impl std::fmt::Debug for FastEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastEmbedder")
            .field("model", &self.code)
            .field("dim", &self.dim)
            .finish()
    }
}

impl FastEmbedder {
    /// Load (downloading if needed) `model` into `cache_dir`.
    pub async fn new(model: Option<&str>, cache_dir: &Path) -> Result<Self> {
        let (m, dim, code) = embedding_model(model)?;
        let cache = cache_dir.to_path_buf();
        std::fs::create_dir_all(&cache)?;
        let te = tokio::task::spawn_blocking(move || {
            TextEmbedding::try_new(
                TextInitOptions::new(m)
                    .with_cache_dir(cache)
                    .with_show_download_progress(false),
            )
        })
        .await
        .map_err(ferr)?
        .map_err(ferr)?;
        Ok(Self {
            model: Arc::new(Mutex::new(te)),
            dim,
            code,
        })
    }
}

#[async_trait]
impl Embedder for FastEmbedder {
    fn id(&self) -> String {
        format!("fastembed:{}:{}", self.code, self.dim)
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let model = self.model.clone();
        let texts = texts.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut m = model.lock().map_err(ferr)?;
            let mut out = m.embed(&texts, None).map_err(ferr)?;
            out.iter_mut().for_each(|v| crate::util::l2_normalize(v));
            Ok(out)
        })
        .await
        .map_err(ferr)?
    }
}

/// fastembed cross-encoder reranker. Scores are squashed with a logistic
/// so they land in `[0, 1]` like the lexical reranker's.
pub struct FastEmbedReranker {
    model: Arc<Mutex<TextRerank>>,
    code: String,
}

impl std::fmt::Debug for FastEmbedReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FastEmbedReranker")
            .field("model", &self.code)
            .finish()
    }
}

impl FastEmbedReranker {
    /// Load (downloading if needed) `model` into `cache_dir`.
    pub async fn new(model: Option<&str>, cache_dir: &Path) -> Result<Self> {
        let (m, code) = reranker_model(model)?;
        let cache = cache_dir.to_path_buf();
        std::fs::create_dir_all(&cache)?;
        let tr = tokio::task::spawn_blocking(move || {
            TextRerank::try_new(
                RerankInitOptions::new(m)
                    .with_cache_dir(cache)
                    .with_show_download_progress(false),
            )
        })
        .await
        .map_err(ferr)?
        .map_err(ferr)?;
        Ok(Self {
            model: Arc::new(Mutex::new(tr)),
            code,
        })
    }
}

#[async_trait]
impl Reranker for FastEmbedReranker {
    fn name(&self) -> &'static str {
        "fastembed"
    }
    async fn rerank(&self, query: &str, candidates: &[String]) -> Result<Vec<f32>> {
        if candidates.is_empty() {
            return Ok(vec![]);
        }
        let model = self.model.clone();
        let (query, docs) = (query.to_string(), candidates.to_vec());
        tokio::task::spawn_blocking(move || {
            let mut m = model.lock().map_err(ferr)?;
            let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
            let results = m.rerank(query.as_str(), refs, false, None).map_err(ferr)?;
            let mut scores = vec![0.0f32; docs.len()];
            for r in results {
                if let Some(s) = scores.get_mut(r.index) {
                    *s = 1.0 / (1.0 + (-r.score).exp());
                }
            }
            Ok(scores)
        })
        .await
        .map_err(ferr)?
    }
}
