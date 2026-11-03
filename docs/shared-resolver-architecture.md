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

### Persistent cache memory

The persistent analysis cache (`pascal-cache`) does not add a second memory
budget: what it retains (decoded facts, scratch, page caches) is reserved from
`maxCacheBytes` before it is allocated (`ProjectCache::reserve`). These are
building blocks: nothing in production opens a store, reuses facts or
publishes until `l4d-zf1b.8` adds the first caller. Three kinds of
reservation are reported with the cache statistics:

- **Facts**: decoded persisted facts. Reuse reserves its decode limit before
  decoding, keeps what decoding used and holds that charge for as long as the
  facts live, also after eviction.
- **Scratch**: the store's transient encode/decode buffers (16 MiB at most,
  `persistentCacheScratchBytes` once `l4d-zf1b.11` adds the option).
  `AdmittedStore` reserves this budget from `maxCacheBytes` up front, not
  from `ProjectCache` per use. Inside the store, record and evidence
  encodings and query result pages reserve from that scratch budget first,
  from an upper bound computed from their inputs.
- **Pages**: the SQLite page caches (16 MiB aggregate) plus a fixed overhead
  allowance of 1 MiB per connection (2 readers and 1 writer). The store is
  to be opened only through `AdmittedStore`, which reserves Pages and Scratch
  together first and bypasses persistence, evicting nothing, when they do not
  both fit. Both rules are documented, not enforced: `Store::open` stays
  public, and one admitted store per process is expected.

Reservations count in eviction like retained entries and leased outstanding
parses. Pinned entries are never evicted for a reservation. Lowering
`maxCacheBytes` at runtime keeps held reservations charged and refuses new
ones until they fit.

Not measured or not accounted (as of `l4d-zf1b.7`):

- **Process RSS on the large repository** (`l4d-10`): the 48 bytes per
  source byte retained-parse factor is calibrated by an ignored RSS test on
  RAD Studio 7.0 units; the 24k-file repository measurement with
  `maxCacheBytes` = 512 MiB was never made, and tree-sitter's C allocations
  are invisible to any in-process count.
- **Main-index retention** (`l4d-131`): the long-lived workspace index is
  admitted by source bytes (`maxTotalBytes`), not charged to `maxCacheBytes`.
