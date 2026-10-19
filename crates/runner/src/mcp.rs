//! `kiln-runner mcp`: minimal MCP stdio server for the company brain.
//!
//! JSON-RPC 2.0, one message per line (MCP stdio transport). Implements
//! `initialize`, `ping`, `tools/list` and `tools/call` for two tools,
//! `search_memory` and `propose_memory`, which are forwarded to the daemon's
//! memory gateway as [`RunnerMessage::MemoryRequest`] over a separate,
//! authenticated socket connection (same token as the run). The daemon
//! enforces visibility; nothing here trusts the agent's arguments beyond
//! basic shape checks.

use crate::env::RunnerEnv;
use crate::lines::{LineReader, write_json};
use crate::uplink::{self, Conn};
use kiln_types::memory::{MemoryProposal, MemoryType, Visibility};
use kiln_types::protocol::{DaemonMessage, MemoryCall, RunnerMessage};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::BufReader;

pub const SERVER_NAME: &str = "kiln-memory";
const DEFAULT_PROTOCOL: &str = "2025-06-18";
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The tools this server exposes.
pub fn tools() -> Value {
    json!([
        {
            "name": "search_memory",
            "description": "Search the Kiln company brain (facts, conventions, decisions, procedures) visible to the run's owner. Use before making assumptions about project conventions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to look for."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Max results (default 8)."}
                },
                "required": ["query"],
                "additionalProperties": false
            }
        },
        {
            "name": "propose_memory",
            "description": "Propose a durable piece of knowledge learned during this run. Private items are saved directly; shared/company items go to the owner's approval inbox.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["fact", "convention", "decision", "procedure", "preference"]},
                    "title": {"type": "string"},
                    "subject": {"type": "string"},
                    "relation": {"type": "string"},
                    "object": {"type": "string"},
                    "body": {"type": "string"},
                    "visibility": {"type": "string", "enum": ["private", "shared", "company"], "default": "private"},
                    "shared_with": {"type": "array", "items": {"type": "string"}},
                    "project_id": {"type": "string"}
                },
                "required": ["kind", "title", "subject", "relation", "object"],
                "additionalProperties": false
            }
        }
    ])
}

/// Parse `tools/call` arguments into a gateway call.
pub fn to_call(name: &str, args: &Value) -> Result<MemoryCall, String> {
    let str_arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    match name {
        "search_memory" => {
            let query = str_arg("query").ok_or("`query` is required")?.to_string();
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|l| l.clamp(1, 50) as u32);
            Ok(MemoryCall::SearchMemory { query, limit })
        }
        "propose_memory" => {
            let kind = str_arg("kind")
                .and_then(MemoryType::parse)
                .filter(|k| *k != MemoryType::Episode)
                .ok_or("`kind` must be one of fact, convention, decision, procedure, preference")?;
            let visibility = match str_arg("visibility") {
                None => Visibility::Private,
                Some(v) => {
                    Visibility::parse(v).ok_or("`visibility` must be private, shared or company")?
                }
            };
            let req = |k: &str| {
                str_arg(k)
                    .map(str::to_string)
                    .ok_or(format!("`{k}` is required"))
            };
            let shared_with = match args.get("shared_with") {
                None | Some(Value::Null) => vec![],
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .and_then(|s| s.parse().ok())
                            .ok_or("`shared_with` must be user ids".to_string())
                    })
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err("`shared_with` must be an array".into()),
            };
            let project_id = match str_arg("project_id") {
                None => None,
                Some(p) => Some(
                    p.parse()
                        .map_err(|_| "`project_id` must be a uuid".to_string())?,
                ),
            };
            Ok(MemoryCall::ProposeMemory {
                proposal: MemoryProposal {
                    kind,
                    title: req("title")?,
                    subject: req("subject")?,
                    relation: req("relation")?,
                    object: req("object")?,
                    body: str_arg("body").unwrap_or_default().to_string(),
                    visibility,
                    shared_with,
                    project_id,
                },
            })
        }
        other => Err(format!("unknown tool {other:?}")),
    }
}

/// Lazily connected client for the daemon's memory gateway.
pub struct MemoryClient {
    env: Option<RunnerEnv>,
    conn: Option<Conn>,
    next_id: u64,
}

impl MemoryClient {
    pub fn new(env: Option<RunnerEnv>) -> Self {
        Self {
            env,
            conn: None,
            next_id: 1,
        }
    }

    async fn call_once(&mut self, call: &MemoryCall) -> Result<Value, String> {
        let env = self
            .env
            .as_ref()
            .ok_or("runner environment (KILN_RUN_TOKEN/KILN_RUN_ID) not available")?;
        if self.conn.is_none() {
            let (conn, _) = uplink::handshake(env)
                .await
                .map_err(|e| format!("memory gateway unreachable: {e}"))?;
            self.conn = Some(conn);
        }
        let conn = self.conn.as_mut().ok_or("not connected")?;
        let req_id = self.next_id;
        self.next_id += 1;
        conn.send(&RunnerMessage::MemoryRequest {
            req_id,
            call: call.clone(),
        })
        .await
        .map_err(|e| e.to_string())?;
        loop {
            let line = conn
                .reader
                .next_line()
                .await
                .map_err(|e| e.to_string())?
                .ok_or("daemon closed the connection")?;
            match serde_json::from_str::<DaemonMessage>(&line) {
                Ok(DaemonMessage::MemoryResponse { req_id: id, result }) if id == req_id => {
                    return result;
                }
                Ok(DaemonMessage::Reject { reason }) => return Err(format!("rejected: {reason}")),
                _ => continue,
            }
        }
    }

