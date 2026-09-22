# pascal-lsp

A native, source-based Delphi/Object Pascal language server. Runs on Linux
without Windows, Wine, RAD Studio, or `DelphiLSP.exe`. This slice provides
declaration/definition navigation, source-based hover and type definitions,
document and workspace symbols, semantic references, document highlights,
semantic completion and signature help, syntax folding ranges, conservative
structural selection ranges, conservative symbol-aware rename, naming quick
fixes, and reuses lint4d and fmt4d for diagnostics and formatting.
It is not a replacement for Delphi's compiler or its complete type system.

### Source-semantic diagnostics

In addition to lint4d rules, diagnostics report
`pascal-unresolved-identifier` and `pascal-missing-member` only when the
bounded source model proves absence. It also reports
`pascal-type-mismatch` for a proven incompatible assignment and
`pascal-incompatible-argument` for a proven incompatible argument to a
source-declared routine. These are error diagnostics with stable messages:

```text
type mismatch: cannot assign '<actual>' to '<expected>'
incompatible argument: expected '<expected>', found '<actual>'
```

The type/argument pass is deliberately a small extension of the existing
overload model, not a parallel compiler. It covers assignments and bound
routine-call arguments whose types are proven from source, built-in literals,
safe numeric widening, character/string conversion, `nil` to class/interface
references, pointers, callable types, and dynamic arrays, class/interface
upcasts with known ancestry, proven indexed/dereferenced addressed storage,
proven default-property result types, and exact writable `var`/`out` arguments.
Direct property expressions are not proven writable, while proven pointer
targets and dynamic-array elements retain their addressed-storage semantics;
unsupported property storage and unresolved index children remain unknown.
Default-property result types require an accessible, unique declaration and a
proven substitution for that property's declaring generic owner; unresolved or
cyclic index aliases and incomplete index arguments remain unknown.
Grouped parameters and omitted defaults are honored.
An overload call is diagnosed only when every applicable retained candidate is
proven incompatible; one compatible, ambiguous, unsupported, or uncertain
candidate suppresses the claim. When all invalid candidates disagree about an
expected type, the argument message uses `a compatible overload parameter`.

Ranges cover exactly the offending right-hand expression or argument and are
mapped back to physical include files just like the existing semantic
diagnostics. The new codes are:

| Diagnostic | Code |
| --- | --- |
| Type mismatch | `pascal-type-mismatch` |
| Incompatible argument | `pascal-incompatible-argument` |
| Invalid explicit override | `pascal-invalid-override` |
| Missing interface implementation | `pascal-missing-interface-implementation` |

Override and interface diagnostics use the same complete source-backed ancestry,
visibility, conditional-state, generic-substitution, cancellation, and traversal
budgets as the other semantic diagnostics. An explicit `override` is reported
only when a complete superclass chain proves that no matching inherited
`virtual` or `dynamic` routine exists. An implicit class root is never treated as
an empty chain: the pass requires a complete local/source-backed `TObject` fact
or a unique complete `System.TObject`. Class/interface method identity includes
routine kind, exact parameter types, `var`/`out`/`const` modes, generic arity,
calling-convention boundaries, and function results. Supported
method-resolution clauses, uniquely resolved interface `implements`
delegations (including inherited and derived-interface targets), and inherited
implementations are followed; interface diamonds are memoized and charged on
every visit, while cyclic or depth/budget-exhausted ancestry is treated as
incomplete.

Missing-interface diagnostics are emitted only for concrete source-declared
classes with complete source-declared interface ancestry, recursively complete
providers/imports, and resolved obligation signatures. Abstract classes and
inherited abstract methods may defer obligations. The diagnostic range is the
responsible class declaration in the requesting source, never a foreign
interface declaration; include mappings retain a claim only when the physical
owner is unambiguous. The pass is bounded by the semantic diagnostic
node/work, ancestry-depth, byte, and 256-diagnostic limits described by the
existing worker pipeline.

Unsupported compiler-only `IInterface` members, unresolved or ambiguous
providers, recursively incomplete imports, unknown conditional branches,
parser recovery, unresolved obligation signatures, inaccessible or overloaded
candidates when identity is uncertain, unsupported generic constraints/variance
or calling conventions, property delegation whose target cannot be proven, and
exhausted ancestry or signature work remain silent. These checks do not attempt
to replace Delphi's compiler or to diagnose declarations that are merely
incomplete.

Aliases whose target type is not proven, unresolved by-reference identities,
non-`nil` pointer/variant/anonymous callable conversions, enum/record
compatibility beyond retained identity, unmodeled record operators,
unsupported default-property storage relationships,
compiler-dependent conversions, conditional or parser-recovery states,
unresolved values, and incomplete imports/receivers/helpers/ancestry remain
silent. A mismatch is not published merely because an unresolved implicit
`System` lookup occurred when both assignment types were independently proven;
the unresolved value itself still remains incomplete.

The resolver reuses lexical bindings, selected imports, receiver/member lookup,
helper and `with` precedence, ancestry, accessibility, and conditional state.
Unknown or ambiguous imports/receivers, incomplete `with`/helper/owner lookup,
unknown ancestry, an implicit-root runtime member surface not represented by
the source index, inaccessible members, parser recovery, unknown conditional
branches, and exhausted traversal limits are deliberately silent. The index
does not claim to contain a complete compiler-provided `System` export
catalogue: a source-backed `System` unit can establish positive bindings, but a
failed unqualified value/call lookup remains incomplete and is silent rather
than being closed by a hand-maintained intrinsic-name allowlist. When no
implicit `System` source is available, failed unqualified value/call lookups,
including assignment targets, remain incomplete because compiler-provided
writable globals may be absent from the source index. Source-backed or
ambiguous implicit namespaces likewise remain incomplete on misses. Built-in
types, casts, and type-valued intrinsic arguments are classified separately and
are not treated as unresolved value uses.
Declarations, type/generic parameters, routine directives, labels,
named-argument syntax, units, strings, and comments are not treated as value
uses. Generic-provider symbol scans are charged to the same semantic work and
byte budgets; exhaustion discards the whole semantic result rather than
publishing a partial scan. These diagnostics are source-based and do not
attempt compiler-only type inference, general type checking beyond the listed
contracts, or quick fixes.

