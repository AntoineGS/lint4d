//! SQLite's heap count (`sqlite3_memory_used`) is process-global, so this
//! binary holds exactly one test: no other test's connections may run
//! while it measures.
#![cfg(unix)]

use pascal_cache::{
    CONNECTION_OVERHEAD_BYTES, ContextKey, Digest, MIN_CONNECTION_PAGE_CACHE_BYTES, Namespace,
    Store, StoreLimits,
};
use pascal_project::ProjectContext;
use std::os::unix::fs::PermissionsExt;

fn sqlite_heap() -> i64 {
    // SAFETY: `sqlite3_memory_used` only reads SQLite's memory statistics.
    unsafe { rusqlite::ffi::sqlite3_memory_used() }
}

#[test]
fn an_open_store_stays_within_its_connection_allowance() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), PermissionsExt::from_mode(0o700)).unwrap();
    // The smallest page cache, so the overhead dominates what is measured.
    let limits = StoreLimits {
        page_cache_bytes: 3 * MIN_CONNECTION_PAGE_CACHE_BYTES,
        ..StoreLimits::default()
    };
    let before = sqlite_heap();
    let store = Store::open(
        directory.path(),
        &Namespace::new(Digest::from_bytes([7; 32])),
        limits,
    )
    .unwrap();
    // Opening used the writer (container, namespace, retirement). Each
    // reader now prepares and runs a lookup.
    let context = ContextKey::new(&ProjectContext::default());
    {
        let _first = store.hold_reader_for_test().unwrap();
        store.find_snapshots(&context).unwrap();
    }
    store.find_snapshots(&context).unwrap();
    store.check_scope().unwrap();
    assert_eq!(store.connection_count_for_test(), limits.connections());
    let used = u64::try_from(sqlite_heap() - before).unwrap();
    eprintln!(
        "SQLite heap of an open store with {} connections: {used} bytes \
         (allowance {} = {} page cache + {} x {CONNECTION_OVERHEAD_BYTES})",
        limits.connections(),
        limits.page_admission_bytes(),
        limits.page_cache_bytes,
        limits.connections()
    );
    assert!(
        used > 0,
        "SQLite memory statistics are disabled; the comparison would prove nothing"
    );
    assert!(
        used <= limits.page_admission_bytes(),
        "{used} bytes exceed the {} byte allowance",
        limits.page_admission_bytes()
    );
    drop(store);
}
