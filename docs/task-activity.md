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
   for that logical work item and bind it to its run, organization, role, and
   concrete agent identity. A retry keeps the same `taskId`, increments
   `attempt`, and records the session and agent used by that attempt. A
   deliberately repeated loop iteration is a new logical task with a new ID;
   retrying one iteration is not.
2. **Runtime-created or delegated work.** A harness/runtime may publish a task
   only when it supplies a stable task ID and typed creation/status events.
   Delegation uses the protocol's stable parent task and delegation IDs. Core
   validates that these IDs belong to the current organization and run before
   persisting them. ACP v1 Plan snapshots do not meet this requirement.

The two producers use the same AUXP task lifecycle, but Core must retain source
ownership internally so it can reconcile and recover each inventory. IDs must
be globally unique within the `(organizationId, runId)` task namespace; an
incoming duplicate ID from a different source is rejected unless it is an
explicit update to the already-bound task.

`agentId` identifies the concrete agent execution actor and `roleId` its
assigned role. `sessionId` identifies the corresponding harness session when
one exists. A role name or session ID must not be substituted for `agentId`.
Core persists the mapping before emitting task activity. Every retry has a
deterministic attempt number beginning at 1; the task ID remains unchanged.

## Lifecycle and ownership boundaries

- Creation starts in `pending` or `running`, as reported by the authoritative
  producer. Status changes use the protocol's `TaskStatus` values.
- A retry is another attempt of the same logical task, not a new task. A loop
  iteration that represents new work gets a new task ID and may name its
  parent task.
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
The organization and run binding is checked on every write. A local-only run
cannot be exported into an organization-scoped task snapshot.

## Implementation gate

Task remains `unavailable` in `snapshots.source_capture` until Core has a
complete durable inventory for every enabled producer, including removals,
restart recovery, sequence-bound tombstones, and organization isolation. An
empty database is not proof of an empty task inventory. ACP Plan snapshots
remain excluded under the [plan snapshot mapping](superpowers/specs/2026-11-06-core-86-acp-plan-mapping.md).

The implementation must add durable task identity and attempt/session/agent
bindings for Core-owned agent work, plus a validated ingestion path for
runtime-owned task events. Tests must cover creation, transitions, approvals,
retry and loop identity, replay after restart, source collisions,
organization isolation, and complete inventory/removal capture before the
unavailable marker is removed. Until Protocol provides a typed source for
runtime-created Tasks, that producer remains disabled; Core-owned tasks alone
do not establish completeness for the combined task inventory.

See [Core #117](https://github.com/agentux-os/agentux-core/issues/117) for the
decision review and [Core #104](https://github.com/agentux-os/agentux-core/issues/104)
for producer integration and the snapshot gate.
