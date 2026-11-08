//! Cache administration on the command line (spec §11.2 amendments
//! 2026-11-03, D26). Each mode writes one JSON line of at most 4 KiB on
//! stdout and exits 0 (done), 1 (refused or failed) or 2 (usage, nothing
//! on stdout). None starts the server.

use crate::persistent_cache::names;
use crate::persistent_cache::status::{UsageView, truncate, usage_json};
use pascal_cache::admin::{self, PruneLimits, StoreStatus};
use pascal_cache::{
    Bypass, DEFAULT_MAX_NAMESPACES, Digest, MAX_NAMESPACES, Namespace, PolicyError, Store,
    StoreLimits, StorePolicy,
};
use serde_json::{Value, json};
use std::fmt;
use std::path::PathBuf;

/// Longest `--cache-directory` accepted, in bytes.
const MAX_DIRECTORY_BYTES: usize = 4096;
/// Longest directory echoed in the JSON output, in escaped bytes.
const MAX_REPORTED_DIRECTORY_BYTES: usize = 2048;
/// Longest JSON line a mode writes.
const MAX_OUTPUT_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Server,
    Help,
    Version,
    Cache {
        directory: Option<PathBuf>,
        mode: CacheMode,
        /// `--cache-max-namespaces`: the namespaces the opened store keeps
        /// (l4d-1ypi); `None` keeps the default.
        max_namespaces: Option<usize>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheMode {
    Status,
    Prune,
    Reset,
    Policy {
        max_disk_bytes: Option<u64>,
        wal_threshold_bytes: Option<u64>,
    },
}

impl CacheMode {
    fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Prune => "prune",
            Self::Reset => "reset",
            Self::Policy { .. } => "policy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(String);

impl fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

fn usage(message: impl Into<String>) -> UsageError {
    UsageError(message.into())
}

fn positive(flag: &str, value: Option<&String>) -> Result<u64, UsageError> {
    let value = value.ok_or_else(|| usage(format!("{flag} needs a value")))?;
    match value.parse::<u64>() {
        Ok(number) if number > 0 => Ok(number),
        _ => Err(usage(format!(
            "{flag} needs a positive integer, got {value:?}"
        ))),
    }
}

pub fn parse_arguments(arguments: &[String]) -> Result<Invocation, UsageError> {
    if !arguments
        .iter()
        .any(|argument| argument.starts_with("--cache-"))
    {
        return parse_server_arguments(arguments);
    }
    let mut directory: Option<PathBuf> = None;
    let mut simple: Option<CacheMode> = None;
    let mut max_disk_bytes = None;
    let mut wal_threshold_bytes = None;
    let mut max_namespaces = None;
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        let mode = match argument.as_str() {
            "--cache-status" => Some(CacheMode::Status),
            "--cache-prune" => Some(CacheMode::Prune),
            "--cache-reset" => Some(CacheMode::Reset),
            _ => None,
        };
        if let Some(mode) = mode {
            if simple.replace(mode).is_some() {
                return Err(usage(format!(
                    "{argument} repeats or conflicts with a mode"
                )));
            }
            continue;
        }
        match argument.as_str() {
            "--cache-directory" => {
                let value = iterator
                    .next()
                    .ok_or_else(|| usage("--cache-directory needs a value"))?;
                let path = PathBuf::from(value);
                if !path.is_absolute() || value.len() > MAX_DIRECTORY_BYTES {
                    return Err(usage(format!(
                        "--cache-directory needs an absolute path of at most {MAX_DIRECTORY_BYTES} bytes"
                    )));
                }
                if directory.replace(path).is_some() {
                    return Err(usage("--cache-directory given twice"));
                }
            }
            "--cache-max-disk-bytes" => {
                let value = positive(argument, iterator.next())?;
                if max_disk_bytes.replace(value).is_some() {
                    return Err(usage("--cache-max-disk-bytes given twice"));
                }
            }
            "--cache-wal-threshold-bytes" => {
                let value = positive(argument, iterator.next())?;
                if wal_threshold_bytes.replace(value).is_some() {
                    return Err(usage("--cache-wal-threshold-bytes given twice"));
                }
            }
            "--cache-max-namespaces" => {
                let value = positive(argument, iterator.next())?;
                let value = usize::try_from(value)
                    .ok()
                    .filter(|value| *value <= MAX_NAMESPACES)
                    .ok_or_else(|| {
                        usage(format!(
                            "--cache-max-namespaces needs an integer from 1 to {MAX_NAMESPACES}"
                        ))
                    })?;
                if max_namespaces.replace(value).is_some() {
                    return Err(usage("--cache-max-namespaces given twice"));
                }
            }
            "--stdio" | "-h" | "--help" | "-V" | "--version" => {
                return Err(usage(format!(
                    "{argument} cannot be combined with a cache mode"
                )));
            }
            _ => return Err(usage(format!("unknown argument {argument:?}"))),
        }
    }
    let policy = max_disk_bytes.is_some() || wal_threshold_bytes.is_some();
    let mode = match (simple, policy) {
        (Some(_), true) => {
            return Err(usage(
                "the policy options cannot be combined with --cache-status, --cache-prune or --cache-reset",
            ));
        }
        (Some(mode), false) => mode,
        (None, true) => CacheMode::Policy {
            max_disk_bytes,
            wal_threshold_bytes,
        },
        (None, false) => {
            return Err(usage(
                "--cache-directory and --cache-max-namespaces need a cache mode",
            ));
        }
    };
    if mode == CacheMode::Status && max_namespaces.is_some() {
        return Err(usage(
            "--cache-max-namespaces cannot be combined with --cache-status, which retires nothing",
        ));
    }
    Ok(Invocation::Cache {
        directory,
        mode,
        max_namespaces,
    })
}

fn parse_server_arguments(arguments: &[String]) -> Result<Invocation, UsageError> {
    for argument in arguments {
        match argument.as_str() {
            "--stdio" => {}
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            _ => return Err(usage(format!("unknown argument {argument:?}"))),
        }
    }
    Ok(Invocation::Server)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub code: i32,
    pub stdout: String,
    pub stderr: Option<String>,
}

fn error_json(bypass: &Bypass) -> Value {
    let location = match bypass {
        Bypass::Location(issue) => Some(names::location_issue_name(issue)),
        _ => None,
    };
    json!({
        "bypass": names::bypass_name(bypass),
        "location": location,
        "advice": names::advice_name(admin::advice(bypass)),
    })
}

fn usage_pair(usage: &pascal_cache::Usage) -> Value {
    json!({
        "physicalBytes": usage.database_bytes
            .saturating_add(usage.wal_bytes)
            .saturating_add(usage.shm_bytes),
        "freeBytes": usage.free_bytes(),
    })
}

fn failure(
    base: Value,
    mode: &str,
    error: Value,
    kind: &str,
    advice: &str,
) -> (Value, i32, String) {
    let mut base = base;
    base["error"] = error;
    // The policy mode has two flags, so it is named by what failed.
    let operation = match mode {
        "policy" => "policy change".to_owned(),
        _ => format!("--cache-{mode}"),
    };
    (
        base,
        1,
        format!("pascal-lsp: {operation} failed: {kind}; advice: {advice}"),
    )
}

fn bypass_failure(base: Value, mode: &str, bypass: &Bypass) -> (Value, i32, String) {
    let advice = names::advice_name(admin::advice(bypass));
    failure(
        base,
        mode,
        error_json(bypass),
        names::bypass_name(bypass),
        advice,
    )
}

fn directory_json(directory: Option<&PathBuf>) -> (Value, bool) {
    let Some(directory) = directory else {
        return (Value::Null, false);
    };
    let lossy = directory.to_string_lossy();
    let kept = truncate(&lossy, MAX_REPORTED_DIRECTORY_BYTES);
    (json!(kept), kept.len() < lossy.len())
}

pub fn run(directory: Option<PathBuf>, mode: CacheMode, max_namespaces: Option<usize>) -> Outcome {
    let resolved = match directory {
        Some(directory) => Ok(directory),
        None => pascal_cache::default_directory(),
    };
    let (shown, truncated) = directory_json(resolved.as_ref().ok());
    let mut base = json!({ "schema": 1, "mode": mode.name(), "directory": shown });
    if truncated {
        base["directoryTruncated"] = json!(true);
    }
    let (json, code, stderr) = match &resolved {
        Err(bypass) => bypass_failure(base, mode.name(), bypass),
        Ok(directory) => {
            let limits = StoreLimits {
                max_namespaces: max_namespaces.unwrap_or(DEFAULT_MAX_NAMESPACES),
                ..StoreLimits::default()
            };
            execute(base, directory, &mode, limits)
        }
    };
    Outcome {
        code,
        stdout: json.to_string(),
        stderr: (code == 1).then_some(stderr),
    }
}

fn execute(
    mut base: Value,
    directory: &std::path::Path,
    mode: &CacheMode,
    limits: StoreLimits,
) -> (Value, i32, String) {
    let namespace = Namespace::current();
    let name = mode.name();
    match mode {
        CacheMode::Status => match admin::status(directory) {
            StoreStatus::Absent => {
                base["store"] = json!("absent");
                (base, 0, String::new())
            }
            StoreStatus::Unreadable { bypass, .. } => {
                base["store"] = json!("unreadable");
                bypass_failure(base, name, &bypass)
            }
            StoreStatus::Present(report) => {
                let view = UsageView {
                    usage: report.usage,
                    policy: report.policy,
                    pressure: report.pressure,
                };
                base["store"] = json!("present");
                base["container"] = json!({
                    "version": report.container.version,
                    "compatible": report.container.compatible,
                });
                base["usage"] = usage_json(&view);
                base["policy"] = json!({
                    "revision": report.policy.revision,
                    "maxDiskBytes": report.policy.policy.max_disk_bytes,
                    "walThresholdBytes": report.policy.policy.wal_threshold_bytes,
                });
                base["pressure"] = json!(names::pressure_name(report.pressure));
                base["namespaces"] = json!({
                    "live": report.live_namespaces,
                    "retired": report.retired_namespaces,
                    "list": [],
                    "listTruncated": report.namespaces_truncated,
                });
                base["retiredScopes"] = json!(report.retired_scopes);
                base["liveLeases"] = json!(report.live_leases);
                base["advice"] = json!(names::advice_name(report.advice));
                list_namespaces(&mut base, &report.namespaces, namespace.id());
                (base, 0, String::new())
            }
        },
        CacheMode::Prune => {
            match admin::prune_until(directory, namespace, limits, PruneLimits::default()) {
                Ok(None) => {
                    base["store"] = json!("absent");
                    (base, 0, String::new())
                }
                Ok(Some(report)) => {
                    base["outcome"] = json!(names::outcome_name(report.outcome));
                    base["cycles"] = json!(report.cycles);
                    base["rows"] = json!(report.rows);
                    base["before"] = usage_pair(&report.before);
                    base["after"] = usage_pair(&report.after);
                    base["pressure"] = json!(names::pressure_name(report.pressure));
                    (base, 0, String::new())
                }
                Err(bypass) => bypass_failure(base, name, &bypass),
            }
        }
        CacheMode::Reset => match admin::reset(directory, namespace, limits) {
            Ok(false) => {
                base["store"] = json!("absent");
                (base, 0, String::new())
            }
            Ok(true) => {
                base["reset"] = json!(true);
                (base, 0, String::new())
            }
            Err(bypass) => bypass_failure(base, name, &bypass),
        },
        CacheMode::Policy {
            max_disk_bytes,
            wal_threshold_bytes,
        } => set_policy(
            base,
            directory,
            *max_disk_bytes,
            *wal_threshold_bytes,
            limits,
        ),
    }
}

/// List `namespaces` in `base`, the most recently used first, dropping the
/// least recently used ones that would take the line past
/// [`MAX_OUTPUT_BYTES`].
fn list_namespaces(base: &mut Value, namespaces: &[admin::NamespaceStatus], current: Digest) {
    let entries: Vec<Value> = namespaces
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id.to_hex(),
                "current": entry.id == current,
                "resetEpoch": entry.reset_epoch,
                "lastUsed": entry.last_used_unix_seconds,
                "estimatedBytes": entry.estimated_bytes,
                "held": entry.held,
            })
        })
        .collect();
    let mut listed = entries.len();
    loop {
        base["namespaces"]["list"] = Value::Array(entries[..listed].to_vec());
        if listed < entries.len() {
            base["namespaces"]["listTruncated"] = json!(true);
        }
        if listed == 0 || base.to_string().len() <= MAX_OUTPUT_BYTES {
            return;
        }
        listed -= 1;
    }
}

