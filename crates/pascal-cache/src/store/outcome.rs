use crate::Mismatch;
use std::io;

/// Why an operation did not use the persistent store. The caller continues
/// with uncached analysis; a bypass never says anything about the sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bypass {
    Location(LocationIssue),
    InvalidLimits(super::LimitsError),
    SqliteTooOld {
        version_number: i32,
    },
    WalUnavailable,
    /// The file is a SQLite database this program did not create.
    ForeignDatabase,
    /// A container of another physical format; it is left untouched.
    UnsupportedContainer {
        version: i64,
    },
    NamespaceLockBusy,
    /// The store's scope was reset or retired after it was opened.
    Fenced,
    Busy,
    ReadOnly,
    Full,
    Corrupt,
    OutOfMemory,
    ScratchExhausted,
    /// Decoded facts did not fit the caller's retained-byte admission.
    FactBudget,
    TooLarge(TooLarge),
    InvalidRowKind {
        ordinal: usize,
    },
    Integrity(Integrity),
    /// The artifact is being reclaimed; publish again later.
    Retiring,
    Io(io::ErrorKind),
    Sqlite {
        extended_code: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationIssue {
    NotAbsolute,
    NoDefaultDirectory,
    NotADirectory,
    Symlink,
    NotOwned,
    NotPrivate {
        mode: u32,
    },
    /// Not positively a local filesystem; `filesystem` is the platform's
    /// filesystem type when it has one.
    NotLocal {
        filesystem: u64,
    },
    FileNotRegular,
    /// The platform's locality and permission checks are not implemented
    /// (Windows: `l4d-x2ft`), so the store is not used there.
    PlatformChecksNotImplemented,
}

/// A write over an admission limit; the artifact is not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TooLarge {
    Manifest { bytes: usize },
    Supplemental { bytes: usize },
    Rows { count: usize },
    Row { ordinal: usize, bytes: usize },
    IndexKey { ordinal: usize, bytes: usize },
    Content { bytes: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Integrity {
    /// A stored manifest with the same digest has different bytes.
    ConflictingManifest,
    /// An immutable artifact with the same key has different content.
    ConflictingArtifact,
}

/// Why a lookup found nothing usable. A miss is never proof of absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miss {
    Absent,
    KeyMismatch(Mismatch),
    Retired,
    Malformed(Malformed),
    Oversized,
    ChecksumMismatch,
    /// The record disappeared between two bounded reads.
    Vanished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
    Header,
    Row,
    Ordinal,
    Count,
    /// Rows whose checksums hold but whose facts break a DTO invariant.
    Facts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup<T> {
    Hit(T),
    Miss(Miss),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    Inserted,
    AlreadyPresent,
}
