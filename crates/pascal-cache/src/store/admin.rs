//! The store functions behind the administration modes (spec §11.2 and its
//! amendment 2026-11-03): status, prune and reset of a directory, and the
//! bounded advice a bypass earns.
//!
//! Nothing here unlinks or renames a database file: an unreadable container
//! is reported and left in place (spec §10).

use super::budget::{PolicyState, Pressure, Usage, pressure_of, read_policy, read_usage_at};
use super::container::{self, CONTAINER_VERSION, Container};
use super::gc::MaintenanceLimits;
use super::leases::live_since;
use super::limits::MIN_CONNECTION_PAGE_CACHE_BYTES;
use super::location::{DATABASE_FILE, StoreLocation};
use super::maintenance::{Maintenance, StepBudget, StepReport};
use super::namespaces;
use super::outcome::{Bypass, LocationIssue};
use super::sqlite::{begin, check_sqlite_version, classify, open_connection};
use super::{Store, StoreLimits, unix_seconds};
use crate::Namespace;
use rusqlite::{Connection, TransactionBehavior};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

/// What a caller can do about a bypass, bounded to five answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAdvice {
    /// Nothing to do, or nothing the user can fix.
    None,
    /// Transient: the same operation may succeed later.
    Retry,
    /// Reclaim space (`--cache-prune`).
    Prune,
    /// The namespace's data is inconsistent (`--cache-reset`).
    ResetNamespace,
    /// The container cannot be used: select another cache directory (the
    /// old one is left in place).
    FreshDirectory,
}

/// The recommendation for `bypass`. Exhaustive: a new [`Bypass`] variant
/// does not compile until it is classified.
pub fn advice(bypass: &Bypass) -> RecoveryAdvice {
    match bypass {
        Bypass::Busy
        | Bypass::Recovering
        | Bypass::NamespaceLockBusy
        | Bypass::ScratchExhausted
        | Bypass::FactBudget
        | Bypass::Retiring
        | Bypass::BuilderFenced
        | Bypass::Fenced
        | Bypass::TooManyHolds => RecoveryAdvice::Retry,
        Bypass::Full | Bypass::OverBudget | Bypass::WalPressure | Bypass::LowDiskSpace => {
            RecoveryAdvice::Prune
        }
        Bypass::Integrity(_) => RecoveryAdvice::ResetNamespace,
        Bypass::Corrupt
        | Bypass::ReadOnly
        | Bypass::ForeignDatabase
        | Bypass::UnsupportedContainer { .. }
        | Bypass::WalUnavailable
        | Bypass::Location(_)
        | Bypass::SqliteTooOld { .. } => RecoveryAdvice::FreshDirectory,
        Bypass::InvalidLimits(_)
        | Bypass::InvalidMaintenanceLimits
        | Bypass::OutOfMemory
        | Bypass::TooLarge(_)
        | Bypass::InvalidRowKind { .. }
        | Bypass::EvidenceForLocalArtifact
        | Bypass::InvalidRecord(_)
        | Bypass::MissingReference
        | Bypass::DuplicateMember
        | Bypass::SealRefused(_)
        | Bypass::Io(_)
        | Bypass::Sqlite { .. } => RecoveryAdvice::None,
    }
}

/// The container's version and whether this program can use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerInfo {
    pub version: i64,
    pub compatible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreStatus {
    /// No database file: nothing was created.
    Absent,
    Unreadable {
        bypass: Bypass,
        advice: RecoveryAdvice,
    },
    Present(StatusReport),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub container: ContainerInfo,
    pub usage: Usage,
    pub policy: PolicyState,
    pub pressure: Pressure,
    pub live_namespaces: u64,
    pub retired_namespaces: u64,
    pub retired_scopes: u64,
    pub live_leases: u64,
    /// The live namespaces, the most recently used first, at most
    /// [`MAX_LISTED_NAMESPACES`] (l4d-1ypi).
    pub namespaces: Vec<NamespaceStatus>,
    /// More live namespaces than are listed.
    pub namespaces_truncated: bool,
    /// [`RecoveryAdvice::None`] for a healthy container; [`RecoveryAdvice::Prune`]
    /// when writes are refused for space.
    pub advice: RecoveryAdvice,
}

/// Live namespaces a status report lists.
pub const MAX_LISTED_NAMESPACES: usize = 16;

/// One live namespace of a store: one build's facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceStatus {
    /// The namespace identity, derived from the build fingerprint.
    pub id: crate::Digest,
    pub reset_epoch: u64,
    /// When a process last opened or closed it; `None` without a lock file.
    pub last_used_unix_seconds: Option<u64>,
    /// Estimated bytes of its live scope's rows.
    pub estimated_bytes: u64,
    /// A live process lease is registered in it.
    pub held: bool,
}

