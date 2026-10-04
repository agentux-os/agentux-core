//! Helpers shared by the engine and API tests.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentux_api::rpc::StartRun;
use agentux_api::{Run, RunStatus};
use agentux_store::Store;
use agentuxd::{Engine, StepExecutor};
use tempfile::TempDir;

pub struct Fixture {
    pub tmp: TempDir,
    pub repo: PathBuf,
    pub database: PathBuf,
}

/// A temp dir holding `repo/`, a git repository with `agentux.yaml`
/// committed, and a path for the database.
pub fn fixture(agentux_yaml: &str) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    fs::write(repo.join("agentux.yaml"), agentux_yaml).unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    git(&repo, &["add", "."]);
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
            "-m",
            "initial",
        ],
    );
    let database = tmp.path().join("agentuxd.db");
    Fixture {
        tmp,
        repo,
        database,
    }
}

fn git(dir: &Path, args: &[&str]) {
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
}

impl Fixture {
    pub fn engine(&self, executor: Arc<dyn StepExecutor>) -> Engine {
        Engine::new(Store::open(&self.database).unwrap(), executor)
    }

    /// Registers the repo and starts a run with a prompt.
    pub async fn start(&self, engine: &Engine) -> Run {
        let project = engine
            .register_project(self.repo.to_str().unwrap())
            .await
            .unwrap();
        engine
            .start_run(StartRun {
                project_id: project.id,
                prompt: Some("Fix the login bug".into()),
                ..Default::default()
            })
            .unwrap()
    }
}

/// Polls until the run satisfies `done`, or panics after 20 s.
pub async fn wait_for(engine: &Engine, run_id: &str, done: impl Fn(&Run) -> bool) -> Run {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (run, _, _) = engine.run(run_id).unwrap();
        if done(&run) {
            return run;
        }
        assert!(
            Instant::now() < deadline,
            "timed out; run is {:?} at {} ({}), error {:?}",
            run.status,
            run.step,
            run.activity,
            run.error
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

pub async fn wait_until_settled(engine: &Engine, run_id: &str) -> Run {
    wait_for(engine, run_id, |r| r.status != RunStatus::Running).await
}

/// A full pipeline with every step type except `custom`.
pub fn pipeline(check: &str) -> String {
    format!(
        "version: 1
roles:
  planner:
    harness: fake-a
  implementer:
    harness: fake-a
  reviewer:
    harness: fake-b
checks:
  - name: check
    run: {check}
pipeline:
  - step: plan
    role: planner
  - step: implement
    role: implementer
  - step: gate
    checks: [check]
    on_fail: implement
    max_attempts: 3
  - step: review
    role: reviewer
    on_changes_requested: implement
    max_rounds: 2
  - step: pull_request

"
    )
}
