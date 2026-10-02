---
id: TASK-56
title: 'LINT-11: Shared source-semantic facts below the LSP for exception dispatch'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lint
  - maintenance
  - cross-repo
milestone: m-6
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: task
ordinal: 56000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-11). Read that file's Global constraints section before starting.

Severity: medium. Cost: maintenance.

**Problem.** `../cfg-pascal/src/exception_types.rs` (2,065 lines;
`:78-219`, `:261-337`, `:516-594`, `:927`, `:1064`) models lexical scopes,
shadowing, aliases, imports, visibility, constructor accessibility,
ancestry and cycles; `pascal-lsp/src/navigation.rs:12833-12962` does the
same independently. Meanwhile `rules/exception.rs:370-392` and
`rules/resource_leak.rs:306-313`, `:661` reason syntactically about whether
an `except` exists instead of consuming the precise handler model.

**Approach.** Define a `SemanticFacts` trait in pascal-core (declarations by
name, ancestry chain, visibility, imports) with a file-local
implementation; cfg-pascal's exception index consumes it; later the LSP
implements it over `NavigationIndex`. Keep `Known`/`SubtypeOf` dispatch in
cfg-pascal.

**Tests first.** Pure refactor; `../cfg-pascal/tests/exception_*` green.

**Depends on.** LSP-21 if the LSP implementation is attempted; the
cfg-pascal part stands alone.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `exception_types.rs` no longer contains its own scope/alias resolver; its tests pass against the trait implementation.
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
