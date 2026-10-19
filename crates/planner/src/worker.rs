//! Pipeline worker prompts: Builder, QA, Reviewer.

use crate::render::{bullets, fenced, numbered, one_line, or_none, truncate};
use kiln_types::protocol::paths;
use kiln_types::qa::{PassFail, QaProfile, QaReport};
use kiln_types::{DiffSummary, PRODUCT_NAME, Task};
use std::fmt::Write as _;

/// Example `qa_report.json` shown to QA (parses as [`QaReport`]).
pub const QA_REPORT_EXAMPLE: &str = r#"{
  "criteria": [
    {
      "criterion": "Submitting the form with an empty email shows 'Email is required'",
      "status": "pass",
      "evidence": ["artifacts/1-empty-email.png", "artifacts/1-empty-email.trace.zip"],
      "notes": "Opened /signup, left email empty, clicked Sign up. The message appeared under the field and no POST /api/signup was sent (checked the network log)."
    },
    {
      "criterion": "A valid signup redirects to /welcome",
      "status": "fail",
      "evidence": ["artifacts/2-valid-signup.png", "artifacts/e2e.log"],
      "notes": "Expected redirect to /welcome. Actual: stayed on /signup with toast 'Unexpected error'; server log shows 500 'column users.plan does not exist'. Repro: npm run seed && open /signup, submit a@b.co / password123."
    }
  ],
  "tests": {
    "passed": 41,
    "failed": 1,
    "skipped": 0,
    "suites": [
      { "name": "unit", "passed": 38, "failed": 0, "duration_ms": 5120 },
      { "name": "e2e", "passed": 3, "failed": 1, "duration_ms": 48200 }
    ],
    "failing_assertion": "signup.spec.ts:42 expect(page).toHaveURL('/welcome') — received '/signup'"
  },
  "summary": "1 of 2 criteria failed: valid signups hit a 500 because the users.plan migration is missing.",
  "test_branch": "kiln/fp-12-signup-validation-qa"
}"#;

/// Example `verdict.json` shown to the Reviewer (parses as `ReviewVerdict`).
pub const VERDICT_EXAMPLE: &str = r#"{
  "decision": "changes_requested",
  "findings": [
    {
      "severity": "major",
      "file": "src/webhook/retry.rs",
      "line": 57,
      "message": "The delay doubles without a cap, so attempt 10 sleeps ~17 minutes; criterion 2 requires a 60 s maximum. Clamp with .min(MAX_DELAY) and add a test for attempt 10."
    },
    {
      "severity": "minor",
      "file": "src/webhook/retry.rs",
      "line": 12,
      "message": "Use the crate's tracing macros instead of println! (see src/webhook/send.rs)."
    }
  ],
  "summary": "Backoff is implemented and tested, but the delay is not capped as required."
}"#;

/// Builder system instructions.
pub fn builder_system() -> String {
    format!(
        "You are the {PRODUCT_NAME} Builder: a careful senior engineer implementing exactly one task in a git worktree at `{ws}`. \
You work autonomously — nobody will answer questions mid-run. Make the smallest change that fully satisfies the acceptance criteria, \
prove it with tests, commit it to the current branch, and write a summary to `{summary}`. QA and a Reviewer check your work next; \
their feedback comes back to you if anything fails.",
        ws = paths::WORKSPACE,
        summary = paths::SUMMARY
    )
}

/// QA system instructions.
pub fn qa_system() -> String {
    format!(
        "You are {PRODUCT_NAME} QA: a demanding tester who verifies one task's acceptance criteria by actually running the software \
like a user would, and records evidence. You never fix product code. You report one pass/fail result per acceptance criterion \
in `{report}`; a criterion passes only if you observed it working.",
        report = paths::QA_REPORT
    )
}

