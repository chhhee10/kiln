//! Distillation pipeline: L0 → L1 episode → L2 proposals.
//!
//! ```text
//! L0 → distill → L1 episode → extract → proposals → salience gate
//!    → propose (dedupe / supersede / visibility rules) → L2 → packs rebuilt
//! ```

use crate::Memory;
use crate::db::{Arg, fetch_episodes};
use crate::error::{MemoryError, Result};
use crate::gateway::{EPISODE_PREFIX, ProposeOpts};
use crate::l0::L0Row;
use crate::traits::{IndexMeta, SearchFilter, TextDoc};
use crate::util::{first_line, normalize, now_ts, truncate, vec_to_blob};
use kiln_types::event::{LogLevel, RunEvent, RunEventKind};
use kiln_types::memory::{
    Episode, MemoryItem, MemoryProposal, MemoryStatus, MemoryType, Visibility,
};
use kiln_types::{EpisodeId, ProjectId, Run, RunState, Task, UserId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const MAX_LIST: usize = 20;
const MAX_LINE: usize = 300;

/// Inputs of the salience score.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SalienceInputs {
    /// Outcome link, 0–1: 1.0 when the knowledge is proven by a passing
    /// command / successful run, 0.8 for a verified fix inside a failed run,
    /// 0.5 unknown, 0.3 when it only comes from a failed run.
    pub outcome: f32,
    /// How many *other* episodes of the same owner and project mention the
    /// same subject (saturates at 3).
    pub recurrence: u32,
    /// 1 − max cosine similarity to items the owner can already see (0–1).
    pub novelty: f32,
    /// Human signal, 0–1: 1.0 when a person acted on the task (a task
    /// transition with a human actor: retry, feedback, approval).
    pub human: f32,
}

/// The salience score (0–1):
///
/// `0.35·outcome + 0.25·min(1, recurrence/3) + 0.25·novelty + 0.15·human`
///
/// Items scoring below `MemoryConfig::promote_threshold` (default 0.35)
/// stay in L0/L1 and expire with them. Salience keeps evolving after
/// promotion: +0.05 per duplicate proposal, +`feedback_boost` per passing
/// run that used the item, +`approval_boost` when the owner approves.
pub fn salience(i: SalienceInputs) -> f32 {
    let recurrence = (i.recurrence as f32 / 3.0).min(1.0);
    (0.35 * i.outcome.clamp(0.0, 1.0)
        + 0.25 * recurrence
        + 0.25 * i.novelty.clamp(0.0, 1.0)
        + 0.15 * i.human.clamp(0.0, 1.0))
    .clamp(0.0, 1.0)
}

pub(crate) fn episode_embed_text(ep: &Episode) -> String {
    format!(
        "{}\n{}\n{}\n{}\n{}",
        ep.goal,
        ep.outcome,
        ep.errors.join("\n"),
        ep.fixes.join("\n"),
        ep.files.join(" ")
    )
}

pub(crate) fn episode_meta(ep: &Episode) -> IndexMeta {
    IndexMeta {
        owner: ep.owner_id,
        kind: MemoryType::Episode,
        visibility: ep.visibility,
        shared_with: vec![],
        project: ep.project_id,
        status: MemoryStatus::Active,
    }
}

pub(crate) fn episode_doc(ep: &Episode) -> TextDoc {
    TextDoc {
        id: format!("{EPISODE_PREFIX}{}", ep.id),
        title: format!("{} run: {}", ep.role.label(), ep.goal),
        body: format!(
            "{}\n{}\n{}",
            ep.errors.join("\n"),
            ep.fixes.join("\n"),
            ep.files.join(" ")
        ),
        subject: ep.goal.clone(),
        relation: "outcome".into(),
        object: ep.outcome.clone(),
        meta: episode_meta(ep),
    }
}

/// A shell command observed in L0 with its result.
#[derive(Clone, Debug)]
struct CommandRun {
    /// Index in the event list.
    at: usize,
    command: String,
    ok: bool,
    output: String,
}

