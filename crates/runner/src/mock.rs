//! `kiln-runner mock-harness`: deterministic stand-in for a real agent.
//!
//! Reads the [`RunConfig`] JSON from the first stdin line and prints
//! [`RunEventKind`] JSON lines. Later stdin lines `{"type":"feedback",…}` are
//! echoed as log events. No network, no model calls.
//!
//! Options (`RunConfig.options`):
//!
//! | key                        | default | effect                                                      |
//! |----------------------------|---------|-------------------------------------------------------------|
//! | `mock.sleep_ms`            | 50      | pause after each step                                       |
//! | `mock.steps`               | 3       | number of Step/Log/Progress/Command rounds                   |
//! | `mock.fail_qa_times`       | 0       | QA fails while `attempt <= n`                                |
//! | `mock.fail_review_times`   | 0       | Reviewer requests changes while `attempt <= n`               |
//! | `mock.exit_code`           | 0       | exit with this code after doing the work                     |
//! | `mock.hang`                | false   | after the first step, sleep forever (stop/timeout tests)     |
//! | `mock.ignore_sigterm`      | false   | ignore SIGTERM (SIGKILL escalation tests)                    |
//! | `mock.leave_uncommitted`   | false   | Builder edits but does not commit (runner must commit)       |
//! | `mock.no_output`           | false   | skip the role's output (Builder: no edit, QA: no report, …)  |
//!
//! Per role: the Builder appends a line to `KILN_MOCK.md` in the workspace
//! and commits; QA writes `out/qa_report.json` and a PNG screenshot in
//! `out/artifacts/`; the Reviewer writes `out/verdict.json`; the Planner
//! writes `out/plan.json` (two tasks, t2 depends on t1) and `out/reply.md`;
//! the Distiller writes `out/summary.md`.

use kiln_types::WorkerRole;
use kiln_types::event::{LogLevel, RunEventKind};
use kiln_types::plan::{PlanSpec, TaskSpec};
use kiln_types::protocol::RunConfig;
use kiln_types::qa::*;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A valid 1×1 PNG.
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0,
    0x1F, 0x00, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

struct Mock {
    cfg: RunConfig,
    run_dir: PathBuf,
    workspace: PathBuf,
    sleep: Duration,
    transcript: Option<std::fs::File>,
}

impl Mock {
    fn opt(&self, k: &str) -> Option<&str> {
        self.cfg.options.get(k).map(String::as_str)
    }
    fn num(&self, k: &str, d: u64) -> u64 {
        self.opt(k).and_then(|v| v.trim().parse().ok()).unwrap_or(d)
    }
    fn flag(&self, k: &str) -> bool {
        matches!(self.opt(k).map(str::trim), Some("1" | "true" | "yes"))
    }

    fn emit(&mut self, k: RunEventKind) {
        let line = serde_json::to_string(&k).unwrap_or_default();
        {
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
        if let Some(t) = self.transcript.as_mut() {
            let _ = writeln!(t, "{line}");
        }
    }

    fn log(&mut self, level: LogLevel, m: impl Into<String>) {
        self.emit(RunEventKind::Log {
            level,
            message: m.into(),
        });
    }

    fn pause(&self) {
        std::thread::sleep(self.sleep);
    }

    fn out(&self, name: &str) -> PathBuf {
        self.run_dir.join("out").join(name)
    }

    fn write_json<T: serde::Serialize>(&mut self, name: &str, v: &T) {
        let p = self.out(name);
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(Path::new(".")));
        match serde_json::to_vec_pretty(v).map(|b| std::fs::write(&p, b)) {
            Ok(Ok(())) => self.emit(RunEventKind::FileChange {
                path: p.to_string_lossy().into_owned(),
                change: "add".into(),
            }),
            _ => self.emit(RunEventKind::Error {
                message: format!("mock could not write {}", p.display()),
            }),
        }
    }

    fn git(&mut self, args: &[&str]) -> bool {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.workspace)
            .args(args)
            .env("GIT_AUTHOR_NAME", crate::collect::BUILDER_NAME)
            .env("GIT_AUTHOR_EMAIL", crate::collect::BUILDER_EMAIL)
            .env("GIT_COMMITTER_NAME", crate::collect::BUILDER_NAME)
            .env("GIT_COMMITTER_EMAIL", crate::collect::BUILDER_EMAIL)
            .output();
        let (code, output) = match out {
            Ok(o) => (
                o.status.code(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
            ),
            Err(e) => (None, e.to_string()),
        };
        self.emit(RunEventKind::Command {
            command: format!("git {}", args.join(" ")),
            exit_code: code,
            output: output.trim().to_string(),
        });
        code == Some(0)
    }

