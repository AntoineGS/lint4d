# Shared resolver integration architecture

This is the current integration boundary between `pascal-project`,
`pascal-core`, `cfg-pascal`, lint4d, and pascal-lsp. It describes the APIs in
the `shared-resolver` worktree. It is not a promise of full Delphi compiler or
MSBuild fidelity, and the Git revisions below are pins rather than crates.io
release claims.

For user-facing behavior, see the [pascal-lsp README](../crates/pascal-lsp/README.md)
and the [pascal-project README](../crates/pascal-project/README.md).

## Ownership and data flow

```text
ProjectOptions
    -> pascal-project::ProjectContext
    -> pascal-core::resolver::UnitResolver<S>
    -> ResolvedProject + ResolutionReport
       |                         |
       |                         +-- pascal-lsp keeps overlays, snapshots,
       |                             generations, indexes, and stale-result policy
       +-- lint4d cfg adapter -> cfg_pascal::ProjectSnapshot -> lint runner
```

### `pascal-project`

`ProjectOptions` contains `project_file`, `build_config`, `platform`, and
ordered `source_paths`. `ProjectContext::discover` and
`ProjectContext::discover_with_overrides` produce an immutable, requester-scoped
context containing project metadata, defines, aliases, unit/include search
paths, package names, path provenance, read policy, warnings, and freshness
observations. The resolver-facing helpers are:

- `path_entry_for(&Path) -> Option<ProjectPathEntry>` for exact/project/search
  provenance and authorization;
- `include_search_entries(&Path) -> Vec<ProjectPathEntry>` for owner directory,
  include paths, then unit paths, with path-equivalent duplicates removed; and
- `selected_project_options() -> ProjectOptions` for the selected configuration
  and platform.

Discovery examines bounded metadata (`.dproj`, `.dpr`, `.dpk`, and supported
`.optset` inputs) and directory entries in ancestors up to the workspace
boundary. It prefers `.dproj` candidates, then a `.dpr`/`.dpk` fallback;
multiple candidates are ownership-probed within fixed limits. Ambiguous,
unreadable, unsafe, unsupported, or otherwise incomplete selection is retained
with warnings and `discovery_complete = false` rather than guessed.

This crate does not recursively index a workspace, expand Pascal includes, or
run a complete MSBuild evaluator. The existing callers still decide how much
state to retain and when to rediscover it.

### `pascal-core::resolver`

The resolver module is public and is also re-exported from `pascal_core`. The
main entry point is the request-scoped generic
`UnitResolver<S>::new(context, workspace_roots, source_store, limits)`. Callers
provide the `SourceStore`, `CancellationToken`, `ResolverLimits`, project
selection, and workspace state; the resolver owns only request-local caches,
lookup policy, observations, and bounded graph work.

The public session operations are `resolve_unit`, `resolve_imports`,
`resolve_include`, `resolve_project`, `resolve_project_from_source`, and `finish`.
CLI and LSP analysis use `resolve_project_from_source` to retain the authorized,
caller-selected full path, including `.dpr` roots. Aliases and import precedence
apply to dependencies, not to replacing the selected document.
A `ResolvedProject` retains
the root, loaded units, import/include occurrences, non-unit `include_sources`,
completion status, and a `ResolutionReport`. A result is represented as
`Found`, `Unavailable`, `Ambiguous`, or `Incomplete`; only `Found` is a precise
binding.

Unit lookup checks explicit project entries, the importer directory, and the
ordered search paths. Candidates are loaded and their declared unit names are
validated; ambiguity blocks a lower-precedence guess. Projectless filename
catalogues and named package descriptors are separately bounded. Include lookup
uses the including directory, `DCC_IncludePath`, then unit paths, with path
mapping, case adjustment, provenance, and read-policy checks.

`SourceStore` is deliberately caller supplied. `FilesystemSourceStore` provides
the normal disk implementation and explicit `OverlaySource` entries; the LSP
uses its own `LspSourceStore` adapter to add workspace overlays and document
policy without moving workspace ownership into `pascal-core`.

## Conditional and include behavior

