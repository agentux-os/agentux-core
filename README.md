# agentux-core

The orchestration engine of [AgentUX](https://github.com/agentux-os/agentux): the daemon, CLI and integrations that let coding agents from different vendors work as one team.

> **Status:** early. `agentuxd` runs pipelines from `agentux.yaml` as persisted state machines in run worktrees, with gates, bounded loops, approvals and crash recovery, and `aux` drives it over a local socket. Agent steps run in real harness sessions over ACP (tried with OpenCode; Claude Code and Codex go through their ACP adapters), with agent permission requests in the approvals inbox, session events in the event log, budgets enforced from reported cost, and pull requests opened with `gh`. `--fake-agents` swaps in a scripted fake. Design decisions live in [agentux/docs/adr](https://github.com/agentux-os/agentux/tree/main/docs/adr).

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
| [`agentux-bus`](crates/agentux-bus) | The agent bus ([ADR 0004](https://github.com/agentux-os/agentux/blob/main/docs/adr/0004-unified-interface-and-agent-bus.md)): sessions with a scoped identity (run, project, role, vendor), mailboxes, routing to a session, a role, the run or the human, `request_review`, `handoff`, `get_run_state` and `ask_human`, with the `bus` limits from `agentux.yaml` (allowed tools, turns per exchange). Every exchange is an audit event; delivery is a wake event the daemon turns into an ACP prompt. Served to each session as the `agentux` MCP server (rmcp), with the session system prompt. Not wired into `agentuxd` yet. |
| [`agentuxd`](crates/agentuxd) | The daemon: run state machine ([ADR 0003](https://github.com/agentux-os/agentux/blob/main/docs/adr/0003-workflow-engine.md)), the ACP step executor (`AcpExecutor`, with the step prompts in `prompts.rs` and the `gh` pull request step in `forge.rs`) and the API on a Unix socket ([docs/api.md](docs/api.md)). Also a library, so `aux daemon` runs the same code. |
| [`agentux-fake-agent`](crates/agentux-fake-agent) | Test support, not shipped: a scriptable ACP agent that runs in-process over a byte pipe, used by the harness and daemon tests. |
| [`aux-cli`](crates/aux-cli) | The `aux` binary. |

### Running

```sh
aux daemon [--fake-agents] [--auto-approve-permissions]
                                    # agentuxd in the foreground (or: agentuxd, or the systemd unit below)
aux run [project-dir] --prompt "Add a health endpoint" [--issue 42] [--watch]
aux ps [--all]                      # runs, and approvals and agent permission requests waiting for you
aux approve <request-id> [-m note]
aux deny <request-id> [-m reason]
aux watch <run-id>                  # history, then live events until the run ends
aux cancel <run-id>
```

The socket is `$XDG_RUNTIME_DIR/agentux/agentuxd.sock` (override with `--socket` or `$AGENTUX_SOCKET`); state lives in `$XDG_STATE_HOME/agentux/agentuxd.db`. [`contrib/agentuxd.service`](contrib/agentuxd.service) is a systemd user unit for the image.

A run gets its own worktree, then walks the pipeline: agent steps call the executor, gates run the configured checks with `sh -c` in the worktree, failures and requested changes loop back within `max_attempts`/`max_rounds`, and `approve: true` pauses the run until `aux approve`. Each transition is committed before its side effect, so a restarted daemon resumes every unfinished run, re-running a step that was interrupted. The API, event stream and lifecycle are documented in [docs/api.md](docs/api.md).

How agent steps work:

- **Sessions.** Each role gets one harness session (the role's `harness` from `agentux.yaml`, launched as in the [`agentux-harness` table](crates/agentux-harness/README.md#harnesses)) in the run's worktree, started when the run first reaches the role and reused by its later steps. Each harness uses the login you set up for it. Sessions end with the run; after a daemon restart, new ones start. A role's `model` is recorded but not passed to the harness yet.
- **Prompts** ([`prompts.rs`](crates/agentuxd/src/prompts.rs)) are self-contained: the run's prompt or issue, the step, the role, the plan, and the gate output or review comments that sent the run back. The planner's reply is the plan.
- **Commits.** Agents are told to edit files and not commit; after each agent step the daemon commits whatever changed in the worktree (`git add --all`, so keep build artifacts in `.gitignore`), with a subject derived from the run (`<title>`, `Fix failing checks: <title>`, `Address review comments: <title>`). Without a git identity it commits as `AgentUX <agentux@localhost>`.
- **Review verdicts.** The reviewer is asked to end with a fenced JSON block, `{"verdict": "APPROVE" | "CHANGES_REQUESTED", "comments": ...}`. The last such object wins; without one, the last upper-case `APPROVE` / `CHANGES_REQUESTED` keyword does; without either, the reviewer is asked once more, then the step fails.
- **Permissions.** When an agent asks before a tool call, the run waits (`waiting`) with a `permission` request in `aux ps`; the agent waits for `aux approve` or `aux deny`. A denial tells the agent no and the run goes on. `--auto-approve-permissions` allows everything without asking: **dangerous**, since agents can then run any command with your user's rights; use it only for unattended runs in a sandbox you trust. (Some harnesses, OpenCode among them, run tools without asking through ACP; then there is nothing to approve.)
- **Budget.** Harnesses that report cost update the run's `costUsd`. Before each agent step, a run over `budget.max_usd_per_run` pauses with a `budget` request; approving gives it another `max_usd_per_run`, denying fails it. A single step can overshoot.
- **Pull requests.** With a GitHub `origin` and `gh` logged in, the `pull_request` step pushes `aux/<run-id>` and opens a pull request (title: the run's title; body: the request, the plan and the review), or reuses the one already open for the branch. Otherwise the branch stays and the step records `PR skipped: <reason>`.

Other commands:

```sh
aux validate [path]                 # check agentux.yaml (file or project dir) without starting a run;
                                    # without a file, shows the default pipeline and detected checks
aux exec --harness <id> [--cwd <dir>] "<prompt>"
aux worktree create <run-id> [--from <ref>] [--base-dir <dir>] [--repo <dir>]
aux worktree list [--repo <dir>]
aux worktree remove <run-id> [--force] [--delete-branch] [--repo <dir>]
aux bus-stdio --standalone [--project <dir>] [--role <r>]   # the agentux MCP server over stdio, on an in-memory bus
```

`aux worktree` and `aux exec` are development aids: `agentuxd` manages run worktrees itself, and `aux exec` runs one prompt against a harness (`claude-code`, `codex`, `opencode`, `antigravity`), streams what it does and asks y/n for each permission request; Ctrl-C cancels the turn.

`aux bus-stdio` is the stdio MCP server harnesses will launch to reach the bus (passed in ACP `session/new`, which `agentux-harness` now supports). Until `agentuxd` serves the bus it only runs with `--standalone`; see the [`agentux-bus` README](crates/agentux-bus/README.md#transport-and-the-daemon-bridge) for the integration plan.

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

- Passing a role's `model` to harnesses; per-project permission policies in `agentux.yaml`.
- Headless fallbacks for the harnesses, behind the same `Harness` trait ([ADR 0002](https://github.com/agentux-os/agentux/blob/main/docs/adr/0002-harness-integration-via-acp.md)).
- Agent bus in `agentuxd`: the bridge behind `aux bus-stdio`, a `BusBackend` over run state and the approvals inbox, wakes turned into prompts, and bus events in the event log ([`agentux-bus`](crates/agentux-bus/README.md#transport-and-the-daemon-bridge)).
- `aux attach` (follow and talk to one session); the cockpit's real `DaemonClient` on top of the sessions API.

## Relevant ADRs

- [0002 — Harness integration via ACP](https://github.com/agentux-os/agentux/blob/main/docs/adr/0002-harness-integration-via-acp.md)
- [0003 — Workflow engine](https://github.com/agentux-os/agentux/blob/main/docs/adr/0003-workflow-engine.md)
- [0004 — Unified interface and agent bus](https://github.com/agentux-os/agentux/blob/main/docs/adr/0004-unified-interface-and-agent-bus.md)
- [0005 — Pipelines in `agentux.yaml`](https://github.com/agentux-os/agentux/blob/main/docs/adr/0005-agentux-yaml.md)

## License

[Apache 2.0](LICENSE)
