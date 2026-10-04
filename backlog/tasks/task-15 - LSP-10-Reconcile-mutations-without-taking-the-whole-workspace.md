---
id: TASK-15
title: 'LSP-10: Reconcile mutations without taking the whole workspace'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 02:39'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies:
  - TASK-14
modified_files:
  - crates/pascal-lsp/src/server.rs
  - crates/pascal-lsp/tests/protocol.rs
priority: high
type: enhancement
ordinal: 15000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-10). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime latency.

**Problem.** `server.rs:10581-10608`, `:10920-10928`, `:10990-11009`,
`:11196-11208`: the mutation worker `mem::take`s the entire `Workspace`.
While it runs, new requests are deferred and completed results are not
delivered. The 30 s deadline only requests cooperative cancellation.

**Approach.** Reconcile against an immutable workspace revision into a
`ChangeSet`, then apply the change set under a short critical section.
Completed results whose generation predates the commit are still
deliverable if their dependency token (LSP-7) still matches.

**Tests first.** Barrier-harness protocol test for the above.

**Depends on.** LSP-7 for the dependency token.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 During a 1,000-file watcher batch, a hover request already completed before the batch is delivered without waiting for reconciliation.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/lsp-protocol-commits, branch perf/lsp-protocol-commits (stacked on TASK-14, be90681).

Root cause: spawn_workspace_file_notification (server.rs) mem::takes the Workspace into PascalLspWorkspaceMutation; while it runs the event loop skips jobs.poll (workspace_busy), so analysis results that already completed sit in the result channel until reconciliation commits.

Ruling (scope): converting handle_notification_with_control into a pure ChangeSet producer means rewriting the workspace.rs mutation paths, which a later lane owns (TASK-10..13/17/18) and which is far beyond this lane. Instead:
1. Capture the pre-batch revision (source/configuration generation + admission fence) when the worker takes the workspace.
2. While the worker runs, drain completed client results: a result whose generations equal the pre-batch revision is exactly as fresh as it would have been had it been delivered just before the batch (the batch has not been applied and every later workspace message is deferred), so it is delivered now if its delivery is read-only (hover, navigation, symbols, ...). Everything else (diagnostics, project operations, partial deliveries, automatic/manual selection, cancelled jobs, older generations) is held and processed by the normal poll after the commit, as before.
3. Workspace side effects of an early navigation result (NavigationState cache) are held and applied after the commit only if the result is still fresh against the committed workspace — the same rule the normal delivery path uses.

Tests first: protocol_barriers test completed_navigation_is_delivered_while_a_watcher_batch_reconciles — definition held at the partial-validation barrier, 1,000-file didChangeWatchedFiles batch held at the workspace-file-worker barrier, release the definition, expect its response while the batch is still held.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Citations at d55c0f6: mem::take in spawn_workspace_file_notification (server.rs ~10610); worker completion/commit in event_loop (~10788-10913); workspace_busy gating of jobs.poll (~11021).

RED: `cargo test -p pascal-lsp --features test-support --test protocol_barriers completed_navigation_is_delivered_while` on the original code: panicked 'a result computed before the batch must not wait for reconciliation' (no response within IO_TIMEOUT while the worker barrier was held). GREEN after the change.

Ruling: no ChangeSet refactor. handle_notification_with_control mutates Workspace through workspace.rs paths owned by the later workspace lane; instead the protocol thread compares completed results with the pre-batch WorkspaceRevision and delivers read-only ones during reconciliation (see plan). Equal generations ⇒ dependency_scoped_result_is_fresh would also pass against the pre-batch workspace (every per-record check compares change generations > the result's generation, and open-overlay text only changes with a generation bump). A result whose read set the batch changes is still answered as of the pre-batch state; that matches LSP ordering (the request preceded the notification). Results the batch commit makes stale are only those that complete after the commit, which keep the old rule.

Ruling: the 30 s deadline is unchanged (still cooperative); with read-only results flowing during reconciliation it no longer blocks interactive answers that are already computed. New requests still queue behind the batch, since dispatching needs a workspace snapshot.

Finding: the AC's 1,000-file watcher batch exceeds MAX_FILE_OPERATION_BATCH_ENTRIES (64) and permanently fences analysis once it commits (server.rs `watched-file batch exceeded`); that is TASK-97's scope, noted in the test. The early delivery happens before the commit, so the AC holds.

Added guard test result_older_than_the_reconciling_revision_waits_for_the_commit: a definition computed before an applied didChange completes during a batch, is held (no response within 500 ms) and is rejected as stale after the commit.

Verification: full protocol_barriers 904 passed, 2 failed (TASK-1.2 sixty-sixth-frame timing tests only); server lib tests 91 passed; clippy -D warnings with and without test-support; fmt clean.

Implemented on branch perf/lsp-protocol-commits (worktree .worktrees/lsp-protocol-commits), awaiting review and merge.

Follow-up for the deferred ChangeSet reconciler and dispatch-during-reconciliation: TASK-126

Merged into master as 153ab6b (branch perf/lsp-protocol-commits, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: the file-notification worker mem::takes the Workspace; until it commits, the event loop does not poll analysis results, so already-computed interactive answers wait for the whole watcher batch.

Change (commit 012f7a0): WorkspaceFileNotificationWorker records the WorkspaceRevision (generations + admission fence) it took the workspace at. While it runs, AnalysisJobs::deliver_during_reconciliation drains the result channel: client results whose generations equal that revision and whose delivery is read-only (is_read_only_delivery: hover, completion, navigation, symbols, references, highlights, hierarchies, formatting, code actions, ...) and that are not cancelled, partial, or tied to automatic/manual project selection are sent immediately via send_read_only_analysis_result (now shared with deliver_analysis_result_with_store). Their navigation state is held and applied after the commit only if the result is still fresh. Everything else is held in held_results and processed first by the next normal poll.

Tests: protocol_barriers completed_navigation_is_delivered_while_a_watcher_batch_reconciles (RED before, GREEN after; 1,000-entry batch held at the worker barrier, definition answered meanwhile) and result_older_than_the_reconciling_revision_waits_for_the_commit (guard: older-generation results still wait and fail closed). Full protocol_barriers: 904/906 (two TASK-1.2 timing flakes). Server lib tests 91/91. clippy/fmt clean.

Known limits: not the ChangeSet design from the Approach (see ruling); new requests still queue during reconciliation; a batch over 64 entries still fences analysis at commit (TASK-97).
<!-- SECTION:FINAL_SUMMARY:END -->
