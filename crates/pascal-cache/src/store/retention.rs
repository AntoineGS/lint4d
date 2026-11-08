//! Namespace retention (l4d-1ypi, spec §10 amendment 2026-11-08): which
//! live namespaces a retirement pass keeps, by recency and by size, and the
//! size estimates it uses.
//!
//! A namespace's last use is its lock file's modification time. Under
//! disk pressure a pass retires the least recently used kept namespace at
//! once; only a current size estimate lets it go on to the next. An
//! estimate is read for the next kept namespace after a pass stopped, on a
//! reader in chunks bounded per step and per cycle by maintenance, never at
//! open and never on the writer (reviews I1, I-new-1, M-r2-2), and recorded
//! beside the lock file (`<hex>.size`) for later passes, other processes
//! and `--cache-status`. A record is current while the namespace's epoch
//! and lock-file time are unchanged: rows of a namespace change only while
//! a process holds it, and every holder records its use at open, at least
//! once a minute while open, and at close.

use super::Store;
use super::budget::{ROW_OVERHEAD_BYTES, excess_bytes, read_policy, read_usage};
use super::location::StoreLocation;
use super::namespaces::{self, LIVE, RetireReport, Retirement, digest_from};
use super::outcome::Bypass;
use super::pool::{Deadline, Role};
use super::sqlite::{begin_within, classify};
use crate::Digest;
use rusqlite::{Connection, TransactionBehavior, params};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Live namespaces one pass reads from the database. They are ranked by
/// recency and the [`MAX_RANKED`] most recent are examined (review M9).
const MAX_LISTED: i64 = 4096;
const MAX_RANKED: usize = 256;
/// Namespaces retired per pass; the rest wait for the next one.
const MAX_RETIRED_PER_PASS: usize = 64;
/// Retired scopes whose estimates count as freed space, per pass.
const MAX_PENDING_SCOPES: i64 = 64;
/// How often an open namespace records its use (review M2).
pub(crate) const USE_REFRESH: Duration = Duration::from_secs(60);

/// The tables an estimate reads, each by a unique key of its scope: the
/// table, its key and the column of rows each entry lists. Observations
/// are not read: evidence sets' `content_bytes` count them.
const TABLES: [(&str, &str, &str); 3] = [
    ("artifacts", "artifact_id", "row_count"),
    ("evidence_sets", "evidence_id", "observation_count"),
    ("member_records", "record_id", "binding_count"),
];

/// One live namespace of a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ranked {
    pub(crate) id: Digest,
    pub(crate) epoch: i64,
    /// The lock file's time as read (the recorded estimates' key).
    pub(crate) used: Option<SystemTime>,
}

impl Ranked {
    /// The last use, a future time counting as now (review M3).
    pub(crate) fn last_used(&self) -> Option<SystemTime> {
        self.used.map(|used| used.min(SystemTime::now()))
    }
}

/// The modification time of namespace `id`'s lock file; `None` when the
/// file is gone or has no readable time.
fn lock_time(location: &StoreLocation, id: Digest) -> Option<SystemTime> {
    fs::symlink_metadata(location.namespace_lock(id))
        .ok()
        .filter(|metadata| metadata.is_file())
        .and_then(|metadata| metadata.modified().ok())
}

pub(crate) fn unix_seconds_of(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

fn nanos(time: Option<SystemTime>) -> Option<u128> {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_nanos())
}

/// Live namespaces other than `current`, the most recently used first (a
/// namespace without a lock file last, ties by id), at most [`MAX_RANKED`].
pub(crate) fn ranked_live(
    connection: &Connection,
    location: &StoreLocation,
    current: Option<Digest>,
) -> Result<Vec<Ranked>, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let rows: Vec<(Vec<u8>, i64)> = {
        let mut statement = connection
            .prepare_cached(
                "SELECT namespace_id, reset_epoch FROM namespaces \
                 WHERE state = ?1 AND namespace_id <> ?2 LIMIT ?3",
            )
            .map_err(fail)?;
        let excluded = current.map_or(Vec::new(), |id| id.as_bytes().to_vec());
        statement
            .query_map(params![LIVE, excluded, MAX_LISTED], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(fail)?
            .collect::<Result<_, _>>()
            .map_err(fail)?
    };
    let mut ranked = rows
        .into_iter()
        .map(|(id, epoch)| {
            let id = digest_from(&id)?;
            Ok(Ranked {
                id,
                epoch,
                used: lock_time(location, id),
            })
        })
        .collect::<Result<Vec<_>, Bypass>>()?;
    ranked.sort_by(|a, b| {
        b.last_used()
            .cmp(&a.last_used())
            .then_with(|| a.id.as_bytes().cmp(b.id.as_bytes()))
    });
    ranked.truncate(MAX_RANKED);
    Ok(ranked)
}

