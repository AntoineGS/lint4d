//! Spec §8.2 and §12 (`l4d-zf1b.9`, AC1/AC2): read-only queries of a warm
//! session answer from persisted facts. A warm pair shares the test's
//! `environment/cache`; the uncached server has its own cache home.
//! Every server runs with `PASCAL_LSP_ANALYSIS_METRICS=1`; with
//! `PASCAL_LSP_TRACE` set in the test's environment, each server appends
//! to its own `<trace>.<label>` file.
#![cfg(unix)]

use super::persistent_cache::request;
use super::persistent_index::{complete, persisted, shutdown, wait_index};
use super::*;
use std::path::PathBuf;

pub(super) struct FixtureSpec {
    pub(super) units: usize,
    pub(super) importers: usize,
    pub(super) with_include_every: usize,
    pub(super) unresolved_includes: usize,
    pub(super) shared_name: &'static str,
}

/// `App.dproj` (search path `lib`) and `App.dpr`, `lib/Shared.pas`
/// declaring `shared_name`, then `units` units: `importers` use `Shared`
/// and spell its type in a body; `unresolved_includes` include the missing
/// `Missing.inc`, declare an unrelated `TOther` and spell `shared_name`
/// without importing `Shared`; the others declare and spell their own
/// `shared_name`, every `with_include_every`-th including the resolving
/// `lib/Common.inc`, which does not spell it.
pub(super) fn query_fixture(environment: &Path, spec: &FixtureSpec) -> PathBuf {
    let root = environment.join("workspace");
    let name = spec.shared_name;
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup><ItemGroup></ItemGroup></Project>",
    );
    write_file(&root.join("App.dpr"), "program App;\nbegin\nend.\n");
    write_file(
        &root.join("lib/Shared.pas"),
        &format!("unit Shared;\ninterface\ntype {name} = class end;\nimplementation\nend.\n"),
    );
    write_file(&root.join("lib/Common.inc"), "{ common }\n");
    let body = format!("procedure Run;\nbegin\n  {name}.Create;\nend;\n");
    for index in 0..spec.importers {
        write_file(
            &root.join(format!("lib/Importer{index}.pas")),
            &format!(
                "unit Importer{index};\ninterface\nuses Shared;\nimplementation\n{body}end.\n"
            ),
        );
    }
    for index in 0..spec.unresolved_includes {
        write_file(
            &root.join(format!("lib/Unresolved{index}.pas")),
            &format!(
                "unit Unresolved{index};\ninterface\n{{$I Missing.inc}}\ntype TOther = class end;\nimplementation\n{body}end.\n"
            ),
        );
    }
    let others = spec.units - spec.importers - spec.unresolved_includes;
    for index in 0..others {
        let include = if index % spec.with_include_every == 0 {
            "{$I Common.inc}\n"
        } else {
            ""
        };
        write_file(
            &root.join(format!("lib/Other{index}.pas")),
            &format!(
                "unit Other{index};\ninterface\n{include}type {name} = class end;\nimplementation\n{body}end.\n"
            ),
        );
    }
    pascal_project::backdate_tree_for_test(&root);
    root
}

fn launch_labelled(environment: &Path, cache: &str, label: &str) -> TestServer {
    let cache_home = environment.join(cache);
    let trace = std::env::var("PASCAL_LSP_TRACE")
        .ok()
        .map(|path| format!("{path}.{label}"));
    let mut variables = vec![
        ("XDG_CACHE_HOME", cache_home.to_str().expect("UTF-8 path")),
        ("PASCAL_LSP_ANALYSIS_METRICS", "1"),
    ];
    if let Some(trace) = &trace {
        variables.push(("PASCAL_LSP_TRACE", trace.as_str()));
    }
    TestServer::launch_test_server_with_environment_path_and_variables(environment, variables)
}

fn open_disk(server: &mut TestServer, root: &Path, opens: &[&str]) {
    for relative in opens {
        let path = root.join(relative);
        server.send_notification(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri(&path),
                "languageId": "pascal",
                "version": 1,
                "text": fs::read_to_string(&path).unwrap(),
            }}),
        );
    }
}

/// The second of two servers over the shared store: the first indexed and
/// persisted every member and shut down; this one reused them.
pub(super) fn warm_server(environment: &Path, root: &Path, opens: &[&str]) -> TestServer {
    let mut first = launch_labelled(environment, "cache", "warm-first");
    first.initialize(root, Value::Null);
    open_disk(&mut first, root, opens);
    wait_index(&mut first, persisted);
    shutdown(first);
    let mut second = launch_labelled(environment, "cache", "warm");
    second.initialize(root, Value::Null);
    open_disk(&mut second, root, opens);
    let done = wait_index(&mut second, complete);
    assert_eq!(done["extracted"], 0, "{done}");
    settle_background(&mut second);
    second
}

