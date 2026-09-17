# pascal-lsp

A native, source-based Delphi/Object Pascal language server. Runs on Linux
without Windows, Wine, RAD Studio, or `DelphiLSP.exe`. This slice provides
declaration/definition navigation, source-based hover and type definitions,
document and workspace symbols, semantic references, document highlights,
semantic completion and signature help, syntax folding ranges, conservative
structural selection ranges, conservative symbol-aware rename, naming quick
fixes, and reuses lint4d and fmt4d for diagnostics and formatting.
It is not a replacement for Delphi's compiler or its complete type system.

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
| `:lua vim.lsp.buf.type_definition()` | Go from a variable, parameter, field, or property to its named type declaration |
| `K` / `:lua vim.lsp.buf.hover()` | Show a bounded source declaration excerpt |
| `:lua vim.lsp.buf.completion()` | Request semantic completion; Neovim may also trigger it after `.` |
| `:lua vim.lsp.buf.signature_help()` | Show source-declared callable signatures after `(` or `,` |
| `<C-o>` | Return to the previous jump location |
| `grr` / `:lua vim.lsp.buf.references()` | List semantic references; the default client request includes declarations |
| `gO` / `:lua vim.lsp.buf.document_symbol()` | List the current document's symbols |
| `:lua vim.lsp.buf.workspace_symbol('query')` | Search workspace declarations by name |
| `grn` / `:lua vim.lsp.buf.rename()` | Rename the symbol under the cursor |
| `gra` / `:lua vim.lsp.buf.code_action()` | Show naming quick fixes for the requested range |
| `:lua vim.lsp.buf.format({ name = 'pascal_lsp' })` | Explicitly format the current buffer |
| `:lua vim.lsp.buf.document_highlight()` | Request current-document highlights explicitly |
| `:checkhealth vim.lsp` | Inspect attachment, executable, and client configuration |
| `:lua print(vim.lsp.log.get_filename())` | Locate the LSP log, including server stderr |

### Background analysis

Navigation, formatting, and open-buffer diagnostics run as cancellable,
snapshot-based background work. The server limits analysis to two concurrent
jobs, so the stdio loop remains responsive while a source or dependency graph
is being read. Up to 32 analysis jobs wait in a bounded FIFO-within-priority
queue; one slot is reserved for diagnostics. Completion, hover, signature,
navigation, type-definition, and other interactive requests are preferred over
bulk symbols, references, rename, and formatting, but at most three
interactive jobs are dispatched before a waiting diagnostic or bulk job. A
diagnostic burst is limited to two jobs when bulk work is also waiting, so
neither lower-priority class can starve. Queue overflow returns an explicit
`RequestFailed` response (`-32803`) with `analysis queue is full; retry the
request`; the server does not capture source snapshots until a job is
dispatched.

Client request recipients have a separate total bound of 33 outstanding
request IDs across running jobs, queued jobs, and coalesced attachments. Every
coalesced client request consumes one recipient slot, even when it asks for
the same computation as another request; reaching this limit returns the same
explicit overflow response rather than attaching an unbounded number of
clients. Cancelling a client request releases its recipient slot immediately,
while the shared computation may remain queued or running for its other
recipients.

Diagnostics stay debounced and coalesced per document; if both worker slots are
occupied, the diagnostic request is retried rather than spawning an unbounded
worker. Identical observational requests share a computation only when their
method, document, position, options, and captured version/generations match.
Queued observations invalidated by a newer document version receive one
`RequestCanceled` response (`-32800`) and are replaced by the latest request;
distinct-position references and edits are never silently merged. Explicit
cancellation removes queued work immediately, and shutdown drains queued
client requests with cancellation responses before stopping workers.

Results are checked against the worker's captured source/configuration read set
and the live workspace's cheap dependency change stamps; diagnostics also
verify the open-document version. Hover, completion, signature help,
declaration/definition/implementation/type-definition navigation, document
symbols, document highlights, formatting, and diagnostics can survive an
unrelated open-file or configuration change when their recorded source,
provider, project metadata, lint/fmt configuration, and negative membership or
recursive provider-absence scope observations are unchanged. Empty results
retain the same dependency and absence checks. Requested sources, imported
providers, consumed configuration,
project selection, and workspace-folder changes invalidate the result.
An incomplete recursive filename catalogue records potential provider scopes
for conservative live revalidation and logs its bounded-entry warning instead
of silently claiming complete absence.

