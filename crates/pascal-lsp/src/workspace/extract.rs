use super::rename::CANCELLATION_MESSAGE;
use crate::text;
use lsp_types::{CodeActionKind, Range, TextEdit};
use pascal_core::{FileInfo, parser};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const VARIABLE_KIND: CodeActionKind = CodeActionKind::new("refactor.extract.variable");
const ROUTINE_KIND: CodeActionKind = CodeActionKind::new("refactor.extract.function");

pub(crate) struct Extraction {
    pub(crate) kind: CodeActionKind,
    pub(crate) edits: Vec<TextEdit>,
}

pub(crate) fn plan(
    source: &str,
    selection: Range,
    cancel: &AtomicBool,
) -> Result<Vec<Extraction>, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLATION_MESSAGE.to_string());
    }
    if source.len() > MAX_SOURCE_BYTES || !source.contains('\n') {
        return Ok(Vec::new());
    }
    let Some(start) = text::position_to_offset(source, selection.start) else {
        return Ok(Vec::new());
    };
    let Some(end) = text::position_to_offset(source, selection.end) else {
        return Ok(Vec::new());
    };
    if start >= end || !source.is_char_boundary(start) || !source.is_char_boundary(end) {
        return Ok(Vec::new());
    }
    let info = FileInfo::new("Extract.pas".into());
    let (tree, diagnostics) = parser::parse_file(&info, source.as_bytes())?;
    if tree.root_node().has_error() || !diagnostics.is_empty() {
        return Ok(Vec::new());
    }
    let Some(node) = tree
        .root_node()
        .named_descendant_for_byte_range(start, end - 1)
    else {
        return Ok(Vec::new());
    };
    let mut current = Some(node);
    let mut assignment = None;
    while let Some(candidate) = current {
        if candidate.kind() == "assignment" {
            assignment = Some(candidate);
            break;
        }
        current = candidate.parent();
    }
    let Some(assignment) = assignment else {
        return Ok(Vec::new());
    };
    let Some(block) = assignment.parent().filter(|node| node.kind() == "block") else {
        return Ok(Vec::new());
    };
    let Some(routine) = block.parent().filter(|node| {
        node.kind() == "defProc"
            && node
                .child_by_field_name("body")
                .is_some_and(|body| body == block)
    }) else {
        return Ok(Vec::new());
    };
    let Some(header) = routine.child_by_field_name("header") else {
        return Ok(Vec::new());
    };
    let Some(header_name) = header
        .child_by_field_name("name")
        .filter(|name| name.kind() == "identifier")
    else {
        return Ok(Vec::new());
    };
    let name = &source[header_name.start_byte()..header_name.end_byte()];
    if header.kind() != "declProc"
        || header.child_by_field_name("args").is_some()
        || !source[header.start_byte()..header.end_byte()]
            .eq_ignore_ascii_case(&format!("procedure {name};"))
    {
        return Ok(Vec::new());
    }
    let Some(lhs) = assignment
        .child_by_field_name("lhs")
        .filter(|n| n.kind() == "identifier")
    else {
        return Ok(Vec::new());
    };
    let Some(rhs) = assignment
        .child_by_field_name("rhs")
        .filter(|n| n.kind() == "literalNumber")
    else {
        return Ok(Vec::new());
    };
    let target = &source[lhs.start_byte()..lhs.end_byte()];
    if !target.is_ascii()
        || !target
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        || !target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Ok(Vec::new());
    }
    let literal = &source[rhs.start_byte()..rhs.end_byte()];
    if !literal.bytes().all(|byte| byte.is_ascii_digit())
        || literal.parse::<i32>().is_err()
        || source[assignment.end_byte()..].as_bytes().first() != Some(&b';')
    {
        return Ok(Vec::new());
    }
    if source[routine.start_byte()..routine.end_byte()].contains(['{', '}'])
        || source[routine.start_byte()..routine.end_byte()].contains("(*")
        || source[routine.start_byte()..routine.end_byte()].contains("//")
    {
        return Ok(Vec::new());
    }
    let mut local_cursor = routine.walk();
    let locals = routine.children_by_field_name("local", &mut local_cursor);
    let mut target_declarations = 0;
    let mut existing_var = false;
    for local in locals {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        if local.kind() != "declVars" {
            return Ok(Vec::new());
        }
        existing_var = true;
        let mut cursor = local.walk();
        for decl in local
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "declVar")
        {
            if cancel.load(Ordering::Relaxed) {
                return Err(CANCELLATION_MESSAGE.to_string());
            }
            let Some(ty) = decl.child_by_field_name("type") else {
                return Ok(Vec::new());
            };
            let mut name_cursor = decl.walk();
            let names = decl
                .children_by_field_name("name", &mut name_cursor)
                .collect::<Vec<_>>();
            if !names
                .iter()
                .any(|name| source[name.start_byte()..name.end_byte()].eq_ignore_ascii_case(target))
            {
                continue;
            }
            if names.len() != 1
                || !matches!(ty.kind(), "type" | "typeref")
                || !source[ty.start_byte()..ty.end_byte()].eq_ignore_ascii_case("Integer")
                || !source[decl.start_byte()..decl.end_byte()]
                    .eq_ignore_ascii_case(&format!("{target}: Integer;"))
            {
                return Ok(Vec::new());
            }
            target_declarations += 1;
        }
    }
    if target_declarations != 1 {
        return Ok(Vec::new());
    }
    let lower = source.to_ascii_lowercase();
    let line_ending = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let statement_start = source[..assignment.start_byte()]
        .rfind('\n')
        .map_or(0, |at| at + 1);
    let block_start = source[..block.start_byte()]
        .rfind('\n')
        .map_or(0, |at| at + 1);
    let statement_indent = &source[statement_start..assignment.start_byte()];
    let block_indent = &source[block_start..block.start_byte()];
    if !statement_indent.bytes().all(|b| b == b' ' || b == b'\t')
        || !block_indent.bytes().all(|b| b == b' ' || b == b'\t')
        || !source[assignment.end_byte() + 1..]
            .lines()
            .next()
            .is_none_or(|tail| tail.trim().is_empty())
    {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    if start == rhs.start_byte() && end == rhs.end_byte() && !lower.contains("extractedvalue") {
        let declaration = if existing_var {
            format!("{block_indent}  ExtractedValue: Integer;{line_ending}")
        } else {
            format!(
                "{block_indent}var{line_ending}{block_indent}  ExtractedValue: Integer;{line_ending}"
            )
        };
        let edits = [
            (block_start, block_start, declaration),
            (
                statement_start,
                statement_start,
                format!("{statement_indent}ExtractedValue := {literal};{line_ending}"),
            ),
            (
                rhs.start_byte(),
                rhs.end_byte(),
                "ExtractedValue".to_owned(),
            ),
        ];
        if let Some(edits) = checked_edits(source, &info, edits, cancel)? {
            result.push(Extraction {
                kind: VARIABLE_KIND,
                edits,
            });
        }
    }
    if start == assignment.start_byte()
        && end == assignment.end_byte() + 1
        && !lower.contains("extractedroutine")
    {
        let insertion = format!(
            "{block_indent}procedure ExtractedRoutine(var {target}: Integer);{line_ending}{block_indent}begin{line_ending}{block_indent}  {target} := {literal};{line_ending}{block_indent}end;{line_ending}"
        );
        let edits = [
            (block_start, block_start, insertion),
            (
                assignment.start_byte(),
                assignment.end_byte() + 1,
                format!("ExtractedRoutine({target});"),
            ),
        ];
        if let Some(edits) = checked_edits(source, &info, edits, cancel)? {
            result.push(Extraction {
                kind: ROUTINE_KIND,
                edits,
            });
        }
    }
    Ok(result)
}

