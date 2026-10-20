//! `--demo-seed` (dev only): a fictional demo company.
//!
//! * admin `admin@company.dev` (Alex Rivera) and members Jordan Lee, Morgan
//!   Kim and Sam Diaz, all with password [`DEMO_PASSWORD`];
//! * allowed domain `company.dev`;
//! * three projects — Acme CLI/SDK (`FP`), Acme Cloud (`FC`),
//!   Marketing website (`WEB`) — each backed by a local bare repo under
//!   `<data>/seed-repos/` with a tiny codebase and a `qa.yaml`;
//! * every seeded user's worker defaults set to the mock harness.
//!
//! Idempotent: existing users/projects are left alone.

use anyhow::{Context, Result, bail};
use kiln_auth::Auth;
use kiln_store::{NewProject, Store, UserPatch};
use kiln_types::api::{CreateUserRequest, SetupRequest};
use kiln_types::{Project, User, UserLimits, UserRole, UserSettings, WorkerRole};
use std::path::{Path, PathBuf};

pub const DEMO_DOMAIN: &str = "company.dev";
pub const DEMO_ADMIN: &str = "admin@company.dev";
pub const DEMO_PASSWORD: &str = "kiln-demo-password";

/// (name, email, role, concurrency, budget).
const PEOPLE: [(&str, &str, UserRole, u32, f64); 4] = [
    ("Alex Rivera", DEMO_ADMIN, UserRole::Admin, 4, 400.0),
    (
        "Jordan Lee",
        "jordan@company.dev",
        UserRole::Member,
        3,
        300.0,
    ),
    (
        "Morgan Kim",
        "morgan@company.dev",
        UserRole::Member,
        3,
        300.0,
    ),
    ("Sam Diaz", "sam@company.dev", UserRole::Member, 2, 300.0),
];

/// What the seed created / found.
#[derive(Debug)]
pub struct Seeded {
    pub users: Vec<User>,
    pub projects: Vec<Project>,
}

/// Create the demo data. `defaults` are the worker defaults to give every
/// seeded user (the mock harness in dev).
pub async fn seed(
    store: &Store,
    auth: &Auth,
    data_dir: &Path,
    settings: UserSettings,
) -> Result<Seeded> {
    if auth.setup_required().await? {
        auth.setup(SetupRequest {
            admin_email: DEMO_ADMIN.into(),
            admin_name: PEOPLE[0].0.into(),
            password: DEMO_PASSWORD.into(),
            allowed_domain: DEMO_DOMAIN.into(),
        })
        .await
        .context("creating the demo admin")?;
    }
    let Some(admin) = store.get_user_by_email(DEMO_ADMIN).await? else {
        bail!(
            "--demo-seed needs a fresh data dir (or one seeded before): {DEMO_ADMIN} does not exist but other users do"
        );
    };
    let mut users = Vec::new();
    for (name, email, role, concurrency, budget) in PEOPLE {
        let user = match store.get_user_by_email(email).await? {
            Some(u) => u,
            None => auth
                .create_user(
                    &admin,
                    CreateUserRequest {
                        email: email.into(),
                        name: name.into(),
                        password: DEMO_PASSWORD.into(),
                        role,
                    },
                )
                .await
                .with_context(|| format!("creating {email}"))?,
        };
        let limits = UserLimits {
            concurrency,
            ram_share: 0.5,
            monthly_budget_usd: budget,
        };
        let user = store
            .update_user(
                user.id,
                UserPatch {
                    limits: Some(limits),
                    ..Default::default()
                },
            )
            .await?;
        store.set_user_settings(user.id, &settings).await?;
        users.push(user);
    }

    let repos = data_dir.join("seed-repos");
    std::fs::create_dir_all(&repos)?;
    let mut projects = Vec::new();
    for spec in &PROJECTS {
        let project = match store.get_project_by_key(spec.key).await {
            Ok(p) => p,
            Err(kiln_store::StoreError::NotFound) => {
                let url = make_repo(&repos, spec)
                    .await
                    .with_context(|| format!("creating seed repo {}", spec.slug))?;
                store
                    .create_project(NewProject::new(spec.key, spec.name, url.to_string_lossy()))
                    .await?
            }
            Err(e) => return Err(e.into()),
        };
        projects.push(project);
    }
    Ok(Seeded { users, projects })
}

/// Settings used for seeded users: mock harness for every role.
pub fn mock_settings(mut base: UserSettings) -> UserSettings {
    for role in [
        WorkerRole::Planner,
        WorkerRole::Builder,
        WorkerRole::Qa,
        WorkerRole::Reviewer,
        WorkerRole::Distiller,
    ] {
        base.worker_defaults
            .insert(role, kiln_types::plan::WorkerChoice::new("mock", "mock-1"));
    }
    base
}

/// Banner printed on startup.
pub fn banner(url: &str, s: &Seeded) -> String {
    let mut out = format!(
        "\n  Kiln demo seed ready at {url}/app/\n  Password for every account: {DEMO_PASSWORD}\n"
    );
    for u in &s.users {
        out.push_str(&format!(
            "    {:<22} {:<7} {}\n",
            u.email,
            if u.is_admin() { "admin" } else { "member" },
            u.name
        ));
    }
    out.push_str("  Projects:\n");
    for p in &s.projects {
        out.push_str(&format!("    {:<4} {:<22} {}\n", p.key, p.name, p.repo_url));
    }
    out
}

struct ProjectSeed {
    key: &'static str,
    name: &'static str,
    slug: &'static str,
    files: &'static [(&'static str, &'static str)],
}

