//! Spec §12.1 (`l4d-zf1b.12`): several server processes over one store,
//! with the test server's store fault points (`PASCAL_LSP_TEST_STORE_FAULT`)
//! and growth limit (`PASCAL_LSP_TEST_STORE_GROWTH_PAGES`,
//! `pascal/testStoreGrowth`).
#![cfg(unix)]

use super::persistent_cache::{request, settle, watched_change};
use super::persistent_index::{
    MEMBERS, UNITS, complete, fixture, initialize_and_open, persisted, shutdown, wait_index,
};
use super::persistent_queries::*;
use super::*;
use std::path::PathBuf;

/// A fault the test server arms on its store: the barrier files it holds
/// at, and the `PASCAL_LSP_TEST_STORE_FAULT` value.
pub(super) struct StoreFault {
    pub(super) barrier: TestBarrier,
    pub(super) spec: String,
}

impl StoreFault {
    /// `point` is a `FaultPoint` name, `action` `hold` or `abort`, `nth` the
    /// arrival acted on (earlier ones pass).
    pub(super) fn new(environment: &Path, point: &str, action: &str, nth: usize) -> Self {
        let directory = environment.join(format!("store-fault-{point}-{action}-{nth}"));
        fs::create_dir_all(&directory).unwrap();
        let barrier = TestBarrier {
            entered: directory.join("entered"),
            release: directory.join("release"),
        };
        let spec = format!(
            "{point}|{}|{}|{action}|{nth}",
            barrier.entered.display(),
            barrier.release.display()
        );
        Self { barrier, spec }
    }
}

/// A server over `environment/<cache>` armed with `fault` (the value of
/// `PASCAL_LSP_TEST_STORE_FAULT`), plus the `extra` variables.
pub(super) fn launch_with_store_fault(
    environment: &Path,
    cache: &str,
    label: &str,
    fault: &str,
    extra: &[(&str, &str)],
) -> TestServer {
    let cache_home = environment.join(cache);
    let trace = std::env::var("PASCAL_LSP_TRACE")
        .ok()
        .map(|path| format!("{path}.{label}"));
    let mut variables = vec![
        ("XDG_CACHE_HOME", cache_home.to_str().expect("UTF-8 path")),
        ("PASCAL_LSP_ANALYSIS_METRICS", "1"),
        ("PASCAL_LSP_TEST_STORE_FAULT", fault),
    ];
    if let Some(trace) = &trace {
        variables.push(("PASCAL_LSP_TRACE", trace.as_str()));
    }
    variables.extend_from_slice(extra);
    TestServer::launch_test_server_with_environment_path_and_variables(environment, variables)
}

/// Server A is held inside the transaction of its third record
/// publication. Server B, started meanwhile on the same workspace, is
/// refused the store as busy (its `pascal/cacheStatus` says so), so it
/// reuses nothing and extracts every member: the held record, like the two
/// committed ones, is not read by a process that has no store. After the
/// release both finish and a fourth server reuses every member.
#[test]
fn a_held_record_publication_is_visible_to_no_other_process() {
    let environment = tempfile::tempdir().unwrap();
    let root = fixture(environment.path(), UNITS);
    let fault = StoreFault::new(environment.path(), "RecordBeforeCommit", "hold", 3);
    let mut held = launch_with_store_fault(environment.path(), "cache", "held", &fault.spec, &[]);
    initialize_and_open(&mut held, &root, "App.dpr");
    fault.barrier.wait_until_entered();

    let mut other = super::persistent_cache::launch(environment.path());
    initialize_and_open(&mut other, &root, "App.dpr");
    let done = wait_index(&mut other, complete);
    assert_eq!(done["members"], MEMBERS, "{done}");
    // While A holds the write transaction, B's store open is refused as
    // busy and B runs memory-only: it never sees the held record.
    let status = cache_status(&mut other);
    assert_eq!(status["state"], "bypassed", "{status}");
    assert_eq!(status["cause"]["kind"], "busy", "{status}");
    assert_eq!(done["reused"], 0, "{done}");
    assert_eq!(
        done["extracted"].as_u64().unwrap() + done["pending"].as_u64().unwrap(),
        MEMBERS,
        "{done}"
    );

    fault.barrier.release();
    let finished = wait_index(&mut held, persisted);
    assert_eq!(finished["members"], MEMBERS, "{finished}");
    shutdown(held);
    shutdown(other);

    let mut fourth = super::persistent_cache::launch(environment.path());
    initialize_and_open(&mut fourth, &root, "App.dpr");
    let done = wait_index(&mut fourth, complete);
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], MEMBERS, "{done}");
    shutdown(fourth);
}

