//! Dollar cost for runs whose harness reports tokens but no cost (Codex).
//!
//! Prices come from `[prices]` in kiln.toml first, then from the run owner's
//! LiteLLM gateway (`GET /model/info`, cached per gateway for an hour).
//! Unknown models stay at $0 rather than guessing.

use crate::Scheduler;
use crate::state::lock;
use kiln_types::protocol::RunOutcome;
use kiln_types::{Run, UserId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl ModelPrice {
    pub fn cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        (tokens_in as f64 * self.input_per_mtok + tokens_out as f64 * self.output_per_mtok) / 1e6
    }
}

const CACHE_TTL: Duration = Duration::from_secs(3600);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

type PriceTable = Arc<HashMap<String, ModelPrice>>;

fn cache() -> &'static Mutex<HashMap<String, (Instant, PriceTable)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, PriceTable)>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// Parse LiteLLM's `/model/info` (per-token costs) into per-million prices.
pub(crate) fn parse_model_info(v: &serde_json::Value) -> HashMap<String, ModelPrice> {
    let mut out = HashMap::new();
    for m in v
        .get("data")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
    {
        let (Some(name), Some(info)) = (
            m.get("model_name").and_then(|n| n.as_str()),
            m.get("model_info"),
        ) else {
            continue;
        };
        let per_tok = |k: &str| info.get(k).and_then(|x| x.as_f64());
        if let (Some(i), Some(o)) = (
            per_tok("input_cost_per_token"),
            per_tok("output_cost_per_token"),
        ) && (i > 0.0 || o > 0.0)
        {
            out.insert(
                name.to_string(),
                ModelPrice {
                    input_per_mtok: i * 1e6,
                    output_per_mtok: o * 1e6,
                },
            );
        }
    }
    out
}

async fn gateway_prices(base_url: &str, key: &str) -> Option<PriceTable> {
    let root = base_url.trim().trim_end_matches('/');
    let root = root.strip_suffix("/v1").unwrap_or(root).to_string();
    if let Some((at, t)) = lock(cache()).get(&root)
        && at.elapsed() < CACHE_TTL
    {
        return Some(t.clone());
    }
    let resp = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .ok()?
        .get(format!("{root}/model/info"))
        .bearer_auth(key)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), "gateway /model/info unavailable");
        return None;
    }
    let table = Arc::new(parse_model_info(&resp.json().await.ok()?));
    lock(cache()).insert(root, (Instant::now(), table.clone()));
    Some(table)
}

impl Scheduler {
    /// Fill in `outcome`'s cost when the harness reported tokens but no cost.
    pub(crate) async fn price_outcome(&self, run: &Run, outcome: &mut RunOutcome) {
        let u = &outcome.usage;
        if u.cost_usd > 0.0 || (u.tokens_in == 0 && u.tokens_out == 0) {
            return;
        }
        if let Some(p) = self.model_price(run.owner_id, &run.model).await {
            outcome.usage.cost_usd = p.cost(u.tokens_in, u.tokens_out);
        }
    }

    async fn model_price(&self, owner: UserId, model: &str) -> Option<ModelPrice> {
        if let Some(p) = self.cfg.prices.get(model) {
            return Some(*p);
        }
        let env = self.vault.dispatch_env(owner).await.ok()?.env;
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let (base, key) = (get("LITELLM_BASE_URL")?, get("LITELLM_API_KEY")?);
        gateway_prices(&base, &key).await?.get(model).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_litellm_model_info() {
        let v = serde_json::json!({"data": [
            {"model_name": "gpt-6.1-sol", "model_info": {"input_cost_per_token": 0.000002, "output_cost_per_token": 0.00001}},
            {"model_name": "free", "model_info": {"input_cost_per_token": 0, "output_cost_per_token": 0}},
            {"model_name": "noinfo"}
        ]});
        let t = parse_model_info(&v);
        assert_eq!(t.len(), 1);
        let p = t["gpt-6.1-sol"];
        assert!((p.input_per_mtok - 2.0).abs() < 1e-9);
        assert!((p.cost(1_000_000, 100_000) - 3.0).abs() < 1e-9);
    }
}
