use super::outcome::Bypass;
use super::pool::Deadline;
use super::sqlite::{begin_within, classify, limit_wait};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// The physical container format. Phases that add tables increment it; a
/// store of another version is bypassed, never migrated (spec §6).
pub const CONTAINER_VERSION: i64 = 7;
/// `L4DC` in the SQLite header's `application_id`.
pub const APPLICATION_ID: i64 = 0x4C34_4443;
const DEFAULT_MAX_DISK_BYTES: i64 = 8 << 30;
const DEFAULT_WAL_THRESHOLD_BYTES: i64 = 256 << 20;

/// Container version 7. The numeric bounds repeat `limits.rs`: rows,
/// 1 MiB manifests/payloads/supplemental data, 16 MiB content and 4 KiB
/// index keys. Spec §6.1 (amended 2026-10-16) describes every table.
/// Spec §6.1 (amended 2026-10-27) adds evidence purposes and the publication sequence.
/// Spec §6.1 (amended 2026-11-03) adds sequences on artifacts and records, leases and
/// reference indexes.
/// Container 5 (l4d-zf1b.8, 13c-4) keys artifact rows by an INTEGER
/// surrogate of their artifact and stores each distinct observation once
/// per scope, listed by the evidence sets through `evidence_observations`.
/// The surrogates are internal: no identity, record or snapshot holds one.
/// Container 6 (l4d-zf1b.8, 13d-3) stores no per-row checksum: a read
/// verifies every row against the artifact's `fact_checksum`, which binds
/// each row to its artifact and ordinal (spec §6.2).
/// Container 7 (l4d-zf1b.9, l4d-rpfc) copies an INTEGER surrogate of the
/// artifact's scope (`scopes`) onto each artifact row and leads the name
/// index with it, so a lookup never visits another scope's rows. It has no
/// foreign key: only the publishing transaction writes it, and the lookup
/// still checks the scope through `artifacts`, so a wrong ref hides a row
/// and never returns another scope's.
const SCHEMA: &str = "
CREATE TABLE store_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    container_version INTEGER NOT NULL,
    policy_revision INTEGER NOT NULL CHECK (policy_revision >= 0),
    reset_epoch INTEGER NOT NULL CHECK (reset_epoch >= 0),
    max_disk_bytes INTEGER NOT NULL CHECK (max_disk_bytes > 0),
    wal_threshold_bytes INTEGER NOT NULL
        CHECK (wal_threshold_bytes > 0 AND wal_threshold_bytes < max_disk_bytes),
    publication_seq INTEGER NOT NULL CHECK (publication_seq >= 0)
) STRICT;

CREATE TABLE namespaces (
    namespace_id BLOB PRIMARY KEY CHECK (length(namespace_id) = 32),
    manifest BLOB NOT NULL CHECK (length(manifest) <= 1048576),
    reset_epoch INTEGER NOT NULL CHECK (reset_epoch >= 0),
    state INTEGER NOT NULL CHECK (state IN (1, 2)),
    created_unix_seconds INTEGER NOT NULL CHECK (created_unix_seconds >= 0)
) STRICT;

CREATE TABLE retired_scopes (
    namespace_id BLOB NOT NULL REFERENCES namespaces (namespace_id),
    reset_epoch INTEGER NOT NULL CHECK (reset_epoch >= 0),
    PRIMARY KEY (namespace_id, reset_epoch)
) STRICT;

CREATE TABLE key_manifests (
    namespace_id BLOB NOT NULL REFERENCES namespaces (namespace_id),
    reset_epoch INTEGER NOT NULL CHECK (reset_epoch >= 0),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    kind INTEGER NOT NULL CHECK (kind IN (2, 3, 4, 5, 7, 8)),
    manifest BLOB NOT NULL CHECK (length(manifest) BETWEEN 9 AND 1048576),
    PRIMARY KEY (namespace_id, reset_epoch, digest)
) STRICT;

CREATE TABLE artifacts (
    artifact_ref INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL,
    artifact_id BLOB NOT NULL,
    kind INTEGER NOT NULL CHECK (kind IN (2, 3)),
    root_key BLOB CHECK (root_key IS NULL OR length(root_key) = 32),
    lifecycle INTEGER NOT NULL CHECK (lifecycle IN (1, 2)),
    row_count INTEGER NOT NULL CHECK (row_count BETWEEN 0 AND 1048576),
    content_bytes INTEGER NOT NULL CHECK (content_bytes BETWEEN 0 AND 16777216),
    fact_checksum BLOB NOT NULL CHECK (length(fact_checksum) = 32),
    supplemental BLOB NOT NULL CHECK (length(supplemental) <= 1048576),
    publication_seq INTEGER NOT NULL CHECK (publication_seq >= 1),
    UNIQUE (namespace_id, reset_epoch, artifact_id),
    FOREIGN KEY (namespace_id, reset_epoch, artifact_id)
        REFERENCES key_manifests (namespace_id, reset_epoch, digest),
    FOREIGN KEY (namespace_id, reset_epoch, root_key)
        REFERENCES key_manifests (namespace_id, reset_epoch, digest),
    CHECK ((kind = 2) = (root_key IS NULL))
) STRICT;

