//! End-to-end git flow against a local "remote" repository in a temp dir.

use chrono::Utc;
use kiln_gitops::*;
use kiln_types::{Project, ProjectId, QaProfileStatus, ResourceProfile};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run git with a hermetic config; panic on failure; return stdout.
fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    remote: PathBuf,
    data: PathBuf,
    project: Project,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README.md"), "# demo\n").unwrap();
    std::fs::write(
        seed.join("qa.yaml"),
        "version: 1\nchecks:\n  unit: cargo test\n",
    )
    .unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "initial"]);
    let remote = root.join("remote.git");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--bare",
            seed.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    let data = root.join("data");
    let project = Project {
        id: ProjectId::new(),
        key: "FP".into(),
        name: "Demo".into(),
        repo_url: remote.display().to_string(),
        default_branch: "main".into(),
        qa_path: "qa.yaml".into(),
        qa_status: QaProfileStatus::Unknown,
        image: None,
        resources: ResourceProfile::default(),
        loop_cap: 3,
        created_at: Utc::now(),
    };
    Fixture {
        _tmp: tmp,
        root,
        remote,
        data,
        project,
    }
}

/// Clone the remote into a scratch working copy (to simulate humans/GitHub).
fn human_clone(f: &Fixture, name: &str) -> PathBuf {
    let dir = f.root.join(name);
    git(
        &f.root,
        &[
            "clone",
            "-q",
            f.remote.to_str().unwrap(),
            dir.to_str().unwrap(),
        ],
    );
    dir
}

