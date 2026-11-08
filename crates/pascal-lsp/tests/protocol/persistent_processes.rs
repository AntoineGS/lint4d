//! Spec §12.1 (`l4d-zf1b.12`): several server processes over one store,
//! with the test server's store fault points (`PASCAL_LSP_TEST_STORE_FAULT`)
//! and growth limit (`PASCAL_LSP_TEST_STORE_GROWTH_PAGES`,
//! `pascal/testStoreGrowth`).
#![cfg(unix)]

use super::persistent_cache::{request, settle};
use super::persistent_index::{
    MEMBERS, UNITS, complete, fixture, initialize_and_open, persisted, shutdown, wait_index,
};
use super::*;

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
/// publication. The two records it committed are visible to another
/// process; the held one is not: server B indexes the same workspace, reuses
/// at most those two and extracts the rest (it is refused as busy meanwhile). After the release both finish
/// and a fourth server reuses every member.
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
    let reused = done["reused"].as_u64().unwrap();
    let extracted = done["extracted"].as_u64().unwrap();
    // While A holds the write transaction, B's store use is refused as
    // busy, so B extracts what it cannot reuse; it never sees the held record.
    assert!(
        reused <= 2,
        "only the two committed records are visible: {done}"
    );
    assert!(
        reused + extracted + done["pending"].as_u64().unwrap() >= MEMBERS,
        "{done}"
    );
    assert!(
        extracted >= MEMBERS - 3,
        "the held member is extracted: {done}"
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
