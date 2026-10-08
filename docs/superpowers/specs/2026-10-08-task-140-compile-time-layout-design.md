# TASK-140: compile-time type layout and library-unit navigation

- Date: 2026-10-08
- Status: architecture agreed in chat; written spec awaiting user review
- Task: TASK-140

## 1. Intent and constraints

Restore all 13 unit-definition jumps from
`Projects/ChainDriveAPI/src/Database/CDAPI.Database.Centrale.pas`, and fix the
underlying compiler-directive, `SizeOf`, and constant-initializer include cases.
Header-only navigation is a safety-preserving fallback, not a replacement for
working conditional evaluation.

The user explicitly selected the full `SizeOf` operand list: built-in types,
user-defined aliases, records, arrays, and variables. The agreed design includes
scope and imported-type resolution, record packing/alignment/variant sections,
and field, indexed, and dereferenced variable operands.

Work stays in the current checkout. Preserve TASK-139's uncommitted project-name
fix and tests. Do not change the real ChainDriveAPI sources, Delphi installation
sources, editor configuration, or installation mappings. Obsolete project
references remain separate work. This spec changes no product code and does not
authorize implementation before written-spec and implementation-plan approval.

## 2. Confirmed failures

| Import | Failure | Required correction |
| --- | --- | --- |
| `FireDAC.Phys.IB` | A conditional compiler-directive preamble prevents the raw parse from retaining the unit declaration. Resolver candidate validation consequently rejects the existing source. | Recognize a proven source declaration despite a directive-only preamble; preserve candidate identity and conditional safety. |
| `SysUtils` | `{$IF SizeOf(Extended) >= 10}` is explicitly unsupported. Incomplete analysis turns the whole file, including the preceding header, into unknown activity. | Evaluate `SizeOf` from proven type and target facts; do not suppress an independently proven header because of later unsupported body syntax. |
| `mormot.core.base` | `SYNOPSE_FRAMEWORK_VERSION = {$I ..\mormot.commit.inc};` crosses an include boundary. The constant scanner sees a segment without the initializer's terminating semicolon and invalidates whole-file analysis. | Carry a partial declaration through include expansion and evaluate its completed initializer in source order. |

The diagnostic probe in `/tmp/opencode/task140-probe` reproduced these failures
using public APIs and unchanged source files. Its output is
`/tmp/opencode/task140-root-causes.out`. Those temporary files are supplementary
evidence, not dependencies of the design or regression suite.

## 3. Architecture decision

Use a shared compile-time binding/layout layer in `pascal-core`, fed by
source-order conditional/include processing. `pascal-project` supplies immutable
compiler/target facts. Filesystem access remains with the existing resolver and
caller-owned source stores.

Rejected alternatives:

- An LSP-only `SizeOf` implementation would leave the CLI and resolver evaluating
  the same source differently and introduce a preprocessing/indexing cycle.
- Expanding the current ad-hoc constant scanner into a full type resolver would
  mix token scanning, scope rules, target ABI rules, and include state in the
  already-large `conditional.rs`.

Existing LSP type helpers remain useful references for declaration extraction
and name resolution, but their private `TypeShape` drops static-array bounds and
does not represent record layout. Do not move the entire navigation index into
the evaluator or undertake an unrelated navigation refactor.

### Ownership

| Component | Responsibility |
| --- | --- |
| `pascal-project` target facts | Selected compiler/dialect and platform; verified target storage/alignment defaults. No Pascal parsing or source reads. |
| `pascal-core` declaration/binding layer | Source identities, scopes, declaration order, type definitions, variable types, and constant facts. |
| `pascal-core` layout solver | Resolve a bound type to proven size/alignment or a structured unknown reason. No filesystem or host-layout inference. |
| Shared conditional/include session | Source-order directive decisions, declaration continuation, include entry/exit, and branch-state merges. |
| Resolver and consumer adapters | Authorized source/provider lookup, source maps, freshness observations, and adapter-specific delivery policy. |

Keep declaration ingestion, binding, and layout arithmetic in focused modules,
rather than adding all three responsibilities to `conditional.rs`. A data-only
target descriptor belongs below `pascal-core` in the existing dependency graph;
the graph must not acquire a `pascal-project`/`pascal-core` dependency cycle.

## 4. Facts, identities, and evaluation results

The layout solver returns one of:

- **Known:** storage size, alignment, resolved type identity, and the facts used
  to prove them.
- **Unknown:** a bounded reason such as unresolved name, ambiguous provider,
  uncertain declaration activity, unsupported layout, or invalid operand.
- **Incomplete:** cancellation, exhausted work/storage bounds, failed required
  source access, or malformed processing state. Existing consumers retain their
  stricter incomplete-result policies.

