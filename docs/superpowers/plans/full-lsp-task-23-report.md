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

The correction verification below was run fresh after the Task23 review fixes;
all commands exited 0:

- `cargo fmt --manifest-path crates/pascal-lsp/Cargo.toml -- --check`
- `cargo fmt --check`
- `cargo check --workspace --all-targets --locked`
- `cargo clippy --locked -- -W clippy::all -D warnings`
- `cargo test --workspace --locked`
- `cargo test --locked -p pascal-lsp --all-targets --all-features`
- `git diff --check`

The workspace test run passed every target, including the Pascal LSP unit
target (286 tests), navigation (297), protocol (404), and rename (75). The
all-target/all-feature Pascal LSP run passed the feature-enabled unit target
(287), navigation (297), protocol (495), protocol barriers (495), and rename
(75), with no failures.

Complete command output is retained locally in these correction logs:

- `/tmp/opencode/task23-correction-workspace-tests.out` (SHA-256
  `08c8b632433a2f3a69aecba3b63a818bebaeb48e16231bf0ac284a6889fdcaf4`)
- `/tmp/opencode/task23-correction-all-targets-all-features-final-success.out`
  (SHA-256
  `80155b4ade916886d97f4a4c91b1db5f2009076599ba7b1156446593a9fb97ba`)
- `/tmp/opencode/task23-correction-workspace-clippy.out` (SHA-256
  `38ddfd99be694234cb6a2f1da873221e4c38b840d6da865a834e47928c719c05`)
- `/tmp/opencode/task23-correction-workspace-check.out` (SHA-256
  `e449059fb1621fb10a1a61dc6f28f0ee693397c13792b93e15fc9c784735801b`)

## Deliberate limitations

- The expander does not emulate compiler-version, `IFOPT`, environment, or
  Pascal-dependent conditional state. Unknown active include activity withholds
  diagnostics and blocks unsafe navigation/rename guesses.
- Missing, unreadable, cyclic, out-of-root, stale, or incompletely audited
  active includes do not produce partial locations or edits.
- Expansion remains bounded by the configured resource limits; exceeding a
  limit fails closed.

## Correction dispositions

The independent review of `2947477..51ef188` identified R1-R9. The
corrections and their current regression evidence are:

| Finding | Disposition and evidence |
| --- | --- |
| R1 physical include edit closure | Fixed by retaining contextual binding identity and rejecting repeated/conflicting physical owners before edits are returned. Evidence: `rename_rejects_a_repeated_include_with_distinct_local_bindings`, `rename_rejects_a_physical_include_edit_with_conflicting_root_bindings`, `include_declaration_rename_updates_its_root_consumers`, and `public_rename_does_not_silently_omit_include_only_consumers`. |
| R2 navigation freshness provenance | Fixed by retaining include observations, overlay versions, disk stamps, authorization, and content hashes for expansion revalidation. Evidence: `navigation_rejects_a_changed_include_overlay_before_delivery`, `navigation_revalidates_disk_without_watcher_notifications`, and `changed_include_content_invalidates_and_recomputes_root_diagnostics`. |
| R3 expansion and mapping bounds | Fixed with syntactic admission preflight, bounded source/depth/directive/payload work, cancellable `MappingBudget` use across mapping consumers, and a stack-safe default expansion depth. Evidence: `exhausted_admission_budgets_do_not_resolve_or_retain_includes`, `expansion_limits_and_cancellation_fail_closed`, `rename_rejects_excessive_nested_include_depth_without_crashing`, and the full Clippy/check/test runs above. |
| R4 tri-state conditional handoff | Fixed with shared conditional environments and source-order include callbacks; unknown Pascal-bearing activity remains fail-closed while harmless compiler-only activity is not treated as source code. Evidence: `include_define_and_undef_facts_flow_in_source_order`, `workspace_navigation_expands_same_file_conditionals_after_define_and_undef`, `unknown_conditional_source_does_not_publish_confident_lint_diagnostics`, and `rename_rejects_a_reference_in_an_unknown_boolean_comparison_branch`. |
| R5 per-root diagnostic ownership | Fixed by retaining per-root diagnostic contributions and aggregating/deduplicating physical URI publications. Evidence: `shared_include_diagnostics_survive_closing_one_root`, `diagnostics_expand_source_bearing_includes_and_publish_physical_ranges`, and `removing_a_source_bearing_include_clears_its_published_diagnostics`. |
| R6 include owner recovery | Fixed with bounded authorized owner discovery for physical `.inc` documents and conservative rejection of ambiguous ownership. Evidence: `fresh_include_rename_uses_its_single_owning_root_context`, `references_and_highlights_map_source_bearing_include_occurrences`, and `prepare_rename_rejects_an_unresolved_include_before_returning_a_range`. |
| R7 physical document highlights | Fixed by filtering mapped locations to the requested physical URI and asserting exact physical ranges. Evidence: `references_and_highlights_map_source_bearing_include_occurrences` and the Unicode/CRLF navigation regressions. |
| R8 overlay precedence | Fixed by checking ordered overlay/disk candidates at each include search route. Evidence: `source_bearing_include_prefers_a_local_overlay_before_a_later_disk_search_path` and `source_bearing_include_expansion_prefers_an_open_overlay`. |
| R9 aggregate reference cap | Fixed with syntactic preflight, remaining per-context limits, cancellation, bounded mapping work, and one shared 10,000-location cap across reverse contexts. Evidence: `source_bearing_reverse_contexts_share_the_10000_reference_cap`, `references_and_highlights_enforce_exact_10000_entry_boundaries`, and `references_report_the_actual_response_bound`. |

The original RED reproductions remain under `/tmp/opencode` (including the
R1-R9 probe logs named in the independent review); the repository regressions
above are the corresponding GREEN checks. No merge, push, or Task24
integration was performed.
