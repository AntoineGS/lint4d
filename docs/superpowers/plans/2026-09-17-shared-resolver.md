# Shared Pascal Resolver Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move reusable Pascal unit/package/include resolution into `pascal-core`, keep discovery metadata in `pascal-project`, and let lint4d build safe `cfg_pascal::ProjectSnapshot` values for the shared lint runner.

**Architecture:** `pascal-project::ProjectContext` remains the immutable metadata and authorization input. A request-scoped generic resolver in `pascal-core` owns precedence, candidate validation, package/include lookup, conservative conditional analysis, limits, cancellation, and observations; LSP and CLI adapters supply overlays or disk sources. lint4d converts the resulting graph to the existing caller-owned CFG snapshot, while `pascal-lsp` retains workspace state, navigation indexes, snapshot modes, and stale-result handling.

**Tech Stack:** Rust 2024/1.85 in the lint4d workspace, Rust 2021 in cfg-pascal, tree-sitter 0.24, `pascal-project` metadata/read policy, `pascal-core`, `cfg-pascal`, Rayon CLI workers, LSP overlays, and existing Cargo integration tests.

**Spec:** `docs/superpowers/specs/2026-09-17-shared-resolver-design.md`

## Global Constraints

- Execution override: do not commit or push. Keep changes in the two isolated worktrees. Proposed signatures may be refined against actual APIs; preserve the architectural and behavioral contracts. Prefer focused modules over a monolithic resolver. The existing conditional semantics take precedence over illustrative test assumptions below.
- Dependency correction: use published `cfg-pascal` revision `208d6743c61e0b391195958270a5e04e3a4328d4` (verified on origin/master), not an unpublished feature branch. Use ignored local Cargo overrides only if new CFG APIs become necessary, and document that requirement.

- `cfg-pascal` remains free of filesystem, environment, MSBuild, LSP, and project-index dependencies.
- `pascal-project` remains responsible for project/package discovery metadata, path provenance, read policy, and metadata observations; it does not expand includes or scan all Pascal sources.
- Resolver status must distinguish `Found`, `Unavailable`, `Ambiguous`, and `Incomplete`; only `Found` may create a precise import/include binding.
- LSP overlays win over disk for the same canonical path; overlay version and content hash remain observable.
- Unit precedence is explicit mappings, importer directory, ordered search paths, projectless filename catalogues, then named packages; an ambiguity at a winning tier blocks lower tiers.
- Package descriptors remain separate from unit search paths; `.dpk` wins over `.dproj` for the same package stem.
- Configured and mapped payload reads are regular-file and symlink-free; the legacy-native route remains narrow and caller-proved.
- `MetadataObservation::Stat` never authorizes a later payload read.
- Resolver defaults retain the existing LSP safety values: 256 dependency units, 1,048,576 directory entries, 8 GiB scanned bytes, 16 MiB source bytes, 4,096 include files, 256 MiB include bytes, 16,384 include directives, 256 include depth, 256 package lookups, 1,024 package candidates, 524,288 package catalogue entries, 64 package catalogues, and 256 warnings.
- Conditional/include failures, missing or cyclic includes, incomplete metadata, exceeded limits, cancellation, and stale observations fail closed.
- `cfg-pascal` pins `cfg-core` to `b4131e61a939e0685194712689d1fd1497a41492`; no tracked absolute path is permitted in either repository.
- Existing public CFG APIs and lint4d wrappers remain available; the file-local path remains the default.
- Future implementation must preserve the baselines: `cfg-pascal` has 212 passing tests; `lint4d cargo test --workspace` has 1,909 passing tests and 5 ignored tests.

---

## File map

### Task 1: shared contracts and resolver

- Modify: `/home/antoinegs/gits/cfg-pascal/.worktrees/shared-resolver/Cargo.toml` — pin `cfg-core` by repository URL and revision.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-project/src/lib.rs` — expose path-entry/include-search helpers without moving discovery ownership.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-project/tests/public_api.rs` — test the new metadata helpers and authorization invariants.
- Create: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-core/src/conditional.rs` — shared conservative conditional analysis moved from LSP.
- Create: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-core/src/resolver.rs` — source-store contract, lookup policy, package/include resolution, reports, and limits.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-core/src/lib.rs` — export the new resolver and conditional APIs.
- Create: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-core/tests/unit_resolver.rs` — resolver behavior matrix.

### Task 2: LSP migration

- Create: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-lsp/src/workspace/resolver.rs` — overlay-aware `SourceStore` bridge and observation conversion.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-lsp/src/lib.rs` — stop declaring the private duplicate conditional module.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-lsp/src/workspace.rs` — use the shared resolver for unit/import lookup and remove duplicate unit/package catalogue policy.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-lsp/src/workspace/rename.rs` — use shared include/conditional resolution while retaining LSP snapshot and revalidation orchestration.
- Delete: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-lsp/src/conditional.rs` — no second conditional implementation.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-lsp/src/navigation.rs` — retain index binding semantics while consuming shared import results where its internal helper needs an import status.
- Modify: existing LSP unit tests in `workspace.rs`, `workspace/rename.rs`, and navigation modules — assert parity and fail-closed behavior.

### Task 3: CFG adapter and shared lint runner

