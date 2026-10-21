# agentuxd API

`agentuxd` serves one local API, used by `aux` and (later) the cockpit. Types and a Rust client live in [`agentux-api`](../crates/agentux-api); the daemon, `aux` and the cockpit's Tauri backend all depend on that crate (ADR 0006).

## Transport

- **Unix domain socket** at `$XDG_RUNTIME_DIR/agentux/agentuxd.sock`. `$AGENTUX_SOCKET`, or `--socket` on `agentuxd`/`aux`, overrides it. The directory is created with mode `0700` and the socket with `0600`: only the user who runs the daemon can talk to it. Nothing listens on the network.
- **Newline-delimited JSON-RPC 2.0.** Each line is one JSON object: a request, a response or a notification. Requests on one connection are answered in order, so a slow request (a `bus.call` waiting for the human) holds up the ones after it: use another connection for those. A request without `id` is a notification and gets no response.
- Field names are `camelCase`; enum values are `snake_case`; timestamps are milliseconds since the Unix epoch.

Why JSON-RPC over a raw socket rather than HTTP: the API is a handful of commands plus one event stream, both directions fit on one connection, and a client is a couple hundred lines on top of tokio (see `agentux-api/src/client.rs`) with no HTTP stack in the daemon or `aux`. ADR 0006 mentions server-sent events; an event subscription on a JSON-RPC connection gives the same push semantics. Talking to it by hand:

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"runs.list"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/agentux/agentuxd.sock
```

## Methods

| Method | Params | Result |
|---|---|---|
| `projects.register` | `{ path }` — absolute path inside a git repository | `Project` (idempotent per repository) |
| `projects.list` | — | `Project[]` |
| `runs.start` | `{ projectId, prompt?, issue?, title?, roles?, models? }` — `prompt` or `issue` required; `roles` maps role names to harness ids for this run, `models` role names to models | `Run` |
| `harnesses.test` | `{ harness, model? }` | `{ harness, ok, detail, seconds, limit }`: one short prompt, answered like a helper question (no tools); a usage limit in the answer is recorded |
| `preferences.get` | none | `AgentPrefs`: the user's agent preferences; see [Agent preferences](#agent-preferences) |
| `preferences.set` | `AgentPrefs` | `AgentPrefs`, as written to the file; an unknown harness or 0 minutes is `-32602` |
| `limits.list` | none | `HarnessLimit[]`: the usage limits in force; see [Usage limits](#usage-limits) |
| `limits.clear` | `{ harness, model? }` — without `model`, every limit of the harness | `HarnessLimit[]` still in force; runs waiting for the harness go on |
| `harnesses.list` | none | `[{ id, command, installed, signedIn, experimental, limits, version, usage: { todayUsd, weekUsd, weekSessions }, quota: [{ label, usedPercent, resetsAt, seenAt }] }]`: the built-in harnesses, whether each one's vendor program (`command`) is on the daemon's `PATH`, whether the user signed in to it (`null`: cannot tell), its `--version`, what its sessions cost, and its plan's usage windows where the harness's own files say (Codex) |
| `runs.list` | `{ projectId? }` | `Run[]`, newest first |
| `runs.get` | `{ runId }` | `{ run: Run, attempts: StepAttempt[], requests: PermissionRequest[], sessions: Session[] }` |
| `runs.cancel` | `{ runId }` | `Run` |
| `runs.events` | `{ runId, sinceSeq?, limit? }` — `limit` defaults to 1000, at most 10000 | `{ events: Event[], more, headSeq }`: the run's stored events with `seq > sinceSeq`, oldest first; see [History](#history) |
| `sessions.list` | `{ runId? }` | `Session[]`, oldest first |
| `sessions.prompt` | `{ sessionId, text }` | `{ session: Session, queued }` — the human's message to a live session; see [Talking to a session](#talking-to-a-session) |
| `requests.list` | `{ pending? }` — `true` for the approvals inbox | `PermissionRequest[]`, newest first |
| `requests.approve` | `{ requestId, answer? }` — `answer` is required for `question` requests | `PermissionRequest` |
| `requests.deny` | `{ requestId, answer? }` | `PermissionRequest` (the run fails, except for `permission` requests, where the agent is told no and goes on, and `question` requests, where the agent is told the human declined) |
| `events.subscribe` | `{ runId?, since? }` | `{ seq }`, then `event` notifications, with one `replay_done` notification where the replay ends |
| `bus.list` | `{ runId }` | `BusMessage[]`, the run's agent bus log, oldest first |
| `bus.post` | `{ runId, to?: BusEndpoint, body, subject?, inReplyTo? }` | `{ messageId, exchange, turn, deliveredTo, queuedForRole }` — a message from the human on the run's bus; see [Agent bus](#agent-bus) |
| `terminals.open` | `{ sessionId?, runId?, command?: "harness-tui" \| "shell", cols, rows }` | `Terminal`, then `terminal_output` / `terminal_exit` notifications on this connection; see [Terminal mode](#terminal-mode) |
| `terminals.attach` | `{ terminalId }` | `Terminal`, then the terminal's scrollback and live output on this connection |
| `terminals.write` | `{ terminalId, data }` — `data` is base64 | `null` |
| `terminals.resize` | `{ terminalId, cols, rows }` | `null` |
| `terminals.close` | `{ terminalId }` | `null`; `terminal_exit` follows |
| `terminals.list` | `{ sessionId?, runId? }` | `Terminal[]`, oldest first |
| `helper.ask` | `{ harness, question, context?, model? }` | `{ harness, answer }` — one read-only question for the cockpit's helper, answered by a fresh session of `harness`; see [Helper questions](#helper-questions) |
| `bus.hello` | `{ sessionToken }` | `{ identity, allowedTools, maxTurnsPerExchange }` — bus bridge only, see [Agent bus](#agent-bus) |
| `bus.call` | `{ sessionToken, call }` | `{ ok: <tool result> }` or `{ err: <refusal> }` — bus bridge only |

`runs.start` reads the project's `agentux.yaml` at that moment and stores it with the run; editing the file later does not affect a run in progress. Without the file, the built-in default pipeline applies, with the harnesses you can use: installed, and not known to be signed out (`harnesses.list`). Each role keeps its default harness (Claude Code to plan and implement, Codex to review) when that one is usable, and otherwise takes the first usable one, in the order of `harnesses.list`; the reviewer prefers a different harness than the implementer. When none is usable, the defaults stay, and the run fails with what is missing when it reaches a role.

Sign-in is read from where each CLI keeps it: `~/.claude/.credentials.json` or `ANTHROPIC_API_KEY` / `CLAUDE_CODE_OAUTH_TOKEN` for Claude Code, `$CODEX_HOME/auth.json` (`~/.codex`) or `OPENAI_API_KEY` for Codex, OpenCode's `auth.json` under `$XDG_DATA_HOME/opencode`, and `~/.gemini/antigravity-cli/antigravity-oauth-token` for Antigravity. OpenCode can also take keys from its config, and Antigravity may keep its token elsewhere, so for those two a missing file is `null`, not signed out.

`roles` overrides the harness of the pipeline's roles for this run (`{"implementer": "opencode", "reviewer": "antigravity"}`). A role the pipeline does not have, or an unknown harness id, is `-32602`. A role's `model` is dropped when its harness is replaced. The run's `roles` field shows what each role got.

### Errors

Standard JSON-RPC codes (`-32700` parse error, `-32600` invalid request, `-32601` unknown method, `-32602` invalid params, `-32603` internal), plus:

| Code | Meaning |
|---|---|
| `-32001` | Not found: no such project, run, request, session or terminal |
| `-32002` | Conflict: e.g. approving a request that is no longer pending, cancelling a finished run, prompting a session that has ended, posting on the bus of a finished run |
| `-32003` | Invalid project: not a git repository, or its `agentux.yaml` is invalid |
| `-32004` | Unauthorized: `bus.hello` / `bus.call` with an unknown session token, or the token of a session that has ended |

## Events

```json
{"jsonrpc":"2.0","method":"event","params":{"seq":42,"at":1790000000000,"runId":"3f9a0c12","kind":"run","run":{...}}}
```

Every state change is stored as an event in the same transaction as the change, then pushed to subscribers after the commit. `seq` increases by one per event. `events.subscribe` with `since: N` first replays stored events with `seq > N` (`since: 0` replays everything), then continues live, without gaps or duplicates; without `since`, only new events are sent. `runId` limits the stream to one run. A client that reconnects passes the last `seq` it saw.

Where the replay ends, the subscription gets one notification of its own, exactly once, before any live event (right away when there is nothing to replay):

```json
{"jsonrpc":"2.0","method":"replay_done","params":{"seq":57}}
```

`seq` is the newest event in the log when the replay was read: the client has every event of its stream up to it, and everything after the marker is live. Clients that only want events (the Rust `Subscription::next`) skip it; `Subscription::next_notice` returns it as `Notice::ReplayDone`.

### History

`runs.events { runId, sinceSeq?, limit? }` returns a run's stored events (every kind: `session_event`s, `bus_message`s, run snapshots, logs...) in pages, without a subscription: `{ events, more, headSeq }`. While `more` is true, ask again with `sinceSeq` set to the last event's `seq`. `headSeq` is the newest `seq` in the whole log when the page was read; once `more` is false, `events.subscribe { runId, since: headSeq }` continues live without a gap or a duplicate. An unknown run is `-32001`.

| `kind` | Payload | When |
|---|---|---|
| `project` | `project: Project` | A project was registered |
| `run` | `run: Run` | Any change to a run (full snapshot) |
| `request` | `request: PermissionRequest` | An approval was requested or resolved |
| `attempt` | `attempt: StepAttempt` | A step started or finished |
| `log` | `text` | Check output, agent summaries, commits, notes such as a resume after restart |
| `session` | `session: Session` | A session started or changed state or usage (full snapshot) |
| `session_event` | `sessionId`, `event: SessionEvent` | Something happened in a session |
| `bus_message` | `message: BusMessage` | Traffic on the run's agent bus (see [Agent bus](#agent-bus)) |
| `limits` | `limits: HarnessLimit[]` | The usage limits in force changed (no `runId`) |

Session events are stored in the event log like the others, so `events.subscribe` with `since: 0` replays a run's sessions. Agent message chunks are coalesced (up to about 4 KB, or 750 ms); reasoning chunks are not recorded; tool output and diff texts are cut at 16 KB.

Snapshot events carry the whole object, so a client can keep a map keyed by id and replace entries.

## Types

```ts
type StepKind = "plan" | "implement" | "gate" | "review" | "pull_request" | "custom";
type RunStatus = "running" | "waiting" | "done" | "failed" | "cancelled"; // waiting = paused on an approval