For conditional evaluation, only a known size becomes an integer expression
value. Unknown layout produces unknown activity, never an assumed zero or a
guessed true/false condition. A syntactically valid but unresolved `SizeOf`
operand is distinguished from malformed expression syntax.

Retain both nominal declaration identity and structural layout information.
Declarations are keyed by source/unit identity, lexical scope, and occurrence;
display spelling alone is not an identity. Provenance includes source revision,
definition span, target facts, and applicable layout directives.

Represent named/strong aliases, scalar types, pointer/reference types, record
fields and variant alternatives, array kind/bounds/element type, and variable
declarations without erasing information needed for layout. By-value recursive
types are rejected; pointer recursion can have a known pointer size without
requiring the pointee's complete layout.

## 5. Compiler and target layout

Never use Rust `size_of`, the Linux host ABI, or mutable Pascal defines as the
source of Delphi storage widths. Target selection comes from the existing
project/build selection, not from `{$DEFINE WIN64}` encountered in a source.
Conflicting, invalid, or unavailable compiler/target facts remain unproven.

The initial platform profiles to implement and verify are **Delphi Win32 and
Win64**, matching the project's currently modeled target platforms. Other
platforms retain an explicit unsupported-target result until their ABI rules are
verified; they do not inherit a Windows or host profile. Version-sensitive facts such as `Char`
and default `String` require a known applicable compiler version and switches.
Target-independent widths can be admitted when the applicable dialect/version
facts prove them; target-dependent widths still require a verified target.

Required examples for verified Windows profiles:

| Type/storage | Win32 | Win64 |
| --- | ---: | ---: |
| `Byte`, `ShortInt`, `AnsiChar`, `Boolean` | 1 | 1 |
| `Word`, `SmallInt`, `WideChar` | 2 | 2 |
| `Integer`, `LongInt`, `Cardinal`, `LongWord`, `Single` | 4 | 4 |
| `Int64`, `UInt64`, `Double`, `Currency` | 8 | 8 |
| `Pointer`, `NativeInt`, `NativeUInt`, dynamic-array reference | 4 | 8 |
| `Extended` | 10 | 8 |

Size and alignment are separate facts: a 10-byte type is not automatically
10-byte aligned. Built-in spellings, qualified `System` types, managed reference
storage, fixed short strings, and version/switch-dependent string forms must be
resolved according to the selected compiler, not by name heuristics.

Capture declaration-site layout settings, including record alignment/packing,
enum storage, string representation, and relevant legacy layout switches. Later
switches must not retroactively alter a type already declared. Unknown or
unsupported layout-affecting switches invalidate affected layout facts rather
than being treated as harmless. Project-level settings and supported source
directives participate in the same fact model.

The directive families that must be accounted for are `A`/`ALIGN`,
`Z`/`MINENUMSIZE`, `H`/`LONGSTRINGS`, `OLDTYPELAYOUT`, and `REALCOMPATIBILITY`.
Model their verified settings for each supported compiler profile; unverified
settings explicitly block the affected fact. An uncertain string switch need
not invalidate an unrelated proven integer width. Preserve directive scope and
any supported save/restore behavior rather than applying one global setting to
every declaration.