/// A size estimate as recorded: the scope and lock-file time it was read
/// at, and the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Recorded {
    pub(crate) epoch: i64,
    used: Option<u128>,
    pub(crate) bytes: u64,
}

impl Recorded {
    fn current_for(&self, ranked: &Ranked) -> bool {
        self.epoch == ranked.epoch && self.used == nanos(ranked.used)
    }

    fn encode(&self) -> String {
        let used = self.used.map_or("-".to_string(), |used| used.to_string());
        format!("l4d-size 1 {} {used} {}\n", self.epoch, self.bytes)
    }

    fn decode(text: &str) -> Option<Self> {
        let mut fields = text.strip_suffix('\n')?.split(' ');
        if (fields.next()?, fields.next()?) != ("l4d-size", "1") {
            return None;
        }
        let epoch = fields.next()?.parse().ok()?;
        let used = match fields.next()? {
            "-" => None,
            used => Some(used.parse().ok()?),
        };
        let bytes = fields.next()?.parse().ok()?;
        fields
            .next()
            .is_none()
            .then_some(Self { epoch, used, bytes })
    }
}

/// The estimate recorded for namespace `id`, if any is readable. At most
/// 64 bytes are read.
pub(crate) fn read_recorded(location: &StoreLocation, id: Digest) -> Option<Recorded> {
    let path = location.namespace_size(id);
    StoreLocation::check_file(&path).ok()?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(options.open(path).ok()?, 64),
        &mut text,
    )
    .ok()?;
    Recorded::decode(&text)
}

/// Record `recorded` for namespace `id`: a private temporary file renamed
/// over the record, so a reader sees the old or the new one whole.
fn write_recorded(location: &StoreLocation, id: Digest, recorded: Recorded) -> Result<(), Bypass> {
    let io = |error: std::io::Error| Bypass::Io(error.kind());
    let path = location.namespace_size(id);
    StoreLocation::check_file(&path)?;
    let temporary = path.with_extension(format!(
        "size.{:032x}.tmp",
        u128::from_be_bytes(super::nonce::nonce())
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let written = options
        .open(&temporary)
        .and_then(|mut file| file.write_all(recorded.encode().as_bytes()))
        .and_then(|()| fs::rename(&temporary, &path));
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written.map_err(io)
}

/// Entries of `namespaces/` [`clean_temporaries`] looks at, and how old a
/// record temporary must be to be removed (its writer crashed).
const MAX_CLEANED_ENTRIES: usize = 256;
const STALE_TEMPORARY: Duration = Duration::from_secs(300);

/// Remove record temporaries (`<hex>.size.<nonce>.tmp`) older than
/// [`STALE_TEMPORARY`] among the first [`MAX_CLEANED_ENTRIES`] entries of
/// `namespaces/` (review M-new-4). Best effort.
pub(crate) fn clean_temporaries(location: &StoreLocation) {
    let Ok(entries) = fs::read_dir(location.namespace_locks()) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.take(MAX_CLEANED_ENTRIES).flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.ends_with(".tmp") && name.contains(".size.")) {
            continue;
        }
        let stale = entry
            .metadata()
            .ok()
            .filter(|metadata| metadata.is_file())
            .and_then(|metadata| metadata.modified().ok())
            .is_some_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age >= STALE_TEMPORARY)
            });
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// An estimate being read, one chunk per maintenance transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scan {
    ranked: Ranked,
    table: usize,
    cursor: Vec<u8>,
    bytes: u64,
}

