# Cache benchmarking (phase 1 baselines)

`scripts/cache_baseline.py` is a Python-stdlib runner that drives `pascal-lsp --stdio`
over JSON-RPC and writes one schema-1 JSON sample per line. Phase 1 measures only:
no persistent cache exists yet, so nothing here reports a cache win. The last sections
record the phase-1 baselines, the acceptance targets they feed and the known gaps.

```sh
cargo build -p pascal-lsp --release
python3 scripts/cache_baseline.py --binary target/release/pascal-lsp \
  --scenario warm --samples 30 --fixture-units 500 --fixture-importers 5 \
  --output /tmp/opencode/l4d-cache-phase1-warm.jsonl
python3 -m unittest discover -s scripts/tests -p 'test_cache_baseline.py' -v
```

## Options

| Option | Meaning |
| --- | --- |
| `--binary` | `pascal-lsp` to run (argv list, no shell) |
| `--output` | Created exclusively; an existing file makes the run fail without touching it. `--append` opts in to appending |
| `--scenario` | `cold`, `warm`, `restart`, `edit`, `config_switch`, `two_sessions`, `budget_pressure` or `all` |
| `--samples` | Measured samples per scenario (1..1000, default 30) |
| `--fixture-units`, `--fixture-importers` | Synthetic fixture size (default 500 units, 5 importers) |
| `--timeout-seconds` | Per-request and shutdown bound; expiry kills the owned child and invalidates the sample |
| `--input-manifest` | Read-only production input (see below) |
| `--profiling on\|off` | Sets `PASCAL_LSP_ANALYSIS_METRICS=1` for the child (default **on** for this tool; the server itself defaults to off) |
| `--test-server` | Allows `pascal/testAnalysisMetrics` snapshots; requires `--profiling on` and a binary built with `--features test-support` (`pascal-lsp-test-server`). Support is never inferred from a file name |
| `--gnu-time PATH` | Optional GNU `time` wrapper |

## Fixture

Each scenario generates an owned fixture below a fresh temporary directory (the name
contains spaces and a non-ASCII character on purpose). Exactly `--fixture-units` `.pas`
units: `Provider.pas` (`const SharedValue = 1;`), `--fixture-importers` consumers whose
`Run` contains `Log(SharedValue);` and `Log(Provider.SharedValue);`, and filler units
that shadow `SharedValue = 2` without importing `Provider`. `Root.dpr` uses only
`Provider`; `A.dproj`/`B.dproj` differ in `DCC_Define` (Debug/Release selections).
The query is the `Provider` declaration with `includeDeclaration: false`; the expected
result is **two locations per importer**, computed from the generated text. The fixture
digest hashes sorted relative names and bytes (SHA-256, tooling only, not a cache key)
and is independent of the temporary parent. Importers never form one 500-unit resolver
closure, so the 256-dependency cap is not what is benchmarked.

## Scenarios

- `cold`: new fixture copy and new process per sample; timed first references request.
- `warm`: one process; one untimed warm-up request (`warmupRequests` in `extra`), then
  `--samples` timed requests. The warm-up is a request, **not** a full-index
  reconciliation.
- `restart`: per sample, process 1 answers a request and exits; process 2 starts on the
  same bytes and its first request is timed. No persistence is implied.
- `edit` (`source`, `include`, `metadata` variants): warm request, then an on-disk edit to
  the owned copy announced with `workspace/didChangeWatchedFiles`, then the timed request.
  The source edit adds one use (expected count +1); include and metadata edits keep the
  expected set. Files are restored after each sample.
- `config_switch`: one process, A/B/A project switches via `pascal/selectProject` and
  Debug/Release/Debug via `workspace/didChangeConfiguration`; each timed request follows one
  switch (`variant`). The requested selection and the server's `selectedProjectUri`
  acknowledgement are recorded separately; effective context is *not* exposed by the
  current API and is not invented.
- `two_sessions`: two simultaneous processes on the same fixture, fresh pair per sample,
  both requests released by a barrier (`session0`/`session1` variants).