interface Project { id: string; name: string; path: string; createdAt: number }

interface HarnessLimit {
  harness: string;           // e.g. "claude-code"
  model: string | null;      // the model the limit is for; null: the whole harness
  kind: "session" | "weekly" | "credits" | "rate" | "stalled";
  until: number | null;      // when it ends; null: only when cleared (credits)
  untilKnown: boolean;       // the harness said when; false: a guess, then tried again
  message: string;           // what the harness said
  at: number;
}

interface Run {
  id: string;              // 8 hex digits; the branch is aux/<id>
  projectId: string;
  title: string;
  prompt: string | null;
  issue: number | null;
  branch: string | null;   // set once the worktree exists
  worktree: string | null;
  steps: StepKind[];       // the pipeline
  stepIndex: number;
  step: StepKind;
  status: RunStatus;
  roles: Record<string, string>;  // role -> harness
  sessions: Record<string, string>;  // role -> session id, as the run reaches each role
  checks: { name: string; command: string; status: "pending" | "running" | "passed" | "failed" }[];
  gateAttempt: number; gateMaxAttempts: number;   // most recent gate
  reviewRound: number; reviewMaxRounds: number;   // most recent review
  budgetUsd: number | null;  // budget.max_usd_per_run, raised by that much per approved overrun
  costUsd: number;           // sum of the sessions' reported cost (0 if none reports it)
  startedAt: number; updatedAt: number; finishedAt: number | null;
  pullRequest: { number: number; url: string } | null;
  activity: string;        // one line: what is happening now
  error: string | null;    // why it failed
}

