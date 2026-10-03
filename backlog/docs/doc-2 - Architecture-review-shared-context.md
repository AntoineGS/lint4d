---
id: doc-2
title: 'Architecture review: shared context'
type: guide
created_date: '2026-10-03 01:28'
tags:
  - arch-review
---
Shared context for every task labelled `arch-review`. It holds the parts of the
2026-10-02 architecture review that apply to all of its tasks rather than to one. Each task's own
description, acceptance criteria and dependencies carry the task-specific detail.

## Scope and method

- **Scope:** the lint4d workspace (`pascal-project`, `pascal-core`, `lint4d`, `fmt4d`,
  `pascal-lsp`) and the sibling repos it pins: `../tree-sitter-pascal`, `../cfg-core`,
  `../cfg-pascal`.
- **Method:** static review by five reviewers, one per domain, plus build timings taken on the
  maintainer's machine. Runtime claims describe algorithmic shape, not measured timings, unless a
  number is given.
- **Citations:** every `file:line` was spot-checked at commit `8af7498`. Line numbers drift; grep
  for the quoted identifiers if a citation no longer lands, and fix the citation in the task.
- **Task conventions:** "Done when" became acceptance criterion #1. "Tests first" lists the
  regression or characterization tests to add before changing behaviour; for pure refactors it names
  the suites that must stay green. "Depends on" in the description includes soft or optional
  ordering; hard dependencies are also set as task dependencies. "Do not" marks scope boundaries.
- **Review IDs:** task titles keep the review ID (`LSP-3`, `CORE-7`, ...). Descriptions refer to
  other tasks by review ID; the table at the end maps them to task IDs.
- **AGENT_TODOS.md:** descriptions that mention `AGENT_TODOS.md` refer to the former agent TODO
  list. Its text is archived in doc-1 and its items are now tasks labelled `agent-todos`.

## Global constraints

Every arch-review task's requirements implicitly include these.

- Toolchain: `rust-toolchain.toml` pins `1.99.0`. `cargo fmt --check` and
  `cargo clippy --workspace --all-targets -- -D warnings` must pass.
- `warnings = "deny"` and `clippy::dbg_macro = "deny"` are workspace lints.
- Sibling repos are git dependencies pinned by rev. The tree-sitter-pascal rev must be identical in
  `crates/lint4d/Cargo.toml`, `crates/pascal-core/Cargo.toml` and `../cfg-pascal/Cargo.toml`.
  Bump cfg-pascal first, push, then bump here. A gitignored `.cargo/config.toml` with `paths`
  overrides builds against the checkouts; it cannot change a crate's dependency list. See
  `CLAUDE.md`, section "Editable dependencies".
- No new git dependencies: `.cargo/deny.toml` has `unknown-git = "deny"`.
- Some tests and clippy lints already fail on master; they are listed in TASK-1.1. Do not count those
  as regressions you caused, but do not add to them.
- Bug-shaped tasks follow the TDD rule in `CLAUDE.md`: failing regression test first, minimal fix,
  refactor.

## Measured baselines

Re-run these after any task that claims a build or test win, and record before and after numbers in
the task's final summary. Several tasks refer to this as "the baseline table".

| Measurement | Value | Command |
|---|---|---|
| Full workspace build, all targets, warm deps | 2m04s wall, 183% CPU | `time cargo build --workspace --all-targets --locked` |
| Touch one leaf file in pascal-lsp, rebuild its targets | 1m07s wall | `touch crates/pascal-lsp/src/text.rs && time cargo build -p pascal-lsp --all-targets --locked` |
| Touch one leaf file in pascal-project, rebuild workspace | 9.7s wall | `touch crates/pascal-project/src/path_issues.rs && time cargo build --workspace --all-targets --locked` |
| Test executables produced | 72 | `cargo test --workspace --no-run 2>&1 \| grep -c Executable` |

Size facts at commit `8af7498` that motivate the structural tasks:

