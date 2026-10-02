---
id: TASK-33
title: >-
  LSP-16: Versioned workspace index for symbols, occurrences and reverse
  dependencies
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-02 23:28'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-5
dependencies:
  - TASK-6
  - TASK-12
  - TASK-13
  - TASK-20
references:
  - 2026-10-02-architecture-review-backlog.md
priority: high
type: enhancement
ordinal: 33000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-16). Read that file's Global constraints section before starting.

Severity: high. Cost: runtime; this is the structural investment.

**Problem.** `NavigationIndex` (`navigation.rs:423-429`, `:15571-15613`)
stores documents, unit providers and per-document declaration indexes. There
is no workspace reference index or reverse dependency graph. Non-local
references choose `SnapshotMode::Workspace` (`workspace/queries.rs:1120-1130`)
and `workspace/rename.rs:6031-6057`, `:6163`, `:6217`, `:6296-6350`,
`:6637-6655`, `:7558-7618` walk source roots, read and hash files, filter by
identifier spelling, build a fresh index and walk every identifier in every
retained document. Lower bound per public-symbol request is
O(workspace bytes + retained AST nodes).

**Approach.**
1. Per project context, maintain `WorkspaceIndex { exports: name ->
   Vec<(doc, decl)>, importers: unit -> Vec<doc>, occurrences: name ->
   Vec<(doc, range)> }` built from the per-document indexes that already
   exist, updated incrementally on document change and watcher events.
2. Candidate consumers for a symbol are `importers[declaring unit]`
   filtered by `occurrences[name]`; only those documents are bound.
3. The index carries a generation; LSP-7/LSP-19 tokens reference it.
4. Build it in the warmer (`warmer.rs`) so interactive requests find it
   ready; interactive requests on a missing index fall back to the current
   path with coverage marked incomplete (LSP-1).

**Tests first.** The 500-unit synthetic test against the current code
(records that all 500 are bound today).

**Depends on.** LSP-1, LSP-6, LSP-8, LSP-15. Coordinate with LSP-17,
LSP-18, LSP-24 which consume it.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 References on a public symbol in the 24k-file repo answer from the index in seconds, not minutes, once warm. A test with 500 synthetic units and one shared symbol binds only the importing units (count via a test hook).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related task converted from `AGENT_TODOS.md`: TASK-82 (Find out why many sources still miss project discovery reuse).

Related task converted from `AGENT_TODOS.md`: TASK-83 (Decide workspace-wide request policy on very large or partially evaluable repos).
<!-- SECTION:NOTES:END -->
