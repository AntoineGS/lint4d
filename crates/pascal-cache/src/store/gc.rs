//! Retirement by reachability, top-down (spec §10 amendment 2026-11-03,
//! D12): abandoned builds and unused sealed snapshots, then member records
//! no building or sealed snapshot lists, then evidence sets and artifacts
//! no live record or snapshot references. Each batch is one `UPDATE` whose
//! subquery re-checks every condition inside the `IMMEDIATE` transaction,
//! so a publication either committed before it (and its reference keeps
//! the row live) or runs after it (and is refused as `Retiring`, spec §6.1
//! amendment 2026-11-03). Retired rows are deleted later, children first,
//! by [`delete_retired`] inside `Store::reclaim`.

use super::Store;
use super::budget::{Pressure, pressure_of, read_policy, read_usage};
use super::health::Access;
use super::leases::{horizon, live_since};
use super::namespaces::{Scope, ensure_live, epoch};
use super::outcome::Bypass;
use super::sqlite::{begin, classify};
use rusqlite::{Connection, TransactionBehavior, params};

/// Bounds of one maintenance pass. Callers may lower them, never raise
/// them above [`MaintenanceLimits::DEFAULT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceLimits {
    /// Sealed snapshots kept per context, newest publication first.
    pub keep_snapshots: usize,
    /// `keep_snapshots` while the store is over its headroom or low on disk.
    pub keep_snapshots_under_pressure: usize,
    /// Unreferenced rows within this many newest publications are kept,
    /// unless the store is under pressure.
    pub recency_window: u64,
    /// Rows one maintenance transaction changes at most.
    pub batch_rows: usize,
    /// Pages one `incremental_vacuum` releases at most.
    pub vacuum_pages: usize,
    /// Free bytes in the database file above which maintenance vacuums.
    pub vacuum_free_bytes: u64,
}

impl MaintenanceLimits {
    pub const DEFAULT: Self = Self {
        keep_snapshots: 4,
        keep_snapshots_under_pressure: 1,
        recency_window: 65_536,
        batch_rows: 256,
        vacuum_pages: 256,
        vacuum_free_bytes: 64 << 20,
    };

    /// Whether every bound is at or below its default (and the counts at
    /// least 1).
    pub fn validate(&self) -> Result<(), MaintenanceLimitsError> {
        let max = Self::DEFAULT;
        if !(1..=max.keep_snapshots).contains(&self.keep_snapshots) {
            return Err(MaintenanceLimitsError::KeepSnapshots);
        }
        if !(1..=self.keep_snapshots).contains(&self.keep_snapshots_under_pressure) {
            return Err(MaintenanceLimitsError::KeepSnapshotsUnderPressure);
        }
        if self.recency_window > max.recency_window {
            return Err(MaintenanceLimitsError::RecencyWindow);
        }
        if !(1..=max.batch_rows).contains(&self.batch_rows) {
            return Err(MaintenanceLimitsError::BatchRows);
        }
        if !(1..=max.vacuum_pages).contains(&self.vacuum_pages) {
            return Err(MaintenanceLimitsError::VacuumPages);
        }
        Ok(())
    }
}

impl Default for MaintenanceLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// The bound a [`MaintenanceLimits`] breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceLimitsError {
    KeepSnapshots,
    KeepSnapshotsUnderPressure,
    RecencyWindow,
    BatchRows,
    VacuumPages,
}

/// One retirement pass, in the order maintenance runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetireStep {
    /// Building snapshots no live lease holds.
    AbandonedBuilds,
    /// Sealed snapshots beyond the newest kept per context, unheld.
    UnusedSnapshots,
    /// Live member records no building or sealed snapshot lists.
    Records,
    /// Live evidence sets no live record or building/sealed snapshot references.
    Evidence,
    /// Live artifacts no live record or evidence set references.
    Artifacts,
}

impl RetireStep {
    pub const ALL: [Self; 5] = [
        Self::AbandonedBuilds,
        Self::UnusedSnapshots,
        Self::Records,
        Self::Evidence,
        Self::Artifacts,
    ];
}