    /// Forward one call; reconnects once on a transport failure.
    pub async fn call(&mut self, call: &MemoryCall) -> Result<Value, String> {
        for attempt in 0..2 {
            match tokio::time::timeout(CALL_TIMEOUT, self.call_once(call)).await {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(e)) if attempt == 0 && self.conn.is_some() => {
                    tracing::warn!(error = %e, "memory call failed; reconnecting");
                    self.conn = None;
                }
                Ok(Err(e)) => {
                    self.conn = None;
                    return Err(e);
                }
                Err(_) => {
                    self.conn = None;
                    return Err("memory gateway timed out".into());
                }
            }
        }
        Err("memory gateway unavailable".into())
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Handle one JSON-RPC message; `None` for notifications.
pub async fn handle(msg: &Value, client: &mut MemoryClient) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str);
    let Some(method) = method else {
        // A response or garbage object: only answer if it has an id.
        return id.map(|id| rpc_error(&id, -32600, "invalid request"));
    };
    let id = id?; // notifications get no response
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let proto = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL);
            rpc_result(
                &id,
                json!({
                    "protocolVersion": proto,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Kiln company brain: search_memory before assuming conventions; propose_memory for durable lessons."
                }),
            )
        }
        "ping" => rpc_result(&id, json!({})),
        "tools/list" => rpc_result(&id, json!({"tools": tools()})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let res = match to_call(name, &args) {
                Ok(call) => client.call(&call).await,
                Err(e) => Err(e),
            };
            match res {
                Ok(v) => rpc_result(
                    &id,
                    json!({
                        "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
                        "structuredContent": if v.is_object() { v.clone() } else { json!({"result": v}) },
                        "isError": false
                    }),
                ),
                Err(e) => rpc_result(
                    &id,
                    json!({"content": [{"type": "text", "text": e}], "isError": true}),
                ),
            }
        }
        "resources/list" => rpc_result(&id, json!({"resources": []})),
        "prompts/list" => rpc_result(&id, json!({"prompts": []})),
        _ => rpc_error(&id, -32601, &format!("method not found: {method}")),
    })
}

/// Entry point for `kiln-runner mcp`.
pub async fn serve() -> i32 {
    let env = RunnerEnv::from_env()
        .map_err(|e| tracing::warn!(error = %e, "memory MCP without daemon access"))
        .ok();
    let mut client = MemoryClient::new(env);
    let mut stdin = LineReader::new(BufReader::new(tokio::io::stdin()));
    let mut stdout = tokio::io::stdout();
    loop {
        let line = match stdin.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => return 0,
            Err(e) => {
                tracing::error!(error = %e, "stdin read failed");
                return 1;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let mut out = Vec::new();
                for m in &batch {
                    if let Some(r) = handle(m, &mut client).await {
                        out.push(r);
                    }
                }
                (!out.is_empty()).then_some(Value::Array(out))
            }
            Ok(msg @ Value::Object(_)) => handle(&msg, &mut client).await,
            Ok(_) => Some(rpc_error(&Value::Null, -32600, "invalid request")),
            Err(_) => Some(rpc_error(&Value::Null, -32700, "parse error")),
        };
        if let Some(r) = reply
            && write_json(&mut stdout, &r).await.is_err()
        {
            return 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_parsing() {
        assert_eq!(
            to_call("search_memory", &json!({"query": " retry ", "limit": 500})).unwrap(),
            MemoryCall::SearchMemory {
                query: "retry".into(),
                limit: Some(50)
            }
        );
        assert!(to_call("search_memory", &json!({})).is_err());
        let c = to_call(
            "propose_memory",
            &json!({"kind": "convention", "title": "t", "subject": "s", "relation": "r", "object": "o"}),
        )
        .unwrap();
        match c {
            MemoryCall::ProposeMemory { proposal } => {
                assert_eq!(proposal.visibility, Visibility::Private);
                assert_eq!(proposal.kind, MemoryType::Convention);
            }
            other => panic!("{other:?}"),
        }
        assert!(to_call("propose_memory", &json!({"kind": "episode", "title": "t", "subject": "s", "relation": "r", "object": "o"})).is_err());
        assert!(
            to_call(
                "propose_memory",
                &json!({"kind": "fact", "title": "t", "subject": "s", "relation": "r"})
            )
            .is_err()
        );
        assert!(to_call("propose_memory", &json!({"kind": "fact", "title": "t", "subject": "s", "relation": "r", "object": "o", "shared_with": ["nope"]})).is_err());
        assert!(to_call("nope", &json!({})).is_err());
    }

    #[tokio::test]
    async fn protocol_basics() {
        let mut c = MemoryClient::new(None);
        let r = handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}), &mut c).await.unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert!(
            handle(
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                &mut c
            )
            .await
            .is_none()
        );
        let r = handle(
            &json!({"jsonrpc":"2.0","id":"a","method":"tools/list"}),
            &mut c,
        )
        .await
        .unwrap();
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 2);
        let r = handle(&json!({"jsonrpc":"2.0","id":2,"method":"bogus"}), &mut c)
            .await
            .unwrap();
        assert_eq!(r["error"]["code"], -32601);
        // No daemon env: tool errors are reported as tool results, not RPC errors.
        let r = handle(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_memory","arguments":{"query":"x"}}}), &mut c).await.unwrap();
        assert_eq!(r["result"]["isError"], true);
    }
}
