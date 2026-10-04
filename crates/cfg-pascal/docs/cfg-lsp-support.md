# CFG and LSP alignment boundary

`cfg-pascal` builds a control-flow graph from a parsed Pascal syntax tree. The
legacy `build_file_cfgs` API is file-local; the additive
`build_file_cfgs_in_project` API consumes an immutable caller-built project
snapshot. This crate still does not provide project indexing or discovery,
filesystem include lookup, full compiler-equivalent conditional evaluation,
receiver resolution, or an LSP server. It does provide bounded, caller-supplied
conditional/include preparation; that contract is documented below. Syntax
recognition and CFG semantic precision are therefore documented separately.

## Current syntax and CFG semantics

| Syntax | Parser support | CFG support and limits |
| --- | --- | --- |
| Modern `ppBlock` statement conditionals | Yes | Branches are mutually exclusive alternatives. Every branch is possible because this crate does not evaluate project defines. A block without `else` retains a skip path. Nested blocks, loop transfers, labels, and `finally` cleanup are preserved. Directives are not executable statement references. |
| Preprocessor-wrapped declarations | Yes | Exception type inference treats modern `pp*` nodes as a file-wide uncertainty barrier. A conditional type or alias never removes a conservative exception edge. Ordinary comments are not barriers. |
| `case ... otherwise` / `case ... else` | Yes | The default arm is a `CaseArm` alternative. An additional no-match path is emitted only when no default arm exists. |
| Inline variables | Yes | Inline value bindings shadow types from their declaration onward. `for var` and `foreach var` bindings are scoped to the loop syntax and do not leak into later code. |
| Anonymous functions/procedures | Yes | Lambda arguments and local declarations have a separate lexical scope for semantic lookup. Anonymous bodies are not walked as statements in the enclosing routine and do not produce a separate public CFG name. |
| Delphi conditional expressions (`if ... then ... else ...`) | Yes | The enclosing expression-bearing statement is currently atomic in the CFG. The graph does not create branch blocks or claim that both expression arms execute; exception behavior is conservatively attributed to the containing statement. Expression-level side effects and branch-specific source references are not modeled yet. |
| Malformed or incremental trees | Tree-sitter can represent them | Callers must pass a syntax tree and source bytes from the same snapshot. The builder uses conservative fallbacks for unsupported or incomplete nodes; it does not fabricate compiler semantics. |

## Project snapshot API

Project-aware builds never discover files or infer imports from spelling. The
caller creates a `ProjectSnapshot` from `ProjectUnitInput` values (each pairing
a parsed `tree_sitter::Tree` with its source bytes) and supplies one
`ImportBinding` for each `uses`-clause `moduleName` occurrence. A binding can
select a loaded `ProjectUnitId`, or explicitly record an unavailable or
ambiguous target. Authorized qualifiers are occurrence-specific, so aliases
must be supplied by the project model rather than guessed by this crate.

The snapshot validates duplicate unit/source IDs, exact `uses`-site spans,
duplicate bindings, dangling loaded targets, qualifier syntax, and root/source
byte bounds. The tree/source pairing remains a caller contract because
tree-sitter does not expose the bytes originally used to create a `Tree`.
Snapshot fields are private and the builder only borrows them, so a completed
snapshot cannot be mutated through the resolver.

```rust
use cfg_pascal::{
    build_file_cfgs_in_project, ProjectSnapshot, ProjectSourceId, ProjectUnitId,
    ProjectUnitInput,
};
use tree_sitter::Parser;

let source = b"unit Demo; interface implementation end.";
let mut parser = Parser::new();
parser.set_language(&cfg_pascal::LANGUAGE.into()).unwrap();
let tree = parser.parse(source, None).unwrap();
let unit = ProjectUnitInput::new(
    ProjectUnitId::from("demo"),
    ProjectSourceId::from("demo.pas"),
    tree,
    source,
);
let snapshot = ProjectSnapshot::new(vec![unit], Vec::new()).unwrap();
let cfgs = build_file_cfgs_in_project(&snapshot, &ProjectUnitId::from("demo")).unwrap();
assert!(cfgs.is_empty());
```

