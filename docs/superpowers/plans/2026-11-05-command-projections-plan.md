# Command projection implementation plan

> **For Codex:** Follow TDD for each behavior. Keep this issue slice limited to the Core gateway Command producer; do not enable snapshot checkpoints.

**Goal:** Make Core's authenticated gateway publish durable, organization-scoped Command lifecycle state and handle retries safely across process restart.

**Architecture:** Add a typed local API backed by a SQLite transaction for safe Command projection state. The gateway writes lifecycle transitions through that API. Operation handlers receive stable `commandId` only where Core can make deduplication atomic with the operation; otherwise an interrupted state is terminal for automatic retries and requires local reconciliation.

**Tech stack:** Rust workspace, Tokio, serde JSON-RPC, SQLite via `agentux-store`.

### Task 1: Command lifecycle store API

- Add a typed command lifecycle request/result in `agentux-api::rpc`.
- Add store transaction methods for create/update, enforcing organization scope, valid transitions, safe state shape, and event sequence attribution.
- Add tests for create/update, invalid transition, duplicate ID, organization isolation, tombstone/restart behavior, and parameter/secret exclusion.

### Task 2: Local daemon RPC

- Route the new typed method through `agentuxd::server` into one SQLite transaction.
- Add client helper and RPC integration tests proving durable reads after reopening the store.

### Task 3: Gateway lifecycle and safe retry behavior

- Validate organization/node/Command instance before any lifecycle write.
- Persist received and policy decision states before acknowledging held/denied commands.
- Persist executing before allowed effects and completed/failed after the outcome.
- Recover unfinished records as interrupted; never automatically replay non-idempotent operations.
- Add operation-specific deduplication only for handlers with Core-owned atomic persistence; allow `run.start` recovery only with the persisted matching mission run ID, and preserve the existing wire response for a repeated completed command.
- Retain local confirmation results until `command.completed` is committed to the durable gateway outbox; only then remove the pending record.
- Add tests for duplicate delivery, restart before/after execution, held decision recovery, and uncertain-effect reconciliation.

### Task 4: Documentation and review

- Update `docs/api.md` and `docs/gateway.md` with ownership, stored fields, retry/reconciliation behavior, and retention.
- Keep all three business projection families unavailable and checkpoint capability disabled.
- Project typed pull requests from Command-owned runs as run-scoped artifacts; keep the artifact family unavailable until generic artifact lifecycle events have an authoritative producer.
- Run CI on Linux, review the full diff for sensitive-field leaks and crash windows, add PR description and AGENTS.md-style independent-review request.

**Verification:** Focused Rust tests and full Core CI matrix on Linux (`x86_64` and `aarch64`), plus fmt, Clippy, and cargo-deny.