- Create: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/lint4d/src/cfg/project_snapshot.rs` — resolver graph to `cfg_pascal::ProjectSnapshot` adapter.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/lint4d/src/cfg/mod.rs` — export the adapter.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/lint4d/src/engine/mod.rs` — add the snapshot-aware runner and preserve existing wrappers.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/lint4d/src/main.rs` — construct project metadata/resolver input for CLI project runs while preserving direct disk discovery.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/lint4d/Cargo.toml` — pin `cfg-core` and consume the shared-resolver `cfg-pascal` through its repository URL, not a local path.
- Regenerate: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/Cargo.lock` — let Cargo record the selected git revisions.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/pascal-lsp/src/workspace.rs` — call the shared lint runner for LSP diagnostics.
- Create: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/lint4d/tests/cfg_project_snapshot_test.rs` — adapter, statuses, IDs, qualifiers, raw fallback, and maps.
- Modify: `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver/crates/lint4d/tests/engine_run_test.rs` — wrapper parity tests.

---

## Task 1: Shared contracts and resolver

**Files:** Use the Task 1 file map above. The implementation spans both
worktrees because cfg-pascal supplies the dependency pin while lint4d houses
the reusable core crates.

**Interfaces:**

- Consumes: `pascal_project::ProjectContext`, `ProjectPathEntry`, `ReadPolicy`, `ProjectReadStamp`, `PackageMetadataRead`, `EffectiveOverrides`, ordered workspace roots, a `SourceStore`, and a `CancellationToken`.
- Produces: `pascal_core::UnitResolver<S>`, `ResolutionOutcome<T>`, `ResolvedProject`, `ResolvedImport`, `ResolvedInclude`, `ResolutionReport`, and `ConditionalAnalysis` with the signatures below.

```rust
pub trait CancellationToken {
    fn is_cancelled(&self) -> bool;
}

impl CancellationToken for std::sync::atomic::AtomicBool {
    fn is_cancelled(&self) -> bool {
        self.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoCancellation;

impl CancellationToken for NoCancellation {
    fn is_cancelled(&self) -> bool { false }
}

pub trait SourceStore {
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError>;

    fn overlay_candidates(
        &self,
        roots: &[std::path::PathBuf],
        names: &[String],
    ) -> Vec<std::path::PathBuf>;

    fn load(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError>;
}

pub struct DirectoryRequest<'a> {
    pub directory: &'a std::path::Path,
    pub entry: &'a pascal_project::ProjectPathEntry,
    pub read_policy: &'a pascal_project::ReadPolicy,
}

pub struct SourceRequest<'a> {
    pub path: &'a std::path::Path,
    pub entry: &'a pascal_project::ProjectPathEntry,
    pub read_policy: &'a pascal_project::ReadPolicy,
    pub legacy_route: Option<&'a LegacyRoute>,
    pub kind: SourceKind,
    pub max_bytes: usize,
}

pub enum SourceKind {
    Unit,
    Include,
    PackageDescriptor,
}

pub struct UnitResolver<S> {
    // Private request-local catalogues, caches, and observations.
    store: S,
}

impl<S: SourceStore> UnitResolver<S> {
    pub fn new(
        context: pascal_project::ProjectContext,
        workspace_roots: Vec<std::path::PathBuf>,
        store: S,
        limits: ResolverLimits,
    ) -> Self;

    pub fn resolve_unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> ResolutionOutcome<ResolvedUnit>;

    pub fn resolve_imports(
        &mut self,
        importer: &ResolvedUnit,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedImports, ResolverError>;

    pub fn resolve_include(
        &mut self,
        request: IncludeResolveRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> ResolutionOutcome<ResolvedSource>;

    pub fn resolve_project(
        &mut self,
        root: UnitResolveRequest<'_>,
        sites: &[ImportSite],
        cancel: &dyn CancellationToken,
    ) -> Result<ResolvedProject, ResolverError>;

    pub fn finish(self) -> ResolutionReport;
}

```

Use the result fields from the specification without local shortcuts:
`ResolvedImport` carries `importer_source_id`, `ResolvedInclude` carries
`including_source_id`, and `ResolvedProject` keeps `root` separate from
dependency `units` plus its de-duplicated `include_sources`. Built-in stores
must return the same `source:<canonical-path>` ID for disk and overlay content
at one path, regardless of revision or candidate order.
Canonicalization is absolute, lexical, and platform-normalized without
following symlinks, so an overlay-only path receives the same identity it will
have when a disk file later appears.

- [ ] **Step 1: Pin cfg-pascal's cfg-core dependency without a local path.**

Modify `/home/antoinegs/gits/cfg-pascal/.worktrees/shared-resolver/Cargo.toml`
so the dependency is exactly:

```toml
cfg-core = {
    git = "https://github.com/AntoineGS/cfg-core.git",
    rev = "b4131e61a939e0685194712689d1fd1497a41492",
}
```

Run:

```bash
cargo metadata --format-version 1 --no-deps
```

from `/home/antoinegs/gits/cfg-pascal/.worktrees/shared-resolver` and verify
that the manifest contains no `/home/`, `C:\`, or `file://` dependency path.

- [ ] **Step 2: Add the project metadata helpers before changing consumers.**