- **SQLite statement heap beyond the per-connection allowance**: the
  allowance was checked once against `sqlite3_memory_used` on a fresh store
  with every connection opened and used (`pascal-cache`
  `tests/sqlite_heap.rs`, measured: `sqlite3_memory_used` rose by 598,008
  bytes, stable over 5 runs, against the 3 MiB allowance of 3 × 1 MiB; this
  is that test's measurement, not an RSS figure); large sorts or unusual
  plans can exceed it and are not counted.
- **WAL shared memory and OS page cache**: the `-shm` mapping and the
  kernel's file cache are outside the Rust and SQLite heaps and are not
  charged.
- **Transient validation reads**: root bytes and evidence payloads read while
  validating persisted facts are bounded by the file size limit and the
  request work budget, not reserved from `maxCacheBytes`.

The include auditor uses the shared conditional analyzer and resolver. A known-
inactive unresolved include can be skipped, but an active or unknown include
that is unmappable, unavailable, ambiguous, unauthorized, cyclic, malformed,
or over budget is retained in `include_errors`/the incomplete reason. Rename,
reference, highlight, and name-free assistance requests then return an
actionable incomplete result (for example, `rename cannot prove completeness
because include ... could not be read: ...`) instead of assuming the include is
empty. Semantic-token requests may retain lexical tokens while suppressing
semantic classifications when include resolution is uncertain.

## Workspace index (l4d-33)

Non-local `textDocument/references` and `workspace/symbol` used to build a
whole-workspace snapshot per request: walk every source root, resolve an owner
context per file, read and filter every file. The workspace index
(`workspace_index.rs`, `workspace/indexing.rs`) replaces that walk with
per-context facts that survive the request, so both answer from candidates and
report what they could not search.

### What it holds

The index is built from compact per-document facts
(`pascal_core::facts::ArtifactFacts`), never from retained syntax trees. A
document's facts enter through one function, `MemberFacts::from_facts`: unit
names (declared unit and file stem, canonical), imports by name, identifier
postings of every activity, declarations (kind, shape, unit scope, owner,
location as include slot plus range) and the include slot paths. The
declaration facts carry `extent`, `shape`, `unit_scope` and `owner`, so symbols
can be served without a parse. Fresh extraction is the only caller today;
persisted reuse (`l4d-zf1b.8`) will call the same function.

One `ContextIndex` exists per `ContextKey` (at most 8, least recently used
evicted, `MAX_INDEXED_CONTEXTS`). It keeps the members, an interner of names,
inverted lists (postings, importers, declared units), the include-dependents
map, the membership scopes with their catalogue generations, a generation
counter and the last 4,096 `(generation, path)` changes. Disk facts only:
open documents are never indexed and edits never mutate the index. An open
document is always a candidate and is answered from its live parse.

### Membership

Membership is computed per context from `l4d-20` directory catalogues, not from
a walk.

- **Project contexts**: the main source, every explicit unit and the `.pas`
  files of the `Listing` catalogues of the main-source directory, each
  search-path directory and each explicit-unit directory. A path is accepted
  when `(accepts_path && is_safe_source_path) || mapped_path_is_readable`, so
  mapped library sources stay in or out exactly as today's mapped roots do.
  Units provided only through runtime packages are not members.
- **Projectless contexts**: the `.pas` files of the recursive workspace-root and
  search-path catalogues whose discovered owner is that context. A file owned by
  a project is a member of that project only.
- **Cap**: `MAX_WORKSPACE_UNITS = 100,000` members per context, truncated in
  path order.

Owner discovery reuses a sibling directory's owner only when its context is
retained and fresh, memoized per key per request. A membership (computed by
the warmer, or for a searched target of a query) discovers at most 256
directories per call (`MAX_REQUEST_OWNER_DISCOVERY`); the rest stay pending and
the membership is marked incomplete. An interactive request discovers at most
32 directory owners for its outside sources (`MAX_INTERACTIVE_OWNER_DISCOVERY`);
the rest are the outside-source gap, and the owners it found are remembered for
the next request. A `.dpr`/`.dpk` descriptor with a
same-stem `.dproj` in its directory is skipped as a symbol target, since the
`.dproj` is its project.

Targets of one request, in order and without duplicates: the requesting
document's owner, the owners of open documents (most recent first), the
configured project, for symbols the contexts of project descriptors under the
workspace roots, then the owners of workspace sources no target holds. At most
16 targets (`MAX_TARGET_CONTEXTS`); descriptor resolution attempts are capped at
twice that. Indexed requests do not walk mapped roots: units reachable only
through runtime-package mappings are a coverage gap, not searched.

Only the first three kinds (requesting owner, open documents' owners,
configured project) are searched in full: their membership is computed when it
is not current, their unverified members are read within the bound, and their
index is requested from the warmer. Descriptor and outside-source targets
contribute only the members an existing, current index already holds `Fresh`:
a descriptor or owner without an index is not a target at all (its sources are
the outside-source gap and it is never resolved or requested), a stored index
whose membership is no longer current is the gap "`<context>` is not indexed
yet; it was not searched", and unindexed or stale members are the gap "`<n>`
units of `<context>` are not indexed yet; 0 were searched directly". A query
therefore never computes such a membership nor stores such an index, so the
contexts the warmer builds for the searched targets are not evicted by them.
On a small workspace (A16) every target is searched, as before. On multidev
this took `indexed_reference_scope` from 16.7–18.7 s (the memberships of 15
outside owners, four projectless ones walking the 20,893-file root catalogue
with up to 256 owner discoveries each, plus the requesting project's
membership again because it had been evicted) to 0.35–1.0 s.

A context with a project file is searched under the key its `Project` anchor
resolves to, once per request: the key the warmer indexes. A document owner can
hold an older resolution of the same project (its descriptor changed since the
owner was recorded), so its own key is not used as the target. A target whose
context is not retained is restored from the request's document owners when one
holds that key, otherwise resolved again from its anchor; when the anchor now
names another context the target is a coverage gap, never a request error.

### Candidate rules and soundness

References resolve the binding first through the bounded `LocalWithImports`
snapshot and read its members. If every member is top-level (unit scope, no
owner type, not an enum value, field or property), the top-level rule applies,
otherwise the member rule:

- **Top-level**: the declaring unit plus every member that imports it and has an
  identifier posting for one of the binding's names.
- **Member**: every member of every target context with a posting for the name.
- Postings are matched on the last dotted segment of each name after
  `canonical_name`. A non-ASCII name, or an empty one (or one ending in `.`),
  proves nothing and skips the filter.

A member is an importer of unit `D` when an import name, or an alias target,
matches `import_names_for(D)` exactly, or when its last dotted segment equals
the last segment of a name in that set. The widening follows the resolver's
short-name tier, which binds `uses Vendor.Foo` to `Foo.pas`. Every member
imports the implicit units `system` and `sysinit`. Ambiguous or same-named
imports still make a member a candidate, so ambiguity is conservative without
resolving anything.

Soundness argument. Naming a unit-scope identifier of `D` from unit `U`
requires `D` in `U`'s scope: `U` is `D`, or `U` imports a name that resolves to
`D`. Pascal has no re-export, so direct importers suffice. Every spelling the
resolver can map to `D` (exact, namespace-prefixed, aliased, short-name) is
covered by the widened name set, and the reference text contains the binding
name's last segment as an identifier run, which is a posting when the name is
ASCII. Conditional references are postings of another activity, and every
activity counts. A type member can be reached through inheritance, `with`,
helpers or another unit's declarations without importing the declaring unit, so
the member rule uses postings alone. Nothing is pruned without proof: anything
the rules cannot decide is a candidate, anything outside the searched contexts
is reported.

Today's binding finds no member reference from a unit that does not import the
declaring unit; the member rule is therefore a superset of what binding can
return. Cross-unit member resolution through types reached via non-imported
units is not implemented in either path.

### Freshness

Facts are trusted only for members whose stamps still prove them unchanged. A
member has one stamp for its own path and one per expanded include slot, taken
before the bytes were read; include stamps come from the load's own read
observations. Every request stats the excluded members' stamps (and the
contributing members' for symbols); a mismatch makes the member unverified for
this request and marks it stale for the warmer. A member with an unresolved
include slot but unchanged stamps is unverified on every request without being
marked stale, so a created include file is noticed. The excluded members'
paths and stamps, and the membership catalogue directories (from the catalogue
generation the membership recorded), join the snapshot baseline, so a result is
rejected when one of them changed.

Unindexed, stale and unverified members of a searched target are candidates:
the snapshot reads and filters them as before, in path order, at most
`MAX_UNVERIFIED_CANDIDATES = 2,048` per references request and
`MAX_UNVERIFIED_SYMBOL_CANDIDATES = 256` per `workspace/symbol` request (an
interactive bound: on multidev 2,050 direct reads took about 65 s, 258 about
10 s). Members refused for a stable reason (over the size limit,
unreadable, an include expansion over a compile-time limit, which a direct
read in a request fails the same way) are reported, not searched. Transient
failures leave a member pending, and one member's failure never stops a batch:
it is counted as deferred, goes behind every pending member no batch has
deferred (longest deferred first), and the batch goes on. The context keeps
the latest deferral reason for the coverage. On multidev (wave 3) the
expansion limit failures were classed as transient: 36 Woll2Woll and RTL
sources over `max_byte_work`, then the `Common/BOBase` units over `max_work`,
sat at the head of the path order, every batch retried them first, and after
about 700 members a batch of 64 deferrals stopped the index at 6,087 pending.
With them refused, the context (about 6,800 members) completes in about
330 s. A member whose session was limited (a
compile-time limit or an incomplete include lookup, `NotPersistable::Transient`)
is `Limited`: it keeps no facts, is an unverified candidate of every request
and is not indexed again until it changes. Members of other targets outside
the workspace roots that the indexed enumeration may not bind are filtered
before that bound and reported.

Invalidation: a changed member path goes `Stale` (from `Fresh`, `Refused` or
`Limited`); a changed include marks its dependents `Stale`; a listing change
dirties the membership scopes that listed the directory (or that list the
deleted directory itself) and the next batch or query reconciles them; overflow
stales every member, dirties every membership and clears the owner map.

### Memory

Index bytes are charged to `maxCacheBytes` through a
`ReservationKind::WorkspaceIndex` bucket held by the store (no `Reservation`
value, to avoid a cycle with the cache), capped at a quarter of
`maxCacheBytes` and reported as `reserved_index_bytes`. The charge of a
publish is computed under the index lock before inserting
(`ContextIndex::charge_for`) and checked again under the lock that inserts;
names interned in between only make it an overcount. It has two parts:

- **The member** (`ContextIndex::member_charge`): its facts without the names
  (unit display, declarations, slot paths), its stamps, its include keys, and
  each list entry twice (the inverted list, with room to grow, and the
  per-member id list kept for removal). Once inserted, a member keeps only the
  ids of its names. Released when the member is refused, limited, replaced or
  removed.
- **The names**, once per context: each name new to the interner (its length
  rounded to 16 plus 80 bytes) and each new key of an inverted list (96 bytes).
  A list that empties keeps its key and gives back its room, so reindexing a
  member never charges its names twice. Released when the interner is
  compacted (the context's membership empties) or the context is evicted.

`ContextIndex::bytes()` is the sum of both. The membership charge per path is
`3 × len + size_of::<Member>() + 64`. When a publish cannot be charged the least
recently used other index is evicted once; if it still fails the member stays
unindexed and the coverage says so. When a recomputed membership cannot be
charged, a stored context keeps the membership and members it holds, is marked
memory-exhausted and incomplete, and is computed again on the next look; a new
context is stored empty with the same gap. Two contexts sharing a document each
charge their own copy: an overcount, never an undercount.

The test `index_bytes_per_member_stay_within_the_estimate` builds 1,000 members
with 300 postings, 20 imports and 40 declarations each. With 30 postings per
member distinct and the rest drawn from a shared vocabulary of 2,000 names it
measures 17,100 bytes per member; with every posting distinct (the worst case,
each member paying for all its names) 72,844 bytes, asserted at most 96 KiB.
The per-member model it replaced measured 28,406 and 27,326 bytes.

An index build does not publish parses (`Workspace::index_build`): the batch
view drops its `ProjectCache` claim after extracting facts, so a build does not
evict interactive parses.

### Scheduling

The warmer builds indexes in batches of 64 units (`INDEX_BATCH_UNITS`) only when
its open-file queue is empty, project anchors before document anchors, at most
16 queued anchors. A batch yields before every unit through the interactive
gate, and also when a new crawl is pending (a crawl flag checked between
units); a paused batch is requeued. A batch is requeued when it is not
complete, did not hit the memory limit, and made progress (indexed, refreshed,
refused or limited something) or left pending members that no batch has
deferred and this batch did not try (a member skipped unstamped every batch
would otherwise keep it requeued); a batch that only deferred members already deferred is not, which avoids
livelock. Such an anchor is asked for again by the next request that finds
its index incomplete and by the warm end of its open file (an edit or
invalidation), and then retries the longest deferred members first. Warm `End` of an open file requests its
context. Requests that find an incomplete index of a searched target request
its anchor too; descriptor and outside-source targets are never requested. Each
batch stamps a member before its read, one `before_io` call per member.
Configuration changes produce new `ContextKey`s and so new indexes. Rename,
code actions and references on `.inc` documents keep the whole-workspace walk.

### Coverage messages

Each gap is one coverage note (at most 32 are kept). The wordings emitted:

- `<n> units of <context> are not indexed yet; <m> were searched directly`
- `<n> sources under the workspace roots belong to contexts that were not searched`
- `<n> requested sources belong to no searched context and were not searched`
- `<n> units of <context> outside the workspace roots were not searched`
- `<context> has more than <MAX_WORKSPACE_UNITS> units; the rest were not searched`
- `<context> membership is incomplete; some units were not searched`
- `units provided through runtime packages of <context> were not searched`
- `<n> units could not be indexed: <first reason>`
- `<n> units of <context> failed to load for the index and are retried later: <latest reason>`
- `the workspace index memory share is exhausted`
- `no project context is open; open a source or select a project`
- `<context> changed while the request ran; it was not searched`
- `<context> is not indexed yet; it was not searched`
- `the workspace index is unavailable; only the current context's imports were searched`
- `the workspace index is unavailable; no workspace symbols were searched`

When the index cannot answer (no target resolves, or the targets fail), only a
small workspace (the A16 test below) is walked. Otherwise references answer
from the phase-1 `LocalWithImports` snapshot, the current document's context
and its imports, and `workspace/symbol` answers nothing; each says so with the
corresponding message above (U4).

`workspace/symbol` answers from the Fresh members' declarations of the target
contexts, merged with the unverified members of searched targets (at most
256) and open documents, deduplicated on
`(uri, range, name)`, ordered by URI then start. The 10,000-result error fires
early, on deduplicated facts. A workspace with no target from the first three
target kinds keeps today's snapshot path only when it is small (at most 2,048
sources and 64 descriptors); otherwise the last message above applies. A
declaration in an include slot is reported at the include file; nested routines
and routine-local types are not facts and appear only for open documents.

### Not yet built

- The `maxWorkspaceUnits` client option (the cap is a constant; tests lower it
  through the store).
- Hash-consing members shared by several contexts.
- Compaction of the per-context interner (names of removed members stay, still
  charged, until the context empties).
- A native-watch fast path for verifying excluded members (`l4d-20`
  `SharedCatalogue::watched`); each request stats them today.
- `.inc` references on `ContextIndex::include_dependents` (`l4d-35`).
- Population from persisted facts (`l4d-zf1b.8`).
- Relative `projectFile` anchors.
- Cross-unit member resolution through types reached via non-imported units.
- References on very large workspaces remain subject to the Workspace-mode
  completeness precheck (`l4d-xsvq`).

## Dependency identity

cfg-core and cfg-pascal are workspace crates (`crates/cfg-core`,
`crates/cfg-pascal`) reached by path dependency. `cfg-pascal` publicly
re-exports its cfg-core dependency as `cfg_pascal::cfg_core`; lint4d
intentionally has no direct `cfg-core` dependency and imports those types
through that re-export. `cargo tree --locked -p lint4d -i cfg-core` reports one
cfg-core crate identity through cfg-pascal. Keeping the re-export as the only
route avoids duplicate cfg-core types at the adapter boundary.
