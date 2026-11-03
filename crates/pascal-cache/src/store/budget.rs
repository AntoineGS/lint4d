//! Store-wide disk policy and write admission (spec §10 and its amendment
//! 2026-11-03, D14). Writers are serialized by SQLite, so a check made
//! inside the write's IMMEDIATE transaction against the policy read there
//! is the reservation. The budget is a managed target, not a hard quota.

use super::Store;
use super::health::Access;
use super::outcome::Bypass;
use super::pool::{Deadline, Role};
use super::sqlite::{begin, begin_within, classify};
use rusqlite::{Connection, TransactionBehavior, params};

pub const MIN_WAL_THRESHOLD_BYTES: u64 = 8 << 20;
pub const MIN_BUDGET_TO_WAL_RATIO: u64 = 4;
pub const ROW_OVERHEAD_BYTES: u64 = 64;
pub const LOW_SPACE_MARGIN_BYTES: u64 = 256 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorePolicy {
    pub max_disk_bytes: u64,
    pub wal_threshold_bytes: u64,
}

impl Default for StorePolicy {
    fn default() -> Self {
        Self {
            max_disk_bytes: 8 << 30,
            wal_threshold_bytes: 256 << 20,
        }
    }
}

impl StorePolicy {
    /// Whether the policy keeps room for maintenance.
    pub fn check(&self) -> Result<(), PolicyError> {
        if self.wal_threshold_bytes < MIN_WAL_THRESHOLD_BYTES {
            return Err(PolicyError::WalThresholdTooSmall);
        }
        if self.max_disk_bytes
            < self
                .wal_threshold_bytes
                .saturating_mul(MIN_BUDGET_TO_WAL_RATIO)
        {
            return Err(PolicyError::NoMaintenanceHeadroom);
        }
        Ok(())
    }
}

/// The store's policy and the revision it was read at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyState {
    pub revision: u64,
    pub policy: StorePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    WalThresholdTooSmall,
    NoMaintenanceHeadroom,
    /// Another administrator changed the policy first.
    RevisionChanged {
        current: u64,
    },
    Store(Bypass),
}

/// The physical size of the store and the room around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub shm_bytes: u64,
    pub page_size: u64,
    pub page_count: u64,
    pub free_pages: u64,
    pub filesystem_free_bytes: Option<u64>,
}

impl Usage {
    pub fn physical_bytes(&self) -> u64 {
        self.database_bytes
            .saturating_add(self.wal_bytes)
            .saturating_add(self.shm_bytes)
    }

    /// Bytes of free pages inside the database file.
    pub fn free_bytes(&self) -> u64 {
        self.free_pages.saturating_mul(self.page_size)
    }
}

/// What keeps new writes out, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    None,
    /// The store has no maintenance headroom left under its budget.
    OverHeadroom,
    WalPressure,
    LowDiskSpace,
}

/// The size of a write, estimated before its transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WriteEstimate {
    pub(crate) bytes: u64,
    pub(crate) rows: u64,
}

impl WriteEstimate {
    pub(crate) fn charged(&self) -> u64 {
        self.bytes
            .saturating_add(self.rows.saturating_mul(ROW_OVERHEAD_BYTES))
    }

    pub(crate) fn plus(self, other: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_add(other.bytes),
            rows: self.rows.saturating_add(other.rows),
        }
    }
}

/// The admission decision, separated from IO so it is unit-tested.
pub(crate) fn decide(
    policy: StorePolicy,
    usage: &Usage,
    estimate: WriteEstimate,
) -> Result<(), Bypass> {
    if usage.wal_bytes >= policy.wal_threshold_bytes {
        return Err(Bypass::WalPressure);
    }
    let charged = estimate.charged();
    // Once in the WAL, then in the database where free pages cannot absorb it.
    let growth = charged.saturating_add(charged.saturating_sub(usage.free_bytes()));
    let needed = usage
        .physical_bytes()
        .saturating_add(growth)
        .saturating_add(policy.wal_threshold_bytes);
    if usage
        .filesystem_free_bytes
        .is_some_and(|free| free < growth.saturating_add(LOW_SPACE_MARGIN_BYTES))
    {
        return Err(Bypass::LowDiskSpace);
    }
    if needed > policy.max_disk_bytes {
        return Err(Bypass::OverBudget);
    }
    Ok(())
}

