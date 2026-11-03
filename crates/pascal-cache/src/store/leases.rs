//! Process leases (spec §9 amendment 2026-11-03): each open store renews
//! one row at most once a minute; maintenance spares what live leases
//! hold and rows published since their last renewal. Build and snapshot
//! holds name the snapshots a process uses; recency hints bump the
//! sequence of rows a process reused, coalesced and rate-limited.

use super::Store;
use super::health::Access;
use super::namespaces::{LIVE, Scope, ensure_live, epoch};
use super::outcome::Bypass;
use super::pool::{Deadline, Role};
use super::sequence::next_publication_seq;
use super::snapshots::{SnapshotHeader, SnapshotId};
use super::sqlite::{begin, begin_within, classify};
use crate::Digest;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

pub const LEASE_RENEWAL: Duration = Duration::from_secs(60);
pub const LEASE_EXPIRY: Duration = Duration::from_secs(600);
pub const MAX_LEASE_HOLDS: usize = 16;
pub const MAX_PENDING_HINTS: usize = 1024;

const SEALED: i64 = 2;

pub(crate) struct ProcessLease {
    pub(crate) id: [u8; 16],
    /// Unix seconds of the last renewal this process made.
    renewed: Mutex<u64>,
}

impl ProcessLease {
    pub(crate) fn new(id: [u8; 16]) -> Self {
        Self {
            id,
            renewed: Mutex::new(0),
        }
    }
}

const CURRENT_SEQ: &str = "(SELECT publication_seq FROM store_state WHERE singleton = 1)";

fn namespace(scope: Scope) -> Vec<u8> {
    scope.namespace.as_bytes().to_vec()
}

/// Renew `lease`, or insert it again when another process deleted it as
/// expired (its holds are gone with it). Call only when [`due`], at most
/// once per transaction, after `ensure_live` (spec §9: the grace horizon
/// depends on renewals being at least a minute apart).
pub(crate) fn renew(
    connection: &Connection,
    scope: Scope,
    lease: &ProcessLease,
    now: u64,
) -> Result<(), Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let params = params![
        namespace(scope),
        epoch(scope),
        lease.id.as_slice(),
        now as i64
    ];
    let changed = connection
        .execute(
            &format!(
                "UPDATE process_leases SET renewed_unix_seconds = ?4, floor_seq = renewal_seq, \
                 renewal_seq = {CURRENT_SEQ} \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND lease_id = ?3"
            ),
            params,
        )
        .map_err(fail)?;
    if changed == 0 {
        connection
            .execute(
                &format!(
                    "INSERT INTO process_leases (namespace_id, reset_epoch, lease_id, \
                     renewed_unix_seconds, renewal_seq, floor_seq) \
                     VALUES (?1, ?2, ?3, ?4, {CURRENT_SEQ}, {CURRENT_SEQ})"
                ),
                params,
            )
            .map_err(fail)?;
    }
    *lease.renewed.lock().unwrap_or_else(PoisonError::into_inner) = now;
    Ok(())
}

/// Make sure the lease row exists (a hold needs it for its foreign key,
/// and another process may have deleted it as expired) without moving an
/// existing floor. Call after `ensure_live`.
pub(crate) fn ensure_row(
    connection: &Connection,
    scope: Scope,
    lease: &ProcessLease,
    now: u64,
) -> Result<(), Bypass> {
    connection
        .execute(
            &format!(
                "INSERT OR IGNORE INTO process_leases (namespace_id, reset_epoch, lease_id, \
                 renewed_unix_seconds, renewal_seq, floor_seq) \
                 VALUES (?1, ?2, ?3, ?4, {CURRENT_SEQ}, {CURRENT_SEQ})"
            ),
            params![
                namespace(scope),
                epoch(scope),
                lease.id.as_slice(),
                now as i64
            ],
        )
        .map(|_| ())
        .map_err(|error| classify(&error))
}

pub(crate) fn due(lease: &ProcessLease, now: u64) -> bool {
    now.saturating_sub(*lease.renewed.lock().unwrap_or_else(PoisonError::into_inner))
        >= LEASE_RENEWAL.as_secs()
}

pub(crate) fn live_since(now: u64) -> i64 {
    now.saturating_sub(LEASE_EXPIRY.as_secs()) as i64
}

/// The lowest `floor_seq` of the scope's live leases, else the current
/// sequence: rows published above it are inside the grace window.
pub(crate) fn horizon(connection: &Connection, scope: Scope, now: u64) -> Result<i64, Bypass> {
    connection
        .query_row(
            &format!(
                "SELECT coalesce(min(floor_seq), {CURRENT_SEQ}) FROM process_leases \
                 WHERE namespace_id = ?1 AND reset_epoch = ?2 AND renewed_unix_seconds >= ?3"
            ),
            params![namespace(scope), epoch(scope), live_since(now)],
            |row| row.get(0),
        )
        .map_err(|error| classify(&error))
}

/// A row a process reused, whose sequence a flush bumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Hint {
    Evidence(Digest),
    Artifact(Digest),
    Record(Digest),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HintStats {
    pub pending: usize,
    /// Hints refused because `MAX_PENDING_HINTS` were pending.
    pub dropped: u64,
}

