//! Adapter parsers against recorded sample output, plus garbage-input fuzzing.

use kiln_harness::*;
use kiln_types::event::LogLevel;
use kiln_types::protocol::RunConfig;
use kiln_types::{RunId, WorkerRole};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(fixture(name))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn stream(h: &dyn Harness, name: &str) -> Vec<RunEventKind> {
    lines(name).iter().flat_map(|l| h.parse_event(l)).collect()
}

fn cfg(role: WorkerRole, read_only: bool) -> RunConfig {
    RunConfig {
        run_id: RunId::new(),
        task_id: None,
        task_key: Some("FP-1".into()),
        role,
        harness: "x".into(),
        model: "some-model".into(),
        prompt: "Do the \"thing\"".into(),
        system: "Be careful.\nLine two \\ backslash".into(),
        workdir: "/workspace".into(),
        branch: None,
        base_branch: "main".into(),
        attempt: 1,
        timeout_secs: 60,
        qa_profile: None,
        options: Default::default(),
        read_only,
    }
}

fn usage_total(ev: &[RunEventKind]) -> (u64, u64, f64) {
    ev.iter().fold((0, 0, 0.0), |acc, e| match e {
        RunEventKind::Usage {
            tokens_in,
            tokens_out,
            cost_usd,
        } => (acc.0 + tokens_in, acc.1 + tokens_out, acc.2 + cost_usd),
        _ => acc,
    })
}

// ---------------------------------------------------------------- claude_sdk

#[test]
fn claude_stream_maps_messages() {
    let ev = stream(&ClaudeSdk, "claude_sdk_stream.jsonl");
    assert!(
        matches!(&ev[0], RunEventKind::Log { level: LogLevel::Info, message } if message.contains("claude-sonnet-4-5") && message.contains("kiln: connected"))
    );
    assert!(ev.contains(&RunEventKind::Reasoning {
        text: "I should look at the retry module first.".into()
    }));
    assert!(ev.contains(&RunEventKind::Assistant {
        text: "I'll start by reading the retry module.".into()
    }));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolCall { id, name, input } if id == "toolu_01XyZ" && name == "Read" && input["file_path"] == "/workspace/src/retry.rs")));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolResult { id, ok: true, output } if id == "toolu_01XyZ" && output.contains("backoff"))));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolResult { id, ok: false, output } if id == "toolu_02Bash" && output.contains("E0425"))));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::Log { level: LogLevel::Warn, message } if message.contains("API retry 1/10"))));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::Log { level: LogLevel::Warn, message } if message.contains("feedback received"))));
    let (tin, tout, cost) = usage_total(&ev);
    assert_eq!((tin, tout), (1200 + 5000 + 20000, 3400));
    assert!((cost - 0.1834).abs() < 1e-9);
    // stream_event / tool_progress are ignored, success emits no error.
    assert!(!ev.iter().any(|e| matches!(e, RunEventKind::Error { .. })));
}

#[test]
fn claude_errors() {
    let ev = stream(&ClaudeSdk, "claude_sdk_error_result.jsonl");
    let errors: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            RunEventKind::Error { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(errors[0].contains("authentication_failed"));
    assert!(errors[1].contains("error_max_turns") && errors[1].contains("maximum number of turns"));
    assert!(errors[2].contains("ENOENT"));
    assert_eq!(usage_total(&ev).2, 2.5);
}

#[test]
fn claude_transcript_dedupes_usage() {
    let ev = ClaudeSdk.parse_transcript(&fixture("claude_transcript.jsonl"));
    let (tin, tout, _) = usage_total(&ev);
    assert_eq!(tin, (2 + 24489 + 28082) + (10 + 100));
    assert_eq!(tout, 185 + 15);
    assert!(ev.contains(&RunEventKind::Assistant {
        text: "I'll take a look.".into()
    }));
    assert!(ev.contains(&RunEventKind::Assistant {
        text: "Done.".into()
    }));
    assert!(
        !ev.iter()
            .any(|e| matches!(e, RunEventKind::Assistant { text } if text.contains("SHOULD NOT")))
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, RunEventKind::ToolResult { id, .. } if id == "toolu_017Q"))
    );
    // Directory form.
    let d = tempfile::tempdir().unwrap();
    let sub = d.path().join("-workspace");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::copy(fixture("claude_transcript.jsonl"), sub.join("s.jsonl")).unwrap();
    std::fs::write(sub.join("ignored.txt"), "{}").unwrap();
    assert_eq!(ClaudeSdk.parse_transcript(d.path()), ev);
    assert!(
        ClaudeSdk
            .parse_transcript(Path::new("/nonexistent/kiln"))
            .is_empty()
    );
}

