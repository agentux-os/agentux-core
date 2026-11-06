# Generic artifact lifecycle

**Decision status: Proposed** (Core #118)

## Decision

Core owns the node's artifact registry, durable lifecycle, and content bytes for
generic run artifacts. A producer must explicitly publish an artifact through
a trusted Core-owned producer interface. There is no generic artifact
producer in the current harness or gateway, so the current typed pull-request
projection remains a partial source and `artifact` stays unavailable in
`snapshots.source_capture`.

The publishing path is:

1. A Core-controlled producer receives an explicit artifact output from a run
   step and supplies a bounded byte stream plus a producer-scoped logical key,
   display name, and media type to the Core artifact registry. Harness text,
   tool output, diffs, and guessed local paths are not artifact declarations.
2. Core verifies the source is authorized for that persisted run, enforces the
   Protocol limit of 25 MiB per reference, calculates byte length and SHA-256,
   and stores the bytes in a private node-local content store. Core assigns and
   persists the stable `artifactId`; producers never supply a URI or claim a
   digest that Core has not verified.
3. Core writes the organization-scoped projection and local event sequence.
   It emits `artifact_available` to Command only after the authenticated
   dereference contract is implemented; until then the artifact remains local
   and the generic snapshot family stays unavailable.

Protocol #12 requires `missionId`, `runId`, `agentId`, and `roleId` on this
activity. The producer must therefore be bound to the actual run session and
role; Core validates this association and never fabricates an agent identity.
The initial remote publisher is limited to agent-originated artifacts with
that verified attribution. Outputs from Core-only gates, project-level
artifacts, or other producers without an agent/role identity stay local until
a compatible protocol contract represents their true source.

The current storage table contains projection metadata, not artifact bytes.
The first implementation therefore needs a managed blob store and a typed
producer API in `agentuxd`; importing files directly from an arbitrary path or
fetching a producer-supplied HTTPS URL is out of scope. Core must not make
network requests to arbitrary artifact URLs, which would create an SSRF and
credential-leak boundary.

## Identity, revisions, and ownership

An artifact is a logical output identified by a stable Core-generated
`artifactId`, scoped to its organization and run. Repeated publication for the
same producer key updates that logical artifact; it does not derive identity
from a filename, path, display name, or digest. Each published content version
is immutable and addressed internally by its verified SHA-256 digest. The
current projection points to the latest version and stores its safe name,
media type, byte length, and URI.

Every remote artifact must belong to a persisted Command-originated run. Core
derives `organizationId` and `missionId` from `Run.origin` and verifies the
organization against the authenticated gateway enrollment. A local-only run
may retain a local artifact under local policy, but it is not part of an
organization-scoped snapshot and is not sent to Command. A supplied run link
must resolve to the same organization; callers cannot choose the organization
or override a run's owner.

This decision covers run artifacts. An organization-owned artifact with no
run may be valid in the projection model, but it needs its own authenticated
producer and complete-inventory contract. This run producer does not claim
that source; the artifact family remains unavailable until every enabled
organization-level source is also accounted for.

Core stores only bounded metadata in projection state. It does not include
host paths, raw tool input/output, credentials, or unbounded logs. Downloads
are served only after node and organization authorization; the response never
reveals a host filesystem path. Clients verify byte length and SHA-256 after
retrieval and handle content as an attachment unless a separately reviewed
safe renderer exists. A node-level storage quota and retention policy must be
configured before publication is enabled. At quota, Core rejects publication
with a clear error; it does not silently evict live artifacts to admit new
content.

## Atomic lifecycle and recovery

- **Create:** Core assigns the ID, writes a staged blob, flushes and verifies
  it, atomically promotes it to immutable storage, then commits the artifact
  state and event sequence in SQLite. A crash may leave an unreferenced blob
  for later cleanup, but must not commit a projection pointing to missing
  bytes.
- **Update:** Keep the same `artifactId`; write a new immutable content
  version, then atomically update the current projection and sequence. Retain
  older versions while a replay or snapshot could still reference them.
- **Remove:** Write a projection tombstone with the exact local event sequence
  in the same SQLite transaction. Do not silently delete the record or blob.
  Garbage collection follows the agreed replay/snapshot retention window.
- **Retry and restart:** The producer supplies a stable publication
  idempotency key. A retry returns the recorded result for the same key and
  content digest; reuse with different content is rejected. Startup recovery
  reconciles staged/orphaned blobs and verifies that every live projection
  points to a present, digest-matching object.

Protocol #12 carries `artifact_available` and a bounded reference, but it does
not define authenticated byte retrieval or a removal activity. Protocol #17
provides checkpoint/tombstone recovery for snapshot projections. Core must not
advertise generic artifacts in remote snapshots until a compatible protocol
path exists for authenticated dereference and removal/recovery. This gap is
tracked in [Protocol #20](https://github.com/agentux-os/agentux-protocol/issues/20).
Until that work lands, `artifact://` remains a local registry identifier and
is not a promise that a remote client can fetch content.

## Completeness gate

`artifact` remains explicitly `unavailable` in `snapshots.source_capture`
until Core can prove a complete inventory for all enabled artifact producers,
including owner/run binding, byte availability, updates, removal tombstones,
replay after restart, and organization isolation. A database with no artifact
rows is not evidence that the source is complete. Command-owned pull requests
remain valid typed artifacts but do not make the generic producer complete.

Implementation coverage must include bounded and oversized content, digest
verification, duplicate publication, update and removal, orphan cleanup,
missing/corrupt blob recovery, local-only and cross-organization rejection,
authenticated retrieval, and same-sequence projection capture. Snapshot
checkpoint capability remains disabled until this and every other required
source is complete.

See [Core #118](https://github.com/agentux-os/agentux-core/issues/118),
[Core #104](https://github.com/agentux-os/agentux-core/issues/104),
[Protocol #12](https://github.com/agentux-os/agentux-protocol/issues/12), and
[Protocol #17](https://github.com/agentux-os/agentux-protocol/pull/17), and
[Protocol #20](https://github.com/agentux-os/agentux-protocol/issues/20).
The detailed [design](superpowers/specs/2026-11-06-core-118-generic-artifacts-design.md)
and [implementation plan](superpowers/plans/2026-11-06-core-118-generic-artifacts.md)
track the proposed sequence.
