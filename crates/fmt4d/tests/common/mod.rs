//! Shared test helpers for fmt4d integration tests.
//!
//! Previously each test file duplicated its own copy of `format_source`,
//! `format_aligned`, etc. This module consolidates them so a change to
//! the helper surface is one edit.

#![allow(dead_code)] // each test file uses a subset — don't warn.

use pascal_core::FileInfo;
use std::collections::HashSet;
use std::path::PathBuf;

/// Format `source` with the default config.
pub fn format_source(source: &str) -> String {
    let info = FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig::default();
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config, &HashSet::new())
        .expect("formatting failed")
}

/// Format `source` with alignment enabled.
pub fn format_aligned(source: &str) -> String {
    let info = FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig {
        alignment: fmt4d::config::AlignmentConfig {
            enabled: true,
            ..fmt4d::config::AlignmentConfig::default()
        },
        ..fmt4d::config::FmtConfig::default()
    };
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config, &HashSet::new())
        .expect("formatting failed")
}

/// Format `source` with an overridden `max_line_length`.
pub fn format_source_with_max(source: &str, max: usize) -> String {
    let info = FileInfo::new(PathBuf::from("test.pas"));
    let config = fmt4d::config::FmtConfig {
        max_line_length: max,
        ..fmt4d::config::FmtConfig::default()
    };
    fmt4d::formatter::format_source(source.as_bytes(), &info, &config, &HashSet::new())
        .expect("formatting failed")
}

/// Idempotence check: running the formatter twice produces identical output.
pub fn idempotency_check(source: &str) {
    let first = format_source(source);
    let second = format_source(&first);
    assert_eq!(
        first, second,
        "formatter is not idempotent.\nFirst pass:\n{first}\nSecond pass:\n{second}"
    );
}

/// Idempotence check with alignment enabled.
pub fn idempotency_check_aligned(source: &str) {
    let first = format_aligned(source);
    let second = format_aligned(&first);
    assert_eq!(
        first, second,
        "aligned formatter is not idempotent.\nFirst pass:\n{first}\nSecond pass:\n{second}"
    );
}
