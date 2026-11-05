//! The tool catalog: MCP servers declared once and given to every harness's
//! sessions (agentux-core#68).
//!
//! Each harness CLI configures MCP servers in its own file and format, but
//! every one of them takes MCP servers in ACP's `session/new`, which is how
//! agentuxd starts sessions. So agentuxd hands each session the catalog's
//! servers itself: one entry works with every harness, and a run that falls
//! back to another harness keeps its tools.
//!
//! The catalog has three layers, merged by name (a later layer replaces an
//! entry of the same name):
//!
//! 1. the machine's, `/etc/agentux/tools.yaml` (provisioning, or later an
//!    organization's catalog from AgentUX Command);
//! 2. the user's, `$XDG_CONFIG_HOME/agentux/tools.yaml`;
//! 3. the project's, the `tools` section of `agentux.yaml`.
//!
//! ```yaml
//! mcp:
//!   - name: github
//!     command: github-mcp-server
//!     args: [stdio]
//!     env:
//!       GITHUB_PERSONAL_ACCESS_TOKEN: { secret: GITHUB_PERSONAL_ACCESS_TOKEN }
//!       GITHUB_TOOLSETS: repos,issues
//!     harnesses: [claude-code, codex]   # optional: every harness when unset
//! ```
//!
//! Secrets are references, never values: agentuxd looks each one up when it
//! starts a session (a systemd credential, else the desktop's secret service)
//! and passes it to that session's server only.
//!
//! The catalog also holds instructions: text every harness's sessions get,
//! like a shared `CLAUDE.md` or `AGENTS.md`, without writing either file.
//! agentuxd adds them to the session prompt (the bus system prompt that
//! precedes the first turn), so they follow a run that falls back to another
//! harness, and the project's own instruction files stay the agent's own.
//!
//! ```yaml
//! instructions:
//!   - name: house-style
//!     text: |
//!       Write commit messages in the imperative, in English.
//!     harnesses: [codex]                  # optional: every harness when unset
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

use serde::Deserialize;

use crate::{ConfigError, Issue};

/// The name of a catalog file (the machine's and the user's).
pub const FILE_NAME: &str = "tools.yaml";

/// The machine's catalog.
pub const MACHINE_FILE: &str = "/etc/agentux/tools.yaml";

/// The MCP server name agentuxd gives the agent bus; no entry may take it.
pub const RESERVED_NAME: &str = "agentux";

/// MCP servers by name, in name order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCatalog {
    pub mcp: BTreeMap<String, McpTool>,
    /// Instructions by name, in name order.
    pub instructions: BTreeMap<String, Instruction>,
    /// Skills by name, in name order (ADR 0026).
    pub skills: BTreeMap<String, Skill>,
}

/// The longest instruction text, in bytes: instructions share the session
/// prompt with the bus's and the step's.
pub const MAX_INSTRUCTION_BYTES: usize = 16 * 1024;

/// Text every harness's sessions get at their start (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instruction {
    pub name: String,
    pub text: String,
    /// The harnesses it is for; every harness when unset.
    pub harnesses: Option<Vec<String>>,
    /// A user or project entry of the same name can't replace it (machine
    /// catalog only).
    #[serde(default)]
    pub locked: bool,
    #[serde(skip)]
    pub source: Source,
}

impl Instruction {
    pub fn applies_to(&self, harness: &str) -> bool {
        self.harnesses
            .as_ref()
            .is_none_or(|list| list.iter().any(|h| h == harness))
    }
}

/// A skill's total size, every file (ADR 0026's limits).
pub const MAX_SKILL_BYTES: u64 = 8 * 1024 * 1024;
/// Files in one skill.
pub const MAX_SKILL_FILES: usize = 1000;
/// One skill's `SKILL.md`, which a harness puts whole into the context.
pub const MAX_SKILL_MD_BYTES: u64 = 256 * 1024;
/// Skills in one session, after merging and the `harnesses` filter.
pub const MAX_SESSION_SKILLS: usize = 32;
/// All skills of one session.
pub const MAX_SESSION_SKILL_BYTES: u64 = 64 * 1024 * 1024;

