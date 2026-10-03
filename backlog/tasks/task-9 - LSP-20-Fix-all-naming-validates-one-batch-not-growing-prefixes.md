---
id: TASK-9
title: 'LSP-20: Fix-all naming validates one batch, not growing prefixes'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-1
dependencies: []
priority: high
type: enhancement
ordinal: 9000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-20). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: runtime.

**Problem.** `workspace/codeactions.rs:1713-1733`, `:1749-1796`: for every
proven candidate, fix-all pushes it onto `retained` and calls
`validate_fix_all_subset` again, which normalizes cumulative edits,
rebuilds the transformed source, builds offset maps and rebinds the index.
Then `:2023-2088`, `:2138-2164` parse, lint and search the final source
again. At least quadratic in candidate count.

**Approach.** Build a conflict graph over candidates keyed by binding id
(two candidates conflict when their new spellings collide in a scope);
select a maximal non-conflicting set; validate once.

**Tests first.** Golden test capturing current output on existing fixtures;
call-count test.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Fix-all with 500 naming candidates performs one `validate_fix_all_subset` (count via test hook); the resulting edit equals the current implementation's edit on the existing codeaction fixtures.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
