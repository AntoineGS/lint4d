//! Checkpoints and compaction, run by maintenance only (spec §10
//! amendment 2026-11-03, D12/D13): the interactive writer never
//! checkpoints (`wal_autocheckpoint = 0`), PASSIVE never waits, and a
//! TRUNCATE is tried once, without waiting, after a complete copy. Both run
//! on a pooled read connection, never in the writer's slot.

use super::Store;
use super::budget::{Pressure, pressure_of, read_policy, read_usage};
use super::health::Access;
use super::limits::JOURNAL_SIZE_LIMIT_BYTES;
use super::namespaces::ensure_live;
use super::outcome::Bypass;
use super::sqlite::{begin, classify};
use rusqlite::{Connection, TransactionBehavior};

/// What one maintenance checkpoint achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpointed {
    /// Frames in the WAL when the PASSIVE checkpoint ran.
    pub wal_frames: i64,
    /// Frames it copied into the database.
    pub copied_frames: i64,
    /// The WAL file is now at most `JOURNAL_SIZE_LIMIT_BYTES` long. A
    /// truncation refused because a reader is still on an older snapshot
    /// leaves this false, and writes stay refused at the WAL threshold
    /// until a later step succeeds (Decision 24).
    pub truncated: bool,
}

impl Checkpointed {
    /// The whole WAL was copied.
    pub fn complete(&self) -> bool {
        self.copied_frames == self.wal_frames
    }
}

/// `(busy, wal frames, copied frames)`.
fn checkpoint_with(connection: &Connection, mode: &str) -> Result<(bool, i64, i64), Bypass> {
    connection
        .query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |row| {
            Ok((row.get::<_, i64>(0)? != 0, row.get(1)?, row.get(2)?))
        })
        .map_err(|error| classify(&error))
}

impl Store {
    /// Copy the WAL into the database without waiting for anyone and, when
    /// that copied all of it and the file is longer than
    /// `JOURNAL_SIZE_LIMIT_BYTES`, try one non-waiting `TRUNCATE`.
    /// Always allowed, whatever the pressure.
    pub fn checkpoint(&self) -> Result<Checkpointed, Bypass> {
        self.guarded(Access::Maintenance, || {
            let result = {
                let reader = self.reader()?;
                let (_, wal_frames, copied_frames) = checkpoint_with(&reader, "PASSIVE")?;
                let wal = self.location.database_files()[1].clone();
                let long = std::fs::metadata(&wal)
                    .is_ok_and(|metadata| metadata.len() > JOURNAL_SIZE_LIMIT_BYTES as u64);
                let truncated = if !long {
                    true
                } else if wal_frames == copied_frames {
                    reader
                        .busy_timeout(std::time::Duration::ZERO)
                        .map_err(|error| classify(&error))?;
                    let outcome = checkpoint_with(&reader, "TRUNCATE");
                    reader
                        .busy_timeout(self.limits.wait)
                        .map_err(|error| classify(&error))?;
                    !outcome?.0
                } else {
                    false
                };
                Checkpointed {
                    wal_frames,
                    copied_frames,
                    truncated,
                }
            };
            fault_point!(self, CheckpointCompleted);
            Ok(result)
        })
    }

    /// Release at most `limits.vacuum_pages` free pages in one `IMMEDIATE`
    /// transaction and return how many. Returns 0 unless the free pages
    /// exceed `limits.vacuum_free_bytes` or the store is under pressure; at
    /// the WAL threshold it writes nothing (`WalPressure`).
    pub fn vacuum_step(&self, limits: &super::MaintenanceLimits) -> Result<usize, Bypass> {
        limits
            .validate()
            .map_err(|_| Bypass::InvalidMaintenanceLimits)?;
        self.guarded(Access::Maintenance, || {
            let fail = |error: rusqlite::Error| classify(&error);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            ensure_live(&transaction, self.scope)?;
            let policy = read_policy(&transaction)?.policy;
            let before = read_usage(self, &transaction)?;
            match pressure_of(policy, &before) {
                Pressure::WalPressure => return Err(Bypass::WalPressure),
                Pressure::None if before.free_bytes() <= limits.vacuum_free_bytes => {
                    return Ok(0);
                }
                _ => {}
            }
            if before.free_pages == 0 {
                return Ok(0);
            }
            transaction
                .execute_batch(&format!(
                    "PRAGMA incremental_vacuum({})",
                    limits.vacuum_pages
                ))
                .map_err(fail)?;
            let after = read_usage(self, &transaction)?;
            fault_point!(self, VacuumBeforeCommit);
            self.before_commit();
            transaction.commit().map_err(fail)?;
            Ok(before.free_pages.saturating_sub(after.free_pages) as usize)
        })
    }
}
