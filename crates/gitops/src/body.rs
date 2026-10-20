//! PR body rendering.

use kiln_types::plan::PlanSpec;
use kiln_types::qa::{CriterionResult, Decision, PassFail, QaReport, ReviewVerdict};
use kiln_types::{PRODUCT_NAME, Task};
use std::fmt::Write as _;

/// A link to a run artifact for the PR body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactLink {
    /// What QA cites in `CriterionResult::evidence`: an artifact id, or a
    /// path relative to `/run/kiln/out` before ingest.
    pub key: String,
    /// Link text (usually the file name or caption).
    pub label: String,
    /// Absolute URL (daemon artifact route).
    pub url: String,
}

/// Render the PR description: plan feature + task description, acceptance
/// criteria as a checklist (checked per QA result), QA summary table with
/// evidence links, test summary, Reviewer verdict + findings, artifact list
/// and a footer `Opened by Kiln · attempt n/cap`.
pub fn render_pr_body(
    plan: &PlanSpec,
    task: &Task,
    qa: Option<&QaReport>,
    verdict: Option<&ReviewVerdict>,
    artifact_links: &[ArtifactLink],
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "## {} · {}", task.key, one_line(&task.title));
    s.push('\n');
    let _ = writeln!(s, "> Part of **{}**", one_line(&plan.feature));
    if let Some(p) = first_paragraph(&plan.description) {
        let _ = writeln!(s, "> {p}");
    }
    s.push('\n');
    if !task.description.trim().is_empty() {
        let _ = writeln!(s, "{}\n", task.description.trim());
    }

    // Acceptance criteria checklist.
    s.push_str("### Acceptance criteria\n\n");
    if task.acceptance.is_empty() {
        s.push_str("_None recorded._\n");
    }
    for (i, c) in task.acceptance.iter().enumerate() {
        let passed = qa
            .and_then(|r| result_for(r, &task.acceptance, i))
            .is_some_and(|r| r.status == PassFail::Pass);
        let _ = writeln!(s, "- [{}] {}", if passed { "x" } else { " " }, one_line(c));
    }
    s.push('\n');

    // QA.
    s.push_str("### QA\n\n");
    match qa {
        None => s.push_str("_No QA report._\n\n"),
        Some(r) => {
            let verdict_line = if r.passed() {
                "✓ All criteria passed."
            } else {
                "✕ QA reported failures."
            };
            let _ = writeln!(s, "**{verdict_line}**");
            if !r.summary.trim().is_empty() {
                let _ = writeln!(s, "\n{}", r.summary.trim());
            }
            s.push('\n');
            if !r.criteria.is_empty() {
                s.push_str("| Criterion | Status | Evidence |\n|---|---|---|\n");
                for c in &r.criteria {
                    let status = match c.status {
                        PassFail::Pass => "✓ pass",
                        PassFail::Fail => "✕ fail",
                    };
                    let _ = writeln!(
                        s,
                        "| {} | {} | {} |",
                        cell(&c.criterion),
                        status,
                        evidence(&c.evidence, artifact_links)
                    );
                }
                s.push('\n');
            }
            let failed: Vec<&CriterionResult> = r
                .criteria
                .iter()
                .filter(|c| c.status == PassFail::Fail && !c.notes.trim().is_empty())
                .collect();
            if !failed.is_empty() {
                s.push_str("<details><summary>Failure notes</summary>\n\n");
                for c in failed {
                    let _ = writeln!(
                        s,
                        "- **{}** — {}",
                        one_line(&c.criterion),
                        one_line(&c.notes)
                    );
                }
                s.push_str("\n</details>\n\n");
            }
            let t = &r.tests;
            let _ = write!(
                s,
                "**Tests:** {} passed · {} failed · {} skipped",
                t.passed, t.failed, t.skipped
            );
            if t.running > 0 {
                let _ = write!(s, " · {} still running", t.running);
            }
            s.push('\n');
            if !t.suites.is_empty() {
                s.push('\n');
                s.push_str("| Suite | Passed | Failed | Duration |\n|---|---:|---:|---:|\n");
                for suite in &t.suites {
                    let _ = writeln!(
                        s,
                        "| {} | {} | {} | {:.1}s |",
                        cell(&suite.name),
                        suite.passed,
                        suite.failed,
                        suite.duration_ms as f64 / 1000.0
                    );
                }
            }
            if let Some(a) = t
                .failing_assertion
                .as_deref()
                .filter(|a| !a.trim().is_empty())
            {
                let fence = fence_for(a);
                let _ = writeln!(
                    s,
                    "\nFirst failing assertion:\n\n{fence}\n{}\n{fence}",
                    a.trim()
                );
            }
            if let Some(b) = r.test_branch.as_deref().filter(|b| !b.is_empty()) {
                let _ = writeln!(s, "\nQA tests are on branch `{b}`.");
            }
            s.push('\n');
        }
    }

    // Review.
    s.push_str("### Review\n\n");
    match verdict {
        None => s.push_str("_No review verdict._\n\n"),
        Some(v) => {
            let d = match v.decision {
                Decision::Approve => "✓ Approved",
                Decision::ChangesRequested => "✕ Changes requested",
            };
            let _ = writeln!(s, "**Verdict:** {d}\n");
            if !v.summary.trim().is_empty() {
                let _ = writeln!(s, "{}\n", v.summary.trim());
            }
            if !v.findings.is_empty() {
                let mut findings: Vec<_> = v.findings.iter().collect();
                findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
                s.push_str("| Severity | Location | Finding |\n|---|---|---|\n");
                for f in findings {
                    let loc = match (&f.file, f.line) {
                        (Some(file), Some(line)) => format!("`{}:{line}`", cell(file)),
                        (Some(file), None) => format!("`{}`", cell(file)),
                        _ => "—".into(),
                    };
                    let sev = format!("{:?}", f.severity).to_lowercase();
                    let _ = writeln!(s, "| {sev} | {loc} | {} |", cell(&f.message));
                }
                s.push('\n');
            }
        }
    }

    if !artifact_links.is_empty() {
        s.push_str("### Artifacts\n\n");
        for a in artifact_links {
            let _ = writeln!(s, "- [{}]({})", link_text(&a.label), a.url);
        }
        s.push('\n');
    }

    let _ = writeln!(
        s,
        "---\n<sub>Opened by {PRODUCT_NAME} · attempt {}/{}</sub>",
        task.attempt(),
        task.loop_cap.max(1)
    );
    s
}