| File or count | Value | Notes |
|---|---|---|
| `crates/pascal-lsp/src/navigation.rs` | 27,733 lines | tests from line 21499 |
| `crates/pascal-lsp/src/workspace.rs` | 24,098 lines | tests from line 15353 |
| `crates/pascal-lsp/src/server.rs` | 20,723 lines | tests from line 15329 |
| `crates/pascal-lsp/src/workspace/rename.rs` | 12,693 lines | |
| `crates/pascal-project/src/lib.rs` | 11,207 lines | tests from line 9614 |
| `crates/pascal-lsp/tests/protocol.rs` | 60,273 lines | 855 tests, 835 `TestServer::launch` sites, 21 `thread::sleep` |
| Named `MAX_*`/`*_LIMIT`/`*_BUDGET` constants in pascal-lsp | 261 | pascal-core has 9, lint4d 0 |
| Functions in pascal-lsp | 3,377 | |

## Why the LSP tasks matter

The `lsp` tasks in milestones M2, M3 and M6 explain the symptoms seen on the 24,050-file production
repository: workspace-wide requests burn a core for 8+ minutes and then fail, RSS grows by gigabytes
during indexing, and "analysis result became stale" is frequent. The review traced these to three
compounding choices: workspace semantics rebuilt per request instead of indexed (LSP-16 and its
consumers), every result requiring global completeness (LSP-1, LSP-2), and bounding machinery that
does not bound actual memory or latency (LSP-3, LSP-4, LSP-5, LSP-9).

## Order and parallel work

- Milestones M1 to M7 follow the review's suggested order, and task ordinals follow it within each
  milestone. M1 unblocks verification and should land first: nothing else can show a green
  `cargo test --workspace` until TASK-1 (BUILD-6) and its subtasks are done.
- Tasks in the same milestone without a dependency link can run in parallel, each in its own
  worktree.
- Never let two agents hold LSP-16, LSP-17, LSP-18 or LSP-19 (TASK-33 to TASK-36) at the same time;
  they edit the same files.
- Mechanical file splits (LSP-25, CORE-9) move large regions. Coordinate them with any task editing
  the same file so two agents are not moving the same code.

## Review ID to task ID

