---
id: TASK-48
title: 'LSP-21: One memoized type and expression query layer'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - lsp
  - runtime
  - maintenance
milestone: m-6
dependencies: []
priority: medium
type: enhancement
ordinal: 48000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-21).

Severity: medium. Cost: both.

**Problem.** Binding classification builds a single-file index without using
the parsed-document cache then discards it (`workspace/rename.rs:2973-3012`).
Semantic tokens build a fresh `ResolutionState` per identifier
(`navigation/semantic_tokens.rs:445-454`), assignment diagnostics per
assignment (`navigation.rs:2460-2470`). Type interpretation is duplicated:
`navigation.rs:14076-14239`, `:9831-9928` versus
`navigation/overload.rs:1569`, `:1642`, `:2729-2862` versus
`navigation/assistance.rs:4024-4277`, `:2450-2550`, with differing depth
limits (32 in completion's array recursion, 64 elsewhere).

**Approach.** Introduce `TypeQueries` with memo tables keyed by (document
version, node id): `type_of_expr`, `resolve_alias`, `element_type`,
`callable_signature`, `is_assignable`. Memoize unknown outcomes too. Port
completion, hover, signature help, tokens and diagnostics to it one at a
time, each behind the existing tests.

**Tests first.** Pure refactor; keep `tests/navigation.rs` and the
`navigation.rs` unit tests green per port. Add one test per query for
memo hit on second call.

**Depends on.** nothing; large, do it after Theme A runtime tasks.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 No feature module defines its own alias-following or array-rank recursion; one depth constant.
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