CREATE TABLE scopes (
    scope_ref INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace_id BLOB NOT NULL REFERENCES namespaces (namespace_id),
    reset_epoch INTEGER NOT NULL CHECK (reset_epoch >= 0),
    UNIQUE (namespace_id, reset_epoch)
) STRICT;

CREATE TABLE artifact_rows (
    artifact_ref INTEGER NOT NULL REFERENCES artifacts (artifact_ref),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    scope_ref INTEGER NOT NULL,
    row_kind INTEGER NOT NULL CHECK (row_kind BETWEEN 1 AND 255),
    index_key BLOB CHECK (index_key IS NULL OR length(index_key) <= 4096),
    payload BLOB NOT NULL CHECK (length(payload) <= 1048576),
    PRIMARY KEY (artifact_ref, ordinal)
) STRICT, WITHOUT ROWID;

CREATE INDEX artifact_rows_by_index_key
    ON artifact_rows (scope_ref, row_kind, index_key, artifact_ref, ordinal)
    WHERE index_key IS NOT NULL;

CREATE INDEX artifacts_by_root
    ON artifacts (namespace_id, reset_epoch, root_key) WHERE root_key IS NOT NULL;

CREATE INDEX artifacts_by_lifecycle
    ON artifacts (namespace_id, reset_epoch, lifecycle, publication_seq);

CREATE TABLE evidence_sets (
    evidence_ref INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL,
    evidence_id BLOB NOT NULL CHECK (length(evidence_id) = 32),
    purpose INTEGER NOT NULL CHECK (purpose BETWEEN 1 AND 3),
    context_key BLOB NOT NULL CHECK (length(context_key) = 32),
    subject_key BLOB NOT NULL CHECK (length(subject_key) = 32),
    artifact_id BLOB CHECK (artifact_id IS NULL OR length(artifact_id) = 32),
    lifecycle INTEGER NOT NULL CHECK (lifecycle IN (1, 2)),
    observation_count INTEGER NOT NULL CHECK (observation_count BETWEEN 0 AND 65536),
    content_bytes INTEGER NOT NULL CHECK (content_bytes BETWEEN 0 AND 16777216),
    publication_seq INTEGER NOT NULL CHECK (publication_seq >= 1),
    UNIQUE (namespace_id, reset_epoch, evidence_id),
    FOREIGN KEY (namespace_id, reset_epoch, context_key)
        REFERENCES key_manifests (namespace_id, reset_epoch, digest),
    FOREIGN KEY (namespace_id, reset_epoch, subject_key)
        REFERENCES key_manifests (namespace_id, reset_epoch, digest),
    FOREIGN KEY (namespace_id, reset_epoch, artifact_id)
        REFERENCES artifacts (namespace_id, reset_epoch, artifact_id),
    CHECK ((purpose = 1) = (artifact_id IS NOT NULL))
) STRICT;

CREATE INDEX evidence_by_subject
    ON evidence_sets (namespace_id, reset_epoch, purpose, context_key, subject_key,
        publication_seq);

CREATE INDEX evidence_by_artifact
    ON evidence_sets (namespace_id, reset_epoch, artifact_id)
    WHERE artifact_id IS NOT NULL;

CREATE INDEX evidence_by_context ON evidence_sets (namespace_id, reset_epoch, context_key);

CREATE INDEX evidence_by_subject_key ON evidence_sets (namespace_id, reset_epoch, subject_key);

CREATE INDEX evidence_by_lifecycle
    ON evidence_sets (namespace_id, reset_epoch, lifecycle, publication_seq);

CREATE TABLE observation_content (
    observation INTEGER PRIMARY KEY,
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL CHECK (reset_epoch >= 0),
    content_digest BLOB NOT NULL CHECK (length(content_digest) = 32),
    kind INTEGER NOT NULL CHECK (kind BETWEEN 1 AND 3),
    path_key BLOB NOT NULL CHECK (length(path_key) <= 4096),
    payload BLOB NOT NULL CHECK (length(payload) <= 1048576),
    UNIQUE (namespace_id, reset_epoch, content_digest)
) STRICT;

CREATE INDEX observation_content_by_path
    ON observation_content (namespace_id, reset_epoch, path_key);

CREATE TABLE evidence_observations (
    evidence_ref INTEGER NOT NULL REFERENCES evidence_sets (evidence_ref),
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    observation INTEGER NOT NULL REFERENCES observation_content (observation),
    PRIMARY KEY (evidence_ref, ordinal)
) STRICT, WITHOUT ROWID;

CREATE INDEX evidence_observations_by_observation
    ON evidence_observations (observation, evidence_ref);

CREATE TABLE member_records (
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL,
    record_id BLOB NOT NULL CHECK (length(record_id) = 32),
    lifecycle INTEGER NOT NULL CHECK (lifecycle IN (1, 2)),
    header BLOB NOT NULL CHECK (length(header) <= 1048576),
    source_key BLOB NOT NULL CHECK (length(source_key) <= 4096),
    artifact_id BLOB CHECK (artifact_id IS NULL OR length(artifact_id) = 32),
    member_evidence BLOB CHECK (member_evidence IS NULL OR length(member_evidence) = 32),
    binding_count INTEGER NOT NULL CHECK (binding_count BETWEEN 0 AND 65536),
    content_bytes INTEGER NOT NULL CHECK (content_bytes BETWEEN 0 AND 16777216),
    publication_seq INTEGER NOT NULL CHECK (publication_seq >= 1),
    PRIMARY KEY (namespace_id, reset_epoch, record_id),
    FOREIGN KEY (namespace_id, reset_epoch, artifact_id)
        REFERENCES artifacts (namespace_id, reset_epoch, artifact_id),
    FOREIGN KEY (namespace_id, reset_epoch, member_evidence)
        REFERENCES evidence_sets (namespace_id, reset_epoch, evidence_id),
    CHECK (member_evidence IS NULL OR artifact_id IS NOT NULL)
) STRICT;

