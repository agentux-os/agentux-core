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
For Core-owned work, `agentId` comes from the durable Core actor assignment
bound to the run role, following the identity rule in
[`task-activity.md`](task-activity.md); a runtime actor ID is accepted only
when Core binds it to the authenticated producer/session and role. If that
mapping is absent, publication fails closed. The initial remote publisher is
limited to agent-originated artifacts with verified attribution. Outputs from
Core-only gates, project-level artifacts, or other producers without an
agent/role identity stay local until a compatible protocol contract represents
their true source.

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
media type, byte length, and URI. Every content URI identifies one exact
revision, for example `artifact://<nodeId>/<artifactId>/<sha256>`; no URI means
"latest". Dereference requires both the logical artifact ID and the digest.
This keeps old activity events and replay references bound to the bytes they
originally described.

Every remote artifact must belong to a persisted Command-originated run. Core
derives `organizationId` and `missionId` from `Run.origin` and verifies the
organization against the authenticated gateway enrollment. A local-only run
may retain a local artifact under local policy, but it is not part of an
organization-scoped snapshot and is not sent to Command. A supplied run link
must resolve to the same organization; callers cannot choose the organization
or override a run's owner.

This contract supports only run-scoped generic artifacts. Core rejects a
generic publication without a persisted `RunOrigin` and `runId`; an
organization-owned artifact without a run is outside this projection
contract. If the product later needs organization-level artifacts, that
requires a distinct scope variant and producer inventory, and the artifact
family cannot be marked complete until that source is implemented and
reconciled.

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
  older versions while a replay or snapshot could still reference them. The
  new URI contains the new digest, while the old URI continues to identify the
  previous bytes for at least 30 days after supersession, and longer while an
  event remains in the replay/outbox window, a checkpoint can still reference
  it, or a download is active.
- **Remove:** Write a projection tombstone with the exact local event sequence
  in the same SQLite transaction. Do not silently delete the record or blob.
  Keep the bytes for at least 30 days after removal and longer while replay,
  outbox, checkpoint, or active-download references remain. After that, the
  old URI returns `410 Gone`; it never resolves to different bytes. Retain the
  tombstone until the replay and checkpoint recovery windows expire.
- **Republish after removal:** A producer key may be published again only as a
  new artifact incarnation with a new `artifactId`. Core retains the mapping
  from producer key to incarnation and never reuses a tombstoned ID. Old
  replayed events therefore cannot make the new incarnation appear removed or
  resurrect the old one.
- **Retry and restart:** The producer supplies a stable publication ID. Core
  scopes idempotency by `(organizationId, runId, producerId, producerKey,
  publicationId)`. A retry returns the recorded result when this tuple and
  content digest match; reuse of that tuple with different bytes is rejected.
  Manifest generation is recorded for reconciliation but is not part of the
  idempotency key, so a deployment does not duplicate a publication. Updates
  use a new publication ID while the artifact is active; after removal, a new
  publication creates a new artifact incarnation and ID. Startup recovery
  reconciles staged/orphaned blobs and verifies that every live projection
  points to a present, digest-matching object.

Thirty days is the minimum content-retention window for superseded or removed
versions. An active replay/outbox/checkpoint reference extends the hold beyond
that minimum. Garbage collection cannot remove a version while any such hold
is active. At quota, Core rejects new publications rather than shortening the
retention guarantee or evicting live artifacts.

Protocol #12 carries `artifact_available` and a bounded reference, but it does
not define authenticated byte retrieval or a removal activity. Protocol #17
provides checkpoint/tombstone recovery for snapshot projections. Core must not
advertise generic artifacts in remote snapshots until a compatible protocol
path exists for authenticated dereference and removal/recovery. This gap is
tracked in [Protocol #20](https://github.com/agentux-os/agentux-protocol/issues/20).
Until that work lands, `artifact://` remains a local registry identifier and
is not a promise that a remote client can fetch content.

For snapshot recovery, Core and Command must additionally validate a complete
artifact `currentState` containing `artifactId`, `organizationId`, `runId`,
`revision` (the verified SHA-256), `mediaType`, `byteLength`, and the exact
revision URI. Protocol #17's generic artifact projection schema does not
currently require these content fields, so its permissive schema is not, by
itself, sufficient evidence that a checkpoint can restore artifact state.
Protocol #20 must define this stricter profile or a compatible schema
extension; Core rejects incomplete artifact entries before materialization.

## Completeness gate

Core maintains an authoritative producer registry from its enabled
artifact-publisher manifest. Each registered producer has a stable source ID,
manifest generation, and reconciliation cursor. At startup, Core compares the
enabled manifest with its persisted registry; `source_capture` reports
`artifact` unavailable if any enabled producer is missing, if a producer has
not reconciled through the captured event head, or if a source was removed
without a final inventory/tombstone pass. Producer retirement remains in the
registry until its tombstones have passed the replay/snapshot retention window.

`artifact` remains explicitly `unavailable` in `snapshots.source_capture`
until this registry proves a complete inventory, including owner/run binding,
byte availability, updates, removal tombstones, replay after restart, and
organization isolation. A database with no artifact rows is not evidence
that the source is complete. Command-owned pull requests remain valid typed
projections but do not make the generic producer complete.

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