/// Reviewer system instructions.
pub fn reviewer_system() -> String {
    format!(
        "You are the {PRODUCT_NAME} Reviewer: a senior engineer reviewing one task's diff for correctness, scope, tests, conventions \
and security. You are read-only: never modify files, commit or push. Your verdict goes in `{verdict}`; your findings are sent \
to the Builder verbatim, so make them specific and actionable.",
        verdict = paths::VERDICT
    )
}

/// Push Markdown headings in `text` down by `levels` so embedded feedback
/// nests under the section that contains it.
fn demote_headings(text: &str, levels: usize) -> String {
    let extra = "#".repeat(levels);
    text.lines()
        .map(|l| {
            if l.starts_with('#') {
                format!("{extra}{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn task_header(s: &mut String, task: &Task) {
    let _ = writeln!(s, "# {}: {}\n", task.key, one_line(&task.title));
}

fn branch_of(task: &Task) -> String {
    task.branch
        .clone()
        .unwrap_or_else(|| "(the current branch)".into())
}

/// Builder prompt for `attempt` of `cap`.
///
/// On a loop (`attempt > 1` or non-empty `feedback`) the feedback — QA
/// failures, Reviewer findings, human notes — is shown first and must be
/// addressed before anything else.
pub fn builder_prompt(
    task: &Task,
    feature: &str,
    attempt: u32,
    cap: u32,
    feedback: &[String],
    memory_pack: &str,
) -> String {
    let mut s = String::new();
    task_header(&mut s, task);
    let _ = writeln!(
        s,
        "Attempt **{attempt} of {}** · branch `{}` · part of the feature **{}**\n",
        cap.max(attempt),
        branch_of(task),
        one_line(feature)
    );

    let feedback: Vec<&String> = feedback.iter().filter(|f| !f.trim().is_empty()).collect();
    if !feedback.is_empty() || attempt > 1 {
        s.push_str("## Fix this first: feedback from the previous attempt\n\n");
        if feedback.is_empty() {
            s.push_str("_The previous attempt did not pass, but no specific feedback was recorded. Re-check every acceptance criterion yourself._\n\n");
        }
        let levels = if feedback.len() > 1 { 2 } else { 1 };
        for (i, f) in feedback.iter().enumerate() {
            if feedback.len() > 1 {
                let _ = writeln!(s, "### Feedback {}\n", i + 1);
            }
            let _ = writeln!(s, "{}\n", demote_headings(f.trim(), levels));
        }
        s.push_str(
            "Address **every** point above, then re-verify all acceptance criteria — not just the ones that failed. \
The commits from earlier attempts are still on this branch: build on them, and only start over if the feedback says the approach itself is wrong. \
If you believe a point is mistaken, do not ignore it silently: explain why in the summary, with evidence.\n\n",
        );
        if attempt >= cap && cap > 0 {
            s.push_str("This is the **last attempt** before the task is escalated to a human. Be thorough.\n\n");
        }
    }

    s.push_str("## What to build\n\n");
    s.push_str(&or_none(&task.description, &one_line(&task.title)));
    s.push_str("\n\n## Acceptance criteria\n\nQA will verify each of these literally, by running the software:\n\n");
    s.push_str(&numbered(&task.acceptance));
    s.push_str("\n## Likely files\n\n");
    s.push_str(&bullets(
        &task.files_hint,
        "_No hint — find the right place by reading the code._",
    ));
    s.push_str("\n## Relevant company memory\n\n");
    s.push_str(&or_none(memory_pack, "_None._"));
    s.push_str("\n\nMore is available through the `search_memory` tool (conventions, procedures, past gotchas).\n\n");

    let _ = write!(
        s,
        r#"## How to work
1. **Understand first.** Read `AGENTS.md`/`CLAUDE.md`, the files above and nearby code. Follow the existing structure, naming, error handling and test style; the memory above records this company's conventions.
2. **Keep the scope tight.** Implement what the criteria require and nothing else: no unrelated refactors, renames, reformatting of untouched code, dependency upgrades or drive-by fixes. If you notice something else that is broken, mention it in the summary instead.
3. **Test it.** Add or update tests that exercise each criterion, next to the existing tests and in their style. Never delete, skip or weaken existing tests or assertions to get a green run.
4. **Run the checks** that cover your change: build, the relevant test suites, and the linters/formatters the project uses. Fix what you broke. If a failure pre-dates your change, leave it and say so in the summary.
5. **Commit** to the current branch with messages like `{key}: <imperative summary>` (several commits are fine). Do not push, switch or create branches, rebase, amend or squash earlier commits, or touch other branches. Never commit secrets, credentials, `.env` files, build output, or the generated `AGENTS.md`/`CLAUDE.md`.
6. **Write `{summary}`**: what changed (files and why), how you verified it (commands and results), what QA needs to exercise the feature (URLs, commands, flags, seed data), and any known limitations.

If you are blocked — a missing credential, a contradiction in the requirements, a broken environment — do not fake progress or stub out behaviour to look done. Commit what is solid and explain the blocker precisely in the summary.
"#,
        key = task.key,
        summary = paths::SUMMARY
    );
    s
}

/// QA prompt: follow `qa.yaml`, exercise every criterion like a user, keep
/// tests on a scratch branch, collect evidence and write `qa_report.json`.
pub fn qa_prompt(task: &Task, qa_profile: Option<&QaProfile>, builder_summary: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# QA for {}: {}\n", task.key, one_line(&task.title));
    let branch = branch_of(task);
    let scratch = format!("{}-qa", task.branch.as_deref().unwrap_or("kiln/qa"));
    let _ = writeln!(
        s,
        "Verify the Builder's work on branch `{branch}` (checked out at `{}`, read-only for you) against every acceptance criterion.\n",
        paths::WORKSPACE
    );

    s.push_str("## Acceptance criteria\n\nReport exactly one result per criterion, in this order, copying the criterion text verbatim:\n\n");
    s.push_str(&numbered(&task.acceptance));
    s.push_str("\n## Task description\n\n");
    s.push_str(&or_none(&task.description, "_None beyond the title._"));
    s.push_str("\n\n## Builder's summary\n\n");
    s.push_str(&or_none(builder_summary, "_The Builder left no summary._"));
    s.push_str("\n\n## QA profile (`qa.yaml`)\n\n");
    match qa_profile {
        Some(p) => s.push_str(&profile_text(p)),
        None => s.push_str(
            "_This project has no usable `qa.yaml`._ Work out how to build, start and test it from the README, package scripts, Makefile or CI config, and state in the summary which commands you used.\n",
        ),
    }

    let _ = write!(
        s,
        r#"
## Procedure
1. **Boot** the stack with the boot commands, in order, from `{ws}` (or your scratch copy, below). If boot fails, every criterion that needs the running app fails: attach the logs as evidence and explain.
2. **Seed** data with the seed commands.
3. **Run the checks** (unit, e2e, cli, extra) and save each one's full output, e.g. `artifacts/unit.log`, `artifacts/e2e.log`. Rerun a failing test up to two times to rule out flakiness and say if it is flaky.
4. **Exercise every criterion like a user**, including at least one edge or error case where it makes sense:
   - Web apps: drive a real browser with Playwright (headless Chromium). Navigate, click and type as a user would; assert on visible text and state. Screenshot each key state (`artifacts/<n>-<slug>.png`); keep traces or videos for multi-step flows.
   - CLIs: run the real binary in a real terminal session (use a pty or `expect` for interactive prompts). Save the exact command, stdout, stderr and exit code (`artifacts/<n>-<slug>.txt`).
   - Libraries and APIs: call the public interface from a small test or script; for HTTP save the full request and response.
5. **Tests you write go on a scratch branch, never the task branch.** The workspace is read-only, so work in a copy:
   `git clone --quiet {ws} /tmp/qa && cd /tmp/qa && git checkout -b {scratch}`
   Commit your tests there, then export them with `git format-patch origin/HEAD..HEAD --stdout > {artifacts}/qa-tests.patch` and set `test_branch` to `{scratch}`.
6. **Evidence** goes under `{artifacts}/`. Cite it by path relative to `{out}` (for example `artifacts/1-empty-email.png`).
7. **Be strict and honest.** `pass` only when you saw the behaviour work. If something cannot be tested, it is a `fail` with notes saying why — never pass by assumption. Do not modify product code to make something pass.

## Output: `{report}`
Write valid JSON exactly in this shape (field names and enum values are fixed; `status` is `"pass"` or `"fail"`):
"#,
        ws = paths::WORKSPACE,
        out = paths::OUT_DIR,
        artifacts = paths::ARTIFACTS_DIR,
        report = paths::QA_REPORT,
    );
    s.push_str(&fenced("json", QA_REPORT_EXAMPLE));
    s.push_str(
        r#"
- `criteria`: one entry per acceptance criterion above, same order, verbatim text.
- `evidence`: paths of the files that show the result.
- `notes`: what you did and what you observed. For a failure: expected vs actual, and exact repro steps — the Builder receives this verbatim.
- `tests`: totals over every suite you ran; `failing_assertion` is the first failing assertion message (omit when nothing failed).
- `summary`: one to three sentences.

Finally, reply with a one-paragraph summary of the result.
"#,
    );
    s
}

fn profile_text(p: &QaProfile) -> String {
    let mut s = String::new();
    let list = |cmds: &[String]| {
        if cmds.is_empty() {
            "_(none)_\n".to_string()
        } else {
            cmds.iter().map(|c| format!("- `{}`\n", c.trim())).collect()
        }
    };
    if !p.services.is_empty() {
        s.push_str("**Services** (already started by Kiln as sidecars; reach them by hostname):\n");
        for (name, svc) in &p.services {
            let ports = svc
                .ports
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                s,
                "- `{name}` — image `{}`{}",
                svc.image,
                if ports.is_empty() {
                    String::new()
                } else {
                    format!(", ports {ports}")
                }
            );
        }
        s.push('\n');
    }
    s.push_str("**Boot:**\n");
    s.push_str(&list(&p.boot));
    s.push_str("\n**Seed:**\n");
    s.push_str(&list(&p.seed));
    s.push_str("\n**Checks:**\n");
    let c = &p.checks;
    let mut any = false;
    for (name, cmd) in [("unit", &c.unit), ("e2e", &c.e2e), ("cli", &c.cli)] {
        if let Some(cmd) = cmd.as_deref().filter(|c| !c.trim().is_empty()) {
            let _ = writeln!(s, "- {name}: `{}`", cmd.trim());
            any = true;
        }
    }
    if !c.extra.is_empty() {
        let _ = writeln!(
            s,
            "- extra: {}",
            c.extra
                .iter()
                .map(|e| format!("`{e}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        any = true;
    }
    if !any {
        s.push_str("_(none declared — find the project's test commands yourself)_\n");
    }
    if !p.artifacts.is_empty() {
        let _ = writeln!(
            s,
            "\n**Artifacts collected automatically** (also cite them as evidence): {}",
            p.artifacts
                .iter()
                .map(|a| format!("`{a}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    s
}

/// Reviewer prompt: review the diff of the task branch against
/// `base_branch` for correctness, scope, tests, conventions and security,
/// and write `verdict.json`.
pub fn reviewer_prompt(
    task: &Task,
    base_branch: &str,
    diff: &DiffSummary,
    qa: Option<&QaReport>,
    conventions_pack: &str,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Review {}: {}\n", task.key, one_line(&task.title));
    let branch = branch_of(task);
    let _ = writeln!(
        s,
        "Review the changes on `{branch}` (checked out read-only at `{}`) relative to `origin/{base_branch}`.\n",
        paths::WORKSPACE
    );
    s.push_str("## Task\n\n");
    s.push_str(&or_none(&task.description, &one_line(&task.title)));
    s.push_str("\n\n## Acceptance criteria\n\n");
    s.push_str(&numbered(&task.acceptance));

    let _ = writeln!(
        s,
        "\n## Diff summary\n\n+{} −{} across {} file(s):\n",
        diff.additions,
        diff.deletions,
        diff.files.len()
    );
    const MAX_FILES: usize = 80;
    for f in diff.files.iter().take(MAX_FILES) {
        let _ = writeln!(s, "- `{}` (+{} −{})", f.path, f.additions, f.deletions);
    }
    if diff.files.len() > MAX_FILES {
        let _ = writeln!(s, "- … and {} more", diff.files.len() - MAX_FILES);
    }
    let _ = writeln!(
        s,
        "\nRead the full diff yourself: `git log --oneline origin/{base_branch}..HEAD` and `git diff origin/{base_branch}...HEAD`. Open surrounding code where needed.\n"
    );

    s.push_str("## QA report\n\n");
    match qa {
        None => s.push_str("_QA produced no report. Judge the criteria from the code and tests alone, and be stricter about test coverage._\n"),
        Some(r) => {
            let _ = writeln!(s, "{}\n", or_none(&r.summary, "_No summary._"));
            for c in &r.criteria {
                let mark = if c.status == PassFail::Pass { "pass" } else { "FAIL" };
                let notes = if c.notes.trim().is_empty() { String::new() } else { format!(" — {}", truncate(&one_line(&c.notes), 400)) };
                let _ = writeln!(s, "- [{mark}] {}{notes}", one_line(&c.criterion));
            }
            let _ = writeln!(s, "\nTests: {} passed, {} failed, {} skipped.", r.tests.passed, r.tests.failed, r.tests.skipped);
        }
    }
    s.push_str("\n## Project conventions\n\n");
    s.push_str(&or_none(
        conventions_pack,
        "_No recorded conventions; infer them from the surrounding code._",
    ));

    let _ = write!(
        s,
        r#"

## What to check
1. **Correctness** — does the change actually implement each acceptance criterion? Look for logic errors, unhandled edge cases and errors, races, resource leaks, broken backwards compatibility.
2. **Scope** — anything not required by the task (unrelated refactors, formatting churn, dependency changes, debug leftovers, committed generated files such as `AGENTS.md`/`CLAUDE.md`) is a finding.
3. **Tests** — are the criteria covered by meaningful tests? Were tests deleted, skipped or weakened?
4. **Conventions** — naming, structure, error handling and logging consistent with the conventions above and the surrounding code. Do not nitpick what formatters and linters already enforce.
5. **Security** — committed secrets, injection, missing authorization checks, unsafe handling of untrusted input, sensitive data in logs.
6. **QA consistency** — if QA passed a criterion the code clearly does not implement (or failed one it does), say so.

Severity: `critical` (security hole, data loss, broken build or main flow) · `major` (a criterion not met, a bug users will hit, core behaviour untested) · `minor` (should fix: edge case, convention violation, unclear code) · `info` (optional suggestion).
Decision: `changes_requested` if there is any critical or major finding or any criterion is not met; otherwise `approve`. Minor and info findings alone never block.
Every finding must be actionable: the file, the line in the new version, what is wrong, and what to do instead. The Builder receives them verbatim.

## Output: `{verdict}`
Write valid JSON exactly in this shape (`decision` is `"approve"` or `"changes_requested"`; `severity` is `"info"`, `"minor"`, `"major"` or `"critical"`; `file` and `line` may be omitted for general findings):
"#,
        verdict = paths::VERDICT
    );
    s.push_str(&fenced("json", VERDICT_EXAMPLE));
    s.push_str("\nThen reply with a two- or three-sentence summary of your verdict.\n");
    s
}