/// `pascal/testStoreGrowth` limits the open store and says so; the server
/// started with `PASCAL_LSP_TEST_STORE_GROWTH_PAGES` armed its store alike.
#[test]
fn the_growth_request_applies_to_the_open_store() {
    let environment = tempfile::tempdir().unwrap();
    let root = fixture(environment.path(), 2);
    let mut server = super::persistent_cache::launch(environment.path());
    server.initialize(&root, Value::Null);
    settle(&mut server, false);
    let reply = request(
        &mut server,
        "pascal/testStoreGrowth",
        json!({"extraPages": 100000}),
    );
    assert_eq!(reply["applied"], true, "{reply}");
    shutdown(server);
}

fn cache_status(server: &mut TestServer) -> Value {
    request(server, "pascal/cacheStatus", json!({}))
}

/// A server over `environment/cache` with the `extra` variables and no
/// store fault.
fn launch_with(environment: &Path, extra: &[(&str, &str)]) -> TestServer {
    let cache_home = environment.join("cache");
    let mut variables = vec![
        ("XDG_CACHE_HOME", cache_home.to_str().expect("UTF-8 path")),
        ("PASCAL_LSP_ANALYSIS_METRICS", "1"),
    ];
    variables.extend_from_slice(extra);
    TestServer::launch_test_server_with_environment_path_and_variables(environment, variables)
}

/// The server's `coreParserInvocations` right now (no settling).
fn parses(server: &mut TestServer) -> u64 {
    let result = request(server, "pascal/testAnalysisMetrics", json!({}));
    result["counters"]["coreParserInvocations"]
        .as_u64()
        .unwrap()
}

fn started(server: &mut TestServer, root: &Path, opens: &[&str]) {
    server.initialize(root, Value::Null);
    open_disk(server, root, opens);
}

/// The context of `App.dpr` reached `persisted` and every member is
/// accounted for as reused or extracted, none refused.
fn assert_accounted(context: &Value) {
    let members = context["members"].as_u64().unwrap();
    let reused = context["reused"].as_u64().unwrap();
    let extracted = context["extracted"].as_u64().unwrap();
    assert_eq!(reused + extracted, members, "{context}");
    assert_eq!(context["refused"], 0, "{context}");
    assert_eq!(context["limited"], 0, "{context}");
    assert_eq!(context["pending"], 0, "{context}");
}

/// Waits for the child of a server armed with an `abort` fault to die and
/// asserts that it was `abort()` that ended it.
fn wait_for_abort(server: &mut TestServer) {
    use std::os::unix::process::ExitStatusExt;
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the server never reached its fault point"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.signal(), Some(libc::SIGABRT), "{status}");
}

const OTHER_NAME: FixtureSpec = FixtureSpec {
    shared_name: "TAlien",
    ..MATRIX
};

/// A on the workspace holds at the first membership batch of its snapshot
/// while B, whose store opened before the hold, indexes the same
/// workspace: A has published every record, so B reuses them without a
/// snapshot (its seal waits for the writer). After the release both
/// processes finish with a sealed or held snapshot, and a third server
/// reuses every member and answers like an uncached one.
#[test]
fn concurrent_publishers_converge() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let mut other = launch_labelled(environment.path(), "cache", "b");
    other.initialize(&root, Value::Null);
    settle(&mut other, false);
    let fault = StoreFault::new(environment.path(), "MembersBeforeCommit", "hold", 1);
    let mut held = launch_with_store_fault(environment.path(), "cache", "a", &fault.spec, &[]);
    started(&mut held, &root, &OPENS);
    fault.barrier.wait_until_entered();

    open_disk(&mut other, &root, &OPENS);
    let meanwhile = wait_index(&mut other, complete);
    assert!(
        meanwhile["reused"].as_u64().unwrap() > 0,
        "B reuses A's committed records: {meanwhile}"
    );
    assert_ne!(meanwhile["snapshot"], "sealed", "{meanwhile}");

    fault.barrier.release();
    for server in [&mut held, &mut other] {
        let done = wait_index(server, persisted);
        assert_eq!(done["complete"], true, "{done}");
        assert!(
            done["snapshot"] == "sealed" || done["snapshot"] == "held",
            "{done}"
        );
        assert_eq!(done["refused"], 0, "{done}");
    }
    shutdown(held);
    shutdown(other);

    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];
    let mut third = cached_server(environment.path(), &root, &Value::Null, &OPENS);
    let done = wait_index(&mut third, complete);
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], done["members"], "{done}");
    let found = answers(&mut third, &root, &queries);
    shutdown(third);
    let uncached = uncached_answers(environment.path(), &root, &Value::Null, &OPENS, &queries);
    assert_agree(&found, &uncached);
}

