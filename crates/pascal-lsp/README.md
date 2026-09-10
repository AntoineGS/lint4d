# pascal-lsp

A native, source-based Delphi/Object Pascal language server. Runs on Linux
without Windows, Wine, RAD Studio, or `DelphiLSP.exe`. This first slice prioritizes
declaration/definition navigation and reuses lint4d and fmt4d for diagnostics and
formatting. It is not a replacement for Delphi's compiler or its complete type
system.

## Build and Run

From the lint4d repository root, with Rust and a C compiler installed:

```sh
cargo build --release --locked -p pascal-lsp
./target/release/pascal-lsp --version
```

The workspace currently requires your `tree-sitter-pascal` checkout beside
`lint4d`, because `pascal-core` uses a local path dependency. cfg-core and
cfg-pascal are fetched from the Git revisions in Cargo.lock. Building in a nested
worktree requires the grammar at that worktree's sibling path as well.

To install the executable into Cargo's bin directory:

```sh
cargo install --locked --path crates/pascal-lsp
```

Ensure `~/.cargo/bin` is on your PATH, or provide Neovim the full executable
path. `pascal-lsp --stdio` (also the default) speaks LSP over stdin/stdout;
logs go to stderr. Do not use `cargo run` as your editor's server command.

## Neovim 0.11+

No third-party Neovim LSP plugin is required. The complete configuration is in
[`examples/neovim.lua`](examples/neovim.lua). Load it from your `init.lua`:

```lua
dofile('/absolute/path/to/lint4d/crates/pascal-lsp/examples/neovim.lua')
```

If you have not installed the binary on PATH, set its location before starting
Neovim:

```sh
PASCAL_LSP_BIN=/absolute/path/to/lint4d/target/release/pascal-lsp nvim MyUnit.pas
```

The example selects the nearest `.lint4d.toml` or `.git` workspace root and
attaches to `.pas`, `.dpr`, and `.dpk` buffers. Put an empty `.lint4d.toml` at a
project root if you want a narrower scope than the repository. Avoid attaching
another Pascal language server to the same buffer while evaluating this slice.

| Key / Command | Behavior |
| --- | --- |
| `gd` | Go to a routine's implementation body, falling back to its declaration |
| `gD` | Go to the visible declaration, such as the unit interface or class header |
| `gi` | Go to implementation; uses the same fallback as `gd` in this slice |
| `<C-o>` | Return to the previous jump location |
| `:lua vim.lsp.buf.format({ name = 'pascal_lsp' })` | Explicitly format the current buffer |
| `:checkhealth vim.lsp` | Inspect attachment, executable, and client configuration |
| `:lua print(vim.lsp.log.get_filename())` | Locate the LSP log, including server stderr |

If several overloads/candidates remain, Neovim presents multiple locations
rather than the server guessing based on argument types. Keep the cursor on the
identifier, not on whitespace following it.

## Source Paths and Configuration

The server lazily indexes Pascal source under the workspace roots. Add library
source directories outside those roots using `init_options` in the example:

```lua
init_options = {
  projectFile = 'src/Shop.dproj',
  buildConfig = 'Debug',
  platform = 'Win32',
  sourcePaths = {
    '../shared',
    '/home/me/delphi-sources/rtl',
    '/home/me/delphi-sources/vcl',
  },
  exclude = { '**/__history/**', '**/__recovery/**', 'vendor/obsolete/**' },
},
```

`sourcePaths` adds to the workspace root; relative paths resolve against each
workspace folder. Libraries must be available as source files on Linux for
source navigation. `projectFile`, `buildConfig`, and `platform` select the
project metadata context used for navigation. The server reads `.dproj`,
`.dpr`/`.dpk`, imported `.optset` files, `DCC_UnitSearchPath`, `DCCReference`,
`DCC_Namespace`, `DCC_UnitAlias`, and explicit unit paths in a project main
source. It does not launch a compiler, read the Windows registry, automatically
translate Windows paths, or resolve compiled-only DCUs. Only use library sources
you are entitled to access.

Without `projectFile`, discovery searches ancestor directories up to the
workspace boundary for an unambiguous `.dproj`, falling back to a `.dpr` or
`.dpk`. Ambiguous projects require an explicit selection; the server does not
choose one arbitrarily. The project's configuration/platform defaults apply
unless overridden. Ordered project mappings and search paths take precedence
over unrelated repository files. Missing units and unsupported project settings
are reported as warnings in the LSP log, not as compiler-grade undeclared-symbol
errors. Defines are collected as metadata, but source conditional compilation
is not yet evaluated.

`.lint4d.toml` is discovered for each open file's lint settings and suppressions.
Root-level excludes also apply to source discovery. `.fmt4d.toml` controls
formatting; this slice does not translate LSP indentation options into formatter
settings. Formatting is explicit and returns an edit to the client: the server
does not write files. DCU-dependent lint rules are not enabled through a project
context in this slice, and the CLI's baseline filtering is not applied.

