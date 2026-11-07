//! Admission of the candidate trees an indexed query parses (R9-8, `.7`
//! AC3): each is charged to `maxCacheBytes` as `QueryTrees` before it is
//! parsed. A candidate past the budget is left out and counted as one gap;
//! a required source (the requesting document, a declaring unit, an open
//! buffer) fails the request instead.

use crate::project_cache::{
    ProjectCache, RETAINED_BYTES_PER_SOURCE_BYTE, Reservation, ReservationKind,
};

pub(crate) const HYDRATION_REFUSED: &str =
    "the analysis memory budget is exhausted; retry when other work finishes";

pub(crate) fn hydration_gap(count: usize) -> String {
    format!("{count} candidate units were not searched: the analysis memory budget is exhausted")
}

#[cfg(test)]
thread_local! {
    static TEST_LIMIT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Admit on this thread only while the admission's `QueryTrees` bytes stay
/// within `limit`, besides the cache's own budget.
#[cfg(test)]
pub(crate) fn test_set_limit(limit: Option<usize>) {
    TEST_LIMIT.with(|cell| cell.set(limit));
}

pub(crate) struct HydrationAdmission {
    cache: ProjectCache,
    held: Vec<Reservation>,
    refused: usize,
    /// Bytes of the tickets not yet settled and of `held`.
    admitted: usize,
}

pub(crate) enum Admitted {
    /// A cached parse; its lease charges it.
    Charged,
    Reserved(HydrationTicket),
    /// A candidate over the budget (counted in `refused`).
    Refused,
}

pub(crate) struct HydrationTicket(Reservation);

#[derive(Debug)]
pub(crate) struct HydrationRefused;

impl HydrationAdmission {
    pub(crate) fn new(cache: ProjectCache) -> Self {
        Self {
            cache,
            held: Vec::new(),
            refused: 0,
            admitted: 0,
        }
    }

    /// Reserve `len × RETAINED_BYTES_PER_SOURCE_BYTE` of `QueryTrees` before
    /// a source of `len` bytes is parsed; `charged` (a parse already in
    /// memory is reused) reserves nothing. `Err(HydrationRefused)` for a
    /// refused required source.
    pub(crate) fn admit(
        &mut self,
        len: u64,
        charged: bool,
        required: bool,
    ) -> Result<Admitted, HydrationRefused> {
        if charged {
            return Ok(Admitted::Charged);
        }
        let bytes = usize::try_from(len)
            .unwrap_or(usize::MAX)
            .saturating_mul(RETAINED_BYTES_PER_SOURCE_BYTE);
        #[cfg(test)]
        let within_test_limit = TEST_LIMIT.with(std::cell::Cell::get).is_none_or(|limit| {
            self.admitted
                .checked_add(bytes)
                .is_some_and(|total| total <= limit)
        });
        #[cfg(not(test))]
        let within_test_limit = true;
        let reservation = within_test_limit
            .then(|| self.cache.reserve(ReservationKind::QueryTrees, bytes))
            .flatten();
        match reservation {
            Some(reservation) => {
                self.admitted = self.admitted.saturating_add(bytes);
                Ok(Admitted::Reserved(HydrationTicket(reservation)))
            }
            None if required => Err(HydrationRefused),
            None => {
                self.refused += 1;
                Ok(Admitted::Refused)
            }
        }
    }

    /// After the parse: a parse the cache leased is charged by its lease, so
    /// the reservation is dropped; one whose lease was refused keeps it.
    pub(crate) fn settle(&mut self, ticket: HydrationTicket, leased: bool) {
        if leased {
            self.admitted = self.admitted.saturating_sub(ticket.0.bytes());
        } else {
            self.held.push(ticket.0);
        }
    }

    pub(crate) fn refused(&self) -> usize {
        self.refused
    }

    pub(crate) fn into_held(self) -> Vec<Reservation> {
        self.held
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_cache::ProjectCache;

    #[test]
    fn candidates_beyond_the_budget_are_refused_and_counted() {
        let cache = ProjectCache::new(48 * 1_000);
        let reserved = || cache.stats().reserved_bytes();
        let mut admission = HydrationAdmission::new(cache.clone());
        let ticket = |admitted| match admitted {
            Ok(Admitted::Reserved(ticket)) => ticket,
            _ => panic!("admitted"),
        };
        let first = ticket(admission.admit(500, false, true)); // required, fits
        let second = ticket(admission.admit(400, false, false));
        assert!(
            matches!(admission.admit(400, false, false), Ok(Admitted::Refused)),
            "over the budget"
        );
        assert!(matches!(
            admission.admit(10_000, true, false),
            Ok(Admitted::Charged)
        ));
        assert_eq!(reserved(), 48 * 900, "charged parses reserve nothing");
        assert_eq!(admission.refused(), 1);
        assert!(
            admission.admit(400, false, true).is_err(),
            "a required source is never dropped silently"
        );
        admission.settle(first, true); // leased: the lease charges it now
        assert_eq!(reserved(), 48 * 400);
        admission.settle(second, false); // lease refused: held
        let held = admission.into_held();
        assert_eq!(reserved(), 48 * 400);
        drop(held);
        assert_eq!(reserved(), 0);
    }
}
