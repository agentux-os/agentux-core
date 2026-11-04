//! Agent sessions in a Bubblewrap sandbox (ADRs 0023 and 0024).
//!
//! A sandboxed harness sees the system read-only, an empty home with only
//! its run's worktree, the repository's git directory (hooks and config
//! read-only), its own sandbox state with its credential file from the host,
//! its install, a sandbox-only npm cache and its session's bus socket. Never
//! the daemon's control socket, the user's files, `/run/user`, or the host's
//! loopback: `outbound` gives it a network namespace of its own through
//! pasta, `none` no network.
//!
//! [`Profile::command`] is the whole policy as a command line; it is pure so
//! tests can check it without Bubblewrap.

use std::path::{Path, PathBuf};
use std::{fs, io};

use agentux_config::AgentNetwork;
use agentux_harness::HarnessSandbox;

/// Where the sandboxes' own state lives, under `$HOME`.
pub const SANDBOX_DIR: &str = ".local/share/agentux/sandbox";

/// System paths mounted read-only (or recreated as symlinks) when present.
const SYSTEM: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/lib32",
    "/etc",
    "/opt",
    "/var/usrlocal",
    "/var/opt",
];

/// Variables never passed into a sandbox: agents, sockets and tokens of the
/// user's session.
const UNSET: &[&str] = &[
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "SSH_ASKPASS",
    "GIT_ASKPASS",
    "GPG_AGENT_INFO",
    "DBUS_SESSION_BUS_ADDRESS",
    "XDG_RUNTIME_DIR",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "KRB5CCNAME",
    "CREDENTIALS_DIRECTORY",
];

/// Everything one sandboxed session is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub harness: String,
    pub network: AgentNetwork,
    pub home: PathBuf,
    pub worktree: PathBuf,
    /// The repository's git directory (`git rev-parse --git-common-dir`).
    pub git_dir: Option<PathBuf>,
    /// The session's bus socket, when it has the bus.
    pub bus_socket: Option<PathBuf>,
    /// `(name, email)` for commits, from the host's configuration.
    pub git_identity: Option<(String, String)>,
    /// The API key variables of other harnesses, to keep out.
    pub other_keys: Vec<String>,
    /// Host paths the machine or the user allows, mounted read-only
    /// ([`extra_paths`]); never chosen by a project.
    pub extra_read_only: Vec<PathBuf>,
    sandbox: HarnessSandbox,
}

impl Profile {
    /// A profile for `harness`, or why it can't be sandboxed.
    pub fn new(
        harness: &str,
        network: AgentNetwork,
        home: &Path,
        worktree: &Path,
    ) -> Result<Self, String> {
        let sandbox = HarnessSandbox::for_harness(harness).ok_or_else(|| {
            format!("{harness} is not supported in the agent sandbox yet (its credential paths are not verified)")
        })?;
        let own = crate::secrets::variables(harness);
        let other_keys = crate::secrets::VARIABLES
            .iter()
            .filter(|var| !own.contains(var))
            .map(|var| var.to_string())
            .collect();
        Ok(Self {
            harness: harness.to_string(),
            network,
            home: home.to_path_buf(),
            worktree: worktree.to_path_buf(),
            git_dir: None,
            bus_socket: None,
            git_identity: None,
            other_keys,
            extra_read_only: Vec::new(),
            sandbox,
        })
    }

    /// The harness's sandbox state directory.
    pub fn state_dir(&self) -> PathBuf {
        self.home
            .join(SANDBOX_DIR)
            .join("state")
            .join(&self.harness)
    }

    fn npm_cache(&self) -> PathBuf {
        self.home.join(SANDBOX_DIR).join("npm")
    }

    /// The host's credential file and where it goes, if the host has one.
    fn credential(&self) -> Option<(PathBuf, PathBuf)> {
        let (host, inside) = self.sandbox.credential?;
        let host = self.home.join(host);
        host.is_file()
            .then(|| (host, self.state_dir().join(inside)))
    }

