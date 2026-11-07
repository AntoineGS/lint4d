use super::gc;
use super::location::StoreLocation;
use super::outcome::{Bypass, Integrity};
use super::pool::Deadline;
use super::sqlite::{begin_within, classify, limit_wait};
use crate::{Digest, Namespace};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The `lifecycle` of every namespace, artifact, evidence set and member
/// record: retiring one ends its read eligibility at once.
pub(crate) const LIVE: i64 = 1;
pub(crate) const RETIRED: i64 = 2;
/// Namespaces examined per retirement pass; the rest wait for the next start.
const MAX_RETIRED_PER_PASS: usize = 64;
/// Retired scopes visited per reclamation transaction.
const MAX_SCOPES_PER_RECLAIM: i64 = 16;
/// Scoped tables, each before every table it references.
const RECLAIM_TABLES: [&str; 14] = [
    "lease_holds",
    "process_leases",
    "snapshot_members",
    "snapshots",
    "binding_targets",
    "member_bindings",
    "member_records",
    "evidence_observations",
    "evidence_sets",
    "observation_content",
    "artifact_rows",
    "artifacts",
    "scopes",
    "key_manifests",
];

/// At most `?3` rows of `table` in scope `(?1, ?2)`. Rows keyed by a
/// parent's surrogate are in the scope of that parent.
fn reclaim_sql(table: &str) -> String {
    match table {
        "evidence_observations" => "DELETE FROM evidence_observations \
             WHERE (evidence_ref, ordinal) IN (SELECT o.evidence_ref, o.ordinal \
             FROM evidence_sets e CROSS JOIN evidence_observations o \
             ON o.evidence_ref = e.evidence_ref \
             WHERE e.namespace_id = ?1 AND e.reset_epoch = ?2 LIMIT ?3)"
            .to_string(),
        "artifact_rows" => "DELETE FROM artifact_rows \
             WHERE (artifact_ref, ordinal) IN (SELECT r.artifact_ref, r.ordinal \
             FROM artifacts a CROSS JOIN artifact_rows r ON r.artifact_ref = a.artifact_ref \
             WHERE a.namespace_id = ?1 AND a.reset_epoch = ?2 LIMIT ?3)"
            .to_string(),
        table => format!(
            "DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 LIMIT ?3)"
        ),
    }
}

/// The scope every row of this store is published and read in (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scope {
    pub namespace: Digest,
    pub reset_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetireReport {
    pub retired: usize,
    /// Obsolete namespaces another live process still holds.
    pub in_use: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reclaimed {
    pub rows: usize,
    /// No retired scope remains, and nothing in the live scope is left to
    /// delete: no retired row that nothing references, no orphan key
    /// manifest. `reclaim(0)` does not examine the live scope, so its
    /// `complete` speaks for the retired scopes only.
    pub complete: bool,
}

pub(crate) fn epoch(scope: Scope) -> i64 {
    scope.reset_epoch as i64
}

fn digest_from(bytes: &[u8]) -> Result<Digest, Bypass> {
    <[u8; 32]>::try_from(bytes)
        .map(Digest::from_bytes)
        .map_err(|_| Bypass::Corrupt)
}

/// A namespace lock file: shared while a process uses the namespace,
/// exclusive while one retires it (spec §10, amendment 2026-10-16).
pub(crate) struct NamespaceLock {
    _file: File,
    path: PathBuf,
}

pub(crate) fn open_lock_file(path: &Path) -> Result<File, Bypass> {
    StoreLocation::check_file(path)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|error| Bypass::Io(error.kind()))
}

/// Whether `file` is still the file at `path`: a retiring process removes
/// the lock file while holding it exclusively.
#[cfg(unix)]
fn still_linked(file: &File, path: &Path) -> Result<bool, Bypass> {
    use std::os::unix::fs::MetadataExt;
    let open = file.metadata().map_err(|error| Bypass::Io(error.kind()))?;
    match fs::symlink_metadata(path) {
        Ok(linked) => Ok(open.dev() == linked.dev() && open.ino() == linked.ino()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Bypass::Io(error.kind())),
    }
}

/// Unreachable until `l4d-x2ft`: the location checks bypass other platforms.
#[cfg(not(unix))]
fn still_linked(_: &File, _: &Path) -> Result<bool, Bypass> {
    Ok(true)
}

