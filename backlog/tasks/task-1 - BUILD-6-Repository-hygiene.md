---
id: TASK-1
title: 'BUILD-6: Repository hygiene'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
updated_date: '2026-10-02 23:28'
labels:
  - arch-review
  - build
  - maintenance
  - agent-todos
milestone: m-0
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: low
type: chore
ordinal: 1000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (BUILD-6). Read that file's Global constraints section before starting.

Severity: low. Cost: maintenance.

**Problem.** 22 directories under `.worktrees/` (`git worktree list` shows
only 4 registered), 6 stashes, a stray `full-lsp-task-17-review.md` at the
repo root (its content belongs under `docs/superpowers/plans/` with the
other task reports), and `AGENT_TODOS.md` records 8 tests failing on master
plus clippy failures on the pinned toolchain.

**Approach.**
1. Ask the maintainer before deleting: list each worktree directory with
   its branch and whether the branch is merged (`git branch --merged
   master`); propose removal of merged ones with `git worktree prune` and
   `rm -rf`.
2. Same for stashes: `git stash list` with `git stash show -p` summaries.
3. `git mv full-lsp-task-17-review.md docs/superpowers/plans/`.
4. Fix or quarantine the failing tests so `cargo test --workspace` and
   clippy are green on the pinned toolchain; this is a prerequisite for
   every other task's **Done when**.

**Depends on.** nothing. Do first.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` green on 1.99.0; no unregistered worktree directories.
<!-- AC:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
From `AGENT_TODOS.md` (section "Workspace-Wide Performance"), folded into this task:

With the user's OK, drop the redundant agent stashes (fixes #1+#2
and the two temporary barrier baseline diffs). The agent worktrees and
branches are already gone.
<!-- SECTION:NOTES:END -->
