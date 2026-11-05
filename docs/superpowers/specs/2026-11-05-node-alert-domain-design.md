# Node alert domain design

## Intent

Define `node_alert` as a Core-owned projection of actionable conditions on one
AgentUX installation. The same local alert domain is available in standalone
installations and in installations enrolled with Command. This gives the
Cockpit a local source of truth without making remote connectivity a runtime
requirement.

## Goals

- Keep alert creation, updates, resolution, and retention with Core, the service
  that observes local machine and harness conditions.
- Persist complete alert state and its publication sequence so snapshot capture
  and event replay agree.
- When enrolled, publish the same authoritative local state to Command with the
  authenticated organization and node identity supplied by the gateway.
- Keep Command's `online`, `degraded`, and `offline` fleet presence derived from
  heartbeat freshness. Presence does not create a second `node_alert`.
- Keep standalone operation useful without an organization, gateway, or Command.

## Non-goals

- Define fleet-wide incidents, heartbeat alerts, policy alerts, or Command-side
  alert ownership.
- Turn every failed run or tool call into an alert; those have their own
  lifecycle events.
- Add acknowledgement, notification routing, severity policy, or alert rules
  before a concrete local producer requires them.
- Advertise `node.snapshot_checkpoint.v1` until all required projections,
  including node and harness, have durable sources at the same sequence
  boundary.

## Ownership and lifecycle

- Core owns the alert identity and lifecycle. A producer opens an alert only for
  an actionable local condition; it updates the same record while the condition
  remains active and resolves it when that condition clears.
- The identifier is stable for a producer and condition key, so repeated
  observations update one alert instead of creating duplicates. It must not
  contain credentials or other secret material.
- The first contract uses `active` and `resolved` states. Resolution is a
  source-observed transition, not an operator acknowledgement. Removing a
  record is reserved for retention or correction and is represented by a
  sequence-bound tombstone.
- Alert state, event publication, and local event sequence advancement must be
  committed atomically by Core. A snapshot reads alert records and the event
  head from one SQLite read transaction. The gateway relays sequenced events;
  it does not infer alerts from heartbeat or harness polling.
- Standalone records are local to the installation and remain visible in the
  Cockpit. On an enrolled installation, the gateway binds exported state to its
  authenticated organization/node envelope. Organization identity is not
  required to create or inspect a local alert.
- Command stores/projects the received alert state. It may show presence
  separately, but it does not open, resolve, or rewrite Core-owned alert IDs or
  statuses.

## Data and transport

The AUXP `node_alert` state includes `alertId` and `status`; its stable
`entryId` is `node_alert:<alertId>`. The Core record also needs a producer/code,
condition key, safe human-readable summary, first/last observed timestamps,
and the last local event sequence. Optional details must be bounded and
redacted before persistence or publication. The gateway adds the authenticated
organization and node scope when materializing the remote snapshot.

`active` and `resolved` records are current state. Retention/removal produces a
tombstone with the sequence that removed the entry. Snapshot support remains
blocked until the gateway's node and harness sources join the same captured
high-water barrier; a durable alert table alone does not satisfy the checkpoint
contract.

## Approaches considered

1. **Core-owned local alerts (selected):** works identically standalone and
   enrolled; Command receives a projection when connected. This preserves one
   authority and separates machine health from fleet presence.
2. **Command-owned fleet alerts:** suitable for heartbeat and organization-wide
   conditions, but cannot serve standalone and would duplicate the locally
   observed condition in Core.
3. **Mixed ownership:** permits both local and fleet alerts, but needs producer
   namespaces, conflict rules, and distinct resolution authorities. Defer until
   an actual cross-node incident use case exists.

## Constraints and implementation choices

- Do not invent alert producers just to populate the snapshot. The first
  producer must name an actionable condition, its stable condition key, and its
  resolve rule.
- Keep alert payloads independent of user secrets, harness credentials, and
  unrestricted logs.
- Preserve existing AUXP projection identity and sequence rules; if the
  lifecycle needs fields not represented in AUXP, update Protocol before
  exporting them.
- Keep the snapshot capability disabled while any required source is
  unavailable.

## Open implementation detail

The initial producer and its condition taxonomy are intentionally not selected
here. Before adding one, choose a concrete local condition from the daemon or
harness lifecycle and document when it opens, updates, resolves, and expires.
