//! Harness adapters (PRD §7.8, ARCHITECTURE.md "harness").
//!
//! A [`Harness`] knows how to launch one agent CLI/SDK inside the worker
//! container and how to normalize its output into [`RunEventKind`]s:
//!
//! * [`ClaudeSdk`] (`claude_sdk`): `node /opt/kiln/claude-entry.mjs`, a small
//!   wrapper around `@anthropic-ai/claude-agent-sdk` that prints every SDK
//!   message as one JSON line.
//! * [`Codex`] (`codex`): `codex exec --json`.
//! * [`OpenCode`] (`opencode`, experimental): `opencode run --format json`.
//! * [`Mock`] (`mock`): `kiln-runner mock-harness`, deterministic, no model
//!   calls; what every pipeline test uses.
//!
//! Parsers are total: any input line (garbage, truncated JSON, unknown event
//! types) yields zero or more events and never panics.
//!
//! Secrets: adapters never copy credential values into [`Launch`]; harness
//! processes inherit the container env the daemon injected.

mod claude_sdk;
mod codex;
mod mock;
mod opencode;
pub mod util;

pub use claude_sdk::ClaudeSdk;
pub use codex::Codex;
pub use kiln_types::event::RunEventKind;
pub use kiln_types::protocol::RunConfig;
pub use mock::Mock;
pub use opencode::OpenCode;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type Result<T, E = HarnessError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error("unknown harness {0:?}")]
    Unknown(String),
    #[error("invalid run config: {0}")]
    InvalidConfig(String),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// How to start a harness process.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    /// Extra env on top of the runner's own env. Never contains secrets.
    pub env: Vec<(String, String)>,
    /// Files to write before launch. Relative paths are resolved against the
    /// run directory (`/run/kiln`).
    pub files: Vec<(PathBuf, String)>,
    /// Written to the harness's stdin right after spawn.
    pub stdin: Option<String>,
    /// Keep stdin open after `stdin` was written so live feedback can be
    /// forwarded with [`Harness::feedback_input`]. When false the runner
    /// closes stdin (harnesses like `codex exec` read it to EOF).
    pub live_input: bool,
}

pub trait Harness: Send + Sync {
    /// Registry id ("claude_sdk", "codex", "opencode", "mock").
    fn id(&self) -> &'static str;
    /// Build the launch for one run.
    fn prepare(&self, cfg: &RunConfig) -> Result<Launch>;
    /// Normalize one stdout line (0..n events). Must never panic.
    fn parse_event(&self, line: &str) -> Vec<RunEventKind>;
    /// Post-run ingest of the native transcript. `path` may be a file or a
    /// directory (searched recursively for `*.jsonl`). Missing paths yield
    /// no events.
    fn parse_transcript(&self, path: &Path) -> Vec<RunEventKind>;
    /// The harness's native transcript dir inside the container
    /// ("~/.claude/projects" | "~/.codex/sessions").
    fn transcript_dir(&self) -> Option<&'static str>;
    /// Subdirectory of `<run_dir>/transcripts/` the native transcript dir is
    /// bind-mounted to (matches `kiln_sandbox::TRANSCRIPT_DIRS`).
    fn transcript_key(&self) -> Option<&'static str> {
        None
    }
    /// The stdin line(s) that deliver live human feedback, when the harness
    /// supports it (requires [`Launch::live_input`]).
    fn feedback_input(&self, _text: &str) -> Option<String> {
        None
    }
}

/// Harnesses by id.
#[derive(Clone, Default)]
pub struct Registry {
    map: BTreeMap<&'static str, Arc<dyn Harness>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.map.keys()).finish()
    }
}

impl Registry {
    pub fn empty() -> Self {
        Self::default()
    }
    pub fn register(&mut self, h: Arc<dyn Harness>) {
        self.map.insert(h.id(), h);
    }
    pub fn get(&self, id: &str) -> Option<Arc<dyn Harness>> {
        self.map.get(id).cloned()
    }
    /// Like [`Registry::get`] but with a typed error.
    pub fn require(&self, id: &str) -> Result<Arc<dyn Harness>> {
        self.get(id)
            .ok_or_else(|| HarnessError::Unknown(id.to_string()))
    }
    pub fn ids(&self) -> Vec<&'static str> {
        self.map.keys().copied().collect()
    }
}

/// All built-in adapters.
pub fn registry() -> Registry {
    let mut r = Registry::empty();
    r.register(Arc::new(ClaudeSdk));
    r.register(Arc::new(Codex));
    r.register(Arc::new(OpenCode));
    r.register(Arc::new(Mock));
    r
}

/// Path of the `kiln-runner` binary (for `mcp` / `mock-harness`):
/// `$KILN_RUNNER_BIN`, else the current executable when it *is* the runner,
/// else `kiln-runner` from `PATH`.
pub fn runner_bin() -> String {
    if let Ok(b) = std::env::var("KILN_RUNNER_BIN")
        && !b.trim().is_empty()
    {
        return b;
    }
    if let Ok(exe) = std::env::current_exe()
        && exe
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("kiln-runner"))
    {
        return exe.to_string_lossy().into_owned();
    }
    "kiln-runner".into()
}

/// Effective run dir (`$KILN_RUN_DIR`, default `/run/kiln`).
pub fn run_dir() -> PathBuf {
    std::env::var_os("KILN_RUN_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(kiln_types::protocol::paths::RUN_DIR))
}

/// Env vars the memory MCP subprocess needs (names only; values are inherited).
pub const MCP_ENV_VARS: &[&str] = &[
    "KILN_SOCKET",
    "KILN_RUN_TOKEN",
    "KILN_RUN_ID",
    "KILN_RUN_DIR",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_all() {
        let r = registry();
        assert_eq!(r.ids(), vec!["claude_sdk", "codex", "mock", "opencode"]);
        assert!(r.get("nope").is_none());
        assert!(matches!(r.require("nope"), Err(HarnessError::Unknown(_))));
        for id in r.ids() {
            assert_eq!(r.get(id).unwrap().id(), id);
        }
    }
}
