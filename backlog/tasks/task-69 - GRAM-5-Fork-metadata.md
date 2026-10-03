---
id: TASK-69
title: 'GRAM-5: Fork metadata'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - grammar
  - maintenance
  - cross-repo
milestone: m-6
dependencies: []
priority: low
type: chore
ordinal: 69000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (GRAM-5).

Severity: low. Cost: maintenance.

**Problem.** `Cargo.toml:7` still names `Isopod/tree-sitter-pascal` as the
repository; `package.json` has `repository: null`.

**Approach.** Point both at the fork; add an "Upstream" section to the
README describing the relationship and the 56-commit delta.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Metadata updated.
<!-- AC:END -->
