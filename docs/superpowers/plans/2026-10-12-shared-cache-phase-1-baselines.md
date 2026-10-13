# Shared Cache Phase 1: Baselines and Instrumentation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `subagent-driven-development` or `executing-plans` to implement this plan
> task-by-task, after user plan review and execution-method selection.
> Beads is the authoritative task tracker. The numbered procedures below are
> instructions, not a second mutable progress checklist.

**Goal:** Establish reproducible analysis baselines and bounded opt-in counters
before implementing shared persistence.

**Architecture:** Put fixed-size process counters below both the resolver and
LSP, in `pascal-project`, which both already depend on. Instrument existing
operation boundaries without changing analysis behavior. A bounded reporter and
benchmark harness consume these counters; no SQLite backend is added here.

**Tech Stack:** Workspace Rust 1.99.0; `std` atomics; existing `serde`/`serde_json`;
existing LSP protocol/test-server support; Python 3 standard library for the
standalone runner and its tests; optional GNU `time` for Linux child RSS/IO.

**Spec:** [Approved shared-cache design](../specs/2026-10-12-shared-persistent-analysis-cache-design.md).
Read it before executing this plan.

**Status:** Proposed phase plan awaiting review. Neither this file nor the
approved specification authorizes product implementation on its own.

## Global constraints

- Same OS user, same machine; local disk only. No daemon/network cache.
- Dedicated future `pascal-cache` crate; no SQLite or BLAKE3 dependency added by
  this measurement phase, and no persistence dependency in `pascal-core`.
- Persistence will be enabled by default, with feature-preserving OFF and zero
  persistent-cache IO after runtime-disable acknowledgement.
- Existing retained-cache default: 2 GiB `maxCacheBytes`.
- Future SQLite page-cache allowance: 16 MiB aggregate per process, charged to
  retained-cache admission; transient cache buffers: 16 MiB aggregate separately.
- Future managed store budget: 8 GiB, including DB/WAL/temp/old namespaces;
  WAL pressure: 256 MiB; interactive contention allowance: 25 ms.
- Future compact `maxWorkspaceUnits`: 100,000 per project/configuration;
  existing materialization `maxFiles`: 10,000; per file: 2 MiB;
  request source bytes: 256 MiB. This phase does not change any of those limits.
- SQLite floor for the later backend: 3.51.3 with the WAL-reset fix. Preliminary
  dependency selection is `rusqlite = "=0.40.2"` with `default-features = false`
  and `bundled`, `hooks`, `limits`; its tagged bundled header reports 3.53.2.
  Reverify the resolved lockfile/runtime in the backend plan. BLAKE3 1.8.7 and
  `getrandom` 0.4.3 are preliminary later-phase selections, not installed here.
- No unsaved-overlay publication, native-tree/source-text persistence, inferred
  completeness, weakened authorization or edit-proof changes.
- Fixed benchmark inputs; at least 30 measured warm/disabled samples per scenario,
  five paired cold builds, and honest unavailable measurements.
- No subagent dispatch unless the user selects it or an applicable workflow
  explicitly requires it. This plan's preparation used no subagents.
- No git operations/commits/pushes without explicit authority. Record progress in
  Beads; any later authorized commit must name `l4d-zf1b.1`.
- Regression tests first; touched-package tests, `cargo fmt --check`, and
  `cargo clippy --workspace --all-targets -- -D warnings` for product changes.
- Do not edit `TODOS.md`, introduce ad hoc trackers, add old sibling git dependencies,
  or restructure the navigation/resolver monoliths for unrelated reasons.

## Review focus

1. Counter overflow and concurrent workers must not wrap or produce believable
   negative/zero deltas: Task 1 tests saturation, monotonicity and joined workers.
2. Background work must not be mislabelled as work exclusively caused by a single
   request: Tasks 3 and 4 report process-window scope and non-quiescent snapshots.
3. Full/malformed stdout or stderr pipes must not deadlock the runner or become
   successful timing samples: Task 4 tests bounded concurrent draining and failures.
4. Paths with spaces/Unicode and pre-existing output files must not be mangled or
   overwritten: Task 4 tests argv handling, URI encoding and exclusive output.
5. Missing RSS/index-coverage/storage measurements must be unavailable, not zero
   or complete: Tasks 3–5 test/report capability and availability fields explicitly.

## Scope and phased delivery

The approved spec describes several separately testable deliveries. Planning all
their implementation code against today's open prerequisites would guess APIs
that `l4d-51`, `l4d-122`, `l4d-33` and `l4d-20` must still establish. Use a delivery
sequence plus a complete execution plan for each phase.

| Phase | Beads work | Independently testable delivery and next-plan input |
| --- | --- | --- |
| 1: this plan | `l4d-zf1b.1` | Reproducible current-behavior baselines, scalar counters, bounded reports and harness |
| 2: facts/identities | `l4d-zf1b.2`, `.4`; prerequisite `l4d-51` | Neutral DTOs/extraction and versioned canonical local/expanded/context identity vectors; no database required |
| 3: eligibility/freshness | `l4d-zf1b.5`, `.7`; prerequisite `l4d-122` | Session evidence/authorization/overlay isolation and accounting, testable with the memory adapter |
| 4: storage/publication | `l4d-zf1b.3`, `.6`, `.10` | Private local SQLite store, epoch-scoped atomic records, sealed historical snapshots, GC and fault tests |
| 5: workspace consumers | `l4d-zf1b.8`, `.9`, session controls from `.11`; prerequisites `l4d-33`, `l4d-20` | Both memory/SQLite catalogue adapters, bounded restart reconciliation and lazy query integration |
| 6: administration/release | Remaining `.11`, `.12` | Status/prune/reset/policy modes, full differential/multi-process matrix and measured release gates |

Each later phase gets its own written plan, exact interface declarations,
test-first steps and user review before execution. This table is a delivery map,
not permission to execute unwritten phases. Existing issues retain ownership and
dependencies; complete prerequisite work on its existing issue rather than
quietly absorbing it into a cache child.

