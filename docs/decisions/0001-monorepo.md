# 0001: Merge the grammar and CFG repositories into this workspace

- Status: accepted
- Date: 2026-10-03
- Decided by: Antoine Gaudreau Simard
- Backlog: TASK-3 (BUILD-4)

## Context

lint4d depended on three sibling repositories through git dependencies pinned
by revision:

- `tree-sitter-pascal` (`github.com/AntoineGS/tree-sitter-pascal`), a fork of
  `Isopod/tree-sitter-pascal`: grammar, generated parser, queries;
- `cfg-core` (`github.com/AntoineGS/cfg-core`): language-agnostic CFG and
  interprocedural analysis;
- `cfg-pascal` (`github.com/AntoineGS/cfg-pascal`): the Pascal CFG builder,
  itself depending on the other two.

cfg-pascal parses with the same grammar as lint4d, so the tree-sitter-pascal
revision had to be identical in `crates/lint4d/Cargo.toml`,
`crates/pascal-core/Cargo.toml` and cfg-pascal's manifest. cargo-deny cannot
resolve git workspace dependencies, so the revision was repeated per crate. A
grammar change took six steps: edit the grammar, regenerate, push, bump
cfg-pascal, push, bump two manifests here. Local development used a gitignored
`.cargo/config.toml` with `paths` overrides, which cannot change a crate's
dependency list and silently diverged from what CI built.

Several planned changes (CORE-7, LINT-1, LINT-9, LINT-12, LINT-14, LINT-16)
need coordinated edits across these crates.

## Options

1. **Monorepo.** Move the three repositories into this workspace as path
   crates. Atomic cross-crate changes, one lockfile, no overrides. Costs: merged
   history (kept with `git subtree add`), the grammar's npm, Python, Swift, Go
   and C bindings live inside a Rust workspace, and no independent versioning
   (not needed: nothing is published).
2. **`[patch]` in the root `Cargo.toml` for local development** instead of
   `paths`. Honours changed dependency lists, but still three repositories and
   three pushes per grammar change.
3. **Git submodules.** Separate histories pinned by commit in the parent. Still
   cross-repository pushes, plus recursive-checkout discipline in CI and every
   worktree.

## Decision

Option 1, merged with `git subtree add` (full history, not squashed) at the
revisions the build used on 2026-10-03:

| Crate | Location | Imported revision |
|---|---|---|
| tree-sitter-pascal | `crates/tree-sitter-pascal` | `22cf861dc87bde0dd0b0d6397dd85dbef375006e` |
| cfg-core | `crates/cfg-core` | `b4131e61a939e0685194712689d1fd1497a41492` |
| cfg-pascal | `crates/cfg-pascal` | `dd64c7587cabf2bd3c4d4b3bed10e30743defcfc` |

All three are workspace members, and every former git dependency on them is a
path dependency. The grammar sits under `crates/` like the others: its Cargo
manifest is at the root of the grammar directory, so membership needs no layout
changes, and the non-Rust bindings stay where tree-sitter tooling expects them.

## Consequences

- A grammar, CFG and lint change lands in one commit and one review. There are
  no revisions to keep in step and no `.cargo/config.toml` overrides; the
  gitignored override file in existing checkouts must be deleted, or it will
  shadow the in-tree crates with the old sibling checkouts.
- `cargo fmt`, `cargo clippy --workspace` and `cargo test --workspace` now
  cover the three crates (their own unit and integration tests included). They
  keep their own editions and do not opt into the workspace lint table; doing
  so is a separate change.
- `.cargo/deny.toml` no longer allows any git source (`unknown-git = "deny"`
  with an empty allow list).
- The imported crates set `publish = false`, like the rest of the workspace.
  The grammar's nested `Cargo.lock` was removed: Cargo ignores lockfiles in
  workspace members.
- **Editing the grammar.** Edit `crates/tree-sitter-pascal/grammar.js`, then in
  that directory run `tree-sitter generate` and `tree-sitter test` with
  tree-sitter CLI 0.24 (the checked-in parser is ABI 14; `npm install` there
  provides the pinned CLI as `npx tree-sitter`; a newer system CLI produces a
  different parser). Commit `grammar.js` and the regenerated `src/parser.c`,
  `src/grammar.json` and `src/node-types.json` together with the Rust changes
  that rely on them. `bindings/rust/build.rs` compiles `src/parser.c` and
  `src/scanner.c` with the C compiler CI already uses.
- **Upstream grammar changes.** The fork still tracks `Isopod/tree-sitter-pascal`.
  Pull from it with
  `git subtree pull --prefix=crates/tree-sitter-pascal https://github.com/Isopod/tree-sitter-pascal.git <branch>`
  and regenerate.
- **The old repositories.** `github.com/AntoineGS/tree-sitter-pascal`,
  `cfg-core` and `cfg-pascal` are no longer used by the build. Archive them on
  GitHub (read-only) with a README pointer to this repository rather than
  deleting them, so old lint4d revisions that pin them still build. Changes
  made there after the imported revisions are not picked up; bring any over
  with `git subtree pull` before archiving.
