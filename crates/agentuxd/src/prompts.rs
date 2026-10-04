//! What AgentUX tells agents at each step, and how it reads a reviewer's
//! verdict. Every prompt is self-contained (task, plan, feedback), so a step
//! works the same in a fresh session after a restart as in a reused one.
//!
//! Commit policy: agents edit files but do not commit; the daemon commits
//! whatever changed after each implement or custom step (see
//! `engine::commit_changes`).

use agentux_api::StepKind;
use serde_json::Value;

use crate::executor::AgentTask;

/// The prompt for one agent step.
pub fn step_prompt(task: &AgentTask) -> String {
    let mut out = format!(
        "You are the {role} in an AgentUX run (run {run}). You work in the git worktree {cwd}{branch}.\n\n## Task\n\n",
        role = task.role,
        run = task.run_id,
        cwd = task.worktree.display(),
        branch = task
            .branch
            .as_deref()
            .map(|b| format!(", on branch {b}"))
            .unwrap_or_default(),
    );
    if let Some(prompt) = &task.prompt {
        out.push_str(prompt.trim());
        out.push('\n');
    }
    if let Some(issue) = task.issue {
        out.push_str(&format!(
            "\nThis run is about issue #{issue} of this repository; read it (for example with `gh issue view {issue}`) if you can.\n"
        ));
    }

    match task.step {
        StepKind::Plan => out.push_str(
            "\n## Your step: plan\n\n\
             Write an implementation plan for the task. Read the code as needed, but do not modify any files.\n\
             Reply with the plan only, in Markdown: the changes to make (files, functions), the tests to add or update, and risks or open questions. \
             Your reply is stored as the plan and handed to the implementer.\n",
        ),
        StepKind::Implement => {
            out.push_str("\n## Your step: implement\n\n");
            out.push_str(if task.plan.is_some() {
                "Implement the task following the plan below. "
            } else {
                "Implement the task. "
            });
            out.push_str(
                "Edit files in this worktree. Do not commit, push or open a pull request: \
                 AgentUX commits your changes when you finish, with a message derived from this run.\n\
                 Run the project's tests and linters if you can, and fix what they report.\n\
                 When you are done, reply with a short summary of what you changed.\n",
            );
        }
        StepKind::Review => {
            let base = task.base_commit.as_deref().unwrap_or("the branch's starting point");
            out.push_str(&format!(
                "\n## Your step: review\n\n\
                 Review the changes made on this branch: see `git log {base}..HEAD` and `git diff {base}...HEAD`. Do not modify any files.\n\
                 Check that they do what the task asks{plan}, are correct, are tested, and fit the code base. \
                 Request changes only for problems that must be fixed before merging. \
                 AgentUX commits the implementer's edits after each step, so ask for changes to files, not for git operations.\n\n\
                 End your reply with exactly one fenced JSON block holding your verdict, either\n\n\
                 ```json\n{{\"verdict\": \"APPROVE\", \"comments\": \"one-paragraph summary of the change\"}}\n```\n\n\
                 or\n\n\
                 ```json\n{{\"verdict\": \"CHANGES_REQUESTED\", \"comments\": [\"each change that must be made, concretely\"]}}\n```\n",
                plan = if task.plan.is_some() { " and follow the plan below" } else { "" },
            ));
        }
        StepKind::Custom => {
            out.push_str("\n## Your step\n\n");
            out.push_str(task.instructions.as_deref().unwrap_or_default().trim());
            out.push_str(
                "\n\nIf you edit files, do not commit: AgentUX commits your changes when you finish.\n\
                 Reply with a short summary of what you did.\n",
            );
        }
        StepKind::Gate | StepKind::PullRequest => {}
    }

    if task.step != StepKind::Plan
        && let Some(plan) = &task.plan
    {
        out.push_str("\n## Plan\n\n");
        out.push_str(plan.trim());
        out.push('\n');
    }
    if let Some(feedback) = &task.feedback {
        out.push_str("\n## Feedback to address\n\n");
        out.push_str(match task.feedback_from {
            Some(StepKind::Gate) => {
                "The project's checks failed after the previous attempt. Fix the cause:\n\n"
            }
            Some(StepKind::Review) => "The reviewer requested these changes:\n\n",
            _ => "",
        });
        out.push_str(feedback.trim());
        out.push('\n');
    }
    out
}

/// Sent to a reviewer whose reply had no recognizable verdict.
pub const VERDICT_REMINDER: &str = "Your reply did not contain a verdict. Reply with only the fenced JSON block: \
     {\"verdict\": \"APPROVE\" or \"CHANGES_REQUESTED\", \"comments\": ...}.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// With the reviewer's summary.
    Approve(String),
    /// With what must change.
    ChangesRequested(String),
}

/// Reads a reviewer's verdict: the last JSON object with a `verdict` field
/// (fenced or not), else the last `APPROVE` / `CHANGES_REQUESTED` keyword
/// (upper case, as asked). `None` when there is neither.
pub fn parse_verdict(reply: &str) -> Option<Verdict> {
    let from_json = reply.match_indices('{').rev().find_map(|(i, _)| {
        let value = serde_json::Deserializer::from_str(&reply[i..])
            .into_iter::<Value>()
            .next()?
            .ok()?;
        let verdict = verdict_word(value.get("verdict")?.as_str()?)?;
        let comments = value
            .get("comments")
            .or_else(|| value.get("summary"))
            .map(comments_text)
            .unwrap_or_default();
        Some((verdict, comments))
    });
    if let Some((approve, comments)) = from_json {
        let comments = if comments.trim().is_empty() {
            without_json(reply)
        } else {
            comments
        };
        return Some(make(approve, comments));
    }

    // Keyword fallback: whichever verdict word comes last.
    let last = |words: &[&str]| words.iter().filter_map(|w| reply.rfind(w)).max();
    let changes = last(&["CHANGES_REQUESTED", "CHANGES REQUESTED", "REQUEST_CHANGES"]);
    let approve = last(&["APPROVE"]);
    let approve = match (approve, changes) {
        (Some(a), Some(c)) => a > c,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        (None, None) => return None,
    };
    Some(make(approve, reply.trim().to_string()))
}