#[derive(Default)]
pub(crate) struct Hints {
    pending: Mutex<BTreeSet<Hint>>,
    dropped: AtomicU64,
    flushed: Mutex<Option<u64>>,
}

/// Delete this store's lease and its holds; best effort, one `wait`.
pub(crate) fn release(store: &Store) {
    let _ = (|| -> Result<(), Bypass> {
        let deadline = Deadline::after(store.limits.wait);
        let mut writer = store.pool.acquire(Role::Writer, deadline)?;
        let transaction = begin_within(&mut writer, TransactionBehavior::Immediate, deadline)?;
        let key = params![
            namespace(store.scope),
            epoch(store.scope),
            store.lease.id.as_slice()
        ];
        let fail = |error: rusqlite::Error| classify(&error);
        transaction
            .execute(
                "DELETE FROM lease_holds WHERE namespace_id = ?1 AND reset_epoch = ?2 \
                 AND lease_id = ?3",
                key,
            )
            .map_err(fail)?;
        transaction
            .execute(
                "DELETE FROM process_leases WHERE namespace_id = ?1 AND reset_epoch = ?2 \
                 AND lease_id = ?3",
                key,
            )
            .map_err(fail)?;
        transaction.commit().map_err(fail)
    })();
}

impl Store {
    /// Renew this store's lease if a renewal interval has passed since the
    /// last one; otherwise a no-op that touches nothing (not even the
    /// scope check). Writes renew inside their own transaction, so callers
    /// without writes of their own (maintenance) call this.
    pub fn renew_lease(&self) -> Result<(), Bypass> {
        self.guarded(Access::Maintenance, || {
            let now = self.clock.unix_seconds();
            if !due(&self.lease, now) {
                return Ok(());
            }
            fault_point!(self, LeaseRenewDecided);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            // A write of this store may have renewed while we waited for
            // the writer; renewing again would raise the floor too soon.
            let now = self.clock.unix_seconds();
            if !due(&self.lease, now) {
                return Ok(());
            }
            ensure_live(&transaction, self.scope)?;
            renew(&transaction, self.scope, &self.lease, now)?;
            transaction.commit().map_err(|error| classify(&error))
        })
    }

    /// Delete at most `max_rows` rows of expired leases (holds first, then
    /// lease rows left without holds), whatever their scope.
    pub fn expire_leases(&self, max_rows: usize) -> Result<usize, Bypass> {
        self.guarded(Access::Maintenance, || {
            let fail = |error: rusqlite::Error| classify(&error);
            let since = live_since(self.clock.unix_seconds());
            let limit = |rows: usize| i64::try_from(rows).unwrap_or(i64::MAX);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            let holds = transaction
                .execute(
                    "DELETE FROM lease_holds WHERE rowid IN (SELECT h.rowid FROM lease_holds h \
                     JOIN process_leases l ON l.namespace_id = h.namespace_id \
                     AND l.reset_epoch = h.reset_epoch AND l.lease_id = h.lease_id \
                     WHERE l.renewed_unix_seconds < ?1 LIMIT ?2)",
                    params![since, limit(max_rows)],
                )
                .map_err(fail)?;
            let leases = transaction
                .execute(
                    "DELETE FROM process_leases WHERE rowid IN (SELECT l.rowid FROM process_leases l \
                     WHERE l.renewed_unix_seconds < ?1 AND NOT EXISTS (SELECT 1 FROM lease_holds h \
                     WHERE h.namespace_id = l.namespace_id AND h.reset_epoch = l.reset_epoch \
                     AND h.lease_id = l.lease_id) LIMIT ?2)",
                    params![since, limit(max_rows - holds)],
                )
                .map_err(fail)?;
            transaction.commit().map_err(fail)?;
            Ok(holds + leases)
        })
    }

