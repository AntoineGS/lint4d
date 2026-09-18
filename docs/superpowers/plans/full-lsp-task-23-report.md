# Full LSP Task 23 Verification Report

This report records the implementation and fresh local verification for
source-bearing Pascal include expansion in `full-lsp`.

## Implemented

- Added a bounded recursive `{$I ...}`/`{$INCLUDE ...}` expansion engine with
  reversible physical source maps, repeated/nested include occurrences, cycle
  detection, cancellation checks, and depth/source/directive/byte/segment/work
  limits.
- Preserved requester-scoped readable-root authorization and
  `ProjectPathEntry` provenance through nested include resolution. Open-buffer
  overlays take precedence over disk content, including include-only overlays.
- Carried known `DEFINE`/`UNDEF` facts across expanded source while keeping
  unknown or incomplete conditional activity fail-closed.
- Integrated expanded virtual buffers into navigation, references, document
  highlights, and diagnostics. Locations and diagnostics map back to the
  physical source URI/range, including Unicode and CRLF input; repeated
  physical occurrences are retained.
- Invalidated indexed parents and published dependent diagnostics when an
  include changes, disappears, or is replaced by an overlay. Stale and
  cancelled work is rejected before publication.
- Extended rename snapshots to audit and expand active includes. Fully
  resolved, complete includes can contribute physical `.inc`/Pascal edits;
  synthetic, cross-segment, stale, unmapped, incomplete, or unauthorized edits
  are rejected without partial output.
- Kept formatting restricted to `.pas`, `.dpr`, and `.dpk`; `.inc` files are
  analyzable but are rejected as direct formatting targets.
- Added unit, navigation, protocol, diagnostics, overlay, invalidation,
  conditional, provenance, bounds, cancellation, source-map, reference,
  highlight, rename, stale-validation, and formatting regressions.
- Documented include behavior and conservative limitations in
  `crates/pascal-lsp/README.md`.

## Verification

All commands below were run fresh on the final pre-commit tree and exited 0:

- `cargo fmt --manifest-path crates/pascal-lsp/Cargo.toml -- --check`
- `cargo check --workspace --all-targets --locked`
- `cargo clippy --manifest-path crates/pascal-lsp/Cargo.toml --all-targets --no-deps -- -D warnings`
- `cargo test --workspace --locked`
- `cargo test --locked -p pascal-lsp --all-targets --all-features`
- `git diff --check`
- Repository `.git/hooks/pre-commit` (workspace formatting, strict Clippy,
  and quiet workspace tests)

The workspace test run passed every target, including the Pascal LSP unit
target (284 tests), navigation (295), protocol (400), and rename (72). The
all-target/all-feature Pascal LSP run passed the feature-enabled unit target
(285), navigation (295), protocol (490), protocol barriers (490), and rename
(72), with no failures.

Complete command output is retained locally at
`/tmp/opencode/task23-final-verification.txt` (SHA-256
`bd3a66a020c72139f887b4411c179e24a9f38aa953cd42f00566bafc3e315152`). The
pre-commit output is at `/tmp/opencode/task23-hook-verification.txt` (SHA-256
`731cc2f55d2a4face6389b5c14c092d04d87970f4401302c8a6a4793a4078004`).

## Deliberate limitations

- The expander does not emulate compiler-version, `IFOPT`, environment, or
  Pascal-dependent conditional state. Unknown active include activity withholds
  diagnostics and blocks unsafe navigation/rename guesses.
- Missing, unreadable, cyclic, out-of-root, stale, or incompletely audited
  active includes do not produce partial locations or edits.
- Expansion remains bounded by the configured resource limits; exceeding a
  limit fails closed.
