---
id: TASK-5
title: 'LSP-3: Replace the permanent recovery fence with cache invalidation'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-1
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: high
type: bug
ordinal: 5000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-3). Read that file's Global constraints section before starting.

Severity: high. Cost: runtime, availability.

**Problem.** `crates/pascal-lsp/src/workspace.rs:4435-4442`
(`invalidate_for_reconciliation_budget`) falls through to
`fail_closed_notification_recovery`, which sets
`rejected_open_fence_permanent` (see `:4516-4535`, `:4969-4997`;
consumers at `server.rs:7750-7772`, `:8486-8495`). The preflight uses its
own ceilings (128 MiB payload, 65,536 nested entries) that differ from the
admission limits, so state that was admitted can become unrecoverable.
After that every request gets a misleading "rejected editor document"
error until restart.

**Approach.**
1. Split recovery outcomes into `Recovered`, `DiscardedReconstructible`
   and `LostAuthoritativeEditorState`.
2. Only the last sets the permanent fence. Reconstructible state (parsed
   documents, indexes, catalogues, project graphs) is dropped and rebuilt
   lazily.
3. Align the preflight ceilings with `ResourceLimits`
   (`workspace.rs:452-465`) so admission and recovery agree.

**Tests first.** The protocol test above, written against the current code
so it fails with the permanent-fence error. Unit test for the three outcome
variants.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A protocol test opens enough documents to exceed the recovery preflight, sends one more `didChange`, and the next `textDocument/definition` still answers.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
