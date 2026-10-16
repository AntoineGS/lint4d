#![cfg_attr(not(test), allow(dead_code))]

use super::limits::SQLITE_LENGTH_LIMIT;
use super::outcome::{Bypass, Malformed, Miss};
use rusqlite::config::DbConfig;
use rusqlite::ffi::ErrorCode;
use rusqlite::limits::Limit;
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use std::path::Path;

/// SQLite 3.51.3, the first release with the WAL-reset fix (spec §6).
pub const MIN_SQLITE_VERSION_NUMBER: i32 = 3_051_003;

const LIMITS: &[(Limit, i32)] = &[
    (Limit::SQLITE_LIMIT_LENGTH, SQLITE_LENGTH_LIMIT),
    (Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024),
    (Limit::SQLITE_LIMIT_COLUMN, 64),
    (Limit::SQLITE_LIMIT_EXPR_DEPTH, 64),
    (Limit::SQLITE_LIMIT_COMPOUND_SELECT, 4),
    (Limit::SQLITE_LIMIT_VDBE_OP, 50_000),
    (Limit::SQLITE_LIMIT_FUNCTION_ARG, 8),
    (Limit::SQLITE_LIMIT_ATTACHED, 0),
    (Limit::SQLITE_LIMIT_LIKE_PATTERN_LENGTH, 64),
    (Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 32),
    (Limit::SQLITE_LIMIT_TRIGGER_DEPTH, 1),
    (Limit::SQLITE_LIMIT_WORKER_THREADS, 0),
];

pub(crate) fn check_sqlite_version(version_number: i32) -> Result<(), Bypass> {
    if version_number < MIN_SQLITE_VERSION_NUMBER {
        Err(Bypass::SqliteTooOld { version_number })
    } else {
        Ok(())
    }
}

pub(crate) fn classify(error: &rusqlite::Error) -> Bypass {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => match failure.code {
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked => Bypass::Busy,
            ErrorCode::ReadOnly => Bypass::ReadOnly,
            ErrorCode::DiskFull => Bypass::Full,
            ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase => Bypass::Corrupt,
            ErrorCode::OutOfMemory => Bypass::OutOfMemory,
            _ => Bypass::Sqlite {
                extended_code: failure.extended_code,
            },
        },
        _ => Bypass::Sqlite { extended_code: -1 },
    }
}

/// A read failure that only makes the record being read unusable.
pub(crate) fn classify_read(error: &rusqlite::Error) -> Result<Miss, Bypass> {
    match error {
        rusqlite::Error::SqliteFailure(failure, _) if failure.code == ErrorCode::TooBig => {
            Ok(Miss::Oversized)
        }
        rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::IntegralValueOutOfRange(..) => Ok(Miss::Malformed(Malformed::Row)),
        other => Err(classify(other)),
    }
}

/// Open `path` (never through a symlink) and apply the store's settings.
pub(crate) fn open_connection(path: &Path, page_cache_bytes: u64) -> Result<Connection, Bypass> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_PRIVATE_CACHE;
    let connection = Connection::open_with_flags(path, flags).map_err(|error| classify(&error))?;
    configure(&connection, page_cache_bytes)?;
    Ok(connection)
}

fn configure(connection: &Connection, page_cache_bytes: u64) -> Result<(), Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    connection
        .set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .map_err(fail)?;
    connection
        .pragma_update(None, "trusted_schema", "OFF")
        .map_err(fail)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(fail)?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(fail)?;
    connection
        .pragma_update(None, "mmap_size", 0)
        .map_err(fail)?;
    connection
        .pragma_update(None, "cache_size", -((page_cache_bytes / 1024) as i64))
        .map_err(fail)?;
    connection
        .pragma_update(None, "temp_store", "FILE")
        .map_err(fail)?;
    connection
        .pragma_update(None, "cell_size_check", "ON")
        .map_err(fail)?;
    for &(limit, value) in LIMITS {
        connection.set_limit(limit, value).map_err(fail)?;
    }
    connection.set_prepared_statement_cache_capacity(16);
    if connection.is_readonly(rusqlite::MAIN_DB).map_err(fail)? {
        return Err(Bypass::ReadOnly);
    }
    Ok(())
}

