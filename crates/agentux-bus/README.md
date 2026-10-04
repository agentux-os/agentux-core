# agentux-bus

The agent bus from [ADR 0004](https://github.com/agentux-os/agentux/blob/main/docs/adr/0004-unified-interface-and-agent-bus.md): harness sessions of a run talk to each other and to the human through a mediated bus, exposed to each session as the `agentux` MCP server. Built on [`rmcp`](https://crates.io/crates/rmcp) 3.5, the official MCP Rust SDK.

## Pieces

- **`Bus`**: the core, independent of transport. The daemon opens a run (`open_run`, with a `BusConfig` built from the project's `agentux.yaml`), adds each harness session with its identity (run, project, role, vendor, session id) and removes it when it ends. The bus keeps a mailbox per session, routes messages and enforces limits. It never calls a harness.
- **Events** (`Bus::subscribe`): every join, leave, message, escalation, answer and refusal is a `BusEvent` with a gapless sequence number: the audit log the daemon persists and the cockpit shows. A `Wake` event asks the daemon to prompt a session (or start one for a role) so it notices new mail; the daemon sends `Wake::prompt` through ACP.
- **`BusBackend`**: what the daemon provides. `run_state` (step, branch, checks, open requests from the workflow engine) and `ask_human` (resolves when the human answers in the cockpit inbox). `MemoryBackend` implements it in memory for tests and standalone mode.
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

ACP `session/new` takes a list of MCP servers for the agent to connect to; stdio servers are the one transport every ACP agent must support. `agentux-harness` attaches them with `AcpHarness::with_mcp_servers` / `AcpSession::connect_with_mcp_servers`. The daemon will pass, for each session:

```text
name: agentux
command: <path to aux>
args: bus-stdio --socket <agentuxd socket> --session-token <token>   (see bus_stdio_args)
```

The agent launches `aux bus-stdio`, which serves `BusServer` over its stdio. The server does not hold a `Bus` but a `BusEndpoint`: one session's access to the bus.

- `LocalEndpoint` calls a `Bus` in the same process. Used by the daemon if it serves MCP itself, by the tests, and by `aux bus-stdio --standalone`.
- The daemon bridge (not written yet: `agentuxd` does not host a `Bus` nor serve it on its socket) will be a second `BusEndpoint` inside `aux bus-stdio`. It connects to the daemon socket (the global `--socket`) and exchanges the messages in [`src/bridge.rs`](src/bridge.rs):
  1. `aux` sends `BridgeHello { session_token }`.
  2. The daemon looks the token up (issued when it created the session, valid only while the session lives) and answers `BridgeWelcome::Accepted { identity, allowed_tools, max_turns_per_exchange }` or `Rejected { reason }`.
  3. Each tool call becomes `BridgeRequest { id, call: BusCall }`, answered by `BridgeResponse { id, result }`, where `result` is the `BusReply` or the `BusError` the agent sees as a tool error.

  These map naturally onto the daemon's newline-delimited JSON-RPC API ([docs/api.md](../../docs/api.md)) as two methods, e.g. `bus.hello` and `bus.call`, with the JSON-RPC id as the request id. One caveat: that API answers requests on a connection in order, and `ask_human` can wait for minutes, so either the bridge opens a connection per in-flight call or the daemon answers `bus.call` out of order. On the daemon side, each call maps to `Bus::call(&session, call)`. The token, not anything the agent sends, decides the identity, so a session cannot act as another one.

Until then, `aux bus-stdio --standalone [--project <dir>] [--role <r>] [--vendor <v>] [--run <id>]` serves an in-memory bus with that one session on it (roles and limits from the project's `agentux.yaml`) and prints bus events to stderr as JSON lines. Questions to the human stay pending.

## Tests

`tests/bus.rs` covers routing to sessions, roles, the run and the human; messages queued for a role without session; refusals (unknown role or session, messaging yourself, replying to a session that left); the turn limit; allowed-tools filtering; review requests and handoffs; run state; `ask_human` answered in time and late; and the audit log. `tests/mcp.rs` connects an rmcp client to `BusServer` in-process: tool listing and schemas, `post_message`/`read_messages` round trips, hidden tools, and limits reported as tool errors. `aux-cli/tests/bus_stdio.rs` drives `aux bus-stdio --standalone` as a child process over stdio.
