# Shared Pascal unit/package resolver design

**Date:** 2026-09-17
**Status:** Design for review
**Scope:** `cfg-pascal/.worktrees/shared-resolver` and `lint4d/.worktrees/shared-resolver`

## Summary

`pascal-lsp` currently owns the policy for resolving Pascal units, includes, and
source packages. That policy is useful to both the CFG builder and lint4d, but
it is entangled with LSP workspace state, overlays, navigation indexes, and
stale-result handling. This design moves the reusable resolution policy into a
request-scoped `pascal-core` resolver while keeping project discovery metadata
in `pascal-project` and stateful orchestration in `pascal-lsp`.

The CFG integration is additive. `cfg-pascal` keeps its existing
caller-owned `ProjectSnapshot`, preparation, and source-map contracts. lint4d
adds an adapter that converts a resolver result into that snapshot and adds a
shared lint runner which can consume either the existing file-local CFG path or
the project snapshot path.

The implementation is deliberately conservative: an unavailable, ambiguous,
incomplete, unauthorized, stale, or cancelled lookup never becomes a unique
resolved unit. No resolver API infers a unit from a basename suffix, a unit ID,
or a guessed namespace.

## Existing boundaries

The implementation uses these isolated worktrees:

| Worktree | Existing responsibility | New responsibility |
| --- | --- | --- |
| `/home/antoinegs/gits/cfg-pascal/.worktrees/shared-resolver` | CFG construction, immutable `ProjectSnapshot`, `prepare_source`, `SourceMap` | Keep the public CFG contracts stable; pin the `cfg-core` dependency without a local absolute path |
| `/home/antoinegs/gits/lint4d/.worktrees/shared-resolver` | `pascal-project` discovery, `pascal-core` parsing/discovery, `pascal-lsp` workspace resolution, lint4d engine | Add the reusable resolver, migrate LSP policy, and add lint4d's CFG adapter/runner |

The current `cfg-pascal` project API is already the correct semantic boundary:

- `ProjectUnitInput` owns an immutable tree/source pair.
- `ProjectSnapshot` validates stable unit/source IDs, exact `uses` spans,
  explicit import targets, authorized qualifiers, and prepared-source
  consistency.
- `build_file_cfgs` remains file-local.
- `build_file_cfgs_in_project` consumes only the caller-selected snapshot and
  never discovers files or infers imports.
- `prepare_source` expands only caller-selected include bindings and evaluates
  only its documented bounded conditional subset.
- `SourceMap` validates complete prepared-buffer coverage and preserves copied,
  masked, and synthetic segments.

The current `pascal-project::ProjectContext` already contains the metadata the
resolver needs: ordered unit/include paths, path provenance, explicit unit
references, aliases, namespaces, defines, packages, effective overrides,
requester-scoped `ReadPolicy`, metadata files, metadata observations, warnings,
and discovery completeness. Its project and package parsers remain the source
of truth for that metadata.

## Goals

1. Make unit, package, and include lookup policy reusable by lint4d and LSP.
2. Preserve the current CLI disk-source and LSP overlay behavior.
3. Preserve exact precedence, aliases, namespaces, package fallback, and
   ambiguity behavior.
4. Preserve requester-scoped read authorization, path provenance, symlink
   rules, resource limits, cancellation, observations, and revalidation data.
5. Give lint4d a safe path from a resolved project graph to
   `cfg_pascal::ProjectSnapshot`.
6. Keep `cfg-pascal` and `pascal-project` independent of LSP orchestration at
   the crate dependency level, retain the existing `pascal-lsp -> lint4d`
   edge, and introduce no dependency cycle.
7. Keep `run_lint`, `run_lint_with_context`, and file-local CFG behavior
   source-compatible and behavior-compatible.

## Non-goals

- `cfg-pascal` will not gain filesystem, environment, MSBuild, LSP, or project
  index dependencies.
- `pascal-core` will not own open-document state, LSP generations, navigation
  indexes, worker scheduling, or stale-result decisions.
- `pascal-project` will not expand include files or parse every Pascal source
  in a workspace.
- The resolver will not become a full Delphi compiler, a full MSBuild
  evaluator, or a type-system implementation.
- The first implementation will not change receiver/property resolution in
  `pascal-lsp` or lint4d rules unrelated to CFG construction.
- A configured source will not be silently treated as a precise source when
  conditional or include information is incomplete.

## Architecture

