//! The store-wide publication sequence (spec §6.1 amendment 2026-10-27):
//! strictly increasing across processes because it is taken inside a write
//! transaction. It orders candidates; it never proves freshness.

use super::outcome::{Bypass, Lookup, Malformed, Miss};
use super::sqlite::{classify, classify_read};
use rusqlite::{Connection, Params, Row};

/// The next sequence, taken inside the caller's write transaction. A
/// transaction that rolls back gives its number back.
pub(crate) fn next_publication_seq(connection: &Connection) -> Result<i64, Bypass> {
    connection
        .query_row(
            "UPDATE store_state SET publication_seq = publication_seq + 1 \
             WHERE singleton = 1 RETURNING publication_seq",
            [],
            |row| row.get(0),
        )
        .map_err(|error| classify(&error))
}

/// One candidate query: `select` (its columns, table and filter) bound to
/// `params`, at most `limit` rows, and the miss a refused row makes.
pub(crate) struct Candidates<P> {
    pub(crate) select: &'static str,
    pub(crate) params: P,
    pub(crate) limit: usize,
    pub(crate) malformed: Malformed,
}

/// At most `limit` rows that `select` finds, most recently published first,
/// each made a header by `header`. Every candidate query goes through here,
/// so none can fall back to an order that hides the newest publications
/// behind the cap (l4d-e8bl). A row `header` refuses makes the whole lookup
/// `malformed`.
pub(crate) fn newest_first<P: Params, R, H>(
    connection: &Connection,
    candidates: Candidates<P>,
    row: impl FnMut(&Row<'_>) -> rusqlite::Result<R>,
    mut header: impl FnMut(R) -> Option<H>,
) -> Result<Lookup<Vec<H>>, Bypass> {
    let Candidates {
        select,
        params,
        limit,
        malformed,
    } = candidates;
    let rows: rusqlite::Result<Vec<R>> = connection
        .prepare_cached(&format!(
            "{select} ORDER BY publication_seq DESC LIMIT {limit}"
        ))
        .and_then(|mut statement| statement.query_map(params, row)?.collect());
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => return classify_read(&error).map(Lookup::Miss),
    };
    let mut headers = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(found) = header(row) else {
            return Ok(Lookup::Miss(Miss::Malformed(malformed)));
        };
        headers.push(found);
    }
    if headers.is_empty() {
        return Ok(Lookup::Miss(Miss::Absent));
    }
    Ok(Lookup::Hit(headers))
}
