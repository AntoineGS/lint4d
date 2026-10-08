---
id: TASK-142
title: >-
  Opening or closing an unchanged document does not evict other files' cache
  entries or re-index open files
status: Done
assignee:
  - '@OpenCode'
created_date: '2026-10-08 18:08'
updated_date: '2026-10-08 18:44'
labels:
  - lsp
  - runtime
  - performance
dependencies: []
priority: high
type: bug
ordinal: 142000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Navigating with gd into a library unit and back (ChainDriveAPI: CDAPI.Database.Centrale.pas -> mormot.db.sql.pas via TSqlDBConnectionProperties, then Ctrl-O) re-indexes CDAPI.Database.Centrale.pas from scratch after the mormot crawl finishes, although no file changed. Requests issued meanwhile can fail with "workspace metadata or membership changed while resolving ...; retry the request".

Cause, established with the PASCAL_LSP_TRACE timeline (commit 141353e): didOpen of a file with no existing overlay calls mark_source_change with include_parent = true (workspace.rs accept_open_document), because an open overlay counts as part of its directory listing; didClose does the same (close_document). The project cache then evicts every entry that observed the parent directory (8 URIs for mormot.db.sql.pas) and bumps the invalidation epoch. The bump makes the warmer reject the in-flight crawl's stores ("cache rejected Interface ... from epoch 1 at epoch 2") and marks every other open file's completed crawl stale, so the next snapshot's closure misses trigger a full re-crawl.

The overlay-membership rule must keep holding where it matters: opening a document whose file does not exist on disk, or whose text differs from the disk file, still changes what directory-dependent resolution may observe.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [x] #1 Opening a document whose text matches its file on disk does not evict cache entries of other files that only observed its directory, and does not mark other open files' completed crawls stale; regression test fails on the original code
- [x] #2 Opening a document that does not exist on disk, or whose text differs from disk, still invalidates entries that depend on it (existing behaviour kept, covered by tests)
- [x] #3 pascal-lsp tests, cargo fmt and clippy pass
- [x] #4 Closing a document whose file exists on disk no longer evicts entries that only observed its directory (exact dependents are still invalidated, because entries computed while it was open recorded it as an overlay)
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
User chose approach A (2026-10-08).
1. RED (project_cache probe_tests): a Content probe holds when an overlay exists for its path and the overlay bytes hash to the probe's content_hash; still fails when they differ.
2. RED (workspace tests): with a warmed/stored entry for another unit that observed the directory and depends on the file's content, didOpen of the file with text identical to disk leaves those entries Ready and the cache invalidation_epoch unchanged.
3. RED: closing an open document whose file exists on disk does not evict an entry that only observed its directory; opening a document with differing text, or one that does not exist on disk, still evicts dependents (existing behaviour, keep/extend tests).
4. GREEN: probes_hold Content arm compares overlay_content_hash(overlay.text) to content_hash instead of failing outright. accept_open_document: when there was no overlay and the text's bytes equal the disk file (not tombstoned in deleted_overrides), skip the project-cache invalidation but keep the rest of mark_source_change (generation bookkeeping, expansion dependents). Directory listing rule: an overlay of a file listed on disk does not change its directory listing, so open/close of such a file use include_parent = false.
5. fmt, clippy, pascal-lsp tests; rerun the headless ChainDriveAPI scenario with PASCAL_LSP_TRACE and confirm no Centrale re-crawl after gd into mormot.db.sql.pas.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Research (perf/cache-thrash, after TASK-141 cf30c99):
- accept_open_document -> mark_source_change(uri, !had_overlay) -> ProjectCache::invalidate_path evicts exact + observed(path) + observed(parent) and bumps invalidation_epoch. Even invalidate_file_contents (no parent) bumps the epoch, and the epoch bump is what makes Warmer::request_closure_crawl accept a new crawl for every other open file, and makes put_interface_imports reject in-flight crawl stores.
- probes_hold: a Content probe fails whenever an overlay exists for its path (project_cache.rs:273), even when the overlay text equals the disk bytes. So entries depending on the opened file's content are invalid at lookup regardless of eviction; skipping invalidation alone would leave them failing while the unchanged epoch suppresses the crawl that would recompute them (fenced closure).
- Entries computed while a document is open carry Probe::Overlay / SourceRevision::Overlay, which fail once it closes, so close needs invalidation unless resolution never recorded the document as an overlay.
- SourceRevision kind is not cosmetic: legacy-route authorization (resolver.rs:1422, workspace.rs:7183) and disk_size/disk_stamps differ between Disk and Overlay.
- With TASK-141 in place the post-navigation Centrale re-crawl measured ~6 s (was ~33 s); it still happens.
Awaiting user choice between: (A) Content probes accept a byte-identical overlay + skip cache invalidation on didOpen when text == disk; (B) only drop parent-directory eviction for files that exist on disk (epoch still bumps, re-crawl remains); (C) treat an unedited open document as transparent to resolution until its first change.

