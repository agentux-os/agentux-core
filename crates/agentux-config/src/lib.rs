//! Types, parsing and strict validation of `agentux.yaml`, the per-project
//! pipeline file described in ADR 0005.
//!
//! Parsing happens in two stages: serde rejects unknown keys and unknown step
//! types (with the line and column of the offending key), then [`Config`] is
//! built from the parsed file while checking the rules serde cannot express:
//! which fields each step type accepts, loops that only point backwards and
//! carry a limit, and references to roles and checks that must exist.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::{fmt, fs, io};

use serde::Deserialize;

mod validate;

/// Name of the pipeline file at the root of a project.
pub const FILE_NAME: &str = "agentux.yaml";

/// The only `version` this release understands.
pub const SUPPORTED_VERSION: u32 = 1;

/// The built-in pipeline used when a project has no `agentux.yaml`.
/// It is the example from ADR 0005, verbatim.
const DEFAULT_YAML: &str = include_str!("default.yaml");

/// A validated `agentux.yaml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub version: u32,
    /// Role name to the harness that plays it.
    pub roles: BTreeMap<String, Role>,
    /// Commands that gate steps run inside the run's worktree.
    pub checks: Vec<Check>,
    pub pipeline: Vec<Step>,
    pub bus: Bus,
    pub budget: Budget,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    pub harness: String,
    /// Passed to the harness if it supports choosing a model.
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub run: String,
}

/// The fixed set of step types. New types are added by ADR, not by users.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    Plan,
    Implement,
    Gate,
    Review,
    PullRequest,
    Custom,
}

impl StepKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Implement => "implement",
            Self::Gate => "gate",
            Self::Review => "review",
            Self::PullRequest => "pull_request",
            Self::Custom => "custom",
        }
    }

    /// Whether the step hands work to an agent. Only such steps can be the
    /// target of a loop.
    pub fn runs_agent(self) -> bool {
        !matches!(self, Self::Gate | Self::PullRequest)
    }
}

impl fmt::Display for StepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One validated pipeline step. `approve: true` means the run waits for the
/// human before moving past the step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Plan {
        role: String,
        approve: bool,
    },
    Implement {
        role: String,
        approve: bool,
    },
    Gate {
        checks: Vec<String>,
        on_fail: Option<Loop>,
    },
    Review {
        role: String,
        approve: bool,
        on_changes_requested: Option<Loop>,
    },
    PullRequest {
        draft: bool,
        approve: bool,
    },
    Custom {
        role: String,
        prompt: String,
        approve: bool,
    },
}

impl Step {
    pub fn kind(&self) -> StepKind {
        match self {
            Self::Plan { .. } => StepKind::Plan,
            Self::Implement { .. } => StepKind::Implement,
            Self::Gate { .. } => StepKind::Gate,
            Self::Review { .. } => StepKind::Review,
            Self::PullRequest { .. } => StepKind::PullRequest,
            Self::Custom { .. } => StepKind::Custom,
        }
    }
}

/// A bounded jump back to an earlier step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loop {
    /// Index into [`Config::pipeline`] of the step to return to.
    pub target: usize,
    /// `max_attempts` for a gate, `max_rounds` for a review.
    pub limit: u32,
}

/// Agent bus limits (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Bus {
    pub max_turns_per_exchange: u32,
    pub allow: Vec<BusTool>,
}

impl Default for Bus {
    fn default() -> Self {
        Self {
            max_turns_per_exchange: 6,
            allow: BusTool::ALL.to_vec(),
        }
    }
}

/// Tools the agent bus exposes to harnesses (ADR 0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusTool {
    PostMessage,
    ReadMessages,
    RequestReview,
    Handoff,
    GetRunState,
    AskHuman,
}

impl BusTool {
    pub const ALL: [BusTool; 6] = [
        Self::PostMessage,
        Self::ReadMessages,
        Self::RequestReview,
        Self::Handoff,
        Self::GetRunState,
        Self::AskHuman,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PostMessage => "post_message",
            Self::ReadMessages => "read_messages",
            Self::RequestReview => "request_review",
            Self::Handoff => "handoff",
            Self::GetRunState => "get_run_state",
            Self::AskHuman => "ask_human",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Pause the run and ask the human when exceeded.
    pub max_usd_per_run: Option<f64>,
}

impl Config {
    /// Parses and validates the contents of an `agentux.yaml`.
    pub fn from_yaml(input: &str) -> Result<Self, ConfigError> {
        let raw: validate::RawConfig = serde_saphyr::from_str(input).map_err(|e| {
            let message = e.to_string();
            // Callers add their own "error:" prefix.
            let message = message.strip_prefix("error: ").unwrap_or(&message);
            ConfigError::Parse(message.to_string())
        })?;
        raw.validate()
    }

    /// Reads, parses and validates the file at `path`.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let input = fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_yaml(&input)
    }

    /// The pipeline used when a project has no `agentux.yaml`.
    pub fn builtin_default() -> Self {
        Self::from_yaml(DEFAULT_YAML).expect("the built-in default pipeline is valid")
    }

    /// Loads `agentux.yaml` from a project root, falling back to the built-in
    /// default pipeline when the file does not exist.
    pub fn load(project_root: &Path) -> Result<Self, ConfigError> {
        let path = project_root.join(FILE_NAME);
        match fs::read_to_string(&path) {
            Ok(input) => Self::from_yaml(&input),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::builtin_default()),
            Err(source) => Err(ConfigError::Read { path, source }),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    /// The file could not be read.
    Read { path: PathBuf, source: io::Error },
    /// The YAML is malformed, has an unknown key or a value of the wrong type.
    /// The message points at the line and column.
    Parse(String),
    /// The file parsed but breaks one or more pipeline rules.
    Invalid(Vec<Issue>),
}

impl ConfigError {
    /// The rule violations, if this is an [`ConfigError::Invalid`] error.
    pub fn issues(&self) -> &[Issue] {
        match self {
            Self::Invalid(issues) => issues,
            _ => &[],
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse(message) => f.write_str(message),
            Self::Invalid(issues) => {
                let plural = if issues.len() == 1 { "" } else { "s" };
                write!(f, "{} problem{plural} found:", issues.len())?;
                for issue in issues {
                    write!(f, "\n  - {issue}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A rule violation, located by its field path, e.g. `pipeline[2].on_fail`
/// (indices start at 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub path: String,
    pub message: String,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}