Workspace symbols, references, prepare/rename, code actions, and code-action
resolution remain conservative: any source/configuration generation change
rejects them because their completeness or mutation target may depend on
workspace-wide membership. A stale navigation or formatting result is rejected
for retry, while a stale or cancelled diagnostic is silently discarded.
Cancelling a navigation or formatting request returns the standard LSP
`RequestCanceled` error (`-32800`) exactly once.

### Open-document synchronization

The server advertises UTF-16 incremental synchronization for
`textDocument/didChange`. It accepts both full-document replacement events and
arrays of ranged edits; ranged edits are applied in order, with each range
measured against the text produced by the preceding edit in that notification.
Positions use UTF-16 code units, including for non-BMP characters, and handle
CRLF line endings and end-of-file positions. `rangeLength`, when supplied, is
validated in UTF-16 units but is optional.

Each notification is applied atomically. Invalid ranges, reversed ranges,
invalid UTF-16 boundaries, or source-limit violations discard the whole
notification and remove the stale overlay rather than publishing a partial
edit. A rejected document must receive a full-text replacement (or be closed
and reopened) before ranged edits are accepted again. Older document versions
are ignored; an empty change array advances the version as a no-op. Closing an
open document removes its overlay and restores the current disk contents.

If argument types identify one supported overload, Neovim receives that
declaration/result; otherwise it presents the retained overload set rather than
the server guessing. Keep the cursor on the identifier, not on whitespace
following it.

Neovim 0.13-dev may show a quickfix list even for a singleton `gi` result; use
`:cfirst` and choose the entry to navigate. Older clients may jump directly.

For example, put the cursor on `MDIBDatabase` in `uses MDIBDatabase;` and press
`gd` to open the unit. Put it on `TMDIBDatabase` in a type annotation, generic
argument, or constructor receiver to jump to the class declaration. These use
the standard LSP definition/declaration requests, not a new editor-specific
command. The server also advertises `textDocument/typeDefinition`: on a
variable, parameter, field, or property it opens the source declaration of the
named type, on a type identifier it opens that type declaration, and on a
source-backed function or constructor it opens the declared result/constructed
type. Unsaved buffers and the selected project context are used for the bounded
lookup.

Type-definition lookup is source-based rather than compiler-based. Type aliases
retain their own declaration as the target, alias cycles are bounded, and
unresolved, built-in, anonymous, or unsupported types return no location. A
qualified or otherwise ambiguous binding retains its distinct source
candidates; the server does not substitute an unrelated workspace-wide symbol
with the same spelling.

Completion is case-insensitive and uses the nearest lexical locals and
parameters, visible class members, current-unit declarations, and resolved
imported declarations. It preserves declaration casing and returns plain
identifier `TextEdit`s; it does not insert snippets, imports, or additional
edits. Imported private/protected members and unrelated workspace names are not
offered. Conditional uncertainty, ambiguous receivers, and bounded candidate
truncation are reported conservatively with `CompletionList.isIncomplete` rather
than as a falsely complete result. Expression receivers are resolved source-first
as well: calls such as `MakeValue().Member`, constructors such as
`TWidget.Create.Member`, casts such as `TWidget(Value).Member` and
`(Value as TWidget).Member`, and nested result chains are supported when their
named types are unambiguous. The receiver's declaring unit is retained when a
result type has the same spelling as a type in the consuming unit.

Class and interface member lookup follows the source ancestry retained for each
indexed type, including inherited fields and routines and parents in imported
units. A direct member shadows an inherited member, while missing, ambiguous,
cyclic, or conditionally uncertain ancestry does not justify guessing a member
from an unrelated type or global declaration. Rename remains conservative and
rejects inherited class-member references until the complete override model is
available.

