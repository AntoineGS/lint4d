---
id: TASK-121
title: >-
  Navigation results ship a state delta and validate compiled bindings on the
  worker
status: To Do
assignee: []
created_date: '2026-10-03 23:10'
labels:
  - lsp
  - runtime
milestone: m-2
dependencies:
  - TASK-16
priority: medium
type: enhancement
ordinal: 123000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Follow-up to TASK-16 (LSP-13), which made the protocol-thread freshness check proportional and memoised the compiled-binding validation in apply_navigation_state. Two parts of LSP-13's Approach are still open:

1. **Full state copy per navigation result.** `Workspace::navigation_state` (crates/pascal-lsp/src/workspace.rs, grep `pub(crate) fn navigation_state`) clones the worker's whole `contexts`, `document_contexts`, `open_document_contexts`, `document_owners`, `compiled_units` and `owner_last_used` maps into every Navigation result, and `apply_navigation_state` merges all of them on the protocol thread. The cost tracks the workspace size, not the request. Workers should send only the entries they created or changed.
2. **Filesystem work on the protocol thread.** `apply_navigation_state` still runs `context_state_is_fresh_with_cancel` (stats every watched path of a context) and `current_importer_source_hash` (reads and hashes up to 8 MiB of importer source) for every distinct context and importer that holds a compiled provider binding, on every navigation delivery. Bindings are checked again when they are used (`compiled_virtual_document_snapshot` + `read_compiled_virtual_document_snapshot` on a worker, and the transferred-binding check), so the delivery-time check mostly prunes. Move it to the worker that produces the state, or prune lazily at use time. If you drop it, keep incoming bindings ahead of existing ones in the dedup so a stale existing pair cannot shadow a fresh one.

Overlaps the workspace.rs memory/copying lane (TASK-10..13, 17, 18). Coordinate with it.

**Tests first.** Characterise the size of the navigation state shipped per definition request on a fixture with many contexts, and the number of path stats performed on the protocol thread per delivery (test hook).
<!-- SECTION:DESCRIPTION:END -->