    /// Creates the state directory, the npm cache, and an empty file where
    /// the credential is mounted.
    pub fn prepare(&self) -> io::Result<()> {
        let state = self.state_dir();
        for dir in self.sandbox.state_dirs {
            fs::create_dir_all(if dir.is_empty() {
                state.clone()
            } else {
                state.join(dir)
            })?;
        }
        fs::create_dir_all(self.npm_cache())?;
        if let Some((_, inside)) = self.credential() {
            if let Some(parent) = inside.parent() {
                fs::create_dir_all(parent)?;
            }
            if !inside.exists() {
                fs::File::create(&inside)?;
            }
        }
        Ok(())
    }

    /// `program args` run in the sandbox, as the program and arguments to
    /// start.
    pub fn command(&self, program: &str, args: &[String]) -> (String, Vec<String>) {
        let mut bwrap: Vec<String> = Vec::new();
        push(
            &mut bwrap,
            &[
                "--die-with-parent",
                "--new-session",
                "--unshare-user",
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts",
                "--unshare-cgroup-try",
            ],
        );
        if self.network == AgentNetwork::None {
            push(&mut bwrap, &["--unshare-net"]);
        }
        for path in SYSTEM {
            system_mount(Path::new(path), &mut bwrap);
        }
        if let Ok(meta) = fs::symlink_metadata("/home")
            && meta.file_type().is_symlink()
            && let Ok(target) = fs::read_link("/home")
        {
            bind(&mut bwrap, "--symlink", &target, Path::new("/home"));
        }
        push(
            &mut bwrap,
            &[
                "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--tmpfs", "/var/tmp",
                "--tmpfs", "/run",
            ],
        );
        resolver(&mut bwrap);

        bind(&mut bwrap, "--tmpfs", &self.home, &self.home);
        bind(&mut bwrap, "--bind", &self.worktree, &self.worktree);
        if let Some(git) = &self.git_dir {
            bind(&mut bwrap, "--bind", git, git);
            for part in ["hooks", "config"] {
                let path = git.join(part);
                if path.exists() {
                    bind(&mut bwrap, "--ro-bind", &path, &path);
                }
            }
        }
        let state = self.state_dir();
        bind(&mut bwrap, "--bind", &state, &state);
        if let Some((host, inside)) = self.credential() {
            bind(&mut bwrap, "--bind", &host, &inside);
        }
        for path in &self.extra_read_only {
            bind(&mut bwrap, "--ro-bind", path, path);
        }
        for rel in self.sandbox.install {
            let path = self.home.join(rel);
            if path.exists() {
                bind(&mut bwrap, "--ro-bind", &path, &path);
            }
        }
        let npm = self.npm_cache();
        bind(&mut bwrap, "--bind", &npm, &npm);
        if let Some(socket) = &self.bus_socket {
            bind(&mut bwrap, "--bind", socket, socket);
        }

        for var in UNSET
            .iter()
            .copied()
            .chain(self.other_keys.iter().map(String::as_str))
        {
            bwrap.extend(["--unsetenv".to_string(), var.to_string()]);
        }
        let mut set = |var: &str, value: &str| {
            bwrap.extend(["--setenv".to_string(), var.to_string(), value.to_string()]);
        };
        for (var, value) in self.sandbox.env(&state) {
            set(&var, &value);
        }
        set("NPM_CONFIG_CACHE", &npm.to_string_lossy());
        // Git: none of the user's configuration, no signing key, and the
        // host's identity.
        set("GIT_CONFIG_GLOBAL", "/dev/null");
        set("GIT_CONFIG_COUNT", "1");
        set("GIT_CONFIG_KEY_0", "commit.gpgsign");
        set("GIT_CONFIG_VALUE_0", "false");
        if let Some((name, email)) = &self.git_identity {
            for who in ["AUTHOR", "COMMITTER"] {
                set(&format!("GIT_{who}_NAME"), name);
                set(&format!("GIT_{who}_EMAIL"), email);
            }
        }
        bwrap.extend([
            "--chdir".to_string(),
            self.worktree.to_string_lossy().into_owned(),
        ]);
        bwrap.push("--".to_string());
        bwrap.push(program.to_string());
        bwrap.extend(args.iter().cloned());

        match self.network {
            AgentNetwork::None => ("bwrap".to_string(), bwrap),
            // pasta makes the network namespace: outward only, no mapping of
            // the host's loopback or gateway, no port forwarding either way.
            AgentNetwork::Outbound => {
                let mut pasta: Vec<String> = [
                    "--config-net",
                    "--quiet",
                    "--no-map-gw",
                    "-t",
                    "none",
                    "-u",
                    "none",
                    "-T",
                    "none",
                    "-U",
                    "none",
                    "--",
                    "bwrap",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                pasta.extend(bwrap);
                ("pasta".to_string(), pasta)
            }
        }
    }

    /// The programs the profile needs on the host.
    pub fn required_programs(&self) -> &'static [&'static str] {
        match self.network {
            AgentNetwork::None => &["bwrap"],
            AgentNetwork::Outbound => &["bwrap", "pasta"],
        }
    }
}

