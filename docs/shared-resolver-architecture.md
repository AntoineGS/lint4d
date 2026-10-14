# Shared resolver integration architecture

This is the current integration boundary between `pascal-project`,
`pascal-core`, `cfg-pascal`, lint4d, and pascal-lsp. It describes the APIs in
this workspace. It is not a promise of full Delphi compiler or MSBuild fidelity
or a crates.io release claim.

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
`.optset`/`.props` inputs) and directory entries in ancestors up to the workspace
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
parentheses, and bounded numeric/string/boolean comparisons. `IFOPT` uses
explicit named compiler-option facts; absent facts remain unknown because
compiler-option state is not inferred from the host.

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

The resolver's compile-time session now continues active include declarations,
supported initializer values, lexer state, scopes, branch correlations and
layout switches into the parent in source order. It does not require a second
import pass to see a supported included `SizeOf` constant. Physical occurrence
and definition identities are retained across nested/repeated includes and
split tokens, operands and initializers. Unknown include activity is not read
authority. A missing/ambiguous/denied/cyclic/over-budget include is a required
failure, never an empty fragment or an initializer value of one. Scalar-only
legacy callbacks cannot claim typed include continuation. Unsupported cases
remain incomplete and keep the established file-local lint fallback; this is
not full compiler preprocessing or MSBuild fidelity.

## Compile-time storage facts

`pascal-core::compile_time` owns declaration ingestion, binding, layout and
source-order sessions. `pascal-project` owns target/settings metadata, not a
second layout engine. `Fact::Known` requires proven storage and alignment;
`Unknown` preserves unsupported/ambiguous/cyclic/overflow facts, and
`Incomplete` preserves cancellation, required-source, malformed-source or
limit failures. Declaration identity is physical source + include occurrence +
scope, with sequence-sensitive queries and definition-site settings. Local
shadowing blocks intrinsic/provider guessing; same-width candidates do not
establish unique identity.

Supported operands comprise intrinsic scalar/pointer/string/reference types,
ordinary and strong aliases, source-backed imported/qualified types, ordinal
enums/subranges, static multidimensional arrays and dynamic-array references,
nested/packed records with final padding and proven variant parts, source
global/local variables and parameters, record fields, typed array selectors
and pointer dereferences. Index expressions prove static ordinal category;
they do not evaluate runtime values or dereference memory. Open-array parameters
are not dynamic-array variables. Routine/nested scopes, declaration order,
interface/implementation visibility, qualified unit/project aliases and lexical
intrinsic shadows remain binding constraints. An unsupported declaration does
not authorize a built-in fallback.

Windows ABI profiles admit Delphi 2007/18.5 Win32 and exact 20.0–37.0 versions,
with Win64 from 23.0; widths and inherent alignments are reference-derived,
not local compiler measurements. The complete
[ABI registry](../crates/pascal-core/tests/fixtures/compile_time/abi.toml)
contains official Delphi data-format/switch/Extended references and the
Delphi/MSVC correspondence plus Microsoft's mapped alignment rules. Host
`sizeof`/alignment and mutable conditional defines are never ABI evidence.
Pointers and managed-reference variables are 4/8 bytes on Win32/Win64, not
their payloads. `Extended` is 10/8 bytes with inherent alignment 8; Win64
`Real48` remains unknown because alignment is unverified. Only Athens/36.0
has referenced A8/H+ defaults; enum size, OLDTYPELAYOUT and REALCOMPATIBILITY
defaults remain unproven, including Florence/37.0 defaults. Project DCC
alignment/enum/long-string settings apply only to project sources; library
sources and explicit whole-layout overrides retain their independent state.
Explicit A1/2/4/8, Z1/2/4, H and supported legacy switches are captured at
definition sites. No-argument 37.0 Windows PUSHOPT/POPOPT preserves their state;
unsupported/subset/malformed restores are not proof.

Generic/DCU/function/property storage operands, class/interface instances,
sets, Variant, unverified grouped-field legacy layouts and ALIGN 16 remain
unknown. Variant alternatives also require proven unmanaged eligibility;
managed data is not accepted just because its reference has a width. Supported
`SizeOf` constants use the existing bounded expression grammar, including
source-backed whitespace/plain-comment trivia. This is not a general evaluator
(TASK-47), an exhaustive Delphi type checker or a compiler-parity promise.

Unit candidate verification and LSP `uses` definition fallback share a bounded
source-backed header proof. The proof can stop at an authorized, active,
uniquely matching unit header before an unsupported body; it returns physical
name fragments/URI/ranges, including include-owned spans. It does not promote
that body to complete analysis, fabricate a DCU layout, authorize a rename or
change search precedence. Missing/ambiguous/denied providers stay fail-closed.
Payload revisions, negative directory/candidate probes, read policy, physical
root/route, project/library contexts and all layout settings remain freshness
inputs. Revoked routes cannot be replayed from an older session.