/// A store's estimates: the scan in progress, the records read, and the
/// estimates of the scopes it retired, which count as freed space until
/// they are reclaimed.
#[derive(Debug, Default)]
pub(crate) struct Estimates {
    scan: Option<Scan>,
    recorded: HashMap<Digest, Recorded>,
    retired: HashMap<(Digest, i64), u64>,
}

/// What one [`Store::estimate_for`] transaction did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Estimate {
    /// Read this many entries (at least 1); the scan goes on.
    Read(usize),
    /// The estimate is recorded.
    Done,
    /// Held, or used since the scan started: given up for this cycle.
    Abandoned,
}

impl Estimates {
    /// The current estimate of `ranked`, from memory or its record.
    fn current(&mut self, location: &StoreLocation, ranked: &Ranked) -> Option<u64> {
        if let Some(recorded) = self.recorded.get(&ranked.id)
            && recorded.current_for(ranked)
        {
            return Some(recorded.bytes);
        }
        let recorded = read_recorded(location, ranked.id)?;
        self.recorded.insert(ranked.id, recorded);
        recorded.current_for(ranked).then_some(recorded.bytes)
    }
}

/// One chunk of at most `limit` entries of `table` in `scan`'s scope:
/// the entries read and their estimated bytes, advancing the cursor.
fn read_chunk(connection: &Connection, scan: &mut Scan, limit: usize) -> Result<usize, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let (table, key, rows) = TABLES[scan.table];
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT {key}, content_bytes + ?4 * (1 + {rows}) FROM {table} \
             WHERE namespace_id = ?1 AND reset_epoch = ?2 AND {key} > ?3 \
             ORDER BY {key} LIMIT ?5"
        ))
        .map_err(fail)?;
    let mut entries = statement
        .query(params![
            scan.ranked.id.as_bytes().as_slice(),
            scan.ranked.epoch,
            scan.cursor,
            ROW_OVERHEAD_BYTES as i64,
            i64::try_from(limit).unwrap_or(i64::MAX)
        ])
        .map_err(fail)?;
    let mut read = 0;
    while let Some(entry) = entries.next().map_err(fail)? {
        scan.cursor = entry.get(0).map_err(fail)?;
        let bytes: i64 = entry.get(1).map_err(fail)?;
        scan.bytes = scan.bytes.saturating_add(u64::try_from(bytes).unwrap_or(0));
        read += 1;
    }
    if read < limit {
        scan.table += 1;
        scan.cursor.clear();
    }
    Ok(read)
}

