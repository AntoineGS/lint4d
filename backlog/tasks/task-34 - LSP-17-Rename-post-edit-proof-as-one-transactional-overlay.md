---
id: TASK-34
title: 'LSP-17: Rename post-edit proof as one transactional overlay'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lsp
  - runtime
  - maintenance
milestone: m-5
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: high
type: enhancement
ordinal: 34000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-17). Read that file's Global constraints section before starting.

Severity: high. Cost: runtime and maintenance.

**Problem.** `workspace/rename.rs:4925-4936` (`post_edit_rebind_proof`)
calls `rebind_with_replaced_source_for_fix_all` once per edited document;
that method (`navigation.rs:954-1013`, `:1034-1040`) sorts all indexed
documents, clones unit/provider maps and every document's import bindings.
With E edited documents and F indexed documents the container work is
O(E x F log F), then another reference search and contract comparison run.

**Approach.** Build one `IndexOverlay` holding all replaced sources, rebind
once, then validate the affected binding closure (the importers of edited
units, from LSP-16) once.

**Tests first.** Rebind-count characterization test.

**Depends on.** LSP-16 for the closure; can be done before with the current
full index.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Renaming a symbol with 200 edited documents performs one rebind (count via test hook) and the existing `tests/rename.rs` suite stays green.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