| Review ID | Task | Title |
|---|---|---|
| LSP-1 | TASK-6 | Read-only requests return partial results with coverage metadata |
| LSP-2 | TASK-7 | Semantic diagnostics keep proven findings when a budget trips |
| LSP-3 | TASK-5 | Replace the permanent recovery fence with cache invalidation |
| LSP-4 | TASK-10 | Make memory accounting cover owned heap and outstanding snapshots |
| LSP-5 | TASK-11 | Sparse newline map and position index |
| LSP-6 | TASK-12 | Stop copying overlays and inputs per request and per import lookup |
| LSP-7 | TASK-14 | Validate partial-result chunks once, not per chunk |
| LSP-8 | TASK-13 | Take superlinear scans and watcher calls out of the cache mutex |
| LSP-9 | TASK-8 | Make the interactive slot actually isolate interactive latency |
| LSP-10 | TASK-15 | Reconcile mutations without taking the whole workspace |
| LSP-11 | TASK-17 | One prepared artifact per source revision for diagnostics |
| LSP-12 | TASK-18 | Warm cache hits should not read and decode the file first |
| LSP-13 | TASK-16 | Keep protocol-thread result commits proportional to the change |
| LSP-14 | TASK-19 | Cache coordinate indexes and binary-search source maps |
| LSP-15 | TASK-20 | Share directory catalogues across requests and prune excluded trees |
| LSP-16 | TASK-33 | Versioned workspace index for symbols, occurrences and reverse dependencies |
| LSP-17 | TASK-34 | Rename post-edit proof as one transactional overlay |
| LSP-18 | TASK-35 | Index include owners and drop the 256-owner discovery cap |
| LSP-19 | TASK-36 | Freshness witnesses validated by generation, not by re-reading |
| LSP-20 | TASK-9 | Fix-all naming validates one batch, not growing prefixes |
| LSP-21 | TASK-48 | One memoized type and expression query layer |
| LSP-22 | TASK-49 | Remove budgeted/unbudgeted duplicates and repeated AST primitives |
| LSP-23 | TASK-50 | Cut allocation in identifier hot paths |
| LSP-24 | TASK-37 | Serve code lenses and hierarchies from cached graphs |
| LSP-25 | TASK-31 | Split the three monolith files along existing seams |
| LSP-26 | TASK-73 | Quarantine template-only refactorings |
| CORE-1 | TASK-38 | One owner for project metadata interpretation |
| CORE-2 | TASK-39 | Typed completeness instead of warning-string matching |
| CORE-3 | TASK-40 | One conditional and include analysis per source occurrence |
| CORE-4 | TASK-41 | Directory listings as shared indexes, no re-stat |
| CORE-5 | TASK-42 | Indexed observation accumulator |
| CORE-6 | TASK-43 | Bound the case-insensitive path-resolution walk |
| CORE-7 | TASK-70 | Move partial-fragment syntax into the grammar |
| CORE-8 | TASK-51 | Canonical immutable `ProjectContext` and a complete cache key |
| CORE-9 | TASK-32 | Split `pascal-project/src/lib.rs` |
| CORE-10 | TASK-52 | Shared text and path primitives with explicit contracts |
| CORE-11 | TASK-27 | Drain MSBuild pipes while waiting |
| CORE-12 | TASK-53 | Narrow the public API surface |
| CORE-13 | TASK-54 | Named operation budgets instead of scattered constants |
| LINT-1 | TASK-21 | CFG routine identity must not collide on overloads |
| LINT-2 | TASK-44 | Run-scoped shared project analysis in the CLI |
| LINT-3 | TASK-45 | Per-unit fidelity instead of whole-project fallback |
| LINT-4 | TASK-22 | Naming auto-fix must be binding-aware or restricted |
| LINT-5 | TASK-23 | DCU parser capabilities, and unknown versus absent |
| LINT-6 | TASK-24 | Use-after-free on AST effects, deduplicated after convergence |
| LINT-7 | TASK-46 | Separate "needs project trust", "needs routine index" and "needs CFG" |
| LINT-8 | TASK-55 | One syntax index per file for the simple rules |
| LINT-9 | TASK-47 | One conditional evaluator shared with cfg-pascal preparation |
| LINT-10 | TASK-28 | Single-flight, negative-cached DCU loading |
| LINT-11 | TASK-56 | Shared source-semantic facts below the LSP for exception dispatch |
| LINT-12 | TASK-57 | Centralize grammar helpers |
| LINT-13 | TASK-58 | Single-pass edit application and neutral traversal budgets |
| LINT-14 | TASK-59 | Indexed position conversion for prepared diagnostics |
| LINT-15 | TASK-72 | Benchmarks that cover the expensive paths; fix the empty snapshot |
| LINT-16 | TASK-60 | Fold cfg-core into cfg-pascal |
| FMT-1 | TASK-25 | Blank-line normalization must not touch protected spans |
| FMT-2 | TASK-26 | Uses clause must preserve comments and EOF trivia |
| FMT-3 | TASK-61 | Index blank lines once |
| FMT-4 | TASK-62 | Fill rendering without `Vec::remove(0)` |
| FMT-5 | TASK-63 | Round-trip oracle compares token content |
| FMT-6 | TASK-64 | One trivia index for comments and directives |
| FMT-7 | TASK-65 | Shared CLI input and config orchestration |
| GRAM-1 | TASK-66 | Track parser size and narrow preprocessor conflicts |
| GRAM-2 | TASK-67 | Abort rejected fragment scans at the first newline |
| GRAM-3 | TASK-68 | One conditional-node adapter for consumers |
| GRAM-4 | TASK-4 | Reproducible generation check in CI |
| GRAM-5 | TASK-69 | Fork metadata |
| BUILD-1 | TASK-29 | Move the protocol behavior matrix in-process |
| BUILD-2 | TASK-30 | Separate the barrier target and run it in CI |
| BUILD-3 | TASK-2 | CI toolchain pin and job consolidation |
| BUILD-4 | TASK-3 | Decide the multi-repo pinning model |
| BUILD-5 | TASK-71 | Measure the dev-profile optimization policy |
| BUILD-6 | TASK-1 | Repository hygiene |
