# Task activity contract

**Decision status: Proposed** (Core #117)

## Decision

AUXP task activity represents a logical unit of agent work with a stable
identity and an explicit lifecycle. Core is responsible for validating,
persisting, sequencing, and replaying task activity on a node. Command consumes
that source; it does not invent tasks from run steps or presentation data.

There are two valid producers, with distinct ownership:

1. **Core-orchestrated agent work.** Core may publish a task for an agent
   execution it starts. Before publishing, Core must persist a stable `taskId`
   for that logical work item and bind it to its run, trusted organization
   scope (when enrolled), role, and concrete agent identity. The agent identity
   is allocated and persisted by Core for the assigned actor; it is not
   synthesized from `roleId` or `sessionId`. A retry keeps the same `taskId`,
   increments `attempt`, and records the session used by that attempt. A
   deliberately repeated loop iteration is a new logical task with a new ID;
   retrying one iteration is not. Core may publish a loop task only after the
   orchestration layer persists an explicit iteration boundary and identity.
2. **Runtime-created or delegated work.** A harness/runtime may publish a task
   only when it supplies a stable task ID and typed creation/status events,
   including `attempt`. Delegation uses the protocol's stable parent task and
   delegation IDs. Core validates that these IDs belong to the current run
   before persisting them. The event envelope has no `organizationId`; Core
   derives trusted scope from the persisted run origin and, for enrolled runs,
   the authenticated gateway enrollment. It never accepts organization scope
   asserted by a runtime event. Local-only runs have no exportable organization
   scope. ACP v1 Plan snapshots do not meet this requirement.

The two producers use the same AUXP task lifecycle, but Core must retain source
ownership internally so it can reconcile and recover each inventory. IDs must
be globally unique within the `(organizationId, runId)` task namespace; an
incoming duplicate ID from a different source is rejected unless it is an
explicit update to the already-bound task.

`agentId` identifies the concrete agent execution actor and `roleId` its
assigned role. For Core-owned work, Core allocates and durably stores the
actor ID with the task before emitting activity; it remains stable across
retries. For runtime-owned work, the runtime supplies its actor ID. `sessionId`
identifies the corresponding harness session when one exists and may change
between attempts. A role name or session ID must not be substituted for
`agentId`. Every task lifecycle event accepted by Core includes a deterministic
`attempt` beginning at 1; the task ID remains unchanged. Protocol #12 currently
allows `attempt` to be omitted in its general activity schema, so Core's task
ingestion rule is intentionally stricter and rejects task lifecycle events
without it. Tightening the shared protocol requirement is a separate
compatibility decision; this proposal does not silently change the schema.
For runtime-originated events, Core also binds the event's producer, `agentId`,
`roleId`, and optional `sessionId` to the authenticated local harness/session
that belongs to the run; an unbound actor identity is rejected.

## Lifecycle and ownership boundaries

- Creation starts in `pending` or `running`, as reported by the authoritative
  producer. Status changes use the protocol's `TaskStatus` values.
- A retry is another attempt of the same logical task, not a new task. The
  producer records a recoverable attempt failure as `blocked` while retry is
  pending, with the current attempt number. Starting the next attempt changes
  the status to `running` and increments `attempt`. The task's `failed` status
  is terminal and is represented by exactly one `task_failed` event, emitted
  only when the producer gives up. A producer must not also emit
  `task_status_changed` with `status: failed`; that avoids duplicate terminal
  failure notifications. Success is represented by exactly one
  `task_completed` event, not a `task_status_changed` with `status: completed`.
  Cancellation is represented by `task_status_changed` with `status: cancelled`
  because Protocol #12 has no separate cancellation event. Consumers can
  therefore distinguish retry scheduling from terminal failure.
- A loop iteration that represents new work gets a new task ID and may name
  its parent task. The orchestration layer must persist an explicit iteration
  identity before publication; `stepIndex`, attempt count, or event ordering
  alone cannot distinguish a loop iteration from a retry or restart. Until
  that producer support exists, Core must not publish loop iterations as
  separate tasks.
- Approval pauses and resumes the existing task; it does not create a task.
  `blocked` is a deliberately generic task status: it means the task is not
  currently executing, but does not encode whether it awaits approval or a
  retry. Approval state is obtained from the separate typed approval flow.
  Consumers must not use free-text `summary` as a control signal or infer the
  blocked reason. Core may retain the precise local reason for recovery. A
  machine-readable blocked reason requires a compatible Protocol evolution.
- `completed`, `failed`, and `cancelled` are terminal for one logical task.
  A terminal task cannot silently restart. A recoverable attempt failure is
  represented as `blocked` while a retry is scheduled, never as task status
  `failed`.
- Pipeline `gate` and `pull_request` steps are orchestration or artifact work,
  not agent tasks. They do not emit `task_*` events unless a future contract
  explicitly models them as a separate task class.
- Tool calls remain tool activity. A tool name, plan entry, narration, or
  `StepAttempt` without the persisted task/agent/session binding is not enough
  to create a task.

Core records each activity with a stable activity/event ID and its local
sequence in the same durable transaction as the task state. Recovery replays
the same event IDs and sequence; it does not re-emit creation with a new ID.
For every write, Core resolves the run from the persisted run ID. For
Command-originated runs, it takes `organizationId` and `missionId` from the
durable `Run.origin` (`RunOrigin`). Core compares the origin's
`organizationId` with the authenticated gateway enrollment and requires each
activity's `missionId` to match the persisted `RunOrigin.missionId`. The
enrollment does not supply mission identity. Core ignores organization
identity in untrusted event data. For local standalone runs, `Run.origin` is
absent and the task remains local, not eligible for an organization-scoped
snapshot.

## Implementation gate

Task remains `unavailable` in `snapshots.source_capture` until Core has a
complete durable inventory for every enabled producer, including removals,
restart recovery, sequence-bound tombstones, and organization isolation. An
empty database is not proof of an empty task inventory. ACP Plan snapshots
remain excluded under the [plan snapshot mapping](superpowers/specs/2026-11-06-core-86-acp-plan-mapping.md).

The implementation must add durable task identity, attempt/session/agent
bindings, and explicit loop iteration IDs for Core-owned agent work, plus a
validated ingestion path for the typed runtime task events already defined by
Protocol #12. If the producer cannot provide the required task and actor
bindings, or a required field cannot be represented under Protocol #12, that
producer must remain disabled until an additive, capability-negotiated
protocol change is agreed; Core must not infer a substitute. Tests must cover
creation, transitions, approvals, recoverable and terminal failure, retry and
loop identity, replay after restart, source collisions, organization
isolation, and complete inventory/removal capture before the unavailable
marker is removed. Runtime-created Tasks remain disabled until a runtime emits
the typed events and Core can ingest, persist, replay, and reconcile their
complete inventory; Core-owned tasks alone do not establish completeness for
the combined task inventory.

See [Core #117](https://github.com/agentux-os/agentux-core/issues/117) for the
decision review and [Core #104](https://github.com/agentux-os/agentux-core/issues/104)
for producer integration and the snapshot gate.