Parsing, directive ranges, CFG preparation, and diagnostic positions use decoded
UTF-8 analysis bytes. Disk sources retain their original payload for byte limits,
hashes, and freshness checks; non-UTF-8 input follows the existing Latin-1
decoding contract. An unchanged decoded editor overlay therefore shares the
same analysis coordinates as its disk source.

`pascal_core::conditional::analyze_with_cancel` returns an offset-preserving
`ConditionalAnalysis` with `projected_source`, inactive/unknown spans,
directive metadata, and `complete`. The bounded analyzer understands `IFDEF`,
`IFNDEF`, boolean `IF`/`ELSEIF`/`ELIF`, `ELSE`, `ENDIF`/`IFEND`, `DEFINE`,
`UNDEF`, and `I`/`INCLUDE` directives in the supported Pascal directive comment
forms. Expressions cover defined/true/false values, `NOT`/`AND`/`OR`,
parentheses, and bounded numeric/string/boolean comparisons. `IFOPT` is
recognized but remains unknown because compiler-option facts are not inferred.

Known-inactive branches can be excluded. Unknown activity remains potentially
active. Malformed nesting, unsupported active directives, cancellation, encoding
coordinate mismatch, and exhausted bounds mark analysis incomplete; they never
become an apparently complete empty projection.

An include mapping failure is retained as a resolver warning such as
`include <name> could not be mapped: ...`. If no precise candidate survives,
the lookup is `Incomplete`, not a silent `Unavailable` success. Active missing,
ambiguous, unauthorized, cyclic, or limit-exhausted includes likewise prevent a
`ResolvedProject` from claiming completeness. Known-inactive includes may be
retained as unavailable without requiring a payload. Repeated and nested include
occurrences keep their exact owner source ID and directive range, and successful
non-unit payloads remain in `ResolvedProject::include_sources`.

The conservative resolver does not propagate symbols defined inside includes
back into the parent's initial conditional analysis. Such patterns can remain
incomplete even when the lower-level configured preparer could evaluate them.
The shared lint pipeline then uses its file-local fallback; it does not claim
full compiler preprocessing or MSBuild fidelity.

The default shared-resolver bounds are:

- 256 dependency units; 1,048,576 directory entries and 8 GiB of scanned
  bytes; 16 MiB per source;
- 4,096 include files, 256 MiB of include bytes, 16,384 include directives,
  and depth 256; and
- 256 package lookups, 1,024 package-unit candidates, 524,288 catalogue
  entries, 64 catalogues, and 256 retained warnings.

The conditional analyzer additionally bounds one walk at 16,384 directives,
conditional depth 256, 4 KiB per expression, 256 expression tokens, 32,768
environment entries/1 MiB of key storage, 1,000,000 environment operations,
and 16 MiB of environment copy/merge work. `cfg-pascal`'s separate
`PreparationLimits::default()` bounds configured preparation at 16 MiB input,
64 MiB output, 10,000 expanded occurrences, include depth 64, conditional depth
256, 100,000 directives, 1 MiB of expression bytes, 1,000,000 expression
tokens, 100,000,000 work units, and expression depth 128.

## lint4d CFG adapter and fallback

`crates/lint4d/src/cfg/project_snapshot.rs` exposes the adapter API
`to_cfg_project_snapshot(ResolvedProject, CfgSnapshotOptions)`, returning a
`CfgProjectSnapshot` or `CfgSnapshotError`. It copies the resolver's explicit
source identities and import decisions into the caller-owned
`cfg_pascal::ProjectSnapshot`; it does not rediscover files or infer imports.

With `CfgSnapshotOptions::prepare_configured_sources = true`, the adapter passes
the selected configuration ID, project defines, and
`cfg_pascal::PreparationLimits` to `cfg_pascal::prepare_source`. Preparation is
all-or-nothing. Missing configuration identity, unknown active directives,
unresolved/ambiguous active includes, or source-map validation failures retain
the raw snapshot with `CfgSnapshotStatus::Incomplete` rather than mixing precise
and partial prepared units. Only found resolver includes become
`cfg_pascal::IncludeBinding` values.

