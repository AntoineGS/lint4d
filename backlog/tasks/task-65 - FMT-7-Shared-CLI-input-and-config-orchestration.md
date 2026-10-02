---
id: TASK-65
title: 'FMT-7: Shared CLI input and config orchestration'
status: To Do
assignee: []
created_date: '2026-10-02 23:23'
updated_date: '2026-10-02 23:23'
labels:
  - arch-review
  - fmt
  - correctness
  - maintenance
milestone: m-6
dependencies:
  - TASK-38
references:
  - 2026-10-02-architecture-review-backlog.md
priority: medium
type: bug
ordinal: 65000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (FMT-7). Read that file's Global constraints section before starting.

Severity: medium. Cost: maintenance and correctness.

**Problem.** `fmt4d/src/main.rs:143-186`, `:217` and
`lint4d/src/main.rs:272-280`, `:405-415` both implement config-start
selection, project/file discovery and rayon processing with different
semantics: fmt4d merges positional discovery with project references and
starts config lookup at the first input; lint4d prefers project references
and starts at the working directory; fmt4d does not deduplicate before
parallel writes.

**Approach.** `pascal_core::cli_inputs::collect(args) -> Inputs { files,
config_root, project }` with one documented policy; both binaries use it.
Also addresses the CORE-1 file-list source.

**Tests first.** The table test in `fmt_cli_test.rs` and `cli_test.rs`.

**Depends on.** CORE-1.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 Both CLIs produce the same file list and config root for the same arguments (table test); a duplicate input is formatted once.
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