CREATE INDEX member_records_by_artifact
    ON member_records (namespace_id, reset_epoch, artifact_id)
    WHERE artifact_id IS NOT NULL;

CREATE INDEX member_records_by_evidence
    ON member_records (namespace_id, reset_epoch, member_evidence)
    WHERE member_evidence IS NOT NULL;

CREATE INDEX member_records_by_lifecycle
    ON member_records (namespace_id, reset_epoch, lifecycle, publication_seq);

CREATE TABLE member_bindings (
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL,
    record_id BLOB NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    kind INTEGER NOT NULL CHECK (kind IN (1, 2)),
    outcome INTEGER NOT NULL CHECK (outcome BETWEEN 1 AND 4),
    target_key BLOB CHECK (target_key IS NULL OR length(target_key) <= 4096),
    evidence_id BLOB CHECK (evidence_id IS NULL OR length(evidence_id) = 32),
    payload BLOB NOT NULL CHECK (length(payload) <= 1048576),
    PRIMARY KEY (namespace_id, reset_epoch, record_id, ordinal),
    FOREIGN KEY (namespace_id, reset_epoch, record_id)
        REFERENCES member_records (namespace_id, reset_epoch, record_id),
    FOREIGN KEY (namespace_id, reset_epoch, evidence_id)
        REFERENCES evidence_sets (namespace_id, reset_epoch, evidence_id),
    CHECK ((outcome = 1) = (target_key IS NOT NULL))
) STRICT;

CREATE TABLE binding_targets (
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL,
    record_id BLOB NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    target_key BLOB NOT NULL CHECK (length(target_key) <= 4096),
    dependency INTEGER NOT NULL CHECK (dependency IN (1, 2, 3)),
    PRIMARY KEY (namespace_id, reset_epoch, record_id, ordinal, target_key),
    FOREIGN KEY (namespace_id, reset_epoch, record_id, ordinal)
        REFERENCES member_bindings (namespace_id, reset_epoch, record_id, ordinal),
    CHECK ((dependency = 3) = (length(target_key) = 0))
) STRICT;

CREATE INDEX binding_targets_by_target
    ON binding_targets (namespace_id, reset_epoch, target_key, record_id, ordinal);

CREATE INDEX member_bindings_by_evidence
    ON member_bindings (namespace_id, reset_epoch, evidence_id, record_id)
    WHERE evidence_id IS NOT NULL;

CREATE TABLE snapshots (
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL,
    snapshot_id BLOB NOT NULL CHECK (length(snapshot_id) = 16),
    context_key BLOB NOT NULL CHECK (length(context_key) = 32),
    state INTEGER NOT NULL CHECK (state IN (1, 2, 3)),
    builder_token BLOB NOT NULL CHECK (length(builder_token) = 16),
    build_revision INTEGER NOT NULL CHECK (build_revision >= 0),
    member_count INTEGER NOT NULL CHECK (member_count BETWEEN 0 AND 100000),
    discovery_evidence BLOB
        CHECK (discovery_evidence IS NULL OR length(discovery_evidence) = 32),
    coverage BLOB CHECK (coverage IS NULL OR length(coverage) <= 64),
    sealed_digest BLOB CHECK (sealed_digest IS NULL OR length(sealed_digest) = 32),
    publication_seq INTEGER CHECK (publication_seq IS NULL OR publication_seq >= 1),
    PRIMARY KEY (namespace_id, reset_epoch, snapshot_id),
    FOREIGN KEY (namespace_id, reset_epoch, context_key)
        REFERENCES key_manifests (namespace_id, reset_epoch, digest),
    FOREIGN KEY (namespace_id, reset_epoch, discovery_evidence)
        REFERENCES evidence_sets (namespace_id, reset_epoch, evidence_id),
    CHECK (state <> 1
        OR (coverage IS NULL AND sealed_digest IS NULL AND publication_seq IS NULL)),
    CHECK (state <> 2
        OR (coverage IS NOT NULL AND sealed_digest IS NOT NULL AND publication_seq IS NOT NULL))
) STRICT;

CREATE INDEX snapshots_by_context
    ON snapshots (namespace_id, reset_epoch, context_key, state, publication_seq);

CREATE INDEX snapshots_by_discovery
    ON snapshots (namespace_id, reset_epoch, discovery_evidence)
    WHERE discovery_evidence IS NOT NULL;

CREATE INDEX snapshots_by_state ON snapshots (namespace_id, reset_epoch, state);