## Configured conditional and include preparation

`prepare_source` evaluates a bounded conditional subset over caller-supplied
`SourceSnapshot` values and expands explicitly selected `IncludeBinding`
occurrences. It does not read files, discover search paths, evaluate MSBuild,
or infer a compiler environment. Its executable rustdoc example demonstrates
the complete preparation call.

- `PrepareSourceOptions` supplies the configuration ID and initially defined
  and undefined symbols. Omitted symbols are unknown in a `Partial`
  environment and false only in an explicitly `Complete` environment.
- Supported directives include `IFDEF`, `IFNDEF`, boolean `IF`/`ELSEIF`,
  `ELSE`, `ENDIF`, `DEFINE`, `UNDEF`, and `I`/`INCLUDE`, in both brace and
  parenthesized-comment spellings. Boolean conditions support `Defined`,
  `NOT`, `AND`, `OR`, parentheses, `TRUE`, and `FALSE`.
- Define state flows into and out of nested includes in source order.
  Include bindings identify original directive spans; they do not authorize
  filesystem lookup or certify malformed directive syntax.
- Unknown active conditions, unsupported active directives, missing/cyclic
  includes, malformed input, and exceeded resource budgets return errors.
  They never become an apparently empty complete projection.
- Include return boundaries receive a mapped synthetic line separator when
  needed, so an include's final token or line comment cannot consume parent
  code.

Convert the result with `ProjectUnitInput::from_prepared`. Construct project
`UsesSite` ranges from that prepared tree, not the original source. CFG and
statement ranges also use prepared coordinates; translate them with the
retained source map when reporting diagnostics or navigating original files.

## Prepared source and mapping contract

Project configuration discovery and include selection remain caller-owned.
The in-memory helper above constructs the mapping automatically; external
preparers can instead provide immutable
[`SourceSnapshot`](../src/source_map.rs) values and an ordered
[`SourceMap`](../src/source_map.rs) whose segments cover the complete
prepared buffer.  `Copied` segments must match their original bytes;
`Masked` segments may contain only whitespace and must preserve line breaks;
`Synthetic` segments explicitly have no origin.  The `ExpansionId` on each
segment distinguishes repeated and nested include occurrences, even when they
reuse the same original range.  Each origin-bearing expansion is constrained to
one original source with non-overlapping original ranges, while discontiguous
root ranges around nested includes remain valid.  `map_range` returns every
clipped mapped span, so a statement crossing an include boundary is not forced
into one file.

`PreparedSource::new` is intentionally strict: the caller must mark the
projection as `PreparationFidelity::Complete`, provide a configuration ID and
provenance, and the prepared bytes must parse cleanly with this crate's
`LANGUAGE` without any preprocessor nodes.  `Unresolved`, `Lossy`, and
`Incomplete` preparation is rejected; `Complete` is an explicit caller
assertion, not permission to leave an unresolved include or other directive in
the prepared bytes.  `PreparedSource::from_segments` and the
`PreparedSource::identity` convenience path enforce the same check.  The raw
`build_file_cfgs`/`ProjectUnitInput::new` APIs remain available and
conservative.  Callers with configured source must resolve or mask every
directive first, typically through `prepare_source`; raw source that still
contains directives must use the raw APIs rather than claim a precise
prepared CFG.

The executable rustdoc example on `PreparedSource::new` shows the full
construction path:

```rust
use cfg_pascal::{
    PreparationFidelity, PreparationProvenance, PreparedSource, ProjectSnapshot,
    ProjectSourceId, ProjectUnitId, ProjectUnitInput, SourceMap, SourceSnapshot,
};

let bytes = b"unit Demo; interface implementation end.";
let map = SourceMap::identity(SourceSnapshot::new(
    ProjectSourceId::from("demo.pas"),
    bytes,
))
.unwrap();
let prepared = PreparedSource::new(
    ProjectSourceId::from("demo.prepared"),
    bytes,
    map,
    "debug",
    PreparationFidelity::Complete,
    PreparationProvenance::Configured,
)
.unwrap();
let unit = ProjectUnitInput::from_prepared(ProjectUnitId::from("demo"), prepared);
let snapshot = ProjectSnapshot::new(vec![unit], Vec::new()).unwrap();
assert_eq!(snapshot.configuration_id(), Some("debug"));
```

Prepared units retain their map and original snapshots through
`ProjectUnitInput::source_map()` and `original_sources()`.  All prepared units
in a project snapshot must use the same configuration ID.  If several prepared
units retain an original source with the same source ID, their bytes must be
identical; otherwise snapshot construction fails rather than mixing revisions.

## Exception precision boundary

Typed constructor dispatch is precise only for proven non-generic classes and
transparent aliases. The legacy API proves same-file declarations; the project
API can additionally prove declarations in explicitly loaded units through
explicit uses bindings. Missing, unavailable, ambiguous, or unimported types,
conditional or malformed declarations, unresolved method owners, generic
forms, inaccessible constructor members, implicit `with` members, and
unsupported expression/type shapes retain conservative alternatives.

Project import lookup uses interface declarations as the exported namespace,
honors interface versus implementation uses sections and later-entry
precedence, and treats an unresolved higher-priority import as a blocker. Unit
identities are caller-supplied; type identity is qualified by the stable unit
identity and does not depend on input order. Cyclic ancestry and alias graphs
remain unknown.

## Downstream integration boundary

`cfg-pascal` remains caller-driven. It validates the immutable
`ProjectSnapshot` and prepared-source contracts, but it does not own project
selection, filesystem reads, include lookup, workspace indexing, LSP state, or
MSBuild evaluation. Callers must provide parsed trees and bytes, explicit import
bindings, and (for configured preparation) explicit include bindings and a
validated source map.

### Conditional/include propagation boundary

The lower-level `prepare_source` API propagates `DEFINE`/`UNDEF` state through
include expansion when the caller supplies a complete, occurrence-aware
snapshot. The shared resolver walk used by the current CLI/LSP integration is
deliberately more conservative: it resolves imports before walking includes,
and nested include walks do not feed newly discovered definitions back into the
parent's initial conditional environment. Consequently, a symbol defined by an
include and consumed by a later `IFDEF` may leave the resolver project
incomplete. The adapter then retains the raw snapshot and uses the file-local
CFG fallback rather than claiming configured precision. This is an intentional
fail-closed integration limitation, not a promise that every preparation-time
propagation case is reachable through CLI/LSP project analysis.

The shared integration now lives in the sibling `lint4d` worktree rather than
in this library. Its `pascal-core::resolver::UnitResolver<S>` consumes the
bounded `pascal-project::ProjectContext` and a caller-supplied source store; the
lint4d CFG adapter at
`crates/lint4d/src/cfg/project_snapshot.rs` converts its
`ResolvedProject` into `CfgProjectSnapshot` values for the existing
`build_file_cfgs_in_project` API. Incomplete resolution is retained as an
incomplete/raw snapshot and the lint runner falls back to file-local CFG rather
than claiming precise cross-unit facts. `pascal-lsp` supplies its own overlay
source store and retains workspace snapshots, generations, invalidation, and
diagnostic policy.

The current lint4d dependency is pinned to the upstream cfg-pascal Git revision
`208d6743c61e0b391195958270a5e04e3a4328d4`; this documentation is not a claim
that the downstream adapter is part of a released cfg-pascal package. Receiver
resolution, semantic-token integration, and compiler/MSBuild fidelity remain
owned by downstream callers.
