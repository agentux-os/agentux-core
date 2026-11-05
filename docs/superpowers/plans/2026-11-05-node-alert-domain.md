# Node Alert Domain Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record the Core-owned local node alert contract and examples required to resolve Core #109.

**Architecture:** This is a documentation-only domain decision in agentux-core. Core owns local alert identity and lifecycle in both standalone and enrolled installs; Command projects received state and independently derives fleet presence. Durable producers and snapshot publication remain in Core #102.

**Tech Stack:** Markdown documentation and JSON examples; no runtime dependencies.

**Spec:** `docs/superpowers/specs/2026-11-05-node-alert-domain-design.md`

## Global Constraints

- Keep `node.snapshot_checkpoint.v1` disabled until every required projection has a durable, sequence-bound source.
- Do not define Command fleet presence as a Core `node_alert`.
- Use stable alert IDs, `active`/`resolved` current states, and sequence-bound removal tombstones.
- Keep organization scope optional for local standalone alerts; the gateway supplies authenticated organization/node scope when exporting.

## Review Focus

- Standalone creation without organization identity: examples/documentation must show local scope.
- Enrolled export: document that gateway binds authenticated organization/node identity.
- Repeated observation: document stable ID and update-in-place semantics.
- Condition recovery: document source-observed `active` → `resolved` transition.
- Retention/correction: document removal as a sequenced tombstone, not silent deletion.

---

### Task 1: Publish the node alert domain contract

**Files:**
- Create: `docs/node-alerts.md`
- Modify: `docs/gateway.md`

**Interfaces:**
- Consumes: approved design at `docs/superpowers/specs/2026-11-05-node-alert-domain-design.md`.
- Produces: Core-owned contract for alert identity, fields, transitions, scope, and retention.

- [x] **Step 1: Write `docs/node-alerts.md`**

Document ownership, first-version fields (`alertId`, producer/code, condition key, safe summary, first/last observed timestamps, status, last local sequence), stable identity, active/update/resolve/remove transitions, retention and secret-redaction constraints. Include JSON examples for active, resolved, and removed states. Make clear that a concrete producer must separately specify its condition and recovery rule.

- [x] **Step 2: Run JSON syntax validation on each example**

Run: `python -c "import json,pathlib; s=pathlib.Path('docs/node-alerts.md').read_text(encoding='utf-8'); spans=[('### Active','### Resolved'),('### Resolved','### Removed'),('### Removed','## Producers and snapshot gate')]; sections=[s.split(a,1)[1].split(b,1)[0] for a,b in spans]; fence=chr(96)*3; examples=[section.split(fence+'json',1)[1].split(fence,1)[0] for section in sections]; [json.loads(x) for x in examples]; print('3 examples valid')"`
Expected: three examples parse successfully.

- [x] **Step 3: Link the contract from `docs/gateway.md`**

Add a concise link explaining that gateway relays Core-owned local alerts when enrolled and does not derive them from heartbeat or harness polling.

- [x] **Step 4: Review the diff against the approved design**

Run: `git diff --check` and `git diff -- docs/node-alerts.md docs/gateway.md`.
Expected: no whitespace errors; documented fields, transitions, scope, and removal match the design.

### Task 2: Publish the decision and update dependent snapshot issues

**Files:**
- No additional files.

**Interfaces:**
- Consumes: Task 1 contract and examples.
- Produces: reviewable evidence that Core #109 acceptance criteria are met and cross-references in #97/#102; implementation remains tracked by #102.

- [x] **Step 1: Confirm issue acceptance coverage**

Check that `docs/node-alerts.md` identifies the owner, scope, stable ID, states, resolution authority, and retention/tombstone behavior, and includes active, resolved, and removed examples.

- [x] **Step 2: Confirm the checkpoint gate remains explicit**

Check that neither the docs nor the implementation advertises snapshot capability; node/harness same-barrier capture remains a #102 prerequisite.

- [x] **Step 3: Open a conventional documentation PR**

Use a `docs:` title and include reviewer instructions in the PR body: check the standalone/enrolled boundary, separation from fleet presence, current-state/tombstone examples, and the snapshot gate. State that no runtime producer is implemented.

- [x] **Step 4: Cross-reference the decision from #97 and #102**

Comment on both issues with the PR link and the ownership decision. Keep #102 open for the durable sequenced source, concrete producer, and same-barrier node/harness capture. Keep #97 open and snapshot capability disabled until all sources are complete. Do not close #109 before its PR is merged.