Phase 5 must wire the default-on/OFF control before persistence is opened by the
LSP; phase 6 is not permission to postpone that safety switch. A library-only
phase 4 does not advertise a partially shipped LSP cache.

## File structure

| File | Responsibility |
| --- | --- |
| Create `crates/pascal-project/src/analysis_metrics.rs` | Fixed counter IDs, disabled fast path, snapshots/deltas and test-scoped collector |
| Modify `crates/pascal-project/src/lib.rs` | Export metrics; count existing authorized payload/stamp/hash operations |
| Modify `crates/pascal-core/src/parser.rs` | Count actual parser invocations at `parse_bytes` |
| Modify `crates/pascal-core/src/resolver.rs` | Count metadata parses at `parse_source_metadata` |
| Modify `crates/pascal-lsp/src/navigation.rs` | Count document preparation and import-binding API activity at existing boundaries |
| Modify `crates/pascal-lsp/src/project_cache.rs` | Measure existing probe calls/time without changing freshness decisions |
| Create `crates/pascal-lsp/src/analysis_metrics.rs` | Bounded versioned report and opt-in startup/shutdown integration helpers |
| Modify `crates/pascal-lsp/src/lib.rs`, `src/main.rs` | Register reporter and initialize only when profiling is explicitly enabled |
| Modify `crates/pascal-lsp/src/server.rs`, `src/test_server.rs` | Test-server-only counter snapshot method, never ordinary server capability |
| Modify `crates/pascal-lsp/Cargo.toml` | Forward `test-support` to `pascal-project/test-support`; no new crate dependency |
| Create `crates/pascal-lsp/tests/protocol/cache_baseline.rs` | Counter/feature/no-protocol-text regressions using existing `TestServer` |
| Modify `crates/pascal-lsp/tests/protocol.rs` | Include that focused test file; avoid duplicating the protocol harness |
| Create `scripts/cache_baseline.py` | Bounded stdlib LSP benchmark runner, fixture generation and JSONL output |
| Create `scripts/tests/test_cache_baseline.py` | Pure harness/fixture/report tests and fake-child protocol tests |
| Create `docs/cache-benchmarking.md` | Exact metric meanings, benchmark commands, availability and result interpretation |
| Modify `crates/pascal-lsp/README.md` | Document opt-in profiling and link the benchmark guide |

Existing symbols were checked during planning; locate them by name rather than
assuming line numbers survive intervening work. No `src/test_support.rs` exists
in the LSP: use its actual `test-support` feature and `test_server.rs` harness.

## Execution preflight: preserve an uninstrumented binary

After phase-1 execution approval, claim the issue, establish the approved isolated
workspace, and build before changing Rust code:

```sh
cargo build -p pascal-lsp --release
```

Copy that executable into a newly owned directory; do not overwrite an existing
baseline artifact or label a stale target binary as the current source:

```python
from pathlib import Path
import hashlib
import shutil
import tempfile

source = Path("target/release/pascal-lsp")
directory = Path(tempfile.mkdtemp(prefix="l4d-uninstrumented-", dir="/tmp/opencode"))
destination = directory / "pascal-lsp"
digest = hashlib.sha256()
with source.open("rb") as reader, destination.open("xb") as writer:
    while chunk := reader.read(64 * 1024):
        writer.write(chunk)
        digest.update(chunk)
shutil.copymode(source, destination)
print(destination)
print(digest.hexdigest())
```

Record the path/hash, build flags/toolchain and current source/lockfile identity
in Beads. Do not use git operations to obtain identity under the current policy.
Task 4's completed harness can later run this retained binary for timing/RSS/result
comparison. Its absent counter reporter is an explicit unavailable capability,
not a run failure or invented zero. Counter-on/off runs of the new binary isolate
instrumentation cost; later cache comparisons use the same instrumented binary.

## Task 1: Fixed-size counters and trustworthy deltas

**Beads:** `l4d-zf1b.1`.

**Files:** Create `crates/pascal-project/src/analysis_metrics.rs`; export it from
`crates/pascal-project/src/lib.rs`. Tests live in the new module.

**Interfaces to produce:**

```rust
#[repr(usize)]
#[derive(Clone, Copy, Debug)]
pub enum Metric {
    AuthorizedPayloadReadAttempts = 0,
    AuthorizedPayloadBytes = 1,
    AuthorizedPayloadReadFailures = 2,
    ProjectStampProbes = 3,
    LegacyHashBytes = 4,
    CoreParserInvocations = 5,
    LspDocumentPreparations = 6,
    ResolverMetadataParses = 7,
    ImportBindingCalls = 8,
    ImportBindingPairs = 9,
    ProjectCacheProbeCalls = 10,
    ProjectCacheProbeNanos = 11,
}
pub const METRIC_COUNT: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetricSnapshot {
    pub schema: u32,
    pub counts: [u64; METRIC_COUNT],
    pub saturated: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaError { SchemaMismatch, Saturated, Regressed }

pub struct Counters {
    enabled: std::sync::atomic::AtomicBool,
    counts: [std::sync::atomic::AtomicU64; METRIC_COUNT],
    saturated: std::sync::atomic::AtomicBool,
}
impl Counters {
    pub const fn new() -> Self;
    pub fn set_enabled(&self, enabled: bool);
    pub fn enabled(&self) -> bool;
    pub fn record(&self, metric: Metric, amount: u64);
    pub fn snapshot(&self) -> MetricSnapshot;
}
impl MetricSnapshot {
    pub fn value(&self, metric: Metric) -> u64;
    pub fn checked_delta(&self, before: &Self) -> Result<Self, DeltaError>;
}
pub static PROCESS_COUNTERS: Counters = Counters::new();
pub fn record(metric: Metric, amount: u64);
pub fn enabled() -> bool;

// cfg(any(test, feature = "test-support")); current-thread override only.
// A guard restores the previous override on drop and never resets global counts.
pub struct TestScope {
    collector: std::sync::Arc<Counters>,
    previous: Option<std::sync::Arc<Counters>>,
    not_send: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl TestScope {
    pub fn new() -> Self;
    pub fn snapshot(&self) -> MetricSnapshot;
}
```

