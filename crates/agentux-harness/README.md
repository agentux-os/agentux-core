# agentux-harness

Drives coding-agent harnesses through the [Agent Client Protocol](https://agentclientprotocol.com) (ACP), as decided in [ADR 0002](https://github.com/agentux-os/agentux/blob/main/docs/adr/0002-harness-integration-via-acp.md). Built on the official [`agent-client-protocol`](https://crates.io/crates/agent-client-protocol) crate (2.2, protocol v1).

- `Harness` / `HarnessSession`: the vendor-neutral interface. Start a session in a working directory (the run's worktree), send prompts, cancel, shut down. The headless fallbacks from ADR 0002 will implement the same traits.
- `AcpHarness`: spawns the harness from a `HarnessSpec` with the worktree as its working directory, initializes ACP and opens a session there. The harness's stderr is inherited. Shutdown closes its stdin, waits 5 s, then kills it (on Unix, its whole process group, so agents behind `npx` do not linger).
- `Event`: what a session streams: agent message and thought chunks, tool calls and their updates, plans, file diffs carried by tool calls, and context/cost usage. Other ACP updates (echoed user messages, slash commands, modes) are dropped.
- Permission requests go to a caller-supplied async `PermissionHandler` that answers `Allow` or `Deny` for that one tool call. The session keeps streaming while it waits. `cancel` withdraws pending requests.
- `SessionOptions`: MCP servers passed in `session/new` (the `agentux` bus) and a model. ACP has no model field in `session/new`; agents that offer a choice list a session config option of category `model`, and the session selects the model there with `session/set_config_option` (matching the option's value or name). `AcpSession::model_selection` reports `Selected`, or `Unavailable` with the reason when the agent offers no choice or not that model; the agent's default applies then.

`aux exec --harness <id> [--cwd <dir>] "<prompt>"` uses this crate to try a harness by hand.

## Harnesses

How each harness is launched as an ACP agent, as of 2026-10-03. Versions are pinned to the ones in the [ACP registry](https://github.com/agentclientprotocol/registry) so a run's behavior does not change when an adapter is released. Bump them in [`src/spec.rs`](src/spec.rs).

| id | Command | ACP support | Source |
|---|---|---|---|
| `claude-code` | `npx -y @agentclientprotocol/claude-agent-acp@0.85.1` | Adapter (Claude Agent SDK), maintained under the ACP org. It replaces `@zed-industries/claude-code-acp`, which is deprecated. | [registry entry](https://github.com/agentclientprotocol/registry/blob/main/claude-acp/agent.json), [repo](https://github.com/agentclientprotocol/claude-agent-acp) |
| `codex` | `npx -y @agentclientprotocol/codex-acp@2.1.1` | Adapter around Codex, maintained under the ACP org. It replaces `@zed-industries/codex-acp`, which is deprecated. | [registry entry](https://github.com/agentclientprotocol/registry/blob/main/codex-acp/agent.json), [repo](https://github.com/agentclientprotocol/codex-acp) |
| `opencode` | `opencode acp` | Native | [registry entry](https://github.com/agentclientprotocol/registry/blob/main/opencode/agent.json), [docs](https://opencode.ai/docs/acp/) |
| `antigravity` (experimental) | `agy_acp_server.par --uid=` | Separate ACP server binary from Google (`agy-acp-server`, proprietary), downloaded from `dl.google.com`. It is not the `agy` CLI and must be put on `PATH` by hand. | [registry entry](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json) |

### Vendor session ids and the TUI

`AcpSession::id` is the ACP session id. For the three non-experimental harnesses it is also the vendor's own session id, so the vendor's interactive TUI can resume the same conversation (`HarnessSpec::tui_resume`, used by `agentuxd`'s terminal mode). All three advertise `loadSession`, so the session can be reopened over ACP afterwards (`SessionOptions::load`; the history the agent replays while loading is dropped, not streamed as events).

| id | TUI command | Why the ids match |
|---|---|---|
| `claude-code` | `claude --resume <id>` | `claude-agent-acp` 0.85.1 (`src/acp-agent.ts`) creates a random UUID per `session/new` and passes it to the Claude Agent SDK as the session id ("`resume` names the Claude session, which shares the ACP session id") |
| `codex` | `codex resume <id>` | `codex-acp` 2.1.1 (`src/CodexAcpClient.ts`) returns the Codex app-server's `thread.id` as the `sessionId` |
| `opencode` | `opencode --session <id>` | `opencode acp` returns the id of the OpenCode session it creates; with OpenCode 1.18.34 the id from `session/new` is the one `opencode session list` shows |

The command lines were checked against the `--help` of Claude Code 2.1.257, Codex CLI 0.153.2 and OpenCode 1.18.34. Antigravity has no such command.

Antigravity stays experimental: the `agy` CLI's headless mode hangs without a TTY ([google-antigravity/antigravity-cli#318](https://github.com/google-antigravity/antigravity-cli/issues/318)), so there is no fallback, and the ACP server has not been tried with AgentUX. The `--uid=` argument comes from the registry's Linux entry and is undocumented.

Each harness uses the login the user set up for it (ADR 0002); nothing here handles credentials.

## Tests

`tests/acp.rs` runs the scriptable fake ACP agent from [`agentux-fake-agent`](../agentux-fake-agent) (built with the SDK's agent side; `agentuxd`'s tests use it too) in-process over a byte pipe. It covers initialize, session creation in the given directory, a prompt that streams every event kind, allowed and denied permission requests, and cancellation while a permission request is pending. A Unix-only test checks that the subprocess gets the working directory and environment, and that a harness exiting during startup is an error rather than a hang. No vendor CLI or network is needed.
