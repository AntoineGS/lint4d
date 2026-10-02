---
id: TASK-27
title: 'CORE-11: Drain MSBuild pipes while waiting'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
labels:
  - arch-review
  - core
  - runtime
milestone: m-3
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: bug
ordinal: 27000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (CORE-11). Read that file's Global constraints section before starting.

Severity: medium. Cost: runtime.

**Problem.** `discovery_msbuild.rs:315-345` (`wait_with_timeout`) pipes
stdout and stderr but reads them only after `try_wait` reports exit. If
either pipe fills, MSBuild blocks and the 15 s timeout fires. Timeout
kills the `cmd` child without reaping or handling descendants
(`:245-250`).

**Approach.** Spawn two reader threads at launch that read to a bounded
buffer (1 MiB each, then discard with a flag); join them after exit. On
timeout, kill and `wait()` the child; on Windows use a job object or
`taskkill /T` for the tree.

**Tests first.** That test with a small helper script (fails today).

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test child that writes 4 MiB to stdout then exits completes without timeout.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
