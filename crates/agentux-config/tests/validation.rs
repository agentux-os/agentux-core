use std::collections::BTreeMap;
use std::fs;

use agentux_config::{
    Budget, Bus, BusTool, Check, Config, ConfigError, FILE_NAME, Loop, Role, Step, StepKind,
};

/// The full example from ADR 0005, which is also the built-in default.
const ADR_EXAMPLE: &str = include_str!("../src/default.yaml");

/// Roles and checks shared by the smaller test files; append a `pipeline:`.
const PREAMBLE: &str = "
version: 1
roles:
  implementer:
    harness: claude-code
  reviewer:
    harness: codex
checks:
  - name: test
    run: just test
";

fn with_pipeline(pipeline: &str) -> String {
    format!("{PREAMBLE}pipeline:\n{pipeline}")
}

fn issues(yaml: &str) -> Vec<String> {
    match Config::from_yaml(yaml) {
        Err(ConfigError::Invalid(issues)) => issues.iter().map(ToString::to_string).collect(),
        other => panic!("expected validation issues, got {other:?}"),
    }
}

/// Asserts that validation reports `path` with a message containing `needle`.
fn assert_issue(yaml: &str, path: &str, needle: &str) {
    let found = issues(yaml);
    let prefix = format!("{path}: ");
    assert!(
        found
            .iter()
            .any(|i| i.starts_with(&prefix) && i.contains(needle)),
        "expected an issue at `{path}` mentioning `{needle}`, got {found:#?}"
    );
}

fn parse_error(yaml: &str) -> String {
    match Config::from_yaml(yaml) {
        Err(ConfigError::Parse(message)) => message,
        other => panic!("expected a parse error, got {other:?}"),
    }
}

#[test]
fn adr_example_parses_into_the_expected_config() {
    let config = Config::from_yaml(ADR_EXAMPLE).expect("ADR 0005 example is valid");

    let roles = BTreeMap::from([
        (
            "implementer".to_string(),
            Role {
                harness: "claude-code".into(),
                model: None,
            },
        ),
        (
            "reviewer".to_string(),
            Role {
                harness: "codex".into(),
                model: None,
            },
        ),
        (
            "planner".to_string(),
            Role {
                harness: "claude-code".into(),
                model: Some("opus".into()),
            },
        ),
    ]);
    let expected = Config {
        version: 1,
        roles,
        checks: vec![
            Check {
                name: "lint".into(),
                run: "just lint".into(),
            },
            Check {
                name: "test".into(),
                run: "just test".into(),
            },
        ],
        pipeline: vec![
            Step::Plan {
                role: "planner".into(),
                approve: true,
            },
            Step::Implement {
                role: "implementer".into(),
                approve: false,
            },
            Step::Gate {
                checks: vec!["lint".into(), "test".into()],
                on_fail: Some(Loop {
                    target: 1,
                    limit: 3,
                }),
            },
            Step::Review {
                role: "reviewer".into(),
                approve: false,
                on_changes_requested: Some(Loop {
                    target: 1,
                    limit: 2,
                }),
            },
            Step::PullRequest {
                draft: false,
                approve: false,
            },
        ],
        bus: Bus {
            max_turns_per_exchange: 6,
            allow: vec![
                BusTool::PostMessage,
                BusTool::RequestReview,
                BusTool::Handoff,
                BusTool::GetRunState,
                BusTool::AskHuman,
            ],
        },
        budget: Budget {
            max_usd_per_run: Some(10.0),
        },
    };
    assert_eq!(config, expected);
}

#[test]
fn builtin_default_is_the_adr_example() {
    let default = Config::builtin_default();
    assert_eq!(default, Config::from_yaml(ADR_EXAMPLE).unwrap());
    let kinds: Vec<StepKind> = default.pipeline.iter().map(Step::kind).collect();
    assert_eq!(
        kinds,
        [
            StepKind::Plan,
            StepKind::Implement,
            StepKind::Gate,
            StepKind::Review,
            StepKind::PullRequest
        ]
    );
}