/// A live lease of this scope holds snapshot `s` (`?3`: live-since seconds).
macro_rules! held {
    () => {
        "EXISTS (SELECT 1 FROM lease_holds h JOIN process_leases l \
         ON l.namespace_id = h.namespace_id AND l.reset_epoch = h.reset_epoch \
         AND l.lease_id = h.lease_id \
         WHERE h.namespace_id = s.namespace_id AND h.reset_epoch = s.reset_epoch \
         AND h.snapshot_id = s.snapshot_id AND l.renewed_unix_seconds >= ?3)"
    };
}

// Every statement: `?1` namespace, `?2` epoch, `?4` the batch size. A
// unary `+` keeps a `lifecycle` term off the lifecycle index, so the probe
// searches the reference's own index (checked by the plan test below).

const ABANDONED_BUILDS: &str = concat!(
    "UPDATE snapshots SET state = 3 WHERE rowid IN (SELECT s.rowid FROM snapshots s \
     WHERE s.namespace_id = ?1 AND s.reset_epoch = ?2 AND s.state = 1 AND NOT ",
    held!(),
    " LIMIT ?4)"
);

/// `?5`: snapshots kept per context, newest publication first.
const UNUSED_SNAPSHOTS: &str = concat!(
    "UPDATE snapshots SET state = 3 WHERE rowid IN (SELECT s.rowid FROM snapshots s \
     WHERE s.namespace_id = ?1 AND s.reset_epoch = ?2 AND s.state = 2 AND NOT ",
    held!(),
    " AND (SELECT count(*) FROM snapshots n WHERE n.namespace_id = s.namespace_id \
     AND n.reset_epoch = s.reset_epoch AND n.context_key = s.context_key AND n.state = 2 \
     AND n.publication_seq > s.publication_seq) >= ?5 LIMIT ?4)"
);

/// `?3`: the cutoff sequence (grace horizon and recency window).
const RECORDS: &str = "UPDATE member_records SET lifecycle = 2 WHERE rowid IN (SELECT r.rowid \
    FROM member_records r WHERE r.namespace_id = ?1 AND r.reset_epoch = ?2 AND r.lifecycle = 1 \
    AND r.publication_seq <= ?3 AND NOT EXISTS (SELECT 1 FROM snapshot_members m JOIN snapshots s \
        ON s.namespace_id = m.namespace_id AND s.reset_epoch = m.reset_epoch \
        AND s.snapshot_id = m.snapshot_id \
        WHERE m.namespace_id = r.namespace_id AND m.reset_epoch = r.reset_epoch \
        AND m.record_id = r.record_id AND s.state IN (1, 2)) LIMIT ?4)";

/// `?3`: the cutoff sequence.
const EVIDENCE: &str = "UPDATE evidence_sets SET lifecycle = 2 WHERE rowid IN (SELECT e.rowid \
    FROM evidence_sets e WHERE e.namespace_id = ?1 AND e.reset_epoch = ?2 AND e.lifecycle = 1 \
    AND e.publication_seq <= ?3 \
    AND NOT EXISTS (SELECT 1 FROM member_records r WHERE r.namespace_id = e.namespace_id \
        AND r.reset_epoch = e.reset_epoch AND r.member_evidence = e.evidence_id \
        AND +r.lifecycle = 1) \
    AND NOT EXISTS (SELECT 1 FROM member_bindings b JOIN member_records r \
        ON r.namespace_id = b.namespace_id AND r.reset_epoch = b.reset_epoch \
        AND r.record_id = b.record_id \
        WHERE b.namespace_id = e.namespace_id AND b.reset_epoch = e.reset_epoch \
        AND b.evidence_id = e.evidence_id AND r.lifecycle = 1) \
    AND NOT EXISTS (SELECT 1 FROM snapshots s WHERE s.namespace_id = e.namespace_id \
        AND s.reset_epoch = e.reset_epoch AND s.discovery_evidence = e.evidence_id \
        AND s.state IN (1, 2)) \
    LIMIT ?4)";

/// `?3`: the cutoff sequence.
const ARTIFACTS: &str = "UPDATE artifacts SET lifecycle = 2 WHERE rowid IN (SELECT a.rowid \
    FROM artifacts a WHERE a.namespace_id = ?1 AND a.reset_epoch = ?2 AND a.lifecycle = 1 \
    AND a.publication_seq <= ?3 \
    AND NOT EXISTS (SELECT 1 FROM member_records r WHERE r.namespace_id = a.namespace_id \
        AND r.reset_epoch = a.reset_epoch AND r.artifact_id = a.artifact_id \
        AND +r.lifecycle = 1) \
    AND NOT EXISTS (SELECT 1 FROM evidence_sets e WHERE e.namespace_id = a.namespace_id \
        AND e.reset_epoch = a.reset_epoch AND e.artifact_id = a.artifact_id \
        AND +e.lifecycle = 1) \
    LIMIT ?4)";