#[test]
fn claude_prepare() {
    let l = ClaudeSdk.prepare(&cfg(WorkerRole::Reviewer, true)).unwrap();
    assert_eq!(l.program, "node");
    assert_eq!(l.args, vec!["/opt/kiln/claude-entry.mjs".to_string()]);
    assert!(l.live_input);
    let stdin = l.stdin.unwrap();
    assert!(stdin.ends_with('\n') && stdin.trim().lines().count() == 1);
    let v: serde_json::Value = serde_json::from_str(stdin.trim()).unwrap();
    assert_eq!(v["permissionMode"], "bypassPermissions");
    assert_eq!(v["model"], "some-model");
    assert_eq!(v["cwd"], "/workspace");
    assert_eq!(v["prompt"], "Do the \"thing\"");
    assert!(
        v["disallowedTools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "Write")
    );
    assert_eq!(v["mcp"]["args"][0], "mcp");
    assert!(
        v["mcp"]["envVars"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "KILN_RUN_TOKEN")
    );
    let rw = ClaudeSdk.prepare(&cfg(WorkerRole::Builder, false)).unwrap();
    let v: serde_json::Value = serde_json::from_str(rw.stdin.unwrap().trim()).unwrap();
    assert!(v["disallowedTools"].as_array().unwrap().is_empty());
    let fb = ClaudeSdk.feedback_input("use \"jitter\"").unwrap();
    let v: serde_json::Value = serde_json::from_str(fb.trim()).unwrap();
    assert_eq!(v["type"], "feedback");
    assert_eq!(v["text"], "use \"jitter\"");
    assert_eq!(ClaudeSdk.transcript_dir(), Some("~/.claude/projects"));
    assert_eq!(ClaudeSdk.transcript_key(), Some("claude"));
}

// --------------------------------------------------------------------- codex

#[test]
fn codex_exec_events() {
    let ev = stream(&Codex, "codex_exec.jsonl");
    assert!(matches!(&ev[0], RunEventKind::Log { message, .. } if message.contains("0199a213")));
    assert!(
        ev.iter()
            .any(|e| matches!(e, RunEventKind::Reasoning { text } if text.contains("Inspecting")))
    );
    let cmds: Vec<_> = ev
        .iter()
        .filter(|e| matches!(e, RunEventKind::Command { .. }))
        .collect();
    assert_eq!(cmds.len(), 2, "started items must not duplicate commands");
    assert!(
        matches!(cmds[0], RunEventKind::Command { exit_code: Some(0), output, .. } if output.contains("retry.rs"))
    );
    assert!(matches!(
        cmds[1],
        RunEventKind::Command {
            exit_code: Some(101),
            ..
        }
    ));
    assert!(ev.contains(&RunEventKind::Progress {
        percent: 33,
        label: Some("Add jitter".into())
    }));
    assert!(ev.contains(&RunEventKind::FileChange {
        path: "/workspace/src/retry.rs".into(),
        change: "update".into()
    }));
    assert!(ev.contains(&RunEventKind::FileChange {
        path: "/workspace/src/jitter.rs".into(),
        change: "add".into()
    }));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolCall { id, name, input } if id == "item_5" && name == "kiln.search_memory" && input["query"] == "retry conventions")));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolResult { id, ok: true, output } if id == "item_5" && output.contains("Use jitter"))));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolResult { id, ok: false, output } if id == "item_6" && output == "daemon unreachable")));
    assert!(
        ev.iter()
            .any(|e| matches!(e, RunEventKind::ToolCall { name, .. } if name == "web_search"))
    );
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::Log { level: LogLevel::Warn, message } if message.contains("refused"))));
    assert!(ev.contains(&RunEventKind::Assistant {
        text: "Implemented jittered backoff and updated tests.".into()
    }));
    assert_eq!(usage_total(&ev), (24763, 122, 0.0));
    let errs = ev
        .iter()
        .filter(|e| matches!(e, RunEventKind::Error { .. }))
        .count();
    assert_eq!(errs, 2);
}