Signature help reports every supported source declaration, including overloads.
When source-resolved argument types identify one exact, safe widening, or safe
class upcast match, `active_signature` identifies that overload while retaining
the complete candidate list. Unknown arguments, equal-ranked matches,
unsupported relationships, `with`/`inherited` receivers, and anonymous or
generic callables remain unselected; opaque calls return no signature. The
active parameter is counted syntactically while skipping nested calls, indexers,
Pascal strings, and comments; grouped formal parameter names are expanded
individually. Callable members reached through source-backed function results,
constructors, casts, and nested expression receivers use the same bounded
selection as completion and navigation.

Overload matching is intentionally conservative and source-based. It recognizes
Delphi built-in integer, real, string, character, Boolean, and `nil` literals,
including radix integers and concatenated string fragments, typed
variables/parameters, defaults, and `var`/`out` lvalue requirements. Character
values can match string parameters, but ordinal numeric conversion requires an
explicit source call such as `Ord(value)`. Integer literal ranges and declared
integer widths are preserved, including the proven `Integer`/`LongInt` alias;
`var`/`out` matches require an exact known type and a writable argument (value
parameters are writable, while `const` parameters and properties are not), and
omitted defaults do not make an otherwise less-specific overload win. Inherited
routine overloads are combined only when source `overload`/`override` metadata
proves the relationship; direct methods with `reintroduce` or otherwise hidden
signatures remain authoritative, while direct fields and properties still
shadow inherited members. Deep parenthesized arguments are traversed iteratively
under the existing work/byte budgets and honor request cancellation.
Explicit named types retain their declaring-unit identity, so same-spelled types
from different units do not match. Generic inference, anonymous callable types,
full pointer/variant/record compatibility, and compiler-level overload rules are
outside this source-only model; ambiguous or unsupported calls stay incomplete
instead of selecting arbitrarily.

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
execute Delphi/MSBuild targets, or resolve compiled-only DCUs. Only use library
sources you are entitled to access.

### Delphi path overrides (LSP only)

When a Delphi project records Windows installation paths but the corresponding
source is installed locally on Linux, `pascal-lsp` can translate those paths
only when you explicitly configure a mapping. In this release, **only the LSP
consumes these files**. `lint4d` and `fmt4d` CLI project discovery do not read
them; CLI integration is separate follow-up work.

The layers below are merged in order. A missing file is an empty layer.

| Layer | Location |
| --- | --- |
| User | `$XDG_CONFIG_HOME/delphi-tools/config.toml`, when `XDG_CONFIG_HOME` is nonempty and absolute |
| User fallback | `$HOME/.config/delphi-tools/config.toml` otherwise, when `HOME` is nonempty and absolute |
| Workspace | `<workspace-root>/.delphi-tools.local.toml` |
| Project | `<project-directory>/.delphi-tools.local.toml` |

Use this configuration as a starting point:

```toml
[properties]
BDS = 'C:\Program Files\Embarcadero\RAD Studio\7.0'

[[path_mappings]]
from = 'C:\Program Files\Embarcadero\RAD Studio\7.0'
to = '/opt/delphi/2010'

[[path_mappings]]
from = 'C:\Program Files\Embarcadero\RAD Studio\7.0\lib\Indy10'
to = '/home/you/sources/indy10'
```

Properties and mapping prefixes are ASCII case-insensitive per key. A higher
layer replaces the complete value for the same property or normalized Windows
prefix; unrelated entries remain inherited. The resolver then chooses the
longest matching Windows path prefix after that merge, at path-component
boundaries, so the Indy mapping above wins for paths below `lib\Indy10`.
There is no deletion/tombstone syntax: to change an inherited entry, override
the same key or prefix in the higher layer.

Configured property values are literal values used while evaluating project
metadata; configured values containing `$(` are rejected rather than recursively
expanded. Client `buildConfig` and `platform` take precedence over `[properties]`
`Config` and `Platform`; those file properties in turn take precedence over
project and option-set defaults. Values from either higher-precedence source are
immutable evaluator inputs: project XML and imports cannot replace or taint
them. The server evaluates supported `.optset` imports, but still ignores
non-`.optset` imports and does not execute targets.
Consequently, an ignored import such as
`$(BDS)\Bin\CodeGear.Delphi.Targets` remains a warning/limitation rather than a
way to discover a Delphi installation.

