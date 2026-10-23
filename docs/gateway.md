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
| `harness.status` | When the agent CLIs change, checked every 5 minutes and after limit changes: installed, signed in, version, spend, plan windows, limits |
| `harness.limits_changed` | When the usage limits in force change |
| `approval.requested` | A run waits for the human: kind, title, detail (cut at 4000 characters), options |
| `run.updated` / `run.completed` / `run.failed` | A run changed: state, step, roles and models, activity, cost and budget, branch, pull request, error |

Messages are POSTed to `<command>/v1/protocol/messages` with `Authorization: Bearer <node token>`. Events carry a `sequence` that keeps growing across restarts (`~/.local/state/agentux/gateway.json`) and an `eventId` of `<nodeId>-<sequence>`. While Command is unreachable they wait in order (up to 5000, oldest dropped first) and go out when it answers again; runs go on regardless.

## Commands from Command

The gateway also keeps a command connection open: a WebSocket to `<command>/v1/protocol/connect?nodeId=<id>` with the node token, reconnecting with backoff. Each command Command sends is checked, then run against the local agentuxd, held, or refused:

- It must be for this node (`nodeId`), carry a `commandId`, and carry no credentials.
- The node policy, `/etc/agentux/node-policy.yaml`, decides: `allow` runs it, `confirm` holds it, `deny` refuses it. Methods the policy does not know are refused. `aux node status` shows the policy in force.

```yaml
commands:
  run.start: confirm        # default
  preferences.set: confirm  # default
  limits.clear: deny
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

Either way a `command.completed` event reports the outcome under the original `commandId`. A command sent again with the same `commandId` gets the first answer.
