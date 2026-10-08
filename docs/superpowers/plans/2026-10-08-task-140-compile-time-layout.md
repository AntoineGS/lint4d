# TASK-140 Compile-Time Type Layout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task, according to the user's execution choice. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Support the full agreed `SizeOf` operand list and includes inside constant initializers, fix FireDAC unit-header recognition, and restore all 13 real ChainDriveAPI import jumps.

**Architecture:** `pascal-project` provides immutable selected compiler/target facts and source-specific layout defaults. A focused `pascal-core::compile_time` module owns declaration continuation, scoped binding, and checked type layout; the shared conditional/include session consumes those facts in source order. Existing resolver, LSP, and lint adapters retain source access, observations, source maps, and delivery policy.

**Tech Stack:** Rust 2024, workspace Rust floor 1.99, tree-sitter-pascal, existing `pascal-project` and `pascal-core` APIs, Cargo tests, TOML ABI fixtures, and a stdio LSP probe.

**Spec:** `docs/superpowers/specs/2026-10-08-task-140-compile-time-layout-design.md` (user-approved).

## Global Constraints

- "Work stays in the current checkout." Do not create a worktree unless the user changes this preference.
- "Preserve TASK-139's project-name fix and tests." They are in existing commit `8e47b4f`; do not modify them merely to make this task pass.
- "Do not change the real ChainDriveAPI sources, Delphi installation sources, editor configuration, or installation mappings."
- "Header-only navigation is a safety-preserving fallback, not a replacement for working conditional evaluation."
- "The user explicitly selected the full `SizeOf` operand list: built-in types, user-defined aliases, records, arrays, and variables."
- "The initial platform profiles to implement and verify are **Delphi Win32 and Win64**." Unverified targets/settings remain unknown; do not substitute Linux layouts.
- "An ambiguous provider remains ambiguous even if candidate files happen to suggest equal sizes."
- "An include boundary is neither end-of-file for the declaration nor a new declaration scope."
- "All size multiplication, field-offset addition, alignment rounding, and conversion to expression integers use checked arithmetic."
- "Do not rely on a final reparse to repair a branch/include selection already made incorrectly by the first pass."
- "Broader evaluation deduplication belongs to TASK-47 and is not absorbed into TASK-140."
- "Ordinary tests must run without a proprietary compiler or the user's installed source tree." Record references/compiler results for numeric ABI expectations.
- No new runtime dependencies, full navigation-index extraction, arbitrary Delphi execution, DCU layout inference, or unbounded provider scans.
- The additional limits introduced below supplement, rather than bypass, existing source/include/recovery budgets.
- Product implementation begins only after this plan is reviewed and the user chooses an execution method. No delegation is authorized by writing this plan.

## Review Focus

1. **Include failure after a valid prefix:** a missing/cyclic child must not turn a partially assembled initializer into a known constant (Task 8, `rf_partial_initializer_failure`).
2. **Shadowing of familiar names:** a local variable/type named `Extended`, or a qualified provider name, must not accidentally use the intrinsic type's size (Tasks 3 and 6, `rf_shadowed_intrinsic`).
3. **Signed and ordinal array bounds:** negative lower bounds, enum indices, and overflow must preserve element counts or remain unknown, never truncate/wrap (Task 4, `rf_ordinal_bounds`).
4. **Selection/default provenance:** an explicit project alignment switch must not change an already-declared type or leak into a separately compiled library's defaults (Tasks 1 and 5, `rf_library_defaults` and `rf_declaration_settings`).
5. **Decoded/source-map coordinates:** UTF-16 disk input and decoded overlays with multibyte comments before a header must return the physical identifier location, never a synthetic parse-wrapper range (Task 12, `rf_decoded_header_map`).

---

## Repository landmarks and file ownership

Line numbers are landmarks in the approved-spec checkout; locate functions by name after earlier tasks change them.

| Path | Responsibility / owner task |
| --- | --- |
| `crates/pascal-project/src/layout.rs` (new) | Data-only `LayoutContext`, verified defaults and target selection; Task 1. |
| `crates/pascal-project/src/conditional.rs` | Add layout context to construction/hash/recovery accounting; Task 1. |
| `crates/pascal-project/src/lib.rs` | Populate project/library facts near `merge_project_conditional_context` / lines 5213–5267; Task 1. |
| `crates/pascal-core/src/compile_time/mod.rs` (new) | Narrow public re-exports; Tasks 2–9. |
| `crates/pascal-core/src/compile_time/model.rs` (new) | Fact/result, source anchor, binding/type representation; Tasks 2–3. |
| `crates/pascal-core/src/compile_time/budget.rs` (new) | Accounted request-wide limits/cancellation; Task 2. |
| `crates/pascal-core/src/compile_time/scalars.rs` (new) | Intrinsic/reference/string widths; Task 2. |
| `crates/pascal-core/src/compile_time/declarations.rs` (new) | Resumable token/declaration stream and physical spans; Task 3. |
| `crates/pascal-core/src/compile_time/bindings.rs` (new) | Scoped names, aliases, constants, parameters, visibility; Tasks 3, 9. |
| `crates/pascal-core/src/compile_time/arrays.rs` (new) | Ordinal bounds and static/dynamic array storage; Task 4. |
| `crates/pascal-core/src/compile_time/records.rs` (new) | Field/variant layout and captured switches; Task 5. |
| `crates/pascal-core/src/compile_time/operands.rs` (new) | Non-executing type/access-operand parser; Task 6. |
| `crates/pascal-core/src/compile_time/session.rs` (new) | Occurrence-aware source-order evaluation and shared resources; Tasks 7–9. |
| `crates/pascal-core/src/compile_time/header.rs` (new) | Independently proven physical unit header; Task 10. |
| `crates/pascal-core/src/conditional.rs` | Expression dispatch and complete snapshot/merge semantics; Tasks 7–8. |
| `crates/pascal-core/src/resolver.rs` | Source/provider adapter, header validation and include walks; Tasks 8–10. |
| `crates/lint4d/src/cfg/project_snapshot.rs` | Shared evaluation before the strict CFG splicer; Task 11. |
| `crates/pascal-lsp/src/include_expansion.rs` | Shared session to existing expansion/source maps; Task 12. |
| `crates/pascal-lsp/src/navigation.rs` | Store/use verified header only; no general type-checker rewrite; Task 12. |
| `crates/pascal-lsp/src/workspace.rs` and `src/workspace/rename.rs` | Shared session use, resource adapters, audits; Task 12. |
| `crates/pascal-lsp/src/project_cache.rs` and `src/navigation/compiled_dcu.rs` | Fact costs, context hashes, observation/probe revalidation; Task 13. |
| `crates/pascal-core/tests/support/compile_time.rs` (new) | Fully specified in-memory test harness; Tasks 3, 7–9. |
| `crates/pascal-core/tests/compile_time_abi.rs`, `compile_time_bindings.rs`, `compile_time_arrays.rs`, `compile_time_records.rs`, `compile_time_operands.rs`, `compile_time_conditionals.rs`, `compile_time_includes.rs`, `compile_time_providers.rs`, `compile_time_headers.rs` (new) | One focused regression target per deliverable. |
| `crates/pascal-core/tests/fixtures/compile_time/abi.toml` (new) | Referenced compiler/target/switch expectations; Tasks 2, 4–5. |
| `crates/pascal-lsp/tests/compile_time_layout.rs` (new) | Workspace navigation/overlay/target regression suite; Tasks 12–13. |
| `crates/pascal-project/tests/layout_context.rs` (new) | Build selection and default provenance; Task 1. |
| `crates/lint4d/tests/cfg_compile_time_layout.rs` (new) | CLI adapter consistency; Task 11. |
| `crates/pascal-lsp/README.md`, `crates/pascal-project/README.md`, `docs/shared-resolver-architecture.md` | Supported forms, explicit limits, source-map and consumer boundaries; Task 14. |

No new module should accumulate parsing, binding, and ABI arithmetic together.
Keep internal helper types private unless the interfaces below require them.

## Shared interface contracts

These are the names/signatures neighboring tasks must agree on. Their bodies are
implemented by the owning tasks, not by scaffolding ahead of a failing test.

### Project facts — Task 1

```rust
// pascal_project::layout; all data types derive Clone, Debug, PartialEq, Eq, Hash.
pub enum LayoutPlatform { Win32, Win64, Other(String) }
pub struct LayoutSettings {
    pub record_alignment: Option<u8>,
    pub minimum_enum_size: Option<u8>,
    pub long_strings: ConditionalFact,
    pub old_type_layout: ConditionalFact,
    pub real_compatibility: ConditionalFact,
}
pub struct LayoutContext {
    pub platform: Option<LayoutPlatform>,
    pub defaults: LayoutSettings,
}
// Default is unproven platform/settings, not Win32 defaults.
// LayoutContext::for_target(version: Option<CompilerVersion>,
//                           platform: Option<&TargetPlatform>) -> Self
// ConditionalContext::with_layout(self, layout: LayoutContext) -> Self
// ConditionalContext gains: pub layout: LayoutContext
```

`compiler_version` remains the existing `ConditionalContext` field; do not create
a second independently mutable compiler-version value. Source directives can
change settings, not the selected platform. Existing bare struct initializers
must be updated without accidentally closing their absent-define sets.

### Core facts and budgets — Tasks 2–3

```rust
pub enum Fact<T> {
    Known(T),
    Unknown(UnknownReason),
    Incomplete(IncompleteReason),
}
pub enum UnknownReason {
    MissingTarget, UnsupportedTarget, UnsupportedVersion, UnresolvedName,
    AmbiguousBinding, UnknownActivity, UnsupportedLayout, InvalidOperand,
    CyclicType, CyclicProvider, UnresolvedInclude, Overflow, HeaderNotAnalyzed,
}
pub enum IncompleteReason {
    Cancelled,
    Limit { name: &'static str, maximum: usize },
    RequiredSource { source: SourceId, reason: String },
    MalformedSource,
}
pub struct StorageLayout { pub size: u64, pub alignment: u32 }
pub struct OccurrenceId(pub u64);
pub struct ScopeId(pub u64);
pub struct Sequence(pub u64);
pub struct BindingSite { pub scope: ScopeId, pub sequence: Sequence }
pub struct SourceAnchor {
    pub source: SourceId,
    pub occurrence: OccurrenceId,
    pub range: std::ops::Range<usize>,
}
pub struct DefinitionKey { pub anchor: SourceAnchor, pub scope: ScopeId }
pub enum TypeIdentity { Intrinsic(String), Declared(DefinitionKey) }
pub struct KnownLayout {
    pub storage: StorageLayout,
    pub identity: TypeIdentity,
    pub dependencies: Vec<ResolutionObservation>,
}
pub struct SourceFragment<'a> {
    pub source: &'a SourceId,
    pub occurrence: OccurrenceId,
    pub physical_start: usize,
    pub text: &'a str,
}
```