fn set_policy(
    mut base: Value,
    directory: &std::path::Path,
    max_disk_bytes: Option<u64>,
    wal_threshold_bytes: Option<u64>,
    limits: StoreLimits,
) -> (Value, i32, String) {
    let store = match Store::open(directory, Namespace::current(), limits) {
        Ok(store) => store,
        Err(bypass) => return bypass_failure(base, "policy", &bypass),
    };
    let current = match store.policy() {
        Ok(current) => current,
        Err(bypass) => return bypass_failure(base, "policy", &bypass),
    };
    let wanted = StorePolicy {
        max_disk_bytes: max_disk_bytes.unwrap_or(current.policy.max_disk_bytes),
        wal_threshold_bytes: wal_threshold_bytes.unwrap_or(current.policy.wal_threshold_bytes),
    };
    match store.set_policy(current.revision, wanted) {
        Ok(state) => {
            base["policy"] = json!({
                "revision": state.revision,
                "maxDiskBytes": state.policy.max_disk_bytes,
                "walThresholdBytes": state.policy.wal_threshold_bytes,
            });
            (base, 0, String::new())
        }
        Err(PolicyError::Store(bypass)) => bypass_failure(base, "policy", &bypass),
        Err(PolicyError::RevisionChanged { current }) => failure(
            base,
            "policy",
            json!({ "policy": "revisionChanged", "current": current }),
            "revisionChanged",
            "rerun",
        ),
        Err(other) => {
            let kind = match other {
                PolicyError::WalThresholdTooSmall => "walThresholdTooSmall",
                PolicyError::NoMaintenanceHeadroom => "noMaintenanceHeadroom",
                _ => "valueTooLarge",
            };
            failure(base, "policy", json!({ "policy": kind }), kind, "none")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn modes_parse_and_exclude_each_other() {
        assert!(matches!(
            parse_arguments(&args(&[])),
            Ok(Invocation::Server)
        ));
        assert!(matches!(
            parse_arguments(&args(&["--stdio"])),
            Ok(Invocation::Server)
        ));
        assert!(matches!(
            parse_arguments(&args(&["--help", "--stdio"])),
            Ok(Invocation::Help)
        ));
        assert!(matches!(
            parse_arguments(&args(&["--cache-status"])),
            Ok(Invocation::Cache {
                directory: None,
                mode: CacheMode::Status,
                max_namespaces: None,
            })
        ));
        assert!(matches!(
            parse_arguments(&args(&[
                "--cache-directory",
                "/c",
                "--cache-max-disk-bytes",
                "1073741824",
                "--cache-wal-threshold-bytes",
                "16777216"
            ])),
            Ok(Invocation::Cache {
                mode: CacheMode::Policy {
                    max_disk_bytes: Some(1073741824),
                    wal_threshold_bytes: Some(16777216)
                },
                ..
            })
        ));
        assert!(matches!(
            parse_arguments(&args(&["--cache-prune", "--cache-max-namespaces", "16"])),
            Ok(Invocation::Cache {
                mode: CacheMode::Prune,
                max_namespaces: Some(16),
                ..
            })
        ));
        let long = format!("/{}", "a".repeat(4096));
        for bad in [
            &["--cache-status", "--cache-prune"][..],
            &["--cache-reset", "--cache-max-disk-bytes", "1"],
            &["--cache-directory", "/c"],
            &["--cache-status", "--stdio"],
            &["--cache-status", "--help"],
            &["--cache-status", "--version"],
            &["--cache-status", "--cache-status"],
            &["--cache-max-disk-bytes", "0"],
            &["--cache-max-disk-bytes", "1k"],
            &["--cache-max-disk-bytes"],
            &["--cache-max-disk-bytes", "1", "--cache-max-disk-bytes", "2"],
            &["--cache-directory", "relative", "--cache-status"],
            &[
                "--cache-directory",
                "/a",
                "--cache-directory",
                "/b",
                "--cache-status",
            ],
            &["--cache-directory", &long, "--cache-status"],
            &["--bogus"],
            &["--cache-max-namespaces", "2"],
            &["--cache-status", "--cache-max-namespaces", "2"],
            &["--cache-prune", "--cache-max-namespaces", "0"],
            &["--cache-prune", "--cache-max-namespaces", "17"],
        ] {
            assert!(parse_arguments(&args(bad)).is_err(), "{bad:?}");
        }
    }
}
