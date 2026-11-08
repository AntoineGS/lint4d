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
    // Four reads per importer (the snapshot's scan, the resolver's load
    // while binding, two revalidation reads) and a constant few of the
    // requesting context's project files (12 measured: App.dproj x9,
    // App.dpr x3); the open sources are not read. Never per unit.
    assert!(
        used[0] <= 4 * SPEC.importers as u64 + 16,
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

/// R9-8: a required source the analysis memory budget refuses fails the
/// request as `ServerCancelled` (-32802), which clients retry, not as a
/// final `RequestFailed`.
#[test]
fn a_refused_required_source_is_a_retryable_server_cancellation() {
    const SMALL: FixtureSpec = FixtureSpec {
        units: 6,
        importers: 3,
        with_include_every: 10,
        unresolved_includes: 0,
        shared_name: "TShared",
    };
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SMALL);
    let cache_home = environment.path().join("cold-cache");
    let mut server = TestServer::launch_test_server_with_environment_path_and_variables(
        environment.path(),
        vec![
            ("XDG_CACHE_HOME", cache_home.to_str().expect("UTF-8 path")),
            ("PASCAL_LSP_TEST_HYDRATION_LIMIT_BYTES", "0"),
        ],
    );
    server.initialize(&root, json!({"persistentCacheEnabled": false}));
    open_disk(&mut server, &root, &["App.dpr"]);
    wait_index(&mut server, complete);
    settle_background(&mut server);
    let id = RequestId::from("refused-references".to_string());
    server.send_request(
        id.clone(),
        "textDocument/references",
        json!({
            "textDocument": {"uri": uri(&root.join("lib/Shared.pas"))},
            "position": {"line": 2, "character": 6},
            "context": {"includeDeclaration": true},
        }),
    );
    let response = server.response(&id);
    let error = response.error.expect("the declaring unit is refused");
    assert_eq!(error.code, -32802, "{error:?}");
    assert_eq!(
        error.message,
        "the analysis memory budget is exhausted; retry when other work finishes"
    );
    shutdown(server);
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

/// The index contexts `pascal/indexStatus` reports, polled until they
/// satisfy `predicate`.
fn wait_contexts(server: &mut TestServer, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let deadline = Instant::now() + IDLE_DEADLINE;
    loop {
        let status = request(server, "pascal/indexStatus", json!({}));
        let contexts = status["contexts"].as_array().cloned().unwrap_or_default();
        if predicate(&contexts) {
            return contexts;
        }
        assert!(
            Instant::now() < deadline,
            "index did not reach the expected state: {status}"
        );
    }
}

/// AC4 (R9-6): the eight one-unit projects `others/p1..p8` fill the
/// index store, evicting App.dproj's context index. A request on it then
/// answers partially, the members past the bound of direct reads being the
/// "not indexed yet" gap, and queues its reuse pass; once that pass has
/// reloaded every member from the store, the same request answers fully.
#[test]
fn an_evicted_target_answers_partially_then_fully_after_its_reuse_pass() {
    const EVICTED: FixtureSpec = FixtureSpec {
        units: 300,
        importers: 5,
        with_include_every: 10,
        unresolved_includes: 0,
        shared_name: "TShared",
    };
    const OTHERS: usize = 8;
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &EVICTED);
    for index in 1..=OTHERS {
        let project = root.join(format!("others/p{index}"));
        write_file(
            &project.join("App.dproj"),
            "<Project><PropertyGroup><MainSource>App.dpr</MainSource></PropertyGroup></Project>",
        );
        write_file(&project.join("App.dpr"), "program App;\nbegin\nend.\n");
    }
    pascal_project::backdate_tree_for_test(&root);
    let shared = root.join("lib/Shared.pas");
    let is_app = |context: &Value| context["members"].as_u64().unwrap() > 1;

    let mut server = launch_labelled(environment.path(), "cache", "evicted");
    server.initialize(&root, Value::Null);
    open_disk(&mut server, &root, &OPENS);
    wait_index(&mut server, persisted);
    settle_background(&mut server);
    gaps(&mut server);
    let expected = references(&mut server, &shared, 2, 6);
    assert_eq!(expected.len(), EVICTED.importers + 1, "{expected:?}");
    assert_eq!(gaps(&mut server), Vec::<String>::new());

    let others = (1..=OTHERS)
        .map(|index| format!("others/p{index}/App.dpr"))
        .collect::<Vec<_>>();
    open_disk(
        &mut server,
        &root,
        &others.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    wait_contexts(&mut server, |contexts| {
        contexts.len() == OTHERS && contexts.iter().all(complete) && !contexts.iter().any(is_app)
    });
    settle_background(&mut server);
    assert!(
        wait_contexts(&mut server, |_| true)
            .iter()
            .all(|context| !is_app(context)),
        "nothing indexes App.dproj again until a request needs it"
    );

    gaps(&mut server);
    let partial = references(&mut server, &shared, 2, 6);
    let notes = gaps(&mut server);
    assert!(
        notes
            .iter()
            .any(|note| note.contains("units of App.dproj are not indexed yet")),
        "{notes:?}"
    );
    assert!(
        partial.iter().all(|location| expected.contains(location)),
        "{partial:?}"
    );

    let reloaded = wait_contexts(&mut server, |contexts| {
        contexts
            .iter()
            .any(|context| is_app(context) && complete(context))
    });
    let reloaded = reloaded.into_iter().find(is_app).unwrap();
    assert_eq!(reloaded["extracted"], 0, "{reloaded}");
    assert_eq!(reloaded["reused"], reloaded["members"], "{reloaded}");
    settle_background(&mut server);
    gaps(&mut server);
    assert_eq!(references(&mut server, &shared, 2, 6), expected);
    assert_eq!(gaps(&mut server), Vec::<String>::new());
    shutdown(server);
}

/// Spec §7 (D16): the second server's reuse pass is held at
/// `PASCAL_LSP_TEST_REUSE_PASS_BARRIER` once `HELD` members were reused.
/// A request then answers before `complete` with only proven locations and
/// the "not indexed yet" gap; after `complete` it answers fully.
#[test]
fn a_query_during_the_reuse_pass_answers_proven_partial_results() {
    const HELD: usize = 5;
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SPEC);
    let shared = root.join("lib/Shared.pas");
    let not_indexed = |notes: &[String]| {
        notes
            .iter()
            .any(|note| note.contains("units of App.dproj are not indexed yet"))
    };

    let mut first = launch_labelled(environment.path(), "cache", "reuse-first");
    first.initialize(&root, Value::Null);
    open_disk(&mut first, &root, &OPENS);
    wait_index(&mut first, persisted);
    settle_background(&mut first);
    gaps(&mut first);
    let expected = references(&mut first, &shared, 2, 6);
    assert_eq!(expected.len(), SPEC.importers + 1, "{expected:?}");
    assert!(!not_indexed(&gaps(&mut first)));
    shutdown(first);

    let directory = environment.path().join("reuse-pass-barrier");
    fs::create_dir_all(&directory).unwrap();
    let barrier = TestBarrier {
        entered: directory.join("entered"),
        release: directory.join("release"),
    };
    let spec = format!(
        "{}|{}|{HELD}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let cache_home = environment.path().join("cache");
    let mut second = TestServer::launch_test_server_with_environment_path_and_variables(
        environment.path(),
        [
            ("XDG_CACHE_HOME", cache_home.to_str().expect("UTF-8 path")),
            ("PASCAL_LSP_TEST_REUSE_PASS_BARRIER", spec.as_str()),
        ],
    );
    second.initialize(&root, Value::Null);
    open_disk(&mut second, &root, &OPENS);
    barrier.wait_until_entered();
    gaps(&mut second);
    let partial = references(&mut second, &shared, 2, 6);
    let notes = gaps(&mut second);
    let held = request(&mut second, "pascal/indexStatus", json!({}));
    assert!(
        held["contexts"]
            .as_array()
            .is_some_and(|contexts| !contexts.iter().any(complete)),
        "the response arrived before complete: {held}"
    );
    assert!(not_indexed(&notes), "{notes:?}");
    assert!(
        !partial.is_empty(),
        "the declaring unit and open sources are proven: {partial:?}"
    );
    assert!(
        partial.iter().all(|location| expected.contains(location)),
        "only proven locations: {partial:?}"
    );

    fs::write(&barrier.release, b"").unwrap();
    let done = wait_index(&mut second, complete);
    assert_eq!(done["extracted"], 0, "{done}");
    settle_background(&mut second);
    gaps(&mut second);
    assert_eq!(references(&mut second, &shared, 2, 6), expected);
    assert!(!not_indexed(&gaps(&mut second)));
    shutdown(second);
}

/// A4: a didOpen whose `text` is `text`, not the file's.
fn open_text(server: &mut TestServer, path: &Path, text: &str) {
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(path),
            "languageId": "pascal",
            "version": 1,
            "text": text,
        }}),
    );
}