`Fact::known(self) -> Option<T>` extracts only `Known`. Operational errors map to
existing `ResolverError` at adapter boundaries. Do not add a `Hash` derive to
`SourceRevision` merely for convenience; fingerprint its validated version/hash
and replay its full existing observations instead.

`CompileTimeLimits` defaults: definitions/fields/variant alternatives 32,768 each;
pending declaration bytes 1 MiB; scope/type depth 256 each; array dimensions 256; live branch states
64; provider queries 256. Reuse existing 1,000,000 environment operation and
16 MiB byte-work bounds, with 1 MiB retained-fact admission accounting. Source
bytes use byte-work, not one structural operation per byte. Existing outer
source/include budgets remain effective and may be tighter.

The corresponding public fields are `max_definitions`, `max_fields`,
`max_variant_alternatives`, `max_pending_bytes`, `max_scope_depth`,
`max_type_depth`, `max_array_dimensions`, `max_branch_states`,
`max_provider_queries`, `max_work`, `max_byte_work`, and `max_retained_bytes`.

`CompileTimeBudget::new(limits: CompileTimeLimits, cancel: &dyn CancellationToken)`
creates one shared request budget. `charge(work: usize, byte_work: usize,
retained: usize) -> Result<(), IncompleteReason>` admits before inspecting/copying
payloads. `snapshot()` exposes counters for tests; `release(retained: usize)`
releases retained capacity only, never refunds work. Snapshot/fork operations are
charged against the same budget; they do not reset it per include/provider.
`snapshot() -> BudgetSnapshot` returns `work`, `byte_work`, and `retained_bytes`
as public `usize` counters. Pending-fragment admission separately checks
`max_pending_bytes`; generic `charge` checks the total retained-byte limit.

`BuiltinType` is a closed enum covering the table in spec §5 and `Char`, `String`,
`ShortString`, `ByteBool`, `WordBool`, `LongBool`, `Real`, `Real48`, `AnsiString`,
`UnicodeString`, `WideString`, and `NativeUInt` aliases. The registry must reject
names not admitted by the selected dialect/version.

`builtin_layout(ty: BuiltinType, context: &ConditionalContext,
settings: &LayoutSettings) -> Fact<StorageLayout>` is implemented in Task 2.

### Declaration and query layer — Tasks 3–6

`CompileTimeState` owns an accounted declaration stream, scope/sequence map,
binding graph, captured declaration settings, and source/provider provenance.
It is an opaque owned type; callers cannot mutate its internal maps.

- `CompileTimeState::new(context: &ConditionalContext, root: &LoadedSource, budget: &mut CompileTimeBudget<'_>) -> Result<Self, IncompleteReason>`
- `feed(&mut self, fragment: SourceFragment<'_>, activity: Truth, budget: &mut CompileTimeBudget<'_>) -> Result<(), IncompleteReason>`
- `finish_root(&mut self, budget: &mut CompileTimeBudget<'_>) -> Result<(), IncompleteReason>`
- `site_at(&self, source: &SourceId, occurrence: OccurrenceId, offset: usize) -> Option<BindingSite>`
- `layout_name(&self, site: BindingSite, name: &str, budget: &mut CompileTimeBudget<'_>) -> Fact<KnownLayout>`
- `apply_layout_directive(&mut self, body: &str, activity: Truth, budget: &mut CompileTimeBudget<'_>) -> Result<(), IncompleteReason>`
- `layout_operand(&self, site: BindingSite, operand: &SizeOfOperand, budget: &mut CompileTimeBudget<'_>) -> Fact<KnownLayout>`
- `fingerprint(&self) -> u64` and `visit_recovery_payload(&self, visit: &mut dyn FnMut(usize) -> Result<(), String>) -> Result<(), String>`

Use an internal recursive `TypeExpr` for named/strong aliases, pointers, records,
arrays, ordinal types, and string/reference forms. An array retains bound
expression text plus bound declaration sites; a record retains every field,
variant/tag structure, and declaration settings. Freeze names to definition
identities where the compiler binds them; do not resolve an old alias in a newer
shadowing scope. Ordinal constant expressions are evaluated from source-order
bindings, not from string-to-integer casts alone.

`parse_sizeof_operand(text: &str, budget: &mut CompileTimeBudget<'_>) -> Fact<SizeOfOperand>` in
Task 6 accepts a named operand followed by field/index/dereference selectors.
Its bounded token representation retains the original argument span. Functions,
property getters, and unknown generic instantiations do not become executable
expressions. Binding validates index-expression types without evaluating values.

### Shared session and resource callbacks — Tasks 7–9

```rust
pub struct ProviderSource {
    pub unit: ResolvedUnit,
    pub context: ConditionalContext,
    pub authorized_qualifiers: Vec<String>,
}
pub trait CompileTimeResolver {
    fn include(
        &mut self,
        request: IncludeResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<LoadedSource>, ResolverError>;
    fn unit(
        &mut self,
        request: UnitResolveRequest<'_>,
        budget: &mut CompileTimeBudget<'_>,
        cancel: &dyn CancellationToken,
    ) -> Result<ResolutionOutcome<ProviderSource>, ResolverError>;
}
pub struct OccurrenceAnalysis {
    pub id: OccurrenceId,
    pub source: LoadedSource,
    pub analysis: ConditionalAnalysis,
}
pub struct IncludeOccurrence {
    pub owner: OccurrenceId,
    pub directive: SourceAnchor,
    pub target: Resolution<LoadedSource>,
    pub child: Option<OccurrenceId>,
}
pub struct CompileTimeAnalysis {
    pub root: ConditionalAnalysis,
    pub occurrences: Vec<OccurrenceAnalysis>,
    pub includes: Vec<IncludeOccurrence>,
    pub facts: CompileTimeState,
    pub header: Fact<VerifiedUnitHeader>,
    pub reasons: Vec<IncompleteReason>,
    pub observations: Vec<ResolutionObservation>,
    pub warnings: Vec<String>,
}
pub fn analyze_source(
    root: &LoadedSource,
    context: &ConditionalContext,
    resolver: &mut dyn CompileTimeResolver,
    limits: CompileTimeLimits,
    cancel: &dyn CancellationToken,
) -> Result<CompileTimeAnalysis, ResolverError>;
```

`VerifiedUnitHeader` (Task 10) is defined when Task 7 introduces this carrier, so
intermediate commits compile: `pub struct VerifiedUnitHeader { pub name: String,
pub declaration: SourceAnchor, pub name_parts: Vec<SourceAnchor> }`. Until Task
10, its value is conservatively `Fact::Unknown(UnknownReason::HeaderNotAnalyzed)`;
that is an explicit unsupported stage, not a guessed header.

Include records preserve target ambiguity/incompleteness; they are not booleans.
Callbacks return existing `ResolutionOutcome` carriers, so successful and failed
lookups both retain observations and warnings. The session aggregates these in
its output; provider-derived facts retain the subset that proves their binding.
Failed required includes attach `IncompleteReason::RequiredSource`; ambiguous
bindings retain their unknown reason as well. Do not flatten these into empty text.
The session itself compiles provider interfaces from `ProviderSource`, with a
fresh source-specific environment but the shared request budget and provider
cycle guard. The callback resolves/loads sources, not types. Exported interface
facts are an opaque internal `InterfaceFacts` graph remapped by declaration
identity; never export implementation-private names or importer locals.

`ConditionalEnvironment` snapshots gain the complete compile-time state and
occurrence continuation. `to_context()` remains the data-only explicit-fact
projection; it cannot be used as a lossless typed-state snapshot. Existing public
conditional entry points remain available. Context-only calls use an anonymous
source identity and no provider capability; nested include callbacks propagate
the typed state but do not call `finish_root()` at child exit.

### Header API — Task 10

`verify_unit_header(root: &LoadedSource, context: &ConditionalContext,
resolver: &mut dyn CompileTimeResolver, limits: CompileTimeLimits,
cancel: &dyn CancellationToken) -> Result<Fact<VerifiedUnitHeader>, ResolverError>`
halts after proving the declaration/name semicolon. It reads necessary
pre-header includes but does not parse the entire implementation to validate a
candidate. Cache its result only for the exact source/context/proof dependencies.

The public header function creates a budget for standalone calls only. Its
private helper `verify_unit_header_in_session(root: &LoadedSource,
context: &ConditionalContext, resolver: &mut dyn CompileTimeResolver,
budget: &mut CompileTimeBudget<'_>, cancel: &dyn CancellationToken)
-> Result<Fact<VerifiedUnitHeader>, ResolverError>` consumes the caller's existing
budget for recursive candidate/provider validation. Thread that budget through
resolver candidate helpers; do not restart budgets for provider headers.

## Test harness contracts

Task 3 creates `tests/support/compile_time.rs` with `#![allow(dead_code)]` limited
to this shared test module. Export these helpers:

- `context(platform: LayoutPlatform) -> ConditionalContext`: compiler 37.0,
  explicit verified target/defaults, open absent defines.
- `loaded(path: &str, text: &str) -> LoadedSource`: absolute `/fixture/` path,
  deterministic `SourceId`, `Arc` UTF-8 bytes/decoded text, overlay revision 1 and
  `content_hash_bytes(text.as_bytes())`.
- `Harness::plain(text: &str, context: ConditionalContext) -> Self`: feed one
  directive-free source as occurrence 0 and finish its root.
- `Harness::stream(text: &str, context: ConditionalContext) -> Self`: feed an
  unfinished source without calling `finish_root`; used for subsequent fragments.
- `finish(&mut self)`: finalize the streamed root exactly once before final queries.
- `size(&self, name: &str) -> Option<u64>`: query at root end, return only proven
  layout size.
- `size_at(&self, marker: &str, name: &str) -> Option<u64>`: find the unique marker
  offset, use `site_at`, and return only the size visible there.
- `query(&self, marker: &str, operand: &str) -> Fact<KnownLayout>`: added in Task
  6 using the operand parser and the marker's binding site.
