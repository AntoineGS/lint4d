//! Bounded, stable names for the diagnostics (spec §11.2 amendment
//! 2026-11-03, D27). No name carries a path or source text.

use pascal_cache::admin::{PruneOutcome, RecoveryAdvice};
use pascal_cache::{Bypass, LocationIssue, Pressure};

pub(crate) const BYPASS_KINDS: usize = 33;

pub(crate) const BYPASS_NAMES: [&str; BYPASS_KINDS] = [
    "location",
    "invalidLimits",
    "sqliteTooOld",
    "walUnavailable",
    "foreignDatabase",
    "unsupportedContainer",
    "namespaceLockBusy",
    "fenced",
    "busy",
    "recovering",
    "readOnly",
    "full",
    "walPressure",
    "overBudget",
    "lowDiskSpace",
    "tooManyHolds",
    "invalidMaintenanceLimits",
    "corrupt",
    "outOfMemory",
    "scratchExhausted",
    "factBudget",
    "tooLarge",
    "invalidRowKind",
    "integrity",
    "retiring",
    "evidenceForLocalArtifact",
    "invalidRecord",
    "missingReference",
    "builderFenced",
    "duplicateMember",
    "sealRefused",
    "io",
    "sqlite",
];

/// Position of `bypass` in [`BYPASS_NAMES`]. Exhaustive: a new variant does
/// not compile until it is named.
pub(crate) fn bypass_index(bypass: &Bypass) -> usize {
    match bypass {
        Bypass::Location(_) => 0,
        Bypass::InvalidLimits(_) => 1,
        Bypass::SqliteTooOld { .. } => 2,
        Bypass::WalUnavailable => 3,
        Bypass::ForeignDatabase => 4,
        Bypass::UnsupportedContainer { .. } => 5,
        Bypass::NamespaceLockBusy => 6,
        Bypass::Fenced => 7,
        Bypass::Busy => 8,
        Bypass::Recovering => 9,
        Bypass::ReadOnly => 10,
        Bypass::Full => 11,
        Bypass::WalPressure => 12,
        Bypass::OverBudget => 13,
        Bypass::LowDiskSpace => 14,
        Bypass::TooManyHolds => 15,
        Bypass::InvalidMaintenanceLimits => 16,
        Bypass::Corrupt => 17,
        Bypass::OutOfMemory => 18,
        Bypass::ScratchExhausted => 19,
        Bypass::FactBudget => 20,
        Bypass::TooLarge(_) => 21,
        Bypass::InvalidRowKind { .. } => 22,
        Bypass::Integrity(_) => 23,
        Bypass::Retiring => 24,
        Bypass::EvidenceForLocalArtifact => 25,
        Bypass::InvalidRecord(_) => 26,
        Bypass::MissingReference => 27,
        Bypass::BuilderFenced => 28,
        Bypass::DuplicateMember => 29,
        Bypass::SealRefused(_) => 30,
        Bypass::Io(_) => 31,
        Bypass::Sqlite { .. } => 32,
    }
}

pub(crate) fn bypass_name(bypass: &Bypass) -> &'static str {
    BYPASS_NAMES[bypass_index(bypass)]
}

pub(crate) fn location_issue_name(issue: &LocationIssue) -> &'static str {
    match issue {
        LocationIssue::NotAbsolute => "notAbsolute",
        LocationIssue::NoDefaultDirectory => "noDefaultDirectory",
        LocationIssue::NotADirectory => "notADirectory",
        LocationIssue::Symlink => "symlink",
        LocationIssue::NotOwned => "notOwned",
        LocationIssue::NotPrivate { .. } => "notPrivate",
        LocationIssue::NotLocal { .. } => "notLocal",
        LocationIssue::FileNotRegular => "fileNotRegular",
        LocationIssue::PlatformChecksNotImplemented => "platformChecksNotImplemented",
    }
}

pub(crate) fn advice_name(advice: RecoveryAdvice) -> &'static str {
    match advice {
        RecoveryAdvice::None => "none",
        RecoveryAdvice::Retry => "retry",
        RecoveryAdvice::Prune => "prune",
        RecoveryAdvice::ResetNamespace => "resetNamespace",
        RecoveryAdvice::FreshDirectory => "freshDirectory",
    }
}

pub(crate) fn pressure_name(pressure: Pressure) -> &'static str {
    match pressure {
        Pressure::None => "none",
        Pressure::OverHeadroom => "overHeadroom",
        Pressure::WalPressure => "walPressure",
        Pressure::LowDiskSpace => "lowDiskSpace",
    }
}

pub(crate) fn outcome_name(outcome: PruneOutcome) -> &'static str {
    match outcome {
        PruneOutcome::WithinBudget => "withinBudget",
        PruneOutcome::NoProgress => "noProgress",
        PruneOutcome::TimeLimit => "timeLimit",
        PruneOutcome::Busy => "busy",
        PruneOutcome::WalPressure => "walPressure",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One value of every variant (the list in `pascal-cache`
    /// `admin.rs` `advice_maps_every_bypass_to_a_bounded_recommendation`).
    #[test]
    fn every_bypass_has_a_distinct_bounded_name() {
        use pascal_cache::*;
        let all = [
            Bypass::Location(LocationIssue::NotAbsolute),
            Bypass::InvalidLimits(LimitsError::Readers),
            Bypass::SqliteTooOld { version_number: 1 },
            Bypass::WalUnavailable,
            Bypass::ForeignDatabase,
            Bypass::UnsupportedContainer { version: 9 },
            Bypass::NamespaceLockBusy,
            Bypass::Fenced,
            Bypass::Busy,
            Bypass::Recovering,
            Bypass::ReadOnly,
            Bypass::Full,
            Bypass::WalPressure,
            Bypass::OverBudget,
            Bypass::LowDiskSpace,
            Bypass::TooManyHolds,
            Bypass::InvalidMaintenanceLimits,
            Bypass::Corrupt,
            Bypass::OutOfMemory,
            Bypass::ScratchExhausted,
            Bypass::FactBudget,
            Bypass::TooLarge(TooLarge::Rows { count: 1 }),
            Bypass::InvalidRowKind { ordinal: 0 },
            Bypass::Integrity(Integrity::ConflictingRecord),
            Bypass::Retiring,
            Bypass::EvidenceForLocalArtifact,
            Bypass::InvalidRecord(MemberError::EvidenceWithoutArtifact),
            Bypass::MissingReference,
            Bypass::BuilderFenced,
            Bypass::DuplicateMember,
            Bypass::SealRefused(SealRefusal::CountMismatch),
            Bypass::Io(std::io::ErrorKind::Other),
            Bypass::Sqlite { extended_code: 1 },
        ];
        let indexes: std::collections::BTreeSet<usize> = all.iter().map(bypass_index).collect();
        assert_eq!(indexes.len(), BYPASS_NAMES.len());
        assert_eq!(indexes.len(), all.len());
        for name in BYPASS_NAMES {
            assert!(
                name.len() <= 32 && name.chars().all(|c| c.is_ascii_alphanumeric()),
                "{name}"
            );
        }
    }
}
