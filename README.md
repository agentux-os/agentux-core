# agentux-core

The orchestration engine of [AgentUX](https://github.com/agentux-os/agentux): the daemon, CLI and integrations that let coding agents from different vendors work as one team.

> **Status:** foundation only. `agentux.yaml` parsing and validation, run worktrees and the first `aux` commands exist; nothing drives an agent yet. Design decisions live in [agentux/docs/adr](https://github.com/agentux-os/agentux/tree/main/docs/adr).

## Components

| Component | Responsibility |
|---|---|
| `agentuxd` | Daemon. Owns runs, git worktrees, harness sessions and workflow state (SQLite). Serves the API used by the cockpit and `aux`. |
| `aux` | CLI twin of the cockpit: `aux run`, `aux ps`, `aux attach`, `aux approve`. Works over SSH. |
| Harness adapters | Drive Claude Code, Codex, OpenCode and Antigravity CLI through the Agent Client Protocol, with each CLI's headless mode as fallback. |
| Workflow engine | Executes pipelines declared in each project's `agentux.yaml`: plan → implement → test → review → PR, with approval gates and bounded retries. |
| Agent bus | MCP server exposed to every harness so agents can message each other, request cross-vendor reviews, hand off work and escalate to the human. |

## Crates

A Cargo workspace. What exists today:

| Crate | What it does |
|---|---|
| [`agentux-config`](crates/agentux-config) | Types, parsing and strict validation of `agentux.yaml` ([ADR 0005](https://github.com/agentux-os/agentux/blob/main/docs/adr/0005-agentux-yaml.md)). Unknown keys fail with line and column; rule violations are reported together, each with its field path (`pipeline[2].on_fail`). Without a file, the built-in default pipeline (the ADR's example) applies. |
| [`agentux-worktree`](crates/agentux-worktree) | Creates, lists and removes the git worktree of a run: branch `aux/<run-id>`, checked out under a configurable base directory. Shells out to `git`. |
| [`aux-cli`](crates/aux-cli) | The `aux` binary. |

`aux` commands available now:

```sh
aux validate [path]                 # check agentux.yaml (file or project dir) without starting a run
aux worktree create <run-id> [--from <ref>] [--base-dir <dir>] [--repo <dir>]
aux worktree list [--repo <dir>]
aux worktree remove <run-id> [--force] [--delete-branch] [--repo <dir>]
```

`aux worktree` is a development aid until `agentuxd` manages worktrees itself. `aux run` exists but only reports that it is not implemented yet.

## Build and test

Needs `git` and a Rust toolchain; [`rust-toolchain.toml`](rust-toolchain.toml) pins the version, and `rustup` picks it up.

```sh
cargo build                          # binary at target/debug/aux
cargo test                           # worktree tests create throwaway repos in a temp dir
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
```

CI runs the last three on every pull request.

Linux is the target. The crate holding `aux` is named `aux-cli` because `aux` is a reserved file name on Windows.

## Next steps

- `agentuxd`: run state machine persisted in SQLite ([ADR 0003](https://github.com/agentux-os/agentux/blob/main/docs/adr/0003-workflow-engine.md)), driving the pipeline from `agentux-config` inside worktrees from `agentux-worktree`.
- Harness adapters over ACP, with headless fallbacks ([ADR 0002](https://github.com/agentux-os/agentux/blob/main/docs/adr/0002-harness-integration-via-acp.md)).
- Agent bus as an MCP server ([ADR 0004](https://github.com/agentux-os/agentux/blob/main/docs/adr/0004-unified-interface-and-agent-bus.md)).
- `aux run`, `aux ps`, `aux attach`, `aux approve` on top of the daemon's API.

## Relevant ADRs

- [0002 — Harness integration via ACP](https://github.com/agentux-os/agentux/blob/main/docs/adr/0002-harness-integration-via-acp.md)
- [0003 — Workflow engine](https://github.com/agentux-os/agentux/blob/main/docs/adr/0003-workflow-engine.md)
- [0004 — Unified interface and agent bus](https://github.com/agentux-os/agentux/blob/main/docs/adr/0004-unified-interface-and-agent-bus.md)
- [0005 — Pipelines in `agentux.yaml`](https://github.com/agentux-os/agentux/blob/main/docs/adr/0005-agentux-yaml.md)

## License

[Apache 2.0](LICENSE)