In `pascal-project/src/lib.rs`, add these methods to `ProjectContext`:

```rust
pub fn path_entry_for(&self, path: &Path) -> Option<ProjectPathEntry>;

pub fn include_search_entries(&self, owner: &Path) -> Vec<ProjectPathEntry>;

pub fn selected_project_options(&self) -> ProjectOptions;
```

Implement `path_entry_for` with this order: exact `main_source_entry` or
explicit-unit entry, most-specific `search_path_entries` entry, then
`read_policy.entry_for_path(path)`. Implement `include_search_entries` as
owner directory, include entries, then unit search entries, removing
path-equivalent duplicates without changing first occurrence. Implement
`selected_project_options` by copying the context's selected `config` and
`platform` into the `build_config` and `platform` fields of a default
`ProjectOptions`.

Add tests to `pascal-project/tests/public_api.rs` for exact mapped provenance,
most-specific roots, owner-directory-first include ordering, duplicate removal,
and the fact that a stat-only metadata observation is not accepted as a
payload authorization.

Run:

```bash
cargo test -p pascal-project --test public_api
```

from `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver`.

- [ ] **Step 3: Write failing shared conditional-analysis tests.**

Create tests in `pascal-core/tests/unit_resolver.rs` for the moved analyzer:

```rust
#[test]
fn unknown_project_define_keeps_include_potentially_active() {
    let analysis = pascal_core::conditional::analyze_with_cancel(
        "{$IFDEF FEATURE}{$I body.inc}{$ENDIF}",
        &[],
        &pascal_core::NoCancellation,
    );
    assert!(analysis.complete);
    assert_eq!(analysis.directives.len(), 3);
    assert!(analysis.directives.iter().any(|directive| {
        directive.kind == pascal_core::conditional::DirectiveKind::Include
            && directive.activity == pascal_core::conditional::Truth::Unknown
    }));
}

#[test]
fn inactive_include_is_not_potentially_active() {
    let analysis = pascal_core::conditional::analyze_with_cancel(
        "{$IFDEF FEATURE}{$I body.inc}{$ENDIF}",
        &["NOT_FEATURE".to_string()],
        &pascal_core::NoCancellation,
    );
    assert!(analysis.complete);
    assert!(analysis.directives.iter().any(|directive| {
        directive.kind == pascal_core::conditional::DirectiveKind::Include
            && !directive.potentially_active()
    }));
}
```

Run:

```bash
cargo test -p pascal-core --test unit_resolver unknown_project_define_keeps_include_potentially_active
```

Expected: FAIL until `pascal-core::conditional` exists and exposes the moved
analysis contract.

- [ ] **Step 4: Move the conservative conditional module into pascal-core.**

Create `pascal-core/src/conditional.rs` from the current LSP implementation,
preserving `Truth`, `DirectiveKind`, `ConditionalDirective`,
`ConditionalAnalysis`, inactive/unknown spans, offset-preserving projected
bytes, and the existing limits. Change the cancellation parameter to
`&dyn CancellationToken`; cancellation, malformed structure, or a budget
failure must set `ConditionalAnalysis::complete` to `false` and clear precision
spans. A structurally complete walk may still have unknown activity, so the
resolver must inspect `Truth::Unknown` separately. Export the module and the
public types needed by the resolver from `pascal-core/src/lib.rs`.

Run the focused test from Step 3 and expect PASS.

- [ ] **Step 5: Write failing unit-resolution tests for precedence and names.**

Add a memory-backed `SourceStore` test fixture and these cases to
`pascal-core/tests/unit_resolver.rs`:

```rust
#[test]
fn explicit_mapping_wins_and_alias_is_applied_once() {
    let context = fixture_context_with_alias("compat", "Vendor.Errors");
    let mut resolver = fixture_resolver(context);
    let result = resolver.resolve_unit(
        UnitResolveRequest {
            requested_name: "Compat",
            importer_path: Path::new("/workspace/App.pas"),
            legacy_route: None,
        },
        &NoCancellation,
    );
    let found = match result.result {
        Resolution::Found(unit) => unit,
        other => panic!("expected explicit alias target, got {other:?}"),
    };
    assert_eq!(found.declared_name, "Vendor.Errors");
}

```

The fixture must expose `fixture_context_with_alias`, `fixture_resolver`, and
an in-memory `SourceStore` that returns deterministic `SourceId` and
`SourceRevision::Overlay` values. Add these concrete cases alongside the
complete alias test above:

- `ambiguity_at_current_tier_blocks_search_path_fallback`: place two valid
  files in the importer directory and one valid file in a later search path;
  assert `Resolution::Ambiguous` and assert the later path was never loaded.
- `namespace_candidates_are_tried_only_for_unqualified_names`: configure two
  namespaces and assert their candidate load order for an unqualified request;
  repeat with a dotted request and assert no namespace-expanded candidate is
  loaded.
- `declared_name_mismatch_is_not_a_unit_match`: place a basename-matching file
  whose declaration names another unit and assert `Unavailable` rather than a
  guessed binding.

Run:

```bash
cargo test -p pascal-core --test unit_resolver ambiguity_at_current_tier_blocks_search_path_fallback
```

Expected: FAIL until the resolver implementation is present.

- [ ] **Step 6: Implement source-store types and bounded resolver state.**

