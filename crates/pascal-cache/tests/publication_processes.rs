//! Two-process publication tests (spec §12.1 amendment 2026-10-27): the
//! test binary re-executes itself in a named role, coordinates through
//! lines on its standard streams, pauses or aborts at named fault points,
//! and the parent then requires a complete committed record or a clean miss.

#![cfg(unix)]

mod common;

use common::private_tempdir;
use common::records::{
    EAGER, SEAL, age_past_grace, artifact, binding_evidence, check_invariant, context, member,
    open, publish_bulk, raw, read_members, read_record, records, sealed, unit_lookup,
};
use pascal_cache::{
    Bypass, DATABASE_FILE, Digest, EvidencePurpose, EvidenceSubject, EvidenceTarget, EvidenceWrite,
    FaultPoint, LEASE_EXPIRY, Lookup, Maintenance, MaintenanceLimits, Observation, RetireStep,
    SnapshotHeader, StepBudget, Store, evidence_id,
};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const ROLE: &str = "PASCAL_CACHE_PUBLISHER_ROLE";
const DIRECTORY: &str = "PASCAL_CACHE_PUBLISHER_DIRECTORY";
const VARIANT: &str = "PASCAL_CACHE_PUBLISHER_VARIANT";
/// A fault point name, or "begun" (after `begin_snapshot` committed).
const PAUSE: &str = "PASCAL_CACHE_PUBLISHER_PAUSE";
const ABORT: &str = "PASCAL_CACHE_PUBLISHER_ABORT";

/// The provider path `variant` resolves the unit `provider` to.
fn target(variant: &str) -> String {
    format!("/w/{variant}.pas")
}

/// One whole publication: binding evidence for `provider`, the record of
/// `/w/Main.pas` importing it, and a sealed snapshot of that record.
fn build(store: &Store, variant: &str, pause: &dyn Fn(&str)) -> SnapshotHeader {
    try_build(store, variant, pause).unwrap()
}

/// [`build`], returning the first store call's bypass.
fn try_build(store: &Store, variant: &str, pause: &dyn Fn(&str)) -> Result<SnapshotHeader, Bypass> {
    let target = target(variant);
    let observations = [Observation::Presence {
        path: PathBuf::from(&target),
        entry: None,
        present: true,
    }];
    let evidence = store
        .publish_evidence(
            EvidenceTarget::Binding(&unit_lookup("provider")),
            &EvidenceWrite {
                context: &context(),
                observations: &observations,
            },
        )?
        .evidence_id;
    let record = store
        .publish_member_record(&member("/w/Main.pas", &[target.as_str()], evidence))?
        .record_id;
    let mut builder = store.begin_snapshot(&context())?;
    pause("begun");
    store.add_members(&mut builder, &[record])?;
    store.seal_snapshot(builder, &SEAL)
}

/// The record `build(…, variant, …)` publishes, computed without a store.
fn expected_record(variant: &str) -> Digest {
    let target = target(variant);
    let observations = [Observation::Presence {
        path: PathBuf::from(&target),
        entry: None,
        present: true,
    }];
    let evidence = evidence_id(
        &EvidenceSubject {
            purpose: EvidencePurpose::Binding,
            context: context().digest(),
            subject: unit_lookup("provider").digest(),
        },
        None,
        &observations,
    );
    member("/w/Main.pas", &[target.as_str()], evidence)
        .id()
        .unwrap()
}

fn wait_for_parent(point: &str) {
    println!("at {point}");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();
}

