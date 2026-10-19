//! Secret redaction: exact-match redaction of known secrets ([`Redactor`])
//! and pattern-based redaction of anything that looks like a credential
//! ([`scan_for_secrets`]). Used on transcripts, logs and memory at ingest.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use base64::Engine;
use regex::{Captures, Regex};
use std::borrow::Cow;
use std::sync::LazyLock;

/// Replacement text for every redacted secret.
pub const REDACTED: &str = "[REDACTED]";

/// Known secrets shorter than this are ignored: redacting a 3-character
/// "secret" would shred ordinary text.
pub const MIN_SECRET_LEN: usize = 6;

/// Exact-string redactor for a set of known secret values.
///
/// Matching is leftmost-longest, so when one secret contains another the
/// longer one is redacted whole. The base64 and percent-encoded forms of each
/// secret are redacted too.
#[derive(Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
    matcher: Option<AhoCorasick>,
}

impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field("secrets", &self.secrets.len())
            .finish()
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn variants(secret: &str) -> impl Iterator<Item = String> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(secret);
    let b64_nopad = b64.trim_end_matches('=').to_string();
    let b64url = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
    [
        secret.to_string(),
        b64,
        b64_nopad,
        b64url,
        percent_encode(secret),
    ]
    .into_iter()
}

impl Redactor {
    /// A redactor for these secret values.
    pub fn new<I, S>(secrets: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut r = Self::default();
        r.extend(secrets);
        r
    }

    /// Add more secret values.
    pub fn extend<I, S>(&mut self, secrets: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for s in secrets {
            let s = s.as_ref().trim();
            if s.chars().count() < MIN_SECRET_LEN {
                continue;
            }
            for v in variants(s) {
                if v.len() >= MIN_SECRET_LEN && !self.secrets.contains(&v) {
                    self.secrets.push(v);
                }
            }
        }
        // Longest first, for readability of `secrets`; leftmost-longest
        // matching does the real work.
        self.secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        self.matcher = if self.secrets.is_empty() {
            None
        } else {
            AhoCorasickBuilder::new()
                .match_kind(MatchKind::LeftmostLongest)
                .build(&self.secrets)
                .ok()
        };
    }

    /// Merge another redactor's secrets into this one.
    pub fn merge(&mut self, other: &Redactor) {
        self.extend(other.secrets.clone());
    }

    /// True when no secrets are registered.
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Replace every known secret (and its encodings) with `[REDACTED]`.
    pub fn redact<'a>(&self, s: &'a str) -> Cow<'a, str> {
        match &self.matcher {
            Some(m) if m.is_match(s) => {
                Cow::Owned(m.replace_all(s, &vec![REDACTED; self.secrets.len()]))
            }
            _ => Cow::Borrowed(s),
        }
    }

    /// [`Redactor::redact`] followed by [`scan_for_secrets`]: the full
    /// ingest-time scrub.
    pub fn scrub(&self, s: &str) -> String {
        let known = self.redact(s);
        scan_for_secrets(&known).into_owned()
    }
}

// ---------------------------------------------------------------- pattern scan

struct Pattern {
    re: Regex,
    /// Replace the whole match (`None`) or keep capture group 1 as a prefix.
    keep_prefix: bool,
}

fn pattern(re: &str, keep_prefix: bool) -> Option<Pattern> {
    match Regex::new(re) {
        Ok(re) => Some(Pattern { re, keep_prefix }),
        Err(e) => {
            // Patterns are constants; this only fires on a programming error.
            tracing::error!(error = %e, "invalid secret pattern");
            None
        }
    }
}