#[test]
fn codex_rollout_transcript() {
    let ev = Codex.parse_transcript(&fixture("codex_rollout.jsonl"));
    assert_eq!(
        usage_total(&ev),
        (20000, 500, 0.0),
        "last cumulative token_count wins"
    );
    assert!(ev.contains(&RunEventKind::Reasoning {
        text: "Planning the change".into()
    }));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolCall { id, name, input } if id == "call_A" && name == "mcp__kiln.search_memory" && input["query"] == "retry")));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolResult { id, output, .. } if id == "call_A" && output.contains("Wall time"))));
    assert!(ev.iter().any(
        |e| matches!(e, RunEventKind::ToolCall { id, name, .. } if id == "call_B" && name == "exec")
    ));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolResult { id, output, .. } if id == "call_B" && output.contains("ok"))));
    assert!(ev.contains(&RunEventKind::FileChange {
        path: "/workspace/src/retry.rs".into(),
        change: "update".into()
    }));
    let assistant: Vec<_> = ev
        .iter()
        .filter(|e| matches!(e, RunEventKind::Assistant { .. }))
        .collect();
    assert_eq!(
        assistant.len(),
        1,
        "developer/user messages and agent_message duplicates are skipped"
    );
}

#[test]
fn codex_prepare_args() {
    let mut c = cfg(WorkerRole::Builder, false);
    c.options
        .insert("litellm.base_url".into(), "http://litellm:4000/".into());
    c.options
        .insert("codex.reasoning_effort".into(), "high".into());
    let l = Codex.prepare(&c).unwrap();
    assert_eq!(l.program, "codex");
    assert!(!l.live_input);
    assert_eq!(l.stdin.as_deref(), Some("Do the \"thing\"\n"));
    let a = l.args;
    assert_eq!(&a[..2], &["exec".to_string(), "--json".to_string()]);
    assert_eq!(a.last().map(String::as_str), Some("-"));
    let has = |s: &str| a.iter().any(|x| x == s);
    assert!(has("--dangerously-bypass-approvals-and-sandbox"));
    assert!(has("--skip-git-repo-check"));
    assert!(has("/workspace"));
    assert!(has("some-model"));
    assert!(
        has("developer_instructions=\"Be careful.\\nLine two \\\\ backslash\""),
        "{a:?}"
    );
    assert!(has("model_reasoning_effort=\"high\""));
    assert!(has("mcp_servers.kiln.args=[\"mcp\"]"));
    assert!(
        a.iter()
            .any(|x| x.starts_with("mcp_servers.kiln.env_vars=[") && x.contains("\"KILN_SOCKET\""))
    );
    assert!(has("model_provider=\"kiln_litellm\""));
    assert!(has(
        "model_providers.kiln_litellm.base_url=\"http://litellm:4000/v1\""
    ));
    assert!(has("model_providers.kiln_litellm.wire_api=\"responses\""));
    // Every -c is followed by a value.
    for (i, x) in a.iter().enumerate() {
        if x == "-c" {
            assert!(
                a.get(i + 1).is_some_and(|v| v.contains('=')),
                "dangling -c at {i}"
            );
        }
    }
    let mut c = cfg(WorkerRole::Qa, true);
    c.options
        .insert("litellm.base_url".into(), "http://x/v1".into());
    c.options.insert("codex.provider".into(), "default".into());
    c.options
        .insert("codex.sandbox".into(), "workspace-write".into());
    let a = Codex.prepare(&c).unwrap().args;
    assert!(!a.iter().any(|x| x.contains("kiln_litellm")));
    assert!(
        a.windows(2)
            .any(|w| w[0] == "--sandbox" && w[1] == "workspace-write")
    );
    assert!(
        !a.iter()
            .any(|x| x == "--dangerously-bypass-approvals-and-sandbox")
    );
    assert_eq!(Codex.transcript_key(), Some("codex"));
    assert!(Codex.feedback_input("x").is_none());
}