    /// Hold a sealed snapshot for this lease so maintenance keeps it. `false`
    /// when it is no longer sealed; at most `MAX_LEASE_HOLDS` sealed
    /// snapshots per lease (builds' own holds do not count).
    pub fn hold_snapshot(&self, header: &SnapshotHeader) -> Result<bool, Bypass> {
        self.guarded(Access::Maintenance, || {
            if header.scope != self.scope {
                return Err(Bypass::Fenced);
            }
            let fail = |error: rusqlite::Error| classify(&error);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            ensure_live(&transaction, self.scope)?;
            ensure_row(
                &transaction,
                self.scope,
                &self.lease,
                self.clock.unix_seconds(),
            )?;
            let key = params![
                namespace(self.scope),
                epoch(self.scope),
                self.lease.id.as_slice(),
                header.id.as_bytes().as_slice()
            ];
            let held: Option<i64> = transaction
                .query_row(
                    "SELECT 1 FROM lease_holds h JOIN snapshots s \
                     ON s.namespace_id = h.namespace_id AND s.reset_epoch = h.reset_epoch \
                     AND s.snapshot_id = h.snapshot_id \
                     WHERE h.namespace_id = ?1 AND h.reset_epoch = ?2 AND h.lease_id = ?3 \
                     AND h.snapshot_id = ?4 AND s.state = ?5",
                    params![
                        namespace(self.scope),
                        epoch(self.scope),
                        self.lease.id.as_slice(),
                        header.id.as_bytes().as_slice(),
                        SEALED
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(fail)?;
            if held.is_some() {
                return Ok(true);
            }
            let count: i64 = transaction
                .query_row(
                    "SELECT count(*) FROM lease_holds h JOIN snapshots s \
                     ON s.namespace_id = h.namespace_id AND s.reset_epoch = h.reset_epoch \
                     AND s.snapshot_id = h.snapshot_id \
                     WHERE h.namespace_id = ?1 AND h.reset_epoch = ?2 AND h.lease_id = ?3 \
                     AND s.state = ?4",
                    params![
                        namespace(self.scope),
                        epoch(self.scope),
                        self.lease.id.as_slice(),
                        SEALED
                    ],
                    |row| row.get(0),
                )
                .map_err(fail)?;
            if count as usize >= MAX_LEASE_HOLDS {
                return Err(Bypass::TooManyHolds);
            }
            let changed = transaction
                .execute(
                    "INSERT OR IGNORE INTO lease_holds (namespace_id, reset_epoch, lease_id, \
                     snapshot_id) SELECT namespace_id, reset_epoch, ?3, snapshot_id FROM snapshots \
                     WHERE namespace_id = ?1 AND reset_epoch = ?2 AND snapshot_id = ?4 \
                     AND state = 2",
                    key,
                )
                .map_err(fail)?;
            transaction.commit().map_err(fail)?;
            Ok(changed == 1)
        })
    }

    /// Release this lease's hold on `id`; `false` if it held none.
    pub fn release_snapshot(&self, id: SnapshotId) -> Result<bool, Bypass> {
        self.guarded(Access::Maintenance, || {
            let fail = |error: rusqlite::Error| classify(&error);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            ensure_live(&transaction, self.scope)?;
            let changed = transaction
                .execute(
                    "DELETE FROM lease_holds WHERE namespace_id = ?1 AND reset_epoch = ?2 \
                     AND lease_id = ?3 AND snapshot_id = ?4",
                    params![
                        namespace(self.scope),
                        epoch(self.scope),
                        self.lease.id.as_slice(),
                        id.as_bytes().as_slice()
                    ],
                )
                .map_err(fail)?;
            transaction.commit().map_err(fail)?;
            Ok(changed == 1)
        })
    }

    /// Note that a row was reused. No IO: a later [`Store::flush_hints`]
    /// writes it. Beyond `MAX_PENDING_HINTS` pending hints are dropped
    /// (hints are advisory).
    pub fn hint_used(&self, hint: Hint) {
        let mut pending = self
            .hints
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if pending.contains(&hint) {
            return;
        }
        if pending.len() >= MAX_PENDING_HINTS {
            self.hints.dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            pending.insert(hint);
        }
    }

    pub fn hint_stats(&self) -> HintStats {
        HintStats {
            pending: self
                .hints
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len(),
            dropped: self.hints.dropped.load(Ordering::Relaxed),
        }
    }

    /// Bump the sequence of every pending hinted row in one transaction;
    /// rows retired or deleted meanwhile are skipped. Returns the rows
    /// changed, or 0 when the last flush is less than `LEASE_RENEWAL` old.
    /// On a bypass the taken hints are dropped.
    pub fn flush_hints(&self) -> Result<usize, Bypass> {
        self.guarded(Access::Maintenance, || {
            let now = self.clock.unix_seconds();
            let mut flushed = self
                .hints
                .flushed
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if flushed.is_some_and(|last| now.saturating_sub(last) < LEASE_RENEWAL.as_secs()) {
                return Ok(0);
            }
            let taken = std::mem::take(
                &mut *self
                    .hints
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
            );
            if taken.is_empty() {
                return Ok(0);
            }
            let fail = |error: rusqlite::Error| classify(&error);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            ensure_live(&transaction, self.scope)?;
            let sequence = next_publication_seq(&transaction)?;
            let mut changed = 0;
            for hint in &taken {
                let (table, column, id) = match hint {
                    Hint::Evidence(id) => ("evidence_sets", "evidence_id", id),
                    Hint::Artifact(id) => ("artifacts", "artifact_id", id),
                    Hint::Record(id) => ("member_records", "record_id", id),
                };
                changed += transaction
                    .execute(
                        &format!(
                            "UPDATE {table} SET publication_seq = ?4 WHERE namespace_id = ?1 \
                             AND reset_epoch = ?2 AND {column} = ?3 AND lifecycle = {LIVE}"
                        ),
                        params![
                            namespace(self.scope),
                            epoch(self.scope),
                            id.as_bytes().as_slice(),
                            sequence
                        ],
                    )
                    .map_err(fail)?;
            }
            transaction.commit().map_err(fail)?;
            *flushed = Some(now);
            Ok(changed)
        })
    }
}
