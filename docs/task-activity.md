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

## Lifecycle and ownership boundaries

- Creation starts in `pending` or `running`, as reported by the authoritative
  producer. Status changes use the protocol's `TaskStatus` values.
- A retry is another attempt of the same logical task, not a new task. The
  producer records a recoverable attempt failure as `blocked` while retry is
  pending, with the current attempt number. Starting the next attempt changes
  the status to `running` and increments `attempt`. The task's `failed` status
  and `task_failed` event are terminal and are emitted only when the producer
  gives up. Consumers can therefore distinguish retry scheduling from final
  failure without treating `failed` as both terminal and retryable.
- A loop iteration that represents new work gets a new task ID and may name
  its parent task. The orchestration layer must persist an explicit iteration
  identity before publication; `stepIndex`, attempt count, or event ordering
  alone cannot distinguish a loop iteration from a retry or restart. Until
  that producer support exists, Core must not publish loop iterations as
  separate tasks.
- Approval pauses and resumes the existing task; it does not create a task.
  The producer reports `blocked` while waiting when that state is known.
- `completed`, `failed`, and `cancelled` are terminal for one logical task.
  A retry after a non-terminal failed attempt may return the task to `running`
  with the next attempt number; a terminal task cannot silently restart.
- Pipeline `gate` and `pull_request` steps are orchestration or artifact work,
  not agent tasks. They do not emit `task_*` events unless a future contract
  explicitly models them as a separate task class.
- Tool calls remain tool activity. A tool name, plan entry, narration, or
  `StepAttempt` without the persisted task/agent/session binding is not enough
  to create a task.

Core records each activity with a stable activity/event ID and its local
sequence in the same durable transaction as the task state. Recovery replays
the same event IDs and sequence; it does not re-emit creation with a new ID.
For every write, Core resolves the run from the persisted run ID and derives
organization scope from that run's stored origin plus the authenticated
enrollment context; it ignores organization identity in untrusted event data.
A local-only run cannot be exported into an organization-scoped task snapshot.

## Implementation gate

Task remains `unavailable` in `snapshots.source_capture` until Core has a
complete durable inventory for every enabled producer, including removals,
restart recovery, sequence-bound tombstones, and organization isolation. An
empty database is not proof of an empty task inventory. ACP Plan snapshots
remain excluded under the [plan snapshot mapping](superpowers/specs/2026-11-06-core-86-acp-plan-mapping.md).

The implementation must add durable task identity, attempt/session/agent
bindings, and explicit loop iteration IDs for Core-owned agent work, plus a
validated ingestion path for runtime-owned task events. If a required field or
event cannot be represented under Protocol #12, the producer must remain
disabled until an additive, capability-negotiated protocol change is agreed;
it must not infer a substitute. Tests must cover creation, transitions,
approvals, recoverable and terminal failure, retry and loop identity, replay
after restart, source collisions, organization isolation, and complete
inventory/removal capture before the unavailable marker is removed. Until
Protocol provides a typed source for runtime-created Tasks, that producer
remains disabled; Core-owned tasks alone do not establish completeness for
the combined task inventory.

See [Core #117](https://github.com/agentux-os/agentux-core/issues/117) for the
decision review and [Core #104](https://github.com/agentux-os/agentux-core/issues/104)
for producer integration and the snapshot gate.
