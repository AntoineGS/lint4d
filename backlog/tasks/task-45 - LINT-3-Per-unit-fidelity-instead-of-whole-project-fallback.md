---
id: TASK-45
title: 'LINT-3: Per-unit fidelity instead of whole-project fallback'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lint
  - runtime
  - maintenance
  - cross-repo
milestone: m-5
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: high
type: enhancement
ordinal: 45000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LINT-3). Read that file's Global constraints section before starting.

Severity: high. Cost: both.

**Problem.** `cfg/project_snapshot.rs:149-156`, `:187-197`, `:303-334`,
`:529-533`, `:588-631` abandon the project-aware CFG path for the whole
target when any dependency is incomplete: an active unavailable import,
unknown conditional activity anywhere in the closure, dependency limits,
unresolved includes, unsupported directives, preparation/parser/map
rejection, or an include seen under two entry environments. Missing SDK
source alone trips it (`resolver.rs:668-684`, `:788-818`) even though
cfg-pascal supports conservative unavailable import bindings
(`../cfg-pascal/docs/cfg-lsp-support.md:159-174`). One unmappable diagnostic
then discards the whole prepared pass (`engine/mod.rs:319-329`,
`:230-241`).

**Approach.** Carry `Fidelity { Prepared, RawComplete, Incomplete(reason) }`
per unit in `CfgProjectSnapshot`. Build CFGs for the target from its own
prepared source whenever the target itself is complete; treat incomplete
dependencies as `Unavailable` import bindings. Map diagnostics individually;
an unmappable one falls back to its file-local location, not the whole run.

**Tests first.** That fixture in `crates/lint4d/tests/cfg_project_snapshot_test.rs`
(currently falls back).

**Depends on.** LINT-9 reduces the triggers; not required.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A project whose target is complete but imports a missing SDK unit produces prepared-path diagnostics for the target.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