fn make(approve: bool, comments: String) -> Verdict {
    if approve {
        Verdict::Approve(comments)
    } else {
        Verdict::ChangesRequested(comments)
    }
}

/// `true` for an approval, `false` for requested changes.
fn verdict_word(word: &str) -> Option<bool> {
    let word = word.trim().to_ascii_uppercase().replace([' ', '-'], "_");
    match word.as_str() {
        "APPROVE" | "APPROVED" | "LGTM" => Some(true),
        "CHANGES_REQUESTED" | "REQUEST_CHANGES" | "REQUESTED_CHANGES" | "CHANGES" => Some(false),
        _ => None,
    }
}

fn comments_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.trim().to_string(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => format!("- {}", text.trim()),
                other => format!("- {other}"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// The reply without its fenced blocks, for a verdict without comments.
fn without_json(reply: &str) -> String {
    let mut out = String::new();
    let mut fenced = false;
    for line in reply.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if !fenced {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn task(step: StepKind) -> AgentTask {
        AgentTask {
            run_id: "3f9a0c12".into(),
            title: "Add a health endpoint".into(),
            step_index: 0,
            step,
            role: "implementer".into(),
            harness: "opencode".into(),
            model: None,
            prompt: Some("Add a health endpoint".into()),
            issue: Some(7),
            instructions: None,
            plan: Some("1. add /health".into()),
            feedback: None,
            feedback_from: None,
            worktree: PathBuf::from("/src/app.worktrees/3f9a0c12"),
            branch: Some("aux/3f9a0c12".into()),
            base_commit: Some("abc123".into()),
            attempt: 1,
        }
    }

    #[test]
    fn implement_prompt_carries_plan_feedback_and_commit_policy() {
        let mut t = task(StepKind::Implement);
        t.feedback = Some("rename the helper".into());
        t.feedback_from = Some(StepKind::Review);
        let prompt = step_prompt(&t);
        for needle in [
            "You are the implementer",
            "/src/app.worktrees/3f9a0c12",
            "aux/3f9a0c12",
            "Add a health endpoint",
            "issue #7",
            "Do not commit",
            "## Plan\n\n1. add /health",
            "The reviewer requested these changes:\n\nrename the helper",
        ] {
            assert!(
                prompt.contains(needle),
                "{needle:?} missing from:\n{prompt}"
            );
        }
    }

    #[test]
    fn plan_and_review_prompts() {
        let plan = step_prompt(&task(StepKind::Plan));
        assert!(plan.contains("do not modify any files"));
        assert!(!plan.contains("## Plan"));
        let review = step_prompt(&task(StepKind::Review));
        assert!(review.contains("git diff abc123...HEAD"));
        assert!(review.contains("\"verdict\": \"CHANGES_REQUESTED\""));
        let mut custom = task(StepKind::Custom);
        custom.instructions = Some("Update the changelog".into());
        assert!(step_prompt(&custom).contains("## Your step\n\nUpdate the changelog"));
    }

    #[test]
    fn verdicts_from_json_blocks() {
        let reply = "Looks fine overall.\n\n```json\n{\"verdict\": \"APPROVE\", \"comments\": \"Adds /health.\"}\n```\n";
        assert_eq!(
            parse_verdict(reply),
            Some(Verdict::Approve("Adds /health.".into()))
        );
        let reply = "```json\n{\"verdict\": \"changes requested\", \"comments\": [\"add a test\", \"rename x\"]}\n```";
        assert_eq!(
            parse_verdict(reply),
            Some(Verdict::ChangesRequested("- add a test\n- rename x".into()))
        );
        // The last block wins; an example in prose does not.
        let reply = "Format: {\"verdict\": \"APPROVE\"}\nActually:\n{\"verdict\": \"CHANGES_REQUESTED\", \"comments\": \"no\"}";
        assert_eq!(
            parse_verdict(reply),
            Some(Verdict::ChangesRequested("no".into()))
        );
        // Without comments, the prose is the summary.
        let reply = "All good.\n```json\n{\"verdict\": \"APPROVE\"}\n```";
        assert_eq!(
            parse_verdict(reply),
            Some(Verdict::Approve("All good.".into()))
        );
    }

    #[test]
    fn verdicts_from_keywords() {
        assert_eq!(
            parse_verdict("Fine work.\n\nVerdict: APPROVE"),
            Some(Verdict::Approve("Fine work.\n\nVerdict: APPROVE".into()))
        );
        assert!(matches!(
            parse_verdict("I would APPROVE once fixed, but: CHANGES_REQUESTED - add tests"),
            Some(Verdict::ChangesRequested(_))
        ));
        assert_eq!(parse_verdict("looks good, approve"), None);
        assert_eq!(parse_verdict("{\"verdict\": \"maybe\"}"), None);
    }
}
