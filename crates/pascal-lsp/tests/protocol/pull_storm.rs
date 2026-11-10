//! l4d-zh7i: a burst of file changes (a `git pull`, or a build writing
//! compiled units beside the sources) while the editor is open. Requests
//! stay admitted and answer, nothing stale is proven, and the server
//! converges, re-extracting each changed unit once.
#![cfg(unix)]

use super::persistent_cache::request;
use super::persistent_index::wait_index;
use super::persistent_queries::{
    FixtureSpec, delta, metrics, query_fixture, settle_background, warm_server,
};
use super::*;
use std::path::PathBuf;

const SPEC: FixtureSpec = FixtureSpec {
    units: 400,
    importers: 20,
    with_include_every: 10,
    unresolved_includes: 0,
    shared_name: "TShared",
};
/// `file_watch::WATCH_EVENT_CAPACITY`.
const WATCH_EVENT_QUEUE: usize = 1024;
const OPENS: [&str; 2] = ["App.dpr", "lib/Shared.pas"];

fn others(root: &Path) -> Vec<PathBuf> {
    (0..SPEC.units - SPEC.importers)
        .map(|index| root.join(format!("lib/Other{index}.pas")))
        .collect()
}

#[derive(Debug, Default)]
struct Probe {
    answered: usize,
    errors: Vec<(i32, String)>,
    slowest: Duration,
}

fn probe(server: &mut TestServer, method: &str, params: Value, probe: &mut Probe) -> Option<Value> {
    let id = RequestId::from(format!(
        "storm-{method}-{}",
        probe.answered + probe.errors.len()
    ));
    let started = Instant::now();
    server.send_request(id.clone(), method, params);
    let response = server.response(&id);
    probe.slowest = probe.slowest.max(started.elapsed());
    match response.error {
        Some(error) => {
            probe.errors.push((error.code, error.message));
            None
        }
        None => {
            probe.answered += 1;
            response.result
        }
    }
}

fn references_params(root: &Path) -> Value {
    json!({
        "textDocument": {"uri": uri(&root.join("lib/Shared.pas"))},
        "position": {"line": 2, "character": 6},
        "context": {"includeDeclaration": true},
    })
}

/// A server over the test's shared store (`environment/cache`), with
/// analysis metrics, its trace in `$PASCAL_LSP_TRACE.<label>`, and `extra`
/// variables.
fn launch(environment: &Path, label: &str, extra: &[(&str, &str)]) -> TestServer {
    let cache_home = environment.join("cache");
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
    variables.extend_from_slice(extra);
    TestServer::launch_test_server_with_environment_path_and_variables(environment, variables)
}

