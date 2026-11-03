//! `l4d-vrw0`: a persistent-cache store inside a workspace root (the
//! workspace is `$HOME`, the store `~/.cache/lint4d/analysis`) is not part
//! of the workspace. The store's own writes and file events must never look
//! like workspace changes.
#![cfg(unix)]

use super::*;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const MAIN_SOURCE: &str = "unit Main;\ninterface\nuses Provider;\nimplementation\nprocedure Run;\nbegin\n  PublicRoutine;\nend;\nend.\n";
const PROVIDER_SOURCE: &str = "unit Provider;\ninterface\nprocedure PublicRoutine;\nimplementation\nprocedure PublicRoutine;\nbegin\nend;\nend.\n";

fn next_id(prefix: &str) -> RequestId {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    RequestId::from(format!("{prefix}-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
}

fn settle(server: &mut TestServer, step: bool) -> Value {
    let id = next_id("settle");
    server.send_request(id.clone(), "pascal/testCacheSettle", json!({"step": step}));
    let response = server.response(&id);
    assert!(response.error.is_none(), "{response:?}");
    response.result.unwrap()
}

fn configure(server: &mut TestServer, settings: Value) {
    server.send_notification(
        "workspace/didChangeConfiguration",
        json!({"settings": {"pascalLsp": settings}}),
    );
}

/// A workspace root (standing for `$HOME`) holding `Main.pas`,
/// `Provider.pas`, four units with a lint finding each, and the cache home
/// `cache`.
struct RootWithCache {
    _temp: tempfile::TempDir,
    environment: PathBuf,
    root: PathBuf,
    main: PathBuf,
}

impl RootWithCache {
    /// The store directory already exists, as after a first run.
    fn new() -> Self {
        let fixture = Self::first_run();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(fixture.store())
            .unwrap();
        fixture
    }

    /// Neither the cache home nor the store exist yet.
    fn first_run() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let environment = temp.path().join("environment");
        let root = temp.path().join("home");
        fs::create_dir_all(&environment).unwrap();
        let main = root.join("Main.pas");
        write_file(&main, MAIN_SOURCE);
        write_file(&root.join("Provider.pas"), PROVIDER_SOURCE);
        for index in 0..4 {
            write_file(
                &root.join(format!("U{index:02}.pas")),
                &format!(
                    "unit U{index:02};\ninterface\nconst badConst = 1;\nimplementation\nend.\n"
                ),
            );
        }
        Self {
            _temp: temp,
            environment,
            root,
            main,
        }
    }

    fn cache_home(&self) -> PathBuf {
        self.root.join("cache")
    }

    fn store(&self) -> PathBuf {
        self.cache_home().join("lint4d/analysis")
    }

    fn launch(&self) -> TestServer {
        let cache_home = self.cache_home();
        TestServer::launch_test_server_with_environment_path_and_variables(
            &self.environment,
            [("XDG_CACHE_HOME", cache_home.to_str().unwrap())],
        )
    }
}

fn open(server: &mut TestServer, path: &Path, text: &str) {
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(path), "languageId": "pascal", "version": 1, "text": text}}),
    );
}

fn assert_success(server: &mut TestServer, id: &RequestId, what: &str) -> Value {
    let response = server.response(id);
    assert!(response.error.is_none(), "{what}: {response:?}");
    response.result.unwrap_or(Value::Null)
}

fn workspace_diagnostic(server: &mut TestServer) -> Value {
    let id = next_id("workspace-diagnostic");
    let token = format!("{id}-partial");
    server.send_request(
        id.clone(),
        "workspace/diagnostic",
        json!({"previousResultIds": [], "partialResultToken": token}),
    );
    let result = assert_success(server, &id, "workspace/diagnostic");
    let _ = server.take_partial_items(&token);
    result
}

fn definition_request(server: &mut TestServer, path: &Path) -> RequestId {
    let id = next_id("definition");
    server.send_request(
        id.clone(),
        "textDocument/definition",
        navigation_params(path, MAIN_SOURCE, "PublicRoutine", 0),
    );
    id
}

fn references_request(server: &mut TestServer, path: &Path) -> RequestId {
    let id = next_id("references");
    let mut params = navigation_params(path, MAIN_SOURCE, "PublicRoutine", 0);
    params["context"] = json!({"includeDeclaration": true});
    server.send_request(id.clone(), "textDocument/references", params);
    id
}

fn workspace_symbol_request(server: &mut TestServer) -> RequestId {
    let id = next_id("symbols");
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "PublicRoutine"}),
    );
    id
}

/// Watched-file events for the store's own files, as a client watching
/// `**/*` reports them.
fn store_file_events(server: &mut TestServer, store: &Path) {
    let changes: Vec<_> = [
        ("analysis.sqlite3", 2),
        ("analysis.sqlite3-wal", 1),
        ("analysis.sqlite3-shm", 1),
        ("maintenance.lock", 1),
        ("namespaces", 1),
        ("namespaces/0123.lock", 3),
    ]
    .iter()
    .map(|(name, kind)| json!({"uri": uri(&store.join(name)), "type": kind}))
    .collect();
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": changes}),
    );
}

