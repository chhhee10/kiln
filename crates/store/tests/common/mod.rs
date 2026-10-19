#![allow(dead_code)]

use kiln_store::*;
use kiln_types::plan::{PlanSpec, TaskSpec, WorkerChoice};
use kiln_types::*;
use std::collections::BTreeMap;

pub async fn store() -> Store {
    Store::open_in_memory().await.expect("open in-memory store")
}

pub async fn user(s: &Store, email: &str) -> User {
    s.create_user(NewUser {
        email: email.into(),
        name: "Ada Rao".into(),
        role: UserRole::Member,
        password_hash: "$argon2id$fake".into(),
        limits: UserLimits::default(),
    })
    .await
    .expect("create user")
}

pub async fn project(s: &Store, key: &str) -> Project {
    s.create_project(NewProject::new(
        key,
        format!("Project {key}"),
        format!("https://example.com/{key}.git"),
    ))
    .await
    .expect("create project")
}

pub fn spec(ids: &[(&str, &[&str])]) -> PlanSpec {
    PlanSpec {
        feature: "Retry backoff".into(),
        description: String::new(),
        tasks: ids
            .iter()
            .map(|(id, deps)| TaskSpec {
                id: id.to_string(),
                title: format!("Task {id}"),
                description: String::new(),
                acceptance_criteria: vec!["works".into()],
                files_hint: vec![],
                depends_on: deps.iter().map(|d| d.to_string()).collect(),
                workers: Default::default(),
            })
            .collect(),
    }
}

pub fn new_tasks(spec: &PlanSpec) -> Vec<NewTask> {
    let mut workers = BTreeMap::new();
    workers.insert(WorkerRole::Builder, WorkerChoice::new("mock", "mock-1"));
    spec.tasks
        .iter()
        .map(|t| NewTask {
            spec_id: t.id.clone(),
            title: t.title.clone(),
            description: t.description.clone(),
            acceptance: t.acceptance_criteria.clone(),
            files_hint: t.files_hint.clone(),
            depends_on: t.depends_on.clone(),
            workers: workers.clone(),
        })
        .collect()
}

/// A user, project, thread and plan.
pub struct Fixture {
    pub store: Store,
    pub user: User,
    pub project: Project,
    pub thread: Thread,
    pub plan: Plan,
}

pub async fn fixture() -> Fixture {
    let store = store().await;
    let user = user(&store, "ada@example.com").await;
    let project = project(&store, "FP").await;
    let thread = store
        .create_thread(NewThread {
            project_id: project.id,
            owner_id: user.id,
            title: "Retry".into(),
        })
        .await
        .unwrap();
    let plan = store
        .create_plan(NewPlan {
            project_id: project.id,
            owner_id: user.id,
            thread_id: thread.id,
            spec: spec(&[("t1", &[])]),
        })
        .await
        .unwrap();
    Fixture {
        store,
        user,
        project,
        thread,
        plan,
    }
}

pub async fn one_task(f: &Fixture) -> Task {
    let s = spec(&[("t1", &[])]);
    f.store
        .create_tasks_for_plan(f.plan.id, new_tasks(&s))
        .await
        .unwrap()
        .remove(0)
}

pub async fn run_for(f: &Fixture, task: &Task, role: WorkerRole) -> Run {
    f.store
        .create_run(NewRun {
            task_id: Some(task.id),
            thread_id: None,
            owner_id: f.user.id,
            role,
            harness: "mock".into(),
            model: "mock-1".into(),
            attempt: 1,
        })
        .await
        .unwrap()
}
