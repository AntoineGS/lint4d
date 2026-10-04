---
id: TASK-14
title: 'LSP-7: Validate partial-result chunks once, not per chunk'
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
dependencies: []
modified_files:
  - crates/pascal-lsp/src/server.rs
  - crates/pascal-lsp/tests/protocol.rs
priority: high
type: enhancement
ordinal: 14000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-7). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime.

**Problem.** `server.rs:8213-8285`, `:8329-8359`, `:8388-8418`: every
partial chunk (at most 128 items or 64 KiB) spawns an OS thread that
revalidates the complete source-record set. Delivery cost is
chunks x full-read-set validation, and these threads are outside the
`MAX_ANALYSIS_JOBS` limit (`server.rs:62-65`). A slow validation at the
front blocks later chunks.

**Approach.** Compute one dependency-scoped revision token when the result
is produced; each chunk compares the token to the current workspace
generation (an integer compare). Deliver chunks from a single bounded
executor thread.

**Tests first.** The counting test above against current code (expect 50).

**Depends on.** LSP-19 is the same idea for whole results; do LSP-7 first,
it is smaller.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A test producing 50 chunks performs validation work once (count via a test hook on the validation function).
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/lsp-protocol-commits, branch perf/lsp-protocol-commits (from master d55c0f6).

Root cause: AnalysisJobs::start_partial_delivery (server.rs, grep `fn start_partial_delivery`) spawns a PascalLspPartialValidation thread that runs rename::revalidate_revalidation_input over the whole read set, and pump_partial_deliveries calls validation.request() again after every chunk, so a 50-chunk delivery runs the full read-set validation 51 times on 51 OS threads. pump_partial_deliveries also returns when the front delivery's validation is pending, so one slow validation stalls every other delivery.

Steps:
1. Tests first: unit test in server.rs that drives start_partial_delivery + pump_partial_deliveries for a 50-chunk references payload and counts validation runs through a counter on PartialDeliveryValidation (expect 51 on old code).
2. Revision token: capture (source_generation, configuration_generation) once when the delivery starts; every chunk compares it with the workspace (integer compare + admission fence), as today.
3. Run the read-set validation once per delivery, after the last chunk and before the first successful final response, so an unnotified disk change between chunks still fails the request closed (protocol tests unnotified_disk_change_between_*_chunks_fails_closed).
4. Pump: a delivery waiting on its validation is rotated to the back instead of blocking the others.
5. Keep the per-delivery validation thread (now one per delivery instead of one per chunk); it stays inside the existing recipient admission bound (MAX_PARTIAL_VALIDATION_RETIREMENTS).

Tests: new unit test; protocol tests with partial/chunk/validator in their names; server lib tests for partial delivery.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Citations at d55c0f6: validation thread `PartialDeliveryValidation::request` (server.rs ~4190), `start_partial_delivery` (~8062), `pump_partial_deliveries` (~8322), MAX_ANALYSIS_JOBS at server.rs:62-67.

RED: `cargo test -p pascal-lsp --lib fifty_chunk_partial_delivery` on the original code: `left: 51, right: 1` (one validation at delivery start plus one per chunk). GREEN after the change: 1.

Ruling: the read-set validation runs once per delivery AFTER the last chunk (before the first successful final response), not at the start — protocol tests `unnotified_disk_change_between_{workspace_symbol,reference}_chunks_fails_closed` lock in that an unnotified disk change during delivery must fail the request; the worker already validated the read set when the result was produced (server.rs `rename::revalidate_input` after the worker barrier), so a start-of-delivery pass was redundant.

Ruling: the 'revision token' is the (source_generation, configuration_generation) pair the delivery already captured, compared per chunk together with the admission fence. Partial payloads (references, workspace symbols, workspace diagnostics) were already generation-exact per chunk; making them dependency-scoped is LSP-19 (TASK-36) territory.

Ruling: kept one validation thread per delivery (spawned lazily, once) instead of a single shared executor thread. A shared executor would serialise independent deliveries behind one slow validation (the head-of-line problem this task names) and contradicts `cancelled_partial_validators_are_bounded_until_they_retire`, whose barrier holds every validator. Thread count is now bounded by deliveries + retirements (MAX_PARTIAL_VALIDATION_RETIREMENTS, which is the recipient admission budget), not by chunks.

Test change: `large_ordinary_results_survive_temporary_control_byte_pressure` cancelled the flood delivery after draining the ordinary results and waited for its response; delivery is now fast enough that the flood completes during the drain (its success response was swallowed by the `_ => {}` arm), so the test now records the flood's completion and only cancels if it is still running. Verified passing 3/3 after the change; it passed on the original code too (2/2).

Full `--features test-support --test protocol_barriers` run under load: 898 passed, 6 failed — the two TASK-1.2 sixty-sixth-frame timing tests plus references_and_highlights_enforce_exact_10000_entry_boundaries, sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results, source_bearing_mixed_roots_and_repeated_includes_deduplicate_physical_results, source_bearing_repeated_include_deduplicates_before_reference_cap, all receive-timeouts (protocol.rs:1208) that pass when rerun in isolation (5/5 ok).

Implemented on branch perf/lsp-protocol-commits (worktree .worktrees/lsp-protocol-commits), awaiting review and merge.

Fix round 1 (commit 2838b9e). Ruling: re-run rather than reword. PartialDeliveryValidation tracks chunks_sent, started_at and passed_at, and reuses a pass only if it covers every chunk sent; otherwise it runs once more, at most one extra pass per recipient and none in the normal round-robin case (fifty-chunk test still counts 1). Regression test server::tests::chunks_sent_after_a_validation_started_are_validated_again: RED 'left: 1, right: 2', GREEN after. Doc comment updated to state the guarantee precisely.

Merged into master as 153ab6b (branch perf/lsp-protocol-commits, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: every partial-result chunk spawned a thread that re-validated the whole source-record set against the filesystem (51 validations for a 50-chunk delivery), and pump_partial_deliveries stalled every delivery while the front one's validation was pending.

Change (commit be90681): PartialDeliveryValidation is created idle and runs once, via check(), when a recipient has received its last chunk and before the first successful final response; chunks themselves are guarded by the generation pair captured at delivery start plus the admission fence (integer compares). A delivery waiting on its validation rotates to the back of the queue. The validation thread is spawned once per delivery instead of once per chunk.

Tests: new unit test server::tests::fifty_chunk_partial_delivery_validates_the_read_set_once (RED 51, GREEN 1). Partial-delivery protocol tests (unnotified disk change between chunks, source change between chunks, cancellation, coalesced recipients, retiring validators, stale workspace diagnostics) pass. large_ordinary_results_survive_temporary_control_byte_pressure adjusted for faster delivery (see notes). Server lib tests (91) pass; full protocol_barriers: 898/904 with only load-related timeouts, which pass in isolation. clippy -D warnings (with and without test-support) and fmt clean.

Known limits: a stale chunk can still reach the client before the final error (as before, the old per-chunk check also left a window between validation and send). Validation threads still sit outside MAX_ANALYSIS_JOBS but are now bounded by the partial delivery/retirement admission budget rather than by chunk count.
<!-- SECTION:FINAL_SUMMARY:END -->