- `with_layout(&mut self, body: &str)` / `feed(&mut self, text: &str)`: update a
  proven directive or append a same-source fragment with its physical offset.

Task 7 extends it with `Fixture` and an in-memory `CompileTimeResolver`:

- `Fixture::new(text: &str, context: ConditionalContext) -> Self`;
- `include(&mut self, name: &str, text: &str)` and, in Task 9,
  `unit(&mut self, name: &str, text: &str, context: ConditionalContext)`;
- `analyze(&mut self) -> Result<CompileTimeAnalysis, ResolverError>`;
- `read_names(&self) -> Vec<String>` and `deny_include(&mut self, name: &str)`;
- `activity(analysis: &CompileTimeAnalysis, body: &str) -> Truth` finds the one
  root directive with matching body and returns its recorded activity.

Helpers use shared budgets and explicit source contexts, not the production
home-directory configuration. Failed lookup is recorded as failed lookup, never
an empty child. Add a `NoResources` implementation for context-only tests that
returns `ResolutionOutcome { result: Resolution::Unavailable { reason:
"no resource capability".to_owned() }, observations: Vec::new(), warnings:
Vec::new() }` without reading any filesystem path.

## Execution discipline

For every task: add the listed failing regression, run the named target and
record RED, implement the checked behavior, run that target and relevant earlier
targets and record GREEN, review the task against the spec, then commit only its
named files. Missing new API compile errors can establish RED for a new module;
unrelated harness/compiler failures cannot. Do not mark a task complete merely
because later work could make its tests pass.

Use the `test-driven-development`, `systematic-debugging`, and
`verification-before-completion` skills at their applicable execution boundaries.
Use `jev.review_select` at actual review gates with the changed-file scope and
the user's chosen delegation policy; never treat its selection as a verdict.
Do not push, merge, reset, or discard unrelated changes.
The `git add` commands below assume the named files contain only task changes.
Before each commit, inspect the full diff: when a file also has user/other-session
hunks, stage only the task's hunks or ask before committing mixed work. The
planning-time checkout contains unrelated LSP tracing edits, including in
`project_cache.rs` and `workspace/rename.rs`; preserve them and reread their
updated code before execution. A file being listed here does not authorize
staging or overwriting unrelated hunks within it.

### Task 1: Immutable target facts and source-specific defaults

**Files:** Create `crates/pascal-project/src/layout.rs` and `tests/layout_context.rs`;
modify `src/conditional.rs`, `src/lib.rs`, and `README.md` in that crate.

**Interfaces:** Produce `LayoutContext`, `LayoutSettings`, `LayoutPlatform`,
`LayoutContext::for_target`, `ConditionalContext.layout`, and `.with_layout`.
Existing `conditional_context_for` continues to own source-origin selection.

- [ ] **Step 1: Add the failing target/fingerprint tests.**

```rust
#[test]
fn target_facts_are_not_defines() {
    let version = Some(CompilerVersion::new(37, 0));
    let win32 = LayoutContext::for_target(version, Some(&TargetPlatform::Win32));
    let win64 = LayoutContext::for_target(version, Some(&TargetPlatform::Win64));
    let mut ctx = ConditionalContext::default()
        .with_compiler_version(CompilerVersion::new(37, 0))
        .with_layout(win32.clone());
    let before = ctx.fingerprint();
    ctx.set_define("WIN64", ConditionalFact::True);
    assert_eq!(ctx.layout, win32);
    assert_ne!(before, ctx.clone().with_layout(win64).fingerprint());
    assert_eq!(LayoutContext::default().platform, None);
}
```

- [ ] **Step 2: Add `rf_library_defaults`.** Create a temporary `App.dproj` with
  Win32, compiler 37 supplied through `ProjectOptions.conditional_context`,
  `DCC_Alignment=1`, and a project source plus a library source outside its roots.
  Assert project defaults use alignment 1; library target is still Win32 but
  uses the independently verified compiler default, not project alignment 1.
  Include invalid platform selection, unknown property expression, and absent
  compiler cases that retain unproven defaults.
- [ ] **Step 3: Run RED.**

```bash
cargo test -p pascal-project --test layout_context
```

- [ ] **Step 4: Implement target/default facts and explicit property mapping.**
  Use `DCC_Alignment`, `DCC_MinimumEnumSize`, and `DCC_LongStrings`; their spellings
  were confirmed in the installed Delphi 37 targets file, lines 483–489, but do
  not read or execute that targets file at runtime. Validate resolved values
  (`1,2,4,8,16` alignment where compiler-supported; `1,2,4` enum minimum), and
  preserve property provenance/unknowns. Set target facts only after validating
  build selection; library defaults start independently. Update `Default`, all
  explicit constructors, hashes, and recovery payload traversal. Defaults for
  old layout/real compatibility come only from referenced compiler profiles.

```rust
let version = conditional_context.compiler_version;
let target = platform.as_deref().and_then(TargetPlatform::parse);
let base = LayoutContext::for_target(version, target.as_ref());
library_conditional_context.layout = base.clone();
conditional_context.layout = base;
```

  Apply proven project properties to the project context after this assignment;
  apply explicit caller-supplied facts at their existing precedence, with target
  conflicts remaining unproven rather than trusting mutable source defines.
- [ ] **Step 5: Run GREEN plus public-API regressions.**

```bash
cargo test -p pascal-project --test layout_context
cargo test -p pascal-project --test public_api
```

- [ ] **Step 6: Review and commit this deliverable.**

```bash
git add -- crates/pascal-project/src/layout.rs crates/pascal-project/src/conditional.rs crates/pascal-project/src/lib.rs crates/pascal-project/tests/layout_context.rs crates/pascal-project/README.md
git commit -m "feat(project): preserve immutable Delphi layout target facts"
```

### Task 2: Accounted fact model and verified scalar layouts

**Files:** Create `compile_time/{mod,model,budget,scalars}.rs`,
`tests/compile_time_abi.rs`, and `tests/fixtures/compile_time/abi.toml` under
`crates/pascal-core`; modify `src/lib.rs` to export `compile_time`.

**Interfaces:** Consume Task 1 facts; produce `Fact`, reasons, identities,
`StorageLayout`, `KnownLayout`, `CompileTimeLimits`, `CompileTimeBudget`,
`BuiltinType`, and `builtin_layout` as specified above.

- [ ] **Step 1: Add concrete scalar/budget regressions.**

```rust
#[test]
fn extended_and_longword_have_delphi_not_host_widths() {
    for (platform, extended) in [(LayoutPlatform::Win32, 10), (LayoutPlatform::Win64, 8)] {
        let ctx = ConditionalContext::default()
            .with_compiler_version(CompilerVersion::new(37, 0))
            .with_layout(LayoutContext::for_target(Some(CompilerVersion::new(37, 0)),
                Some(&match platform { LayoutPlatform::Win32 => TargetPlatform::Win32,
                                      LayoutPlatform::Win64 => TargetPlatform::Win64,
                                      LayoutPlatform::Other(name) => TargetPlatform::Other(name) })));
        let e = builtin_layout(BuiltinType::Extended, &ctx, &ctx.layout.defaults).known().unwrap();
        let w = builtin_layout(BuiltinType::LongWord, &ctx, &ctx.layout.defaults).known().unwrap();
        assert_eq!(e.size, extended);
        assert_eq!(w.size, 4);
    }
}
```

  Add tables for every spec scalar, pointer/native widths, `Char` with unknown and
  Unicode/pre-Unicode compiler facts, `String` under known/unknown H settings,
  short-string storage, boolean aliases, and unsupported target/version results.
  Add a cancellation token already set to cancelled and a zero-retained-limit
  budget: both must reject admission before payload processing.
- [ ] **Step 2: Run RED.** `cargo test -p pascal-core --test compile_time_abi`.
- [ ] **Step 3: Implement the closed scalar registry and checked budget.**
  Store widths/alignment from cited ABI facts, not Rust host types. A missing H
  setting blocks `String` but not `Byte`. No unqualified name binding occurs in
  this registry; Task 3 must prove intrinsic identity first. Implement accounting
  admission before copying keys/values and count each branch/provider operation
  against the same request budget.

```rust
fn checked_align_up(offset: u64, alignment: u32) -> Option<u64> {
    let a = u64::from(alignment);
    if a == 0 || !a.is_power_of_two() { return None; }
    offset.checked_add(a - 1).map(|n| n & !(a - 1))
}
```

  Put this shared checked arithmetic helper in `model.rs`; Task 5 consumes it.
  Record each ABI fixture's `compiler`, `platform`, `switches`, `expression`,
  `expected_size`, and `reference` in TOML. Parse with existing serde/TOML
  dependencies; do not invent expected alignment for `Extended` from its size.
- [ ] **Step 4: Run GREEN and existing conditional suites.**

```bash
cargo test -p pascal-core --test compile_time_abi
cargo test -p pascal-core --test conditional_task24 --test conditional_closed_world
```

- [ ] **Step 5: Review/commit.**

```bash
git add -- crates/pascal-core/src/lib.rs crates/pascal-core/src/compile_time/mod.rs crates/pascal-core/src/compile_time/model.rs crates/pascal-core/src/compile_time/budget.rs crates/pascal-core/src/compile_time/scalars.rs crates/pascal-core/tests/compile_time_abi.rs crates/pascal-core/tests/fixtures/compile_time/abi.toml
git commit -m "feat(core): add bounded Delphi scalar layout facts"
```

### Task 3: Resumable declarations, scoped bindings, and aliases

**Files:** Create `compile_time/declarations.rs`, `compile_time/bindings.rs`,
`tests/support/compile_time.rs`, and `tests/compile_time_bindings.rs`; extend
`compile_time/model.rs`/`mod.rs`.

**Interfaces:** Produce `CompileTimeState`, declaration-site identities, the
`feed`/`finish_root`/`site_at`/`layout_name` methods and the `Harness` helpers.
Types outside this task's alias/scalar/pointer subset remain explicit unknown.

- [ ] **Step 1: Write alias/scope and fragment-boundary tests.**