fn open_disk(server: &mut TestServer, root: &Path) {
    for relative in OPENS {
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

/// `coreParserInvocations` now, without waiting for background work.
fn parses_now(server: &mut TestServer) -> u64 {
    let result = request(server, "pascal/testAnalysisMetrics", json!({}));
    result["counters"]["coreParserInvocations"]
        .as_u64()
        .expect("parser counter")
}

/// Every `Other` unit loses its `TShared` and declares `TPulled<n>`.
fn pull(root: &Path) -> usize {
    let others = others(root);
    for (index, path) in others.iter().enumerate() {
        write_file(
            path,
            &format!(
                "unit Other{index};\ninterface\ntype TPulled{index} = class end;\nimplementation\nend.\n"
            ),
        );
    }
    others.len()
}

fn the_context(server: &mut TestServer) -> Value {
    let status = request(server, "pascal/indexStatus", json!({}));
    let contexts = status["contexts"].as_array().expect("contexts");
    assert_eq!(contexts.len(), 1, "{status}");
    contexts[0].clone()
}

/// The files declaring a symbol named exactly `name`.
fn symbol_units(symbols: &Value, name: &str) -> Vec<String> {
    let mut units: Vec<String> = symbols
        .as_array()
        .expect("symbols")
        .iter()
        .filter(|symbol| symbol["name"] == name)
        .filter_map(|symbol| symbol["location"]["uri"].as_str())
        .map(|uri| uri.rsplit('/').next().unwrap().to_string())
        .collect();
    units.sort();
    units.dedup();
    units
}

/// l4d-zh7i: a workspace-wide request held for the whole burst does not
/// stall the index (D50 lets it hold index batches back, but not forever):
/// the index converges while it runs, every changed unit extracted once,
/// and the request then answers.
#[test]
fn the_index_converges_while_a_workspace_wide_request_runs_through_a_burst() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SPEC);
    let mut first = launch(environment.path(), "storm-first", &[]);
    first.initialize(&root, Value::Null);
    open_disk(&mut first, &root);
    wait_index(&mut first, super::persistent_index::persisted);
    first.shutdown();

    let barrier_directory = environment.path().join("references-barrier");
    fs::create_dir_all(&barrier_directory).unwrap();
    let barrier = TestBarrier {
        entered: barrier_directory.join("entered"),
        release: barrier_directory.join("release"),
    };
    let spec = format!(
        "{}|{}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let mut server = launch(
        environment.path(),
        "storm",
        &[("PASCAL_LSP_TEST_REFERENCES_BARRIER", spec.as_str())],
    );
    server.initialize(&root, Value::Null);
    open_disk(&mut server, &root);
    let warm = wait_index(&mut server, |context| context["complete"] == true);
    assert_eq!(warm["extracted"], 0, "{warm}");
    settle_background(&mut server);
    let parses = parses_now(&mut server);

    let changed = pull(&root);
    let held = RequestId::from("held-references".to_string());
    server.send_request(
        held.clone(),
        "textDocument/references",
        references_params(&root),
    );
    barrier.wait_until_entered();
    let deadline = Instant::now() + Duration::from_secs(120);
    let converged = loop {
        let context = the_context(&mut server);
        if context["complete"] == true && context["fresh"] == context["members"] {
            break context;
        }
        assert!(
            Instant::now() < deadline,
            "the index did not converge while the request was held: {context}"
        );
    };
    let parsed = parses_now(&mut server) - parses;
    barrier.release();
    let response = server.response(&held);
    assert!(response.error.is_none(), "{response:?}");

    assert_eq!(converged["extracted"], changed as u64, "{converged}");
    assert!(
        parsed as usize <= changed + OPENS.len() + 2,
        "{parsed} parses for {changed} changed units"
    );
    let symbols = request(
        &mut server,
        "workspace/symbol",
        json!({"query": "TPulled7"}),
    );
    assert_eq!(
        symbol_units(&symbols, "TPulled7"),
        ["Other7.pas"],
        "{symbols}"
    );
    let stale = request(&mut server, "workspace/symbol", json!({"query": "TShared"}));
    assert_eq!(symbol_units(&stale, "TShared"), ["Shared.pas"], "{stale}");
    server.shutdown();
}

/// The paths and overflows the server's watcher applied so far.
fn watch_counts(server: &mut TestServer) -> (u64, u64) {
    let state = request(server, "pascal/testBackgroundIdle", json!({}));
    (
        state["watchPaths"].as_u64().expect("watch paths"),
        state["watchOverflows"].as_u64().expect("watch overflows"),
    )
}