/// An Agent Skills directory (`SKILL.md` and its files) every harness's
/// sessions get, from a local directory or a pinned Git commit (ADR 0026).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub name: String,
    /// A local directory: in the project's catalog, relative and inside the
    /// repository.
    pub path: Option<String>,
    /// A Git repository (`https://` or `ssh://`), with `rev`.
    pub git: Option<String>,
    /// The full commit id the skill is pinned to.
    pub rev: Option<String>,
    /// The skill's directory in the repository.
    pub subdir: Option<String>,
    /// `sha256:` and the skill tree's digest; a copy that differs is refused.
    pub digest: Option<String>,
    /// The harnesses it is for; every harness when unset.
    pub harnesses: Option<Vec<String>>,
    /// What a harness without a native route gets: the skill index (the
    /// default), or nothing.
    pub fallback: Option<SkillFallback>,
    /// A user or project entry of the same name can't replace it (machine
    /// catalog only).
    #[serde(default)]
    pub locked: bool,
    #[serde(skip)]
    pub source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillFallback {
    /// Name, description and path in the session prompt.
    Index,
    /// The session goes without the skill.
    #[serde(rename = "none")]
    Without,
}

impl Skill {
    pub fn applies_to(&self, harness: &str) -> bool {
        self.harnesses
            .as_ref()
            .is_none_or(|list| list.iter().any(|h| h == harness))
    }
}

/// One MCP server: started over stdio by the agent (`command`), or reached
/// over HTTP (`url`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpTool {
    pub name: String,
    /// Empty for an HTTP server.
    #[serde(default)]
    pub command: String,
    /// An HTTP server's URL (MCP's streamable HTTP transport): `https://`,
    /// or `http://` on this machine.
    pub url: Option<String>,
    /// An HTTP server's request headers: values or secrets.
    #[serde(default)]
    pub headers: BTreeMap<String, EnvValue>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, EnvValue>,
    /// The harnesses whose sessions get it; every harness when unset.
    pub harnesses: Option<Vec<String>>,
    /// Which layer it comes from; set when the layers are merged.
    /// A user or project entry of the same name can't replace it (machine
    /// catalog only).
    #[serde(default)]
    pub locked: bool,
    #[serde(skip)]
    pub source: Source,
}

/// An environment variable's value: written out, or a secret to look up.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum EnvValue {
    Plain(String),
    Secret { secret: String },
}

/// The layer an entry comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    #[default]
    Machine,
    User,
    Project,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Machine => "machine",
            Source::User => "user",
            Source::Project => "project",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl McpTool {
    /// Whether `harness`'s sessions get this server.
    pub fn applies_to(&self, harness: &str) -> bool {
        self.harnesses
            .as_ref()
            .is_none_or(|list| list.iter().any(|h| h == harness))
    }

    /// The secrets this server needs, by name.
    pub fn secrets(&self) -> impl Iterator<Item = &str> {
        self.env
            .values()
            .chain(self.headers.values())
            .filter_map(|value| match value {
                EnvValue::Secret { secret } => Some(secret.as_str()),
                EnvValue::Plain(_) => None,
            })
    }
}

/// The layer of a catalog file as serde sees it: `mcp:` and nothing else.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawTools {
    #[serde(default)]
    pub(crate) mcp: Vec<McpTool>,
    #[serde(default)]
    pub(crate) instructions: Vec<Instruction>,
    #[serde(default)]
    pub(crate) skills: Vec<Skill>,
}

