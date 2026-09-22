# Task 34: Identity-Safe Unit References and Document Highlights

Date: 2026-09-21
Worktree: `/home/antoinegs/gits/lint4d/.worktrees/full-lsp`
Base: `41ed3aa970289c51018d8bed792b4f5cb65c758d`

## Delivered

- Added binding-resolved `textDocument/documentHighlight` results with LSP
  `Text`, `Read`, and `Write` kinds.
- Reused the selected binding identity for highlights, so same-spelled
  identifiers from unrelated locals, units, overloads, properties, or
  ambiguous contexts are not merged.
- Classified declarations and uncertain/unsupported storage as `Text`, proven
  value reads as `Read`, and proven storage operations as `Write`. AST storage
  forms include assignment bases, indexed expressions, dereferences, foreach
  iterators, and resolved `var`/`out` call arguments.
- Added identity-safe unit references and highlights. Unit declarations,
  `uses` entries, aliases, and qualified references use the complete bound
  prefix span. Explicit project unit aliases determine the provider identity;
  unit rename remains unsupported.
- Preserved virtual include/conditional query mapping, cancellation, stale
  snapshot handling, and bounded work/byte/result limits. Highlight overflow
  uses the same fail-closed 10,000-entry error as references.
- Added a negative cache for repeated unresolved unqualified call targets and
  a bounded identifier-span lookup for large highlight documents, keeping the
  exact 10,000-entry boundary responsive without weakening semantic checks.
- Updated protocol/unit/navigation tests and documented the policy in
  `crates/pascal-lsp/README.md`.

## Shared interfaces

- `NavigationIndex::unit_reference_candidates_at*` resolves unit declarations,
  `uses` entries, aliases, and qualified prefixes before ordinary symbol
  fallback. The resolver refuses a unit fallback when a bound or inaccessible
  receiver already owns the prefix.
- `NavigationIndex::binding_locations_impl` remains the shared occurrence
  collector for references and rename. Unit occurrences use the selected
  provider identity and replace the component span with the complete bound
  prefix; the `allow_unit` gate keeps unit/module rename unsupported.
- `NavigationIndex::binding_highlights_in_document_with_cancel_and_work_budget`
  reuses the binding plan and occurrence collector, then classifies each local
  AST occurrence. `workspace::rename::binding_highlights_in_document` maps
  virtual locations through include overlays and deduplicates only after
  physical mapping while retaining each LSP highlight kind.
- `workspace::queries::highlights_from_input` uses the same owner, read-set,
  source/configuration generation, cancellation, and stale-result barriers as
  references. No protocol-thread filesystem or semantic traversal was added.

## Policy

References and highlights use the resolver's selected symbol identity rather
than textual matching. Unit occurrences are represented by the complete
provider prefix (`Ns.Provider`, `Alias`), including project-configured aliases.
Highlights are document-local and include declarations. A declaration or
routine/type/unit reference is `Text`; proven right-hand values, bases, indices,
receivers, and value arguments are `Read`; proven assignments and writable
`var`/`out` arguments are `Write`. Ambiguous calls, uncertain properties,
addresses, unsupported lookup, unknown conditionals, and parser-recovery
contexts remain `Text` or fail closed rather than being guessed.

## Bounds and failure behavior

- Physical reference/highlight responses are capped at 10,000 entries.
- Highlight semantic traversal is bounded by the existing assistance work and
  byte budgets; identifier-span lookup and unresolved-call caching charge their
  work/materialization to those budgets.
- Include expansion and virtual-to-physical mapping retain their existing
  bounded mapping and resolution budgets. Physical ranges are deduplicated
  after mapping while preserving the classified highlight kind.
- Cancellation is checked during binding resolution, occurrence collection,
  role classification, position conversion, and physical mapping. Incomplete
  or stale snapshots are not published as successful partial results.

## RED/GREEN evidence

The exact boundary regression initially timed out while waiting for the
highlight response:

```text
cargo test --locked -p pascal-lsp --features test-support --test protocol \
  references_and_highlights_enforce_exact_10000_entry_boundaries \
  -- --exact --nocapture
```

The corrected implementation passes the same command with 1 passed and 0
failed. The regression verifies 10,000 reference locations, 10,000 highlights,
and fail-closed behavior for 10,001 entries.

## Verification

### RED/GREEN follow-up evidence

- Unit receiver precedence RED: the existing navigation regression
  `inaccessible_unqualified_receiver_does_not_fall_back_to_imported_unit`
  selected the imported unit for an inaccessible receiver member. GREEN:
  `cargo test --locked -p pascal-lsp --test navigation
  inaccessible_unqualified_receiver_does_not_fall_back_to_imported_unit --
  --exact --nocapture` — 1 passed, 0 failed, 298 filtered out.
- Neovim compatibility RED: the full workspace run reached the new highlight
  response and failed because the smoke fixture still asserted that `kind` was
  absent. The unresolved `Log(...)` argument is intentionally `Text`, not
  `Read`, under the uncertainty policy. GREEN:
  `cargo test --locked -p pascal-lsp --test neovim
  neovim_standard_symbol_reference_and_highlight_queries -- --exact
  --nocapture` — 1 passed, 0 failed, 6 filtered out.
- Exact boundary RED/GREEN: the retained boundary run initially timed out
  while constructing the highlight response. The corrected command
  `cargo test --locked -p pascal-lsp --features test-support --test protocol
  references_and_highlights_enforce_exact_10000_entry_boundaries -- --exact
  --nocapture` passes with 1 passed, 0 failed. It verifies exactly 10,000
  references, exactly 10,000 highlights, and fail-closed behavior at 10,001.
- Strict Clippy initially reported `clippy::too_many_arguments` for the
  bounded occurrence classifier. The project-local allow was added to that
  helper; the final strict Clippy commands pass.

### Final commands and results

- `cargo test --locked --workspace --all-features`: passed. The final
  `pascal-lsp` targets were 375 library tests, 299 navigation tests, 7
  Neovim tests, 86 project tests, 651 protocol tests, 651 serial
  `protocol_barriers` tests, and 84 rename tests; all had 0 failures. Other
  workspace targets and doctests also passed.
- `cargo check --locked --workspace --all-targets`: passed.
- `cargo clippy --locked --workspace --all-targets -- -D warnings`: passed.
- `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`:
  passed.
- `cargo fmt --check`: passed (the active pre-commit formatter command).
- `cargo fmt --manifest-path crates/pascal-lsp/Cargo.toml -- --check`:
  passed.
- `git diff --check`: passed.
- The active `.git/hooks/pre-commit` runs format, Clippy, and tests; it is
  enabled and will be exercised by the authorized commit. The broader
  `cargo fmt --all -- --check` form is not used because this environment has
  an unrelated sibling `tree-sitter-pascal` worktree that Cargo reports as a
  workspace-membership metadata error.

The Task 28 report remains untouched by this task, aside from its pre-existing
historical changes in this worktree. No push, merge, amend, or resynchronization
is performed.
