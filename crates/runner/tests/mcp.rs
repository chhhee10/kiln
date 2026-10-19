//! `kiln-runner mcp` against the fake daemon over a real socket.

mod common;

use common::*;
use kiln_types::WorkerRole;
use kiln_types::protocol::MemoryCall;
use serde_json::{Value, json};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test(flavor = "multi_thread")]
async fn mcp_tools_round_trip_through_daemon() {
    let d = dirs();
    let cfg = config(WorkerRole::Builder, &d.ws);
    let daemon = FakeDaemon::start(&d.socket, cfg.clone(), Behavior::default());
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_kiln-runner"))
        .arg("mcp")
        .env("KILN_RUN_DIR", &d.run)
        .env("KILN_SOCKET", &d.socket)
        .env("KILN_RUN_TOKEN", TOKEN)
        .env("KILN_RUN_ID", cfg.run_id.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let reqs = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_memory","arguments":{"query":"retry","limit":3}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"propose_memory","arguments":{"kind":"convention","title":"Jitter","subject":"retries","relation":"use","object":"jittered backoff"}}}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"propose_memory","arguments":{"kind":"bogus"}}}),
    ];
    let mut replies: Vec<Value> = Vec::new();
    for r in &reqs {
        stdin.write_all(format!("{r}\n").as_bytes()).await.unwrap();
        if r.get("id").is_some() {
            let line = tokio::time::timeout(Duration::from_secs(10), out.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            replies.push(serde_json::from_str(&line).unwrap());
        }
    }
    stdin.write_all(b"not json\n").await.unwrap();
    let line = tokio::time::timeout(Duration::from_secs(10), out.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let parse_err: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(parse_err["error"]["code"], -32700);
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());

    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "kiln-memory");
    let names: Vec<_> = replies[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["search_memory", "propose_memory"]);
    assert_eq!(replies[2]["result"]["isError"], false);
    assert!(
        replies[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("about retry")
    );
    assert_eq!(replies[3]["result"]["structuredContent"]["title"], "Jitter");
    assert_eq!(replies[4]["result"]["isError"], true);

    let s = daemon.state.lock().unwrap();
    assert_eq!(s.hellos, 1, "one authenticated connection for all calls");
    assert_eq!(s.memory_calls.len(), 2);
    assert_eq!(
        s.memory_calls[0],
        MemoryCall::SearchMemory {
            query: "retry".into(),
            limit: Some(3)
        }
    );
}
