---
id: TASK-5
title: 'LSP-3: Replace the permanent recovery fence with cache invalidation'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 02:25'
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
- [x] #1 A protocol test opens enough documents to exceed the recovery preflight, sends one more `didChange`, and the next `textDocument/definition` still answers.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Research findings (fix/recovery-fence @ c456f8c):
- Recovery is reached only from file-notification paths (workspace/didChangeWatchedFiles reconciliation, notification-queue overflow, failed diagnostic notifications), never from textDocument/didChange. AC #1's "didChange" is read as didChangeWatchedFiles.
- apply_notification_recovery already clears almost all derived state. The preflight only bounds the work of clearing it. On refusal or cancellation, fail_closed_notification_recovery sets rejected_open_fence_permanent, so analysis_admission_fenced() stays true until restart.
- About 11 unit tests in workspace.rs (recovery_refuses_*, oversized_recovery_at_2048_owners..., cancelled_recovery_latches..., tombstone_capacity_overflow...) and the protocol test sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results deliberately pin the fence-on-refusal behaviour. That protocol test is also on TASK-1.1's list of tests failing on master.

Plan (user approved the narrowed fence on 2026-10-02):
1. RED, protocol: model a test on the 64-project test. Open documents, send a didChangeWatchedFiles batch that exhausts reconciliation and overflows the recovery preflight, then assert that textDocument/definition answers. It must fail on current code with the fence error.
2. RED, unit: invalidate_for_reconciliation_budget returns RecoveryOutcome { Recovered, DiscardedReconstructible, LostAuthoritativeEditorState }, with one test per variant.
3. GREEN: when preflight refuses or the worker is cancelled, discard derived state wholesale by swapping each container for an empty one (std::mem::take). Mark owners needs_revalidation, bump generations and mark a global change. Reject rename-endpoint overlays the way the success path does, and record deletion tombstones without syscalls. Set the permanent fence only when authoritative state cannot be retained: a tombstone over MAX_DELETED_OVERRIDES, or a rename endpoint whose version changed after preflight.
4. Dropped (user decision, 2-Oct-2026): align the preflight ceilings with ResourceLimits. A refused recovery now discards cheaply instead of fencing, so raising the ceilings would only make the inline bounded path do more work under the workspace lock.
5. Update the existing tests that pin fence-on-refusal so they assert discard without a fence.
6. Out of scope: permanently_fence_file_notification_analysis in server.rs (unattributable notification evidence) still fences.
Verify: cargo test -p pascal-lsp, fmt, clippy.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
2026-10-02: User approved narrowing the fence: discard reconstructible state instead of fencing, and keep the permanent fence only for authoritative state that cannot be retained. Cancellation also discards instead of fencing. Freeing the discarded containers may happen off the worker thread. Code work happens in .worktrees/recovery-fence on branch fix/recovery-fence; master holds the only backlog copy.

2026-10-02 progress (fix/recovery-fence, uncommitted):
- RED protocol: changed sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results (64 projects x 2 MiB descriptors overflow the 128 MiB preflight) to expect fresh answers. On c456f8c it failed with -32803 "analysis is disabled..." at the first definition. This test passes on master, so TASK-1.1's list is stale for it.
- RED unit: added RecoveryOutcome {Recovered, DiscardedReconstructible, LostAuthoritativeEditorState}, returned by invalidate_for_reconciliation_budget. Rewrote the 2048-owner, cancellation and 7 recovery_refuses_* tests (renamed refused_recovery_discards_*) to expect discard without a fence. All 10 failed with LostAuthoritativeEditorState on the old behaviour. The Recovered (512 owners) and tombstone-overflow (Lost) tests recorded the old behaviour and passed both before and after.
- GREEN: discard_reconstructible_state swaps every derived container out (std::mem::take) into DiscardedRecoveryState, which is dropped on a short-lived thread. It marks owners for revalidation, rejects rename-endpoint overlays, records tombstones without syscalls (fences only on MAX_DELETED_OVERRIDES overflow) and reschedules diagnostics for open documents. fail_closed_notification_recovery remains only for the explicit permanently_fence_notification_analysis path.
- Open question for user: plan step 4 (align preflight ceilings with ResourceLimits). With refusal now cheap and non-fatal, raising the ceilings would only make the inline bounded path do more work under the workspace lock. Proposed to drop step 4.