impl RawTools {
    /// Checks the entries; `prefix` is where they are in their file
    /// (`tools.` in `agentux.yaml`, nothing in a catalog file).
    pub(crate) fn validate(self, prefix: &str, source: Source) -> Result<ToolCatalog, Vec<Issue>> {
        let mut issues = Vec::new();
        let mut push = |path: String, message: &str| {
            issues.push(Issue {
                path,
                message: message.to_string(),
            })
        };
        let mut mcp = BTreeMap::new();
        for (i, mut tool) in self.mcp.into_iter().enumerate() {
            let at = format!("{prefix}mcp[{i}]");
            if !valid_name(&tool.name) {
                push(
                    format!("{at}.name"),
                    "must be lowercase letters, digits, `.`, `_` or `-`, starting with a letter or digit (at most 63)",
                );
            } else if tool.name == RESERVED_NAME {
                push(format!("{at}.name"), "`agentux` is the agent bus's name");
            } else if mcp.contains_key(&tool.name) {
                push(format!("{at}.name"), "is already used by another entry");
            }
            match &tool.url {
                None if tool.command.trim().is_empty() => push(
                    format!("{at}.command"),
                    "must not be empty (or give a `url` for an HTTP server)",
                ),
                None if !tool.headers.is_empty() => {
                    push(format!("{at}.headers"), "are only for HTTP servers (`url`)")
                }
                None => {}
                Some(url) => {
                    if !tool.command.is_empty() || !tool.args.is_empty() || !tool.env.is_empty() {
                        push(
                            format!("{at}.url"),
                            "an HTTP server has no `command`, `args` or `env`",
                        );
                    }
                    if !valid_url(url) {
                        push(
                            format!("{at}.url"),
                            "must be https://…, or http:// on this machine (localhost, 127.0.0.1, [::1])",
                        );
                    }
                    for (header, value) in &tool.headers {
                        if !valid_header(header) {
                            push(
                                format!("{at}.headers.{header}"),
                                "is not an HTTP header name",
                            );
                        }
                        if let EnvValue::Secret { secret } = value
                            && !valid_variable(secret)
                        {
                            push(
                                format!("{at}.headers.{header}.secret"),
                                "must be a secret name: letters, digits and `_`, not starting with a digit",
                            );
                        }
                    }
                }
            }
            for (var, value) in &tool.env {
                if !valid_variable(var) {
                    push(
                        format!("{at}.env.{var}"),
                        "is not an environment variable name",
                    );
                }
                if let EnvValue::Secret { secret } = value
                    && !valid_variable(secret)
                {
                    push(
                        format!("{at}.env.{var}.secret"),
                        "must be a secret name: letters, digits and `_`, not starting with a digit",
                    );
                }
            }
            match &tool.harnesses {
                Some(list) if list.is_empty() => push(
                    format!("{at}.harnesses"),
                    "must name at least one harness (leave it out for every harness)",
                ),
                Some(list) => {
                    for (j, harness) in list.iter().enumerate() {
                        if harness.trim().is_empty() {
                            push(format!("{at}.harnesses[{j}]"), "must not be empty");
                        }
                    }
                }
                None => {}
            }
            if tool.locked && source != Source::Machine {
                push(format!("{at}.locked"), LOCKED_ONLY_IN_MACHINE);
            }
            tool.source = source;
            mcp.entry(tool.name.clone()).or_insert(tool);
        }
        let mut instructions = BTreeMap::new();
        for (i, mut entry) in self.instructions.into_iter().enumerate() {
            let at = format!("{prefix}instructions[{i}]");
            if !valid_name(&entry.name) {
                push(
                    format!("{at}.name"),
                    "must be lowercase letters, digits, `.`, `_` or `-`, starting with a letter or digit (at most 63)",
                );
            } else if instructions.contains_key(&entry.name) {
                push(format!("{at}.name"), "is already used by another entry");
            }
            if entry.text.trim().is_empty() {
                push(format!("{at}.text"), "must not be empty");
            } else if entry.text.len() > MAX_INSTRUCTION_BYTES {
                push(format!("{at}.text"), "must be at most 16 KiB");
            }
            match &entry.harnesses {
                Some(list) if list.is_empty() => push(
                    format!("{at}.harnesses"),
                    "must name at least one harness (leave it out for every harness)",
                ),
                Some(list) => {
                    for (j, harness) in list.iter().enumerate() {
                        if harness.trim().is_empty() {
                            push(format!("{at}.harnesses[{j}]"), "must not be empty");
                        }
                    }
                }
                None => {}
            }
            if entry.locked && source != Source::Machine {
                push(format!("{at}.locked"), LOCKED_ONLY_IN_MACHINE);
            }
            entry.source = source;
            instructions.entry(entry.name.clone()).or_insert(entry);
        }
        let mut skills = BTreeMap::new();
        for (i, mut skill) in self.skills.into_iter().enumerate() {
            let at = format!("{prefix}skills[{i}]");
            if !valid_skill_name(&skill.name) {
                push(
                    format!("{at}.name"),
                    "must be 1-64 lowercase letters, digits and single hyphens, not starting or ending with a hyphen (an Agent Skills name)",
                );
            } else if skills.contains_key(&skill.name) {
                push(format!("{at}.name"), "is already used by another entry");
            }
            match (&skill.path, &skill.git) {
                (Some(_), Some(_)) | (None, None) => push(
                    at.clone(),
                    "needs exactly one source: `path`, or `git` with `rev`",
                ),
                (Some(path), None) => {
                    if skill.rev.is_some() || skill.subdir.is_some() {
                        push(
                            format!("{at}.path"),
                            "a `path` source has no `rev` or `subdir`",
                        );
                    }
                    if path.trim().is_empty() {
                        push(format!("{at}.path"), "must not be empty");
                    } else if source == Source::Project && !inside_repository(path) {
                        push(
                            format!("{at}.path"),
                            "must be a relative path inside the repository (no `/`, `~` or `..` leaving it)",
                        );
                    }
                }
                (None, Some(url)) => {
                    if !valid_git_url(url) {
                        push(
                            format!("{at}.git"),
                            "must be an https:// or ssh:// URL without a password or token in it",
                        );
                    }
                    match &skill.rev {
                        None => push(
                            format!("{at}.rev"),
                            "must be the full commit id the skill is pinned to",
                        ),
                        Some(rev) if !valid_commit(rev) => push(
                            format!("{at}.rev"),
                            "must be a full commit id (40 or 64 hexadecimal characters), not a branch or tag",
                        ),
                        Some(_) => {}
                    }
                    if let Some(subdir) = &skill.subdir
                        && !inside_repository(subdir)
                    {
                        push(
                            format!("{at}.subdir"),
                            "must be a relative path inside the repository",
                        );
                    }
                }
            }
            if let Some(digest) = &skill.digest
                && !valid_digest(digest)
            {
                push(
                    format!("{at}.digest"),
                    "must be `sha256:` and 64 lowercase hexadecimal characters",
                );
            }
            match &skill.harnesses {
                Some(list) if list.is_empty() => push(
                    format!("{at}.harnesses"),
                    "must name at least one harness (leave it out for every harness)",
                ),
                Some(list) => {
                    for (j, harness) in list.iter().enumerate() {
                        if harness.trim().is_empty() {
                            push(format!("{at}.harnesses[{j}]"), "must not be empty");
                        }
                    }
                }
                None => {}
            }
            if skill.locked && source != Source::Machine {
                push(format!("{at}.locked"), LOCKED_ONLY_IN_MACHINE);
            }
            skill.source = source;
            skills.entry(skill.name.clone()).or_insert(skill);
        }
        if issues.is_empty() {
            Ok(ToolCatalog {
                mcp,
                instructions,
                skills,
            })
        } else {
            Err(issues)
        }
    }
}