In `pascal-core/src/resolver.rs`, define `SourceId`, `SourceKind`,
`DirectoryRequest`, `SourceRequest`, `DirectoryListing`, `LoadedSource`,
`SourceRevision`, `SourceStoreError`, `ResolverLimits`, `Resolution`,
`ResolutionTarget`, `ResolutionOutcome`, `ResolutionCandidate`,
`ResolutionObservation`, and `ResolutionReport` with the names and fields in
the design specification. Both request types carry the exact
`ReadPolicy`/`ProjectPathEntry` pair used to authorize the operation.
Preserve both `MetadataObservation` values and `ProjectReadObservation` values
from package/project metadata in the report; do not reduce stat-only facts to
payload authorization.

Implement `FilesystemSourceStore` so it:

1. lists immediate children without following symlinks and returns a stamped
   incomplete listing on traversal failure, the directory-entry limit, or the
   aggregate scanned-byte limit;
2. returns overlay candidates from its explicit overlay table, never from a
   global index;
3. validates `ReadPolicy`/`ProjectPathEntry` before reading;
4. reads at most `max_bytes + 1` bytes;
5. uses the strict payload method for configured/mapped paths and the legacy
   payload method only for an authorized legacy entry; and
6. returns the deterministic `source:<canonical-path>` ID and a content hash
   plus disk stamp or overlay version in every loaded source; and
7. distinguishes a complete `NotFound` from unauthorized, non-regular,
   oversized, I/O, and incomplete failures so the resolver can select
   `Unavailable` versus `Incomplete` without guessing, and maps store
   cancellation to `ResolverError::Cancelled`.

Run:

```bash
cargo test -p pascal-core --test unit_resolver
```

Expected: the source-store and conditional tests pass; unit lookup tests may
still fail until Step 7 is complete.

- [ ] **Step 7: Implement unit candidate ordering and declared-name validation.**

Implement `UnitResolver::resolve_unit` using the exact sequence from the spec:
explicit mapping, importer directory, ordered search paths, projectless
catalogue, then packages. For each directory, create the tiers
`<requested>.pas`/short spelling followed by configured namespace spellings.
Merge overlay candidates into the corresponding tier before loading, so an
overlay can supply a candidate absent from disk. Sort paths deterministically,
remove path-equivalent duplicates, load each candidate through `SourceStore`,
parse its declaration with the shared parser, and accept only an exact
requested/alias/authorized namespace match. A context with
`discovery_complete == false` is incomplete before it can produce a unique
project result.

Return `Ambiguous` immediately when two valid candidates occur in the first
non-empty tier. Return `Incomplete` when a relevant listing, load, or limit
cannot prove the candidate set. Return `Unavailable` only after a complete
search has no valid target. Record all directory/candidate observations and
bounded warnings.

Run:

```bash
cargo test -p pascal-core --test unit_resolver
```

Expected: all alias, namespace, precedence, ambiguity, and declared-name tests
pass.

- [ ] **Step 8: Implement package and include lookup.**

Implement package lookup with these concrete rules:

- scan only workspace roots and effective mapped roots;
- cap catalogue entries at 524,288 and package catalogues at 64;
- select `.dpk` descriptors over `.dproj` descriptors for one stem;
- treat multiple descriptor paths as ambiguous;
- call `read_package_metadata_with_observations` using
  `selected_project_options`, `context.overrides`, `context.read_policy`, and
  the exact descriptor entry;
- preserve package metadata files and payload observations; and
- keep a missing compiled-only package as a warning plus `Unavailable`, not a
  source candidate.

Implement include lookup with owner directory, include paths, and unit paths;
apply `EffectiveOverrides`, case-insensitive component lookup after an exact
miss, legacy-route restrictions, include file/byte/directive/depth limits,
and an `active` set for cycles. Call shared conditional analysis before
resolving a potentially active include and return `Incomplete` for unknown or
malformed conditional state. Record the including source ID and exact
directive range in every `ResolvedInclude`, and retain each successful
non-unit payload in `ResolvedProject::include_sources` so the result remains
self-contained after the resolver session ends.

Add tests for `.dpk` preference, ambiguous descriptors, incomplete catalogue,
mapped include authorization, case-adjusted include path, missing/cyclic
include, inactive include, overlay precedence, and every include limit.

Run:

```bash
cargo test -p pascal-core --test unit_resolver package
cargo test -p pascal-core --test unit_resolver include
```

Expected: PASS with deterministic observations and no unauthorized payload
read.

- [ ] **Step 9: Implement import/project graph assembly and status mapping.**

Implement `resolve_imports` so it returns one `ResolvedImport` for every input
site, carries the importing source ID, keeps site order, forwards exact
authorized qualifier spellings, and returns unique loaded dependencies without
duplicate source IDs. Implement `resolve_project` as a bounded breadth-first
closure over the root's supplied sites and each dependency's parsed sites,
parse include occurrences from each loaded source, and return the root
separately from dependency units; check the cancellation token before every
queue pop and candidate load.

Finalize the embedded `ResolvedProject::report` at the end of that walk. Use
`finish` only for sessions that use the individual resolve methods, and do not
replace a project report with a later report containing unrelated operations.

