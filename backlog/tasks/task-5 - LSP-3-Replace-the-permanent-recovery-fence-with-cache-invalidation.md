---
id: TASK-5
title: 'LSP-3: Replace the permanent recovery fence with cache invalidation'
status: In Progress
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:33'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-1
dependencies: []
priority: high
type: bug
ordinal: 5000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-3).

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

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Research findings (fix/recovery-fence @ c456f8c):
- Recovery is reached only from file-notification paths (workspace/didChangeWatchedFiles reconciliation, notification-queue overflow, failed diagnostic notifications), never from textDocument/didChange. AC #1's "didChange" is read as didChangeWatchedFiles.
- apply_notification_recovery already clears almost all derived state. The preflight only bounds the work of clearing it. On refusal or cancellation, fail_closed_notification_recovery sets rejected_open_fence_permanent, so analysis_admission_fenced() stays true until restart.
- About 11 unit tests in workspace.rs (recovery_refuses_*, oversized_recovery_at_2048_owners..., cancelled_recovery_latches..., tombstone_capacity_overflow...) and the protocol test sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results deliberately pin the fence-on-refusal behaviour. That protocol test is also on TASK-1.1's list of tests failing on master.

Plan (awaiting user approval of the fence change):
1. RED, protocol: model a test on the 64-project test. Open documents, send a didChangeWatchedFiles batch that exhausts reconciliation and overflows the recovery preflight, then assert that textDocument/definition answers. It must fail on current code with the fence error.
2. RED, unit: invalidate_for_reconciliation_budget returns RecoveryOutcome { Recovered, DiscardedReconstructible, LostAuthoritativeEditorState }, with one test per variant.
3. GREEN: when preflight refuses or the worker is cancelled, discard derived state wholesale by swapping each container for an empty one (std::mem::take). Mark owners needs_revalidation, bump generations and mark a global change. Reject rename-endpoint overlays the way the success path does, and record deletion tombstones without syscalls. Set the permanent fence only when authoritative state cannot be retained: a tombstone over MAX_DELETED_OVERRIDES, or a rename endpoint whose version changed after preflight.
4. Align the preflight ceilings (MAX_NOTIFICATION_RECOVERY_BYTES and MAX_NOTIFICATION_RECOVERY_NESTED_ENTRIES) with ResourceLimits, so state that admission accepted takes the precise path.
5. Update the existing tests that pin fence-on-refusal so they assert discard without a fence.
6. Out of scope: permanently_fence_file_notification_analysis in server.rs (unattributable notification evidence) still fences.
Verify: cargo test -p pascal-lsp, fmt, clippy.
<!-- SECTION:PLAN:END -->
