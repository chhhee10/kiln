//! OpenCode adapter (experimental): `opencode run --format json --auto`.
//!
//! The prompt goes to stdin (OpenCode appends piped stdin to the message).
//! The memory MCP server is configured through `OPENCODE_CONFIG_CONTENT`
//! (it inherits `KILN_SOCKET` / `KILN_RUN_TOKEN` from the runner's env).
//! The parser is tolerant: OpenCode's JSON events are
//! `{"type": "text" | "reasoning" | "tool_use" | "step_start" | "step_finish" | "error", "part": {…}}`.

use crate::util::{self, MAX_INPUT_STRING_CHARS, MAX_OUTPUT_CHARS};
use crate::{Harness, Launch, Result, runner_bin};
use kiln_types::event::RunEventKind;
use kiln_types::protocol::RunConfig;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCode;

fn error_message(e: &Value) -> String {
    e.get("data")
        .and_then(|d| util::s(d, "message"))
        .or_else(|| util::s(e, "message"))
        .or_else(|| util::s(e, "name"))
        .map(str::to_string)
        .or_else(|| e.as_str().map(str::to_string))
        .unwrap_or_else(|| "opencode error".into())
}

pub(crate) fn parse_opencode(v: &Value) -> Vec<RunEventKind> {
    let part = v.get("part").unwrap_or(&Value::Null);
    match util::s(v, "type") {
        Some("text") => util::s(part, "text")
            .filter(|t| !t.trim().is_empty())
            .map(|t| {
                vec![RunEventKind::Assistant {
                    text: t.to_string(),
                }]
            })
            .unwrap_or_default(),
        Some("reasoning") => util::s(part, "text")
            .filter(|t| !t.trim().is_empty())
            .map(|t| {
                vec![RunEventKind::Reasoning {
                    text: t.to_string(),
                }]
            })
            .unwrap_or_default(),
        Some("tool_use") | Some("tool") => {
            let state = part.get("state").unwrap_or(&Value::Null);
            let id = util::s(part, "callID")
                .or_else(|| util::s(part, "id"))
                .unwrap_or_default()
                .to_string();
            let name = util::s(part, "tool").unwrap_or("tool").to_string();
            let mut out = vec![RunEventKind::ToolCall {
                id: id.clone(),
                name,
                input: util::clamp_json(
                    state.get("input").unwrap_or(&Value::Null),
                    MAX_INPUT_STRING_CHARS,
                ),
            }];
            let status = util::s(state, "status").unwrap_or("completed");
            if matches!(status, "completed" | "error") {
                let ok = status == "completed";
                let output = if ok {
                    util::content_text(state.get("output").unwrap_or(&Value::Null))
                } else {
                    state.get("error").map(error_message).unwrap_or_default()
                };
                out.push(RunEventKind::ToolResult {
                    id,
                    ok,
                    output: util::truncate(&output, MAX_OUTPUT_CHARS),
                });
            }
            out
        }
        Some("step_finish") => {
            let t = part.get("tokens").unwrap_or(&Value::Null);
            let cache = t.get("cache").unwrap_or(&Value::Null);
            let tokens_in = util::u(t, "input") + util::u(cache, "read") + util::u(cache, "write");
            let tokens_out = util::u(t, "output") + util::u(t, "reasoning");
            let cost_usd = util::f(part, "cost");
            if tokens_in == 0 && tokens_out == 0 && cost_usd == 0.0 {
                vec![]
            } else {
                vec![RunEventKind::Usage {
                    tokens_in,
                    tokens_out,
                    cost_usd,
                }]
            }
        }
        Some("error") => {
            vec![RunEventKind::Error {
                message: error_message(v.get("error").unwrap_or(&Value::Null)),
            }]
        }
        _ => vec![],
    }
}

impl Harness for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }

    fn prepare(&self, cfg: &RunConfig) -> Result<Launch> {
        let mut args: Vec<String> = vec![
            "run".into(),
            "--format".into(),
            "json".into(),
            "--auto".into(),
        ];
        let model = cfg.model.trim();
        if !model.is_empty() && model != "default" {
            args.push("-m".into());
            args.push(model.into());
        }
        args.push("--dir".into());
        args.push(cfg.workdir.clone());
        let mut prompt = String::new();
        if !cfg.system.trim().is_empty() {
            prompt.push_str("<instructions>\n");
            prompt.push_str(&cfg.system);
            prompt.push_str("\n</instructions>\n\n");
        }
        prompt.push_str(&cfg.prompt);
        let config = json!({
            "$schema": "https://opencode.ai/config.json",
            "mcp": {
                "kiln": {
                    "type": "local",
                    "command": [runner_bin(), "mcp"],
                    "enabled": true,
                }
            },
        });
        Ok(Launch {
            program: util::opt(cfg, "opencode.bin")
                .unwrap_or("opencode")
                .to_string(),
            args,
            env: vec![("OPENCODE_CONFIG_CONTENT".into(), config.to_string())],
            files: vec![],
            stdin: Some(prompt),
            live_input: false,
        })
    }

    fn parse_event(&self, line: &str) -> Vec<RunEventKind> {
        match util::json_object(line) {
            Some(v) => parse_opencode(&v),
            None => util::plain_line(line),
        }
    }

    fn parse_transcript(&self, path: &Path) -> Vec<RunEventKind> {
        util::read_jsonl(path)
            .iter()
            .flatten()
            .flat_map(parse_opencode)
            .collect()
    }

    fn transcript_dir(&self) -> Option<&'static str> {
        None
    }
}