/// A indexes a tree of its own and aborts at the `nth` arrival at `point`.
/// B, over another tree and the same store, was persisted and answering
/// before; it answers identically after A's death. A third server on A's
/// tree finds the store usable: every member reused or extracted, nothing
/// refused, answers equal to an uncached server's, and a fourth server
/// reuses everything the third persisted.
fn abort_at(point: &str, nth: usize) {
    let environment = tempfile::tempdir().unwrap();
    let root_b = fixture(&environment.path().join("b"), 20);
    let root_a = fixture(environment.path(), UNITS);
    let opens_b = ["App.dpr", "lib/U3.pas"];
    let opens_a = ["App.dpr", "lib/U7.pas"];
    let queries_b = [Query::Symbols("P3"), Query::References("lib/U3.pas", 2, 10)];
    let queries_a = [Query::Symbols("P7"), Query::References("lib/U7.pas", 2, 10)];

    let mut other = launch_labelled(environment.path(), "cache", "b");
    started(&mut other, &root_b, &opens_b);
    wait_index(&mut other, persisted);
    let before = answers(&mut other, &root_b, &queries_b);

    let fault = StoreFault::new(environment.path(), point, "abort", nth);
    let mut dying = launch_with_store_fault(environment.path(), "cache", "a", &fault.spec, &[]);
    started(&mut dying, &root_a, &opens_a);
    wait_for_abort(&mut dying);

    let after = answers(&mut other, &root_b, &queries_b);
    assert_agree(&after, &before);
    let uncached = uncached_answers(
        environment.path(),
        &root_b,
        &Value::Null,
        &opens_b,
        &queries_b,
    );
    assert_agree(&after, &uncached);

    let mut third = cached_server(environment.path(), &root_a, &Value::Null, &opens_a);
    let done = wait_index(&mut third, persisted);
    assert_accounted(&done);
    assert_eq!(done["members"], MEMBERS, "{done}");
    if point == "RecordBeforeCommit" {
        assert!(
            done["reused"].as_u64().unwrap() >= nth as u64 - 1,
            "records committed before the abort are reused: {done}"
        );
    }
    let found = answers(&mut third, &root_a, &queries_a);
    let uncached = uncached_answers(
        environment.path(),
        &root_a,
        &Value::Null,
        &opens_a,
        &queries_a,
    );
    assert_agree(&found, &uncached);
    shutdown(third);
    shutdown(other);

    let mut fourth = cached_server(environment.path(), &root_a, &Value::Null, &opens_a);
    let done = wait_index(&mut fourth, complete);
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], MEMBERS, "{done}");
    shutdown(fourth);
}

/// Every publication boundary of the protocol, chosen mid-run for the
/// per-member points (all of them fire in this fixture).
#[test]
fn a_process_aborted_at_each_publication_boundary() {
    for (point, nth) in [
        ("EvidenceBeforeCommit", 3),
        ("RecordBeforeCommit", 5),
        ("MembersBeforeCommit", 1),
        ("SealBeforeCommit", 1),
    ] {
        eprintln!("abort at {point} #{nth}");
        abort_at(point, nth);
    }
}

