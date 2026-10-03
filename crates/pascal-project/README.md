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

## Delphi installation profiles

Project discovery can combine project metadata with locally installed Delphi
metadata without launching MSBuild, `rsvars.bat`, or the Delphi compiler. The
configuration files are read as data; no process environment is modified.
Installation profiles are opt-in and do not depend on Multidev Makefiles or
`delphi-config` files.

### Configuration locations and merge priority

The loader captures these configuration layers in order:

1. `$XDG_CONFIG_HOME/delphi-tools/config.toml` (or the `$HOME/.config` fallback; `%USERPROFILE%` stands in for an unset `HOME` on Windows).
2. `<workspace-root>/.delphi-tools.local.toml`.
3. `<selected-project-directory>/.delphi-tools.local.toml`.

Each layer may contain shared `[properties]` and `[[path_mappings]]` and
installation-specific `[installations."<id>".properties]` and
`[[installations."<id>".path_mappings]]`. Values merge by key; within each
layer shared values are applied before the selected profile. Thus a project
layer's shared value can override a user-layer profile value. Property names
and mapping prefixes are case-insensitive; explicit mappings use longest-prefix
matching.

```toml
[properties]
MULTIDEV = '/work/Delphi-Main'

[installations."7.0".properties]
BDS = '/opt/delphi/RAD Studio/7.0'
APPDATA = '/home/me/delphi/AppData/CodeGear/BDS/7.0'

[installations."37.0".properties]
BDS = '/opt/delphi/Studio/37.0'
APPDATA = '/home/me/delphi/AppData/Embarcadero/BDS/37.0'

[[installations."37.0".path_mappings]]
from = 'C:\DelphiSources\db'
to = '/work/Delphi-Main/data'

[projects."Projects/ChainDriveAPI/ChainDriveAPI.dproj"]
installation = '37.0'
config = 'Debug'
platform = 'Win64'

[installations."37.0"]
rtlVersionConstants = ['RTLVersion131']
```

`BDS` is the direct local installation root. `APPDATA` is also a direct path,
but means that version's IDE configuration directory containing
`EnvOptions.proj` and optionally `environment.proj`. It is **not** the Windows
roaming `%APPDATA%` root: do not append vendor, `BDS`, or version components.
Values are literal TOML strings; use native absolute paths or mapped Windows
paths (no shell `~` or TOML `$(...)` expansion).

The default files are read directly beneath `APPDATA`. Optional explicit
properties `EnvOptions` and `EnvironmentSettings` locate `EnvOptions.proj` and
`environment.proj` individually, using native absolute or mapped Windows file
paths. Imported values cannot redirect these input files. The loader parses
bounded `SET` assignments from `BDS/bin/rsvars.bat` as data and never executes
it or changes the server environment. `rsvars.bat` supplies bootstrap defaults,
`environment.proj` supplies IDE-recorded values, and explicit configured
properties override both. Known original roots below BDS (for example BDSLIB)
are relocated below local BDS. Independent roots such as BDSCOMMONDIR and
BDSUSERDIR are not guessed: supply them explicitly or map their paths.

Project selector keys are exact paths, not globs. Relative paths are relative
to the containing config file; a project-local file can simply name
`"ChainDriveAPI.dproj"`. User config also accepts absolute native paths.
`config` and `platform` choose that project's build configuration and target;
more-local selectors override the same project's choice. Names are matched
case-insensitively against `.dproj` candidates when candidates are declared.
`rtlVersionConstants` belongs to an installation table, not its `properties`
subtable; it replaces automatic RTL update-constant discovery, and an empty list
means none are declared. Interactive installation and build selections do not
persist themselves to disk. The parser is
[`installation_config.rs`](src/installation_config.rs); input and IDE path
handling lives in [`installations/`](src/installations/mod.rs).

### Resolution boundaries and limitations

Ordered installation library paths follow project unit-search paths. IDE
browsing paths are source-navigation fallback, not proof of compiler identity
when an explicit unit binding conflicts. Installation roots/mappings enable
bounded project-scoped reads, not writes. Platform-conditioned settings are
not combined when platform facts are unknown. Missing paths make overall
metadata incomplete but do not disable unrelated positive unit lookups; a
missing explicit unit remains reserved instead of silently rebinding to a
same-named unit elsewhere.

Real copied-install checks are opt-in: set `PASCAL_TEST_DELPHI_ROOT` to the
parent containing `RAD Studio/7.0`, `RAD Studio/10.0`, `Studio/37.0`, and their
`AppData` IDE folders; set `PASCAL_TEST_PROJECT_ROOT` to the repository
containing `Projects/ChainDriveAPI/ChainDriveAPI.dproj`. Then run
`cargo test -p pascal-project --test local_installations -- --ignored` (and set
both variables in the command environment). Do not
interpret this as a green real-RTL-resolution guarantee: versions 7.0, 10.0,
and 37.0 can return `Incomplete` because earlier IDE/environment path metadata
is unknown. Version 23.0 is partial and supplies only partial evidence.
Portable fixtures are in [`tests/installations.rs`](tests/installations.rs).
