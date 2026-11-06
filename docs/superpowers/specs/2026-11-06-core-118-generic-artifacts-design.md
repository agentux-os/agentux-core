# Core #118: Generic Artifact Lifecycle Design

## Problem

Core #115 added a typed projection for Command-owned pull requests. The store
has durable projection metadata and tombstones, but it has no generic producer
or content store. ACP session events expose tool text, diffs, and local
context, none of which is a verified artifact reference. Protocol #12 defines
bounded `ArtifactRef` metadata and `artifact_available`, but no authenticated
content retrieval or artifact-removal event.

Enabling the artifact family now would tell snapshot consumers that the
inventory is complete even though generic content cannot be fetched, updated,
removed, or reconciled after a restart.

## Goals

- Assign one owner for generic artifact identity, content bytes, metadata, and
  lifecycle on each node.
- Define the trusted write path and its boundaries before connecting a
  producer.
- Preserve stable logical identity across content revisions while verifying
  every immutable content version.
- Define organization/run binding, retrieval authorization, replay, removal,
  and snapshot completeness.
- Keep the snapshot family unavailable until every source is complete.

## Non-goals

- Infer artifacts from ACP plans, tool output, diffs, local paths, or run text.
- Fetch arbitrary HTTPS URLs supplied by a harness or agent.
- Change the Protocol #12 schema in this Core-only decision.
- Enable remote references before the authenticated dereference contract
  exists.
- Treat current pull-request metadata as proof that generic artifact bytes are
  stored or retrievable.

## Decision

Core owns a node-local artifact registry and private content store. A trusted
Core-controlled producer publishes a bounded byte stream, a producer-scoped
logical key, display name, and media type. The Core verifies the producer's
run/session/role binding, computes byte length and SHA-256, assigns a stable
`artifactId`, and stores immutable content versions. The initial remote source
is limited to agent-originated outputs because Protocol #12 requires
`agentId` and `roleId`; Core does not fabricate those identities for gates or
system processes.

The Core API accepts bytes through a typed publisher interface, not an
arbitrary host path, URI, or caller-asserted digest. Content is staged and
verified before its immutable blob is promoted. SQLite commits the current
artifact projection and local event sequence only after the blob is durable.
Orphan staged blobs can be collected; a live projection must never reference a
missing or digest-mismatched object.

Publication is bounded per artifact and by a configured node-level storage
quota. At quota, publication fails visibly; Core does not evict live objects.
The quota and retention defaults must be selected before the producer is
enabled.

Updates retain `artifactId` and point to a new immutable content version; each
URI identifies the exact SHA-256 revision, never a moving "latest" alias.
Removal writes a same-transaction tombstone and sequence. Republishing the
same producer key after removal creates a new `artifactId` incarnation;
tombstoned IDs are never reused. Idempotency keys make a repeated publication
recover the recorded result and reject changed content for the same key. Core
scopes each key by `(organizationId, runId, producerId, producerKey,
publicationId)`; manifest generation is recorded for reconciliation but is
not in the idempotency tuple. Updates use a new publication ID. After removal,
a new publication ID creates a new artifact incarnation. Superseded or
removed bytes remain dereferenceable for at least 30 days, and longer while
replay/outbox, an unexpired checkpoint, or an active download references them.
Garbage collection runs only after all holds expire; an expired URI returns
`410 Gone` and never resolves to another digest.

For enrolled runs, Core derives organization and mission ownership from the
persisted `RunOrigin`; it verifies the organization against the authenticated
gateway enrollment and checks every activity's mission against the origin.
The mandatory `agentId` comes from the durable Core actor assignment bound to
the run role, as defined by the task activity contract; a runtime ID must be
bound to the authenticated producer/session. Missing actor mappings block
publication instead of causing Core to invent an identity.
Standalone local artifacts may remain local, but cannot enter an
organization-scoped snapshot. This contract rejects generic artifacts without
a persisted run. An organization-owned artifact without a run is outside this
projection contract; if introduced later, it requires a distinct typed scope
and source inventory before the artifact family can be complete.

Protocol #12 currently carries the reference, but does not specify how
`artifact://` is authenticated and resolved or how removal is announced.
Protocol #20 owns that contract. Until it and the Core retrieval path are
implemented, a remote event must not claim that `artifact://` is downloadable,
and the generic artifact snapshot family remains unavailable. Protocol #17
provides the checkpoint path for projection tombstones after replay expiry.
Because the generic checkpoint schema does not currently require content
metadata for artifacts, Core and Command must validate an artifact checkpoint
entry's `revision` (SHA-256), `mediaType`, `byteLength`, and exact revision URI
in addition to its ID and organization. Protocol #20 must formalize this
profile before checkpoint recovery can establish artifact completeness.

Core's enabled publisher manifest is the authoritative inventory of artifact
sources. A persisted registry records each stable producer ID, manifest
generation, and reconciliation cursor. Capture remains unavailable when the
configured and persisted manifests differ, a producer has not reconciled
through the captured event head, or a source is retired without a final
tombstone pass. A retired source remains in the registry through tombstone
retention.

## Alternatives

1. **Trust producer-supplied URI and digest.** Rejected: Core cannot verify the
   content and may persist stale, inaccessible, or cross-organization links.
2. **Discover artifacts by scanning worktrees.** Rejected: a filename or path
   does not express publication intent and could expose credentials, caches,
   or unrelated files.
3. **Fetch arbitrary HTTPS references.** Rejected: this creates SSRF and
   credential-forwarding risks and still does not establish stable ownership.
4. **Core-owned registry and content store.** Chosen: Core can verify bytes,
   enforce limits, apply local authorization, and atomically bind projection
   state to its own event sequence. Remote use still requires Protocol #20.

## Security and failure behavior

- Enforce the Protocol 25 MiB per-reference limit; reject oversize before
  committing metadata.
- Bind every content URI to an immutable digest and retain that version for
  at least 30 days after supersession/removal and for all active replay,
  outbox, checkpoint, and download references, whichever is longer.
- Require checkpoint artifacts to contain the complete content revision
  metadata even if a permissive generic schema would accept less.
- Never store host paths, credentials, raw prompts, tool input/output, or
  unbounded logs in projection state or protocol references.
- Require authenticated node and organization authorization before retrieval;
  verify digest and byte length on the receiving side.
- Serve bytes as attachments by default. A renderer must separately handle
  active content safely.
- On a blob/SQLite crash boundary, prefer an orphan blob that can be collected
  over a live metadata row with missing bytes.
- Keep the artifact family unavailable until all producer inventories,
  update/removal recovery, and organization boundaries are complete.
- Define producer registry reconciliation and non-reuse of tombstoned artifact
  IDs so source completeness and replay cannot be inferred from an empty table.

## Consequences

- Generic artifacts are not ready for snapshot publication on current main.
- Core needs a typed local publisher and a managed content store.
- Protocol #20 must define authenticated retrieval, content revisions,
  removal, and replay compatibility.
- Command-owned pull-request references remain supported as a separate partial
  producer.

See the [Core artifact contract](../../artifacts.md), Core #118, Core #104, and
Protocol #20.
