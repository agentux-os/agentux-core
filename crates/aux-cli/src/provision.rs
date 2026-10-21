//! `aux provision`: sets a machine up from one file, with no one at it — for
//! fleets of virtual machines. The file says the profile, the user whose
//! agents run there, the Command server to join (with a single-use
//! enrollment token), the node policy and the agent preferences.
//!
//! ```yaml
//! version: 1
//! profile: node
//! hostname: build-01
//! user: dev
//! command:
//!   url: https://command.acme.org
//!   enrollmentToken: auxe1_cmd-..._...
//!   nodeName: build-01
//! policy:
//!   commands:
//!     run.start: allow
//! preferences:
//!   order: [opencode, claude-code]
//! ```
//!
//! `aux provision apply FILE` runs as root. `aux provision auto` (the
//! agentux-provision service on first boot) finds the file itself: the
//! kernel command line (`agentux.provision=/path`), QEMU's fw_cfg
//! (`opt/org.agentux/provision`), a filesystem labelled AGENTUX, `/boot` or
//! `/etc/agentux/provision.yaml`. Each step is idempotent, and a machine is
//! provisioned once (`/var/lib/agentux/provisioned.json`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agentux_api::rpc::AgentPrefs;
use serde::Deserialize;

use crate::Result;

pub const DONE: &str = "/var/lib/agentux/provisioned.json";
const PROFILES: &[&str] = &[
    "workstation",
    "node",
    "command",
    "executive",
    "complete",
    "custom",
];

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provision {
    pub version: u32,
    pub profile: String,
    #[serde(default)]
    pub hostname: Option<String>,
    /// The user whose agentuxd and gateway run on this machine; must exist.
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub command: Option<CommandJoin>,
    /// The node policy (`/etc/agentux/node-policy.yaml`), as that file.
    #[serde(default)]
    pub policy: Option<serde_json::Value>,
    #[serde(default)]
    pub preferences: Option<AgentPrefs>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandJoin {
    pub url: String,
    pub enrollment_token: String,
    #[serde(default)]
    pub node_name: Option<String>,
}

fn name_ok(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !s.starts_with(['-', '.'])
}

impl Provision {
    /// YAML or JSON.
    pub fn parse(text: &str) -> std::result::Result<Provision, String> {
        let provision: Provision =
            serde_saphyr::from_str(text).map_err(|e| format!("not a valid provision file: {e}"))?;
        provision.validate()?;
        Ok(provision)
    }

    fn validate(&self) -> std::result::Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported version {} (this aux reads version 1)",
                self.version
            ));
        }
        if !PROFILES.contains(&self.profile.as_str()) {
            return Err(format!(
                "unknown profile `{}` (one of {})",
                self.profile,
                PROFILES.join(", ")
            ));
        }
        if let Some(host) = &self.hostname
            && !name_ok(host, 63)
        {
            return Err(format!("not a host name: {host}"));
        }
        if let Some(user) = &self.user
            && !name_ok(user, 32)
        {
            return Err(format!("not a user name: {user}"));
        }
        let runs_agents = matches!(
            self.profile.as_str(),
            "workstation" | "node" | "complete" | "custom"
        );
        if let Some(join) = &self.command {
            if agentux_gateway::token_instance(&join.enrollment_token).is_none() {
                return Err("command.enrollmentToken is not an AgentUX Command enrollment token (auxe1_...)".into());
            }
            if !runs_agents {
                return Err(format!(
                    "a {} machine runs no node to join Command",
                    self.profile
                ));
            }
            if self.user.is_none() {
                return Err(
                    "joining Command needs `user`: the account whose agents run here".into(),
                );
            }
            if let Some(name) = &join.node_name
                && !name_ok(name, 63)
            {
                return Err(format!("not a node name: {name}"));
            }
        }
        if self.preferences.is_some() && self.user.is_none() {
            return Err("`preferences` need `user`".into());
        }
        if let Some(prefs) = &self.preferences {
            agentuxd::prefs::validate(prefs)?;
        }
        if let Some(policy) = &self.policy {
            agentux_gateway::policy::Policy::parse(
                &serde_json::to_string(policy).map_err(|e| e.to_string())?,
            )?;
        }
        Ok(())
    }
}

