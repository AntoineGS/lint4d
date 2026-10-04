---
id: TASK-8
title: 'LSP-9: Make the interactive slot actually isolate interactive latency'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 02:39'
labels:
  - arch-review
  - lsp
  - runtime
  - agent-todos
milestone: m-1
dependencies: []
modified_files:
  - crates/pascal-lsp/src/project_cache.rs
  - crates/pascal-lsp/src/server.rs
  - crates/pascal-lsp/src/workspace.rs
  - crates/pascal-lsp/tests/protocol.rs
priority: high
type: enhancement
ordinal: 8000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-9). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime latency.

**Problem.** `server.rs:62-65` reserves one of three slots for interactive
work, but an interactive worker can wait indefinitely on a cache
`Computing` claim owned by a bulk job or the warmer
(`project_cache.rs:1335-1339`, `:1838-1847`, `warmer.rs:709-719`). The claim
key excludes the input hash, so a newer document revision waits behind the
older computation. Repository project listing and broad hierarchy/call
operations are classified interactive (`server.rs:953-979`, `:7774-7786`).
`AGENT_TODOS.md` also notes Neovim never cancels superseded requests.

**Approach.**
1. Include the input hash in the claim key; a mismatching claim is not
   waited on.
2. Interactive workers wait on a claim at most N ms (start with 50), then
   compute locally without caching.
3. Reclassify workspace-mode `prepareRename`, project listing, subtypes and
   incoming calls as bulk.
4. Cancel a queued request when a newer request of the same method and
   document arrives (server-side supersession).

**Tests first.** The protocol test above using the `test-support` barrier
harness so it is deterministic, not sleep-based.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 A protocol test starts a bulk references request on a large fixture, then sends `textDocument/definition`; definition answers within 200 ms on the test machine.
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/lsp-protocol-commits, branch perf/lsp-protocol-commits (stacked on TASK-14/15/16).
Root cause: project_cache::lookup waits on any Computing slot for the key (layer, uri, context fingerprint) — no input hash, no time bound — so an interactive worker can wait for a bulk job's or the warmer's claim indefinitely, including for an older revision. Workspace-scanning requests (ListProjects, subtypes, incoming calls) are classified interactive and take the reserved slot. Superseded cursor requests pile up.
Steps (TDD each): 1) project_cache unit tests: other-revision claim not waited on; limited wait computes uncached and leaves the claim. 2) Computing{input_hash}; detached claims; thread-local wait limit set to 50 ms by interactive analysis workers. 3) Protocol test with a test-support import-claim barrier holding the bulk references worker's claim: definition answers meanwhile. 4) Reclassify ListProjects / TypeHierarchySubtypes / IncomingCalls as bulk (unit test). 5) Queued same-kind cursor requests (hover, signatureHelp, documentHighlight, prepareRename) for the same document are cancelled by a newer one (protocol test).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From `AGENT_TODOS.md` (section "Workspace-Wide Performance"), folded into this task:

Scheduling: long workspace requests can occupy all analysis workers,
including the slot reserved for interactive requests (`prepareRename`
counts as interactive). Neovim never cancels superseded requests, so
repeated keypresses pile up. Consider classifying workspace-mode
`prepareRename` as bulk, cancelling superseded duplicates, and a work
budget per request.

Citations at d55c0f6: claim wait project_cache.rs:1838-1848 (lookup Computing arm), Slot::Computing :1361, Key :1335; priorities server.rs:953-996 (AnalysisPriority::for_request); dispatcher server.rs:7810 (AnalysisJobs::pump).

RED/GREEN: project_cache a_claim_for_other_input_is_not_waited_on (RED: 'a newer revision must not wait behind an older claim: Timeout') and limited_waits_compute_without_caching_and_leave_the_claim (RED: waiter got a Hit after the claim stored) — GREEN after. protocol_barriers definition_does_not_wait_out_a_bulk_references_cache_claim: RED with the interactive wait limit disabled (original lookup behaviour) — 'definition must not wait for the bulk job's cache claim'; GREEN: definition answered in ~61 ms (3 runs: 61.66, 61.72, 61.40 ms) while the references worker held the import claim at the barrier. reverse_hierarchy_requests_do_not_take_the_interactive_slot RED (left: Interactive, right: Bulk) with the old classification, GREEN after. queued_hover_is_replaced_by_a_newer_hover_for_the_same_document RED (receive timeout: older hover never cancelled) with supersession disabled, GREEN after.

