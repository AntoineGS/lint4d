use super::Store;
use super::health::Access;
use super::limits::MAX_INDEX_PAGE;
use super::namespaces::{LIVE, ensure_live, epoch};
use super::outcome::Bypass;
use super::sqlite::{begin, classify};
use crate::Digest;
use rusqlite::{TransactionBehavior, params};

/// Keyset-paginated, binary-compared lookup over live artifacts. The partial
/// index `artifact_rows_by_index_key` serves both the filter and the order
/// (`CROSS JOIN` keeps it outermost); the artifact's header restricts the
/// rows to the scope. `pub` only so `test-support` can re-export it; the
/// module is private.
pub const INDEX_QUERY: &str = "SELECT a.artifact_id, r.artifact_ref, r.ordinal \
     FROM artifact_rows r CROSS JOIN artifacts a ON a.artifact_ref = r.artifact_ref \
     WHERE r.row_kind = ?3 AND r.index_key = ?4 AND (r.artifact_ref, r.ordinal) > (?5, ?6) \
         AND a.namespace_id = ?1 AND a.reset_epoch = ?2 AND a.lifecycle = ?8 \
     ORDER BY r.artifact_ref, r.ordinal LIMIT ?7";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexHit {
    pub artifact_id: Digest,
    pub ordinal: u64,
}

/// Where a page ended: the position of its last hit in the store's own
/// order, meaningful only to the store that returned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexCursor {
    artifact_ref: i64,
    ordinal: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPage {
    pub hits: Vec<IndexHit>,
    /// Pass back as `after` for the next page; `None` on the last page.
    pub next: Option<IndexCursor>,
}

impl Store {
    /// Rows of live artifacts whose kind and index key match exactly, in
    /// publication order of their artifacts and then ordinal, after
    /// `after`, at most `limit`
    /// (clamped to 1..=256). A hit is a candidate only: not membership,
    /// authorization or freshness evidence, and its artifact's rows are
    /// unverified until read (spec §6.1, §7). Its result is charged to the
    /// scratch while it is built; a page the caller retains is the caller's
    /// to admit.
    pub fn find_by_index(
        &self,
        row_kind: u8,
        index_key: &[u8],
        after: Option<IndexCursor>,
        limit: usize,
    ) -> Result<IndexPage, Bypass> {
        self.guarded(Access::Read, || {
            let limit = limit.clamp(1, MAX_INDEX_PAGE);
            // The page, charged before it is allocated (spec §10).
            let Some(_scratch) = self
                .scratch
                .reserve(((limit + 1) * std::mem::size_of::<IndexHit>()) as u64)
            else {
                return Err(Bypass::ScratchExhausted);
            };
            let fail = |error: rusqlite::Error| classify(&error);
            let after = after.unwrap_or(IndexCursor {
                artifact_ref: 0,
                ordinal: -1,
            });
            let mut reader = self.reader()?;
            let transaction = begin(&mut reader, TransactionBehavior::Deferred)?;
            ensure_live(&transaction, self.scope)?;
            let mut statement = transaction.prepare_cached(INDEX_QUERY).map_err(fail)?;
            let rows = statement
                .query_map(
                    params![
                        self.scope.namespace.as_bytes().as_slice(),
                        epoch(self.scope),
                        i64::from(row_kind),
                        index_key,
                        after.artifact_ref,
                        after.ordinal,
                        (limit + 1) as i64,
                        LIVE
                    ],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .map_err(fail)?;
            let mut hits = Vec::with_capacity(limit + 1);
            let mut cursors = Vec::with_capacity(limit + 1);
            for row in rows {
                let (id, artifact_ref, ordinal) = row.map_err(fail)?;
                cursors.push(IndexCursor {
                    artifact_ref,
                    ordinal,
                });
                let artifact_id = <[u8; 32]>::try_from(id.as_slice())
                    .map(Digest::from_bytes)
                    .map_err(|_| Bypass::Corrupt)?;
                let ordinal = u64::try_from(ordinal).map_err(|_| Bypass::Corrupt)?;
                hits.push(IndexHit {
                    artifact_id,
                    ordinal,
                });
            }
            let next = if hits.len() > limit {
                hits.truncate(limit);
                cursors.get(limit - 1).copied()
            } else {
                None
            };
            Ok(IndexPage { hits, next })
        })
    }
}