fn unreadable(bypass: Bypass) -> StoreStatus {
    StoreStatus::Unreadable {
        advice: advice(&bypass),
        bypass,
    }
}

/// Whether `directory` holds a database file, without creating anything.
fn database_exists(directory: &Path) -> Result<bool, Bypass> {
    if !directory.is_absolute() {
        return Err(Bypass::Location(LocationIssue::NotAbsolute));
    }
    match std::fs::symlink_metadata(directory.join(DATABASE_FILE)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Bypass::Io(error.kind())),
    }
}

/// Report the store in `directory`. An absent database is reported, not
/// created. Otherwise one connection reads the existing file: no namespace
/// is registered, no namespace lock is taken and no row is written (SQLite
/// itself may create the `-wal` and `-shm` side files of a WAL database).
/// An unreadable container is reported with its advice and left in place.
pub fn status(directory: &Path) -> StoreStatus {
    super::count_operation();
    match database_exists(directory) {
        Ok(true) => {}
        Ok(false) => return StoreStatus::Absent,
        Err(bypass) => return unreadable(bypass),
    }
    match read_status(directory) {
        Ok(Some(report)) => StoreStatus::Present(report),
        // An empty file holds no container yet.
        Ok(None) => StoreStatus::Absent,
        Err(bypass) => unreadable(bypass),
    }
}

fn read_status(directory: &Path) -> Result<Option<StatusReport>, Bypass> {
    check_sqlite_version(rusqlite::version_number())?;
    let location = StoreLocation::prepare(directory)?;
    for file in location.database_files() {
        StoreLocation::check_file(&file)?;
    }
    let mut connection = open_connection(
        &location.database(),
        MIN_CONNECTION_PAGE_CACHE_BYTES,
        StoreLimits::default().open_wait,
    )?;
    if container::inspect(&connection)? == Container::Fresh {
        return Ok(None);
    }
    let transaction = begin(&mut connection, TransactionBehavior::Deferred)?;
    let policy = read_policy(&transaction)?;
    let usage = read_usage_at(&location, &transaction)?;
    let pressure = pressure_of(policy.policy, &usage);
    let live_since = live_since(unix_seconds());
    let ranked = namespaces::ranked_live(&transaction, &location, None)?;
    let namespaces_truncated = ranked.len() > MAX_LISTED_NAMESPACES;
    let namespaces = ranked
        .into_iter()
        .take(MAX_LISTED_NAMESPACES)
        .map(|(id, reset_epoch, used)| {
            Ok(NamespaceStatus {
                id,
                reset_epoch: u64::try_from(reset_epoch).map_err(|_| Bypass::Corrupt)?,
                last_used_unix_seconds: used.and_then(namespaces::unix_seconds_of),
                estimated_bytes: namespaces::estimated_bytes(
                    &transaction,
                    id.as_bytes(),
                    reset_epoch,
                )?,
                held: namespace_held(&transaction, id, reset_epoch, live_since)?,
            })
        })
        .collect::<Result<Vec<_>, Bypass>>()?;
    Ok(Some(StatusReport {
        container: ContainerInfo {
            version: CONTAINER_VERSION,
            compatible: true,
        },
        usage,
        policy,
        pressure,
        live_namespaces: count(
            &transaction,
            "SELECT count(*) FROM namespaces WHERE state = 1",
            0,
        )?,
        retired_namespaces: count(
            &transaction,
            "SELECT count(*) FROM namespaces WHERE state <> 1",
            0,
        )?,
        retired_scopes: count(&transaction, "SELECT count(*) FROM retired_scopes", 0)?,
        live_leases: count(
            &transaction,
            "SELECT count(*) FROM process_leases WHERE renewed_unix_seconds >= ?1",
            live_since,
        )?,
        namespaces,
        namespaces_truncated,
        advice: if pressure == Pressure::None {
            RecoveryAdvice::None
        } else {
            RecoveryAdvice::Prune
        },
    }))
}

fn namespace_held(
    connection: &Connection,
    id: crate::Digest,
    reset_epoch: i64,
    live_since: i64,
) -> Result<bool, Bypass> {
    connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM process_leases WHERE namespace_id = ?1 \
             AND reset_epoch = ?2 AND renewed_unix_seconds >= ?3)",
            rusqlite::params![id.as_bytes().as_slice(), reset_epoch, live_since],
            |row| row.get(0),
        )
        .map_err(|error| classify(&error))
}

