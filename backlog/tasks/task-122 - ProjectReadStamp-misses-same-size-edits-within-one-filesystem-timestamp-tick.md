---
id: TASK-122
title: ProjectReadStamp misses same-size edits within one filesystem timestamp tick
status: To Do
assignee: []
created_date: '2026-10-03 23:12'
updated_date: '2026-10-04 00:58'
labels:
  - core
  - correctness
dependencies:
  - TASK-94
references:
  - crates/pascal-project/src/lib.rs
  - crates/pascal-lsp/src/project_cache.rs
priority: low
type: bug
ordinal: 124000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
ProjectReadStamp (crates/pascal-project/src/lib.rs, path_stamp_result) is length + mtime + kind. Freshness probes (pascal-lsp project_cache.rs probes_hold) only reread content once the stamp moved, so an edit that keeps the length and lands in the same mtime tick as the previous write is treated as unchanged.

Seen on Windows CI (TASK-94): project_cache::probe_tests::changed_content_breaks_content_probe wrote "unit A;" then "unit B;" back to back and the probe held, because NTFS stamped both writes with the same tick (~15 ms system timer). Linux and macOS have finer stamps, so the window is smaller but not zero (coarse kernel clocks).

TASK-94 made the test set the mtime explicitly so it tests what it names; the product gap remains. Options: add a file identity/change counter where the platform has one (Linux ctime/inode, Windows file index + change time via GetFileInformationByHandleEx), or treat a stamp younger than the clock granularity as unproven and reread.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A content probe (pascal-lsp project_cache.rs probes_hold) reports a miss when a file is rewritten with different content of the same length and the same modification time, without the test moving the mtime
- [ ] #2 The same holds for project/metadata read freshness checks that compare ProjectReadStamp (pascal-project path_stamp_result callers)
- [ ] #3 Regression tests run on Linux, macOS and Windows CI; the explicit set_modified workaround in project_cache.rs changed_content_breaks_content_probe and peek_unit_misses_when_a_probe_fails is removed
- [ ] #4 Probes on files not modified since well before the probe was recorded still hold without rereading their content (no blanket reread of every probe)
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Also affected (from the TASK-18 review, branch perf/lsp-workspace-memory, 0f90bd1): definition on a closed unit is now served from a cache hit keyed on one file stat (workspace.rs ~8008-8044, project_cache.rs ~2256-2285). A same-length rewrite that keeps the mtime and sends no change event used to be corrected by the next fresh worker read; now it is served from the cache for the server's lifetime. Fixing same-tick detection should cover this path too.
<!-- SECTION:NOTES:END -->
