use std::collections::{HashMap, HashSet, VecDeque};

use tree_sitter::Tree;

use crate::cfg::analysis::AnalysisContext;
use crate::engine::{Diagnostic, FileInfo, Severity};
use crate::rules::{LintContext, Rule, RuleCategory, RuleMeta};

pub struct UseAfterFreeRule {
    meta: RuleMeta,
}

impl Default for UseAfterFreeRule {
    fn default() -> Self {
        Self::new()
    }
}

impl UseAfterFreeRule {
    pub fn new() -> Self {
        UseAfterFreeRule {
            meta: RuleMeta {
                id: "use-after-free",
                name: "Use After Free",
                category: RuleCategory::ResourceManagement,
                default_severity: Severity::Error,
                description: "Detects variables used after being freed.",
                enabled_by_default: true,
            },
        }
    }
}

impl Rule for UseAfterFreeRule {
    fn meta(&self) -> &RuleMeta {
        &self.meta
    }

    fn requires_cfg(&self) -> bool {
        true
    }

    fn check(
        &self,
        _file: &FileInfo,
        _tree: &Tree,
        _source: &[u8],
        _config: &crate::config::Config,
        _ctx: &mut LintContext,
    ) {
        // CFG-based rule; analysis happens in check_cfg.
    }

    fn check_cfg(
        &self,
        _file: &FileInfo,
        _tree: &Tree,
        source: &[u8],
        _config: &crate::config::Config,
        analysis: &AnalysisContext<'_>,
        ctx: &mut LintContext,
    ) {
        for cfg in analysis.cfgs.values() {
            analyze_cfg(cfg, source, ctx);
        }
    }
}

/// Analyze a single procedure's CFG for use-after-free patterns.
fn analyze_cfg(cfg: &cfg_core::types::Cfg, source: &[u8], ctx: &mut LintContext) {
    // BFS/worklist: propagate freed-variable state through CFG blocks.
    // State per block: HashMap<String, bool> where key is lowercase var name,
    // value is true if freed.
    type FreedState = HashMap<String, bool>;

    let graph = &cfg.graph;
    let entry = cfg.entry;

    // Map from BlockId to the merged "freed" state at block entry.
    let mut block_entry_state: HashMap<cfg_core::BlockId, FreedState> = HashMap::new();
    let mut visited: HashSet<cfg_core::BlockId> = HashSet::new();
    let mut worklist: VecDeque<cfg_core::BlockId> = VecDeque::new();

    block_entry_state.insert(entry, FreedState::new());
    worklist.push_back(entry);

    while let Some(block_id) = worklist.pop_front() {
        let entry_state = block_entry_state
            .get(&block_id)
            .cloned()
            .unwrap_or_default();

        let block = &graph[block_id.index()];
        let mut state = entry_state;

        // Process each statement in the block.
        for stmt in &block.stmts {
            let start = stmt.byte_range.start;
            let end = stmt.byte_range.end.min(source.len());
            if start >= end {
                continue;
            }
            let text = pascal_core::decode_bytes(&source[start..end]);

            process_statement(text.as_ref(), start, source, &mut state, ctx);
        }

        // Propagate state to successors.
        for successor_idx in graph.neighbors(block_id.index()) {
            let successor = cfg_core::BlockId::from(successor_idx);
            let existing = block_entry_state.get(&successor);
            let merged = match existing {
                Some(existing_state) => merge_states(existing_state, &state),
                None => state.clone(),
            };

            let changed = existing.map(|e| e != &merged).unwrap_or(true);

            if changed || !visited.contains(&successor) {
                block_entry_state.insert(successor, merged);
                if visited.insert(successor) {
                    worklist.push_back(successor);
                } else {
                    // Re-add to worklist if state changed even though visited.
                    worklist.push_back(successor);
                }
            }
        }
    }
}

/// Merge two freed-state maps. A variable is freed if it is freed in ANY
/// predecessor (conservative for use-after-free: if any path frees it, we
/// flag subsequent use).
fn merge_states(a: &HashMap<String, bool>, b: &HashMap<String, bool>) -> HashMap<String, bool> {
    let mut merged = a.clone();
    for (var, &freed) in b {
        if freed {
            merged.insert(var.clone(), true);
        }
    }
    merged
}

/// Process a single statement's text: detect frees, assignments, and
/// references to freed variables.
fn process_statement(
    text: &str,
    byte_offset: usize,
    source: &[u8],
    state: &mut HashMap<String, bool>,
    ctx: &mut LintContext,
) {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();

    // 1. Check for FreeAndNil(varname)
    if let Some(var) = parse_freeandnil(&lower) {
        if state.get(&var).copied().unwrap_or(false) {
            // Double free via FreeAndNil
            let (line, col) = byte_offset_to_line_col(source, byte_offset);
            let (end_line, end_col) = byte_offset_to_line_col(source, byte_offset + trimmed.len());
            ctx.report(Diagnostic {
                rule_id: "use-after-free".to_string(),
                severity: Severity::Error,
                message: format!("Double free: '{}' has already been freed", var),
                line,
                column: col,
                end_line,
                end_column: end_col,
                help: Some(
                    "Remove the duplicate free call or check if the variable was reassigned."
                        .to_string(),
                ),
                scope: None,
            });
        }
        state.insert(var, true);
        return;
    }

    // 2. Check for varname.Free or varname.Destroy
    if let Some(var) = parse_free_call(&lower) {
        if state.get(&var).copied().unwrap_or(false) {
            // Double free
            let (line, col) = byte_offset_to_line_col(source, byte_offset);
            let (end_line, end_col) = byte_offset_to_line_col(source, byte_offset + trimmed.len());
            ctx.report(Diagnostic {
                rule_id: "use-after-free".to_string(),
                severity: Severity::Error,
                message: format!("Double free: '{}' has already been freed", var),
                line,
                column: col,
                end_line,
                end_column: end_col,
                help: Some(
                    "Remove the duplicate free call or check if the variable was reassigned."
                        .to_string(),
                ),
                scope: None,
            });
        }
        state.insert(var, true);
        return;
    }

    // 3. Check for assignment: varname :=
    if let Some(var) = parse_assignment(&lower) {
        // Clear the freed state for this variable.
        state.remove(&var);
        return;
    }

    // 4. Check if any freed variable is referenced.
    check_freed_references(trimmed, &lower, byte_offset, source, state, ctx);
}

