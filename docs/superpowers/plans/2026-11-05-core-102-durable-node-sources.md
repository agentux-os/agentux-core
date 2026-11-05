# Core #102 Durable Node Sources Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist complete node, harness, and node-alert source state with Core event sequences, and make a checkpoint capture read those sources and the event head from one SQLite transaction.

**Architecture:** Core SQLite owns the durable source records, removal tombstones, event records, and immutable capture bindings. Trusted gateway-to-daemon calls supply node identity and harness observations; node heartbeat stays an unsequenced liveness signal. Snapshot capability remains disabled until every required source and wire event contract is available.

**Tech Stack:** Rust workspace, `rusqlite`, serde JSON-RPC over the local Unix socket, AUXP v1 schemas and fixtures.

**Spec:** `docs/superpowers/specs/2026-11-05-node-alert-domain-design.md`; Core #102; merged Protocol #18 / Protocol #19.

## Global Constraints

- Never persist or publish tokens, environment values, API keys, or unrestricted logs.
- Bind enrolled node scope to the gateway's configured organization and node identity; standalone records remain local.
- Keep `node.heartbeat` separate from sequenced current state.
- Do not advertise `node.snapshot_checkpoint.v1` until the full source and event barrier is implemented.
- Use append-only schema migrations; do not edit shipped migrations.

## Review Focus

- Restart after a source update or removal: persisted record and sequence must survive.
- Organization/node mismatch: reject writes and captures outside the authenticated gateway scope.
- Concurrent source change and capture: the returned head and source states must come from one SQLite read snapshot.
- Harness values containing credential-like fields: reject or omit them before durable storage.
- Missing alert producer or protocol payload contract: report an explicit blocker rather than claiming an empty complete projection.

**Dependency update:** Protocol #18 merged as [agentux-protocol#19](https://github.com/agentux-os/agentux-protocol/pull/19) (`0ca8d7d`). The schema blocker is cleared. Core #104 owns connecting concrete lifecycle producers; this plan may persist and capture typed state but must keep the alert projection unavailable until such a producer is connected.

---

### Task 1: Specify the typed event and source boundary

**Files:**
- Modify: `docs/superpowers/specs/2026-11-05-node-alert-domain-design.md`
- Create: `docs/superpowers/plans/2026-11-05-core-102-durable-node-sources.md`
- External dependency: `agentux-protocol#18`

**Interfaces:**
- Consumes: Core #110 alert lifecycle decision and current AUXP snapshot projection identity rules.
- Produces: explicit mapping of node, harness, and alert source identities, safe fields, event shapes, and sequence behavior.

- [x] Confirm Protocol #18 is merged as Protocol #19 and linked to Core #102.
- [x] Use its final wire fields: node state is `{ nodeId, status, capacity }` with `ready`/`draining`/`error`; alerts are full `active`/`resolved` states or explicit removal, with stable IDs, producer/code, condition key, safe summary, and observation timestamps. Heartbeat remains unsequenced.

### Task 2: Add durable typed projection records and mutations

**Files:**
- Modify: `crates/agentux-api/src/lib.rs` (typed local node/alert state and event bodies)
- Modify: `crates/agentux-store/src/schema.rs`
- Modify: `crates/agentux-store/src/snapshot.rs`
- Modify: `crates/agentux-store/src/lib.rs`
- Test: `crates/agentux-store/src/tests.rs`

**Interfaces:**
- Consumes: approved source shapes from Task 1.
- Produces: transaction-scoped methods for upserting complete node, harness, and alert state and recording tombstones with `events.seq`.

- [ ] Write failing tests for identity scope, update sequence, tombstone sequence, restart persistence, and redaction/secret exclusion.
- [ ] Run each targeted test and confirm it fails for the missing source behavior.
- [ ] Add an append-only SQLite migration and minimal transaction methods that write source state and its event atomically.
- [ ] Run targeted store tests and then the Core workspace test suite.

### Task 3: Integrate trusted daemon RPC and atomic capture

**Files:**
- Modify: `crates/agentux-api/src/rpc.rs`
- Modify: `crates/agentux-api/src/client.rs`
- Modify: `crates/agentux-api/src/lib.rs` (internal RPC method constants)
- Modify: `crates/agentuxd/src/server.rs`
- Modify: `crates/agentux-store/src/lib.rs`
- Test: relevant API/store/daemon tests.

**Interfaces:**
- Consumes: Task 2 transaction methods.
- Produces: trusted local producer calls and `DurableCapture` fields sourced from one SQLite read transaction.

- [ ] Write failing RPC/capture tests for organization and node scope, unsupported sources, and a source change racing a capture.
- [ ] Run the tests to confirm the expected missing behavior.
- [ ] Integrate the trusted local RPC without adding source mutation methods to the sandbox session bus.
- [ ] Return current source records and tombstones with the same-transaction event head; keep incomplete sources explicitly unavailable.
- [ ] Run targeted tests and Core workspace tests.

### Task 4: Persist capture sequence bindings and integrate gateway observations

**Files:**
- Modify: `crates/agentux-gateway/src/lib.rs`
- Modify: `crates/agentux-api/src/client.rs`
- Modify: `crates/agentux-store/src/schema.rs`
- Modify: `crates/agentux-store/src/lib.rs`
- Test: gateway restart, binding idempotency, and interleaved event/capture tests.

**Interfaces:**
- Consumes: Tasks 2 and 3.
- Produces: restart-safe immutable binding of local event head to gateway high-water sequence, while heartbeat remains unsequenced.

- [ ] Write failing tests for binding persistence across restart, conflicting retries, and event processing around the capture barrier.
- [ ] Verify expected failures before implementation.
- [ ] Serialize binding with the sequenced gateway event loop and persist the immutable binding in Core SQLite.
- [ ] Persist node identity and complete safe harness inventory from trusted gateway observations; do not include heartbeat freshness or secrets.
- [ ] Run targeted gateway and store tests.

### Task 5: Resolve remaining producers and capability gates

**Files:**
- Modify: `crates/agentux-store/src/snapshot.rs`
- Modify: `crates/agentux-gateway/src/lib.rs`
- Test: capability and capture coverage tests.

**Interfaces:**
- Consumes: Protocol #18 typed schemas and all durable source implementations.
- Produces: capability remains off until every required source is authoritative; once all are complete, explicit evidence for enabling it.

- [ ] Keep node alerts unavailable until a concrete local producer exists and follows the Core #110 open/update/resolve/retention rules.
- [ ] Keep snapshot capability disabled while Protocol #18 or any source producer is incomplete.
- [ ] Add tests proving unsupported sources cannot be represented as fabricated empty collections.
- [ ] Run the complete verification matrix and independent review before PR.

## Self-Review

- Coverage: the plan includes durable writes, authenticated scope, same-transaction capture, gateway binding, restart behavior, secret exclusion, and capability gating.
- Boundary: Protocol payload semantics and a concrete alert producer are explicit dependencies; no implementation may guess them.
- Risk: gateway JSON sequence state and Core SQLite sources are distinct stores. The binding task must prove a crash-safe barrier; if the proof requires moving gateway sequence/outbox state too, split that migration into a separate deliverable before enabling checkpoint support.
- Proportion: implementation is divided by independently reviewable storage, RPC, and gateway-boundary changes.
