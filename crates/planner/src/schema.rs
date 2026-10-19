//! The plan JSON Schema and a worked example.

/// JSON Schema (draft 2020-12) for [`kiln_types::plan::PlanSpec`].
///
/// Hand-written to mirror the serde shape exactly: optional fields are those
/// with `#[serde(default)]`; `additionalProperties: false` everywhere so the
/// Planner cannot invent keys. `PlanSpec::validate` remains the authoritative
/// semantic check (unique ids, known and acyclic dependencies).
pub fn plan_json_schema() -> &'static str {
    PLAN_SCHEMA
}

const PLAN_SCHEMA: &str = r##"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://kiln.local/schemas/plan.json",
  "title": "Kiln plan",
  "description": "A feature broken into small, independently verifiable tasks.",
  "type": "object",
  "additionalProperties": false,
  "required": ["feature", "tasks"],
  "properties": {
    "feature": {
      "type": "string",
      "minLength": 1,
      "description": "Short feature title, e.g. \"Retry webhook deliveries\"."
    },
    "description": {
      "type": "string",
      "description": "One or two paragraphs: the goal, the approach, assumptions and what is out of scope."
    },
    "tasks": {
      "type": "array",
      "minItems": 1,
      "items": { "$ref": "#/$defs/task" }
    }
  },
  "$defs": {
    "task": {
      "type": "object",
      "additionalProperties": false,
      "required": ["id", "title", "acceptance_criteria"],
      "properties": {
        "id": {
          "type": "string",
          "pattern": "^[A-Za-z0-9_-]+$",
          "description": "Stable id within the plan: t1, t2, ..."
        },
        "title": {
          "type": "string",
          "minLength": 1,
          "description": "Imperative, under 60 characters, e.g. \"Add exponential backoff to webhook sender\"."
        },
        "description": {
          "type": "string",
          "description": "What to change and why, design decisions, existing code to follow, new files, out-of-scope notes."
        },
        "acceptance_criteria": {
          "type": "array",
          "minItems": 1,
          "items": { "type": "string", "minLength": 1 },
          "description": "Concrete, observable, testable statements. QA reports pass/fail for each one."
        },
        "files_hint": {
          "type": "array",
          "items": { "type": "string" },
          "description": "Repo-relative paths likely to be touched (verified to exist, or new)."
        },
        "depends_on": {
          "type": "array",
          "uniqueItems": true,
          "items": { "type": "string", "pattern": "^[A-Za-z0-9_-]+$" },
          "description": "Ids of tasks whose changes must land first. Only when truly necessary."
        },
        "workers": { "$ref": "#/$defs/workers" }
      }
    },
    "workers": {
      "type": "object",
      "additionalProperties": false,
      "description": "Per-role harness/model overrides. Omit to use the user's defaults.",
      "properties": {
        "builder": { "anyOf": [{ "$ref": "#/$defs/worker" }, { "type": "null" }] },
        "qa": { "anyOf": [{ "$ref": "#/$defs/worker" }, { "type": "null" }] },
        "reviewer": { "anyOf": [{ "$ref": "#/$defs/worker" }, { "type": "null" }] }
      }
    },
    "worker": {
      "type": "object",
      "additionalProperties": false,
      "required": ["harness", "model"],
      "properties": {
        "harness": { "type": "string", "minLength": 1, "description": "claude_sdk | codex | opencode | ..." },
        "model": { "type": "string", "minLength": 1 }
      }
    }
  }
}"##;

