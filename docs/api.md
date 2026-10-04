# agentuxd API

`agentuxd` serves one local API, used by `aux` and (later) the cockpit. Types and a Rust client live in [`agentux-api`](../crates/agentux-api); the daemon, `aux` and the cockpit's Tauri backend all depend on that crate (ADR 0006).

## Transport

- **Unix domain socket** at `$XDG_RUNTIME_DIR/agentux/agentuxd.sock`. `$AGENTUX_SOCKET`, or `--socket` on `agentuxd`/`aux`, overrides it. The directory is created with mode `0700` and the socket with `0600`: only the user who runs the daemon can talk to it. Nothing listens on the network.
- **Newline-delimited JSON-RPC 2.0.** Each line is one JSON object: a request, a response or a notification. Requests on one connection are answered in order. A request without `id` is a notification and gets no response.
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
| `runs.start` | `{ projectId, prompt?, issue?, title? }` — `prompt` or `issue` required | `Run` |
| `runs.list` | `{ projectId? }` | `Run[]`, newest first |
| `runs.get` | `{ runId }` | `{ run: Run, attempts: StepAttempt[], requests: PermissionRequest[] }` |
| `runs.cancel` | `{ runId }` | `Run` |
| `requests.list` | `{ pending? }` — `true` for the approvals inbox | `PermissionRequest[]`, newest first |
| `requests.approve` | `{ requestId, answer? }` | `PermissionRequest` |
| `requests.deny` | `{ requestId, answer? }` | `PermissionRequest` (the run fails) |
| `events.subscribe` | `{ runId?, since? }` | `{ seq }`, then `event` notifications |

`runs.start` reads the project's `agentux.yaml` at that moment and stores it with the run; editing the file later does not affect a run in progress. Without the file, the built-in default pipeline applies.

### Errors

Standard JSON-RPC codes (`-32700` parse error, `-32600` invalid request, `-32601` unknown method, `-32602` invalid params, `-32603` internal), plus:

| Code | Meaning |
|---|---|
| `-32001` | Not found: no such project, run or request |
| `-32002` | Conflict: e.g. approving a request that is no longer pending, cancelling a finished run |
| `-32003` | Invalid project: not a git repository, or its `agentux.yaml` is invalid |

## Events

```json
{"jsonrpc":"2.0","method":"event","params":{"seq":42,"at":1790000000000,"runId":"3f9a0c12","kind":"run","run":{...}}}
```

Every state change is stored as an event in the same transaction as the change, then pushed to subscribers after the commit. `seq` increases by one per event. `events.subscribe` with `since: N` first replays stored events with `seq > N` (`since: 0` replays everything), then continues live, without gaps or duplicates; without `since`, only new events are sent. `runId` limits the stream to one run. A client that reconnects passes the last `seq` it saw.

| `kind` | Payload | When |
|---|---|---|
| `project` | `project: Project` | A project was registered |
| `run` | `run: Run` | Any change to a run (full snapshot) |
| `request` | `request: PermissionRequest` | An approval was requested or resolved |
| `attempt` | `attempt: StepAttempt` | A step started or finished |
| `log` | `text` | Check output, agent summaries, notes such as a resume after restart |

Snapshot events carry the whole object, so a client can keep a map keyed by id and replace entries.

## Types

```ts
type StepKind = "plan" | "implement" | "gate" | "review" | "pull_request" | "custom";
type RunStatus = "running" | "waiting" | "done" | "failed" | "cancelled"; // waiting = paused on an approval

interface Project { id: string; name: string; path: string; createdAt: number }

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
  checks: { name: string; command: string; status: "pending" | "running" | "passed" | "failed" }[];
  gateAttempt: number; gateMaxAttempts: number;   // most recent gate
  reviewRound: number; reviewMaxRounds: number;   // most recent review
  budgetUsd: number | null;  // stored, not enforced yet
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
  kind: "plan" | "step";   // plan = approve the plan; step = any other approve: true step
  runId: string; projectId: string; stepIndex: number; step: StepKind;
  title: string; detail: string;
  status: "pending" | "approved" | "denied" | "cancelled";
  answer: string | null;
  createdAt: number; resolvedAt: number | null;
}
```

## Run lifecycle

1. **Setup.** The daemon creates branch `aux/<run-id>` and its worktree (`<repo>.worktrees/<run-id>`) from the project's `HEAD`.
2. **Steps**, in pipeline order:
   - `plan`, `implement`, `review`, `custom` call an agent through the harness adapter. A review may request changes.
   - `gate` runs each listed check with `sh -c` in the worktree; all checks run, and the failures' output becomes feedback.
   - `pull_request` opens (or finds) the pull request for the branch.
3. **Loops.** A failing gate goes back to its `on_fail` step with the failure output, up to `max_attempts` consecutive gate attempts; a passing gate resets the count. A review requesting changes goes back to `on_changes_requested` with the comments, up to `max_rounds` reviews per run. When a limit is reached the run fails. A gate without `on_fail` fails the run on its first failure.
4. **Approvals.** `approve: true` on an agent step pauses the run after the step (e.g. to approve the plan); on `pull_request` it pauses before opening it. The run is `waiting` until `requests.approve` (continue) or `requests.deny` (fail).
5. **Budget.** `budget.max_usd_per_run` is stored on the run (`budgetUsd`) but not enforced: there is no usage data until harness adapters report it.

### Crash safety

Every transition is committed to SQLite before the side effect it leads to, and each step attempt is recorded as `running` before it starts. If the daemon stops mid-step, on restart that attempt is marked `interrupted` and the step runs again from the run's last committed state; runs waiting for approval keep waiting. Steps must therefore be idempotent: the worktree is reused if it exists, checks are re-run, and harness adapters must tolerate a repeated prompt and find an existing pull request instead of opening a second one. `runs.cancel` commits first, then stops the step (killing a running check).
