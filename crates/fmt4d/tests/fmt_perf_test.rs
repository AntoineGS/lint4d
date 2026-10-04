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

const CHAIN_OPERANDS: usize = 20_000;

fn chain_unit() -> String {
    let operands: Vec<String> = (0..CHAIN_OPERANDS).map(|i| format!("V{i}")).collect();
    format!(
        "unit Big;\n\ninterface\n\nimplementation\n\nprocedure P;\nbegin\n  X := {};\nend;\n\nend.\n",
        operands.join(" + ")
    )
}

/// Runs `f` on a thread with a large stack: debug builds recurse deeply on a
/// left-nested chain this long.
fn on_big_stack(f: impl FnOnce() -> String + Send + 'static) -> (String, Duration) {
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(move || timed(f))
        .unwrap()
        .join()
        .unwrap()
}

#[test]
fn long_binary_chain_formats_in_linear_time() {
    let src = chain_unit();
    let (out, elapsed) = on_big_stack(move || format_source(&src));
    eprintln!("{CHAIN_OPERANDS}-operand chain: {elapsed:?}");
    assert!(out.contains(&format!("V{}", CHAIN_OPERANDS - 1)));
    assert!(elapsed < BOUND, "took {elapsed:?}, expected < {BOUND:?}");
}