Use `ResolutionTarget::Unavailable` for no result, `Ambiguous` for multiple
valid candidates, and `Incomplete` for an incomplete search. A failed
dependency does not become a guessed basename binding. `finish` must mark the
report incomplete whenever any operation returned `Incomplete` or a warning
indicates an unsafe omitted candidate.

Add tests for stable dependency order, duplicate import sites, section-aware
site retention, qualifier forwarding, cancellation, and report observation
deduplication.

Run:

```bash
cargo test -p pascal-core --test unit_resolver
cargo test -p pascal-project
```

- [ ] **Step 10: Verify Task 1 across both repositories and record future implementation commits.**

Run the repository baselines before handing Task 1 to the next task:

```bash
cargo test
```

from `/home/antoinegs/gits/cfg-pascal/.worktrees/shared-resolver`, then:

```bash
cargo test --workspace
```

from `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver`.

At implementation time, commit the cfg-pascal manifest pin in its repository
and the pascal-project/pascal-core resolver in the lint4d repository. This
planning task itself creates no commits.

---

## Task 2: Migrate pascal-lsp to the shared resolver

**Files:** Use the Task 2 file map above. Keep `WorkspaceInput`, `Computed`,
`RenameSnapshot`, `SnapshotMode`, `SourceRecord`, generations, and revalidation
in `pascal-lsp`.

**Interfaces:**

- Consumes: `ProjectContext`, `WorkspaceInput`, `OverlayInput`, `NavigationIndex::imports`, `pascal_core::UnitResolver`, and `pascal_core::ConditionalAnalysis`.
- Produces: `LspSourceStore`, shared resolver reports merged into existing `SourceRecord`/baseline data, and unchanged LSP navigation/rename/code-action results.

```rust
pub(crate) struct LspSourceStore {
    pub(crate) overlays: HashMap<PathBuf, OverlayInput>,
    pub(crate) paths_to_uris: HashMap<PathBuf, Url>,
}

pub(crate) fn resolver_for_context(
    context: ProjectContext,
    roots: Vec<PathBuf>,
    input: &WorkspaceInput,
    cancel: &AtomicBool,
) -> UnitResolver<LspSourceStore>;
```

- [ ] **Step 1: Add the LSP source-store bridge before replacing lookup code.**

Create `workspace/resolver.rs`. Canonicalize every overlay URL to a path and
retain its text/version. Implement `SourceStore::overlay_candidates` by
returning only overlays inside the supplied authorized roots whose filenames
match the requested candidate names. Implement `SourceStore::load` by:

1. returning the overlay for the canonical path before checking disk;
2. returning `SourceRevision::Overlay { version, content_hash }` for that
   overlay;
3. looking up `ProjectPathEntry` through `ProjectContext::path_entry_for` for
   disk candidates;
4. preserving the current legacy-route proof when the requesting document is
   allowed to use it; and
5. passing the exact `ReadPolicy`/`ProjectPathEntry` pair to both directory
   listing and payload loading; and
6. delegating all disk bytes to `FilesystemSourceStore`/`ReadPolicy`.

Add a conversion function from `LoadedSource` to the existing `SourceRecord`
that retains URI, text, version/stamp, content hash, `ReadPolicy`, path entry,
and include-payload status.

Run the existing LSP unit tests that exercise open overlays and disk loading:

```bash
cargo test -p pascal-lsp workspace::tests
```

If the test target name differs in Cargo output, run the complete package
instead and inspect only the existing workspace tests; no behavior change is
expected at this step.

- [ ] **Step 2: Replace unit lookup without changing NavigationIndex behavior.**

In `Workspace::load_imports_with_cancel`, convert each
`ImportMetadata { name, span }` to an `ImportSite` and create one resolver
session for the batch. Pass the current source path and existing legacy-route
proof in `UnitResolveRequest`.

For each `ResolvedImport`:

- bind `Found(source_id)` to the URL supplied by the `LspSourceStore`;
- bind an explicit empty map for `Unavailable`, `Ambiguous`, and `Incomplete`;
- add only unique found URLs to the existing dependency frontier; and
- merge warnings and observations into workspace state.

Keep the current `NavigationIndex` rule: once a document is workspace-owned,
an unresolved import must not fall back to a same-named unit from another
project. Keep the 256 dependency-work cap in the existing closure walker.

Run:

```bash
cargo test -p pascal-lsp navigation
cargo test -p pascal-lsp workspace
```

Expected: navigation resolution, project precedence, alias, namespace, and
ambiguity tests pass unchanged.

- [ ] **Step 3: Replace package/filename catalogues with resolver calls.**

Remove the duplicate LSP-only unit/package policy from `workspace.rs`,
including `resolve_unit_with_cancel`, `directory_unit_candidate_groups`,
`filename_unit_candidates`, `unit_filename_candidate_tiers`,
`unit_name_matches`, `package_unit_candidates`, `package_descriptors`,
`package_catalogue`, `scan_package_catalogue`, and their package-specific
cache fields. Keep generic workspace source enumeration and retention because
those are snapshot orchestration, not unit resolution.

Update `Workspace` construction and invalidation so no stale package catalogue
can survive a source/configuration generation change. The shared resolver's
request-local catalogues must be bounded and must return their observations to
the workspace rather than mutating a global index.

