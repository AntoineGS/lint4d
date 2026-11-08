//! Spec §12.1 (`l4d-zf1b.12`): several server processes over one store,
//! with the test server's store fault points (`PASCAL_LSP_TEST_STORE_FAULT`)
//! and growth limit (`PASCAL_LSP_TEST_STORE_GROWTH_PAGES`,
//! `pascal/testStoreGrowth`).
#![cfg(unix)]

use super::persistent_cache::{request, settle, store_directory, watched_change};
use super::persistent_index::{
    INDEX_DEADLINE, MEMBERS, UNITS, complete, fixture, initialize_and_open, persisted, shutdown,
    wait_index,
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

    /// Waits for the fault point to be reached; the crawl that leads to it
    /// is not bounded by the request timeout, so `INDEX_DEADLINE` applies.
    pub(super) fn wait_until_entered(&self) {
        wait_barrier_entered(&self.barrier);
    }
}

/// Polls for the barrier's `entered` file until `INDEX_DEADLINE`.
fn wait_barrier_entered(barrier: &TestBarrier) {
    let deadline = Instant::now() + INDEX_DEADLINE;
    while !barrier.entered.exists() {
        assert!(
            Instant::now() < deadline,
            "the barrier {:?} was not reached within {INDEX_DEADLINE:?}",
            barrier.entered
        );
        // Deadline pacing of a file poll; no event is ordered by it.
        thread::sleep(Duration::from_millis(5));
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
/// publication. Server B, started meanwhile on the same workspace, has its
/// store open refused as busy (its `pascal/cacheStatus` says so), so it
/// reuses nothing and extracts every member: it has no store to read the
/// committed records or the held one from. After the
/// release both finish and a fourth server reuses every member.
#[test]
fn a_store_held_at_open_is_bypassed_busy_and_converges() {
    let environment = tempfile::tempdir().unwrap();
    let root = fixture(environment.path(), UNITS);
    let fault = StoreFault::new(environment.path(), "RecordBeforeCommit", "hold", 3);
    let mut held = launch_with_store_fault(environment.path(), "cache", "held", &fault.spec, &[]);
    initialize_and_open(&mut held, &root, "App.dpr");
    fault.wait_until_entered();

    let mut other = super::persistent_cache::launch(environment.path());
    initialize_and_open(&mut other, &root, "App.dpr");
    let done = wait_index(&mut other, complete);
    assert_eq!(done["members"], MEMBERS, "{done}");
    // While A holds the write transaction, B's store open is refused as
    // busy and B runs memory-only: it never sees the held record.
    let status = cache_status(&mut other);
    // The refusal is counted as busy; the state is `bypassed` or, while
    // the server retries the open, `opening`, never `enabled`.
    assert!(
        status["counters"]["bypasses"]["busy"].as_u64() > Some(0),
        "{status}"
    );
    assert!(
        status["state"] == "bypassed" || status["state"] == "opening",
        "{status}"
    );
    // Everything still pending at `complete` awaits the reuse pass the
    // bypass made impossible; nothing is refused.
    assert_eq!(done["pending"], done["reusePending"], "{done}");
    assert_eq!(done["refused"], 0, "{done}");
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

/// `pascal/testStoreGrowth` limits the open store and says so
/// (`applied: true`); a server without an open store (persistence
/// disabled) reports `applied: false`. The
/// `PASCAL_LSP_TEST_STORE_GROWTH_PAGES` variable is exercised by
/// `a_full_store_in_one_process_changes_no_answer_and_recovers`.
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

    let mut disabled = super::persistent_cache::launch(environment.path());
    disabled.initialize(&root, json!({"persistentCacheEnabled": false}));
    settle(&mut disabled, false);
    let reply = request(
        &mut disabled,
        "pascal/testStoreGrowth",
        json!({"extraPages": 100000}),
    );
    assert_eq!(reply["applied"], false, "{reply}");
    shutdown(disabled);
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

/// Waits, until `INDEX_DEADLINE`, for the child of a server armed with an
/// `abort` fault at `point` to die and asserts that it was `abort()` that
/// ended it.
fn wait_for_abort(server: &mut TestServer, point: &str) {
    use std::os::unix::process::ExitStatusExt;
    let deadline = Instant::now() + INDEX_DEADLINE;
    let status = loop {
        if let Some(status) = server.child.try_wait().expect("poll the aborting server") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the server did not abort at {point} within {INDEX_DEADLINE:?}"
        );
        // Deadline pacing only: the server may already be gone, so no
        // request can order this poll.
        thread::sleep(Duration::from_millis(5));
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
    fault.wait_until_entered();

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
    // "P" matches every unit's procedure, so a half record of the aborted
    // member would show as a missing or extra symbol.
    let queries_a = [Query::Symbols("P"), Query::References("lib/U7.pas", 2, 10)];

    let mut other = launch_labelled(environment.path(), "cache", "b");
    started(&mut other, &root_b, &opens_b);
    wait_index(&mut other, persisted);
    let before = answers(&mut other, &root_b, &queries_b);

    let fault = StoreFault::new(environment.path(), point, "abort", nth);
    let mut dying = launch_with_store_fault(environment.path(), "cache", "a", &fault.spec, &[]);
    started(&mut dying, &root_a, &opens_a);
    wait_for_abort(&mut dying, point);

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
    assert!(
        found[0].locations.len() >= UNITS,
        "{}",
        found[0].locations.len()
    );
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
/// answering warm. Once the limit is lifted, the store's test clock is
/// advanced past the longest health backoff (`pascal/testStoreClock`) and a
/// closed member changes, A's persistence resumes.
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
    let uncached_b = uncached_answers(
        environment.path(),
        &root_b,
        &Value::Null,
        &OPENS,
        &queries_b,
    );
    assert_agree(&warm, &uncached_b);

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
    assert_agree(&again, &uncached_b);
    let status = cache_status(&mut other);
    assert_eq!(status["state"], "enabled", "{status}");

    let persisted_before = done["persisted"].as_u64().unwrap();
    let lifted = request(
        &mut full,
        "pascal/testStoreGrowth",
        json!({"extraPages": 1_048_576}),
    );
    assert_eq!(lifted["applied"], true, "{lifted}");
    let advanced = request(
        &mut full,
        "pascal/testStoreClock",
        json!({"advanceMs": pascal_cache::MAX_HEALTH_BACKOFF.as_millis() as u64}),
    );
    assert_eq!(advanced["applied"], true, "{advanced}");
    let edited = root_a.join("lib/Other4.pas");
    write_file(
        &edited,
        "unit Other4;\ninterface\ntype TAlien = class end;\nprocedure Edited;\nimplementation\nprocedure Edited;\nbegin\nend;\nend.\n",
    );
    pascal_project::backdate_tree_for_test(&root_a);
    watched_change(&mut full, &edited);
    let resumed = wait_index(&mut full, |context| {
        context["persisted"].as_u64() > Some(persisted_before)
    });
    assert!(
        resumed["persisted"].as_u64().unwrap() > persisted_before,
        "persistence resumed: {resumed}"
    );
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
    let database = fs::read_dir(&directory)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("analysis-v") && name.ends_with(".sqlite3"))
        })
        .expect("the store database");
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
    fault.wait_until_entered();

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
    let parses_before = parses(&mut other);

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
    fault.wait_until_entered();

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

/// A distinct build fingerprint (64 hex characters) per `n`.
fn build_fingerprint(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

/// The id `--cache-status` lists for the namespace of build `n`.
fn build_id(n: u8) -> String {
    let fingerprint = pascal_cache::Digest::from_hex(&build_fingerprint(n)).unwrap();
    pascal_cache::Namespace::new(fingerprint).id().to_hex()
}

/// A test server playing build `n` over `environment/cache`, with the
/// `extra` variables.
fn launch_build(environment: &Path, n: u8, extra: &[(&str, &str)]) -> TestServer {
    let fingerprint = build_fingerprint(n);
    let mut variables = vec![("PASCAL_LSP_TEST_NAMESPACE", fingerprint.as_str())];
    variables.extend_from_slice(extra);
    launch_with(environment, &variables)
}

/// Build `n` indexes `OPENS` of `root` until persisted and stops; returns
/// its context. Its last use is then recorded as `rank` (seconds after a
/// fixed origin earlier than every real start), so the least recently used
/// order does not depend on the clock's granularity.
fn run_build(environment: &Path, root: &Path, n: u8, options: Value, rank: u64) -> Value {
    let mut server = launch_build(environment, n, &[]);
    server.initialize(root, options);
    open_disk(&mut server, root, &OPENS);
    let done = wait_index(&mut server, persisted);
    maintained(&mut server);
    shutdown(server);
    used_at(environment, n, rank);
    done
}

/// Runs maintenance steps on the server, one request each, until a whole
/// cycle completed: the namespace retirement pass is a cycle's first phase,
/// so the store's retention policy has been applied when this returns.
fn maintained(server: &mut TestServer) {
    for _ in 0..64 {
        let settled = settle(server, true);
        let cycles = &settled["status"]["counters"]["maintenance"]["cycles"];
        if cycles.as_u64().is_some_and(|cycles| cycles >= 1) {
            return;
        }
    }
    panic!("no maintenance cycle completed");
}

fn used_at(environment: &Path, n: u8, rank: u64) {
    let lock = store_directory(environment)
        .join("namespaces")
        .join(format!("{}.lock", build_id(n)));
    let file = fs::OpenOptions::new().write(true).open(lock).unwrap();
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + rank))
        .unwrap();
}

