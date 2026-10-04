//! Running a gate's check commands, on the host or in a rootless Podman
//! container (ADR 0009).
//!
//! Every check runs as `sh -c <command>` in its own process group with a
//! timeout. When it times out, or the run is cancelled while it runs (the
//! driver task is aborted and this future dropped), the whole process group is
//! killed, not only `sh`, and an isolated check's container is removed: the
//! Podman client does not own the container's processes, so killing it alone
//! would leave them running.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agentux_config::format_timeout;
use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::task::JoinHandle;

/// Keep at most this much of each check's output.
const MAX_CHECK_OUTPUT: usize = 8 * 1024;

/// After a check's `sh` exits, how long to wait for processes it left in the
/// background to close its output before they are killed.
const OUTPUT_GRACE: Duration = Duration::from_secs(5);

/// Podman exits with this when it could not run the container at all (bad
/// image, failed pull, invalid option), as opposed to the command failing.
const PODMAN_ERROR: i32 = 125;

/// One check to run.
#[derive(Debug, Clone)]
pub(crate) struct CheckCommand<'a> {
    pub worktree: &'a Path,
    pub command: &'a str,
    pub timeout: Duration,
    /// `None`: run on the host.
    pub container: Option<Container>,
}

/// How an isolated check's container is set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Container {
    pub image: String,
    pub network: bool,
    /// Also the key that replaces a container left by an interrupted run of
    /// the same check.
    pub name: String,
    /// The repository's git directory, mounted read-only at its own path so
    /// git works in the worktree. `None`: not mounted.
    pub git_dir: Option<PathBuf>,
}

impl Container {
    /// A container name for check `index` of a run; Podman names allow
    /// `[a-zA-Z0-9][a-zA-Z0-9_.-]*`.
    pub fn name_for(run_id: &str, index: usize) -> String {
        let id: String = run_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("agentux-check-{id}-{index}")
    }
}

/// What a check that ran did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Passed(String),
    Failed(String),
    TimedOut(String),
}

impl Outcome {
    pub fn passed(&self) -> bool {
        matches!(self, Self::Passed(_))
    }

    pub fn output(&self) -> &str {
        match self {
            Self::Passed(output) | Self::Failed(output) | Self::TimedOut(output) => output,
        }
    }
}

/// The program and arguments that run `check`.
pub(crate) fn command_line(check: &CheckCommand<'_>) -> (OsString, Vec<OsString>) {
    match &check.container {
        None => ("sh".into(), vec!["-c".into(), check.command.into()]),
        Some(container) => (
            "podman".into(),
            podman_args(check.worktree, container, check.command),
        ),
    }
}

/// `podman run` arguments for one isolated check: rootless, the user's own
/// uid inside (`keep-id`, so files the check writes in the worktree belong to
/// the user), the worktree at its own path, nothing else from the host — no
/// home directory, no environment, no network unless asked for.
fn podman_args(worktree: &Path, container: &Container, command: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "run",
        "--rm",
        "--replace",
        "--pull=missing",
        "--userns=keep-id",
        "--security-opt=no-new-privileges",
        "--label=io.agentux.check=1",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(format!("--name={}", container.name).into());
    if !container.network {
        args.push("--network=none".into());
    }
    args.push("--volume".into());
    args.push(volume(worktree, "Z"));
    if let Some(git_dir) = &container.git_dir {
        args.push("--volume".into());
        args.push(volume(git_dir, "ro,z"));
    }
    args.push("--workdir".into());
    args.push(worktree.into());
    // The image is a separate argument after `--`-free options; it was
    // checked not to start with `-` (agentux_config::Isolation).
    args.push(container.image.clone().into());
    args.extend(["sh".into(), "-c".into(), command.into()]);
    args
}

/// `<path>:<path>:<options>`. `Z` relabels the worktree for this container
/// only (SELinux); the git directory is shared (`z`) because several runs'
/// checks may mount it at once.
fn volume(path: &Path, options: &str) -> OsString {
    let mut volume = OsString::from(path);
    volume.push(":");
    volume.push(path);
    volume.push(":");
    volume.push(options);
    volume
}