These are declarations of the new API, not existing functions; method signatures
are shown without bodies. Implement `Default` using `new()` for both collectors
and guards. Schema v1 fixes these IDs; append/change metrics only with a reporter
schema decision. Snapshots are process aggregates, not atomic filesystem/index
snapshots and not per-request attribution.

### Step 1: Write counter tests before exporting the implementation

Use these tests in the new module, importing the planned names from `super`:

```rust
#[test]
fn disabled_counters_do_not_record() {
    let c = Counters::new();
    c.record(Metric::CoreParserInvocations, 1);
    assert_eq!(c.snapshot().value(Metric::CoreParserInvocations), 0);
}

#[test]
fn saturation_invalidates_delta() {
    let c = Counters::new();
    c.set_enabled(true);
    let before = c.snapshot();
    c.record(Metric::LegacyHashBytes, u64::MAX);
    c.record(Metric::LegacyHashBytes, 1);
    assert_eq!(c.snapshot().checked_delta(&before), Err(DeltaError::Saturated));
}

#[test]
fn joined_workers_have_exact_aggregate() {
    let c = std::sync::Arc::new(Counters::new());
    c.set_enabled(true);
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let c = std::sync::Arc::clone(&c);
            scope.spawn(move || {
                for _ in 0..100 { c.record(Metric::CoreParserInvocations, 1); }
            });
        }
    });
    assert_eq!(c.snapshot().value(Metric::CoreParserInvocations), 200);
}
```

Also add `delta_rejects_schema_change`, `delta_rejects_regression`,
`disable_reenable_never_resets_counts`, and `nested_test_scope_restores_parent`.
Construct changed `MetricSnapshot` values directly to pin the first two cases;
for nested scopes call the module-level `record` and assert each scope's own
delta. No sleeps or process-global resets.

### Step 2: Run the red tests

Run:

```sh
cargo test -p pascal-project analysis_metrics -- --test-threads=1
```

Expected first run: missing planned module/API or failing count assertions.
Record the exact failure on the issue; a command/setup failure is not a regression.

### Step 3: Implement saturating recording and checked subtraction

Use relaxed atomics for independent statistics. The hot method follows:

```rust
pub fn record(&self, metric: Metric, amount: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if !self.enabled.load(Relaxed) { return; }
    let previous = self.counts[metric as usize]
        .fetch_update(Relaxed, Relaxed, |n| Some(n.saturating_add(amount)))
        .expect("the update closure always returns Some");
    if previous.checked_add(amount).is_none() {
        self.saturated.store(true, Relaxed);
    }
}
```

Implement the other scalar methods with:

```rust
pub const fn new() -> Self {
    use std::sync::atomic::{AtomicBool, AtomicU64};
    Self {
        enabled: AtomicBool::new(false),
        counts: [const { AtomicU64::new(0) }; METRIC_COUNT],
        saturated: AtomicBool::new(false),
    }
}
pub fn set_enabled(&self, enabled: bool) {
    self.enabled.store(enabled, std::sync::atomic::Ordering::Relaxed);
}
pub fn enabled(&self) -> bool {
    self.enabled.load(std::sync::atomic::Ordering::Relaxed)
}
pub fn snapshot(&self) -> MetricSnapshot {
    use std::sync::atomic::Ordering::Relaxed;
    MetricSnapshot {
        schema: 1,
        counts: std::array::from_fn(|i| self.counts[i].load(Relaxed)),
        saturated: self.saturated.load(Relaxed),
    }
}
```

`checked_delta` rejects schema mismatch/saturation first, then uses checked
subtraction for every field. `snapshot` loads twelve scalars; it must not imply
cross-field atomic consistency while workers are active. The module-level `record`
selects a test-scoped override only in test/support builds, otherwise the process
collector. `TestScope` swaps a thread-local `Option<Arc<Counters>>`, enables its
new collector, restores the previous one on drop and cannot move between threads.
The module-level `enabled()` checks that same override before the process collector,
so disabled production avoids clocks while scoped tests can exercise timing hooks.
It never formats text, performs IO, allocates or invokes an analysis callback.

### Step 4: Verify green and existing project tests

Run the focused command again, then `cargo test -p pascal-project`.
Expected: counter assertions pass and existing results remain unchanged. Record
pre-existing failures separately. Confirm the module imports no SQLite/LSP types.

### Step 5: Review and record the deliverable

Review saturation, disabled fast path, thread-scope restoration and fixed storage.
Append commands/results to `l4d-zf1b.1`; do not close the issue or commit merely
because this first task passes.

## Task 2: Instrument actual operation boundaries without double counting

**Files:** Modify `pascal-project/src/lib.rs`, `pascal-core/src/parser.rs`,
`pascal-core/src/resolver.rs`, `pascal-lsp/src/navigation.rs`,
`pascal-lsp/src/project_cache.rs`; tests in their
existing/new focused test modules. All paths are under `crates/`.

**Consumes:** Task 1 `record(Metric, u64)`, `enabled()`, `TestScope::new/snapshot` and
`MetricSnapshot::value/checked_delta`.

**Produces:** The metric meanings below, with no changed analysis APIs/results:

| Counter | Increment point and exact meaning |
| --- | --- |
| Authorized payload attempts/failures | `ReadPolicy::read_payload_bytes_after_authorization`; one admitted API attempt and one failure if it returns an error |
| Authorized payload bytes | Actual bytes read, including a partial/over-limit prefix; not physical disk-sector IO and not only Pascal files |
| Project stamp probes | Entry to `path_stamp_result`; function invocations, not every OS metadata syscall |
| Legacy hash bytes | Input length at `content_hash_bytes`; original/current hash implementation, not future BLAKE3 work |
| Core parser invocations | Immediately before actual `Parser::parse` in production `parse_bytes`; includes real rewrite/retry invocations |
| Resolver metadata parses | Entry to `parse_source_metadata`; separate from core parser invocations, never added to them as if independent parser calls |
| LSP document preparations | Existing `Document::parse_with_session` path after the reusable-document early return, beside its test preparation counter; can precede cancellation, so not an actual parser-call count |
| Import-binding calls/pairs | `NavigationIndex::bind_imports` entry and its existing binding loop; API activity, not a claim of semantic-reference documents bound |
| Project-cache probe calls/nanoseconds | Existing `project_cache::probes_hold`; elapsed validation time only when profiling is enabled, not future startup reconciliation or a proof that today's stamp shortcuts are correct |

### Step 1: Add operation-level regression tests

In project tests, pin successful payload recording with real current types:

```rust
#[test]
fn metrics_payload_counts_authorized_bytes_once() {
    use crate::analysis_metrics::{Metric, TestScope};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("A.pas");
    std::fs::write(&path, b"unit A;").unwrap();
    let policy = ReadPolicy::new(
        &[root.path().to_path_buf()], &[], &[],
        &delphi_overrides::EffectiveOverrides::default(),
    );
    let entry = ProjectPathEntry {
        path, provenance: ProjectPathProvenance::Configured,
    };
    let scope = TestScope::new();
    assert_eq!(policy.read_payload_bytes(&entry, 100).unwrap(), b"unit A;");
    let s = scope.snapshot();
    assert_eq!(s.value(Metric::AuthorizedPayloadReadAttempts), 1);
    assert_eq!(s.value(Metric::AuthorizedPayloadBytes), 7);
    assert_eq!(s.value(Metric::AuthorizedPayloadReadFailures), 0);
}
```

Add `metrics_overlimit_preserves_read_error_and_prefix_count` using the same
payload and a limit of 3: error remains the existing safety-limit error, bytes
count is 4, attempts/failures are 1. Add
`metrics_unauthorized_payload_does_not_claim_an_authorized_attempt` with a path
outside roots and assert counts stay zero.

In core parser tests:

```rust
#[test]
fn metrics_core_parse_counts_real_invocation() {
    use pascal_project::analysis_metrics::{Metric, TestScope};
    let scope = TestScope::new();
    let info = FileInfo::new(std::path::PathBuf::from("A.pas"));
    crate::parser::parse_file(&info, b"unit A; interface implementation end.").unwrap();
    assert_eq!(scope.snapshot().value(Metric::CoreParserInvocations), 1);
}
```

Add a rewrite/retry fixture test asserting the count matches the actual parser
calls, not a guessed one-call-per-document rule. In resolver tests, reuse a
metadata-cache fixture already exercising `parse_source_metadata`, capture a
scope around the first/repeated resolution, and assert metadata counts match the
existing `test_metadata_parses` behavior. In navigation tests, adapt existing
document-reuse tests: reuse increments neither preparation nor parser counts;
changed content increments preparation and real parser calls. Pin binding pairs
with two entries passed to `bind_imports`.

In project-cache tests add `metrics_probe_wrapper_preserves_outcome`: capture a
scope around an empty probe set (true) and a recorded existing-file stamp whose
owned temporary file was removed before checking (false). Assert two probe calls,
unchanged Boolean results and a non-regressing elapsed counter. Do not require
nonzero elapsed time on every clock/platform or fix the existing freshness bug.

### Step 2: Run red filters

```sh
cargo test -p pascal-project metrics_ -- --test-threads=1
cargo test -p pascal-core metrics_ -- --test-threads=1
cargo test -p pascal-lsp --features test-support metrics_ -- --test-threads=1
```

Expected: missing increments, not different semantic results. Add
`pascal-project/test-support` to the LSP feature forwarding so tests can enter
the scoped collector; do not turn test-server features on by default. New navigation
tests that import `TestScope` are gated by `feature = "test-support"`; ordinary
`cargo test -p pascal-lsp --lib` must still compile without the helper feature.

### Step 3: Add only the listed hook calls

At parser entry use:

```rust
pascal_project::analysis_metrics::record(
    pascal_project::analysis_metrics::Metric::CoreParserInvocations, 1,
);
```

At a hashing boundary use:

```rust
analysis_metrics::record(
    analysis_metrics::Metric::LegacyHashBytes,
    u64::try_from(bytes.len()).unwrap_or(u64::MAX),
);
```

The read helper records bytes immediately after `read_to_end`, before error/
over-limit returns; preserve its original error and authorization order. Wrap
its authorized attempt in a small scope/result match to record failure exactly
once. Do not count the same payload again in `workspace::read_disk_source_with_budget`
or at resolver callers. Preserve the original iterator in `bind_imports` and
increment pair activity inside its existing loop; do not collect another vector.
Leave current thread-local `trace::CacheCounters` and their semantics intact.

In `probes_hold`, retain its original `.all(...)` predicate as `let held = ...`.
Count the call, start `Instant` only if the collector is enabled, record checked
elapsed nanoseconds after the predicate, then return `held`. Avoid clocks when
profiling is off. An elapsed conversion overflow must saturate/invalidate the
snapshot, not wrap. The existing raw `fs::read` within content probes is outside
the authorized-payload counter's scope; document this exclusion rather than
double counting it or claiming authorized reads are total IO. Probe time and
GNU-time IO give distinct validation-cost observations.

### Step 4: Verify behavior stays unchanged

Rerun red filters, then:

```sh
cargo test -p pascal-project
cargo test -p pascal-core
cargo test -p pascal-lsp --lib
cargo test -p pascal-lsp --test protocol references_include_unopened_consumers
```

Expected: unchanged references/authorization/errors. Counter tests must not rely
on global resets or race with unrelated test threads.

### Step 5: Review and record hook coverage