#[tokio::test]
async fn full_task_branch_lifecycle() {
    let f = fixture();
    let ops = GitOps::new(&f.data);

    // Mirror: created, then refreshed.
    let mirror = ops.ensure_mirror(&f.project, &GitAuth::None).await.unwrap();
    assert_eq!(mirror, f.data.join("repos/FP.git"));
    assert!(mirror.join("HEAD").is_file());
    git(
        &mirror,
        &["rev-parse", "--verify", "refs/remotes/origin/main"],
    );
    ops.ensure_mirror(&f.project, &GitAuth::None).await.unwrap();

    // read_file_at via gix.
    let qa = ops
        .read_file_at(&f.project, "main", "qa.yaml")
        .await
        .unwrap();
    assert!(qa.unwrap().contains("unit: cargo test"));
    assert_eq!(
        ops.read_file_at(&f.project, "main", "nope.yaml")
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        ops.read_file_at(&f.project, "no-such-branch", "qa.yaml")
            .await,
        Err(GitError::RevNotFound(_))
    ));

    // Worktree.
    let branch = branch_name("FP-1", "Add greeting");
    assert_eq!(branch, "kiln/fp-1-add-greeting");
    let wt = ops
        .create_worktree(&f.project, "FP-1", &branch, "main")
        .await
        .unwrap();
    assert_eq!(wt.path, f.data.join("worktrees/FP-1"));
    assert_eq!(wt.git_common_dir, mirror);
    assert_eq!(wt.branch, branch);
    assert_eq!(git(&wt.path, &["symbolic-ref", "--short", "HEAD"]), branch);
    assert_eq!(
        Path::new(&git(
            &wt.path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"]
        ))
        .canonicalize()
        .unwrap(),
        mirror.canonicalize().unwrap()
    );
    // No upstream tracking configured.
    assert!(
        Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "@{u}"])
            .current_dir(&wt.path)
            .output()
            .unwrap()
            .status
            .code()
            != Some(0)
    );

    // Generated files are excluded (linked worktree → common dir info/exclude).
    ops.exclude_paths(&wt.path, &GENERATED_FILES).unwrap();
    ops.exclude_paths(&wt.path, &GENERATED_FILES).unwrap(); // idempotent
    let exclude = std::fs::read_to_string(mirror.join("info/exclude")).unwrap();
    assert_eq!(exclude.matches("AGENTS.md").count(), 1);
    std::fs::write(wt.path.join("AGENTS.md"), "generated").unwrap();
    std::fs::write(wt.path.join("CLAUDE.md"), "generated").unwrap();
    assert_eq!(git(&wt.path, &["status", "--porcelain"]), "");

    // Builder commits.
    std::fs::create_dir_all(wt.path.join("src")).unwrap();
    std::fs::write(wt.path.join("src/hello.txt"), "a\nb\nc\n").unwrap();
    std::fs::write(wt.path.join("README.md"), "# demo app\n").unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-q", "-m", "FP-1: add greeting"]);
    let head = ops.head(&wt.path).await.unwrap();
    assert_eq!(head, git(&wt.path, &["rev-parse", "HEAD"]));

    let diff = ops.diff_summary(&wt.path, "main").await.unwrap();
    assert_eq!(diff.files.len(), 2, "{diff:?}");
    assert_eq!(diff.additions, 4);
    assert_eq!(diff.deletions, 1);
    let hello = diff
        .files
        .iter()
        .find(|d| d.path == "src/hello.txt")
        .unwrap();
    assert_eq!((hello.additions, hello.deletions), (3, 0));

    // Guarded push.
    assert!(matches!(
        ops.push_branch(&f.project, &wt.path, "main", &GitAuth::None)
            .await,
        Err(GitError::Refused(_))
    ));
    assert!(matches!(
        ops.push_branch(&f.project, &wt.path, "feature/x", &GitAuth::None)
            .await,
        Err(GitError::Refused(_))
    ));
    assert!(matches!(
        ops.push_branch(&f.project, &wt.path, "+kiln/x", &GitAuth::None)
            .await,
        Err(GitError::Refused(_))
    ));
    ops.push_branch(&f.project, &wt.path, &branch, &GitAuth::None)
        .await
        .unwrap();
    assert_eq!(
        git(&f.remote, &["rev-parse", &format!("refs/heads/{branch}")]),
        head
    );
    // Pushing again is a no-op success.
    ops.push_branch(&f.project, &wt.path, &branch, &GitAuth::None)
        .await
        .unwrap();

    // Idempotent re-create (a loop): leftovers discarded, commits kept.
    std::fs::write(wt.path.join("scratch.tmp"), "junk").unwrap();
    std::fs::write(wt.path.join("README.md"), "dirty\n").unwrap();
    let again = ops
        .create_worktree(&f.project, "FP-1", &branch, "main")
        .await
        .unwrap();
    assert_eq!(again, wt);
    assert_eq!(ops.head(&wt.path).await.unwrap(), head);
    assert!(!wt.path.join("scratch.tmp").exists());
    assert_eq!(
        std::fs::read_to_string(wt.path.join("README.md")).unwrap(),
        "# demo app\n"
    );
    assert!(
        wt.path.join("AGENTS.md").exists(),
        "ignored generated files survive clean"
    );

    // Local PRs: open (idempotent), then merged after a human merges on the remote.
    let prs = LocalPrs::new(&f.data);
    let new_pr = NewPr {
        repo_url: f.project.repo_url.clone(),
        base: "main".into(),
        head: branch.clone(),
        title: "FP-1: Add greeting".into(),
        body: "body".into(),
        draft: false,
    };
    let pr = prs.open_pr(&new_pr).await.unwrap();
    assert_eq!(pr.number, 1);
    assert_eq!(prs.open_pr(&new_pr).await.unwrap(), pr);
    assert_eq!(
        prs.pr_state(&f.project.repo_url, 1).await.unwrap(),
        PrState::Open
    );
    assert_eq!(
        prs.git_auth(&f.project.repo_url).await.unwrap(),
        GitAuth::None
    );

    let human = human_clone(&f, "human");
    git(
        &human,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge PR #1",
            &format!("origin/{branch}"),
        ],
    );
    git(&human, &["push", "-q", "origin", "main"]);
    assert_eq!(
        prs.pr_state(&f.project.repo_url, 1).await.unwrap(),
        PrState::Merged
    );
    assert_eq!(prs.get(1).await.unwrap().unwrap().state, PrState::Merged);
    assert!(matches!(
        prs.pr_state(&f.project.repo_url, 99).await,
        Err(GitError::NotFound(_))
    ));

    // Remove (idempotent); branch survives; re-create keeps commits.
    ops.remove_worktree(&f.project, "FP-1").await.unwrap();
    assert!(!wt.path.exists());
    ops.remove_worktree(&f.project, "FP-1").await.unwrap();
    git(
        &mirror,
        &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
    );
    let wt2 = ops
        .create_worktree(&f.project, "FP-1", &branch, "main")
        .await
        .unwrap();
    assert_eq!(ops.head(&wt2.path).await.unwrap(), head);
    ops.remove_worktree(&f.project, "FP-1").await.unwrap();
    ops.delete_local_branch(&f.project, &branch).await.unwrap();
    assert!(matches!(
        ops.delete_local_branch(&f.project, "main").await,
        Err(GitError::Refused(_))
    ));
}

