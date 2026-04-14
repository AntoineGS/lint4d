//! Source-level preprocessing pass that detects `{$if*}...{$endif}` directive
//! pairs wrapping a **partial control-flow header** (body ending in `then`,
//! `else`, or `do`) and rewrites the directive markers to ASCII spaces so
//! tree-sitter-pascal can parse the remaining content as a normal statement.
//!
//! The erased spans are recorded as [`DirectivePatch`] records; `fmt4d` later
//! re-injects them into `DirectiveMap` as virtual directives so the formatter
//! emits the original directive text in the output.
//!
//! See `.full-review/bucket-c-design.md` Part 3 for the full design rationale.

use std::borrow::Cow;

/// A directive pair that was rewritten from the source.
///
/// `opening_*` and `closing_*` byte ranges are relative to the **original**
/// source (and also to the rewritten source, since the rewrite preserves byte
/// offsets). `*_text` contains the original directive bytes verbatim so the
/// formatter can re-emit them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectivePatch {
    pub opening_start: usize,
    pub opening_end: usize,
    pub opening_text: String,
    pub opening_row: usize,
    pub opening_col: usize,
    pub closing_start: usize,
    pub closing_end: usize,
    pub closing_text: String,
    pub closing_row: usize,
    pub closing_col: usize,
}

/// Scan `source` for partial-control-flow directive pairs, rewrite the
/// directive markers to ASCII spaces, and return the rewritten bytes plus
/// the list of patches.
///
/// Returns `Cow::Borrowed(source)` when no patches are found — the common
/// case — so files without partial-control-flow directives pay zero
/// allocation cost.
///
/// Safe to run multiple times on the same input: the output of a rewrite has
/// no `{$if*}` markers left to match, so a second pass produces zero
/// additional patches.
pub fn rewrite_partial_control_flow(source: &[u8]) -> (Cow<'_, [u8]>, Vec<DirectivePatch>) {
    // TODO(Task 4): implement
    (Cow::Borrowed(source), Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests added in Task 4 steps.
}
