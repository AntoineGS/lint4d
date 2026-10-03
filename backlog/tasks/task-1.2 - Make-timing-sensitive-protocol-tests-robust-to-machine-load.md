---
id: TASK-1.2
title: Make timing-sensitive protocol tests robust to machine load
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
updated_date: '2026-10-03 02:21'
labels:
  - agent-todos
  - workspace-perf
  - build
dependencies: []
parent_task_id: TASK-1
priority: medium
type: bug
ordinal: 88000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Converted from `AGENT_TODOS.md`, section "Workspace-Wide Performance".

Section context: on `multidev` (24,050 `.pas` files, 230+ projects), workspace-wide requests (`workspace/diagnostic`, workspace symbols, `references`/`rename` on public symbols) burn a core for 8+ minutes and then fail, because the snapshot is always incomplete. Neovim sends `workspace/diagnostic` automatically. Done items below are merged into master (`05c2c79`..`5bdf66b`).

Original item:

Timing-sensitive protocol tests fail under machine load on `master`
too: `shutdown_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline`
and `cancel_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline`
(750 ms shutdown window). Consider making them robust to load.
`configuration_revalidation_readers_do_not_block_on_fifo_replacement`
takes 2.9-4.2 s alone against its 5 s deadline and fails under the
full parallel suite.
<!-- SECTION:DESCRIPTION:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Related review task: TASK-29 (BUILD-1). Check it before starting to avoid duplicate work.

2026-10-02 (from TASK-5): shutdown_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline and cancel_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline failed 6/6 on unmodified c456f8c at load average ~2.6 on an 8-core machine, so they fail even at moderate load, not only under heavy load. A probe measured the shutdown response at 1.07-1.28 s against the 750 ms window, with the same timing on the old fail-closed recovery and on TASK-5's discard path.
<!-- SECTION:NOTES:END -->
