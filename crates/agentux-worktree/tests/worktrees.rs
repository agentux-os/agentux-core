use std::fs;
use std::path::Path;
use std::process::Command;

use agentux_worktree::{Error, Worktrees};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A temp dir holding `repo/`, a repository with one commit on `main`.
/// Worktrees go to `<tmp>/worktrees`.
fn setup() -> (TempDir, Worktrees) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=AgentUX Test",
            "-c",
            "user.email=test@agentux.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "--message=init",
        ],
    );
    let worktrees = Worktrees::open(&repo)
        .unwrap()
        .with_base_dir(tmp.path().join("worktrees"))
        .unwrap();
    (tmp, worktrees)
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    !git(repo, &["branch", "--list", branch]).is_empty()
}

#[test]
fn default_base_dir_is_next_to_the_repo() {
    let (tmp, _) = setup();
    let worktrees = Worktrees::open(&tmp.path().join("repo")).unwrap();
    let expected = tmp.path().canonicalize().unwrap().join("repo.worktrees");
    assert_eq!(worktrees.base_dir(), expected);
}

#[test]
fn create_checks_out_a_run_branch_in_its_own_worktree() {
    let (tmp, worktrees) = setup();
    let worktree = worktrees.create("run-1", "HEAD").unwrap();

    assert_eq!(worktree.run_id, "run-1");
    assert_eq!(worktree.branch, "aux/run-1");
    assert_eq!(
        worktree.path.canonicalize().unwrap(),
        tmp.path().join("worktrees/run-1").canonicalize().unwrap()
    );
    assert_eq!(worktree.head, git(worktrees.repo(), &["rev-parse", "HEAD"]));
    assert_eq!(
        git(&worktree.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "aux/run-1"
    );
}

#[test]
fn list_shows_only_run_worktrees() {
    let (_tmp, worktrees) = setup();
    assert!(worktrees.list().unwrap().is_empty());

    worktrees.create("b", "main").unwrap();
    worktrees.create("a", "main").unwrap();
    let mut ids: Vec<String> = worktrees
        .list()
        .unwrap()
        .into_iter()
        .map(|w| w.run_id)
        .collect();
    ids.sort();
    assert_eq!(ids, ["a", "b"]);
}

#[test]
fn creating_the_same_run_twice_fails() {
    let (_tmp, worktrees) = setup();
    worktrees.create("dup", "HEAD").unwrap();
    let err = worktrees.create("dup", "HEAD").unwrap_err();
    assert!(matches!(err, Error::Git { .. }), "{err}");
}

#[test]
fn invalid_run_ids_are_rejected_before_calling_git() {
    let (_tmp, worktrees) = setup();
    for bad in ["../escape", "a/b", "-rf"] {
        let err = worktrees.create(bad, "HEAD").unwrap_err();
        assert!(matches!(err, Error::InvalidRunId(_)), "{bad}: {err}");
    }
}

#[test]
fn remove_keeps_the_branch_unless_asked() {
    let (_tmp, worktrees) = setup();
    let repo = worktrees.repo().to_path_buf();

    let kept = worktrees.create("keep", "HEAD").unwrap();
    worktrees.remove("keep", false, false).unwrap();
    assert!(!kept.path.exists());
    assert!(worktrees.list().unwrap().is_empty());
    assert!(branch_exists(&repo, "aux/keep"));

    worktrees.create("drop", "HEAD").unwrap();
    worktrees.remove("drop", false, true).unwrap();
    assert!(!branch_exists(&repo, "aux/drop"));
}

#[test]
fn remove_refuses_uncommitted_changes_without_force() {
    let (_tmp, worktrees) = setup();
    let worktree = worktrees.create("dirty", "HEAD").unwrap();
    fs::write(worktree.path.join("notes.txt"), "work in progress").unwrap();

    let err = worktrees.remove("dirty", false, false).unwrap_err();
    assert!(matches!(err, Error::Git { .. }), "{err}");
    assert!(worktree.path.exists());

    worktrees.remove("dirty", true, true).unwrap();
    assert!(!worktree.path.exists());
}

#[test]
fn remove_unknown_run_is_not_found() {
    let (_tmp, worktrees) = setup();
    let err = worktrees.remove("ghost", false, false).unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");
}

#[test]
fn open_outside_a_repository_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let err = Worktrees::open(tmp.path()).unwrap_err();
    assert!(matches!(err, Error::Git { .. }), "{err}");
}