static PATTERNS: LazyLock<Vec<Pattern>> = LazyLock::new(|| {
    [
        // PEM private key blocks (RSA, EC, OPENSSH, ENCRYPTED, ...).
        pattern(
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            false,
        ),
        // Anthropic keys and OAuth tokens.
        pattern(r"sk-ant-[A-Za-z0-9_\-]{10,}", false),
        // OpenAI-style keys (sk-, sk-proj-, sk-svcacct-, LiteLLM sk-...).
        pattern(r"\bsk-[A-Za-z0-9_\-]{16,}", false),
        // GitHub tokens.
        pattern(r"\b(?:ghp|gho|ghs|ghu|ghr)_[A-Za-z0-9]{20,}", false),
        pattern(r"\bgithub_pat_[A-Za-z0-9_]{20,}", false),
        // AWS access key ids.
        pattern(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", false),
        // Slack tokens.
        pattern(r"\bxox[abposr]-[A-Za-z0-9\-]{10,}", false),
        // Authorization: Bearer <token>
        pattern(r"(?i)(\bbearer\s+)[A-Za-z0-9._~+/=\-]{12,}", true),
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// `NAME=value` / `"name": "value"` assignments whose name says "secret".
static ASSIGNMENT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([A-Za-z0-9_\-]*(?:token|secret|api[_\-]?key|password|passwd|private[_\-]?key|access[_\-]?key|auth)[A-Za-z0-9_\-]*["']?\s*[=:]\s*["']?)([^\s"',;&]{8,})"#,
    )
    .map_err(|e| tracing::error!(error = %e, "invalid assignment pattern"))
    .ok()
});

/// Heuristic: does an assigned value look like a credential rather than a
/// number or a word? (Avoids redacting `max_tokens=100000`.)
fn looks_secret(v: &str) -> bool {
    if v == REDACTED || v.starts_with('[') || v.starts_with('$') || v.starts_with('<') {
        return false;
    }
    let has_alpha = v.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = v.chars().any(|c| c.is_ascii_digit());
    (has_alpha && has_digit) || v.len() >= 24
}

/// Pattern-based redaction of things that look like credentials: `sk-ant-…`,
/// `sk-…`, `ghp_/gho_/ghs_/github_pat_…`, AWS `AKIA…`, Slack `xox?-…`,
/// `Bearer <token>`, PEM private-key blocks and `*_TOKEN=…`-style
/// assignments. Each is replaced with `[REDACTED]`.
pub fn scan_for_secrets(s: &str) -> Cow<'_, str> {
    let mut out: Cow<'_, str> = Cow::Borrowed(s);
    for p in PATTERNS.iter() {
        if !p.re.is_match(&out) {
            continue;
        }
        let replaced = if p.keep_prefix {
            p.re.replace_all(&out, |c: &Captures| format!("{}{REDACTED}", &c[1]))
                .into_owned()
        } else {
            p.re.replace_all(&out, REDACTED).into_owned()
        };
        out = Cow::Owned(replaced);
    }
    if let Some(re) = ASSIGNMENT.as_ref()
        && re.is_match(&out)
    {
        let replaced = re
            .replace_all(&out, |c: &Captures| {
                if looks_secret(&c[2]) {
                    format!("{}{REDACTED}", &c[1])
                } else {
                    c[0].to_string()
                }
            })
            .into_owned();
        if replaced != *out {
            out = Cow::Owned(replaced);
        }
    }
    out
}