#[test]
fn load_uses_the_default_without_a_file_and_the_file_when_present() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(Config::load(dir.path()).unwrap(), Config::builtin_default());

    let yaml = with_pipeline("  - step: implement\n    role: implementer\n");
    fs::write(dir.path().join(FILE_NAME), &yaml).unwrap();
    let config = Config::load(dir.path()).unwrap();
    assert_eq!(config.pipeline.len(), 1);

    fs::write(dir.path().join(FILE_NAME), "version: 2\npipeline: []\n").unwrap();
    assert!(matches!(
        Config::load(dir.path()),
        Err(ConfigError::Invalid(_))
    ));
}

#[test]
fn from_file_reports_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let err = Config::from_file(&dir.path().join("nope.yaml")).unwrap_err();
    assert!(matches!(err, ConfigError::Read { .. }));
    assert!(err.to_string().contains("nope.yaml"), "{err}");
}

#[test]
fn minimal_file_gets_defaults() {
    let config = Config::from_yaml(&with_pipeline(
        "  - step: implement\n    role: implementer\n",
    ))
    .unwrap();
    assert_eq!(config.bus, Bus::default());
    assert_eq!(config.bus.allow, BusTool::ALL);
    assert_eq!(config.budget.max_usd_per_run, None);
    assert_eq!(
        config.pipeline,
        [Step::Implement {
            role: "implementer".into(),
            approve: false
        }]
    );
}

#[test]
fn custom_step_carries_its_prompt() {
    let config = Config::from_yaml(&with_pipeline(
        "  - step: custom\n    role: reviewer\n    prompt: Update the changelog\n    approve: true\n",
    ))
    .unwrap();
    assert_eq!(
        config.pipeline,
        [Step::Custom {
            role: "reviewer".into(),
            prompt: "Update the changelog".into(),
            approve: true
        }]
    );
}

#[test]
fn custom_step_requires_a_prompt() {
    assert_issue(
        &with_pipeline("  - step: custom\n    role: reviewer\n"),
        "pipeline[0].prompt",
        "is required",
    );
}

#[test]
fn version_is_required() {
    assert_issue(
        "pipeline:\n  - step: pull_request\n",
        "version",
        "is required",
    );
}

#[test]
fn other_versions_are_rejected() {
    assert_issue(
        "version: 2\npipeline:\n  - step: pull_request\n",
        "version",
        "unsupported version 2",
    );
}

#[test]
fn pipeline_is_required_and_not_empty() {
    assert_issue("version: 1\n", "pipeline", "is required");
    assert_issue(
        "version: 1\npipeline: []\n",
        "pipeline",
        "at least one step",
    );
}

#[test]
fn unknown_top_level_key_fails() {
    let message = parse_error(&format!("{ADR_EXAMPLE}\npipelines: []\n"));
    assert!(message.contains("unknown field `pipelines`"), "{message}");
}

#[test]
fn unknown_key_in_a_step_fails() {
    let message = parse_error(&with_pipeline(
        "  - step: implement\n    role: implementer\n    retries: 3\n",
    ));
    assert!(message.contains("unknown field `retries`"), "{message}");
}

#[test]
fn unknown_key_in_a_role_fails() {
    let message = parse_error(
        "version: 1\nroles:\n  implementer:\n    harness: codex\n    api_key: secret\n\
         pipeline:\n  - step: implement\n    role: implementer\n",
    );
    assert!(message.contains("unknown field `api_key`"), "{message}");
}

#[test]
fn unknown_key_in_bus_and_budget_fails() {
    let message = parse_error(&format!(
        "{}bus:\n  max_turns: 3\n",
        with_pipeline("  - step: pull_request\n")
    ));
    assert!(message.contains("unknown field `max_turns`"), "{message}");

    let message = parse_error(&format!(
        "{}budget:\n  max_usd: 3\n",
        with_pipeline("  - step: pull_request\n")
    ));
    assert!(message.contains("unknown field `max_usd`"), "{message}");
}

