---
id: TASK-12
title: 'LSP-6: Stop copying overlays and inputs per request and per import lookup'
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
  - crates/pascal-lsp/src/workspace.rs
  - crates/pascal-lsp/src/workspace/rename.rs
  - crates/pascal-lsp/src/workspace/resolver.rs
  - crates/pascal-lsp/src/workspace/queries.rs
  - crates/pascal-lsp/src/workspace/codeactions.rs
priority: high
type: enhancement
ordinal: 12000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-6). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime memory and cpu.

**Problem.** Each analysis dispatch captures a workspace input then clones it
again for validation (`server.rs:5565`, `:5747`). Worker workspace
reconstruction clones every open document's `String`
(`workspace.rs:3205-3265`). `overlay_inputs()` at `workspace.rs:7814-7826`
copies all open editor text for every indexed unit and every import lookup;
an import miss creates another whole input. `warmer.rs:543`, `:683` repeat
the pattern.

**Approach.** Store open document text as `Arc<str>` with a version; build
one `Arc<OverlayMap>` per workspace revision and pass it by reference into
index construction and import lookup. Replace the validation clone with a
compact `RevalidationInput` containing only hashes and versions.

**Tests first.** Unit test that `overlay_inputs` (or its replacement) called
twice for the same revision returns the same `Arc`.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 With 50 open documents, a references request allocates overlay text at most once (assert via a counting allocator in a test, or via `Arc::strong_count` observations in a unit test).
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Branch perf/lsp-workspace-memory (worktree .worktrees/lsp-workspace-memory).
Root cause (d55c0f6): OpenDocument.text and OverlayInput.text are String, so analysis_input()/revalidation_input() (rename.rs), Workspace::from_analysis_input, every overlay_inputs() call (per indexed unit and per import lookup, workspace.rs), LspSourceStore::from_input and overlay loads (resolver.rs) copy every open document's text.
Steps: 1) Test first: 50 open documents; overlay text pointers from two overlay_inputs() calls, analysis_input().overlays, a worker built from the input and its overlay_inputs() must all equal the open document's text pointer (RED: copies). 2) OpenDocument.text: Option<Arc<str>>, OverlayInput.text: Arc<str>; LoadedSource for overlays shares the Arc (Arc<[u8]>::from(Arc<str>)). 3) overlay_inputs() returns Arc<HashMap>, cached in the workspace and reused while every open document's text Arc and version still match (validation by pointer compare, so no mutation site can forget to invalidate). 4) Keep server.rs untouched (other branches); its input clone now copies reference counts, not text.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED: open_document_text_is_shared_by_inputs_workers_and_lookups on the original code: 'first lookup copied file:///tmp/.tmpJCUqVt/U0.pas' (left 0x7f62001c62d0, right 0x7f62000040f0).

GREEN: same test (plus Arc::ptr_eq of two overlay_inputs() calls) passes; overlay_inputs_follow_document_changes covers change/version-only/close invalidation. pascal-lsp lib 713 passed; fmt and clippy clean.

Ruling: the overlay map cache validates against open_documents (text Arc identity + version + count) on each call instead of being reset at mutation sites: O(open documents) pointer compares, and tests or future code that mutate open_documents directly cannot leave a stale map.

Ruling: server.rs is not changed (perf/lsp-protocol-commits and fix/notification-fence edit it). Its dispatch/validation clones of WorkspaceInput now copy Arc text handles; a revalidation_input() with only overlays/options already exists in rename.rs.

Limits: indexing an open document still copies its text into the parse (NavigationIndex::update takes String; Document::parse_with_cancel builds its own Arc<str>), and SourceRecord.text for analysis records is still a String copy. An import miss still builds a whole WorkspaceInput (now without text copies). These are outside the overlay-copy AC.

Implemented on branch perf/lsp-workspace-memory (worktree .worktrees/lsp-workspace-memory), awaiting review and merge.

Merged into master as 16139a2 (branch perf/lsp-workspace-memory, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: every analysis input, worker workspace, overlay_inputs() call (per indexed unit and import lookup) and resolver overlay load copied all open documents' text.

Change (commit 3cdcf4f): OpenDocument.text is Option<Arc<str>> and OverlayInput.text is Arc<str>; inputs, worker reconstruction, revalidation input and LspSourceStore overlay loads share the Arc (LoadedSource bytes via Arc<[u8]>::from(Arc<str>)). overlay_inputs() returns Arc<HashMap<..>> cached on the workspace and reused while all open documents keep the same text Arc and version.

Tests: open_document_text_is_shared_by_inputs_workers_and_lookups (50 open documents; pointer identity across two lookups, analysis input, worker and worker lookups; same Arc for two lookups) RED on the original, GREEN now; overlay_inputs_follow_document_changes. pascal-lsp lib 713 passed; fmt and clippy clean.

Limits: indexing an open document still copies its text into the parse; analysis SourceRecord text is still copied; server.rs dispatch/validation still clones WorkspaceInput (now text-free) — not touched because other branches edit server.rs.
<!-- SECTION:FINAL_SUMMARY:END -->
