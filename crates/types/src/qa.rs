//! `qa.yaml` profile (PRD §8.2) and the structured QA report / review verdict
//! (PRD §8.4). Workers write these as JSON files into `/run/out/`.

use crate::ids::ArtifactId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Per-project QA profile. Deserialized from YAML by the daemon.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QaProfile {
    pub version: u32,
    /// Full custom image. When absent, the base worker image is used plus `extras`.
    #[serde(default)]
    pub image: Option<String>,
    /// Extra packages layered onto the base image (decision #2).
    #[serde(default)]
    pub extras: QaExtras,
    #[serde(default)]
    pub resources: QaResources,
    /// Sidecar services started by the daemon on a per-run network (decision #4).
    #[serde(default)]
    pub services: BTreeMap<String, QaService>,
    /// Commands run inside the QA container to boot the stack.
    #[serde(default)]
    pub boot: Vec<String>,
    #[serde(default)]
    pub seed: Vec<String>,
    #[serde(default)]
    pub checks: QaChecks,
    /// Globs (relative to the worktree) collected as artifacts.
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub loop_cap: Option<u32>,
    /// Per-role timeouts in minutes.
    #[serde(default)]
    pub timeouts: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct QaExtras {
    #[serde(default)]
    pub apt: Vec<String>,
    #[serde(default)]
    pub npm: Vec<String>,
    #[serde(default)]
    pub cargo: Vec<String>,
    #[serde(default)]
    pub pip: Vec<String>,
}

impl QaExtras {
    pub fn is_empty(&self) -> bool {
        self.apt.is_empty() && self.npm.is_empty() && self.cargo.is_empty() && self.pip.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QaResources {
    #[serde(default = "default_cpus")]
    pub cpus: f32,
    /// Docker-style size: "6g", "512m".
    #[serde(default = "default_memory")]
    pub memory: String,
}

fn default_cpus() -> f32 {
    2.0
}
fn default_memory() -> String {
    "4g".into()
}

impl Default for QaResources {
    fn default() -> Self {
        Self {
            cpus: default_cpus(),
            memory: default_memory(),
        }
    }
}

impl QaResources {
    pub fn memory_mb(&self) -> Option<u64> {
        parse_size_mb(&self.memory)
    }
}

/// Parse "6g", "512m", "1.5G", "2048" (MiB) into MiB.
pub fn parse_size_mb(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = if let Some(n) = s.strip_suffix("gb").or_else(|| s.strip_suffix('g')) {
        (n, 1024.0)
    } else if let Some(n) = s.strip_suffix("mb").or_else(|| s.strip_suffix('m')) {
        (n, 1.0)
    } else {
        (s.as_str(), 1.0)
    };
    let v: f64 = num.trim().parse().ok()?;
    (v > 0.0).then(|| (v * mult).round() as u64)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QaService {
    pub image: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Shell command run inside the service until it exits 0.
    #[serde(default)]
    pub healthcheck: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct QaChecks {
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub e2e: Option<String>,
    #[serde(default)]
    pub cli: Option<String>,
    #[serde(default)]
    pub extra: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassFail {
    Pass,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CriterionResult {
    pub criterion: String,
    pub status: PassFail,
    /// Artifact ids, or before ingest, paths relative to `/run/out`.
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct TestSummary {
    pub passed: u32,
    pub failed: u32,
    #[serde(default)]
    pub skipped: u32,
    #[serde(default)]
    pub running: u32,
    #[serde(default)]
    pub suites: Vec<TestSuite>,
    /// The first failing assertion, shown in a red box.
    #[serde(default)]
    pub failing_assertion: Option<String>,
}

impl TestSummary {
    pub fn total(&self) -> u32 {
        self.passed + self.failed + self.skipped + self.running
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TestSuite {
    pub name: String,
    pub passed: u32,
    pub failed: u32,
    #[serde(default)]
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QaReport {
    pub criteria: Vec<CriterionResult>,
    #[serde(default)]
    pub tests: TestSummary,
    #[serde(default)]
    pub summary: String,
    /// Scratch branch with tests QA wrote, if any (PRD §5.2).
    #[serde(default)]
    pub test_branch: Option<String>,
}

impl QaReport {
    pub fn passed(&self) -> bool {
        !self.criteria.is_empty()
            && self.criteria.iter().all(|c| c.status == PassFail::Pass)
            && self.tests.failed == 0
    }
    pub fn failures(&self) -> impl Iterator<Item = &CriterionResult> {
        self.criteria.iter().filter(|c| c.status == PassFail::Fail)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Approve,
    ChangesRequested,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Minor,
    Major,
    Critical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewVerdict {
    pub decision: Decision,
    #[serde(default)]
    pub findings: Vec<Finding>,
    pub summary: String,
}

impl ReviewVerdict {
    pub fn approved(&self) -> bool {
        self.decision == Decision::Approve
    }
}

/// Evidence references after ingest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub artifact_id: ArtifactId,
    pub caption: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size_mb("6g"), Some(6144));
        assert_eq!(parse_size_mb("512m"), Some(512));
        assert_eq!(parse_size_mb("1.5G"), Some(1536));
        assert_eq!(parse_size_mb("2048"), Some(2048));
        assert_eq!(parse_size_mb("x"), None);
    }

    #[test]
    fn report_pass_logic() {
        let mut r = QaReport {
            criteria: vec![CriterionResult {
                criterion: "a".into(),
                status: PassFail::Pass,
                evidence: vec![],
                notes: "".into(),
            }],
            tests: TestSummary::default(),
            summary: "".into(),
            test_branch: None,
        };
        assert!(r.passed());
        r.tests.failed = 1;
        assert!(!r.passed());
    }
}
