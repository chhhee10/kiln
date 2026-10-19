//! Deterministic mock harness: `kiln-runner mock-harness`.
//!
//! The runner writes the full [`RunConfig`] as the first stdin line; the mock
//! prints [`RunEventKind`] JSON lines on stdout. Behaviour is driven by
//! `RunConfig.options` (`mock.sleep_ms`, `mock.fail_qa_times`,
//! `mock.fail_review_times`, …); see `kiln_runner::mock`.

use crate::util;
use crate::{Harness, Launch, Result, runner_bin};
use kiln_types::event::RunEventKind;
use kiln_types::protocol::RunConfig;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default)]
pub struct Mock;

impl Harness for Mock {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn prepare(&self, cfg: &RunConfig) -> Result<Launch> {
        let mut line = serde_json::to_string(cfg)
            .map_err(|e| crate::HarnessError::InvalidConfig(e.to_string()))?;
        line.push('\n');
        Ok(Launch {
            program: runner_bin(),
            args: vec!["mock-harness".into()],
            env: vec![],
            files: vec![],
            stdin: Some(line),
            live_input: true,
        })
    }

    fn parse_event(&self, line: &str) -> Vec<RunEventKind> {
        let t = line.trim();
        if t.is_empty() {
            return vec![];
        }
        match serde_json::from_str::<RunEventKind>(t) {
            Ok(k) => vec![k],
            Err(_) => util::plain_line(t),
        }
    }

    fn parse_transcript(&self, path: &Path) -> Vec<RunEventKind> {
        util::read_jsonl(path)
            .into_iter()
            .flatten()
            .filter_map(|v| serde_json::from_value::<RunEventKind>(v).ok())
            .collect()
    }

    fn transcript_dir(&self) -> Option<&'static str> {
        None
    }

    fn transcript_key(&self) -> Option<&'static str> {
        Some("mock")
    }

    fn feedback_input(&self, text: &str) -> Option<String> {
        Some(format!(
            "{}\n",
            serde_json::json!({"type": "feedback", "text": text})
        ))
    }
}