Mappings apply at supported project-metadata filesystem-use boundaries; they do
not translate arbitrary strings, unit names, source directives with new macro
semantics, or URIs. Native absolute property values bypass Windows translation,
but they do not grant broad filesystem access. Mapped destinations are
context-scoped, read-only roots for the project that selected them, subject to
the existing containment and symlink-safety checks. They are not workspace
roots or implicit unit-search paths, and mappings do not grant edit
authorization: existing writable-workspace authorization remains authoritative.
An explicit formatting request for the current buffer is not newly forbidden by
a mapping. Unmapped Windows paths on Linux remain unavailable and produce a
diagnostic; use an explicit mapping or the existing native `sourcePaths` option.

Each applicable file is captured on its first read for the LSP session: user and
initial workspace layers are captured at startup, and a project layer when its
candidate is first evaluated. Editing a captured override file has no effect
until the LSP is restarted. This snapshot behavior is independent of the
existing lint/formatter sidecar reload behavior.

Malformed, unreadable, oversized, or invalid override files are errors for the
scope that selects them, not a silent fallback. A bad user file affects all
project-dependent contexts; a bad workspace file affects contexts using that
longest-containing workspace; a bad project file affects projects in that
directory. Other contexts can continue. The server reports provenance in its
log and project-context warnings without dumping configured property values.

These local machine paths commonly differ between developers. Add
`.delphi-tools.local.toml` to your global Git ignore or the repository's local
exclude settings if it should not be committed; the `.local` suffix alone does
not make Git ignore it.

When an imported unit is not found through explicit project mappings or the
ordinary unit search paths, the server lazily checks only source packages named
by the selected project's evaluated `DCC_UsePackage` property. It performs a
bounded, deterministic filename-only catalogue lookup under configured workspace
roots, `sourcePaths`, and the requester project's mapped destinations. The
catalogue matches only descriptor filename stems named by `DCC_UsePackage`
(case-insensitively); it never opens unrelated package headers to infer names.
The catalogue retains only matching paths plus bounded directory stamps, and it
must complete within its fixed safety bound before a package can be treated as
unique; an incomplete catalogue is reported rather than guessed. Only the
selected package descriptor and its relevant `contains` or project-reference
metadata are read after that lookup. Package evaluation inherits the requesting
project's effective properties and mappings; it does not load a package-local
override sidecar. A selected descriptor must still have valid package syntax,
but its header name may use a legacy variant: the matching filename remains the
package identity. A same-stem `.dpk` is authoritative over its `.dproj`;
duplicate package descriptors are reported as ambiguous rather than guessed.
Package `.dproj` imports, imported option-set files, and the package `MainSource`
are tracked for cache freshness and invalidation. Legacy descriptor filename
aliases are not inferred; add the library's source directory to `sourcePaths`
when its units need direct lookup. Package sources must remain under those
configured or requester-mapped roots, and missing compiled-only packages are
skipped with a warning only when they are relevant to a failed import. Package
`requires` dependencies are not followed transitively. Reference or candidate
safety limits also report an incomplete result instead of returning a partial
unique match. This discovery is not compiler-install or Windows-registry
auto-detection and does not add package exports to global unit search paths.

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
references still do. Project defines are used as positive conditional facts;
`buildConfig` and `platform` select the project context but do not synthesize
compiler-version or host-environment facts.

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
Excludes from that sidecar suppress lint diagnostics for matching files; they do
not apply to source discovery. The initialization `exclude` option controls
source discovery. `.fmt4d.toml` controls formatting; this slice does not
translate LSP indentation options into formatter settings. Formatting is
explicit and returns an edit to the client: the server does not write files.
DCU-dependent lint rules are not enabled through a project context in this
slice, and the CLI's baseline filtering is not applied.

### Per-project lint and formatter configuration

Configuration is resolved independently for lint4d and fmt4d, for the effective
project context of each document. This keeps a shared source file associated with
the project selected for that buffer rather than applying one workspace-wide
configuration. The lookup directories, in precedence order, are the selected
project directory, the longest containing workspace folder, and the nearest Git
root. A `.git` directory **or worktree `.git` file** establishes that final
boundary. With no project context, the longest containing workspace folder is
used; with neither workspace nor Git root, the document's parent directory is
used.