/// Whether the server asks the client to pull diagnostics again: what a
/// watched change of a workspace file does.
fn refreshes(server: &mut TestServer) -> bool {
    let Some(refresh) =
        server.request_with_timeout("workspace/diagnostic/refresh", Duration::from_millis(500))
    else {
        return false;
    };
    server.send(Message::Response(Response::new_ok(refresh.id, Value::Null)));
    true
}

#[test]
fn workspace_diagnostics_never_retry_while_a_store_inside_the_root_writes() {
    let fixture = RootWithCache::new();
    let mut server = fixture.launch();
    server.initialize_with_pull_diagnostics_options(&fixture.root, Value::Null);
    // The first request races the store's open.
    workspace_diagnostic(&mut server);
    for round in 1..4 {
        assert_eq!(settle(&mut server, true)["stepped"], true, "round {round}");
        workspace_diagnostic(&mut server);
    }
    assert_eq!(settle(&mut server, false)["status"]["state"], "enabled");
    server.shutdown();
}

#[test]
fn navigation_and_workspace_queries_never_retry_while_a_store_inside_the_root_writes() {
    let fixture = RootWithCache::new();
    let mut server = fixture.launch();
    server.initialize(&fixture.root, Value::Null);
    open(&mut server, &fixture.main, MAIN_SOURCE);
    for round in 0..4 {
        let definition = definition_request(&mut server, &fixture.main);
        let references = references_request(&mut server, &fixture.main);
        let symbols = workspace_symbol_request(&mut server);
        // Maintenance writes to the store while the requests run.
        assert_eq!(settle(&mut server, true)["stepped"], true, "round {round}");
        let result = assert_success(&mut server, &definition, "definition");
        assert!(!result.is_null(), "round {round}");
        assert_success(&mut server, &references, "references");
        let symbols = assert_success(&mut server, &symbols, "workspace/symbol");
        assert!(
            symbols
                .as_array()
                .is_some_and(|symbols| !symbols.is_empty()),
            "{symbols}"
        );
    }
    server.shutdown();
}

#[test]
fn watched_events_under_the_store_invalidate_nothing() {
    let fixture = RootWithCache::new();
    let mut server = fixture.launch();
    server.initialize_with_pull_diagnostics_options(&fixture.root, Value::Null);
    assert_eq!(settle(&mut server, false)["status"]["state"], "enabled");
    workspace_diagnostic(&mut server);
    while refreshes(&mut server) {}

    store_file_events(&mut server, &fixture.store());
    assert!(!refreshes(&mut server), "a store file event refreshed");
    workspace_diagnostic(&mut server);

    // Control: a workspace file event still refreshes.
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&fixture.main), "type": 2}]}),
    );
    assert!(refreshes(&mut server), "a source event did not refresh");
    server.shutdown();
}

#[test]
fn reconfiguration_updates_the_store_exclusion() {
    let fixture = RootWithCache::new();
    let mut server = fixture.launch();
    server.initialize_with_pull_diagnostics_options(
        &fixture.root,
        json!({"persistentCacheEnabled": false}),
    );
    assert_eq!(settle(&mut server, false)["status"]["state"], "disabled");
    workspace_diagnostic(&mut server);
    while refreshes(&mut server) {}
    // Persistence off: the directory is an ordinary workspace directory.
    store_file_events(&mut server, &fixture.store());
    assert!(
        refreshes(&mut server),
        "disabled: the directory was excluded"
    );
    while refreshes(&mut server) {}

    configure(&mut server, json!({"persistentCacheEnabled": true}));
    assert_eq!(settle(&mut server, true)["status"]["state"], "enabled");
    while refreshes(&mut server) {}
    workspace_diagnostic(&mut server);
    store_file_events(&mut server, &fixture.store());
    assert!(
        !refreshes(&mut server),
        "enabled: a store file event refreshed"
    );
    assert_eq!(settle(&mut server, true)["stepped"], true);
    workspace_diagnostic(&mut server);

    // A runtime directory is ignored: the store and its exclusion stay.
    let elsewhere = fixture.root.join("elsewhere");
    configure(
        &mut server,
        json!({"persistentCacheEnabled": true, "persistentCacheDirectory": elsewhere}),
    );
    let status = settle(&mut server, true);
    assert_eq!(
        status["status"]["directory"],
        fixture.store().to_str().unwrap()
    );
    while refreshes(&mut server) {}
    store_file_events(&mut server, &fixture.store());
    assert!(
        !refreshes(&mut server),
        "a runtime directory moved the exclusion"
    );
    workspace_diagnostic(&mut server);

    configure(&mut server, json!({"persistentCacheEnabled": false}));
    assert_eq!(settle(&mut server, false)["status"]["state"], "disabled");
    while refreshes(&mut server) {}
    store_file_events(&mut server, &fixture.store());
    assert!(refreshes(&mut server), "disabled again: still excluded");
    server.shutdown();
}