Document the exact meanings and exclusions above. File a linked follow-up if an
additional uninstrumented production parser/reader bypass is discovered; do not
silently label these counters total physical IO or all semantic bindings.

## Task 3: Bounded versioned reporting and a test-only snapshot seam

**Files:** Create `crates/pascal-lsp/src/analysis_metrics.rs`; modify `src/lib.rs`,
`src/main.rs`, `src/server.rs`, `src/test_server.rs`, `Cargo.toml`, and
`tests/protocol.rs`; create `tests/protocol/cache_baseline.rs`.

**Consumes:** Task 1 counter snapshot and stable IDs; Task 2 operation meanings.

**Produces:**

```rust
// In pascal-lsp::analysis_metrics; only startup/shutdown helpers need public
// exposure to the separate binary crate. The formatter can stay crate-private.
pub fn initialize_profiling() -> bool;
pub fn finish_profiling(enabled: bool, orderly_shutdown: bool);
pub(crate) enum ReportMoment {
    ProcessExit { orderly_shutdown: bool },
    WindowEndpoint,
}
pub(crate) fn bounded_report(
    snapshot: &pascal_project::analysis_metrics::MetricSnapshot,
    moment: ReportMoment,
) -> Result<String, String>;
```

`PASCAL_LSP_ANALYSIS_METRICS=1` opts in. Unset/other values leave counters off and
produce no new output. Ordinary production emits at most one prefixed JSON line
on stderr after shutdown: `pascal-lsp analysis-metrics ` plus schema-v1 JSON.
Never write non-LSP text to stdout. The entire prefixed line including its newline
is at most 4,096 bytes; the formatter reserves space for that prefix/newline.

Schema fields: `schema: 1`, the twelve named counters, `saturated: bool`,
`countScope: "process_lifetime"`, `snapshotConsistency: "independent_atomics"`,
`orderlyShutdown: bool` for exit reports (`null` for live endpoints), and
an explicit unavailable-field map. Before index/store
implementation, report `indexCoverage`, `storageUsage`, `decodedPeakBytes` and
`semanticCandidateDocumentsBound` as `null` with explicit unavailability reasons.
Do not substitute existing source-byte estimates for measured heap/RSS.

Use this exact shape (the numeric zeros illustrate fields, not baseline results):

```json
{
  "schema": 1,
  "counters": {
    "authorizedPayloadReadAttempts": 0,
    "authorizedPayloadBytes": 0,
    "authorizedPayloadReadFailures": 0,
    "projectStampProbes": 0,
    "legacyHashBytes": 0,
    "coreParserInvocations": 0,
    "lspDocumentPreparations": 0,
    "resolverMetadataParses": 0,
    "importBindingCalls": 0,
    "importBindingPairs": 0,
    "projectCacheProbeCalls": 0,
    "projectCacheProbeNanos": 0
  },
  "saturated": false,
  "countScope": "process_lifetime",
  "snapshotConsistency": "independent_atomics",
  "orderlyShutdown": true,
  "indexCoverage": null,
  "storageUsage": null,
  "decodedPeakBytes": null,
  "semanticCandidateDocumentsBound": null,
  "unavailable": {
    "indexCoverage": "not_instrumented",
    "storageUsage": "persistence_not_implemented",
    "decodedPeakBytes": "not_instrumented",
    "semanticCandidateDocumentsBound": "not_instrumented"
  }
}
```

Test binary only: `pascal/testAnalysisMetrics` returns the same bounded scalar
snapshot with `countScope: "process_window_endpoint"`. It neither resets counts,
claims worker quiescence nor requests analysis. Add
`TestBarrierConfig::with_analysis_metrics(bool) -> Self`, default false; enable it
only in `test_server.rs`. Both the compile-time `test-support` flag and that
runtime test configuration are required. Ordinary `run_stdio` rejects the method,
even if the package was built with the test feature.

### Step 1: Add report and protocol tests

In the reporter module, write `metrics_report_is_bounded_and_has_no_paths`,
`metrics_report_marks_unavailable_fields_null`, and
`metrics_report_preserves_saturation`. Construct a twelve-counter snapshot directly,
format it, parse with `serde_json`, assert prefixed-line length <=4,096, and assert
every unavailable reason is present. No path/text parameter exists to leak.

In the included protocol test file, use existing `TestServer` helpers:

```rust
#[test]
fn metrics_private_method_is_not_a_normal_server_feature() {
    let temp = tempfile::tempdir().unwrap();
    let mut server = TestServer::launch();
    server.initialize(temp.path(), Value::Null);
    let id = RequestId::from("ordinary-metrics".to_string());
    server.send_request(id.clone(), "pascal/testAnalysisMetrics", json!({}));
    let response = server.response(&id);
    assert_eq!(response.error.unwrap().code, -32601);
    server.shutdown();
}
```

Add `TestServer::launch_with_analysis_metrics(environment: TempDir) -> Self`
using its existing test-server/environment launch path, and
`metrics_test_snapshot_is_monotonic_across_worker_requests`: query counters,
request known references using the existing provider/consumer fixture, query
counters again, and verify non-regression plus positive parser activity. Label
the delta as a process window, not an exclusive request count. Extend the existing
shutdown/stdout protocol regression with profiling enabled.

### Step 2: Run red tests

```sh
cargo test -p pascal-lsp --features test-support metrics_ -- --test-threads=1
cargo test -p pascal-lsp --features test-support --test protocol_barriers metrics_ -- --test-threads=1
```

Expected: report/helper assertions fail until wired. The ordinary-server rejection
test may already pass; that is a guard, not evidence that reporting is implemented.

### Step 3: Wire aggregate output without source-level logging

