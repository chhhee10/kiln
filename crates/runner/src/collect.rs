//! Post-run collection: role outputs, Builder commit, artifacts → `RunOutcome`.

use crate::emit::Emitter;
use crate::env::RunnerEnv;
use kiln_harness::Harness;
use kiln_types::WorkerRole;
use kiln_types::event::{LogLevel, RunEventKind};
use kiln_types::model::ArtifactKind;
use kiln_types::plan::PlanSpec;
use kiln_types::protocol::{RunConfig, RunOutcome, RunnerMessage, Usage};
use kiln_types::qa::{QaReport, ReviewVerdict};
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub const BUILDER_NAME: &str = "Kiln Builder";
pub const BUILDER_EMAIL: &str = "builder@kiln.local";
/// Max artifact files registered per run.
const MAX_ARTIFACTS: usize = 500;

/// How the harness process ended.
#[derive(Debug, Clone, Default)]
pub struct HarnessExit {
    pub exit_code: Option<i32>,
    pub spawn_error: Option<String>,
    pub stopped: Option<String>,
    pub timed_out: Option<u64>,
}

impl HarnessExit {
    pub fn ok(&self) -> bool {
        self.spawn_error.is_none()
            && self.stopped.is_none()
            && self.timed_out.is_none()
            && self.exit_code == Some(0)
    }

    /// Why the harness did not finish cleanly, if it didn't.
    pub fn failure(&self, last_error: Option<&str>) -> Option<String> {
        if let Some(e) = &self.spawn_error {
            return Some(format!("failed to start harness: {e}"));
        }
        if let Some(r) = &self.stopped {
            return Some(format!("stopped: {r}"));
        }
        if let Some(t) = self.timed_out {
            return Some(format!("timed out after {t}s"));
        }
        match self.exit_code {
            Some(0) => None,
            code => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                Some(match last_error {
                    Some(e) => format!("harness exited with {code}: {e}"),
                    None => format!("harness exited with {code}"),
                })
            }
        }
    }
}

/// Artifact kind from a file name.
pub fn artifact_kind(path: &Path) -> ArtifactKind {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => ArtifactKind::Screenshot,
        "webm" | "mp4" | "mov" | "mkv" => ArtifactKind::Video,
        "diff" | "patch" => ArtifactKind::Diff,
        "log" | "txt" | "out" => ArtifactKind::Log,
        "jsonl" | "cast" => ArtifactKind::Transcript,
        "json" if name == "qa_report.json" => ArtifactKind::QaReport,
        "json" if name == "verdict.json" => ArtifactKind::Verdict,
        // The scheduler persists the reviewed `DiffSummary` here.
        "json" if name == "diff.json" => ArtifactKind::Diff,
        "json" | "xml" | "html" | "htm" | "zip" | "lcov" => ArtifactKind::TestReport,
        _ => ArtifactKind::Other,
    }
}

async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", BUILDER_NAME)
        .env("GIT_AUTHOR_EMAIL", BUILDER_EMAIL)
        .env("GIT_COMMITTER_NAME", BUILDER_NAME)
        .env("GIT_COMMITTER_EMAIL", BUILDER_EMAIL)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Builder: commit leftovers, report HEAD. Returns (commit, error).
async fn builder_commit(
    cfg: &RunConfig,
    workspace: &Path,
    em: &mut Emitter,
) -> (Option<String>, Option<String>) {
    let status = match git(workspace, &["status", "--porcelain"]).await {
        Ok(s) => s,
        Err(e) => return (None, Some(e)),
    };
    if !status.is_empty() {
        let key = cfg.task_key.as_deref().unwrap_or("task");
        let msg = format!("kiln: {key} attempt {}", cfg.attempt);
        let res = async {
            git(workspace, &["add", "-A"]).await?;
            git(workspace, &["commit", "--no-verify", "-q", "-m", &msg]).await
        }
        .await;
        match res {
            Ok(_) => {
                em.emit(RunEventKind::Command {
                    command: format!("git commit -m {msg:?}"),
                    exit_code: Some(0),
                    output: format!(
                        "committed {} changed path(s) left by the harness",
                        status.lines().count()
                    ),
                });
            }
            Err(e) => {
                em.emit(RunEventKind::Command {
                    command: format!("git commit -m {msg:?}"),
                    exit_code: Some(1),
                    output: e.clone(),
                });
                return (None, Some(e));
            }
        }
    }
    let head = match git(workspace, &["rev-parse", "HEAD"]).await {
        Ok(h) => h,
        Err(e) => return (None, Some(e)),
    };
    // "Must leave a commit": compare with the base branch when it resolves.
    let base_spec = format!("{}^{{commit}}", cfg.base_branch);
    if let Ok(base) = git(workspace, &["rev-parse", "--verify", "--quiet", &base_spec]).await
        && base == head
    {
        return (
            Some(head),
            Some(format!(
                "Builder left no commits on top of {}",
                cfg.base_branch
            )),
        );
    }
    (Some(head), None)
}