    fn builder(&mut self) {
        if self.flag("mock.no_output") {
            self.emit(RunEventKind::Assistant {
                text: "Nothing to change (mock).".into(),
            });
            return;
        }
        let path = self.workspace.join("KILN_MOCK.md");
        let key = self.cfg.task_key.clone().unwrap_or_else(|| "task".into());
        let line = format!("- {key} attempt {}\n", self.cfg.attempt);
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = res {
            self.emit(RunEventKind::Error {
                message: format!("cannot edit {}: {e}", path.display()),
            });
            return;
        }
        self.emit(RunEventKind::FileChange {
            path: "KILN_MOCK.md".into(),
            change: "update".into(),
        });
        if !self.flag("mock.leave_uncommitted") {
            let msg = format!("mock: {key} attempt {}", self.cfg.attempt);
            if self.git(&["add", "KILN_MOCK.md"]) {
                self.git(&["commit", "--no-verify", "-q", "-m", &msg]);
            }
        }
        self.emit(RunEventKind::Assistant {
            text: format!("Implemented {key} (mock attempt {}).", self.cfg.attempt),
        });
    }

    fn qa(&mut self) {
        let fail = (self.cfg.attempt as u64) <= self.num("mock.fail_qa_times", 0);
        let shot = format!("mock-screenshot-attempt-{}.png", self.cfg.attempt);
        let shot_path = self.out("artifacts").join(&shot);
        let _ = std::fs::create_dir_all(self.out("artifacts"));
        if std::fs::write(&shot_path, TINY_PNG).is_ok() {
            self.log(LogLevel::Capt, format!("screenshot {shot}"));
        }
        if self.flag("mock.no_output") {
            return;
        }
        let evidence = vec![format!("artifacts/{shot}")];
        let report = QaReport {
            criteria: vec![
                CriterionResult {
                    criterion: "builds cleanly".into(),
                    status: PassFail::Pass,
                    evidence: vec![],
                    notes: "mock".into(),
                },
                CriterionResult {
                    criterion: "behaves as described".into(),
                    status: if fail { PassFail::Fail } else { PassFail::Pass },
                    evidence,
                    notes: if fail {
                        "mock failure: filter resets on refresh".into()
                    } else {
                        "looks right".into()
                    },
                },
            ],
            tests: TestSummary {
                passed: if fail { 4 } else { 5 },
                failed: if fail { 1 } else { 0 },
                skipped: 0,
                running: 0,
                suites: vec![TestSuite {
                    name: "mock".into(),
                    passed: if fail { 4 } else { 5 },
                    failed: u32::from(fail),
                    duration_ms: 12,
                }],
                failing_assertion: fail.then(|| "expected filter=open, got filter=all".to_string()),
            },
            summary: if fail {
                "Mock QA failed".into()
            } else {
                "Mock QA passed".into()
            },
            test_branch: None,
        };
        self.write_json("qa_report.json", &report);
        self.log(
            if fail { LogLevel::Fail } else { LogLevel::Pass },
            report.summary.clone(),
        );
    }

    fn reviewer(&mut self) {
        if self.flag("mock.no_output") {
            return;
        }
        let changes = (self.cfg.attempt as u64) <= self.num("mock.fail_review_times", 0);
        let v = ReviewVerdict {
            decision: if changes {
                Decision::ChangesRequested
            } else {
                Decision::Approve
            },
            findings: if changes {
                vec![Finding {
                    severity: Severity::Major,
                    file: Some("KILN_MOCK.md".into()),
                    line: Some(1),
                    message: "mock: please add a test".into(),
                }]
            } else {
                vec![]
            },
            summary: if changes {
                "Mock review: changes requested".into()
            } else {
                "Mock review: approved".into()
            },
        };
        self.write_json("verdict.json", &v);
    }

    fn planner(&mut self) {
        let feature = mock_feature(&self.cfg.prompt);
        let plan = PlanSpec {
            feature: feature.clone(),
            description: format!("Mock plan for: {feature}"),
            tasks: vec![
                TaskSpec {
                    id: "t1".into(),
                    title: format!("Implement {feature}"),
                    description: "Core change.".into(),
                    acceptance_criteria: vec![format!("{feature} works end to end")],
                    files_hint: vec!["KILN_MOCK.md".into()],
                    depends_on: vec![],
                    workers: Default::default(),
                },
                TaskSpec {
                    id: "t2".into(),
                    title: format!("Test {feature}"),
                    description: "Tests for the change.".into(),
                    acceptance_criteria: vec!["tests cover the happy path".into()],
                    files_hint: vec![],
                    depends_on: vec!["t1".into()],
                    workers: Default::default(),
                },
            ],
        };
        let reply =
            format!("Here's a plan for **{feature}**: two tasks — implement it, then test it.");
        if !self.flag("mock.no_output") {
            self.write_json("plan.json", &plan);
            let _ = std::fs::write(self.out("reply.md"), &reply);
        }
        self.emit(RunEventKind::Assistant { text: reply });
    }

