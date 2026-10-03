---
id: TASK-35
title: 'LSP-18: Index include owners and drop the 256-owner discovery cap'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-5
dependencies:
  - TASK-33
priority: high
type: enhancement
ordinal: 35000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-18).

Severity: high. Cost: runtime and usability.

**Problem.** `workspace/rename.rs:559-600`, `:675-700`, `:760-816`,
`:880-891`, `:7499-7539`: a physical include query is reverse-mapped by
scanning every expansion; each virtual query runs a whole-index reference
search; rename scans expansions again per mapped edit. Local `.inc`
discovery marks the result incomplete when more than
`MAX_RENAME_INCLUDE_OWNER_DISCOVERY = 256` owners exist
(`workspace/rename.rs:79`) and only examines the first 256. On a 24k-file
repo that is a structural failure, not a safety bound.

**Approach.** Add `include_owners: physical path -> Vec<(owner doc, mapping
interval)>` to the workspace index (LSP-16). Resolve equivalent virtual
bindings once per physical range. Apply the owner limit to candidate
owners that actually import the include, not the whole catalogue.

**Tests first.** That fixture against current code (expect incomplete).

**Depends on.** LSP-16.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A fixture with 300 units including the same `.inc` returns references for a symbol declared in the `.inc`.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