For each tool, the first existing sidecar in that order wins:

```text
project/.lint4d.toml  -> workspace/.lint4d.toml -> repository/.lint4d.toml
project/.fmt4d.toml   -> workspace/.fmt4d.toml  -> repository/.fmt4d.toml
```

There is **no merge** with lower-priority files. For example, a project
`.fmt4d.toml` that sets only `max_line_length` uses the formatter default for
`indent_size`, not the repository's value. The lint and formatter searches are
also independent: one can select a project sidecar while the other falls back
to a workspace sidecar. A selected project directory takes precedence over a
source file's intermediate directory; the resolver does not walk every ancestor.

Lint `exclude` patterns are evaluated relative to the selected `.lint4d.toml`;
an excluded document publishes no lint diagnostics. They do not globally remove
the file from project-aware navigation or rename discovery. `sourcePaths` and
the initialization `exclude` option remain client startup options, not values
merged from sidecars. Relative `sourcePaths` resolve against each workspace root.
For formatter `uses.group`, relative `uses.external_paths` resolve against the
directory containing the selected `.fmt4d.toml`; the external-unit scan accepts
only regular `.pas` files, does not follow symlinks, and is bounded by traversal,
file-count, per-file, and total-byte limits. Project main and explicitly
referenced units remain project units even when their stems also appear externally.

Malformed, unreadable, non-UTF-8, non-regular, or over-4 MiB sidecars are not
silently skipped in favor of a lower-priority file. Project-context responses
report them in `warnings`; lint diagnostics report an error, and formatting or
other requests that need the configuration fail with the reported error. The
server watches selected and candidate sidecar paths through
`workspace/didChangeWatchedFiles`, using a `RelativePattern` rooted at each
sidecar's actual parent when the client advertises relative-pattern support. It
invalidates the configuration generation and recomputes diagnostics and
subsequent requests. A rename/code-action
request whose configuration changes while it is being prepared is rejected with
a retry error rather than returning edits based on stale input. Watcher events
improve freshness but are not required: request-time checks preserve the same
degraded, on-demand behavior when a client cannot register file watchers.
Explicit paths that cannot be registered by such a client are degraded rather
than emitted as invalid absolute string globs. Rejected explicit watcher
registrations are retried up to three times; paths
that remain rejected enter that degraded mode and no longer consume the bounded
explicit-watcher allowance.

`workspace/didChangeConfiguration` and general runtime settings overrides are
not supported. Change sidecar files or restart the client after changing
`initializationOptions`.

### Project selection protocol and Neovim picker

Project selection is session-local and scoped to the candidate directory: it is
not written to `.dproj`, `.lint4d.toml`, `.fmt4d.toml`, or any other file. The
server advertises `experimental.projectSelection: true`. A client can request:

```text
pascal/projectContext { textDocument: { uri } }
pascal/selectProject { textDocument: { uri }, projectUri: URI | null }
```

Both request parameter objects and the response use camelCase. The context
response contains `scopeUri`, `candidates`, `selectedProjectUri`,
`selectionMode`, `lintConfigUri`, `fmtConfigUri`, and `warnings`.
`selectionMode` is `directory` for a valid session selection, `configured` for
the startup `projectFile`, `automatic` for unambiguous discovery, `ambiguous`
when candidates need a choice, `standalone` when none exist, or `invalid` when
a saved session choice is no longer current. `projectUri: null` chooses
**Automatic**: it clears the directory choice and falls back to configured
`projectFile` or automatic discovery. A selection must be a current `.dproj`
candidate in the document's directory scope; invalid or malformed choices do
not replace the prior live selection.

The shipped Neovim helper creates the buffer-local `:PascalProject` command and
`<leader>wp` mapping. It first checks `client.server_capabilities.experimental.projectSelection`; an older installed server (including the existing 0.4.0
binary) shows an update warning instead of sending unsupported requests. The
picker retains the buffer URI and verifies that the buffer is still valid and
attached before applying asynchronous results, so a response cannot select a
project for a different buffer. Install or point Neovim at an updated server to
use this capability; this documentation does not install or deploy one.

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

### Incremental parsing and cached documents

