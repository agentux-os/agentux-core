//! The agent sandbox for real (ADRs 0023 and 0024): commands run through
//! [`Profile::command`] with Bubblewrap (and pasta for `outbound`), checked
//! against ADR 0024's phase 1 acceptance list.
//!
//! They need `bwrap` (and `pasta`) with unprivileged user namespaces. Where
//! those are missing the tests say so and pass, unless
//! `AGENTUX_SANDBOX_TESTS=1` (CI) makes that a failure.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use agentux_config::AgentNetwork;
use agentuxd::sandbox::Profile;

/// Whether `network` can be sandboxed here; panics when it can't and CI
/// requires it.
fn available(network: AgentNetwork) -> bool {
    // With a timeout: a pasta that can't set up its namespace may wait.
    let works = |program: &str, args: &[&str]| {
        Command::new("timeout")
            .arg("10")
            .arg(program)
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    let mut ok = works(
        "bwrap",
        &["--unshare-user", "--ro-bind", "/", "/", "--", "true"],
    );
    if network == AgentNetwork::Outbound {
        ok = ok && works("pasta", &["--config-net", "--quiet", "--", "true"]);
    }
    if !ok {
        assert!(
            std::env::var_os("AGENTUX_SANDBOX_TESTS").is_none(),
            "AGENTUX_SANDBOX_TESTS is set but the sandbox can't run here"
        );
        eprintln!("skipped: bwrap/pasta with user namespaces are not available");
    }
    ok
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A home with things a sandbox must not see, and a repository with a run's
/// worktree outside it.
struct Setup {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    worktree: PathBuf,
    git_dir: PathBuf,
}

fn setup() -> Setup {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_ed25519"), "PRIVATE KEY").unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex/auth.json"), r#"{"token":"host"}"#).unwrap();
    std::fs::write(home.join(".codex/config.toml"), "notify = [\"host\"]\n").unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude/.credentials.json"), "CLAUDE TOKEN").unwrap();

    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "--initial-branch=main"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Setup",
            "-c",
            "user.email=setup@agentux.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    let worktree = tmp.path().join("worktrees").join("run-1");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "aux/run-1",
            worktree.to_str().unwrap(),
        ],
    );
    let git_dir = agentuxd::sandbox::git_dir(&worktree).expect("the worktree's git directory");
    Setup {
        _tmp: tmp,
        home,
        worktree,
        git_dir,
    }
}

fn profile(s: &Setup, network: AgentNetwork) -> Profile {
    let mut p = Profile::new("codex", network, &s.home, &s.worktree).unwrap();
    p.git_dir = Some(s.git_dir.clone());
    p.git_identity = Some(("Ana Agent".into(), "ana@agentux.invalid".into()));
    p.prepare().unwrap();
    p
}