impl NamespaceLock {
    pub(crate) fn share(path: PathBuf, deadline: Deadline) -> Result<Self, Bypass> {
        loop {
            let file = open_lock_file(&path)?;
            match file.try_lock_shared() {
                Ok(()) if still_linked(&file, &path)? => return Ok(Self { _file: file, path }),
                Ok(()) | Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(error)) => return Err(Bypass::Io(error.kind())),
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                return Err(Bypass::NamespaceLockBusy);
            }
            std::thread::sleep(remaining.min(Duration::from_millis(5)));
        }
    }

    /// `None` while a live process shares the namespace.
    fn try_exclusive(path: PathBuf) -> Result<Option<Self>, Bypass> {
        let file = open_lock_file(&path)?;
        match file.try_lock() {
            Ok(()) if still_linked(&file, &path)? => Ok(Some(Self { _file: file, path })),
            Ok(()) | Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(Bypass::Io(error.kind())),
        }
    }

    /// Unlink the file before releasing it, so a process that opened it in
    /// the meantime sees it is no longer linked and starts again.
    fn remove(self) -> Result<(), Bypass> {
        fs::remove_file(&self.path).map_err(|error| Bypass::Io(error.kind()))
    }
}

pub(crate) fn register(
    connection: &mut Connection,
    namespace: &Namespace,
    now_unix_seconds: u64,
    deadline: Deadline,
) -> Result<Scope, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let transaction = begin_within(connection, TransactionBehavior::Immediate, deadline)?;
    let id = namespace.id();
    let existing: Option<(Vec<u8>, i64, i64)> = transaction
        .query_row(
            "SELECT manifest, reset_epoch, state FROM namespaces WHERE namespace_id = ?1",
            [id.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(fail)?;
    let reset_epoch = match existing {
        None => {
            transaction
                .execute(
                    "INSERT INTO namespaces (namespace_id, manifest, reset_epoch, state, \
                     created_unix_seconds) VALUES (?1, ?2, 0, ?3, ?4)",
                    params![
                        id.as_bytes().as_slice(),
                        namespace.manifest(),
                        LIVE,
                        now_unix_seconds as i64
                    ],
                )
                .map_err(fail)?;
            0
        }
        Some((manifest, _, _)) if manifest != namespace.manifest() => {
            return Err(Bypass::Integrity(Integrity::ConflictingManifest));
        }
        Some((_, reset_epoch, LIVE)) => reset_epoch,
        Some((_, reset_epoch, _)) => {
            // Retired by another process: come back in a fresh scope, so no
            // row of the retired one can satisfy this process.
            let next = reset_epoch + 1;
            transaction
                .execute(
                    "UPDATE namespaces SET reset_epoch = ?2, state = ?3 WHERE namespace_id = ?1",
                    params![id.as_bytes().as_slice(), next, LIVE],
                )
                .map_err(fail)?;
            next
        }
    };
    transaction.commit().map_err(fail)?;
    Ok(Scope {
        namespace: id,
        reset_epoch: u64::try_from(reset_epoch).map_err(|_| Bypass::Corrupt)?,
    })
}

/// Fail with [`Bypass::Fenced`] unless `scope` is still the live scope of
/// its namespace. Every read and write transaction starts with this.
pub(crate) fn ensure_live(connection: &Connection, scope: Scope) -> Result<(), Bypass> {
    let live: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM namespaces WHERE namespace_id = ?1 AND reset_epoch = ?2 AND state = ?3",
            params![scope.namespace.as_bytes().as_slice(), epoch(scope), LIVE],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| classify(&error))?;
    live.map(|_| ()).ok_or(Bypass::Fenced)
}

/// The surrogate artifact rows of `scope` carry, or `None` while no
/// artifact was ever published in it.
pub(crate) fn scope_ref(connection: &Connection, scope: Scope) -> Result<Option<i64>, Bypass> {
    connection
        .prepare_cached("SELECT scope_ref FROM scopes WHERE namespace_id = ?1 AND reset_epoch = ?2")
        .and_then(|mut statement| {
            statement
                .query_row(
                    params![scope.namespace.as_bytes().as_slice(), epoch(scope)],
                    |row| row.get(0),
                )
                .optional()
        })
        .map_err(|error| classify(&error))
}

