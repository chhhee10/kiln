//! Runner ↔ daemon protocol (PRD §7.7).
//!
//! Transport: the daemon creates a Unix socket per run at
//! `<run_dir>/kiln.sock`, mounted into the container at `/run/kiln.sock`.
//! Framing: one JSON object per line (UTF-8, `\n`-terminated).
//!
//! Handshake: the runner connects and sends `Hello` with the per-run token
//! from env `KILN_RUN_TOKEN`; the daemon replies `Welcome` with the
//! `RunConfig` or closes the socket. Every later message is unauthenticated
//! because the socket is private to the run.
//!
//! Durability: before sending, the runner appends each `RunEvent` to
//! `/run/spool.jsonl`. The daemon acknowledges with `Ack { seq }` after it
//! has persisted the event. On reconnect the runner resends everything after
//! the last ack; the daemon dedupes on `(run_id, seq)`.

use crate::event::RunEvent;
use crate::ids::*;
use crate::model::ArtifactKind;
use crate::plan::PlanSpec;
use crate::qa::{QaProfile, QaReport, ReviewVerdict};
use crate::status::WorkerRole;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PROTOCOL_VERSION: u32 = 1;

/// Paths inside the worker container.
pub mod paths {
    /// Run folder (bind mount of `<data>/runs/<run_id>`).
    pub const RUN_DIR: &str = "/run/kiln";
    pub const SOCKET: &str = "/run/kiln/kiln.sock";
    pub const SPOOL: &str = "/run/kiln/spool.jsonl";
    /// Structured outputs workers must write.
    pub const OUT_DIR: &str = "/run/kiln/out";
    pub const QA_REPORT: &str = "/run/kiln/out/qa_report.json";
    pub const VERDICT: &str = "/run/kiln/out/verdict.json";
    pub const PLAN: &str = "/run/kiln/out/plan.json";
    pub const SUMMARY: &str = "/run/kiln/out/summary.md";
    pub const ARTIFACTS_DIR: &str = "/run/kiln/out/artifacts";
    pub const TRANSCRIPTS_DIR: &str = "/run/kiln/transcripts";
    /// Task worktree.
    pub const WORKSPACE: &str = "/workspace";
    /// Env var carrying the per-run token.
    pub const ENV_TOKEN: &str = "KILN_RUN_TOKEN";
    pub const ENV_RUN_ID: &str = "KILN_RUN_ID";
    pub const ENV_SOCKET: &str = "KILN_SOCKET";
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // sent once per line; boxing buys nothing
pub enum RunnerMessage {
    Hello { run_id: RunId, token: String, runner_version: String, protocol: u32 },
    Event { event: RunEvent },
    /// A file under `/run/kiln/out/artifacts` is ready to index.
    Artifact { path: String, kind: ArtifactKind, caption: Option<String>, size: u64 },
    /// Memory MCP call proxied to the daemon's gateway.
    MemoryRequest { req_id: u64, call: MemoryCall },
    Heartbeat,
    Complete { outcome: RunOutcome },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    Welcome { config: Box<RunConfig> },
    Ack { seq: u64 },
    /// Stop the harness (SIGTERM, then SIGKILL after the grace period).
    Stop { reason: String },
    /// Human feedback to inject into a live Builder session, if the harness
    /// supports it. Otherwise the daemon restarts the run with feedback.
    Feedback { text: String },
    MemoryResponse { req_id: u64, result: Result<serde_json::Value, String> },
    Reject { reason: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum MemoryCall {
    SearchMemory { query: String, limit: Option<u32> },
    ProposeMemory { proposal: crate::memory::MemoryProposal },
}

/// Everything the runner needs to drive one harness session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunConfig {
    pub run_id: RunId,
    pub task_id: Option<TaskId>,
    pub task_key: Option<String>,
    pub role: WorkerRole,
    pub harness: String,
    pub model: String,
    /// Fully rendered prompt for this role (task, criteria, prior feedback).
    pub prompt: String,
    /// Role system instructions (appended to the harness's system prompt).
    pub system: String,
    pub workdir: String,
    /// Branch the Builder commits to; QA's scratch branch for its tests.
    pub branch: Option<String>,
    pub base_branch: String,
    pub attempt: u32,
    pub timeout_secs: u64,
    /// QA only.
    pub qa_profile: Option<QaProfile>,
    /// Extra harness-specific options from config (e.g. LiteLLM base URL key name).
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    /// Planner/Reviewer/QA run with a read-only workspace.
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_usd: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunOutcome {
    pub success: bool,
    pub summary: String,
    pub exit_code: Option<i32>,
    pub usage: Usage,
    /// Builder: HEAD of the task branch after the run.
    pub commit: Option<String>,
    pub qa_report: Option<QaReport>,
    pub verdict: Option<ReviewVerdict>,
    /// Planner: the proposed plan (validated by the daemon again).
    pub plan: Option<PlanSpec>,
    /// Planner: chat reply text.
    pub reply: Option<String>,
    pub error: Option<String>,
    /// Last spooled event sequence number.
    pub last_seq: u64,
}

impl RunOutcome {
    pub fn failed(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Self {
            success: false,
            summary: msg.clone(),
            exit_code: None,
            usage: Usage::default(),
            commit: None,
            qa_report: None,
            verdict: None,
            plan: None,
            reply: None,
            error: Some(msg),
            last_seq: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_roundtrip() {
        let m = RunnerMessage::MemoryRequest {
            req_id: 7,
            call: MemoryCall::SearchMemory { query: "retry".into(), limit: Some(5) },
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"type\":\"memory_request\""));
        assert!(s.contains("\"method\":\"search_memory\""));
        let back: RunnerMessage = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
        let d = DaemonMessage::MemoryResponse { req_id: 7, result: Err("nope".into()) };
        let back: DaemonMessage = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }
}