CREATE TABLE snapshot_members (
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL,
    snapshot_id BLOB NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    record_id BLOB NOT NULL CHECK (length(record_id) = 32),
    source_key BLOB NOT NULL CHECK (length(source_key) <= 4096),
    PRIMARY KEY (namespace_id, reset_epoch, snapshot_id, ordinal),
    UNIQUE (namespace_id, reset_epoch, snapshot_id, source_key),
    FOREIGN KEY (namespace_id, reset_epoch, snapshot_id)
        REFERENCES snapshots (namespace_id, reset_epoch, snapshot_id),
    FOREIGN KEY (namespace_id, reset_epoch, record_id)
        REFERENCES member_records (namespace_id, reset_epoch, record_id)
) STRICT;

CREATE INDEX snapshot_members_by_record
    ON snapshot_members (namespace_id, reset_epoch, record_id, snapshot_id);

CREATE TABLE process_leases (
    namespace_id BLOB NOT NULL REFERENCES namespaces (namespace_id),
    reset_epoch INTEGER NOT NULL CHECK (reset_epoch >= 0),
    lease_id BLOB NOT NULL CHECK (length(lease_id) = 16),
    renewed_unix_seconds INTEGER NOT NULL CHECK (renewed_unix_seconds >= 0),
    renewal_seq INTEGER NOT NULL CHECK (renewal_seq >= 0),
    floor_seq INTEGER NOT NULL CHECK (floor_seq BETWEEN 0 AND renewal_seq),
    PRIMARY KEY (namespace_id, reset_epoch, lease_id)
) STRICT;

CREATE TABLE lease_holds (
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL,
    lease_id BLOB NOT NULL,
    snapshot_id BLOB NOT NULL,
    PRIMARY KEY (namespace_id, reset_epoch, lease_id, snapshot_id),
    FOREIGN KEY (namespace_id, reset_epoch, lease_id)
        REFERENCES process_leases (namespace_id, reset_epoch, lease_id),
    FOREIGN KEY (namespace_id, reset_epoch, snapshot_id)
        REFERENCES snapshots (namespace_id, reset_epoch, snapshot_id)
) STRICT;

CREATE INDEX lease_holds_by_snapshot
    ON lease_holds (namespace_id, reset_epoch, snapshot_id, lease_id);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Container {
    Fresh,
    Current,
}

fn classify_container(
    application_id: i64,
    user_version: i64,
    tables: i64,
) -> Result<Container, Bypass> {
    match (application_id, user_version) {
        (0, 0) if tables == 0 => Ok(Container::Fresh),
        (APPLICATION_ID, CONTAINER_VERSION) => Ok(Container::Current),
        (APPLICATION_ID, version) => Err(Bypass::UnsupportedContainer { version }),
        _ => Err(Bypass::ForeignDatabase),
    }
}

/// Where a test may let another opener act.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookPoint {
    Inspected,
    BeforeWal,
    WalBusy,
    WalRetried,
}

#[cfg(test)]
type Hook = Box<dyn FnMut(HookPoint)>;

#[cfg(test)]
thread_local! {
    static HOOK: std::cell::RefCell<Option<Hook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_hook(hook: Hook) {
    HOOK.set(Some(hook));
}

/// The hook is taken while it runs, so an opener it starts does not reenter it.
#[cfg(test)]
fn run_hook(point: HookPoint) {
    if let Some(mut hook) = HOOK.take() {
        hook(point);
        HOOK.with_borrow_mut(|slot| {
            if slot.is_none() {
                *slot = Some(hook);
            }
        });
    }
}

/// Read-only inspection: nothing is written to a file that is not ours.
/// The header and schema are read in one statement, so one snapshot: read
/// separately, a creation committed between them looks foreign.
pub(crate) fn inspect(connection: &Connection) -> Result<Container, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let (application_id, user_version, tables): (i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT application_id FROM pragma_application_id), \
             (SELECT user_version FROM pragma_user_version), \
             (SELECT count(*) FROM sqlite_schema)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(fail)?;
    #[cfg(test)]
    run_hook(HookPoint::Inspected);
    let container = classify_container(application_id, user_version, tables)?;
    if container == Container::Current {
        let version: Option<i64> = connection
            .query_row(
                "SELECT container_version FROM store_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|_| Bypass::Corrupt)?;
        if version != Some(CONTAINER_VERSION) {
            return Err(Bypass::Corrupt);
        }
    }
    Ok(container)
}

/// Validate the container, creating it in a new or empty file. WAL mode is
/// confirmed, not assumed.
pub(crate) fn prepare_container(
    connection: &mut Connection,
    deadline: Deadline,
) -> Result<(), Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    limit_wait(connection, deadline)?;
    let container = inspect(connection)?;
    if container == Container::Fresh {
        // Takes effect only before the first table exists.
        limit_wait(connection, deadline)?;
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .map_err(fail)?;
    }
    #[cfg(test)]
    run_hook(HookPoint::BeforeWal);
    let mode = switch_to_wal(connection, deadline)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Bypass::WalUnavailable);
    }
    if container == Container::Fresh {
        create(connection, deadline)?;
    }
    Ok(())
}

