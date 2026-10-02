---
id: TASK-3
title: 'BUILD-4: Decide the multi-repo pinning model'
status: To Do
assignee: []
created_date: '2026-10-02 23:22'
labels:
  - arch-review
  - build
  - maintenance
milestone: m-0
dependencies: []
references:
  - 2026-10-02-architecture-review-backlog.md
priority: high
type: chore
ordinal: 3000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (BUILD-4). Read that file's Global constraints section before starting.

Severity: high. Cost: maintenance.

**Problem.** `CLAUDE.md:7-14` describes the three-way same-rev invariant and
the `paths` override limits. A grammar change needs: edit grammar,
regenerate, push, bump cfg-pascal, push, bump two manifests here. cargo-deny
cannot resolve git workspace deps so revs are duplicated per crate.

**Options with tradeoffs.**
- Monorepo: move `tree-sitter-pascal`, `cfg-core`, `cfg-pascal` into this
  workspace as path crates. Atomic changes, one lockfile, path overrides go
  away. Costs: merged history (use `git subtree add` to keep it), the
  grammar's npm/py bindings live inside a Rust workspace, and no
  independent versioning (not needed today: `publish = false` everywhere).
- `[patch]` in a root `Cargo.toml` for local dev instead of `paths`: honours
  changed dependency lists; still three repos and three pushes.
- Submodules: separate histories, pinned by commit in the parent; still
  cross-repo pushes and recursive checkout discipline.

**Recommendation.** Monorepo via `git subtree`, given one maintainer, no
publishing, and that CORE-7, LINT-1, LINT-9, LINT-12, LINT-14 and LINT-16
all need coordinated bumps.

**Depends on.** nothing; do early because it removes pin ceremony from
many other tasks.
<!-- SECTION:DESCRIPTION:END -->

## Acceptance Criteria
<!-- AC:BEGIN -->
- [ ] #1 A written decision in `docs/` and, if monorepo, the three crates as workspace members with `cargo test --workspace` green and `.cargo/config.toml`, `deny.toml` `allow-git` and `CLAUDE.md` updated.
<!-- AC:END -->