Semantic diagnostics mapped from a physical include are retained only when
the include's root-specific binding context is unambiguous. Equal project or
configuration keys do not establish equal lexical bindings, so shared includes
used by multiple roots are deliberately silent until one context can be
proven. A competing incomplete expansion is also a conflicting owner: its
retained prefix is checked and its undiscovered suffix is treated as uncertain,
so no false unique physical claim is published. Opening or changing another
root refreshes previously published semantic claims; ordinary lint diagnostics
remain independently aggregated per root.

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

### Fix all supported naming diagnostics

The server advertises the standard `source.fixAll` hierarchy for one
proof-gated, current-document action. The supported finite subset is exactly
`constant-naming` and `local-variable-naming`; the rule-specific kinds are
`source.fixAll.constant-naming` and
`source.fixAll.local-variable-naming`. The action performs fresh analysis of
the effective document and does not trust the client's diagnostic messages or
ranges. Unsupported lint rules, including type/interface prefixes and
identifier casing, remain untouched and are not claimed fixed.

The fix builder is reused to produce byte edits. Before an edit is returned,
the LSP validates UTF-16/CRLF/non-BMP coordinates, deduplicates identical
replacements, rejects overlap or ambiguous insertion, proves each rename with
the existing binding-aware rename index, checks references and name
collisions, reparses the combined result, and confirms that the selected
naming diagnostics disappear. A declaration whose references escape the
requested physical document is withheld rather than partially renamed;
independent declarations with complete local proofs may still be batched. The
same proof also rejects swaps, chains, shadow capture, conditional uncertainty,
parser recovery, include/shared physical provenance, incomplete workspace
discovery, read-only/external files, stale configuration, and changed source
content. No file creation, resource operation, or implicit workspace-wide
batch is performed.

Eager clients receive a normal `WorkspaceEdit`; resolving clients receive the
same frozen action identity and a fresh versioned edit. Open documents retain
their checked version, while closed documents use the compatibility null
version form. Unrelated overlay changes and a no-op document version change do
not alter the action's effective source proof, but a referenced source,
configuration, binding, or collision witness changing causes resolution to
fail closed. Quickfix-only and `source.organizeImports`-only filters do not
include `source.fixAll`.

The request is bounded by a 4 MiB source, 512 supported candidates, 2,048
combined edits, 64 KiB replacement text, 4,096 retained dependency records,
the shared 64 KiB serialized code-action limit, and a request-wide 1,000,000
work-unit/32 MiB proof and materialization budget. Candidate collection stops
before constructing an over-limit batch. Cancellation, output limits, or
proof-budget exhaustion return no partial edit.

### Generate missing method implementations

Code actions also offer `Implement 'TOwner.Method'` for a source-backed class
method declaration when the indexed unit proves that its implementation is
missing. The supported finite subset is ordinary `procedure`/`function`,
`constructor`, and `destructor` declarations, including `class` (static)
methods, grouped `var`/`out`/`const`/value parameters, named types, pointers,
dynamic arrays, supported generic owners and method type parameters, and a
known function result type. Declaration-only defaults are removed from the
implementation header; parameter modes, result types, supported generic
parameter names, source spelling, and comments are retained. Declaration-only
generic constraints are omitted from implementation owner and method
references, as required by Delphi implementation syntax. Operators,
abstract/forward/external declarations, unknown calling conventions,
unsupported generic constraints, static arrays, unknown types, and
parser-recovery or conditional-uncertain declarations are withheld.

The edit is limited to the declaration's own editable physical unit. It is
inserted before `initialization`, `finalization`, or the unit's final `end.`
and never writes an include, creates a file, appends after `end.`, or guesses
across an include expansion. Existing full or abbreviated implementations,
overloads whose identity is uncertain, conditional implementations, and
include-owned provenance suppress the action. The generated body is deliberately
minimal and contains an explicit single-line `// TODO: Implement ...` comment;
its owner identity is rendered from legal generic parameter names rather than
multiline declaration text. It does not invent a function return value or
exception behavior. UTF-16 ranges, BOMs,
non-BMP text, comments, indentation, and the source's line-ending convention
are preserved.

The action is a standard `quickfix` and supports both eager edits and
`codeAction/resolve`. Resolution rechecks the source, owner, declaration,
configuration, conditional context, implementation proof, and output version;
stale or cancelled work fails closed. Traversal and serialized output remain
bounded by the assistance work/byte budgets and the 16 KiB method-header and
64 KiB code-action response limits. Uncertain source ownership, imports,
conditional state, physical mapping, or insertion safety produces no edit.

### Generate missing interface-member implementations

For a selected source-backed class declaration, code actions also offer one
scoped `Implement 'TWidget.Run'` action per proven missing interface
obligation. These actions use the hierarchical kind
`quickfix.implement-interface-method` (and are included by a `quickfix`
request); each action completes only its named obligation, not an unsupported
or unproven "implement all" operation. The private resolve-data discriminator
is `implement-interface-method`.
The action is associated with `pascal-missing-interface-implementation` when
the requesting context supplies the matching diagnostic. A declaration is
added only when the exact class member is absent; an existing exact
declaration receives only its missing body.

