# Task 35: Binding-family references and coordinated rename — report

Date: 2026-09-22
Worktree: `/home/antoinegs/gits/lint4d/.worktrees/full-lsp`
Base: `f463c56`

## Delivered

- Added exact virtual/dynamic override-family identity to references,
  highlights, and rename. A proven virtual root plus compatible, source-backed
  `override` descendants share one slot; same-name overloads, helpers, static
  methods, hiding/reintroduced methods, and unproven ancestry remain separate or
  fail closed.
- Reused instantiated owner substitutions and routine contract matching for
  generic ancestry. Unknown or incompatible substitutions/signatures do not
  widen a family.
- Preserved name-free `inherited;` as a semantic family occurrence without
  inventing a text range, while explicit inherited and qualified uses require
  an actual resolved candidate.
- Added immutable post-edit reparse/rebind proof before workspace edit
  admission. Direct documents are rebound directly; physical include edits are
  mapped back to their unique owning virtual roots and rebound there. Repeated,
  ambiguous, incomplete, stale, read-only, conditional, or conflicting edits
  remain request-level failures with no partial edit.
- Added clone support for navigation proof snapshots and documented the family,
  include, scope, and conservative-unsupported policies in the LSP README.

## Proof model and bounds

The family starts from the selected binding identity, not spelling. Candidate
members must have matching routine kind, staticness, calling convention,
generic-aware owner substitution, parameter/result contract, and proven
ancestor/descendant identity. Source declaration/implementation cardinality is
validated before occurrence collection. Workspace discovery remains bounded and
authorized by the existing project/source catalogue and freshness records.

Post-edit proof applies every proposed edit to an immutable source snapshot,
rebinds each changed direct document or owning expanded include root, then
requires the resolved binding locations to equal the complete transformed edit
set. Mapping work is bounded by `MAX_SNAPSHOT_MAPPING_WORK`; semantic proof
materialization is bounded by `MAX_SNAPSHOT_SEMANTIC_BYTES`; existing workspace,
include, location, source, and cancellation limits remain authoritative.

## RED/GREEN evidence

The original Task 35 RED probe was run before implementation:

```text
cargo test -p pascal-lsp --test rename virtual_override_family_renames_exact_slot_and_dispatch_calls -- --exact
```

RED: exit 101; the override-family rename test failed because the pre-task
implementation did not coordinate the exact virtual slot family.

During final integration, the new post-edit proof initially failed four
include tests with `document is not indexed` because included physical files
are intentionally not standalone navigation documents. The corrected proof
rebinds owning virtual roots. The stale-source protocol regression also
initially exposed a generic unresolved error masking a disk race; the final
boundary converts that case to the required stale-source failure.

Final focused GREEN:

```text
cargo test -p pascal-lsp --test rename
```

Result: 94 passed, 0 failed.

Reviewer regression coverage added in the final rename suite includes sibling
override branches, transitive descendants, same-named virtual hiding,
reintroduced virtual slots, incomplete concrete families, and selecting a
generic concrete child with its parent substitution. Workspace-level coverage
also rejects unknown intermediate override-slot signatures, including
omitted-versus-explicit calling conventions, and rejects unsupported virtual
class-method overrides.

## Verification

- `cargo test --locked -p pascal-lsp --test rename`: 94 passed.
- `cargo test --locked -p pascal-lsp --test navigation`: 299 passed.
- `cargo test --locked -p pascal-lsp --test project`: 86 passed.
- `cargo test --locked -p pascal-lsp --test protocol`: 559 passed.
- `cargo test --locked -p pascal-lsp --test neovim`: 7 passed.
- `cargo test --locked -p pascal-lsp --features test-support --test protocol_barriers -- --test-threads=1`: 657 passed.
- `cargo test --locked -p pascal-lsp --lib`: 382 passed.
- `cargo check --locked -p pascal-lsp --all-targets`: passed.
- `cargo clippy --locked -p pascal-lsp --all-targets --no-deps -- -D warnings`: passed.
- `cargo fmt --manifest-path crates/pascal-lsp/Cargo.toml -- --check`: passed.
- `git diff --check`: passed.

An initial serial barrier invocation exceeded the 120-second command timeout
after reporting no test failures; the final isolated rerun completed
successfully with all 657 tests passing.

## Limitations

This remains a bounded source model, not a Delphi compiler. Compiler-only
consumers, external/compiled descendants, unknown conditional branches,
ambiguous overloads or ancestry, unsupported calling conventions, incomplete
project/provider discovery, repeated contextual includes, and unauthorized or
read-only sources are intentionally conservative. Unit/module/file-resource
rename remains unsupported. No filesystem source is modified by proof or by
the analysis worker.

No push, merge, amend, or resynchronization was performed.
