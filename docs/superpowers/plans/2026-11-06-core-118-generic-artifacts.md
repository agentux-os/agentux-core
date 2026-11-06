# Core #118: Generic Artifact Lifecycle Plan

> **Status:** implementation in progress; Core #118 and Protocol #20 contracts
> were accepted by the owner on 2026-11-06.

**Goal:** Connect an authoritative, verifiable artifact lifecycle to Core
snapshots without advertising incomplete artifact inventories.

**Architecture:** Core owns a private content store and the authoritative
artifact registry. Trusted run producers publish bounded byte streams through
a typed Core API. Core computes digest/length, persists immutable revisions,
and commits projection state with its local event sequence. Remote references
remain disabled until authenticated fetch and removal/replay are defined by
Protocol #20.

**Design:** `docs/superpowers/specs/2026-11-06-core-118-generic-artifacts-design.md`

## Global constraints

- Keep artifact projection in `unavailable_projections()` until all enabled
  producers have a complete, restart-recoverable inventory.
- Never infer artifacts from tool output, ACP plans, arbitrary paths, diffs, or
  free text.
- Do not fetch arbitrary URLs supplied by a producer.
- Verify byte length and SHA-256, enforce the 25 MiB reference limit, and
  preserve immutable content versions.
- Bind enrolled artifacts to persisted `RunOrigin` and authenticated gateway
  organization; reject generic remote artifacts without a run and keep
  standalone artifacts local.
- Persist update/removal sequences and tombstones atomically with projections.
- Do not advertise remote `artifact://` as retrievable until Protocol #20 and
  authenticated gateway dereference are implemented.

## Review focus

- The publishing interface proves producer/run/session/role authorization and
  does not expose arbitrary host paths.
- A crash cannot leave a current projection pointing to a missing blob.
- Retry idempotency, same-ID updates, and tombstone ordering survive restart.
- Snapshot capture is complete only when every artifact source is reconciled.
- Non-agent producers do not masquerade as an agent under Protocol #12.
- Remote retrieval checks the authenticated organization and verifies digest
  before returning content.

## Tasks

### 1. Finalize protocol and Core contracts

- [x] Accept the Core #118 proposal and Protocol #20 retrieval/removal contract.
- [x] Implement idempotency scope as
  `(organizationId, runId, producerId, producerKey, publicationId)`; validate
  media type and enforce a conservative 1 GiB node-wide cap. Retain
  superseded/removed bytes for at least 30 days. Configurable quota,
  hold-aware collection, and garbage collection remain outstanding.
- [x] Add an opt-in typed AgentUX bus publisher bound to an authorized
  run/session/role; end-to-end producer validation remains outstanding.

### 2. Add managed content storage

- [ ] Stage, flush, hash, verify, and atomically promote content-addressed
  blobs; apply private filesystem permissions.
- [ ] Store stable artifact identity, source key, revision, digest, media type,
  byte length, immutable revision URI, and current projection state.
- [ ] Persist the enabled producer manifest and per-source reconciliation
  cursor; retain retired source/tombstone records through the recovery window.
- [ ] Recover staged/orphan blobs and detect missing or corrupt live objects.
- [ ] Test size bounds, content mismatch, duplicate publication, and crash
  recovery.

### 3. Connect an explicit producer

- [ ] Select a concrete agent-originated output producer; require an explicit
  artifact declaration and verified session/role attribution.
- [ ] Emit `artifact_available` only with the content verified by Core.
- [ ] Keep gate/system outputs local until a protocol actor model represents
  their actual source.
- [ ] Test the real producer through create, update, and removal; the MCP
  publication route is implemented but not yet fixture-validated end to end.

### 4. Add authenticated remote retrieval and reconciliation

- [ ] Implement Protocol #20 retrieval with authorization, bounded transfer,
  digest verification, and no host-path exposure.
- [ ] Bind removals to tombstones and exact event sequence; retain them for the
  protocol replay/snapshot recovery window.
- [ ] Reject incomplete checkpoint artifact state even if generic Protocol
  schema fields are optional; require the exact content revision metadata.
- [ ] Verify run/organization isolation, local-only exclusion, and complete
  inventory capture at one SQLite event head.
- [ ] Remove the artifact unavailable marker only after every source passes
  the completeness criteria; keep checkpoint capability gated on all families.

## Integration

- [ ] Update `docs/api.md`, `docs/gateway.md`, and this contract with the actual
  producer path, content retrieval, retention, and failure behavior.
- [ ] Obtain independent security/contract review and green CI on x86_64 and
  aarch64 before merging implementation changes.

### Next implementation slice: producer lifecycle through snapshot capture

**Files:**
- Modify `crates/agentux-config/src/lib.rs`: keep publish/remove MCP tools opt-in.
- Modify `crates/agentux-bus/src/{tools,backend,bus,server,prompt,lib}.rs` and
  `crates/agentux-bus/tests/mcp.rs`: define and authorize `remove_artifact`.
- Modify `crates/agentuxd/src/{bus,engine}.rs` and
  `crates/agentuxd/tests/bus.rs`: derive the removal owner from the live
  session, attach the private store in the daemon fixture, and exercise an
  actual Command-originated producer through capture and removal.
- Modify `crates/agentux-config/tests/validation.rs`: lock down default opt-out.

**Interfaces:** `RemoveArtifact { artifact_id: String }`;
`ArtifactRemoved { artifact_id: String, sequence: i64 }`;
Core resolves organization, run and session from authenticated bus identity,
then delegates to the store's existing original-session tombstone check.

- [ ] Add the MCP integration scenario: opted-in fake agent publishes bounded
  content; Core persists and hashes it; snapshot capture materializes the exact
  node/artifact/revision URI while `artifact` remains unavailable; the same
  active producer session removes it; capture includes the tombstone and the
  artifact family remains unavailable.
- [ ] Implement the opt-in remove tool and ensure both publish/remove are
  excluded by default.
- [ ] Run targeted tests and full x86_64/aarch64 CI; update this plan and PR
  checklist with exact outcomes.

Authenticated remote retrieval, protocol capability negotiation, source
reconciliation, and hold-aware garbage collection remain separate follow-up
work; this slice must not advertise complete artifact inventory to Command.