/// QA result for acceptance criterion `i`: exact (normalized) text match,
/// else positional when QA reported exactly one entry per criterion.
fn result_for<'a>(qa: &'a QaReport, criteria: &[String], i: usize) -> Option<&'a CriterionResult> {
    let want = norm(&criteria[i]);
    qa.criteria
        .iter()
        .find(|r| norm(&r.criterion) == want)
        .or_else(|| (qa.criteria.len() == criteria.len()).then(|| &qa.criteria[i]))
}

fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_paragraph(s: &str) -> Option<String> {
    let p = s.trim().split("\n\n").next()?.trim();
    (!p.is_empty()).then(|| one_line(p))
}

/// Make text safe inside a Markdown table cell.
fn cell(s: &str) -> String {
    one_line(s).replace('|', "\\|")
}

/// A backtick fence longer than any backtick run in `s`.
fn fence_for(s: &str) -> String {
    let longest = s.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    "`".repeat(longest.max(2) + 1)
}

fn link_text(s: &str) -> String {
    one_line(s).replace('[', "\\[").replace(']', "\\]")
}

fn evidence(items: &[String], links: &[ArtifactLink]) -> String {
    if items.is_empty() {
        return "—".into();
    }
    items
        .iter()
        .map(|e| match links.iter().find(|l| l.key == *e) {
            Some(l) => format!("[{}]({})", link_text(&l.label), l.url),
            None => format!("`{}`", e.replace('`', "'").replace('|', "\\|")),
        })
        .collect::<Vec<_>>()
        .join(", ")
}
