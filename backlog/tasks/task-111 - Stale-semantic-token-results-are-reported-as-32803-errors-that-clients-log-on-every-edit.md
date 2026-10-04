---
id: TASK-111
title: >-
  Stale semantic-token results are reported as -32803 errors that clients log on
  every edit
status: To Do
assignee: []
created_date: '2026-10-03 22:11'
labels:
  - lsp
  - spike-92
dependencies:
  - TASK-36
priority: low
type: enhancement
ordinal: 113000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Found by TASK-92. 54 `analysis result became stale; retry the request` errors for semantic tokens in the nvim log (2026-09-16..10-01), in bursts of 1-8 within seconds, almost all within 15-170 s of a server start (and one 36,000 s after the nvim session began, after an idle period).

**Cause.** Every didChange calls `advance_document_version` (workspace.rs:4319) which bumps the source generation. A result computed against the old text is rejected by `analysis_result_is_stale` (server.rs:9145) / `dependency_scoped_result_is_fresh` (workspace.rs:12868) and sent through `send_error` with ErrorCode::RequestFailed (-32803) (server.rs:9225-9232), so the client logs it as an error. Results with an empty read set (early `failed(...)` returns in queries.rs) are compared on bare generations, so an edit in any file invalidates them. Warm-up `ClosureStored` events also trigger semantic-token refreshes (server.rs:11805-11809), which re-request tokens while the user is typing.

**Question.** Is -32803 the right code? LSP says a server may answer ContentModified (-32801) when a result is outdated by an edit; clients retry or drop it quietly. Decide per method whether stale-by-edit should be ContentModified, keeping -32803 for real failures. Needs a protocol test pinning the code for semanticTokens/full and range after a didChange. The broader fix (not rejecting results whose bound documents are unchanged) is TASK-36.
<!-- SECTION:DESCRIPTION:END -->
