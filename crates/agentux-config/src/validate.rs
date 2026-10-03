//! The file as serde sees it, and the rules that turn it into a [`Config`].

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use crate::{
    Budget, Bus, Check, Config, ConfigError, Issue, Loop, Role, SUPPORTED_VERSION, Step, StepKind,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConfig {
    version: Option<u32>,
    #[serde(default)]
    roles: BTreeMap<String, Role>,
    #[serde(default)]
    checks: Vec<Check>,
    pipeline: Option<Vec<RawStep>>,
    #[serde(default)]
    bus: Bus,
    #[serde(default)]
    budget: Budget,
}

/// Every field any step type may use; which ones are allowed depends on
/// `step` and is checked in [`RawStep::validate`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    step: StepKind,
    role: Option<String>,
    approve: Option<bool>,
    checks: Option<Vec<String>>,
    on_fail: Option<StepKind>,
    max_attempts: Option<u32>,
    on_changes_requested: Option<StepKind>,
    max_rounds: Option<u32>,
    draft: Option<bool>,
    prompt: Option<String>,
}

fn allowed_fields(kind: StepKind) -> &'static [&'static str] {
    match kind {
        StepKind::Plan | StepKind::Implement => &["role", "approve"],
        StepKind::Gate => &["checks", "on_fail", "max_attempts"],
        StepKind::Review => &["role", "approve", "on_changes_requested", "max_rounds"],
        StepKind::PullRequest => &["draft", "approve"],
        StepKind::Custom => &["role", "prompt", "approve"],
    }
}

#[derive(Default)]
struct Issues(Vec<Issue>);

impl Issues {
    fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.0.push(Issue {
            path: path.into(),
            message: message.into(),
        });
    }
}

impl RawConfig {
    pub(crate) fn validate(self) -> Result<Config, ConfigError> {
        let mut issues = Issues::default();

        match self.version {
            Some(SUPPORTED_VERSION) => {}
            Some(other) => issues.push(
                "version",
                format!("unsupported version {other}; expected {SUPPORTED_VERSION}"),
            ),
            None => issues.push(
                "version",
                format!("is required; add `version: {SUPPORTED_VERSION}` to the file"),
            ),
        }

        for (name, role) in &self.roles {
            if role.harness.trim().is_empty() {
                issues.push(format!("roles.{name}.harness"), "must not be empty");
            }
            if role.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
                issues.push(format!("roles.{name}.model"), "must not be empty when set");
            }
        }

        let mut check_names = HashSet::new();
        for (i, check) in self.checks.iter().enumerate() {
            if check.name.trim().is_empty() {
                issues.push(format!("checks[{i}].name"), "must not be empty");
            } else if !check_names.insert(check.name.as_str()) {
                issues.push(
                    format!("checks[{i}].name"),
                    format!("duplicate check `{}`", check.name),
                );
            }
            if check.run.trim().is_empty() {
                issues.push(format!("checks[{i}].run"), "must not be empty");
            }
        }

        let raw_steps = match self.pipeline {
            Some(steps) if !steps.is_empty() => steps,
            Some(_) => {
                issues.push("pipeline", "must contain at least one step");
                Vec::new()
            }
            None => {
                issues.push("pipeline", "is required");
                Vec::new()
            }
        };
        let kinds: Vec<StepKind> = raw_steps.iter().map(|s| s.step).collect();
        let cx = Context {
            roles: &self.roles,
            checks: &self.checks,
            kinds: &kinds,
        };
        let pipeline: Vec<Step> = raw_steps
            .into_iter()
            .enumerate()
            .filter_map(|(i, raw)| raw.validate(i, &cx, &mut issues))
            .collect();

        if self.bus.max_turns_per_exchange == 0 {
            issues.push("bus.max_turns_per_exchange", "must be at least 1");
        }
        let mut tools = HashSet::new();
        for (i, tool) in self.bus.allow.iter().enumerate() {
            if !tools.insert(*tool) {
                issues.push(
                    format!("bus.allow[{i}]"),
                    format!("`{}` is listed twice", tool.as_str()),
                );
            }
        }

        if let Some(usd) = self.budget.max_usd_per_run {
            let valid = usd.is_finite() && usd > 0.0;
            if !valid {
                issues.push("budget.max_usd_per_run", "must be a positive number");
            }
        }

        if !issues.0.is_empty() {
            return Err(ConfigError::Invalid(issues.0));
        }
        Ok(Config {
            version: SUPPORTED_VERSION,
            roles: self.roles,
            checks: self.checks,
            pipeline,
            bus: self.bus,
            budget: self.budget,
        })
    }
}

/// What a step may refer to: defined roles and checks, and the kinds of all
/// steps in order (for resolving loop targets).
struct Context<'a> {
    roles: &'a BTreeMap<String, Role>,
    checks: &'a [Check],
    kinds: &'a [StepKind],
}