Ruling: the AC's 200 ms is measured and recorded (~61 ms = the 50 ms wait bound + compute) rather than asserted, to avoid a tight wall-clock assertion under parallel test load; the test asserts the definition answers while the bulk claim is still held, which is deterministic.

Ruling: workspace-mode prepareRename stays interactive. Its mode (SnapshotMode::Workspace vs Local) is only known inside the worker after binding classification (rename.rs prepare_from_input), so classifying all prepareRename as bulk would also delay the local case. It now benefits from the bounded claim wait and from queued-prepareRename supersession instead.

Ruling: supersession cancels only *queued* requests of the same kind for the same document (hover, signatureHelp, documentHighlight, prepareRename), with -32800 'request superseded by a newer request of the same kind for the document'. Running jobs are left alone; definition/completion are not included (a client may legitimately want both answers).

Concurrency: no new locks. The claim-wait limit is a thread-local set at the start of interactive PascalLspAnalysis workers; detached claims never touch the slot table on store/drop.

Verification: pascal-lsp lib (test-support) 697 passed, 1 ignored; protocol_barriers 906 passed, 2 failed (TASK-1.2 timing tests only); navigation 317, rename 94, project 88, compiled_dcu 8, build_context_freshness 2 passed; clippy -D warnings with and without test-support; fmt clean.

Implemented on branch perf/lsp-protocol-commits (worktree .worktrees/lsp-protocol-commits), awaiting review and merge.

Follow-up for workspace-mode prepareRename reclassification (Approach item 3): TASK-127

Fix round 1 (commit cd102f3): replaced queued requests are now cancelled only after the newer request is admitted (attach path and new-job path), so a rejected request no longer costs the client the older answer. Regression test server::tests::rejected_newer_hover_does_not_cancel_the_queued_one — RED before ('the full admission budget rejects the newer hover: ()' because cancelling the older freed its slot), GREEN after. The pre-existing version-based supersession (master behaviour) still runs before admission; left unchanged. definition_does_not_wait_out_a_bulk_references_cache_claim now also asserts the barrier marker has exactly one entry (the references job's claim).

Merged into master as 153ab6b (branch perf/lsp-protocol-commits, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: the reserved interactive worker could wait indefinitely on a project-cache claim owned by a bulk job or the warmer (even for an older revision of the input, since the claim key lacked the input hash); workspace-scanning requests took the interactive slot; superseded cursor requests piled up.

Change (commit 7488fba):
- project_cache: Slot::Computing records input_hash; a lookup for another revision returns a detached claim (compute uncached; store/drop leave the slot). A thread-local claim-wait limit (limit_claim_waits_on_this_thread) makes lookups give up after the limit with a detached claim.
- server: interactive PascalLspAnalysis workers set the limit to INTERACTIVE_CLAIM_WAIT (50 ms). ListProjects, TypeHierarchySubtypes and IncomingCalls are now Bulk. Queued hover/signatureHelp/documentHighlight/prepareRename requests are cancelled (-32800) when a newer request of the same kind arrives for the same document.
- test-support: PASCAL_LSP_TEST_IMPORT_CLAIM_BARRIER holds a non-interactive thread that owns an import claim.

Tests: 2 project_cache unit tests, 1 server unit test, 2 protocol_barriers tests (definition answered in ~61 ms while a bulk references job held the claim; queued hover replacement), all RED before / GREEN after. Lib 697 passed; protocol_barriers 906/908 (TASK-1.2 flakes); other pascal-lsp targets green; clippy/fmt clean.

Known limits: workspace-mode prepareRename stays interactive (mode only known in the worker); the 200 ms target is measured (~61 ms) not asserted; detached computations are not cached, so repeated interactive requests during a long bulk claim recompute that slot.
<!-- SECTION:FINAL_SUMMARY:END -->