interface StepAttempt {
  id: number; runId: string; stepIndex: number; step: StepKind;
  status: "running" | "succeeded" | "failed" | "changes_requested" | "interrupted" | "cancelled";
  output: string | null;   // agent summary, review comments or check output
  startedAt: number; finishedAt: number | null;
}

interface PermissionRequest {
  id: string;
  // plan = approve the plan; step = any other approve: true step;
  // permission = an agent asks before a tool call; budget = the run is over budget;
  // question = an agent asks the human through the bus (ask_human)
  kind: "plan" | "step" | "permission" | "budget" | "question" | "limit";
  runId: string; projectId: string; stepIndex: number; step: StepKind;
  sessionId: string | null;  // the session that asked, for permission and question requests
  title: string; detail: string;
  options: string[];         // suggested answers of a question (free text is fine too); [] otherwise
  status: "pending" | "approved" | "denied" | "cancelled";
  answer: string | null;
  createdAt: number; resolvedAt: number | null;
}

// One harness process working for one role of a run. Close to the cockpit's
// Session; `harness` is its `vendor`, and events are not embedded (they are
// `session_event` events in the log).
interface Session {
  id: string; runId: string; projectId: string;
  role: string; harness: string; model: string | null;
  // waiting = on a permission request; attached = open in its harness's TUI
  // (terminal mode): turns for it wait until the terminal closes
  state: "active" | "idle" | "waiting" | "attached" | "ended";
  cwd: string;                                      // the run's worktree
  usage: { usedTokens: number; contextTokens: number; costUsd: number | null };
  startedAt: number; updatedAt: number; endedAt: number | null;
  // The harness's own id of the session: the ACP sessionId, which Claude Code,
  // Codex and OpenCode share with their CLI. null until the harness started it.
  vendorSessionId: string | null;
}

// A process on a pseudo-terminal managed by the daemon (terminal mode).
interface Terminal {
  terminalId: string;
  sessionId: string | null; runId: string | null;
  command: "harness-tui" | "shell";  // what actually runs (a fallback is "shell")
  fallback: string | null;           // why a harness-tui request got a shell
  argv: string[];                    // empty while waiting
  cwd: string;                       // the run's worktree
  cols: number; rows: number;
  state: "waiting" | "running" | "exited";  // waiting = for the session's turn in progress
  exitCode: number | null;
  createdAt: number;
}

// ACP reports context-window usage and cumulative session cost, not
// input/output token counts, so `usage` differs from the cockpit's TokenUsage.
type SessionEvent =
  | { kind: "message"; from: "user" | "agent" | "system" | "human"; text: string }  // user = the prompt AgentUX sent; human = sessions.prompt
  | { kind: "tool_call"; toolCallId: string; tool?: ToolKind; title?: string;
      status?: "running" | "ok" | "error"; output?: string }  // first event of a call has tool and title; updates carry what changed
  | { kind: "diff"; toolCallId: string; path: string; oldText: string | null; newText: string }  // whole texts, not hunks
  | { kind: "plan"; items: { text: string; status: "pending" | "in_progress" | "done" }[] }
  | { kind: "permission"; requestId: string }
  | { kind: "usage"; usage: Session["usage"] };