// ------------------------------------------------------------------ opencode

#[test]
fn opencode_events() {
    let ev = stream(&OpenCode, "opencode.jsonl");
    assert!(ev.contains(&RunEventKind::Assistant {
        text: "Looking at the retry module.".into()
    }));
    assert!(ev.contains(&RunEventKind::Reasoning {
        text: "Need jitter.".into()
    }));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolCall { id, name, input } if id == "call_1" && name == "bash" && input["command"] == "ls")));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolResult { id, ok: true, output } if id == "call_1" && output.contains("Cargo.toml"))));
    assert!(ev.iter().any(|e| matches!(e, RunEventKind::ToolResult { id, ok: false, output } if id == "call_2" && output == "File not found")));
    let (tin, tout, cost) = usage_total(&ev);
    assert_eq!((tin, tout), (1300, 250));
    assert!((cost - 0.0123).abs() < 1e-9);
    assert!(ev.contains(&RunEventKind::Error {
        message: "Invalid API key".into()
    }));
    let l = OpenCode.prepare(&cfg(WorkerRole::Builder, false)).unwrap();
    assert_eq!(&l.args[..4], &["run", "--format", "json", "--auto"]);
    assert!(l.stdin.unwrap().contains("Be careful."));
    let conf = &l
        .env
        .iter()
        .find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT")
        .unwrap()
        .1;
    let v: serde_json::Value = serde_json::from_str(conf).unwrap();
    assert_eq!(v["mcp"]["kiln"]["command"][1], "mcp");
}

// ---------------------------------------------------------------------- mock

#[test]
fn mock_events_and_prepare() {
    let ev = stream(&Mock, "mock_stream.jsonl");
    assert_eq!(
        ev[0],
        RunEventKind::Step {
            title: "read task".into()
        }
    );
    assert_eq!(
        ev[3],
        RunEventKind::Progress {
            percent: 50,
            label: Some("halfway".into())
        }
    );
    assert_eq!(
        ev[5],
        RunEventKind::Log {
            level: LogLevel::Info,
            message: "not an event".into()
        }
    );
    assert_eq!(
        Mock.parse_transcript(&fixture("mock_stream.jsonl")).len(),
        5
    );
    let c = cfg(WorkerRole::Qa, true);
    let l = Mock.prepare(&c).unwrap();
    assert_eq!(l.args, vec!["mock-harness".to_string()]);
    let back: RunConfig = serde_json::from_str(l.stdin.unwrap().trim()).unwrap();
    assert_eq!(back, c);
    assert!(l.live_input);
}

// ------------------------------------------------------------------- garbage