fn read_json<T: DeserializeOwned>(path: &Path, what: &str) -> Result<Option<T>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<T>(&bytes)
            .map(Some)
            .map_err(|e| format!("invalid {what}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {what}: {e}")),
    }
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn first_line(s: &str) -> String {
    let l = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    kiln_harness::util::truncate(l, 200)
}

/// Every file below `dir` (bounded), sorted.
fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() && depth < 8 {
                stack.push((e.path(), depth + 1));
            } else if t.is_file() {
                out.push(e.path());
            }
            if out.len() >= MAX_ARTIFACTS {
                break;
            }
        }
    }
    out.sort();
    out
}

fn register(
    env: &RunnerEnv,
    em: &mut Emitter,
    path: &Path,
    kind: ArtifactKind,
    caption: Option<String>,
) {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let rel = path
        .strip_prefix(&env.run_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    em.uplink().send_artifact(RunnerMessage::Artifact {
        path: rel.clone(),
        kind,
        caption: caption.clone(),
        size,
    });
    em.emit(RunEventKind::Artifact {
        path: rel,
        kind,
        caption,
    });
}

/// Register `out/artifacts/**` (and the structured outputs).
pub fn register_artifacts(
    env: &RunnerEnv,
    em: &mut Emitter,
    structured: &[(PathBuf, ArtifactKind)],
) {
    for (p, kind) in structured {
        if p.is_file() {
            register(env, em, p, *kind, None);
        }
    }
    for p in walk_files(&env.artifacts_dir()) {
        let caption = p
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.replace(['_', '-'], " "));
        register(env, em, &p, artifact_kind(&p), caption);
    }
}

