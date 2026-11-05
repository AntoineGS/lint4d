//! Spec §12.1 and D34/D40/D42 (`l4d-zf1b.8`): persistent indexing through
//! the protocol. Every server of a test shares the test's private
//! `XDG_CACHE_HOME` (`environment/cache`), and the workspace root is
//! `environment/workspace`: neither contains the other (`l4d-vrw0`).
#![cfg(unix)]

use super::persistent_cache::{launch, request};
use super::*;
use std::path::PathBuf;

pub(super) const UNITS: usize = 300;
/// `App.dpr` and the units.
pub(super) const MEMBERS: u64 = UNITS as u64 + 1;
const INDEX_DEADLINE: Duration = Duration::from_secs(120);

/// `bulk` extra declarations per unit make each analysis slower.
fn named_unit(path: &Path, identifier: &str, include: bool, bulk: usize) {
    let unit = path.file_stem().unwrap().to_string_lossy().into_owned();
    let include = if include { "{$I Shared.inc}\n" } else { "" };
    let declarations: String = (0..bulk)
        .map(|index| format!("procedure {identifier}_{index};\n"))
        .collect();
    let bodies: String = (0..bulk)
        .map(|index| format!("procedure {identifier}_{index};\nbegin\nend;\n"))
        .collect();
    write_file(
        path,
        &format!(
            "unit {unit};\ninterface\n{include}procedure {identifier};\n{declarations}implementation\nprocedure {identifier};\nbegin\nend;\n{bodies}end.\n"
        ),
    );
}

/// `App.dproj` with the search path `lib`, `App.dpr` and `units` units
/// `lib/U<n>.pas`, each declaring `P<n>`; one in ten includes `Shared.inc`.
/// With `uses_units`, `App.dpr` uses every unit, so the warm crawl of the
/// open `App.dpr` analyses them all.
fn project_fixture(environment: &Path, units: usize, uses_units: bool, bulk: usize) -> PathBuf {
    let root = environment.join("workspace");
    write_file(
        &root.join("App.dproj"),
        "<Project><PropertyGroup><MainSource>App.dpr</MainSource><DCC_UnitSearchPath>lib</DCC_UnitSearchPath></PropertyGroup><ItemGroup></ItemGroup></Project>",
    );
    let uses = if uses_units {
        let names: Vec<String> = (0..units).map(|index| format!("U{index}")).collect();
        format!("uses {};\n", names.join(", "))
    } else {
        String::new()
    };
    write_file(
        &root.join("App.dpr"),
        &format!("program App;\n{uses}begin\nend.\n"),
    );
    write_file(&root.join("lib/Shared.inc"), "{ shared }\n");
    for index in 0..units {
        named_unit(
            &root.join(format!("lib/U{index}.pas")),
            &format!("P{index}"),
            index % 10 == 0,
            bulk,
        );
    }
    pascal_project::backdate_tree_for_test(&root);
    root
}

pub(super) fn fixture(environment: &Path, units: usize) -> PathBuf {
    project_fixture(environment, units, false, 0)
}

fn open_disk(server: &mut TestServer, path: &Path) {
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {
            "uri": uri(path),
            "languageId": "pascal",
            "version": 1,
            "text": fs::read_to_string(path).unwrap(),
        }}),
    );
}

fn initialize_and_open(server: &mut TestServer, root: &Path, relative: &str) {
    server.initialize(root, Value::Null);
    open_disk(server, &root.join(relative));
}

/// The context of the project: every fixture document is in it, so it is
/// anchored on the project file or on the most recently opened document.
fn app_context(status: &Value) -> Option<Value> {
    let contexts = status["contexts"].as_array()?;
    assert!(contexts.len() <= 1, "one project context: {status}");
    contexts.first().cloned()
}

/// Polls `pascal/indexStatus` until the context of `App.dpr` satisfies
/// `predicate`; panics with the last status after `INDEX_DEADLINE`.
pub(super) fn wait_index(server: &mut TestServer, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + INDEX_DEADLINE;
    loop {
        let status = request(server, "pascal/indexStatus", json!({}));
        assert!(status.to_string().len() <= 8192, "{status}");
        if let Some(context) = app_context(&status)
            && predicate(&context)
        {
            return context;
        }
        assert!(
            Instant::now() < deadline,
            "index did not reach the expected state: {status}"
        );
    }
}