/// Small deterministic PRNG (xorshift) so the fuzz corpus is reproducible.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn parsers_never_panic_on_garbage() {
    let reg = registry();
    let mut corpus: Vec<String> = vec![
        String::new(),
        " ".into(),
        "{".into(),
        "}".into(),
        "null".into(),
        "[]".into(),
        "{}".into(),
        "\u{0}\u{1}\u{7f}".into(),
        "{\"type\":null}".into(),
        "{\"type\":5}".into(),
        "{\"type\":\"assistant\",\"message\":5}".into(),
        "{\"type\":\"assistant\",\"message\":{\"content\":\"plain string\"}}".into(),
        "{\"type\":\"assistant\",\"message\":{\"content\":[1,null,{\"type\":\"tool_use\"}]}}".into(),
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"content\":{\"x\":1}}]}}".into(),
        "{\"type\":\"result\",\"usage\":\"x\",\"total_cost_usd\":\"NaN\"}".into(),
        "{\"type\":\"result\",\"usage\":{\"input_tokens\":-5,\"output_tokens\":1e300}}".into(),
        "{\"type\":\"system\",\"subtype\":\"init\",\"mcp_servers\":[5,{\"name\":1}]}".into(),
        "{\"type\":\"item.completed\",\"item\":null}".into(),
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"file_change\",\"changes\":\"x\"}}".into(),
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"exit_code\":99999999999}}".into(),
        "{\"type\":\"item.started\",\"item\":{\"type\":\"todo_list\",\"items\":[{},{\"completed\":\"yes\"}]}}".into(),
        "{\"type\":\"turn.completed\",\"usage\":[]}".into(),
        "{\"type\":\"tool_use\",\"part\":{\"state\":{\"status\":\"error\",\"error\":{\"data\":7}}}}".into(),
        "{\"type\":\"step_finish\",\"part\":{\"tokens\":{\"cache\":\"no\"}}}".into(),
        "{\"type\":\"log\",\"level\":\"NOPE\",\"message\":1}".into(),
        "{\"type\":\"progress\",\"percent\":300}".into(),
        "\"just a string\"".into(),
        "x".repeat(100_000),
        "é".repeat(10_000),
    ];
    // Every prefix of every fixture line, plus byte-level mutations.
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for f in [
        "claude_sdk_stream.jsonl",
        "claude_sdk_error_result.jsonl",
        "codex_exec.jsonl",
        "codex_rollout.jsonl",
        "opencode.jsonl",
        "mock_stream.jsonl",
    ] {
        for l in lines(f) {
            let b = l.as_bytes();
            for cut in (0..b.len()).step_by(7) {
                corpus.push(String::from_utf8_lossy(&b[..cut]).into_owned());
            }
            for _ in 0..20 {
                let mut m = b.to_vec();
                for _ in 0..3 {
                    if m.is_empty() {
                        break;
                    }
                    let i = (rng.next() as usize) % m.len();
                    m[i] = (rng.next() & 0xff) as u8;
                }
                corpus.push(String::from_utf8_lossy(&m).into_owned());
            }
        }
    }
    let mut total = 0usize;
    for id in reg.ids() {
        let h = reg.get(id).unwrap();
        for line in &corpus {
            total += h.parse_event(line).len();
        }
    }
    assert!(total > 0);
    // Garbage transcript files.
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.jsonl"), corpus.join("\n")).unwrap();
    std::fs::write(d.path().join("b.jsonl"), [0xffu8, 0xfe, b'\n', b'{']).unwrap();
    for id in reg.ids() {
        let _ = reg.get(id).unwrap().parse_transcript(d.path());
    }
}

#[test]
fn long_outputs_are_truncated() {
    let big = "y".repeat(100_000);
    let line = serde_json::json!({"type":"item.completed","item":{"id":"i","type":"command_execution","command":"cat big","aggregated_output":big,"exit_code":0,"status":"completed"}}).to_string();
    match &Codex.parse_event(&line)[0] {
        RunEventKind::Command { output, .. } => {
            assert!(output.len() < 20_000 && output.contains("more chars"))
        }
        other => panic!("{other:?}"),
    }
    let line = serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"t","name":"Write","input":{"file_path":"/a","content":"z".repeat(50_000)}}]}}).to_string();
    match &ClaudeSdk.parse_event(&line)[0] {
        RunEventKind::ToolCall { input, .. } => {
            assert!(input["content"].as_str().unwrap().len() < 10_000)
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn codex_openai_provider() {
    let mut c = cfg(WorkerRole::Builder, false);
    c.options.insert("codex.provider".into(), "openai".into());
    let a = Codex.prepare(&c).unwrap().args;
    assert!(
        a.iter().any(|x| x == "model_provider=\"kiln_openai\""),
        "{a:?}"
    );
    assert!(
        a.iter()
            .any(|x| x == "model_providers.kiln_openai.env_key=\"OPENAI_API_KEY\"")
    );
    assert!(
        a.iter()
            .any(|x| x == "model_providers.kiln_openai.base_url=\"https://api.openai.com/v1\"")
    );
}
