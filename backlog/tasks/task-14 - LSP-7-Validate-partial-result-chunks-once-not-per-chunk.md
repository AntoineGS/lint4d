---
id: TASK-14
title: 'LSP-7: Validate partial-result chunks once, not per chunk'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
labels:
  - arch-review
  - lsp
  - runtime
milestone: m-2
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: high
type: enhancement
ordinal: 14000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-7). Read that file's Global constraints section before starting.

Severity: high. Cost: runtime.

**Problem.** `server.rs:8213-8285`, `:8329-8359`, `:8388-8418`: every
partial chunk (at most 128 items or 64 KiB) spawns an OS thread that
revalidates the complete source-record set. Delivery cost is
chunks x full-read-set validation, and these threads are outside the
`MAX_ANALYSIS_JOBS` limit (`server.rs:62-65`). A slow validation at the
front blocks later chunks.

**Approach.** Compute one dependency-scoped revision token when the result
is produced; each chunk compares the token to the current workspace
generation (an integer compare). Deliver chunks from a single bounded
executor thread.

**Tests first.** The counting test above against current code (expect 50).

**Depends on.** LSP-19 is the same idea for whole results; do LSP-7 first,
it is smaller.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A test producing 50 chunks performs validation work once (count via a test hook on the validation function).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