/// A's store may not grow (`PASCAL_LSP_TEST_STORE_GROWTH_PAGES=0`) over a
/// store B warmed. A answers like an uncached server, buffer included, and
/// reports the full store through its `full` bypass counter; B keeps
/// answering warm. Once the limit is lifted and a closed member changes,
/// A's persistence resumes after the store's health backoff (wall clock,
/// one second doubling per failed probe), so the edit is repeated until a
/// publication lands.
#[test]
fn a_full_store_in_one_process_changes_no_answer_and_recovers() {
    let environment = tempfile::tempdir().unwrap();
    let root_b = query_fixture(&environment.path().join("b"), &MATRIX);
    let root_a = query_fixture(&environment.path().join("a"), &OTHER_NAME);
    let queries_b = [SHARED_REFERENCES, Query::Symbols("TShared")];
    let queries_a = [
        Query::References("lib/Shared.pas", 2, 6),
        Query::Symbols("TAlien"),
        Query::Symbols("OnlyInTheBuffer"),
    ];
    let buffer = "unit Other3;\ninterface\nprocedure OnlyInTheBuffer;\nimplementation\nprocedure OnlyInTheBuffer;\nbegin\nend;\nend.\n";

    let mut other = launch_labelled(environment.path(), "cache", "b");
    started(&mut other, &root_b, &OPENS);
    wait_index(&mut other, persisted);
    settle_background(&mut other);
    let warm = answers(&mut other, &root_b, &queries_b);

    let mut full = launch_with(
        environment.path(),
        &[("PASCAL_LSP_TEST_STORE_GROWTH_PAGES", "0")],
    );
    started(&mut full, &root_a, &OPENS);
    open_text(&mut full, &root_a.join("lib/Other3.pas"), buffer);
    let done = wait_index(&mut full, complete);
    settle_background(&mut full);
    assert!(done["extracted"].as_u64().unwrap() > 0, "{done}");
    let status = cache_status(&mut full);
    let fulls = status["counters"]["bypasses"]["full"].as_u64();
    assert!(fulls.is_some_and(|count| count > 0), "{status}");
    let found = answers(&mut full, &root_a, &queries_a);
    assert!(
        !found[2].locations.is_empty(),
        "the unsaved buffer still answers: {found:?}"
    );
    let uncached = uncached_answers_with_overlays(
        environment.path(),
        &root_a,
        &Value::Null,
        &OPENS,
        &[("lib/Other3.pas", buffer)],
        &queries_a,
    );
    assert_agree(&found, &uncached);

    let again = answers(&mut other, &root_b, &queries_b);
    assert_agree(&again, &warm);
    let status = cache_status(&mut other);
    assert_eq!(status["state"], "enabled", "{status}");

    let persisted_before = done["persisted"].as_u64().unwrap();
    let lifted = request(
        &mut full,
        "pascal/testStoreGrowth",
        json!({"extraPages": 1_048_576}),
    );
    assert_eq!(lifted["applied"], true, "{lifted}");
    let edited = root_a.join("lib/Other4.pas");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut round = 0;
    loop {
        round += 1;
        write_file(
            &edited,
            &format!(
                "unit Other4;\ninterface\ntype TAlien = class end;\nprocedure Edit{round};\nimplementation\nprocedure Edit{round};\nbegin\nend;\nend.\n"
            ),
        );
        pascal_project::backdate_tree_for_test(&root_a);
        watched_change(&mut full, &edited);
        settle_background(&mut full);
        let status = wait_index(&mut full, complete);
        if status["persisted"].as_u64().unwrap() > persisted_before {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "persistence did not resume: {status}"
        );
        thread::sleep(Duration::from_millis(250));
    }
    shutdown(full);
    shutdown(other);
}

/// Restores the permissions of the paths it chmodded when dropped, so the
/// temporary directory can be removed even when the test fails.
struct Restore(Vec<(PathBuf, u32)>);

impl Restore {
    fn chmod(&mut self, path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let original = fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        self.0.push((path.to_path_buf(), original));
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        for (path, mode) in self.0.iter().rev() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(*mode));
        }
    }
}

/// The store directory is read-only (`0500`, database `0400`) after a
/// warm-up persisted: a server answers like an uncached one and reports
/// the bypass; after the permissions are restored another server reuses
/// every member.
#[test]
fn a_read_only_store_is_bypassed() {
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root, permissions are not enforced");
        return;
    }
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];
    let mut warmup = launch_labelled(environment.path(), "cache", "warmup");
    started(&mut warmup, &root, &OPENS);
    wait_index(&mut warmup, persisted);
    shutdown(warmup);
    let uncached = uncached_answers(environment.path(), &root, &Value::Null, &OPENS, &queries);

    let directory = environment.path().join("cache/lint4d/analysis");
    let database = directory.join("analysis-v7.sqlite3");
    assert!(database.is_file(), "{database:?}");
    let mut guard = Restore(Vec::new());
    guard.chmod(&database, 0o400);
    guard.chmod(&directory, 0o500);

    let mut bypassed = cached_server(environment.path(), &root, &Value::Null, &OPENS);
    let status = cache_status(&mut bypassed);
    assert_eq!(status["state"], "bypassed", "{status}");
    let found = answers(&mut bypassed, &root, &queries);
    assert_agree(&found, &uncached);
    let buffer = "unit Other3;\ninterface\nprocedure OnlyInTheBuffer;\nimplementation\nprocedure OnlyInTheBuffer;\nbegin\nend;\nend.\n";
    open_text(&mut bypassed, &root.join("lib/Other3.pas"), buffer);
    let symbols = answers(&mut bypassed, &root, &[Query::Symbols("OnlyInTheBuffer")]);
    assert!(!symbols[0].locations.is_empty(), "{symbols:?}");
    shutdown(bypassed);
    drop(guard);

    let mut restored = cached_server(environment.path(), &root, &Value::Null, &OPENS);
    let done = wait_index(&mut restored, complete);
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], done["members"], "{done}");
    let found = answers(&mut restored, &root, &queries);
    shutdown(restored);
    assert_agree(&found, &uncached);
}