/// The switch reads the header, then upgrades to a write lock while holding
/// that read, and SQLite does not wait on that upgrade (a holder of a read
/// lock waiting for a writer may be what blocks it). So a busy switch waits
/// for the writer through an immediate transaction, which does wait, then
/// tries again.
fn switch_to_wal(connection: &mut Connection, deadline: Deadline) -> Result<String, Bypass> {
    loop {
        limit_wait(connection, deadline)?;
        match connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0)) {
            Ok(mode) => return Ok(mode),
            Err(error) => {
                let bypass = classify(&error);
                if bypass != Bypass::Busy {
                    return Err(bypass);
                }
            }
        }
        // A reader blocks the switch but not the transaction below, so only
        // the deadline ends the retries.
        if deadline.remaining().is_zero() {
            super::waits::record_busy();
            return Err(Bypass::Busy);
        }
        #[cfg(test)]
        run_hook(HookPoint::WalBusy);
        let transaction = begin_within(connection, TransactionBehavior::Immediate, deadline)?;
        transaction.commit().map_err(|error| classify(&error))?;
        #[cfg(test)]
        run_hook(HookPoint::WalRetried);
    }
}

fn create(connection: &mut Connection, deadline: Deadline) -> Result<(), Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let transaction = begin_within(connection, TransactionBehavior::Immediate, deadline)?;
    // Another process may have created the container since `inspect`.
    if inspect(&transaction)? == Container::Current {
        return Ok(());
    }
    transaction.execute_batch(SCHEMA).map_err(fail)?;
    transaction
        .execute(
            "INSERT INTO store_state (singleton, container_version, policy_revision, \
             reset_epoch, max_disk_bytes, wal_threshold_bytes, publication_seq) \
             VALUES (1, ?1, 0, 0, ?2, ?3, 0)",
            params![
                CONTAINER_VERSION,
                DEFAULT_MAX_DISK_BYTES,
                DEFAULT_WAL_THRESHOLD_BYTES
            ],
        )
        .map_err(fail)?;
    transaction
        .pragma_update(None, "application_id", APPLICATION_ID)
        .map_err(fail)?;
    transaction
        .pragma_update(None, "user_version", CONTAINER_VERSION)
        .map_err(fail)?;
    transaction.commit().map_err(fail)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::store::sqlite::open_connection;
    use std::path::Path;
    use std::time::Duration;

    fn deadline() -> Deadline {
        Deadline::after(Duration::from_secs(2))
    }

    fn connect(path: &Path) -> Connection {
        let connection = open_connection(path, 1 << 20, Duration::from_secs(2)).unwrap();
        connection.busy_timeout(Duration::from_secs(2)).unwrap();
        connection
    }

    fn tables(connection: &Connection) -> Vec<String> {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn a_fresh_file_gets_the_current_schema() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = connect(&root.path().join("a.sqlite3"));
        prepare_container(&mut connection, deadline()).unwrap();
        let pragma = |name: &str| -> i64 {
            connection
                .pragma_query_value(None, name, |row| row.get(0))
                .unwrap()
        };
        assert_eq!(pragma("application_id"), APPLICATION_ID);
        assert_eq!(pragma("user_version"), CONTAINER_VERSION);
        assert_eq!(pragma("auto_vacuum"), 2, "incremental");
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(
            tables(&connection),
            [
                "artifact_rows",
                "artifacts",
                "binding_targets",
                "evidence_observations",
                "evidence_sets",
                "key_manifests",
                "lease_holds",
                "member_bindings",
                "member_records",
                "namespaces",
                "observation_content",
                "process_leases",
                "retired_scopes",
                "scopes",
                "snapshot_members",
                "snapshots",
                "sqlite_sequence",
                "store_state"
            ]
        );
        let state: (i64, i64, i64, i64, i64, i64) = connection
            .query_row(
                "SELECT container_version, policy_revision, reset_epoch, max_disk_bytes, \
                 wal_threshold_bytes, publication_seq FROM store_state",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(state, (CONTAINER_VERSION, 0, 0, 8 << 30, 256 << 20, 0));
    }

    /// Every reference a per-row deletion checks is served by an index
    /// whose leading columns are the scope and the referencing columns,
    /// so deleting a parent never scans its children (spec §6.1, 2026-11-03).
    #[test]
    fn every_reference_is_indexed_by_its_scope_and_columns() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = connect(&root.path().join("a.sqlite3"));
        prepare_container(&mut connection, deadline()).unwrap();
        let names = |sql: &str| -> Vec<String> {
            connection
                .prepare(sql)
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let tables = names("SELECT name FROM sqlite_schema WHERE type = 'table'");
        for table in &tables {
            let mut references: std::collections::BTreeMap<i64, Vec<String>> = Default::default();
            let mut statement = connection
                .prepare(&format!(
                    "SELECT id, \"from\", \"table\" FROM pragma_foreign_key_list('{table}') ORDER BY id, seq"
                ))
                .unwrap();
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .unwrap();
            for row in rows {
                let (id, from, parent) = row.unwrap();
                if parent != "namespaces" {
                    references.entry(id).or_default().push(from);
                }
            }
            let indexes = names(&format!("SELECT name FROM pragma_index_list('{table}')"));
            for columns in references.values() {
                let covered = indexes.iter().any(|index| {
                    let leading = names(&format!(
                        "SELECT name FROM pragma_index_info('{index}') ORDER BY seqno"
                    ));
                    leading.len() >= columns.len() && leading[..columns.len()] == columns[..]
                });
                assert!(covered, "{table} ({columns:?}) has no index");
            }
        }
    }

    #[test]
    fn reopening_an_existing_container_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        prepare_container(&mut connect(&path), deadline()).unwrap();
        let mut again = connect(&path);
        prepare_container(&mut again, deadline()).unwrap();
        let rows: i64 = again
            .query_row("SELECT count(*) FROM store_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn an_unsupported_container_is_bypassed_and_left_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        {
            let mut connection = connect(&path);
            prepare_container(&mut connection, deadline()).unwrap();
            connection
                .pragma_update(None, "user_version", CONTAINER_VERSION + 1)
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            prepare_container(&mut connect(&path), deadline()),
            Err(Bypass::UnsupportedContainer {
                version: CONTAINER_VERSION + 1
            })
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn older_containers_are_bypassed_not_migrated() {
        for version in [1, 2, 3, 4, 5, 6] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("a.sqlite3");
            {
                let mut connection = connect(&path);
                prepare_container(&mut connection, deadline()).unwrap();
                connection
                    .pragma_update(None, "user_version", version)
                    .unwrap();
            }
            let before = std::fs::read(&path).unwrap();
            assert_eq!(
                prepare_container(&mut connect(&path), deadline()),
                Err(Bypass::UnsupportedContainer { version })
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }

    #[test]
    fn a_foreign_database_is_bypassed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        connect(&path)
            .execute_batch("CREATE TABLE other (a INTEGER);")
            .unwrap();
        assert_eq!(
            prepare_container(&mut connect(&path), deadline()),
            Err(Bypass::ForeignDatabase)
        );
        let other = root.path().join("b.sqlite3");
        connect(&other)
            .pragma_update(None, "application_id", 7)
            .unwrap();
        assert_eq!(
            prepare_container(&mut connect(&other), deadline()),
            Err(Bypass::ForeignDatabase)
        );
    }

    #[test]
    fn a_garbage_file_is_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        std::fs::write(&path, vec![0x5a; 8192]).unwrap();
        // The connection settings already read the header, so either step reports it.
        let opened =
            open_connection(&path, 1 << 20, Duration::from_secs(2)).and_then(|mut connection| {
                connection.busy_timeout(Duration::from_secs(2)).unwrap();
                prepare_container(&mut connection, deadline())
            });
        assert_eq!(opened, Err(Bypass::Corrupt));
    }

    #[test]
    fn a_container_without_its_state_row_is_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        prepare_container(&mut connect(&path), deadline()).unwrap();
        connect(&path)
            .execute_batch("DELETE FROM store_state;")
            .unwrap();
        assert_eq!(
            prepare_container(&mut connect(&path), deadline()),
            Err(Bypass::Corrupt)
        );
    }

    #[test]
    fn concurrent_creation_converges_on_one_container() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        let mut connection = connect(&path);
                        barrier.wait();
                        prepare_container(&mut connection, deadline())
                    })
                })
                .collect();
            for handle in handles {
                assert_eq!(handle.join().unwrap(), Ok(()));
            }
        });
        let connection = connect(&path);
        let rows: i64 = connection
            .query_row("SELECT count(*) FROM store_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_container_created_during_inspection_is_not_foreign() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        let mut connection = connect(&path);
        let creator = path.clone();
        let mut created = false;
        set_hook(Box::new(move |point| {
            if point == HookPoint::Inspected && !created {
                created = true;
                prepare_container(&mut connect(&creator), deadline()).unwrap();
            }
        }));
        assert_eq!(prepare_container(&mut connection, deadline()), Ok(()));
        let rows: i64 = connection
            .query_row("SELECT count(*) FROM store_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[test]
    fn a_wal_switch_waits_for_a_concurrent_writer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        let mut connection = connect(&path);
        let writer = connect(&path);
        // The writer takes the lock just before the switch and releases it
        // once the switch has hit it.
        set_hook(Box::new(move |point| match point {
            HookPoint::BeforeWal => writer.execute_batch("BEGIN IMMEDIATE").unwrap(),
            HookPoint::WalBusy if !writer.is_autocommit() => {
                writer.execute_batch("COMMIT").unwrap()
            }
            _ => {}
        }));
        assert_eq!(prepare_container(&mut connection, deadline()), Ok(()));
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    /// A reader that lets go for each retry and reads again before the
    /// next switch never stops `BEGIN IMMEDIATE`, so only the deadline ends
    /// the retries.
    #[test]
    fn a_wal_switch_blocked_by_a_returning_reader_gives_up_at_the_deadline() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        prepare_container(&mut connect(&path), deadline()).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let opener = path.clone();
        std::thread::spawn(move || {
            let reader = connect(&opener);
            let _: String = reader
                .pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))
                .unwrap();
            let read = |reader: &Connection| {
                reader.execute_batch("BEGIN").unwrap();
                let _: i64 = reader
                    .query_row("SELECT count(*) FROM store_state", [], |row| row.get(0))
                    .unwrap();
            };
            read(&reader);
            set_hook(Box::new(move |point| match point {
                HookPoint::WalBusy => reader.execute_batch("COMMIT").unwrap(),
                HookPoint::WalRetried => read(&reader),
                _ => {}
            }));
            let mut connection = connect(&opener);
            let short = Deadline::after(Duration::from_millis(50));
            let _ = sender.send(prepare_container(&mut connection, short));
        });
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(5)),
            Ok(Err(Bypass::Busy))
        );
    }

    #[test]
    fn the_schema_rejects_out_of_range_rows() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = connect(&root.path().join("a.sqlite3"));
        prepare_container(&mut connection, deadline()).unwrap();
        let error = connection
            .execute(
                "INSERT INTO namespaces VALUES (?1, x'00', 0, 3, 0)",
                [vec![0u8; 32]],
            )
            .unwrap_err();
        assert!(error.to_string().contains("CHECK"), "{error}");
    }

    #[test]
    fn prepare_classifies_a_damaged_file_itself() {
        let root = tempfile::tempdir().unwrap();
        // A raw connection runs no pragmas, so `prepare_container` is the
        // first to read the file.
        let garbage = root.path().join("garbage.sqlite3");
        std::fs::write(&garbage, vec![0x5a; 8192]).unwrap();
        assert_eq!(
            prepare_container(&mut Connection::open(&garbage).unwrap(), deadline()),
            Err(Bypass::Corrupt)
        );
        // A valid header over a damaged schema page.
        let damaged = root.path().join("damaged.sqlite3");
        prepare_container(&mut connect(&damaged), deadline()).unwrap();
        let mut bytes = std::fs::read(&damaged).unwrap();
        bytes[100..1024].fill(0xff);
        std::fs::write(&damaged, &bytes).unwrap();
        assert_eq!(
            prepare_container(&mut Connection::open(&damaged).unwrap(), deadline()),
            Err(Bypass::Corrupt)
        );
    }

    #[test]
    fn a_database_without_wal_support_is_bypassed() {
        // An in-memory database keeps its `memory` journal.
        let mut connection = Connection::open_in_memory().unwrap();
        assert_eq!(
            prepare_container(&mut connection, deadline()),
            Err(Bypass::WalUnavailable)
        );
    }

    #[test]
    fn schema_bounds_match_the_store_limits() {
        use crate::store::limits::{
            MAX_ARTIFACT_CONTENT_BYTES, MAX_ARTIFACT_ROWS, MAX_EVIDENCE_OBSERVATIONS,
            MAX_INDEX_KEY_BYTES, MAX_MANIFEST_BYTES, MAX_MEMBER_BINDINGS, MAX_ROW_BATCH_BYTES,
            MAX_SNAPSHOT_MEMBERS, MAX_SUPPLEMENTAL_BYTES,
        };
        let root = tempfile::tempdir().unwrap();
        let mut connection = connect(&root.path().join("a.sqlite3"));
        prepare_container(&mut connection, deadline()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO namespaces VALUES (zeroblob(32), x'00', 0, 1, 0);
                 INSERT INTO key_manifests VALUES (zeroblob(32), 0, zeroblob(32), 2, zeroblob(9));
                 INSERT INTO artifacts VALUES
                     (1, zeroblob(32), 0, zeroblob(32), 2, NULL, 1, 0, 0, zeroblob(32), x'', 1);
                 INSERT INTO key_manifests VALUES
                     (zeroblob(32), 0, CAST(zeroblob(31) || x'01' AS BLOB), 4, zeroblob(9));
                 INSERT INTO evidence_sets VALUES
                     (1, zeroblob(32), 0, zeroblob(32), 1, CAST(zeroblob(31) || x'01' AS BLOB),
                      zeroblob(32), zeroblob(32), 1, 0, 0, 1);
                 INSERT INTO member_records VALUES
                     (zeroblob(32), 0, zeroblob(32), 1, x'', x'', NULL, NULL, 0, 0, 1);
                 INSERT INTO snapshots VALUES
                     (zeroblob(32), 0, zeroblob(16), CAST(zeroblob(31) || x'01' AS BLOB), 1,
                      zeroblob(16), 0, 0, NULL, NULL, NULL, NULL);",
            )
            .unwrap();
        // Each statement takes the bound as ?1: accepted at the limit,
        // refused by a CHECK one past it.
        let cases = [
            (
                "UPDATE namespaces SET manifest = zeroblob(?1)",
                MAX_MANIFEST_BYTES,
            ),
            (
                "UPDATE key_manifests SET manifest = zeroblob(?1)",
                MAX_MANIFEST_BYTES,
            ),
            ("UPDATE artifacts SET row_count = ?1", MAX_ARTIFACT_ROWS),
            (
                "UPDATE artifacts SET content_bytes = ?1",
                MAX_ARTIFACT_CONTENT_BYTES,
            ),
            (
                "UPDATE artifacts SET supplemental = zeroblob(?1)",
                MAX_SUPPLEMENTAL_BYTES,
            ),
            (
                "INSERT INTO artifact_rows VALUES (1, 0, 1, 1, zeroblob(?1), x'')",
                MAX_INDEX_KEY_BYTES,
            ),
            (
                "INSERT INTO artifact_rows VALUES (1, 0, 1, 1, NULL, zeroblob(?1))",
                MAX_ROW_BATCH_BYTES,
            ),
            (
                "UPDATE evidence_sets SET observation_count = ?1",
                MAX_EVIDENCE_OBSERVATIONS,
            ),
            (
                "UPDATE evidence_sets SET content_bytes = ?1",
                MAX_ARTIFACT_CONTENT_BYTES,
            ),
            (
                "INSERT INTO observation_content VALUES \
                 (1, zeroblob(32), 0, zeroblob(32), 1, zeroblob(?1), x'')",
                MAX_INDEX_KEY_BYTES,
            ),
            (
                "INSERT INTO observation_content VALUES \
                 (1, zeroblob(32), 0, zeroblob(32), 1, x'', zeroblob(?1))",
                MAX_ROW_BATCH_BYTES,
            ),
            (
                "UPDATE member_records SET header = zeroblob(?1)",
                MAX_MANIFEST_BYTES,
            ),
            (
                "UPDATE member_records SET source_key = zeroblob(?1)",
                MAX_INDEX_KEY_BYTES,
            ),
            (
                "UPDATE member_records SET binding_count = ?1",
                MAX_MEMBER_BINDINGS,
            ),
            (
                "UPDATE member_records SET content_bytes = ?1",
                MAX_ARTIFACT_CONTENT_BYTES,
            ),
            (
                "INSERT INTO member_bindings VALUES \
                 (zeroblob(32), 0, zeroblob(32), 0, 1, 1, zeroblob(?1), NULL, x'')",
                MAX_INDEX_KEY_BYTES,
            ),
            (
                "INSERT INTO member_bindings VALUES \
                 (zeroblob(32), 0, zeroblob(32), 0, 1, 3, NULL, NULL, zeroblob(?1))",
                MAX_ROW_BATCH_BYTES,
            ),
            (
                "UPDATE snapshots SET member_count = ?1",
                MAX_SNAPSHOT_MEMBERS,
            ),
            (
                "INSERT INTO snapshot_members VALUES \
                 (zeroblob(32), 0, zeroblob(16), 0, zeroblob(32), zeroblob(?1))",
                MAX_INDEX_KEY_BYTES,
            ),
        ];
        for (sql, limit) in cases {
            let at = connection.execute(sql, [limit as i64]);
            assert!(at.is_ok(), "{sql} at {limit}: {at:?}");
            connection.execute("DELETE FROM artifact_rows", []).unwrap();
            connection
                .execute("DELETE FROM observation_content", [])
                .unwrap();
            connection
                .execute("DELETE FROM member_bindings", [])
                .unwrap();
            connection
                .execute("DELETE FROM snapshot_members", [])
                .unwrap();
            let past = connection.execute(sql, [limit as i64 + 1]).unwrap_err();
            assert!(
                past.to_string().contains("CHECK"),
                "{sql} past {limit}: {past}"
            );
        }
    }

    #[test]
    fn only_artifact_evidence_names_an_artifact() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = connect(&root.path().join("a.sqlite3"));
        prepare_container(&mut connection, deadline()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO namespaces VALUES (zeroblob(32), x'00', 0, 1, 0);
                 INSERT INTO key_manifests VALUES (zeroblob(32), 0, zeroblob(32), 2, zeroblob(9));
                 INSERT INTO artifacts VALUES
                     (1, zeroblob(32), 0, zeroblob(32), 2, NULL, 1, 0, 0, zeroblob(32), x'', 1);",
            )
            .unwrap();
        for (purpose, artifact) in [(1, "NULL"), (2, "zeroblob(32)"), (3, "zeroblob(32)")] {
            let error = connection
                .execute(
                    &format!(
                        "INSERT INTO evidence_sets VALUES (NULL, zeroblob(32), 0, zeroblob(32), {purpose}, \
                         zeroblob(32), zeroblob(32), {artifact}, 1, 0, 0, 1)"
                    ),
                    [],
                )
                .unwrap_err();
            assert!(error.to_string().contains("CHECK"), "{purpose}: {error}");
        }
        for (purpose, artifact) in [(1, "zeroblob(32)"), (2, "NULL"), (3, "NULL")] {
            let id = format!("x'{}{purpose:02x}'", "00".repeat(31));
            connection
                .execute(
                    &format!(
                        "INSERT INTO evidence_sets VALUES (NULL, zeroblob(32), 0, {id}, {purpose}, \
                         zeroblob(32), zeroblob(32), {artifact}, 1, 0, 0, 1)"
                    ),
                    [],
                )
                .unwrap_or_else(|error| panic!("{purpose}: {error}"));
        }
    }

    #[test]
    fn only_a_sealed_snapshot_carries_its_digest_and_sequence() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = connect(&root.path().join("a.sqlite3"));
        prepare_container(&mut connection, deadline()).unwrap();
        connection
            .execute_batch(
                "INSERT INTO namespaces VALUES (zeroblob(32), x'00', 0, 1, 0);
                 INSERT INTO key_manifests VALUES (zeroblob(32), 0, zeroblob(32), 4, zeroblob(9));",
            )
            .unwrap();
        let insert = |state: i64, sealed: &str| {
            connection.execute(
                &format!(
                    "INSERT INTO snapshots VALUES (zeroblob(32), 0, zeroblob(16), zeroblob(32), \
                     {state}, zeroblob(16), 0, 0, NULL, {sealed})"
                ),
                [],
            )
        };
        for (state, sealed) in [
            (1, "x'0000', zeroblob(32), 1"),
            (2, "NULL, NULL, NULL"),
            (2, "x'0000', NULL, 1"),
        ] {
            let error = insert(state, sealed).unwrap_err();
            assert!(
                error.to_string().contains("CHECK"),
                "{state} {sealed}: {error}"
            );
        }
        assert!(insert(2, "x'0000', zeroblob(32), 1").is_ok());
    }
}