Run the package fixture tests in `workspace.rs` and the complete package:

```bash
cargo test -p pascal-lsp package
cargo test -p pascal-lsp
```

- [ ] **Step 4: Move LSP conditional analysis to pascal-core.**

Change `workspace/rename.rs` and `navigation.rs` imports from
`crate::conditional` to `pascal_core::conditional`. Remove
`pub(crate) mod conditional;` from `pascal-lsp/src/lib.rs`, then delete
`pascal-lsp/src/conditional.rs` after all references are gone.

Keep existing LSP checks: unknown conditional spans block precise rename,
unsupported potentially active directives block the operation, inactive
includes are skipped, and cancellation returns `request cancelled`.

Run:

```bash
rg 'crate::conditional|mod conditional' crates/pascal-lsp/src
cargo test -p pascal-lsp conditional
```

Expected: the ripgrep command returns no LSP conditional module reference and
conditional tests pass through pascal-core.

- [ ] **Step 5: Replace the duplicate include auditor lookup.**

In `workspace/rename.rs`, retain `IncludeAuditor`'s LSP-specific relevance
checks, error limits, `BaselineAccumulator` integration, snapshot modes, and
stale-record generation. Replace `resolve_include_path_with_overrides`,
`IncludeRoute`, duplicate case-insensitive path lookup, and direct package/path
authorization with `UnitResolver::resolve_include`.

For every include occurrence, pass the exact including path and directive byte
range. Merge all checked directory/candidate stamps into the baseline. Use
`Found` content for nested relevance inspection; use `Incomplete` for missing,
ambiguous, unauthorized, malformed, cyclic, or budget-exceeded include state.
Preserve `include_payload = true` on records for every actual include payload
read and preserve the current 4,096-file/256 MiB/16,384-directive/256-depth
caps through `ResolverLimits`.

Run:

```bash
cargo test -p pascal-lsp rename
cargo test -p pascal-lsp codeactions
```

Expected: include, conditional, package, overlay, and rename completeness
tests pass without any direct LSP package lookup function.

- [ ] **Step 6: Verify observation and stale-result parity.**

Add or update tests for:

- an absent candidate becoming a file between compute and revalidation;
- a package descriptor mutation;
- a mapped root becoming unreadable;
- an overlay version/text change;
- a project candidate membership change;
- cancellation during directory scan and include analysis; and
- a stat-only observation that cannot be reused to authorize a payload.

Run the full LSP package and workspace baselines:

```bash
cargo test -p pascal-lsp
cargo test --workspace
```

At implementation time, commit the LSP migration only after the complete
package suite passes. This planning task creates no commit.

---

## Task 3: Add the CFG adapter and shared lint runner

**Files:** Use the Task 3 file map above. The existing compiled-DCU
`lint4d::dcu::ProjectContext` remains intact and is not replaced by the
source-project context.

**Interfaces:**

- Consumes: `pascal_core::ResolvedProject`, `ResolutionReport`, resolver source revisions, `cfg_pascal::ProjectSnapshot` constructors, and existing `lint4d::engine` inputs.
- Produces: `CfgProjectSnapshot`, `CfgSnapshotStatus`, `to_cfg_project_snapshot`, `run_lint_with_cfg_project`, and unchanged `run_lint`/`run_lint_with_context` behavior.

```rust
pub struct CfgProjectSnapshot {
    pub snapshot: cfg_pascal::ProjectSnapshot,
    pub target_unit: cfg_pascal::ProjectUnitId,
    pub status: CfgSnapshotStatus,
    pub resolution: pascal_core::ResolutionReport,
}

pub enum CfgSnapshotStatus {
    Complete,
    Incomplete { reason: String },
}

pub enum CfgSnapshotError {
    Parse { source_id: String, message: String },
    Snapshot(cfg_pascal::ProjectSnapshotError),
    Preparation { source_id: String, message: String },
    MissingTarget(String),
}

pub fn to_cfg_project_snapshot(
    project: pascal_core::ResolvedProject,
    options: CfgSnapshotOptions,
) -> Result<CfgProjectSnapshot, CfgSnapshotError>;

pub fn run_lint_with_cfg_project(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    dcu_project: Option<&crate::dcu::ProjectContext>,
    cfg_project: Option<&crate::cfg::project_snapshot::CfgProjectSnapshot>,
    registry: &RuleRegistry,
) -> Vec<Diagnostic>;
```

- [ ] **Step 1: Add adapter tests before implementation.**

Create `crates/lint4d/tests/cfg_project_snapshot_test.rs` with helper sources
for two units and these failing tests:

```rust
#[test]
fn adapter_uses_stable_source_ids_for_unit_ids() {
    let project = fixture_resolved_project_with_two_units();
    let snapshot = to_cfg_project_snapshot(project, raw_snapshot_options())
        .expect("snapshot conversion");
    let ids = snapshot
        .snapshot
        .units()
        .iter()
        .map(|unit| unit.id().as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["source:/workspace/App.pas", "source:/workspace/Errors.pas"]);
}

```

Add these additional concrete cases to the same test file:

- `adapter_forwards_ambiguous_import_without_loading_a_target`: construct an
  `Ambiguous` resolver target, convert the project, and assert the snapshot
  contains `ImportTarget::Ambiguous` with no loaded target unit.