/// A4 over persisted facts: an open buffer of `Importer3.pas` without its
/// occurrence removes its location, and one of `Other7.pas` that imports
/// `Shared` and spells `TShared` adds one; an uncached server with the same
/// buffers gives the same answer.
#[test]
fn an_unsaved_buffer_overrides_persisted_postings_both_ways() {
    const SMALL: FixtureSpec = FixtureSpec {
        units: 60,
        importers: 5,
        with_include_every: 10,
        unresolved_includes: 0,
        shared_name: "TShared",
    };
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SMALL);
    let shared = root.join("lib/Shared.pas");
    let importer = root.join("lib/Importer3.pas");
    let other = root.join("lib/Other7.pas");
    let removed = fs::read_to_string(&importer)
        .unwrap()
        .replace("  TShared.Create;\n", "");
    let added = fs::read_to_string(&other)
        .unwrap()
        .replace("type TShared = class end;\n", "uses Shared;\n");
    assert!(added.contains("uses Shared;") && added.contains("TShared.Create"));
    let in_file =
        |location: &(String, u32, u32), name: &str| location.0.ends_with(&format!("/lib/{name}"));
    let buffered = |server: &mut TestServer| {
        open_text(server, &importer, &removed);
        open_text(server, &other, &added);
        settle_background(server);
        references(server, &shared, 2, 6)
    };

    let mut warm = warm_server(environment.path(), &root, &OPENS);
    let before = references(&mut warm, &shared, 2, 6);
    assert_eq!(before.len(), SMALL.importers + 1, "{before:?}");
    assert!(
        before
            .iter()
            .any(|location| in_file(location, "Importer3.pas"))
    );
    let found = buffered(&mut warm);
    shutdown(warm);
    assert!(
        !found
            .iter()
            .any(|location| in_file(location, "Importer3.pas")),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|location| in_file(location, "Other7.pas") && (location.1, location.2) == (6, 2)),
        "{found:?}"
    );
    let unchanged = |locations: &[(String, u32, u32)]| {
        locations
            .iter()
            .filter(|location| {
                !in_file(location, "Importer3.pas") && !in_file(location, "Other7.pas")
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(unchanged(&found), unchanged(&before));
    assert_eq!(found.len(), before.len());

    let mut cold = cold_server(environment.path(), &root, &OPENS);
    assert_eq!(buffered(&mut cold), found, "the uncached answer");
    shutdown(cold);
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

/// A source with conditional directives open with its saved text: the
/// closed-file load of the index batch must not take the open document's
/// cached expansion, whose root revision is the buffer's, or the member
/// never persists and the context never completes.
#[test]
fn an_open_source_with_directives_persists_like_its_closed_file() {
    const SMALL: FixtureSpec = FixtureSpec {
        units: 60,
        importers: 5,
        with_include_every: 10,
        unresolved_includes: 0,
        shared_name: "TShared",
    };
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SMALL);
    write_file(
        &root.join("App.dpr"),
        "program App;\n{$IFDEF DEBUG}{$ENDIF}\nbegin\nend.\n",
    );
    pascal_project::backdate_tree_for_test(&root);
    let mut server = launch_labelled(environment.path(), "cache", "directives");
    server.initialize(&root, Value::Null);
    open_disk(&mut server, &root, &["App.dpr"]);
    wait_index(&mut server, persisted);
    shutdown(server);
}