After the first parse, a changed document computes one conservative byte and
`Point` edit and gives tree-sitter an edited copy of the previous tree. The
parser input is the exact offset-preserving result of conditional and compiler
directive preprocessing, so parser diagnostics and directive patches retain
source coordinates. If the previous tree extent is not trusted, or a rewrite
cannot preserve byte offsets, the document is parsed from scratch. Opaque
`{$IF ...}{$IFEND}` bodies are checked again after an edit even when the
incremental tree itself reports no syntax error.

Semantic extraction is still rebuilt for changed source or effective project
defines; this is not a fine-grained incremental compiler. When source,
effective defines, and the complete project context are unchanged, the
immutable parsed document is reused. Import bindings remain per-index state
and are rebuilt rather than shared. Background snapshots receive only the
bounded set of documents retained by the workspace's `maxFiles` and source-byte
limits, and reuse a cached model only after exact source/context checks. Open
overlay text, disk reloads, project changes, and evictions therefore invalidate
or bypass the reusable model safely.

## Symbols, References and Highlights

The server advertises the standard query capabilities
`textDocument/documentSymbol`, `workspace/symbol`,
`textDocument/references`, `textDocument/documentHighlight`, and semantic
tokens. These queries are read-only: the server does not write source files.

### Document outlines

`textDocument/documentSymbol` returns full declaration ranges and identifier
selection ranges. When the client advertises
`hierarchicalDocumentSymbolSupport`, the result is nested `DocumentSymbol`
data. Otherwise the server returns the same outline flattened to
`SymbolInformation` entries with container names. Child ranges are contained by
their parents; an out-of-class method implementation remains a separate source
entry rather than being placed under the class declaration.

An outline request has a hard 10,000-entry response bound and a 32-level
hierarchy bound. Exceeding either bound returns an error instead of silently
truncating the result.

### Workspace symbols

`workspace/symbol` always returns resolved flat `SymbolInformation` entries and
does not require a separate resolve request. Queries are case-insensitive
substring matches; an empty query requests every eligible declaration. The
search includes unopened Pascal sources under workspace roots and configured
`sourcePaths`, while an open buffer's unsaved text takes precedence over its
disk bytes. Routine-local variables and parameters are excluded, while
project-level routines and type members remain searchable. Container labels
identify the unit or owning type.

Matching declaration and implementation sites remain separate locations, as do
overloads; declarations are not collapsed merely because their names match. A
workspace-symbol response is capped at 10,000 entries, and exceeding the bound
returns an error directing the client to narrow the query. Configured
`sourcePaths` are read-only input for these queries; the server never edits or
saves those files.

### References

`textDocument/references` uses the binding identity produced by the navigation
resolver rather than textual matching or a rename-to-a-dummy-name probe.
`context.includeDeclaration = false` excludes the binding's declaration sites;
`true` includes every supported declaration site as well as its references.
Open overlays are authoritative, and unopened consumers may be included. Unit
and module identifiers are outside the supported binding subset: a references
request returns an unsupported-binding error, while a document-highlight
request at a unit or module identifier returns an empty list. The separate
`textDocument/rename` limitation for units remains guarded by `RenameFile`
support.

If workspace discovery, a required source/include read, or binding resolution
is incomplete, the request returns an actionable error rather than a partial
location list. Unsupported or ambiguous bindings are rejected rather than
guessed; inherited or `with`-dependent lookup, unknown class ancestors, and unsupported
overload relationships can therefore make a reference request fail.

### Document highlights

`textDocument/documentHighlight` is restricted to the requested document and,
for supported bindings, includes its declaration. Unit and module identifiers
are unsupported and return an empty list. Highlight `kind` is intentionally unspecified
(`None`) until a reliable read/write classifier exists. The shipped Neovim
configuration does not install `CursorHold` or `CursorMoved` autocmds; clients
request highlights explicitly, for example with
`vim.lsp.buf.document_highlight()`.

### Semantic tokens

The server supports both `textDocument/semanticTokens/full` and
`textDocument/semanticTokens/range`. Full results are complete snapshots rather
than delta responses. The legend includes standard Pascal-oriented namespace,
type, class, interface, record, enum, routine, parameter, variable, property,
literal, comment, keyword, and operator token types.