/// What provisioning does to the machine: real in `aux`, recorded in tests.
pub trait System {
    fn run(
        &mut self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> std::result::Result<(), String>;
    fn write(
        &mut self,
        path: &Path,
        text: &str,
        mode: u32,
        owner: Option<&str>,
    ) -> std::result::Result<(), String>;
    fn home(&self, user: &str) -> std::result::Result<PathBuf, String>;
}

/// Applies a provision; returns what it did, one line per step.
pub fn apply(
    provision: &Provision,
    system: &mut impl System,
) -> std::result::Result<Vec<String>, String> {
    let mut done = Vec::new();
    if let Some(host) = &provision.hostname {
        system.run("hostnamectl", &["set-hostname", host], &[])?;
        done.push(format!("host name {host}"));
    }
    system.run("agentux", &["profile", "seed", &provision.profile], &[])?;
    done.push(format!(
        "profile {} (applied by agentux-profile-apply)",
        provision.profile
    ));
    if let Some(policy) = &provision.policy {
        let yaml = serde_json::to_string_pretty(policy).map_err(|e| e.to_string())?;
        system.write(
            Path::new(agentux_gateway::policy::SYSTEM_POLICY),
            &yaml,
            0o644,
            None,
        )?;
        done.push(format!(
            "node policy {}",
            agentux_gateway::policy::SYSTEM_POLICY
        ));
    }
    let Some(user) = &provision.user else {
        return Ok(done);
    };
    let home = system.home(user)?;
    // Its agentuxd and gateway run without anyone logged in.
    system.run("loginctl", &["enable-linger", user], &[])?;
    done.push(format!("{user}'s services run without a login"));
    if let Some(prefs) = &provision.preferences {
        let path = home.join(".config/agentux/agents.yaml");
        system.write(&path, &agentuxd::prefs::to_yaml(prefs), 0o644, Some(user))?;
        done.push(format!("agent preferences {}", path.display()));
    }
    if let Some(join) = &provision.command {
        let mut args = vec![
            "-u",
            user.as_str(),
            "--",
            "aux",
            "node",
            "enroll",
            join.url.as_str(),
        ];
        if let Some(name) = &join.node_name {
            args.extend(["--node-id", name.as_str()]);
        }
        let home_text = home.to_string_lossy().into_owned();
        system.run(
            "runuser",
            &args,
            &[
                ("AGENTUX_NODE_TOKEN", join.enrollment_token.as_str()),
                ("HOME", home_text.as_str()),
            ],
        )?;
        let machine = format!("{user}@.host");
        system.run(
            "systemctl",
            &[
                "--user",
                "--machine",
                &machine,
                "enable",
                "--now",
                "agentuxd.service",
                "agentux-gateway.service",
            ],
            &[],
        )?;
        done.push(format!("joined Command at {}", join.url));
    }
    Ok(done)
}

/// The real machine.
pub struct Host;

impl System for Host {
    fn run(
        &mut self,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> std::result::Result<(), String> {
        let output = Command::new(program)
            .args(args)
            .envs(env.iter().copied())
            .output()
            .map_err(|e| format!("cannot run {program}: {e}"))?;
        if output.status.success() {
            return Ok(());
        }
        Err(format!(
            "{program} {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }

    fn write(
        &mut self,
        path: &Path,
        text: &str,
        mode: u32,
        owner: Option<&str>,
    ) -> std::result::Result<(), String> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|e| e.to_string())?;
        }
        if let Some(owner) = owner {
            // The user's own config directories, then the file.
            let mut dirs = Vec::new();
            let home = self.home(owner)?;
            let mut dir = path.parent();
            while let Some(d) = dir {
                if d == home || !d.starts_with(&home) {
                    break;
                }
                dirs.push(d.to_string_lossy().into_owned());
                dir = d.parent();
            }
            let mut args = vec![format!("{owner}:"), path.to_string_lossy().into_owned()];
            args.extend(dirs);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.run("chown", &args, &[])?;
        }
        Ok(())
    }

    fn home(&self, user: &str) -> std::result::Result<PathBuf, String> {
        let passwd = fs::read_to_string("/etc/passwd").map_err(|e| e.to_string())?;
        passwd
            .lines()
            .map(|l| l.split(':').collect::<Vec<_>>())
            .find(|f| f.len() > 5 && f[0] == user)
            .map(|f| PathBuf::from(f[5]))
            .ok_or_else(|| format!("no user {user} on this machine"))
    }
}

/// Where `auto` looks, in order.
pub fn sources(cmdline: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for word in cmdline.split_whitespace() {
        if let Some(path) = word.strip_prefix("agentux.provision=")
            && path.starts_with('/')
        {
            out.push(PathBuf::from(path));
        }
    }
    out.push(PathBuf::from(
        "/sys/firmware/qemu_fw_cfg/by_name/opt/org.agentux/provision/raw",
    ));
    out.push(PathBuf::from(
        "/run/agentux/provision-media/agentux-provision.yaml",
    ));
    out.push(PathBuf::from("/boot/agentux-provision.yaml"));
    out.push(PathBuf::from("/etc/agentux/provision.yaml"));
    out
}

pub fn run_apply(file: &Path, dry_run: bool) -> Result {
    let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let provision = Provision::parse(&text)?;
    if dry_run {
        println!("{} is valid: profile {}", file.display(), provision.profile);
        return Ok(());
    }
    for line in apply(&provision, &mut Host)? {
        println!("provisioned: {line}");
    }
    mark_done(file)?;
    Ok(())
}

pub fn run_auto() -> Result {
    if Path::new(DONE).exists() {
        println!("already provisioned ({DONE})");
        return Ok(());
    }
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    // fw_cfg needs its module; a missing one only means no fw_cfg file.
    let _ = Command::new("modprobe").arg("qemu_fw_cfg").output();
    mount_media();
    let found = sources(&cmdline).into_iter().find(|p| p.is_file());
    let Some(file) = found else {
        println!("no provision file; nothing to do");
        return Ok(());
    };
    println!("provisioning from {}", file.display());
    let result = run_apply(&file, false);
    let _ = Command::new("umount")
        .arg("/run/agentux/provision-media")
        .output();
    result?;
    // The enrollment token is spent; do not leave it lying around.
    if file.starts_with("/etc") || file.starts_with("/boot") {
        let _ = fs::remove_file(&file);
    }
    Ok(())
}

/// Mounts a filesystem labelled AGENTUX (an ISO or small disk attached to
/// the VM) read-only.
fn mount_media() {
    let device = Path::new("/dev/disk/by-label/AGENTUX");
    if !device.exists() {
        return;
    }
    let target = "/run/agentux/provision-media";
    let _ = fs::create_dir_all(target);
    let _ = Command::new("mount")
        .args(["-o", "ro"])
        .arg(device)
        .arg(target)
        .output();
}

fn mark_done(file: &Path) -> Result {
    let record = serde_json::json!({
        "source": file.to_string_lossy(),
        "at": jiff::Timestamp::now().to_string(),
    });
    if let Some(dir) = Path::new(DONE).parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(DONE, record.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Recorded {
        runs: Vec<String>,
        files: BTreeMap<PathBuf, (String, Option<String>)>,
    }

    impl System for Recorded {
        fn run(
            &mut self,
            program: &str,
            args: &[&str],
            env: &[(&str, &str)],
        ) -> std::result::Result<(), String> {
            let env: Vec<String> = env.iter().map(|(k, _)| format!("{k}=…")).collect();
            self.runs.push(format!(
                "{}{program} {}",
                if env.is_empty() {
                    String::new()
                } else {
                    format!("{} ", env.join(" "))
                },
                args.join(" ")
            ));
            Ok(())
        }
        fn write(
            &mut self,
            path: &Path,
            text: &str,
            _: u32,
            owner: Option<&str>,
        ) -> std::result::Result<(), String> {
            self.files.insert(
                path.to_path_buf(),
                (text.to_string(), owner.map(str::to_string)),
            );
            Ok(())
        }
        fn home(&self, user: &str) -> std::result::Result<PathBuf, String> {
            Ok(PathBuf::from(format!("/var/home/{user}")))
        }
    }

    const TOKEN: &str = "auxe1_cmd-0123456789abcdef_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn full() -> String {
        format!(
            "version: 1\nprofile: node\nhostname: build-01\nuser: dev\ncommand:\n  url: https://command.acme.org\n  enrollmentToken: {TOKEN}\n  nodeName: build-01\npolicy:\n  commands:\n    run.start: allow\npreferences:\n  order: [opencode]\n"
        )
    }

    #[test]
    fn a_node_is_set_up_and_joins_command() {
        let provision = Provision::parse(&full()).unwrap();
        let mut system = Recorded::default();
        let done = apply(&provision, &mut system).unwrap();
        assert_eq!(done.len(), 6, "{done:?}");
        assert_eq!(
            system.runs,
            [
                "hostnamectl set-hostname build-01",
                "agentux profile seed node",
                "loginctl enable-linger dev",
                "AGENTUX_NODE_TOKEN=… HOME=… runuser -u dev -- aux node enroll https://command.acme.org --node-id build-01",
                "systemctl --user --machine dev@.host enable --now agentuxd.service agentux-gateway.service",
            ]
        );
        let (policy, owner) = &system.files[Path::new("/etc/agentux/node-policy.yaml")];
        assert!(policy.contains("run.start") && owner.is_none());
        let (prefs, owner) = &system.files[Path::new("/var/home/dev/.config/agentux/agents.yaml")];
        assert!(prefs.contains("order") && owner.as_deref() == Some("dev"));
        // The token never shows in what was run.
        assert!(!system.runs.join(" ").contains("auxe1_"));
    }

    #[test]
    fn bad_files_are_refused_before_anything_changes() {
        let refuse = |text: &str| Provision::parse(text).unwrap_err();
        assert!(refuse("version: 2\nprofile: node\n").contains("version"));
        assert!(refuse("version: 1\nprofile: server\n").contains("unknown profile"));
        assert!(refuse("version: 1\nprofile: node\nsecret: x\n").contains("not a valid"));
        assert!(refuse(&format!("version: 1\nprofile: node\ncommand:\n  url: https://c\n  enrollmentToken: {TOKEN}\n")).contains("needs `user`"));
        assert!(refuse(&format!("version: 1\nprofile: executive\nuser: dev\ncommand:\n  url: https://c\n  enrollmentToken: {TOKEN}\n")).contains("runs no node"));
        assert!(refuse("version: 1\nprofile: node\nuser: dev\ncommand:\n  url: https://c\n  enrollmentToken: static\n").contains("enrollment token"));
        assert!(
            refuse("version: 1\nprofile: node\npolicy:\n  commands:\n    shell.exec: allow\n")
                .contains("shell.exec")
        );
        assert!(refuse("version: 1\nprofile: node\nhostname: 'a b'\n").contains("host name"));
        // JSON works too.
        assert!(Provision::parse(r#"{"version": 1, "profile": "workstation"}"#).is_ok());
    }

    #[test]
    fn sources_follow_the_kernel_command_line_first() {
        let found = sources("quiet agentux.provision=/run/media/p.yaml ro");
        assert_eq!(found[0], PathBuf::from("/run/media/p.yaml"));
        assert_eq!(
            found.last().unwrap(),
            &PathBuf::from("/etc/agentux/provision.yaml")
        );
        assert_eq!(
            sources("agentux.provision=relative")[0],
            PathBuf::from("/sys/firmware/qemu_fw_cfg/by_name/opt/org.agentux/provision/raw")
        );
    }
}