/// The rows of one table that point at retired parent `p` of `parent`
/// (`state` or `lifecycle` column `status`), reached from the parent's
/// state index and then by the parent-id prefix of the child's key
/// (`CROSS JOIN` keeps that order).
macro_rules! children_of_retired {
    ($child:literal, $parent:literal, $status:literal, $id:literal) => {
        concat!(
            "DELETE FROM ",
            $child,
            " WHERE rowid IN (SELECT c.rowid FROM ",
            $parent,
            " p CROSS JOIN ",
            $child,
            " c ON c.namespace_id = p.namespace_id AND c.reset_epoch = p.reset_epoch AND c.",
            $id,
            " = p.",
            $id,
            " WHERE p.namespace_id = ?1 AND p.reset_epoch = ?2 AND p.",
            $status,
            " LIMIT ?3)"
        )
    };
}

/// No row of `table` names `p`'s `id` in `column`.
macro_rules! unnamed_by {
    ($table:literal, $column:literal, $id:literal) => {
        concat!(
            " AND NOT EXISTS (SELECT 1 FROM ",
            $table,
            " x WHERE x.namespace_id = p.namespace_id AND x.reset_epoch = p.reset_epoch AND x.",
            $column,
            " = p.",
            $id,
            ")"
        )
    };
}

/// The rows of `child`, keyed `(ref, ordinal)` without a scope of their
/// own, that hang off retired parent `p` through its surrogate `ref`.
macro_rules! rows_of_retired {
    ($child:literal, $parent:literal, $ref:literal) => {
        concat!(
            "DELETE FROM ",
            $child,
            " WHERE (",
            $ref,
            ", ordinal) IN (SELECT c.",
            $ref,
            ", c.ordinal FROM ",
            $parent,
            " p CROSS JOIN ",
            $child,
            " c ON c.",
            $ref,
            " = p.",
            $ref,
            " WHERE p.namespace_id = ?1 AND p.reset_epoch = ?2 AND p.lifecycle = 2 LIMIT ?3)"
        )
    };
}

/// No row of `table` lists `p` by its surrogate `ref`.
macro_rules! unlisted_by {
    ($table:literal, $ref:literal) => {
        concat!(
            " AND NOT EXISTS (SELECT 1 FROM ",
            $table,
            " x WHERE x.",
            $ref,
            " = p.",
            $ref,
            ")"
        )
    };
}

