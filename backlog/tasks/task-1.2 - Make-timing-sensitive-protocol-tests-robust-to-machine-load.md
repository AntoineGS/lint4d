---
id: TASK-1.2
title: Make timing-sensitive protocol tests robust to machine load
status: To Do
assignee: []
created_date: '2026-10-02 23:28'
updated_date: '2026-10-04 01:47'
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

2026-10-03 (TASK-21 run): source_bearing_mixed_roots_and_repeated_includes_deduplicate_physical_results (crates/pascal-lsp/tests/protocol.rs, references over a generated 10k-line include) timed out once in a loaded full pascal-lsp run and passed 3/3 alone. Workload-sensitive; consider it alongside the timing tests listed above.

2026-10-03 (LSP-S run, TASK-14): a full protocol_barriers run under load had 4 receive timeouts besides the two known TASK-1.2 tests; all passed in isolation and later full runs were 904/906 and 906/908. More load-sensitive protocol tests exist than the ones listed here.

2026-10-03 (found during TASK-97, fix/notification-fence): `sixty_four_project_metadata_changes_share_the_notification_budget_and_stale_pull_results` and `source_bearing_mixed_roots_and_repeated_includes_deduplicate_physical_results` (protocol.rs) also time out at the ~10 s `receive_until` deadline under load. Reproduced on unmodified d55c0f6 sources at load average ~18-22 on the 8-core machine: the first failed 2 of 3 isolated `--exact` runs (passing runs took 17-23 s). Both pass on an idle machine.

2026-10-03 (integration run): shutdown_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline and cancel_after_sixty_sixth_ordinary_frame_is_reached_by_worker_deadline fail even when run alone on an idle-ish machine (~0.8 s each vs the 750 ms window), identically on master d55c0f6 and on every LSP branch tip. They are deterministic failures under --features test-support, not merely load-sensitive.
<!-- SECTION:NOTES:END -->
