---
id: TASK-4
title: 'GRAM-4: Reproducible generation check in CI'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - grammar
  - maintenance
  - cross-repo
milestone: m-0
dependencies: []
priority: medium
type: chore
ordinal: 4000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (GRAM-4).

Severity: medium. Cost: maintenance.

**Problem.** `package-lock.json` pins tree-sitter-cli 0.24.7 but nothing
verifies committed `src/parser.c` matches `grammar.js`; history has
regeneration-then-patch commits (`144aeab`, `13353f7`, `3a9955c`).
`AGENT_TODOS.md` notes the system 0.26 CLI rewrites every generated file.

**Approach.** GitHub workflow in the grammar repo: `npm ci`,
`npx tree-sitter generate`, `git diff --exit-code src/`, then
`npx tree-sitter test` and `cargo test`.

**Tests first.** The workflow is the test.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 The workflow passes on master and fails on a PR that edits `grammar.js` without regenerating.
<!-- AC:END -->
