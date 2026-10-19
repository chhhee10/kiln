//! Default offline reranker: lexical overlap.

use crate::error::Result;
use crate::traits::Reranker;
use crate::util::words;
use async_trait::async_trait;
use std::collections::HashSet;

/// Scores a candidate by how much of the query it covers.
///
/// `score = 0.7 · unigram_recall + 0.3 · bigram_recall`, where recall is the
/// fraction of distinct query unigrams (resp. bigrams) present in the
/// candidate. Single-word queries use unigram recall only. Scores are in
/// `[0, 1]`; an empty query scores every candidate 0.
#[derive(Clone, Debug, Default)]
pub struct LexicalReranker;

impl LexicalReranker {
    /// Score one candidate synchronously.
    pub fn score(query: &str, candidate: &str) -> f32 {
        let q = words(query);
        if q.is_empty() {
            return 0.0;
        }
        let c = words(candidate);
        let c_uni: HashSet<&str> = c.iter().map(String::as_str).collect();
        let q_uni: HashSet<&str> = q.iter().map(String::as_str).collect();
        let uni = q_uni.iter().filter(|w| c_uni.contains(*w)).count() as f32 / q_uni.len() as f32;
        let q_bi: HashSet<(&str, &str)> = q
            .windows(2)
            .map(|w| (w[0].as_str(), w[1].as_str()))
            .collect();
        if q_bi.is_empty() {
            return uni;
        }
        let c_bi: HashSet<(&str, &str)> = c
            .windows(2)
            .map(|w| (w[0].as_str(), w[1].as_str()))
            .collect();
        let bi = q_bi.iter().filter(|b| c_bi.contains(*b)).count() as f32 / q_bi.len() as f32;
        0.7 * uni + 0.3 * bi
    }
}

#[async_trait]
impl Reranker for LexicalReranker {
    fn name(&self) -> &'static str {
        "lexical"
    }
    async fn rerank(&self, query: &str, candidates: &[String]) -> Result<Vec<f32>> {
        Ok(candidates.iter().map(|c| Self::score(query, c)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_scores() {
        assert_eq!(LexicalReranker::score("", "anything"), 0.0);
        assert!((LexicalReranker::score("cargo test", "we run cargo test") - 1.0).abs() < 1e-6);
        let partial = LexicalReranker::score("cargo test flaky", "cargo build");
        assert!(partial > 0.0 && partial < 0.5);
        assert_eq!(LexicalReranker::score("upload", "download"), 0.0);
    }
}
