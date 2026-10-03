---
id: TASK-11
title: 'LSP-5: Sparse newline map and position index'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies: []
priority: high
type: enhancement
ordinal: 11000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-5). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime memory.

**Problem.** `workspace.rs:14505-14541` `NormalizedSource` stores one
`usize` per byte boundary even for LF-only input. For a 256 MiB expansion
that is about 2 GiB of map. `text.rs:166-173` and `:329` onward
(`PositionIndex`) store two `usize` arrays per character.

**Approach.** Represent CRLF normalization as a sorted `Vec<u32>` of removed
`\r` offsets and translate with binary search. Represent positions as line
starts plus a per-line list of non-ASCII corrections, only for lines that
contain non-ASCII.

**Tests first.** Property-style test: for random mixed CRLF/LF/UTF-16 inputs,
old and new implementations agree on every offset mapping. Keep the old
implementation in the test module until the new one passes.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `NormalizedSource` for a 100 MiB LF-only input allocates under 1 MiB beyond the text. Existing text/position tests pass.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