/// `environment/workspace` with `App.dproj` (installation 7.0), `App.dpr`
/// and `Main.pas` declaring `Run`; the installation's IDE directory is
/// `environment/ide`, outside the workspace (Headless-Delphi keeps it under
/// `~/.local/share`), with an `environment.proj`.
fn installation_fixture(environment: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let root = environment.join("workspace");
    let sdk = environment.join("sdk");
    let ide = environment.join("ide");
    write_file(&sdk.join("bin/rsvars.bat"), "@SET BDS=C:\\SDK\n");
    write_file(
        &sdk.join("source/SdkUnit.pas"),
        "unit SdkUnit; interface implementation end.",
    );
    write_file(
        &ide.join("EnvOptions.proj"),
        "<Project><PropertyGroup><Win32LibraryPath>$(BDS)\\source</Win32LibraryPath></PropertyGroup></Project>",
    );
    write_file(
        &ide.join("environment.proj"),
        "<Project><PropertyGroup><EnvValue>1</EnvValue></PropertyGroup></Project>",
    );
    write_file(
        &root.join(".delphi-tools.local.toml"),
        &format!(
            "[installations.\"7.0\".properties]\nBDS='{}'\nAPPDATA='{}'\nPlatform='Win32'\n[projects.\"App.dproj\"]\ninstallation='7.0'\n",
            sdk.display(),
            ide.display(),
        ),
    );
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><Platform>Win32</Platform></PropertyGroup></Project>",
    );
    write_file(&root.join("App.dpr"), "program App; uses Main; begin end.");
    let main = root.join("Main.pas");
    write_file(
        &main,
        "unit Main;\ninterface\nuses SdkUnit;\nprocedure Run;\nimplementation\nprocedure Run;\nbegin\nend;\nend.\n",
    );
    pascal_project::backdate_tree_for_test(environment);
    (root, main, ide.join("environment.proj"))
}

/// l4d-zh7i: Headless-Delphi builds rewrite the installation's
/// `environment.proj`, outside the workspace, with the same content and a
/// new inode and modification time; in w2-gap every workspace/symbol after
/// the first build was refused (-32803) naming that file. Requests over a
/// rewrite between sessions and one at the end of a build must still
/// answer. This scenario passes on 037e4f8 too: it guards the case, the
/// w2-gap refusals are not reproduced by it.
#[test]
fn a_same_content_rewrite_of_an_unwatched_metadata_file_does_not_stale_every_request() {
    let environment = tempfile::tempdir().unwrap();
    let (root, main, environment_proj) = installation_fixture(environment.path());
    let open = |server: &mut TestServer| {
        server.initialize(&root, Value::Null);
        server.send_notification(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri(&main),
                "languageId": "pascal",
                "version": 1,
                "text": fs::read_to_string(&main).unwrap(),
            }}),
        );
    };
    let mut first = launch(environment.path(), "metadata-first", &[]);
    open(&mut first);
    let context = request(
        &mut first,
        "pascal/projectContext",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    assert!(
        context["installationConfigUris"]
            .as_array()
            .is_some_and(|uris| uris.iter().any(|uri| uri
                .as_str()
                .is_some_and(|uri| uri.ends_with("/ide/environment.proj")))),
        "{context}"
    );
    wait_index(&mut first, super::persistent_index::persisted);
    first.shutdown();

    // A build between the sessions rewrites it.
    let rewrite = || {
        let content = fs::read(&environment_proj).unwrap();
        let replacement = environment_proj.with_extension("proj.tmp");
        fs::write(&replacement, &content).unwrap();
        fs::rename(&replacement, &environment_proj).unwrap();
    };
    rewrite();
    let mut server = launch(environment.path(), "metadata", &[]);
    open(&mut server);
    wait_index(&mut server, |context| context["complete"] == true);
    settle_background(&mut server);
    let mut refused = Vec::new();
    let symbols = |server: &mut TestServer, label: &str, refused: &mut Vec<String>| {
        for attempt in 0..3 {
            let id = RequestId::from(format!("symbols-{label}-{attempt}"));
            server.send_request(id.clone(), "workspace/symbol", json!({"query": "Run"}));
            let response = server.response(&id);
            match response.error {
                Some(error) => refused.push(format!("{label}: {}", error.message)),
                None => {
                    let symbols = response.result.unwrap();
                    assert_eq!(symbol_units(&symbols, "Run"), ["Main.pas"], "{symbols}");
                }
            }
        }
    };
    symbols(&mut server, "warm", &mut refused);

    // A build during the session writes its compiled units beside the
    // sources (on 037e4f8, more events than its watcher queue held: the
    // overflow lost which paths changed), then rewrites it again.
    for index in 0..3 * WATCH_EVENT_QUEUE {
        fs::write(root.join(format!("Unit{index}.dcu")), b"dcu").unwrap();
    }
    rewrite();
    symbols(&mut server, "after the build", &mut refused);
    assert!(refused.is_empty(), "{refused:?}");
    server.shutdown();
}

