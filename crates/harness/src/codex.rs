//! OpenAI Codex CLI adapter: `codex exec --json`.
//!
//! The container is the sandbox, so Codex's own sandbox/approvals are
//! bypassed (`--dangerously-bypass-approvals-and-sandbox`, the documented
//! mode for externally sandboxed environments); read-only roles are enforced
//! by the read-only workspace mount. The prompt is written to stdin (`-`).
//!
//! Model provider (option `codex.provider`, auto-detected when unset):
//! * `litellm` — when `LITELLM_BASE_URL` (or option `litellm.base_url`) is
//!   set: a `kiln_litellm` provider with `wire_api = "responses"` and the key
//!   from `LITELLM_API_KEY` (falling back to `OPENAI_API_KEY`);
//! * `openai` — when `OPENAI_API_KEY` is set: a `kiln_openai` provider for
//!   the OpenAI API (base URL overridable with `openai.base_url`);
//! * `default` — Codex's own configuration/login.
//!
//! Only env var *names* go on the command line, never key values.

use crate::util::{self, MAX_INPUT_STRING_CHARS, MAX_OUTPUT_CHARS};
use crate::{Harness, Launch, MCP_ENV_VARS, Result, runner_bin};
use kiln_types::event::{LogLevel, RunEventKind};
use kiln_types::protocol::RunConfig;
use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default)]
pub struct Codex;

const LITELLM_PROVIDER: &str = "kiln_litellm";
const OPENAI_PROVIDER: &str = "kiln_openai";

