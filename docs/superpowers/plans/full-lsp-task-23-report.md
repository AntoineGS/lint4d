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

## Round-two corrective dispositions (R1, R4, R6, R9)

The four remaining review boundaries were added as tests before the corrective
implementation and were observed failing against `2a7fe5ad`. The focused GREEN
set was rerun after the final implementation.

| Finding | Corrective disposition and boundary evidence |
| --- | --- |
| R1 physical-owner closure | Include-containing rename targets now use the workspace snapshot, and every mapped physical edit is rejected when the same physical span is repeated across lexical include contexts. This closes cross-root/local repeated-include edits without returning a partial workspace edit. Evidence: `rename_rejects_a_cross_root_repeated_include_with_distinct_local_bindings`, plus the existing compatible-consumer and repeated-local regressions. |
| R4 granular conditional/include completeness | Unknown activity on an include directive now remains fail-closed even when the conditional and include directives are adjacent; both compact and line-separated forms are covered. Evidence: `workspace_navigation_fails_closed_for_adjacent_unknown_include_activity`, alongside the source-order DEFINE/UNDEF and unknown-diagnostic regressions. |
| R6 bounded owner discovery | Navigation owner discovery now has a named complete/incomplete outcome, deterministic candidate ordering, an explicit candidate bound, and fail-closed handling for incomplete catalogues/read/context failures. Rename snapshots also reject truncated owner discovery instead of treating the retained prefix as proof. Evidence: `include_navigation_rejects_owner_discovery_at_and_over_its_bound`, `fresh_include_references_recover_a_single_owning_root_context`, and `fresh_include_rename_rejects_bounded_owner_discovery`. |
| R9 aggregate physical-result cap | Reverse-context resolution now uses one bounded discovery/work budget, applies per-context limits, deduplicates mapped physical locations before the final cap, and rejects over-cap results before partial delivery. Position-index caching keeps bounded mapping practical for large results. Evidence: `source_bearing_reverse_contexts_count_deduplicated_physical_results` (exact 10,000 ordinary and partial cases), `source_bearing_reverse_contexts_share_the_10000_reference_cap` (distributed 10,001 ordinary case), and `references_and_highlights_enforce_exact_10000_entry_boundaries`. |

### Round-two evidence hashes

- RED boundary probes:
  - R1 `/tmp/opencode/task23-round2-red-r1.out` — SHA-256 `a76008c3334bf827d4dfaeaf361b709574ca7fcea812256709b3ee64a7c1eddc`
  - R4 `/tmp/opencode/task23-round2-red-r4.out` — SHA-256 `3a0465ea8f173a0b3708733034eeaaaf0b39dd485a3e8891cb62ee1d4bcafb6e`
  - R6 `/tmp/opencode/task23-round2-red-r6.out` — SHA-256 `72af927f6caad14ed72fdbf49f0519c416b7becb964c88012ad0081b0262312a`
  - R9 `/tmp/opencode/task23-round2-red-r9.out` — SHA-256 `5bc62679944623517bd4bb53b9eeabcba903fc14b362af8570f160ddb10e1d01`
- Final focused GREEN set `/tmp/opencode/task23-round2-focused-final.out` — SHA-256 `18909af1dd623b80862b148a01d6761b691cb91f89fb062bc94ae54145cb741f`
- Final verification logs:
  - workspace tests `/tmp/opencode/task23-round2-workspace-tests.out` — SHA-256 `7a159352c3ceb93223e1e14adaf67673fca886dbf7d5bee14767d14b45d9cfd2`
  - all Pascal-LSP targets/features `/tmp/opencode/task23-round2-all-targets-all-features-final.out` — SHA-256 `1d0094b9d1e5071dbd8f75be61ec7047d138930e5bfe54b017319508ef6cec22`
  - Clippy `/tmp/opencode/task23-round2-clippy-final.out` — SHA-256 `fbc8e54c9cca4bf0e4dc2bc96a7795b452ff322862f1a0730a03fe770deede16`
  - workspace check `/tmp/opencode/task23-round2-check-final.out` — SHA-256 `41193aa58493f6d525dc819ae48f24da89d9381a8f9188d4d553835c1649ae05`
  - focused final tests `/tmp/opencode/task23-round2-focused-final.out` — SHA-256 `18909af1dd623b80862b148a01d6761b691cb91f89fb062bc94ae54145cb741f`

No merge, push, Task24 integration, or independent approval was performed.

## Round-three corrective disposition (R9.2)