```rust
#[test]
fn rf_shadowed_intrinsic() {
    let h = Harness::plain("unit T; interface type E = Extended; \
        procedure P; implementation procedure P; type Extended = Byte; \
        var x: Extended; begin {LOCAL} end; end.", context(LayoutPlatform::Win32));
    assert_eq!(h.size("E"), Some(10));
    assert_eq!(h.size_at("LOCAL", "Extended"), Some(1));
}
#[test]
fn alias_chains_and_pointer_recursion_keep_identity() {
    let h = Harness::plain("unit T; interface type A=LongWord; B=type A; \
        P=^Node; Node=record Next:P; end; implementation end.",
        context(LayoutPlatform::Win64));
    assert_eq!(h.size("B"), Some(4));
    assert_eq!(h.size("P"), Some(8));
}
```

  For the pointer query, the pointee record may still be unsupported at this
  stage. Assert that `A=A` by-value cycles, a forward alias queried before its
  binding, missing/recovered declarations, and same-named hidden locals do not
  produce known layouts. Add comments/strings containing `type`/`end` as lexical
  scope non-events. Feed `type A = `, `Long`, and `Word;` as three adjacent
  same-source fragments: token continuation yields `LongWord`, not two names.
- [ ] **Step 2: Run RED.** `cargo test -p pascal-core --test compile_time_bindings`.
- [ ] **Step 3: Implement stream and binder in small loops.** Preserve lexer
  state across adjacent chunks; flush complete declarative forms only. Use
  tree-sitter declaration fields (`declType.name/type`, `declVar.name/type`,
  `declArg`, record fields) and reject error/missing nodes for admitted facts.
  Maintain routine/record/block scopes and declaration sequence. Resolve an
  alias's target at its declaration site, with an active by-value query set;
  stop pointer layout at proven pointer width. Do not clear all global type facts
  simply because a routine begins, or retain local facts after its scope ends.

```rust
// Alias layout uses its captured site, never the later use site's shadows.
let target = self.resolve_type_at(alias.declaration_site, &alias.target, budget);
match target {
    Fact::Known(mut layout) => { layout.identity = TypeIdentity::Declared(alias.key.clone()); Fact::Known(layout) }
    Fact::Unknown(reason) => Fact::Unknown(reason),
    Fact::Incomplete(reason) => Fact::Incomplete(reason),
}
```

  This snippet belongs inside the alias-query branch; its `alias` descriptor is
  the binder's stored declaration with `declaration_site`, `target`, and `key`.
  Implement `fingerprint`/payload visitation now, including pending lexer state,
  scope stack, declaration sites, and provenance; later tasks extend their data.
- [ ] **Step 4: Implement all declared `Harness` helpers used so far.** Use
  accounted queries and physical marker-to-site lookup, not a second independent
  size parser. `with_layout` becomes active in Task 5; leave it absent until then.
- [ ] **Step 5: Run GREEN plus ABI tests.**

```bash
cargo test -p pascal-core --test compile_time_bindings --test compile_time_abi
```

- [ ] **Step 6: Review/commit.**

```bash
git add -- crates/pascal-core/src/compile_time/declarations.rs crates/pascal-core/src/compile_time/bindings.rs crates/pascal-core/src/compile_time/model.rs crates/pascal-core/src/compile_time/mod.rs crates/pascal-core/tests/support/compile_time.rs crates/pascal-core/tests/compile_time_bindings.rs
git commit -m "feat(core): bind compile-time declarations in lexical source order"
```

### Task 4: Ordinal bounds and array storage

**Files:** Create `compile_time/arrays.rs`, `tests/compile_time_arrays.rs`; extend
`declarations.rs`, `bindings.rs`, `model.rs`, `mod.rs`, and `abi.toml` as needed.

**Interfaces:** Consume captured type/bound sites and scalar layouts. Produce
`array_layout` internally, called by `CompileTimeState::layout_name` and later
operand queries; exported query signatures do not change.

- [ ] **Step 1: Add static/dynamic/ordinal tests.**

```rust
#[test]
fn rf_ordinal_bounds() {
    let h = Harness::plain("unit T; interface type Color=(Red,Green,Blue); \
        A=array[-2..2,1..3] of Word; B=array[Color] of Byte; \
        C=array of Byte; implementation end.", context(LayoutPlatform::Win64));
    assert_eq!(h.size("A"), Some(30));
    assert_eq!(h.size("B"), Some(3));
    assert_eq!(h.size("C"), Some(8));
}
#[test]
fn array_size_does_not_wrap() {
    let h = Harness::plain("unit T; interface type A=array[0..9223372036854775807] \
        of Int64; implementation end.", context(LayoutPlatform::Win64));
    assert_eq!(h.size("A"), None);
}
```

  Add boolean/subrange index types, earlier constant bounds and checked bound
  arithmetic, invalid reversed bounds, unknown bound names, multidimensional
  index counts, and open-array parameter distinctions. Nested-record element
  stride assertions are added when Task 5 supplies record layout.
- [ ] **Step 2: Run RED.** `cargo test -p pascal-core --test compile_time_arrays`.
- [ ] **Step 3: Implement checked counts and retained bound binding.**

```rust
let count = upper.checked_sub(lower).and_then(|n| n.checked_add(1));
let count = count.and_then(|n| u64::try_from(n).ok());
let size = count.and_then(|n| element.storage.size.checked_mul(n));
```

  For several dimensions, fold with checked multiplication before element size.
  Resolve every bound at its captured declaration site using the existing typed
  constant-expression semantics; typed ordinals retain domain cardinality
  independently of enum storage width. Dynamic-array storage is pointer width;
  dereferenced/indexed element layout remains separate. Reject unsupported open
  forms and invalid operand/type contexts rather than guessing descriptor sizes.
- [ ] **Step 4: Run GREEN and alias/scalar suites.**

```bash
cargo test -p pascal-core --test compile_time_arrays --test compile_time_bindings --test compile_time_abi
```

- [ ] **Step 5: Review/commit named array deliverable files.**

```bash
git add -- crates/pascal-core/src/compile_time/arrays.rs crates/pascal-core/src/compile_time/declarations.rs crates/pascal-core/src/compile_time/bindings.rs crates/pascal-core/src/compile_time/model.rs crates/pascal-core/src/compile_time/mod.rs crates/pascal-core/tests/compile_time_arrays.rs crates/pascal-core/tests/fixtures/compile_time/abi.toml
git commit -m "feat(core): evaluate checked ordinal array layouts"
```

### Task 5: Records, variants, and declaration-site switches

**Files:** Create `compile_time/records.rs`, `tests/compile_time_records.rs`;
extend stream/bindings/model/scalars/modules, support helpers, array tests,
and `abi.toml` in `pascal-core`.

**Interfaces:** Produce record layout and `apply_layout_directive`; Task 3's
query API remains stable. The binder snapshots `LayoutSettings` per definition.

- [ ] **Step 1: Add packing/grouping/variant tests.**

```rust
#[test]
fn packed_variants_count_only_instance_storage() {
    let h = Harness::plain("unit T; interface type \
        P=packed record X,Y:Byte; Z:LongWord; end; \
        V=packed record case Tag:Byte of 0:(A:Word);1:(B:Int64); end; \
        U=packed record case Byte of 0:(A:Word);1:(B:Int64); end; \
        implementation end.", context(LayoutPlatform::Win32));
    assert_eq!(h.size("P"), Some(6));
    assert_eq!(h.size("V"), Some(9));
    assert_eq!(h.size("U"), Some(8));
}
```

  Add ordinary Byte+LongWord=8 under explicit alignment 8; packed equivalent=5;
  nested records, arrays of padded records, nested alternatives, methods and
  class/static fields excluded, unknown conditional fields, duplicate/recovered
  field declarations rejected, and unknown legacy settings returning unknown.
- [ ] **Step 2: Add `rf_declaration_settings`.** Create a harness with known
  defaults, apply A1 and feed `type Early=record B:Byte; I:LongWord; end;`, then
  apply A8 and feed an equivalent `Late`; assert `Early=5`, `Late=8`, and an
  alias of `Early` still equals 5. Check Z/H/OLDTYPELAYOUT/REALCOMPATIBILITY
  unknowns affect only dependent types. Use `Harness::stream("unit T; interface ",
  context(LayoutPlatform::Win32))`, append `implementation end.`, then `finish()`.
  Model supported save/restore directives;
  unknown/unmatched restore cannot fabricate defaults.
- [ ] **Step 3: Run RED.** `cargo test -p pascal-core --test compile_time_records`.
- [ ] **Step 4: Implement per-field checked layout.**

```rust
let field_alignment = if record.packed { 1 } else { field.storage.alignment.min(cap) };
let start = checked_align_up(offset, field_alignment);
let end = start.and_then(|n| n.checked_add(field.storage.size));
```

  Use verified target/compiler rules for final padding and variant overlap:
  align alternatives at the common variant area, include a named tag's storage,
  take the largest complete alternative, then apply the record's final alignment.
  Invalid or uncertain field layouts propagate unknown, not a skipped field.
  Preserve definition-site settings across nested types and aliases. Record each
  nontrivial fixture's referenced expected layout before claiming it is modeled.
- [ ] **Step 5: Run GREEN and array/alias/scalar tests.**

```bash
cargo test -p pascal-core --test compile_time_records --test compile_time_arrays --test compile_time_bindings --test compile_time_abi
```

- [ ] **Step 6: Review/commit.**

```bash
git add -- crates/pascal-core/src/compile_time/records.rs crates/pascal-core/src/compile_time/declarations.rs crates/pascal-core/src/compile_time/bindings.rs crates/pascal-core/src/compile_time/model.rs crates/pascal-core/src/compile_time/scalars.rs crates/pascal-core/src/compile_time/mod.rs crates/pascal-core/tests/support/compile_time.rs crates/pascal-core/tests/compile_time_records.rs crates/pascal-core/tests/compile_time_arrays.rs crates/pascal-core/tests/fixtures/compile_time/abi.toml
git commit -m "feat(core): prove record layouts with captured compiler switches"
```

### Task 6: Static type/access operands for SizeOf

**Files:** Create `compile_time/operands.rs`, `tests/compile_time_operands.rs`;
extend bindings/model/modules, declaration parameter extraction, and harness.

**Interfaces:** Produce `SizeOfOperand`, `parse_sizeof_operand`,
`CompileTimeState::layout_operand`, and `Harness::query`.

- [ ] **Step 1: Add access/scope regressions.**