type ToolKind = "read" | "edit" | "delete" | "move" | "search" | "execute" | "think" | "fetch" | "other";

// One entry of a run's agent bus log. Close to the cockpit's BusMessage
// (id, runId, projectId, tool, from, to, subject, body, at, turn, maxTurns);
// the fields after maxTurns are additions, and BusEndpoint has two more kinds.
interface BusMessage {
  id: string;                  // unique id of the log entry
  runId: string; projectId: string;
  kind: BusMessageKind;
  tool: string | null;         // post_message | request_review | handoff | ask_human | ..., when a tool call caused it
  from: BusEndpoint; to: BusEndpoint;
  subject: string;             // one line
  body: string;                // the message text; for a wake, the prompt sent
  at: number;
  turn: number; maxTurns: number;  // turn within the exchange (messages; 0 otherwise), and the run's limit
  messageId: number | null;    // the bus's message id (agents pass it as in_reply_to); also on the wake it caused
  exchange: number | null;
  inReplyTo: number | null;
  questionId: number | null;   // question and answer
  requestId: string | null;    // the `question` request holding a question
  deliveredTo: string[];       // session ids whose mailbox received a message
  queuedForRole: string | null;  // no session played the target role: the message waits for one
}

type BusMessageKind =
  | "message" | "review_request" | "handoff"  // routed messages (post_message, request_review, handoff)
  | "human_answer"                            // an answer that outlived the ask_human call, delivered as mail
  | "question" | "answer"                     // ask_human and the human's answer
  | "wake"                                    // the daemon prompts a session (or starts one for a role) for new mail
  | "turn_limit" | "tool_denied"              // refusals
  | "joined" | "left";                        // sessions entering and leaving the bus

type BusEndpoint =
  | { kind: "session"; sessionId: string; role: string; vendor: string }  // vendor = harness; role and vendor may be omitted in bus.post
  | { kind: "role"; role: string }   // every session playing the role
  | { kind: "run" }                  // the run's channel (no wake)
  | { kind: "human" }
  | { kind: "daemon" };              // agentuxd: wakes, refusals
```

## Run lifecycle

1. **Setup.** The daemon creates branch `aux/<run-id>` and its worktree (`<repo>.worktrees/<run-id>`) from the project's `HEAD`.
2. **Steps**, in pipeline order:
   - `plan`, `implement`, `review`, `custom` prompt the role's harness session (see the README for prompts, the review verdict format and the commit after each `implement` or `custom` step). A review may request changes.
   - `gate` runs each listed check with `sh -c` in the worktree, on the host or, with `isolation.mode: podman`, in a rootless Podman container (see the README); all checks run, and the failures' output becomes feedback. A check that exceeds its `timeout` (default 30 minutes) fails, and everything it started is killed. When checks cannot run at all (Podman missing, the container cannot start), the run fails with that reason and does not loop back.
   - `pull_request` pushes the branch and opens (or finds) its pull request with `gh` when `origin` is on GitHub and `gh` is logged in; otherwise it succeeds with `PR skipped: <reason>` and the branch stays.
3. **Loops.** A failing gate goes back to its `on_fail` step with the failure output, up to `max_attempts` consecutive gate attempts; a passing gate resets the count. A review requesting changes goes back to `on_changes_requested` with the comments, up to `max_rounds` reviews per run. When a limit is reached the run fails. A gate without `on_fail` fails the run on its first failure.
4. **Approvals.** `approve: true` on an agent step pauses the run after the step (e.g. to approve the plan); on `pull_request` it pauses before opening it. The run is `waiting` until `requests.approve` (continue) or `requests.deny` (fail).
5. **Permissions.** An agent asking before a tool call creates a `permission` request and the run is `waiting` while the agent waits (the step does not end). `requests.approve` lets the tool run; `requests.deny` tells the agent no and the step goes on. Requests still pending when the agent's turn ends, the run is cancelled or the daemon restarts become `cancelled`. With `--auto-approve-permissions` no request is created and a `log` event records each allowed call.
6. **Budget.** Before each agent step, a run whose `costUsd` exceeds `budgetUsd` pauses on a `budget` request. Approving raises `budgetUsd` to `costUsd + budget.max_usd_per_run`; denying fails the run.

7. **Usage limits.** A harness that says it ran out is kept out of use until its limit resets, and the step runs again with another agent, or waits; see [Usage limits](#usage-limits).
8. **Agent bus.** Agents talk to each other and to the human through the `agentux` MCP server; see [Agent bus](#agent-bus).
9. **The human in a session.** `sessions.prompt` sends the human's message to a live session; see [Talking to a session](#talking-to-a-session).

### Usage limits

When an agent step's harness answers with a usage limit instead of work (an error, or a short reply that is only the notice), the daemon records a `HarnessLimit` and runs the step again. It reads what Claude Code, Codex, Gemini / Antigravity and HTTP APIs say:

- **Which limit** (`kind`): `session` (Claude Code's 5-hour window, Codex's usage limit), `weekly`, `credits` (credit balance too low, insufficient quota, billing), or `rate` (HTTP 429, quota exceeded).
- **For what:** a model the notice names (`opus`, `gemini-2.5-pro`, `gpt-5-codex`) when the role uses that model, else the whole harness.
- **Until when** (`until`, `untilKnown: true`): Claude Code's `limit reached|<epoch>`, `resets 3pm (America/Sao_Paulo)`, `resets Oct 27, 9am`; Codex's `try again in 2 days 3 hours` or `try again at 3:05 PM`; Gemini's `retry in 41.5s`, `reset after 3h12m`, `"retryDelay": "30s"`; `retry-after: 120`. Times without a zone are in the daemon's local time zone. Without a time, the daemon tries the harness again after a guess (`untilKnown: false`): 2 minutes for `rate`, an hour for `session`, a day for `weekly`. `credits` have no time: they hold until `limits.clear`.

**Stalled agents.** A turn that goes 10 minutes without a sign of life from the agent (no message, tool call or update), or 60 minutes while one of its tool calls runs (a long build or test suite says nothing until it ends), is given up on: the daemon cancels it, closes the session and records a `stalled` limit for the harness, for 15 minutes. Time the agent spends waiting for the human (a permission request, a bus `ask_human`) does not count. The step then goes on like any other limit: another agent, or a wait.

While a limit holds, the harness (or model) is out of use: the built-in pipeline does not pick it, and an agent step of a role it plays gives the role to the role's `fallback` harnesses in `agentux.yaml`, in order, or, without the key, to the first other harness the user can use (`harnesses.list` order); `fallback: []` keeps the role. When no harness can take it, the run waits on a `limit` request saying why and until when. It goes on by itself once the limit is over (checked every 15 seconds), or when the human approves the request or calls `limits.clear` (after adding credits or resetting the limit on the vendor's site). Denying the request fails the run.

```yaml
roles:
  implementer:
    harness: antigravity
    fallback: [opencode, claude-code]