Keywords, strings, numbers, comments, and operators are classified lexically.
Identifiers receive a semantic type only when the source index can prove their
binding; declarations and definitions are marked separately, class members are
marked static, and constants and enum members are marked read-only. Unknown,
ambiguous, conditionally uncertain, or parser-recovery identifiers remain
unclassified instead of being guessed. If import discovery is incomplete or an
include audit is uncertain, the response keeps syntax-derived tokens but
suppresses semantic identifier classifications rather than using partially
resolved imports.
Ranges are clipped to the requested UTF-16 range, including CRLF and non-BMP
text, and open-buffer overlays are used in preference to disk content. Semantic
token requests use the same bounded snapshots, cancellation, and stale-result
checks as the other analysis queries.

### Folding ranges

The server advertises `textDocument/foldingRange` and returns sorted,
deduplicated ranges for multiline Pascal declarations and implementations,
classes, records, interfaces, begin/end and control-flow blocks, unit sections,
multiline comments, and balanced `REGION`/`ENDREGION` directives. Single-line
constructs and directive-looking text inside strings or comments are ignored.
Conditional, parser-recovery, opaque, and unmatched regions are handled
conservatively so a range does not hide source whose extent is uncertain.

Ranges use the original source offsets and UTF-16 positions, including CRLF and
non-BMP text. Open-buffer overlays take precedence over disk content. The
request honors the client's `rangeLimit`, `lineFoldingOnly`, and
`foldingRangeKind.valueSet` capabilities. Structural ranges are untagged;
comments, imports, and explicit regions use the standard `comment`, `imports`,
and `region` kinds. A zero range limit returns an empty list, and bounded
analysis returns an explicit request failure rather than traversing without a
limit. Folding is syntax-based and does not require imported units to resolve.

All analysis queries run in bounded analysis workers. `$/cancelRequest` is honored,
and source/configuration generations plus the observed read set are revalidated
before delivery. A changed input produces a stale-result error for the client
to retry rather than returning data computed from an older snapshot.

### Selection ranges

`textDocument/selectionRange` returns one range per requested cursor position.
Each result is a strict inner-to-outer chain of syntax ranges, ending at the
document range when necessary. Request order and duplicate positions are
preserved. The query uses the original source snapshot, including open-buffer
overlays, and reports UTF-16 positions correctly across CRLF and non-BMP text.

Selection ranges are syntax-based and do not require imported units to resolve.
Invalid UTF-16 boundaries, batches larger than 256 positions, excessive syntax
depth, and excessive traversal work fail the request without partial results.
Empty documents return a valid zero-length document range. Selection requests
also honor cancellation and the same generation/read-set stale-result checks as
the other bounded analysis queries.

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
  member access, class/record helper members (including class-target ancestry),
  parent-helper members, and `Self` members.
- Typed `with` scopes for class/record variables, `Self`, qualified fields,
  factory and generic receiver expressions, nested and comma-separated
  receivers, ordered shadowing, and single-statement or block body boundaries.
  Receiver expressions are evaluated in their enclosing lexical scope; unknown
  receiver, ancestry, helper, or conditional resolution fails closed.
- Class and record helpers are selected using lexical visibility and ordered
  `uses` clauses. Helper members feed navigation, completion, hover, signature
  help, type definitions, and safe binding-based rename; unresolved or
  ambiguous helper targets fail closed.
- Source-based `textDocument/typeDefinition` for named variable, parameter,
  field, property, and type declarations, including bounded aliases and
  selected-project unit bindings.
- Class members taking precedence over unit globals, including implementation
  classes and nested procedures inside methods.
- In-memory replacements, Unicode UTF-16 coordinates, and CRLF source files.
- Unknown receivers do not trigger an unrelated workspace-wide name search.
- Document/workspace symbols, semantic references, document-local highlights,
  and semantic tokens use the standard LSP requests and preserve UTF-16 source
  ranges.
- Syntax folding covers multiline declarations, implementations, control-flow
  blocks, unit sections, comments, and balanced regions with conservative
  conditional/recovery handling.
- Structural selection ranges return bounded, cancellable inner-to-outer syntax
  chains while preserving request order and duplicate positions.