#[test]
#[ignore = "run by the publication process tests"]
fn publisher_child_entry() {
    let Some(role) = std::env::var_os(ROLE) else {
        return;
    };
    let directory = std::env::var_os(DIRECTORY).expect("directory");
    let variant = std::env::var(VARIANT).unwrap_or_default();
    let pause = std::env::var(PAUSE).unwrap_or_default();
    let store = open(Path::new(&directory));
    if let Ok(name) = std::env::var(ABORT) {
        let point = FaultPoint::from_name(&name).expect("a fault point");
        store.set_fault_for_test(point, || std::process::abort());
    }
    if let Some(point) = FaultPoint::from_name(&pause) {
        // One-shot: a maintenance cycle reaches its points once per batch,
        // and the parent sends one line.
        let waited = AtomicBool::new(false);
        store.set_fault_for_test(point, move || {
            if !waited.swap(true, Ordering::SeqCst) {
                wait_for_parent(point.name());
            }
        });
    }
    let pause_at = |step: &str| {
        if step == pause {
            wait_for_parent(step);
        }
    };
    match role.to_str().unwrap() {
        "build" => {
            let header = build(&store, &variant, &pause_at);
            println!("result sealed {}", header.publication_seq());
        }
        "build-tolerant" => match try_build(&store, &variant, &pause_at) {
            Ok(header) => println!("result sealed {}", header.publication_seq()),
            Err(bypass) => println!("result bypass {bypass:?}"),
        },
        "maintain" => {
            if variant == "expire" {
                // Every other process's lease looks expired from here.
                store.advance_clock_for_test(LEASE_EXPIRY);
            }
            age_past_grace(&store);
            let mut maintenance = Maintenance::new(MaintenanceLimits {
                recency_window: 0,
                vacuum_free_bytes: 0,
                ..MaintenanceLimits::DEFAULT
            })
            .unwrap();
            let never = AtomicBool::new(false);
            let budget = StepBudget {
                time: Duration::from_secs(60),
                rows: 100_000,
            };
            let mut rows = 0;
            loop {
                let report = maintenance.step(&store, budget, &never).unwrap();
                assert!(!report.skipped, "{report:?}");
                rows += report.rows;
                if report.cycle_complete {
                    break;
                }
            }
            println!("result maintained {rows}");
        }
        "reset" => {
            store.reset_namespace().unwrap();
            println!("result reset");
        }
        "retire" => {
            let Lookup::Hit(headers) = store.find_snapshots(&context()).unwrap() else {
                panic!("a sealed snapshot to retire")
            };
            println!(
                "result retired {}",
                store.retire_snapshot(headers[0].id()).unwrap()
            );
        }
        other => panic!("unknown role {other}"),
    }
}

fn child(role: &str, directory: &Path, variant: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "publisher_child_entry",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE, role)
        .env(DIRECTORY, directory)
        .env(VARIANT, variant)
        .env_remove(PAUSE)
        .env_remove(ABORT);
    command
}

/// Read lines until one contains `needle`; return what follows it.
fn expect_line(stdout: &mut impl BufRead, needle: &str) -> String {
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(
            stdout.read_line(&mut line).unwrap(),
            0,
            "the child exited before printing {needle:?}"
        );
        if let Some((_, rest)) = line.split_once(needle) {
            return rest.trim_end().to_string();
        }
    }
}

/// Run a child to completion and return what it printed after "result ".
fn run(mut command: Command) -> String {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut stdout = output.stdout.as_slice();
    expect_line(&mut stdout, "result ")
}

/// Run a child that must die by `abort` at `point`.
fn abort_at(mut command: Command, point: FaultPoint) {
    let status = command
        .env(ABORT, point.name())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(
        status.signal(),
        Some(libc::SIGABRT),
        "{point:?}: {status:?}"
    );
}

