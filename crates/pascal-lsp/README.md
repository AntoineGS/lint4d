# pascal-lsp

A native, source-based Delphi/Object Pascal language server. Runs on Linux
without Windows, Wine, RAD Studio, or `DelphiLSP.exe`. This slice provides
declaration/definition navigation, conservative symbol-aware rename, naming
quick fixes, and reuses lint4d and fmt4d for diagnostics and formatting. It is
not a replacement for Delphi's compiler or its complete type system.

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

The grammar checkout must include `tree-sitter-pascal` commit `6881c9b`
(conditional method attributes) for the unit/type navigation regression tests
and affected Delphi units to parse correctly.

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
| `gd` on a unit in `uses` | Open that unit at its declaration |
| `gd` or `gD` on a type name | Jump to the class, record, interface, or other type declaration |
| `gd` | Go to a routine's implementation body, falling back to its declaration |
| `gD` | Go to the visible declaration, such as the unit interface or class header |
| `gi` | Go to implementation; uses the same fallback as `gd` in this slice |
| `<C-o>` | Return to the previous jump location |
| `grn` / `:lua vim.lsp.buf.rename()` | Rename the symbol under the cursor |
| `gra` / `:lua vim.lsp.buf.code_action()` | Show naming quick fixes for the requested range |
| `:lua vim.lsp.buf.format({ name = 'pascal_lsp' })` | Explicitly format the current buffer |
| `:checkhealth vim.lsp` | Inspect attachment, executable, and client configuration |
| `:lua print(vim.lsp.log.get_filename())` | Locate the LSP log, including server stderr |

If several overloads/candidates remain, Neovim presents multiple locations
rather than the server guessing based on argument types. Keep the cursor on the
identifier, not on whitespace following it.

For example, put the cursor on `MDIBDatabase` in `uses MDIBDatabase;` and press
`gd` to open the unit. Put it on `TMDIBDatabase` in a type annotation, generic
argument, or constructor receiver to jump to the class declaration. These use
the standard LSP definition/declaration requests, not a new editor-specific
command. Navigation from a variable name to its type (`textDocument/typeDefinition`)
is a separate feature and is not advertised by this server yet.

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
`DCC_UsePackage`, `DCC_Namespace`, `DCC_UnitAlias`, and explicit unit paths in a
project main source. It does not launch a compiler, read the Windows registry,
automatically translate Windows paths, or resolve compiled-only DCUs. Only use
library sources you are entitled to access.

When an imported unit is not found through explicit project mappings or the
ordinary unit search paths, the server lazily checks only source packages named
by the selected project's evaluated `DCC_UsePackage` property. It performs a
bounded, deterministic filename-only catalogue lookup under configured workspace
roots and `sourcePaths`. The catalogue matches only descriptor filename stems
named by `DCC_UsePackage` (case-insensitively); it never opens unrelated package
headers to infer names. The catalogue retains only matching paths plus bounded
directory stamps, and it must complete within its fixed safety bound before a
package can be treated as unique; an incomplete catalogue is reported rather
than guessed. Only the selected package descriptor and its relevant `contains`
or project-reference metadata are read after that lookup. A selected descriptor
must still have valid package syntax, but its header name may use a legacy
variant: the matching filename remains the package identity. A same-stem `.dpk`
is authoritative over its `.dproj`; duplicate package descriptors are reported
as ambiguous rather than guessed. Package `.dproj` imports, imported option-set
files, and the package `MainSource` are tracked for cache freshness and
invalidation. Legacy descriptor filename aliases are not inferred; add the
library's source directory to `sourcePaths` when its units need direct lookup.
Package sources must remain under those configured roots, and missing compiled-only
packages are skipped with a warning only when they are relevant to a failed
import. Package `requires` dependencies are not followed transitively. Reference
or candidate safety limits also report an incomplete result instead of returning
a partial unique match. This discovery is not compiler-install or Windows-
registry auto-detection and does not add package exports to global unit search
paths.

