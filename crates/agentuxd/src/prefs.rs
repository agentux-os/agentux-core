//! The user's agent preferences: `$XDG_CONFIG_HOME/agentux/agents.yaml`
//! ([`AgentPrefs`]). They hold for every project; a project's
//! `agentux.yaml` comes first. The daemon reads the file each time it needs
//! it, so an edit applies to the next run without a restart.

use std::path::{Path, PathBuf};
use std::{env, fs, io};

use agentux_api::rpc::AgentPrefs;
use agentux_harness::HarnessSpec;

/// The file the daemon reads: `$XDG_CONFIG_HOME/agentux/agents.yaml`, else
/// `~/.config/agentux/agents.yaml`.
pub fn default_path() -> Option<PathBuf> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("agentux").join("agents.yaml"))
}

/// The preferences in `path`; the defaults when there is no file.
pub fn load(path: &Path) -> Result<AgentPrefs, String> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(AgentPrefs::default()),
        Ok(text) => {
            let prefs: AgentPrefs = serde_saphyr::from_str(&text)
                .map_err(|e| format!("{} is invalid: {e}", path.display()))?;
            validate(&prefs).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(prefs)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(AgentPrefs::default()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Harness ids must be built-in ones, names and models not empty, minutes
/// above zero.
pub fn validate(prefs: &AgentPrefs) -> Result<(), String> {
    let known = |id: &str, at: &str| {
        if HarnessSpec::find(id).is_some() {
            Ok(())
        } else {
            let ids: Vec<String> = HarnessSpec::builtin().into_iter().map(|s| s.id).collect();
            Err(format!(
                "{at}: unknown harness `{id}` (known: {})",
                ids.join(", ")
            ))
        }
    };
    for id in &prefs.order {
        known(id, "order")?;
    }
    for id in prefs.fallback.iter().flatten() {
        known(id, "fallback")?;
    }
    for (role, pref) in &prefs.roles {
        if role.trim().is_empty() {
            return Err("roles: a role name is empty".into());
        }
        if let Some(id) = &pref.harness {
            known(id, &format!("roles.{role}.harness"))?;
        }
        if pref.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
            return Err(format!("roles.{role}.model: empty"));
        }
    }
    for (name, minutes) in [
        ("idle_minutes", prefs.idle_minutes),
        ("tool_minutes", prefs.tool_minutes),
    ] {
        if minutes == Some(0) {
            return Err(format!("{name}: must be at least 1"));
        }
    }
    Ok(())
}

/// Writes `prefs` to `path` (validated, atomically).
pub fn save(path: &Path, prefs: &AgentPrefs) -> Result<(), String> {
    validate(prefs)?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("yaml.tmp");
    fs::write(&tmp, to_yaml(prefs)).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The preferences as the YAML file the user may also edit by hand.
pub fn to_yaml(prefs: &AgentPrefs) -> String {
    // JSON strings are valid YAML scalars, quoted and escaped.
    let q = |s: &str| serde_json::to_string(s).unwrap_or_default();
    let list = |items: &[String]| {
        let items: Vec<String> = items.iter().map(|s| q(s)).collect();
        format!("[{}]", items.join(", "))
    };
    let mut out = String::from(
        "# AgentUX agent preferences: which agents runs use, for projects without\n\
         # an agentux.yaml. The cockpit's Agents view writes this file.\n",
    );
    out.push_str("# Harness ids, most preferred first.\n");
    out.push_str(&format!("order: {}\n", list(&prefs.order)));
    if prefs.roles.is_empty() {
        out.push_str("roles: {}\n");
    } else {
        out.push_str("roles:\n");
        for (role, pref) in &prefs.roles {
            out.push_str(&format!("  {}:", q(role)));
            if pref.harness.is_none() && pref.model.is_none() {
                out.push_str(" {}\n");
                continue;
            }
            out.push('\n');
            if let Some(harness) = &pref.harness {
                out.push_str(&format!("    harness: {}\n", q(harness)));
            }
            if let Some(model) = &pref.model {
                out.push_str(&format!("    model: {}\n", q(model)));
            }
        }
    }
    if let Some(fallback) = &prefs.fallback {
        out.push_str("# Agents that take a role while its own is at a usage limit.\n");
        out.push_str(&format!("fallback: {}\n", list(fallback)));
    }
    out.push_str("# Minutes without a sign of life before a turn is given up on.\n");
    for (name, minutes) in [
        ("idle_minutes", prefs.idle_minutes),
        ("tool_minutes", prefs.tool_minutes),
    ] {
        if let Some(minutes) = minutes {
            out.push_str(&format!("{name}: {minutes}\n"));
        }
    }
    out.push_str(&format!("notifications: {}\n", prefs.notifications));
    out
}

/// Harness ids in the user's order: `order` first, then the other built-in
/// harnesses in the built-in order.
pub fn ordered(prefs: &AgentPrefs) -> Vec<String> {
    let builtin: Vec<String> = HarnessSpec::builtin().into_iter().map(|s| s.id).collect();
    let mut out: Vec<String> = Vec::new();
    for id in prefs.order.iter().chain(&builtin) {
        if builtin.contains(id) && !out.contains(id) {
            out.push(id.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentux_api::rpc::RolePref;

    #[test]
    fn written_preferences_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentux/agents.yaml");
        assert_eq!(load(&path).unwrap(), AgentPrefs::default());

        let prefs = AgentPrefs {
            order: vec!["opencode".into(), "claude-code".into()],
            roles: [(
                "planner".to_string(),
                RolePref {
                    harness: Some("claude-code".into()),
                    model: Some("opus".into()),
                },
            )]
            .into(),
            fallback: Some(vec!["opencode".into()]),
            idle_minutes: Some(15),
            tool_minutes: None,
            notifications: false,
        };
        save(&path, &prefs).unwrap();
        assert_eq!(load(&path).unwrap(), prefs);
        assert_eq!(
            ordered(&prefs),
            ["opencode", "claude-code", "codex", "antigravity"]
        );
    }

    #[test]
    fn hand_written_snake_case_files_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.yaml");
        fs::write(
            &path,
            "order: [antigravity]\nroles:\n  reviewer:\n    harness: codex\nidle_minutes: 5\n",
        )
        .unwrap();
        let prefs = load(&path).unwrap();
        assert_eq!(prefs.idle_minutes, Some(5));
        assert!(prefs.notifications);
        assert_eq!(prefs.roles["reviewer"].harness.as_deref(), Some("codex"));
    }

    #[test]
    fn unknown_harnesses_and_zero_minutes_are_refused() {
        let bad = |prefs: AgentPrefs| validate(&prefs).unwrap_err();
        assert!(
            bad(AgentPrefs {
                order: vec!["nope".into()],
                ..Default::default()
            })
            .contains("unknown harness `nope`")
        );
        assert!(
            bad(AgentPrefs {
                tool_minutes: Some(0),
                ..Default::default()
            })
            .contains("tool_minutes")
        );
    }
}