/// Runs one check. `Err` means it could not run at all — `sh` or `podman`
/// is missing, or Podman could not start the container — which is not
/// something the project's code can fix.
pub(crate) async fn run(check: &CheckCommand<'_>) -> Result<Outcome, String> {
    let (program, args) = command_line(check);
    let mut child = tokio::process::Command::new(&program)
        .args(&args)
        .current_dir(check.worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| spawn_error(check, &e))?;
    // Declared after `child`, so on cancellation it is dropped first: the
    // group is killed while its leader is unreaped and its id still taken.
    let mut cleanup = Cleanup {
        group: child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw),
        container: check.container.as_ref().map(|c| c.name.clone()),
    };
    let mut stdout = read_all(child.stdout.take());
    let mut stderr = read_all(child.stderr.take());

    let status = match tokio::time::timeout(check.timeout, child.wait()).await {
        Ok(status) => status.map_err(|e| format!("cannot wait for the check: {e}"))?,
        Err(_) => {
            drop(cleanup); // kill everything it started, remove the container
            let _ = child.wait().await;
            let mut output = join(stdout, stderr).await;
            output.push_str(&format!(
                "\n[agentux: the check timed out after {} and was killed]",
                format_timeout(check.timeout)
            ));
            return Ok(Outcome::TimedOut(tail(output.trim())));
        }
    };
    // `sh` exited. Processes it left in the background that still hold its
    // output keep the group alive, so killing the group after the grace
    // cannot hit a reused id.
    let drained = tokio::time::timeout(OUTPUT_GRACE, async {
        let out = (&mut stdout).await;
        let err = (&mut stderr).await;
        (out, err)
    })
    .await;
    let output = match drained {
        Ok((out, err)) => {
            cleanup.disarm();
            combine(out, err)
        }
        Err(_) => {
            drop(cleanup);
            join(stdout, stderr).await
        }
    };
    let output = tail(output.trim_end());
    if check.container.is_some() && status.code() == Some(PODMAN_ERROR) {
        return Err(format!(
            "podman could not run the check container (exit {PODMAN_ERROR}):\n{output}"
        ));
    }
    Ok(if status.success() {
        Outcome::Passed(output)
    } else {
        Outcome::Failed(output)
    })
}

fn spawn_error(check: &CheckCommand<'_>, e: &io::Error) -> String {
    match (&check.container, e.kind()) {
        (Some(_), io::ErrorKind::NotFound) => "podman is not installed or not on PATH; \
             `isolation.mode: podman` in agentux.yaml needs it (install podman, \
             or set `isolation.mode: none` to run checks on the host)"
            .to_string(),
        (Some(_), _) => format!("cannot run podman: {e}"),
        (None, _) => format!("cannot run `sh -c {}`: {e}", check.command),
    }
}

type Reader = JoinHandle<Vec<u8>>;

fn read_all(pipe: Option<impl AsyncRead + Unpin + Send + 'static>) -> Reader {
    tokio::spawn(async move {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes).await;
        }
        bytes
    })
}

async fn join(stdout: Reader, stderr: Reader) -> String {
    combine(stdout.await, stderr.await)
}

fn combine(
    stdout: Result<Vec<u8>, tokio::task::JoinError>,
    stderr: Result<Vec<u8>, tokio::task::JoinError>,
) -> String {
    let mut text = String::from_utf8_lossy(&stdout.unwrap_or_default()).into_owned();
    text.push_str(&String::from_utf8_lossy(&stderr.unwrap_or_default()));
    text
}

/// Kills a check's process group and removes its container when dropped,
/// unless disarmed after the check finished on its own.
struct Cleanup {
    group: Option<Pid>,
    container: Option<String>,
}

impl Cleanup {
    fn disarm(&mut self) {
        self.group = None;
        self.container = None;
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(group) = self.group.take() {
            let _ = kill_process_group(group, Signal::KILL);
        }
        if let Some(name) = self.container.take() {
            // Drop cannot wait; a thread keeps the removal from being a
            // zombie. `--replace` covers a daemon that exits before it ran.
            std::thread::spawn(move || {
                let _ = std::process::Command::new("podman")
                    .args(["rm", "--force", "--ignore", "--time", "0", &name])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            });
        }
    }
}

