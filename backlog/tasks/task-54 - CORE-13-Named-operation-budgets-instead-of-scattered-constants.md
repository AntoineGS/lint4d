---
id: TASK-54
title: 'CORE-13: Named operation budgets instead of scattered constants'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-03 01:16'
labels:
  - arch-review
  - core
  - maintenance
milestone: m-6
dependencies:
  - TASK-32
  - TASK-53
priority: medium
type: task
ordinal: 54000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-13).

Severity: medium. Cost: maintenance.

**Problem.** 31 distinct `MAX`/`LIMIT` names across the two crates
(`conditional.rs:19-26`, `resolver.rs:43-75`, `:2935`, `:2998`,
`pascal-project/src/lib.rs:59-74`, `installations/rsvars.rs:3-5`,
`ide_paths.rs:841-842`, `rtl_constants.rs:7`, `:43-44`) plus 13
configurable `ResolverLimits` fields. The same resource is bounded
differently in two places (10,000 vs 1,048,576 directory entries; 16 shared
vs 64 request-local catalogues; 4/8/16 MiB source reads). pascal-lsp has
261 more (see LSP-25 and the baseline table).

**Approach.** One `Budgets` struct per crate with named fields grouped by
resource (`source_bytes`, `directory_entries`, `catalogues`, `conditional`)
and a single exhaustion reason enum; delete duplicate constants.

**Tests first.** None; existing limit tests must keep passing with the
same effective values.

**Depends on.** CORE-9, CORE-12.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `grep -rhoE 'const [A-Z_]*(LIMIT|BUDGET|MAX)[A-Z_]*:' crates/pascal-core/src crates/pascal-project/src | sort -u | wc -l` reports under 10.
- [ ] #2 No behaviour change: the existing suites named in the task stay green (cargo fmt --check, clippy -D warnings, and the affected crate's tests).
<!-- AC:END -->