/// Commands from `Command` events and from shell tool calls
/// (`ToolCall{name: Bash|shell|…, input.command}` + its `ToolResult`).
fn commands(events: &[RunEvent]) -> Vec<CommandRun> {
    let mut out = vec![];
    let mut pending: HashMap<String, (usize, String)> = HashMap::new();
    for (i, e) in events.iter().enumerate() {
        match &e.kind {
            RunEventKind::Command {
                command,
                exit_code,
                output,
            } => out.push(CommandRun {
                at: i,
                command: command.trim().to_string(),
                ok: matches!(exit_code, Some(0) | None),
                output: output.clone(),
            }),
            RunEventKind::ToolCall { id, name, input } => {
                let n = name.to_lowercase();
                if matches!(
                    n.as_str(),
                    "bash" | "shell" | "exec" | "run_command" | "local_shell"
                ) && let Some(c) = input.get("command").and_then(|c| c.as_str())
                {
                    pending.insert(id.clone(), (i, c.trim().to_string()));
                }
            }
            RunEventKind::ToolResult { id, ok, output } => {
                if let Some((at, command)) = pending.remove(id) {
                    out.push(CommandRun {
                        at,
                        command,
                        ok: *ok,
                        output: output.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    out.sort_by_key(|c| c.at);
    out
}

/// What kind of project command this is, if recognisable.
fn command_role(cmd: &str) -> Option<&'static str> {
    let c = cmd.to_lowercase();
    const TEST: [&str; 24] = [
        "cargo test",
        "cargo nextest",
        "npm test",
        "npm run test",
        "pnpm test",
        "pnpm run test",
        "yarn test",
        "bun test",
        "pytest",
        "go test",
        "mvn test",
        "gradle test",
        "gradlew test",
        "make test",
        "vitest",
        "jest",
        "playwright test",
        "rspec",
        "phpunit",
        "dotnet test",
        "mix test",
        "tox",
        "ctest",
        "deno test",
    ];
    const LINT: [&str; 8] = [
        "cargo clippy",
        "eslint",
        "npm run lint",
        "pnpm lint",
        "pnpm run lint",
        "ruff",
        "golangci-lint",
        "yarn lint",
    ];
    const BUILD: [&str; 6] = [
        "cargo build",
        "npm run build",
        "pnpm build",
        "pnpm run build",
        "yarn build",
        "go build",
    ];
    if TEST.iter().any(|t| c.contains(t)) {
        Some("runs_tests_with")
    } else if LINT.iter().any(|t| c.contains(t)) {
        Some("lints_with")
    } else if BUILD.iter().any(|t| c.contains(t)) {
        Some("builds_with")
    } else {
        None
    }
}

/// The most informative line of a failure output.
fn error_line(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let pick = lines
        .iter()
        .find(|l| {
            let l = l.to_lowercase();
            l.starts_with("error")
                || l.contains("error:")
                || l.contains("failed")
                || l.contains("panicked")
        })
        .or(lines.first())
        .copied()
        .unwrap_or("");
    truncate(pick, 160)
}

fn push_unique(list: &mut Vec<String>, s: String) {
    let n = normalize(&s);
    if !n.is_empty() && list.len() < MAX_LIST && !list.iter().any(|x| normalize(x) == n) {
        list.push(truncate(&s, MAX_LINE));
    }
}

/// Extract the JSON array of proposals from an LLM answer: tolerant of
/// code fences, prose around the JSON, an `{"items"|"proposals"|"memories": [...]}`
/// wrapper, and missing optional fields.
pub(crate) fn parse_llm_proposals(
    text: &str,
    project: Option<ProjectId>,
    default_vis: Visibility,
) -> Vec<MemoryProposal> {
    let mut candidates: Vec<&str> = vec![];
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let Some(end) = after[body_start..].find("```") else {
            break;
        };
        candidates.push(&after[body_start..body_start + end]);
        rest = &after[body_start + end + 3..];
    }
    candidates.push(text);
    for c in candidates {
        let c = c.trim();
        let slice = match (c.find(['[', '{']), c.rfind([']', '}'])) {
            (Some(a), Some(b)) if b > a => &c[a..=b],
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(slice) else {
            continue;
        };
        let arr = match v {
            serde_json::Value::Array(a) => a,
            serde_json::Value::Object(mut m) => {
                match ["items", "proposals", "memories"]
                    .iter()
                    .find_map(|k| m.remove(*k))
                {
                    Some(serde_json::Value::Array(a)) => a,
                    _ => vec![serde_json::Value::Object(m)],
                }
            }
            _ => continue,
        };
        let mut out = vec![];
        for mut o in arr {
            let Some(m) = o.as_object_mut() else { continue };
            if let Some(t) = m.remove("type")
                && !m.contains_key("kind")
            {
                m.insert("kind".into(), t);
            }
            m.entry("kind").or_insert_with(|| "fact".into());
            m.entry("title").or_insert_with(|| "".into());
            m.entry("relation").or_insert_with(|| "is".into());
            // Narrowest of what the model asked for and the owner's default.
            let asked = m
                .get("visibility")
                .and_then(|v| v.as_str())
                .and_then(Visibility::parse);
            let vis = match asked {
                Some(a) if vis_le(a, default_vis) => a,
                _ => default_vis,
            };
            m.insert("visibility".into(), vis.as_str().into());
            if m.get("project_id").is_none_or(|p| p.is_null())
                && let Some(p) = project
            {
                m.insert("project_id".into(), p.to_string().into());
            }
            match serde_json::from_value::<MemoryProposal>(o) {
                Ok(p) => out.push(p),
                Err(e) => tracing::warn!(error = %e, "skipping malformed memory proposal"),
            }
        }
        return out;
    }
    vec![]
}

fn vis_le(a: Visibility, b: Visibility) -> bool {
    let r = |v| match v {
        Visibility::Private => 0,
        Visibility::Shared => 1,
        Visibility::Company => 2,
    };
    r(a) <= r(b)
}

impl Memory {
    /// Heuristic L0 → L1: build (or rebuild) the run's episode.
    ///
    /// * goal — task key + title (else the first step / assistant line)
    /// * outcome — last `Done` event, else the run state
    /// * errors — `Error` events, `FAIL` log lines, failing commands
    /// * fixes — a failing command that later passed (with the files changed
    ///   in between), or an error followed by a later `PASS` line
    /// * files — `FileChange` paths; duration and cost from the run/usage
    ///
    /// Idempotent per run (the episode id is kept on rebuild).
    pub async fn distill(&self, run: &Run, task: Option<&Task>) -> Result<Episode> {
        let rows = self.read_l0_rows(run.id).await?;
        let events: Vec<RunEvent> = rows.iter().filter_map(L0Row::to_event).collect();
        let project = task
            .map(|t| t.project_id)
            .or_else(|| rows.iter().find_map(|r| r.project));

        let goal = match task {
            Some(t) => format!("{}: {}", t.key, t.title),
            None => events
                .iter()
                .find_map(|e| match &e.kind {
                    RunEventKind::Step { title } => Some(title.clone()),
                    RunEventKind::Assistant { text } => Some(first_line(text).to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| format!("{} run", run.role.label())),
        };
        let goal = truncate(&goal, MAX_LINE);

        let mut errors = vec![];
        let mut fixes = vec![];
        let mut files: Vec<String> = vec![];
        let mut usage_cost = 0.0;
        let mut done: Option<(bool, String)> = None;
        let mut open_errors: Vec<String> = vec![];
        for e in &events {
            match &e.kind {
                RunEventKind::Error { message } => {
                    push_unique(&mut errors, first_line(message).to_string());
                    open_errors.push(first_line(message).to_string());
                }
                RunEventKind::Log {
                    level: LogLevel::Fail,
                    message,
                } => {
                    push_unique(&mut errors, first_line(message).to_string());
                    open_errors.push(first_line(message).to_string());
                }
                RunEventKind::Log {
                    level: LogLevel::Pass,
                    message,
                } => {
                    for err in open_errors.drain(..) {
                        push_unique(
                            &mut fixes,
                            format!("{err} → resolved ({})", first_line(message)),
                        );
                    }
                }
                RunEventKind::FileChange { path, .. }
                    if !files.contains(path) && files.len() < 200 =>
                {
                    files.push(path.clone());
                }
                RunEventKind::Usage { cost_usd, .. } => usage_cost += cost_usd,
                RunEventKind::Done { success, summary } => done = Some((*success, summary.clone())),
                _ => {}
            }
        }
        let cmds = commands(&events);
        for (i, c) in cmds.iter().enumerate() {
            if c.ok {
                continue;
            }
            push_unique(
                &mut errors,
                format!("`{}` failed: {}", c.command, error_line(&c.output)),
            );
            if let Some(pass) = cmds[i + 1..]
                .iter()
                .find(|p| p.ok && p.command == c.command)
            {
                let changed = changed_between(&events, c.at, pass.at);
                let how = if changed.is_empty() {
                    "re-ran".to_string()
                } else {
                    format!("changed {} then re-ran", changed.join(", "))
                };
                push_unique(
                    &mut fixes,
                    format!(
                        "`{}` failed ({}); {how} and it passed",
                        c.command,
                        error_line(&c.output)
                    ),
                );
            }
        }
        let outcome = match (&done, run.state) {
            (Some((true, s)), _) => format!("succeeded: {}", first_line(s)),
            (Some((false, s)), _) => format!("failed: {}", first_line(s)),
            (None, RunState::Succeeded) => "succeeded".into(),
            (None, state) => match &run.error {
                Some(err) => format!("{}: {}", state.as_str(), first_line(err)),
                None => state.as_str().to_string(),
            },
        };
        let outcome = truncate(&outcome, MAX_LINE);
        let duration_secs =
            run.duration_secs()
                .unwrap_or_else(|| match (events.first(), events.last()) {
                    (Some(a), Some(b)) => (b.ts - a.ts).num_seconds(),
                    _ => 0,
                });
        let cost_usd = if run.cost_usd > 0.0 {
            run.cost_usd
        } else {
            usage_cost
        };
        let succeeded = episode_succeeded(&outcome);
        let ep_salience = if succeeded || !fixes.is_empty() {
            0.6
        } else {
            0.4
        };

        let existing: Option<(String, String)> =
            sqlx::query_as("SELECT id, created_at FROM episodes WHERE run_id = ?")
                .bind(run.id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let (id, created) = match existing {
            Some((id, c)) => (
                id.parse::<EpisodeId>()
                    .map_err(|_| MemoryError::Corrupt("episode id".into()))?,
                c,
            ),
            None => (EpisodeId::new(), now_ts()),
        };
        let ep = Episode {
            id,
            run_id: run.id,
            task_id: task.map(|t| t.id).or(run.task_id),
            owner_id: run.owner_id,
            project_id: project,
            role: run.role,
            goal,
            outcome,
            errors,
            fixes,
            files,
            duration_secs,
            cost_usd,
            visibility: Visibility::Private,
            created_at: crate::util::parse_ts(&created)?,
        };
        let emb = self.embed_one(&episode_embed_text(&ep)).await?;
        sqlx::query(
            "INSERT INTO episodes (id, run_id, task_id, owner_id, project_id, role, visibility, goal, outcome, \
             errors_json, fixes_json, files_json, duration_secs, cost_usd, status, salience, embedding, created_at, \
             updated_at) VALUES (?, ?, ?, ?, ?, ?, 'private', ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?, ?) \
             ON CONFLICT(run_id) DO UPDATE SET task_id = excluded.task_id, project_id = excluded.project_id, \
             goal = excluded.goal, outcome = excluded.outcome, errors_json = excluded.errors_json, \
             fixes_json = excluded.fixes_json, files_json = excluded.files_json, \
             duration_secs = excluded.duration_secs, cost_usd = excluded.cost_usd, salience = excluded.salience, \
             embedding = excluded.embedding, updated_at = excluded.updated_at",
        )
        .bind(ep.id.to_string())
        .bind(run.id.to_string())
        .bind(ep.task_id.map(|t| t.to_string()))
        .bind(run.owner_id.to_string())
        .bind(project.map(|p| p.to_string()))
        .bind(run.role.as_str())
        .bind(&ep.goal)
        .bind(&ep.outcome)
        .bind(serde_json::to_string(&ep.errors)?)
        .bind(serde_json::to_string(&ep.fixes)?)
        .bind(serde_json::to_string(&ep.files)?)
        .bind(ep.duration_secs)
        .bind(ep.cost_usd)
        .bind(ep_salience)
        .bind(vec_to_blob(&emb))
        .bind(&created)
        .bind(now_ts())
        .execute(&self.pool)
        .await?;
        if let Err(e) = self.c.text.upsert(&[episode_doc(&ep)]).await {
            tracing::warn!(error = %e, "episode text index update failed");
        }
        if let Err(e) = self
            .c
            .episode_vectors
            .upsert(&ep.id.to_string(), &emb, &episode_meta(&ep))
            .await
        {
            tracing::warn!(error = %e, "episode vector index update failed");
        }
        Ok(ep)
    }

    /// The owner's L1 episodes, newest first (owner-only; L1 is private).
    pub async fn episodes(&self, owner: UserId, limit: u32) -> Result<Vec<Episode>> {
        let w = format!(
            "e.owner_id = ? ORDER BY e.created_at DESC LIMIT {}",
            limit.clamp(1, 1000)
        );
        Ok(
            fetch_episodes(&self.pool, &w, vec![Arg::Text(owner.to_string())])
                .await?
                .into_iter()
                .map(|(e, _)| e)
                .collect(),
        )
    }

    /// L1 → L2. With `llm_output` (the distiller model's answer), parse its
    /// JSON array of [`MemoryProposal`]s; otherwise use heuristics:
    /// * a failing command fixed by editing files → `procedure`
    ///   (subject = the error, relation `fixed_by`);
    /// * passing test / lint / build commands → `convention`
    ///   ("FP runs tests with `cargo test`").
    ///
    /// Each proposal gets a [`salience`] score; those below
    /// `promote_threshold` are dropped. The rest are proposed as the run
    /// owner with the owner's default visibility (`UserSettings.default_visibility`;
    /// "Shared" means the project's other task owners), through the normal
    /// dedupe/supersede/approval rules. Packs of the project are rebuilt.
    /// Returns the created or merged items.
    pub async fn extract(
        &self,
        episode: &Episode,
        llm_output: Option<&str>,
    ) -> Result<Vec<MemoryItem>> {
        let owner = episode.owner_id;
        let (default_vis, teammates) = self.default_visibility(owner, episode.project_id).await?;
        let succeeded = episode_succeeded(&episode.outcome);
        let mut proposals: Vec<(MemoryProposal, f32)> = match llm_output {
            Some(text) => parse_llm_proposals(text, episode.project_id, default_vis)
                .into_iter()
                .map(|p| (p, if succeeded { 1.0 } else { 0.3 }))
                .collect(),
            None => self.heuristic_proposals(episode, default_vis).await?,
        };
        for (p, _) in proposals.iter_mut() {
            if p.visibility == Visibility::Shared && p.shared_with.is_empty() {
                p.shared_with = teammates.clone();
            }
            if p.visibility == Visibility::Shared && p.shared_with.is_empty() {
                p.visibility = Visibility::Private;
            }
        }
        let human = self.human_signal(episode).await?;
        let label = self.source_label(episode).await?;
        let mut out = vec![];
        for (p, outcome) in proposals {
            let text = format!(
                "{}\n{} {} {}\n{}",
                p.title,
                p.subject,
                p.relation.replace('_', " "),
                p.object,
                p.body
            );
            let emb = self.embed_one(&text).await?;
            let filter = SearchFilter {
                viewer: owner,
                project: p.project_id,
                statuses: crate::RETRIEVABLE.to_vec(),
                kinds: None,
            };
            let nearest = self
                .c
                .item_vectors
                .search(&emb, 1, &filter)
                .await?
                .first()
                .map(|(_, s)| *s)
                .unwrap_or(0.0);
            let recurrence = self.recurrence(episode, &p.subject).await?;
            let score = salience(SalienceInputs {
                outcome,
                recurrence,
                novelty: 1.0 - nearest.max(0.0),
                human,
            });
            if score < self.cfg.promote_threshold {
                tracing::debug!(title = %p.title, score, "proposal below promote threshold; kept in L0/L1 only");
                continue;
            }
            let opts = ProposeOpts {
                salience: Some(score),
                source_label: label.clone(),
            };
            match self
                .propose_inner(owner, Some(episode.run_id), p, opts)
                .await
            {
                Ok(o) => out.push(o.item),
                Err(e @ (MemoryError::Invalid(_) | MemoryError::Forbidden(_))) => {
                    tracing::warn!(error = %e, "extracted proposal rejected");
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(project) = episode.project_id {
            self.rebuild_packs(owner, project).await?;
        }
        Ok(out)
    }

    async fn heuristic_proposals(
        &self,
        ep: &Episode,
        vis: Visibility,
    ) -> Result<Vec<(MemoryProposal, f32)>> {
        let rows = self.read_l0_rows(ep.run_id).await?;
        let events: Vec<RunEvent> = rows.iter().filter_map(L0Row::to_event).collect();
        let cmds = commands(&events);
        let project_name = self.project_label(ep.project_id).await?;
        let mut out: Vec<(MemoryProposal, f32)> = vec![];
        let mk =
            |kind, title: String, subject: String, relation: &str, object: String, body: String| {
                MemoryProposal {
                    kind,
                    title,
                    subject,
                    relation: relation.into(),
                    object,
                    body,
                    visibility: vis,
                    shared_with: vec![],
                    project_id: ep.project_id,
                }
            };
        // error → fix procedures
        for (i, c) in cmds.iter().enumerate() {
            if c.ok {
                continue;
            }
            let Some(pass) = cmds[i + 1..]
                .iter()
                .find(|p| p.ok && p.command == c.command)
            else {
                continue;
            };
            let changed = changed_between(&events, c.at, pass.at);
            if changed.is_empty() {
                continue; // flaky or environmental; nothing to learn
            }
            let err = error_line(&c.output);
            let subject = if err.is_empty() {
                format!("`{}` fails", c.command)
            } else {
                err.clone()
            };
            let object = format!(
                "editing {} and re-running `{}`",
                changed.join(", "),
                c.command
            );
            if out
                .iter()
                .any(|(p, _)| normalize(&p.subject) == normalize(&subject))
            {
                continue;
            }
            out.push((
                mk(
                    MemoryType::Procedure,
                    truncate(&format!("Fix: {subject}"), 120),
                    subject.clone(),
                    "fixed_by",
                    object,
                    format!(
                        "When `{}` fails with:\n    {}\nthe fix was to edit {} and re-run it.",
                        c.command,
                        if err.is_empty() { "(no output)" } else { &err },
                        changed.join(", ")
                    ),
                ),
                if episode_succeeded(&ep.outcome) {
                    1.0
                } else {
                    0.8
                },
            ));
        }
        // passing project commands → conventions
        let mut per_relation: HashMap<&str, usize> = HashMap::new();
        for c in cmds.iter().filter(|c| c.ok) {
            let Some(rel) = command_role(&c.command) else {
                continue;
            };
            let n = per_relation.entry(rel).or_default();
            if *n >= if rel == "runs_tests_with" { 2 } else { 1 } {
                continue;
            }
            let cmd = truncate(first_line(&c.command), 120);
            if out
                .iter()
                .any(|(p, _)| p.relation == rel && normalize(&p.object) == normalize(&cmd))
            {
                continue;
            }
            *n += 1;
            let verb = match rel {
                "runs_tests_with" => "runs tests with",
                "lints_with" => "lints with",
                _ => "builds with",
            };
            out.push((
                mk(
                    MemoryType::Convention,
                    format!("{project_name} {verb} `{cmd}`"),
                    project_name.clone(),
                    rel,
                    format!("`{cmd}`"),
                    format!("Observed passing in a {} run.", ep.role.label()),
                ),
                1.0,
            ));
        }
        Ok(out)
    }

    /// The owner's default visibility for extracted items and, for
    /// `Shared`, the project teammates (other owners of tasks in the project).
    async fn default_visibility(
        &self,
        owner: UserId,
        project: Option<ProjectId>,
    ) -> Result<(Visibility, Vec<UserId>)> {
        let settings: Option<Option<String>> =
            sqlx::query_scalar("SELECT settings_json FROM users WHERE id = ?")
                .bind(owner.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let vis = settings
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                v.get("default_visibility")
                    .and_then(|d| d.as_str())
                    .and_then(Visibility::parse)
            })
            .unwrap_or(Visibility::Private);
        let mut mates = vec![];
        if vis == Visibility::Shared
            && let Some(p) = project
        {
            let ids: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT owner_id FROM tasks WHERE project_id = ? AND owner_id <> ? ORDER BY owner_id",
            )
            .bind(p.to_string())
            .bind(owner.to_string())
            .fetch_all(&self.pool)
            .await?;
            mates = ids.iter().filter_map(|s| s.parse().ok()).collect();
        }
        Ok((vis, mates))
    }

    async fn human_signal(&self, ep: &Episode) -> Result<f32> {
        let Some(task) = ep.task_id else {
            return Ok(0.0);
        };
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM task_transitions WHERE task_id = ? AND actor IS NOT NULL",
        )
        .bind(task.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(if n > 0 { 1.0 } else { 0.0 })
    }

    async fn recurrence(&self, ep: &Episode, subject: &str) -> Result<u32> {
        let sig: String = normalize(subject).chars().take(60).collect();
        if sig.len() < 4 {
            return Ok(0);
        }
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM episodes WHERE owner_id = ? AND id <> ? AND project_id IS ? \
             AND instr(lower(goal || ' ' || errors_json || ' ' || fixes_json), ?) > 0",
        )
        .bind(ep.owner_id.to_string())
        .bind(ep.id.to_string())
        .bind(ep.project_id.map(|p| p.to_string()))
        .bind(&sig)
        .fetch_one(&self.pool)
        .await?;
        Ok(n as u32)
    }

    /// "FP-131 · Reviewer" (or just the role).
    async fn source_label(&self, ep: &Episode) -> Result<Option<String>> {
        let key: Option<String> = match ep.task_id {
            Some(t) => {
                sqlx::query_scalar("SELECT key FROM tasks WHERE id = ?")
                    .bind(t.to_string())
                    .fetch_optional(&self.pool)
                    .await?
            }
            None => None,
        };
        Ok(Some(match key {
            Some(k) => format!("{k} · {}", ep.role.label()),
            None => ep.role.label().to_string(),
        }))
    }

    /// Display name of a project for generated text ("FP" / "this project").
    pub(crate) async fn project_label(&self, project: Option<ProjectId>) -> Result<String> {
        let Some(p) = project else {
            return Ok("This project".into());
        };
        let key: Option<String> = sqlx::query_scalar("SELECT key FROM projects WHERE id = ?")
            .bind(p.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(key.unwrap_or_else(|| "This project".into()))
    }
}

/// Files changed between two event indices (exclusive).
fn changed_between(events: &[RunEvent], from: usize, to: usize) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for e in &events[from + 1..to.min(events.len())] {
        if let RunEventKind::FileChange { path, .. } = &e.kind
            && !out.contains(path)
        {
            out.push(path.clone());
        }
    }
    out.truncate(5);
    out
}

fn episode_succeeded(outcome: &str) -> bool {
    outcome.starts_with("succeeded")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salience_formula() {
        let s = salience(SalienceInputs {
            outcome: 1.0,
            recurrence: 0,
            novelty: 1.0,
            human: 0.0,
        });
        assert!((s - 0.6).abs() < 1e-6);
        let s = salience(SalienceInputs {
            outcome: 1.0,
            recurrence: 9,
            novelty: 1.0,
            human: 1.0,
        });
        assert!((s - 1.0).abs() < 1e-6);
        let s = salience(SalienceInputs {
            outcome: 0.3,
            recurrence: 0,
            novelty: 0.0,
            human: 0.0,
        });
        assert!(s < 0.35);
    }

    #[test]
    fn parses_fenced_and_wrapped_json() {
        let text = "Here you go:\n```json\n[{\"kind\":\"convention\",\"title\":\"t\",\"subject\":\"FP\",\
                    \"relation\":\"uses\",\"object\":\"tokio\",\"visibility\":\"company\"}]\n```\nthanks";
        let p = parse_llm_proposals(text, None, Visibility::Private);
        assert_eq!(p.len(), 1);
        assert_eq!(
            p[0].visibility,
            Visibility::Private,
            "capped at the owner's default"
        );
        let wrapped = r#"{"items": [{"type": "fact", "subject": "api", "object": "v2", "visibility": "private"}, {"bogus": 1}]}"#;
        let p = parse_llm_proposals(wrapped, None, Visibility::Company);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, MemoryType::Fact);
        assert_eq!(p[0].relation, "is");
        assert!(parse_llm_proposals("no json here", None, Visibility::Private).is_empty());
    }

    #[test]
    fn command_roles() {
        assert_eq!(
            command_role("cargo test --workspace"),
            Some("runs_tests_with")
        );
        assert_eq!(command_role("npx playwright test"), Some("runs_tests_with"));
        assert_eq!(
            command_role("cargo clippy -- -D warnings"),
            Some("lints_with")
        );
        assert_eq!(command_role("pnpm run build"), Some("builds_with"));
        assert_eq!(command_role("ls -la"), None);
    }

    #[test]
    fn error_line_prefers_error_text() {
        assert_eq!(
            error_line("Compiling x\nerror[E0425]: cannot find value `y`\n"),
            "error[E0425]: cannot find value `y`"
        );
        assert_eq!(error_line("just one line"), "just one line");
    }
}