```

### Agent preferences

`$XDG_CONFIG_HOME/agentux/agents.yaml` (`~/.config/agentux/agents.yaml`) holds the user's choices for every project; a project's `agentux.yaml` comes first. The daemon reads it each time, so edits apply to the next run (and the next turn, for the minutes). The cockpit's Agents view writes it through `preferences.set`.

```yaml
order: [opencode, claude-code]     # most preferred first; the others follow
roles:                             # projects without agentux.yaml
  planner:
    harness: claude-code
    model: opus
fallback: [opencode]               # roles with no fallback of their own
idle_minutes: 10                   # a turn with no sign of life is given up on
tool_minutes: 60                   # the same while a tool call runs
notifications: true                # desktop notifications
```

`order` is the order automatic picks and fallbacks try harnesses in. A preferred role harness that is not usable (not installed, signed out, at a limit) is replaced like a default one, and its model goes with it. **Notifications** go to the desktop (`org.freedesktop.Notifications`) when an agent reaches a limit or stops responding, when another takes over, when a run waits for a limit and when it goes on.

**Plan usage.** Codex writes its 5-hour and weekly windows into its session logs (`$CODEX_HOME/sessions`); `harnesses.list` shows them, and a spent window keeps Codex out of use until its reset before a step even starts.

### Talking to a session

`sessions.prompt { sessionId, text }` sends `text`, as typed, to a live session as an extra ACP turn (`aux say <session-id> <text...>`). Like a bus wake, it is queued behind the turn in progress (a step's or a wake's) and any message queued before it; `queued` in the result says whether the session was in a turn. The message is recorded when it is accepted, as a `session_event` with `{ kind: "message", from: "human", text }` (it is not recorded again as a `user` prompt when it is sent). The turn's events are `session_event`s like a step's; its permission requests are `permission` requests of the run's current step. An empty `text` is `-32602`, an unknown session `-32001`, and a session that has ended (or whose run has finished) `-32002`. If the session ends before the message's turn, a `log` event says the message was not handled.

### Terminal mode

ADR 0004's escape hatch: a session opened in its harness's own TUI, or a shell in the run's worktree, on a pseudo-terminal the daemon manages (`portable-pty`). The cockpit renders it with xterm.js; `aux attach` uses the local terminal.

**Opening.** `terminals.open { sessionId?, runId?, command?, cols, rows }` (`cols` and `rows` at least 1). `command` defaults to `"harness-tui"` with a `sessionId` and `"shell"` with only a `runId` (`"harness_tui"` is accepted too). The working directory is the run's worktree. The process gets the daemon's environment plus `TERM=xterm-256color`, `COLORTERM=truecolor`, `AGENTUX_RUN_ID`, `AGENTUX_WORKTREE` and, for a session, `AGENTUX_SESSION_ID`, `AGENTUX_ROLE`, `AGENTUX_HARNESS`, `AGENTUX_VENDOR_SESSION_ID`. The shell is the daemon's `$SHELL`, else `/bin/sh`. Errors: no `sessionId` or `runId`, `harness-tui` without a session, or a size of 0 `-32602`; an unknown session or run `-32001`; a run without a worktree yet `-32002`.

**`harness-tui`.** The TUI resumes the session's `vendorSessionId`:

| Harness | Command | Same id as ACP |
|---|---|---|
| `claude-code` | `claude --resume <id>` | yes: `claude-agent-acp` starts Claude Code with the ACP session id as its session id |
| `codex` | `codex resume <id>` | yes: `codex-acp` uses the Codex thread id as the ACP session id |
| `opencode` | `opencode --session <id>` | yes: OpenCode's ACP session id is its own session id |
| `antigravity` | none | the terminal is a shell |

An ACP session and the vendor's TUI are separate processes, so a live session is handed over, never driven by both:

1. The terminal starts `waiting` and prints a line saying so; the session's turn in progress, if any, ends first (turns queued before the terminal run first too).
2. The daemon shuts down the session's ACP adapter process and sets the session `attached`, with a `system` message. From then on every turn for the session (the run's next step for that role, bus wakes, `sessions.prompt`) waits; a turn that waits gets a `system` message saying so, and `sessions.prompt` returns `queued: true`.
3. The TUI starts (`running`) on the same vendor session. The run goes on; only the turns of that session wait.
4. When the TUI exits (the user quits it, `terminals.close`, or the connection that opened it closes), the daemon reopens the session over ACP with `session/load` and the same id, so the agent continues with whatever was said in the TUI (the history the agent replays while loading is not recorded again), sets it `idle` with a `system` message, and the waiting turns run. If reopening fails, the session ends and the role's next turn starts a new one.

For a session that has ended, or whose run has finished, no ACP adapter holds it: the TUI just resumes it. The agent bus's `agentux` MCP server is not passed to the TUI.

**Fallback.** When the TUI cannot resume the session (the harness has no such command, the session has no `vendorSessionId` yet, the executor cannot hand it over, e.g. with `--fake-agents`, or the TUI does not start, e.g. its CLI is not installed), the terminal is a shell instead: `command` is `"shell"`, `fallback` says why, and the output starts with a banner (`[agentux] <reason>`, then `[agentux] This is a shell in the run's worktree, <path>. ...`). The session is not handed over then.