impl RawStep {
    fn present_fields(&self) -> Vec<&'static str> {
        [
            ("role", self.role.is_some()),
            ("approve", self.approve.is_some()),
            ("checks", self.checks.is_some()),
            ("on_fail", self.on_fail.is_some()),
            ("max_attempts", self.max_attempts.is_some()),
            ("on_changes_requested", self.on_changes_requested.is_some()),
            ("max_rounds", self.max_rounds.is_some()),
            ("draft", self.draft.is_some()),
            ("prompt", self.prompt.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, present)| present.then_some(name))
        .collect()
    }

    /// Checks step `i` and returns it, or `None` after recording issues.
    fn validate(self, i: usize, cx: &Context<'_>, issues: &mut Issues) -> Option<Step> {
        let before = issues.0.len();
        let kind = self.step;
        for field in self.present_fields() {
            if !allowed_fields(kind).contains(&field) {
                issues.push(
                    format!("pipeline[{i}].{field}"),
                    format!("is not allowed in a `{kind}` step"),
                );
            }
        }

        let approve = self.approve.unwrap_or(false);
        let step = match kind {
            StepKind::Plan => Step::Plan {
                role: cx.role(i, self.role, issues),
                approve,
            },
            StepKind::Implement => Step::Implement {
                role: cx.role(i, self.role, issues),
                approve,
            },
            StepKind::Gate => Step::Gate {
                checks: cx.gate_checks(i, self.checks, issues),
                on_fail: cx.loop_back(
                    i,
                    (self.on_fail, "on_fail"),
                    (self.max_attempts, "max_attempts"),
                    issues,
                ),
            },
            StepKind::Review => Step::Review {
                role: cx.role(i, self.role, issues),
                approve,
                on_changes_requested: cx.loop_back(
                    i,
                    (self.on_changes_requested, "on_changes_requested"),
                    (self.max_rounds, "max_rounds"),
                    issues,
                ),
            },
            StepKind::PullRequest => Step::PullRequest {
                draft: self.draft.unwrap_or(false),
                approve,
            },
            StepKind::Custom => Step::Custom {
                role: cx.role(i, self.role, issues),
                prompt: match self.prompt {
                    Some(prompt) if !prompt.trim().is_empty() => prompt,
                    _ => {
                        issues.push(format!("pipeline[{i}].prompt"), "is required");
                        String::new()
                    }
                },
                approve,
            },
        };
        (issues.0.len() == before).then_some(step)
    }
}

impl Context<'_> {
    fn role(&self, i: usize, role: Option<String>, issues: &mut Issues) -> String {
        let path = format!("pipeline[{i}].role");
        let Some(role) = role else {
            issues.push(path, "is required");
            return String::new();
        };
        if !self.roles.contains_key(&role) {
            issues.push(
                path,
                format!(
                    "role `{role}` is not defined under `roles`{}",
                    defined(self.roles.keys().map(String::as_str))
                ),
            );
        }
        role
    }

    fn gate_checks(
        &self,
        i: usize,
        checks: Option<Vec<String>>,
        issues: &mut Issues,
    ) -> Vec<String> {
        let path = format!("pipeline[{i}].checks");
        let checks = checks.unwrap_or_default();
        if checks.is_empty() {
            issues.push(path, "must list at least one check");
            return checks;
        }
        let mut seen = HashSet::new();
        for (j, name) in checks.iter().enumerate() {
            if !self.checks.iter().any(|check| check.name == *name) {
                issues.push(
                    format!("{path}[{j}]"),
                    format!(
                        "check `{name}` is not defined under `checks`{}",
                        defined(self.checks.iter().map(|check| check.name.as_str()))
                    ),
                );
            } else if !seen.insert(name.as_str()) {
                issues.push(
                    format!("{path}[{j}]"),
                    format!("check `{name}` is listed twice"),
                );
            }
        }
        checks
    }

    /// Resolves the loop on step `i` (`on_fail` with `max_attempts`, or
    /// `on_changes_requested` with `max_rounds`). Loops must point to a single
    /// earlier step that runs an agent, and must carry a limit.
    fn loop_back(
        &self,
        i: usize,
        (target, target_field): (Option<StepKind>, &str),
        (limit, limit_field): (Option<u32>, &str),
        issues: &mut Issues,
    ) -> Option<Loop> {
        let target_path = format!("pipeline[{i}].{target_field}");
        let limit_path = format!("pipeline[{i}].{limit_field}");
        let (target, limit) = match (target, limit) {
            (None, None) => return None,
            (None, Some(_)) => {
                issues.push(
                    limit_path,
                    format!("has no effect without `{target_field}`"),
                );
                return None;
            }
            (Some(_), None) => {
                issues.push(
                    target_path,
                    format!("requires `{limit_field}` so the loop is bounded"),
                );
                return None;
            }
            (Some(target), Some(limit)) => (target, limit),
        };
        if limit == 0 {
            issues.push(limit_path, "must be at least 1");
        }
        if !target.runs_agent() {
            issues.push(
                target_path,
                format!("cannot loop back to a `{target}` step; the target must run an agent"),
            );
            return None;
        }

        let earlier: Vec<usize> = self.kinds[..i]
            .iter()
            .enumerate()
            .filter(|&(_, &kind)| kind == target)
            .map(|(j, _)| j)
            .collect();
        match earlier[..] {
            [j] => Some(Loop { target: j, limit }),
            [] => {
                issues.push(
                    target_path,
                    format!(
                        "must point to an earlier step (loops only go backwards); \
                         no `{target}` step comes before pipeline[{i}]"
                    ),
                );
                None
            }
            _ => {
                let steps: Vec<String> = earlier.iter().map(|j| format!("pipeline[{j}]")).collect();
                issues.push(
                    target_path,
                    format!(
                        "is ambiguous: `{target}` matches several earlier steps ({})",
                        steps.join(", ")
                    ),
                );
                None
            }
        }
    }
}

/// " (defined: a, b)" or " (none are defined)", appended to "not defined" errors.
fn defined<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let names: Vec<&str> = names.collect();
    if names.is_empty() {
        " (none are defined)".to_string()
    } else {
        format!(" (defined: {})", names.join(", "))
    }
}
