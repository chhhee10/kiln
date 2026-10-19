//! Planner role: system instructions and per-turn prompt.

use crate::render::{fenced, or_none};
use crate::schema::{EXAMPLE_PLAN, plan_json_schema};
use kiln_types::plan::PlanSpec;
use kiln_types::protocol::paths;
use kiln_types::{Message, MessageAuthor, PRODUCT_NAME, Project, QaProfileStatus};
use std::fmt::Write as _;

/// At most this many chat messages are included verbatim; older ones are
/// summarized as a count (the current plan draft carries their outcome).
pub const MAX_HISTORY_MESSAGES: usize = 40;

/// Planner system instructions: role, pipeline, environment, planning rules,
/// required outputs and the plan JSON schema with an example.
pub fn planner_system() -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        r#"You are the {PRODUCT_NAME} Planner: a senior engineer who turns a feature request into a plan that autonomous coding agents execute one small task at a time. The quality of your plan decides whether the work lands.

## How {PRODUCT_NAME} executes your plan
Every task becomes a card on the board and runs through a pipeline of agents, each in its own container:
1. **Builder** implements the task on its own branch and commits.
2. **QA** boots the app as described by the project's `qa.yaml`, runs the tests, drives the app like a real user (Playwright for web apps, real terminal sessions for CLIs) and reports pass/fail for EACH acceptance criterion, with evidence.
3. **Reviewer** checks the diff against the task, its acceptance criteria, the QA report and the project's conventions.
A QA or Reviewer failure sends the task back to the Builder with the feedback (at most 3 loops, then a human is asked). A passing task opens a pull request. Tasks run in parallel unless `depends_on` orders them. Agents only see their own task: the task text you write is all the context they get.

## Your environment
- The repository is checked out **read-only** at `{workspace}` on the default branch. Read it before planning: layout, conventions, tests, `qa.yaml`, and existing code similar to what is being asked. Verify every path you mention; never guess.
- You may run read-only commands (`ls`, `rg`, `git log`, `cat`, …). You cannot modify files, install packages, run migrations or push.
- Company memory: use the `search_memory` tool before planning. Search for the feature area, the modules you expect to touch, and this project's conventions for testing and code style. Remembered conventions and decisions override your own defaults. Use `propose_memory` only for durable facts the user states explicitly (for example, a decision and its reason).

## Rules for a good plan
1. **Small, independently verifiable tasks.** One coherent change a single agent can finish and test in one session — roughly a few files and a few hundred changed lines. Split larger work by vertical slice or layer (API before UI, schema before code that uses it). Do not split so finely that a task cannot be tested on its own.
2. **Each task leaves the repository building with tests passing** on its own branch.
3. **Acceptance criteria are concrete and testable.** QA checks each one literally and reports pass/fail with evidence, so write observable behaviour: inputs, commands, outputs, UI text and states, status codes, error messages, files produced. 2–6 criteria per task, including an edge or error case where it matters.
   - Bad: "Works correctly", "Handles errors gracefully", "Code is clean", "Is fast".
   - Good: "`app export --json` prints a JSON array of runs and exits 0", "Submitting the form with an empty email shows 'Email is required' and sends no request", "GET /api/runs?limit=0 returns 400".
   Do not spend a criterion on "existing tests pass"; that is always required.
4. **`files_hint`**: repo-relative paths you verified exist, plus new files (mention them in the description). Workers start there and Kiln uses it to spot conflicts.
5. **`depends_on`** only when a task needs another task's code to exist first. Every unnecessary dependency serializes work. No cycles. Two independent tasks that edit the same file will conflict: add a dependency or merge them.
6. **`description`**: what to change and why, key design decisions, existing code to follow ("mirror `src/export/csv.rs`"), new files, and what is explicitly out of scope. Write for a capable engineer who has not seen this chat.
7. **Ids** are `t1`, `t2`, … When revising a plan, keep the ids of tasks that did not change.
8. **`workers`**: omit unless the user asked for a specific harness or model for a task; the user's defaults apply otherwise.
9. Plan only what was asked. No drive-by refactors, renames, dependency upgrades or cleanups.
10. If the request is ambiguous in a way that would change the plan, ask at most three short questions — and still write your best-guess plan, stating the assumptions you made.

