# agentux-core

The orchestration engine of [AgentUX](https://github.com/agentux-os/agentux): the daemon, CLI and integrations that let coding agents from different vendors work as one team.

> **Status:** not started. Design decisions live in [agentux/docs/adr](https://github.com/agentux-os/agentux/tree/main/docs/adr).

## Components

| Component | Responsibility |
|---|---|
| `agentuxd` | Daemon. Owns runs, git worktrees, harness sessions and workflow state (SQLite). Serves the API used by the cockpit and `aux`. |
| `aux` | CLI twin of the cockpit: `aux run`, `aux ps`, `aux attach`, `aux approve`. Works over SSH. |
| Harness adapters | Drive Claude Code, Codex, OpenCode and Antigravity CLI through the Agent Client Protocol, with each CLI's headless mode as fallback. |
| Workflow engine | Executes pipelines declared in each project's `agentux.yaml`: plan → implement → test → review → PR, with approval gates and bounded retries. |
| Agent bus | MCP server exposed to every harness so agents can message each other, request cross-vendor reviews, hand off work and escalate to the human. |

## Relevant ADRs

- [0002 — Harness integration via ACP](https://github.com/agentux-os/agentux/blob/main/docs/adr/0002-harness-integration-via-acp.md)
- [0003 — Workflow engine](https://github.com/agentux-os/agentux/blob/main/docs/adr/0003-workflow-engine.md)
- [0004 — Unified interface and agent bus](https://github.com/agentux-os/agentux/blob/main/docs/adr/0004-unified-interface-and-agent-bus.md)

## License

[Apache 2.0](LICENSE)
