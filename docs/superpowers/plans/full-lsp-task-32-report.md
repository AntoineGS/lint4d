# Task 32: Organize imports source action — implementation report

Date: 2026-09-21
Worktree: `/home/antoinegs/gits/lint4d/.worktrees/full-lsp`
Base: `e5b904fb56c1edb8b86d306e5e8db86c47cab9bd`
Feature commit: `4dbf280` (`feat(pascal-lsp): organize imports source actions`)

## Delivered

- Added the standard `source.organizeImports` code-action capability and
  `context.only` filtering. Quickfix-only requests do not receive this source
  action.
- Added independent interface and implementation `uses`-clause planning. No
  import is removed merely because it has no syntactic reference; unit
  initialization and finalization semantics remain outside the unused-import
  heuristic.
- Added provider-aware duplicate removal. Entries are duplicates only when
  their case-insensitive/project- or namespace-resolved provider URI and
  explicit path qualifier match. The first entry and all other relative order
  are preserved.
- Added a proof-gated relative ordering transformation. It is offered only for
  complete source-backed, path-free providers with no initialization or
  finalization section, helper, exported-name conflict, or selected-provider
  dependency. The supported ordering is case-insensitive unit-name order; it
  is not advertised as arbitrary alphabetical sorting.
- Added conservative clause parsing and exact text edits. Comments, compiler
  directives, conditional or unknown branches, includes, multi-root physical
  provenance, parser recovery, malformed layouts, unresolved providers, and
  incomplete discovery are withheld rather than guessed or rewritten.
- Added eager edits for clients without action resolution and frozen deferred
  resolve identity. Resolution rechecks source/configuration generations,
  clause hashes, provider URI/source/conditional/order facts, discovery
  completeness, and the recomputed plan before returning an edit.
- Added navigation metadata for uses-clause boundaries, provider bindings, and
  source-backed ordering safety. Snapshot duplicate-import completeness now
  counts case-insensitive unique imported names so equivalent duplicates do not
  masquerade as incomplete discovery.
- Documented the safety policy and limits in `crates/pascal-lsp/README.md`.

## Safety and preservation policy

The organizer changes only a finite, trivia-free uses-clause subset. It never
merges interface and implementation scopes, moves an import between scopes,
removes an import because it is unused, or rewrites an include expansion. An
exact duplicate may be deleted even when its provider has initialization, but
that provider prevents relative reordering. Unknown or ambiguous provider
identity suppresses both transformations for that entry.

The current tree-sitter grammar recovers explicit `in 'path'` uses syntax in
the tested form. The planner therefore withholds that clause rather than
pretending it can prove the path binding; the behavior is covered by
`source_action_withholds_recovered_explicit_path_uses`. If a path-qualified
clause is parsed cleanly in a future grammar, its exact quoted path remains a
deduplication key and is never changed by a name-only edit. Comments and
directives are likewise structural boundaries, not whitespace to normalize.

Edits replace only unit-name bytes or the comma-plus-duplicate entry range.
Existing line endings, indentation, BOM/non-BMP text, surrounding text, and
path spelling are retained. A second request over the edited source produces
no action when no further proven change exists.

## Bounds

| Organizer resource | Bound |
| --- | ---: |
| `uses` clauses per request | 64 |
| parsed entries per request | 512 |
| clause bytes considered | 64 KiB per clause |
| generated edit text | 64 KiB per request |
| serialized action response | shared 64 KiB code-action bound |

Workspace/source discovery, snapshot, assistance-byte, output, and dependency
bounds remain active. Cancellation is checked while planning entries and while
building the source-backed snapshot; incomplete or cancelled proofs do not
produce partial edits.

## RED/GREEN evidence

The first organizer regression was run against the base implementation. The
retained log is `/tmp/opencode/task32-red-initial.log`.

RED command:

```text
cargo test --locked -p pascal-lsp --test protocol source_action_organizes_equivalent_uses_without_reordering_bindings -- --exact --nocapture
```

RED result: exit 101. The base server returned zero actions (`left: 0`) while
the regression expected one source action (`right: 1`).

GREEN evidence:

```text
cargo test --locked -p pascal-lsp --test protocol organize -- --nocapture
```

Result: 2 passed, 0 failed (the eager duplicate and deferred safe-order tests).

```text
cargo test --locked -p pascal-lsp --test protocol withholds_recovered -- --nocapture
```

Result: 1 passed, 0 failed (the explicit-path/parser-recovery withholding
regression).

## Verification

- `cargo test --locked -p pascal-lsp --lib`: 354 passed, 0 failed.
- `cargo test --locked -p pascal-lsp --test protocol`: 533 passed, 0 failed.
- `cargo test --locked -p pascal-lsp --test neovim`: 7 passed, 0 failed.
- `cargo test --locked -p pascal-lsp --features test-support --test protocol_barriers -- --test-threads=1`: 631 passed, 0 failed.
- The isolated existing timing-sensitive control-byte test passed: 1 passed,
  0 failed; log `/tmp/opencode/task32-large-ordinary-focused.log`.
- `cargo fmt --manifest-path crates/pascal-lsp/Cargo.toml -- --check`,
  `git diff --check`, `cargo check --locked --workspace --all-targets`, and
  `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`
  all passed; log `/tmp/opencode/task32-verification-static.log`.
- Deterministic full workspace verification passed with
  `cargo test --locked --workspace --all-features --jobs 1 -- --test-threads=1`:
  631 normal protocol tests, 631 barrier protocol tests, 355 LSP library
  tests, 84 rename tests, 21 project-library tests, 12 public-API tests, and
  all remaining workspace/doc tests passed with 0 failures. Log:
  `/tmp/opencode/task32-verification-workspace-serial.log`.

The unmodified parallel command `cargo test --locked --workspace
--all-features` was also exercised twice. Its duplicated protocol targets
occasionally timed out the pre-existing large ordinary-result test under
cross-target contention (630 passed, 1 failed); the test passed in isolation,
and the serial full-workspace command above passed both duplicate targets.
The first broad-run log is `/tmp/opencode/task32-verification-workspace.log`;
the rerun is `/tmp/opencode/task32-verification-workspace-rerun.log`.

## Limitations

This is deliberately not an unused-import linter or a compiler replacement.
The supported parser subset refuses comments, line comments, directives,
conditional activity, includes, recovered/malformed clauses, ambiguous
providers, and incomplete discovery. Explicit path clauses that enter grammar
recovery are withheld. Providers with helpers, initialization/finalization,
export collisions, or selected-provider dependencies retain their original
relative order, although independently proven exact duplicates may still be
removed. No file outside the editable source document is changed.
