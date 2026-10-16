# Canonical immutable `ProjectContext` and a complete cache key (l4d-51)

Status: written spec approved 2026-10-14 for implementation planning, not
product implementation.

Issue: `l4d-51` (CORE-8, arch-review, M7). Blocks `l4d-zf1b.2` (phase 2 of the
shared persistent analysis cache), which reuses the canonical context defined
here. Source locations are for `master` at `15c9126`, after the
`pascal-project` module split (`e1c0489`).

## 1. Problem

1. `ProjectOptionsCacheKey` (`crates/pascal-project/src/discovery_cache.rs:90`)
   omits `ProjectOptions::build_selections`, although context construction
   uses them to choose the configuration and platform
   (`assembly.rs:269`). Two discoveries that differ only in the session build
   choice can share one cached context.
2. `ProjectContext` (`context.rs:59`) stores paths twice: bare vectors
   (`search_paths`, `include_paths`, `explicit_units`, `main_source`) and
   provenance entries (`search_path_entries`, `include_path_entries`,
   `explicit_unit_entries`, `main_source_entry`). All fields are public, so the
   two copies can disagree. Readers fall back from entries to bare paths when
   the entries are empty (`context.rs` `include_search_entries`,
   `pascal-core/src/resolver.rs:3919`), and production code edits one copy
   without the other (`pascal-lsp/src/workspace/rename.rs:1225`
   `normalize_standalone_legacy_roots`).
3. `defines: Vec<String>` is a compatibility projection of
   `conditional_context`, built from the `DCC_Define` list plus other True
   facts (`assembly.rs:857`). Project defines never overwrite a caller fact,
   so a caller-False define can appear in `defines` while
   `conditional_context` says False.
4. Import resolution appends `metadata_files`, `metadata_observations` and
   `warnings` to each worker's copy of the context. Equality therefore needs
   special comparators that ignore them (`ProjectContext::same_analysis_inputs`,
   `rename.rs` `evaluated_contexts_equal`), and any future identity or digest
   would have to keep excluding them by hand.

## 2. Goals

- One stored, provenance-bearing representation; bare views derived by
  accessors.
- `ProjectContext` immutable after construction: no public fields, no
  `&mut self` mutators, shared as `Arc<ProjectContext>`.
- Observations accumulated during resolution held outside the context.
- `ProjectContext` equality means "same analysis inputs".
- The context build cache key includes every session selection that
  construction consumes, looked up by the same code construction uses.

## 3. Non-goals

- Persistent encodings, strong digests or BLAKE3 (phase 2, `l4d-zf1b.2`).
- Typed completeness instead of warning strings (`l4d-39`).
- Reconciling `config`/`platform` with `config_selection.selected` /
  `platform_selection.selected`. They differ in spelling (candidate case,
  canonical platform names); they only become private with accessors.
- Changes to cache eviction or to the other key fields.

## 4. Data model and API

### 4.1 `ProjectContext`

- No public fields. Fields are `pub(crate)`: construction is split across
  sibling modules of `pascal-project` (`assembly.rs`, `selection.rs`,
  `discovery_cache.rs`), which Rust privacy would otherwise exclude. Other
  crates read through `&self` accessors with the current field names.
- Stored path data: `search_path_entries`, `include_path_entries`,
  `explicit_unit_entries`, `main_source_entry`, plus the existing
  provenance-only lists (`browsing_path_entries`, `debug_dcu_path_entries`).
  The bare fields `search_paths`, `include_paths`, `explicit_units`,
  `main_source` and `defines` are removed from storage.
- Derived accessors:
  - `search_paths()` and `include_paths()` map the paths of the matching
    entry lists, in order.
  - `explicit_units()` maps `explicit_unit_entries()` to paths per unit name.
  - `main_source()` is the path of `main_source_entry()`.
  - `defines()` returns the names whose fact in `conditional_context()` is
    `ConditionalFact::True`, in the context's sorted key order.
- The empty-entries fallbacks are removed from `include_search_entries` and
  from the resolver (`resolver.rs:3919`).
- `metadata_files`, `metadata_observations`, `warnings` and
  `system_pas_searches` move to `ProjectObservations` (4.2).
  `system_pas_searches` is included because the LSP already treats it as an
  observation: it merges retained searches into a live context
  (`pascal-lsp/src/workspace.rs:2325`) and only uses them to revalidate stamps
  (`workspace.rs:14885`); the RTL facts they produced stay in
  `conditional_context` and `conditional_closure`. Every other current field,
  including `path_issues` and `installation_config_files`, stays in the
  context.
- Derived `PartialEq`/`Eq` replaces `same_analysis_inputs`; its callers in
  `pascal-lsp/src/project_cache.rs` use `==`.
- Accessors return borrowed data (`&[ProjectPathEntry]`, `Option<&Path>`)
  where it is stored, and owned or iterator values where it is derived.

### 4.2 `ProjectObservations`

