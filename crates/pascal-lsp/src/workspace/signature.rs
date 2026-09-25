use super::rename::CANCELLATION_MESSAGE;
use crate::text;
use lsp_types::{Range, TextEdit};
use pascal_core::{FileInfo, parser};
use std::sync::atomic::{AtomicBool, Ordering};
use tree_sitter::Node;

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_CALLS: usize = 64;
const MAX_ROUTINE_NAME_BYTES: usize = 64;

pub(crate) fn plan(
    source: &str,
    selection: Range,
    cancel: &AtomicBool,
) -> Result<Option<Vec<TextEdit>>, String> {
    check_cancel(cancel)?;
    if source.len() > MAX_SOURCE_BYTES
        || source.contains(['{', '}'])
        || source.contains("(*")
        || source.contains("//")
    {
        return Ok(None);
    }
    let Some(start) = text::position_to_offset(source, selection.start) else {
        return Ok(None);
    };
    let Some(end) = text::position_to_offset(source, selection.end) else {
        return Ok(None);
    };
    if start >= end {
        return Ok(None);
    }
    let info = FileInfo::new("Signature.pas".into());
    let (tree, diagnostics) = parser::parse_file(&info, source.as_bytes())?;
    if tree.root_node().has_error() || !diagnostics.is_empty() {
        return Ok(None);
    }
    let Some(name_node) = tree
        .root_node()
        .named_descendant_for_byte_range(start, end - 1)
    else {
        return Ok(None);
    };
    if name_node.kind() != "identifier"
        || name_node.start_byte() != start
        || name_node.end_byte() != end
    {
        return Ok(None);
    }
    let name = &source[start..end];
    if name.is_empty()
        || name.len() > MAX_ROUTINE_NAME_BYTES
        || !name.as_bytes()[0].is_ascii_alphabetic()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Ok(None);
    }
    let Some(header) = name_node.parent().filter(|node| node.kind() == "declProc") else {
        return Ok(None);
    };
    if header.child_by_field_name("name") != Some(name_node) {
        return Ok(None);
    }
    let Some(nested) = header.parent().filter(|node| node.kind() == "defProc") else {
        return Ok(None);
    };
    let Some(outer) = nested.parent().filter(|node| node.kind() == "defProc") else {
        return Ok(None);
    };
    let mut local_cursor = outer.walk();
    let locals = outer
        .children_by_field_name("local", &mut local_cursor)
        .collect::<Vec<_>>();
    if locals.len() != 1 || locals[0] != nested {
        return Ok(None);
    }
    let Some(outer_header) = outer.child_by_field_name("header") else {
        return Ok(None);
    };
    let Some(outer_name) = outer_header
        .child_by_field_name("name")
        .filter(|node| node.kind() == "identifier")
    else {
        return Ok(None);
    };
    if !source[outer_header.start_byte()..outer_header.end_byte()].eq_ignore_ascii_case(&format!(
        "procedure {};",
        &source[outer_name.start_byte()..outer_name.end_byte()]
    )) {
        return Ok(None);
    }
    let Some(block) = outer
        .child_by_field_name("body")
        .filter(|node| node.kind() == "block")
    else {
        return Ok(None);
    };
    let Some(args) = header
        .child_by_field_name("args")
        .filter(|node| node.kind() == "declArgs")
    else {
        return Ok(None);
    };
    if !source[header.start_byte()..header.end_byte()].eq_ignore_ascii_case(&format!(
        "procedure {name}{};",
        &source[args.start_byte()..args.end_byte()]
    )) {
        return Ok(None);
    }
    let mut args_cursor = args.walk();
    let declarations = args.named_children(&mut args_cursor).collect::<Vec<_>>();
    if declarations.len() != 2 || declarations.iter().any(|node| node.kind() != "declArg") {
        return Ok(None);
    }
    let Some(first) = plain_integer_argument(declarations[0], source) else {
        return Ok(None);
    };
    let Some(second) = plain_integer_argument(declarations[1], source) else {
        return Ok(None);
    };
    if first.0.eq_ignore_ascii_case(second.0) || first.1 != second.1 {
        return Ok(None);
    }
    let replacement = format!("({}; {})", second.2, first.2);
    let mut raw_edits = vec![(args.start_byte(), args.end_byte(), replacement)];
    let mut body_cursor = block.walk();
    for statement in block.named_children(&mut body_cursor) {
        check_cancel(cancel)?;
        if statement.kind() != "statement" || statement.named_child_count() != 1 {
            continue;
        }
        let Some(call) = statement
            .named_child(0)
            .filter(|node| node.kind() == "exprCall")
        else {
            continue;
        };
        let Some(entity) = call
            .child_by_field_name("entity")
            .filter(|node| node.kind() == "identifier")
        else {
            continue;
        };
        if !source[entity.start_byte()..entity.end_byte()].eq_ignore_ascii_case(name) {
            continue;
        }
        let Some(call_args) = call
            .child_by_field_name("args")
            .filter(|node| node.kind() == "exprArgs")
        else {
            return Ok(None);
        };
        let mut call_cursor = call_args.walk();
        let literals = call_args
            .named_children(&mut call_cursor)
            .collect::<Vec<_>>();
        if literals.len() != 2 || literals.iter().any(|node| node.kind() != "literalNumber") {
            return Ok(None);
        }
        let first_value = &source[literals[0].start_byte()..literals[0].end_byte()];
        let second_value = &source[literals[1].start_byte()..literals[1].end_byte()];
        let call_start = entity.end_byte();
        let call_end = call.end_byte();
        if !plain_i32_literal(first_value)
            || !plain_i32_literal(second_value)
            || source
                .get(call_start..literals[0].start_byte())
                .is_none_or(|prefix| prefix.trim() != "(")
            || source
                .get(literals[0].end_byte()..literals[1].start_byte())
                .is_none_or(|separator| separator.trim() != ",")
            || source
                .get(literals[1].end_byte()..call_end)
                .is_none_or(|suffix| suffix.trim() != ")")
        {
            return Ok(None);
        }
        if raw_edits.len() > MAX_CALLS {
            return Ok(None);
        }
        raw_edits.push((
            call_start,
            call_end,
            format!("({second_value}, {first_value})"),
        ));
    }
    if raw_edits.len() == 1 || count_name_occurrences(source, name, cancel)? != raw_edits.len() {
        return Ok(None);
    }
    let mut updated = source.to_owned();
    let mut edits = Vec::with_capacity(raw_edits.len());
    for (start, end, replacement) in raw_edits.into_iter().rev() {
        check_cancel(cancel)?;
        let Some(start_position) = text::offset_to_position(source, start) else {
            return Ok(None);
        };
        let Some(end_position) = text::offset_to_position(source, end) else {
            return Ok(None);
        };
        updated.replace_range(start..end, &replacement);
        edits.push(TextEdit {
            range: Range::new(start_position, end_position),
            new_text: replacement,
        });
    }
    let (tree, diagnostics) = parser::parse_file(&info, updated.as_bytes())?;
    if tree.root_node().has_error() || !diagnostics.is_empty() {
        return Ok(None);
    }
    Ok(Some(edits))
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLATION_MESSAGE.to_string())
    } else {
        Ok(())
    }
}

