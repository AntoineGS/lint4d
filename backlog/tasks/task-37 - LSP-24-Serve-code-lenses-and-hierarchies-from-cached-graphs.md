---
id: TASK-37
title: 'LSP-24: Serve code lenses and hierarchies from cached graphs'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-5
dependencies:
  - TASK-6
  - TASK-33
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: enhancement
ordinal: 37000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-24). Read that file's Global constraints section before starting.

Severity: medium. Cost: runtime.

**Problem.** `workspace/code_lenses.rs:102`, `:182-200`, `:286-308`: lens
resolve re-discovers the document's lenses then runs the full references
request; up to 64 lenses per document. `navigation/call_hierarchy.rs:95-148`,
`:187-204` walks calls in every retained document for incoming, and the
entire source tree for outgoing. `navigation/type_hierarchy.rs:151-160`
scans all class declarations and fails after 4,096 candidates.

**Approach.** Lens counts and incoming calls come from
`WorkspaceIndex.occurrences` (LSP-16). Outgoing calls walk only the selected
routine's subtree. Supertypes use the local dependency closure. Replace the
4,096 subtype cap with partial results plus coverage (LSP-1).

**Tests first.** Walk-count test.

**Depends on.** LSP-1, LSP-16.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Resolving 64 lenses performs zero workspace walks.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
