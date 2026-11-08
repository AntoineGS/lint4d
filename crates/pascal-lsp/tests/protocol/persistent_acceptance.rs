//! Spec §12.1 (`l4d-zf1b.12`): the integrated cached-versus-uncached
//! matrix. Each test compares a cold persisting server, a warm second
//! server over the same store and a persistence-disabled server.
#![cfg(unix)]

use super::persistent_queries::*;
use super::*;

/// The unmutated matrix fixture: the persisting first server and the warm
/// second server both answer like an uncached server, and the warm one
/// reused every member.
#[test]
fn an_unchanged_tree_answers_alike_cold_warm_and_uncached() {
    let environment = tempfile::tempdir().unwrap();
    let root = query_fixture(environment.path(), &MATRIX);
    let run = differential_full(
        environment.path(),
        &root,
        &Value::Null,
        &OPENS,
        |_| {},
        &[SHARED_REFERENCES, Query::Symbols("TShared")],
    );
    assert_agree(&run.cold, &run.uncached);
    assert_agree(&run.warm, &run.uncached);
    assert_truthful(&run.warm, &run.uncached);
    assert_eq!(run.warm_status["extracted"], 0, "{}", run.warm_status);
    assert!(
        run.warm_status["reused"].as_u64() > Some(0),
        "{}",
        run.warm_status
    );
}
