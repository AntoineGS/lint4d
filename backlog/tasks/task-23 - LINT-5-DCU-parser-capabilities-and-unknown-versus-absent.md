---
id: TASK-23
title: 'LINT-5: DCU parser capabilities, and unknown versus absent'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:29'
labels:
  - arch-review
  - lint
  - correctness
milestone: m-3
dependencies: []
priority: high
type: bug
ordinal: 23000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-5). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

Severity: high. Cost: correctness.

**Problem.** `crates/lint4d/src/dcu/class_parser.rs:65-73`, `:99-107`,
`:341`, `:385-403` discard class parent handles (`parent: None`), keep
empty method parameter lists and no return types; `decl_parser.rs:303-308`
leaves field references unresolved; visibility defaults to private.
Callers (`dcu/mod.rs:257-297`, `rules/helpers.rs:194-211`) ask for ancestry
and constructor ownership and treat an empty parameter list as proof of
"not owner-managed". Consumers include resource-leak, transaction and nil
rules, not only the two field rules. The LSP reuses this parser and
exposes only verified D13 Win64 shells
(`pascal-lsp/src/navigation/compiled_dcu.rs:46-50`, `:97-111`).

**Approach.** Add `enum Fact<T> { Known(T), Unknown }` to `TypeInfo` for
`parent`, `methods[].params`, `methods[].result`, `visibility`. Parser sets
`Unknown` where it does not decode. Each consumer treats `Unknown` as
"cannot prove" and emits nothing. Document supported version/platform
pairs in `crates/lint4d/README.md` with the fixture that verifies each.

**Tests first.** Unit test constructing a `TypeInfo` with `Unknown`
parameters and asserting no diagnostic (fails today).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `field-not-freed` on a class whose constructor parameter list is `Unknown` emits no diagnostic; existing DCU fixture tests pass.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
