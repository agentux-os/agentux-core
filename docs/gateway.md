# Node gateway

`agentux-gateway` (in `aux`: `aux node ...`) connects one AgentUX node to [AgentUX Command](https://github.com/agentux-os/agentux-command), the remote control plane, over the [AgentUX Protocol](https://github.com/agentux-os/agentux-protocol) (AUXP v1). It reads everything from the local `agentuxd` API and never exposes the daemon's socket, its database, worktrees or credentials.

```sh
AGENTUX_NODE_TOKEN=... aux node enroll https://command.example.org --organization org-acme
aux node status
aux node gateway        # foreground; on AgentUX: systemctl --user enable --now agentux-gateway
aux node leave
```

`enroll` writes `~/.config/agentux/node.json` (mode 0600): Command's URL (https, or http to this machine only), the organization, the node id (`node-<hostname>` by default) and the node token. The token is read from `$AGENTUX_NODE_TOKEN` or stdin, never from the command line.

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

Commands from Command (`run.start`, `request.resolve`, `preferences.set`, ...) come with the next phase, over a WebSocket the node opens, checked against the node's policy in `/etc/agentux/node-policy.yaml` (see the protocol's "Node policy").