A new public, owned, mutable value in `pascal-project`:

- Holds `metadata_files: Vec<PathBuf>`, `metadata_observations:
  Vec<MetadataObservation>`, `warnings: Vec<String>` and
  `system_pas_searches: Vec<SystemPasSearchObservation>`.
- These are public fields: the value is meant to be mutated by its owner.
  The existing writers keep their deduplication logic and only change the
  target, for example `pascal-lsp/src/workspace/resolver.rs:1259-1295`
  (`merge_report_into_context`), `pascal-lsp/src/workspace.rs:2294-2335`
  (`merge_observations`) and `rename.rs:4570`. Readers used for revalidation
  and budget charging (`workspace.rs`, `rename.rs`, `resolver.rs:2646-2652`)
  read the same fields from the observation record.
- `visit_recovery_payload` moves the observation part of
  `ProjectContext::visit_recovery_payload` here; callers that account for a
  context's recovery payload visit both.
- It carries no analysis inputs and plays no part in context equality.
- `pascal_core::UnitResolver` today seeds its resolution report from the
  context's metadata observations and warnings (`resolver.rs` `new_inner`).
  It gains `with_context_observations(self, &ProjectObservations) -> Self`;
  production callers that hold observations call it, so reports keep the same
  observations as before.
- `ProjectDiscovery` (`reads.rs:245`) gains
  `context_observations: ProjectObservations`, carrying the construction
  observations alongside `context`.
- `has_invalid_project_selection` (`selection.rs`) reads warnings, so it takes
  `&ProjectObservations` instead of `&ProjectContext`.
- The LSP `ContextState` holds `context_observations` beside `context`. Its
  `PartialEq` compares them too, so state equality is unchanged.
  `rename.rs` `project_contexts_match_for_explicit_in_path`, which filters
  and compares metadata files, compares the observation records it needs.
  Comparisons that guard reuse of parsed documents or resolver session
  caches (`queries.rs`, `rename.rs`, `workspace.rs`, `resolver.rs`
  `ResolverSessionCacheScope`) use context `==`, the meaning
  `project_cache.rs` already gives them through `same_analysis_inputs`.

### 4.3 `DiscoveredProject`

`ProjectContext::discover` and `discover_with_overrides` return
`DiscoveredProject { context: Arc<ProjectContext>, observations:
ProjectObservations }`. The observations start with what construction read,
which is what `CachedProjectContext` (`discovery_cache.rs`) already keeps
separately. Holders that need both, such as the LSP workspace state and the
resolver, hold both. About 155 call sites, mostly tests, take `.context` or
destructure.

### 4.4 `ProjectContextParts`

- A plain struct with public fields and `Default`, holding exactly the
  canonical stored fields of 4.1: no bare path duplicates, no `defines` list,
  no observations.
- `ProjectContext::from_parts(parts) -> ProjectContext` moves the fields
  unchanged; it does not normalize paths, because tests that build literals
  today rely on exact values. Both functions destructure exhaustively, so a
  new field must be added to both types. `into_parts(self)` returns the parts
  for a modified copy, for example a reset `conditional_context`.
- `from_parts`, `into_parts` and the parts type exist only under
  `#[cfg(any(test, feature = "test-support"))]`, where they are public.
  Production construction inside `pascal-project` uses struct literals on the
  `pub(crate)` fields, so production code in other crates can neither
  construct nor edit a context.
- `ProjectPathEntry::configured(path)` builds a `Configured` entry for test
  literals that set bare `search_paths` today. Tests that relied on the
  read-policy fallback state their provenance explicitly. It arrives in step
  3, when the bare fields disappear.
- Until step 5, `DiscoveredProject::context` is a plain `ProjectContext`.

## 5. Cache key and selection lookup

- New `pub(crate)` helper on `ProjectOptions`:
  `session_selections_for(&self, project_file: &Path) -> SessionSelections`,
  where `SessionSelections { installation: Option<String>, build:
  Option<BuildChoice> }` derives `Clone, Debug, PartialEq, Eq, Hash`.
- Lookup matches keys with `project_paths_equal` and is deterministic: an
  exact path match wins; otherwise the first match in sorted path order. This
  replaces `HashMap::iter().find(...)` at `assembly.rs:269` (build) and
  `assembly.rs:352` (installation), whose result depends on iteration order
  when two keys match one path.
- `ProjectOptionsCacheKey` is built from `(options, project_file)`. Its sorted
  `installation_selections` vector is replaced by
  `selections: SessionSelections` for that project. The only user is
  `ProjectContextBuildCacheKey` (`selection.rs:801`), which already knows the
  project file.
- The key stays structural and compares with full `Eq`. No `u64` digest is
  introduced.
- Effect: a change to the session build choice of this project misses the
  cache; a selection change for another project no longer evicts this one.

## 6. Comparisons that stay special