Build JSON with fixed metric names using `serde_json::json!`, serialize, then reject
anything exceeding the 4,096-byte line allowance after prefix/newline. Main
initializes after ordinary argument validation,
stores the `run_stdio()` result, emits the optional summary and then maps the
existing result to its original exit code. No cache directory or output file is
opened. Test-server initialization enables the collector only for its opt-in
configuration/environment; set its runtime opt-in from the same profiling setting,
not unconditionally for every barrier test. The private method is handled before
ordinary method dispatch and uses the existing bounded protocol sender.

The shutdown call shape is:

```rust
let profiling = pascal_lsp::analysis_metrics::initialize_profiling();
let result = pascal_lsp::server::run_stdio();
pascal_lsp::analysis_metrics::finish_profiling(
    profiling, matches!(&result, Ok(true)),
);
```

Do not expose source lists, enlarge normal LSP capabilities, alter the analysis
queue or replace existing tracing. Independent atomic snapshots can include
background work; tests/benchmarks must say so. A failed/crashed shutdown is not
a valid final measurement merely because some counts were emitted.

### Step 4: Verify both ordinary and test-server surfaces

Rerun the focused commands; then run the normal protocol target without the test
feature and the barrier target with it. Check stdout remains framed JSON-RPC and
normal help/version output is unchanged. Measure disabled fast-path overhead
later; do not assert it is free based only on the branch shape.

### Step 5: Review reporting boundaries

Record schema v1 and test-only/runtime guards on the issue. The report explicitly
does not provide full-index coverage or actual retained memory yet; those fields
cannot satisfy later release gates until the owning phases instrument them.

## Task 4: Reproducible stdlib benchmark runner and synthetic fixture

**Files:** Create `scripts/cache_baseline.py`,
`scripts/tests/test_cache_baseline.py`, and `docs/cache-benchmarking.md`.

**Consumes:** Ordinary `pascal-lsp` stdio and optional shutdown metrics; test-server
snapshot method only when a caller explicitly selects that binary.

**Produces these Python interfaces:**

```text
encode_frame(message: dict) -> bytes
read_frame(stream, max_bytes: int = 16 * 1024 * 1024) -> dict | None
write_fixture(root: pathlib.Path, units: int = 500, importers: int = 5) -> dict
fixture_digest(root: pathlib.Path) -> str
normalize_locations(locations: list, root: pathlib.Path) -> list
result_fingerprint(locations: list, root: pathlib.Path) -> str
unavailable(reason: str) -> dict
validate_sample(sample: dict) -> None
write_sample(stream, sample: dict) -> None
LspSession.__init__(argv: list[str], timeout_seconds: float, profiling: bool, test_server: bool = False)
LspSession.initialize(root: pathlib.Path, options: dict) -> dict
LspSession.request(method: str, params: dict) -> tuple[dict, int]
LspSession.notify(method: str, params: dict) -> None
LspSession.close() -> dict
run_scenario(args: argparse.Namespace, scenario: str) -> list[dict]
main(argv: list[str] | None = None) -> int
```

These are interface signatures, not placeholder function bodies. Import dependencies are
only `argparse`, `hashlib`, `json`, `os`, `pathlib`, `queue`, `subprocess`,
`signal`, `tempfile`, `threading`, `time`, `typing` and standard error/IO modules.

The runner accepts `--binary`, `--output`, `--scenario`, `--samples`, `--fixture-units`,
`--fixture-importers`, `--timeout-seconds`, `--input-manifest`, `--append`,
`--profiling on|off` (default on for this explicit profiling tool), and
`--test-server` (default false). Ordinary LSP profiling remains off by default.
Private snapshot requests require both profiling-on and explicit test-server mode;
never infer support solely from a binary filename. Default scenario
suite: cold request, warmed requests in one process, restart, source/include/
metadata edit, A/B/A configuration switch, two simultaneous sessions and RAM-budget
pressure. No user source edits: mutation scenarios run on an owned fixture copy.

The input manifest is a checked JSON object with `schema: 1`, root, target URI,
line/UTF-16 character, initialization options, fixture/query identity and a label.
User-selected production inputs are read-only; only cold/warm/restart/two-session
scenarios run there. Input/output files are bounded and opened explicitly, output
with exclusive creation unless the caller explicitly selects append.

### Step 1: Write runner unit tests before the runner

Use `unittest` and load the script via `importlib.util` so no package installation
is needed. Test frame bytes directly:

```python
def test_frame_length_uses_utf8_bytes(self):
    import io
    message = {"jsonrpc": "2.0", "id": "é", "result": "界"}
    framed = baseline.encode_frame(message)
    header, body = framed.split(b"\r\n\r\n", 1)
    self.assertEqual(int(header.split(b":", 1)[1]), len(body))
    self.assertEqual(baseline.read_frame(io.BytesIO(framed)), message)
```

Also write `test_oversized_frame_is_rejected`,
`test_missing_rss_is_null_not_zero`, `test_output_existing_file_is_not_overwritten`,
`test_fixture_digest_ignores_temporary_parent`, and
`test_failed_response_is_not_a_successful_timing`.

The fixture tests create a directory whose name includes spaces and `é`, then
check exactly 500 `.pas` units, five real importing consumers, stable source bytes
and a query on `SharedValue`. Reject importers >= units-1 and unreasonable unit
counts before generating files. Files live only below a newly owned directory.

### Step 2: Run the red Python tests

```sh
python3 -m unittest discover -s scripts/tests -p 'test_cache_baseline.py' -v
```

Expected: missing script/functions or failing frame/report/fixture assertions.
Do not weaken expectations to make malformed responses into zero-cost samples.

### Step 3: Implement fixture, framing and bounded child handling

Generate `Provider.pas` with:

```pascal
unit Provider;
interface
const SharedValue = 1;
implementation
end.
```

Generate five consumer units whose `Run` body contains both
`Log(SharedValue);` and `Log(Provider.SharedValue);`, with `uses Provider;`.
Generate remaining units with their own shadowing `SharedValue = 2` and no provider
import. Query the provider declaration with `includeDeclaration: false`; expected
complete fixture locations are two per importer, not every spelling match.
Add a minimal main `.dpr` using only Provider, and A/B `.dproj` fixtures with
Debug/Release selections. Do not put all 500 units in a single resolver dependency
closure and accidentally benchmark the existing 256-dependency cap.