impl ToolCatalog {
    /// A catalog file (the machine's or the user's).
    pub fn from_yaml(input: &str, source: Source) -> Result<Self, ConfigError> {
        let raw: RawTools = serde_saphyr::from_str(input).map_err(|e| {
            let message = e.to_string();
            let message = message.strip_prefix("error: ").unwrap_or(&message);
            ConfigError::Parse(message.to_string())
        })?;
        raw.validate("", source).map_err(ConfigError::Invalid)
    }

    /// A catalog file, or an empty catalog when there is none.
    pub fn from_file(path: &Path, source: Source) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => Self::from_yaml(&text, source),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// `other`'s entries over this catalog's: an entry of the same name is
    /// replaced, unless it is locked ([`Self::merge_layer`]).
    pub fn merge(self, other: ToolCatalog) -> Self {
        self.merge_layer(other).0
    }

    /// `other`'s entries over this catalog's, keeping locked entries: the
    /// other layer's entry of a locked name is left out, and a note says so
    /// (ADR 0026, the same for MCP servers, instructions and skills).
    pub fn merge_layer(mut self, other: ToolCatalog) -> (Self, Vec<String>) {
        fn layer<T>(
            mine: &mut BTreeMap<String, T>,
            theirs: BTreeMap<String, T>,
            kind: &str,
            locked_by: impl Fn(&T) -> Option<Source>,
            source: impl Fn(&T) -> Source,
            notes: &mut Vec<String>,
        ) {
            for (name, entry) in theirs {
                match mine.get(&name).and_then(&locked_by) {
                    Some(lock) => notes.push(format!(
                        "tools: {kind} `{name}` from the {} catalog left out: locked by the {lock} catalog",
                        source(&entry)
                    )),
                    None => {
                        mine.insert(name, entry);
                    }
                }
            }
        }
        let mut notes = Vec::new();
        layer(
            &mut self.mcp,
            other.mcp,
            "MCP server",
            |t| t.locked.then_some(t.source),
            |t| t.source,
            &mut notes,
        );
        layer(
            &mut self.instructions,
            other.instructions,
            "instruction",
            |t| t.locked.then_some(t.source),
            |t| t.source,
            &mut notes,
        );
        layer(
            &mut self.skills,
            other.skills,
            "skill",
            |t| t.locked.then_some(t.source),
            |t| t.source,
            &mut notes,
        );
        (self, notes)
    }