/// Deletion inside the live scope, children first (spec §10: retire from
/// lookup, delete bounded row batches, then remove the header). A parent
/// goes only once no row of any lifecycle references it, so the foreign
/// keys hold after every batch. `?1` namespace, `?2` epoch, `?3` the rows
/// left in the batch.
///
/// Observation content is shared by the evidence sets that list it, so it
/// has no lifecycle: once no set (of any lifecycle) lists it, the sweep
/// deletes it. The sweep visits the scope's content, which holds each
/// distinct observation once, the way orphan manifests are found. A
/// publisher finds or inserts content and lists it in one write
/// transaction, and the sweep runs in another, so content is never deleted
/// while a committed or committing set lists it.
pub(crate) const DELETE_STEPS: [&str; 12] = [
    children_of_retired!("lease_holds", "snapshots", "state = 3", "snapshot_id"),
    children_of_retired!("snapshot_members", "snapshots", "state = 3", "snapshot_id"),
    concat!(
        "DELETE FROM snapshots WHERE rowid IN (SELECT p.rowid FROM snapshots p \
         WHERE p.namespace_id = ?1 AND p.reset_epoch = ?2 AND p.state = 3",
        unnamed_by!("snapshot_members", "snapshot_id", "snapshot_id"),
        unnamed_by!("lease_holds", "snapshot_id", "snapshot_id"),
        " LIMIT ?3)"
    ),
    children_of_retired!(
        "binding_targets",
        "member_records",
        "lifecycle = 2",
        "record_id"
    ),
    children_of_retired!(
        "member_bindings",
        "member_records",
        "lifecycle = 2",
        "record_id"
    ),
    concat!(
        "DELETE FROM member_records WHERE rowid IN (SELECT p.rowid FROM member_records p \
         WHERE p.namespace_id = ?1 AND p.reset_epoch = ?2 AND p.lifecycle = 2",
        unnamed_by!("binding_targets", "record_id", "record_id"),
        unnamed_by!("member_bindings", "record_id", "record_id"),
        unnamed_by!("snapshot_members", "record_id", "record_id"),
        " LIMIT ?3)"
    ),
    rows_of_retired!("evidence_observations", "evidence_sets", "evidence_ref"),
    concat!(
        "DELETE FROM evidence_sets WHERE rowid IN (SELECT p.rowid FROM evidence_sets p \
         WHERE p.namespace_id = ?1 AND p.reset_epoch = ?2 AND p.lifecycle = 2",
        unlisted_by!("evidence_observations", "evidence_ref"),
        unnamed_by!("member_records", "member_evidence", "evidence_id"),
        unnamed_by!("member_bindings", "evidence_id", "evidence_id"),
        unnamed_by!("snapshots", "discovery_evidence", "evidence_id"),
        " LIMIT ?3)"
    ),
    concat!(
        "DELETE FROM observation_content WHERE observation IN (SELECT p.observation \
         FROM observation_content p WHERE p.namespace_id = ?1 AND p.reset_epoch = ?2",
        unlisted_by!("evidence_observations", "observation"),
        " LIMIT ?3)"
    ),
    rows_of_retired!("artifact_rows", "artifacts", "artifact_ref"),
    concat!(
        "DELETE FROM artifacts WHERE rowid IN (SELECT p.rowid FROM artifacts p \
         WHERE p.namespace_id = ?1 AND p.reset_epoch = ?2 AND p.lifecycle = 2",
        unlisted_by!("artifact_rows", "artifact_ref"),
        unnamed_by!("member_records", "artifact_id", "artifact_id"),
        unnamed_by!("evidence_sets", "artifact_id", "artifact_id"),
        " LIMIT ?3)"
    ),
    // Orphan manifests (Decision 19). One `NOT EXISTS` per column, so each
    // probe searches its own index.
    concat!(
        "DELETE FROM key_manifests WHERE rowid IN (SELECT p.rowid FROM key_manifests p \
         WHERE p.namespace_id = ?1 AND p.reset_epoch = ?2",
        unnamed_by!("artifacts", "artifact_id", "digest"),
        unnamed_by!("artifacts", "root_key", "digest"),
        unnamed_by!("evidence_sets", "context_key", "digest"),
        unnamed_by!("evidence_sets", "subject_key", "digest"),
        unnamed_by!("snapshots", "context_key", "digest"),
        " LIMIT ?3)"
    ),
];

/// Run [`DELETE_STEPS`] in order in the caller's transaction while
/// `budget` rows remain. Returns the rows deleted and whether nothing
/// deletable is left: every step deleted fewer rows than it was allowed.
/// Retired rows something still references stay until it goes.
pub(crate) fn delete_retired(
    connection: &Connection,
    scope: Scope,
    budget: usize,
) -> Result<(usize, bool), Bypass> {
    let namespace = scope.namespace.as_bytes().as_slice();
    let reset_epoch = epoch(scope);
    let mut left = budget;
    for sql in DELETE_STEPS {
        if left == 0 {
            return Ok((budget, false));
        }
        let deleted = connection
            .prepare_cached(sql)
            .and_then(|mut statement| {
                statement.execute(params![namespace, reset_epoch, left as i64])
            })
            .map_err(|error| classify(&error))?;
        left -= deleted.min(left);
    }
    // A step that used the whole remainder may have left rows behind.
    Ok((budget - left, left > 0))
}

