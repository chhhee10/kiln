//! Claude Agent SDK adapter.
//!
//! Launches `node /opt/kiln/claude-entry.mjs` (see `images/worker/`), which
//! reads one JSON config line from stdin, runs `query()` from
//! `@anthropic-ai/claude-agent-sdk` with the Kiln memory MCP server attached,
//! and prints every SDK message as one JSON line on stdout. Further stdin
//! lines `{"type":"feedback","text":…}` are injected as user turns.

use crate::util::{self, MAX_INPUT_STRING_CHARS, MAX_OUTPUT_CHARS};
use crate::{Harness, Launch, MCP_ENV_VARS, Result, run_dir, runner_bin};
use kiln_types::event::{LogLevel, RunEventKind};
use kiln_types::protocol::RunConfig;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;

/// Default entry script inside the worker image.
pub const CLAUDE_ENTRY: &str = "/opt/kiln/claude-entry.mjs";

/// Tools a read-only role may not use (the workspace mount is RO anyway;
/// this keeps the agent from trying).
pub const READ_ONLY_DISALLOWED: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeSdk;

impl ClaudeSdk {
    /// The JSON config line the entry script reads from stdin.
    pub fn entry_config(cfg: &RunConfig) -> Value {
        let model = cfg.model.trim();
        let run = run_dir();
        let disallowed: Vec<&str> = if cfg.read_only {
            READ_ONLY_DISALLOWED.to_vec()
        } else {
            vec![]
        };
        json!({
            "runId": cfg.run_id,
            "role": cfg.role,
            "prompt": cfg.prompt,
            "systemAppend": cfg.system,
            "model": (!model.is_empty() && model != "default").then_some(model),
            "cwd": cfg.workdir,
            "permissionMode": util::opt(cfg, "claude.permission_mode").unwrap_or("bypassPermissions"),
            "disallowedTools": disallowed,
            "additionalDirectories": [run.join("out").to_string_lossy()],
            "maxTurns": util::opt(cfg, "claude.max_turns").and_then(|v| v.parse::<u32>().ok()),
            "mcp": {
                "command": runner_bin(),
                "args": ["mcp"],
                "envVars": MCP_ENV_VARS,
            },
        })
    }
}

fn level(s: Option<&str>) -> LogLevel {
    match s.unwrap_or("").to_ascii_uppercase().as_str() {
        "WARN" | "WARNING" => LogLevel::Warn,
        "FAIL" | "ERROR" => LogLevel::Fail,
        "STEP" => LogLevel::Step,
        "PASS" => LogLevel::Pass,
        "CAPT" => LogLevel::Capt,
        _ => LogLevel::Info,
    }
}

/// Assistant-message content blocks → events.
fn assistant_blocks(content: &Value, out: &mut Vec<RunEventKind>) {
    let Some(blocks) = content.as_array() else {
        if let Some(t) = content.as_str().filter(|t| !t.trim().is_empty()) {
            out.push(RunEventKind::Assistant {
                text: t.to_string(),
            });
        }
        return;
    };
    for b in blocks {
        match util::s(b, "type") {
            Some("text") => {
                if let Some(t) = util::s(b, "text").filter(|t| !t.trim().is_empty()) {
                    out.push(RunEventKind::Assistant {
                        text: t.to_string(),
                    });
                }
            }
            Some("thinking") => {
                if let Some(t) = util::s(b, "thinking").filter(|t| !t.trim().is_empty()) {
                    out.push(RunEventKind::Reasoning {
                        text: t.to_string(),
                    });
                }
            }
            Some("tool_use") | Some("server_tool_use") | Some("mcp_tool_use") => {
                out.push(RunEventKind::ToolCall {
                    id: util::s(b, "id").unwrap_or_default().to_string(),
                    name: util::s(b, "name").unwrap_or("tool").to_string(),
                    input: util::clamp_json(
                        b.get("input").unwrap_or(&Value::Null),
                        MAX_INPUT_STRING_CHARS,
                    ),
                })
            }
            _ => {}
        }
    }
}

/// User-message content blocks (tool results) → events.
fn user_blocks(content: &Value, out: &mut Vec<RunEventKind>) {
    let Some(blocks) = content.as_array() else {
        return;
    };
    for b in blocks {
        if util::s(b, "type") == Some("tool_result") {
            out.push(RunEventKind::ToolResult {
                id: util::s(b, "tool_use_id").unwrap_or_default().to_string(),
                ok: !b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                output: util::truncate(
                    &util::content_text(b.get("content").unwrap_or(&Value::Null)),
                    MAX_OUTPUT_CHARS,
                ),
            });
        }
    }
}

fn usage_of(u: &Value) -> (u64, u64) {
    let tin = util::u(u, "input_tokens")
        + util::u(u, "cache_creation_input_tokens")
        + util::u(u, "cache_read_input_tokens");
    (tin, util::u(u, "output_tokens"))
}