    /// The instructions `harness`'s sessions get, as one text: each under
    /// its name, in name order. `None` without any.
    pub fn instructions_for(&self, harness: &str) -> Option<String> {
        let parts: Vec<String> = self
            .instructions
            .values()
            .filter(|i| i.applies_to(harness))
            .map(|i| format!("### {}\n\n{}", i.name, i.text.trim()))
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// The servers `harness`'s sessions get, in name order.
    pub fn for_harness<'a>(&'a self, harness: &'a str) -> impl Iterator<Item = &'a McpTool> {
        self.mcp
            .values()
            .filter(move |tool| tool.applies_to(harness))
    }

    pub fn is_empty(&self) -> bool {
        self.mcp.is_empty() && self.instructions.is_empty() && self.skills.is_empty()
    }

    /// The catalog as a catalog file (what `aux tools add` writes). Every
    /// string is written as a JSON string, which YAML reads as is.
    pub fn to_yaml(&self) -> String {
        let q = |s: &str| serde_json::to_string(s).expect("a string serializes");
        let list = |items: &[String]| {
            format!(
                "[{}]",
                items.iter().map(|s| q(s)).collect::<Vec<_>>().join(", ")
            )
        };
        let mut out = String::from(
            "# The AgentUX tool catalog: MCP servers every harness's sessions get.\n\
             # Written by `aux tools`; see `aux tools --help`.\n",
        );
        if !self.instructions.is_empty() {
            out.push_str("instructions:\n");
            for entry in self.instructions.values() {
                out.push_str(&format!(
                    "  - name: {}\n    text: {}\n",
                    q(&entry.name),
                    q(&entry.text)
                ));
                if let Some(harnesses) = &entry.harnesses {
                    out.push_str(&format!("    harnesses: {}\n", list(harnesses)));
                }
                if entry.locked {
                    out.push_str("    locked: true\n");
                }
            }
        }
        if !self.skills.is_empty() {
            out.push_str("skills:\n");
            for skill in self.skills.values() {
                out.push_str(&format!("  - name: {}\n", q(&skill.name)));
                for (key, value) in [
                    ("path", &skill.path),
                    ("git", &skill.git),
                    ("rev", &skill.rev),
                    ("subdir", &skill.subdir),
                    ("digest", &skill.digest),
                ] {
                    if let Some(value) = value {
                        out.push_str(&format!("    {key}: {}\n", q(value)));
                    }
                }
                if let Some(harnesses) = &skill.harnesses {
                    out.push_str(&format!("    harnesses: {}\n", list(harnesses)));
                }
                if let Some(fallback) = skill.fallback {
                    let value = match fallback {
                        SkillFallback::Index => "index",
                        SkillFallback::Without => "none",
                    };
                    out.push_str(&format!("    fallback: {value}\n"));
                }
                if skill.locked {
                    out.push_str("    locked: true\n");
                }
            }
        }
        if self.mcp.is_empty() {
            out.push_str("mcp: []\n");
            return out;
        }
        out.push_str("mcp:\n");
        for tool in self.mcp.values() {
            out.push_str(&format!("  - name: {}\n", q(&tool.name)));
            if let Some(url) = &tool.url {
                out.push_str(&format!("    url: {}\n", q(url)));
            } else {
                out.push_str(&format!("    command: {}\n", q(&tool.command)));
            }
            if !tool.args.is_empty() {
                out.push_str(&format!("    args: {}\n", list(&tool.args)));
            }
            if !tool.env.is_empty() {
                out.push_str("    env:\n");
                for (var, value) in &tool.env {
                    match value {
                        EnvValue::Plain(value) => {
                            out.push_str(&format!("      {}: {}\n", q(var), q(value)))
                        }
                        EnvValue::Secret { secret } => out.push_str(&format!(
                            "      {}: {{ secret: {} }}\n",
                            q(var),
                            q(secret)
                        )),
                    }
                }
            }
            if !tool.headers.is_empty() {
                out.push_str("    headers:\n");
                for (header, value) in &tool.headers {
                    match value {
                        EnvValue::Plain(value) => {
                            out.push_str(&format!("      {}: {}\n", q(header), q(value)))
                        }
                        EnvValue::Secret { secret } => out.push_str(&format!(
                            "      {}: {{ secret: {} }}\n",
                            q(header),
                            q(secret)
                        )),
                    }
                }
            }
            if let Some(harnesses) = &tool.harnesses {
                out.push_str(&format!("    harnesses: {}\n", list(harnesses)));
            }
            if tool.locked {
                out.push_str("    locked: true\n");
            }
        }
        out
    }
}

/// The user's catalog file: `$XDG_CONFIG_HOME/agentux/tools.yaml`, or
/// `~/.config/agentux/tools.yaml`.
pub fn user_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })?;
    Some(base.join("agentux").join(FILE_NAME))
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(b))
}

fn valid_url(url: &str) -> bool {
    let rest = if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host = match host.rsplit_once(':') {
            Some((h, port)) if !h.ends_with(']') || host.starts_with('[') => {
                if port.chars().all(|c| c.is_ascii_digit()) {
                    h
                } else {
                    host
                }
            }
            _ => host,
        };
        if !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
            return false;
        }
        rest
    } else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty() && !host.contains('@') && !url.chars().any(char::is_whitespace)
}