/// What keeps new writes out under `policy`, if anything.
pub(crate) fn pressure_of(policy: StorePolicy, usage: &Usage) -> Pressure {
    if usage.wal_bytes >= policy.wal_threshold_bytes {
        Pressure::WalPressure
    } else if usage
        .filesystem_free_bytes
        .is_some_and(|free| free <= LOW_SPACE_MARGIN_BYTES)
    {
        Pressure::LowDiskSpace
    } else if usage
        .physical_bytes()
        .saturating_add(policy.wal_threshold_bytes)
        > policy.max_disk_bytes
    {
        Pressure::OverHeadroom
    } else {
        Pressure::None
    }
}

pub(crate) fn read_policy(connection: &Connection) -> Result<PolicyState, Bypass> {
    connection
        .query_row(
            "SELECT policy_revision, max_disk_bytes, wal_threshold_bytes \
             FROM store_state WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|error| classify(&error))
        .and_then(|(revision, max, wal)| {
            Ok(PolicyState {
                revision: u64::try_from(revision).map_err(|_| Bypass::Corrupt)?,
                policy: StorePolicy {
                    max_disk_bytes: u64::try_from(max).map_err(|_| Bypass::Corrupt)?,
                    wal_threshold_bytes: u64::try_from(wal).map_err(|_| Bypass::Corrupt)?,
                },
            })
        })
}

fn pragma(connection: &Connection, name: &str) -> Result<u64, Bypass> {
    let value: i64 = connection
        .pragma_query_value(None, name, |row| row.get(0))
        .map_err(|error| classify(&error))?;
    u64::try_from(value).map_err(|_| Bypass::Corrupt)
}

#[cfg(unix)]
fn filesystem_free_bytes(directory: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(directory.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string and `stat` is writable
    // storage of the right type; it is read only after a zero return.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: initialized by the successful call above.
    let stat = unsafe { stat.assume_init() };
    #[allow(clippy::unnecessary_cast)]
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(not(unix))]
fn filesystem_free_bytes(_directory: &std::path::Path) -> Option<u64> {
    None
}

pub(crate) fn read_usage(store: &Store, connection: &Connection) -> Result<Usage, Bypass> {
    let [database, wal, shm] = store
        .location
        .database_files()
        .map(|file| std::fs::metadata(file).map_or(0, |metadata| metadata.len()));
    #[allow(unused_mut)]
    let mut usage = Usage {
        database_bytes: database,
        wal_bytes: wal,
        shm_bytes: shm,
        page_size: pragma(connection, "page_size")?,
        page_count: pragma(connection, "page_count")?,
        free_pages: pragma(connection, "freelist_count")?,
        filesystem_free_bytes: filesystem_free_bytes(store.location.directory()),
    };
    #[cfg(feature = "test-support")]
    {
        usage.database_bytes = usage
            .database_bytes
            .saturating_add(store.hooks.pretend_usage());
        if let Some(free) = store.hooks.pretend_free_space() {
            usage.filesystem_free_bytes = Some(free);
        }
    }
    Ok(usage)
}

impl Store {
    /// Refuse the write when the policy read in `transaction` leaves no
    /// room for it. Called inside the write's IMMEDIATE transaction, so the
    /// check is the reservation.
    pub(crate) fn admit_write(
        &self,
        transaction: &Connection,
        estimate: WriteEstimate,
    ) -> Result<(), Bypass> {
        let policy = read_policy(transaction)?.policy;
        decide(policy, &read_usage(self, transaction)?, estimate)?;
        let now = self.clock.unix_seconds();
        if super::leases::due(&self.lease, now) {
            super::leases::renew(transaction, self.scope, &self.lease, now)?;
        }
        Ok(())
    }

    /// The store-wide policy and its revision.
    pub fn policy(&self) -> Result<PolicyState, Bypass> {
        self.guarded(Access::Read, || {
            let mut reader = self.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            read_policy(&transaction)
        })
    }

    /// The physical size of the store and the room around it.
    pub fn usage(&self) -> Result<Usage, Bypass> {
        self.guarded(Access::Read, || {
            let mut reader = self.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            read_usage(self, &transaction)
        })
    }

    /// What currently keeps new writes out, if anything.
    pub fn pressure(&self) -> Result<Pressure, Bypass> {
        self.guarded(Access::Read, || {
            let mut reader = self.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            let policy = read_policy(&transaction)?.policy;
            Ok(pressure_of(policy, &read_usage(self, &transaction)?))
        })
    }

    /// Replace the policy if it is still at `expected_revision`.
    pub fn set_policy(
        &self,
        expected_revision: u64,
        policy: StorePolicy,
    ) -> Result<PolicyState, PolicyError> {
        policy.check()?;
        self.guarded(Access::Maintenance, || {
            Ok(self.change_policy(expected_revision, policy))
        })
        .map_err(PolicyError::Store)?
    }

    fn change_policy(
        &self,
        expected_revision: u64,
        policy: StorePolicy,
    ) -> Result<PolicyState, PolicyError> {
        let store = PolicyError::Store;
        let deadline = Deadline::after(self.limits.open_wait);
        let mut writer = self.pool.acquire(Role::Writer, deadline).map_err(store)?;
        let transaction =
            begin_within(&mut writer, TransactionBehavior::Immediate, deadline).map_err(store)?;
        let to_i64 =
            |value: u64| i64::try_from(value).map_err(|_| PolicyError::Store(Bypass::Corrupt));
        let changed = transaction
            .execute(
                "UPDATE store_state SET max_disk_bytes = ?1, wal_threshold_bytes = ?2, \
                 policy_revision = policy_revision + 1 \
                 WHERE singleton = 1 AND policy_revision = ?3",
                params![
                    to_i64(policy.max_disk_bytes)?,
                    to_i64(policy.wal_threshold_bytes)?,
                    to_i64(expected_revision)?
                ],
            )
            .map_err(|error| PolicyError::Store(classify(&error)))?;
        let current = read_policy(&transaction).map_err(store)?;
        if changed == 0 {
            return Err(PolicyError::RevisionChanged {
                current: current.revision,
            });
        }
        transaction
            .commit()
            .map_err(|error| PolicyError::Store(classify(&error)))?;
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    fn usage(physical: u64, wal: u64, free: u64) -> Usage {
        Usage {
            database_bytes: physical - wal,
            wal_bytes: wal,
            shm_bytes: 0,
            page_size: 4096,
            page_count: (physical - wal) / 4096,
            free_pages: free / 4096,
            filesystem_free_bytes: Some(u64::MAX / 2),
        }
    }

    const POLICY: StorePolicy = StorePolicy {
        max_disk_bytes: 64 * MIB,
        wal_threshold_bytes: 16 * MIB,
    };

    #[test]
    fn a_write_needs_the_maintenance_headroom() {
        let small = WriteEstimate {
            bytes: MIB,
            rows: 1,
        };
        assert_eq!(decide(POLICY, &usage(40 * MIB, 0, 0), small), Ok(()));
        assert_eq!(
            decide(POLICY, &usage(47 * MIB, 0, 0), small),
            Err(Bypass::OverBudget)
        );
    }

    #[test]
    fn free_pages_absorb_the_main_file_growth_but_not_the_wal() {
        let write = WriteEstimate {
            bytes: 4 * MIB,
            rows: 0,
        };
        // 40 + 4 (WAL) + 4 (file) + 16 > 64; with 4 MiB free pages the file does not grow.
        assert_eq!(
            decide(POLICY, &usage(41 * MIB, 0, 0), write),
            Err(Bypass::OverBudget)
        );
        assert_eq!(decide(POLICY, &usage(41 * MIB, 0, 4 * MIB), write), Ok(()));
    }

    #[test]
    fn the_wal_threshold_refuses_every_write() {
        let tiny = WriteEstimate { bytes: 1, rows: 0 };
        assert_eq!(
            decide(POLICY, &usage(20 * MIB, 16 * MIB, 0), tiny),
            Err(Bypass::WalPressure)
        );
    }

    #[test]
    fn low_free_space_refuses_before_the_budget() {
        let mut low = usage(MIB, 0, 0);
        low.filesystem_free_bytes = Some(LOW_SPACE_MARGIN_BYTES);
        assert_eq!(
            decide(POLICY, &low, WriteEstimate { bytes: 1, rows: 0 }),
            Err(Bypass::LowDiskSpace)
        );
    }

    #[test]
    fn rows_are_charged_their_overhead() {
        assert_eq!(
            WriteEstimate { bytes: 10, rows: 2 }.charged(),
            10 + 2 * ROW_OVERHEAD_BYTES
        );
    }

    #[test]
    fn policies_keep_maintenance_headroom() {
        assert_eq!(StorePolicy::default().check(), Ok(()));
        assert_eq!(
            StorePolicy {
                max_disk_bytes: 1 << 30,
                wal_threshold_bytes: MIN_WAL_THRESHOLD_BYTES - 1
            }
            .check(),
            Err(PolicyError::WalThresholdTooSmall)
        );
        assert_eq!(
            StorePolicy {
                max_disk_bytes: 4 * 8 * MIB - 1,
                wal_threshold_bytes: 8 * MIB
            }
            .check(),
            Err(PolicyError::NoMaintenanceHeadroom)
        );
    }
}
