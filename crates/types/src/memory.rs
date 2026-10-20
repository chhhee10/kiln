//! Company brain (PRD §5.4, §7.9).

use crate::ids::*;
use crate::model::Timestamp;
use crate::status::WorkerRole;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Private,
    /// Shared with the users in `shared_with`.
    Shared,
    Company,
}

impl Visibility {
    pub fn glyph(self) -> &'static str {
        match self {
            Visibility::Private => "◌",
            Visibility::Shared => "◑",
            Visibility::Company => "●",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Visibility::Private => "Private",
            Visibility::Shared => "Shared",
            Visibility::Company => "Company",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Shared => "shared",
            Visibility::Company => "company",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "private" => Some(Self::Private),
            "shared" => Some(Self::Shared),
            "company" => Some(Self::Company),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    Fact,
    Convention,
    Decision,
    Procedure,
    /// Personal preference: always Private, can never be shared (PRD §5.4).
    Preference,
    /// L1 episode surfaced as a memory row.
    Episode,
}

impl MemoryType {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryType::Fact => "fact",
            MemoryType::Convention => "convention",
            MemoryType::Decision => "decision",
            MemoryType::Procedure => "procedure",
            MemoryType::Preference => "preference",
            MemoryType::Episode => "episode",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Fact,
            Self::Convention,
            Self::Decision,
            Self::Procedure,
            Self::Preference,
            Self::Episode,
        ]
        .into_iter()
        .find(|t| t.as_str() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    /// Live and retrievable at its current visibility.
    Active,
    /// Proposed for Shared/Company; awaiting the owner's approval. Retrieval
    /// treats it as Private to the owner until approved.
    Pending,
    /// Flagged stale or harmful by feedback / nightly job.
    Flagged,
    Superseded,
    Archived,
    Rejected,
}

impl MemoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryStatus::Active => "active",
            MemoryStatus::Pending => "pending",
            MemoryStatus::Flagged => "flagged",
            MemoryStatus::Superseded => "superseded",
            MemoryStatus::Archived => "archived",
            MemoryStatus::Rejected => "rejected",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Active,
            Self::Pending,
            Self::Flagged,
            Self::Superseded,
            Self::Archived,
            Self::Rejected,
        ]
        .into_iter()
        .find(|t| t.as_str() == s)
    }
}

/// An L2 knowledge item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: MemoryId,
    pub owner_id: UserId,
    pub kind: MemoryType,
    pub project_id: Option<ProjectId>,
    pub visibility: Visibility,
    pub shared_with: Vec<UserId>,
    /// Requested visibility while `status == Pending`.
    pub requested_visibility: Option<Visibility>,
    pub title: String,
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub body: String,
    pub status: MemoryStatus,
    pub salience: f32,
    pub source_run_id: Option<RunId>,
    /// Display: "FP-131 · Reviewer".
    pub source_label: Option<String>,
    pub use_count: u32,
    pub last_used_at: Option<Timestamp>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl MemoryItem {
    /// The central visibility rule. Every read goes through this.
    pub fn visible_to(&self, user: UserId) -> bool {
        if self.owner_id == user {
            return true;
        }
        if matches!(self.status, MemoryStatus::Pending | MemoryStatus::Rejected) {
            return false;
        }
        if self.kind == MemoryType::Preference {
            return false;
        }
        match self.visibility {
            Visibility::Private => false,
            Visibility::Shared => self.shared_with.contains(&user),
            Visibility::Company => true,
        }
    }
}

/// An item as proposed by the extractor or a worker's `propose_memory` call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryProposal {
    pub kind: MemoryType,
    pub title: String,
    pub subject: String,
    pub relation: String,
    pub object: String,
    #[serde(default)]
    pub body: String,
    pub visibility: Visibility,
    #[serde(default)]
    pub shared_with: Vec<UserId>,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryHit {
    pub item: MemoryItem,
    pub score: f32,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct MemoryQuery {
    pub q: String,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub visibility: Option<Visibility>,
    #[serde(default)]
    pub kind: Option<MemoryType>,
    #[serde(default)]
    pub pending_only: bool,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Token budget for packed results (worker MCP use).
    #[serde(default)]
    pub token_budget: Option<u32>,
}

/// L1 episode: one row per run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    pub id: EpisodeId,
    pub run_id: RunId,
    pub task_id: Option<TaskId>,
    pub owner_id: UserId,
    pub project_id: Option<ProjectId>,
    pub role: WorkerRole,
    pub goal: String,
    pub outcome: String,
    pub errors: Vec<String>,
    pub fixes: Vec<String>,
    pub files: Vec<String>,
    pub duration_secs: i64,
    pub cost_usd: f64,
    pub visibility: Visibility,
    pub created_at: Timestamp,
}

/// "Recently used by" entries on the memory detail pane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryUse {
    pub task_key: String,
    pub role: WorkerRole,
    pub at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn item(
        owner: UserId,
        vis: Visibility,
        kind: MemoryType,
        status: MemoryStatus,
        shared: Vec<UserId>,
    ) -> MemoryItem {
        MemoryItem {
            id: MemoryId::new(),
            owner_id: owner,
            kind,
            project_id: None,
            visibility: vis,
            shared_with: shared,
            requested_visibility: None,
            title: "t".into(),
            subject: "s".into(),
            relation: "r".into(),
            object: "o".into(),
            body: "".into(),
            status,
            salience: 0.5,
            source_run_id: None,
            source_label: None,
            use_count: 0,
            last_used_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn visibility_rules() {
        let (a, b, c) = (UserId::new(), UserId::new(), UserId::new());
        use MemoryStatus::*;
        use MemoryType::*;
        use Visibility::*;
        assert!(item(a, Private, Fact, Active, vec![]).visible_to(a));
        assert!(!item(a, Private, Fact, Active, vec![]).visible_to(b));
        assert!(item(a, Shared, Fact, Active, vec![b]).visible_to(b));
        assert!(!item(a, Shared, Fact, Active, vec![b]).visible_to(c));
        assert!(item(a, Company, Fact, Active, vec![]).visible_to(c));
        assert!(!item(a, Company, Fact, Pending, vec![]).visible_to(c));
        assert!(!item(a, Company, Preference, Active, vec![]).visible_to(c));
    }
}