fn plain_i32_literal(source: &str) -> bool {
    let digits = source
        .strip_prefix('-')
        .or_else(|| source.strip_prefix('+'))
        .unwrap_or(source);
    !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && source.parse::<i32>().is_ok()
}

fn plain_integer_argument<'a>(
    argument: Node<'_>,
    source: &'a str,
) -> Option<(&'a str, bool, &'a str)> {
    let name_node = argument.child_by_field_name("name")?;
    let ty = argument.child_by_field_name("type")?;
    let name = &source[name_node.start_byte()..name_node.end_byte()];
    if name.is_empty()
        || !name.as_bytes()[0].is_ascii_alphabetic()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || !source[ty.start_byte()..ty.end_byte()].eq_ignore_ascii_case("Integer")
        || !source[argument.start_byte()..name_node.start_byte()]
            .chars()
            .all(char::is_whitespace)
        || source[name_node.end_byte()..ty.start_byte()].trim() != ":"
    {
        return None;
    }
    let original = &source[argument.start_byte()..argument.end_byte()];
    let has_default = if let Some(default) = argument.child_by_field_name("defaultValue") {
        let mut default_cursor = default.walk();
        let literal_node = default
            .named_children(&mut default_cursor)
            .find(|node| node.kind() == "literalNumber")?;
        let literal = &source[literal_node.start_byte()..literal_node.end_byte()];
        if !plain_i32_literal(literal)
            || !source[ty.end_byte()..default.start_byte()]
                .chars()
                .all(char::is_whitespace)
            || source[default.start_byte()..default.end_byte()]
                .trim()
                .strip_prefix('=')
                .is_none_or(|value| value.trim() != literal)
            || !source[default.end_byte()..argument.end_byte()]
                .chars()
                .all(char::is_whitespace)
        {
            return None;
        }
        true
    } else {
        if !source[ty.end_byte()..argument.end_byte()]
            .chars()
            .all(char::is_whitespace)
        {
            return None;
        }
        false
    };
    Some((name, has_default, original))
}

