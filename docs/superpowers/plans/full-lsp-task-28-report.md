# Task 28: Workspace and Pull Diagnostics — Implementation Report

Date: 2026-09-19
Worktree: `/home/antoinegs/gits/lint4d/.worktrees/full-lsp`
Base: `f21058e72854f6ad3303aad197577663e41558fa`
Commit: `734566b1f75e2ef281a3a5bfaa64723f5465c5f7`

## Delivered

- Added negotiated `textDocument/diagnostic` and `workspace/diagnostic` routing
  on the existing bounded analysis scheduler.
- Added capability negotiation for the `pascal-lsp` provider, inter-file
  dependencies, related-document reports, workspace reports, and diagnostic
  refresh. Clients without pull support retain debounced push diagnostics.
- Added full/unchanged reports, opaque process-local result IDs, previous-ID
  validation, deterministic workspace ordering, and explicit empty clears for
  deleted or newly excluded documents.
- Added authorized unopened-document reads using the existing project/source
  read policy. Unauthorized requested URIs receive a bounded diagnostic without
  reading the path.
- Added physical include/dependent related reports with ownership and UTF-16
  mapping checks, worker-side disk freshness validation, stale-result
  `-32802`/`retriggerRequest` handling, and explicit `-32800` cancellation.
- Added coalesced, bounded `workspace/diagnostic/refresh` request handling and
  suppressed push publication for clients that negotiated pull ownership.
- Added workspace partial-result delivery without confusing partial items with
  the final report or work-done progress. Incomplete bounded workspace scans
  fail instead of being reported as complete.
- Added bounded diagnostic result/cache state and workspace report encoding
  checks. Code actions now tolerate an overlapping stale diagnostic range after
  an asynchronous pull update.
- Documented the protocol behavior and limits in
  `crates/pascal-lsp/README.md`.

## Protocol policy

`diagnosticProvider.identifier` is `pascal-lsp` and
`interFileDependencies` is true. The regular pull-capability shape advertises
workspace reports. The current plural `workspace/diagnostics` capability shape
used by some Neovim development clients receives document pull and refresh
compatibility while workspace-provider advertisement is suppressed. A client
that does not advertise `textDocument.diagnostic` uses the existing push path;
the server does not publish contradictory push diagnostics to a negotiated
pull client.

Document reports reuse a result ID only when diagnostics, source/dependency
records, source/configuration generations, and live dependency freshness still
match. Unknown, foreign, or evicted IDs return full reports. Workspace reports
contain URI-sorted per-document full or unchanged entries and clear previous
entries that are no longer current. Related reports are only returned for
physical documents owned by the requesting context. Refresh requests are
coalesced to one in flight, and errors, late replies, duplicates, and shutdown
are non-blocking.

## Bounds

| Resource | Bound |
| --- | ---: |
| Retained pull result entries | 2,048 |
| Retained pull result/cache bytes | 32 MiB |
| Workspace report documents | 10,000 |
| Workspace report item bytes | 64 KiB |
| Workspace encoded report bytes | 7 MiB |
| Partial workspace items per chunk | 128 |
| Partial chunk bytes | 64 KiB |
| Partial chunks pumped per event-loop turn | 1 |
| General LSP payload | 8 MiB |

The existing workspace file, source-byte, analysis queue, output backpressure,
and semantic-diagnostic budgets remain active. A bounded workspace catalogue
that cannot be completed is an error, not a silently truncated complete result.

## RED/GREEN evidence

The new protocol test was applied to a throwaway worktree at the base commit
`f21058e72854f6ad3303aad197577663e41558fa`.

RED command:

```text
cargo test --locked -p pascal-lsp --test protocol pull_diagnostics_advertises_provider_and_returns_document_report -- --exact --nocapture
```

RED result: exit 101; the base server returned `null` for
`capabilities.diagnosticProvider.identifier`, while the test expected
`"pascal-lsp"` (`left: Null`, `right: "pascal-lsp"`).

GREEN command:

```text
cargo test --locked -p pascal-lsp --test protocol pull_diagnostics_advertises_provider_and_returns_document_report -- --exact --nocapture
```

GREEN result on commit `734566b1f75e2ef281a3a5bfaa64723f5465c5f7`: 1 passed,
0 failed, 453 filtered out.

## Verification

- `cargo test --locked -p pascal-lsp`: 454 protocol tests and 84 rename tests
  passed; 0 failed.
- `cargo test --locked -p pascal-lsp --features test-support --test protocol pull_ -- --nocapture`:
  20 passed; 0 failed.
- `cargo test --locked -p pascal-lsp --features test-support --test protocol_barriers -- --test-threads=1`:
  551 passed; 0 failed.