fn push(args: &mut Vec<String>, items: &[&str]) {
    args.extend(items.iter().map(|s| s.to_string()));
}

fn bind(args: &mut Vec<String>, how: &str, source: &Path, dest: &Path) {
    args.push(how.to_string());
    if how != "--tmpfs" {
        args.push(source.to_string_lossy().into_owned());
    }
    args.push(dest.to_string_lossy().into_owned());
}

/// A system path: read-only, or the same symlink when it is one (Fedora's
/// `/bin -> usr/bin`, Atomic's `/opt -> var/opt`).
fn system_mount(path: &Path, args: &mut Vec<String>) {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if let Ok(target) = fs::read_link(path) {
                bind(args, "--symlink", &target, path);
            }
        }
        Ok(_) => bind(args, "--ro-bind", path, path),
        Err(_) => {}
    }
}

/// DNS with a private `/run`: `/etc/resolv.conf` often points into `/run`
/// (systemd-resolved's stub, NetworkManager's file). The stub's 127.0.0.53
/// is the host's loopback, which a sandbox can't reach, so it gets the
/// upstream servers resolved knows instead.
fn resolver(args: &mut Vec<String>) {
    let Ok(real) = fs::canonicalize("/etc/resolv.conf") else {
        return;
    };
    if !real.starts_with("/run") {
        return;
    }
    let upstream = Path::new("/run/systemd/resolve/resolv.conf");
    let source = if real.ends_with("stub-resolv.conf") && upstream.is_file() {
        upstream.to_path_buf()
    } else {
        real.clone()
    };
    bind(args, "--ro-bind", &source, &real);
}

/// The machine's file of extra sandbox paths.
pub const MACHINE_POLICY: &str = "/etc/agentux/sandbox.yaml";

/// Locations no extra path may be, contain or be inside, relative to the
/// home: credentials, keys, browser profiles and the harnesses' own state
/// (ADR 0024).
const DENIED: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".pki",
    ".local/share/keyrings",
    ".password-store",
    ".config/gh",
    ".config/hub",
    ".config/git/credentials",
    ".git-credentials",
    ".netrc",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".config/containers/auth.json",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/BraveSoftware",
    ".var/app",
    ".claude",
    ".claude.json",
    ".codex",
    ".config/opencode",
    ".local/share/opencode",
    ".gemini",
    ".config/agentux",
    ".local/share/agentux",
    ".local/state/agentux",
];

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ExtraPaths {
    #[serde(default)]
    read_only: Vec<String>,
}

/// The extra read-only paths allowed by the machine (`/etc/agentux/
/// sandbox.yaml`) and the user (`~/.config/agentux/sandbox.yaml`, which no
/// sandbox can write), and notes on the ones refused. A path is refused
/// when it doesn't exist, goes through a symlink, is the home or the root,
/// or is, contains or is inside a [`DENIED`] location.
pub fn extra_paths(home: &Path) -> (Vec<PathBuf>, Vec<String>) {
    let user = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("agentux/sandbox.yaml");
    let mut paths = Vec::new();
    let mut notes = Vec::new();
    for file in [PathBuf::from(MACHINE_POLICY), user] {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let listed: ExtraPaths = match serde_saphyr::from_str(&text) {
            Ok(listed) => listed,
            Err(e) => {
                notes.push(format!("sandbox: {} is not used: {e}", file.display()));
                continue;
            }
        };
        for entry in listed.read_only {
            match allowed(home, &entry) {
                Ok(path) if !paths.contains(&path) => paths.push(path),
                Ok(_) => {}
                Err(why) => notes.push(format!(
                    "sandbox: {entry} ({}) is not mounted: {why}",
                    file.display()
                )),
            }
        }
    }
    (paths, notes)
}

