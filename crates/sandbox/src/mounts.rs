//! Mount layout for worker containers (DECISIONS.md #3, PRD §7.7).
//!
//! | host                                   | container                      | mode                    |
//! |----------------------------------------|--------------------------------|-------------------------|
//! | task worktree                          | `/workspace`                   | RW Builder, RO others   |
//! | repo common git dir (`<repo>.git`)     | *same absolute path*           | RW Builder, RO others   |
//! | `<data>/runs/<run_id>`                 | `/run/kiln`                    | RW                      |
//! | `<run_dir>/transcripts/claude`         | `~kiln/.claude/projects`       | RW                      |
//! | `<run_dir>/transcripts/codex`          | `~kiln/.codex/sessions`        | RW                      |
//!
//! The worktree's `.git` file points at `<common>/worktrees/<name>`, so the
//! common dir must be visible at the same path for git to work inside the
//! container. The Builder commits, which writes objects and refs into the
//! common dir, so it gets that mount read-write.
//!
//! The run folder holds the daemon socket (`kiln.sock`), the spool, `out/`
//! and the harness transcripts, so everything survives the container.

use crate::spec::Mount;
use kiln_types::WorkerRole;
use kiln_types::protocol::paths;
use std::path::Path;

/// Home directory of the `kiln` user in the worker image.
pub const CONTAINER_HOME: &str = "/home/kiln";

/// Harness transcript dirs: (`<run_dir>/transcripts/<key>`, path under the
/// container user's home).
pub const TRANSCRIPT_DIRS: &[(&str, &str)] =
    &[("claude", ".claude/projects"), ("codex", ".codex/sessions")];

/// Mounts for one run. `worktree` is `None` for runs without a checkout
/// (e.g. a Planner chat turn); `git_common_dir` is `None` when the worktree
/// is a standalone clone.
pub fn mounts_for(
    role: WorkerRole,
    worktree: Option<&Path>,
    git_common_dir: Option<&Path>,
    run_dir: &Path,
) -> Vec<Mount> {
    let writable = role.writes_code();
    let mut out = Vec::new();
    if let Some(wt) = worktree {
        out.push(Mount {
            host: wt.to_path_buf(),
            container: paths::WORKSPACE.into(),
            read_only: !writable,
        });
    }
    if let Some(common) = git_common_dir {
        out.push(Mount {
            host: common.to_path_buf(),
            container: common.to_string_lossy().into_owned(),
            read_only: !writable,
        });
    }
    out.push(Mount::rw(run_dir, paths::RUN_DIR));
    out.extend(transcript_mounts(run_dir));
    out
}

/// Bind mounts that redirect the harnesses' native transcript dirs into the
/// run folder.
pub fn transcript_mounts(run_dir: &Path) -> Vec<Mount> {
    TRANSCRIPT_DIRS
        .iter()
        .map(|(key, rel)| {
            Mount::rw(
                run_dir.join("transcripts").join(key),
                format!("{CONTAINER_HOME}/{rel}"),
            )
        })
        .collect()
}

/// Create the run folder skeleton so every bind-mount source exists:
/// `out/`, `out/artifacts/`, `transcripts/<harness>/`.
pub fn prepare_run_dir(run_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(run_dir.join("out").join("artifacts"))?;
    for (key, _) in TRANSCRIPT_DIRS {
        std::fs::create_dir_all(run_dir.join("transcripts").join(key))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(m: &'a [Mount], target: &str) -> &'a Mount {
        m.iter()
            .find(|m| m.container == target)
            .unwrap_or_else(|| panic!("no mount at {target}"))
    }

    #[test]
    fn builder_gets_rw_worktree_and_common_dir() {
        let m = mounts_for(
            WorkerRole::Builder,
            Some(Path::new("/data/worktrees/FP-1")),
            Some(Path::new("/data/repos/FP.git")),
            Path::new("/data/runs/r1"),
        );
        assert!(!find(&m, "/workspace").read_only);
        let common = find(&m, "/data/repos/FP.git");
        assert!(!common.read_only);
        assert_eq!(common.host, Path::new("/data/repos/FP.git"));
        assert!(!find(&m, "/run/kiln").read_only);
        assert_eq!(
            find(&m, "/home/kiln/.claude/projects").host,
            Path::new("/data/runs/r1/transcripts/claude")
        );
        assert_eq!(
            find(&m, "/home/kiln/.codex/sessions").host,
            Path::new("/data/runs/r1/transcripts/codex")
        );
    }

    #[test]
    fn read_only_roles() {
        for role in [
            WorkerRole::Qa,
            WorkerRole::Reviewer,
            WorkerRole::Planner,
            WorkerRole::Distiller,
        ] {
            let m = mounts_for(
                role,
                Some(Path::new("/w")),
                Some(Path::new("/repos/x.git")),
                Path::new("/runs/r"),
            );
            assert!(find(&m, "/workspace").read_only, "{role:?}");
            assert!(find(&m, "/repos/x.git").read_only, "{role:?}");
            assert!(!find(&m, "/run/kiln").read_only, "{role:?}");
        }
    }

    #[test]
    fn optional_worktree() {
        let m = mounts_for(WorkerRole::Planner, None, None, Path::new("/runs/r"));
        assert!(m.iter().all(|m| m.container != "/workspace"));
        assert_eq!(m.len(), 1 + TRANSCRIPT_DIRS.len());
    }

    #[test]
    fn prepare_creates_sources() {
        let d = tempfile::tempdir().unwrap();
        prepare_run_dir(d.path()).unwrap();
        for m in mounts_for(WorkerRole::Qa, None, None, d.path()) {
            assert!(m.host.is_dir(), "{}", m.host.display());
        }
        assert!(d.path().join("out/artifacts").is_dir());
    }
}