- `adapter_forwards_exact_authorized_qualifiers`: provide dotted and short
  qualifier spellings and assert `ImportBinding::authorized_qualifiers()` is
  byte-for-byte/order-for-order identical to the resolver result.
- `incomplete_resolution_does_not_claim_prepared_source`: provide an
  incomplete include/conditional result, enable preparation, and assert the
  target input is raw plus `CfgSnapshotStatus::Incomplete`.

Run:

```bash
cargo test -p lint4d --test cfg_project_snapshot_test adapter_uses_stable_source_ids_for_unit_ids
```

Expected: FAIL until the adapter exists.

- [ ] **Step 2: Implement raw resolver-graph conversion.**

In `cfg/project_snapshot.rs`, add the root first and then dependency units in
`ResolvedProject::units` order. Map each `ResolvedUnit.source.id` to
`ProjectUnitId::new(source.id.as_str())` and the same text to
`ProjectSourceId::new(source.id.as_str())`, parse the source bytes with
`cfg_pascal::LANGUAGE`, and create `ProjectUnitInput::new` from the exact
tree/source pair. Add `include_sources` to the immutable source catalogue for
preparation, without creating logical units for include-only files. Maintain a
`HashMap<SourceId, ProjectUnitId>` so import targets do not depend on input
order, and set `target_unit` from `ResolvedProject::root.source.id` through
that map.

Convert every `ResolvedImport` using this table:

```rust
let target = match import.target {
    ResolutionTarget::Found(source_id) => {
        ImportTarget::Loaded(unit_ids[&source_id].clone())
    }
    ResolutionTarget::Unavailable | ResolutionTarget::Incomplete => {
        ImportTarget::Unavailable
    }
    ResolutionTarget::Ambiguous => ImportTarget::Ambiguous,
};
```

Create `UsesSite` from `importer_source_id` and the importer's exact tree span,
pass `authorized_qualifiers` unchanged to `ImportBinding::new`, and call
`ProjectSnapshot::new`. Set `CfgSnapshotStatus::Incomplete` whenever
`ResolvedProject::complete` or its report is false, or a non-found target was
mapped from an incomplete search. Preserve `ImportTarget::Ambiguous` as an
explicit ambiguity. Never synthesize a target from a filename.

Run the four adapter tests from Step 1 and expect PASS for stable IDs, status
mapping, qualifier forwarding, and raw fallback.

- [ ] **Step 3: Add configured preparation and source-map preservation.**

Implement `CfgSnapshotOptions` with:

```rust
pub struct CfgSnapshotOptions {
    pub prepare_configured_sources: bool,
    pub configuration_id: Option<String>,
    pub preparation_environment: cfg_pascal::PreparationEnvironment,
    pub initial_defined_symbols: Vec<String>,
    pub initial_undefined_symbols: Vec<String>,
    pub preparation_limits: cfg_pascal::PreparationLimits,
}
```

When preparation is enabled, convert all loaded resolver originals to
`cfg_pascal::SourceSnapshot`, convert only `Found` include targets to
`cfg_pascal::IncludeBinding`, and call `cfg_pascal::prepare_source` with
`PrepareSourceOptions::new(prepared_id, configuration_id, environment)`, the
initial define/undef symbol sets, and the supplied limits. Require a non-empty
`configuration_id`; derive each prepared source ID deterministically from its
original source ID and configuration ID. Wrap successful results with
`ProjectUnitInput::from_prepared`. Build each include binding from
`ResolvedInclude::including_source_id`, its exact range, and the found target's
source ID; never turn `Unavailable`, `Ambiguous`, or `Incomplete` into a
binding.

If the resolver reports an unknown/incomplete condition, missing/ambiguous/cyclic
include, unsupported directive, or an exceeded preparation budget, retain the
raw sources as `ProjectUnitInput::new` for the whole snapshot (at minimum the
requested target), set `CfgSnapshotStatus::Incomplete`, and do not expose
partially prepared dependencies or call `PreparedSource::new` with a false
`Complete` assertion.

For a successfully prepared unit, map each original import site into prepared
coordinates by enumerating prepared-tree `moduleName` nodes and accepting only
the node whose `SourceMap::map_range` returns the exact original
`importer_source_id`/range. Omit known inactive sites; an unknown, synthetic,
split, or missing mapping forces that unit back to its raw input and marks the
snapshot incomplete. This keeps every `UsesSite` range valid for the tree
stored in the snapshot.

Add tests for copied/masked/synthetic source-map segments, include-boundary
mapping, prepared-coordinate `UsesSite`, and rejection of an incomplete
configured projection.

Run:

```bash
cargo test -p lint4d --test cfg_project_snapshot_test
cargo test --manifest-path /home/antoinegs/gits/cfg-pascal/.worktrees/shared-resolver/Cargo.toml
```

- [ ] **Step 4: Add the snapshot-aware runner behind additive APIs.**

Refactor `engine/mod.rs` so the existing rule/suppression/severity/scope
pipeline accepts a preselected CFG map. Add:

```rust
pub fn run_lint_with_cfg_project(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    dcu_project: Option<&crate::dcu::ProjectContext>,
    cfg_project: Option<&CfgProjectSnapshot>,
    registry: &RuleRegistry,
) -> Vec<Diagnostic>
```