/// [`scope_ref`], registering the scope first in the caller's write
/// transaction.
pub(crate) fn ensure_scope_ref(transaction: &Connection, scope: Scope) -> Result<i64, Bypass> {
    transaction
        .prepare_cached(
            "INSERT INTO scopes (namespace_id, reset_epoch) VALUES (?1, ?2) ON CONFLICT DO NOTHING",
        )
        .and_then(|mut statement| {
            statement.execute(params![scope.namespace.as_bytes().as_slice(), epoch(scope)])
        })
        .map_err(|error| classify(&error))?;
    scope_ref(transaction, scope)?.ok_or(Bypass::Corrupt)
}

/// Retire `scope` and move its namespace to the next epoch in one
/// transaction (spec §11.2): every handle of `scope` is fenced from its
/// next transaction; its rows are reclaimed in batches like any retired
/// scope. `store_state.reset_epoch` is not used.
pub(crate) fn reset(transaction: &Connection, scope: Scope) -> Result<(), Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    ensure_live(transaction, scope)?;
    let id = scope.namespace.as_bytes().as_slice();
    transaction
        .execute(
            "INSERT OR IGNORE INTO retired_scopes (namespace_id, reset_epoch) VALUES (?1, ?2)",
            params![id, epoch(scope)],
        )
        .map_err(fail)?;
    transaction
        .execute(
            "UPDATE namespaces SET reset_epoch = reset_epoch + 1 \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 AND state = ?3",
            params![id, epoch(scope), LIVE],
        )
        .map_err(fail)?;
    Ok(())
}

/// Retire every other live namespace no process holds, one short
/// transaction each.
pub(crate) fn retire_obsolete(
    connection: &mut Connection,
    location: &StoreLocation,
    current: Digest,
    deadline: Deadline,
) -> Result<RetireReport, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    limit_wait(connection, deadline)?;
    let candidates: Vec<(Vec<u8>, i64)> = {
        let mut statement = connection
            .prepare(
                "SELECT namespace_id, reset_epoch FROM namespaces \
                 WHERE state = ?1 AND namespace_id <> ?2 ORDER BY namespace_id LIMIT ?3",
            )
            .map_err(fail)?;
        statement
            .query_map(
                params![
                    LIVE,
                    current.as_bytes().as_slice(),
                    MAX_RETIRED_PER_PASS as i64
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(fail)?
            .collect::<Result<_, _>>()
            .map_err(fail)?
    };
    let mut report = RetireReport::default();
    for (id, reset_epoch) in candidates {
        let namespace = digest_from(&id)?;
        // Never hold the lock file across a SQLite wait: take the write
        // lock first, then try the (non-blocking) exclusive lock.
        let transaction = begin_within(connection, TransactionBehavior::Immediate, deadline)?;
        let Some(lock) = NamespaceLock::try_exclusive(location.namespace_lock(namespace))? else {
            drop(transaction);
            report.in_use += 1;
            continue;
        };
        let changed = transaction
            .execute(
                "UPDATE namespaces SET state = ?3 \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND state = ?4",
                params![id, reset_epoch, RETIRED, LIVE],
            )
            .map_err(fail)?;
        if changed == 1 {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO retired_scopes (namespace_id, reset_epoch) VALUES (?1, ?2)",
                    params![id, reset_epoch],
                )
                .map_err(fail)?;
        }
        transaction.commit().map_err(fail)?;
        // The scope is retired either way. A lock file that cannot be
        // removed is reused if the namespace is registered again.
        let _ = lock.remove();
        report.retired += changed;
    }
    Ok(report)
}