/// The last [`MAX_CHECK_OUTPUT`] bytes of `text`.
fn tail(text: &str) -> String {
    if text.len() <= MAX_CHECK_OUTPUT {
        return text.to_string();
    }
    let mut start = text.len() - MAX_CHECK_OUTPUT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[…]{}", &text[start..])
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn container(network: bool, git_dir: Option<&str>) -> Container {
        Container {
            image: "registry.fedoraproject.org/fedora-toolbox:44".into(),
            network,
            name: Container::name_for("ab12cd34", 1),
            git_dir: git_dir.map(PathBuf::from),
        }
    }

    fn strings(args: &[OsString]) -> Vec<&str> {
        args.iter().map(|a| a.to_str().unwrap()).collect()
    }

    #[test]
    fn host_checks_run_with_sh() {
        let check = CheckCommand {
            worktree: Path::new("/w"),
            command: "just test",
            timeout: Duration::from_secs(1),
            container: None,
        };
        let (program, args) = command_line(&check);
        assert_eq!(program, "sh");
        assert_eq!(strings(&args), ["-c", "just test"]);
    }

    #[test]
    fn podman_command_line() {
        let check = CheckCommand {
            worktree: Path::new("/home/u/repo.worktrees/ab12cd34"),
            command: "cargo test && echo 'done'",
            timeout: Duration::from_secs(1),
            container: Some(container(false, Some("/home/u/repo/.git"))),
        };
        let (program, args) = command_line(&check);
        assert_eq!(program, "podman");
        assert_eq!(
            strings(&args),
            [
                "run",
                "--rm",
                "--replace",
                "--pull=missing",
                "--userns=keep-id",
                "--security-opt=no-new-privileges",
                "--label=io.agentux.check=1",
                "--name=agentux-check-ab12cd34-1",
                "--network=none",
                "--volume",
                "/home/u/repo.worktrees/ab12cd34:/home/u/repo.worktrees/ab12cd34:Z",
                "--volume",
                "/home/u/repo/.git:/home/u/repo/.git:ro,z",
                "--workdir",
                "/home/u/repo.worktrees/ab12cd34",
                "registry.fedoraproject.org/fedora-toolbox:44",
                "sh",
                "-c",
                "cargo test && echo 'done'",
            ]
        );
    }

    #[test]
    fn podman_with_network_and_without_git_dir() {
        let check = CheckCommand {
            worktree: Path::new("/w"),
            command: "true",
            timeout: Duration::from_secs(1),
            container: Some(container(true, None)),
        };
        let (_, args) = command_line(&check);
        let args = strings(&args);
        assert!(!args.iter().any(|a| a.starts_with("--network")), "{args:?}");
        assert_eq!(args.iter().filter(|a| **a == "--volume").count(), 1);
        // Nothing from the host environment or home directory is passed in.
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("--env") || a.contains("HOME"))
        );
    }

    #[test]
    fn container_names_are_valid() {
        assert_eq!(
            Container::name_for("ab12cd34", 0),
            "agentux-check-ab12cd34-0"
        );
        assert_eq!(Container::name_for("a/b c", 2), "agentux-check-a-b-c-2");
    }

    #[test]
    fn missing_podman_is_explained() {
        let check = CheckCommand {
            worktree: Path::new("/w"),
            command: "true",
            timeout: Duration::from_secs(1),
            container: Some(container(false, None)),
        };
        let message = spawn_error(&check, &io::Error::from(io::ErrorKind::NotFound));
        assert!(message.starts_with("podman is not installed"), "{message}");
        assert!(message.contains("isolation.mode: none"), "{message}");
    }

    fn host<'a>(dir: &'a Path, command: &'a str, timeout: Duration) -> CheckCommand<'a> {
        CheckCommand {
            worktree: dir,
            command,
            timeout,
            container: None,
        }
    }

    #[tokio::test]
    async fn host_check_passes_and_fails_with_output() {
        let dir = tempfile::tempdir().unwrap();
        let passed = run(&host(
            dir.path(),
            "echo out; echo err >&2",
            Duration::from_secs(10),
        ))
        .await
        .unwrap();
        assert_eq!(passed, Outcome::Passed("out\nerr".into()));
        let failed = run(&host(
            dir.path(),
            "echo broken; exit 3",
            Duration::from_secs(10),
        ))
        .await
        .unwrap();
        assert_eq!(failed, Outcome::Failed("broken".into()));
    }

    /// Whether `pid` runs (a zombie, waiting to be reaped, does not count).
    fn alive(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            let state = stat.rsplit(')').next().unwrap_or("").trim_start();
            !state.starts_with('Z')
        })
    }

    async fn wait_gone(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            assert!(Instant::now() < deadline, "process {pid} is still running");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn read_pid(path: &Path) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .filter(|s| s.ends_with('\n'))
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "{} was not written",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        // A grandchild of `sh` that would outlive a kill of `sh` alone.
        let started = Instant::now();
        let outcome = run(&host(
            dir.path(),
            "sleep 300 & echo $! > grandchild; echo started; wait",
            Duration::from_secs(1),
        ))
        .await
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(10));
        let Outcome::TimedOut(output) = outcome else {
            panic!("expected a timeout, got {outcome:?}");
        };
        assert!(output.starts_with("started"), "{output}");
        assert!(
            output.ends_with("timed out after 1s and was killed]"),
            "{output}"
        );
        wait_gone(read_pid(&dir.path().join("grandchild")).await).await;
    }

    #[tokio::test]
    async fn cancelling_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let task = tokio::spawn(async move {
            run(&host(
                &path,
                "sleep 300 & echo $! > grandchild; wait",
                Duration::from_secs(300),
            ))
            .await
        });
        let pid = read_pid(&dir.path().join("grandchild")).await;
        task.abort();
        let _ = task.await;
        wait_gone(pid).await;
    }

    #[tokio::test]
    async fn background_leftovers_do_not_hold_up_a_finished_check() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let outcome = run(&host(
            dir.path(),
            "sleep 300 & echo $! > leftover; echo ok",
            Duration::from_secs(60),
        ))
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::Passed("ok".into()));
        assert!(started.elapsed() < OUTPUT_GRACE + Duration::from_secs(5));
        wait_gone(read_pid(&dir.path().join("leftover")).await).await;
    }

    #[test]
    fn long_output_keeps_its_tail() {
        let text = "x".repeat(MAX_CHECK_OUTPUT + 10) + "end";
        let cut = tail(&text);
        assert!(cut.starts_with("[…]") && cut.ends_with("end"));
    }

    /// Runs only where rootless Podman works; `AGENTUX_REQUIRE_PODMAN=1`
    /// (set in CI) turns a skip into a failure.
    fn podman_available() -> bool {
        let works = std::process::Command::new("podman")
            .args(["info", "--format", "{{.Host.Security.Rootless}}"])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        if !works {
            assert!(
                std::env::var_os("AGENTUX_REQUIRE_PODMAN").is_none(),
                "AGENTUX_REQUIRE_PODMAN is set but podman does not work here"
            );
            eprintln!("skipped: podman is not available");
        }
        works
    }

    /// A small image with `sh`, pulled once per machine.
    const TEST_IMAGE: &str = "docker.io/library/busybox:1.37";

    fn isolated<'a>(dir: &'a Path, command: &'a str, network: bool) -> CheckCommand<'a> {
        CheckCommand {
            worktree: dir,
            command,
            timeout: Duration::from_secs(300),
            container: Some(Container {
                image: TEST_IMAGE.into(),
                network,
                name: Container::name_for(&agentux_store::new_id(), 0),
                git_dir: None,
            }),
        }
    }

    #[tokio::test]
    async fn podman_check_runs_isolated_in_the_worktree() {
        if !podman_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        std::fs::write(dir.join("input"), "from the host\n").unwrap();

        // The worktree is mounted at its own path and is the working
        // directory; files written there belong to the user (keep-id).
        let outcome = run(&isolated(
            &dir,
            "pwd; cat input; echo made > output; id -u",
            false,
        ))
        .await
        .unwrap();
        let Outcome::Passed(output) = &outcome else {
            panic!("expected a pass, got {outcome:?}");
        };
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], dir.to_str().unwrap());
        assert_eq!(lines[1], "from the host");
        let uid = rustix::process::getuid().as_raw();
        assert_eq!(lines[2], uid.to_string(), "keep-id runs as the user");
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(dir.join("output")).unwrap().uid(), uid);

        // No network but loopback, and nothing of the host's home.
        let home = std::env::var("HOME").unwrap_or_default();
        let command = format!("ls /sys/class/net; test -e '{home}/.bashrc' && echo leaked || true");
        let outcome = run(&isolated(&dir, &command, false)).await.unwrap();
        assert_eq!(outcome, Outcome::Passed("lo".into()));

        // A failing command fails the check, not the run.
        let outcome = run(&isolated(&dir, "echo nope; exit 4", false))
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Failed("nope".into()));
    }

    #[tokio::test]
    async fn podman_check_timeout_removes_the_container() {
        if !podman_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().canonicalize().unwrap();
        // Pull first, so the timeout does not hit the pull.
        run(&isolated(&dir, "true", false)).await.unwrap();

        let mut check = isolated(&dir, "echo started; sleep 300", false);
        check.timeout = Duration::from_secs(3);
        let name = check.container.as_ref().unwrap().name.clone();
        let outcome = run(&check).await.unwrap();
        assert!(matches!(outcome, Outcome::TimedOut(_)), "{outcome:?}");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let listed = std::process::Command::new("podman")
                .args(["ps", "--all", "--quiet", "--filter"])
                .arg(format!("name=^{name}$"))
                .output()
                .unwrap();
            if listed.stdout.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "container {name} is still there");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    #[tokio::test]
    async fn podman_error_is_not_a_check_failure() {
        if !podman_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut check = isolated(dir.path(), "true", false);
        check.container.as_mut().unwrap().image = "localhost/agentux-no-such-image:0".into();
        let error = run(&check).await.unwrap_err();
        assert!(error.starts_with("podman could not run"), "{error}");
    }
}
