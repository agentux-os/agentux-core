# Core #86: ACP Plan Snapshot Mapping Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Specify and verify that ACP v1 plan snapshots do not produce misleading AUXP plan or task activities.

**Architecture:** Keep plan snapshots in the local session event stream. Document the producer requirements for a future `plan_created` activity and lock the current suppression behavior with fixture-driven gateway coverage. No protocol schema or runtime behavior changes.

**Tech Stack:** Rust, serde_json fixtures, Cargo workspace.

**Spec:** `docs/superpowers/specs/2026-11-06-core-86-acp-plan-mapping.md`

## Global Constraints

- Do not infer stable IDs or lifecycle transitions from free-text ACP plan entries.
- Do not emit `plan_created` without explicit creation semantics and stable replay identity.
- Keep snapshot capability and AUXP v1 schemas unchanged.

## Review Focus

- Initial snapshots after a reconnect must not be mislabeled as newly created plans.
- Reordered or replaced entries must not be converted into task identities.
- Plan text must not leak into remote execution activity.
- Suppressed snapshots must not advance the replay sequence.

---

### Task 1: Lock plan snapshot suppression with gateway fixtures

**Files:**
- Create: `crates/agentux-gateway/tests/fixtures/acp-plan-snapshots.json`
- Modify: `crates/agentux-gateway/src/lib.rs`

**Interfaces:**
- Consumes: `agentux_api::Event` with `SessionEvent::Plan`.
- Produces: a test proving the converter emits no remote activity for either the initial or replacement snapshot.

- [ ] Add fixtures for an initial and replacement plan snapshot with distinct entries and private marker text.
- [ ] Feed both fixtures through `Messages::from_event` with `execution.activity.v1` accepted; assert no message and no replay sequence advance.
- [ ] Run the targeted gateway test and the workspace CI matrix.

### Task 2: Document the mapping rule and close the decision issue

**Files:**
- Modify: `crates/agentux-harness/README.md`
- Modify: `docs/superpowers/specs/2026-11-06-core-86-acp-plan-mapping.md`

- [ ] State when `plan_created` is valid and why ACP v1 snapshots do not qualify.
- [ ] Cross-check Core #86 acceptance criteria and report the decision and test evidence on the issue.

### Task 3: Review and integrate

- [ ] Run `cargo fmt --all -- --check`, `git diff --check`, and `cargo metadata --locked --no-deps --format-version 1`.
- [ ] Obtain independent review and green Linux CI for x86_64 and aarch64.
- [ ] Merge the PR and close Core #86 only after its acceptance criteria are satisfied.
