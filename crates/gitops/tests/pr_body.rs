//! Snapshot-style checks for `render_pr_body`.

use chrono::Utc;
use kiln_gitops::{ArtifactLink, render_pr_body};
use kiln_types::plan::{PlanSpec, TaskSpec};
use kiln_types::qa::*;
use kiln_types::*;
use std::collections::BTreeMap;

fn task() -> Task {
    Task {
        id: TaskId::new(),
        plan_id: PlanId::new(),
        project_id: ProjectId::new(),
        owner_id: UserId::new(),
        number: 142,
        key: "FP-142".into(),
        spec_id: "t1".into(),
        title: "Retry backoff".into(),
        description: "Retry failed webhook deliveries with exponential backoff.".into(),
        acceptance: vec![
            "Retries 5 times with doubling delay".into(),
            "Gives up after 5 attempts and logs | error".into(),
        ],
        files_hint: vec!["src/webhook.rs".into()],
        depends_on: vec![],
        workers: BTreeMap::new(),
        state: TaskState::Reviewing,
        last_active: None,
        loop_count: 1,
        loop_cap: 3,
        branch: Some("kiln/fp-142-retry-backoff".into()),
        pr_url: None,
        pr_number: None,
        progress: 0,
        meta: String::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn plan() -> PlanSpec {
    PlanSpec {
        feature: "Reliable webhooks".into(),
        description: "Make webhook delivery survive outages.\n\nSecond paragraph not shown.".into(),
        tasks: vec![TaskSpec {
            id: "t1".into(),
            title: "Retry backoff".into(),
            description: String::new(),
            acceptance_criteria: vec![],
            files_hint: vec![],
            depends_on: vec![],
            workers: Default::default(),
        }],
    }
}

fn qa() -> QaReport {
    QaReport {
        criteria: vec![
            CriterionResult {
                criterion: "Retries 5 times with doubling delay".into(),
                status: PassFail::Pass,
                evidence: vec!["artifacts/retry.log".into(), "artifacts/timing.png".into()],
                notes: String::new(),
            },
            CriterionResult {
                criterion: "Gives up after 5 attempts and logs | error".into(),
                status: PassFail::Fail,
                evidence: vec![],
                notes: "Logged at WARN, not ERROR".into(),
            },
        ],
        tests: TestSummary {
            passed: 41,
            failed: 1,
            skipped: 2,
            running: 0,
            suites: vec![TestSuite {
                name: "webhook".into(),
                passed: 12,
                failed: 1,
                duration_ms: 2300,
            }],
            failing_assertion: Some("assert_eq!(level, Level::Error)".into()),
        },
        summary: "One criterion failed.".into(),
        test_branch: Some("kiln/fp-142-retry-backoff-qa".into()),
    }
}

fn verdict() -> ReviewVerdict {
    ReviewVerdict {
        decision: Decision::ChangesRequested,
        findings: vec![
            Finding {
                severity: Severity::Minor,
                file: None,
                line: None,
                message: "Consider a jitter".into(),
            },
            Finding {
                severity: Severity::Major,
                file: Some("src/webhook.rs".into()),
                line: Some(88),
                message: "Unbounded sleep".into(),
            },
        ],
        summary: "Solid, one blocking issue.".into(),
    }
}

const EXPECTED: &str = r#"## FP-142 · Retry backoff

> Part of **Reliable webhooks**
> Make webhook delivery survive outages.

Retry failed webhook deliveries with exponential backoff.

### Acceptance criteria

- [x] Retries 5 times with doubling delay
- [ ] Gives up after 5 attempts and logs | error

### QA

**✕ QA reported failures.**

One criterion failed.

| Criterion | Status | Evidence |
|---|---|---|
| Retries 5 times with doubling delay | ✓ pass | [retry.log](https://kiln.example/a/1), `artifacts/timing.png` |
| Gives up after 5 attempts and logs \| error | ✕ fail | — |

<details><summary>Failure notes</summary>

- **Gives up after 5 attempts and logs | error** — Logged at WARN, not ERROR

</details>

**Tests:** 41 passed · 1 failed · 2 skipped

| Suite | Passed | Failed | Duration |
|---|---:|---:|---:|
| webhook | 12 | 1 | 2.3s |

First failing assertion:

```
assert_eq!(level, Level::Error)
```

QA tests are on branch `kiln/fp-142-retry-backoff-qa`.

### Review

**Verdict:** ✕ Changes requested

Solid, one blocking issue.

| Severity | Location | Finding |
|---|---|---|
| major | `src/webhook.rs:88` | Unbounded sleep |
| minor | — | Consider a jitter |

### Artifacts

- [retry.log](https://kiln.example/a/1)

---
<sub>Opened by Kiln · attempt 2/3</sub>
"#;

#[test]
fn full_body_snapshot() {
    let links = vec![ArtifactLink {
        key: "artifacts/retry.log".into(),
        label: "retry.log".into(),
        url: "https://kiln.example/a/1".into(),
    }];
    let body = render_pr_body(&plan(), &task(), Some(&qa()), Some(&verdict()), &links);
    assert_eq!(body, EXPECTED, "\n--- got ---\n{body}");
}

#[test]
fn minimal_body_without_reports() {
    let mut t = task();
    t.loop_count = 0;
    t.description = String::new();
    let body = render_pr_body(&plan(), &t, None, None, &[]);
    assert!(body.contains("- [ ] Retries 5 times with doubling delay"));
    assert!(body.contains("_No QA report._"));
    assert!(body.contains("_No review verdict._"));
    assert!(!body.contains("### Artifacts"));
    assert!(body.ends_with("<sub>Opened by Kiln · attempt 1/3</sub>\n"));
}

#[test]
fn positional_match_and_approval() {
    let mut r = qa();
    // QA paraphrased the criteria; positions still line up.
    r.criteria[0].criterion = "retries five times".into();
    r.criteria[1].criterion = "gives up".into();
    r.criteria[1].status = PassFail::Pass;
    r.tests.failed = 0;
    r.tests.failing_assertion = Some("contains ``` fence".into());
    let v = ReviewVerdict {
        decision: Decision::Approve,
        findings: vec![],
        summary: "LGTM".into(),
    };
    let body = render_pr_body(&plan(), &task(), Some(&r), Some(&v), &[]);
    assert!(body.contains("- [x] Retries 5 times with doubling delay"));
    assert!(body.contains("- [x] Gives up after 5 attempts"));
    assert!(body.contains("**✓ All criteria passed.**"));
    assert!(body.contains("**Verdict:** ✓ Approved"));
    assert!(body.contains("````\ncontains ``` fence\n````"));
    assert!(!body.contains("Failure notes"));
}