- `cargo check --locked -p pascal-lsp --all-targets`: passed.
- `cargo fmt --manifest-path crates/pascal-lsp/Cargo.toml -- --check`: passed.
- `cargo clippy --locked -p pascal-lsp --all-targets --no-deps -- -D warnings`:
  passed.
- The commit hook ran format, Clippy, and workspace tests and ended with
  `All checks passed.`

## Limitations

Diagnostics remain source-backed and bounded; this is not a Delphi compiler or
a complete type checker. Unsupported, ambiguous, incomplete, unauthorized, or
conditional/recovery-dependent source remains conservative rather than guessed.
Result IDs are process-local and clients must treat them as opaque. Cache
eviction or a changed dependency intentionally causes a full report. The
workspace provider scans only the authorized catalogue and does not blanket
scan outside configured roots or external source paths.

## Corrective round 1

The independent review in `docs/superpowers/plans/full-lsp-task-28-review.md`
recorded findings R1–R8. The corrective round made the following changes:

- Canonical LSP 3.17 `workspace.diagnostics` and the pinned typed singular
  spelling both enable workspace-provider advertisement. Text-document-only
  clients do not receive the workspace provider. Neovim 0.12 is narrowly kept
  on document-pull refresh because its workspace refresh handler ignores
  workspace reports for attached document-pull buffers and opens unopened
  report URIs; ordinary clients retain the canonical workspace behavior.
- Pull-owned diagnostics no longer use push debounce deadlines for event-loop
  timeouts. Refresh state is retired on shutdown, including pending and late
  responses.
- Related reports retain bounded root-to-physical ownership, emit explicit full
  empty clears when a contribution disappears, and preserve contributions from
  other roots.
- Watched source/catalogue changes, configuration changes, workspace-folder
  changes, and overlay closes request coalesced pull refreshes, including for
  authorized unopened files.
- Workspace diagnostics establish the bounded competing-root expansion set
  before evaluating context-sensitive physical include diagnostics.
- Workers prepare compact bounded dependency evidence and stable effective
  fingerprints that exclude version-only and disk-stamp-only observations.
  Workspace entries retain per-publication dependency evidence, and cache
  pressure degrades to an uncached full report instead of rejecting a valid
  small report.

The corrective regressions cover standard capability parsing, text-only
capability rejection, expired pull deadlines, open and unopened workspace
reports, unopened refresh, late shutdown responses, related empty clears,
shared-owner preservation, unopened competing roots, no-op disk/overlay edits,
unrelated workspace reuse, and oversized-cache degradation.

### Detached-base chronology

The retained RED probes and the base-commit protocol regression establish that
the reviewed implementation did not satisfy these cases. They are regression
detection evidence run against detached base snapshots, not a claim that every
corrective test was observed failing before edits in this worktree. The original
report above remains preserved, and its original misplaced copy is preserved at
`/home/antoinegs/docs/superpowers/plans/full-lsp-task-28-report.md`.

### Corrective evidence

Observational probe outputs are retained outside the repository:

- `/tmp/opencode/task28-round1-current-probe.stdout` — canonical capability,
  zero idle CPU after opening, related clears, unopened refresh, and competing
  ownership controls.
- `/tmp/opencode/task28-round1-current-extra.stdout` — no-op disk/overlay
  unchanged IDs, shutdown refresh retirement, and bounded scan measurements.
- `/tmp/opencode/task28-round1-current-bounds.stdout` and
  `/tmp/opencode/task28-round1-current-controls.stdout` — bounded work,
  protocol responsiveness, and push/code-action controls.

The final verification commands and results are recorded below after the
corrective implementation is complete. No push, merge, amend, or
resynchronization is performed by this worktree.

### Final verification

- `cargo test --locked -p pascal-lsp --test protocol pull_ -- --nocapture`:
  23 passed; 0 failed.
- `cargo test --locked -p pascal-lsp --lib -- --nocapture`: 352 passed; 0
  failed.
- `cargo test --locked -p pascal-lsp --test neovim -- --nocapture`: 7 passed;
  0 failed.
- `cargo test --locked -p pascal-lsp --features test-support --test protocol_barriers -- --test-threads=1`:
  558 passed; 0 failed.
- `cargo check --locked --workspace --all-targets`: passed.
- `cargo test --locked --workspace`: passed, including 461 protocol tests and
  the Neovim integration suite.
- `cargo test --locked --workspace --all-features`: passed, including both
  serial barrier runs (558 tests each).
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`:
  passed.
- `cargo fmt --check` and
  `cargo fmt --manifest-path crates/pascal-lsp/Cargo.toml -- --check`: passed.
- The configured pre-commit hook was run during the final commit and completed
  its format, Clippy, and test checks successfully.