The main source is `program Baseline; uses Provider; begin end.`. Use this A
project XML, substituting the B define for the B project and retaining the same
main source:

```xml
<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <PropertyGroup>
    <MainSource>Root.dpr</MainSource>
    <Config Condition="'$(Config)' == ''">Debug</Config>
    <Platform Condition="'$(Platform)' == ''">Win32</Platform>
    <DCC_UnitSearchPath>.</DCC_UnitSearchPath>
    <DCC_Define>BASELINE_A</DCC_Define>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)' == 'Debug'">
    <DCC_Define>$(DCC_Define);BASELINE_DEBUG</DCC_Define>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)' == 'Release'">
    <DCC_Define>$(DCC_Define);BASELINE_RELEASE</DCC_Define>
  </PropertyGroup>
</Project>
```

Initialize with an explicit A project selector, Debug/Win32 and the fixture source
root. Switch projects with `pascal/selectProject` using `textDocument.uri` and
`projectUri`; switch Debug/Release with the existing configuration notification.
Record requested selections and available server acknowledgements separately;
do not invent effective-context evidence when the current API does not expose it.

For the include-edit case, use a fixture variant with `Baseline.inc` declaring
an independent `IncludeValue` and an include directive before the root's unchanged
`SharedValue` declaration. Find the query position from the generated actual
source, rather than hardcoding line numbers after inserting directives. Source
edit adds/removes one known consumer use; metadata/config edits use existing LSP
configuration/project-selection operations. Compute expected locations per
scenario; an error/incorrect location set is an invalid timing sample.

Encode JSON with byte-counted framing:

```python
def encode_frame(message):
    body = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    return f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body
```

Read headers with a separate 8 KiB ceiling; validate a single non-negative content
length <=16 MiB, read exactly that many bytes, and reject malformed/early EOF JSON.
Use bounded stdout/stderr reader threads and a bounded message queue; the request
loop drains notifications and responds only to supported progress/configuration
requests. Reject unexpected apply-edit requests rather than approving changes.
Use argv lists and `shell=False`, not quoted command strings. Time only the target
request/response window with `perf_counter_ns`, outside fixture construction and
optional counter snapshots. After timeout, stop/terminate the owned child with a
bounded grace period, drain/reap it, and mark the run invalid.

With a GNU-time wrapper on Unix, create a new owned session/process group using
`start_new_session=True`, and terminate that group on timeout so the actual LSP
child cannot outlive its wrapper. Never signal a reused or unrelated group.
Without a wrapper, retain/reap the exact owned child handle. Pin wrapper cleanup
in `test_child_timeout_reaps_process` as well as the direct-child case.

Retain at most 64 stderr lines of 512 bytes plus the checked 4,096-byte metric
summary; consume excess rather than blocking the pipe. Normal response arrays
do not prove complete coverage: report coverage as unavailable unless an actual
coverage-capable owning-phase API supplies it. For optional GNU `time`, run a
separate `-o` report per child with `%M`, `%I`, `%O`, `%U`, `%S`; never use cumulative
`RUSAGE_CHILDREN` high-water RSS as a per-run measurement.

Hash sorted relative fixture names and bytes with tooling SHA-256; it is not a
durable cache artifact key. Normalize only the owned fixture root in returned URIs
before fingerprinting sorted locations. Preserve ranges/duplicates and keep
unknown external paths distinct. A standard production binary provides aggregate
lifetime counters at shutdown; a selected test binary can provide endpoints.
Never reset process counts or describe a window as exclusive request work.

### Step 4: Verify the harness with a fake child, then a real fixture

Add fake-child tests:

- `test_large_stderr_is_drained_without_deadlock`.
- `test_nonprotocol_stdout_invalidates_run`.
- `test_child_timeout_reaps_process`.
- `test_unexpected_apply_edit_is_rejected`.
- `test_unicode_root_survives_argv_and_uri_round_trip`.
- `test_process_windows_are_not_request_exclusive`.

The fake child is created by the test inside its temporary directory, responds
to initialize/shutdown, and emits deliberate malformed/oversized/slow frames for
the named case. Tests use bounded queue/barrier events, not sleeps to assert races.

Run Python tests again, build real binaries only after execution approval, then:

```sh
cargo build -p pascal-lsp --release
python3 scripts/cache_baseline.py --binary target/release/pascal-lsp --scenario warm --samples 30 --fixture-units 500 --fixture-importers 5 --output /tmp/opencode/l4d-cache-phase1-warm.jsonl
```

Output must validate as schema-v1 samples, contain correct result fingerprints/
counts, and label unavailable coverage/storage/heap metrics. A pre-existing
output makes this command fail safely; choose a new explicit filename rather
than deleting prior results. Do not call request warm-up full-index reconciliation.

### Step 5: Review runner/report reproducibility

Confirm fixed fixture/query identity, bounded pipes, process lifecycle, safe
output creation, consistent counter scopes and unavailable fields. Document
GNU-time availability, warm-up versus samples, and exactly what a failed sample
means. No unrelated root traversal, source writes or cache purge is performed.

## Task 5: Capture baselines, bind acceptance targets and hand off phase 2

**Files:** Complete `docs/cache-benchmarking.md`; adjust the focused protocol tests
and runner tests as needed; link the guide from `crates/pascal-lsp/README.md`.

**Consumes:** Tasks 1–4 schemas, counters, fixture, command and result fingerprints.

**Produces:** Beads-recorded fixed baseline manifest, validated samples and an
explicit status for every measurement. No persistent feature or performance win
is claimed by completing instrumentation.

### Step 1: Pin sample validation rules with tests

Add `test_saturated_snapshot_cannot_satisfy_parse_gate`,
`test_unknown_coverage_cannot_satisfy_complete_query_gate`,
`test_unimplemented_storage_is_unavailable`, and
`test_sample_records_fixture_binary_and_counter_scope`.