```rust
#[test]
fn sizeof_access_uses_static_type_without_execution() {
    let h = Harness::plain("unit T; interface type TRecord=packed record V:Int64; end; \
        TPtr=^TRecord; TArray=array[-1..1] of TRecord; var p:TPtr; a:TArray; i:Integer; \
        implementation procedure Test(var x:TRecord); begin {HERE} end; end.",
        context(LayoutPlatform::Win32));
    for operand in ["x", "x.V", "p^", "p^.V", "a[i]", "a[i].V"] {
        assert_eq!(h.query("HERE", operand).known().unwrap().storage.size, 8);
    }
    assert_eq!(h.query("HERE", "p").known().unwrap().storage.size, 4);
}
```

  Also test a local variable called
  `Extended: Byte`, qualified variables, grouped parameters, `out`/`const`/`var`
  passing, invalid field, wrong index count/type, untyped pointers, empty/comma
  operands, nested calls/properties, and unsupported generic forms. All unproven
  cases return a reason rather than a known scalar by familiar spelling.
- [ ] **Step 2: Run RED.** `cargo test -p pascal-core --test compile_time_operands`.
- [ ] **Step 3: Implement a bounded non-executing operand parser and binder.**
  Tokenize argument names/selectors with span preservation and existing 4 KiB /
  256-token expression limits. Resolve namespace/unit prefixes through authorized
  bindings; a field suffix is not automatically a unit qualifier. Index binding
  checks a proven ordinal expression type but does not read its value. Typed
  pointer dereference resolves the pointee type; no memory operation occurs.

```rust
match selector {
    Selector::Field(name) => binding.record_field(name, site, budget),
    Selector::Index(indices) => binding.array_element(indices, site, budget),
    Selector::Dereference => binding.pointee(site, budget),
}
```

  `Selector` is the operand representation created in this task; the three
  binding methods return `Fact<BoundType>`, where private `BoundType` represents
  an intrinsic or declaration-bound `TypeExpr` plus its binding site/provenance.
  Keep `SizeOf(65536)` and unsupported expression operands conservative; this
  task does not infer widths from current constant values.
- [ ] **Step 4: Run GREEN plus all earlier layout targets.**

```bash
cargo test -p pascal-core --test compile_time_operands --test compile_time_records --test compile_time_arrays --test compile_time_bindings --test compile_time_abi
```

- [ ] **Step 5: Review/commit.**

```bash
git add -- crates/pascal-core/src/compile_time/operands.rs crates/pascal-core/src/compile_time/bindings.rs crates/pascal-core/src/compile_time/declarations.rs crates/pascal-core/src/compile_time/model.rs crates/pascal-core/src/compile_time/mod.rs crates/pascal-core/tests/support/compile_time.rs crates/pascal-core/tests/compile_time_operands.rs
git commit -m "feat(core): resolve static SizeOf type and access operands"
```

### Task 7: Source-order SizeOf conditions and complete branch-state merges

**Files:** Create `compile_time/session.rs` and
`tests/compile_time_conditionals.rs`; modify `conditional.rs`, the new core
modules, and `tests/support/compile_time.rs`.

**Interfaces:** Produce `CompileTimeResolver`, `ProviderSource`, occurrence/output
carriers, `VerifiedUnitHeader` data carrier, and `analyze_source`. Extend
`ConditionalEnvironment` with lossless typed snapshots; preserve existing
context-only/include-callback entry points. This task evaluates a root without
textual include/provider facts; those capabilities explicitly remain unknown.

- [ ] **Step 1: Add the failing expression/source-order tests.**

```rust
#[test]
fn sizeof_selects_target_branch_and_admits_initializer_value() {
    let source = "unit T; interface const Width=SizeOf(LongWord); \
        {$IF Width=4}{$DEFINE FIXED}{$ENDIF} \
        {$IF SizeOf(Extended)>=10}{$DEFINE WIDE}{$ELSE}{$DEFINE NARROW}{$ENDIF} \
        implementation end.";
    for (platform, wide, narrow) in [
        (LayoutPlatform::Win32, Truth::True, Truth::False),
        (LayoutPlatform::Win64, Truth::False, Truth::True),
    ] {
        let mut f = Fixture::new(source, context(platform));
        let a = f.analyze().unwrap();
        assert!(a.root.complete);
        assert_eq!(activity(&a, "DEFINE FIXED"), Truth::True);
        assert_eq!(activity(&a, "DEFINE WIDE"), wide);
        assert_eq!(activity(&a, "DEFINE NARROW"), narrow);
    }
}
#[test]
fn unknown_branches_do_not_splice_different_type_definitions() {
    let mut f = Fixture::new("unit T; interface {$IF Mystery} \
        type R=packed record X:Byte; end; {$ELSE} \
        type R=packed record X:Int64; end; {$ENDIF} \
        {$IF SizeOf(R)=1}{$DEFINE WRONG}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32));
    assert_eq!(activity(&f.analyze().unwrap(), "DEFINE WRONG"), Truth::Unknown);
}
```

  Add the full operand categories through `IF` and `const` initialization,
  source-defined switches, local parameter scopes, declaration-before-use,
  equal-size/different-identity branch merges, inactive malformed declarations,
  mixed legacy function expressions, and malformed `SizeOf` arity/syntax.
  Retain `SizeOf(65536)` as unknown; replace the old test's supposed unsupported
  `SizeOf(Integer)` initializer with `UnsupportedFunction(Integer)` so the old
  unsupported-initializer safety assertion remains meaningful.
- [ ] **Step 2: Run RED on new and existing expression tests.**

```bash
cargo test -p pascal-core --test compile_time_conditionals
cargo test -p pascal-core --test conditional_task24 --test conditional_closed_world
```

- [ ] **Step 3: Feed declaration regions before each directive.** The session
  supplies exact logical binding sites, and records pending lexical/declaration
  state separately from end-of-root incompleteness. Replace the `"sizeof" =>
  Value::Error` arm with argument-span parsing and the proven-layout bridge;
  invoke it before the generic value-function argument evaluator consumes the
  type identifier. Keep numeric/string functions' existing arity checks.

```rust
match state.layout_operand(site, &operand, budget) {
    Fact::Known(layout) => i64::try_from(layout.storage.size)
        .map(Value::Number).unwrap_or(Value::Unknown),
    Fact::Unknown(_) => Value::Unknown,
    Fact::Incomplete(reason) => { incomplete.push(reason); Value::Unknown }
}
```

  `Value` is the existing private conditional-expression result. The session's
  `incomplete` reason collection and directive decisions distinguish failed
  admission from syntactically valid unknown layout.
- [ ] **Step 4: Integrate every source constant with the scoped binder.** Admit
  the completed initializer before the next `IF`, array bound, or type query.
  Keep explicit context constants as explicit facts, but let proven local
  declarations shadow them without corrupting unrelated outer facts. Capture
  declaration expression sites and reuse the existing checked arithmetic/string
  evaluator rather than introducing a different constant language.
- [ ] **Step 5: Merge complete typed state.** Fork snapshots with charged
  retained bytes; include pending fragments, scope/sequence maps, switch stack,
  alias identities, variable types, constants, and dependencies. Preserve a
  known result only when every reachable path proves it; equal size can be
  merged for a `SizeOf` value without inventing one declaration identity/location.
  Bound alternatives at 64; on exhaustion return incomplete, never select the
  cheapest branch. Update environment hash/length/payload accounting and
  `to_context()` documentation: it is not a typed snapshot.
- [ ] **Step 6: Run GREEN for core and scalar/layout regression targets.**

```bash
cargo test -p pascal-core
```

- [ ] **Step 7: Review/commit only the shared-expression deliverable.**

```bash
git add -- crates/pascal-core/src/conditional.rs crates/pascal-core/src/compile_time/session.rs crates/pascal-core/src/compile_time/model.rs crates/pascal-core/src/compile_time/bindings.rs crates/pascal-core/src/compile_time/declarations.rs crates/pascal-core/src/compile_time/mod.rs crates/pascal-core/tests/support/compile_time.rs crates/pascal-core/tests/compile_time_conditionals.rs crates/pascal-core/tests/conditional_closed_world.rs
git commit -m "feat(core): evaluate SizeOf in source-order conditional state"
```

### Task 8: Textual includes continue declarations and drive later reads

**Files:** Create `tests/compile_time_includes.rs`; extend `session.rs`,
`declarations.rs`, `bindings.rs`, `conditional.rs`, support harness, and
`resolver.rs`. No LSP-only second-pass repair.

**Interfaces:** Implement the include capability of `analyze_source` and produce
`UnitResolver::analyze_compile_time(&mut self, root: &LoadedSource,
cancel: &dyn CancellationToken) -> Result<CompileTimeAnalysis, ResolverError>`.
It uses `context.conditional_context_for(&root.path)` and shared resolver limits.
The generic resource adapter consumes existing include resolution/observations;
unit-provider loading is completed in Task 9.

- [ ] **Step 1: Add included constants and later read-selection tests.**

```rust
#[test]
fn included_initializer_controls_later_include_reads() {
    let mut f = Fixture::new("unit T; interface const Revision={$I revision.inc}; \
        {$IF Revision=7}{$I chosen.inc}{$ELSE}{$I forbidden.inc}{$ENDIF} \
        implementation end.", context(LayoutPlatform::Win32));
    f.include("revision.inc", "7");
    f.include("chosen.inc", "type Small=array[1..Revision] of Byte;");
    f.deny_include("forbidden.inc");
    let a = f.analyze().unwrap();
    assert!(a.root.complete);
    assert_eq!(f.read_names(), vec!["revision.inc", "chosen.inc"]);
    assert!(a.reasons.is_empty());
}
#[test]
fn rf_partial_initializer_failure() {
    let mut f = Fixture::new("unit T; interface const Revision=1+{$I missing.inc}; \
        {$IF Revision=1}{$DEFINE WRONG}{$ENDIF} implementation end.",
        context(LayoutPlatform::Win32));
    f.deny_include("missing.inc");
    let a = f.analyze().unwrap();
    assert!(!a.root.complete || !a.reasons.is_empty());
    assert_eq!(activity(&a, "DEFINE WRONG"), Truth::Unknown);
}
```

  Add nested child payload `{$I inner.inc}` with `inner.inc=7`, several include
  fragments in one expression, missing/cyclic/ambiguous/unauthorized children,
  different repeated occurrences in different lexical scopes, child switches
  affecting only types declared afterwards, and equal/unequal conditional
  initializer fragments. An unknown branch containing an include must not be
  read as if proven active or quietly treated as empty.
