# ACP Plan Snapshot Mapping

## Decision

Core emits AUXP `plan_created` only when its producer identifies a plan creation explicitly and supplies a stable event identity for replay. ACP v1 `Plan` is a replaceable current-plan snapshot with entries containing text and status; it has no plan identity and does not distinguish creation from replacement. Therefore Core does not emit an AUXP plan activity for any ACP v1 `Plan` snapshot, including the first snapshot observed in a session.

Plan text and status remain local session data. Core does not derive task IDs, task lifecycle events, or delegation data from plan entries. This keeps reconnects and later replacements from being mislabeled as new plans and avoids publishing identities the source did not provide.

This decision does not change AUXP v1. A future source may emit `plan_created` when it has an explicit creation event and stable replay identity. Task activities additionally require source-provided stable task IDs and lifecycle signals.

## Verification

- Fixture-driven gateway coverage feeds an initial ACP plan snapshot and a replacement snapshot through the production event converter.
- Neither snapshot produces `execution.activity`, consumes a replay sequence, or exposes plan text.
- Existing AUXP schema validation for emitted activity remains unchanged.

## Consequences

- ACP plan contents remain available to local session consumers.
- Remote activity timelines do not show ACP plans until a source can distinguish creation from replacement safely.
- Task and delegation projection work remains blocked on typed source identifiers and lifecycle events.
