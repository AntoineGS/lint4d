---
id: TASK-3
title: 'BUILD-4: Decide the multi-repo pinning model'
status: Done
assignee:
  - '@claude'
created_date: '2026-10-02 23:22'
updated_date: '2026-10-04 02:39'
labels:
  - arch-review
  - build
  - maintenance
milestone: m-0
dependencies: []
modified_files:
  - .cargo/deny.toml
  - CLAUDE.md
  - Cargo.lock
  - Cargo.toml
  - crates/cfg-core/**
  - crates/cfg-pascal/**
  - crates/tree-sitter-pascal/**
  - crates/lint4d/Cargo.toml
  - crates/pascal-core/Cargo.toml
  - crates/pascal-lsp/README.md
  - docs/shared-resolver-architecture.md
  - docs/decisions/0001-monorepo.md
priority: high
type: chore
ordinal: 3000
---

## Description

<!-- SECTION:DESCRIPTION:BEGIN -->
Imported from `2026-10-02-architecture-review-backlog.md` (BUILD-4). Shared constraints, measured baselines and parallel-work rules for review tasks: document doc-2 (`document_view`).

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
- [x] #1 A written decision in `docs/` and, if monorepo, the three crates as workspace members with `cargo test --workspace` green and `.cargo/config.toml`, `deny.toml` `allow-git` and `CLAUDE.md` updated.
<!-- AC:END -->

## Implementation Plan

<!-- SECTION:PLAN:BEGIN -->
Worktree /home/a.simard@multidev.local/gits/lint4d-monorepo, branch chore/monorepo (from master d55c0f6).

1. git subtree add (full history, no squash) from the local sibling checkouts at the revs pinned in Cargo.lock:
   - crates/tree-sitter-pascal <- ../tree-sitter-pascal 22cf861dc87bde0dd0b0d6397dd85dbef375006e
   - crates/cfg-core <- ../cfg-core b4131e61a939e0685194712689d1fd1497a41492
   - crates/cfg-pascal <- ../cfg-pascal dd64c7587cabf2bd3c4d4b3bed10e30743defcfc
2. Add the three crates to [workspace].members; replace the git deps in crates/lint4d, crates/pascal-core and crates/cfg-pascal with path deps; let cargo re-resolve Cargo.lock (sources only, no unrelated bumps).
3. Drop the allow-git entries from .cargo/deny.toml (unknown-git = "deny" stays); check CI; rewrite CLAUDE.md "Editable dependencies"; fix README/doc text describing the sibling setup.
4. Decision record docs/decisions/0001-monorepo.md.
5. Verify: cargo fmt --all --check, clippy --workspace --all-targets -D warnings, one cargo test --workspace, cargo tree showing no git sources, cargo deny if installed.
6. One commit for the manifests/docs on top of the three subtree merge commits.
<!-- SECTION:PLAN:END -->

## Implementation Notes

<!-- SECTION:NOTES:BEGIN -->
Decision (user, 2026-10-03): monorepo via git subtree. Move tree-sitter-pascal, cfg-core and cfg-pascal into this workspace as path crates with their history (git subtree add from the pinned revs), drop the per-crate git pins, the .cargo/config.toml `paths` overrides and the cargo-deny allow-git entries for them, and update CLAUDE.md.

Ruling: locations crates/tree-sitter-pascal, crates/cfg-core, crates/cfg-pascal — all three have a Cargo.toml at their root, so plain workspace membership works; the grammar's npm/py/swift/go bindings stay in place where tree-sitter tooling expects them.

Ruling: subtree add without --squash (full history) from the local sibling checkouts, whose HEADs equal origin/master and the Cargo.lock pins (22cf861, b4131e6, dd64c75).

Ruling: the imported crates keep their own editions (2018/2021) and do not opt into [lints] workspace = true — that is a code-wide lint change, out of scope. They gain publish = false so cargo-deny's allow-wildcard-paths accepts their version-less path deps.

Ruling: removed crates/tree-sitter-pascal/Cargo.lock (Cargo ignores lockfiles in workspace members; a stale one would mislead).

Ruling: workspace clippy now covers cfg-pascal tests; clippy 1.99's chunks_exact_to_as_chunks fired 3x in crates/cfg-pascal/tests/structured_flow_test.rs (699, 766, 884). Fixed mechanically with as_chunks::<2>(). rustfmt reformatted one tuple in crates/tree-sitter-pascal/bindings/rust/lib.rs.

Ruling: /docs/ is gitignored at the root (a48bcfb) but tracked docs exist there via force-add; the AC requires docs/, so docs/decisions/0001-monorepo.md is force-added.

The user's gitignored .cargo/config.toml in the main checkout (paths overrides to ../tree-sitter-pascal etc.) was NOT touched. It must be deleted after merging, otherwise it shadows the in-tree crates with the sibling checkouts.

Implemented on branch chore/monorepo (worktree /home/a.simard@multidev.local/gits/lint4d-monorepo), awaiting review and merge. After merging, the user must delete the gitignored .cargo/config.toml in the main checkout.

Merged into master as eadb3fb (branch chore/monorepo, via integration/2026-10-03, fast-forwarded 2026-10-03). Merged tree verified (identical to integration/2026-10-03 aebf633): cargo fmt --check clean; clippy --workspace --all-targets -D warnings clean (also --all-features); cargo test --workspace 3603 passed / 0 failed / 9 ignored; pascal-lsp protocol + protocol_barriers with test-support 914/916 (the two TASK-1.2 shutdown/cancel timing tests fail identically on d55c0f6); fmt corpus gate 326 files idempotent; Windows/macOS/Linux CI green on PR #6/#7.
<!-- SECTION:NOTES:END -->

## Final Summary

<!-- SECTION:FINAL_SUMMARY:BEGIN -->
Problem: the grammar, cfg-core and cfg-pascal were separate repos pinned by git rev, with a three-way same-rev invariant, per-crate duplicated pins, a multi-push bump ceremony and a local .cargo/config.toml paths override that could not change dependency lists.

Change (branch chore/monorepo, worktree /home/a.simard@multidev.local/gits/lint4d-monorepo):
- 1bc08b7, 45bf5bc, 75bd4d6: git subtree add (full history) of ../tree-sitter-pascal 22cf861 -> crates/tree-sitter-pascal, ../cfg-core b4131e6 -> crates/cfg-core, ../cfg-pascal dd64c75 -> crates/cfg-pascal (the exact Cargo.lock pins).
- eee0751: the three crates are workspace members; git deps in crates/lint4d, crates/pascal-core and crates/cfg-pascal became path deps; Cargo.lock only lost the three git sources and gained the members' optional/dev dep edges (no new packages, no version bumps); .cargo/deny.toml allow-git list removed (unknown-git = "deny" stays); publish = false on the imported crates; grammar's nested Cargo.lock removed; clippy fix (as_chunks) in cfg-pascal/tests/structured_flow_test.rs and one rustfmt change in the grammar's Rust binding; CLAUDE.md "Editable dependencies" rewritten; pascal-lsp README and docs/shared-resolver-architecture.md updated; decision record docs/decisions/0001-monorepo.md (force-added, /docs/ is gitignored). CI needed no change (it never referenced the siblings; the grammar's C build already ran there from the git source).

Verified: cargo fmt --all --check clean; CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets -- -D warnings clean; CARGO_BUILD_JOBS=2 cargo test --workspace --locked --no-fail-fast: 104 result lines, 3490 passed, 0 failed, 9 ignored (includes cfg-core, cfg-pascal and tree-sitter-pascal suites); Cargo.lock has 0 git sources and cargo tree -i shows tree-sitter-pascal/cfg-core resolved to crates/ paths. cargo-deny is not installed locally, so cargo deny check was not run (CI's deny job will).

User actions after merge: delete the gitignored .cargo/config.toml in the main checkout (its paths overrides would shadow the in-tree crates); archive the three GitHub repos (not touched). Follow-up: TASK-113 for workspace lints/edition of the imported crates.
<!-- SECTION:FINAL_SUMMARY:END -->