fn count_name_occurrences(source: &str, name: &str, cancel: &AtomicBool) -> Result<usize, String> {
    let mut count = 0;
    let bytes = source.as_bytes();
    for (offset, window) in bytes.windows(name.len()).enumerate() {
        if offset % 1024 == 0 {
            check_cancel(cancel)?;
        }
        if !window.eq_ignore_ascii_case(name.as_bytes()) {
            continue;
        }
        let identifier_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
        if offset > 0 && identifier_byte(bytes[offset - 1]) {
            continue;
        }
        if bytes
            .get(offset + name.len())
            .is_some_and(|byte| identifier_byte(*byte))
        {
            continue;
        }
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::plan;
    use lsp_types::{Position, Range};
    use std::sync::atomic::AtomicBool;

    const SOURCE: &str = "unit Sample;\ninterface\nimplementation\nprocedure Run;\n  procedure Pair(A: Integer; B: Integer);\n  begin\n  end;\nbegin\n  Pair(1, 2);\n  Pair(3, 4);\nend;\nend.\n";

    fn selected_name() -> Range {
        Range::new(Position::new(4, 12), Position::new(4, 16))
    }

    #[test]
    fn swaps_private_signature_and_every_pure_call_argument() {
        let cancel = AtomicBool::new(false);
        let edits = plan(SOURCE, selected_name(), &cancel)
            .unwrap()
            .expect("one complete plan");
        assert_eq!(edits.len(), 3);
        assert!(
            edits
                .iter()
                .any(|edit| edit.new_text == "(B: Integer; A: Integer)")
        );
        assert!(edits.iter().any(|edit| edit.new_text == "(2, 1)"));
        assert!(edits.iter().any(|edit| edit.new_text == "(4, 3)"));
    }

    #[test]
    fn swaps_tight_literal_calls_without_moving_evaluation() {
        let cancel = AtomicBool::new(false);
        let compact = SOURCE.replace("Pair(1, 2)", "Pair(1,2)");
        let edits = plan(&compact, selected_name(), &cancel)
            .unwrap()
            .expect("compact two-literal call");
        assert!(edits.iter().any(|edit| edit.new_text == "(2, 1)"));
    }

    #[test]
    fn preserves_parameter_type_case_and_spacing_while_swapping() {
        let cancel = AtomicBool::new(false);
        let changed = SOURCE.replace("A: Integer; B: Integer", "A: integer ; B : INTEGER");
        let edits = plan(&changed, selected_name(), &cancel)
            .unwrap()
            .expect("ordinary spaced and cased parameter list");
        assert!(
            edits
                .iter()
                .any(|edit| edit.new_text.contains("B : INTEGER; A: integer"))
        );
    }

    #[test]
    fn swaps_signed_integer_literals_in_calls_and_defaults() {
        let cancel = AtomicBool::new(false);
        let changed = SOURCE
            .replace("A: Integer; B: Integer", "A: Integer = -7; B: Integer = 8")
            .replace("Pair(1, 2)", "Pair(-1, 2)");
        let edits = plan(&changed, selected_name(), &cancel)
            .unwrap()
            .expect("negative integer literals do not change evaluation order");
        assert!(edits.iter().any(|edit| edit.new_text == "(2, -1)"));
        assert!(
            edits
                .iter()
                .any(|edit| edit.new_text == "(B: Integer = 8; A: Integer = -7)")
        );
    }

    #[test]
    fn preserves_literal_defaults_and_refuses_partial_or_impure_calls() {
        let cancel = AtomicBool::new(false);
        let defaults = SOURCE.replace("A: Integer; B: Integer", "A: Integer = 7; B: Integer = 8");
        let edits = plan(&defaults, selected_name(), &cancel)
            .unwrap()
            .expect("defaulted full calls");
        assert!(
            edits
                .iter()
                .any(|edit| edit.new_text == "(B: Integer = 8; A: Integer = 7)")
        );

        let omitted = defaults.replace("Pair(3, 4)", "Pair(3)");
        assert!(plan(&omitted, selected_name(), &cancel).unwrap().is_none());
        let impure = SOURCE.replace("Pair(3, 4)", "Pair(Compute(), 4)");
        assert!(plan(&impure, selected_name(), &cancel).unwrap().is_none());
        let conditional = SOURCE.replace("  Pair(3, 4);", "  if True then Pair(3, 4);");
        assert!(
            plan(&conditional, selected_name(), &cancel)
                .unwrap()
                .is_none()
        );
        let overloaded = SOURCE.replace(
            "  begin\n  end;\nbegin",
            "  begin\n  end;\n  procedure Pair(C: Integer; D: Integer);\n  begin\n  end;\nbegin",
        );
        assert!(
            plan(&overloaded, selected_name(), &cancel)
                .unwrap()
                .is_none()
        );
        let by_reference = SOURCE.replace("A: Integer; B: Integer", "var A: Integer; B: Integer");
        assert!(
            plan(&by_reference, selected_name(), &cancel)
                .unwrap()
                .is_none()
        );
        let grouped = SOURCE.replace("A: Integer; B: Integer", "A, C: Integer; B: Integer");
        assert!(plan(&grouped, selected_name(), &cancel).unwrap().is_none());
        let outside = SOURCE.replace("end.\n", "procedure Other; begin Pair(5, 6); end;\nend.\n");
        assert!(plan(&outside, selected_name(), &cancel).unwrap().is_none());
        assert!(
            plan(
                SOURCE,
                Range::new(Position::new(8, 2), Position::new(8, 6)),
                &cancel,
            )
            .unwrap()
            .is_none()
        );
    }
}