fn complete(context: &Value) -> bool {
    context["complete"] == true
}

/// Complete, every member's record stored and a snapshot sealed or held:
/// `complete` alone is reported as soon as the last member is indexed,
/// before its publication and the seal, which a shutdown would cancel.
fn persisted(context: &Value) -> bool {
    complete(context) && context["persisted"] == context["members"] && context["snapshot"] != "none"
}

fn assert_nothing_refused_yet(context: &Value) {
    assert_eq!(context["refused"], 0, "{context}");
    assert_eq!(context["limited"], 0, "{context}");
}

fn assert_nothing_refused(context: &Value) {
    assert_nothing_refused_yet(context);
    assert_eq!(context["pending"], 0, "{context}");
}

/// The `workspace/symbol` result for `query`, asserting that the server
/// logged no coverage gap for it.
fn workspace_symbols(server: &mut TestServer, query: &str) -> Vec<Value> {
    let result = request(server, "workspace/symbol", json!({"query": query}));
    let gap = server.pending.iter().find(|message| {
        matches!(message, Message::Notification(notification)
            if notification.method == "window/logMessage"
                && notification.params["message"]
                    .as_str()
                    .is_some_and(|text| text.contains("workspace/symbol")))
    });
    assert!(gap.is_none(), "coverage gap for {query}: {gap:?}");
    result.as_array().cloned().unwrap_or_default()
}

fn declares(symbols: &[Value], name: &str, unit: &str) -> bool {
    symbols.iter().any(|symbol| {
        symbol["name"] == name
            && symbol["location"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.ends_with(&format!("/lib/{unit}.pas")))
    })
}

fn shutdown(mut server: TestServer) {
    server.shutdown();
}

#[test]
fn a_second_server_fills_the_index_from_the_store_without_extracting() {
    let environment = tempfile::tempdir().unwrap();
    let root = fixture(environment.path(), UNITS);
    let mut first = launch(environment.path());
    initialize_and_open(&mut first, &root, "App.dpr");
    let done = wait_index(&mut first, persisted);
    assert_eq!(done["members"], MEMBERS, "{done}");
    assert_eq!(done["extracted"], done["members"], "{done}");
    assert_eq!(done["reused"], 0, "{done}");
    assert_eq!(done["snapshot"], "sealed", "{done}");
    assert_nothing_refused(&done);
    shutdown(first);

    let mut second = launch(environment.path());
    initialize_and_open(&mut second, &root, "App.dpr");
    let done = wait_index(&mut second, complete);
    assert_eq!(done["members"], MEMBERS, "{done}");
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], done["members"], "{done}");
    assert_nothing_refused(&done);
    assert_eq!(done["snapshot"], "held", "{done}");
    let symbols = workspace_symbols(&mut second, "P7");
    assert!(declares(&symbols, "P7", "U7"), "{symbols:?}");
    shutdown(second);
}

#[test]
fn a_killed_server_resumes_from_published_members() {
    const PUBLISHED: u64 = 100;
    let environment = tempfile::tempdir().unwrap();
    let root = fixture(environment.path(), UNITS);
    let mut first = launch(environment.path());
    initialize_and_open(&mut first, &root, "App.dpr");
    let before = wait_index(&mut first, |context| {
        context["persisted"].as_u64().unwrap() >= PUBLISHED
    });
    first.kill();

    let mut second = launch(environment.path());
    initialize_and_open(&mut second, &root, "App.dpr");
    let done = wait_index(&mut second, complete);
    assert_eq!(done["members"], MEMBERS, "{done}");
    assert!(
        done["reused"].as_u64().unwrap() >= PUBLISHED,
        "{before} then {done}"
    );
    assert_eq!(
        done["reused"].as_u64().unwrap() + done["extracted"].as_u64().unwrap(),
        MEMBERS,
        "{done}"
    );
    assert_nothing_refused(&done);
    shutdown(second);
}

/// Units the first server's crawl analyses and publishes before it is held
/// at the warm-crawl barrier and killed.
const CRAWLED: u64 = 100;