impl Store {
    fn estimates(&self) -> std::sync::MutexGuard<'_, Estimates> {
        self.estimates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record that this process still uses its namespace, at most once per
    /// [`USE_REFRESH`] (review M2): a crashed long session is not ranked
    /// as old as its start.
    pub(crate) fn refresh_use(&self, now: u64) {
        let last = self.last_use.load(std::sync::atomic::Ordering::Relaxed);
        if now.saturating_sub(last) >= USE_REFRESH.as_secs() {
            self._lock.touch();
            self.last_use
                .store(now, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Read one chunk of at most `limit` entries (`limit >= 1`) of
    /// `target`'s estimate on a reader, resuming its scan. A namespace used
    /// since its scan began (its rows may have changed) is abandoned; the
    /// next cycle starts again.
    pub(crate) fn estimate_for(&self, target: &Ranked, limit: usize) -> Result<Estimate, Bypass> {
        let deadline = Deadline::after(self.limits.wait);
        let mut estimates = self.estimates();
        let used = lock_time(&self.location, target.id);
        if let Some(scan) = &estimates.scan
            && (scan.ranked.id != target.id || scan.ranked.epoch != target.epoch)
        {
            estimates.scan = None;
        }
        if let Some(scan) = &estimates.scan
            && nanos(used) != nanos(scan.ranked.used)
        {
            estimates.scan = None;
            return Ok(Estimate::Abandoned);
        }
        if estimates.scan.is_none() {
            if namespaces::held(&self.location.namespace_lock(target.id))? {
                return Ok(Estimate::Abandoned);
            }
            estimates.scan = Some(Scan {
                ranked: Ranked { used, ..*target },
                table: 0,
                cursor: Vec::new(),
                bytes: 0,
            });
        }
        let scan = estimates.scan.as_mut().expect("set above");
        let read = {
            let mut reader = self.pool.acquire(Role::Reader, deadline)?;
            let transaction = begin_within(&mut reader, TransactionBehavior::Deferred, deadline)?;
            fault_point!(self, EstimateChunk);
            read_chunk(&transaction, scan, limit)?
        };
        if scan.table < TABLES.len() {
            return Ok(Estimate::Read(read.max(1)));
        }
        let scan = estimates.scan.take().expect("set above");
        if nanos(lock_time(&self.location, target.id)) != nanos(scan.ranked.used) {
            return Ok(Estimate::Abandoned);
        }
        let recorded = Recorded {
            epoch: scan.ranked.epoch,
            used: nanos(scan.ranked.used),
            bytes: scan.bytes,
        };
        // Best effort: an unrecorded estimate is kept in memory.
        let _ = write_recorded(&self.location, scan.ranked.id, recorded);
        estimates.recorded.insert(scan.ranked.id, recorded);
        Ok(Estimate::Done)
    }

    /// One retirement pass (see [`Store::retire_obsolete`]). The ranking,
    /// the excess and the estimates are read on a reader and from the
    /// records; only each retirement takes the writer, briefly. No
    /// namespace's rows are read.
    pub(crate) fn retire_within(&self, deadline: Deadline) -> Result<RetireReport, Bypass> {
        self.retire_pass_within(deadline).map(|(report, _)| report)
    }

    /// [`Self::retire_within`] for maintenance: also, when the pass stopped
    /// at a retirement of unknown size with the excess not covered, the
    /// next kept namespace without a current estimate, unheld: estimating
    /// it lets a later pass retire past it.
    pub(crate) fn retire_pass(&self) -> Result<(RetireReport, Option<Ranked>), Bypass> {
        self.retire_pass_within(Deadline::after(self.limits.open_wait))
    }

    fn retire_pass_within(
        &self,
        deadline: Deadline,
    ) -> Result<(RetireReport, Option<Ranked>), Bypass> {
        let current = self.scope.namespace;
        let (ranked, mut excess, retired_scopes) = {
            let mut reader = self.pool.acquire(Role::Reader, deadline)?;
            let transaction = begin_within(&mut reader, TransactionBehavior::Deferred, deadline)?;
            let ranked = ranked_live(&transaction, &self.location, Some(current))?;
            let policy = read_policy(&transaction)?.policy;
            let excess = excess_bytes(policy, &read_usage(self, &transaction)?);
            let retired_scopes = if excess > 0 {
                retired_scopes(&transaction)?
            } else {
                Vec::new()
            };
            (ranked, excess, retired_scopes)
        };
        fault_point!(self, NamespacesRanked);
        let mut estimates = self.estimates();
        // A retired scope still to reclaim of unknown size stops the
        // pressure part: what it frees shows only once it is reclaimed
        // (review I-r3-1).
        let mut pending_unknown = false;
        if excess > 0 {
            estimates
                .retired
                .retain(|scope, _| retired_scopes.contains(scope));
            let mut pending = 0u64;
            for scope in &retired_scopes {
                let bytes = estimates.retired.get(scope).copied().or_else(|| {
                    read_recorded(&self.location, scope.0)
                        .filter(|recorded| recorded.epoch == scope.1)
                        .map(|recorded| recorded.bytes)
                });
                match bytes {
                    Some(bytes) => pending = pending.saturating_add(bytes),
                    None => pending_unknown = true,
                }
            }
            excess = excess.saturating_sub(pending);
        }
        let kept = self
            .limits
            .max_namespaces
            .saturating_sub(1)
            .min(ranked.len());
        let (recent, older) = ranked.split_at(kept);
        let mut report = RetireReport::default();
        // A retirement of unknown size ends the pressure part (review
        // I-r2-1): what it freed shows after reclamation, in a later pass.
        let mut stopped = pending_unknown;
        let retire = |candidate: &Ranked,
                      estimates: &mut Estimates,
                      report: &mut RetireReport|
         -> Result<Option<Option<u64>>, Bypass> {
            // A current record only: one whose lock-file time no longer
            // matches is no size.
            let bytes = estimates.current(&self.location, candidate);
            let outcome = {
                let mut writer = self.pool.acquire(Role::Writer, deadline)?;
                namespaces::retire_one(
                    &mut writer,
                    &self.location,
                    candidate.id,
                    candidate.epoch,
                    candidate.used,
                    deadline,
                )?
            };
            Ok(match outcome {
                Retirement::Done => {
                    report.retired += 1;
                    match bytes {
                        // The record stays until the scope is reclaimed,
                        // so other processes count it as pending.
                        Some(bytes) => {
                            estimates
                                .retired
                                .insert((candidate.id, candidate.epoch), bytes);
                        }
                        None => {
                            report.unknown_size += 1;
                            // A stale record is no size for anyone.
                            estimates.recorded.remove(&candidate.id);
                            let _ = fs::remove_file(self.location.namespace_size(candidate.id));
                        }
                    }
                    Some(bytes)
                }
                Retirement::Held => {
                    report.in_use += 1;
                    None
                }
                Retirement::Gone | Retirement::UsedSince => None,
            })
        };
        // The count rule needs no size.
        for candidate in older.iter().rev() {
            if report.retired == MAX_RETIRED_PER_PASS {
                break;
            }
            match retire(candidate, &mut estimates, &mut report)? {
                Some(Some(bytes)) => excess = excess.saturating_sub(bytes),
                Some(None) => stopped = true,
                None => {}
            }
        }
        // The pressure: the least recently used kept namespace goes at
        // once, size known or not (review M-r2-2); only a current estimate
        // lets the pass go on to the next one.
        let mut next = None;
        for candidate in recent.iter().rev() {
            if excess == 0 || report.retired == MAX_RETIRED_PER_PASS {
                break;
            }
            if stopped {
                // Worth estimating, so that a later pass may go past it.
                if estimates.current(&self.location, candidate).is_none()
                    && !namespaces::held(&self.location.namespace_lock(candidate.id))?
                {
                    next = Some(*candidate);
                    break;
                }
                continue;
            }
            match retire(candidate, &mut estimates, &mut report)? {
                Some(Some(bytes)) => excess = excess.saturating_sub(bytes),
                Some(None) => stopped = true,
                None => {}
            }
        }
        report.kept = ranked.len().saturating_sub(report.retired + report.in_use);
        Ok((report, next))
    }
}

/// Remove the records of `scopes`, reclaimed: a record of another epoch
/// stays. Best effort.
pub(crate) fn forget_reclaimed(location: &StoreLocation, scopes: &[(Digest, i64)]) {
    for (id, epoch) in scopes {
        if read_recorded(location, *id).is_some_and(|recorded| recorded.epoch == *epoch) {
            let _ = fs::remove_file(location.namespace_size(*id));
        }
    }
}

/// The retired scopes still to reclaim, at most [`MAX_PENDING_SCOPES`].
fn retired_scopes(connection: &Connection) -> Result<Vec<(Digest, i64)>, Bypass> {
    let fail = |error: rusqlite::Error| classify(&error);
    let mut statement = connection
        .prepare_cached("SELECT namespace_id, reset_epoch FROM retired_scopes LIMIT ?1")
        .map_err(fail)?;
    let rows: Vec<(Vec<u8>, i64)> = statement
        .query_map([MAX_PENDING_SCOPES], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(fail)?
        .collect::<Result<_, _>>()
        .map_err(fail)?;
    rows.into_iter()
        .map(|(id, epoch)| Ok((digest_from(&id)?, epoch)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_rejects_anything_else() {
        let recorded = Recorded {
            epoch: 3,
            used: Some(1_700_000_000_123_456_789),
            bytes: 42,
        };
        assert_eq!(Recorded::decode(&recorded.encode()), Some(recorded));
        let unused = Recorded {
            used: None,
            ..recorded
        };
        assert_eq!(Recorded::decode(&unused.encode()), Some(unused));
        for bad in [
            "",
            "l4d-size 1 3 - 42",
            "l4d-size 2 3 - 42\n",
            "l4d-size 1 3 - 42 9\n",
            "l4d-size 1 x - 42\n",
        ] {
            assert_eq!(Recorded::decode(bad), None, "{bad:?}");
        }
    }
}
