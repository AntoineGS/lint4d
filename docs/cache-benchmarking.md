# Cache benchmarking (phase 1 baselines)

`scripts/cache_baseline.py` is a Python-stdlib runner that drives `pascal-lsp --stdio`
over JSON-RPC and writes one schema-1 JSON sample per line. Phase 1 measures only:
no persistent cache exists yet, so nothing here reports a cache win. Task 5 extends
this guide with the recorded baselines and acceptance targets.

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
