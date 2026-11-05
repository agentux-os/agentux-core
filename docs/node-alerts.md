# Node alerts

## Ownership and scope

Core owns the identity and lifecycle of actionable conditions observed on one
AgentUX installation. This applies to standalone installations and to nodes
enrolled with AgentUX Command. The Cockpit can therefore show the local source
of truth without requiring a gateway or a Command connection.

The node-alert contract assigns enrolled alert relay and authenticated
organization/node scoping to the gateway. Runtime implementation remains
tracked in Core #102.

When enrolled, the gateway relays sequenced Core-owned alert events and binds
exported state to the authenticated organization and node identity. Command
projects the received state; it does not create, resolve, or rewrite Core alert
identities or statuses. Command separately derives fleet presence (`online`,
`degraded`, and `offline`) from heartbeat freshness. Fleet presence is not a
`node_alert`.

Organization identity is optional for a local standalone alert. It is supplied
by the authenticated gateway when the alert is exported from an enrolled node.

## Alert record

An alert record contains:

- `alertId`: stable identity for the producer and condition key;
- `producer` and `code`: the local source and condition type;
- `conditionKey`: stable key identifying the affected local condition;
- `summary`: safe human-readable description, with no secrets;
- `firstObservedAt` and `lastObservedAt`: timestamps for the first and most
  recent observation;
- `status`: `active` or `resolved`;
- `lastLocalEventSequence`: the local event sequence associated with the
  latest state.

The stable identity is derived from the producer and condition key. Repeated
observations of the same condition update the same alert and advance its last
observed time and local event sequence. Identity and payload must not contain
credentials, tokens, secret values, or unrestricted logs. Optional details
must be bounded and redacted before persistence or publication.

## Lifecycle

A producer opens an alert for an actionable local condition and keeps it
`active` while that condition remains present. The producer resolves it to
`resolved` only after observing recovery. Resolution records source-observed
recovery; it is not an operator acknowledgement.

Removal is reserved for retention or correction. It is represented by a
tombstone carrying the sequence that removed the entry, so consumers can apply
the removal in order. Removal is not a third alert status, and entries must
not be silently deleted when a consumer may need to replay changes.

The following JSON objects illustrate the local current-state record and its
removal tombstone. Organization and node identity are intentionally absent
from the local record; the gateway supplies authenticated scope on export.

### Active

```json
{
  "entryId": "node_alert:harness-claude-unavailable",
  "alertId": "harness-claude-unavailable",
  "producer": "harness-monitor",
  "code": "harness_unavailable",
  "conditionKey": "harness:claude",
  "summary": "Claude harness is unavailable.",
  "firstObservedAt": "2026-11-05T12:00:00Z",
  "lastObservedAt": "2026-11-05T12:00:00Z",
  "status": "active",
  "lastLocalEventSequence": 41
}
```

### Resolved

```json
{
  "entryId": "node_alert:harness-claude-unavailable",
  "alertId": "harness-claude-unavailable",
  "producer": "harness-monitor",
  "code": "harness_unavailable",
  "conditionKey": "harness:claude",
  "summary": "Claude harness is available again.",
  "firstObservedAt": "2026-11-05T12:00:00Z",
  "lastObservedAt": "2026-11-05T12:04:00Z",
  "status": "resolved",
  "lastLocalEventSequence": 44
}
```

### Removed

```json
{
  "entryId": "node_alert:harness-claude-unavailable",
  "removed": true,
  "removalSequence": 52
}
```

These examples remain local domain records and tombstones. Future export must
follow the authoritative AUXP [node snapshot schema](https://github.com/agentux-os/agentux-protocol/blob/main/schemas/v1/node-snapshot.schema.json)
and [node snapshot fixture](https://github.com/agentux-os/agentux-protocol/blob/main/fixtures/v1/node-snapshot.json)
wire shapes, including `state.currentState`, `kind: removed`, and
`removedAtSequence`.

## Producers and snapshot gate

Each concrete producer must document the actionable condition it observes, its
stable condition key, when it opens or updates an alert, and the recovery rule
that resolves it. Do not create alerts merely to populate a snapshot. Alert
state, event publication, and local sequence advancement must be committed
atomically; snapshot capture must read the records and event head from the same
SQLite read transaction.

The `node.snapshot_checkpoint.v1` capability remains disabled until every
required projection has a durable, sequence-bound source and all such sources
share the checkpoint high-water barrier. Adding a durable alert table alone is
not sufficient; node and harness sources must also participate at that same
barrier.
