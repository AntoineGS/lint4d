use super::Store;
use super::limits::MAX_INDEX_PAGE;
use super::namespaces::{ensure_live, epoch};
use super::outcome::Bypass;
use super::sqlite::{begin, classify};
use crate::Digest;
use rusqlite::{TransactionBehavior, params};

/// Keyset-paginated, binary-compared lookup over live artifacts. The partial
/// index `artifact_rows_by_index_key` serves both the filter and the order.
/// `pub` only so `test-support` can re-export it; the module is private.
pub const INDEX_QUERY: &str = "SELECT r.artifact_id, r.ordinal FROM artifact_rows r \
     JOIN artifacts a ON a.namespace_id = r.namespace_id AND a.reset_epoch = r.reset_epoch \
         AND a.artifact_id = r.artifact_id \
     WHERE r.namespace_id = ?1 AND r.reset_epoch = ?2 AND r.row_kind = ?3 AND r.index_key = ?4 \
         AND (r.artifact_id, r.ordinal) > (?5, ?6) AND a.lifecycle = 1 \
     ORDER BY r.artifact_id, r.ordinal LIMIT ?7";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexHit {
    pub artifact_id: Digest,
    pub ordinal: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexPage {
    pub hits: Vec<IndexHit>,
    /// Pass back as `after` for the next page; `None` on the last page.
    pub next: Option<IndexHit>,
}

impl Store {
    /// Rows of live artifacts whose kind and index key match exactly, in
    /// `(artifact_id, ordinal)` order after `after`, at most `limit`
    /// (clamped to 1..=256). A hit is a candidate only: not membership,
    /// authorization or freshness evidence, and its artifact's rows are
    /// unverified until read (spec §6.1, §7).
    pub fn find_by_index(
        &self,
        row_kind: u8,
        index_key: &[u8],
        after: Option<IndexHit>,
        limit: usize,
    ) -> Result<IndexPage, Bypass> {
        let limit = limit.clamp(1, MAX_INDEX_PAGE);
        let fail = |error: rusqlite::Error| classify(&error);
        let (after_id, after_ordinal) = match after {
            Some(hit) => (hit.artifact_id.as_bytes().to_vec(), hit.ordinal as i64),
            None => (Vec::new(), -1),
        };
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
                    after_id,
                    after_ordinal,
                    (limit + 1) as i64
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
            )
            .map_err(fail)?;
        let mut hits = Vec::with_capacity(limit + 1);
        for row in rows {
            let (id, ordinal) = row.map_err(fail)?;
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
            hits.last().copied()
        } else {
            None
        };
        Ok(IndexPage { hits, next })
    }
}
