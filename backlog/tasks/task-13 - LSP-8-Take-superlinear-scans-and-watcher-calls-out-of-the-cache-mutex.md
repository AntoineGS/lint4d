---
id: TASK-13
title: 'LSP-8: Take superlinear scans and watcher calls out of the cache mutex'
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
  - crates/pascal-lsp/src/project_cache.rs
  - crates/pascal-lsp/src/file_watch.rs
priority: high
type: enhancement
ordinal: 13000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-8). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime latency.

**Problem.** `project_cache.rs:1907-1945` LRU eviction scans every slot per
victim and, per candidate, scans all pin-owner sets and clones its URI.
`:1950-1976`, `:2089-2128` path invalidation scans every entry's probes and
deduplicates with `Vec::contains`. `file_watch.rs:57-70`, `:125-136`
repeats for each event path. `workspace.rs:12949-12955` calls
watch/unwatch while holding the mutex.

**Approach.** Keep an intrusive LRU order (`VecDeque` of keys or a linked
map), an aggregate pin count per entry, and a reverse map
`path -> HashSet<entry key>`. Collect watcher operations into a `Vec` under
the lock and execute them after releasing it.

**Tests first.** Characterization tests for eviction order and pin
semantics using the existing public cache API, so the refactor is checked.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Eviction and invalidation are O(affected entries). A test with 10,000 entries invalidating one path touches one entry (count via test hook).
- [x] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree .worktrees/lsp-workspace-memory, branch perf/lsp-workspace-memory (stacked lane: TASK-13, 18, 10, 11, 12, 17).

Root cause (project_cache.rs, cites from d55c0f6):
- evict_to_budget scans every slot per victim and, per candidate, every pin owner set (is_pinned clones the URI); pinned_bytes/has_room scan all slots x all pins.
- invalidate_path_with_parent scans every entry's probes; affected dedup with Vec::contains. declared_unit_name scans all slots.
- file_watch.rs drain/apply_watch_event dedup with Vec::contains per path.
- acquire_watch/release_watches/recover_poisoned_state call DirectoryWatch::watch/unwatch while holding the cache state mutex.

Steps:
1. Tests first (on original code): eviction-order + pin characterization through the public API (LRU across touches, multi-owner pins, unpin re-exposes, pinned bytes/has_room); a 10,000-entry invalidation test counting examined entries via a cfg(test) counter (RED: examines 10,000); a fake DirectoryWatch asserting the state mutex is free during watch/unwatch (RED: held).
2. State gets indexes maintained by insert_ready/remove_ready: lru BTreeMap<last_used, Key> of unpinned ready entries, aggregate pin_counts (uri -> fingerprint -> owners) and pinned_bytes, reverse maps observed-path -> keys (Stamp/Content probes, exact or parent match) and exact-path -> keys (own path, overlay probes), uri -> keys for declared_unit_name.
3. Watcher moves to its own mutex with the registered set; state records pending watch directories; sync_watches runs after the state lock is released and reconciles each pending directory against current watch_counts. Lock order watcher -> state, documented at the locks.
4. file_watch.rs dedup with HashSet.
5. fmt, clippy, focused pascal-lsp tests (project_cache, file_watch, warmer).
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Citations at d55c0f6: eviction/pins project_cache.rs evict_to_budget/is_pinned/pinned_bytes (~1907-1948), watch calls acquire_watch/release_watches/recover_poisoned_state, invalidation invalidate_path_with_parent (~2089), declared_unit_name (~1734); file_watch.rs apply_watch_event. The workspace.rs watch/unwatch citation is the cache invalidation reached from mark_source_change_with_control; the fix is inside the cache.

Ruling: LRU order is a BTreeMap<last_used, Key> of unpinned ready entries (clock values are unique) rather than a VecDeque — touches need removal from the middle, and it keeps eviction O(log n) per victim without skipping pinned entries.

Ruling: reverse index is split into `observed` (stamp/content probes: exact or parent match) and `exact` (own path, overlay probes: exact only) to preserve the old matching rules exactly.

Ruling: watcher moved to its own mutex (lock order watcher -> state). Watch-count transitions are queued under the state lock; sync_watches applies the current count of each queued directory after release, so racing syncs converge.

Implemented on branch perf/lsp-workspace-memory (worktree .worktrees/lsp-workspace-memory), awaiting review and merge.

Merged into master as 16139a2 (branch perf/lsp-workspace-memory, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: project cache eviction scanned every slot per victim (and every pin set per candidate), path invalidation scanned every entry's probes, declared_unit_name scanned all slots, and DirectoryWatch::watch/unwatch ran under the cache state mutex.

Change (commit cb006d7): State keeps indexes updated by insert_ready/remove_ready: BTreeMap LRU of unpinned ready entries, aggregate pin counts with pinned_bytes, reverse maps path -> keys (observed: stamp/content probes; exact: own path and overlay probes), uri -> keys. Eviction pops the oldest unpinned entry; invalidation looks up the path (and its parent for membership changes). The watcher lives in its own mutex; watch-count transitions are queued under the state lock and applied by sync_watches after release (lock order watcher -> state, documented on Inner). file_watch dedups affected URIs with a HashSet.

Tests: characterization tests for LRU order across hits/peeks, multi-owner pins over all layers, repin, fingerprint-scoped pins, pinned bytes (passed on the original code, pass now). Structural tests RED on the original code (10,000-entry invalidation examined 10000, eviction examined 10001, watcher called with lock held, incl. poison recovery) and GREEN now (examined 1, lock free). pascal-lsp lib: 700 passed. fmt and clippy -D warnings clean.

Limits: overflow invalidation, project switch and stats() still walk all slots (whole-cache operations). A poison recovery inside a read-only call defers its unwatch to the next mutating call.
<!-- SECTION:FINAL_SUMMARY:END -->
