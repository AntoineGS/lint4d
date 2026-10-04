---
id: TASK-134
title: Bound and harden the macOS volume case probe and on-disk spelling reads
status: To Do
assignee: []
created_date: '2026-10-04 01:08'
updated_date: '2026-10-04 01:27'
labels:
  - core
  - performance
dependencies:
  - TASK-74
priority: low
ordinal: 136000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-ups from the TASK-74 review (branch fix/macos-case-paths, PR #7):

1. Probe cache cost (crates/pascal-project/src/path_identity.rs, volume_case::is_case_insensitive): one global Mutex<HashMap<PathBuf, bool>> keyed by full path, cleared wholesale at 4096 entries. On large macOS trees every identity check contends on the mutex and the cache thrashes (clear, refill). Options: cache per parent directory with a mount check, a sharded or read-mostly map, gradual eviction (LRU/clock) instead of clear.
2. Probe failure falls back to "case-insensitive" (the macOS default) — document this in the function docs and README, or report it.
3. pathconf(_PC_CASE_SENSITIVE) can block on a stale network mount; identity checks run on request paths. Consider a timeout-free alternative (statfs f_fstypename allow-list) or probing off the request path.
4. crates/pascal-project/src/installations/ide_paths.rs reconcile_ide_path passes its result through path_identity::on_disk_spelling, whose canonicalize/directory walk is not charged to the ProjectReadTracker or the reconciliation budget (check_reconciliation_budget); charge it or fold the spelling fix into the existing walk.
5. crates/pascal-lsp/src/workspace.rs compile_exclude_patterns_with_cancel compiles every exclusion glob twice (as written and case-folding) eagerly, even on Linux where the folding set is never used; compile the folding set lazily as pascal-project's CompiledExclusions does.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Identity checks on macOS do not take a global lock per call, and the cache keeps working sets above its bound without clearing everything
- [ ] #2 The probe's fallback for volumes that cannot answer is documented
- [ ] #3 on_disk_spelling work in IDE path reconciliation is charged to the read tracker / budget, with a test
- [ ] #4 pascal-lsp compiles the case-folding exclusion set only when a case-insensitive path is checked
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From the TASK-74 fix-round re-review: lossy string path keys remain outside the four caches fixed in 17bd4d5 — resolver.rs ~3444 (path_key + replace('\\','/') helper), server.rs ~14711 (watcher_path_key), workspace/rename.rs (path_indices, seen, observation matching). On Linux two paths that only print alike (a\\b vs a/b, non-UTF-8 names) can still collide there; move them to path_identity::path_lookup_key. sort_by_key(path_key) sites only affect ordering.
<!-- SECTION:NOTES:END -->
