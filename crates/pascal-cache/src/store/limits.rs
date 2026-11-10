use std::time::Duration;

/// Aggregate SQLite page cache of one store (spec §10).
pub const MAX_PAGE_CACHE_BYTES: u64 = 16 * 1024 * 1024;
/// Aggregate transient decode buffers of one store (spec §10).
pub const MAX_SCRATCH_BYTES: u64 = 16 * 1024 * 1024;
/// Cumulative lock and connection wait of one cache operation (spec §10).
pub const MAX_WAIT: Duration = Duration::from_millis(25);
/// Wait allowance when opening the store, which is off interactive paths.
pub const MAX_OPEN_WAIT: Duration = Duration::from_secs(2);
/// The WAL file length SQLite truncates to when it resets the WAL
/// (spec §10 amendment 2026-11-03, D13).
pub const JOURNAL_SIZE_LIMIT_BYTES: i64 = 4 << 20;
pub const MAX_READERS: usize = 2;
pub const MIN_CONNECTION_PAGE_CACHE_BYTES: u64 = 256 * 1024;
/// Heap one SQLite connection may use beyond its page cache: schema,
/// lookaside, the prepared-statement cache and parser state (spec §10: "A
/// connection is not opened when its overhead cannot be admitted").
/// `tests/sqlite_heap.rs` checks it against SQLite's own heap count, measured
/// with the statements a fresh store runs (each connection's statement cache
/// can hold more); statement heap beyond it (large sorts, unusual plans) is
/// not accounted.
pub const CONNECTION_OVERHEAD_BYTES: u64 = 1024 * 1024;
pub const MAX_ROW_BATCH_ROWS: usize = 256;
pub const MAX_ROW_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_ARTIFACT_CONTENT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ARTIFACT_ROWS: usize = 1024 * 1024;
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_SUPPLEMENTAL_BYTES: usize = 1024 * 1024;
pub const MAX_INDEX_KEY_BYTES: usize = 4096;
pub const MAX_INDEX_PAGE: usize = 256;
/// Observations in one evidence set (spec §10: multi-row manifests).
pub const MAX_EVIDENCE_OBSERVATIONS: usize = 65_536;
/// Evidence sets one lookup considers for a subject.
pub const MAX_EVIDENCE_CANDIDATES: usize = 16;
/// Bindings in one member record (spec §10: multi-row manifests).
pub const MAX_MEMBER_BINDINGS: usize = 65_536;
/// Members of one snapshot: the `maxWorkspaceUnits` ceiling (spec §8).
pub const MAX_SNAPSHOT_MEMBERS: usize = 100_000;
/// Sealed snapshots one lookup returns for a context, as hints (spec §6.3).
pub const MAX_SNAPSHOT_CANDIDATES: usize = 8;
/// `SQLITE_LIMIT_LENGTH`: SQLite refuses to materialize a longer value, so
/// a planted oversized blob fails before it is copied.
pub const SQLITE_LENGTH_LIMIT: i32 = 2 * 1024 * 1024;
/// Live namespaces a store keeps by default: the opening build's and the
/// most recently used other (l4d-1ypi, l4d-i2tj: a SFClient2-size project
/// takes about 0.9 GB per namespace, so each kept older build is dead weight
/// after an upgrade).
pub const DEFAULT_MAX_NAMESPACES: usize = 2;
/// The most `StoreLimits::max_namespaces` may be.
pub const MAX_NAMESPACES: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreLimits {
    pub page_cache_bytes: u64,
    pub scratch_bytes: u64,
    pub wait: Duration,
    pub open_wait: Duration,
    pub readers: usize,
    /// Live namespaces kept at retirement, this store's own included, the
    /// most recently used first, while the store stays within its disk
    /// policy. A namespace a live process holds is never retired, so more
    /// may stay live.
    pub max_namespaces: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitsError {
    Readers,
    PageCacheTooLarge,
    PageCacheTooSmall,
    ScratchTooLarge,
    ScratchTooSmall,
    WaitTooLong,
    OpenWaitTooLong,
    MaxNamespaces,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            page_cache_bytes: MAX_PAGE_CACHE_BYTES,
            scratch_bytes: MAX_SCRATCH_BYTES,
            wait: MAX_WAIT,
            open_wait: Duration::from_secs(1),
            readers: MAX_READERS,
            max_namespaces: DEFAULT_MAX_NAMESPACES,
        }
    }
}

impl StoreLimits {
    /// Limits are configurable downward only (spec §11.1).
    pub fn validate(&self) -> Result<(), LimitsError> {
        if self.readers == 0 || self.readers > MAX_READERS {
            return Err(LimitsError::Readers);
        }
        if self.page_cache_bytes > MAX_PAGE_CACHE_BYTES {
            return Err(LimitsError::PageCacheTooLarge);
        }
        if self.page_cache_bytes < MIN_CONNECTION_PAGE_CACHE_BYTES * self.connections() as u64 {
            return Err(LimitsError::PageCacheTooSmall);
        }
        if self.scratch_bytes > MAX_SCRATCH_BYTES {
            return Err(LimitsError::ScratchTooLarge);
        }
        if self.scratch_bytes < MAX_ROW_BATCH_BYTES as u64 {
            return Err(LimitsError::ScratchTooSmall);
        }
        if self.wait > MAX_WAIT {
            return Err(LimitsError::WaitTooLong);
        }
        if self.open_wait > MAX_OPEN_WAIT {
            return Err(LimitsError::OpenWaitTooLong);
        }
        if !(1..=MAX_NAMESPACES).contains(&self.max_namespaces) {
            return Err(LimitsError::MaxNamespaces);
        }
        Ok(())
    }

