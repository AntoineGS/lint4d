---
id: TASK-53
title: 'CORE-12: Narrow the public API surface'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-02 23:23'
labels:
  - arch-review
  - core
  - maintenance
milestone: m-6
dependencies:
  - TASK-32
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: task
ordinal: 53000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-12). Read that file's Global constraints section before starting.

Severity: medium. Cost: maintenance.

**Problem.** `pascal-core/src/lib.rs:1-18` makes every module public and
`pub use resolver::*` exports the entire resolver. Discovery wrappers in
`pascal-project/src/lib.rs:1954-2214` grow combinatorially (selections,
observations, overrides, cancellation, budgets, deleted paths). Failure
state has `Resolution<T>`, `ResolutionTarget<T>`, reports and booleans
(`resolver.rs:240-282`, `:337-375`, `:1026-1050`).

**Approach.** Replace wrappers with one `DiscoveryRequest` builder; replace
`pub use resolver::*` with an explicit list; make internal accounting
`pub(crate)`. Collapse to one `Resolution<T>` enum and one report.

**Tests first.** None; compile-driven. Run the whole workspace tests.

**Depends on.** CORE-9.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `cargo doc -p pascal-core --no-deps` lists under 60 public items from the resolver; all consumers compile.
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