/// `sh -c script` in the sandbox, with the test's home as `$HOME`.
fn run(p: &Profile, script: &str) -> Output {
    let (program, args) = p.command("sh", &["-c".to_string(), script.to_string()]);
    Command::new(program)
        .args(args)
        .env("HOME", &p.home)
        .env("SSH_AUTH_SOCK", "/tmp/agent.sock")
        .env("ANTHROPIC_API_KEY", "other-harness-key")
        .output()
        .unwrap()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_home_holds_only_the_run_and_the_harness() {
    if !available(AgentNetwork::None) {
        return;
    }
    let s = setup();
    let p = profile(&s, AgentNetwork::None);
    let home = s.home.display();
    let out = ok(&run(
        &p,
        &format!(
            "test ! -e {home}/.ssh/id_ed25519 && echo no-ssh; \
             test ! -e {home}/.claude/.credentials.json && echo no-other-harness; \
             cat \"$CODEX_HOME/auth.json\"; echo; \
             test ! -e \"$CODEX_HOME/config.toml\" && echo no-host-settings; \
             echo sandbox > \"$CODEX_HOME/config.toml\"; \
             echo changed > {wt}/file && echo worktree-writable; \
             echo \"ssh=${{SSH_AUTH_SOCK:-unset}} other=${{ANTHROPIC_API_KEY:-unset}}\"",
            wt = s.worktree.display()
        ),
    ));
    for expected in [
        "no-ssh",
        "no-other-harness",
        r#"{"token":"host"}"#,
        "no-host-settings",
        "worktree-writable",
        "ssh=unset other=unset",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in {out}");
    }
    // The host's CLI settings are untouched; the sandbox has its own.
    assert_eq!(
        std::fs::read_to_string(s.home.join(".codex/config.toml")).unwrap(),
        "notify = [\"host\"]\n"
    );
    assert_eq!(
        std::fs::read_to_string(s.worktree.join("file")).unwrap(),
        "changed\n"
    );
}

#[test]
fn git_hooks_and_config_are_read_only_and_commits_carry_the_identity() {
    if !available(AgentNetwork::None) {
        return;
    }
    let s = setup();
    let p = profile(&s, AgentNetwork::None);
    let git = s.git_dir.display();
    let out = run(
        &p,
        &format!("echo 'touch /tmp/pwned' > {git}/hooks/pre-commit && echo hook-written"),
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("hook-written"));
    let out = run(
        &p,
        &format!("echo '[core]' >> {git}/config && echo config-written"),
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("config-written"));

    ok(&run(
        &p,
        "git add -A && git commit --quiet --allow-empty -m 'from the sandbox'",
    ));
    let author = Command::new("git")
        .arg("-C")
        .arg(&s.worktree)
        .args(["log", "-1", "--format=%an <%ae> %G?"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&author.stdout).trim(),
        "Ana Agent <ana@agentux.invalid> N"
    );
}

#[test]
fn the_control_socket_is_not_there_and_the_bus_socket_is() {
    if !available(AgentNetwork::None) {
        return;
    }
    let s = setup();
    let run_dir = s.home.parent().unwrap().join("run");
    std::fs::create_dir_all(run_dir.join("bus")).unwrap();
    let control = run_dir.join("agentuxd.sock");
    let bus = run_dir.join("bus").join("s1.sock");
    let _control = std::os::unix::net::UnixListener::bind(&control).unwrap();
    let _bus = std::os::unix::net::UnixListener::bind(&bus).unwrap();
    let mut p = profile(&s, AgentNetwork::None);
    p.bus_socket = Some(bus.clone());
    let out = ok(&run(
        &p,
        &format!(
            "test ! -e {} && echo no-control; test -S {} && echo bus",
            control.display(),
            bus.display()
        ),
    ));
    assert!(out.contains("no-control") && out.contains("bus"), "{out}");
}

#[test]
fn outbound_cannot_reach_the_hosts_loopback() {
    if !available(AgentNetwork::Outbound) {
        return;
    }
    let s = setup();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let p = profile(&s, AgentNetwork::Outbound);
    // bash's /dev/tcp, with a timeout in case a connection hangs.
    let out = run(
        &p,
        &format!(
            "if command -v bash >/dev/null; then \
               timeout 5 bash -c 'echo hi > /dev/tcp/127.0.0.1/{port}' 2>/dev/null && echo reached || echo blocked; \
             else echo no-bash; fi"
        ),
    );
    let text = ok(&out);
    assert!(
        text.contains("blocked") || text.contains("no-bash"),
        "the host's loopback was reachable: {text}"
    );
    listener.set_nonblocking(true).unwrap();
    assert!(
        listener.accept().is_err(),
        "a connection from the sandbox reached the host's loopback"
    );
}

#[test]
fn nothing_outlives_the_sandbox() {
    if !available(AgentNetwork::None) {
        return;
    }
    let s = setup();
    let p = profile(&s, AgentNetwork::None);
    let marker = format!("{}", 3000 + std::process::id() % 1000);
    let (program, args) = p.command(
        "sh",
        &[
            "-c".into(),
            format!("sleep {marker} & sleep {marker} & wait"),
        ],
    );
    let mut child = Command::new(program)
        .args(args)
        .env("HOME", &p.home)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    child.kill().unwrap();
    let _ = child.wait();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = Command::new("pgrep")
            .args(["-f", &format!("sleep {marker}")])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !left {
            break;
        }
        assert!(Instant::now() < deadline, "processes outlived the sandbox");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn allowed_extra_paths_are_readable_and_nothing_else_is() {
    if !available(AgentNetwork::None) {
        return;
    }
    let s = setup();
    std::fs::create_dir_all(s.home.join(".cargo/registry")).unwrap();
    std::fs::write(s.home.join(".cargo/registry/index"), "crates").unwrap();
    let mut p = profile(&s, AgentNetwork::None);
    p.extra_read_only = vec![std::fs::canonicalize(s.home.join(".cargo/registry")).unwrap()];
    let registry = p.extra_read_only[0].display().to_string();
    let out = ok(&run(
        &p,
        &format!(
            "cat {registry}/index; echo; \
             (echo x > {registry}/index) 2>/dev/null && echo writable || echo read-only; \
             test ! -e {home}/.ssh/id_ed25519 && echo no-ssh",
            home = s.home.display()
        ),
    ));
    for expected in ["crates", "read-only", "no-ssh"] {
        assert!(out.contains(expected), "missing {expected:?} in {out}");
    }
}