- [ ] **Step 2: Run RED.** `cargo test -p pascal-core --test compile_time_includes`.
- [ ] **Step 3: Walk children with the parent's continuation state.** On a
  proven active include, authorize/resolve its exact source/range request and
  feed child fragments into the same declaration stream; on child exit resume
  the parent's physical source, lexical scope, and logical sequence. Child EOF
  does not finalize a parent's initializer; only root completion does. Store
  occurrence IDs and exact include results, and preserve dependency observations.

```rust
// These are session private methods, implemented in this task.
self.enter_include(owner, directive, child_source, budget)?;
self.walk_child_preserving_declaration(resolver, budget)?;
self.resume_parent_source(budget)?;
```

  All three methods take the shared session state/budget; they cannot replace
  a failed child with an empty `SourceFragment`. The callback compatibility path
  must carry the typed continuation as well, or explicitly report missing
  capability rather than return a complete false-negative analysis.
- [ ] **Step 4: Replace fragment-scanner false failures.** A fragment ending
  inside a valid declaration is pending, not malformed. After the complete
  declaration arrives, discharge that specific pending token/occurrence reason.
  Genuine missing reads/cycles/limit failures remain attached. Never remove all
  `conditional analysis is incomplete` warnings by substring matching.
- [ ] **Step 5: Connect the generic `UnitResolver` include walk.** Preserve
  `ResolvedInclude` occurrence/source IDs, legacy routes, source decoding,
  observations, and include budget charges. The resource adapter borrows the
  resolver once per root; do not create a fresh unbounded resolver for every
  child. Update active import decisions from the source-order result rather than
  reanalyzing raw text with no include values.
- [ ] **Step 6: Run GREEN and shared include/resolver regressions.**

```bash
cargo test -p pascal-core --test compile_time_includes --test compile_time_conditionals
cargo test -p pascal-core --test unit_resolver --test conditional_task24 --test conditional_closed_world
```

- [ ] **Step 7: Review/commit.**

```bash
git add -- crates/pascal-core/src/conditional.rs crates/pascal-core/src/resolver.rs crates/pascal-core/src/compile_time/session.rs crates/pascal-core/src/compile_time/declarations.rs crates/pascal-core/src/compile_time/bindings.rs crates/pascal-core/tests/support/compile_time.rs crates/pascal-core/tests/compile_time_includes.rs
git commit -m "fix(core): preserve constant initializer state across includes"
```

### Task 9: Authorized imported interface facts and provider cycles

**Files:** Create `tests/compile_time_providers.rs`; extend `session.rs`,
`bindings.rs`, `resolver.rs`, and support harness. Update explicit
`ResolvedProject` constructors in `crates/lint4d/tests/cfg_project_snapshot_test.rs`
and `crates/lint4d/tests/engine_run_test.rs` for the source-context carrier.

**Interfaces:** Complete `CompileTimeResolver::unit` via existing unit lookup;
compile provider interfaces into opaque `InterfaceFacts` with fresh source-local
environments and remapped declaration keys. `UnitResolver::analyze_compile_time`
continues to return the same root/output carrier.
Add `pub conditional_contexts: HashMap<SourceId, ConditionalContext>` to
`ResolvedProject`; `resolve_project_from_source` populates it for each root/unit
from the selected project context's `conditional_context_for(&source.path)`.
Textual include occurrences inherit their parent's continuation state rather
than obtain an independent context from this map. Existing test constructors
can use an empty map when testing raw snapshots; configured/provider fixtures
must supply explicit contexts. This extends a public data carrier, so update all
constructor sites and verify downstream compilation within this task.

- [ ] **Step 1: Add qualified/provider/shadowing tests.**

```rust
#[test]
fn imported_alias_layout_uses_provider_scope_and_context() {
    let mut f = Fixture::new("unit Main; interface uses Types; \
        {$DEFINE PRIVATE} {$IF SizeOf(Types.Export)=8}{$DEFINE HIT}{$ENDIF} \
        implementation end.", context(LayoutPlatform::Win32));
    f.unit("Types", "unit Types; interface {$IFDEF PRIVATE} \
        type Export=Byte; {$ELSE} type Export=Int64; {$ENDIF} \
        implementation type Hidden=Byte; end.",
        context(LayoutPlatform::Win32).with_absent_define(Truth::False));
    let a = f.analyze().unwrap();
    assert_eq!(activity(&a, "DEFINE HIT"), Truth::True);
}
```

  The fixture's closed provider context is explicit evidence for this test,
  not permission to close real library contexts. Add open provider defines,
  namespace/unit aliases, same-named exports and local shadows, attempted access
  to implementation-private types, unavailable/DCU-only providers, recursive
  interface imports, and ambiguous providers whose candidate sizes happen to
  match. Assert unknown/ambiguous bindings remain unproven.
- [ ] **Step 2: Run RED.** `cargo test -p pascal-core --test compile_time_providers`.
- [ ] **Step 3: Implement bounded lazy interface compilation.** Resolve unit
  sources through existing `UnitResolveRequest` precedence, including library
  contexts and authorized qualifiers. Compile only proven visible interface
  declarations; do not use importer source defines, settings, or local scopes.
  Copy/remap exported type graphs by `DefinitionKey`; an imported alias retains
  its provider declaration site and dependencies. Query a provider only after
  a proven import authorizes that qualifier/binding.

```rust
if !active_provider_queries.insert(key.clone()) {
    return Fact::Unknown(UnknownReason::CyclicProvider);
}
let result = compile_provider_interface(source, source_context, resolver, budget);
active_provider_queries.remove(&key);
```

  `key` includes unit/source identity, revision, target/context fingerprint, and
  export visibility. `compile_provider_interface` is a private session method
  introduced here. Every provider uses the root's budget and existing resolver
  observations; no recursive self-owned LSP index or directory-wide scan.
- [ ] **Step 4: Test provider freshness and no-read safety.** A denied provider
  path is never opened; a changed provider revision cannot return old exported
  layout; an incomplete interface cannot export names whose binding/visibility
  depends on unresolved regions. Pointer cycles do not require by-value layout.
- [ ] **Step 5: Run GREEN and earlier session/resolver suites.**

```bash
cargo test -p pascal-core --test compile_time_providers --test compile_time_includes --test compile_time_conditionals --test unit_resolver
```

- [ ] **Step 6: Review/commit.**

```bash
git add -- crates/pascal-core/src/compile_time/session.rs crates/pascal-core/src/compile_time/bindings.rs crates/pascal-core/src/resolver.rs crates/pascal-core/tests/support/compile_time.rs crates/pascal-core/tests/compile_time_providers.rs crates/lint4d/tests/cfg_project_snapshot_test.rs crates/lint4d/tests/engine_run_test.rs
git commit -m "feat(core): resolve compile-time layouts from authorized interfaces"
```

### Task 10: Source-backed unit-header proofs and FireDAC preambles

**Files:** Create `compile_time/header.rs`, `tests/compile_time_headers.rs`;
modify modules/session and resolver candidate validation in `resolver.rs` around
`accept_unit_candidates` / `parse_source_metadata`.

**Interfaces:** Produce `verify_unit_header` and populate
`CompileTimeAnalysis.header` with `VerifiedUnitHeader` when independently proven.
Cached declared-name shortcuts are usable only with equivalent source/context
proof; the existing `SourceStore::declared_unit_name` API is not itself a
conditional-header proof.

- [ ] **Step 1: Add raw-preamble and post-header-body tests.**

```rust
#[test]
fn firedac_preamble_does_not_hide_actual_header() {
    let mut f = Fixture::new("{$IF Defined(IOS) OR Defined(ANDROID)} \
        {$HPPEMIT LINKUNIT}{$ELSE}{$HPPEMIT '#pragma link unit'}{$ENDIF} \
        unit FireDAC.Phys.IB; interface implementation end.",
        context(LayoutPlatform::Win32));
    let header = f.analyze().unwrap().header.known().unwrap();
    assert_eq!(header.name.to_ascii_lowercase(), "firedac.phys.ib");
    assert_eq!(header.name_parts.len(), 3);
}
```

  Add necessary pre-header include resolution; missing/ambiguous include blocks
  proof. Add body `{$IF UnsupportedFunction(T)}...` after an unconditional unit
  header: header remains proven, body analysis remains unknown/incomplete. Test
  fake `unit` in strings/comments/uses, inactive/unknown/duplicate headers,
  mismatched declared names, and case-equivalent ambiguous source candidates.
- [ ] **Step 2: Run RED.**

```bash
cargo test -p pascal-core --test compile_time_headers
cargo test -p pascal-core --test unit_resolver firedac
```

- [ ] **Step 3: Implement prefix-only header validation.** Lex source-backed
  unit/name/semicolon tokens outside comments/strings; interpret required prefix
  directives/includes through the same session. Reject preceding actual Pascal
  declarations or uncertain header activity. Do not mask an unknown branch
  containing Pascal tokens and then pretend the surviving text is a proof.
  Preserve physical declaration/name-part anchors and stop at the semicolon.

```rust
match verify_unit_header(&source, &context, resources, limits, cancel)? {
    Fact::Known(header) if declared_name_matches(Some(&header.name), requested, resolved) => accept(source, header),
    Fact::Known(_) | Fact::Unknown(_) => reject_candidate(source),
    Fact::Incomplete(reason) => mark_candidate_incomplete(source, reason),
}
```

  The match is the candidate-validation decision, using the existing name-match
  function and candidate collection. Preserve negative observations and search
  precedence; rejected proof cannot silently fall back to a lower-priority guess
  when ambiguity/incompleteness blocks that lookup.
- [ ] **Step 4: Add work-bound and stale-name tests.** A huge/unsupported body
  does not cause full-body parsing to prove the header. Changing only a prefix
  include or target invalidates a cached name proof even with an unchanged root.
- [ ] **Step 5: Run GREEN and core resolver tests.**

```bash
cargo test -p pascal-core --test compile_time_headers --test unit_resolver
```

- [ ] **Step 6: Review/commit.**

```bash
git add -- crates/pascal-core/src/compile_time/header.rs crates/pascal-core/src/compile_time/session.rs crates/pascal-core/src/compile_time/mod.rs crates/pascal-core/src/resolver.rs crates/pascal-core/tests/compile_time_headers.rs crates/pascal-core/tests/unit_resolver.rs
git commit -m "fix(core): validate unit headers beyond directive preambles"
```

### Task 11: CLI/CFG adapter uses the same declaration and layout state

**Files:** Modify `crates/lint4d/src/cfg/project_snapshot.rs`; create
`crates/lint4d/tests/cfg_compile_time_layout.rs`.