`rename.rs` `evaluated_contexts_equal` also drops `LegacyNative` search roots
for contexts without a project file before comparing. That is intended
rename behavior, not an artifact of duplicated storage. It remains a named
comparator in `rename.rs`; only its observation clearing is removed. Because
other crates cannot build a modified context, the legacy-root filtering moves
into `ProjectContext::eq_ignoring_legacy_search_roots`, which
`evaluated_contexts_equal` calls when neither context has a project file.

## 7. Intended behavior changes

- `defines()` order is sorted rather than `DCC_Define` order followed by other
  True facts, and names use the normalized spelling stored in
  `conditional_context` (trimmed, leading `&` removed, upper case, as in
  `assembly.rs` `merge_project_conditional_context`). Standalone contexts
  already derive `defines` this way (`assembly.rs:1080`).
- `effective_conditional_context()` no longer merges the bare `defines` list
  into the facts; for discovered contexts the result is unchanged because
  every project define is already a fact.
- A define that the caller sets False no longer appears in `defines()`.
- The in-process `compiled_dcu` context fingerprint
  (`pascal-lsp/src/navigation/compiled_dcu.rs:340`) changes once because it
  hashes `defines`. It is not persisted.
- Session build selections are part of the context cache key (5).
- Parsed-document and resolver-session-cache reuse guarded by context `==`
  no longer misses when only observations differ (4.2).
- Recovery-payload accounting no longer visits the removed bare duplicates,
  and resolver context admission (`resolver.rs` `conditional_context_reserve`
  callers) counts the derived `defines()`; retained-byte estimates shrink
  slightly.

Any other observable change is a regression.

## 8. Sequencing

Each step builds, passes its tests, and is committed separately naming
`l4d-51`.

1. Cache key. Tests 9.1 and 9.2 first (red), then the helper, both
   `assembly.rs` call sites and the key.
2. Observations. Introduce `ProjectObservations` and `DiscoveredProject`;
   move observation writers in `pascal-project`, `pascal-core` and
   `pascal-lsp`; replace `same_analysis_inputs` with `==`.
3. Single representation. Tests 9.4 and 9.5 first; then entries-only storage,
   derived accessors and `defines()`, and removal of the fallbacks.
4. Encapsulation. Make fields `pub(crate)`; add `ProjectContextParts`,
   `from_parts`, `into_parts` and `ProjectPathEntry::configured`; migrate test
   literals (about 100) and test field writes (about 85); add test 9.6.
5. Sharing. Hold contexts as `Arc<ProjectContext>` in `DiscoveredProject`,
   `ProjectDiscovery`, `UnitResolver` and LSP context state.

## 9. Tests

Written before the change they cover; red runs are recorded.

1. Two discoveries of one `.dproj` through the discovery cache, differing
   only in `build_selections` (Debug, then Release): the second context's
   `config()` is Release. Fails today.
2. Changing project B's build or installation selection does not rebuild
   project A's context (`project_context_cache_hits` increases).
3. Guard: a session build choice still overrides the configured choice.
4. `defines()` excludes a define the caller sets False while the project
   declares it, and lists the True facts in sorted order.
5. A context built from parts has bare views equal to the paths of its
   entries; no constructible context has entries and bare paths that
   disagree.
6. A `compile_fail` doctest: code outside `pascal-project` cannot assign a
   `ProjectContext` field.
7. Existing coverage of `same_analysis_inputs` (`context.rs:600`) is kept as
   an equality test that adding observations does not change `==`.

Verification: `cargo test --workspace --no-fail-fast`; `cargo test -p
pascal-lsp --features test-support --test protocol_barriers --no-fail-fast`;
`cargo fmt --check`; `cargo clippy --workspace --all-targets -- -D warnings`.
Failures already present at the base commit (the two `pascal-core` `fix_i3_*`
tests, `l4d-1jfh`/`l4d-l9iv`, `l4d-6qe5`) are recorded before step 1 and are
not counted as regressions.

## 10. Acceptance mapping

| `l4d-51` criterion | Evidence |
| --- | --- |
| Contexts differing only in `build_selections` have different cache keys | Test 9.1 |
| No public mutable field on `ProjectContext` | Step 4, test 9.6 |
| Tests first, failing on the original code | Red runs of 9.1, 9.2, 9.4, 9.5 recorded in Beads |

## 11. Risks

- Wide, mostly mechanical diff in step 4 (about 17 files, mostly tests).
- `rename.rs` and `workspace.rs` are also touched by the in-progress
  `l4d-l9iv`. Check for overlap before steps 2 and 4.
- `Arc` sharing changes ownership in LSP state. Places that currently mutate a
  cloned context must hold their own `ProjectObservations` instead; step 2
  handles every writer listed in 4.2.

## 12. Handoff to phase 2

`l4d-zf1b.2` receives a context with one stored representation, no
observations and equality by analysis inputs. Classifying fields into content,
conditional, expansion and binding identity, plus versioned encodings and
strong digests, stays in phase 2.
