---
id: TASK-143
title: >-
  Closing a never-edited document does not evict cache entries or re-index other
  open files
status: To Do
assignee: []
created_date: '2026-10-08 19:02'
labels:
  - lsp
  - runtime
  - performance
dependencies: []
references:
  - TASK-142
  - TASK-141
priority: medium
type: bug
ordinal: 143000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
After TASK-142, opening a document whose text matches its file on disk leaves the project cache untouched, so go-to-definition into a library unit no longer re-indexes the other open files. Closing that document again (for example :bd after browsing a library unit) still does: entries computed while it was open recorded it as an overlay (Probe::Overlay / SourceRevision::Overlay), so close_document evicts them and bumps the cache invalidation epoch. The epoch bump marks every other open file's completed warmer crawl stale, and the next snapshot's closure misses trigger a re-crawl. With TASK-141 in place such a re-crawl took about 6 s on ChainDriveAPI (CDAPI.Database.Centrale.pas after closing mormot.db.sql.pas), so the cost is smaller than before but still visible as unexpected indexing.

Constraints to keep: an open document's overlay is authoritative over disk (docs/shared-resolver-architecture.md, "LSP overlays, invalidation, and diagnostics"); a document that was edited, or whose disk file changed or disappeared while it was open, must still invalidate the entries that depend on it on close. SourceRevision kind is not cosmetic: legacy-route authorization and disk size/stamp bookkeeping differ between Disk and Overlay revisions.

The PASCAL_LSP_TRACE timeline (commit 141353e) logs cache invalidations, epoch bumps and warmer rewarm reasons, and the headless nvim scenario used for TASK-141/142 can reproduce the open/close sequence.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Closing a document that was never edited and whose disk file still holds the opened bytes does not bump the cache invalidation epoch or make other open files' completed crawls stale; regression test fails on the original code
- [ ] #2 Cached entries that depend on that document remain usable after the close (no re-parse of it or its dependents)
- [ ] #3 Closing a document that was edited, or whose disk file changed or was deleted while it was open, still invalidates the entries that depend on it (covered by tests)
- [ ] #4 pascal-lsp tests, cargo fmt and clippy pass
<!-- AC:END -->
