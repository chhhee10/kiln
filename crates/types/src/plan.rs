//! Plan schema (PRD §8.3). The Planner must emit JSON matching `PlanSpec`;
//! `PlanSpec::validate` is the authoritative check.

use crate::status::WorkerRole;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Harness + model for one worker role. Both are free-form configuration
/// strings so new models need no release (PRD §7.8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerChoice {
    pub harness: String,
    pub model: String,
}

impl WorkerChoice {
    pub fn new(harness: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            harness: harness.into(),
            model: model.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct PlanWorkers {
    pub builder: Option<WorkerChoice>,
    pub qa: Option<WorkerChoice>,
    pub reviewer: Option<WorkerChoice>,
}

impl PlanWorkers {
    pub fn get(&self, role: WorkerRole) -> Option<&WorkerChoice> {
        match role {
            WorkerRole::Builder => self.builder.as_ref(),
            WorkerRole::Qa => self.qa.as_ref(),
            WorkerRole::Reviewer => self.reviewer.as_ref(),
            _ => None,
        }
    }
    /// Fill missing roles from the user's defaults.
    pub fn resolve(
        &self,
        defaults: &BTreeMap<WorkerRole, WorkerChoice>,
    ) -> BTreeMap<WorkerRole, WorkerChoice> {
        WorkerRole::PIPELINE
            .into_iter()
            .filter_map(|r| {
                self.get(r)
                    .or_else(|| defaults.get(&r))
                    .cloned()
                    .map(|c| (r, c))
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub files_hint: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub workers: PlanWorkers,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanSpec {
    pub feature: String,
    #[serde(default)]
    pub description: String,
    pub tasks: Vec<TaskSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("plan has no tasks")]
    NoTasks,
    #[error("feature title is empty")]
    EmptyFeature,
    #[error("task {0}: title is empty")]
    EmptyTitle(String),
    #[error("task {0}: needs at least one acceptance criterion")]
    NoCriteria(String),
    #[error("duplicate task id {0}")]
    DuplicateId(String),
    #[error("task {task} depends on unknown task {dep}")]
    UnknownDependency { task: String, dep: String },
    #[error("dependency cycle involving {0}")]
    Cycle(String),
    #[error("task {0}: id may only contain [A-Za-z0-9_-]")]
    BadId(String),
}

impl PlanSpec {
    /// Validate structure: unique ids, criteria present, dependencies known
    /// and acyclic.
    pub fn validate(&self) -> Result<(), PlanError> {
        if self.feature.trim().is_empty() {
            return Err(PlanError::EmptyFeature);
        }
        if self.tasks.is_empty() {
            return Err(PlanError::NoTasks);
        }
        let mut ids = BTreeSet::new();
        for t in &self.tasks {
            if t.id.is_empty()
                || !t
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(PlanError::BadId(t.id.clone()));
            }
            if !ids.insert(t.id.as_str()) {
                return Err(PlanError::DuplicateId(t.id.clone()));
            }
            if t.title.trim().is_empty() {
                return Err(PlanError::EmptyTitle(t.id.clone()));
            }
            if t.acceptance_criteria.iter().all(|c| c.trim().is_empty()) {
                return Err(PlanError::NoCriteria(t.id.clone()));
            }
        }
        for t in &self.tasks {
            for d in &t.depends_on {
                if !ids.contains(d.as_str()) {
                    return Err(PlanError::UnknownDependency {
                        task: t.id.clone(),
                        dep: d.clone(),
                    });
                }
            }
        }
        self.topo_order().map(|_| ())
    }

    /// Task ids in dependency order (dependencies first).
    pub fn topo_order(&self) -> Result<Vec<String>, PlanError> {
        let by_id: BTreeMap<&str, &TaskSpec> =
            self.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        let mut out = Vec::new();
        let mut state: BTreeMap<&str, u8> = BTreeMap::new(); // 1 = visiting, 2 = done
        fn visit<'a>(
            id: &'a str,
            by_id: &BTreeMap<&'a str, &'a TaskSpec>,
            state: &mut BTreeMap<&'a str, u8>,
            out: &mut Vec<String>,
        ) -> Result<(), PlanError> {
            match state.get(id) {
                Some(2) => return Ok(()),
                Some(1) => return Err(PlanError::Cycle(id.to_string())),
                _ => {}
            }
            state.insert(id, 1);
            if let Some(t) = by_id.get(id) {
                for d in &t.depends_on {
                    visit(d.as_str(), by_id, state, out)?;
                }
            }
            state.insert(id, 2);
            out.push(id.to_string());
            Ok(())
        }
        for t in &self.tasks {
            visit(t.id.as_str(), &by_id, &mut state, &mut out)?;
        }
        Ok(out)
    }

    /// Upper bound on worker runs: tasks × 3 roles × (loop cap + ... ) as
    /// shown in the plan footer: "up to N×9 worker runs".
    pub fn max_worker_runs(&self, loop_cap: u32) -> usize {
        self.tasks.len() * 3 * loop_cap as usize
    }

    /// Extract a plan from planner output: accepts raw JSON or the first
    /// fenced ```json block.
    pub fn from_planner_output(text: &str) -> Result<PlanSpec, String> {
        let trimmed = text.trim();
        if let Ok(p) = serde_json::from_str::<PlanSpec>(trimmed) {
            return Ok(p);
        }
        let mut rest = trimmed;
        while let Some(start) = rest.find("```") {
            let after = &rest[start + 3..];
            let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
            let body = &after[body_start..];
            let Some(end) = body.find("```") else { break };
            if let Ok(p) = serde_json::from_str::<PlanSpec>(body[..end].trim()) {
                return Ok(p);
            }
            rest = &body[end + 3..];
        }
        // Last resort: outermost braces.
        if let (Some(a), Some(b)) = (trimmed.find('{'), trimmed.rfind('}'))
            && a < b
        {
            return serde_json::from_str::<PlanSpec>(&trimmed[a..=b]).map_err(|e| e.to_string());
        }
        Err("no plan JSON found in planner output".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, deps: &[&str]) -> TaskSpec {
        TaskSpec {
            id: id.into(),
            title: format!("task {id}"),
            description: String::new(),
            acceptance_criteria: vec!["works".into()],
            files_hint: vec![],
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            workers: Default::default(),
        }
    }

    #[test]
    fn valid_plan_orders_deps() {
        let p = PlanSpec {
            feature: "f".into(),
            description: "".into(),
            tasks: vec![t("t2", &["t1"]), t("t1", &[])],
        };
        p.validate().unwrap();
        assert_eq!(p.topo_order().unwrap(), vec!["t1", "t2"]);
    }

    #[test]
    fn rejects_cycles_and_unknown() {
        let p = PlanSpec {
            feature: "f".into(),
            description: "".into(),
            tasks: vec![t("a", &["b"]), t("b", &["a"])],
        };
        assert!(matches!(p.validate(), Err(PlanError::Cycle(_))));
        let p = PlanSpec {
            feature: "f".into(),
            description: "".into(),
            tasks: vec![t("a", &["z"])],
        };
        assert!(matches!(
            p.validate(),
            Err(PlanError::UnknownDependency { .. })
        ));
        let p = PlanSpec {
            feature: "f".into(),
            description: "".into(),
            tasks: vec![t("a", &[]), t("a", &[])],
        };
        assert!(matches!(p.validate(), Err(PlanError::DuplicateId(_))));
    }

    #[test]
    fn parses_fenced_output() {
        let txt = "Here's the plan:\n```json\n{\"feature\":\"x\",\"tasks\":[{\"id\":\"t1\",\"title\":\"a\",\"acceptance_criteria\":[\"b\"]}]}\n```\nDone.";
        let p = PlanSpec::from_planner_output(txt).unwrap();
        assert_eq!(p.tasks[0].id, "t1");
    }
}