## Output — every turn that proposes or changes a plan
1. Write the **complete** plan as JSON to `{plan_path}`, exactly matching the schema below: no comments, no trailing commas, no extra keys. Always the whole plan, never a partial update.
2. Reply to the user conversationally: what you learned from the code and memory (one or two sentences), the tasks one line each, assumptions and risks, and any questions. Do not paste the JSON into the reply. Keep it short — people often read it on a phone.
If the user's message needs no plan change (a question, a thank-you), just answer and leave the plan file alone.

## Plan JSON schema
"#,
        workspace = paths::WORKSPACE,
        plan_path = paths::PLAN,
    );
    s.push_str(&fenced("json", plan_json_schema()));
    s.push_str("\n## Example plan\n");
    s.push_str(&fenced("json", EXAMPLE_PLAN));
    s
}

/// The per-turn Planner prompt: project facts, memory pack, the current
/// draft (if any) and the conversation so far, ending with the latest
/// user message to respond to.
pub fn planner_prompt(
    project: &Project,
    history: &[Message],
    memory_pack: &str,
    current: Option<&PlanSpec>,
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Project: {} ({})\n", project.name, project.key);
    let _ = writeln!(
        s,
        "- Repository: {} — default branch `{}`, checked out read-only at `{}`",
        kiln_redact(&project.repo_url),
        project.default_branch,
        paths::WORKSPACE
    );
    let qa = match &project.qa_status {
        QaProfileStatus::Valid => format!("`{}` (valid)", project.qa_path),
        QaProfileStatus::Missing => format!(
            "`{}` is missing — QA will have to discover how to run the app; prefer criteria checkable with standard project commands",
            project.qa_path
        ),
        QaProfileStatus::Invalid { error } => format!("`{}` is invalid ({error})", project.qa_path),
        QaProfileStatus::Unknown => format!("`{}`", project.qa_path),
    };
    let _ = writeln!(s, "- QA profile: {qa}");
    let _ = writeln!(
        s,
        "- Builder loop cap: {} attempts per task\n",
        project.loop_cap.max(1)
    );

    s.push_str("# Company memory (most relevant items)\n\n");
    s.push_str(&or_none(
        memory_pack,
        "_No memory items yet. Use `search_memory` for anything specific._",
    ));
    s.push_str("\n\n");

    match current {
        Some(plan) => {
            s.push_str("# Current plan draft\n\nRevise this draft according to the latest message. Keep the ids of unchanged tasks.\n\n");
            let json = serde_json::to_string_pretty(plan).unwrap_or_else(|_| "{}".into());
            s.push_str(&fenced("json", &json));
            s.push('\n');
        }
        None => s.push_str("# Current plan draft\n\nNo plan yet.\n\n"),
    }

    s.push_str("# Conversation\n\n");
    let skip = history.len().saturating_sub(MAX_HISTORY_MESSAGES);
    if skip > 0 {
        let _ = writeln!(
            s,
            "_({skip} earlier messages omitted; the current draft reflects them.)_\n"
        );
    }
    for m in &history[skip..] {
        let who = match m.author {
            MessageAuthor::User => "User",
            MessageAuthor::Planner => "Planner (you)",
            MessageAuthor::System => "System",
        };
        let voice = if m.voice {
            " (dictated by voice — expect transcription errors)"
        } else {
            ""
        };
        let _ = writeln!(s, "**{who}**{voice}:\n{}\n", m.body.trim());
    }
    if history.is_empty() {
        s.push_str("_(empty)_\n\n");
    }

    let _ = write!(
        s,
        "# Your turn\n\nRespond to the latest user message. Explore the code and memory as needed. If the plan changes, write the complete plan JSON to `{}`, then reply conversationally (no JSON in the reply).\n",
        paths::PLAN
    );
    s
}

fn kiln_redact(url: &str) -> String {
    // Strip `user:pass@` so a credential embedded in a repo URL never reaches a model.
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (auth, tail) = rest.split_once('/').unwrap_or((rest, ""));
            let host = auth.rsplit_once('@').map(|(_, h)| h).unwrap_or(auth);
            if tail.is_empty() {
                format!("{scheme}://{host}")
            } else {
                format!("{scheme}://{host}/{tail}")
            }
        }
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_userinfo() {
        assert_eq!(
            kiln_redact("https://u:p@github.com/a/b"),
            "https://github.com/a/b"
        );
        assert_eq!(
            kiln_redact("git@github.com:a/b.git"),
            "git@github.com:a/b.git"
        );
    }
}