pub(crate) fn begin(
    connection: &mut Connection,
    behavior: TransactionBehavior,
) -> Result<Transaction<'_>, Bypass> {
    connection
        .transaction_with_behavior(behavior)
        .map_err(|error| classify(&error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::ffi;

    fn failure(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(ffi::Error::new(code), None)
    }

    #[test]
    fn bundled_sqlite_has_the_wal_reset_fix() {
        assert_eq!(check_sqlite_version(rusqlite::version_number()), Ok(()));
        assert_eq!(
            check_sqlite_version(3_051_002),
            Err(Bypass::SqliteTooOld {
                version_number: 3_051_002
            })
        );
        assert_eq!(check_sqlite_version(MIN_SQLITE_VERSION_NUMBER), Ok(()));
    }

    #[test]
    fn sqlite_failures_map_to_bypass_reasons() {
        assert_eq!(classify(&failure(ffi::SQLITE_BUSY)), Bypass::Busy);
        assert_eq!(classify(&failure(ffi::SQLITE_LOCKED)), Bypass::Busy);
        assert_eq!(classify(&failure(ffi::SQLITE_READONLY)), Bypass::ReadOnly);
        assert_eq!(classify(&failure(ffi::SQLITE_FULL)), Bypass::Full);
        assert_eq!(classify(&failure(ffi::SQLITE_CORRUPT)), Bypass::Corrupt);
        assert_eq!(classify(&failure(ffi::SQLITE_NOTADB)), Bypass::Corrupt);
        assert_eq!(classify(&failure(ffi::SQLITE_NOMEM)), Bypass::OutOfMemory);
        assert_eq!(
            classify(&failure(ffi::SQLITE_CANTOPEN)),
            Bypass::Sqlite {
                extended_code: ffi::SQLITE_CANTOPEN
            }
        );
    }

    #[test]
    fn oversized_and_mistyped_reads_only_miss_the_record() {
        assert_eq!(
            classify_read(&failure(ffi::SQLITE_TOOBIG)),
            Ok(Miss::Oversized)
        );
        assert_eq!(
            classify_read(&rusqlite::Error::InvalidColumnType(
                0,
                "payload".into(),
                rusqlite::types::Type::Null
            )),
            Ok(Miss::Malformed(Malformed::Row))
        );
        assert_eq!(classify_read(&failure(ffi::SQLITE_BUSY)), Err(Bypass::Busy));
    }

    #[cfg(unix)]
    #[test]
    fn connections_are_configured_defensively() {
        let root = tempfile::tempdir().unwrap();
        let connection = open_connection(&root.path().join("a.sqlite3"), 5_592_405).unwrap();
        let pragma = |name: &str| -> i64 {
            connection
                .pragma_query_value(None, name, |row| row.get(0))
                .unwrap()
        };
        assert_eq!(pragma("foreign_keys"), 1);
        assert_eq!(pragma("trusted_schema"), 0);
        assert_eq!(pragma("mmap_size"), 0);
        assert_eq!(pragma("cache_size"), -5461);
        assert_eq!(pragma("synchronous"), 1);
        assert_eq!(pragma("cell_size_check"), 1);
        use rusqlite::limits::Limit;
        assert_eq!(
            connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(),
            SQLITE_LENGTH_LIMIT
        );
        assert_eq!(connection.limit(Limit::SQLITE_LIMIT_ATTACHED).unwrap(), 0);
        assert_eq!(
            connection
                .limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER)
                .unwrap(),
            32
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_database_is_not_opened() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.sqlite3");
        drop(open_connection(&target, 1 << 20).unwrap());
        let link = root.path().join("link.sqlite3");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            open_connection(&link, 1 << 20),
            Err(Bypass::Sqlite { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_database_is_bypassed() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return; // root ignores file modes
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("a.sqlite3");
        {
            let connection = open_connection(&path, 1 << 20).unwrap();
            connection
                .execute_batch("CREATE TABLE t (a INTEGER) STRICT;")
                .unwrap();
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert_eq!(
            open_connection(&path, 1 << 20).err(),
            Some(Bypass::ReadOnly)
        );
    }
}