/// Build the outcome for the run's role.
pub async fn collect(
    cfg: &RunConfig,
    env: &RunnerEnv,
    workspace: &Path,
    harness: Option<&dyn Harness>,
    exit: &HarnessExit,
    em: &mut Emitter,
) -> RunOutcome {
    let out = env.out_dir();
    let mut outcome = RunOutcome::failed("");
    outcome.error = None;
    outcome.exit_code = exit.exit_code;
    let mut errors: Vec<String> = Vec::new();
    if let Some(f) = exit.failure(em.errors.last().map(String::as_str)) {
        errors.push(f);
    }
    let summary_md = read_text(&out.join("summary.md"));
    let mut structured = Vec::new();
    let mut role_ok = true;
    let mut summary = String::new();

    match cfg.role {
        WorkerRole::Builder => {
            if exit.spawn_error.is_none() {
                let (commit, err) = builder_commit(cfg, workspace, em).await;
                if let Some(e) = err {
                    role_ok = false;
                    errors.push(e);
                }
                if let Some(c) = &commit {
                    summary = format!("committed {}", &c[..c.len().min(12)]);
                    if let Some(a) = &em.last_assistant {
                        summary.push_str(&format!(": {}", first_line(a)));
                    }
                }
                outcome.commit = commit;
            }
        }
        WorkerRole::Qa => {
            let p = out.join("qa_report.json");
            match read_json::<QaReport>(&p, "qa_report.json") {
                Ok(Some(r)) => {
                    let pass = r
                        .criteria
                        .iter()
                        .filter(|c| c.status == kiln_types::qa::PassFail::Pass)
                        .count();
                    summary = format!(
                        "QA {}: {pass}/{} criteria, tests {} passed / {} failed",
                        if r.passed() { "passed" } else { "failed" },
                        r.criteria.len(),
                        r.tests.passed,
                        r.tests.failed
                    );
                    if !r.summary.trim().is_empty() {
                        summary.push_str(&format!(" — {}", first_line(&r.summary)));
                    }
                    em.log(
                        if r.passed() {
                            LogLevel::Pass
                        } else {
                            LogLevel::Fail
                        },
                        summary.clone(),
                    );
                    outcome.qa_report = Some(r);
                    structured.push((p, ArtifactKind::QaReport));
                }
                Ok(None) => {
                    role_ok = false;
                    errors.push("QA did not write out/qa_report.json".into());
                }
                Err(e) => {
                    role_ok = false;
                    errors.push(e);
                }
            }
        }
        WorkerRole::Reviewer => {
            let p = out.join("verdict.json");
            match read_json::<ReviewVerdict>(&p, "verdict.json") {
                Ok(Some(v)) => {
                    summary = format!(
                        "review {}: {}",
                        if v.approved() {
                            "approved"
                        } else {
                            "changes requested"
                        },
                        first_line(&v.summary)
                    );
                    em.log(
                        if v.approved() {
                            LogLevel::Pass
                        } else {
                            LogLevel::Warn
                        },
                        summary.clone(),
                    );
                    outcome.verdict = Some(v);
                    structured.push((p, ArtifactKind::Verdict));
                }
                Ok(None) => {
                    role_ok = false;
                    errors.push("Reviewer did not write out/verdict.json".into());
                }
                Err(e) => {
                    role_ok = false;
                    errors.push(e);
                }
            }
        }
        WorkerRole::Planner => {
            let reply = read_text(&out.join("reply.md")).or_else(|| em.last_assistant.clone());
            let plan = match read_json::<PlanSpec>(&out.join("plan.json"), "plan.json") {
                Ok(Some(p)) => Some(Ok(p)),
                Ok(None) => reply
                    .as_deref()
                    .and_then(|r| PlanSpec::from_planner_output(r).ok())
                    .map(Ok),
                Err(e) => Some(Err(e)),
            };
            match plan {
                Some(Ok(p)) => match p.validate() {
                    Ok(()) => {
                        summary = format!("plan: {} ({} tasks)", p.feature, p.tasks.len());
                        outcome.plan = Some(p);
                    }
                    Err(e) => {
                        role_ok = false;
                        errors.push(format!("invalid plan.json: {e}"));
                    }
                },
                Some(Err(e)) => {
                    role_ok = false;
                    errors.push(e);
                }
                None => {}
            }
            if reply.is_none() && outcome.plan.is_none() {
                role_ok = false;
                errors.push("Planner produced neither a reply nor a plan".into());
            }
            if summary.is_empty() {
                summary = reply.as_deref().map(first_line).unwrap_or_default();
            }
            outcome.reply = reply;
        }
        WorkerRole::Distiller => {
            let reply = summary_md.clone().or_else(|| em.last_assistant.clone());
            summary = reply.as_deref().map(first_line).unwrap_or_default();
            outcome.reply = reply;
        }
    }

    register_artifacts(env, em, &structured);

    // Usage: the stream's totals, else the native transcript's.
    let mut usage = em.usage.clone();
    if usage.tokens_in == 0
        && usage.tokens_out == 0
        && let Some(h) = harness
        && let Some(key) = h.transcript_key()
    {
        let mut t = Usage::default();
        for ev in h.parse_transcript(&env.run_dir.join("transcripts").join(key)) {
            if let RunEventKind::Usage {
                tokens_in,
                tokens_out,
                cost_usd,
            } = ev
            {
                t.tokens_in += tokens_in;
                t.tokens_out += tokens_out;
                t.cost_usd += cost_usd;
            }
        }
        if t.tokens_in > 0 || t.tokens_out > 0 {
            em.emit(RunEventKind::Usage {
                tokens_in: t.tokens_in,
                tokens_out: t.tokens_out,
                cost_usd: t.cost_usd,
            });
            usage = t;
        }
    }
    outcome.usage = usage;

    outcome.success = exit.ok() && role_ok;
    if let Some(s) = summary_md {
        summary = kiln_harness::util::truncate(&s, 2000);
    }
    if summary.is_empty() {
        summary = em
            .last_assistant
            .as_deref()
            .map(first_line)
            .unwrap_or_default();
    }
    if !errors.is_empty() {
        outcome.error = Some(errors.join("; "));
    }
    outcome.summary = if outcome.success {
        if summary.is_empty() {
            format!("{} finished", cfg.role.label())
        } else {
            summary
        }
    } else {
        let e = outcome.error.clone().unwrap_or_else(|| "run failed".into());
        if summary.is_empty() {
            e
        } else {
            format!("{e} ({summary})")
        }
    };
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(
            artifact_kind(Path::new("a/shot.PNG")),
            ArtifactKind::Screenshot
        );
        assert_eq!(artifact_kind(Path::new("rec.webm")), ArtifactKind::Video);
        assert_eq!(
            artifact_kind(Path::new("results.json")),
            ArtifactKind::TestReport
        );
        assert_eq!(
            artifact_kind(Path::new("index.html")),
            ArtifactKind::TestReport
        );
        assert_eq!(
            artifact_kind(Path::new("qa_report.json")),
            ArtifactKind::QaReport
        );
        assert_eq!(artifact_kind(Path::new("x.patch")), ArtifactKind::Diff);
        assert_eq!(artifact_kind(Path::new("out/artifacts/diff.json")), ArtifactKind::Diff);
        assert_eq!(artifact_kind(Path::new("server.log")), ArtifactKind::Log);
        assert_eq!(artifact_kind(Path::new("noext")), ArtifactKind::Other);
    }

    #[test]
    fn exit_failures() {
        let mut e = HarnessExit {
            exit_code: Some(0),
            ..Default::default()
        };
        assert!(e.ok());
        assert_eq!(e.failure(None), None);
        e.exit_code = Some(2);
        assert_eq!(
            e.failure(Some("boom")).unwrap(),
            "harness exited with 2: boom"
        );
        e.timed_out = Some(5);
        assert_eq!(e.failure(None).unwrap(), "timed out after 5s");
        e.stopped = Some("user".into());
        assert_eq!(e.failure(None).unwrap(), "stopped: user");
    }
}