/// A complete, valid example plan shown to the Planner.
pub const EXAMPLE_PLAN: &str = r#"{
  "feature": "Export runs as CSV",
  "description": "Users need to export the run history for billing. Add a CSV export to the API and a download button in the web app. Assumes the existing pagination limits (max 10k rows) are acceptable; streaming larger exports is out of scope.",
  "tasks": [
    {
      "id": "t1",
      "title": "Add GET /api/runs/export.csv endpoint",
      "description": "Add a handler next to the JSON list handler in src/api/runs.rs that reuses RunFilter and writes CSV with the csv crate (already a dependency). Columns: id, task, role, state, started_at (RFC 3339), duration_secs, cost_usd. Same auth and per-user filtering as GET /api/runs.",
      "acceptance_criteria": [
        "GET /api/runs/export.csv returns 200 with Content-Type text/csv and a header row: id,task,role,state,started_at,duration_secs,cost_usd",
        "The CSV contains exactly the runs that GET /api/runs returns for the same query parameters",
        "A request without a session returns 401 and no CSV body",
        "Fields containing commas or quotes are quoted per RFC 4180"
      ],
      "files_hint": ["src/api/runs.rs", "src/api/mod.rs", "tests/api_runs.rs"],
      "depends_on": []
    },
    {
      "id": "t2",
      "title": "Add Export CSV button to the runs page",
      "description": "Add a secondary button to the runs page toolbar (web/src/pages/runs.tsx) that downloads /api/runs/export.csv with the page's current filters. Follow the existing Button component and toolbar layout.",
      "acceptance_criteria": [
        "The runs page shows an \"Export CSV\" button in the toolbar",
        "Clicking it downloads a file named runs-YYYY-MM-DD.csv whose rows match the filtered list on screen",
        "While the download is in progress the button is disabled and shows a spinner"
      ],
      "files_hint": ["web/src/pages/runs.tsx"],
      "depends_on": ["t1"]
    }
  ]
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_types::plan::{PlanSpec, PlanWorkers, TaskSpec, WorkerChoice};
    use serde_json::Value;

    /// Minimal structural JSON Schema checker for the subset used above:
    /// `$ref` (local), `anyOf`, `type`, `required`, `properties`,
    /// `additionalProperties: false`, `items`, `minItems`, `minLength`,
    /// `uniqueItems` and a `^[A-Za-z0-9_-]+$` pattern.
    fn check(schema: &Value, root: &Value, v: &Value, at: &str) -> Result<(), String> {
        if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
            let name = r
                .strip_prefix("#/$defs/")
                .ok_or(format!("unsupported ref {r}"))?;
            return check(&root["$defs"][name], root, v, at);
        }
        if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
            return if any.iter().any(|s| check(s, root, v, at).is_ok()) {
                Ok(())
            } else {
                Err(format!("{at}: matches no anyOf branch"))
            };
        }
        let ty = schema.get("type").and_then(Value::as_str).unwrap_or("any");
        match (ty, v) {
            ("null", Value::Null) | ("any", _) => {}
            ("string", Value::String(s)) => {
                let min = schema.get("minLength").and_then(Value::as_u64).unwrap_or(0) as usize;
                if s.chars().count() < min {
                    return Err(format!("{at}: shorter than {min}"));
                }
                if schema.get("pattern").is_some()
                    && !s
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(format!("{at}: pattern mismatch"));
                }
            }
            ("array", Value::Array(items)) => {
                let min = schema.get("minItems").and_then(Value::as_u64).unwrap_or(0) as usize;
                if items.len() < min {
                    return Err(format!("{at}: fewer than {min} items"));
                }
                if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
                    let set: std::collections::BTreeSet<String> =
                        items.iter().map(|i| i.to_string()).collect();
                    if set.len() != items.len() {
                        return Err(format!("{at}: duplicate items"));
                    }
                }
                if let Some(item_schema) = schema.get("items") {
                    for (i, item) in items.iter().enumerate() {
                        check(item_schema, root, item, &format!("{at}[{i}]"))?;
                    }
                }
            }
            ("object", Value::Object(map)) => {
                let props = schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                for req in schema
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let k = req.as_str().unwrap();
                    if !map.contains_key(k) {
                        return Err(format!("{at}: missing required {k}"));
                    }
                }
                for (k, val) in map {
                    match props.get(k) {
                        Some(s) => check(s, root, val, &format!("{at}.{k}"))?,
                        None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                            return Err(format!("{at}: unexpected key {k}"));
                        }
                        None => {}
                    }
                }
            }
            (t, other) => return Err(format!("{at}: expected {t}, got {other}")),
        }
        Ok(())
    }

    fn validate(v: &Value) -> Result<(), String> {
        let schema: Value = serde_json::from_str(plan_json_schema()).expect("schema is valid JSON");
        check(&schema, &schema, v, "$")
    }

    fn sample() -> PlanSpec {
        PlanSpec {
            feature: "Retry webhooks".into(),
            description: "desc".into(),
            tasks: vec![
                TaskSpec {
                    id: "t1".into(),
                    title: "Backoff".into(),
                    description: "d".into(),
                    acceptance_criteria: vec!["retries 5 times".into()],
                    files_hint: vec!["src/a.rs".into()],
                    depends_on: vec![],
                    workers: PlanWorkers {
                        builder: Some(WorkerChoice::new("claude_sdk", "claude-opus")),
                        qa: None,
                        reviewer: Some(WorkerChoice::new("codex", "gpt-5")),
                    },
                },
                TaskSpec {
                    id: "t2".into(),
                    title: "UI".into(),
                    description: String::new(),
                    acceptance_criteria: vec!["shows a badge".into()],
                    files_hint: vec![],
                    depends_on: vec!["t1".into()],
                    workers: Default::default(),
                },
            ],
        }
    }

    #[test]
    fn schema_is_valid_json_with_expected_shape() {
        let s: Value = serde_json::from_str(plan_json_schema()).unwrap();
        assert_eq!(s["required"], serde_json::json!(["feature", "tasks"]));
        assert_eq!(
            s["$defs"]["task"]["required"],
            serde_json::json!(["id", "title", "acceptance_criteria"])
        );
        let task_props: Vec<&String> = s["$defs"]["task"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect();
        for k in [
            "id",
            "title",
            "description",
            "acceptance_criteria",
            "files_hint",
            "depends_on",
            "workers",
        ] {
            assert!(task_props.iter().any(|p| *p == k), "schema lacks task.{k}");
        }
    }

    #[test]
    fn serialized_plan_spec_matches_schema() {
        let v = serde_json::to_value(sample()).unwrap();
        validate(&v).unwrap();
        // Every key serde emits is known to the schema (additionalProperties: false).
        let minimal: Value = serde_json::from_str(
            r#"{"feature":"x","tasks":[{"id":"t1","title":"a","acceptance_criteria":["b"]}]}"#,
        )
        .unwrap();
        validate(&minimal).unwrap();
        let p: PlanSpec = serde_json::from_value(minimal).unwrap();
        p.validate().unwrap();
    }

    #[test]
    fn schema_rejects_bad_shapes() {
        let bad = [
            r#"{"tasks":[{"id":"t1","title":"a","acceptance_criteria":["b"]}]}"#,
            r#"{"feature":"x","tasks":[]}"#,
            r#"{"feature":"x","tasks":[{"id":"t1","title":"a","acceptance_criteria":[]}]}"#,
            r#"{"feature":"x","tasks":[{"id":"t 1","title":"a","acceptance_criteria":["b"]}]}"#,
            r#"{"feature":"x","tasks":[{"id":"t1","title":"a","acceptance_criteria":["b"],"priority":1}]}"#,
            r#"{"feature":"x","tasks":[{"id":"t1","title":"a","acceptance_criteria":["b"],"workers":{"builder":{"harness":"codex"}}}]}"#,
        ];
        for b in bad {
            assert!(
                validate(&serde_json::from_str(b).unwrap()).is_err(),
                "accepted: {b}"
            );
        }
    }

    #[test]
    fn example_plan_is_valid() {
        let v: Value = serde_json::from_str(EXAMPLE_PLAN).unwrap();
        validate(&v).unwrap();
        let p: PlanSpec = serde_json::from_str(EXAMPLE_PLAN).unwrap();
        p.validate().unwrap();
        assert_eq!(p.topo_order().unwrap(), vec!["t1", "t2"]);
    }
}
