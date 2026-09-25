# pascal-project

`pascal-project` provides bounded, filesystem-only Delphi/Object Pascal project
context discovery for callers that need project metadata without invoking the
Delphi compiler or a full MSBuild evaluator.

It resolves `.dproj`, `.dpr`, `.dpk`, and `.optset` metadata, including selected
configuration/platform values, defines, aliases, project references,
unit-search paths, include paths, path provenance, and filesystem freshness
observations. It preserves ambiguity and reports unsupported or unsafe input
conservatively.

Callers normally use `ProjectContext::discover` (or
`discover_with_overrides`) with `ProjectOptions`. The context exposes
`path_entry_for`, `include_search_entries`, and `selected_project_options` for
downstream resolvers. Automatic selection examines directory entries in
ancestors up to the supplied workspace boundary; it does not recursively scan
the workspace, and ambiguous or incomplete selection remains marked
`discovery_complete = false`.

This crate does **not** expand or look up Pascal include files, recursively
index a workspace, or implement complete MSBuild evaluation. Existing basic
CLI source discovery and generic MSBuild-facing discovery helpers intentionally
remain in `pascal-core`. The stateful workspace orchestration, overlays,
indexes, and include/rename resolver remain in `pascal-lsp`.

The public APIs accept an explicit [`delphi_overrides::OverrideSession`] when a
caller needs deterministic, environment-independent discovery. Filesystem
payload reads are bounded and use the provenance-aware [`ReadPolicy`].

Project imports support deterministic literal or property-expanded `.optset`
and `.props` files, including nested relative imports. The evaluator handles
ordered property assignments and boolean/comparison conditions; it does not
execute targets or expand wildcard imports. Unsupported functions, unknown
conditions, and unauthorized imports are not treated as proven false.

Project discovery loads the nearest `.delphilsp.json` (version `1`) from the
source directory up to the supplied workspace root. Supported properties are
`project`, `sourcePaths`, `defines`, and `buildConfiguration`; paths are
relative to the config file and reject absolute paths and `..`. Client project
selection and explicitly supplied client facts/configuration take precedence.
Unknown fields, malformed or oversized files, path escapes, and symlinked
config files fail closed. The config payload and nearest absent config
candidates are retained for freshness and invalidation.

For the resolver, CFG adapter, CLI, and LSP ownership boundary, see the
[shared resolver integration guide](../../docs/shared-resolver-architecture.md).
