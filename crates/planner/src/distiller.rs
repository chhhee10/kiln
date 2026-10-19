//! Distiller prompt (L1 episode → proposed L2 memory items).

use crate::render::{bullets, fenced, one_line, or_none, truncate};
use kiln_types::PRODUCT_NAME;
use kiln_types::event::RunEvent;
use kiln_types::memory::{Episode, Visibility};
use std::fmt::Write as _;

/// Where the Distiller writes its JSON array (it also replies with it).
pub const MEMORY_OUT: &str = "/run/kiln/out/memory.json";

/// At most this many events are rendered.
pub const MAX_EVENTS: usize = 200;
/// Each rendered event line is cut to this many characters.
pub const MAX_EVENT_CHARS: usize = 300;

/// Example output (parses as `Vec<MemoryProposal>`).
pub const MEMORY_EXAMPLE: &str = r#"[
  {
    "kind": "procedure",
    "title": "Run e2e tests with the seeded test database",
    "subject": "e2e tests",
    "relation": "require",
    "object": "DATABASE_URL pointing at the qa postgres and `npm run seed:test` first",
    "body": "Playwright specs assume the fixtures from scripts/seed-test.ts. Without seeding, login tests fail with 'user not found'.",
    "visibility": "company",
    "project_id": null
  },
  {
    "kind": "convention",
    "title": "API errors are thiserror enums mapped in api/error.rs",
    "subject": "API error handling",
    "relation": "uses",
    "object": "one thiserror enum per module, converted to HTTP status in src/api/error.rs",
    "body": "Handlers return Result<_, ApiError>; never build StatusCode responses inline.",
    "visibility": "company"
  },
  {
    "kind": "preference",
    "title": "Prefers small commits with imperative messages",
    "subject": "owner",
    "relation": "prefers",
    "object": "small commits with imperative subject lines",
    "body": "",
    "visibility": "private"
  }
]"#;

/// Distiller system instructions.
pub fn distiller_system() -> String {
    format!(
        "You curate the {PRODUCT_NAME} company brain. From one finished agent run you extract the few pieces of durable knowledge \
that will help future tasks, as structured JSON. You are selective: most runs yield zero to three items."
    )
}

/// Prompt asking for a JSON array of `MemoryProposal`s from one episode and
/// a sample of its events.
pub fn distiller_prompt(episode: &Episode, sample_events: &[RunEvent]) -> String {
    let mut s = String::new();
    s.push_str("# Distill durable knowledge from this run\n\n## Episode\n\n");
    let _ = writeln!(s, "- Role: {}", episode.role.label());
    let _ = writeln!(s, "- Goal: {}", one_line(&episode.goal));
    let _ = writeln!(
        s,
        "- Outcome: {}",
        or_none(&one_line(&episode.outcome), "unknown")
    );
    let _ = writeln!(
        s,
        "- Duration: {} s · cost ${:.2}",
        episode.duration_secs, episode.cost_usd
    );
    s.push_str("\n**Errors hit:**\n");
    s.push_str(&bullets(&episode.errors, "_none_"));
    s.push_str("\n**Fixes applied:**\n");
    s.push_str(&bullets(&episode.fixes, "_none_"));
    s.push_str("\n**Files touched:**\n");
    s.push_str(&bullets(&episode.files, "_none_"));

    s.push_str("\n## Sample of run events (chronological)\n\n");
    if sample_events.is_empty() {
        s.push_str("_No events sampled._\n");
    } else {
        let mut lines = String::new();
        for e in sample_events.iter().take(MAX_EVENTS) {
            let (level, msg) = e.kind.log_line();
            let _ = writeln!(
                lines,
                "[{}] {}",
                level.as_str(),
                truncate(&one_line(&msg), MAX_EVENT_CHARS)
            );
        }
        if sample_events.len() > MAX_EVENTS {
            let _ = writeln!(
                lines,
                "… {} more events omitted",
                sample_events.len() - MAX_EVENTS
            );
        }
        s.push_str(&fenced("text", &lines));
    }

    let default_vis = match episode.visibility {
        Visibility::Shared => Visibility::Private, // the distiller cannot know who to share with
        v => v,
    };
    let project = episode
        .project_id
        .map(|p| format!("\"{p}\""))
        .unwrap_or_else(|| "null".into());
    let _ = write!(
        s,
        r#"
## What is worth remembering
- `convention` — how this codebase does something, so future work stays consistent (error handling, test layout, naming, module structure).
- `decision` — a choice that was made and why, which later work must respect.
- `procedure` — steps that are known to work: how to run or seed something, required env vars, build flags.
- `fact` — stable facts about the system: where configuration lives, service names and ports, which component owns what. Record **gotchas** as facts or procedures that include the fix (e.g. "test X is flaky unless Y").
- `preference` — a personal working preference the user expressed. **Always `"visibility": "private"`.**

## What is NOT worth remembering
- Transient problems: network blips, timeouts, rate limits, a one-off typo, a flaky run that passed on retry with no lesson.
- Details only relevant to this task's diff, or anything obvious from reading the README or the code.
- Speculation — only record what the run actually demonstrated.
- Secrets, tokens, credentials, personal data, customer data. Never include them, not even partially.

## Quality bar
- Each item stands alone: a reader with no context understands it. Be specific — name files, commands, settings.
- `title` ≤ 80 characters. `body` ≤ 3 sentences.
- Triple: `subject` is the thing (e.g. "e2e tests"), `relation` a short verb phrase ("require", "is configured in", "fails when"), `object` the value.
- Prefer zero to five items. If nothing qualifies, output `[]`.

## Visibility and scope
- Default visibility: `"{vis}"`. Use `"private"` for preferences and anything personal. Never use `"shared"`.
- `project_id`: `{project}` for knowledge specific to this project; `null` for general knowledge.

## Output
A JSON array exactly in this shape (`kind` is one of `fact`, `convention`, `decision`, `procedure`, `preference`):
"#,
        vis = default_vis.as_str(),
    );
    s.push_str(&fenced("json", MEMORY_EXAMPLE));
    let _ = write!(
        s,
        "\nWrite the array to `{MEMORY_OUT}` and reply with exactly the same JSON array and nothing else.\n"
    );
    s
}