impl Codex {
    /// `codex exec` arguments for a run (pure apart from reading
    /// `LITELLM_BASE_URL` / `LITELLM_API_KEY` presence from the env).
    pub fn args(cfg: &RunConfig) -> Vec<String> {
        let mut a: Vec<String> = [
            "exec",
            "--json",
            "--color",
            "never",
            "--skip-git-repo-check",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        match util::opt(cfg, "codex.sandbox") {
            Some(mode) => {
                a.push("--sandbox".into());
                a.push(mode.into());
                a.push("-c".into());
                a.push("approval_policy=\"never\"".into());
            }
            None => a.push("--dangerously-bypass-approvals-and-sandbox".into()),
        }
        a.push("-C".into());
        a.push(cfg.workdir.clone());
        let model = cfg.model.trim();
        if !model.is_empty() && model != "default" {
            a.push("--model".into());
            a.push(model.into());
        }
        let mut c = |kv: String| {
            a.push("-c".into());
            a.push(kv);
        };
        if !cfg.system.trim().is_empty() {
            c(format!(
                "developer_instructions={}",
                util::toml_quote(&cfg.system)
            ));
        }
        if let Some(effort) = util::opt(cfg, "codex.reasoning_effort") {
            c(format!(
                "model_reasoning_effort={}",
                util::toml_quote(effort)
            ));
        }
        // Memory MCP server (the runner itself). Values of the env vars are
        // inherited by name, so no secret lands in argv.
        c(format!(
            "mcp_servers.kiln.command={}",
            util::toml_quote(&runner_bin())
        ));
        c("mcp_servers.kiln.args=[\"mcp\"]".into());
        c(format!(
            "mcp_servers.kiln.env_vars={}",
            util::toml_array(MCP_ENV_VARS)
        ));
        // Model provider: a LiteLLM gateway, the OpenAI API with a key from
        // the env, or Codex's default (login / auth.json).
        let base = util::opt(cfg, "litellm.base_url")
            .map(str::to_string)
            .or_else(|| {
                std::env::var("LITELLM_BASE_URL")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            });
        let has_openai_key = std::env::var_os("OPENAI_API_KEY").is_some_and(|v| !v.is_empty());
        let provider = match util::opt(cfg, "codex.provider") {
            Some(p) => p.to_string(),
            None if base.is_some() => "litellm".into(),
            None if has_openai_key => "openai".into(),
            None => "default".into(),
        };
        let mut provider_cfg = |id: &str, name: &str, base_url: &str, key_var: &str| {
            c(format!("model_provider={}", util::toml_quote(id)));
            c(format!(
                "model_providers.{id}.name={}",
                util::toml_quote(name)
            ));
            c(format!(
                "model_providers.{id}.base_url={}",
                util::toml_quote(base_url)
            ));
            c(format!(
                "model_providers.{id}.env_key={}",
                util::toml_quote(key_var)
            ));
            c(format!("model_providers.{id}.wire_api=\"responses\""));
        };
        match (provider.as_str(), base) {
            ("litellm", Some(base)) => {
                let base = base.trim().trim_end_matches('/');
                let base = if base.ends_with("/v1") {
                    base.to_string()
                } else {
                    format!("{base}/v1")
                };
                let key_var = if std::env::var_os("LITELLM_API_KEY").is_some() {
                    "LITELLM_API_KEY"
                } else {
                    "OPENAI_API_KEY"
                };
                provider_cfg(LITELLM_PROVIDER, "LiteLLM", &base, key_var);
            }
            ("openai", _) => {
                let base = util::opt(cfg, "openai.base_url").unwrap_or("https://api.openai.com/v1");
                provider_cfg(OPENAI_PROVIDER, "OpenAI", base, "OPENAI_API_KEY");
            }
            _ => {}
        }
        a.push("-".into());
        a
    }
}

fn item_events(phase: &str, item: &Value) -> Vec<RunEventKind> {
    let id = util::s(item, "id").unwrap_or_default().to_string();
    let completed = phase == "item.completed";
    match util::s(item, "type") {
        Some("agent_message") if completed => util::s(item, "text")
            .filter(|t| !t.trim().is_empty())
            .map(|t| {
                vec![RunEventKind::Assistant {
                    text: t.to_string(),
                }]
            })
            .unwrap_or_default(),
        Some("reasoning") if completed => util::s(item, "text")
            .filter(|t| !t.trim().is_empty())
            .map(|t| {
                vec![RunEventKind::Reasoning {
                    text: t.to_string(),
                }]
            })
            .unwrap_or_default(),
        Some("command_execution") if completed => vec![RunEventKind::Command {
            command: util::s(item, "command").unwrap_or_default().to_string(),
            exit_code: item
                .get("exit_code")
                .and_then(Value::as_i64)
                .and_then(|c| i32::try_from(c).ok()),
            output: util::truncate(
                util::s(item, "aggregated_output").unwrap_or(""),
                MAX_OUTPUT_CHARS,
            ),
        }],
        Some("file_change") if completed => {
            let failed = util::s(item, "status") == Some("failed");
            item.get("changes")
                .and_then(Value::as_array)
                .map(|cs| {
                    cs.iter()
                        .map(|ch| RunEventKind::FileChange {
                            path: util::s(ch, "path").unwrap_or("?").to_string(),
                            change: match (util::s(ch, "kind").unwrap_or("update"), failed) {
                                (k, true) => format!("{k} (failed)"),
                                (k, false) => k.to_string(),
                            },
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        Some("mcp_tool_call") => {
            if phase == "item.started" {
                let name = format!(
                    "{}.{}",
                    util::s(item, "server").unwrap_or("mcp"),
                    util::s(item, "tool").unwrap_or("tool")
                );
                vec![RunEventKind::ToolCall {
                    id,
                    name,
                    input: util::clamp_json(
                        item.get("arguments").unwrap_or(&Value::Null),
                        MAX_INPUT_STRING_CHARS,
                    ),
                }]
            } else if completed {
                let err = item
                    .get("error")
                    .and_then(|e| util::s(e, "message"))
                    .map(str::to_string);
                let ok = err.is_none() && util::s(item, "status") != Some("failed");
                let output = err.unwrap_or_else(|| {
                    item.get("result")
                        .map(|r| util::content_text(r.get("content").unwrap_or(&Value::Null)))
                        .unwrap_or_default()
                });
                vec![RunEventKind::ToolResult {
                    id,
                    ok,
                    output: util::truncate(&output, MAX_OUTPUT_CHARS),
                }]
            } else {
                vec![]
            }
        }
        Some("web_search") if phase == "item.started" => vec![RunEventKind::ToolCall {
            id,
            name: "web_search".into(),
            input: serde_json::json!({"query": util::s(item, "query").unwrap_or("")}),
        }],
        Some("todo_list") => {
            let items = item
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                return vec![];
            }
            let done = items
                .iter()
                .filter(|i| i.get("completed").and_then(Value::as_bool).unwrap_or(false))
                .count();
            let next = items
                .iter()
                .find(|i| !i.get("completed").and_then(Value::as_bool).unwrap_or(false))
                .and_then(|i| util::s(i, "text"))
                .map(str::to_string);
            vec![RunEventKind::Progress {
                percent: ((done * 100) / items.len()).min(100) as u8,
                label: next,
            }]
        }
        Some("error") if completed || phase == "item.started" => vec![RunEventKind::Log {
            level: LogLevel::Warn,
            message: util::truncate(
                util::s(item, "message").unwrap_or("error"),
                util::MAX_LOG_CHARS,
            ),
        }],
        _ => vec![],
    }
}

/// One `codex exec --json` event → events.
pub(crate) fn parse_exec_event(v: &Value) -> Vec<RunEventKind> {
    match util::s(v, "type") {
        Some("thread.started") => vec![RunEventKind::Log {
            level: LogLevel::Info,
            message: format!("codex thread {}", util::s(v, "thread_id").unwrap_or("?")),
        }],
        Some("turn.started") => vec![],
        Some("turn.completed") => {
            let u = v.get("usage").unwrap_or(&Value::Null);
            // `cached_input_tokens` is a subset of `input_tokens`.
            vec![RunEventKind::Usage {
                tokens_in: util::u(u, "input_tokens"),
                tokens_out: util::u(u, "output_tokens"),
                cost_usd: 0.0,
            }]
        }
        Some("turn.failed") => vec![RunEventKind::Error {
            message: v
                .get("error")
                .and_then(|e| util::s(e, "message"))
                .unwrap_or("turn failed")
                .to_string(),
        }],
        Some("error") => vec![RunEventKind::Error {
            message: util::s(v, "message").unwrap_or("error").to_string(),
        }],
        Some(p @ ("item.started" | "item.updated" | "item.completed")) => {
            v.get("item").map(|i| item_events(p, i)).unwrap_or_default()
        }
        _ => vec![],
    }
}

fn arguments(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => {
            serde_json::from_str::<Value>(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

/// One rollout line (`~/.codex/sessions/**/rollout-*.jsonl`) → events.
/// Returns the token totals separately (they are cumulative per session).
fn parse_rollout_line(v: &Value, out: &mut Vec<RunEventKind>) -> Option<(u64, u64)> {
    let p = v.get("payload")?;
    match (util::s(v, "type"), util::s(p, "type")) {
        (Some("response_item"), Some("message")) if util::s(p, "role") == Some("assistant") => {
            let text = util::content_text(p.get("content").unwrap_or(&Value::Null));
            if !text.trim().is_empty() {
                out.push(RunEventKind::Assistant { text });
            }
        }
        (Some("response_item"), Some("reasoning")) => {
            let text = util::content_text(p.get("summary").unwrap_or(&Value::Null));
            if !text.trim().is_empty() {
                out.push(RunEventKind::Reasoning { text });
            }
        }
        (Some("response_item"), Some(t @ ("function_call" | "custom_tool_call"))) => {
            let input = if t == "function_call" {
                arguments(p.get("arguments"))
            } else {
                arguments(p.get("input"))
            };
            let name = match (util::s(p, "namespace"), util::s(p, "name")) {
                (Some(ns), Some(n)) => format!("{ns}.{n}"),
                (None, Some(n)) => n.to_string(),
                _ => "tool".into(),
            };
            out.push(RunEventKind::ToolCall {
                id: util::s(p, "call_id").unwrap_or_default().to_string(),
                name,
                input: util::clamp_json(&input, MAX_INPUT_STRING_CHARS),
            });
        }
        (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
            let output = match p.get("output") {
                Some(Value::Object(o)) => {
                    o.get("content").map(util::content_text).unwrap_or_default()
                }
                Some(other) => util::content_text(other),
                None => String::new(),
            };
            out.push(RunEventKind::ToolResult {
                id: util::s(p, "call_id").unwrap_or_default().to_string(),
                ok: true,
                output: util::truncate(&output, MAX_OUTPUT_CHARS),
            });
        }
        (Some("response_item"), Some("local_shell_call")) => {
            let cmd = p
                .get("action")
                .and_then(|a| a.get("command"))
                .and_then(Value::as_array)
                .map(|c| {
                    c.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            out.push(RunEventKind::Command {
                command: cmd,
                exit_code: None,
                output: String::new(),
            });
        }
        (Some("event_msg"), Some("patch_apply_end")) => {
            if let Some(changes) = p.get("changes").and_then(Value::as_object) {
                for (path, ch) in changes {
                    out.push(RunEventKind::FileChange {
                        path: path.clone(),
                        change: util::s(ch, "type").unwrap_or("update").to_string(),
                    });
                }
            }
        }
        (Some("event_msg"), Some("error" | "stream_error")) => {
            out.push(RunEventKind::Error {
                message: util::s(p, "message").unwrap_or("error").to_string(),
            });
        }
        (Some("event_msg"), Some("token_count")) => {
            let t = p.get("info")?.get("total_token_usage")?;
            return Some((util::u(t, "input_tokens"), util::u(t, "output_tokens")));
        }
        _ => {}
    }
    None
}

impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn prepare(&self, cfg: &RunConfig) -> Result<Launch> {
        let mut prompt = cfg.prompt.clone();
        if !prompt.ends_with('\n') {
            prompt.push('\n');
        }
        Ok(Launch {
            program: util::opt(cfg, "codex.bin").unwrap_or("codex").to_string(),
            args: Self::args(cfg),
            env: vec![("NO_COLOR".into(), "1".into())],
            files: vec![],
            stdin: Some(prompt),
            live_input: false,
        })
    }

    fn parse_event(&self, line: &str) -> Vec<RunEventKind> {
        match util::json_object(line) {
            Some(v) => parse_exec_event(&v),
            None => util::plain_line(line),
        }
    }

    fn parse_transcript(&self, path: &Path) -> Vec<RunEventKind> {
        let mut out = Vec::new();
        let (mut tin, mut tout) = (0u64, 0u64);
        for file in util::read_jsonl(path) {
            let mut last = (0u64, 0u64);
            for v in &file {
                if let Some(t) = parse_rollout_line(v, &mut out) {
                    last = t;
                }
            }
            tin += last.0;
            tout += last.1;
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
        Some("~/.codex/sessions")
    }

    fn transcript_key(&self) -> Option<&'static str> {
        Some("codex")
    }
}
