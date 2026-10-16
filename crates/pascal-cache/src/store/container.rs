use super::outcome::Bypass;
use super::sqlite::{begin, classify};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// The physical container format. Phases that add tables increment it; a
/// store of another version is bypassed, never migrated (spec §6).
pub const CONTAINER_VERSION: i64 = 1;
/// `L4DC` in the SQLite header's `application_id`.
pub const APPLICATION_ID: i64 = 0x4C34_4443;
const DEFAULT_MAX_DISK_BYTES: i64 = 8 << 30;
const DEFAULT_WAL_THRESHOLD_BYTES: i64 = 256 << 20;

/// Container version 1. The numeric bounds repeat `limits.rs`: rows,
/// 1 MiB manifests/payloads/supplemental data, 16 MiB content and 4 KiB
/// index keys. Spec §6.1 (amended 2026-10-16) describes every table.
const SCHEMA: &str = "
CREATE TABLE store_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    container_version INTEGER NOT NULL,
    policy_revision INTEGER NOT NULL CHECK (policy_revision >= 0),
    reset_epoch INTEGER NOT NULL CHECK (reset_epoch >= 0),
    max_disk_bytes INTEGER NOT NULL CHECK (max_disk_bytes > 0),
    wal_threshold_bytes INTEGER NOT NULL
        CHECK (wal_threshold_bytes > 0 AND wal_threshold_bytes < max_disk_bytes)
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
    kind INTEGER NOT NULL CHECK (kind BETWEEN 2 AND 5),
    manifest BLOB NOT NULL CHECK (length(manifest) BETWEEN 9 AND 1048576),
    PRIMARY KEY (namespace_id, reset_epoch, digest)
) STRICT;

CREATE TABLE artifacts (
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
    PRIMARY KEY (namespace_id, reset_epoch, artifact_id),
    FOREIGN KEY (namespace_id, reset_epoch, artifact_id)
        REFERENCES key_manifests (namespace_id, reset_epoch, digest),
    FOREIGN KEY (namespace_id, reset_epoch, root_key)
        REFERENCES key_manifests (namespace_id, reset_epoch, digest),
    CHECK ((kind = 2) = (root_key IS NULL))
) STRICT;

CREATE TABLE artifact_rows (
    namespace_id BLOB NOT NULL,
    reset_epoch INTEGER NOT NULL,
    artifact_id BLOB NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    row_kind INTEGER NOT NULL CHECK (row_kind BETWEEN 1 AND 255),
    index_key BLOB CHECK (index_key IS NULL OR length(index_key) <= 4096),
    payload BLOB NOT NULL CHECK (length(payload) <= 1048576),
    row_checksum BLOB NOT NULL CHECK (length(row_checksum) = 32),
    PRIMARY KEY (namespace_id, reset_epoch, artifact_id, ordinal),
    FOREIGN KEY (namespace_id, reset_epoch, artifact_id)
        REFERENCES artifacts (namespace_id, reset_epoch, artifact_id)
) STRICT;

CREATE INDEX artifact_rows_by_index_key
    ON artifact_rows (namespace_id, reset_epoch, row_kind, index_key, artifact_id, ordinal)
    WHERE index_key IS NOT NULL;
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
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

/// Read-only inspection: nothing is written to a file that is not ours.
fn inspect(connection: &Connection) -> Result<Container, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let application_id: i64 = connection
        .pragma_query_value(None, "application_id", |row| row.get(0))
        .map_err(fail)?;
    let user_version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(fail)?;
    let tables: i64 = connection
        .query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))
        .map_err(fail)?;
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
pub(crate) fn prepare_container(connection: &mut Connection) -> Result<(), Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let container = inspect(connection)?;
    if container == Container::Fresh {
        // Takes effect only before the first table exists.
        connection
            .pragma_update(None, "auto_vacuum", "INCREMENTAL")
            .map_err(fail)?;
    }
    let mode: String = connection
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .map_err(fail)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Bypass::WalUnavailable);
    }
    if container == Container::Fresh {
        create(connection)?;
    }
    Ok(())
}

fn create(connection: &mut Connection) -> Result<(), Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let transaction = begin(connection, TransactionBehavior::Immediate)?;
    // Another process may have created the container since `inspect`.
    if inspect(&transaction)? == Container::Current {
        return Ok(());
    }
    transaction.execute_batch(SCHEMA).map_err(fail)?;
    transaction
        .execute(
            "INSERT INTO store_state (singleton, container_version, policy_revision, \
             reset_epoch, max_disk_bytes, wal_threshold_bytes) VALUES (1, ?1, 0, 0, ?2, ?3)",
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

    fn connect(path: &Path) -> Connection {
        let connection = open_connection(path, 1 << 20).unwrap();
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
        prepare_container(&mut connection).unwrap();
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
                "key_manifests",
                "namespaces",
                "retired_scopes",
                "store_state"
            ]
        );
        let state: (i64, i64, i64, i64, i64) = connection
            .query_row(
                "SELECT container_version, policy_revision, reset_epoch, max_disk_bytes, \
                 wal_threshold_bytes FROM store_state",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(state, (CONTAINER_VERSION, 0, 0, 8 << 30, 256 << 20));
    }

    #[test]
    fn reopening_an_existing_container_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        prepare_container(&mut connect(&path)).unwrap();
        let mut again = connect(&path);
        prepare_container(&mut again).unwrap();
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
            prepare_container(&mut connection).unwrap();
            connection.pragma_update(None, "user_version", 2).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            prepare_container(&mut connect(&path)),
            Err(Bypass::UnsupportedContainer { version: 2 })
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_foreign_database_is_bypassed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        connect(&path)
            .execute_batch("CREATE TABLE other (a INTEGER);")
            .unwrap();
        assert_eq!(
            prepare_container(&mut connect(&path)),
            Err(Bypass::ForeignDatabase)
        );
        let other = root.path().join("b.sqlite3");
        connect(&other)
            .pragma_update(None, "application_id", 7)
            .unwrap();
        assert_eq!(
            prepare_container(&mut connect(&other)),
            Err(Bypass::ForeignDatabase)
        );
    }

    #[test]
    fn a_garbage_file_is_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        std::fs::write(&path, vec![0x5a; 8192]).unwrap();
        // The connection settings already read the header, so either step reports it.
        let opened = open_connection(&path, 1 << 20).and_then(|mut connection| {
            connection.busy_timeout(Duration::from_secs(2)).unwrap();
            prepare_container(&mut connection)
        });
        assert_eq!(opened, Err(Bypass::Corrupt));
    }

    #[test]
    fn a_container_without_its_state_row_is_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        prepare_container(&mut connect(&path)).unwrap();
        connect(&path)
            .execute_batch("DELETE FROM store_state;")
            .unwrap();
        assert_eq!(prepare_container(&mut connect(&path)), Err(Bypass::Corrupt));
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
                        prepare_container(&mut connection)
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
    fn the_schema_rejects_out_of_range_rows() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = connect(&root.path().join("a.sqlite3"));
        prepare_container(&mut connection).unwrap();
        let error = connection
            .execute(
                "INSERT INTO namespaces VALUES (?1, x'00', 0, 3, 0)",
                [vec![0u8; 32]],
            )
            .unwrap_err();
        assert!(error.to_string().contains("CHECK"), "{error}");
    }
}