- `budget_pressure`: `maxCacheBytes` lowered to 1 MiB at initialization; the effective
  budget is not observable, so only the requested value is recorded.

Production inputs run read-only and only in `cold`, `warm`, `restart` and `two_sessions`.
A manifest is a JSON object (<=64 KiB) with `schema: 1`, `label`, `root`, `targetUri`,
`line`, UTF-16 `character`, `initializationOptions`, `fixture` and `query` identity
objects. Without a fixed expected result the first sample defines the reference and later
samples must agree.

## Sample

Fields: `schema`, `scenario`, `variant`, `sampleIndex`, `fixture` identity, `binary`
(path, SHA-256, size, `profiling`, `testServer`), `input`, `outcome`, `reason`,
`requestNanos`, `result` (count/fingerprint and the expected pair), `protocolError`,
`counters`, `coverage`, `storage`, `heapPeak`, `resources`, optional `extra`, and
`releaseGateStatus: "not_evaluated"`.

- **Failed sample**: `outcome: "invalid"`, `requestNanos: null`, with a `reason`. Causes:
  protocol error response, a location set differing from the expected fingerprint/count,
  non-protocol stdout, an over-limit or malformed frame, timeout (the child, and its whole
  process group when wrapped, is terminated and reaped), an unexpected `workspace/applyEdit`
  (answered `applied: false`), or a non-orderly shutdown. An invalid sample is never a timing.
- **Unavailable measurements** are `{"value": null, "available": false, "reason": ...}`;
  they are never zero. Coverage, storage, heap peak: not instrumented / persistence not
  implemented. Response arrays do not prove complete coverage.
- **Counters**: with an ordinary binary and profiling on, the process-lifetime totals come
  from the single stderr line printed at orderly shutdown and are attached to the *last*
  sample of a shared process (`warm`, `budget_pressure`) or to the sample of a process that
  exits with it. They cover the whole process (startup, warm-up, all requests), not one
  request. A binary without the reporter (for example the retained uninstrumented one)
  yields `metrics_not_reported_by_binary_or_run_not_orderly`, not a failure. With
  `--test-server`, `counters.value.window` holds endpoint snapshots taken outside the timed
  window and their difference with `scope: "process_window_endpoint"` and
  `requestExclusive: false`; counters are never reset and a window is never exclusive
  request work. Do not add overlapping counters (parser invocations, document preparations,
  metadata parses).
- **Resources**: GNU `time` (`--gnu-time /usr/bin/time`) writes a separate `-o` report per
  child (`%M %I %O %U %S`); `resources.value` holds max RSS (KiB), block I/O and CPU for the
  whole child lifetime. Without the wrapper (it is absent on some systems) the field is
  unavailable (`gnu_time_not_selected`). Cumulative `RUSAGE_CHILDREN` is never used.

At most 64 stderr lines of 512 bytes are retained (the rest is drained), plus the checked
4,096-byte metrics line; stdout frames are limited to 8 KiB of headers and 16 MiB of body.

## What the runner does not do

No traversal outside the fixture or manifest root, no source writes outside the owned
temporary fixture, no cache purge, and no gate evaluation (`releaseGateStatus` stays
`not_evaluated` until the owning later phases exist).

## Gate evidence rules

`validate_sample` rejects negative time, schema mismatch, missing fixture/binary/input
identity, malformed or unscoped counter snapshots (negative counts, non-boolean
`saturated`, no `countScope`), unlabelled availability and an `ok` sample that carries a
protocol error. Every phase-1 sample must have `releaseGateStatus: "not_evaluated"`.
`gate_status(sample, gate)` only says whether a valid sample carries the evidence a later
gate needs; it never evaluates a target:

- `parse_count`: needs available, non-saturated counter snapshots (a saturated snapshot
  is a valid timing but cannot satisfy it).
- `complete_query`: needs available coverage. Today coverage is always unavailable
  (`index_coverage_not_instrumented`), so timing-only samples cannot pass it.