#[test]
fn a_cache_directory_holding_pascal_sources_is_refused_not_hidden() {
    let fixture = RootWithCache::new();
    // The configured directory is a source directory of the workspace.
    let sources = fixture.root.join("sources");
    fs::DirBuilder::new().mode(0o700).create(&sources).unwrap();
    let hidden = sources.join("Hidden.pas");
    let hidden_source = "unit Hidden;\ninterface\nprocedure HiddenRoutine;\nimplementation\nprocedure HiddenRoutine;\nbegin\nend;\nend.\n";
    write_file(&hidden, hidden_source);
    let mut server = fixture.launch();
    server.initialize_with_pull_diagnostics_options(
        &fixture.root,
        json!({"persistentCacheDirectory": sources}),
    );
    let settled = settle(&mut server, false);
    let status = &settled["status"];
    assert_eq!(status["state"], "bypassed", "{settled}");
    assert_eq!(status["cause"]["kind"], "configuration", "{settled}");
    let message = status["cause"]["message"].as_str().unwrap();
    assert!(message.contains("persistentCacheDirectory"), "{message}");
    assert!(message.contains("Pascal sources"), "{message}");
    assert!(message.contains("used only by the cache"), "{message}");
    assert_eq!(settled["operations"], 0, "{settled}");

    // Nothing was created there, and its sources stay in the workspace.
    let mut names: Vec<_> = fs::read_dir(&sources)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    assert_eq!(names, ["Hidden.pas"]);
    let id = next_id("symbols");
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "HiddenRoutine"}),
    );
    let symbols = assert_success(&mut server, &id, "workspace/symbol");
    assert!(
        symbols.as_array().is_some_and(|symbols| symbols
            .iter()
            .any(|symbol| symbol["location"]["uri"] == uri(&hidden).as_str())),
        "{symbols}"
    );
    while refreshes(&mut server) {}
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&hidden), "type": 2}]}),
    );
    assert!(refreshes(&mut server), "a refused directory was excluded");

    // Enabling it again at runtime is refused again.
    configure(&mut server, json!({"persistentCacheEnabled": false}));
    assert_eq!(settle(&mut server, false)["status"]["state"], "disabled");
    configure(&mut server, json!({"persistentCacheEnabled": true}));
    let settled = settle(&mut server, false);
    assert_eq!(settled["status"]["state"], "bypassed", "{settled}");
    assert_eq!(settled["status"]["cause"]["kind"], "configuration");
    assert_eq!(fs::read_dir(&sources).unwrap().count(), 1);
    server.shutdown();
}

/// Fix round 1: on a first run the server creates the store directory and
/// its parents before it answers anything, so even the first request never
/// sees the workspace change under it.
#[test]
fn the_first_request_of_a_first_run_never_retries() {
    for attempt in 0..3 {
        let fixture = RootWithCache::first_run();
        let mut server = fixture.launch();
        server.initialize_with_pull_diagnostics_options(&fixture.root, Value::Null);
        workspace_diagnostic(&mut server);
        assert!(fixture.store().is_dir(), "attempt {attempt}");
        assert_eq!(settle(&mut server, true)["stepped"], true);
        workspace_diagnostic(&mut server);
        server.shutdown();
    }
}

/// Fix round 1: sources must never be hidden. A Pascal source that turns up
/// under the excluded store directory refuses it instead of being dropped.
#[test]
fn a_source_appearing_in_the_store_refuses_it() {
    let fixture = RootWithCache::new();
    let mut server = fixture.launch();
    server.initialize_with_pull_diagnostics_options(&fixture.root, Value::Null);
    assert_eq!(settle(&mut server, false)["status"]["state"], "enabled");
    workspace_diagnostic(&mut server);
    while refreshes(&mut server) {}

    let stray = fixture.store().join("Stray.pas");
    write_file(
        &stray,
        "unit Stray;\ninterface\nprocedure StrayRoutine;\nimplementation\nprocedure StrayRoutine;\nbegin\nend;\nend.\n",
    );
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(&stray), "type": 1}]}),
    );
    assert!(refreshes(&mut server), "the source event was dropped");
    let settled = settle(&mut server, false);
    assert_eq!(settled["status"]["state"], "bypassed", "{settled}");
    assert_eq!(
        settled["status"]["cause"]["kind"], "configuration",
        "{settled}"
    );
    let message = settled["status"]["cause"]["message"].as_str().unwrap();
    assert!(message.contains("Stray.pas"), "{message}");
    let id = next_id("symbols");
    server.send_request(
        id.clone(),
        "workspace/symbol",
        json!({"query": "StrayRoutine"}),
    );
    let symbols = assert_success(&mut server, &id, "workspace/symbol");
    assert!(
        symbols.as_array().is_some_and(|symbols| symbols
            .iter()
            .any(|symbol| symbol["location"]["uri"] == uri(&stray).as_str())),
        "{symbols}"
    );
    server.shutdown();
}
