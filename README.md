# agentux-core

The orchestration engine of [AgentUX](https://github.com/agentux-os/agentux): the daemon, CLI and integrations that let coding agents from different vendors work as one team.

> **Status:** early. `agentuxd` runs pipelines from `agentux.yaml` as persisted state machines in run worktrees, with gates, bounded loops, approvals and crash recovery, and `aux` drives it over a local socket. Agent steps go through an executor interface whose ACP-backed implementation (on top of `agentux-harness`) is not wired in yet, so today agent steps either fail with an explanation or, with `--fake-agents`, are answered by a scripted fake. Design decisions live in [agentux/docs/adr](https://github.com/agentux-os/agentux/tree/main/docs/adr).

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
| [`agentux-config`](crates/agentux-config) | Types, parsing and strict validation of `agentux.yaml` ([ADR 0005](https://github.com/agentux-os/agentux/blob/main/docs/adr/0005-agentux-yaml.md)). Unknown keys fail with line and column; rule violations are reported together, each with its field path (`pipeline[2].on_fail`). Without a file, the built-in default pipeline (the ADR's example) applies, with its `lint` and `test` checks detected from the project (justfile recipes, Cargo, npm scripts, pytest/ruff via uv, Go); with nothing detected, the gate step is left out. |
| [`agentux-harness`](crates/agentux-harness) | Drives a harness through the Agent Client Protocol ([ADR 0002](https://github.com/agentux-os/agentux/blob/main/docs/adr/0002-harness-integration-via-acp.md)): spawns it in a worktree, opens a session, sends prompts, streams vendor-neutral events, routes permission requests to the caller, cancels. Knows how to launch Claude Code, Codex, OpenCode and (experimental) Antigravity. |
| [`agentux-worktree`](crates/agentux-worktree) | Creates, lists and removes the git worktree of a run: branch `aux/<run-id>`, checked out under a configurable base directory. Shells out to `git`. |
| [`agentux-store`](crates/agentux-store) | SQLite persistence (bundled SQLite): projects, runs, step attempts, approval requests and the event log, with schema migrations. Every change is one transaction; events are broadcast only after commit. |
| [`agentux-api`](crates/agentux-api) | API types, the JSON-RPC envelope and an async client, shared by the daemon, `aux` and the cockpit backend. |
| [`agentuxd`](crates/agentuxd) | The daemon: run state machine ([ADR 0003](https://github.com/agentux-os/agentux/blob/main/docs/adr/0003-workflow-engine.md)) and the API on a Unix socket ([docs/api.md](docs/api.md)). Also a library, so `aux daemon` runs the same code. |
| [`aux-cli`](crates/aux-cli) | The `aux` binary. |

### Running

```sh
aux daemon [--fake-agents]          # agentuxd in the foreground (or: agentuxd, or the systemd unit below)
aux run [project-dir] --prompt "Add a health endpoint" [--issue 42] [--watch]
aux ps [--all]                      # runs, and approvals waiting for you
aux approve <request-id> [-m note]
aux deny <request-id> [-m reason]
aux watch <run-id>                  # history, then live events until the run ends
aux cancel <run-id>
```

The socket is `$XDG_RUNTIME_DIR/agentux/agentuxd.sock` (override with `--socket` or `$AGENTUX_SOCKET`); state lives in `$XDG_STATE_HOME/agentux/agentuxd.db`. [`contrib/agentuxd.service`](contrib/agentuxd.service) is a systemd user unit for the image.

A run gets its own worktree, then walks the pipeline: agent steps call the executor, gates run the configured checks with `sh -c` in the worktree, failures and requested changes loop back within `max_attempts`/`max_rounds`, and `approve: true` pauses the run until `aux approve`. Each transition is committed before its side effect, so a restarted daemon resumes every unfinished run, re-running a step that was interrupted. `budget.max_usd_per_run` is stored but not enforced yet (no usage data without harness adapters). The API, event stream and lifecycle are documented in [docs/api.md](docs/api.md).

Other commands:

```sh
aux validate [path]                 # check agentux.yaml (file or project dir) without starting a run;
                                    # without a file, shows the default pipeline and detected checks
aux exec --harness <id> [--cwd <dir>] "<prompt>"
aux worktree create <run-id> [--from <ref>] [--base-dir <dir>] [--repo <dir>]
aux worktree list [--repo <dir>]
aux worktree remove <run-id> [--force] [--delete-branch] [--repo <dir>]
```

`aux worktree` and `aux exec` are development aids: `agentuxd` manages run worktrees itself, and `aux exec` runs one prompt against a harness (`claude-code`, `codex`, `opencode`, `antigravity`), streams what it does and asks y/n for each permission request; Ctrl-C cancels the turn.

## Install

The AgentUX image ships `aux` and `agentuxd` preinstalled from the `agentux` RPM. On another Fedora 44 system, download the `.rpm` from the [releases](https://github.com/agentux-os/agentux-core/releases) and install it:

```sh
sudo dnf install ./agentux-<version>-1.fc44.x86_64.rpm
systemctl --user enable --now agentuxd.service   # optional: run the daemon as a user service
```

It installs `/usr/bin/aux`, `/usr/bin/agentuxd` and the user unit `/usr/lib/systemd/user/agentuxd.service`, requires `git` and recommends `gh`. On an rpm-ostree system, use `rpm-ostree install` instead of `dnf`.

The RPM is built by [`.github/workflows/release.yml`](.github/workflows/release.yml) in a Fedora 44 container from [`packaging/agentux.spec`](packaging/agentux.spec): pushing a `v<version>` tag that matches the workspace version in `Cargo.toml` builds it, installs and smoke-tests it in a clean container, and attaches it to the GitHub release; running the workflow manually leaves it as a workflow artifact.

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

- An ACP-backed step executor for `agentuxd` on top of `agentux-harness`, and usage reporting so budgets can be enforced.
- Headless fallbacks for the harnesses, behind the same `Harness` trait ([ADR 0002](https://github.com/agentux-os/agentux/blob/main/docs/adr/0002-harness-integration-via-acp.md)).
- Agent bus as an MCP server ([ADR 0004](https://github.com/agentux-os/agentux/blob/main/docs/adr/0004-unified-interface-and-agent-bus.md)).
- Sessions, permission requests from harnesses and `aux attach` in the API; the cockpit's real `DaemonClient` on top of it.

## Relevant ADRs

- [0002 — Harness integration via ACP](https://github.com/agentux-os/agentux/blob/main/docs/adr/0002-harness-integration-via-acp.md)
- [0003 — Workflow engine](https://github.com/agentux-os/agentux/blob/main/docs/adr/0003-workflow-engine.md)
- [0004 — Unified interface and agent bus](https://github.com/agentux-os/agentux/blob/main/docs/adr/0004-unified-interface-and-agent-bus.md)
- [0005 — Pipelines in `agentux.yaml`](https://github.com/agentux-os/agentux/blob/main/docs/adr/0005-agentux-yaml.md)

## License

[Apache 2.0](LICENSE)
