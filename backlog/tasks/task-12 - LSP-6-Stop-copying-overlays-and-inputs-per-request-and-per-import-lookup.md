---
id: TASK-12
title: 'LSP-6: Stop copying overlays and inputs per request and per import lookup'
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
ordinal: 12000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (LSP-6). Read that file's Global constraints section before starting.

Severity: high. Cost: runtime memory and cpu.

**Problem.** Each analysis dispatch captures a workspace input then clones it
again for validation (`server.rs:5565`, `:5747`). Worker workspace
reconstruction clones every open document's `String`
(`workspace.rs:3205-3265`). `overlay_inputs()` at `workspace.rs:7814-7826`
copies all open editor text for every indexed unit and every import lookup;
an import miss creates another whole input. `warmer.rs:543`, `:683` repeat
the pattern.

**Approach.** Store open document text as `Arc<str>` with a version; build
one `Arc<OverlayMap>` per workspace revision and pass it by reference into
index construction and import lookup. Replace the validation clone with a
compact `RevalidationInput` containing only hashes and versions.

**Tests first.** Unit test that `overlay_inputs` (or its replacement) called
twice for the same revision returns the same `Arc`.

**Depends on.** nothing.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 With 50 open documents, a references request allocates overlay text at most once (assert via a counting allocator in a test, or via `Arc::strong_count` observations in a unit test).
- [ ] #2 The tests listed under Tests first were written before the change, failed (or recorded the old behaviour) on the original code, and pass now.
<!-- AC:END -->
