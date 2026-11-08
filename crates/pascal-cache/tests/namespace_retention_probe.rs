//! Timing probe for namespace retention under disk pressure (l4d-1ypi,
//! review I1). Ignored: it builds two synthetic stores of about 800 MB.
//! Run with `cargo test -p pascal-cache --test namespace_retention_probe --
//! --ignored --nocapture`.
#![cfg(unix)]

mod common;
use common::private_tempdir;

use pascal_cache::{
    Bypass, DATABASE_FILE, Digest, Maintenance, MaintenanceLimits, Namespace, StepBudget, Store,
    StoreLimits,
};
use rusqlite::{Connection, params};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn namespace(byte: u8) -> Namespace {
    Namespace::new(Digest::from_bytes([byte; 32]))
}

fn id(prefix: u8, index: u32) -> Vec<u8> {
    let mut id = vec![prefix; 32];
    id[28..].copy_from_slice(&index.to_be_bytes());
    id
}

/// The reviewer's shape: 60k observations of 8 KB, 100k evidence sets,
/// 20k artifacts of 10 rows of 1 KB, 8,670 member records.
fn build(directory: &Path, byte: u8) {
    drop(Store::open(directory, &namespace(byte), StoreLimits::default()).unwrap());
    let mut connection = Connection::open(directory.join(DATABASE_FILE)).unwrap();
    // The synthetic rows reference no key manifests.
    connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .unwrap();
    let transaction = connection.transaction().unwrap();
    let ns = namespace(byte).id();
    let ns = ns.as_bytes().as_slice();
    let payload = vec![0x5au8; 4000];
    for index in 0..60_000u32 {
        transaction
            .execute(
                "INSERT INTO observation_content VALUES (NULL, ?1, 0, ?2, 1, ?3, ?4)",
                params![ns, id(1, index), id(2, index), payload],
            )
            .unwrap();
    }
    for index in 0..100_000u32 {
        transaction
            .execute(
                "INSERT INTO evidence_sets VALUES (NULL, ?1, 0, ?2, 2, ?3, ?4, NULL, 1, 1, \
                 8192, 1)",
                params![ns, id(3, index), id(4, 0), id(5, index)],
            )
            .unwrap();
    }
    let row = vec![0x33u8; 1024];
    for index in 0..20_000u32 {
        transaction
            .execute(
                "INSERT INTO artifacts VALUES (NULL, ?1, 0, ?2, 2, NULL, 1, 10, 10240, ?3, x'', 1)",
                params![ns, id(6, index), [0u8; 32].as_slice()],
            )
            .unwrap();
        let artifact = transaction.last_insert_rowid();
        for ordinal in 0..10 {
            transaction
                .execute(
                    "INSERT INTO artifact_rows VALUES (?1, ?2, 0, 1, NULL, ?3)",
                    params![artifact, ordinal, row],
                )
                .unwrap();
        }
    }
    let header = vec![0x44u8; 2048];
    for index in 0..8_670u32 {
        transaction
            .execute(
                "INSERT INTO member_records VALUES (?1, 0, ?2, 1, ?3, x'00', NULL, NULL, 0, 2048, 1)",
                params![ns, id(7, index), header],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
}

/// Drop the database's clean pages from the OS page cache, so the next
/// reads are cold (no root needed).
fn evict(directory: &Path) {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::File::open(directory.join(DATABASE_FILE)).unwrap();
    file.sync_all().unwrap();
    // SAFETY: a valid open descriptor; the call only drops cached pages.
    let result = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    assert_eq!(result, 0);
}

fn over_budget(directory: &Path) {
    Connection::open(directory.join(DATABASE_FILE))
        .unwrap()
        .execute(
            "UPDATE store_state SET max_disk_bytes = ?1, wal_threshold_bytes = ?2",
            params![512i64 << 20, 64i64 << 20],
        )
        .unwrap();
}

/// Acquire the store's writer in a loop while `running`; the longest wait
/// and the busy outcomes (the 25 ms allowance passed).
fn hammer(store: &Store, running: &AtomicBool, longest: &AtomicU64, busy: &AtomicUsize) {
    while running.load(Ordering::Relaxed) {
        let started = Instant::now();
        match store.hold_writer_for_test() {
            Ok(held) => drop(held),
            Err(Bypass::Busy) => {
                busy.fetch_add(1, Ordering::Relaxed);
            }
            Err(other) => panic!("{other:?}"),
        }
        longest.fetch_max(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
#[ignore]
fn probe_pressured_retirement_timings() {
    let size = |directory: &Path| {
        std::fs::metadata(directory.join(DATABASE_FILE))
            .unwrap()
            .len()
    };
    // Store::open over budget with one unheld old namespace.
    let root = private_tempdir();
    let first = root.path().join("open");
    let started = Instant::now();
    build(&first, 1);
    eprintln!("built {} MB in {:?}", size(&first) >> 20, started.elapsed());
    over_budget(&first);
    evict(&first);
    let started = Instant::now();
    let store = Store::open(&first, &namespace(2), StoreLimits::default()).unwrap();
    eprintln!("Store::open over budget: {:?}", started.elapsed());
    drop(store);
    std::fs::remove_dir_all(&first).unwrap();

    // Maintenance steps under pressure while another thread uses the writer.
    let second = root.path().join("steps");
    build(&second, 1);
    let store = Store::open(&second, &namespace(2), StoreLimits::default()).unwrap();
    over_budget(&second);
    evict(&second);
    let running = AtomicBool::new(true);
    let (longest, busy) = (AtomicU64::new(0), AtomicUsize::new(0));
    let (mut steps, mut longest_step) = (0, Duration::ZERO);
    let retired = |directory: &Path| -> bool {
        Connection::open(directory.join(DATABASE_FILE))
            .unwrap()
            .query_row(
                "SELECT state FROM namespaces WHERE namespace_id = ?1",
                [namespace(1).id().as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .ok()
            .is_none_or(|state| state != 1)
    };
    let started = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| hammer(&store, &running, &longest, &busy));
        let mut maintenance = Maintenance::new(MaintenanceLimits::default()).unwrap();
        let cancel = AtomicBool::new(false);
        while !retired(&second) && started.elapsed() < Duration::from_secs(120) {
            let step = Instant::now();
            let _ = maintenance.step(
                &store,
                StepBudget {
                    time: Duration::from_millis(50),
                    rows: 256,
                },
                &cancel,
            );
            longest_step = longest_step.max(step.elapsed());
            steps += 1;
        }
        running.store(false, Ordering::Relaxed);
    });
    eprintln!(
        "pressured maintenance: retired={} after {steps} steps in {:?}; longest step {:?}; \
         longest writer wait {} us; writer busy {}",
        retired(&second),
        started.elapsed(),
        longest_step,
        longest.load(Ordering::Relaxed),
        busy.load(Ordering::Relaxed)
    );
}