RED on cf30c99: disk_content_probe_follows_the_overlay_once_the_file_is_open failed at the identical-overlay assertion (project_cache.rs:445); opening_an_unchanged_dependency_keeps_cached_entries_and_requests_no_crawl failed at the epoch assertion; closing_a_document_listed_on_disk_keeps_entries_that_only_observed_its_directory failed at Main's Import entry. The two existing-behaviour tests (differing text, missing on disk) passed before and after. All five pass with the fix; full pascal-lsp + pascal-project suites, fmt and clippy clean (load ~20).
Headless ChainDriveAPI scenario with the worktree release binary and PASCAL_LSP_TRACE (/tmp/lsp-repro/events142.log, trace142.log): opening mormot.db.sql.pas via gd logged no cache invalidation and the epoch stayed 0; no Centrale rewarm/re-index after the mormot crawl (8 s) ended; gd requests during the mormot crawl answered in 276-379 ms. Before TASK-141/142: gd 8.9-10.7 s and a ~33 s Centrale re-index.
Accepted risk: a cache hit computed from disk keeps SourceRevision::Disk for a document that is open with identical bytes; legacy-route authorization then comes from the disk resolution, which reflects the unchanged file.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Opening a document in the editor (for example gd into a library unit) invalidated the project cache as if the directory changed: it evicted every entry that observed the parent directory and bumped the invalidation epoch, which made the warmer reject in-flight stores and re-crawl every other open file. Separately, a disk Content probe failed whenever the path had an overlay, even with identical bytes.

Changes (approach A, chosen by the user):
- project_cache probes_hold: a Content probe on an open document holds when the overlay's bytes hash to the probe's content hash (the overlay stays authoritative; different text still fails).
- workspace accept_open_document: opening a file listed on disk with byte-identical text leaves the cache untouched (no eviction, no epoch bump); differing text evicts only content dependents; opening a file not on disk (or tombstoned) still invalidates the directory listing. Source-generation bookkeeping is unchanged.
- workspace close_document: closing a file listed on disk no longer evicts parent-directory observers; entries that recorded its overlay are still evicted (AC #2 narrowed with the user, since those entries carry Overlay probes).
- mark_source_change_with_control takes a CacheInvalidation (Listing / Contents / Unchanged) instead of a bool.

Tests: disk_content_probe_follows_the_overlay_once_the_file_is_open (replaces the old fails-once-open test), opening_an_unchanged_dependency_keeps_cached_entries_and_requests_no_crawl, closing_a_document_listed_on_disk_keeps_entries_that_only_observed_its_directory, opening_a_dependency_with_other_text_still_evicts_its_dependents, opening_a_document_missing_on_disk_still_evicts_directory_observers. Full suites, fmt and clippy pass. Real-project trace: no re-index of CDAPI.Database.Centrale after gd into mormot.db.sql and back; gd 0.3 s during the crawl.

Follow-up not done: closing a never-edited document still evicts its overlay-dependent entries and bumps the epoch, so a re-crawl can follow a :bd.
<!-- SECTION:FINAL_SUMMARY:END -->