fn count(connection: &Connection, sql: &str, parameter: i64) -> Result<u64, Bypass> {
    let value: i64 = if sql.contains("?1") {
        connection.query_row(sql, [parameter], |row| row.get(0))
    } else {
        connection.query_row(sql, [], |row| row.get(0))
    }
    .map_err(|error| classify(&error))?;
    u64::try_from(value).map_err(|_| Bypass::Corrupt)
}

/// Run the maintenance cycle on the store in `directory` until it completes
/// or `budget.time` is spent; `None` for an absent store. Each step may
/// change `budget.rows` rows; the report sums them. A step another
/// maintainer's lock skips, one stopped by WAL pressure, or one that
/// changed no row and stayed in its phase (a zero row budget, say) ends
/// the call.
pub fn prune(
    directory: &Path,
    namespace: &Namespace,
    limits: StoreLimits,
    budget: StepBudget,
) -> Result<Option<StepReport>, Bypass> {
    super::count_operation();
    if !database_exists(directory)? {
        return Ok(None);
    }
    let store = Store::open(directory, namespace, limits)?;
    let mut maintenance = Maintenance::new(MaintenanceLimits::default())
        .map_err(|_| Bypass::InvalidMaintenanceLimits)?;
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let mut total = 0;
    loop {
        let remaining = budget.time.saturating_sub(started.elapsed());
        let phase = maintenance.phase();
        let mut report = maintenance.step(
            &store,
            StepBudget {
                time: remaining,
                rows: budget.rows,
            },
            &cancel,
        )?;
        let stalled = report.rows == 0 && maintenance.phase() == phase;
        total += report.rows;
        report.rows = total;
        if stalled
            || report.cycle_complete
            || report.skipped
            || report.cancelled
            || started.elapsed() >= budget.time
            || report.pressure == Pressure::WalPressure
        {
            return Ok(Some(report));
        }
    }
}

/// What `--cache-prune` may spend (spec §10 amendment 2026-11-03, D20).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneLimits {
    /// Real elapsed time for the whole prune.
    pub time: std::time::Duration,
    /// Rows one step may change.
    pub step_rows: usize,
}