#[test]
fn unknown_step_type_fails() {
    let message = parse_error(&with_pipeline("  - step: deploy\n"));
    assert!(message.contains("unknown variant `deploy`"), "{message}");
}

#[test]
fn unknown_bus_tool_fails() {
    let message = parse_error(&format!(
        "{}bus:\n  allow: [post_message, run_shell]\n",
        with_pipeline("  - step: pull_request\n")
    ));
    assert!(message.contains("unknown variant `run_shell`"), "{message}");
}

#[test]
fn duplicate_keys_fail() {
    parse_error("version: 1\nversion: 1\npipeline:\n  - step: pull_request\n");
}

#[test]
fn malformed_yaml_fails() {
    parse_error("version: 1\npipeline: [\n");
}

#[test]
fn field_from_another_step_type_is_rejected() {
    assert_issue(
        &with_pipeline(
            "  - step: implement\n    role: implementer\n    checks: [test]\n\
             \x20 - step: gate\n    checks: [test]\n    role: reviewer\n",
        ),
        "pipeline[0].checks",
        "not allowed in a `implement` step",
    );
    assert_issue(
        &with_pipeline("  - step: gate\n    checks: [test]\n    approve: true\n"),
        "pipeline[0].approve",
        "not allowed in a `gate` step",
    );
    assert_issue(
        &with_pipeline("  - step: pull_request\n    on_fail: implement\n"),
        "pipeline[0].on_fail",
        "not allowed in a `pull_request` step",
    );
}

#[test]
fn steps_must_reference_defined_roles() {
    assert_issue(
        &with_pipeline("  - step: implement\n    role: ghost\n"),
        "pipeline[0].role",
        "role `ghost` is not defined under `roles` (defined: implementer, reviewer)",
    );
    assert_issue(
        "version: 1\npipeline:\n  - step: plan\n    role: planner\n",
        "pipeline[0].role",
        "(none are defined)",
    );
}

#[test]
fn agent_steps_require_a_role() {
    for kind in ["plan", "implement", "review"] {
        assert_issue(
            &with_pipeline(&format!("  - step: {kind}\n")),
            "pipeline[0].role",
            "is required",
        );
    }
}

#[test]
fn roles_need_a_harness() {
    assert_issue(
        "version: 1\nroles:\n  implementer:\n    harness: ''\n\
         pipeline:\n  - step: implement\n    role: implementer\n",
        "roles.implementer.harness",
        "must not be empty",
    );
    let message = parse_error(
        "version: 1\nroles:\n  implementer:\n    model: opus\n\
         pipeline:\n  - step: implement\n    role: implementer\n",
    );
    assert!(message.contains("harness"), "{message}");
}

#[test]
fn gate_must_reference_defined_checks() {
    assert_issue(
        &with_pipeline("  - step: gate\n    checks: [test, lint]\n"),
        "pipeline[0].checks[1]",
        "check `lint` is not defined under `checks` (defined: test)",
    );
}

#[test]
fn gate_needs_at_least_one_check() {
    assert_issue(
        &with_pipeline("  - step: gate\n"),
        "pipeline[0].checks",
        "at least one check",
    );
    assert_issue(
        &with_pipeline("  - step: gate\n    checks: []\n"),
        "pipeline[0].checks",
        "at least one check",
    );
}

#[test]
fn gate_rejects_a_check_listed_twice() {
    assert_issue(
        &with_pipeline("  - step: gate\n    checks: [test, test]\n"),
        "pipeline[0].checks[1]",
        "listed twice",
    );
}

#[test]
fn check_names_are_unique_and_commands_not_empty() {
    let yaml = "version: 1\nchecks:\n  - name: test\n    run: just test\n  - name: test\n    run: ''\n\
                pipeline:\n  - step: gate\n    checks: [test]\n";
    assert_issue(yaml, "checks[1].name", "duplicate check `test`");
    assert_issue(yaml, "checks[1].run", "must not be empty");
}