**Streaming.** The result of `terminals.open` and `terminals.attach` is followed, on the same connection, by notifications:

```json
{"jsonrpc":"2.0","method":"terminal_output","params":{"terminalId":"5d1c9a02","data":"aGVsbG8NCg=="}}
{"jsonrpc":"2.0","method":"terminal_exit","params":{"terminalId":"5d1c9a02","code":0}}
```

`data` is base64 (standard alphabet, padded) of raw terminal bytes, to feed to the terminal emulator as is. `code` is the exit code, or `null` when the process was killed by a signal (as on `terminals.close`); `terminal_exit` is the last notification of a terminal. `terminals.attach` (from another connection, or after a reconnect) first sends the scrollback, the last 256 KiB of output, then live output, without gap or overlap. A connection can stream several terminals; notifications carry `terminalId`. A client too slow to keep up loses output rather than holding up the daemon.

**Input.** `terminals.write { terminalId, data }` (base64) and `terminals.resize { terminalId, cols, rows }` may be sent as JSON-RPC notifications (no `id`): no response, nothing to wait for between keystrokes, and they never wait behind other requests' work. Input sent while a terminal is `waiting` is dropped. Writing to or resizing an exited terminal is `-32002`; an unknown terminal `-32001`.

**Lifetime.** A terminal belongs to the connection that opened it: it is closed when that connection closes, on `terminals.close` (from any connection), when its run ends, or when its process exits. Closing sends SIGHUP (the terminal hangs up); a process still there after 3 s gets SIGKILL, with its process group. An exited terminal is forgotten: `terminals.list` no longer shows it and other calls get `-32001`. Terminals do not survive a daemon restart.

### Crash safety

Every transition is committed to SQLite before the side effect it leads to, and each step attempt is recorded as `running` before it starts. If the daemon stops mid-step, on restart that attempt is marked `interrupted` and the step runs again from the run's last committed state, in a new harness session (the old sessions are marked `ended`); runs waiting for approval keep waiting. Steps must therefore be idempotent: the worktree is reused if it exists, checks are re-run, and harness adapters must tolerate a repeated prompt and find an existing pull request instead of opening a second one. `runs.cancel` commits first, then stops the step (killing a running check's whole process group and removing its container).

## Agent bus

Each active run has its own agent bus ([`agentux-bus`](../crates/agentux-bus), ADR 0004), opened when its first session starts, with the `bus` section, roles and review step of the run's `agentux.yaml`, and closed when the run ends. Everything on it is a `bus_message` event and is returned by `bus.list`.

**Sessions.** Every harness session the daemon starts joins its run's bus with an identity (run, project, role, harness as vendor, session id) and gets, in ACP `session/new`, one stdio MCP server:

```text
name: agentux
command: <aux>             # this process if it is `aux daemon`, else the `aux` next to `agentuxd`, else `aux` from PATH
args: bus-stdio --socket <the daemon's socket>
env: AGENTUX_BUS_SESSION_TOKEN=<token>
```