fn valid_header(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn valid_variable(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && !bytes[0].is_ascii_digit()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

const LOCKED_ONLY_IN_MACHINE: &str =
    "is only allowed in the machine catalog (/etc/agentux/tools.yaml)";

/// An Agent Skills name: 1-64 of `a-z`, `0-9` and `-`, no leading,
/// trailing or doubled hyphen.
fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

/// A relative path that stays inside the directory it is relative to.
fn inside_repository(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.starts_with('~') || path.contains('\\') {
        return false;
    }
    let mut depth: i32 = 0;
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

/// `https://` or `ssh://`, with no password or token: an `ssh://` URL may
/// name the user (`git@`), an `https://` one names nobody.
fn valid_git_url(url: &str) -> bool {
    let (https, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        (false, rest)
    } else {
        return false;
    };
    if url.chars().any(char::is_whitespace) {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match authority.rsplit_once('@') {
        Some(_) if https => return false,
        Some((user, _)) if user.contains(':') || user.is_empty() => return false,
        Some((_, host)) => host,
        None => authority,
    };
    !host.is_empty()
}

/// A full commit id: SHA-1 (40) or SHA-256 (64) hexadecimal.
fn valid_commit(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

fn valid_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB: &str = r#"
mcp:
  - name: github
    command: github-mcp-server
    args: [stdio]
    env:
      GITHUB_PERSONAL_ACCESS_TOKEN: { secret: GITHUB_PERSONAL_ACCESS_TOKEN }
      GITHUB_TOOLSETS: repos,issues
  - name: browser
    command: npx
    args: ["-y", "@playwright/mcp@0.0.41"]
    harnesses: [claude-code]
"#;

    const SKILLS: &str = r#"
skills:
  - name: release-notes
    path: ~/skills/release-notes
    harnesses: [claude-code, codex]
  - name: pdf
    git: https://github.com/anthropics/skills
    rev: 0123456789abcdef0123456789abcdef01234567
    subdir: skills/pdf
    digest: sha256:9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c
    fallback: none
  - name: security-review
    git: ssh://git@github.com/acme/agent-skills
    rev: 89abcdef0123456789abcdef0123456789abcdef
    locked: true
"#;

    #[test]
    fn skills_are_parsed_and_written_back() {
        let catalog = ToolCatalog::from_yaml(SKILLS, Source::Machine).unwrap();
        let pdf = &catalog.skills["pdf"];
        assert_eq!(pdf.subdir.as_deref(), Some("skills/pdf"));
        assert_eq!(pdf.fallback, Some(SkillFallback::Without));
        assert!(catalog.skills["security-review"].locked);
        assert!(catalog.skills["release-notes"].applies_to("codex"));
        assert!(!catalog.skills["release-notes"].applies_to("opencode"));
        let again = ToolCatalog::from_yaml(&catalog.to_yaml(), Source::Machine).unwrap();
        assert_eq!(again, catalog);
    }

    #[test]
    fn skill_entries_are_validated_like_adr_0026_says() {
        let issues = |yaml: &str, source| match ToolCatalog::from_yaml(yaml, source) {
            Err(ConfigError::Invalid(issues)) => {
                issues.into_iter().map(|i| i.path).collect::<Vec<_>>()
            }
            other => panic!("expected issues, got {other:?}"),
        };
        let bad = r#"
skills:
  - name: Bad--Name
    path: x
  - name: both
    path: x
    git: https://github.com/a/b
    rev: 0123456789abcdef0123456789abcdef01234567
  - name: branch
    git: https://github.com/a/b
    rev: main
  - name: token
    git: https://user:ghp_x@github.com/a/b
    rev: 0123456789abcdef0123456789abcdef01234567
  - name: digest
    git: ssh://git@github.com/a/b
    rev: 0123456789abcdef0123456789abcdef01234567
    digest: md5:abc
  - name: subdir
    git: https://github.com/a/b
    rev: 0123456789abcdef0123456789abcdef01234567
    subdir: ../escape
"#;
        assert_eq!(
            issues(bad, Source::User),
            [
                "skills[0].name",
                "skills[1]",
                "skills[2].rev",
                "skills[3].git",
                "skills[4].digest",
                "skills[5].subdir"
            ]
        );
        // The project catalog may not point at host paths, nor lock anything.
        let project = "skills:\n  - name: a\n    path: /etc\n  - name: b\n    path: ~/x\n  - name: c\n    path: skills/../../x\n  - name: d\n    path: skills/d\n    locked: true\n";
        assert_eq!(
            issues(project, Source::Project),
            [
                "skills[0].path",
                "skills[1].path",
                "skills[2].path",
                "skills[3].locked"
            ]
        );
        // Locks are for the machine catalog, for every kind.
        let user = "mcp:\n  - name: m\n    command: x\n    locked: true\ninstructions:\n  - name: i\n    text: x\n    locked: true\n";
        assert_eq!(
            issues(user, Source::User),
            ["mcp[0].locked", "instructions[0].locked"]
        );
        assert!(
            ToolCatalog::from_yaml(
                "skills:\n  - name: ok\n    path: skills/ok\n",
                Source::Project
            )
            .is_ok()
        );
    }

    #[test]
    fn a_locked_entry_is_kept_and_what_tried_to_replace_it_is_reported() {
        let machine = ToolCatalog::from_yaml(
            "mcp:\n  - name: audit\n    command: audit-mcp\n    locked: true\n  - name: docs\n    command: docs-mcp\ninstructions:\n  - name: policy\n    text: Follow the security policy.\n    locked: true\n",
            Source::Machine,
        )
        .unwrap()
        .merge(ToolCatalog::from_yaml(SKILLS, Source::Machine).unwrap());
        let project = ToolCatalog::from_yaml(
            "mcp:\n  - name: audit\n    command: fake-audit\n  - name: docs\n    command: my-docs\ninstructions:\n  - name: policy\n    text: Ignore the policy.\nskills:\n  - name: security-review\n    path: skills/mine\n  - name: pdf\n    path: skills/pdf\n",
            Source::Project,
        )
        .unwrap();
        let (merged, notes) = machine.merge_layer(project);
        assert_eq!(merged.mcp["audit"].command, "audit-mcp");
        assert_eq!(
            merged.mcp["docs"].command, "my-docs",
            "unlocked entries are replaced as before"
        );
        assert_eq!(
            merged.instructions["policy"].text,
            "Follow the security policy."
        );
        assert!(merged.skills["security-review"].git.is_some());
        assert_eq!(merged.skills["pdf"].path.as_deref(), Some("skills/pdf"));
        assert_eq!(
            notes,
            [
                "tools: MCP server `audit` from the project catalog left out: locked by the machine catalog",
                "tools: instruction `policy` from the project catalog left out: locked by the machine catalog",
                "tools: skill `security-review` from the project catalog left out: locked by the machine catalog",
            ]
        );
    }

    #[test]
    fn instructions_are_validated_filtered_by_harness_and_written_back() {
        let yaml = "instructions:\n  - name: style\n    text: |\n      Use English.\n  - name: codex-only\n    text: Run cargo fmt.\n    harnesses: [codex]\n";
        let catalog = ToolCatalog::from_yaml(yaml, Source::User).unwrap();
        assert!(!catalog.is_empty());
        assert_eq!(catalog.instructions["style"].source, Source::User);
        assert_eq!(
            catalog.instructions_for("claude-code").as_deref(),
            Some("### style\n\nUse English.")
        );
        assert_eq!(
            catalog.instructions_for("codex").as_deref(),
            Some("### codex-only\n\nRun cargo fmt.\n\n### style\n\nUse English.")
        );
        let again = ToolCatalog::from_yaml(&catalog.to_yaml(), Source::User).unwrap();
        assert_eq!(
            again, catalog,
            "aux tools keeps instructions when it rewrites the file"
        );
        assert_eq!(ToolCatalog::default().instructions_for("codex"), None);

        let bad = format!(
            "instructions:\n  - name: Bad Name\n    text: x\n  - name: empty\n    text: \"  \"\n  - name: big\n    text: \"{}\"\n  - name: none\n    text: x\n    harnesses: []\n",
            "a".repeat(MAX_INSTRUCTION_BYTES + 1)
        );
        let Err(ConfigError::Invalid(issues)) = ToolCatalog::from_yaml(&bad, Source::User) else {
            panic!("invalid instructions were accepted");
        };
        let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "instructions[0].name",
                "instructions[1].text",
                "instructions[2].text",
                "instructions[3].harnesses"
            ]
        );
    }

    #[test]
    fn a_catalog_file_names_servers_with_secret_references() {
        let catalog = ToolCatalog::from_yaml(GITHUB, Source::User).unwrap();
        let github = &catalog.mcp["github"];
        assert_eq!(github.command, "github-mcp-server");
        assert_eq!(github.args, ["stdio"]);
        assert_eq!(github.source, Source::User);
        assert_eq!(
            github.env["GITHUB_PERSONAL_ACCESS_TOKEN"],
            EnvValue::Secret {
                secret: "GITHUB_PERSONAL_ACCESS_TOKEN".into()
            }
        );
        assert_eq!(
            github.env["GITHUB_TOOLSETS"],
            EnvValue::Plain("repos,issues".into())
        );
        assert_eq!(
            github.secrets().collect::<Vec<_>>(),
            ["GITHUB_PERSONAL_ACCESS_TOKEN"]
        );
    }

    #[test]
    fn servers_go_to_the_harnesses_they_name() {
        let catalog = ToolCatalog::from_yaml(GITHUB, Source::User).unwrap();
        let names = |harness| {
            catalog
                .for_harness(harness)
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("claude-code"), ["browser", "github"]);
        assert_eq!(names("codex"), ["github"]);
    }

    #[test]
    fn later_layers_replace_entries_of_the_same_name() {
        let machine = ToolCatalog::from_yaml(GITHUB, Source::Machine).unwrap();
        let project = ToolCatalog::from_yaml(
            "mcp:\n  - name: github\n    command: /opt/github-mcp\n",
            Source::Project,
        )
        .unwrap();
        let merged = machine.merge(project);
        assert_eq!(merged.mcp.len(), 2);
        assert_eq!(merged.mcp["github"].command, "/opt/github-mcp");
        assert_eq!(merged.mcp["github"].source, Source::Project);
        assert_eq!(merged.mcp["browser"].source, Source::Machine);
    }

    #[test]
    fn bad_entries_are_reported_with_their_path() {
        let err = ToolCatalog::from_yaml(
            r#"
mcp:
  - name: agentux
    command: x
  - name: Bad Name
    command: " "
  - name: ok
    command: x
    env:
      "1BAD": a
      GOOD: { secret: "not a name" }
    harnesses: []
  - name: ok
    command: y
"#,
            Source::User,
        )
        .unwrap_err();
        let ConfigError::Invalid(issues) = err else {
            panic!("{err}")
        };
        let paths: Vec<_> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "mcp[0].name",
                "mcp[1].name",
                "mcp[1].command",
                "mcp[2].env.1BAD",
                "mcp[2].env.GOOD.secret",
                "mcp[2].harnesses",
                "mcp[3].name",
            ]
        );
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(ToolCatalog::from_yaml("mcp: []\nplugins: []\n", Source::User).is_err());
        assert!(
            ToolCatalog::from_yaml(
                "mcp:\n  - name: a\n    command: b\n    port: 1\n",
                Source::User
            )
            .is_err()
        );
    }

    #[test]
    fn http_servers_have_a_url_and_headers_instead_of_a_command() {
        let catalog = ToolCatalog::from_yaml(
            r#"
mcp:
  - name: linear
    url: https://mcp.linear.app/mcp
    headers:
      Authorization: { secret: LINEAR_TOKEN }
      X-Team: core
  - name: local
    url: http://127.0.0.1:3845/mcp
"#,
            Source::User,
        )
        .unwrap();
        let linear = &catalog.mcp["linear"];
        assert_eq!(linear.url.as_deref(), Some("https://mcp.linear.app/mcp"));
        assert_eq!(linear.secrets().collect::<Vec<_>>(), ["LINEAR_TOKEN"]);
        assert_eq!(
            ToolCatalog::from_yaml(&catalog.to_yaml(), Source::User).unwrap(),
            catalog
        );

        let err = ToolCatalog::from_yaml(
            r#"
mcp:
  - name: plain
    url: http://example.com/mcp
  - name: both
    url: https://a.example/mcp
    command: x
  - name: headers-without-url
    command: x
    headers: { A: b }
  - name: bad-header
    url: https://a.example/mcp
    headers: { "bad header": x }
  - name: creds
    url: https://user:pw@a.example/mcp
  - name: neither
    args: [x]
"#,
            Source::User,
        )
        .unwrap_err();
        let ConfigError::Invalid(issues) = err else {
            panic!("{err}")
        };
        let paths: Vec<_> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "mcp[0].url",
                "mcp[1].url",
                "mcp[2].headers",
                "mcp[3].headers.bad header",
                "mcp[4].url",
                "mcp[5].command",
            ]
        );
    }

    #[test]
    fn a_written_catalog_reads_back_the_same() {
        let catalog = ToolCatalog::from_yaml(GITHUB, Source::User).unwrap();
        let written = catalog.to_yaml();
        assert_eq!(
            ToolCatalog::from_yaml(&written, Source::User).unwrap(),
            catalog
        );
        let odd = ToolCatalog::from_yaml(
            "mcp:\n  - name: odd\n    command: \"a: b # c\"\n    args: [\"it's\", \"\\\"q\\\"\"]\n",
            Source::User,
        )
        .unwrap();
        assert_eq!(odd.mcp["odd"].args, ["it's", "\"q\""]);
        assert_eq!(
            ToolCatalog::from_yaml(&odd.to_yaml(), Source::User).unwrap(),
            odd
        );
        assert!(
            ToolCatalog::from_yaml(&ToolCatalog::default().to_yaml(), Source::User)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = ToolCatalog::from_file(&dir.path().join(FILE_NAME), Source::Machine).unwrap();
        assert!(catalog.is_empty());
    }
}