**Interfaces:** Consume the shared session and occurrence decisions. Preserve
the existing `to_cfg_project_snapshot(ResolvedProject, CfgSnapshotOptions)` API
and cfg-pascal source-map/preparation carriers. Do not add `pascal-core` to
cfg-pascal or undertake TASK-47's evaluator deduplication. Create private
`SnapshotResources<'a>` borrowing the resolved project; implement
`CompileTimeResolver` with only its authorized imports/includes and supplied
source bytes. No new filesystem reads or live `UnitResolver` are available in
this adapter. Select per-unit contexts from `ResolvedProject.conditional_contexts`;
an explicitly supplied options context can serve the root of legacy snapshots,
but must not be inherited as a provider context. An unavailable provider context
remains unknown. Replay project observations without attributing an unproven
read to this snapshot adapter.

- [ ] **Step 1: Add real adapter regressions.** Use temporary files and the
  existing `UnitResolver`/`to_cfg_project_snapshot` path with a complete explicit
  configuration. The source for the positive test is:

```pascal
program App;
const Revision = {$I revision.inc};
{$IF (Revision = 7) AND (SizeOf(LongWord) = 4)}
procedure Chosen; begin end;
{$ELSE}
procedure Forbidden; begin end;
{$ENDIF}
begin Chosen; end.
```

  `revision.inc` contains `7`. Assert prepared bytes contain `Chosen` and no
  declaration of `Forbidden`, and the prepared source map points copied tokens
  to their original files. A denied initializer include retains the established
  incomplete/file-local fallback and must not publish a complete projection.
  Repeat an include with different entry scopes: preserve occurrence identity
  or retain a deliberate adapter limitation rather than reuse another context.
- [ ] **Step 2: Run RED.** `cargo test -p lint4d --test cfg_compile_time_layout`.
- [ ] **Step 3: Replace adapter-owned constant/include state with shared session
  outputs.** `project_source_occurrence` must not snapshot only defines/constants
  or finalize a child declaration at its source boundary. Supply proven masked
  projection and exact active include selections to the existing strict splicer;
  do not ask its boolean-only evaluator to interpret `SizeOf` again. Keep
  single-source diagnostic mapping rules and source-specific provider contexts.

```rust
let mut resources = SnapshotResources::new(&project);
let evaluated = analyze_source(&root_source, &root_context, &mut resources,
    CompileTimeLimits::default(), &NoCancellation)?;
if !evaluated.reasons.is_empty() || evaluated.root.unknown_activity_requires_fail_closed() {
    return retain_incomplete_snapshot(evaluated);
}
let prepared_inputs = project_occurrences_for_cfg(evaluated)?;
```

  `SnapshotResources::new(project: &ResolvedProject) -> SnapshotResources<'_>`
  borrows the existing imports/includes/contexts. `root_context` is the proven
  context selected above. `retain_incomplete_snapshot` and
  `project_occurrences_for_cfg` are private
  adapter helpers implemented here around the current fallback/projection
  machinery; they do not introduce a new public cfg-pascal evaluator API.
- [ ] **Step 4: Run GREEN and existing CFG/CLI suites.**

```bash
cargo test -p lint4d --test cfg_compile_time_layout
cargo test -p cfg-pascal
cargo test -p lint4d
```

- [ ] **Step 5: Review/commit.**

```bash
git add -- crates/lint4d/src/cfg/project_snapshot.rs crates/lint4d/tests/cfg_compile_time_layout.rs
git commit -m "fix(lint): project compile-time layouts and include values consistently"
```

### Task 12: LSP expansion, audits, and verified header-only fallback

**Files:** Create `crates/pascal-lsp/tests/compile_time_layout.rs`; modify
`src/include_expansion.rs`, `src/navigation.rs`, `src/workspace.rs`, and
`src/workspace/rename.rs` in that crate. Add `src/workspace/compile_time.rs` for
the focused resource/output adapter rather than growing `workspace.rs` further.

**Interfaces:** Shared session outputs convert into existing `ExpandedSource` /
`ExpansionResult` and index inputs. `ParsedDocument` retains an optional proven
physical header with its context/dependency proof. The public Workspace API
and ordinary symbol-query safety gates do not change.

- [ ] **Step 1: Add workspace-based navigation regressions.** Use the existing
  `write` helper pattern and `Workspace::with_override_session` with
  `OverrideSession::new(None)`; supply compiler 37 in
  `WorkspaceOptions.conditional_context` and selected Win32/Win64 options.
  Generate this entire fixture in the new test file:

```pascal
unit Main;
interface
uses PreambleUnit, SysUtils, Base;
{$IF SizeOf(Extended) >= 10}
uses WideProvider;
{$ELSE}
uses NarrowProvider;
{$ENDIF}
implementation
end.
```

  Define `PreambleUnit` with the Task 10 preamble; `System.SysUtils` with an
  unconditional header and a `SizeOf(Extended)` body; `Base` with the Task 8
  included constant. Provide an explicit `SysUtils -> System.SysUtils` unit
  alias in fixture project metadata. Query each uses-entry identifier and assert
  actual physical header URI/range; query the branch providers and verify only
  the target-appropriate one binds. Include unsupported-body/known-header and
  unknown-header/no-jump fixtures.
- [ ] **Step 2: Add `rf_decoded_header_map`.** Write a UTF-16 file with a
  multibyte/comment prefix before its unit declaration, open the equivalent
  decoded UTF-8 overlay, and assert both paths return the same LSP physical
  identifier position. An include-spanning declaration has no guessed editable
  span; reference/rename tests must retain their established incomplete error.
- [ ] **Step 3: Run RED.** `cargo test -p pascal-lsp --test compile_time_layout`.
- [ ] **Step 4: Convert shared occurrence decisions to mapped expansion.** Keep
  exact physical/synthetic segments and dependencies. Known includes contribute
  child occurrences in order; unknown/missing includes remain barriers. Remove
  only discharged, typed provisional continuation reasons, not warning substrings.
  Do not drop branch-local layout state when a cached expansion is indexed.
- [ ] **Step 5: Use the resource adapter in navigation and audit paths.** The
  new workspace child module borrows the current source store/overlays and
  context, reuses authorized shared resolver operations, and polls both request
  cancellation and reconciliation budget before callbacks. Replace independent
  `analyze_with_include_callback` entry/exit assumptions in `workspace/rename.rs`
  without weakening audit completeness or provider-read policy.
- [ ] **Step 6: Add the narrowly scoped header fallback.** Store proven header
  data independently of uncertain body symbols. When resolving a uses-entry
  unit definition, return the selected provider's original header only if the
  source/context/dependency proof is current. Do not mark its body symbols
  known, enable unsafe rename, or treat a fallback filename as a header.

```rust
if target == NavigationTarget::Definition && query_is_unit_import {
    if let Some(header) = selected_provider.current_verified_header() {
        return map_verified_header_location(header);
    }
}
```

  `current_verified_header` and `map_verified_header_location` are private
  adapter/index helpers implemented here. Normal binding/ambiguity selection
  runs before this branch; it cannot bypass an unresolved selected provider.
- [ ] **Step 7: Run GREEN plus all LSP tests.**

```bash
cargo test -p pascal-lsp --test compile_time_layout
cargo test -p pascal-lsp
```

- [ ] **Step 8: Review/commit.**

```bash
git add -- crates/pascal-lsp/src/include_expansion.rs crates/pascal-lsp/src/navigation.rs crates/pascal-lsp/src/workspace.rs crates/pascal-lsp/src/workspace/compile_time.rs crates/pascal-lsp/src/workspace/rename.rs crates/pascal-lsp/tests/compile_time_layout.rs
git commit -m "fix(lsp): share compile-time facts and preserve proven unit jumps"
```

### Task 13: Freshness, recovery accounting, and bounded-work regressions

**Files:** Modify LSP `project_cache.rs`, `navigation/compiled_dcu.rs`, workspace
adapter/tests; add tests beside core `budget.rs`/`session.rs` and new regression
targets. This hardens the established APIs rather than adding another cache.

**Interfaces:** Extend existing context/cache fingerprints, retained-byte
visitors, observation-to-probe conversion, and session-state snapshots.
`CompileTimeState::fingerprint`/payload traversal remain the core inputs.

- [ ] **Step 1: Add edit/target/packing invalidation tests.** With a real
  fixture Workspace, warm/import a provider `type Payload=Byte`, verify an
  importer `{$IF SizeOf(Payload)=1}` binds `Small`, then change that provider
  overlay to `Int64` and verify the same request binds `Large` without stale
  results. Repeat with an include-only bound edit, a packing directive edit,
  target change Win32 to Win64, and a same-spelling alias whose structural
  definition changes. Use `change_document`/`file_event` and current selection
  APIs, not manually clearing caches in the test.
- [ ] **Step 2: Add explicit core bounds/cancellation tests.**

```rust
#[test]
fn budget_rejects_before_retaining_payload() {
    let limits = CompileTimeLimits { max_retained_bytes: 0, ..CompileTimeLimits::default() };
    let mut budget = CompileTimeBudget::new(limits, &NoCancellation);
    let before = budget.snapshot();
    assert!(budget.charge(1, 1, 1).is_err());
    assert_eq!(budget.snapshot().retained_bytes, before.retained_bytes);
}
```

  Add many const parameters, 10,000 simple declarations, deeply nested fields /
  aliases, provider/branch-state limits, cancellation during query/include, and
  large string/name admission cases. Bound actual operation/byte counters, not
  wall-clock timing. No query counter resets per child; no partial typed result
  is published after cancellation. Tests in Task 2 may already pin the basic
  budget case; this task adds whole-session/recovery assertions.
- [ ] **Step 3: Run RED.**

```bash
cargo test -p pascal-lsp --test compile_time_layout
cargo test -p pascal-core --test compile_time_conditionals --test compile_time_includes --test compile_time_providers
```

- [ ] **Step 4: Complete freshness/probe/cost integration.** Hash all layout
  defaults in project/document contexts, include statement occurrence/scope
  fingerprints in stored facts, and replay every provider/include observation
  on hits. Account retained fact graphs/pending fragments through existing
  `visit_recovery_payload` and cache byte visitors. Reconciliation failures
  discard reconstructible facts consistently with existing policy; do not retain
  precise header/type proofs after their dependencies became stale.