/// `entry` (`~/…` or absolute) as a path a sandbox may read, or why not.
fn allowed(home: &Path, entry: &str) -> Result<PathBuf, String> {
    let path = match entry.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if entry == "~" => home.to_path_buf(),
        None => PathBuf::from(entry),
    };
    if !path.is_absolute() {
        return Err("not an absolute path".into());
    }
    let real = fs::canonicalize(&path).map_err(|e| format!("{e}"))?;
    if real != path {
        return Err(format!("it goes through a symlink (to {})", real.display()));
    }
    let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if real == Path::new("/") || real == home || home.starts_with(&real) {
        return Err("it would expose the whole home".into());
    }
    for denied in DENIED {
        let denied = home.join(denied);
        if real.starts_with(&denied) || denied.starts_with(&real) {
            return Err(format!(
                "{} holds credentials or harness state",
                denied.display()
            ));
        }
    }
    Ok(real)
}

/// A warning when the repository runs hooks from its worktree
/// (`core.hooksPath` inside it): the agent can edit them, and a person who
/// commits in that worktree on the host would run them (ADR 0024).
pub fn hooks_warning(worktree: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["config", "--get", "core.hooksPath"])
        .output()
        .ok()?;
    let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if !out.status.success() || value.is_empty() {
        return None;
    }
    let path = Path::new(&value);
    let inside = if path.is_absolute() {
        path.starts_with(worktree)
    } else {
        // Relative hooks paths are relative to the working tree.
        !value.starts_with("..")
    };
    inside.then(|| {
        format!(
            "sandbox: this repository runs git hooks from {value} in its worktree, which the agent can edit; \
             committing in this worktree on the host would run them (agentuxd's own git commands don't)"
        )
    })
}

/// The repository's git directory for `worktree`.
pub fn git_dir(worktree: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let path = PathBuf::from(text.trim());
    (out.status.success() && path.is_dir()).then_some(path)
}

/// `user.name` and `user.email` as git on the host sees them for
/// `worktree`.
pub fn git_identity(worktree: &Path) -> Option<(String, String)> {
    let get = |key: &str| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["config", "--get", key])
            .output()
            .ok()?;
        let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (out.status.success() && !value.is_empty()).then_some(value)
    };
    Some((get("user.name")?, get("user.email")?))
}

