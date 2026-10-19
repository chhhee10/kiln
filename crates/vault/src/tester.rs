//! Credential connection tests ("Test" button in Settings → Credentials).

use async_trait::async_trait;
use kiln_types::{CredentialHealth, CredentialKind};
use std::time::{Duration, Instant};

/// Anthropic models endpoint.
pub const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
/// OpenAI models endpoint.
pub const OPENAI_MODELS_URL: &str = "https://api.openai.com/v1/models";
/// Anthropic API version header value.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Result of testing one credential.
#[derive(Clone, Debug, PartialEq)]
pub struct TestOutcome {
    pub ok: bool,
    pub latency_ms: u32,
    /// Human-readable, never contains the secret.
    pub message: String,
    /// Health to persist; `None` leaves the stored health unchanged.
    pub health: Option<CredentialHealth>,
}

/// Something that can check a credential against its provider. A trait so
/// tests never touch the network.
#[async_trait]
pub trait CredentialTester: Send + Sync {
    /// Test `secret` of `kind` (with its base URL for LiteLLM).
    async fn test(&self, kind: CredentialKind, secret: &str, base_url: Option<&str>)
    -> TestOutcome;
}

/// Tests credentials with one lightweight authenticated `GET …/models` call.
pub struct HttpTester {
    client: reqwest::Client,
}

impl HttpTester {
    /// A tester with a 10 s timeout.
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("kiln/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for HttpTester {
    fn default() -> Self {
        Self::new()
    }
}

/// `<base>/v1/models`, tolerating a base that already ends in `/v1`.
pub fn litellm_models_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.ends_with("/v1") {
        format!("{b}/models")
    } else {
        format!("{b}/v1/models")
    }
}

#[async_trait]
impl CredentialTester for HttpTester {
    async fn test(
        &self,
        kind: CredentialKind,
        secret: &str,
        base_url: Option<&str>,
    ) -> TestOutcome {
        let req = match kind {
            CredentialKind::AnthropicApiKey => self
                .client
                .get(ANTHROPIC_MODELS_URL)
                .header("x-api-key", secret)
                .header("anthropic-version", ANTHROPIC_VERSION),
            CredentialKind::ClaudeOauth => self
                .client
                .get(ANTHROPIC_MODELS_URL)
                .bearer_auth(secret)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("anthropic-beta", "oauth-2025-04-20"),
            CredentialKind::OpenaiApiKey => self.client.get(OPENAI_MODELS_URL).bearer_auth(secret),
            CredentialKind::Litellm => match base_url {
                Some(b) => self.client.get(litellm_models_url(b)).bearer_auth(secret),
                None => {
                    return TestOutcome {
                        ok: false,
                        latency_ms: 0,
                        message: "LiteLLM credential has no base URL".into(),
                        health: Some(CredentialHealth::Invalid {
                            error: "missing base URL".into(),
                        }),
                    };
                }
            },
            CredentialKind::Other => {
                return TestOutcome {
                    ok: false,
                    latency_ms: 0,
                    message: "no connection test is available for custom credentials".into(),
                    health: None,
                };
            }
        };
        let started = Instant::now();
        let res = req.send().await;
        let latency_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        match res {
            Ok(resp) if resp.status().is_success() => TestOutcome {
                ok: true,
                latency_ms,
                message: format!("connected in {latency_ms} ms"),
                health: Some(if kind == CredentialKind::Litellm {
                    CredentialHealth::Reachable { latency_ms }
                } else {
                    CredentialHealth::Valid
                }),
            },
            Ok(resp) => {
                let code = resp.status().as_u16();
                let error = if matches!(code, 401 | 403) {
                    format!("rejected by provider (HTTP {code})")
                } else {
                    format!("unexpected response (HTTP {code})")
                };
                TestOutcome {
                    ok: false,
                    latency_ms,
                    message: error.clone(),
                    health: Some(CredentialHealth::Invalid { error }),
                }
            }
            Err(e) => {
                let error = if e.is_timeout() {
                    "timed out".to_string()
                } else if e.is_connect() {
                    "could not connect".to_string()
                } else {
                    "request failed".to_string()
                };
                TestOutcome {
                    ok: false,
                    latency_ms,
                    message: error.clone(),
                    health: Some(CredentialHealth::Invalid { error }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn litellm_urls() {
        assert_eq!(
            litellm_models_url("http://litellm:4000"),
            "http://litellm:4000/v1/models"
        );
        assert_eq!(
            litellm_models_url("http://litellm:4000/"),
            "http://litellm:4000/v1/models"
        );
        assert_eq!(
            litellm_models_url("https://x.io/v1/"),
            "https://x.io/v1/models"
        );
    }
}
