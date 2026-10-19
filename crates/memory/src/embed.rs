//! Default offline embedder: deterministic feature hashing.

use crate::error::Result;
use crate::traits::Embedder;
use crate::util::{l2_normalize, words};
use async_trait::async_trait;

/// Default dimension, matching common small sentence-embedding models so a
/// switch to fastembed keeps the same storage footprint.
pub const HASH_DIM: usize = 384;

/// Feature-hashing embedder over word unigrams and bigrams.
///
/// Each feature is hashed with 64-bit FNV-1a (stable across platforms, Rust
/// versions and processes), mapped to a bucket `h % dim` with a sign taken
/// from the top bit (the "signed hashing trick", which keeps collisions
/// unbiased). Unigrams weigh 1.0, bigrams 0.5. The result is L2-normalized,
/// so the dot product equals cosine similarity.
///
/// It captures lexical overlap only — no synonyms — which is enough for
/// dedupe of near-identical proposals and for tests that must run offline.
#[derive(Clone, Debug)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    /// A hashing embedder with [`HASH_DIM`] dimensions.
    pub fn new() -> Self {
        Self { dim: HASH_DIM }
    }

    /// A hashing embedder with a custom dimension (≥ 8).
    pub fn with_dim(dim: usize) -> Self {
        Self { dim: dim.max(8) }
    }

    /// Embed one text synchronously.
    pub fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        let toks = words(text);
        let mut add = |feature: &str, weight: f32| {
            let h = fnv1a(feature.as_bytes());
            let idx = (h % self.dim as u64) as usize;
            let sign = if h >> 63 == 1 { -1.0 } else { 1.0 };
            v[idx] += sign * weight;
        };
        for t in &toks {
            add(t, 1.0);
        }
        for w in toks.windows(2) {
            add(&format!("{} {}", w[0], w[1]), 0.5);
        }
        l2_normalize(&mut v);
        v
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    fn id(&self) -> String {
        format!("hash-v1:{}", self.dim)
    }
    fn dim(&self) -> usize {
        self.dim
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }
}

/// 64-bit FNV-1a.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::cosine;

    #[test]
    fn deterministic_and_normalized() {
        let e = HashEmbedder::new();
        let a = e.embed_one("Retry the flaky upload with exponential backoff");
        let b = e.embed_one("Retry the flaky upload with exponential backoff");
        assert_eq!(a, b);
        assert_eq!(a.len(), HASH_DIM);
        let n: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-5);
    }

    #[test]
    fn similar_texts_score_higher() {
        let e = HashEmbedder::new();
        let a = e.embed_one("project FP runs tests with cargo test");
        let b = e.embed_one("project FP runs tests with cargo test --workspace");
        let c = e.embed_one("the login page uses a purple gradient");
        assert!(cosine(&a, &b) > 0.8);
        assert!(cosine(&a, &c) < 0.3);
    }

    #[test]
    fn empty_text_is_zero() {
        let e = HashEmbedder::new();
        assert!(e.embed_one("  ").iter().all(|x| *x == 0.0));
    }
}