/// Whether `program` is on `PATH`.
pub fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(network: AgentNetwork) -> Profile {
        let mut p = Profile::new(
            "codex",
            network,
            Path::new("/home/ana"),
            Path::new("/home/ana/src/app/.agentux/run-1"),
        )
        .unwrap();
        p.git_dir = Some(PathBuf::from("/home/ana/src/app/.git"));
        p.bus_socket = Some(PathBuf::from("/run/user/1000/agentux/bus/s1.sock"));
        p.git_identity = Some(("Ana".into(), "ana@example.com".into()));
        p
    }

    /// The value after `flag … first` in `args`.
    fn has(args: &[String], seq: &[&str]) -> bool {
        args.windows(seq.len())
            .any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
    }

    #[test]
    fn outbound_runs_bwrap_inside_pasta_without_the_hosts_loopback() {
        let (program, args) = profile(AgentNetwork::Outbound).command("npx", &["-y".into()]);
        assert_eq!(program, "pasta");
        assert!(has(&args, &["--no-map-gw"]));
        assert!(has(&args, &["-T", "none"]) && has(&args, &["-U", "none"]));
        assert!(has(&args, &["--", "bwrap"]));
        assert!(!args.iter().any(|a| a == "--unshare-net"));
        assert!(has(&args, &["--", "npx", "-y"]));
    }

    #[test]
    fn no_network_is_bwrap_alone_with_its_own_namespace() {
        let (program, args) = profile(AgentNetwork::None).command("npx", &[]);
        assert_eq!(program, "bwrap");
        assert!(has(&args, &["--unshare-net"]));
    }

    #[test]
    fn the_home_is_empty_but_for_the_run_and_the_harness() {
        let p = profile(AgentNetwork::None);
        let (_, args) = p.command("x", &[]);
        assert!(has(&args, &["--tmpfs", "/home/ana"]));
        let wt = "/home/ana/src/app/.agentux/run-1";
        assert!(has(&args, &["--bind", wt, wt]));
        let git = "/home/ana/src/app/.git";
        assert!(has(&args, &["--bind", git, git]));
        let state = "/home/ana/.local/share/agentux/sandbox/state/codex";
        assert!(has(&args, &["--bind", state, state]));
        assert!(has(&args, &["--setenv", "CODEX_HOME", state]));
        let sock = "/run/user/1000/agentux/bus/s1.sock";
        assert!(has(&args, &["--bind", sock, sock]));
        // Nothing else of the home, and never the control socket.
        for arg in &args {
            assert!(!arg.ends_with("agentuxd.sock"), "{arg}");
            assert!(!arg.contains("/.ssh"), "{arg}");
        }
        assert!(has(&args, &["--chdir", wt]));
    }

    #[test]
    fn the_users_session_and_other_harnesses_keys_stay_out() {
        let (_, args) = profile(AgentNetwork::None).command("x", &[]);
        for var in [
            "SSH_AUTH_SOCK",
            "DBUS_SESSION_BUS_ADDRESS",
            "GH_TOKEN",
            "ANTHROPIC_API_KEY",
        ] {
            assert!(has(&args, &["--unsetenv", var]), "{var}");
        }
        // Codex's own key is not removed.
        assert!(!has(&args, &["--unsetenv", "OPENAI_API_KEY"]));
    }

    #[test]
    fn git_has_no_user_config_no_signing_and_the_hosts_identity() {
        let (_, args) = profile(AgentNetwork::None).command("x", &[]);
        assert!(has(&args, &["--setenv", "GIT_CONFIG_GLOBAL", "/dev/null"]));
        assert!(has(
            &args,
            &["--setenv", "GIT_CONFIG_KEY_0", "commit.gpgsign"]
        ));
        assert!(has(&args, &["--setenv", "GIT_CONFIG_VALUE_0", "false"]));
        assert!(has(&args, &["--setenv", "GIT_AUTHOR_NAME", "Ana"]));
        assert!(has(
            &args,
            &["--setenv", "GIT_COMMITTER_EMAIL", "ana@example.com"]
        ));
    }

    #[test]
    fn extra_paths_are_mounted_read_only() {
        let mut p = profile(AgentNetwork::None);
        p.extra_read_only = vec![PathBuf::from("/home/ana/.cargo/registry")];
        let (_, args) = p.command("x", &[]);
        let path = "/home/ana/.cargo/registry";
        assert!(has(&args, &["--ro-bind", path, path]));
    }

    #[test]
    fn credential_locations_and_the_home_are_never_extra_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        for dir in [
            ".cargo/registry",
            ".ssh",
            ".config/gh",
            ".local/share/opencode",
            "projects",
        ] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        std::os::unix::fs::symlink(home.join(".ssh"), home.join("keys")).unwrap();
        let home = fs::canonicalize(&home).unwrap();
        assert_eq!(
            allowed(&home, "~/.cargo/registry").unwrap(),
            home.join(".cargo/registry")
        );
        for refused in [
            "~/.ssh",
            "~/.config",
            "~/.local/share",
            "~",
            "/",
            "~/keys",
            "~/missing",
            "relative",
        ] {
            assert!(allowed(&home, refused).is_err(), "{refused} was allowed");
        }
    }

    #[test]
    fn unverified_harnesses_are_refused() {
        let err = Profile::new(
            "antigravity",
            AgentNetwork::None,
            Path::new("/h"),
            Path::new("/w"),
        )
        .unwrap_err();
        assert!(err.contains("not supported"), "{err}");
    }
}
