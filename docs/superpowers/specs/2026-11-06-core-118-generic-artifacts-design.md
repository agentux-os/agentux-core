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

Updates retain `artifactId` and point to a new immutable content version.
Removal writes a same-transaction tombstone and sequence. Idempotency keys
make a repeated publication recover the recorded result and reject changed
content for the same key. Older versions remain available through replay and
snapshot retention.

For enrolled runs, Core derives organization and mission ownership from the
persisted `RunOrigin`; it verifies the organization against the authenticated
gateway enrollment and checks every activity's mission against the origin.
Standalone local artifacts may remain local, but cannot enter an
organization-scoped snapshot. Organization-owned artifacts without a run
require a separate authenticated producer and are not covered by this run
producer.

Protocol #12 currently carries the reference, but does not specify how
`artifact://` is authenticated and resolved or how removal is announced.
Protocol #20 owns that contract. Until it and the Core retrieval path are
implemented, a remote event must not claim that `artifact://` is downloadable,
and the generic artifact snapshot family remains unavailable. Protocol #17
provides the checkpoint path for projection tombstones after replay expiry.

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

## Consequences

- Generic artifacts are not ready for snapshot publication on current main.
- Core needs a typed local publisher and a managed content store.
- Protocol #20 must define authenticated retrieval, content revisions,
  removal, and replay compatibility.
- Command-owned pull-request references remain supported as a separate partial
  producer.

See the [Core artifact contract](../../artifacts.md), Core #118, Core #104, and
Protocol #20.