fn checked_edits<const N: usize>(
    source: &str,
    info: &FileInfo,
    edits: [(usize, usize, String); N],
    cancel: &AtomicBool,
) -> Result<Option<Vec<TextEdit>>, String> {
    let mut updated = source.to_owned();
    let mut output = Vec::with_capacity(N);
    for (start, end, replacement) in edits.into_iter().rev() {
        if cancel.load(Ordering::Relaxed) {
            return Err(CANCELLATION_MESSAGE.to_string());
        }
        let Some(start_position) = text::offset_to_position(source, start) else {
            return Ok(None);
        };
        let Some(end_position) = text::offset_to_position(source, end) else {
            return Ok(None);
        };
        updated.replace_range(start..end, &replacement);
        output.push(TextEdit {
            range: Range::new(start_position, end_position),
            new_text: replacement,
        });
    }
    let (tree, diagnostics) = parser::parse_file(info, updated.as_bytes())?;
    if tree.root_node().has_error() || !diagnostics.is_empty() {
        return Ok(None);
    }
    Ok(Some(output))
}

#[cfg(test)]
mod tests {
    use super::plan;
    use lsp_types::{CodeActionKind, Position, Range};
    use std::sync::atomic::AtomicBool;

    const SOURCE: &str = "unit Sample;\ninterface\nimplementation\nprocedure Run;\nvar Target: Integer;\nbegin\n  Target := 42;\nend;\nend.\n";

