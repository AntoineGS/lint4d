---
id: TASK-136
title: >-
  Check project cache path indexes against Windows path spelling (drive-letter
  and case differences)
status: To Do
assignee: []
created_date: '2026-10-04 03:59'
labels:
  - lsp
  - windows
dependencies:
  - TASK-13
priority: medium
type: bug
ordinal: 138000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found while fixing TASK-135 (not a cause of its failures).

ProjectCache::invalidate_path_with_parent (crates/pascal-lsp/src/project_cache.rs, ~line 3382) looks paths up in the TASK-13 HashMap indexes state.exact / state.observed with the exact Path it is given. The entry side comes from key.uri.to_file_path() and probe paths (indexed_paths, ~line 2934).

On Windows (and macOS case-insensitive volumes) a watcher event path can differ in spelling from the URI-derived path: letter case of components, and possibly the drive letter (clients often send file:///c%3A/...). Pre-TASK-13 the linear scan compared with Path ==, which on Windows compares parsed components; a HashMap lookup also needs equal hashes, and Path's Hash on Windows hashes the bytes, so c:\x and C:\x might be == yet miss the index. Component case differences were never matched by either version.

To do: verify whether the server normalises URI and watcher spellings before they reach the cache (server/uri_spelling.rs, file_watch.rs:131) and whether Path hash/eq agree for drive-letter case on Windows; if a miss is possible, key the indexes by pascal_project::path_identity::path_lookup_key (as workspace.rs does) and add a regression test using path_identity's case-insensitive test hook.
<!-- SECTION:DESCRIPTION:END -->