/// Parse `freeandnil(varname)` pattern. Returns the lowercase variable name.
fn parse_freeandnil(lower: &str) -> Option<String> {
    let stripped = lower.trim().trim_end_matches(';').trim();
    if !stripped.starts_with("freeandnil(") {
        return None;
    }
    let inner = stripped
        .strip_prefix("freeandnil(")?
        .strip_suffix(')')?
        .trim();
    if inner.is_empty() || !is_identifier(inner) {
        return None;
    }
    Some(inner.to_string())
}

/// Parse `varname.free` or `varname.destroy` pattern.
/// Returns the lowercase variable name.
fn parse_free_call(lower: &str) -> Option<String> {
    let stripped = lower.trim().trim_end_matches(';').trim();
    let var = stripped
        .strip_suffix(".free")
        .or_else(|| stripped.strip_suffix(".destroy"))?
        .trim();
    if var.is_empty() || !is_identifier(var) {
        return None;
    }
    Some(var.to_string())
}

/// Parse `varname := ...` pattern. Returns the lowercase variable name.
fn parse_assignment(lower: &str) -> Option<String> {
    let idx = lower.find(":=")?;
    let var = lower[..idx].trim();
    if var.is_empty() || !is_identifier(var) {
        return None;
    }
    Some(var.to_string())
}

/// Check if a string is a valid Delphi identifier (letters, digits, underscores).
fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !s.chars().next().unwrap().is_ascii_digit()
}

/// Check if any freed variable appears in the statement text.
fn check_freed_references(
    text: &str,
    lower: &str,
    byte_offset: usize,
    source: &[u8],
    state: &HashMap<String, bool>,
    ctx: &mut LintContext,
) {
    for (var, &freed) in state {
        if !freed {
            continue;
        }
        if contains_word(lower, var) {
            let (line, col) = byte_offset_to_line_col(source, byte_offset);
            let (end_line, end_col) =
                byte_offset_to_line_col(source, byte_offset + text.trim().len());
            ctx.report(Diagnostic {
                rule_id: "use-after-free".to_string(),
                severity: Severity::Error,
                message: format!("Use after free: '{}' is used after being freed", var),
                line,
                column: col,
                end_line,
                end_column: end_col,
                help: Some(
                    "Do not use a variable after it has been freed. \
                     Reassign it first or restructure the code."
                        .to_string(),
                ),
                scope: None,
            });
            // Only report once per statement even if multiple freed vars match.
            return;
        }
    }
}

/// Check if `haystack` contains `needle` as a whole word (bounded by
/// non-identifier characters or string boundaries).
fn contains_word(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.len() > h.len() {
        return false;
    }
    let mut i = 0;
    while i + n.len() <= h.len() {
        if &h[i..i + n.len()] == n {
            let before_ok = i == 0 || !(h[i - 1].is_ascii_alphanumeric() || h[i - 1] == b'_');
            let after_ok = i + n.len() == h.len()
                || !(h[i + n.len()].is_ascii_alphanumeric() || h[i + n.len()] == b'_');
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Convert a byte offset in source to 1-based (line, column).
fn byte_offset_to_line_col(source: &[u8], offset: usize) -> (usize, usize) {
    let offset = offset.min(source.len());
    let mut line = 1;
    let mut last_newline = 0; // byte position right after last newline
    for (i, &b) in source[..offset].iter().enumerate() {
        if b == b'\n' {
            line += 1;
            last_newline = i + 1;
        }
    }
    let col = offset - last_newline + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_freeandnil_basic() {
        assert_eq!(
            parse_freeandnil("freeandnil(aobj);"),
            Some("aobj".to_string())
        );
        assert_eq!(
            parse_freeandnil("  freeandnil( foo ) ;"),
            Some("foo".to_string())
        );
    }

    #[test]
    fn parse_free_call_basic() {
        assert_eq!(parse_free_call("aobj.free;"), Some("aobj".to_string()));
        assert_eq!(parse_free_call("myobj.destroy;"), Some("myobj".to_string()));
        assert_eq!(parse_free_call("something.else;"), None);
    }

    #[test]
    fn parse_assignment_basic() {
        assert_eq!(
            parse_assignment("aobj := tobject.create;"),
            Some("aobj".to_string())
        );
        assert_eq!(parse_assignment("no assignment here"), None);
    }

    #[test]
    fn contains_word_checks() {
        assert!(contains_word("aobj.classname", "aobj"));
        assert!(!contains_word("xaobj.classname", "aobj"));
        assert!(contains_word("dosomething(aobj)", "aobj"));
    }

    #[test]
    fn byte_offset_to_line_col_basic() {
        let source = b"line1\nline2\nline3";
        assert_eq!(byte_offset_to_line_col(source, 0), (1, 1));
        assert_eq!(byte_offset_to_line_col(source, 6), (2, 1));
        assert_eq!(byte_offset_to_line_col(source, 12), (3, 1));
    }
}