/// A server whose warm crawl stops before its unit `CRAWLED`, every unit
/// before it analysed and published, until it is killed.
fn launch_held_crawl(environment: &Path) -> (TestServer, TestBarrier) {
    let directory = environment.join("warm-crawl-barrier");
    fs::create_dir_all(&directory).unwrap();
    let barrier = TestBarrier {
        entered: directory.join("entered"),
        release: directory.join("release"),
    };
    let spec = format!(
        "{}|{}|{CRAWLED}",
        barrier.entered.display(),
        barrier.release.display()
    );
    let cache_home = environment.join("cache");
    let server = TestServer::launch_test_server_with_environment_path_and_variables(
        environment,
        [
            ("XDG_CACHE_HOME", cache_home.to_str().expect("UTF-8 path")),
            ("PASCAL_LSP_TEST_WARM_CRAWL_BARRIER", spec.as_str()),
        ],
    );
    (server, barrier)
}

/// While the crawl is held, nothing was extracted: every context's
/// `extracted` is 0 (extraction batches follow the crawl).
fn assert_nothing_extracted(server: &mut TestServer) {
    let status = request(server, "pascal/indexStatus", json!({}));
    for context in status["contexts"].as_array().unwrap() {
        assert_eq!(context["extracted"], 0, "{status}");
    }
}

/// Opens `App.dpr` with an unsaved edit: the crawl's context then comes
/// from an unsaved project input and publishes nothing, while the index
/// batches read the saved files.
fn open_app_edited(server: &mut TestServer, root: &Path) {
    let app = root.join("App.dpr");
    let edited = format!("{}{{ unsaved }}\n", fs::read_to_string(&app).unwrap());
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&app), "languageId": "pascal", "version": 1, "text": edited}}),
    );
}

/// D42: the warm crawl publishes the disk units it analyses. The first
/// server is killed during its crawl of `App.dpr`, held after exactly
/// `CRAWLED` units and before any extraction batch (those follow the
/// crawl). The second opens `App.dpr` with an unsaved edit: its own crawl
/// then publishes nothing, while its index batches read the saved files, so
/// what it reuses is exactly what the first crawl published.
#[test]
fn a_server_killed_during_the_crawl_reuses_what_the_crawl_published() {
    let environment = tempfile::tempdir().unwrap();
    let root = project_fixture(environment.path(), UNITS, true, 0);
    let (mut first, barrier) = launch_held_crawl(environment.path());
    initialize_and_open(&mut first, &root, "App.dpr");
    barrier.wait_until_entered();
    assert_nothing_extracted(&mut first);
    let held = request(&mut first, "pascal/indexStatus", json!({}));
    assert!(
        held["contexts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|context| context["persisted"] == CRAWLED),
        "persisted counts the crawl's publications before any batch: {held}"
    );
    first.kill();

    let mut second = launch(environment.path());
    second.initialize(&root, Value::Null);
    open_app_edited(&mut second, &root);
    let done = wait_index(&mut second, complete);
    assert_eq!(done["members"], MEMBERS, "{done}");
    assert_eq!(done["reused"], CRAWLED, "{done}");
    assert_eq!(done["extracted"], MEMBERS - CRAWLED, "{done}");
    assert_nothing_refused(&done);
    shutdown(second);
}

/// I-2 (Task 10b): with `projectFile` configured, the crawl of the main
/// source and the project's anchor derive one context, so what a crawl
/// killed midway published is reused for the configured anchor.
#[test]
fn a_configured_project_reuses_what_a_killed_crawl_published() {
    let environment = tempfile::tempdir().unwrap();
    let root = project_fixture(environment.path(), UNITS, true, 0);
    // Absolute: the warmer anchors only an absolute configured project.
    let options = json!({"projectFile": root.join("App.dproj").display().to_string()});
    let (mut first, barrier) = launch_held_crawl(environment.path());
    first.initialize(&root, options.clone());
    open_disk(&mut first, &root.join("App.dpr"));
    barrier.wait_until_entered();
    assert_nothing_extracted(&mut first);
    first.kill();

    let mut second = launch(environment.path());
    second.initialize(&root, options);
    open_app_edited(&mut second, &root);
    let deadline = Instant::now() + INDEX_DEADLINE;
    let done = loop {
        let status = request(&mut second, "pascal/indexStatus", json!({}));
        if let Some(context) = app_context(&status).filter(complete) {
            break context;
        }
        assert!(Instant::now() < deadline, "{status}");
    };
    assert_eq!(done["members"], MEMBERS, "{done}");
    assert!(
        done["reused"].as_u64().unwrap() >= CRAWLED,
        "the configured anchor reuses the crawl's publications: {done}"
    );
    assert_eq!(
        done["reused"].as_u64().unwrap() + done["extracted"].as_u64().unwrap(),
        MEMBERS,
        "{done}"
    );
    assert_nothing_refused(&done);
    shutdown(second);
}

#[test]
fn two_servers_index_the_same_workspace() {
    let environment = tempfile::tempdir().unwrap();
    let root = fixture(environment.path(), UNITS);
    let mut one = launch(environment.path());
    let mut two = launch(environment.path());
    initialize_and_open(&mut one, &root, "App.dpr");
    initialize_and_open(&mut two, &root, "App.dpr");
    let mut finished = [None, None];
    let mut last = [Value::Null, Value::Null];
    let deadline = Instant::now() + INDEX_DEADLINE;
    while finished.iter().any(Option::is_none) {
        assert!(Instant::now() < deadline, "{finished:?} {last:?}");
        for ((server, finished), last) in [&mut one, &mut two]
            .into_iter()
            .zip(&mut finished)
            .zip(&mut last)
        {
            if finished.is_none() {
                *last = request(server, "pascal/indexStatus", json!({}));
                *finished = app_context(last).filter(persisted);
            }
        }
    }
    for done in finished.iter().flatten() {
        assert_eq!(done["members"], MEMBERS, "{done}");
        assert_eq!(
            done["reused"].as_u64().unwrap() + done["extracted"].as_u64().unwrap(),
            MEMBERS,
            "{done}"
        );
        assert_nothing_refused(done);
    }
    let symbols = workspace_symbols(&mut two, "P42");
    assert!(declares(&symbols, "P42", "U42"), "{symbols:?}");
    shutdown(one);
    shutdown(two);

    let mut third = launch(environment.path());
    initialize_and_open(&mut third, &root, "App.dpr");
    let done = wait_index(&mut third, complete);
    assert_eq!(done["members"], MEMBERS, "{done}");
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], MEMBERS, "{done}");
    assert_eq!(done["snapshot"], "held", "{done}");
    assert_nothing_refused(&done);
    shutdown(third);
}