```text
ProjectOptions + filesystem metadata
                |
                v
      pascal-project::ProjectContext
      (paths, aliases, namespaces, packages,
       provenance, read policy, observations)
                |
                v
   pascal-core::UnitResolver<S: SourceStore>
   (precedence, candidates, packages, includes,
    conservative condition analysis, cancellation)
          /                         \
         /                           \
        v                             v
pascal-lsp adapter              lint4d CFG adapter
overlays + index +              resolved graph ->
workspace snapshots             cfg_pascal::ProjectSnapshot
        |                             |
        v                             v
NavigationIndex                 shared lint runner
and LSP revalidation            + build_file_cfgs_in_project
```

### `pascal-project`: discovery metadata only

`pascal-project` remains responsible for selecting a project and evaluating
bounded project metadata. The existing `ProjectContext` fields remain the
canonical inputs to resolution. The implementation adds public, read-only
helpers so consumers do not copy path-provenance logic from `pascal-lsp`:

```rust
impl ProjectContext {
    pub fn path_entry_for(&self, path: &Path) -> Option<ProjectPathEntry>;

    pub fn include_search_entries(&self, owner: &Path) -> Vec<ProjectPathEntry>;

    pub fn selected_project_options(&self) -> ProjectOptions;
}
```

`path_entry_for` must prefer the exact `MainSource`/explicit-reference entry,
then the most-specific ordered search-path entry, then the read-policy mapping
entry. It returns the original `ProjectPathProvenance`; it does not turn a
stat-only observation into read authorization.

`include_search_entries` returns the including file's directory first, then
`include_path_entries`, then `search_path_entries`, with path-equivalent
duplicates removed while preserving first occurrence. The caller supplies a
per-document legacy-route proof when an older legacy-native sibling route is
valid; that proof is never stored as a global directory grant in
`ProjectContext`.

`selected_project_options` copies the selected `config` and `platform` into the
`build_config` and `platform` fields of the `ProjectOptions` shape used by
package metadata parsing. It does not read anything or mutate the context.

The existing public package metadata API remains the only package descriptor
parser used by the resolver:

```rust
pub fn read_package_metadata_with_observations(
    path: &Path,
    options: &ProjectOptions,
    overrides: &EffectiveOverrides,
    read_policy: &ReadPolicy,
    entry: &ProjectPathEntry,
) -> Result<PackageMetadataRead, String>;
```

Package metadata observations are retained with the resolution report. A
stat-only observation is an invalidation fact, not a later authorization to
read the path.

### `pascal-core`: request-scoped resolver

Add `crates/pascal-core/src/resolver.rs` and export it from
`crates/pascal-core/src/lib.rs`. The resolver owns no process-global state and
does not know about URLs, LSP workers, or lint rules. A caller creates a
resolver session for one analysis request, supplies a `ProjectContext`,
workspace roots, a source store, and limits, then drops or explicitly caches
the returned immutable result.

The planned public contract is:

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolverLimits {
    pub max_dependency_units: usize,
    pub max_directory_entries: usize,
    pub max_scanned_bytes: usize,
    pub max_source_bytes: usize,
    pub max_include_files: usize,
    pub max_include_bytes: usize,
    pub max_include_directives: usize,
    pub max_include_depth: usize,
    pub max_package_lookups: usize,
    pub max_package_unit_candidates: usize,
    pub max_package_catalogue_entries: usize,
    pub max_package_catalogues: usize,
    pub max_resolution_warnings: usize,
}

impl Default for ResolverLimits {
    fn default() -> Self {
        Self {
            max_dependency_units: 256,
            max_directory_entries: 1_048_576,
            max_scanned_bytes: 8 * 1024 * 1024 * 1024,
            max_source_bytes: 16 * 1024 * 1024,
            max_include_files: 4_096,
            max_include_bytes: 256 * 1024 * 1024,
            max_include_directives: 16_384,
            max_include_depth: 256,
            max_package_lookups: 256,
            max_package_unit_candidates: 1_024,
            max_package_catalogue_entries: 524_288,
            max_package_catalogues: 64,
            max_resolution_warnings: 256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Unit,
    Include,
    PackageDescriptor,
}

pub struct DirectoryRequest<'a> {
    pub directory: &'a Path,
    pub entry: &'a ProjectPathEntry,
    pub read_policy: &'a ReadPolicy,
}

pub struct SourceRequest<'a> {
    pub path: &'a Path,
    pub entry: &'a ProjectPathEntry,
    pub read_policy: &'a ReadPolicy,
    pub legacy_route: Option<&'a LegacyRoute>,
    pub kind: SourceKind,
    pub max_bytes: usize,
}