/// A server without the persistent cache, indexed in memory.
pub(super) fn cold_server(environment: &Path, root: &Path, opens: &[&str]) -> TestServer {
    let mut server = launch_labelled(environment, "cold-cache", "cold");
    server.initialize(root, json!({"persistentCacheEnabled": false}));
    open_disk(&mut server, root, opens);
    wait_index(&mut server, complete);
    settle_background(&mut server);
    server
}

const IDLE_DEADLINE: Duration = Duration::from_secs(120);

/// Waits until `pascal/testBackgroundIdle` reports no warmer crawl or index
/// batch, no analysis job and no diagnostic deadline: `complete` is
/// reported before the warm `Full` pass and the open documents' crawls
/// have settled. Nothing schedules new background work until the next
/// client message.
pub(super) fn settle_background(server: &mut TestServer) {
    let deadline = Instant::now() + IDLE_DEADLINE;
    loop {
        let state = request(server, "pascal/testBackgroundIdle", json!({}));
        if state["idle"] == true {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "background work did not settle: {state}"
        );
    }
}

/// The names of the `pascal/testAnalysisMetrics` counters, in the order of
/// `pascal_project::analysis_metrics::Metric`.
const METRICS: [&str; 12] = [
    "authorizedPayloadReadAttempts",
    "authorizedPayloadBytes",
    "authorizedPayloadReadFailures",
    "projectStampProbes",
    "legacyHashBytes",
    "coreParserInvocations",
    "lspDocumentPreparations",
    "resolverMetadataParses",
    "importBindingCalls",
    "importBindingPairs",
    "projectCacheProbeCalls",
    "projectCacheProbeNanos",
];

/// The process's analysis counters, indexed as `Metric`, once background
/// work has settled: a window between two calls holds the requests sent in
/// it and the background work they caused, nothing earlier.
pub(super) fn metrics(server: &mut TestServer) -> [u64; 12] {
    settle_background(server);
    let result = request(server, "pascal/testAnalysisMetrics", json!({}));
    assert_eq!(result["schema"], 1, "{result}");
    let counters = result["counters"].as_object().expect("counters");
    assert_eq!(counters.len(), METRICS.len(), "{result}");
    METRICS.map(|name| counters[name].as_u64().expect(name))
}

pub(super) fn delta(after: [u64; 12], before: [u64; 12]) -> [u64; 12] {
    std::array::from_fn(|index| {
        after[index].checked_sub(before[index]).unwrap_or_else(|| {
            panic!(
                "{} went down from {} to {}: counters are monotonic within a process",
                METRICS[index], before[index], after[index]
            )
        })
    })
}

/// The sorted `(uri, line, character)` starts of the references of the
/// symbol at `line`/`character` of `path`, declaration included.
pub(super) fn references(
    server: &mut TestServer,
    path: &Path,
    line: u32,
    character: u32,
) -> Vec<(String, u32, u32)> {
    let result = request(
        server,
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(path)},
            "position": {"line": line, "character": character},
            "context": {"includeDeclaration": true},
        }),
    );
    let mut found: Vec<_> = result
        .as_array()
        .expect("locations")
        .iter()
        .map(|location| {
            let start = &location["range"]["start"];
            (
                location["uri"].as_str().unwrap().to_string(),
                start["line"].as_u64().unwrap() as u32,
                start["character"].as_u64().unwrap() as u32,
            )
        })
        .collect();
    found.sort();
    found
}

/// The coverage notes (`window/logMessage`) received since the last call.
pub(super) fn gaps(server: &mut TestServer) -> Vec<String> {
    let mut notes = Vec::new();
    server.pending.retain(|message| match message {
        Message::Notification(notification)
            if notification.method == "window/logMessage"
                && notification.params["message"]
                    .as_str()
                    .is_some_and(|text| text.contains("returned an incomplete result")) =>
        {
            notes.push(notification.params["message"].as_str().unwrap().to_string());
            false
        }
        _ => true,
    });
    notes
}

/// The `workspace/symbol` result for `query` and its coverage notes.
pub(super) fn symbols_with_notes(
    server: &mut TestServer,
    query: &str,
) -> (Vec<Value>, Vec<String>) {
    gaps(server);
    let result = request(server, "workspace/symbol", json!({"query": query}));
    (result.as_array().cloned().unwrap_or_default(), gaps(server))
}

const OPENS: [&str; 2] = ["App.dpr", "lib/Shared.pas"];

const SPEC: FixtureSpec = FixtureSpec {
    units: 500,
    importers: 20,
    with_include_every: 10,
    unresolved_includes: 30,
    shared_name: "TShared",
};