/// Mask a secret for display: recognised prefix + `…` + last 4 characters
/// (`sk-ant-…3f9a`). Short secrets show only `…`.
pub fn mask(secret: &str) -> String {
    const PREFIXES: &[&str] = &[
        "sk-ant-",
        "sk-proj-",
        "sk-svcacct-",
        "sk-",
        "github_pat_",
        "ghp_",
        "gho_",
        "ghs_",
        "xoxb-",
        "xoxp-",
        "AKIA",
    ];
    let s = secret.trim();
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 12 {
        return "…".into();
    }
    let prefix = PREFIXES
        .iter()
        .find(|p| s.starts_with(*p))
        .copied()
        .unwrap_or("");
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{prefix}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redactor_exact_and_longest_first() {
        let r = Redactor::new(["abcdef123456", "abcdef123456789xyz", "tiny"]);
        assert_eq!(r.redact("k=abcdef123456789xyz end"), "k=[REDACTED] end");
        assert_eq!(r.redact("k=abcdef123456 end"), "k=[REDACTED] end");
        // Short "secrets" are ignored.
        assert_eq!(r.redact("tiny text"), "tiny text");
        assert!(matches!(r.redact("nothing here"), Cow::Borrowed(_)));
    }

    #[test]
    fn redactor_encoded_forms() {
        let secret = "s3cr3t/value+with=chars";
        let r = Redactor::new([secret]);
        let b64 = base64::engine::general_purpose::STANDARD.encode(secret);
        assert_eq!(r.redact(&format!("auth {b64}")), "auth [REDACTED]");
        assert_eq!(
            r.redact(&format!("url?k={}", percent_encode(secret))),
            "url?k=[REDACTED]"
        );
    }

    #[test]
    fn redactor_extend_and_empty() {
        let mut r = Redactor::default();
        assert!(r.is_empty());
        assert_eq!(r.redact("x"), "x");
        r.extend(["supersecretvalue"]);
        assert!(!r.is_empty());
        assert_eq!(r.redact("a supersecretvalue b"), "a [REDACTED] b");
        assert!(!format!("{r:?}").contains("supersecret"));
    }

    #[test]
    fn scan_patterns() {
        let cases = [
            (
                "key sk-ant-api03-AbCdEf0123456789xyz done",
                "key [REDACTED] done",
            ),
            (
                "OPENAI sk-proj-abcdefghijklmnop1234 x",
                "OPENAI [REDACTED] x",
            ),
            ("gh ghp_abcdefghijklmnopqrstuvwxyz0123 x", "gh [REDACTED] x"),
            (
                "gh github_pat_11ABCDEFG0123456789_abcdefgh x",
                "gh [REDACTED] x",
            ),
            ("aws AKIAIOSFODNN7EXAMPLE x", "aws [REDACTED] x"),
            ("slack xoxb-1234567890-abcdefghij x", "slack [REDACTED] x"),
            (
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abc.def",
                "Authorization: Bearer [REDACTED]",
            ),
            (
                "CLAUDE_CODE_OAUTH_TOKEN=abc123def456ghi",
                "CLAUDE_CODE_OAUTH_TOKEN=[REDACTED]",
            ),
            (
                r#"{"api_key": "Zz9yYy8xXx7wWw6"}"#,
                r#"{"api_key": "[REDACTED]"}"#,
            ),
            (
                "export DB_PASSWORD='hunter2hunter2x9'",
                "export DB_PASSWORD='[REDACTED]'",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(scan_for_secrets(input), want, "{input}");
        }
    }

    #[test]
    fn scan_pem_block() {
        let pem = "before\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nabc\n-----END RSA PRIVATE KEY-----\nafter";
        assert_eq!(scan_for_secrets(pem), "before\n[REDACTED]\nafter");
        let ossh = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl\n-----END OPENSSH PRIVATE KEY-----";
        assert_eq!(scan_for_secrets(ossh), REDACTED);
    }

    #[test]
    fn scan_leaves_ordinary_text_alone() {
        for s in [
            "max_tokens=100000",
            "tokens_in=1234 tokens_out=99",
            "the task-runner skipped it",
            "password policy: at least 10 characters",
            "TOKEN=$KILN_RUN_TOKEN",
            "sk-short",
            "cargo test --workspace",
        ] {
            assert!(
                matches!(scan_for_secrets(s), Cow::Borrowed(_)),
                "{s} → {}",
                scan_for_secrets(s)
            );
        }
    }

    #[test]
    fn scrub_combines_both() {
        let r = Redactor::new(["custom-secret-zzz"]);
        assert_eq!(
            r.scrub("a custom-secret-zzz b ghp_abcdefghijklmnopqrstuvwxyz0123"),
            "a [REDACTED] b [REDACTED]"
        );
    }

    #[test]
    fn masks() {
        assert_eq!(mask("sk-ant-api03-abcdefghij3f9a"), "sk-ant-…3f9a");
        assert_eq!(mask("sk-proj-abcdefghij1234"), "sk-proj-…1234");
        assert_eq!(mask("plainsecretvalue9876"), "…9876");
        assert_eq!(mask("short"), "…");
    }
}
