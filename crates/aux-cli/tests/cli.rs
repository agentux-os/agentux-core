use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn aux(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aux"))
        .args(args)
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

const VALID: &str = "version: 1
roles:
  implementer:
    harness: claude-code
pipeline:
  - step: implement
    role: implementer
  - step: pull_request
";

#[test]
fn validate_accepts_a_valid_file_or_its_directory() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("agentux.yaml");
    fs::write(&file, VALID).unwrap();

    for target in [file.as_path(), dir.path()] {
        let output = aux(&["validate", path(target)]);
        assert!(output.status.success(), "{}", stderr(&output));
        assert!(
            stdout(&output).contains("valid (implement -> pull_request)"),
            "{}",
            stdout(&output)
        );
    }
}

#[test]
fn validate_reports_every_issue_and_fails() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("agentux.yaml");
    fs::write(
        &file,
        "version: 1\npipeline:\n  - step: implement\n    role: ghost\n  - step: gate\n",
    )
    .unwrap();

    let output = aux(&["validate", path(&file)]);
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(err.contains("is invalid: 2 problems found"), "{err}");
    assert!(
        err.contains("pipeline[0].role: role `ghost` is not defined"),
        "{err}"
    );
    assert!(
        err.contains("pipeline[1].checks: must list at least one check"),
        "{err}"
    );
}

#[test]
fn validate_reports_unknown_keys() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("agentux.yaml");
    fs::write(&file, format!("{VALID}secrets: {{}}\n")).unwrap();

    let output = aux(&["validate", path(&file)]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("unknown field `secrets`"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn validate_without_a_file_shows_the_effective_default_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let output = aux(&["validate", path(dir.path())]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("built-in default pipeline applies"), "{out}");
    assert!(
        out.contains("(plan -> implement -> review -> pull_request)"),
        "{out}"
    );
    assert!(out.contains("gate step is left out"), "{out}");

    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]
",
    )
    .unwrap();
    let output = aux(&["validate", path(dir.path())]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains("(plan -> implement -> gate -> review -> pull_request)"),
        "{out}"
    );
    assert!(
        out.contains(
            "  test: cargo test
"
        ),
        "{out}"
    );
}

#[test]
fn validate_fails_on_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let output = aux(&["validate", path(&dir.path().join("missing.yaml"))]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("cannot read"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn exec_rejects_unknown_harnesses() {
    let output = aux(&["exec", "--harness", "nope", "hello"]);
    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(err.contains("unknown harness `nope`"), "{err}");
    assert!(
        err.contains("claude-code, codex, opencode, antigravity"),
        "{err}"
    );
}

#[test]
fn worktree_create_list_remove() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let base = tmp.path().join("worktrees");
    fs::create_dir(&repo).unwrap();
    for args in [
        &["init", "--quiet", "--initial-branch=main"][..],
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
    ] {
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
    let repo = path(&repo);

    let output = aux(&[
        "worktree",
        "create",
        "r1",
        "--repo",
        repo,
        "--base-dir",
        path(&base),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(base.join("r1").is_dir());

    let output = aux(&["worktree", "list", "--repo", repo]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("r1\taux/r1\t"),
        "{}",
        stdout(&output)
    );

    let output = aux(&[
        "worktree",
        "remove",
        "r1",
        "--repo",
        repo,
        "--delete-branch",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!base.join("r1").exists());

    let output = aux(&["worktree", "list", "--repo", repo]);
    assert_eq!(stdout(&output), "");
}