Its first prompt starts with the session prompt from `agentux-bus` (its role and run, the tools it may call, the turn limit, the etiquette), then `---`, then the step's prompt.

**Session tokens.** 32 random bytes from `/dev/urandom`, hex-encoded, one per session, kept only in the daemon's memory. A token stands for exactly one session: the daemon takes the caller's identity from the token, never from the request, so a session cannot act as another one or reach another run. A token stops working when its session ends, when the run ends, and when the daemon restarts (sessions do not survive a restart). The token travels in the MCP server spec's environment (`AGENTUX_BUS_SESSION_TOKEN`), not on the command line, which other local users can read (`aux bus-stdio --session-token <t>` still works, for trying it by hand). The socket's `0600` mode remains the outer boundary: only the daemon's user can connect.

**The bridge.** `aux bus-stdio` serves the MCP server over its stdio and forwards each tool call to the daemon:

1. At startup, `bus.hello { sessionToken }` returns `{ identity, allowedTools, maxTurnsPerExchange }` (the MCP server lists only the allowed tools and builds its `instructions` from them), or error `-32004`.
2. Each tool call is `bus.call { sessionToken, call: { tool, arguments } }`. The result is `{ "ok": <tool result> }` or `{ "err": <refusal> }`: refusals (turn limit, unknown role, ...) are results the agent reads as tool errors, not JSON-RPC errors. `tool` and `arguments` are the MCP tool name and arguments, snake_case as agents see them.

Requests on one connection are answered in order, and `ask_human` keeps its call open for up to two minutes, so **the bridge opens one connection per call** (connect, one request, one response, close). Calls in flight at the same time never wait for each other, and the daemon needs no out-of-order responses. A connect per tool call costs little on a local socket.

**Wakes.** A message to a session or role wakes its recipients (messages to `run` do not): the daemon sends the bus's wake prompt (`[agentux bus] New message from ...`) to the session through ACP. A session in the middle of a turn (a step or another wake) gets it after that turn. A message to a role nobody plays yet is queued for the role, and the daemon starts the role's session (the role's harness and model, in the run's worktree) unless the run has ended or has no worktree yet; the new session finds the message in its mailbox. That session is then the role's session for later steps too. A wake turn's events are `session_event`s like a step's; its permission requests are `permission` requests of the run's current step. Wakes that cannot be delivered are logged (`log` event, `bus: ...`).

**The human on the bus.** `bus.post { runId, to, body, subject?, inReplyTo? }` posts a message from the human (`aux bus <run-id> --post <to> <text...>`). `to` is a `session` (`{ kind: "session", sessionId }`; `role` and `vendor` may be omitted), a `role` or `run`; it may be omitted with `inReplyTo` (a bus `messageId`), which answers that message and goes to its sender by default. `subject`, one line, becomes the message's first line (and so its `subject` in the log). Recipients are woken as for an agent's message, and a role nobody plays gets a session started for it. The human is never cut off by turn limits. Errors: an unknown run `-32001`; a finished run, an unknown session or one that left `-32002`; `to: human` or `daemon`, an unknown role, an empty body, an unknown `inReplyTo` `-32602`.

**Turn limits.** Each message uses one turn of its exchange (a message and its replies). Once an exchange has used `bus.max_turns_per_exchange` turns, further posts in it are refused with a `turn_limit` entry, so they wake nobody: two agents cannot ping-pong past the limit. The human is never cut off.

**Questions.** `ask_human` creates a `question` request (title `<role> (<harness>) asks: <question>`, detail with context and options, `options`, `sessionId`) and a `question` bus entry with its `requestId`. It does not pause the run. `requests.approve { requestId, answer }` answers it (free text or one of the options; an empty answer is `-32602`); `requests.deny` tells the agent the human declined. The agent's tool call waits up to two minutes; an answer after that arrives as a `human_answer` message in its mailbox, with a wake. Questions still pending when the run ends or the daemon restarts are cancelled.

**Restarts.** The log is in the event store, so `bus.list` and `events.subscribe { since: 0 }` return it after a restart. Mailboxes, queues and turn counts live in memory, and a run's bus reopened after a restart rebuilds them from its log: message, exchange and question ids continue after the highest ones; each exchange keeps the turns it used, so limits still hold and replies to old messages (`in_reply_to`) route to their sender. Mail is rebuilt too, as far as the log tells: reads are not logged, so a message counts as handled once a wake was issued for its recipient after it arrived. Messages still queued for a role, and messages that reached a session without a wake (sent to `run`) while it was on the bus when the daemon stopped, are queued again for the role (at most 50 per role, the newest). The sessions of the old daemon are gone: each gets a `left` entry with subject `... left the bus (agentuxd restarted)` and `queuedForRole` set to its role, so a later rebuild sees the same queue. On startup, the bus of every unfinished run with mail waiting is reopened at once, and each role with mail gets a `wake` (and a session started for it), with a prompt saying the messages were sent before the restart. A message whose wake had been issued but whose turn never ran (the daemon stopped first) is not delivered again.

**Models.** A role's `model` is selected over ACP when the harness offers it: ACP has no model field in `session/new`, but agents that let clients pick one list a session config option of category `model`; the daemon sets it with `session/set_config_option` (matching the option's value or name). When the harness offers no such option or not that model, the session starts with its default and gets a `system` message saying so.