#[test]
fn two_sessions_with_private_buffers_publish_only_disk_facts() {
    let environment = tempfile::tempdir().unwrap();
    let root = fixture(environment.path(), UNITS);
    let unit = root.join("lib/U3.pas");
    let unsaved = "unit U3;\ninterface\nprocedure Unsaved3;\nimplementation\nprocedure Unsaved3;\nbegin\nend;\nend.\n";
    let mut one = launch(environment.path());
    initialize_and_open(&mut one, &root, "App.dpr");
    one.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(&unit), "languageId": "pascal", "version": 1, "text": unsaved}}),
    );
    let done = wait_index(&mut one, persisted);
    assert_eq!(done["members"], MEMBERS, "{done}");
    assert_eq!(done["snapshot"], "sealed", "{done}");
    assert_nothing_refused(&done);
    // The session itself answers from its buffer.
    let symbols = workspace_symbols(&mut one, "Unsaved3");
    assert!(declares(&symbols, "Unsaved3", "U3"), "{symbols:?}");
    shutdown(one);

    let mut two = launch(environment.path());
    initialize_and_open(&mut two, &root, "App.dpr");
    let done = wait_index(&mut two, complete);
    assert_eq!(done["members"], MEMBERS, "{done}");
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], MEMBERS, "{done}");
    assert_eq!(done["snapshot"], "held", "{done}");
    assert_nothing_refused(&done);
    let symbols = workspace_symbols(&mut two, "Unsaved3");
    assert!(
        !symbols.iter().any(|symbol| symbol["name"] == "Unsaved3"),
        "{symbols:?}"
    );
    let symbols = workspace_symbols(&mut two, "P3");
    assert!(declares(&symbols, "P3", "U3"), "{symbols:?}");
    shutdown(two);
}