The follow-up R9.2 reproduction exposed a remaining boundary: one physical
binding queried through three repeated `Uses.inc` inclusions produced 14,998
virtual occurrences but only 5,000 unique physical locations. The per-virtual-
context 10,000-entry limit rejected that valid result before source-map
deduplication.

The navigation layer now exposes work-budgeted virtual binding collection
without applying the public result cap. `BindingWorkBudget` and the existing
cancel-aware source-map budget still bound traversal, resolution, mapping, and
retained vectors. The workspace layer maps every retained virtual location to
an exact physical URI/range, deduplicates across repeated inclusions and
different root contexts, and applies the 10,000 physical-location cap only
after that mapping. Over-limit analysis still returns an error before partial
delivery; under-limit partial results retain their existing chunked behavior.

Boundary coverage now includes:

- `source_bearing_repeated_include_deduplicates_before_reference_cap`: three
  repeated root inclusions, 4,999 virtual uses, and 5,000 unique physical
  results;
- `source_bearing_mixed_roots_and_repeated_includes_deduplicate_physical_results`:
  repeated inclusions across two roots;
- `source_bearing_reverse_contexts_count_deduplicated_physical_results` and
  `references_and_highlights_enforce_exact_10000_entry_boundaries`: exact
  10,000/over-10,000 physical boundaries, including partial-result success and
  over-limit delivery with no chunks;
- `source_bearing_repeated_include_maps_non_bmp_crlf_ranges_once`: exact
  physical UTF-16 ranges through non-BMP CRLF include text; and
- the existing ambiguous-context controls
  `workspace_queries_reject_an_incomplete_non_priority_consumer_context` and
  `workspace_queries_reject_a_missing_optset_in_a_non_priority_consumer_context`.

### Round-three evidence hashes

- RED repeated-include reproduction
  `/tmp/opencode/task23-round3-red-r9-2.out` — SHA-256
  `fca66b1038ed51d9be3c53a14dc839907db7b8bc6d66bccb38341d6465c93a2a`
- RED mixed-root/repeated reproduction
  `/tmp/opencode/task23-round3-red-r9-2-mixed.out` — SHA-256
  `aae80911f001ee7228bb221e0b2b8926a2afa1ed0d394704b7462db990b01962`
- focused source-bearing GREEN set
  `/tmp/opencode/task23-round3-focused-final.out` — SHA-256
  `d7590c76da3ad7314452d7892238f3528d84e335e74370259c26b2ea9d2251bd`
- exact physical-boundary GREEN check
  `/tmp/opencode/task23-round3-green-r9-2-exact-boundaries.out` — SHA-256
  `865e182bed5428a9d23ef70e3e2dc5fb492eb154c5d8604fe74ade1b91e0eb6c`
- response-bound/partial and ambiguity controls
  `/tmp/opencode/task23-round3-green-r9-2-response-bound.out` — SHA-256
  `3e400365239702a0403b0b0817e471409e40874ac358be9eaf1eb12361291997`
  and `/tmp/opencode/task23-round3-green-r9-2-ambiguous-controls.out` — SHA-256
  `53356db590da5302c8e27c5ea518ef5d8c26cdaff3fd3830309c2fb6275f2fb1`
- non-BMP/CRLF mapping check
  `/tmp/opencode/task23-round3-green-r9-2-nonbmp-crlf.out` — SHA-256
  `ca04028f8e1e0922984d5bc34f6f629c665f16b5013c2f709805f25f99b2b8ff`
- workspace check
  `/tmp/opencode/task23-round3-final-check.out` — SHA-256
  `a41051563a4cb5fc1b1bcdd9aa0ba28ca80f7736d629d90cc138218756e8e3a9`
- Clippy
  `/tmp/opencode/task23-round3-final-clippy.out` — SHA-256
  `99f71393b9903dca098310bb446022a4bdee195ea9996f351a787195555210a6`
- workspace tests
  `/tmp/opencode/task23-round3-final-workspace-tests.out` — SHA-256
  `bea6a90ddd6b07362a7d37b851dc017fdf614cdc5b5958c297ec67439b6d102e`
- all Pascal-LSP targets/features, serialized for deterministic protocol
  harness timing
  `/tmp/opencode/task23-round3-final-all-targets-all-features-serial.out` —
  SHA-256
  `2d4990cffba58a9aab772a6234702075a7a13af24619380cf2aa65c66c3dcc34`

The default parallel all-features harness encountered a transient existing
protocol timeout; the isolated test passed, and the complete serialized
all-target/all-feature run passed. No merge, push, Task24 integration, or
independent approval was performed.