Regression evidence lives in `pascal-core/tests/compile_time_{abi,bindings,
arrays,records,operands,conditionals,includes,providers,headers,admission,
ownership,task13_bounds}.rs`, `pascal-core/tests/unit_resolver.rs`,
`pascal-project/tests/layout_context.rs`, `pascal-lsp/tests/compile_time_layout.rs`
and `lint4d/tests/cfg_compile_time_layout.rs`. These cover positive storage,
shadowing/ambiguity/authorization controls, physical coordinates, include
continuation, checked overflow, budgets, cancellation and cold/warm freshness.
Release validation and the actual unchanged-config ChainDriveAPI 13-import
probe remain pending; fixture evidence does not establish those results.

## Compile-time resource boundary

The ordinary source/include/depth/retention defaults remain unchanged. Compile-time
declaration, provider, include, branch, query, copy and recovery work uses a
shared request budget; live admitted owners retain storage receipts until their
actual storage dies. Releasing live scratch does not refund work or byte-work.
Admission refusal is transactional, cancellation cannot publish partial known
facts, and checked layout arithmetic cannot wrap to a known size.

| Core compile-time default | Value |
| --- | ---: |
| Definitions / fields / variant alternatives (each) | 32,768 |
| Pending fragment bytes / retained storage bytes (each) | 1 MiB |
| Scope / type depth / array dimensions (each) | 256 |
| Branch states | 64 |
| Provider queries | 256 |
| Work units | 2,000,000 |
| Byte-work | 64 MiB |

These are refusal ceilings, not promises that every input at the dimensional
maximum fits. The finite work profile has measured 1.681x operation and 1.704x
byte headroom over bounded SysUtils/base outputs, which still report a genuine
64-state branch Limit; neither resource-free processing nor all-Known semantic
facts is promised. Header-purpose selection/reproof retain 256 states /64 MiB
and inherit the common 2,000,000-operation policy. Explicit tighter callers and
spent shared counters are never replaced or reset. Isolated Task13 probes safely
refuse default include chains
before depth 256 at retention limits, and 10,000 simple declarations also
refuse under unchanged defaults. Neither is maximum-depth/count success.

The LSP deliberately retains the existing finite compatibility policy in
`workspace/compile_time.rs::lsp_compatibility_retention`: the sum of document,
total-source, expanded-output and cache byte ceilings, each clamped to existing
runtime maxima (document/expanded also clamped to total source). This avoids
applying the standalone 1 MiB fuse to LSP-owned source/output domains; it is
**not strict aggregate heap certification**. Universal resolver/LSP allocation
accounting, native parser storage, native cwd/macOS cache migration and sparse
receiving APIs remain deferred. Two strict historical projection-retention
tests remain visibly failing under `l4d-cnm`:
`fix_i3_versions_replace_temporary_projection_and_share_work` and
`fix_i3_repeated_project_outputs_are_admitted_not_free_replays` in
`compile_time_includes.rs`. No process-heap/RSS bound or all-tests-green claim
follows from the finite resource policy.

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
each unit's resolver-retained conditional context through the shared
compile-time session over already resolved sources. It then passes proven
offset-preserving projections, exact active include decisions, the selected
configuration ID and `cfg_pascal::PreparationLimits` to the CFG preparer;
there is no duplicate `SizeOf` evaluator or new filesystem lookup. The strict
CFG splicer rejects repeated physical sources with multiple entry occurrences
and fragmented include directive spans rather than reusing another lexical
context. Preparation is all-or-nothing. Missing configuration identity, unknown active directives,
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

Workers and `PascalLspWarmer` share a project cache (`project_cache.rs`)
of parsed units and import resolutions keyed by URI and project-context
fingerprint. Cache hits verify stat/content-hash probes and replay observations
before delivery. Directory watches (`file_watch.rs`) and authoritative client
file events invalidate affected entries; stores from snapshots preceding an
invalidation are refused. The warmer caches each opened file and its direct
imports, yields to interactive work without holding a cache claim, and keeps
per-file `Indexing <file stem>` progress tokens (`<done>/<total> units`).
`maxCacheBytes` (default 2 GiB) is the eviction budget for unpinned entries;
pinned entries (open files and their direct imports) are not evicted and may
exceed it. If they do, warming pauses and one warning is logged.

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

cfg-core and cfg-pascal are workspace crates (`crates/cfg-core`,
`crates/cfg-pascal`) reached by path dependency. `cfg-pascal` publicly
re-exports its cfg-core dependency as `cfg_pascal::cfg_core`; lint4d
intentionally has no direct `cfg-core` dependency and imports those types
through that re-export. `cargo tree --locked -p lint4d -i cfg-core` reports one
cfg-core crate identity through cfg-pascal. Keeping the re-export as the only
route avoids duplicate cfg-core types at the adapter boundary.