Source discovery is lazy: initialization never walks or parses the workspace.
An opened buffer parses immediately; a navigation request parses its requested
source and then only the direct/transitive units needed to resolve that request,
within bounded dependency work. Project-aware imports are bound to the selected
project, including an explicit empty binding when a unit cannot be resolved, so
an identically named unit from another project cannot leak into the result.
Projectless fallback uses a bounded filename-only catalogue and never parses
unrelated candidates. Open buffers are authoritative until closed; closing
restores the current disk version. Disk metadata and project metadata are
revalidated on demand for the requested source and dependencies it traverses,
and watcher events are supported but not required. Navigation does not
globally rescan a large workspace on each request.

Disk discovery excludes descendants named `.git`, `.worktrees`, `target`,
`node_modules`, `build`, `dist`, and similar generated directories. An explicit
workspace/source root may itself live under one of these names. Symlinked files
and directories are not scanned. Linting is debounced by 250 ms. Source
parsing, navigation and linting currently share a synchronous worker, so this is
not yet a low-latency incremental compiler service.

## Navigation Coverage

Implemented and covered by tests:

- Case-insensitive unit, routine, type, variable, constant, parameter, field,
  and property declaration lookup.
- Property `gD`/declaration navigation stays on the property; `gd`/`gi`
  definition navigation follows its explicit `read` accessor (getter or
  backing field), or its `write` accessor for write-only properties, with a
  safe property-declaration fallback when the accessor cannot be resolved.
- Lexical local/parameter shadowing and nested routines.
- Cross-unit references restricted by `uses` visibility, including separate
  interface/implementation uses clauses; `uses` entries navigate to units.
- Routine and class-method declaration/implementation pairing, including unique
  abbreviated implementation headers.
- Qualified unit/type names, namespaced units, straightforward declared-type
  member access, and `Self` members.
- Class members taking precedence over unit globals, including implementation
  classes and nested procedures inside methods.
- In-memory replacements, Unicode UTF-16 coordinates, and CRLF source files.
- Unknown receivers do not trigger an unrelated workspace-wide name search.

Not implemented or incomplete:

- Inherited-member lookup, `with` resolution, helpers, generic/type-alias
  inference, function-result expression typing, and argument-based overload
  selection.
- Full member accessibility and Delphi declaration-order rules. This is a
  syntactic index, not a compiler-validated semantic model.
- Evaluation of `{$IFDEF}` branches or include-file expansion. Conditional
  variants and duplicate units can produce multiple candidate locations.
- Full MSBuild evaluation, arbitrary `.dproj` targets, and `.delphilsp.json`
  compiler-equivalent search-path/configuration loading.
- Completion, hover, references, rename, code actions, and document/workspace
  symbols. The server does not advertise those capabilities.

Unsupported expressions can return no location. Results should be evaluated
against your own projects before treating navigation as compiler-equivalent.

## Limits

| Initialization option | Default and maximum |
| --- | --- |
| `projectFile` | Optional `.dproj`, `.dpr`, or `.dpk` project selector |
| `buildConfig` | Optional selected Delphi build configuration |
| `platform` | Optional selected Delphi platform |
| `sourcePaths` | Additional ordered unit search paths |
| `exclude` | Additional source-discovery exclude globs |
| `maxFiles` | 10,000 indexed files |
| `maxFileBytes` | 2,097,152 bytes per document |
| `maxTotalBytes` | 268,435,456 source bytes per budget |

Resource-limit options may lower limits, not raise the hard caps. Open buffers
take priority over closed parsed-cache entries, which can be evicted and loaded
again on demand. Current-request dependencies are protected during resolution;
if they cannot fit, navigation reports incomplete resolution rather than
discarding an editable buffer. Source-byte limits are not process RSS limits,
since trees and indexes require additional memory. Limit warnings are
written to stderr. Rejected editor buffers cannot return stale navigation or
formatting edits and recover after a newer acceptable update.

Linting and formatting reject syntax trees deeper than 256 levels to protect
their recursive analysis pipelines. Navigation uses iterative tree walks. LSP
headers are capped at 8 KiB and message bodies at 8 MiB. The stdio endpoint is
intended for a trusted local editor, not network exposure.

## Tests

```sh
cargo test --locked -p pascal-lsp
cargo clippy --locked -p pascal-lsp --all-targets --no-deps -- -D warnings
```

Tests include navigation fixtures, real-process framed LSP sessions, and a
headless Neovim test loading the shipped example and invoking `gd`, `gD`, and
`gi`. The Neovim test skips when `nvim` is not installed; when installed it must
be version 0.11 or newer. No personal Neovim configuration is changed.

## License

Part of lint4d; see the repository's [LICENSE](../../LICENSE), including its
Commons Clause condition. cfg-core and cfg-pascal have their own licenses.
