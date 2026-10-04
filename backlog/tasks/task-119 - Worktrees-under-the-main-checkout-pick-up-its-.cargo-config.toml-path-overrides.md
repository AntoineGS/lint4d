---
id: TASK-119
title: >-
  Worktrees under the main checkout pick up its .cargo/config.toml path
  overrides
status: To Do
assignee: []
created_date: '2026-10-03 22:48'
updated_date: '2026-10-03 23:28'
labels:
  - build
dependencies: []
priority: low
type: chore
ordinal: 121000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Cargo searches parent directories for .cargo/config.toml. Worktrees live at <repo>/.worktrees/<name>, inside the main checkout, so they inherit the main checkout's gitignored .cargo/config.toml with paths overrides: building in .worktrees/lsp-partial-results compiled "cfg-pascal v0.1.0 (/home/.../gits/cfg-pascal)" from the local checkout, not the pinned git rev. The day-run implementer contract assumes worktrees build against the pinned revs (what CI builds). Options: put worktrees outside the checkout, or set CARGO_HOME-independent config (e.g. an empty [patch]/paths override is not possible; use --config or move worktrees). Observed 2026-10-03 while implementing TASK-6.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Controller check (2026-10-03): the sibling checkouts ../cfg-core (b4131e6), ../cfg-pascal (dd64c75) and ../tree-sitter-pascal (22cf861) are clean and at exactly the revs pinned in Cargo.lock, so builds that picked up the main checkout's `paths` overrides compiled the same sources as the pins. Once chore/monorepo (TASK-3) merges and the user deletes the gitignored .cargo/config.toml, the override disappears entirely; this task can then be archived.
<!-- SECTION:NOTES:END -->
