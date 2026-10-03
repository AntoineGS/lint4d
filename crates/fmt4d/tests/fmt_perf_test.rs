//! Scaling regression tests: formatting cost must stay linear in input size.
//!
//! Bounds are deliberately generous (these run unoptimised on loaded
//! machines) but sit far below what the quadratic implementations took.

mod common;

use common::{format_aligned, format_source};
use std::time::{Duration, Instant};

const DECL_LINES: usize = 50_000;
const BOUND: Duration = Duration::from_secs(10);

fn declaration_only_unit() -> String {
    let mut src = String::from("unit Big;\n\ninterface\n\nvar\n");
    // Blank-line separated, so every declaration queries the blank-line index.
    for i in 0..DECL_LINES / 2 {
        src.push_str(&format!("  V{i}: Integer;\n\n"));
    }
    src.push_str("\nimplementation\n\nend.\n");
    src
}

fn timed(f: impl FnOnce() -> String) -> (String, Duration) {
    let start = Instant::now();
    let out = f();
    (out, start.elapsed())
}

#[test]
fn declaration_only_unit_formats_in_linear_time() {
    let src = declaration_only_unit();
    let (out, elapsed) = timed(move || format_source(&src));
    eprintln!("declaration-only unit: {elapsed:?}");
    assert!(out.contains(&format!("V{}: Integer;", DECL_LINES / 2 - 1)));
    assert!(elapsed < BOUND, "took {elapsed:?}, expected < {BOUND:?}");
}

#[test]
fn aligned_declaration_only_unit_formats_in_linear_time() {
    let src = declaration_only_unit();
    let (out, elapsed) = timed(move || format_aligned(&src));
    eprintln!("aligned declaration-only unit: {elapsed:?}");
    assert!(out.contains(&format!("V{}", DECL_LINES / 2 - 1)));
    assert!(elapsed < BOUND, "took {elapsed:?}, expected < {BOUND:?}");
}
