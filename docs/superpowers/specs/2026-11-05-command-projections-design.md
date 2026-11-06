# Durable Command lifecycle and retry safety

## Intent

Connect the authenticated node gateway to Core's organization-scoped SQLite Command projection. Keep command lifecycle state durable across gateway restarts, avoid storing request parameters or credentials in projection state, and make retries safe according to each operation's actual semantics.

## Goals

- Record received, held, confirmed, denied, executing, completed, failed, and interrupted states with stable `commandId` and safe method/status fields.
- Bind every record to the authenticated `NodeConfig.organization_id`.
- Use operation-level idempotency where the effect and deduplication key can be committed together by Core; persist only the stable mission run ID for `run.start`.
- Never automatically replay an operation whose effect may have happened before a crash but cannot be deduplicated. Mark it for local reconciliation.
- Keep command projections unavailable to snapshot capture until the complete authoritative inventory and all other required producers are available.

## Non-goals

- Persist raw command parameters, provider credentials, or arbitrary operation responses.
- Claim exactly-once behavior for external effects that Core cannot commit atomically.
- Enable Protocol snapshot checkpoints or change the existing snapshot capability gate.
- Implement generic AUXP artifact lifecycle events or task producers in this slice; only typed Command-owned pull requests are projected as artifacts.

## Approaches

1. **Gateway-only durable response cache.** Rejected because a crash between the Core side effect and the gateway cache write still duplicates the effect.
2. **Durable journal with blanket replay.** Rejected because replaying non-idempotent effects such as bus posts or external harness tests can duplicate messages or costs.
3. **Core-owned journal and operation-level deduplication (chosen).** Persist lifecycle state in SQLite; pass stable `commandId` to handlers that can deduplicate atomically with their effect. Any operation without such support remains interrupted after an uncertain crash and requires local reconciliation.

## Constraints and behavior

- The gateway authenticates Command instance, organization, and node before any lifecycle write.
- Command projection rows contain only stable identity, organization, safe method/status/current-state fields, and optional run association.
- A retry of a completed idempotent operation returns its recorded safe outcome. A retry of an interrupted unsupported operation is rejected with a reconciliation-required result and does not execute again.
- A confirmed pending `run.start` may be retried after a crash only with the same operation key; other confirmed commands remain claimed and are not re-executed. Its safe completion payload is retained until the gateway event outbox commits it.
- Human-confirmed commands retain the existing local pending-command mechanism for the data needed to execute after confirmation; projection state itself does not copy that payload.
- Snapshot `unavailable_projections()` remains unchanged in this partial delivery.
- Command-owned runs with a typed pull request publish a stable run-scoped artifact projection in the same SQLite transaction as the run event; clearing or replacing the pull request tombstones the prior artifact. This does not make the generic artifact source complete.

## Implementation left to the builder

Add typed local RPC and SQLite transaction support for command lifecycle updates; make supported operations idempotent using the stable command ID; integrate gateway received/decision/execution/recovery transitions; add restart, retry, organization-isolation, and secret-exclusion coverage; update API and gateway ownership docs. Keep unsupported or externally non-atomic handlers fail-closed after an interrupted execution.
