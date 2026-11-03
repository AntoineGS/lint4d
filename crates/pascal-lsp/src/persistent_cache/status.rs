//! The bounded `pascal/cacheStatus` document (spec §11.2 amendment
//! 2026-11-03, D27). Building it performs no IO; [`UsageView::read`] is the
//! one reading, made by the worker under a job.

use super::counters::{CounterSnapshot, MISS_NAMES};
use super::names;
use pascal_cache::admin;
use pascal_cache::{Bypass, LockWaits, PolicyState, Pressure, Store, StoreLimits, Usage};
use serde_json::{Value, json};
use std::path::PathBuf;

pub(crate) const MAX_CAUSE_MESSAGE_BYTES: usize = 1024;
const MAX_DIRECTORY_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StatusCause {
    Configuration(String),
    Memory,
    Store(Bypass),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reserved {
    pub(crate) pages: usize,
    pub(crate) scratch: usize,
    pub(crate) facts: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UsageView {
    pub(crate) usage: Usage,
    pub(crate) policy: PolicyState,
    pub(crate) pressure: Pressure,
}

impl UsageView {
    /// Read the store's usage, policy and pressure. IO: call it under a job.
    pub(crate) fn read(store: &Store) -> Result<Self, Bypass> {
        Ok(Self {
            usage: store.usage()?,
            policy: store.policy()?,
            pressure: store.pressure()?,
        })
    }

    #[cfg(test)]
    pub(crate) fn maximal() -> Self {
        Self {
            usage: Usage {
                database_bytes: u64::MAX,
                wal_bytes: u64::MAX,
                shm_bytes: u64::MAX,
                page_size: u64::MAX,
                page_count: u64::MAX,
                free_pages: u64::MAX,
                filesystem_free_bytes: Some(u64::MAX),
            },
            policy: PolicyState {
                revision: u64::MAX,
                policy: pascal_cache::StorePolicy {
                    max_disk_bytes: u64::MAX,
                    wal_threshold_bytes: u64::MAX,
                },
            },
            pressure: Pressure::LowDiskSpace,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CacheStatus {
    pub(crate) state: &'static str,
    pub(crate) generation: u64,
    pub(crate) enabled: bool,
    pub(crate) cause: Option<StatusCause>,
    pub(crate) directory: Option<PathBuf>,
    pub(crate) limits: StoreLimits,
    pub(crate) reset_epoch: Option<u64>,
    pub(crate) reserved: Reserved,
    pub(crate) usage: Option<UsageView>,
    pub(crate) lock_waits: LockWaits,
    pub(crate) counters: CounterSnapshot,
}

fn truncate(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn cause_json(cause: &StatusCause) -> Value {
    match cause {
        StatusCause::Configuration(message) => json!({
            "kind": "configuration",
            "message": truncate(message, MAX_CAUSE_MESSAGE_BYTES),
            "advice": "none",
        }),
        StatusCause::Memory => json!({ "kind": "memory", "advice": "retry" }),
        StatusCause::Store(bypass) => {
            let location = match bypass {
                Bypass::Location(issue) => Some(names::location_issue_name(issue)),
                _ => None,
            };
            json!({
                "kind": names::bypass_name(bypass),
                "location": location,
                "advice": names::advice_name(admin::advice(bypass)),
            })
        }
    }
}

fn usage_json(view: &UsageView) -> Value {
    json!({
        "databaseBytes": view.usage.database_bytes,
        "walBytes": view.usage.wal_bytes,
        "shmBytes": view.usage.shm_bytes,
        "freeBytes": view.usage.free_bytes(),
        "filesystemFreeBytes": view.usage.filesystem_free_bytes,
        "maxDiskBytes": view.policy.policy.max_disk_bytes,
        "walThresholdBytes": view.policy.policy.wal_threshold_bytes,
        "policyRevision": view.policy.revision,
        "pressure": names::pressure_name(view.pressure),
    })
}

impl CacheStatus {
    pub(crate) fn to_json(&self) -> Value {
        let limits = &self.limits;
        let reserved = &self.reserved;
        let waits = &self.lock_waits;
        let counters = &self.counters;
        let misses: serde_json::Map<String, Value> = MISS_NAMES
            .iter()
            .zip(counters.misses)
            .map(|(name, count)| ((*name).to_owned(), json!(count)))
            .collect();
        let bypasses: serde_json::Map<String, Value> = names::BYPASS_NAMES
            .iter()
            .zip(counters.bypasses)
            .filter(|(_, count)| *count != 0)
            .map(|(name, count)| ((*name).to_owned(), json!(count)))
            .collect();
        json!({
            "schema": 1,
            "state": self.state,
            "generation": self.generation,
            "enabled": self.enabled,
            "cause": self.cause.as_ref().map(cause_json),
            "directory": self
                .directory
                .as_ref()
                .map(|path| truncate(&path.to_string_lossy(), MAX_DIRECTORY_BYTES).to_owned()),
            "limits": {
                "pageBytes": limits.page_cache_bytes,
                "scratchBytes": limits.scratch_bytes,
                "waitMs": limits.wait.as_millis() as u64,
            },
            "resetEpoch": self.reset_epoch,
            "reservedBytes": {
                "pages": reserved.pages,
                "scratch": reserved.scratch,
                "facts": reserved.facts,
            },
            "store": self.usage.as_ref().map(usage_json),
            "lockWaits": {
                "poolWaits": waits.pool_waits,
                "poolWaitMicros": waits.pool_wait_micros,
                "lockWaits": waits.lock_waits,
                "lockWaitMicros": waits.lock_wait_micros,
                "busy": waits.busy,
            },
            "counters": {
                "hits": counters.hits,
                "decodedBytes": counters.decoded_bytes,
                "misses": misses,
                "bypasses": bypasses,
                "maintenance": {
                    "steps": counters.steps,
                    "cycles": counters.cycles,
                    "rows": counters.rows,
                    "skipped": counters.skipped,
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::counters::MISS_CAUSES;
    use super::*;

    fn largest() -> CacheStatus {
        let counters = CounterSnapshot {
            hits: u64::MAX,
            decoded_bytes: u64::MAX,
            misses: [u64::MAX; MISS_CAUSES],
            bypasses: [u64::MAX; names::BYPASS_KINDS],
            steps: u64::MAX,
            cycles: u64::MAX,
            rows: u64::MAX,
            skipped: u64::MAX,
        };
        CacheStatus {
            state: "bypassed",
            generation: u64::MAX,
            enabled: true,
            cause: Some(StatusCause::Configuration(
                "x".repeat(MAX_CAUSE_MESSAGE_BYTES * 2),
            )),
            directory: Some(PathBuf::from(format!("/{}", "d".repeat(4095)))),
            limits: StoreLimits::default(),
            reset_epoch: Some(u64::MAX),
            reserved: Reserved {
                pages: usize::MAX,
                scratch: usize::MAX,
                facts: usize::MAX,
            },
            usage: Some(UsageView::maximal()),
            lock_waits: LockWaits {
                pool_waits: u64::MAX,
                pool_wait_micros: u64::MAX,
                lock_waits: u64::MAX,
                lock_wait_micros: u64::MAX,
                busy: u64::MAX,
            },
            counters,
        }
    }

    #[test]
    fn the_largest_status_fits_eight_kibibytes() {
        let text = largest().to_json().to_string();
        assert!(text.len() <= 8192, "{} bytes", text.len());
    }

    /// Only the cache directory is a path; a bypass carries names, never
    /// source paths (spec §11.2: no source text or project membership).
    #[test]
    fn status_names_no_source_path() {
        let status = CacheStatus {
            cause: Some(StatusCause::Store(Bypass::Location(
                pascal_cache::LocationIssue::NotPrivate { mode: 0o755 },
            ))),
            ..largest()
        };
        let json = status.to_json();
        assert_eq!(json["cause"]["kind"], "location");
        assert_eq!(json["cause"]["location"], "notPrivate");
        assert_eq!(json["cause"]["advice"], "freshDirectory");
        assert_eq!(json["schema"], 1);
    }
}