/// Delete at most `max_rows` rows in the caller's write transaction (spec
/// §10: no unbounded cascades): rows of whole retired scopes first, then,
/// with what is left, retired rows and orphan key manifests of `live`. The
/// second part is skipped when `live` is no longer live (a fenced handle
/// still reclaims retired scopes, its own included).
pub(crate) fn reclaim(
    transaction: &Connection,
    max_rows: usize,
    live: Scope,
) -> Result<Reclaimed, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let scopes: Vec<(Vec<u8>, i64)> = {
        let mut statement = transaction
            .prepare(
                "SELECT namespace_id, reset_epoch FROM retired_scopes \
                 ORDER BY namespace_id, reset_epoch LIMIT ?1",
            )
            .map_err(fail)?;
        statement
            .query_map([MAX_SCOPES_PER_RECLAIM], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(fail)?
            .collect::<Result<_, _>>()
            .map_err(fail)?
    };
    let mut budget = max_rows;
    let mut rows = 0;
    'scopes: for (id, reset_epoch) in &scopes {
        for table in RECLAIM_TABLES {
            if budget == 0 {
                break 'scopes;
            }
            let deleted = transaction
                .execute(&reclaim_sql(table), params![id, reset_epoch, budget as i64])
                .map_err(fail)?;
            rows += deleted;
            budget -= deleted;
            if budget == 0 {
                // The table may still hold rows of this scope.
                break 'scopes;
            }
        }
        transaction
            .execute(
                "DELETE FROM retired_scopes WHERE namespace_id = ?1 AND reset_epoch = ?2",
                params![id, reset_epoch],
            )
            .map_err(fail)?;
    }
    // With no rows left to spend, the live scope is not examined:
    // `complete` then speaks for the retired scopes only.
    let drained = match ensure_live(transaction, live) {
        Ok(()) if budget == 0 => true,
        Ok(()) => {
            let (deleted, drained) = gc::delete_retired(transaction, live, budget)?;
            rows += deleted;
            drained
        }
        Err(Bypass::Fenced) => true,
        Err(error) => return Err(error),
    };
    transaction
        .execute(
            "DELETE FROM namespaces WHERE state = ?1 \
             AND NOT EXISTS (SELECT 1 FROM retired_scopes r WHERE r.namespace_id = namespaces.namespace_id) \
             AND NOT EXISTS (SELECT 1 FROM key_manifests k WHERE k.namespace_id = namespaces.namespace_id) \
             AND NOT EXISTS (SELECT 1 FROM process_leases l WHERE l.namespace_id = namespaces.namespace_id)",
            [RETIRED],
        )
        .map_err(fail)?;
    let remaining: i64 = transaction
        .query_row("SELECT count(*) FROM retired_scopes", [], |row| row.get(0))
        .map_err(fail)?;
    Ok(Reclaimed {
        rows,
        complete: remaining == 0 && drained,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_sharer_that_opened_a_removed_lock_file_starts_again() {
        let root = tempfile::tempdir().unwrap();
        let location = StoreLocation::prepare(&root.path().join("cache")).unwrap();
        location.prepare_namespace_locks().unwrap();
        let path = location.namespace_lock(Digest::from_bytes([3; 32]));
        let retiring = NamespaceLock::try_exclusive(path.clone())
            .unwrap()
            .expect("nobody holds it");
        // A sharer opened the file before the retirer removed it.
        let stale = open_lock_file(&path).unwrap();
        retiring.remove().unwrap();
        stale.try_lock_shared().unwrap();
        assert!(!still_linked(&stale, &path).unwrap());
        // `share` opens again and holds the fresh, linked file.
        let lock =
            NamespaceLock::share(path.clone(), Deadline::after(Duration::from_secs(1))).unwrap();
        assert!(still_linked(&lock._file, &path).unwrap());
        assert!(
            NamespaceLock::try_exclusive(path.clone())
                .unwrap()
                .is_none()
        );
        // The stale handle's lock is on the removed file and blocks nothing.
        drop(lock);
        assert!(NamespaceLock::try_exclusive(path).unwrap().is_some());
    }

    #[test]
    fn reclaim_covers_every_scoped_table_children_first() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = crate::store::sqlite::open_connection(
            &root.path().join("a.sqlite3"),
            1 << 20,
            std::time::Duration::from_secs(2),
        )
        .unwrap();
        crate::store::container::prepare_container(
            &mut connection,
            Deadline::after(std::time::Duration::from_secs(2)),
        )
        .unwrap();
        // Tables without a scope column are keyed by a scoped parent's
        // surrogate.
        let scoped: std::collections::BTreeSet<String> = connection
            .prepare(
                "SELECT name FROM sqlite_schema WHERE type = 'table' \
                 AND name NOT IN ('namespaces', 'retired_scopes', 'store_state', \
                 'sqlite_sequence')",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            scoped,
            RECLAIM_TABLES.iter().map(|t| t.to_string()).collect()
        );
        // A table is deleted from before every table it references.
        for (index, table) in RECLAIM_TABLES.iter().enumerate() {
            let parents: Vec<String> = connection
                .prepare(&format!(
                    "SELECT \"table\" FROM pragma_foreign_key_list('{table}')"
                ))
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            for parent in parents.iter().filter(|parent| *parent != "namespaces") {
                let position = RECLAIM_TABLES.iter().position(|t| t == parent).unwrap();
                assert!(position > index, "{table} references {parent}");
            }
        }
    }
}
