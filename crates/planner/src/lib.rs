//! `kiln-planner`: the role prompts — Kiln's "brain" — as pure templating.
//!
//! No I/O happens here. The scheduler renders a prompt (and, for each role,
//! a short system instruction) into `RunConfig { prompt, system }`; the
//! harness passes them to the model. Every template names the exact output
//! file the runner collects (`kiln_types::protocol::paths`) and shows the
//! JSON shape that file must have, and every example is unit-tested to
//! deserialize into the corresponding `kiln-types` struct.
//!
//! | Role      | System                 | Prompt                 | Output file                  |
//! |-----------|------------------------|------------------------|------------------------------|
//! | Planner   | [`planner_system`]     | [`planner_prompt`]     | `/run/kiln/out/plan.json` + chat reply |
//! | Builder   | [`builder_system`]     | [`builder_prompt`]     | commits + `/run/kiln/out/summary.md` |
//! | QA        | [`qa_system`]          | [`qa_prompt`]          | `/run/kiln/out/qa_report.json` |
//! | Reviewer  | [`reviewer_system`]    | [`reviewer_prompt`]    | `/run/kiln/out/verdict.json` |
//! | Distiller | [`distiller_system`]   | [`distiller_prompt`]   | `/run/kiln/out/memory.json` + identical reply |
//!
//! Helpers: [`plan_json_schema`], [`feedback_from`] (loop feedback for the
//! next Builder attempt), [`memory_file`] (`AGENTS.md` / `CLAUDE.md`).

#![warn(missing_docs)]

mod distiller;
mod feedback;
mod planner;
mod render;
mod schema;
mod worker;

pub use distiller::{
    MAX_EVENT_CHARS, MAX_EVENTS, MEMORY_EXAMPLE, MEMORY_OUT, distiller_prompt, distiller_system,
};
pub use feedback::{feedback_from, memory_file};
pub use planner::{MAX_HISTORY_MESSAGES, planner_prompt, planner_system};
pub use schema::{EXAMPLE_PLAN, plan_json_schema};
pub use worker::{
    QA_REPORT_EXAMPLE, VERDICT_EXAMPLE, builder_prompt, builder_system, qa_prompt, qa_system,
    reviewer_prompt, reviewer_system,
};

use kiln_types::WorkerRole;

/// System instructions for any role.
pub fn role_system(role: WorkerRole) -> String {
    match role {
        WorkerRole::Planner => planner_system(),
        WorkerRole::Builder => builder_system(),
        WorkerRole::Qa => qa_system(),
        WorkerRole::Reviewer => reviewer_system(),
        WorkerRole::Distiller => distiller_system(),
    }
}
