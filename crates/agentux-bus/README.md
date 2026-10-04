# agentux-bus

The agent bus from [ADR 0004](https://github.com/agentux-os/agentux/blob/main/docs/adr/0004-unified-interface-and-agent-bus.md): harness sessions of a run talk to each other and to the human through a mediated bus, exposed to each session as the `agentux` MCP server. Built on [`rmcp`](https://crates.io/crates/rmcp) 3.5, the official MCP Rust SDK.

## Pieces

- **`Bus`**: the core, independent of transport. The daemon opens a run (`open_run`, with a `BusConfig` built from the project's `agentux.yaml`), adds each harness session with its identity (run, project, role, vendor, session id) and removes it when it ends. The bus keeps a mailbox per session, routes messages and enforces limits. It never calls a harness.
- **Events** (`Bus::subscribe`): every join, leave, message, escalation, answer and refusal is a `BusEvent` with a gapless sequence number: the audit log the daemon persists and the cockpit shows. A `Wake` event asks the daemon to prompt a session (or start one for a role) so it notices new mail; the daemon sends `Wake::prompt` through ACP.
- **`BusBackend`**: what the daemon provides. `run_state` (step, branch, checks, open requests from the workflow engine) and `ask_human` (resolves when the human answers in the cockpit inbox). `agentuxd` implements it over its store and approvals inbox; `MemoryBackend` implements it in memory for tests and standalone mode.
- **`BusServer`**: the `agentux` MCP server for one session. Every call is made as that session's identity; tools that `agentux.yaml` does not allow are neither listed nor callable. Its `instructions` are the session prompt.
- **`session_prompt`**: the short system prompt each session starts with ([`src/session_prompt.md`](src/session_prompt.md)): its role and run, the bus tools it has, and the etiquette (be concise, reply in the same exchange, respect the turn limit, do not loop).

## Tools

| Tool | What it does |
|---|---|
| `post_message` | Sends to `role:<name>` (every session playing it in the run), `session:<id>`, `run` (everyone, no wake) or `human`. A reply passes `in_reply_to` and may omit `to`. |
| `read_messages` | Drains the caller's mailbox, oldest first. |
| `request_review` | Review request to the reviewer role (the pipeline's `review` step unless named), with the run's branch. |
| `handoff` | Passes the task to a role with a summary and pointers. |
| `get_run_state` | The backend's run state plus the run's sessions, the caller's unread count and pending human questions. |
| `ask_human` | Escalates to the human through the backend. Waits up to `BusConfig::ask_human_wait` (2 min); a later answer arrives as a mailbox message with a wake. |

Routing rules:

- A message without `in_reply_to` starts an **exchange**; replies join it. Each message uses one turn. Once an exchange has used `bus.max_turns_per_exchange` turns, further posts in it fail with a tool error telling the agent to stop and decide, or ask the human. The human is never cut off.
- A message to a role no live session plays is queued for the role, with a `Wake` for the role; the session the daemon starts for it finds the messages in its mailbox when it joins.
- Messages to `run` are delivered to everyone else in the run without waking them, so a broadcast cannot set off a chain of turns.
- `read_messages` comes with `post_message`: the default `agentux.yaml` lists `post_message` but not `read_messages`, and receiving is the other half of messaging.

Not enforced here: the token budget per run (ADR 0004) belongs to the daemon, which sees usage from the harness events.

## Transport and the daemon bridge

ACP `session/new` takes a list of MCP servers for the agent to connect to; stdio servers are the one transport every ACP agent must support. `agentux-harness` attaches them (`SessionOptions::mcp_servers`). `agentuxd` passes, for each session:

```text
name: agentux
command: <path to aux>
args: bus-stdio --socket <agentuxd socket>   (see bus_stdio_args)
env: AGENTUX_BUS_SESSION_TOKEN=<token>       (see bus_stdio_env; not on the command line, which other users can read)
```

The agent launches `aux bus-stdio`, which serves `BusServer` over its stdio. The server does not hold a `Bus` but a `BusEndpoint`: one session's access to the bus.

- `LocalEndpoint` calls a `Bus` in the same process: the tests and `aux bus-stdio --standalone`.
- `DaemonEndpoint` is the daemon bridge, what `aux bus-stdio` uses with a session token. It speaks two methods of the daemon's newline-delimited JSON-RPC API ([docs/api.md](../../docs/api.md#agent-bus)), with the types in [`src/bridge.rs`](src/bridge.rs):
  1. `bus.hello` with `BridgeHello { sessionToken }`, once at startup: the daemon answers `BridgeWelcome { identity, allowedTools, maxTurnsPerExchange }`, or the JSON-RPC error `-32004` (`bridge::UNAUTHORIZED`) for a token it did not issue or whose session has ended.
  2. `bus.call` with `BridgeCall { sessionToken, call: BusCall }` for each tool call: the result is a `BridgeOutcome`, `{"ok": BusReply}` or `{"err": BusError}` (a refusal the agent sees as a tool error).

  The daemon answers requests on one connection in order and `ask_human` can keep a call open for minutes, so `DaemonEndpoint` sends **each call on a connection of its own**: concurrent calls never queue behind each other, and the daemon needs no out-of-order responses. On the daemon side each call is `Bus::call(&session, call)` with the session the token was issued to. The token, not anything the agent sends, decides the identity, so a session cannot act as another one.

In the daemon, each run has its own `Bus`, opened with `BusConfig::from_config` when the run's first session starts; `agentuxd` is its `BusBackend` (run state from its store, `ask_human` as a `question` approval request), stores every `BusEvent` as a `bus_message` event, and turns each `Wake` into an ACP prompt for the target session (after its current turn), starting the role's session when the wake is for a role. A run reopened after a daemon restart calls `Bus::reserve_ids` with the highest ids in its stored log, so ids never repeat, and `Bus::restore` with what the log says about exchanges, reply routing and mail still waiting (sessions of the old daemon are reported as gone, their mail queued again for their role, with a wake per role). The human posts with `Bus::post_from_human` (the daemon's `bus.post`).

`aux bus-stdio --standalone [--project <dir>] [--role <r>] [--vendor <v>] [--run <id>]` serves an in-memory bus with that one session on it (roles and limits from the project's `agentux.yaml`) and prints bus events to stderr as JSON lines. Questions to the human stay pending.

## Tests

`tests/bus.rs` covers routing to sessions, roles, the run and the human; messages queued for a role without session; refusals (unknown role or session, messaging yourself, replying to a session that left); the turn limit; allowed-tools filtering; review requests and handoffs; run state; `ask_human` answered in time and late; the audit log; and a bus restored after a restart. `tests/mcp.rs` connects an rmcp client to `BusServer` in-process: tool listing and schemas, `post_message`/`read_messages` round trips, hidden tools, and limits reported as tool errors. `aux-cli/tests/bus_stdio.rs` drives `aux bus-stdio --standalone` as a child process over stdio, and `aux-cli/tests/bus_bridge.rs` has a fake agent launch `aux bus-stdio` (token in its environment) from its `session/new` against an in-process daemon. `agentuxd/tests/bus.rs` covers the bus inside the daemon end to end with fake ACP agents: a review request waking a reviewer started for it and its reply waking the implementer, `ask_human` answered through the API, the turn limit stopping a ping-pong, tokens scoped to their session, the log replayed after a restart, the human prompting a session (`sessions.prompt`) and posting on the bus (`bus.post`), paged history and the `replay_done` marker, and mail and exchanges surviving a restart.