#[test]
fn loop_must_point_backwards() {
    let forward = with_pipeline(
        "  - step: gate\n    checks: [test]\n    on_fail: implement\n    max_attempts: 3\n\
         \x20 - step: implement\n    role: implementer\n",
    );
    assert_issue(
        &forward,
        "pipeline[0].on_fail",
        "must point to an earlier step",
    );

    let to_itself = with_pipeline(
        "  - step: implement\n    role: implementer\n\
         \x20 - step: review\n    role: reviewer\n    on_changes_requested: review\n    max_rounds: 2\n",
    );
    assert_issue(
        &to_itself,
        "pipeline[1].on_changes_requested",
        "no `review` step comes before pipeline[1]",
    );
}

#[test]
fn loop_needs_a_limit() {
    assert_issue(
        &with_pipeline(
            "  - step: implement\n    role: implementer\n\
             \x20 - step: gate\n    checks: [test]\n    on_fail: implement\n",
        ),
        "pipeline[1].on_fail",
        "requires `max_attempts`",
    );
    assert_issue(
        &with_pipeline(
            "  - step: implement\n    role: implementer\n\
             \x20 - step: review\n    role: reviewer\n    on_changes_requested: implement\n",
        ),
        "pipeline[1].on_changes_requested",
        "requires `max_rounds`",
    );
}

#[test]
fn loop_limit_must_be_positive() {
    assert_issue(
        &with_pipeline(
            "  - step: implement\n    role: implementer\n\
             \x20 - step: gate\n    checks: [test]\n    on_fail: implement\n    max_attempts: 0\n",
        ),
        "pipeline[1].max_attempts",
        "at least 1",
    );
}

#[test]
fn loop_limit_without_loop_is_rejected() {
    assert_issue(
        &with_pipeline("  - step: review\n    role: reviewer\n    max_rounds: 2\n"),
        "pipeline[0].max_rounds",
        "has no effect without `on_changes_requested`",
    );
}

#[test]
fn loop_cannot_target_a_step_without_an_agent() {
    assert_issue(
        &with_pipeline(
            "  - step: gate\n    checks: [test]\n\
             \x20 - step: gate\n    checks: [test]\n    on_fail: gate\n    max_attempts: 2\n",
        ),
        "pipeline[1].on_fail",
        "cannot loop back to a `gate` step",
    );
}

#[test]
fn loop_target_must_be_unambiguous() {
    assert_issue(
        &with_pipeline(
            "  - step: implement\n    role: implementer\n\
             \x20 - step: implement\n    role: reviewer\n\
             \x20 - step: gate\n    checks: [test]\n    on_fail: implement\n    max_attempts: 2\n",
        ),
        "pipeline[2].on_fail",
        "several earlier steps (pipeline[0], pipeline[1])",
    );
}

#[test]
fn bus_and_budget_limits_are_checked() {
    let base = with_pipeline("  - step: pull_request\n");
    assert_issue(
        &format!("{base}bus:\n  max_turns_per_exchange: 0\n"),
        "bus.max_turns_per_exchange",
        "at least 1",
    );
    assert_issue(
        &format!("{base}bus:\n  allow: [handoff, handoff]\n"),
        "bus.allow[1]",
        "`handoff` is listed twice",
    );
    assert_issue(
        &format!("{base}budget:\n  max_usd_per_run: -5\n"),
        "budget.max_usd_per_run",
        "positive",
    );
}

#[test]
fn all_issues_are_reported_at_once() {
    let yaml = "version: 3\npipeline:\n  - step: implement\n    role: ghost\n\
                \x20 - step: gate\n    checks: [lint]\n";
    let found = issues(yaml);
    assert_eq!(found.len(), 3, "{found:#?}");

    let err = Config::from_yaml(yaml).unwrap_err();
    assert_eq!(err.issues().len(), 3);
    let rendered = err.to_string();
    assert!(rendered.starts_with("3 problems found:"), "{rendered}");
    assert!(
        rendered.contains("\n  - version: unsupported version 3"),
        "{rendered}"
    );
}
