//! Compact JSON helpers behind the workers' MCP tools
//! (`search_memory`, `propose_memory`), served by the daemon over the
//! runner socket (`MemoryCall` in `kiln_types::protocol`).

use crate::Memory;
use crate::error::Result;
use crate::gateway::{ProposeAction, UseContext};
use crate::util::truncate;
use kiln_types::memory::{MemoryProposal, MemoryQuery, MemoryStatus};
use kiln_types::{ProjectId, RunId, UserId};
use serde_json::{Value, json};

/// Token budget for one MCP search answer.
const MCP_TOKEN_BUDGET: u32 = 1200;
const MCP_MAX_LIMIT: u32 = 20;

impl Memory {
    /// `search_memory` for a worker acting as `user`. Same gateway path as
    /// [`Memory::search_ctx`] (visibility-filtered, budgeted to ≈1200
    /// tokens); uses are recorded under `ctx` so feedback can credit them.
    ///
    /// Shape: `{"count": n, "results": [{"id","kind","title","fact","body",
    /// "visibility","status","source","score"}]}`.
    pub async fn mcp_search(
        &self,
        user: UserId,
        project: Option<ProjectId>,
        query: &str,
        limit: Option<u32>,
        ctx: &UseContext,
    ) -> Result<Value> {
        let q = MemoryQuery {
            q: query.to_string(),
            project_id: project,
            limit: Some(limit.unwrap_or(5).clamp(1, MCP_MAX_LIMIT)),
            token_budget: Some(MCP_TOKEN_BUDGET),
            ..Default::default()
        };
        let hits = self.search_ctx(user, &q, ctx).await?;
        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                let i = &h.item;
                json!({
                    "id": i.id.to_string(),
                    "kind": i.kind.as_str(),
                    "title": i.title,
                    "fact": format!("{} {} {}", i.subject, i.relation.replace('_', " "), i.object),
                    "body": truncate(&i.body, 600),
                    "visibility": i.visibility.as_str(),
                    "status": i.status.as_str(),
                    "source": i.source_label,
                    "score": (h.score * 1000.0).round() / 1000.0,
                })
            })
            .collect();
        Ok(json!({ "count": results.len(), "results": results }))
    }

    /// `propose_memory` from a worker acting as `user` during `run`. When
    /// the proposal has no project, the run's task project is used.
    ///
    /// Shape: `{"id","action":"created|merged","status","visibility",
    /// "requested_visibility","message"}`.
    pub async fn mcp_propose(
        &self,
        user: UserId,
        run: Option<RunId>,
        mut proposal: MemoryProposal,
    ) -> Result<Value> {
        if proposal.project_id.is_none()
            && let Some(r) = run
        {
            let project: Option<Option<String>> = sqlx::query_scalar(
                "SELECT t.project_id FROM runs r LEFT JOIN tasks t ON t.id = r.task_id WHERE r.id = ?",
            )
            .bind(r.to_string())
            .fetch_optional(&self.pool)
            .await?;
            proposal.project_id = project.flatten().and_then(|p| p.parse().ok());
        }
        let out = self.propose_detailed(user, run, proposal).await?;
        let i = &out.item;
        let message = match (out.action, i.status) {
            (ProposeAction::Merged, _) => "already known; merged into the existing memory",
            (_, MemoryStatus::Pending) => {
                "saved privately; waiting for the owner's approval to share"
            }
            _ => "saved",
        };
        Ok(json!({
            "id": i.id.to_string(),
            "action": match out.action { ProposeAction::Created => "created", ProposeAction::Merged => "merged" },
            "status": i.status.as_str(),
            "visibility": i.visibility.as_str(),
            "requested_visibility": i.requested_visibility.map(|v| v.as_str()),
            "message": message,
        }))
    }
}