impl Default for PruneLimits {
    fn default() -> Self {
        Self {
            time: std::time::Duration::from_secs(30),
            step_rows: 4096,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneOutcome {
    /// A complete cycle left the store with its maintenance headroom.
    WithinBudget,
    /// A complete cycle changed no row: nothing more is reclaimable now.
    NoProgress,
    TimeLimit,
    /// Another process holds `maintenance.lock`; rerun later.
    Busy,
    /// The WAL stays at its threshold: a reader in another process keeps
    /// the checkpoint from completing.
    WalPressure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    pub outcome: PruneOutcome,
    pub cycles: u32,
    pub rows: usize,
    pub before: Usage,
    pub after: Usage,
    pub pressure: Pressure,
    pub policy: PolicyState,
}

/// [`prune_store`] on the store in `directory`; `None` for an absent store,
/// which is not created.
pub fn prune_until(
    directory: &Path,
    namespace: &Namespace,
    limits: StoreLimits,
    prune: PruneLimits,
) -> Result<Option<PruneReport>, Bypass> {
    super::count_operation();
    if !database_exists(directory)? {
        return Ok(None);
    }
    let store = Store::open(directory, namespace, limits)?;
    prune_store(&store, prune).map(Some)
}

/// Repeat maintenance cycles until a complete cycle leaves the store with
/// its headroom, a complete cycle changes no row, or `prune.time` runs out.
/// At least one complete cycle runs. Rerunning resumes from the database.
pub fn prune_store(store: &Store, prune: PruneLimits) -> Result<PruneReport, Bypass> {
    let before = store.usage()?;
    let mut maintenance = Maintenance::new(MaintenanceLimits::default())
        .map_err(|_| Bypass::InvalidMaintenanceLimits)?;
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let (mut cycles, mut rows, mut cycle_rows) = (0u32, 0usize, 0usize);
    let outcome = loop {
        let remaining = prune.time.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break PruneOutcome::TimeLimit;
        }
        let phase = maintenance.phase();
        let report = maintenance.step(
            store,
            StepBudget {
                time: remaining,
                rows: prune.step_rows,
            },
            &cancel,
        )?;
        rows += report.rows;
        cycle_rows += report.rows;
        let stalled = report.rows == 0 && maintenance.phase() == phase && !report.cycle_complete;
        if report.skipped {
            break PruneOutcome::Busy;
        }
        if report.cycle_complete {
            cycles += 1;
            if store.pressure()? == Pressure::None {
                break PruneOutcome::WithinBudget;
            }
            if cycle_rows == 0 {
                break PruneOutcome::NoProgress;
            }
            cycle_rows = 0;
        } else if stalled {
            break if report.pressure == Pressure::WalPressure {
                PruneOutcome::WalPressure
            } else {
                PruneOutcome::NoProgress
            };
        }
    };
    Ok(PruneReport {
        outcome,
        cycles,
        rows,
        before,
        after: store.usage()?,
        pressure: store.pressure()?,
        policy: store.policy()?,
    })
}

/// Reset `namespace` of the store in `directory` logically and run one
/// bounded reclamation; `false` for an absent store. The reset is durable
/// once this returns `true`: a reclamation that cannot run now (WAL
/// pressure, a busy store) is left to the next prune or open.
pub fn reset(directory: &Path, namespace: &Namespace, limits: StoreLimits) -> Result<bool, Bypass> {
    super::count_operation();
    if !database_exists(directory)? {
        return Ok(false);
    }
    let store = Store::open(directory, namespace, limits)?;
    store.reset_namespace()?;
    let _ = store.reclaim(256);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Integrity, LimitsError, MemberError, SealRefusal, TooLarge};

    /// One value of every variant, with its expected advice. The `match`
    /// is exhaustive, so a new variant fails to compile here until listed.
    #[test]
    fn advice_maps_every_bypass_to_a_bounded_recommendation() {
        use RecoveryAdvice::{FreshDirectory, None as Nothing, Prune, ResetNamespace, Retry};
        let bypasses = [
            Bypass::Location(LocationIssue::NotAbsolute),
            Bypass::InvalidLimits(LimitsError::Readers),
            Bypass::SqliteTooOld { version_number: 1 },
            Bypass::WalUnavailable,
            Bypass::ForeignDatabase,
            Bypass::UnsupportedContainer { version: 9 },
            Bypass::NamespaceLockBusy,
            Bypass::Fenced,
            Bypass::Busy,
            Bypass::Recovering,
            Bypass::ReadOnly,
            Bypass::Full,
            Bypass::WalPressure,
            Bypass::OverBudget,
            Bypass::LowDiskSpace,
            Bypass::TooManyHolds,
            Bypass::InvalidMaintenanceLimits,
            Bypass::Corrupt,
            Bypass::OutOfMemory,
            Bypass::ScratchExhausted,
            Bypass::FactBudget,
            Bypass::TooLarge(TooLarge::Rows { count: 1 }),
            Bypass::InvalidRowKind { ordinal: 0 },
            Bypass::Integrity(Integrity::ConflictingRecord),
            Bypass::Retiring,
            Bypass::EvidenceForLocalArtifact,
            Bypass::InvalidRecord(MemberError::EvidenceWithoutArtifact),
            Bypass::MissingReference,
            Bypass::BuilderFenced,
            Bypass::DuplicateMember,
            Bypass::SealRefused(SealRefusal::CountMismatch),
            Bypass::Io(std::io::ErrorKind::Other),
            Bypass::Sqlite { extended_code: 1 },
        ];
        for bypass in bypasses {
            let expected = match bypass {
                Bypass::Busy
                | Bypass::Recovering
                | Bypass::NamespaceLockBusy
                | Bypass::ScratchExhausted
                | Bypass::FactBudget
                | Bypass::Retiring
                | Bypass::BuilderFenced
                | Bypass::Fenced
                | Bypass::TooManyHolds => Retry,
                Bypass::Full | Bypass::OverBudget | Bypass::WalPressure | Bypass::LowDiskSpace => {
                    Prune
                }
                Bypass::Integrity(_) => ResetNamespace,
                Bypass::Corrupt
                | Bypass::ReadOnly
                | Bypass::ForeignDatabase
                | Bypass::UnsupportedContainer { .. }
                | Bypass::WalUnavailable
                | Bypass::Location(_)
                | Bypass::SqliteTooOld { .. } => FreshDirectory,
                Bypass::InvalidLimits(_)
                | Bypass::InvalidMaintenanceLimits
                | Bypass::OutOfMemory
                | Bypass::TooLarge(_)
                | Bypass::InvalidRowKind { .. }
                | Bypass::EvidenceForLocalArtifact
                | Bypass::InvalidRecord(_)
                | Bypass::MissingReference
                | Bypass::DuplicateMember
                | Bypass::SealRefused(_)
                | Bypass::Io(_)
                | Bypass::Sqlite { .. } => Nothing,
            };
            assert_eq!(advice(&bypass), expected, "{bypass:?}");
        }
    }
}