#[tokio::test]
async fn refuses_force_push_and_bad_inputs() {
    let f = fixture();
    let ops = GitOps::new(&f.data);
    ops.ensure_mirror(&f.project, &GitAuth::None).await.unwrap();

    assert!(matches!(
        ops.create_worktree(&f.project, "FP-2", "main", "main")
            .await,
        Err(GitError::Refused(_))
    ));
    assert!(matches!(
        ops.create_worktree(&f.project, "../evil", "kiln/x", "main")
            .await,
        Err(GitError::Refused(_))
    ));
    assert!(matches!(
        ops.create_worktree(&f.project, "FP-2", "kiln/bad..name", "main")
            .await,
        Err(GitError::Refused(_))
    ));
    assert!(matches!(
        ops.create_worktree(&f.project, "FP-2", "kiln/fp-2-x", "no-such-base")
            .await,
        Err(GitError::RevNotFound(_))
    ));

    let branch = branch_name("FP-2", "Diverge");
    let wt = ops
        .create_worktree(&f.project, "FP-2", &branch, "main")
        .await
        .unwrap();
    std::fs::write(wt.path.join("a.txt"), "1\n").unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-q", "-m", "one"]);
    ops.push_branch(&f.project, &wt.path, &branch, &GitAuth::None)
        .await
        .unwrap();

    // Someone else pushes to the task branch; our rewritten history must not overwrite it.
    let human = human_clone(&f, "other");
    git(&human, &["checkout", "-q", &branch]);
    std::fs::write(human.join("b.txt"), "2\n").unwrap();
    git(&human, &["add", "-A"]);
    git(&human, &["commit", "-q", "-m", "theirs"]);
    git(&human, &["push", "-q", "origin", &branch]);
    let theirs = git(&human, &["rev-parse", "HEAD"]);

    git(
        &wt.path,
        &["commit", "-q", "--amend", "-m", "one (amended)"],
    );
    let err = ops
        .push_branch(&f.project, &wt.path, &branch, &GitAuth::None)
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    assert_eq!(
        git(&f.remote, &["rev-parse", &format!("refs/heads/{branch}")]),
        theirs
    );

    // A directory that is not one of our worktrees cannot be pushed from.
    let stray = f.root.join("stray");
    std::fs::create_dir_all(&stray).unwrap();
    assert!(matches!(
        ops.push_branch(&f.project, &stray, &branch, &GitAuth::None)
            .await,
        Err(GitError::NotAWorktree(_))
    ));

    // A stray directory at the worktree path is replaced by a real worktree.
    let p = ops.worktree_path("FP-3");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("junk"), "x").unwrap();
    let wt3 = ops
        .create_worktree(&f.project, "FP-3", "kiln/fp-3-x", "main")
        .await
        .unwrap();
    assert!(!wt3.path.join("junk").exists());
    assert!(wt3.path.join("README.md").exists());
}

#[tokio::test]
async fn local_pr_closed_when_branch_deleted_unmerged() {
    let f = fixture();
    let ops = GitOps::new(&f.data);
    ops.ensure_mirror(&f.project, &GitAuth::None).await.unwrap();
    let branch = "kiln/fp-4-closed";
    let wt = ops
        .create_worktree(&f.project, "FP-4", branch, "main")
        .await
        .unwrap();
    std::fs::write(wt.path.join("c.txt"), "c\n").unwrap();
    git(&wt.path, &["add", "-A"]);
    git(&wt.path, &["commit", "-q", "-m", "c"]);

    let prs = LocalPrs::new(&f.data);
    let new_pr = NewPr {
        repo_url: f.project.repo_url.clone(),
        base: "main".into(),
        head: branch.into(),
        title: "t".into(),
        body: "b".into(),
        draft: true,
    };
    // Not pushed yet → refused with NotFound.
    assert!(matches!(
        prs.open_pr(&new_pr).await,
        Err(GitError::NotFound(_))
    ));
    ops.push_branch(&f.project, &wt.path, branch, &GitAuth::None)
        .await
        .unwrap();
    let pr = prs.open_pr(&new_pr).await.unwrap();
    assert_eq!(pr.url, "local://pr/1");
    git(&f.remote, &["branch", "-D", branch]);
    assert_eq!(
        prs.pr_state(&f.project.repo_url, pr.number).await.unwrap(),
        PrState::Closed
    );

    // Explicit close of a second PR.
    git(
        &wt.path,
        &[
            "push",
            "-q",
            f.remote.to_str().unwrap(),
            &format!("{branch}:{branch}"),
        ],
    );
    let pr2 = prs.open_pr(&new_pr).await.unwrap();
    assert_eq!(pr2.number, 2);
    prs.close_pr(2).await.unwrap();
    assert_eq!(
        prs.pr_state(&f.project.repo_url, 2).await.unwrap(),
        PrState::Closed
    );
    assert_eq!(prs.list().await.unwrap().len(), 2);
}

#[tokio::test]
async fn exclude_paths_in_plain_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    let ops = GitOps::new(tmp.path().join("data"));
    ops.exclude_paths(tmp.path(), &["AGENTS.md"]).unwrap();
    let ex = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
    assert!(ex.lines().any(|l| l == "AGENTS.md"));
    assert!(matches!(
        ops.exclude_paths(&tmp.path().join("nope"), &["x"]),
        Err(GitError::NotAWorktree(_))
    ));
}

#[tokio::test]
async fn operations_need_a_mirror() {
    let f = fixture();
    let ops = GitOps::new(&f.data);
    assert!(matches!(
        ops.create_worktree(&f.project, "FP-1", "kiln/x", "main")
            .await,
        Err(GitError::NotFound(_))
    ));
    assert!(matches!(
        ops.read_file_at(&f.project, "main", "qa.yaml").await,
        Err(GitError::NotFound(_))
    ));
    ops.remove_worktree(&f.project, "FP-1").await.unwrap(); // nothing to do is fine
    let mut bad = f.project.clone();
    bad.repo_url = f.root.join("missing.git").display().to_string();
    bad.key = "BAD".into();
    assert!(matches!(
        ops.ensure_mirror(&bad, &GitAuth::None).await,
        Err(GitError::Command { .. })
    ));
    assert!(
        !ops.mirror_path(&bad).exists(),
        "failed first fetch leaves no mirror"
    );
}