/// A is held inside the transaction of its first record publication (over a
/// tree of its own); B, warm on another tree before the hold, answers
/// references and symbols within the test's request timeout and equal to an
/// uncached server's.
#[test]
fn a_held_writer_does_not_block_another_processes_queries() {
    let environment = tempfile::tempdir().unwrap();
    let root_b = query_fixture(&environment.path().join("b"), &MATRIX);
    let root_a = query_fixture(&environment.path().join("a"), &OTHER_NAME);
    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];
    let mut other = warm_server(environment.path(), &root_b, &OPENS);

    let fault = StoreFault::new(environment.path(), "RecordBeforeCommit", "hold", 1);
    let mut held = launch_with_store_fault(environment.path(), "cache", "a", &fault.spec, &[]);
    started(&mut held, &root_a, &OPENS);
    fault.barrier.wait_until_entered();

    let found = answers(&mut other, &root_b, &queries);
    assert!(
        !found[0].locations.is_empty() && !found[1].locations.is_empty(),
        "{found:?}"
    );
    let uncached = uncached_answers(environment.path(), &root_b, &Value::Null, &OPENS, &queries);
    assert_agree(&found, &uncached);

    fault.barrier.release();
    let done = wait_index(&mut held, persisted);
    assert_eq!(done["refused"], 0, "{done}");
    shutdown(held);
    shutdown(other);
}

/// B is warm (store open, every member reused) when A starts on a tree
/// where one closed member changed; A re-extracts it and is held inside
/// the transaction of its new record. B gets the watched change, analyses
/// the member itself (it parses more than a repeated query does; the held
/// record is not visible to it, and `pascal/indexStatus` does not count
/// watched changes) and answers like an uncached server on the modified
/// tree. After the release B settles and a third server reuses every
/// member, the new record included.
#[test]
fn a_held_record_is_invisible_to_an_open_reader() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let queries = [SHARED_REFERENCES, Query::Symbols("TShared")];
    let mut other = warm_server(environment.path(), &root, &OPENS);
    let before = answers(&mut other, &root, &queries);
    settle_background(&mut other);
    let mark = parses(&mut other);
    answers(&mut other, &root, &queries);
    settle_background(&mut other);
    let baseline = parses(&mut other) - mark;

    let member = root.join("lib/Other7.pas");
    let edited = fs::read_to_string(&member)
        .unwrap()
        .replace("type TShared = class end;\n", "uses Shared;\n");
    assert!(edited.contains("uses Shared;"));
    write_file(&member, &edited);
    pascal_project::backdate_tree_for_test(&root);

    let fault = StoreFault::new(environment.path(), "RecordBeforeCommit", "hold", 1);
    let mut held = launch_with_store_fault(environment.path(), "cache", "a", &fault.spec, &[]);
    started(&mut held, &root, &OPENS);
    fault.barrier.wait_until_entered();

    settle_background(&mut other);
    let parses_before = parses(&mut other);
    watched_change(&mut other, &member);
    let found = answers(&mut other, &root, &queries);
    let changed = parses(&mut other) - parses_before;
    assert!(
        changed > baseline,
        "B parses the edited member itself: {changed} against {baseline}"
    );
    assert_ne!(found, before, "B saw the edit");
    let uncached = uncached_answers(environment.path(), &root, &Value::Null, &OPENS, &queries);
    assert_agree(&found, &uncached);

    fault.barrier.release();
    wait_index(&mut held, persisted);
    settle_background(&mut other);
    shutdown(held);
    shutdown(other);

    let mut third = cached_server(environment.path(), &root, &Value::Null, &OPENS);
    let done = wait_index(&mut third, complete);
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], done["members"], "{done}");
    let found = answers(&mut third, &root, &queries);
    shutdown(third);
    assert_agree(&found, &uncached);
}