The proof follows complete source-backed interface and class ancestry,
instantiated generic substitutions, exact routine kind/parameters/modes/result,
calling conventions, overload identity, method-resolution mappings, and
supported interface delegation. Inherited concrete implementations suppress
generation, while abstract, compiler-only, ambiguous, conditional, parser-
recovery, incomplete-import, unresolved-type, unsupported-generic, and
unsupported-calling-convention contexts remain silent. Standalone implicit
`TObject` is an open compiler-member namespace: diagnostics do not claim an
interface method is absent, and a missing declaration is withheld until a
source-backed authoritative root is available. An exact direct class
declaration may still receive its missing body; compiler-provided
`IInterface`/`System` members are never invented. Interface diamonds are
deduplicated only when their proven obligation and mapped implementation are
identical.

New declarations use or create a `public` class section without changing an
existing member's visibility. Same-name fields, properties, private members,
or incompatible routines require a safe exact collision/overload proof;
otherwise the action is withheld. The bounded workspace edit is restricted to
the authorized physical source, combines declaration and implementation
insertions deterministically, preserves BOM/CRLF/UTF-16/comments, and is
parsed and pair-checked before it is returned. No include, new file, existing
body, return value, or partial uncertain obligation is written. Eager and
deferred forms revalidate the selected class/interface, provider records,
source/configuration generations, insertion structure, and negative missing-
implementation proof; stale or cancelled resolution fails closed.

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

Stdio output is nonblocking and ordered. The writer-facing queue keeps a
16-message partial-result-data reserve and a 260-message control reserve,
with 16 MiB total pending bytes and an 8 MiB per-message/aggregate pending
control-byte bound. When a valid ordinary response temporarily cannot fit,
it is retained in a separate FIFO deferred queue rather than terminating the
session: deferred results are capped at 260 messages and 64 MiB, deferred
lifecycle/control output at 260 messages and 8 MiB, and the combined deferred
queue at 520 messages and 72 MiB. A result is retired from analysis/admission
accounting only after its response (or a bounded request-scoped fallback) has
been accepted by one of those queues. Individual control/result messages still
must fit the 8 MiB bound; an oversized ordinary result receives `-32803`
without closing the session. Partial data is not accumulated in the deferred
queue, so a paused client cannot create an unbounded retry loop. Shutdown
abandons undrainable output only after the existing bounded teardown deadline.

### Work-done progress and cancellation

The server advertises work-done progress for supported long-running providers,
including document/workspace symbols, references, semantic tokens, navigation,
formatting, rename, and code actions. A request may supply the standard
`workDoneToken` as a string or integer; this is separate from
`partialResultToken` and the JSON-RPC request ID. Accepted request work begins
with one `$/progress` `begin`, reports bounded queued/started stages without
invented percentages, and ends exactly once after success, an error, a stale
result, or cancellation. Reports are operation-level rather than one per
scanned file.

Server-initiated indexing/diagnostic progress is used only when the client
advertises `window.workDoneProgress: true`. The server first sends
`window/workDoneProgress/create` and withholds progress notifications until a
successful response. Failed, duplicate, unknown, or late create responses are
ignored without delaying analysis. Terminal work retires an unacknowledged
create's active token immediately and retains only a bounded response
tombstone, so a late acknowledgement cannot revive `begin`/`report` activity.
At most 32 create requests/tombstones and 128 progress tokens are retained;
once the create bound is saturated by ignored requests, further diagnostics
continue without server-initiated progress until a response or shutdown
releases a slot. Create IDs use a separate `pascal-lsp-progress-create-`
namespace from configuration request IDs.

`$/cancelRequest` cancels the exact request recipient. The server also accepts
`window/workDoneProgress/cancel` for an active progress token; unknown or
finished tokens are harmless. Coalesced requests retain independent progress
tokens, so cancelling one recipient does not cancel the shared computation
while another recipient remains. Cancelling server-initiated indexing cancels
the diagnostic snapshot and permits a later fresh retry; worker snapshots are
never published as a partially built complete index.

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

### Pull diagnostics

