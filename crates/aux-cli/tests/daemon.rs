//! `aux daemon` as a real process, driven by the other `aux` commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

struct Setup {
    tmp: TempDir,
    repo: PathBuf,
    socket: PathBuf,
    database: PathBuf,
}

/// A repo with the given `agentux.yaml`, and socket and database paths.
fn setup(agentux_yaml: &str) -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    fs::write(repo.join("agentux.yaml"), agentux_yaml).unwrap();
    for args in [
        &["init", "--quiet", "--initial-branch=main"][..],
        &["add", "."],
        &[
            "-c",
            "user.name=AgentUX Test",
            "-c",
            "user.email=test@agentux.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
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
    Setup {
        socket: tmp.path().join("run/agentuxd.sock"),
        database: tmp.path().join("state/agentuxd.db"),
        repo,
        tmp,
    }
}

/// Kills the daemon when the test ends, however it ends.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Setup {
    fn aux(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aux"))
            .arg("--socket")
            .arg(&self.socket)
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.aux(args);
        assert!(
            output.status.success(),
            "aux {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn daemon(&self) -> Daemon {
        let _ = fs::remove_file(&self.socket);
        let child = Command::new(env!("CARGO_BIN_EXE_aux"))
            .arg("--socket")
            .arg(&self.socket)
            .arg("daemon")
            .arg("--database")
            .arg(&self.database)
            .arg("--fake-agents")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Daemon(child);
        wait_until("the socket appears", || self.socket.exists());
        daemon
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        sleep(Duration::from_millis(50));
    }
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

const PIPELINE: &str = "version: 1
roles:
  implementer:
    harness: fake
checks:
  - name: check
    run: CHECK
pipeline:
  - step: plan
    role: implementer
    approve: true
  - step: implement
    role: implementer
  - step: gate
    checks: [check]
  - step: pull_request
";

#[test]
fn run_ps_approve_and_watch_through_the_daemon() {
    let s = setup(&PIPELINE.replace("CHECK", "true"));
    let _daemon = s.daemon();

    let run_id = s
        .ok(&["run", path(&s.repo), "--prompt", "Add a health endpoint"])
        .trim()
        .to_string();
    assert_eq!(run_id.len(), 8, "{run_id}");

    // The plan needs approval.
    let mut ps = String::new();
    wait_until("the run waits for approval", || {
        ps = s.ok(&["ps"]);
        ps.contains("WAITING FOR YOU")
    });
    assert!(ps.contains(&run_id), "{ps}");
    assert!(ps.contains("Add a health endpoint"), "{ps}");
    let request_id = ps
        .lines()
        .skip_while(|l| *l != "WAITING FOR YOU")
        .nth(1)
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_string();

    let approved = s.ok(&["approve", &request_id, "--message", "ship it"]);
    assert!(approved.contains("approved"), "{approved}");
    let again = s.aux(&["approve", &request_id]);
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already approved"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );

    let watched = s.ok(&["watch", &run_id]);
    assert!(watched.contains("approval needed"), "{watched}");
    assert!(watched.contains("check check passed"), "{watched}");
    assert!(watched.contains("pull request #1"), "{watched}");
    assert!(watched.contains("[done]"), "{watched}");

    assert!(!s.ok(&["ps"]).contains(&run_id));
    assert!(s.ok(&["ps", "--all"]).contains(&run_id));
}

#[test]
fn a_denied_run_fails_and_watch_says_so() {
    let s = setup(&PIPELINE.replace("CHECK", "true"));
    let _daemon = s.daemon();
    let run_id = s.ok(&["run", path(&s.repo), "-p", "x"]).trim().to_string();
    let mut request_id = String::new();
    wait_until("the run waits for approval", || {
        let ps = s.ok(&["ps"]);
        if let Some(line) = ps.lines().skip_while(|l| *l != "WAITING FOR YOU").nth(1) {
            request_id = line.split_whitespace().next().unwrap().to_string();
        }
        !request_id.is_empty()
    });
    s.ok(&["deny", &request_id, "-m", "not now"]);
    let watched = s.aux(&["watch", &run_id]);
    assert!(!watched.status.success());
    let out = String::from_utf8_lossy(&watched.stdout);
    assert!(out.contains("plan not approved: not now"), "{out}");
}

#[test]
fn a_killed_daemon_resumes_the_run_on_restart() {
    // The gate takes long enough to kill the daemon in the middle of it.
    let s = setup(
        &PIPELINE
            .replace("CHECK", "sleep 2 && touch gate-ran")
            .replace("    approve: true\n", ""),
    );
    let daemon = s.daemon();
    let run_id = s.ok(&["run", path(&s.repo), "-p", "x"]).trim().to_string();
    wait_until("the gate is running", || {
        s.ok(&["ps"]).contains("gate: running check")
    });
    drop(daemon); // SIGKILL: no chance to clean up.

    let _daemon = s.daemon();
    let watched = s.ok(&["watch", &run_id]);
    assert!(watched.contains("gate attempt interrupted"), "{watched}");
    assert!(watched.contains("running it again"), "{watched}");
    assert!(watched.contains("[done]"), "{watched}");
    let worktree = s.tmp.path().join("repo.worktrees").join(&run_id);
    assert!(worktree.join("gate-ran").exists());
}

#[test]
fn commands_explain_a_missing_daemon() {
    let s = setup(&PIPELINE.replace("CHECK", "true"));
    let output = s.aux(&["ps"]);
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("cannot connect to agentuxd"), "{err}");
    assert!(err.contains("aux daemon"), "{err}");
}

impl Setup {
    /// `aux ps --json [extra]`, parsed, with its exit code.
    fn ps_json(&self, extra: &[&str]) -> (Option<i32>, serde_json::Value) {
        let mut args = vec!["ps", "--json"];
        args.extend_from_slice(extra);
        let output = self.aux(&args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!(
                "aux {args:?} printed no JSON ({e}):\n{stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code(), value)
    }
}

#[test]
fn ps_json_reports_an_idle_daemon() {
    let s = setup(&PIPELINE.replace("CHECK", "true"));
    let _daemon = s.daemon();
    let (code, value) = s.ps_json(&[]);
    assert_eq!(code, Some(0));
    assert_eq!(
        value,
        serde_json::json!({ "daemon": "up", "runs": [], "pending": [] })
    );
}

#[test]
fn ps_json_shows_a_run_waiting_for_plan_approval() {
    let s = setup(&PIPELINE.replace("CHECK", "true"));
    let _daemon = s.daemon();
    let run_id = s
        .ok(&["run", path(&s.repo), "--prompt", "Add a health endpoint"])
        .trim()
        .to_string();

    let mut value = serde_json::Value::Null;
    wait_until("the run waits for plan approval", || {
        value = s.ps_json(&[]).1;
        value["pending"].as_array().is_some_and(|p| !p.is_empty())
    });
    assert_eq!(value["daemon"], "up");

    let runs = value["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1, "{value}");
    let run = &runs[0];
    assert_eq!(run["id"], run_id.as_str());
    assert_eq!(run["title"], "Add a health endpoint");
    assert_eq!(run["status"], "waiting");
    assert_eq!(run["step"], "plan");
    assert_eq!(run["stepIndex"], 0);
    assert_eq!(run["project"]["name"], "repo");
    let repo = fs::canonicalize(&s.repo).unwrap();
    assert_eq!(run["project"]["path"], path(&repo));
    assert!(run["projectId"].is_string());
    assert!(run["updatedAt"].as_i64().unwrap() > 0);
    // The pipeline has a gate (no on_fail: one attempt) and no review step.
    assert_eq!(run["gate"]["max"], 1, "{run}");
    assert!(run["gate"]["attempt"].is_u64(), "{run}");
    assert!(run.get("review").is_none(), "{run}");
    assert!(run.get("costUsd").is_none(), "{run}");

    let pending = value["pending"].as_array().unwrap();
    assert_eq!(pending.len(), 1, "{value}");
    let request = &pending[0];
    assert_eq!(request["runId"], run_id.as_str());
    assert_eq!(request["kind"], "plan");
    assert!(request["title"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(request.get("options").is_none(), "{request}");

    // Denying the plan fails the run: gone from the default list, shown with --all.
    let request_id = request["id"].as_str().unwrap();
    s.ok(&["deny", request_id, "-m", "not now"]);
    wait_until("the run fails", || {
        s.ps_json(&[]).1["runs"]
            .as_array()
            .is_some_and(|r| r.is_empty())
    });
    let (code, all) = s.ps_json(&["--all"]);
    assert_eq!(code, Some(0));
    assert_eq!(all["runs"][0]["id"], run_id.as_str());
    assert_eq!(all["runs"][0]["status"], "failed");
    assert_eq!(all["pending"], serde_json::json!([]));
}

#[test]
fn ps_json_reports_a_missing_daemon_with_exit_code_3() {
    let s = setup(&PIPELINE.replace("CHECK", "true"));
    let (code, value) = s.ps_json(&[]);
    assert_eq!(code, Some(3));
    assert_eq!(value, serde_json::json!({ "daemon": "down" }));
}