Without `projectFile`, discovery searches ancestor directories up to the
workspace boundary for an unambiguous `.dproj`, falling back to a `.dpr` or
`.dpk`. Multiple candidates can be narrowed to a unique proven source owner;
shared ownership, uncertain dependencies, and incomplete candidate metadata
still require an explicit selection. Candidate metadata is tracked for freshness,
and ownership probing is bounded. The project's configuration/platform defaults apply
unless overridden. Ordered project mappings and search paths take precedence
over unrelated repository files. Missing units and unsupported project settings
are reported as warnings in the LSP log, not as compiler-grade undeclared-symbol
errors. Ordinary absent project-local configuration properties can evaluate as
empty, while unavailable environment inputs and values affected by uncertain
imports remain unknown. Compiled-only `.dcp` references do not establish source
membership or, by themselves, make source discovery incomplete. Missing source
references still do. Defines are collected as metadata, but source conditional
compilation is not yet evaluated.

In the MultidevComponents example, `MDIBDatabase.pas` is genuinely shared by
multiple packages, so automatic project selection still refuses to guess.
Explicit `MDDatabaseXE3.dproj` with Debug/Win32 now evaluates successfully.
The full-workspace rename remains blocked when includes referenced by
`Common/Vcl.Styles.Ext.pas` (`source/vcl/StyleUtils.inc` and
`source/vcl/StyleAPI.inc`) are unavailable through the configured search paths.
These references are inside a source conditional that the server cannot yet
prove inactive. With the current analysis, the include paths must resolve or
the workspace scope must be explicitly adjusted; the server does not silently
exclude potential consumers.

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
- Completion, hover, references, and document/workspace symbols.

Unsupported expressions can return no location. Results should be evaluated
against your own projects before treating navigation as compiler-equivalent.

## Rename and naming code actions

`textDocument/rename` and `textDocument/prepareRename` build a fresh, bounded
workspace snapshot. The snapshot includes unopened Pascal sources under the
workspace roots and unsaved buffers, then uses the same binding-based planner
for every returned edit. Open buffers receive versioned
`WorkspaceEdit.documentChanges`; closed files receive null-version edits. The
client applies the edit—`pascal-lsp` never writes application files.

`textDocument/codeAction` initially provides `quickfix` actions for
`constant-naming` and `local-variable-naming`. Suggestions use the configured
lint4d naming styles and honor rule-off settings and source suppressions. A
client with code-action resolve support receives a bounded opaque action token;
the server rechecks the declaration, source/configuration generations, rule,
and configuration before resolving it. Clients without resolve support receive
eager edits instead.

For safety, rename is refused rather than returning a partial edit when the
workspace scan is incomplete, an import/project context is unresolved or
ambiguous, a source path is a symlink escape, or an edit would touch an
external `sourcePaths` file. Include lookup searches the including file's
directory, evaluated `DCC_IncludePath`, then ordered unit/client source paths.
Nested includes are audited within depth, file, directive, and byte limits;
lookup observations and content hashes participate in stale-input checks.
Comments and recognized compiler-only directives, including conditional compiler
flags, do not by themselves block a rename. Pascal-dependent conditional
expressions and relevant source-bearing includes remain unsupported; missing or
unreadable includes cannot be treated as evidence that no reference exists.
Source conditional compilation is not expanded. Unit/module renames (which require
`RenameFile`), inherited/`with` lookup, overloaded/override relationships,
compiled-only consumers, and other unsupported bindings are also rejected by
the shared planner. Name collisions and reference capture are rejected before
any edit is returned. Unresolved imports can be irrelevant when all candidate
references are proven to bind within the source document without imports;
import-dependent references still require complete bindings. Unqualified global
fallbacks in class methods are not such a proof because inherited lookup is
not fully modeled. These checks are conservative; they bound races and
avoid false-safe workspace edits but do not eliminate changes made after a
request has completed.

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
cargo fmt --manifest-path crates/pascal-lsp/Cargo.toml -- --check
```

Tests include navigation fixtures, real-process framed LSP sessions, and a
headless Neovim test loading the shipped example. It invokes standard
`vim.lsp.buf.rename()` and `vim.lsp.buf.code_action()` on disposable provider
and initially unopened consumer fixtures, verifies their exact in-memory edits,
and verifies that neither fixture is saved. The Neovim test skips when `nvim` is
not installed; when installed it must be version 0.11 or newer. No personal
Neovim configuration is changed.

## License

Part of lint4d; see the repository's [LICENSE](../../LICENSE), including its
Commons Clause condition. cfg-core and cfg-pascal have their own licenses.
