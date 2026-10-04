---
id: TASK-130
title: Trust watcher-backed invalidation for project-cache probe freshness
status: To Do
assignee: []
created_date: '2026-10-03 23:55'
labels:
  - lsp
  - runtime
dependencies:
  - TASK-13
  - TASK-18
priority: medium
type: enhancement
ordinal: 132000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up from TASK-18 (LSP-12). Its Approach asked to trust watcher-backed invalidation for probe freshness and keep full probe verification behind a configuration flag for environments without a watcher. TASK-18 kept per-hit probe verification (stat-only while stamps hold; reads only when a stamp changed) because the watcher does not cover every probe today:

- `project_cache.rs` `report_probes` registers watches only for import entries' `ResolutionObservation::Directory` paths (`ImportValue::watch_dirs`). Unit entries' include probes (`expansion_probes`), candidate stamps, metadata payloads and project reads are not watched.
- Watches are non-recursive and can fail (`DirectoryWatch::watch` returns false); a failed watch leaves no invalidation source.

To skip verification safely: every Stamp/Content probe path's parent directory (or the file) must be under an active watch for the entry's lifetime, failed watches must force verification for affected entries, and overflow already clears the cache. Then add the configuration flag for verification-always mode.

Done when: a warm hit on an entry whose probes are all watched performs no stat (count via test hook), and an entry with an unwatched probe still verifies.
<!-- SECTION:DESCRIPTION:END -->