    fn distiller(&mut self) {
        let s = "- mock distilled fact: retries use jittered backoff\n";
        let _ = std::fs::create_dir_all(self.run_dir.join("out"));
        let _ = std::fs::write(self.out("summary.md"), s);
        self.emit(RunEventKind::Assistant {
            text: s.trim().to_string(),
        });
    }
}

/// Entry point; returns the process exit code.
pub fn main() -> i32 {
    let stdin = std::io::stdin();
    let mut first = String::new();
    if stdin.lock().read_line(&mut first).is_err() {
        return 2;
    }
    let cfg: RunConfig = match serde_json::from_str(first.trim()) {
        Ok(c) => c,
        Err(e) => {
            println!(
                "{}",
                serde_json::to_string(&RunEventKind::Error {
                    message: format!("mock: bad config line: {e}")
                })
                .unwrap_or_default()
            );
            return 2;
        }
    };
    let run_dir = std::env::var_os("KILN_RUN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(kiln_types::protocol::paths::RUN_DIR));
    let workspace = PathBuf::from(&cfg.workdir);
    let tdir = run_dir.join("transcripts").join("mock");
    let transcript = std::fs::create_dir_all(&tdir).ok().and_then(|_| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(tdir.join("session.jsonl"))
            .ok()
    });
    let mut m = Mock {
        sleep: Duration::ZERO,
        cfg,
        run_dir,
        workspace,
        transcript,
    };
    m.sleep = Duration::from_millis(m.num("mock.sleep_ms", 50));

    if m.flag("mock.ignore_sigterm") {
        // SAFETY: installing SIG_IGN has no preconditions.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
    }

    // Feedback reader.
    std::thread::spawn(|| {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let text = serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
                .unwrap_or(line);
            let ev = RunEventKind::Log {
                level: LogLevel::Info,
                message: format!("mock received feedback: {text}"),
            };
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "{}", serde_json::to_string(&ev).unwrap_or_default());
            let _ = out.flush();
        }
    });

    let role = m.cfg.role;
    let steps = m.num("mock.steps", 3).clamp(1, 1000);
    m.log(
        LogLevel::Info,
        format!(
            "mock {} attempt {} (model {})",
            role.as_str(),
            m.cfg.attempt,
            m.cfg.model
        ),
    );
    for i in 1..=steps {
        m.emit(RunEventKind::Step {
            title: format!("mock {} step {i}/{steps}", role.as_str()),
        });
        if i == 1 && m.flag("mock.hang") {
            m.log(LogLevel::Info, "mock hanging");
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
        m.emit(RunEventKind::Command {
            command: format!("echo step {i}"),
            exit_code: Some(0),
            output: format!("step {i}"),
        });
        m.emit(RunEventKind::Progress {
            percent: ((i * 90) / steps) as u8,
            label: Some(format!("step {i}")),
        });
        m.pause();
    }
    match role {
        WorkerRole::Builder => m.builder(),
        WorkerRole::Qa => m.qa(),
        WorkerRole::Reviewer => m.reviewer(),
        WorkerRole::Planner => m.planner(),
        WorkerRole::Distiller => m.distiller(),
    }
    m.emit(RunEventKind::Progress {
        percent: 100,
        label: Some("done".into()),
    });
    m.emit(RunEventKind::Usage {
        tokens_in: 100 * steps,
        tokens_out: 50 * steps,
        cost_usd: 0.0,
    });
    m.num("mock.exit_code", 0) as i32
}

/// Title for the mock plan: the latest user message in the planner prompt
/// (the line after the last `**User**…:` marker), else the first non-heading
/// line.
fn mock_feature(prompt: &str) -> String {
    let lines: Vec<&str> = prompt.lines().map(str::trim).collect();
    let after_user = lines
        .iter()
        .rposition(|l| l.starts_with("**User**"))
        .and_then(|i| lines[i + 1..].iter().find(|l| !l.is_empty()));
    after_user
        .or_else(|| lines.iter().find(|l| !l.is_empty() && !l.starts_with('#')))
        .map(|l| l.chars().take(80).collect::<String>())
        .unwrap_or_else(|| "Mock feature".into())
}

#[cfg(test)]
mod feature_tests {
    use super::mock_feature;

    #[test]
    fn takes_latest_user_message() {
        let p = "# Project: X\n\n# Conversation\n\n**User**:\nfirst idea\n\n**Planner (you)**:\nok\n\n**User** (dictated by voice):\nAdd a verbose flag\n";
        assert_eq!(mock_feature(p), "Add a verbose flag");
        assert_eq!(mock_feature("# Heading\nplain line"), "plain line");
        assert_eq!(mock_feature(""), "Mock feature");
    }
}
