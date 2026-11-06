# Node gateway

`agentux-gateway` (in `aux`: `aux node ...`) connects one AgentUX node to [AgentUX Command](https://github.com/agentux-os/agentux-command), the remote control plane, over the [AgentUX Protocol](https://github.com/agentux-os/agentux-protocol) (AUXP v1). It reads everything from the local `agentuxd` API and never exposes the daemon's socket, its database, worktrees or credentials.

```sh
AGENTUX_NODE_TOKEN=... aux node enroll https://command.example.org --organization org-acme
aux node status
aux node gateway        # foreground; on AgentUX: systemctl --user enable --now agentux-gateway
aux node leave
```

`enroll` takes an **enrollment token** issued by the Command server (`auxe1_<instanceId>_...`, single use, expiring), read from `$AGENTUX_NODE_TOKEN` or stdin, never from the command line. Before sending it, the node asks the server for its instance id (`GET /v1/instance`) and stops when it is not the server the token names, so a token never goes to another Command server. It then trades the token at `/v1/enroll` for its own credential, its node id (unique on that server) and its organization, and writes `~/.config/agentux/node.json` (mode 0600) with them and the server's **instance id, pinned**:

- every answer that names the server, and the command connection (`X-AgentUX-Command-Instance`), must carry that instance id; otherwise the gateway stops (a DNS name moved to another Command server takes no node with it);
- every command must carry it (`commandInstanceId`) and the node's organization, or it is refused.

Two Command servers in one institution therefore never share or take nodes. A node joins a server only with a token from it; there is no discovery. A static token set on the server (`AGENTUX_COMMAND_NODE_TOKENS`) still works with `--organization`, without pinning.

## What it sends (read-only phase)

| Message | When |
|---|---|
| `node.hello` | On start: capabilities, AgentUX version, the last event sequence it handed out |
| `node.heartbeat` | Every 30 s: `status`, `capacity.activeRuns`, `capacity.waitingForHuman` |
| `node.status_changed` | When the gateway connects to local `agentuxd` (`ready`) or shuts down gracefully (`draining`) |
| `harness.status` | When the agent CLIs change, checked every 5 minutes and after limit changes: installed, signed in, version, spend, plan windows, limits |
| `harness.limits_changed` | When the usage limits in force change |
| `approval.requested` | A run waits for the human: kind, title, detail (cut at 4000 characters), options |
| `run.updated` / `run.completed` / `run.failed` | A run changed: state, step, roles and models, activity, cost and budget, branch, pull request, error, and once a session opened, `agentIsolation` (`mode` `none` or `bubblewrap`, `network`, and `source`: `project`, `node-policy`, `command` or `default`) |
| `execution.activity` | When an ACP session reports a tool call starting or reaching a terminal status: stable tool-call ID, session/role/run correlation and a generic safe summary. Tool titles, inputs, outputs, plans and reasoning are not sent. |
| `node.alert` | Relays a Core-owned alert state or removal when a local alert producer exists |

Messages are POSTed to `<command>/v1/protocol/messages` with `Authorization: Bearer <node token>`. Events carry a `sequence` that keeps growing across restarts (`~/.local/state/agentux/gateway.json`) and an `eventId` of `<nodeId>-<sequence>`. The gateway saves the pending outbox and its local event-log cursor atomically. After a restart it retries the same queued messages and resumes the daemon event stream from that cursor, preserving event IDs for retries. When `node.event_replay.v1` is accepted, sequenced events remain in the outbox until Command acknowledges a contiguous durable cursor; `node.resume` replays retained events in order and reports when the requested cursor expired. Without that capability, the gateway keeps legacy send-and-drain behavior. The outbox holds up to 5000 messages and drops the oldest first when full; runs go on regardless.

Node status and harness inventory changes are first persisted by Core with a local event sequence, then relayed through the same ordered outbox as other local events. The gateway reports `ready` only after it connects and subscribes to local `agentuxd`; a graceful gateway stop records `draining`. Its capacity object is currently empty. Alert events use the same path, but there is no concrete alert producer yet. The local capture-to-gateway sequence binding is persisted in Core SQLite; the gateway JSON file continues to hold its event cursor and retry outbox.

`execution.activity.v1` is offered in `node.hello` and used only when Command returns it in `acceptedCapabilities`. The current Core emits `tool_started`, `tool_completed` and `tool_failed` when the ACP adapter reports the corresponding status. It does not infer task trees, delegation, progress percentages or artifacts from provider-specific data. `node.event_replay.v1` is negotiated the same way.

Core's proposed task ownership, identity, retry, and snapshot completeness rules
are documented in [task-activity.md](task-activity.md). No task events are
emitted until the authoritative producer and durable recovery path are
implemented.

Generic artifact publication and retrieval are not implemented. The proposed
Core ownership and content lifecycle are documented in
[artifacts.md](artifacts.md) and the
[design](superpowers/specs/2026-11-06-core-118-generic-artifacts-design.md);
the gateway must not advertise `artifact://` references as remotely
downloadable until authenticated dereference and removal/recovery are defined.

The node-alert contract assigns enrolled alert relay and authenticated organization/node scoping to the gateway. The relay is implemented; selecting and connecting a concrete local alert producer remains tracked in Core #102. See the [node alert contract](node-alerts.md). The gateway does not derive alerts from heartbeat or harness polling; Command derives fleet presence separately.

## Commands from Command

The gateway also keeps a command connection open: a WebSocket to `<command>/v1/protocol/connect?nodeId=<id>` with the node token, reconnecting with backoff. Each command Command sends is checked, then run against the local agentuxd, held, or refused:

- It must be for this node (`nodeId`), carry a `commandId`, and carry no credentials.
- The node policy, `/etc/agentux/node-policy.yaml`, decides: `allow` runs it, `confirm` holds it, `deny` refuses it. Methods the policy does not know are refused. `aux node status` shows the policy in force.

```yaml
commands:
  run.start: confirm        # default
  preferences.set: confirm  # default
  limits.clear: deny
agents:
  isolation: required       # every agent session in the sandbox; default optional
```

| Method | Default | What it does on the node |
|---|---|---|
| `run.snapshot`, `harness.list`, `harness.test` | allow | `runs.get`, `harnesses.list`, `harnesses.test` |
| `run.cancel`, `request.resolve`, `bus.post`, `limits.clear` | allow | cancel a run, approve or deny a request (`resolution`, `answer`), post on a run's bus (`message.body`, `message.to`), lift a limit |
| `run.pause`, `run.resume` | allow | not implemented by agentuxd yet: `-32601` |
| `run.start`, `preferences.set` | confirm | start a run on the project whose origin is `repository` (cloned into `~/src` when the node has none), with optional `agents` and `models` per role; with `missionId` and `runId` (and `definitionVersion`), the run belongs to that mission run of Command, and its `run.*` events carry `missionId`, `correlationId` (Command's run id), `definitionVersion` and `origin` (ADR 0015); replace the agent preferences |

A held command answers `-32010` and sends `command.denied` (`awaiting_local_confirmation`); a refused one answers `-32011` (`denied_by_policy`). Held commands wait in `~/.local/state/agentux/gateway-pending.json`:

```sh
aux node pending              # what Command asked for
aux node confirm <commandId>  # run it (within a few seconds)
aux node reject <commandId>
```

The gateway durably records safe lifecycle states (`received`, confirmation or denial, `executing`, and the outcome) in Core SQLite, scoped to the enrolled organization. Projection rows contain no command parameters, credentials, or arbitrary results. `run.start` records its stable Command run ID as an operation idempotency key; after an uncertain outcome, a retry with that same key can recover the existing Core run, while a changed key is rejected. Other advanced commands are not automatically executed again after restart and require local reconciliation. The local pending-confirmation file still contains the parameters needed to execute a command after an operator accepts it; completed results remain there until Core durably commits `command.completed` to its event outbox.

This producer does not make `command` a sourced snapshot family yet. Snapshot capture continues to list it as unavailable until restart recovery can prove the authoritative inventory and preserve completion delivery across a crash.