2026-10-02 full-suite findings:
- Removing the fence exposed a real bug in the first discard attempt: it recorded deletion tombstones unstamped (None), as fail_closed did. An unstamped tombstone stops blocking as soon as the not-yet-removed file is observed, which the fence used to mask. The fix calls remember_deleted (stamped; at most MAX_DELETED_OVERRIDES metadata calls, then it fences). New unit test discarded_recovery_keeps_blocking_a_deleted_file_still_on_disk failed against the unstamped version and passes with the fix.
- Updated protocol tests that pinned fail-closed on deadline cancellation: worker_deadline_fallback_restores_unprocessed_delete_tombstone now expects an empty answer (the tombstone hides the provider), and deadline_fallback_rejects_transferred_rename_overlay_and_invalidates_pull_result now asserts the same as the budget variant (overlay rejected, FIFO replayed, pull result invalidated).
- Pre-existing, not caused by this change: shutdown_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline and cancel_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline fail 6/6 on unmodified c456f8c at load ~2.6 (shutdown takes >750 ms). They are not on TASK-1.1's list. Clippy -D warnings fails on two collapsible-if lints in server.rs:371 and :6658 (untouched file). sixty_four_project_metadata... timed out once under full-suite load but passes alone.

2026-10-02: Code committed on fix/recovery-fence as 5e80fe7 (not merged). The backlog edits are uncommitted in master's working tree because master's index already holds unrelated staged changes. Open before Done: decide plan step 4 (preflight ceiling alignment), and how and when to merge.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Replaced the permanent recovery fence with cache invalidation (commit 5e80fe7, merged into master as 2e9684a).

What changed (crates/pascal-lsp/src/workspace.rs):
- When bounded notification recovery is refused by its preflight or interrupted by a deadline or cancellation, discard_reconstructible_state swaps every derived container out (std::mem::take) and frees it on a short-lived thread. It marks owners for revalidation, reschedules diagnostics for open documents and rejects rename-endpoint overlays as the bounded path does. Previously this set rejected_open_fence_permanent, and every request failed with -32803 "analysis is disabled" until restart.
- Deletion tombstones are stamped through remember_deleted. The fence had been masking a bug: unstamped tombstones stop hiding a file that is still on disk.
- invalidate_for_reconciliation_budget returns RecoveryOutcome {Recovered, DiscardedReconstructible, LostAuthoritativeEditorState}. Only a tombstone that cannot be retained (MAX_DELETED_OVERRIDES) still fences. The explicit permanently_fence_file_notification_analysis path in server.rs is unchanged.
- Plan step 4 (align preflight ceilings with ResourceLimits) was dropped by user decision; see the plan.

Tests:
- AC1: sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results (64 open documents whose 2 MiB descriptors overflow the 128 MiB preflight, then a didChangeWatchedFiles batch) now asserts that definitions for open and closed consumers answer with the new provider and that pull results are not reused. On c456f8c it failed with -32803.
- AC2: 10 unit tests rewritten (2048-owner, cancellation, 7 refused_recovery_discards_*), plus the new discarded_recovery_keeps_blocking_a_deleted_file_still_on_disk; all failed on the old behaviour and pass now. Recovered (512 owners) and tombstone-overflow (Lost) recorded old behaviour that is kept. worker_deadline_fallback_restores_unprocessed_delete_tombstone and deadline_fallback_rejects_transferred_rename_overlay_and_invalidates_pull_result updated from expecting the fence to expecting correct answers.
- cargo fmt --check clean. cargo test -p pascal-lsp --features test-support: 684 unit tests and the other binaries pass.

Follow-ups: TASK-97 (the remaining fence for unattributable file notifications in server.rs). Merged master: fmt clean; pascal-lsp and fmt4d tests pass except the two pre-existing shutdown timing tests (tracked in TASK-1.2).

Known failures that predate this change: shutdown_after_/cancel_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline (fail 6/6 on c456f8c at the same load); the 64-project test can time out under full-suite load on its initial pull (TASK-1.1); clippy -D warnings fails on two collapsible-if lints in server.rs:371 and :6658.
<!-- SECTION:FINAL_SUMMARY:END -->