Diagnostics use the standard pull requests when the client advertises
`textDocument.diagnostic`. In that mode the initialize response includes a
`diagnosticProvider` with identifier `pascal-lsp`,
`interFileDependencies: true`, and work-done support. The normal capability
shape also advertises `workspaceDiagnostics: true` when the client advertises
the canonical LSP `workspace.diagnostics` capability (the pinned
`lsp-types` version's legacy singular spelling is accepted too). The evidenced
Neovim 0.12.5 client is kept on document-pull refresh because it tracks
attached buffers as document reports while ignoring workspace reports for
those buffers. The exception is exact and versioned: versionless, unknown, and
newer Neovim clients receive the canonical workspace provider. Clients that do not
advertise document pull keep the existing debounced
`textDocument/publishDiagnostics` push path instead; a negotiated client never
receives both ownership models.

`textDocument/diagnostic` returns a `full` report with an opaque `resultId`, or
an `unchanged` report when the supplied `previousResultId` identifies the same
validated effective diagnostics, source/context fingerprint, and dependency
observations. Version fields and disk stamps are freshness observations, not
effective-report identity: workers re-read and revalidate dependencies before
reusing a no-op report. Result IDs are process-local capabilities, not hashes
that clients should construct. Unknown, foreign, or evicted IDs always receive
a fresh full report. Empty diagnostics are still full reports so they clear an
editor's old state. Pull workers validate disk freshness in the worker rather
than trusting a watcher notification, and stale results return `-32802` with
`{ "retriggerRequest": true }`; explicit cancellation remains `-32800`.

When `relatedDocumentSupport` is negotiated, a document report may include
full reports for owned physical include/dependent documents. Include ranges are
mapped to their physical URI and UTF-16 positions. The server retains bounded
root-to-physical ownership, emits empty related reports when a contribution is
removed, and unions still-current contributions from other roots. Reports are
omitted when ownership is conflicting or the source is not authorized, so an
include cannot leak another project context. Workspace pulls use
`workspace/diagnostic` and
`previousResultIds` to return deterministic URI-sorted reports for open buffers
and authorized unopened Pascal sources. Deleted or newly excluded files that
were present in the previous set receive an empty full report to clear them;
the server does not read an unauthorized URI merely because the client supplied
its previous result ID. An incomplete bounded workspace discovery fails the
request instead of silently returning a complete-looking partial scan.

Changes that affect a diagnostic dependency, overlay, project/configuration
context, workspace membership, or source freshness invalidate the corresponding
pull result. Watched source transitions, provider/catalogue changes, workspace
folder changes, configuration changes, and closing an overlay request a refresh
for pull clients when appropriate, including for unopened authorized files. If
the client advertises `workspace/diagnostic/refresh`, the server coalesces
affected changes and keeps one bounded refresh request in flight; late,
duplicate, rejected, and shutdown-time replies are harmless, and a refresh
response never starts a refresh loop. Unrelated changes reuse valid reports.
Workspace diagnostic requests may use a `partialResultToken`; bounded
`$/progress` chunks are delivered separately from the final report and
work-done progress. Cancellation or a stale race after partial delivery never
produces a successful complete report.

Pull state is bounded independently of the source catalogue: at most 2,048
diagnostic result entries and 32 MiB of retained result/cache state are kept.
Worker-prepared dependency evidence is capped at 32,768 records and 64 MiB per
analysis. Optional result-cache admission remains capped at 8 MiB; cache
pressure disables unchanged-result reuse, while evidence pressure fails with a
retryable diagnostic response before any partial result is published.
Workspace reports are limited to 10,000 document items, 64 KiB per encoded
item, and 7 MiB encoded output. Partial workspace chunks are limited to 128
items and 64 KiB, with at most one chunk pumped per event-loop turn. The
existing 8 MiB LSP message bound remains the final response/output guard.

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
imported declarations. It preserves declaration casing and normally returns
plain identifier `TextEdit`s. Clients that advertise
`textDocument.completion.completionItem.snippetSupport` also receive
source-derived routine-call snippets at an unambiguous callable expression
site, such as `Run(${1:Value})$0`; zero-argument routines use `Run()$0`.
Grouped, default, and optional parameters become ordinary placeholders in
declaration order, while escaped Pascal identifier text and LSP snippet
metacharacters are preserved safely. Snippets remain plain in existing calls,
address-of/procedure-reference expressions, expected procedural-value
assignments or arguments, declarations, type contexts, ambiguous or unknown
signatures, non-expression/qualification positions, and uncertain syntax. A
typed generic suffix and an unresolved routine generic also remain plain; the
server does not invent parameters or statement terminators. For an unqualified
identifier, it
can also offer a unique exported declaration from an authorized project source
unit that is not already imported. Routine auto-import items retain their safe
`additionalTextEdit` for the interface or implementation `uses` clause; the
import edit remains separate from the primary snippet replacement. The edit
preserves CRLF/LF style, existing comments, dotted names, and explicit
`in 'path'` entries. The proposed import spelling is checked through the
requesting project's actual unit resolver, including namespaces, search paths,
and aliases; candidates are omitted when it cannot bind uniquely to the
selected provider. Absent clauses are inserted only after a clean section
header, never through comments, directives, or same-line routine headers.
Auto-import discovery is bounded to 512 provider source units and reports
`CompletionList.isIncomplete` when the authorized filename catalogue is
incomplete; cancellation and read-policy failures fail closed. Ambiguous
units/symbols, conditional or private declarations, the current unit,
compiled-library-only symbols, malformed/conditionally enclosed `uses` clauses,
and unsafe insertion points are omitted. Conditional uncertainty, ambiguous
receivers, and bounded candidate truncation are reported conservatively with
`CompletionList.isIncomplete` rather than as a falsely complete result.
Expression receivers are resolved source-first as well: calls such as
`MakeValue().Member`, constructors such as
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

The server advertises `completionProvider.resolveProvider`. Completion metadata
is negotiated independently: `documentation` and `detail` are deferred only
when the client lists the corresponding property in
`completionItem.resolveSupport.properties`; unsupported properties are computed
eagerly for compatibility. Deferred `completionItem/resolve` runs in the
analysis worker and returns the negotiated documentation format. The server
issues bounded opaque, session-local data and resolves the exact declaration
URI/index captured for the item. It restores the original label, kind, edits,
`insertText`, filtering, and sorting fields instead of trusting presentation
changes made by the client; resolution never adds or changes edits. Missing,
tampered, foreign, evicted, stale, or ambiguous identities fail conservatively
so the client can request completion again.

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
including radix integers and concatenated string fragments (a single certain
logical character is `Char`), typed variables/parameters, defaults, and
`var`/`out` lvalue requirements. Character values can match string parameters,
but ordinal numeric conversion requires an explicit source call such as
`Ord(value)`. Integer literal ranges and declared
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
only when you explicitly configure a mapping. The LSP and lint4d's
`--project` source-CFG path consume these files through `pascal-project`.
`fmt4d` and lint4d runs without `--project` do not use them for source
discovery. lint4d's `--bds-path` remains a DCU auto-discovery override, not a
source-path mapping option.

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
references still do. Project defines and explicit conditional facts are carried
into source analysis; `buildConfig` and `platform` select the project context
but do not synthesize compiler-version or host-environment facts. Conditional
facts are merged with this precedence: valid runtime settings override
initialization settings, caller-provided settings override project metadata,
and project metadata fills only facts that the caller left unknown. Source
`DEFINE` and `UNDEF` directives then update the context in source order, and
the resulting facts are inherited by nested and repeated include occurrences.

### Source-bearing Pascal includes

The LSP expands resolvable `{$I ...}`/`{$INCLUDE ...}` files into bounded,
virtual source buffers for navigation, references, document highlights, and
open-buffer diagnostics. Include declarations, definitions, and diagnostics
are mapped back to their physical `.inc` (or Pascal) URI and source range;
repeated and nested includes retain every physical occurrence. Unsaved open
buffers take precedence over disk files, including when an included file is
opened only as an overlay. Include content carries the requester project's
read authorization and mapping provenance through nested resolution, and
changes to an include invalidate its indexed parents and published diagnostics.

Expansion is deliberately conservative. Active includes must resolve within
the effective readable roots; cycles, unknown or incomplete conditional
activity, stale dependency content, and expansion depth/file/directive/byte/
source-map/work limits fail closed rather than producing partial locations or
edits. Project and local `DEFINE`/`UNDEF` facts are shared across expanded
source in the bounded analyzer. Explicit compiler-version and `IFOPT` facts
are also shared, while host environment values and Pascal-dependent
expressions remain unknown unless proven by the supported source subset. `.inc`
files are analyzable source dependencies, but formatting is restricted to
`.pas`, `.dpr`, and `.dpk` files so the formatter never edits a virtual or
include fragment implicitly.

### Compiler conditional context

The initialization and `pascalLsp` runtime settings accept the following
explicit conditional-context fields:

```json
{
  "compilerVersion": "24.0",
  "compilerOptions": {"R": true, "Q": "off", "Unknown": null},
  "conditionalDefines": ["FEATURE"],
  "conditionalUndefines": ["LEGACY"],
  "conditionalConstants": {
    "BuildNumber": 42,
    "ProductName": "Shop",
    "IsTrial": false
  }
}
```

`compilerVersion` is parsed as a bounded exact decimal number (for example `24`,
`24.0`, or `18.50`); trailing fractional zeroes are normalized and dotted
patch input such as `24.0.1` is rejected because Delphi's value is numeric, not
a semantic-version tuple. It is never parsed as a binary floating-point value.
Option values accept booleans, `on`/`off` (and their true/false spellings), or
`null` for `Unknown`. Runtime JSON constants are booleans, integers, or
strings. Missing facts are `Unknown`; the server never infers them from the
host OS, current date, Rust toolchain, or an unrelated target platform.

Project discovery additionally reads explicit `CompilerVersion`/
`DCC_CompilerVersion` metadata and a bounded set of option properties such as
`DCC_RangeChecks`, `DCC_OverflowChecks`, `DCC_Optimization`,
`DCC_Assertions`, `DCC_RuntimeChecks`, and `DCC_DebugInformation`. Explicit
client facts win over these properties; unsupported or malformed metadata
stays unknown and is reported as a warning. The same context identity is part
of project, parsed-source, include-expansion, package, and completion caches,
so two projects can analyze the same physical source independently.

The evaluator deliberately implements a finite, typed subset of Delphi
conditional expressions: boolean, decimal/hex integer, string, and version
literals; named explicit constants; `CompilerVersion`; `Defined`, `Declared`,
`Length`, `Ord`, and `SizeOf`; unary `not`, `+`, and `-`; arithmetic `*`,
`/`, `div`, `mod`/`%`, `shl`, and `shr`; boolean/bitwise `and`, `or`, and
`xor`; and typed relational comparisons (`=`, `<>`/`!=`, `<`, `<=`, `>`,
`>=`). Delphi `/` is real division and therefore remains unknown in this
integer-only evaluator. Unsupported functions and types, malformed syntax,
overflow, zero divisors, conflicting branch facts, and any safety-bound
exhaustion remain `Unknown` rather than being guessed. Source constants are
admitted only for the conservative unit/program-level global-scope heuristic;
local constants and arbitrary Pascal initializers are not treated as
compiler-wide facts. This is source assistance, not compiler-equivalent
conditional evaluation.

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
Include files (`.inc`) participate in source analysis but are rejected as
direct formatting targets; only `.pas`, `.dpr`, and `.dpk` buffers are
formatted.
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

### Runtime configuration (LSP)

The canonical runtime settings section is `pascalLsp`. It uses the same
supported fields as `initializationOptions`; it does not introduce a second
configuration schema:

```json
{
  "pascalLsp": {
    "projectFile": "src/Shop.dproj",
    "buildConfig": "Debug",
    "platform": "Win32",
    "sourcePaths": ["../shared", "/opt/delphi/rtl"],
    "exclude": ["**/__history/**"],
    "compilerVersion": "24.0",
    "compilerOptions": {"R": true, "Q": "off"},
    "conditionalDefines": ["FEATURE"],
    "conditionalUndefines": ["LEGACY"],
    "conditionalConstants": {"BuildNumber": 42},
    "maxFiles": 10000,
    "maxFileBytes": 2097152,
    "maxTotalBytes": 268435456
  }
}
```

Clients that advertise `workspace.configuration: true` receive a standard
`workspace/configuration` request for section `pascalLsp` after the server has
received `initialized`. A single-root workspace request includes that root's
`scopeUri`. Runtime settings are global in this server: a multi-root request
omits `scopeUri`, and one returned setting set applies to every workspace root.
Adding or removing workspace folders refreshes this scope decision.
The pull response must contain one array item for this one requested section;
`null` is the explicit reset value, while malformed or wrong-length array
responses leave the last valid runtime state unchanged.

Clients without pull capability do not receive a server request. They can
push the same object in `workspace/didChangeConfiguration`, for example:

```json
{
  "settings": {
    "pascalLsp": { "buildConfig": "Release" }
  }
}
```

Runtime values override the corresponding initialization option for the
session. Existing project ownership and explicit session-local project
selections remain authoritative, and the existing project metadata, sidecar,
read-policy, and open-buffer precedence is unchanged. A `null` value or an
omitted field resets that field to its initialization fallback; a `null`
section (or an empty section) resets all runtime overrides. A malformed whole
section is ignored. A malformed individual field is retained at its previous
valid value while valid unrelated fields still apply. Unknown fields are
ignored. Runtime changes that alter effective values bump source/configuration
generations, invalidate affected indexes and contexts, and reschedule open
document diagnostics; identical effective settings do not rebuild state.
Expensive reads and rebuilds remain in the bounded analysis workers, and
runtime settings never grant filesystem access beyond the existing read
policy.

While a runtime configuration rebuild is in progress, configuration-dependent
requests and state-changing notifications share a bounded FIFO of 64 messages.
At most 63 retryable requests are admitted initially, leaving room for an
authoritative notification; if later notifications need more room, the newest
deferred request is rejected with a retryable `-32802` response rather than
dropped edits. The queue remains count-bounded, and each retained LSP payload
is subject to the server's 8 MiB input limit. Cancellation and shutdown still
drain deferred requests with explicit responses.

The coordinator coalesces refresh notifications, keeps at most one pull in
flight, and discards obsolete or duplicate replies. Pull errors preserve the
last valid runtime state. Runtime lists are capped at 256 entries and strings
at 4 KiB; numeric limits are clamped to the safe caps in the Limits table.
The historical `pascal-lsp` spelling is accepted as a compatibility alias,
but new client configurations should use `pascalLsp`.

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
and module identifiers are supported when the resolver proves the selected
unit identity, including explicit project aliases. Unit declarations, `uses`
entries, and qualified references use the complete bound prefix (`Ns.Provider`
or `Alias` in `Alias.TThing`) rather than an arbitrary component. Unit/module
rename remains unsupported because it requires `RenameFile` support.

If workspace discovery, a required source/include read, or binding resolution
is incomplete, the request returns an actionable error rather than a partial
location list. Unsupported or ambiguous bindings are rejected rather than
guessed; inherited or `with`-dependent lookup, unknown class ancestors, and unsupported
overload relationships can therefore make a reference request fail.

### Partial results

`workspace/symbol` and `textDocument/references` accept the standard
`partialResultToken` in either its string or integer form. When supplied, the
server first computes and validates the complete bounded snapshot, then sends
ordered result-array chunks as `$/progress` notifications using that token. A
chunk contains at most 128 items and 64 KiB of encoded JSON (an individual item
may not exceed the same byte bound); delivery sends at most one chunk per
protocol turn and uses the bounded outbound writer queue without blocking the
protocol thread. Workspace-symbol results retain their 10,000-entry limit.

References are never streamed as soon as plausible matches are discovered:
workspace membership, imports, binding identity, declaration inclusion, and
all other completeness checks must succeed before the first reference chunk is
published. This prevents a partial location list from being mistaken for a
complete safe result. A source or configuration change during delivery stops
the stream and returns a stale-result error; cancellation likewise stops future
chunks and returns the request-cancelled error.

Partial delivery is also subject to the global 33-recipient client-analysis
admission bound: queued, running, coalesced, and already-delivering recipients
all count, so a request beyond the bound receives a retryable queue-capacity
error rather than creating unbounded retained work. Retained staged payloads
are bounded to 64 MiB as well; a cancelled freshness validator continues to
consume its bounded worker slot and retained-byte charge until it actually
retires.
An individual item larger than 64 KiB fails only its own partial request with a
request-failed error; it does not terminate the LSP session or affect ordinary
requests. Partial-result data uses nonblocking bounded output with reserved
capacity for responses, cancellation, and progress control messages. Session
shutdown performs a bounded output drain instead of waiting indefinitely for a
client that stopped reading.

After all chunks, the successful final response is an empty array so items are
not duplicated. Empty results use only that final empty response. Without a
`partialResultToken`, the ordinary complete array response is unchanged when it
fits the bounded outbound control budget; an otherwise valid encoded response
larger than 8 MiB returns a request-scoped error rather than terminating the
session. Partial tokens are independent of work-done progress tokens, request
IDs, and server IDs; active collisions are rejected and finished tokens may be
reused. Coalesced recipients retain separate partial tokens and cancellation
lifecycles.

### Document highlights

`textDocument/documentHighlight` is restricted to the requested document and,
for supported bindings, includes its declaration. Unit/module bindings use the
same selected identity and complete bound-prefix ranges as references,
including project aliases. Every returned highlight has a classified `kind`:
declarations and routine/type/unit references are `Text`; proven right-hand
values, bases, indices, receivers, and value arguments are `Read`; proven
storage assignments and `var`/`out` arguments are `Write`. Ambiguous or
unsupported calls, properties, addresses, and other uncertain storage remain
`Text` rather than being guessed. Both references and highlights fail closed at
the 10,000-entry response bound and honor cancellation and bounded snapshot
freshness. The shipped Neovim configuration does not install `CursorHold` or
`CursorMoved` autocmds; clients request highlights explicitly, for example
with `vim.lsp.buf.document_highlight()`.

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

### Source documentation comments

Hover, completion items, and signature help include source documentation when a
documented comment is attached to the resolved declaration. A standalone
Delphi `///` comment immediately before a declaration is attached; consecutive
`///` lines may be joined across one line break. Block comments are attached
only when they contain a supported documentation tag and are also standalone.
Trailing comments, blank-line-separated comments, compiler directives,
license/file-header comments, and comments belonging to another declaration do
not attach. For a uniquely proven routine declaration/implementation pair,
documentation on the visible declaration is preferred and implementation
documentation is used only when the declaration has none. Overloads, helpers,
generic specializations, and cross-unit declarations retain their resolved
symbol identity.

The supported XML-style subset includes `summary`, `remarks`, `param` (matched
case-insensitively by name, including grouped parameters), `returns`, `code`/`c`,
`paramref`, and `see`/`cref`. Basic XML entities are decoded; malformed or
unsupported markup degrades to safe text, without external entity, file, or
network access. Markdown and plaintext are selected independently for hover,
completion documentation, and signature documentation according to the
client's advertised formats, with plaintext as the fallback. Signature help
also exposes the matching parameter's documentation separately.

Documentation processing is bounded and cancellable:

| Documentation limit | Value |
| --- | ---: |
| Source scan | 2 MiB |
| Scanned comments | 8,192 |
| Individual comment body | 64 KiB |
| Individual XML tag | 4 KiB |
| XML nesting depth | 64 levels |
| Aggregate parsed documentation storage | 128 KiB |
| Aggregate parser expansion work | 256 KiB |
| Rendered documentation | 64 KiB |

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
- Unit references and highlights preserve selected provider identity, explicit
  project aliases, and complete bound prefixes; unit rename remains unsupported.
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
- Conservative source-semantic type diagnostics cover proven assignment
  mismatches and bound routine-call argument mismatches, including overload
  exclusion, grouped/default parameters, writable `var`/`out` checks, generic
  substitutions, inherited/helper calls, physical include ranges, overlays,
  cancellation, stale publication, and fail-closed diagnostic budgets.
- Bounded, conservative conditional analysis recognizes `IFDEF`, `IFNDEF`,
  `IF`, `IFOPT`, `ELSEIF`/`ELIF`, `ELSE`, `ENDIF`, local `DEFINE`/`UNDEF`, and
  `DEFINED(...)`. Known-inactive source is omitted from the index; unknown
  branches remain ambiguous instead of being treated as inactive or uniquely
  resolved. Original source bytes remain authoritative for ranges and edits.

Not implemented or incomplete:

- Full member accessibility and Delphi declaration-order rules. This is a
  syntactic index, not a compiler-validated semantic model.
- Full compiler-equivalent conditional evaluation is not implemented. The
  bounded include expander uses only explicit `CompilerVersion`, `IFOPT`,
  define, and constant facts plus its finite typed expression subset; it does
  not infer host compiler state. Unknown alternatives and Pascal-dependent
  include expressions therefore produce no navigation result, withhold
  diagnostics, or block rename rather than being guessed.
- Full MSBuild evaluation, arbitrary `.dproj` targets, and `.delphilsp.json`
  compiler-equivalent search-path/configuration loading.
- Compiler-equivalent overload selection and anonymous callable inference are
  not implemented. Completion and signature help remain conservative when
  imports, conditionals, receivers, or parser state are unknown; snippets are
  limited to source-proven named routines and do not model anonymous or
  compiler-only variadic callables.
- A general Delphi type checker is not implemented; type diagnostics are
  limited to the proven assignment/argument subset described above, and
  semantic-token precision is limited to bindings proven by the source index.

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

The same request can provide one-obligation interface implementation actions.
Their wire kind is `quickfix.implement-interface-method`, a child of
`quickfix`; the private resolve-data discriminator is
`implement-interface-method`. Each action is titled `Implement 'TWidget.Run'`
and uses data version `1`. The planner only emits a source-backed, editable
class/interface obligation whose instantiated signature, ancestry, overload
legality, provider identities, and generated declaration/body pair remain
proven after reanalysis. Unsupported signatures and ambiguous provider or
collision facts are withheld rather than guessed. An implicit compiler
`TObject` remains an open member namespace: missing declarations are withheld
until a source-backed authoritative root is available, while an exact direct
public or published class declaration can still receive its missing body. An
inherited conformance result never authorizes a private, strict-private, or
protected direct shadow; ordinary declaration-selected method actions remain a
separate operation. Deferred interface actions recheck provider/type identity,
including supported transitive constant/enum-value dependencies used by type
bounds and defaults; cycles, limits, or unresolved value dependencies withhold
the action. Source and configuration freshness and the serialized response-size
limit are also rechecked.

The same `quickfix` request can offer `Add unit '<Unit>' to uses` for an
unqualified identifier that is proven unresolved by the source resolver. The
suggestion is limited to one accessible, source-backed exported declaration in
one uniquely identified provider unit, with one independently proven action per
provider unit. Namespaced unit names are retained in the proposed `uses`
entry. Existing imports, local/shadowed or already-resolved names, member and
`with` expressions, inaccessible or ambiguous providers, compiler-only/System
names without source evidence, unknown conditionals, incomplete imports, and
include-owned or otherwise unsafe `uses` clauses are withheld. The safe edit
planner preserves the detected newline style and only edits a simple active
interface or implementation `uses` clause (or inserts a new clause at a safe
section boundary); it never writes files. These actions carry bounded source,
provider, configuration, project, and conditional freshness evidence, and are
revalidated before eager delivery and deferred `codeAction/resolve`.

For safety, rename is refused rather than returning a partial edit when the
workspace scan is incomplete, an import/project context is unresolved or
ambiguous, a source path is a symlink escape, or an edit would touch an
external `sourcePaths` file. Include lookup searches the including file's
directory, evaluated `DCC_IncludePath`, then ordered unit/client source paths.
Nested includes are audited within depth, file, directive, and byte limits;
lookup observations and content hashes participate in stale-input checks.
Comments and recognized compiler-only directives, including conditional compiler
flags, do not by themselves block a rename. Fully resolved and audited active
source-bearing includes may contribute physical edits in their `.inc` or Pascal
sources; the planner maps virtual edits back to those files and rejects
synthetic, cross-segment, external, stale, or incomplete edits. Known-inactive
unresolved includes are skipped; active or unknown unresolved includes still
block. Pascal-dependent conditional expressions and incomplete include audits
remain unsupported; missing or unreadable includes cannot be treated as
evidence that no reference exists. Source conditional compilation is projected
for analysis rather than textually expanded. Unit/module renames (which require
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

The provider export must match the use-site role: types require a source type,
call sites require a routine, and value/assignment sites require a compatible
constant, variable, or enum value. Interface imports are withheld when the
proposed edge forms a direct or transitive interface cycle, or when the
bounded dependency graph is incomplete; implementation-local back-edges remain
eligible. Project aliases, search paths, read policy, conditional state, and
the actual resolved import are checked again for deferred resolve. A no-op
buffer version change or unrelated overlay does not expire an otherwise
unchanged action, while target/provider/project changes do.

`source.organizeImports` is advertised as a separate source action. It never
removes an import merely because it appears unused. A duplicate is removable
only when every selected entry resolves to complete source-backed providers in
one path-free clause, the providers have no dependency/helper/finalization
hazard, exported names are disjoint, and the removed spelling is not used as a
qualified reference. Project and namespace aliases therefore require a proof
of the resolved provider *and* of the removed spelling; the qualified-use
check is parsed-syntax based and covers escaped, spaced, commented, and
namespace-qualified paths. The extraction walks each top-level qualified path
once, charges node storage/source bytes, polls cancellation, and withholds
paths beyond 128 qualified-path nodes. Unsupported, deep, or ambiguous alias
forms are withheld rather than guessed. The retained first occurrence and the
relative order of distinct providers are preserved. Relative alphabetical
ordering is offered only when every selected provider is complete source, has
no initialization/finalization section or helper, has no dependency closure,
and has no exported-name conflict. Otherwise the original relative order is
retained. Interface and implementation `uses` clauses are planned
independently and are never merged.

The organizer preserves the clause's original text outside the changed unit
names/delimiters, including LF/CRLF, indentation, BOM/non-BMP text, and path
spelling. Conditional/compiler-directive clauses, comments, path-qualified
uses, include or
multi-root provenance, parser recovery, malformed layouts, unresolved or
ambiguous providers, and incomplete discovery are withheld rather than
rewritten; this deliberately supported subset avoids moving trivia across a
structural boundary. The action is idempotent and is returned only when a
proven edit exists. Clients with code-action data/resolve support receive a
deferred action and the server rechecks the exact clause hashes, every
selected-clause spelling-to-provider/context binding, provider identity/source
hashes, conditional and ordered proof, configuration, source, and negative
discovery observations. Effective project alias/configuration changes therefore
expire the action even when the provider URI set is unchanged. Unrelated
global generations and no-op overlay versions do not expire a proof when those
effective observations remain unchanged. Other clients receive an eager edit.

## Limits

| Initialization option | Default and maximum |
| --- | --- |
| `compilerVersion` | Optional exact decimal compiler version |
| `compilerOptions` | Up to 256 named `IFOPT` facts |
| `conditionalDefines` / `conditionalUndefines` | Up to 256 symbols each |
| `conditionalConstants` | Up to 256 boolean, integer, or string constants |
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
entries, 1 MiB of retained environment key/value payload bytes, 1,000,000 aggregate
environment operations, and 16 MiB of aggregate environment copy/merge byte
work. Exceeding a bound marks the source unknown rather than returning a
partial proof. Includes used by rename are separately bounded by 4,096 files,
256 MiB, 16,384 directives, and 256 nested include levels.

Source-semantic diagnostics have additional per-request bounds:

| Semantic diagnostic limit | Value |
| --- | ---: |
| Syntax-tree nodes visited | 100,000 |
| Resolver/overload work units | 100,000 |
| Source bytes charged | 8 MiB |
| Published semantic diagnostics | 256 |
| Overload groups considered per call | 128 |
| Arguments considered per call | 256 |

Cancellation or exhaustion discards the complete semantic result; it never
publishes a partial type or argument analysis. These bounds are shared with
the existing binding, import, receiver, generic-substitution, ancestry, and
overload work performed for the document.

Assistance has fixed per-request bounds: at most 256 completion items, 100,000
scanned completion symbols, 100,000 syntax-tree nodes while locating completion
context or a call, 128 signatures, 64 KiB of signature-argument scanning, 256
nested parentheses/indexers, 128 KiB per rendered signature label, 4,096 formal
parameters, and 256 KiB of aggregate signature metadata. Completion reports
item truncation as `isIncomplete` and fails closed when context traversal cannot
finish; typed `with` context traversal is capped at 64 nested contexts and also
reports incomplete rather than guessing. Signature help fails closed when its
bounded parser or output limit is exceeded. Deferred completion state retains
at most 2,048 items and 8 MiB in one server session, with compact dependency
observations capped at 1,024 records and 2 MiB. Resolve data is capped at 512
bytes per item and retained completion items at 64 KiB; oldest entries are
evicted to stay within the bounds.
Organize-imports planning is capped at 64 `uses` clauses, 512 entries, 64 KiB
per clause, and 64 KiB of generated edit text in one request. Provider facts
use request-local shared immutable cache entries; export-conflict comparisons,
variable-length identity work, bounded ordering, and edit planning share the
request-wide 300,000-work-unit and 8 MiB source-byte assistance budget. Parsed
qualified-alias paths are limited to 128 dot nodes and fail closed when the
shape, depth, node-storage, source-byte, or cancellation bound is uncertain.
Cancellation or exhaustion withholds the optional action. Its serialized
code-action response remains subject to the shared 64 KiB response bound.
Missing-unit discovery, absence checking, and every post-edit binding proof
share one request-wide budget of 300,000 bounded work units and 8 MiB of
charged source bytes. The request returns at most 32 provider actions and the
actual serialized code-action result (including escaped titles, resolve data,
and eager edits) is capped at 64 KiB; identities are limited to 256 bytes for
names/unit names and 4 KiB per URI. Cancellation or exhaustion fails closed
when provider discovery, authoritative import resolution, cycle checking, or
edit planning cannot finish.

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