/// A child stopped at a named point, waiting for a line on its stdin.
struct Paused {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Paused {
    fn spawn(mut command: Command, pause: &str) -> Self {
        let mut child = command
            .env(PAUSE, pause)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        expect_line(&mut stdout, &format!("at {pause}"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Let the child continue; what it printed after "result ".
    fn resume(mut self) -> String {
        writeln!(self.stdin, "go").unwrap();
        let result = expect_line(&mut self.stdout, "result ");
        let status = self.child.wait().unwrap();
        assert!(status.success(), "the resumed child failed: {status:?}");
        result
    }
}

impl Drop for Paused {
    /// A test that fails while a child waits must not leave it running.
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn sealed_snapshots(store: &Store) -> Vec<SnapshotHeader> {
    match store.find_snapshots(&context()).unwrap() {
        Lookup::Hit(headers) => headers,
        Lookup::Miss(_) => Vec::new(),
    }
}

/// The import targets of the one member of `header`, read verified.
fn targets_of(store: &Store, header: &SnapshotHeader) -> Vec<PathBuf> {
    let members = read_members(store, header).unwrap();
    assert_eq!(members.len(), 1);
    read_record(store, members[0].record_id)
        .unwrap()
        .bindings
        .iter()
        .map(|binding| binding.outcome.target().unwrap().canonical.clone())
        .collect()
}

#[test]
fn ac1_an_old_publisher_sealing_after_a_new_one_never_replaces_or_mixes_bindings() {
    for pause in ["begun", FaultPoint::SealDigestStreamed.name()] {
        let root = private_tempdir();
        let directory = root.path();
        let store = open(directory);
        let old = Paused::spawn(child("build", directory, "Old"), pause);
        let new_result = run(child("build", directory, "New"));
        let before = sealed_snapshots(&store);
        assert_eq!(
            before.len(),
            1,
            "{pause}: the old build is not a snapshot yet"
        );
        let new = before[0].clone();
        assert_eq!(new_result, format!("sealed {}", new.publication_seq()));
        let old_result = old.resume();
        let after = sealed_snapshots(&store);
        assert_eq!(after.len(), 2, "{pause}");
        assert_eq!(after[1], new, "{pause}: the new snapshot is unchanged");
        let old_header = &after[0];
        assert_eq!(
            old_result,
            format!("sealed {}", old_header.publication_seq())
        );
        assert!(
            old_header.publication_seq() > new.publication_seq(),
            "{pause}: history, newest publication first; sessions validate in order"
        );
        for (header, mine, theirs) in [(old_header, "Old", "New"), (&new, "New", "Old")] {
            assert_eq!(
                targets_of(&store, header),
                [PathBuf::from(target(mine))],
                "{pause}: no mixed bindings"
            );
            assert!(
                matches!(
                    store
                        .find_importers(header, Path::new(&target(theirs)), None, 10)
                        .unwrap(),
                    Lookup::Hit(page) if page.hits.is_empty()
                ),
                "{pause}: one snapshot's importers never include the other's"
            );
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Visible {
    evidence: bool,
    record: bool,
    snapshots: i64,
    members: i64,
    sealed: usize,
}

/// What a fresh process sees after a child died; every sealed snapshot
/// must read back complete and verified.
fn visible(store: &Store, directory: &Path) -> Visible {
    let connection = raw(directory);
    let count = |sql: &str| -> i64 { connection.query_row(sql, [], |row| row.get(0)).unwrap() };
    let sealed = sealed_snapshots(store);
    for header in &sealed {
        assert_eq!(targets_of(store, header), [PathBuf::from(target("New"))]);
    }
    Visible {
        evidence: matches!(
            store
                .find_subject_evidence(
                    &context(),
                    EvidenceTarget::Binding(&unit_lookup("provider"))
                )
                .unwrap(),
            Lookup::Hit(_)
        ),
        record: matches!(
            store.open_member_record(expected_record("New")).unwrap(),
            Lookup::Hit(_)
        ),
        snapshots: count("SELECT count(*) FROM snapshots"),
        members: count("SELECT count(*) FROM snapshot_members"),
        sealed: sealed.len(),
    }
}

#[test]
fn ac2_death_at_each_publication_boundary_leaves_a_complete_record_or_a_clean_miss() {
    let expected = |evidence, record, snapshots, members, sealed| Visible {
        evidence,
        record,
        snapshots,
        members,
        sealed,
    };
    let cases = [
        (
            FaultPoint::EvidenceBeforeCommit,
            expected(false, false, 0, 0, 0),
        ),
        (
            FaultPoint::RecordBeforeCommit,
            expected(true, false, 0, 0, 0),
        ),
        (FaultPoint::BeginBeforeCommit, expected(true, true, 0, 0, 0)),
        (
            FaultPoint::MembersBeforeCommit,
            expected(true, true, 1, 0, 0),
        ),
        (
            FaultPoint::SealDigestStreamed,
            expected(true, true, 1, 1, 0),
        ),
        (FaultPoint::SealBeforeCommit, expected(true, true, 1, 1, 0)),
        (FaultPoint::SealCommitted, expected(true, true, 1, 1, 1)),
    ];
    assert_eq!(
        cases
            .iter()
            .map(|(point, _)| point.name())
            .chain([
                FaultPoint::RetireBeforeCommit.name(),
                FaultPoint::LeaseRenewDecided.name(),
            ])
            .chain(FaultPoint::MAINTENANCE.iter().map(|point| point.name()))
            .collect::<BTreeSet<_>>(),
        FaultPoint::ALL
            .iter()
            .map(|point| point.name())
            .collect::<BTreeSet<_>>(),
        "every point but RetireBeforeCommit, lease renewal and the maintenance points is on the build path"
    );
    for (point, seen) in cases {
        let root = private_tempdir();
        let directory = root.path();
        drop(open(directory));
        abort_at(child("build", directory, "New"), point);
        // A new process recovers the WAL; the dead one held no lock.
        let store = open(directory);
        assert_eq!(visible(&store, directory), seen, "{point:?}");
        // Publication recovers: no stuck lock, no wait beyond the allowance.
        let header = build(&store, "New", &|_| {});
        assert_eq!(targets_of(&store, &header), [PathBuf::from(target("New"))]);
        assert_eq!(
            visible(&store, directory).sealed,
            seen.sealed + 1,
            "{point:?}"
        );
    }
    // Death inside a maintenance retirement leaves the sealed snapshot.
    let root = private_tempdir();
    let directory = root.path();
    let store = open(directory);
    build(&store, "New", &|_| {});
    abort_at(
        child("retire", directory, "New"),
        FaultPoint::RetireBeforeCommit,
    );
    assert_eq!(visible(&store, directory).sealed, 1);
}

#[test]
fn ac4_duplicate_publishers_in_two_processes_converge_and_keep_history() {
    let root = private_tempdir();
    let directory = root.path();
    drop(open(directory));
    let paused = Paused::spawn(child("build", directory, "New"), "begun");
    let second = run(child("build", directory, "New"));
    let first = paused.resume();
    assert_ne!(
        first, second,
        "each seal takes its own publication sequence"
    );
    let store = open(directory);
    let sealed = sealed_snapshots(&store);
    assert_eq!(sealed.len(), 2);
    for header in &sealed {
        let members = read_members(&store, header).unwrap();
        assert_eq!(members[0].record_id, expected_record("New"));
        assert_eq!(targets_of(&store, header), [PathBuf::from(target("New"))]);
    }
    let connection = raw(directory);
    let count = |table: &str| -> i64 {
        connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    };
    assert_eq!(count("member_records"), 1, "identical records converge");
    assert_eq!(count("evidence_sets"), 1, "identical evidence converges");
}

/// Six sealed snapshots of two records each (two beyond `keep_snapshots`),
/// four stray artifacts and evidence sets, and one stray artifact large
/// enough that deleting it frees whole pages for the vacuum.
fn prepare(store: &Store) {
    for index in 0..6 {
        sealed(store, &records(store, &format!("S{index}"), 2));
    }
    for index in 0..4 {
        artifact(store, &format!("unit Stray{index};"));
        binding_evidence(store, &format!("stray{index}"), "/w/Stray.pas");
    }
    publish_bulk(store, "unit Big;", 9, 100_000).unwrap();
}

/// Every sealed snapshot still found reads back complete and verified.
fn sealed_read_verified(store: &Store) -> usize {
    let found = sealed_snapshots(store);
    for header in &found {
        read_members(store, header).expect("a retained snapshot reads back verified");
    }
    found.len()
}

fn first_word(result: &str) -> Option<&str> {
    result.split_whitespace().next()
}

#[test]
fn ac3_death_at_each_maintenance_boundary_keeps_the_invariant() {
    for point in FaultPoint::MAINTENANCE {
        let root = private_tempdir();
        let directory = root.path();
        let store = open(directory);
        prepare(&store);
        age_past_grace(&store);
        let role = if point == FaultPoint::ResetBeforeCommit {
            "reset"
        } else {
            "maintain"
        };
        abort_at(child(role, directory, ""), point);
        let after = open(directory);
        assert_eq!(
            after.scope().reset_epoch,
            0,
            "{point:?}: an aborted reset changed nothing"
        );
        assert_eq!(check_invariant(directory), 0, "{point:?}");
        assert!(
            sealed_read_verified(&after) >= 4,
            "{point:?}: the newest snapshots survive"
        );
        // The next pass resumes from the database and finishes.
        age_past_grace(&after);
        assert_eq!(
            first_word(&run(child("maintain", directory, ""))),
            Some("maintained")
        );
        assert_eq!(check_invariant(directory), 0, "{point:?}");
        assert_eq!(sealed_read_verified(&after), 4, "{point:?}");
        drop(store);
    }
}

#[test]
fn ac1_gc_racing_a_paused_publisher_preserves_what_it_references() {
    // A live builder's lease protects its build and records.
    let root = private_tempdir();
    let directory = root.path();
    let store = open(directory);
    let paused = Paused::spawn(child("build", directory, "New"), "begun");
    age_past_grace(&store);
    assert_eq!(
        first_word(&run(child("maintain", directory, ""))),
        Some("maintained")
    );
    assert!(paused.resume().starts_with("sealed"));
    assert_eq!(check_invariant(directory), 0);
    let headers = sealed_snapshots(&store);
    assert_eq!(
        targets_of(&store, &headers[0]),
        [PathBuf::from(target("New"))]
    );

    // A publisher inside its write transaction makes GC wait, not
    // interleave. The clock moves first: renewing while the child holds
    // the write lock would be refused.
    let root = private_tempdir();
    let directory = root.path();
    let store = open(directory);
    age_past_grace(&store);
    store.advance_clock_for_test(LEASE_EXPIRY);
    let paused = Paused::spawn(
        child("build", directory, "New"),
        FaultPoint::RecordBeforeCommit.name(),
    );
    assert_eq!(
        store.retire_step(RetireStep::Evidence, &EAGER),
        Err(Bypass::Busy)
    );
    assert!(paused.resume().starts_with("sealed"));
    assert_eq!(check_invariant(directory), 0);

    // A builder whose lease expired is fenced and rebuilds; nothing partial
    // seals.
    let root = private_tempdir();
    let directory = root.path();
    let store = open(directory);
    let paused = Paused::spawn(child("build-tolerant", directory, "New"), "begun");
    age_past_grace(&store);
    assert_eq!(
        first_word(&run(child("maintain", directory, "expire"))),
        Some("maintained")
    );
    assert_eq!(paused.resume(), "bypass BuilderFenced");
    assert_eq!(sealed_snapshots(&store).len(), 0);
    assert_eq!(check_invariant(directory), 0);
    let header = build(&store, "New", &|_| {});
    assert_eq!(
        targets_of(&store, &header),
        [PathBuf::from(target("New"))],
        "the retry succeeds"
    );
}

#[test]
fn ac1_a_reader_keeps_its_snapshot_while_another_process_deletes() {
    let root = private_tempdir();
    let directory = root.path();
    let store = open(directory);
    prepare(&store);
    let newest = sealed_snapshots(&store)[0].clone();
    age_past_grace(&store);
    let paused = Paused::spawn(
        child("maintain", directory, ""),
        FaultPoint::GcDeleteBeforeCommit.name(),
    );
    // The other process holds the write lock mid-deletion: live reads go on.
    assert_eq!(read_members(&store, &newest).map(|m| m.len()), Ok(2));
    assert!(paused.resume().starts_with("maintained"));
    assert_eq!(read_members(&store, &newest).map(|m| m.len()), Ok(2));
    assert_eq!(check_invariant(directory), 0);
}

#[test]
fn ac4_maintenance_in_two_processes_is_coordinated() {
    let root = private_tempdir();
    let directory = root.path();
    let store = open(directory);
    prepare(&store);
    age_past_grace(&store);
    let paused = Paused::spawn(
        child("maintain", directory, ""),
        FaultPoint::GcRetireBeforeCommit.name(),
    );
    let mut maintenance = Maintenance::new(EAGER).unwrap();
    let report = maintenance
        .step(
            &store,
            StepBudget {
                time: Duration::from_secs(60),
                rows: 1000,
            },
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(report.skipped && report.rows == 0, "{report:?}");
    assert!(paused.resume().starts_with("maintained"));
    assert_eq!(check_invariant(directory), 0);
}

#[test]
fn ac4_a_reset_in_one_process_fences_the_other_without_touching_files() {
    use std::os::unix::fs::MetadataExt;
    let root = private_tempdir();
    let directory = root.path();
    let store = open(directory);
    let header = sealed(&store, &records(&store, "M", 300));
    let mut reader = store.read_snapshot(&header);
    assert!(matches!(reader.next_batch(), Ok(Lookup::Hit(Some(_)))));
    let inode = std::fs::metadata(directory.join(DATABASE_FILE))
        .unwrap()
        .ino();
    assert_eq!(run(child("reset", directory, "")), "reset");
    assert_eq!(reader.next_batch().err(), Some(Bypass::Fenced));
    assert!(!reader.is_verified());
    assert_eq!(store.check_scope(), Err(Bypass::Fenced));
    assert_eq!(
        std::fs::metadata(directory.join(DATABASE_FILE))
            .unwrap()
            .ino(),
        inode
    );
    drop(store);
    assert_eq!(open(directory).scope().reset_epoch, 1);
}
