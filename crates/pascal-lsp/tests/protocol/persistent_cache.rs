//! Spec §3, §11 and §12.1 (amendments 2026-11-03, `l4d-zf1b.11`): the
//! cache lifecycle through the protocol. `operations` is the process-wide
//! store-operation counter of the server.
#![cfg(unix)]

use super::*;
use pascal_cache::StoreLimits;
use pascal_cache::admin::{self, StoreStatus};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

const UNSAVED_MAIN: &str = "unit Main;\ninterface\nuses Provider;\nprocedure UnsavedRoutine;\nimplementation\nprocedure UnsavedRoutine;\nbegin\n  PublicRoutine;\nend;\nend.\n";

/// The store lands in `environment/cache`, which must not be inside the
/// workspace root.
pub(super) fn launch(environment: &Path) -> TestServer {
    let cache_home = environment.join("cache");
    TestServer::launch_test_server_with_environment_path_and_variables(
        environment,
        [("XDG_CACHE_HOME", cache_home.to_str().expect("UTF-8 path"))],
    )
}

fn store_directory(environment: &Path) -> PathBuf {
    environment.join("cache/lint4d/analysis")
}

fn rand_suffix() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(super) fn settle(server: &mut TestServer, step: bool) -> Value {
    let id = RequestId::from(format!("settle-{step}-{}", rand_suffix()));
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

pub(super) fn request(server: &mut TestServer, method: &str, params: Value) -> Value {
    let id = RequestId::from(format!("{method}-{}", rand_suffix()));
    server.send_request(id.clone(), method, params);
    let response = server.response(&id);
    assert!(response.error.is_none(), "{method}: {response:?}");
    response.result.unwrap_or(Value::Null)
}

fn hover(server: &mut TestServer, path: &Path, source: &str, needle: &str) -> Value {
    let result = request(
        server,
        "textDocument/hover",
        navigation_params(path, source, needle, 0),
    );
    assert!(
        result["contents"]["value"]
            .as_str()
            .is_some_and(|text| text.contains(needle)),
        "{result}"
    );
    result
}

fn definition(server: &mut TestServer, path: &Path, source: &str, needle: &str) -> Value {
    let result = request(
        server,
        "textDocument/definition",
        navigation_params(path, source, needle, 0),
    );
    assert!(!result.is_null(), "definition of {needle}");
    result
}

fn open(server: &mut TestServer, path: &Path, version: i32, text: &str) {
    server.send_notification(
        "textDocument/didOpen",
        json!({"textDocument": {"uri": uri(path), "languageId": "pascal", "version": version, "text": text}}),
    );
}

fn change(server: &mut TestServer, path: &Path, version: i32, text: &str) {
    server.send_notification(
        "textDocument/didChange",
        json!({
            "textDocument": {"uri": uri(path), "version": version},
            "contentChanges": [{"text": text}],
        }),
    );
}

fn watched_change(server: &mut TestServer, path: &Path) {
    server.send_notification(
        "workspace/didChangeWatchedFiles",
        json!({"changes": [{"uri": uri(path), "type": 2}]}),
    );
}

fn live_leases(directory: &Path) -> u64 {
    match admin::status(directory) {
        StoreStatus::Present(report) => report.live_leases,
        other => panic!("{other:?}"),
    }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

/// Names, sizes and modification times of every file under `directory`.
fn snapshot(directory: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut files: Vec<_> = walkdir::WalkDir::new(directory)
        .into_iter()
        .map(|entry| entry.unwrap())
        .map(|entry| {
            let metadata = entry.metadata().unwrap();
            (
                entry.path().to_path_buf(),
                metadata.len(),
                metadata.modified().unwrap(),
            )
        })
        .collect();
    files.sort();
    files
}

#[test]
fn cache_defaults_open_one_store_and_shutdown_releases_it() {
    let (_temp, main, _provider, main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let directory = store_directory(environment.path());
    let mut server = launch(environment.path());
    server.initialize(main.parent().unwrap(), Value::Null);
    let settled = settle(&mut server, false);
    let status = &settled["status"];
    assert_eq!(status["state"], "enabled", "{settled}");
    let reserved = status["reservedBytes"]["pages"].as_u64().unwrap()
        + status["reservedBytes"]["scratch"].as_u64().unwrap();
    assert_eq!(
        reserved,
        StoreLimits::default().admission_bytes(),
        "{status}"
    );
    assert_eq!(status["directory"], directory.to_str().unwrap());
    assert!(directory.join(pascal_cache::DATABASE_FILE).is_file());
    let mode = fs::metadata(&directory).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o700);
    open(&mut server, &main, 1, &main_source);
    hover(&mut server, &main, &main_source, "PublicRoutine");
    definition(&mut server, &main, &main_source, "PublicRoutine");
    assert_eq!(live_leases(&directory), 1);
    server.shutdown();
    assert_eq!(live_leases(&directory), 0);
}

/// An empty one-page SQLite database whose header says lint4d container 4.
fn container_header(version: u32) -> Vec<u8> {
    let mut page = vec![0u8; 4096];
    page[..16].copy_from_slice(b"SQLite format 3\0");
    page[16..18].copy_from_slice(&4096u16.to_be_bytes());
    page[18..24].copy_from_slice(&[1, 1, 0, 64, 32, 32]);
    page[24..28].copy_from_slice(&1u32.to_be_bytes());
    page[28..32].copy_from_slice(&1u32.to_be_bytes());
    page[44..48].copy_from_slice(&4u32.to_be_bytes());
    page[56..60].copy_from_slice(&1u32.to_be_bytes());
    page[60..64].copy_from_slice(&version.to_be_bytes());
    page[68..72].copy_from_slice(&0x4C34_4443u32.to_be_bytes());
    page[92..96].copy_from_slice(&1u32.to_be_bytes());
    page[96..100].copy_from_slice(&3_051_003u32.to_be_bytes());
    page[100] = 0x0D;
    page[105..107].copy_from_slice(&4096u16.to_be_bytes());
    page
}

/// R9-3 (l4d-zf1b.8, 13c-4, 13d-3; l4d-zf1b.9): containers of earlier versions keep
/// their own file names. The server opens its versioned file beside it, with no
/// user action, and never touches or deletes the old one.
#[test]
fn an_earlier_container_beside_is_left_and_the_versioned_store_opened() {
    let (_temp, main, _provider, main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let directory = store_directory(environment.path());
    fs::create_dir_all(&directory).unwrap();
    fs::set_permissions(
        environment.path().join("cache"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::set_permissions(
        environment.path().join("cache/lint4d"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    let olds = [
        (directory.join("analysis.sqlite3"), 4),
        (directory.join("analysis-v5.sqlite3"), 5),
        (directory.join("analysis-v6.sqlite3"), 6),
    ];
    let stamp = |old: &std::path::Path| {
        (
            fs::read(old).unwrap(),
            fs::metadata(old).unwrap().modified().unwrap(),
        )
    };
    let mut before = Vec::new();
    for (old, version) in &olds {
        fs::write(old, container_header(*version)).unwrap();
        before.push(stamp(old));
    }
    let mut server = launch(environment.path());
    server.initialize(main.parent().unwrap(), Value::Null);
    let settled = settle(&mut server, true);
    assert_eq!(settled["status"]["state"], "enabled", "{settled}");
    assert_eq!(pascal_cache::DATABASE_FILE, "analysis-v7.sqlite3");
    assert!(directory.join(pascal_cache::DATABASE_FILE).is_file());
    open(&mut server, &main, 1, &main_source);
    hover(&mut server, &main, &main_source, "PublicRoutine");
    server.shutdown();
    for ((old, _), before) in olds.iter().zip(&before) {
        assert_eq!(&stamp(old), before, "{}", old.display());
        let mut wal = old.as_os_str().to_owned();
        wal.push("-wal");
        assert!(!std::path::Path::new(&wal).exists(), "{}", old.display());
    }
}

#[test]
fn disabled_at_initialization_creates_nothing_and_counts_no_store_operation() {
    let (_temp, main, provider, main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let mut server = launch(environment.path());
    server.initialize(
        main.parent().unwrap(),
        json!({"persistentCacheEnabled": false}),
    );
    open(&mut server, &main, 1, &main_source);
    change(&mut server, &main, 2, UNSAVED_MAIN);
    hover(&mut server, &main, UNSAVED_MAIN, "UnsavedRoutine");
    definition(&mut server, &main, UNSAVED_MAIN, "PublicRoutine");
    watched_change(&mut server, &provider);
    let settled = settle(&mut server, true);
    assert_eq!(settled["status"]["state"], "disabled", "{settled}");
    assert_eq!(settled["operations"], 0, "{settled}");
    assert_eq!(settled["stepped"], Value::Null, "{settled}");
    server.shutdown();
    assert!(!environment.path().join("cache").exists());
}

#[test]
fn invalid_cache_options_fall_back_to_memory_only() {
    let (_temp, main, _provider, main_source, _provider_source) = standard_workspace();
    let root = main.parent().unwrap();
    let expected = {
        let environment = tempfile::tempdir().unwrap();
        let mut server = launch(environment.path());
        server.initialize(root, Value::Null);
        assert_eq!(settle(&mut server, false)["status"]["state"], "enabled");
        open(&mut server, &main, 1, &main_source);
        let expected = hover(&mut server, &main, &main_source, "PublicRoutine");
        server.shutdown();
        expected
    };
    for (options, name) in [
        (
            json!({"persistentCachePageBytes": 1}),
            "persistentCachePageBytes",
        ),
        (
            json!({"persistentCacheScratchBytes": 1}),
            "persistentCacheScratchBytes",
        ),
        (
            json!({"persistentCacheDirectory": "relative"}),
            "persistentCacheDirectory",
        ),
        (
            json!({"persistentCacheEnabled": "yes"}),
            "persistentCacheEnabled",
        ),
    ] {
        let environment = tempfile::tempdir().unwrap();
        let mut server = launch(environment.path());
        server.initialize(root, options.clone());
        let settled = settle(&mut server, false);
        let status = &settled["status"];
        assert_eq!(status["state"], "bypassed", "{options}: {settled}");
        assert_eq!(status["cause"]["kind"], "configuration", "{settled}");
        assert!(
            status["cause"]["message"]
                .as_str()
                .is_some_and(|message| message.contains(name)),
            "{settled}"
        );
        assert_eq!(settled["operations"], 0, "{options}: {settled}");
        open(&mut server, &main, 1, &main_source);
        assert_eq!(
            hover(&mut server, &main, &main_source, "PublicRoutine"),
            expected
        );
        server.shutdown();
        assert!(!environment.path().join("cache").exists(), "{options}");
    }
}

#[test]
fn a_non_private_cache_directory_is_bypassed_and_left_untouched() {
    if is_root() {
        return;
    }
    let (_temp, main, _provider, main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let shared = environment.path().join("shared");
    fs::create_dir(&shared).unwrap();
    fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
    let mut server = launch(environment.path());
    server.initialize(
        main.parent().unwrap(),
        json!({"persistentCacheDirectory": shared}),
    );
    let settled = settle(&mut server, false);
    let cause = &settled["status"]["cause"];
    assert_eq!(settled["status"]["state"], "bypassed", "{settled}");
    assert_eq!(cause["kind"], "location", "{settled}");
    assert_eq!(cause["location"], "notPrivate", "{settled}");
    assert_eq!(cause["advice"], "freshDirectory", "{settled}");
    open(&mut server, &main, 1, &main_source);
    hover(&mut server, &main, &main_source, "PublicRoutine");
    server.shutdown();
    assert_eq!(fs::read_dir(&shared).unwrap().count(), 0);
    let mode = fs::metadata(&shared).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
}

#[test]
fn runtime_disable_is_acknowledged_then_performs_no_cache_io() {
    let (_temp, _main, _provider, main_source, provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let directory = store_directory(environment.path());
    // The 300-unit project, so that an index batch runs when the disable
    // arrives; `Main` and `Provider` are members too.
    let root = super::persistent_index::fixture(environment.path(), super::persistent_index::UNITS);
    let main = root.join("lib/Main.pas");
    let provider = root.join("lib/Provider.pas");
    let app = root.join("App.dpr");
    write_file(&main, &main_source);
    write_file(&provider, &provider_source);
    pascal_project::backdate_tree_for_test(&root);
    let mut server = launch(environment.path());
    server.initialize(&root, Value::Null);
    open(&mut server, &app, 1, &fs::read_to_string(&app).unwrap());
    open(&mut server, &main, 1, &main_source);
    change(&mut server, &main, 2, UNSAVED_MAIN);
    assert_eq!(settle(&mut server, false)["status"]["state"], "enabled");
    let running = super::persistent_index::wait_index(&mut server, |context| {
        context["members"].as_u64().unwrap() > 0
    });
    assert_eq!(running["complete"], false, "{running}");
    let hover_before = hover(&mut server, &main, UNSAVED_MAIN, "UnsavedRoutine");

    configure(&mut server, json!({"persistentCacheEnabled": false}));
    let acknowledged = settle(&mut server, false);
    let status = &acknowledged["status"];
    assert_eq!(status["state"], "disabled", "{acknowledged}");
    assert_eq!(status["store"], Value::Null, "{acknowledged}");
    assert_eq!(status["reservedBytes"]["pages"], 0, "{acknowledged}");
    let operations = acknowledged["operations"].as_u64().unwrap();
    let files = snapshot(&directory);

    // Editor state survived the transition: the unsaved buffer still answers.
    change(&mut server, &main, 3, UNSAVED_MAIN);
    assert_eq!(
        hover(&mut server, &main, UNSAVED_MAIN, "UnsavedRoutine"),
        hover_before
    );
    // The batch that was running completes memory-only.
    let done =
        super::persistent_index::wait_index(&mut server, |context| context["complete"] == true);
    assert_eq!(done["reused"], 0, "{done}");
    assert_eq!(
        done["members"].as_u64().unwrap(),
        super::persistent_index::MEMBERS + 2,
        "{done}"
    );
    assert_eq!(
        settle(&mut server, false)["operations"].as_u64().unwrap(),
        operations
    );
    definition(&mut server, &main, UNSAVED_MAIN, "PublicRoutine");
    request(
        &mut server,
        "textDocument/documentSymbol",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    watched_change(&mut server, &provider);
    server.send_notification(
        "textDocument/didSave",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    server.send_notification(
        "textDocument/didClose",
        json!({"textDocument": {"uri": uri(&main)}}),
    );
    open(&mut server, &main, 4, UNSAVED_MAIN);
    assert_eq!(
        hover(&mut server, &main, UNSAVED_MAIN, "UnsavedRoutine"),
        hover_before
    );

    let after = settle(&mut server, true);
    assert_eq!(after["operations"].as_u64().unwrap(), operations, "{after}");
    assert_eq!(after["stepped"], Value::Null, "{after}");
    assert_eq!(snapshot(&directory), files);
    assert_eq!(live_leases(&directory), 0);

    configure(&mut server, json!({"persistentCacheEnabled": true}));
    let reenabled = settle(&mut server, true);
    assert_eq!(reenabled["status"]["state"], "enabled", "{reenabled}");
    assert!(
        reenabled["operations"].as_u64().unwrap() > operations,
        "{reenabled}"
    );
    assert_eq!(reenabled["stepped"], true, "{reenabled}");
    server.shutdown();
}

#[test]
fn a_runtime_budget_change_reopens_the_store_under_the_new_limits() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let directory = store_directory(environment.path());
    let mut server = launch(environment.path());
    server.initialize(main.parent().unwrap(), Value::Null);
    let before = settle(&mut server, false);
    assert_eq!(before["status"]["state"], "enabled", "{before}");
    let generation = before["status"]["generation"].as_u64().unwrap();
    configure(&mut server, json!({"persistentCacheScratchBytes": 2097152}));
    let after = settle(&mut server, false);
    let status = &after["status"];
    assert_eq!(status["state"], "enabled", "{after}");
    assert!(
        status["generation"].as_u64().unwrap() > generation,
        "{after}"
    );
    assert_eq!(status["limits"]["scratchBytes"], 2097152, "{after}");
    assert_eq!(status["reservedBytes"]["scratch"], 2097152, "{after}");
    assert_eq!(live_leases(&directory), 1);
    server.shutdown();
}

#[test]
fn a_runtime_directory_change_is_ignored() {
    let (_temp, main, _provider, _main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let other = environment.path().join("other");
    let mut server = launch(environment.path());
    server.initialize(main.parent().unwrap(), Value::Null);
    let before = settle(&mut server, false);
    assert_eq!(before["status"]["state"], "enabled", "{before}");
    configure(&mut server, json!({"persistentCacheDirectory": other}));
    let after = settle(&mut server, false);
    assert_eq!(after["status"]["state"], "enabled", "{after}");
    assert_eq!(after["status"]["directory"], before["status"]["directory"]);
    assert_eq!(
        after["status"]["generation"],
        before["status"]["generation"]
    );
    assert!(!other.exists());
    server.shutdown();
}

#[test]
fn an_administrative_reset_fences_the_server_which_reopens() {
    let (_temp, main, _provider, main_source, _provider_source) = standard_workspace();
    let environment = tempfile::tempdir().unwrap();
    let directory = store_directory(environment.path());
    let mut server = launch(environment.path());
    server.initialize(main.parent().unwrap(), Value::Null);
    let before = settle(&mut server, false);
    assert_eq!(before["status"]["state"], "enabled", "{before}");
    assert_eq!(before["status"]["resetEpoch"], 0, "{before}");
    let output = Command::new(env!("CARGO_BIN_EXE_pascal-lsp"))
        .args(["--cache-reset", "--cache-directory"])
        .arg(&directory)
        .env("HOME", environment.path().join("home"))
        .env("XDG_CACHE_HOME", environment.path().join("cache"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    // A step observes `Fenced`: this one (`stepped` false) or the
    // scheduled one, whose first step after the open may run before or
    // after the reset (then this one steps the reopened store). Either way
    // the next settle sees the reopened store, fenced once.
    let stepped = settle(&mut server, true);
    assert!(stepped["stepped"].is_boolean(), "{stepped}");
    let after = settle(&mut server, false);
    let status = &after["status"];
    assert_eq!(status["state"], "enabled", "{after}");
    assert_eq!(status["resetEpoch"], 1, "{after}");
    assert_eq!(status["counters"]["bypasses"]["fenced"], 1, "{after}");
    open(&mut server, &main, 1, &main_source);
    hover(&mut server, &main, &main_source, "PublicRoutine");
    server.shutdown();
}