#[test]
#[ignore = "red until l4d-zf1b.9 Task 5"]
fn warm_references_bind_only_the_candidates_and_match_the_uncached_answer() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SPEC);
    let mut cold = cold_server(environment.path(), &root, &OPENS);
    let expected = references(&mut cold, &root.join("lib/Shared.pas"), 2, 6);
    shutdown(cold);
    let mut warm = warm_server(environment.path(), &root, &OPENS);
    gaps(&mut warm);
    let before = metrics(&mut warm);
    let found = references(&mut warm, &root.join("lib/Shared.pas"), 2, 6);
    let used = delta(metrics(&mut warm), before);
    assert_eq!(
        found, expected,
        "cached and uncached proven locations agree"
    );
    assert_eq!(found.len(), SPEC.importers + 1);
    // The importers, the declaring unit and App.dpr, never the 30 units
    // whose include does not resolve (R9-4) nor the 450 others.
    assert!(used[5] <= (SPEC.importers + 2) as u64, "parses {used:?}");
    assert!(
        used[0] <= 4 * (SPEC.importers + 2) as u64,
        "payload reads {used:?}"
    );
    let notes = gaps(&mut warm);
    assert!(
        notes
            .iter()
            .any(|note| note.contains("30 units") && note.contains("do not resolve")),
        "{notes:?}"
    );
    shutdown(warm);
}

#[test]
fn warm_workspace_symbols_answer_from_facts_without_parsing() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SPEC);
    let mut warm = warm_server(environment.path(), &root, &OPENS);
    // Settles the open documents' state.
    let _ = symbols_with_notes(&mut warm, SPEC.shared_name);
    let before = metrics(&mut warm);
    let (symbols, notes) = symbols_with_notes(&mut warm, SPEC.shared_name);
    let used = delta(metrics(&mut warm), before);
    assert_eq!(
        used[5], 0,
        "no parse for an unchanged warm session: {used:?}"
    );
    assert_eq!(
        symbols.len(),
        SPEC.units - SPEC.importers - SPEC.unresolved_includes + 1,
        "{notes:?}"
    );
    let (units, _) = symbols_with_notes(&mut warm, "Shared");
    assert!(
        units
            .iter()
            .any(|symbol| symbol["name"] == "Shared" && symbol["kind"] == 2),
        "D49/R9-11: the unit itself (SymbolKind::MODULE = 2): {units:?}"
    );
    shutdown(warm);
}

/// The harness's windows are isolated: with background work settled and no
/// request sent, nothing is counted.
#[test]
fn a_settled_warm_server_counts_nothing_without_a_request() {
    const SMALL: FixtureSpec = FixtureSpec {
        units: 50,
        importers: 5,
        with_include_every: 10,
        unresolved_includes: 5,
        shared_name: "TShared",
    };
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SMALL);
    let mut warm = warm_server(environment.path(), &root, &OPENS);
    let before = metrics(&mut warm);
    assert_eq!(delta(metrics(&mut warm), before), [0; 12]);
    shutdown(warm);
}

fn percentiles(mut samples: Vec<Duration>) -> (Duration, Duration) {
    samples.sort();
    let at = |fraction: f64| samples[((samples.len() - 1) as f64 * fraction).round() as usize];
    (at(0.5), at(0.95))
}

/// Spec §12.2: synthetic numbers only, no time assertion. Run with
/// `cargo test --release -p pascal-lsp --features test-support --test
/// protocol_barriers synthetic_warm_query_latency -- --ignored --nocapture`.
#[test]
#[ignore = "release measurement"]
fn synthetic_warm_query_latency() {
    const LARGE: FixtureSpec = FixtureSpec {
        units: 5_000,
        importers: 20,
        with_include_every: 10,
        unresolved_includes: 50,
        shared_name: "TShared",
    };
    const RUNS: usize = 30;
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &LARGE);
    let mut warm = warm_server(environment.path(), &root, &OPENS);
    let shared = root.join("lib/Shared.pas");
    let references_taken = (0..RUNS)
        .map(|_| {
            let started = Instant::now();
            references(&mut warm, &shared, 2, 6);
            let taken = started.elapsed();
            gaps(&mut warm);
            taken
        })
        .collect();
    let symbols_taken = (0..RUNS)
        .map(|_| {
            let started = Instant::now();
            symbols_with_notes(&mut warm, LARGE.shared_name);
            started.elapsed()
        })
        .collect();
    let (p50, p95) = percentiles(references_taken);
    println!("warm textDocument/references: p50 {p50:?}, p95 {p95:?}");
    let (p50, p95) = percentiles(symbols_taken);
    println!("warm workspace/symbol: p50 {p50:?}, p95 {p95:?}");
    shutdown(warm);
}