/// l4d-zh7i (the w2-gap builds): a build writes its compiled units beside
/// the sources, about three times as many watcher events as the queue held.
/// No source changed, so no member is proven again: before, the queue
/// overflowed, and an overflow stales every member of every index (each is
/// read and hashed again) and drops every cache entry.
#[test]
fn a_build_writing_compiled_units_beside_the_sources_stales_no_member() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SPEC);
    let mut server = warm_server(environment.path(), &root, &OPENS);
    let warm = the_context(&mut server);
    let before = metrics(&mut server);
    let (paths, overflows) = watch_counts(&mut server);
    let written = 3 * WATCH_EVENT_QUEUE;
    for index in 0..written {
        let name = if index < SPEC.units - SPEC.importers {
            format!("lib/Other{index}.dcu")
        } else {
            format!("lib/Build{index}.dcu")
        };
        fs::write(root.join(name), b"dcu").unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (now_paths, now_overflows) = watch_counts(&mut server);
        if now_paths - paths >= written as u64 || now_overflows > overflows {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{now_paths} paths, {now_overflows} overflows"
        );
    }
    let used = delta(metrics(&mut server), before);
    let after = the_context(&mut server);
    assert_eq!(after["fresh"], after["members"], "{after}");
    assert_eq!(after["extracted"], 0, "{after}");
    assert_eq!(
        after["reused"], warm["reused"],
        "no member proven again: {after}"
    );
    assert!(used[0] < 32, "payload reads {used:?}");
    server.shutdown();
}

/// l4d-zh7i: back-to-back references and workspace/symbol requests through
/// a burst (each would stop an index batch at its first member, D50) are
/// all answered, and the index converges meanwhile, every changed unit
/// extracted once. Prints its numbers.
#[test]
fn the_index_converges_through_a_stream_of_workspace_wide_requests() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &SPEC);
    let mut server = warm_server(environment.path(), &root, &OPENS);
    let before = metrics(&mut server);
    let started = Instant::now();
    let changed = pull(&root);
    let written = started.elapsed();
    let mut references = Probe::default();
    let mut symbols = Probe::default();
    let mut rounds = 0;
    let converged = loop {
        rounds += 1;
        probe(
            &mut server,
            "textDocument/references",
            references_params(&root),
            &mut references,
        );
        probe(
            &mut server,
            "workspace/symbol",
            json!({"query": "TPulled7"}),
            &mut symbols,
        );
        let context = the_context(&mut server);
        if context["complete"] == true && context["fresh"] == context["members"] {
            break context;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the index did not converge through {rounds} rounds of requests: {context}"
        );
    };
    let elapsed = started.elapsed();
    let used = delta(metrics(&mut server), before);
    eprintln!(
        "pull storm: {changed} rewrites in {written:?}; converged after {elapsed:?}, {rounds} rounds; \
         references {references:?}; symbols {symbols:?}; parses {}; status {converged}",
        used[5],
    );
    assert!(references.errors.is_empty(), "{references:?}");
    assert!(symbols.errors.is_empty(), "{symbols:?}");
    // A unit staled again by the second event of its write, after its
    // extraction, is proven again from what that extraction published.
    let extracted = converged["extracted"].as_u64().unwrap();
    assert!(extracted <= changed as u64, "{converged}");
    assert_eq!(
        extracted + converged["reused"].as_u64().unwrap(),
        converged["members"].as_u64().unwrap(),
        "{converged}"
    );
    let found = request(
        &mut server,
        "workspace/symbol",
        json!({"query": "TPulled7"}),
    );
    assert_eq!(symbol_units(&found, "TPulled7"), ["Other7.pas"], "{found}");
    server.shutdown();
}