Primary reference for the `Extended` distinction:
[Embarcadero System.Extended documentation](https://docwiki.embarcadero.com/Libraries/Florence/en/System.Extended).
All implemented ABI rules require a traceable reference or independently
recorded Delphi compiler result; the table is not a substitute for alignment,
version, and switch verification.

## 6. Binding and supported operands

### Aliases and declarations

Resolve aliases transitively, including strong aliases and qualified imported
types, while retaining declaration identity. Respect lexical shadowing,
declaration order, unit visibility, and the existing resolver's import
precedence. A same-named local variable or type cannot accidentally reuse an
outer or built-in fact.

Imported facts are obtained only from precisely resolved, authorized providers
under the appropriate source-specific conditional context. Preserve the current
distinction between project-compiled and external-library define closure. Do not
leak importer-local defines or variable declarations into a separately compiled
unit. Unit aliases and namespace qualification continue through existing lookup
policy. An ambiguous provider remains ambiguous even if candidate files happen
to suggest equal sizes.

Provider requests are bounded and request-scoped. Export interface-visible facts,
not implementation-private declarations. Avoid recursive preprocessing loops by
tracking active provider queries and distinguishing legal pointer forward
references from unresolved/by-value cycles. An incomplete provider may export an
independently proven fact only when its visibility and binding cannot be changed
by unresolved source regions; otherwise return unknown.

### Records

Support ordinary, packed, nested, and variant records. Count instance data
fields, including each name in grouped field declarations; exclude methods,
properties, and class/static storage from instance layout. Resolve field types
in their declaration scope, then apply the selected compiler's field alignment,
padding, and final record-size rules.

Variant alternatives share storage according to the target compiler's rules.
A named discriminator contributes storage; an unnamed discriminator does not.
Nested alternatives and their alignment requirements are included. A condition
affecting fields must be resolved or safely merged before publishing a layout.
Do not discard uncertain fields and calculate a falsely precise smaller record.

### Arrays

Static arrays retain every dimension's ordinal bounds and use checked element
counts and strides. Bounds may depend on earlier proven constants, including
include-derived constants and resolvable ordinal types. Do not assume zero-based
bounds or discard negative lower bounds. The element stride includes that
element type's required storage/padding.

Dynamic arrays have reference storage size, not the size of their current
elements. Open-array parameters must not be mistaken for either static arrays
or ordinary dynamic-array variables. If the compiler does not admit an operand
or its storage cannot be proven, the result remains unknown.

### Variables and access operands

Support variable identifiers, unit-qualified variables, record-field access,
array indexing, and typed-pointer dereference, in global and local scopes and
for parameters whose declared type is proven. `SizeOf` uses the operand's static
type; it does not read variable values, execute indexing expressions, dereference
memory, or confuse `var` parameter passing with the storage size of its type.

For indexed operands, validate the array kind and index-expression binding/type
without evaluating runtime values. For pointer dereference, the pointer's width
may be known while the dereferenced type remains unresolved.

Parse the `SizeOf` argument as a type/access operand rather than first reducing
an identifier to a constant value. Validate arity and syntax. A general
expression interpreter, overload-selected function results, property getters,
and execution of Delphi code are not introduced by this feature. Unproven generic
instantiations, external/DCU-only layouts, and other unmodeled constructs stay
unknown; they are not aliases for a guessed built-in type.

## 7. Source-order declarations and include continuation

Use one occurrence-aware evaluation session for a root and its textual includes.
Declaration ingestion advances through active source in order, with a bounded
pending declaration/token buffer and an explicit lexical scope stack. Reuse the
Pascal parser for complete declarative forms, but do not admit facts from missing
or recovered syntax. A damaged unrelated routine body does not justify inventing
a declaration or scope boundary.

When an include occurs in a declaration or initializer, suspend the declaration,
feed the selected child occurrence into that same logical declaration/scope,
then resume the parent's suffix. An include boundary is neither end-of-file for
the declaration nor a new declaration scope. Resolve known-active includes only
through existing authorized include lookup and occurrence-specific selections.

For example:

```pascal
const
  Revision = {$I revision.inc};
{$IF Revision = 7}
  {$I selected.inc}
{$ENDIF}
```

If `revision.inc` contains `7`, finish and admit `Revision` before evaluating the
following `IF` or deciding whether to read `selected.inc`. The same requirement
applies to nested includes, expressions split across several includes, type
declarations split by directives, and layout facts used by later `SizeOf` calls.
Do not rely on a final reparse to repair a branch/include selection already made
incorrectly by the first pass.

Keep parent and child source ranges and occurrence IDs. Repeated inclusion of
one file can occur with different scopes or environments; a URI-only cache is
not sufficient. Any necessary include-transition API extension must carry
continuation/occurrence information through every shared consumer, rather than
fixing only the LSP expander. Existing entry points retain compatible behavior
when no source/provider context is available, with unavailable facts unknown.

Unknown branches fork bounded state. Merge only facts proven compatible on every
reachable path; do not combine tokens from different alternative declarations
into a fictitious initializer or record. A later condition may use an equal,
independently proven size across alternatives, but it cannot invent a precise
binding or source location where identity is uncertain. Unknown includes remain
a barrier when they could change a declaration, scope, or required fact.

## 8. Integration, header navigation, and source maps

Integrate shared semantics into:

- `pascal-core` conditional analysis and resolver include walks;
- LSP include expansion, indexing, and include-audit/rename safety paths; and
- the existing lint4d CFG projection adapter, which currently calls the shared
  evaluator before cfg-pascal's strict preparation stage.

`cfg-pascal` does not currently depend on `pascal-core`. Do not add a dependency
cycle or reimplement `SizeOf` inside its narrower preparer. The existing adapter
supplies already-decided projection/include selections. Broader evaluation
deduplication belongs to TASK-47 and is not absorbed into TASK-140.

Fix FireDAC candidate recognition with shared source-backed header validation.
Handle a compiler-directive preamble without trusting the filename, taking a
`uses` entry as the unit declaration, or accepting a header hidden in a string,
comment, inactive branch, or unresolved branch. Pre-header includes must be
resolved/validated when their contents could affect header identity or activity.

For available precisely selected units, retain the actual header span separately
from completeness of later body analysis. Definition navigation may use that
independently proven span when an unrelated later construct remains unsupported.
This exception applies to the header only: uncertain members, rename completeness,
and other body-semantic operations retain their existing safety gates.

Preserve decoded UTF-8 coordinates, original-source identities, and the current
source-map rules. An operand/declaration can span include occurrences without
having one editable physical range. Do not guess edits or diagnostic ranges
across synthetic or multi-source segments. The unit-header jump must map to the
actual physical header, not to a synthetic declaration used for parsing.

## 9. Bounds, errors, and freshness

Reuse the existing cancellation and recovery-budget machinery. Bound declaration
entries, pending fragments, scope/type depth, record fields/variant alternatives,
array dimensions, provider queries, and retained facts as well as include bytes
and directives. Charge work and retained bytes before scanning, allocation,
cloning, or provider reads; do not repeatedly parse an ever-growing prefix.

All size multiplication, field-offset addition, alignment rounding, and conversion
to expression integers use checked arithmetic. Overflow, unsupported ordinal
bounds, and exhausted limits never produce a wrapped known size. Cancellation
publishes no newly precise cache result.

Failed, ambiguous, unauthorized, cyclic, or over-budget includes are not empty
text. Separate provisional declaration continuation from genuine incomplete
analysis. Reconciliation can clear a provisional reason only after proving that
exact declaration/occurrence was completed; it cannot clear errors by broadly
matching warning strings or assuming a successful final parse proves every read.

Cache keys include compiler/target facts, applicable switches, lexical scope and
occurrence, source revisions, and provider dependencies. Changes to a type,
constant, include, selected target, or provider invalidate derived sizes and
conditional decisions. Replay read/freshness observations on cache hits and
retain the LSP's stale-result rejection policy. Snapshot/merge/restore operations
must include layout and continuation state, not only defines/constants.

## 10. Verification contract

Regression tests must establish behavior, not merely that navigation returns a
nonempty result. Require:

1. The FireDAC-style preamble parses and selects the actual matching unit header;
   mismatched/ambiguous units and false header text remain rejected.
2. `SizeOf(Extended)` chooses opposite relevant branches on Win32 and Win64;
   `SizeOf(LongWord)` is 4 on both. Missing/conflicting target facts remain safe.
3. Alias chains, qualified imported aliases, strong aliases, local shadowing,
   forward references, and by-value versus pointer recursion preserve binding.
4. Explicitly aligned and packed records, grouped/nested fields, padding,
   named/unnamed variant tags, and switch changes have verified expected sizes.
5. Static multidimensional arrays with nonzero/negative bounds, include-derived
   bounds, nested elements, and dynamic-array references have correct storage
   sizes. Overflow and unresolved/open-array cases remain unknown.
6. Variable, field, index, dereference, and parameter operands use static types
   and lexical scope; wrong arity, unresolved access, and runtime values do not
   manufacture facts.
7. Included initializer values drive subsequent conditions and active-include
   reads. Nested/repeated occurrences and branch-dependent initializer fragments
   are covered, as are missing, ambiguous, cyclic, and unauthorized includes.
8. The shared evaluator, resolver, LSP expansion/audits, and lint projection
   adapter agree on the same fixtures without weakening rename safety.
9. Overlay/provider edits, packing changes, and target/configuration changes
   invalidate cached values and decisions. Budget and cancellation tests retain
   existing bounded-work guarantees.
10. A proven header remains navigable despite an unsupported later body, while
    uncertain body symbols remain suppressed.

Numeric ABI fixtures state compiler version, target, and layout directives, with
traceable expected values. Delphi-compiler-produced fixtures may supplement the
suite, but ordinary tests must run without a proprietary compiler or the user's
installed source tree. Do not claim compiler parity from Rust-host calculations
or unexecuted Delphi probes.

Final integration requires workspace tests, formatting, Clippy, and correctness
review; then rebuild the configured `target/release/pascal-lsp` and use the real
unchanged configuration to verify all 13 imports from the actual ChainDriveAPI
source. Document remaining unsupported body constructs even when all header
jumps succeed. TASK-140 is not complete if those jumps work solely through the
fallback while its required `SizeOf` or initializer-include evaluation still
fails.

## 11. Delivery boundary

This is one cohesive TASK-140 design with separate internal responsibilities,
not a request for a complete Delphi compiler, a replacement LSP type checker, or
general MSBuild/configuration cleanup. The full agreed operand categories remain
in scope; an unsupported instance is an explicit unknown result, not a reason to
silently reduce delivery to the built-in-only proposal.

Next gate: the user reviews this written spec. After approval, create the
implementation plan and obtain its review and execution-method selection. No
product-code implementation or delegation is authorized by this document alone.