- Generic type and routine substitution/inference covers explicit and inferred
  calls, nested and inherited specializations, constructor results, consistent
  multi-parameter inference, cross-unit type identity, and method/formal
  shadowing. Unsupported or unproven constraints fail closed.
- Specialized generic results feed source-based navigation, completion, hover,
  signature help, type definitions, and overload selection, including primitive
  literal substitutions.
- Bounded, conservative conditional analysis recognizes `IFDEF`, `IFNDEF`,
  `IF`, `IFOPT`, `ELSEIF`/`ELIF`, `ELSE`, `ENDIF`, local `DEFINE`/`UNDEF`, and
  `DEFINED(...)`. Known-inactive source is omitted from the index; unknown
  branches remain ambiguous instead of being treated as inactive or uniquely
  resolved. Original source bytes remain authoritative for ranges and edits.

Not implemented or incomplete:

- Full member accessibility and Delphi declaration-order rules. This is a
  syntactic index, not a compiler-validated semantic model.
- Full compiler-equivalent conditional evaluation and include-file expansion.
  The bounded analyzer does not infer `CompilerVersion`, `IFOPT`, or other host
  compiler state. Unknown alternatives and relevant Pascal-dependent include
  content can therefore produce no navigation result or block rename.
- Full MSBuild evaluation, arbitrary `.dproj` targets, and `.delphilsp.json`
  compiler-equivalent search-path/configuration loading.
- Compiler-equivalent overload selection, auto-imports, snippets, and anonymous
  callable inference are not implemented. Completion and signature help remain
  conservative when imports, conditionals, receivers, or parser state are
  unknown.
- A general Delphi type checker is not implemented; semantic-token precision is
  limited to bindings proven by the source index.

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
flags, do not by themselves block a rename. Known-inactive unresolved includes
are skipped; active or unknown unresolved includes still block. Pascal-dependent
conditional expressions and relevant source-bearing includes remain unsupported;
missing or unreadable includes cannot be treated as evidence that no reference
exists. Source conditional compilation is projected for analysis rather than
textually expanded. Unit/module renames (which require
`RenameFile`), inherited or `with`-dependent lookup, overloaded/override relationships,
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

Conditional analysis has additional fixed safety bounds: at most 16,384
directives per source, 256 nested conditional frames, 4,096 bytes per
conditional expression, 256 expression tokens, 32,768 retained environment
entries, 1 MiB of retained environment-key bytes, 1,000,000 aggregate
environment operations, and 16 MiB of aggregate environment copy/merge byte
work. Exceeding a bound marks the source unknown rather than returning a
partial proof. Includes used by rename are separately bounded by 4,096 files,
256 MiB, 16,384 directives, and 256 nested include levels.

Assistance has fixed per-request bounds: at most 256 completion items, 100,000
scanned completion symbols, 100,000 syntax-tree nodes while locating completion
context or a call, 128 signatures, 64 KiB of signature-argument scanning, 256
nested parentheses/indexers, 128 KiB per rendered signature label, 4,096 formal
parameters, and 256 KiB of aggregate signature metadata. Completion reports
item truncation as `isIncomplete` and fails closed when context traversal cannot
finish; typed `with` context traversal is capped at 64 nested contexts and also
reports incomplete rather than guessing. Signature help fails closed when its
bounded parser or output limit is exceeded.

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

Tests include navigation fixtures, real-process framed LSP sessions, and
headless Neovim tests loading the shipped example. The assistance smoke invokes
standard `textDocument/hover`, `textDocument/typeDefinition`,
`textDocument/completion`, and `textDocument/signatureHelp` requests against a
disposable unsaved overlay, verifies UTF-16 ranges and semantic results, and
verifies that neither fixture is saved. Other Neovim tests invoke standard
`vim.lsp.buf.rename()` and `vim.lsp.buf.code_action()` and verify exact
in-memory edits. The Neovim tests skip when `nvim` is not installed; when
installed it must be version 0.11 or newer. No personal Neovim configuration is
changed.

## License

Part of lint4d; see the repository's [LICENSE](../../LICENSE), including its
Commons Clause condition. cfg-core and cfg-pascal have their own licenses.