    fn selection(start: u32, end: u32) -> Range {
        Range {
            start: Position::new(6, start),
            end: Position::new(6, end),
        }
    }

    #[test]
    fn literal_and_whole_assignment_expose_distinct_proven_extractions() {
        let cancel = AtomicBool::new(false);
        let variable = plan(SOURCE, selection(12, 14), &cancel).unwrap();
        assert_eq!(variable.len(), 1);
        assert_eq!(
            variable[0].kind,
            CodeActionKind::new("refactor.extract.variable")
        );
        assert!(
            variable[0]
                .edits
                .iter()
                .any(|edit| edit.new_text.contains("ExtractedValue := 42;"))
        );

        let routine = plan(SOURCE, selection(2, 15), &cancel).unwrap();
        assert_eq!(routine.len(), 1);
        assert_eq!(
            routine[0].kind,
            CodeActionKind::new("refactor.extract.function")
        );
        assert!(routine[0].edits.iter().any(|edit| {
            edit.new_text
                .contains("procedure ExtractedRoutine(var Target: Integer);")
        }));
    }

    #[test]
    fn partial_tokens_and_side_effecting_or_conditional_expressions_are_withheld() {
        let cancel = AtomicBool::new(false);
        assert!(plan(SOURCE, selection(13, 14), &cancel).unwrap().is_empty());
        assert!(
            plan(
                &SOURCE.replace("42", "Compute()"),
                selection(12, 21),
                &cancel
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            plan(
                &SOURCE.replace("  Target := 42;", "  if True then Target := 42;"),
                selection(25, 27),
                &cancel
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn nonlocal_or_ambiguous_outputs_and_changed_evaluation_contexts_are_withheld() {
        let cancel = AtomicBool::new(false);
        let no_local = SOURCE.replace("var Target: Integer;\n", "");
        assert!(
            plan(&no_local, selection(12, 14), &cancel)
                .unwrap()
                .is_empty()
        );
        let wrong_type = SOURCE.replace("Target: Integer", "Target: Boolean");
        assert!(
            plan(&wrong_type, selection(12, 14), &cancel)
                .unwrap()
                .is_empty()
        );
        let ambiguous = SOURCE.replace(
            "var Target: Integer;",
            "var Target: Integer;\n    Target: Integer;",
        );
        assert!(
            plan(&ambiguous, selection(12, 14), &cancel)
                .unwrap()
                .is_empty()
        );
        let grouped = SOURCE.replace("var Target: Integer;", "var Target, Target: Integer;");
        assert!(
            plan(&grouped, selection(12, 14), &cancel)
                .unwrap()
                .is_empty()
        );
        let conditional = SOURCE.replace("  Target := 42;", "  if True then Target := 42;");
        assert!(
            plan(&conditional, selection(25, 27), &cancel)
                .unwrap()
                .is_empty()
        );
        let directive = SOURCE.replace(
            "var Target: Integer;",
            "var Target: Integer;\n{$IFDEF DEBUG}",
        );
        assert!(
            plan(&directive, selection(12, 14), &cancel)
                .unwrap()
                .is_empty()
        );
        let collision = SOURCE.replace("unit Sample;", "unit ExtractedValue;");
        assert!(
            plan(&collision, selection(12, 14), &cancel)
                .unwrap()
                .is_empty()
        );
        let routine_collision = SOURCE.replace("unit Sample;", "unit ExtractedRoutine;");
        assert!(
            plan(&routine_collision, selection(2, 15), &cancel)
                .unwrap()
                .is_empty()
        );
    }
}
