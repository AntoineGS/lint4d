# pascal-project

`pascal-project` provides bounded, filesystem-only Delphi/Object Pascal project
context discovery for callers that need project metadata without invoking the
Delphi compiler or a full MSBuild evaluator.

It resolves `.dproj`, `.dpr`, `.dpk`, and `.optset` metadata, including selected
configuration/platform values, defines, aliases, project references,
unit-search paths, include paths, path provenance, and filesystem freshness
observations. It preserves ambiguity and reports unsupported or unsafe input
conservatively.

This crate does **not** expand or look up Pascal include files, recursively
index a workspace, or implement complete MSBuild evaluation. Existing basic
CLI source discovery and generic MSBuild-facing discovery helpers intentionally
remain in `pascal-core`. The stateful workspace orchestration, overlays,
indexes, and include/rename resolver remain in `pascal-lsp`.

The public APIs accept an explicit [`delphi_overrides::OverrideSession`] when a
caller needs deterministic, environment-independent discovery. Filesystem
payload reads are bounded and use the provenance-aware [`ReadPolicy`].
