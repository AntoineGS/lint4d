---
id: TASK-18
title: 'LSP-12: Warm cache hits should not read and decode the file first'
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
  - TASK-13
modified_files:
  - crates/pascal-lsp/src/project_cache.rs
  - crates/pascal-lsp/src/workspace.rs
  - crates/pascal-lsp/src/workspace/resolver.rs
  - crates/pascal-lsp/src/navigation.rs
  - crates/pascal-lsp/src/file_watch.rs
priority: high
type: enhancement
ordinal: 18000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-12). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime i/o.

**Problem.** `workspace.rs:7498-7546`, `:7636-7648`: a worker's fresh
`NavigationIndex` reads and decodes a closed source to compute the hash
before it can look up the cached parse. Import lookup
(`:6755-6759`, `:6780-6805`, `:6815-6818`, `:6879-6882`) copies indexed
text and discovers compiled candidates before checking the cache; a hit
clones the resolved graph, report and probes
(`project_cache.rs:200-224`) and re-stats every probe.

**Approach.** Key the first lookup by (path, mtime, len) from one `stat`;
only read the file when the stat key misses. Return `Arc` values from hits
instead of clones. Trust watcher-backed invalidation (LSP-8 reverse map)
for probe freshness; keep a full probe verification path behind a
configuration flag for environments without a watcher.

**Tests first.** Read-count characterization test.

**Depends on.** LSP-8.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Second definition request on the same closed unit performs zero file reads (count via a test hook on the source store).
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Branch perf/lsp-workspace-memory (worktree .worktrees/lsp-workspace-memory), stacked on TASK-13.
Root cause (d55c0f6): the closed-source load (Workspace::load_closed... around workspace.rs:7664, read_disk_source_with_budget) reads+decodes+hashes the file before index_source_with_budget looks up the unit cache by content hash; import lookup hits clone resolved graph/report/probes (load_imports_with_session_cache ~6962) and the cache's extract closures clone probes under the lock.
Steps: 1) Tests first: thread-local disk-read hook (resolver::record_test_disk_read at read_disk_source_with_budget, closed_source_content_hash, LspSourceStore disk loads, probes_hold content reads); test second_definition_on_a_closed_unit_reads_no_files via queries::navigation_from_input (RED expected: Main.pas read again). 2) UnitValue gets DiskOrigin {len, mtime, raw_bytes, text: Arc<str>} (parse's own text when no expansion); ProjectCache::peek_unit_from_disk keyed by (uri, context) + stat; closed load uses it before reading. 3) ImportValue resolved/report become Arc; hits share, dependency loop iterates by reference; Probed trait removes probe clones under the cache lock. 4) Keep probe verification (ruling, follow-up task).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
RED: `cargo test -p pascal-lsp --lib second_definition_on_a_closed_unit_reads_no_files` on the original code: `left: ["/tmp/.tmpSYVtwt/Main.pas"] right: []` — the import cache hit already avoided resolver loads; the only read was the closed-source load of Main.pas.

Ruling: per-hit probe verification is kept (stat-only while stamps hold, no reads). Trusting the watcher instead would serve stale entries: only import entries' directory observations are watched, include/candidate/metadata probes are not, and watches can fail. Follow-up TASK-130 holds the watcher-trust + config-flag part.

Ruling: discover_compiled_units still runs before the import cache lookup because the validated compiled names are part of the import key; moving it needs the key split. Not changed here.

Ruling: the stat fast path is skipped for legacy-route payloads, symlinks, stamps without mtime and oversized files; those take the full read path that authorizes them. A same-length rewrite within the filesystem's mtime granularity is trusted, as the existing disk_stamps fast path already does.

Implemented on branch perf/lsp-workspace-memory (worktree .worktrees/lsp-workspace-memory), awaiting review and merge.

Review note: a same-length rewrite that restores the mtime is not seen by the stat key itself. A change event for the file (client didChangeWatchedFiles or the directory watcher) still evicts the unit entry via mark_source_change -> invalidate_file_contents, so only a silent rewrite without any event is missed. The existing Workspace.disk_stamps fast path has the same blind spot.

End-of-lane: pascal-lsp lib 713 passed; protocol 718 passed; protocol_barriers: only the two TASK-1.2 timing tests failed consistently. sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results timed out in 2 of 3 full runs under load, passes alone (see the LSP-W report).

Merged into master as 16139a2 (branch perf/lsp-workspace-memory, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: a worker's fresh workspace read and decoded every closed source to hash it before it could look up the cached parse; import cache hits cloned the resolved graph and report; cache lookups cloned probe vectors under the lock.

Change (commit 0f90bd1): UnitValue records a DiskOrigin (len, mtime, raw size, decoded text — the parse's own Arc<str> when there is no include expansion). ProjectCache::peek_unit_from_disk finds the entry from one stat; Workspace::cached_closed_source uses it before read_disk_source_with_budget, then indexing hits the unit entry by its stored raw content hash. ImportValue.resolved/report are Arc and shared with the computing request; hits no longer clone them and dependencies are iterated by reference. A Probed trait lets lookups verify probes on the Arc instead of cloning them under the lock.

Tests: second_definition_on_a_closed_unit_reads_no_files (disk-read test hook on the source store and the workspace read sites): RED on the original code (Main.pas read on the second request), GREEN now (no reads); the same test edits Main.pas and checks the changed stamp reads it again. pascal-lsp lib 701 passed; fmt and clippy -D warnings clean.

Limits: probe verification still stats every probe per hit (TASK-130); compiled-unit discovery still precedes the import lookup; the cached text is copied into a String for indexing (TASK-12 scope); interface probes are still cloned from an import hit.
<!-- SECTION:FINAL_SUMMARY:END -->