    /// The readers plus the one write connection.
    pub fn connections(&self) -> usize {
        self.readers + 1
    }

    /// Each connection's equal share of the aggregate page cache.
    pub fn connection_page_cache_bytes(&self) -> u64 {
        self.page_cache_bytes / self.connections() as u64
    }

    /// What the store's connections are admitted for: the aggregate page
    /// cache plus each connection's overhead allowance.
    pub fn page_admission_bytes(&self) -> u64 {
        self.page_cache_bytes
            .saturating_add(CONNECTION_OVERHEAD_BYTES.saturating_mul(self.connections() as u64))
    }

    /// Everything an open store is admitted for: its connections and its
    /// scratch.
    pub fn admission_bytes(&self) -> u64 {
        self.page_admission_bytes()
            .saturating_add(self.scratch_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_covers_pages_connections_and_scratch() {
        let limits = StoreLimits::default();
        assert_eq!(CONNECTION_OVERHEAD_BYTES, 1024 * 1024);
        assert_eq!(
            limits.page_admission_bytes(),
            16 * 1024 * 1024 + 3 * 1024 * 1024
        );
        assert_eq!(
            limits.admission_bytes(),
            16 * 1024 * 1024 + 3 * 1024 * 1024 + 16 * 1024 * 1024
        );
        let small = StoreLimits {
            readers: 1,
            page_cache_bytes: 2 * MIN_CONNECTION_PAGE_CACHE_BYTES,
            scratch_bytes: MAX_ROW_BATCH_BYTES as u64,
            ..limits
        };
        assert_eq!(
            small.page_admission_bytes(),
            2 * MIN_CONNECTION_PAGE_CACHE_BYTES + 2 * CONNECTION_OVERHEAD_BYTES
        );
        assert_eq!(
            small.admission_bytes(),
            small.page_admission_bytes() + MAX_ROW_BATCH_BYTES as u64
        );
        let unvalidated = StoreLimits {
            readers: usize::MAX - 1,
            page_cache_bytes: u64::MAX,
            ..limits
        };
        assert_eq!(unvalidated.page_admission_bytes(), u64::MAX);
        assert_eq!(unvalidated.admission_bytes(), u64::MAX);
    }

    #[test]
    fn defaults_are_the_spec_ceilings() {
        let limits = StoreLimits::default();
        assert_eq!(limits.validate(), Ok(()));
        assert_eq!(limits.page_cache_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.scratch_bytes, 16 * 1024 * 1024);
        assert_eq!(limits.wait, Duration::from_millis(25));
        assert_eq!(limits.readers, 2);
        assert_eq!(limits.connections(), 3);
        assert_eq!(limits.connection_page_cache_bytes(), 16 * 1024 * 1024 / 3);
    }

    #[test]
    fn limits_can_only_be_lowered_within_reason() {
        let base = StoreLimits::default();
        let check = |limits: StoreLimits| limits.validate();
        assert_eq!(
            check(StoreLimits { readers: 0, ..base }),
            Err(LimitsError::Readers)
        );
        assert_eq!(
            check(StoreLimits { readers: 3, ..base }),
            Err(LimitsError::Readers)
        );
        assert_eq!(
            check(StoreLimits {
                page_cache_bytes: MAX_PAGE_CACHE_BYTES + 1,
                ..base
            }),
            Err(LimitsError::PageCacheTooLarge)
        );
        assert_eq!(
            check(StoreLimits {
                page_cache_bytes: 3 * MIN_CONNECTION_PAGE_CACHE_BYTES - 1,
                ..base
            }),
            Err(LimitsError::PageCacheTooSmall)
        );
        assert_eq!(
            check(StoreLimits {
                page_cache_bytes: 2 * MIN_CONNECTION_PAGE_CACHE_BYTES,
                readers: 1,
                ..base
            }),
            Ok(())
        );
        assert_eq!(
            check(StoreLimits {
                scratch_bytes: MAX_SCRATCH_BYTES + 1,
                ..base
            }),
            Err(LimitsError::ScratchTooLarge)
        );
        assert_eq!(
            check(StoreLimits {
                scratch_bytes: MAX_ROW_BATCH_BYTES as u64 - 1,
                ..base
            }),
            Err(LimitsError::ScratchTooSmall)
        );
        assert_eq!(
            check(StoreLimits {
                wait: MAX_WAIT + Duration::from_millis(1),
                ..base
            }),
            Err(LimitsError::WaitTooLong)
        );
        assert_eq!(
            check(StoreLimits {
                open_wait: MAX_OPEN_WAIT + Duration::from_millis(1),
                ..base
            }),
            Err(LimitsError::OpenWaitTooLong)
        );
        assert_eq!(
            check(StoreLimits {
                wait: Duration::ZERO,
                ..base
            }),
            Ok(())
        );
    }
}