pub trait SourceStore {
    fn list_directory(
        &mut self,
        request: DirectoryRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<DirectoryListing, SourceStoreError>;

    fn overlay_candidates(&self, roots: &[PathBuf], names: &[String]) -> Vec<PathBuf>;

    fn load(
        &mut self,
        request: SourceRequest<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<LoadedSource, SourceStoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryListing {
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
    pub stamp: Option<ProjectReadStamp>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSource {
    pub id: SourceId,
    pub path: PathBuf,
    pub bytes: Arc<[u8]>,
    pub revision: SourceRevision,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(String);

impl SourceId {
    pub fn new(value: impl Into<String>) -> Self;
    pub fn as_str(&self) -> &str;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRevision {
    Disk {
        stamp: ProjectReadStamp,
        content_hash: u64,
        read_policy: ReadPolicy,
        path_entry: ProjectPathEntry,
    },
    Overlay {
        version: i32,
        content_hash: u64,
    },
}

pub struct UnitResolver<S> {
    // The concrete fields are private. The session owns only bounded,
    // request-local catalogues and observations.
    store: S,
}

impl<S: SourceStore> UnitResolver<S> {
    pub fn new(
        context: ProjectContext,
        workspace_roots: Vec<PathBuf>,
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

`SourceId` identifies a canonical logical path, not a particular disk revision
or overlay version.  The built-in disk and LSP stores use the deterministic
`source:<canonical-path>` spelling and return the same ID for an overlay and
the disk source at that path.  A custom `SourceStore` must provide the same
stable identity property. Here “canonical” means absolute, lexically
normalized, and platform-normalized without requiring a successful filesystem
canonicalization; it must not follow symlinks, because an overlay may exist
before its disk path does. IDs must never depend on candidate order or a
basename-only guess.

The source-store contract has three required properties:

1. `list_directory` and `load` check the supplied `ReadPolicy` and
   `ProjectPathEntry` before touching disk. `LegacyNative` may use the
   deliberately compatible legacy payload route only when the resolver
   supplies a valid `SourceRequest::legacy_route` proof; configured and mapped
   entries use strict regular-file, symlink-free reads.
2. An overlay for a path wins over disk, including when disk has a different
   revision or is absent. The resolver merges `overlay_candidates` into the
   corresponding directory/name tier before loading candidates. The overlay's
   version and content hash are returned as `SourceRevision::Overlay`.
3. Every directory listing and payload read reports enough identity for the
   caller to revalidate it. Failed, incomplete, and cancelled operations are
   represented in the outcome/report and cannot produce `Found`.

`pascal-core` supplies a `FilesystemSourceStore` for CLI and other disk-only
callers. `pascal-lsp` supplies a thin overlay-aware implementation of the same
trait. The shared resolver performs candidate ordering and interpretation; a
store does not decide which candidate wins.

`resolve_project` receives the root's already indexed `ImportSite` list in its
`sites` argument; it parses each newly loaded dependency only on demand to
discover that dependency's import and include occurrences, then walks imports
in deterministic breadth-first order. `ResolvedProject::units` contains
dependency units only;
the separately named `root` is included exactly once by adapters. Include
payloads that are not units are kept in `include_sources` so the result is
self-contained after the request-scoped resolver is dropped.
`ResolvedProject::report` is the report finalized at the end of that project
walk. `finish` is the equivalent finalization hook for sessions that use the
individual resolve methods; callers must not silently replace an embedded
project report with a later report that includes unrelated operations.

The resolution data types are:

```rust
pub struct UnitResolveRequest<'a> {
    pub requested_name: &'a str,
    pub importer_path: &'a Path,
    pub legacy_route: Option<&'a LegacyRoute>,
}

pub struct LegacyRoute {
    pub source_path: PathBuf,
    pub sibling_directory: PathBuf,
}

pub struct ImportSite {
    pub byte_range: Range<usize>,
    pub requested_name: String,
    pub section: ImportSection,
}

pub struct IncludeResolveRequest<'a> {
    pub including_path: &'a Path,
    pub byte_range: Range<usize>,
    pub requested_name: &'a str,
    pub legacy_route: Option<&'a LegacyRoute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportSection { Module, Interface, Implementation }

pub struct ResolvedUnit {
    pub requested_name: String,
    pub declared_name: String,
    pub source: LoadedSource,
}

pub type ResolvedSource = LoadedSource;

pub struct ResolvedImport {
    pub importer_source_id: SourceId,
    pub site: ImportSite,
    pub target: ResolutionTarget<SourceId>,
    pub authorized_qualifiers: Vec<String>,
}

pub struct ResolvedInclude {
    pub including_source_id: SourceId,
    pub byte_range: Range<usize>,
    pub requested_name: String,
    pub target: ResolutionTarget<SourceId>,
}

pub struct ResolvedImports {
    pub bindings: Vec<ResolvedImport>,
    pub dependencies: Vec<ResolvedUnit>,
    pub complete: bool,
}

pub struct ResolvedProject {
    pub root: ResolvedUnit,
    /// Dependency units in deterministic breadth-first order. The root is not
    /// repeated in this vector.
    pub units: Vec<ResolvedUnit>,
    pub imports: Vec<ResolvedImport>,
    pub includes: Vec<ResolvedInclude>,
    /// Include payloads that are not already represented by `root` or `units`.
    /// The vector is de-duplicated by `SourceId` and is retained so a caller
    /// can construct occurrence-specific `cfg-pascal::IncludeBinding` values.
    pub include_sources: Vec<LoadedSource>,
    pub complete: bool,
    pub report: ResolutionReport,
}

pub enum ResolutionTarget<T> {
    Found(T),
    Unavailable,
    Ambiguous,
    Incomplete,
}

pub enum Resolution<T> {
    Found(T),
    Unavailable { reason: String },
    Ambiguous { candidates: Vec<ResolutionCandidate> },
    Incomplete { reason: String, candidates: Vec<ResolutionCandidate> },
}

pub struct ResolutionOutcome<T> {
    pub result: Resolution<T>,
    pub observations: Vec<ResolutionObservation>,
    pub warnings: Vec<String>,
}

pub struct ResolutionCandidate {
    pub path: PathBuf,
    pub declared_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolutionObservation {
    Directory {
        path: PathBuf,
        entry: ProjectPathEntry,
        stamp: Option<ProjectReadStamp>,
        complete: bool,
    },
    Candidate {
        path: PathBuf,
        entry: Option<ProjectPathEntry>,
        stamp: Option<ProjectReadStamp>,
        present: bool,
    },
    Payload {
        source_id: SourceId,
        path: PathBuf,
        revision: SourceRevision,
    },
    Metadata(MetadataObservation),
    ProjectRead(ProjectReadObservation),
}

pub struct ResolutionReport {
    pub observations: Vec<ResolutionObservation>,
    pub warnings: Vec<String>,
    pub complete: bool,
    pub incomplete_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceStoreError {
    Cancelled,
    NotFound { path: PathBuf },
    Unauthorized { path: PathBuf, reason: String },
    NotRegularFile { path: PathBuf },
    TooLarge { path: PathBuf, maximum: usize },
    Io { path: PathBuf, message: String },
    Incomplete { path: PathBuf, reason: String },
}
```

The exact error enum is public and bounded rather than a free-form panic path:

```rust
pub enum ResolverError {
    Cancelled,
    InvalidRequest(String),
    SourceStore(SourceStoreError),
    LimitExceeded { limit: &'static str, observed: usize, maximum: usize },
}
```

The implementation may add private detail to these types, but the names,
status distinctions, and ownership rules above are the compatibility contract.

## Resolution semantics

### Unit precedence

For `requested_name`, the resolver first applies the case-insensitive alias
map in `ProjectContext.unit_aliases`. It retains both the original spelling
and the alias target for diagnostics and qualifier authorization. It then
searches these tiers in order:

1. `explicit_unit_entries` for the alias target.
2. The importing source's directory.
3. `search_path_entries` in project order.
4. For projectless contexts only, the bounded filename catalogues rooted at
   the supplied workspace roots.
5. Named package descriptors from `ProjectContext.packages`, in package order,
   only after ordinary unit lookup has produced no result.

Within one directory, candidate filenames are evaluated in these ordered
groups:

1. `<requested>.pas`, then the short component of a dotted name when it is a
   distinct spelling.
2. For an unqualified name only, `<namespace>.<name>.pas` for each configured
   namespace in order.

The resolver loads a candidate before accepting it and checks its declared unit
name against the requested and alias spellings. It never accepts a candidate
merely because its suffix resembles the request. A unique candidate in the
first non-empty group wins. Two valid candidates in that group return
`Ambiguous` and block all lower-precedence groups. An incomplete directory or
package catalogue returns `Incomplete` and never permits a unique result from
the remaining candidates.

This preserves the existing distinction between:

- `Unavailable`: the searched, complete set had no valid target;
- `Ambiguous`: more than one valid target matched at the same precedence;
- `Incomplete`: the resolver could not prove the candidate set or policy
  complete; and
- `Found`: one authorized, loaded, declaration-matching source.

### Aliases and namespaces

Aliases are matched case-insensitively and are applied once. A dotted alias
target is treated as fully qualified; namespace expansion is not appended to
it. An unqualified request can use configured namespaces in the order supplied
by the project. Namespace spellings are recorded as explicit authorized
qualifiers on `ResolvedImport`; the CFG adapter passes those exact spellings to
`ImportBinding` rather than deriving qualifiers from a `ProjectUnitId`.

Every `ResolvedImport` carries the `importer_source_id` that owns its
`ImportSite`. This is required when a project result aggregates imports from
more than one unit and prevents the CFG adapter from accidentally constructing
a `UsesSite` against the target unit's tree.

### Packages

Package lookup remains separate from unit search paths and the project-wide
unit index. The resolver scans only supplied workspace/source roots and
authorized mapped roots, does not follow symlinks, and observes the bounded
catalogue. When a package stem has both `.dpk` and `.dproj` descriptors, `.dpk`
descriptors win for that stem, matching current behavior. Multiple matching
descriptors make that package ambiguous; a missing compiled-only package is a
warning and does not become a source result.

For a selected descriptor, the resolver delegates parsing to
`read_package_metadata_with_observations`. Every `contains`/reference path is
checked against the descriptor's inherited path provenance and the requester's
`ReadPolicy`. A package path outside an authorized root is skipped and the
lookup is incomplete when that omission could make a supposed unique result
unsafe.

### Include lookup

`resolve_include` uses the current including directory, then
`ProjectContext.include_path_entries`, then unit search entries. It applies the
effective override path mapping, preserves mapped versus legacy provenance,
performs the existing case-insensitive component lookup when the exact path is
absent, and records every checked directory/candidate stamp.

The resolver does not flatten include files itself. It returns an explicit
`ResolvedInclude` target for callers such as `cfg-pascal::prepare_source` or an
LSP include audit. `ResolvedInclude::including_source_id` identifies the source
whose `byte_range` was checked. A project-resolution call scans the loaded unit
sources for include directives, records one `ResolvedInclude` per occurrence,
and retains successful non-unit payloads in `include_sources`; a direct
`resolve_include` call remains available for callers that already own the
directive walk. Repeated and nested include occurrences retain their exact byte
ranges and receive independent caller-owned expansion identities when
converted to `cfg_pascal::IncludeBinding`.

### Conditionals

Move the conservative directive analysis currently in
`pascal-lsp/src/conditional.rs` to `pascal-core/src/conditional.rs`. Its public
analysis retains:

- offset-preserving projected bytes;
- inactive and unknown spans;
- directive kind, range, activity, and raw body;
- `Truth::{True, False, Unknown}`;
- bounded directive, expression, environment, and work budgets; and
- cancellation as an incomplete analysis.

The planned entry point is:

```rust
pub fn analyze_with_cancel(
    source: &str,
    project_defines: &[String],
    cancel: &dyn CancellationToken,
) -> ConditionalAnalysis;
```

`ConditionalAnalysis::complete` means that the bounded walk finished without a
malformed structure, cancellation, or budget exhaustion; it does not mean that
every condition is known. A structurally complete analysis may still contain
`Truth::Unknown` activity and `unknown_spans`. Consumers must inspect both
fields: unknown activity is conservative `Incomplete` resolution, while a
known inactive include is skipped.

An include known to be inactive is not resolved. An include in an unknown or
incomplete conditional is represented as `Incomplete`; it is never assumed
absent. The resolver uses this analysis for LSP completeness and include
audits. Exact configured projection remains in `cfg-pascal::prepare_source`.

`prepare_source` continues to require an explicit complete environment and
occurrence-specific include bindings. It continues to propagate supported
`DEFINE`/`UNDEF` state through selected nested includes, reject unknown active
conditions, reject malformed/unsupported active directives, enforce its
deterministic `PreparationLimits`, and return a validated `PreparedSource`.

### Read policy and source precedence

All disk payload reads pass the exact `ProjectPathEntry` that produced the
candidate through the exact `ReadPolicy` retained in `ProjectContext`.

- `Configured` and `Mapped` payloads require regular, symlink-free paths under
  their authorized root.
- `LegacyNative` retains the deliberately narrow legacy sibling/search-path
  behavior, including its compatible symlink route only when a caller has
  proved that route for the current source.
- A `MetadataObservation::Stat` records freshness but never authorizes a
  later payload read.
- A path found through an override mapping retains mapped provenance even when
  the native destination is inside a workspace root.
- Overlay bytes are already requester-supplied and take precedence over disk,
  but their URI/path remains checked against the same workspace/project scope.

`FilesystemSourceStore` uses `ReadPolicy::read_payload_bytes` for strict paths
and `ReadPolicy::read_legacy_payload_bytes` only for a resolver-authorized
legacy route. It reads at most `max_bytes + 1` bytes so an oversized file is
rejected before its contents are retained.

### Limits, cancellation, and observations

The limits in `ResolverLimits` preserve the current LSP safety values. Project
metadata limits remain in `pascal-project`; LSP snapshot retention limits
remain in `pascal-lsp`; `cfg-pascal::PreparationLimits` remains authoritative
for configured source expansion. The resolver checks the cancellation token
before directory reads, each directory entry, each candidate load, each
package descriptor, each include, and each recursive step.

The report includes:

- project/option-set/package payload observations from `pascal-project`;
- directory and candidate stamps, including absent candidates;
- source payload identity, content hash, path provenance, read policy, and
  overlay version; and
- incomplete reasons and bounded warnings.

The LSP maps these observations into its existing `SourceRecord`, baseline,
configuration-watch, and candidate-membership records. A computation is still
discarded when source/configuration generations change or any consumed record
fails revalidation. The resolver does not decide whether a result is stale;
that remains a workspace policy.

## LSP integration

`pascal-lsp` remains the owner of:

- `Workspace`, open-document overlays, document versions, and generations;
- `NavigationIndex` parsing/indexing and import binding storage;
- local/import/workspace snapshot modes and retention caps;
- worker cancellation and stale-result rejection; and
- LSP error messages, locations, edits, and result formatting.

It gains a small `SourceStore` bridge in
`crates/pascal-lsp/src/workspace/resolver.rs`. The bridge maps canonical file
paths to open overlays and delegates disk reads to the shared strict reader.
It returns an overlay before disk and records the overlay version. It does not
perform unit/package/include matching. It uses the same canonical path to
construct the `SourceId` for overlay and disk content, so switching between an
unchanged overlay and disk cannot change the CFG snapshot identity.

The existing `Workspace::load_imports_with_cancel` flow becomes:

1. Read import sites from `NavigationIndex`.
2. Construct one resolver session for the current document/context.
3. Convert each site to `ImportSite` and call `resolve_imports`.
4. Bind only `Found` targets into `NavigationIndex`; bind an explicit empty map
   when lookup is unavailable, ambiguous, or incomplete.
5. Return loaded dependency paths to the existing bounded closure walker.
6. Merge the report observations/warnings into the existing workspace state.

`workspace/rename.rs` keeps enumeration, source retention, include relevance,
baseline creation, and revalidation. Its duplicate filename/package/include
candidate algorithms are removed in favor of the shared resolver and shared
conditional analysis. The current `RenameSnapshot` and `SnapshotMode` remain
LSP-only types.

The existing LSP behavior remains fail-closed:

- local binding requests may use a proven self-contained result;
- import-dependent and workspace requests require complete enough context for
  their mode;
- an ambiguous project, package, unit, or include stops precise navigation;
- conditional unknowns and unsupported directives block edits when relevant;
- cancellation returns the existing `request cancelled` outcome; and
- a changed overlay, disk source, metadata file, candidate membership, or
  effective configuration invalidates the computed result.

## CFG and lint4d integration

### Adapter

Add `crates/lint4d/src/cfg/project_snapshot.rs` with this public model:

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

pub struct CfgSnapshotOptions {
    pub prepare_configured_sources: bool,
    pub configuration_id: Option<String>,
    pub preparation_environment: cfg_pascal::PreparationEnvironment,
    pub initial_defined_symbols: Vec<String>,
    pub initial_undefined_symbols: Vec<String>,
    pub preparation_limits: cfg_pascal::PreparationLimits,
}
```

The adapter assigns `cfg_pascal::ProjectUnitId` from the resolver's stable
`SourceId`, not from input order or a basename. It includes `root` first and
then `ResolvedProject::units` in their stable order, parses each selected unit
source with `cfg_pascal::LANGUAGE`, and creates `ProjectUnitInput` values. It
uses the same source ID text for each raw `ProjectSourceId`, while prepared
inputs use the derived prepared ID and retain the original source ID in their
source map. It also adds `include_sources` to the source-snapshot catalogue
when preparation needs them. It sets `target_unit` from `root.source.id` after
the source-to-unit map is built. It maps each `ResolvedImport` as follows:

| Resolver target | `ImportTarget` | Snapshot status |
| --- | --- | --- |
| `Found(source_id)` | `Loaded(mapped unit ID)` | unchanged |
| `Unavailable` | `Unavailable` | unchanged |
| `Ambiguous` | `Ambiguous` | unchanged |
| `Incomplete` | `Unavailable` | `Incomplete` |

The adapter sets `CfgSnapshotStatus::Incomplete` whenever
`ResolvedProject::complete` or its report is false, or whenever a non-found
target came from an incomplete search. An ambiguity remains an explicit
`ImportTarget::Ambiguous`, not an invented unavailable target.

The adapter passes `authorized_qualifiers` unchanged to `ImportBinding`. For
raw inputs it maps `ResolvedImport::importer_source_id` to the importer's tree
and uses the original `byte_range`. For a prepared input it finds the
prepared-tree `moduleName` whose `SourceMap::map_range` maps exactly back to the
same importer source/range; only that one-to-one mapping is accepted. A known
inactive import is omitted, while an unknown, synthetic, split, or otherwise
unmapped import makes that unit fall back to its raw input. Thus every
`UsesSite` range is validated against the exact tree stored in its input and is
never copied between raw and prepared coordinate spaces.

Raw source mode remains the default and preserves current lint behavior. When
`prepare_configured_sources` is enabled, the adapter converts loaded originals
to `cfg_pascal::SourceSnapshot`, converts only `Found` include results to
`IncludeBinding`, and calls `prepare_source`. Each binding uses
`ResolvedInclude::including_source_id`, its exact range, and the found target's
source ID; `include_sources` supplies the target bytes. `configuration_id` is
required in this mode; the caller derives it deterministically from the
selected project configuration/platform and passes the project's define/undef
sets through the two symbol vectors. The adapter derives a distinct
deterministic prepared source ID from each original `SourceId` and that
configuration ID. If any active include or conditional is unavailable,
ambiguous, incomplete, malformed, unsupported, or over budget, the adapter
does not claim `PreparedSource`; it retains a raw
conservative input for every unit in the snapshot (at minimum the requested
target) and marks the result `Incomplete`. An incomplete snapshot must not mix
partially prepared dependency inputs with a target that the runner will treat
as raw.

Prepared inputs retain the validated `SourceMap`. CFG ranges and diagnostics
are first computed in prepared coordinates and are mapped back through
`SourceMap::map_range`; synthetic portions have no invented file origin, and a
range crossing an include boundary remains multi-span. The default raw path
continues to use original coordinates and the existing diagnostics.

### Shared lint runner

Extend `crates/lint4d/src/engine/mod.rs` without changing the current wrappers:

```rust
pub fn run_lint_with_cfg_project(
    file: &FileInfo,
    source: &[u8],
    config: &Config,
    dcu_project: Option<&crate::dcu::ProjectContext>,
    cfg_project: Option<&crate::cfg::project_snapshot::CfgProjectSnapshot>,
    registry: &RuleRegistry,
) -> Vec<Diagnostic>;
```

`run_lint` and `run_lint_with_context` call this path with `cfg_project: None`
and therefore retain the existing file-local `build_file_cfgs` behavior. When
the snapshot is present with `CfgSnapshotStatus::Complete`, the runner calls
`build_file_cfgs_in_project(&cfg_project.snapshot, &cfg_project.target_unit)`
and supplies the resulting CFG map to the same `AnalysisContext` and rules. An
`Incomplete` snapshot uses the target's raw input with file-local
`build_file_cfgs`; it does not use any partially resolved dependency units for
cross-unit facts. The runner uses the target input's tree/source as the CFG
analysis pair and verifies that `FileInfo` plus the caller's raw `source` match
the target's original path/bytes (the prepared bytes may differ only behind
the validated `SourceMap`). A mismatch is an adapter error.

For prepared targets, the runner converts rule positions from prepared byte
offsets through the retained `SourceMap` before producing the existing
line/column-only `Diagnostic`. It never invents an origin for synthetic bytes;
diagnostics whose range maps to multiple files or no single origin are omitted
from the precise prepared result, and the runner reruns the target through its
raw file-local path rather than returning diagnostics with guessed locations.
An adapter error becomes the existing bounded `lint4d-error` diagnostic rather
than a panic. The compiled-DCU `dcu::ProjectContext` remains a separate input
for type-aware lint rules.

The CLI uses `pascal_project::ProjectContext` only when a project context is
available, constructs a disk-only resolver/store for the requested file, and
falls back to the unchanged file-local path for projectless positional inputs.
The LSP diagnostics path uses the same runner with its overlay-aware store.
Both callers keep their own output filtering, suppression, baseline, and stale
result policies.

## Dependency alignment

The dependency graph remains acyclic:

```text
pascal-lsp -> lint4d -> pascal-core -> pascal-project
     |           |             |
     |           +-> cfg-pascal -> cfg-core
     +-> pascal-project
```

Arrows mean "depends on." The existing `pascal-lsp -> lint4d` edge is retained
so LSP diagnostics can use the shared lint runner; no reverse edge is added.
`cfg-pascal` does not depend on `pascal-core`, `pascal-project`, or lint4d.
`lint4d` keeps its existing workspace-relative `pascal-core = { path =
"../pascal-core" }` dependency.

Both `cfg-pascal/Cargo.toml` and `lint4d/Cargo.toml` pin `cfg-core` to the
existing repository revision
`b4131e61a939e0685194712689d1fd1497a41492`:

```toml
cfg-core = {
    git = "https://github.com/AntoineGS/cfg-core.git",
    rev = "b4131e61a939e0685194712689d1fd1497a41492",
}
```

During cross-repository implementation, lint4d consumes the shared-resolver
`cfg-pascal` branch through its repository URL and branch name
`feat/shared-resolver`, never through `/home/...`, `C:\...`, `file://`, or any
other tracked absolute path. Once the upstream branch is integrated, the
manifest and lockfile use the published immutable `cfg-pascal` revision. Cargo
lockfiles are regenerated by Cargo; they are not hand-edited.

## Error and conservative behavior

The following table is normative:

| Situation | Resolver result | Consumer action |
| --- | --- | --- |
| No authorized candidate in a complete search | `Unavailable` | Keep unknown namespace/import; continue only with conservative analysis |
| Multiple candidates at one precedence tier | `Ambiguous` | Do not search lower tiers; do not bind a target |
| Catalogue, project, package, condition, or budget incomplete | `Incomplete` | Mark request/snapshot incomplete; never create a precise binding |
| Unauthorized or unsafe path | `Unavailable` or `Incomplete` according to whether the candidate set is complete | Never read the payload |
| Active include missing or cyclic | `Incomplete` | Refuse precise prepared/rename result |
| Cancelled during any operation | `ResolverError::Cancelled` | Preserve the caller's cancellation/stale-result path |
| Source/configuration revision changed after computation | Report revalidation failure | Discard result and retry/return the existing stale error |
| Unsupported type/receiver/conditional semantics | Unknown/conservative fact | Retain existing CFG/navigation alternatives |

No new warning may turn an unsafe result into a successful precise result.

## Acceptance tests

The implementation is complete only when these test groups pass:

1. `pascal-project`: path provenance, read policy, package metadata, metadata
   observations, absent candidate invalidation, mapped-root safety, and
   cancellation.
2. `pascal-core`: exact/short/namespace filename tiers, aliases, declared-name
   validation, precedence blocking, ambiguity, package `.dpk` preference,
   package incompleteness, include override/case lookup, overlay precedence,
   payload authorization, limits, observations, and cancellation.
3. `pascal-lsp`: existing navigation, rename, code-action, package, include,
   conditional, overlay, stale-result, and protocol-barrier tests with the
   duplicate resolver removed.
4. `cfg-pascal`: existing 212-test baseline, including project snapshots,
   preparation limits, includes, conditionals, and source maps.
5. `lint4d`: existing workspace baseline of 1,909 passing tests and 5 ignored,
   plus adapter tests for stable IDs, import statuses, qualifier forwarding,
   raw fallback, prepared/source-map handling, and shared-runner parity.
6. CLI/LSP parity fixtures: the same project and source graph must select the
   same unit/package/include target when one uses disk bytes and the other uses
   an unchanged overlay.

## Risks and mitigations

- **Performance regression from duplicate parsing:** keep resolver sessions
  bounded, reuse loaded source objects within a session, and let LSP retain
  parsed navigation documents while the resolver owns only lookup policy.
- **Security regression at a new read seam:** make `SourceRequest` carry the
  exact `ProjectPathEntry`; test configured, mapped, legacy, symlink, and
  stat-only routes before migrating callers.
- **False precision from incomplete metadata:** preserve a distinct
  `Incomplete` state and require explicit status checks in both the LSP and CFG
  adapters.
- **Coordinate mismatch after preparation:** construct `UsesSite` and CFG
  inputs from the same prepared tree; map only through the validated
  `SourceMap`; never mix raw ranges with prepared ranges.
- **Cross-repository dependency drift:** pin `cfg-core`, use repository URLs
  rather than local paths, and regenerate both lockfiles in CI.
- **Behavior drift during LSP migration:** keep the old public LSP entrypoints
  and run the existing LSP suite after each resolver seam is replaced.
