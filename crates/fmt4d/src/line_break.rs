//! Line-breaking utilities for long-line enforcement.
//!
//! Provides functions that the printer can use to decide when to break long
//! lines and where valid break points exist in a token sequence.

use pascal_core::node_kind as K;

/// Check if a line exceeds `max_length` and should be broken.
pub fn should_break_line(line: &str, max_length: usize) -> bool {
    line.len() > max_length
}

/// Find indices in a sequence of tokens where line breaks are allowed.
///
/// Each token is a `(text, kind)` pair where `kind` is the tree-sitter node
/// kind. Returns indices where a break can be inserted.
///
/// Break **after**: commas, binary operators (`kAnd`, `kOr`, `kAdd`, etc.)
/// Break **before**: `kThen`, `kDo`, `kOf`
pub fn find_break_points(tokens: &[(&str, &str)]) -> Vec<usize> {
    let mut points = Vec::new();
    for (i, &(_text, kind)) in tokens.iter().enumerate() {
        // Break after commas
        if kind == K::COMMA {
            points.push(i);
            continue;
        }
        // Break after binary operators
        if is_binary_operator(kind) {
            points.push(i);
            continue;
        }
        // Break before keywords that start a clause
        if is_break_before_keyword(kind) && i > 0 {
            // The break point is "before" this token, which means after the
            // previous token — use index i-1 to indicate the break goes
            // after token i-1 (before token i).
            points.push(i - 1);
        }
    }
    points
}

/// Find the best break point for a line that exceeds `max_length`.
///
/// Scans the tokens and returns the index of the break point that is closest
/// to `max_length` without exceeding it, or the first break point if all
/// exceed the limit.
pub fn best_break_point(tokens: &[(&str, &str)], max_length: usize) -> Option<usize> {
    let break_points = find_break_points(tokens);
    if break_points.is_empty() {
        return None;
    }

    // Compute cumulative column positions
    let mut col = 0usize;
    let mut token_end_cols: Vec<usize> = Vec::with_capacity(tokens.len());
    for (i, &(text, kind)) in tokens.iter().enumerate() {
        if i > 0 {
            // Account for spacing between tokens (simplified: 1 space)
            col += 1;
        }
        col += text.len();
        // For kinds that don't add trailing space, still record the column
        let _ = kind; // kind could be used for refined spacing
        token_end_cols.push(col);
    }

    // Find the last break point whose column is within max_length
    let mut best: Option<usize> = None;
    for &bp in &break_points {
        if bp < token_end_cols.len() && token_end_cols[bp] <= max_length {
            best = Some(bp);
        }
    }

    // If no break point fits within max_length, use the first one
    best.or(Some(break_points[0]))
}

fn is_binary_operator(kind: &str) -> bool {
    matches!(
        kind,
        K::K_ADD
            | K::K_SUB
            | K::K_MUL
            | K::K_DIV
            | K::K_MOD
            | K::K_AND
            | K::K_OR
            | K::K_XOR
            | K::K_SHL
            | K::K_SHR
            | K::K_ASSIGN
            | K::K_ASSIGN_ADD
            | K::K_ASSIGN_SUB
            | K::K_ASSIGN_MUL
            | K::K_ASSIGN_DIV
            | K::EQUALS
            | K::NOT_EQUALS
            | K::LESS_THAN
            | K::GREATER_THAN
            | K::LESS_EQUAL
            | K::GREATER_EQUAL
            | K::K_IN
            | K::K_IS
            | K::K_AS
    )
}

fn is_break_before_keyword(kind: &str) -> bool {
    matches!(kind, K::K_THEN | K::K_DO | K::K_OF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_break_when_exceeds_max() {
        assert!(should_break_line("a".repeat(121).as_str(), 120));
    }

    #[test]
    fn should_not_break_when_within_max() {
        assert!(!should_break_line("short line", 120));
    }

    #[test]
    fn should_not_break_at_exact_max() {
        let line = "a".repeat(120);
        assert!(!should_break_line(&line, 120));
    }

    #[test]
    fn find_break_after_comma() {
        let tokens = vec![("x", "ident"), (",", K::COMMA), ("y", "ident")];
        let points = find_break_points(&tokens);
        assert_eq!(points, vec![1]);
    }

    #[test]
    fn find_break_after_binary_operator() {
        let tokens = vec![("x", "ident"), ("+", K::K_ADD), ("y", "ident")];
        let points = find_break_points(&tokens);
        assert_eq!(points, vec![1]);
    }

    #[test]
    fn find_break_before_then() {
        let tokens = vec![
            ("if", K::K_IF),
            ("x", "ident"),
            (">", K::GREATER_THAN),
            ("0", "litInt"),
            ("then", K::K_THEN),
        ];
        let points = find_break_points(&tokens);
        // Break after `>` (binary op at index 2) and before `then` (after index 3)
        assert!(points.contains(&2));
        assert!(points.contains(&3));
    }

    #[test]
    fn find_break_before_do() {
        let tokens = vec![
            ("for", K::K_FOR),
            ("i", "ident"),
            (":=", K::K_ASSIGN),
            ("0", "litInt"),
            ("to", K::K_TO),
            ("10", "litInt"),
            ("do", K::K_DO),
        ];
        let points = find_break_points(&tokens);
        // Break after `:=` (index 2) and before `do` (after index 5)
        assert!(points.contains(&2));
        assert!(points.contains(&5));
    }

    #[test]
    fn no_break_points_for_simple_tokens() {
        let tokens = vec![("x", "ident")];
        let points = find_break_points(&tokens);
        assert!(points.is_empty());
    }

    #[test]
    fn best_break_within_limit() {
        // Tokens: "aaaa" "," "bbbb" "," "cccc" -> columns: 4, 6, 11, 13, 18
        let tokens = vec![
            ("aaaa", "ident"),
            (",", K::COMMA),
            ("bbbb", "ident"),
            (",", K::COMMA),
            ("cccc", "ident"),
        ];
        // max_length 12: break after second comma (index 3) would be col 13 > 12,
        // so best is first comma (index 1) at col 6
        let bp = best_break_point(&tokens, 12);
        assert_eq!(bp, Some(1));
    }

    #[test]
    fn best_break_returns_first_when_none_fit() {
        let tokens = vec![
            ("very_long_identifier", "ident"),
            (",", K::COMMA),
            ("another", "ident"),
        ];
        // max_length 5: comma is at col 22, so nothing fits — use first break point
        let bp = best_break_point(&tokens, 5);
        assert_eq!(bp, Some(1));
    }

    #[test]
    fn best_break_returns_none_when_no_break_points() {
        let tokens = vec![("only_one_token", "ident")];
        let bp = best_break_point(&tokens, 5);
        assert_eq!(bp, None);
    }

    #[test]
    fn multiple_operators_multiple_break_points() {
        let tokens = vec![
            ("a", "ident"),
            ("and", K::K_AND),
            ("b", "ident"),
            ("or", K::K_OR),
            ("c", "ident"),
        ];
        let points = find_break_points(&tokens);
        assert_eq!(points, vec![1, 3]);
    }
}