## Helper questions

`helper.ask { harness, question, context?, model? }` asks the user's own harness one question for the cockpit's helper (the penguin, [ADR 0017](https://github.com/agentux-os/agentux/blob/main/docs/adr/0017-help-mascot.md)) and returns `{ harness, answer }`. It belongs to no run: nothing is stored, no event is emitted, and it does not show up in `runs.list` or the inbox.

- **The session.** A fresh harness session (`harness` as in `agentux.yaml`, with `model` selected as for a role) started in a new, empty directory with mode `0700` under `$XDG_RUNTIME_DIR` (else the temp directory), removed afterwards. It gets no MCP server, so no agent bus. The harness uses the login the user set up for it, like any session.
- **No tools.** Every permission request is denied. When the agent starts a tool call anyway (some harnesses run tools without asking, depending on their own settings), the daemon cancels the turn at that event and returns an error instead of the answer. A harness that runs a tool without asking can still run that one call before the cancel lands; the empty working directory and the prompt (which carries no file contents and no secrets) limit what it could see.
- **The prompt.** The daemon's helper rules (answer about AgentUX only and politely decline anything else, such as KDE or Linux desktop questions; use no tools; say so when the material does not answer it; be brief, in the question's language), then `context` as reference material, then `question`. `context` is whatever the client sends: the cockpit sends excerpts of the AgentUX documentation and a summary of the current view (route and the selected run's title, status and step), never file contents. It is cut at 48 KiB.
- **Limits.** The answer is the agent's reply text of that one turn. A turn that takes longer than 180 s (harness start included) is an error.
- **Errors.** An empty `question` or one over 2000 characters, or an empty `harness`, is `-32602`. A harness that cannot start, refuses, sends nothing, tries a tool or times out is `-32603` with the reason. With `--fake-agents` the answer is a scripted line that starts with `(fake agent)`.

A slow `helper.ask` holds up later requests on its connection (see [Transport](#transport)); clients use a connection of their own for it.

## CLI

`aux` is a client of this API; its text output is for people and may change. One command has a stable, machine-readable form.

### `aux ps --json`

Prints one JSON object on stdout, one line, for status bars (the agentux-os Waybar module) and scripts. It is built from `projects.list`, `runs.list` and `requests.list { pending: true }`. Like `aux ps`, it lists unfinished runs (`running`, `waiting`), and every run with `--all`.

```json
{
  "daemon": "up",
  "runs": [
    {
      "id": "k3x9q2ab",
      "projectId": "p7c1d0e4",
      "project": { "name": "api", "path": "/home/me/src/api" },
      "title": "Add a health endpoint",
      "status": "waiting",
      "step": "plan",
      "stepIndex": 0,
      "activity": "waiting for plan approval",
      "gate": { "attempt": 0, "max": 3 },
      "review": { "round": 0, "max": 2 },
      "costUsd": 0.42,
      "updatedAt": 1791849600000
    }
  ],
  "pending": [
    {
      "id": "r5f2a9c1",
      "runId": "k3x9q2ab",
      "kind": "plan",
      "title": "Approve the plan for \"Add a health endpoint\"",
      "sessionId": "s8d3e6f0",
      "options": ["yes", "no"],
      "createdAt": 1791849600000
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `daemon` | `"up"`; `"down"` when the daemon cannot be reached (then it is the only field) |
| `runs` | Runs, newest first; empty when idle |
| `runs[].id`, `projectId`, `title`, `status`, `step`, `stepIndex`, `activity`, `updatedAt` | As in `Run`: `status` is `running`, `waiting`, `done`, `failed` or `cancelled`; `step` a step kind; `updatedAt` in milliseconds since the epoch |
| `runs[].project` | `{ name, path }` of the run's project (left out if the project is unknown) |
| `runs[].gate` | `{ attempt, max }`: attempts of the most recent gate and its limit (`gateAttempt`, `gateMaxAttempts`); only when the run's pipeline has a gate step |
| `runs[].review` | `{ round, max }`: rounds of the most recent review and its limit; only when the pipeline has a review step |
| `runs[].costUsd` | What the run's sessions have cost so far; only when a harness reported cost |
| `pending` | Requests waiting for the human (approvals inbox), newest first; not affected by `--all` |
| `pending[].id`, `runId`, `kind`, `title`, `createdAt` | As in `PermissionRequest`: `kind` is `plan`, `step`, `permission`, `budget` or `question`; `id` is what `aux approve`, `aux deny` and `aux answer` take |
| `pending[].sessionId` | The session that asked (`permission` and `question` requests) |
| `pending[].options` | Suggested answers of a `question` request, when it has any |

Optional fields are left out, never `null`. New fields may be added; existing ones keep their name and meaning.

Exit codes:

| Code | stdout |
|---|---|
| `0` | The document above, `"daemon": "up"` |
| `3` | `{"daemon":"down"}`: no socket, or nothing accepts connections on it (the reason goes to stderr) |
| `1` | Nothing; the daemon answered with an error (the message goes to stderr) |