- `storage`: needs available storage usage. Persistence does not exist
  (`persistence_not_implemented`).

## Counter coverage and known exclusions

Counters are process-lifetime aggregates; `coreParserInvocations`, `lspDocumentPreparations`
and `resolverMetadataParses` overlap by design and **must never be summed**. Known gaps:

- l4d-yehp: direct `project_content_hash` call sites are not counted in `legacyHashBytes`.
- l4d-vdl5: parse sites outside `pascal_core::parser::parse_bytes` are not in
  `coreParserInvocations` (`directive_fragment_rewrite`, cfg-pascal `prepared.rs`, lint4d
  `cfg/project_snapshot.rs`).

Absolute parse numbers are therefore lower bounds for the LSP references path only.

## Recorded phase-1 baselines (synthetic only)

Setup: release builds, 500 units / 5 importers (digest prefix `197518b3608f`, expected
fingerprint prefix `707e0eff8f2f`, 10 locations), Intel i7-9700, 8 CPUs, 32 GB RAM, Linux.
The machine was **not idle** (load average roughly 1.7-4.6 while running). Binaries: the
retained uninstrumented build (from master + docs, sha256 `34f7251a...c7624d7c79`),
and the instrumented build with `--profiling off` and `on` (profiling off = env unset).
Warm/request scenarios have 30 samples, cold 5, the over-limit fixture 3. Runs were
interleaved per scenario across the three binaries. Median ms (p95 in parentheses):

| Scenario | uninstrumented | new, profiling off | new, profiling on |
| --- | --- | --- | --- |
| cold (5) | 772.8 (960.2) | 717.4 (783.9) | 737.1 (767.0) |
| warm (30) | 525.2 (619.6) | 519.3 (529.9) | 519.6 (659.8) |
| restart (30) | 685.5 (771.8) | 694.9 (736.1) | 690.5 (738.2) |
| budget_pressure (30) | 515.0 (632.8) | 510.1 (536.7) | 511.1 (676.2) |
| two_sessions s0 / s1 (30) | 744.1 / 751.2 | 741.1 / 736.2 | 736.3 / 744.1 |

Edit and configuration-switch variants are in the retained result files. Differences
between binaries are within sample spread on a shared machine; **no overhead target
(disabled <=5%, first cold <=20%) is evaluated or claimed**. Samples were all valid and
result fingerprints agreed across scenarios and binaries.

Profiling-on counters (cold, one request on a fresh process): 508 core parser invocations,
503 document preparations, 1 resolver metadata parse, 4,666 stamp probes, ~460 KB
authorized payload, ~149 KB legacy hash bytes. The warm scenario's process totals are
about 15.5k parser invocations over 31 requests, i.e. roughly one parse per unit per
request: today nothing is reused across requests. Counters are attached to the last
sample of a shared process, so the other warm samples show `metrics_not_reported_by_binary_or_run_not_orderly`;
the uninstrumented binary and `profiling off` show `profiling_off`/the same reason, never zero.

Unavailable: child RSS and block IO (GNU `time` is not installed), index coverage,
storage usage, heap peak.

### Over-limit fixture (10,500 units)

All 3 samples of cold and warm failed on every binary with protocol error `-32803`,
`project directory entry limit (10000) reached`. This is today's directory-entry limit
(not raised); these samples are `invalid` and carry no timing. No larger-fixture baseline
exists until the limit or indexing changes.

### Real-corpus baseline

Real-corpus baseline unavailable: no user-selected manifest. The real-corpus release gate
stays open on l4d-zf1b.12; no synthetic number substitutes for it.

## Later release targets (not measured here)

>=90% fewer unchanged-restart indexing parses; fully reconciled representative references
p95 <=5 s; first cold index overhead <=20%; disabled overhead <=5%. The restart scenario
reuses source bytes with a new process and implies no persistence; the baselines above
are the comparison, to be extended with real instrumentation by the owning phases.