```rust
conditional_context.fingerprint().hash(&mut hasher);
compile_time_state.fingerprint().hash(&mut hasher);
```

  This extends existing hashes at the owning cache/index scope; it does not
  replace revision/stat/provider-probe validation with a hash-only check.
- [ ] **Step 5: Run GREEN and complete workspace tests.**

```bash
cargo test --workspace --no-fail-fast
```

- [ ] **Step 6: Review/commit the hardened freshness/accounting changes.**

```bash
git add -- crates/pascal-lsp/src/project_cache.rs crates/pascal-lsp/src/navigation/compiled_dcu.rs crates/pascal-lsp/src/workspace/compile_time.rs crates/pascal-lsp/tests/compile_time_layout.rs crates/pascal-core/src/compile_time/budget.rs crates/pascal-core/src/compile_time/session.rs crates/pascal-core/tests/compile_time_conditionals.rs crates/pascal-core/tests/compile_time_includes.rs crates/pascal-core/tests/compile_time_providers.rs
git commit -m "fix(core): revalidate and account compile-time layout dependencies"
```

### Task 14: Documentation, full verification, and actual ChainDriveAPI probe

**Files:** Update `crates/pascal-lsp/README.md`,
`crates/pascal-project/README.md`, and `docs/shared-resolver-architecture.md`;
update TASK-140 only through Backlog MCP. Write diagnostic logs/probe files
under `/tmp/opencode`, not real source/config directories.

**Interfaces:** No new product APIs. The verified artifact is the configured
`target/release/pascal-lsp`, with real installation 37.0 and unchanged
ChainDriveAPI project selection/configuration.

- [ ] **Step 1: Document supported behavior and deliberate limits.** State the
  full operand categories, initial verified targets/settings, source-order
  include values, identity/ambiguity rules, budget/overflow/cancellation behavior,
  header-only fallback boundary, and unproven generic/DCU/function/property
  operands. Correct outdated resolver architecture text only for behavior
  actually changed by TASK-140. Do not claim TASK-47 or full compiler parity.
- [ ] **Step 2: Run the complete verification commands, recording fresh logs.**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
git diff --check
```

  Each command must exit 0; read the full test summary and record pass/fail/ignored
  counts rather than reusing TASK-139's counts. If any fails, investigate with
  systematic-debugging and rerun the failed checks after correction.
- [ ] **Step 3: Perform the agreed whole-change review.** Use
  `jev.review_select` with TASK-140 requirements, the implementation scope (base
  commit recorded before execution), mandatory correctness coverage, and the
  user's actual delegation choice. Review ABI sources, scopes/shadowing,
  include/branch continuation, read authorization, cache/recovery costs, source
  coordinates, and both positive/negative tests. The tool selects dimensions,
  not a verdict. Follow the chosen execution method's independent review gate;
  do not silently claim an independent reviewer when only self-review occurred.
- [ ] **Step 4: Build the configured binary after review corrections.**

```bash
cargo build --release -p pascal-lsp
```

- [ ] **Step 5: Reprobe all 13 actual imports with unchanged real configuration.**
  Reuse `/tmp/opencode/probe-centrale-lsp.py` if present and inspect its contents
  first. Run with no `--fixture`/`--explicit` flags and no diagnostic configuration
  override. If the temporary probe is absent, create a fresh stdio probe with
  the following required request sequence (protocol skeleton, not product code):

```python
home = pathlib.Path.home()
root = home / 'gits/multidev'
source = root / 'Projects/ChainDriveAPI/src/Database/CDAPI.Database.Centrale.pas'
binary = home / 'gits/lint4d/target/release/pascal-lsp'
opts = {'sourcePaths': ['C:/DelphiSources/' + p for p in [
    'rtl/sys', 'rtl/common', 'rtl/win', 'vcl', 'db', 'db/ado', 'db/dsnap',
    'db/dbx', 'db/vclctrls', 'IBX', 'internet', 'soap', 'xml']]}
send('initialize', {'processId': None, 'rootUri': root.as_uri(),
    'workspaceFolders': [{'uri': root.as_uri(), 'name': 'multidev'}],
    'capabilities': {}, 'initializationOptions': opts})
send('initialized', {}, request=False)
text = source.read_text()
send('textDocument/didOpen', {'textDocument': {'uri': source.as_uri(),
    'languageId': 'pascal', 'version': 1, 'text': text}}, request=False)
send('pascal/projectContext', {'textDocument': {'uri': source.as_uri()}})
for line, content in enumerate(text.splitlines()[:21]):
    if content.startswith('  ') and content.strip():
        response = send('textDocument/definition', {
            'textDocument': {'uri': source.as_uri()},
            'position': {'line': line, 'character': 3}})
        assert response.get('result'), content
send('shutdown', None)
send('exit', None, request=False)
```

  The complete transport helper is specified in the appendix below, so this
  stage does not depend on a temporary script surviving. Check the context
  response selects the real ChainDriveAPI.dproj and installation 37.0; assert
  exactly 13 responses and validate each physical header URI/range, including
  `FireDAC.Phys.IB`, `System.SysUtils`, and `mormot.core.base`. Inspect stderr for
  remaining conditional limitations. Existing obsolete-reference warnings do
  not authorize editing the real project. A 13/13 header-only result does not
  excuse failed `SizeOf`/include-value regressions or unresolved claimed support.
- [ ] **Step 6: Record evidence and final scope.** Use Backlog task finalization
  instructions before checking acceptance criteria or marking Done. Record
  command outputs/test totals, review results, supported ABI references, actual
  probe paths/results, and remaining limits. Tell the user to restart the Pascal
  LSP/Neovim to load the rebuilt release binary; do not restart their editor.
- [ ] **Step 7: Commit documentation only after fresh verification.**

```bash
git add -- crates/pascal-lsp/README.md crates/pascal-project/README.md
git add -f -- docs/shared-resolver-architecture.md
git commit -m "docs: describe verified compile-time layouts and unit navigation"
```

## Appendix: self-contained stdio probe transport

Use this transport before the Task 14 request sequence if the earlier probe is
absent. Save the resulting complete script to `/tmp/opencode/task140-reprobe.py`.
Wrap the request sequence in `try/finally` with `close_probe()` in `finally`.
It is a diagnostic against the user's existing installation, not a test-suite
dependency or a reason to copy Delphi source into the repository.

```python
import json
import pathlib
import queue
import subprocess
import threading

messages = queue.Queue()
counter = 0
home = pathlib.Path.home()
stderr = open('/tmp/opencode/task140-reprobe.stderr', 'w')
proc = subprocess.Popen([str(home / 'gits/lint4d/target/release/pascal-lsp'), '--stdio'],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr)

def reader():
    while True:
        headers = {}
        while True:
            line = proc.stdout.readline()
            if not line:
                messages.put({'eof': proc.poll()})
                return
            if line == b'\r\n':
                break
            key, value = line.decode().split(':', 1)
            headers[key.lower()] = value.strip()
        size = int(headers['content-length'])
        payload = bytearray()
        while len(payload) < size:
            chunk = proc.stdout.read(size - len(payload))
            if not chunk:
                messages.put({'eof': proc.poll()})
                return
            payload.extend(chunk)
        messages.put(json.loads(payload))

threading.Thread(target=reader, daemon=True).start()

def write_message(message):
    encoded = json.dumps(message).encode()
    proc.stdin.write(f'Content-Length: {len(encoded)}\r\n\r\n'.encode() + encoded)
    proc.stdin.flush()

def send(method, params, request=True):
    global counter
    message = {'jsonrpc': '2.0', 'method': method, 'params': params}
    if request:
        counter += 1
        message['id'] = counter
    write_message(message)
    if not request:
        return None
    while True:
        response = messages.get(timeout=180)
        if 'eof' in response:
            raise RuntimeError(response)
        if response.get('id') == counter and 'method' not in response:
            print(json.dumps({'method': method, 'response': response}), flush=True)
            if 'error' in response:
                raise RuntimeError(response['error'])
            return response
        if 'method' in response and 'id' in response:
            write_message({'jsonrpc': '2.0', 'id': response['id'], 'result': None})
        elif response.get('method') in ('window/logMessage', 'window/showMessage'):
            print(json.dumps(response), flush=True)

def close_probe():
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.terminate()
        proc.wait(timeout=10)
    finally:
        stderr.close()
```

## Spec coverage and execution handoff

| Spec section | Owning tasks |
| --- | --- |
| Intent, constraints, confirmed three failures (§1–2) | 1, 7–8, 10, 14 |
| Shared ownership and fact identities (§3–4) | 1–3, 7, 9 |
| Target/compiler/settings (§5) | 1–2, 5, 7 |
| Aliases/imports/records/arrays/variables (§6) | 3–6, 9 |
| Source-order includes and branch merges (§7) | 7–8 |
| Consumer/header/source-map integration (§8) | 8–12 |
| Bounds/errors/freshness (§9) | 2–3, 7–9, 13 |
| Numeric, branch, safety and actual-file verification (§10) | 1–14 |
| Delivery boundary and non-goals (§11) | Global Constraints, 11, 14 |

Before execution, record the implementation base commit and working-tree state.
Do not include preexisting Backlog files in product commits without inspecting
them. Documentation files under `docs/` are ignored by default; add only the
named plan/spec deliberately, never change `.gitignore` or force-add the entire
directory.

## Planning self-review

- [x] Mapped all 11 spec sections and the full operand categories to owner tasks.
- [x] Scanned placeholders and checked task/fence/checkbox structure.
- [x] Checked existing `ConditionalFact`, `TargetPlatform`, `Value`, resolution
  outcome, Workspace, and CFG adapter interfaces. Corrected lookup observation
  carriers, declaration-constructor admission, access-fixture name collisions,
  shared recursive header budgets, and source-specific snapshot contexts.
- [x] Assigned all five Review Focus classes to named regression steps.
- [x] Reviewed authorization, scope/branch identity, checked ABI arithmetic,
  recovery/cache costs, source mapping, and both adapter safety boundaries inline.
- [x] Confirmed the plan does not begin product implementation or delegate work.

`jev.review_select` covered both planning documents but returned the
`request-too-large` fallback. The ordinary inline self-review was performed;
no independent reviewer or subagent ran. These checks validate the plan, not
new product behavior: execution tests and the real 13-import probe remain pending.

The tasks share declaration identities, continuation state, and request budgets.
Execute them in the listed dependency order. User approval of this plan and the
execution choice is the next gate; this document does not start implementation.