const PROJECTS: [ProjectSeed; 3] = [
    ProjectSeed {
        key: "FP",
        name: "Acme CLI/SDK",
        slug: "acme-cli",
        files: &[
            (
                "README.md",
                "# acme\n\nCLI and SDK for Acme.\n\n    npx acme check ./agent.yaml\n",
            ),
            (
                "package.json",
                "{\n  \"name\": \"acme\",\n  \"version\": \"0.4.2\",\n  \"bin\": { \"acme\": \"bin/acme.js\" },\n  \"scripts\": { \"test\": \"node --test\" }\n}\n",
            ),
            (
                "bin/acme.js",
                "#!/usr/bin/env node\nconst { check } = require('../src/index.js');\nconst file = process.argv[3];\nif (process.argv[2] !== 'check' || !file) {\n  console.error('usage: acme check <config>');\n  process.exit(2);\n}\nprocess.exit(check(file) ? 0 : 1);\n",
            ),
            (
                "src/index.js",
                "const fs = require('fs');\n\n/** Returns true when the config file exists and is non-empty. */\nfunction check(path) {\n  try {\n    return fs.readFileSync(path, 'utf8').trim().length > 0;\n  } catch {\n    return false;\n  }\n}\n\nmodule.exports = { check };\n",
            ),
            (
                "test/index.test.js",
                "const test = require('node:test');\nconst assert = require('node:assert');\nconst { check } = require('../src/index.js');\n\ntest('missing file fails', () => assert.strictEqual(check('/nope'), false));\n",
            ),
            (
                "qa.yaml",
                "version: 1\nresources: { cpus: 1, memory: 1g }\nchecks:\n  unit: \"npm test\"\n  cli: \"./bin/acme.js check package.json\"\nartifacts: [\"qa/out/**\"]\nloop_cap: 3\n",
            ),
        ],
    },
    ProjectSeed {
        key: "FC",
        name: "Acme Cloud",
        slug: "acme-cloud",
        files: &[
            (
                "README.md",
                "# Acme Cloud\n\nDashboard + API. `npm start` serves on :3000.\n",
            ),
            (
                "package.json",
                "{\n  \"name\": \"acme-cloud\",\n  \"version\": \"1.12.0\",\n  \"scripts\": { \"start\": \"node server.js\", \"test\": \"node --test\" }\n}\n",
            ),
            (
                "server.js",
                "const http = require('http');\n\nconst server = http.createServer((req, res) => {\n  if (req.url === '/api/health') {\n    res.writeHead(200, { 'content-type': 'application/json' });\n    return res.end(JSON.stringify({ ok: true }));\n  }\n  res.writeHead(200, { 'content-type': 'text/html' });\n  res.end('<h1>Acme Cloud</h1>');\n});\n\nserver.listen(process.env.PORT || 3000);\n",
            ),
            (
                "qa.yaml",
                "version: 1\nresources: { cpus: 2, memory: 2g }\nservices:\n  db:\n    image: postgres:16-alpine\n    env: { POSTGRES_PASSWORD: kiln }\n    ports: [5432]\n    healthcheck: \"pg_isready -U postgres\"\nboot:\n  - \"npm start &\"\nchecks:\n  unit: \"npm test\"\n  e2e: \"npx playwright test\"\nartifacts: [\"playwright-report/**\"]\nloop_cap: 3\n",
            ),
        ],
    },
    ProjectSeed {
        key: "WEB",
        name: "Marketing website",
        slug: "website",
        files: &[
            ("README.md", "# acme.example\n\nStatic marketing site.\n"),
            (
                "index.html",
                "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><title>Acme</title><link rel=\"stylesheet\" href=\"style.css\"></head>\n<body><h1>Ship agents that don't fail.</h1><a href=\"/docs/\">Docs</a></body>\n</html>\n",
            ),
            (
                "style.css",
                "body { background: #0f1115; color: #eceef2; font-family: system-ui; }\n",
            ),
            (
                "qa.yaml",
                "version: 1\nresources: { cpus: 1, memory: 1g }\nchecks:\n  extra: [\"linkcheck\", \"lighthouse\"]\nartifacts: [\"qa/out/**\"]\nloop_cap: 3\n",
            ),
        ],
    },
];

async fn git(dir: &Path, args: &[&str]) -> Result<()> {
    let out = tokio::process::Command::new("git")
        .args([
            "-c",
            "user.name=Kiln Seed",
            "-c",
            "user.email=seed@company.dev",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Create `<repos>/<slug>.git` (bare) with one commit on `main`.
async fn make_repo(repos: &Path, spec: &ProjectSeed) -> Result<PathBuf> {
    let bare = repos.join(format!("{}.git", spec.slug));
    if bare.join("HEAD").exists() {
        return Ok(std::fs::canonicalize(&bare)?);
    }
    let work = repos.join(format!(".{}-src", spec.slug));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    for (path, content) in spec.files {
        let p = work.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content)?;
        #[cfg(unix)]
        if path.starts_with("bin/") {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    git(&work, &["init", "-q", "-b", "main"]).await?;
    git(&work, &["add", "-A"]).await?;
    git(&work, &["commit", "-q", "-m", "Initial commit"]).await?;
    git(
        repos,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().context("non-UTF-8 path")?,
            bare.to_str().context("non-UTF-8 path")?,
        ],
    )
    .await?;
    std::fs::remove_dir_all(&work)?;
    Ok(std::fs::canonicalize(&bare)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_qa_profiles_parse() {
        for p in PROJECTS {
            let (_, yaml) = p.files.iter().find(|(n, _)| *n == "qa.yaml").unwrap();
            let prof: kiln_types::qa::QaProfile =
                serde_norway::from_str(yaml).unwrap_or_else(|e| panic!("{}: {e}", p.key));
            assert_eq!(prof.version, 1);
        }
    }
}