fn current_seq(connection: &Connection) -> Result<i64, Bypass> {
    connection
        .query_row(
            "SELECT publication_seq FROM store_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| classify(&error))
}

/// The highest sequence a record, evidence set or artifact may have and
/// still be retired: behind every live lease's floor and, unless the store
/// is under pressure, outside the recency window.
fn cutoff(horizon: i64, current: i64, recency_window: u64, pressured: bool) -> i64 {
    if pressured {
        return horizon;
    }
    let window = i64::try_from(recency_window).unwrap_or(i64::MAX);
    horizon.min(current.saturating_sub(window).max(0))
}

impl Store {
    /// Retire at most `limits.batch_rows` rows of `step` in one `IMMEDIATE`
    /// transaction and return how many; 0 means the step has nothing left.
    /// Under pressure (over the headroom or low on disk space) it keeps
    /// `keep_snapshots_under_pressure` snapshots per context and ignores the
    /// recency window. At the WAL threshold it writes nothing
    /// (`WalPressure`: only a checkpoint makes room, spec D13).
    pub fn retire_step(
        &self,
        step: RetireStep,
        limits: &MaintenanceLimits,
    ) -> Result<usize, Bypass> {
        limits
            .validate()
            .map_err(|_| Bypass::InvalidMaintenanceLimits)?;
        self.guarded(Access::Maintenance, || {
            let fail = |error: rusqlite::Error| classify(&error);
            let mut writer = self.writer()?;
            let transaction = begin(&mut writer, TransactionBehavior::Immediate)?;
            ensure_live(&transaction, self.scope)?;
            let policy = read_policy(&transaction)?.policy;
            let pressured = match pressure_of(policy, &read_usage(self, &transaction)?) {
                Pressure::WalPressure => return Err(Bypass::WalPressure),
                Pressure::OverHeadroom | Pressure::LowDiskSpace => true,
                Pressure::None => false,
            };
            let now = self.clock.unix_seconds();
            let namespace = self.scope.namespace.as_bytes().as_slice();
            let reset_epoch = epoch(self.scope);
            let batch = limits.batch_rows as i64;
            let changed = match step {
                RetireStep::AbandonedBuilds => transaction
                    .prepare_cached(ABANDONED_BUILDS)
                    .and_then(|mut statement| {
                        statement.execute(params![namespace, reset_epoch, live_since(now), batch])
                    }),
                RetireStep::UnusedSnapshots => {
                    let keep = if pressured {
                        limits.keep_snapshots_under_pressure
                    } else {
                        limits.keep_snapshots
                    };
                    transaction
                        .prepare_cached(UNUSED_SNAPSHOTS)
                        .and_then(|mut statement| {
                            statement.execute(params![
                                namespace,
                                reset_epoch,
                                live_since(now),
                                batch,
                                keep as i64
                            ])
                        })
                }
                RetireStep::Records | RetireStep::Evidence | RetireStep::Artifacts => {
                    let cutoff = cutoff(
                        horizon(&transaction, self.scope, now)?,
                        current_seq(&transaction)?,
                        limits.recency_window,
                        pressured,
                    );
                    let sql = match step {
                        RetireStep::Records => RECORDS,
                        RetireStep::Evidence => EVIDENCE,
                        _ => ARTIFACTS,
                    };
                    transaction.prepare_cached(sql).and_then(|mut statement| {
                        statement.execute(params![namespace, reset_epoch, cutoff, batch])
                    })
                }
            }
            .map_err(fail)?;
            fault_point!(self, GcRetireBeforeCommit);
            self.before_commit();
            transaction.commit().map_err(fail)?;
            Ok(changed)
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{Digest, Namespace};

    #[test]
    fn every_retirement_statement_prepares_within_the_connection_limits() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            directory.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let store = Store::open(
            directory.path(),
            &Namespace::new(Digest::from_bytes([4; 32])),
            super::super::StoreLimits::default(),
        )
        .unwrap();
        let writer = store.writer().unwrap();
        for sql in [
            ABANDONED_BUILDS,
            UNUSED_SNAPSHOTS,
            RECORDS,
            EVIDENCE,
            ARTIFACTS,
        ] {
            writer
                .prepare(sql)
                .unwrap_or_else(|error| panic!("{error}: {sql}"));
        }
    }

    /// Every `SEARCH`/`SCAN` line of `sql`'s query plan.
    fn plan(connection: &Connection, sql: &str) -> Vec<String> {
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap_or_else(|error| panic!("{error}: {sql}"));
        let unbound = std::iter::repeat_n(rusqlite::types::Null, statement.parameter_count());
        statement
            .query_map(rusqlite::params_from_iter(unbound), |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .filter(|line| line.starts_with("SEARCH") || line.starts_with("SCAN"))
            .collect()
    }

    /// The last term each `SEARCH` of a query alias binds, sorted: a
    /// child or reference probe must bind its parent's ID, not just the
    /// scope (foreign key checks, on table names, are left out).
    fn bound_terms(connection: &Connection, sql: &str) -> Vec<String> {
        let lines = plan(connection, sql);
        assert!(
            lines.iter().all(|line| !line.starts_with("SCAN")),
            "{sql}\n{lines:#?}"
        );
        let mut terms: Vec<String> = lines
            .iter()
            .filter(|line| line.split(' ').nth(1).is_some_and(|alias| alias.len() == 1))
            .map(|line| {
                let inside = &line[line.rfind('(').unwrap() + 1..line.rfind(')').unwrap()];
                inside.rsplit(" AND ").next().unwrap().to_string()
            })
            .collect();
        terms.sort();
        terms
    }

    #[test]
    fn every_maintenance_statement_searches_by_parent_or_reference_id() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            directory.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let store = Store::open(
            directory.path(),
            &Namespace::new(Digest::from_bytes([4; 32])),
            super::super::StoreLimits::default(),
        )
        .unwrap();
        let writer = store.writer().unwrap();
        let expected: [(&str, &[&str]); 17] = [
            (DELETE_STEPS[0], &["snapshot_id=?", "state=?"]),
            (DELETE_STEPS[1], &["snapshot_id=?", "state=?"]),
            (
                DELETE_STEPS[2],
                &["snapshot_id=?", "snapshot_id=?", "state=?"],
            ),
            (DELETE_STEPS[3], &["lifecycle=?", "record_id=?"]),
            (DELETE_STEPS[4], &["lifecycle=?", "record_id=?"]),
            (
                DELETE_STEPS[5],
                &["lifecycle=?", "record_id=?", "record_id=?", "record_id=?"],
            ),
            (DELETE_STEPS[6], &["evidence_ref=?", "lifecycle=?"]),
            (
                DELETE_STEPS[7],
                &[
                    "discovery_evidence=?",
                    "evidence_id=?",
                    "evidence_ref=?",
                    "lifecycle=?",
                    "member_evidence=?",
                ],
            ),
            // Unlisted content is found by visiting the scope's content.
            (DELETE_STEPS[8], &["observation=?", "reset_epoch=?"]),
            (DELETE_STEPS[9], &["artifact_ref=?", "lifecycle=?"]),
            (
                DELETE_STEPS[10],
                &[
                    "artifact_id=?",
                    "artifact_id=?",
                    "artifact_ref=?",
                    "lifecycle=?",
                ],
            ),
            // Orphan manifests are found by visiting the scope's manifests.
            (
                DELETE_STEPS[11],
                &[
                    "artifact_id=?",
                    "context_key=?",
                    "context_key=?",
                    "reset_epoch=?",
                    "root_key=?",
                    "subject_key=?",
                ],
            ),
            (
                ABANDONED_BUILDS,
                &["lease_id=?", "snapshot_id=?", "state=?"],
            ),
            (
                UNUSED_SNAPSHOTS,
                &[
                    "lease_id=?",
                    "publication_seq>?",
                    "snapshot_id=?",
                    "state=?",
                ],
            ),
            (
                RECORDS,
                &["publication_seq<?", "record_id=?", "snapshot_id=?"],
            ),
            (
                EVIDENCE,
                &[
                    "discovery_evidence=?",
                    "evidence_id=?",
                    "member_evidence=?",
                    "publication_seq<?",
                    "record_id=?",
                ],
            ),
            (
                ARTIFACTS,
                &["artifact_id=?", "artifact_id=?", "publication_seq<?"],
            ),
        ];
        for (sql, terms) in expected {
            assert_eq!(bound_terms(&writer, sql), terms, "{sql}");
        }
    }

    #[test]
    fn the_cutoff_is_the_horizon_or_the_recency_window_whichever_is_older() {
        assert_eq!(cutoff(10, 100, 50, false), 10);
        assert_eq!(cutoff(80, 100, 50, false), 50);
        assert_eq!(cutoff(80, 100, 50, true), 80);
        assert_eq!(cutoff(80, 100, 65_536, false), 0);
        assert_eq!(cutoff(80, 100, u64::MAX, false), 0);
    }
}
