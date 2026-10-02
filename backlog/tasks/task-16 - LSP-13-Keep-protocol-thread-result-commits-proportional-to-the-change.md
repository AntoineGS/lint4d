---
id: TASK-16
title: 'LSP-13: Keep protocol-thread result commits proportional to the change'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-02 23:23'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies:
  - TASK-14
references:
  - 2026-10-02-architecture-review-backlog.md
priority: high
type: enhancement
ordinal: 16000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-13). Read that file's Global constraints section before starting.

Severity: high. Cost: runtime latency.

**Problem.** `server.rs:9146-9157`, `:9206`, `:9249-9252` and
`workspace.rs:12742-12791`, `:3324-3343`, `:3000-3018`: delivery checks nest
candidate/scope observations against every retained source-change
observation (up to 4,096 per candidate) with no budget, then re-checks
context freshness and importer hashes for every compiled binding, including
filesystem work. This runs on the protocol thread and blocks edits and
cancellations.

**Approach.** Workers produce an indexed validation token (LSP-7) and a
delta of state entries; the protocol thread applies the delta and compares
tokens. Filesystem checks move to the worker.

**Tests first.** Timing characterization test.

**Depends on.** LSP-7.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Delivery of a result with 4,000 retained observations costs under 1 ms on the protocol thread (measure in a unit test with `Instant`, assert loosely, 10 ms).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