/// A build `n` server over the store, complete and settled.
fn started_build(environment: &Path, root: &Path, n: u8, options: Value) -> (TestServer, Value) {
    let mut server = launch_build(environment, n, &[]);
    server.initialize(root, options);
    open_disk(&mut server, root, &OPENS);
    let done = wait_index(&mut server, complete);
    settle_background(&mut server);
    maintained(&mut server);
    (server, done)
}

/// The admin binary (`--cache-status`, `--cache-prune`, ...) over the same
/// store as the servers; its one JSON line. It registers the real build's
/// namespace like any process opening the store, which is no test build.
fn admin(environment: &Path, arguments: &[&str]) -> Value {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pascal-lsp"))
        .args(arguments)
        .env("HOME", environment.join("home"))
        .env("XDG_CACHE_HOME", environment.join("cache"))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run pascal-lsp");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON line")
}

/// `(id, held)` of the live namespaces `--cache-status` lists.
fn live_namespaces(environment: &Path) -> Vec<(String, bool)> {
    let status = admin(environment, &["--cache-status"]);
    status["namespaces"]["list"]
        .as_array()
        .unwrap_or_else(|| panic!("{status}"))
        .iter()
        .map(|entry| {
            (
                entry["id"].as_str().unwrap().to_string(),
                entry["held"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn is_live(environment: &Path, n: u8) -> bool {
    let id = build_id(n);
    live_namespaces(environment)
        .iter()
        .any(|(listed, _)| *listed == id)
}

const BUILD_QUERIES: [Query; 2] = [SHARED_REFERENCES, Query::Symbols("TShared")];

/// Two builds alternate over one store through the protocol: each finds
/// the facts it persisted before the other build ran, extracts nothing,
/// and answers like an uncached server.
#[test]
fn two_builds_alternating_through_the_protocol_reuse_each_others_facts() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let first = run_build(environment.path(), &root, 1, Value::Null, 1);
    assert!(first["extracted"].as_u64().unwrap() > 0, "{first}");
    let second = run_build(environment.path(), &root, 2, Value::Null, 2);
    assert!(second["extracted"].as_u64().unwrap() > 0, "{second}");
    assert_eq!(second["reused"], 0, "build 2 starts cold: {second}");

    let uncached = uncached_answers(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        &BUILD_QUERIES,
    );
    for (n, rank) in [(1, 3), (2, 4)] {
        let (mut server, done) = started_build(environment.path(), &root, n, Value::Null);
        assert_eq!(done["extracted"], 0, "build {n}: {done}");
        assert!(done["reused"].as_u64().unwrap() > 0, "build {n}: {done}");
        let found = answers(&mut server, &root, &BUILD_QUERIES);
        shutdown(server);
        used_at(environment.path(), n, rank);
        assert_agree(&found, &uncached);
    }
}

/// With the default of three namespaces a fourth build retires the least
/// recently used one: the status lists the other three, a warm restart of
/// build 2 still extracts nothing, and the retired build 1 starts cold with
/// the right answers.
#[test]
fn a_fourth_build_retires_the_least_recently_used_namespace() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    for n in 1..=3u8 {
        run_build(environment.path(), &root, n, Value::Null, n as u64);
    }
    assert!((1..=3).all(|n| is_live(environment.path(), n)));
    run_build(environment.path(), &root, 4, Value::Null, 4);
    assert!(!is_live(environment.path(), 1), "the oldest is retired");
    assert!((2..=4).all(|n| is_live(environment.path(), n)));

    let uncached = uncached_answers(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        &BUILD_QUERIES,
    );
    // Build 2 is used again, so build 3 becomes the least recently used.
    let (mut server, done) = started_build(environment.path(), &root, 2, Value::Null);
    assert_eq!(done["extracted"], 0, "{done}");
    let found = answers(&mut server, &root, &BUILD_QUERIES);
    shutdown(server);
    used_at(environment.path(), 2, 5);
    assert_agree(&found, &uncached);

    let (mut server, done) = started_build(environment.path(), &root, 1, Value::Null);
    assert_eq!(done["reused"], 0, "the retired build starts cold: {done}");
    assert!(done["extracted"].as_u64().unwrap() > 0, "{done}");
    let found = answers(&mut server, &root, &BUILD_QUERIES);
    shutdown(server);
    assert_agree(&found, &uncached);
    assert!(is_live(environment.path(), 1));
    assert!(!is_live(environment.path(), 3), "build 3 was retired");
}

/// Build 1 is live (held) and warm while builds 2 and 3 start with a store
/// keeping one namespace: build 1 is never retired and keeps answering like
/// an uncached server. After it exits, a later start retires it.
#[test]
fn a_live_reader_keeps_its_namespace_while_another_build_needs_room() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let keep_one = json!({"persistentCacheMaxNamespaces": 1});
    run_build(environment.path(), &root, 1, Value::Null, 1);
    let (mut reader, done) = started_build(environment.path(), &root, 1, Value::Null);
    assert_eq!(done["extracted"], 0, "{done}");
    let uncached = uncached_answers(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        &BUILD_QUERIES,
    );

    run_build(environment.path(), &root, 2, keep_one.clone(), 2);
    run_build(environment.path(), &root, 3, keep_one.clone(), 3);
    let listed = live_namespaces(environment.path());
    let held = |n: u8| listed.iter().find(|(id, _)| *id == build_id(n));
    assert_eq!(held(1).map(|(_, held)| *held), Some(true), "{listed:?}");
    assert!(held(2).is_none(), "build 2 was retired: {listed:?}");
    assert!(held(3).is_some(), "{listed:?}");
    let found = answers(&mut reader, &root, &BUILD_QUERIES);
    assert_agree(&found, &uncached);
    shutdown(reader);
    used_at(environment.path(), 1, 4);

    run_build(environment.path(), &root, 4, keep_one, 5);
    assert!(!is_live(environment.path(), 1), "no longer held, retired");
    assert!(is_live(environment.path(), 4));
}

/// Build 1's server is held in its reuse pass (after 5 members, between its
/// store reads, not inside a read transaction) while the admin binary
/// prunes the store keeping one namespace: both builds are live before,
/// the prune changes rows and leaves only the held one. After the release the
/// reader completes extracting nothing and answers like an uncached server.
#[test]
fn gc_in_one_process_while_another_reads() {
    const HELD: usize = 5;
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    for n in 1..=3u8 {
        run_build(environment.path(), &root, n, Value::Null, n as u64);
    }
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
    let mut reader = launch_build(
        environment.path(),
        1,
        &[("PASCAL_LSP_TEST_REUSE_PASS_BARRIER", spec.as_str())],
    );
    started(&mut reader, &root, &OPENS);
    wait_barrier_entered(&barrier);

    assert!(is_live(environment.path(), 2) && is_live(environment.path(), 3));
    let pruned = admin(
        environment.path(),
        &["--cache-prune", "--cache-max-namespaces", "1"],
    );
    assert!(
        pruned["rows"].as_u64().unwrap() > 0,
        "GC changed rows: {pruned}"
    );
    let listed = live_namespaces(environment.path());
    assert!(
        listed.iter().any(|(id, held)| *id == build_id(1) && *held),
        "the reader's namespace is held and kept: {listed:?}"
    );
    assert!(!is_live(environment.path(), 2) && !is_live(environment.path(), 3));

    barrier.release();
    let done = wait_index(&mut reader, complete);
    assert_eq!(done["extracted"], 0, "{done}");
    assert_eq!(done["reused"], done["members"], "{done}");
    settle_background(&mut reader);
    let found = answers(&mut reader, &root, &BUILD_QUERIES);
    shutdown(reader);
    let uncached = uncached_answers(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        &BUILD_QUERIES,
    );
    assert_agree(&found, &uncached);
}