/// One SDK message → events.
pub(crate) fn parse_sdk_message(v: &Value) -> Vec<RunEventKind> {
    let mut out = Vec::new();
    match util::s(v, "type") {
        Some("system") => match util::s(v, "subtype") {
            Some("init") => {
                let tools = v.get("tools").and_then(Value::as_array).map_or(0, Vec::len);
                let model = util::s(v, "model").unwrap_or("?");
                let mut msg = format!("claude session started · model {model} · {tools} tools");
                if let Some(servers) = v.get("mcp_servers").and_then(Value::as_array) {
                    for srv in servers {
                        let name = util::s(srv, "name").unwrap_or("?");
                        let status = util::s(srv, "status").unwrap_or("?");
                        msg.push_str(&format!(" · mcp {name}: {status}"));
                        if status != "connected" {
                            out.push(RunEventKind::Log {
                                level: LogLevel::Warn,
                                message: format!("MCP server {name} is {status}"),
                            });
                        }
                    }
                }
                out.insert(
                    0,
                    RunEventKind::Log {
                        level: LogLevel::Info,
                        message: msg,
                    },
                );
            }
            Some("api_retry") => out.push(RunEventKind::Log {
                level: LogLevel::Warn,
                message: format!(
                    "API retry {}/{} (status {})",
                    util::u(v, "attempt"),
                    util::u(v, "max_retries"),
                    v.get("error_status")
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "?".into())
                ),
            }),
            Some("compact_boundary") => out.push(RunEventKind::Log {
                level: LogLevel::Info,
                message: "context compacted".into(),
            }),
            _ => {}
        },
        Some("assistant") => {
            let msg = v.get("message").unwrap_or(&Value::Null);
            assistant_blocks(msg.get("content").unwrap_or(&Value::Null), &mut out);
            if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                let e = err
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| err.to_string());
                out.push(RunEventKind::Error {
                    message: format!("assistant error: {e}"),
                });
            }
        }
        Some("user") => {
            let msg = v.get("message").unwrap_or(&Value::Null);
            user_blocks(msg.get("content").unwrap_or(&Value::Null), &mut out);
        }
        Some("result") => {
            let (tokens_in, tokens_out) = usage_of(v.get("usage").unwrap_or(&Value::Null));
            out.push(RunEventKind::Usage {
                tokens_in,
                tokens_out,
                cost_usd: util::f(v, "total_cost_usd"),
            });
            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let subtype = util::s(v, "subtype").unwrap_or("success");
            if is_error || subtype != "success" {
                let errors = v
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .filter(|s| !s.is_empty())
                    .or_else(|| util::s(v, "result").map(str::to_string))
                    .unwrap_or_default();
                let message = if errors.is_empty() {
                    format!("claude run ended: {subtype}")
                } else {
                    format!("claude run ended: {subtype}: {errors}")
                };
                out.push(RunEventKind::Error {
                    message: util::truncate(&message, MAX_OUTPUT_CHARS),
                });
            }
        }
        // Diagnostics printed by claude-entry.mjs itself.
        Some("kiln_log") => out.push(RunEventKind::Log {
            level: level(util::s(v, "level")),
            message: util::truncate(util::s(v, "message").unwrap_or(""), util::MAX_LOG_CHARS),
        }),
        Some("kiln_error") => out.push(RunEventKind::Error {
            message: util::truncate(
                util::s(v, "message").unwrap_or("claude entry failed"),
                MAX_OUTPUT_CHARS,
            ),
        }),
        _ => {}
    }
    out
}

impl Harness for ClaudeSdk {
    fn id(&self) -> &'static str {
        "claude_sdk"
    }

    fn prepare(&self, cfg: &RunConfig) -> Result<Launch> {
        let entry = util::opt(cfg, "claude.entry")
            .map(str::to_string)
            .or_else(|| {
                std::env::var("KILN_CLAUDE_ENTRY")
                    .ok()
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or_else(|| CLAUDE_ENTRY.to_string());
        let mut line = Self::entry_config(cfg).to_string();
        line.push('\n');
        Ok(Launch {
            program: util::opt(cfg, "claude.node").unwrap_or("node").to_string(),
            args: vec![entry],
            env: vec![],
            files: vec![],
            stdin: Some(line),
            live_input: true,
        })
    }

    fn parse_event(&self, line: &str) -> Vec<RunEventKind> {
        match util::json_object(line) {
            Some(v) => parse_sdk_message(&v),
            None => util::plain_line(line),
        }
    }

    fn parse_transcript(&self, path: &Path) -> Vec<RunEventKind> {
        let mut out = Vec::new();
        let (mut tin, mut tout) = (0u64, 0u64);
        for file in util::read_jsonl(path) {
            // Claude Code writes one line per content block; the usage is
            // repeated on each, so keep the last usage per message id.
            let mut usage: HashMap<String, (u64, u64)> = HashMap::new();
            let mut anon = (0u64, 0u64);
            for v in file {
                match util::s(&v, "type") {
                    Some("assistant") => {
                        let msg = v.get("message").unwrap_or(&Value::Null);
                        assistant_blocks(msg.get("content").unwrap_or(&Value::Null), &mut out);
                        if let Some(u) = msg.get("usage") {
                            let (i, o) = usage_of(u);
                            match util::s(msg, "id") {
                                Some(id) => {
                                    usage.insert(id.to_string(), (i, o));
                                }
                                None => {
                                    anon.0 += i;
                                    anon.1 += o;
                                }
                            }
                        }
                    }
                    Some("user") => {
                        let msg = v.get("message").unwrap_or(&Value::Null);
                        user_blocks(msg.get("content").unwrap_or(&Value::Null), &mut out);
                    }
                    _ => {}
                }
            }
            tin += anon.0 + usage.values().map(|u| u.0).sum::<u64>();
            tout += anon.1 + usage.values().map(|u| u.1).sum::<u64>();
        }
        if tin > 0 || tout > 0 {
            out.push(RunEventKind::Usage {
                tokens_in: tin,
                tokens_out: tout,
                cost_usd: 0.0,
            });
        }
        out
    }

    fn transcript_dir(&self) -> Option<&'static str> {
        Some("~/.claude/projects")
    }

    fn transcript_key(&self) -> Option<&'static str> {
        Some("claude")
    }

    fn feedback_input(&self, text: &str) -> Option<String> {
        Some(format!("{}\n", json!({"type": "feedback", "text": text})))
    }
}