`run_lint_with_cfg_project` uses a project snapshot only when it is complete and
its target path and original bytes exactly match the lint request. An incomplete
snapshot follows the established file-local CFG path. Resolver warnings and
incomplete reasons are emitted by the CLI; a structural adapter error is a
bounded warning followed by the same file-local fallback at the CLI boundary.
Prepared rule diagnostics are mapped through `cfg_pascal::SourceMap` only when a
single copied span maps back to the target source. A range crossing an include
boundary, touching synthetic/masked bytes, or belonging to another source is
not guessed; the runner reruns the file-local path instead of publishing a
wrong source location.

## CLI project flags

`lint4d --project <file.dproj>` gets the lint file list from the `.dproj`
`DCCReference` entries and enables the source-project path above. The source
context uses `ProjectContext::discover`; each worker creates its own resolver
and filesystem store, so mutable resolver state is not shared across the
parallel lint tasks.

- `--platform <NAME>` and `--build-config <NAME>` override the corresponding
  project/config values for source discovery and DCU auto-discovery. Without
  `--project`, lint4d warns that these flags have no effect.
- `--dcu-path <DIR>` is repeatable and has priority over `.lint4d.toml`
  `dcu_paths`, which has priority over MSBuild/BDS DCU auto-discovery.
- `--bds-path <DIR>` only overrides the RAD Studio root used by DCU
  auto-discovery. It is not a source resolver or path-mapping option.

Source-project discovery failures, unresolved project metadata, and incomplete
resolver graphs are warnings; they do not turn an active include or import into
a precise binding and do not stop the file-local lint pass.

## LSP overlays, invalidation, and diagnostics

The LSP adapter constructs a request-local resolver from the selected
`ProjectContext`, workspace roots, `WorkspaceInput`, and resolver limits. An open
document's overlay is authoritative over disk (including when the backing file
is deleted), but it must still satisfy the requester-scoped read policy and
source-size limit. Overlay and disk content at the same absolute lexical path
share a deterministic `source:<path>` source ID; `SourceRevision::Overlay`
retains the document version and content hash, while disk revisions retain the
stamp, hash, read policy, and path provenance.

Worker snapshots retain the resolver's metadata, directory, candidate, payload,
include-directory, and project-read observations. Delivery is revalidated
against those observations, the open-document version, and the workspace's
source/configuration generations. A source edit, overlay version change, disk
payload change, candidate create/delete/rename, relevant project/option-set
change, mapping change, sidecar change, or project selection change invalidates
the affected result. Project selection bumps both generations, clears indexes
and cached documents, and reschedules open-document diagnostics. Stale
navigation/formatting results are rejected for retry; stale or cancelled
diagnostics are discarded rather than published.

The include auditor uses the shared conditional analyzer and resolver. A known-
inactive unresolved include can be skipped, but an active or unknown include
that is unmappable, unavailable, ambiguous, unauthorized, cyclic, malformed,
or over budget is retained in `include_errors`/the incomplete reason. Rename,
reference, highlight, and name-free assistance requests then return an
actionable incomplete result (for example, `rename cannot prove completeness
because include ... could not be read: ...`) instead of assuming the include is
empty. Semantic-token requests may retain lexical tokens while suppressing
semantic classifications when include resolution is uncertain.

## Dependency identity

The current lint4d `Cargo.toml` pins cfg-pascal to Git revision
`208d6743c61e0b391195958270a5e04e3a4328d4`. `cfg-pascal` publicly re-exports
its cfg-core dependency as `cfg_pascal::cfg_core`; lint4d intentionally has no
direct `cfg-core` dependency and imports those types through that re-export.
The locked cfg-core source is revision
`b4131e61a939e0685194712689d1fd1497a41492`, and
`cargo tree --locked -p lint4d -i cfg-core` reports one cfg-core crate identity
through cfg-pascal. Keeping the re-export and lockfile identity aligned avoids
duplicate cfg-core types at the adapter boundary.

The cfg-pascal revision is an upstream Git pin for this integration; it should
not be read as a claim that the lint4d adapter, or these worktree changes, are
already part of a released upstream package.