Use a versioned sample with fields `schema`, `scenario`, `sampleIndex`, fixture
digest, binary/profile/test-feature identity, input selection, request elapsed
nanoseconds, result count/fingerprint, optional counter endpoints/lifetime totals,
availability/coverage, optional child RSS/IO, and outcome (`ok`/`invalid`).
`validate_sample` rejects negative time, schema mismatch, invalid snapshots,
unlabelled availability and success records containing a protocol error.
Timing-only valid samples can have unavailable counters/coverage; they cannot
pass an acceptance gate requiring those measurements. Phase-1 samples require
`releaseGateStatus: "not_evaluated"`; the owning later phases implement gate
evaluation after full indexing, actual cache mode and comparison data exist.

### Step 2: Run red validation tests

Run the Python unittest command from Task 4. Expected: malformed/unavailable
samples fail the specific validation/gate tests, not generic file-import errors.

### Step 3: Implement validation and baseline recording

Make missing measurements explicit:

```python
def unavailable(reason):
    if not reason:
        raise ValueError("an unavailable measurement needs a reason")
    return {"value": None, "available": False, "reason": reason}
```

Record current synthetic cold/warm/restart/edit/configuration/two-session/budget
pressure results. Where an existing limit blocks the scenario, record its actual
outcome and coverage rather than raising production limits or silently using a
smaller dataset. The greater-than-10k fixture is a characterization of today's
limit, not expected complete indexing yet. Deterministic restart runs reuse source
bytes but launch a new process; they do not pretend persistence exists.

For the real repository, require a user-selected read-only input manifest, fixed
query and compiler/config selections before running it. Record its command,
machine conditions, source/fixture revision identity and settings in Beads. If
the corpus/query is unavailable, record that blocker and leave the real-corpus
release gate open on `l4d-zf1b.12`; do not manufacture paths or substitute
synthetic numbers. `l4d-zf1b.1` itself requires that unavailable production inputs
are reported, not that they exist; use its actual acceptance criteria when closing.

Run the retained uninstrumented binary and the new binary with profiling off/on
against the same fixture/scenario suite. Stream binary/source fingerprints in
bounded chunks and keep source text out of result artifacts. Preserve the same
release profile and test-feature selection across comparisons. Profiling counters
are optional measurements; their absence does not invalidate a correct ordinary
reference response or get silently replaced with zero.

The later release targets remain: >=90% fewer unchanged-restart indexing parses;
fully reconciled representative references p95 <=5s; first cold index overhead
<=20%; disabled overhead <=5%. Phase 1 cannot measure these wins before the
owning index/storage phases exist. These baselines establish the comparison;
later phases extend capability fields with real instrumentation before judging
their gates. Distinguish actual core parser invocations, document preparations
and metadata parsing; do not sum overlapping counters.

### Step 4: Run phase quality gates and validate retained results

```sh
python3 -m unittest discover -s scripts/tests -p 'test_cache_baseline.py' -v
cargo test -p pascal-project
cargo test -p pascal-core
cargo test -p pascal-lsp --lib
cargo test -p pascal-lsp --test protocol
cargo test -p pascal-lsp --features test-support --test protocol_barriers metrics_ -- --test-threads=1
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

Inspect sample validity, result agreement, monotonic counters and availability;
report exact pre-existing failures. Validate benchmark output files before
describing them as usable baselines. Preserve result artifacts in the explicit
user-approved location, and record their paths/digests in Beads, not an ad hoc tracker.

### Step 5: Close only genuinely completed phase work

Check `l4d-zf1b.1` acceptance against observed evidence. It can close after its
reproducible scenarios/counters, bounded opt-in instrumentation and recorded
fixture/machine/command results pass, provided unavailable production inputs are
explicitly reported. If any of those actual criteria remain unverified, keep the
issue open and record the blocker. Close with changed files, commands and baseline
artifact references, never unmeasured benefit claims. A missing production corpus
still blocks the real-corpus release gate in `l4d-zf1b.12`.

The next plan is phase 2 only after checking current `l4d-51` interfaces and this
baseline's source/operation coverage. It defines the exact neutral DTO/key APIs,
encoding vectors and extraction tests before code. Do not start the SQLite,
freshness or workspace phases from this delivery table.

## Approval and execution handoff

User review of this plan and execution-method selection authorize phase 1 only.
`l4d-0jh` remains the architecture/planning gate for the rest of the epic. Do not
close it and accidentally unblock all remaining work after approving one phase.
At an explicit phase-1 execution approval, record the scoped approval and adjust
only `l4d-zf1b.1`'s design-gate dependency to reflect it; other prerequisites and
implementation children remain blocked. No such dependency adjustment has been
made during plan preparation.

Before code, use the selected execution skill, inspect/claim the issue, record
its design, and establish an isolated workspace under repository policy. Do not
discard unfamiliar work or create commits/pushes without separate authority.

For this first phase, Native execution is recommended: the five tasks share a
small counter/report contract and existing harness, so one implementer can keep
them consistent without repeated context setup. A fresh independent final review
is still required by that selected workflow. The later storage/concurrency phases
can be reviewed with a different execution choice if the user prefers.

## Plan self-review coverage

- Spec intent/optional-backend contract: phase 1 measures without adding storage;
  later feature implementation belongs to phases 2–6 in the delivery table.
- All five review-focus conditions have named owning tests in Tasks 1–5.
- Every new production/harness name referenced by these tasks is declared in its
  interface block or names an existing inspected API.
- Counters distinguish actual parser calls from preparations/API attempts and
  logical payload bytes from physical IO. Missing index/store/heap evidence stays
  unavailable; no release target is checked off by instrumentation alone.
- Documentation/profiling preparation does not claim red/green or benchmark runs.
  Those commands execute only after user approval; current open prerequisites
  are not treated as implemented.
