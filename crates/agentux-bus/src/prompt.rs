//! The short system prompt each harness session starts with (ADR 0004,
//! consequences): its role, its run, the bus tools it has and how to behave.

use agentux_config::BusTool;

use crate::tools::descriptions;
use crate::types::SessionIdentity;

const TEMPLATE: &str = include_str!("session_prompt.md");

/// One line per tool, for the prompt.
fn summary(tool: BusTool) -> &'static str {
    match tool {
        BusTool::PostMessage => "send a message to `role:<name>`, `session:<id>`, `run` or `human`",
        BusTool::ReadMessages => "read your mailbox",
        BusTool::RequestReview => "ask the reviewer role to review your changes",
        BusTool::Handoff => "pass the task to another role with a summary and pointers",
        BusTool::GetRunState => "see the run's step, branch, checks, sessions and open questions",
        BusTool::AskHuman => "escalate a decision you cannot take to the human",
    }
}

/// The full MCP description of a tool.
pub fn description(tool: BusTool) -> &'static str {
    match tool {
        BusTool::PostMessage => descriptions::POST_MESSAGE,
        BusTool::ReadMessages => descriptions::READ_MESSAGES,
        BusTool::RequestReview => descriptions::REQUEST_REVIEW,
        BusTool::Handoff => descriptions::HANDOFF,
        BusTool::GetRunState => descriptions::GET_RUN_STATE,
        BusTool::AskHuman => descriptions::ASK_HUMAN,
    }
}

/// The session prompt for `identity`, listing only the tools it may call.
pub fn session_prompt(identity: &SessionIdentity, tools: &[BusTool], max_turns: u32) -> String {
    let tools = if tools.is_empty() {
        "(no bus tools are enabled for this project)".to_string()
    } else {
        tools
            .iter()
            .map(|tool| format!("- {}: {}", tool.as_str(), summary(*tool)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    TEMPLATE
        .replace("{role}", &identity.role)
        .replace("{run}", &identity.run.0)
        .replace("{project}", &identity.project)
        .replace("{vendor}", &identity.vendor)
        .replace("{max_turns}", &max_turns.to_string())
        .replace("{tools}", &tools)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_names_the_session_and_only_allowed_tools() {
        let identity = SessionIdentity {
            run: "42".into(),
            project: "shop".into(),
            role: "implementer".into(),
            vendor: "claude-code".into(),
            session: "s1".into(),
        };
        let prompt = session_prompt(&identity, &[BusTool::PostMessage, BusTool::ReadMessages], 6);
        assert!(prompt.starts_with(
            "You are the implementer of AgentUX run 42 in project shop, running on claude-code."
        ));
        assert!(prompt.contains("- post_message: "));
        assert!(prompt.contains("- read_messages: "));
        assert!(!prompt.contains("ask_human:"));
        assert!(prompt.contains("after 6 turns"));
        assert!(!prompt.contains('{'), "unfilled placeholder in:\n{prompt}");
    }
}