Use `build_file_cfgs_in_project` only for `Some(cfg_project)` whose status is
`Complete`; for an incomplete snapshot use the target's raw input with the
existing file-local `build_file_cfgs` and do not expose partial cross-unit
facts. For `None`, retain the existing file-local path. Select the target
input's tree/source as the CFG pair and verify that `FileInfo` plus the caller's
raw `source` match the target's original path/bytes; prepared bytes may differ
only behind the validated `SourceMap`. A mismatch is an adapter error. Keep the
same `ProcId` construction, parse-error filtering, rule dispatch, suppression,
scope enrichment, sorting, and DCU context. Convert an adapter/build error to
one `lint4d-error` diagnostic with
severity Error. When a prepared target is used, translate diagnostic byte
ranges through its `SourceMap`; a synthetic, multi-file, or otherwise unmappable
range must discard the prepared diagnostics and rerun the target through the raw
file-local path rather than invent a location.

Make `run_lint` and `run_lint_with_context` delegate with `cfg_project: None`.
Add tests that compare their diagnostics byte-for-byte with the pre-refactor
results for clean and failing fixtures.

Run:

```bash
cargo test -p lint4d --test engine_run_test
cargo test -p lint4d --test engine_context_test
```

- [ ] **Step 5: Wire project-aware CLI linting without changing projectless CLI behavior.**

In `main.rs`, retain `discover_source_files` and direct `fs::read` for
positional CLI paths. When `--project` is present, discover one
`pascal_project::ProjectContext` using the existing project options and use a
disk-only `FilesystemSourceStore` for each requested source. Build a bounded
resolver graph and `CfgProjectSnapshot` for each file that needs project CFG
precision. Populate preparation options from the selected project
configuration/platform and define/undef sets; use a deterministic
configuration ID. If project metadata is incomplete, pass the raw
conservative path and retain the warning; do not infer a target.

Keep the existing compiled-DCU `resolve_dcu_context` result as the separate
`dcu_project` argument. Keep parallel file processing deterministic by sorting
the final `(path, source, diagnostics)` list as it is sorted today. Do not
share a mutable resolver session across Rayon workers; each worker gets a
request-local session or an immutable already-built snapshot.

Add CLI integration tests for disk unit resolution, package fallback, an
ambiguous unit, a projectless file, and a configured source with an incomplete
conditional. Assert exit status and diagnostic output remain conservative.

Run:

```bash
cargo test -p lint4d --test cli_test
cargo test -p lint4d --test snapshot_test
```

If a named CLI test target is absent, run `cargo test -p lint4d` and inspect
the existing `assert_cmd` tests by their function names.

- [ ] **Step 6: Wire the same runner into LSP diagnostics.**

In `pascal-lsp/src/workspace.rs`, after the existing context/configuration
checks, construct the overlay-aware resolver store from the current accepted
document and context. Build a project snapshot for the target document and
call `lint4d::engine::run_lint_with_cfg_project`.

If the project graph is incomplete, keep ordinary lint diagnostics running on
the raw conservative snapshot and add only the existing server warning path;
do not emit precise cross-unit CFG facts. Convert diagnostics using the
existing `DiagnosticLineIndex`, and preserve cancellation polling before and
after every resolver/runner call.

Add an LSP diagnostic parity test showing an unchanged overlay selects the same
unit/package/include as disk, and a changed overlay invalidates the computed
result before publication.

Run:

```bash
cargo test -p pascal-lsp workspace
cargo test -p pascal-lsp --test protocol_barriers --features test-support
```

- [ ] **Step 7: Align lint4d dependency manifests and regenerate Cargo.lock.**

In `lint4d/Cargo.toml`, set:

```toml
cfg-core = {
    git = "https://github.com/AntoineGS/cfg-core.git",
    rev = "b4131e61a939e0685194712689d1fd1497a41492",
}
cfg-pascal = {
    git = "https://github.com/AntoineGS/cfg-pascal.git",
    rev = "208d6743c61e0b391195958270a5e04e3a4328d4",
}
```

The branch URL is the cross-repository implementation integration route; after
the upstream branch is merged, replace it with the published immutable
`cfg-pascal` revision before release. Do not add a path dependency to either
repository. Regenerate the lockfile only through Cargo:

```bash
cargo update -p cfg-core
cargo update -p cfg-pascal
cargo metadata --format-version 1 --locked --no-deps
```

Run from `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver`, then verify:

```bash
rg '/home/|file://|C:\\' Cargo.toml Cargo.lock
```

Expected: no tracked absolute dependency path.

- [ ] **Step 8: Run both complete baselines and finish the implementation handoff.**

Run:

```bash
cargo test
```

from `/home/antoinegs/gits/cfg-pascal/.worktrees/shared-resolver`, and:

```bash
cargo test --workspace
```

from `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver`.

Confirm the recorded results are at least 212 passing cfg-pascal tests and
1,909 passing lint4d workspace tests with 5 ignored, plus the new resolver,
adapter, parity, and source-map tests. At implementation time, commit Task 3
in small repository-local commits after each green task boundary. This
planning task creates no code and no commits.
