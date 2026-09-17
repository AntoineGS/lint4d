use super::{Document, NavigationIndex, Span};
use crate::text::{self, PositionIndex};
use lsp_types::{Position, Range, SelectionRange, Url};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_SELECTION_POSITIONS: usize = 256;
const MAX_SELECTION_TRAVERSAL_NODES: usize = 100_000;
const MAX_SELECTION_DEPTH: usize = 128;
const CANCELLATION_MESSAGE: &str = "request cancelled";

#[derive(Debug, Clone)]
struct SelectionTree {
    nodes: Vec<SelectionNode>,
    root: usize,
}

#[derive(Debug, Clone)]
struct SelectionNode {
    span: Span,
    parent: Option<usize>,
    children: Vec<usize>,
    depth: usize,
}

pub(super) fn selection_ranges(
    index: &NavigationIndex,
    uri: &Url,
    positions: &[Position],
) -> Result<Vec<SelectionRange>, String> {
    let cancel = AtomicBool::new(false);
    selection_ranges_with_cancel(index, uri, positions, &cancel)
}

pub(super) fn selection_ranges_with_cancel(
    index: &NavigationIndex,
    uri: &Url,
    positions: &[Position],
    cancel: &AtomicBool,
) -> Result<Vec<SelectionRange>, String> {
    check_cancel(cancel)?;
    if positions.len() > MAX_SELECTION_POSITIONS {
        return Err(format!(
            "selection range request contains more than {MAX_SELECTION_POSITIONS} positions"
        ));
    }
    let document = index
        .documents
        .get(uri)
        .ok_or_else(|| format!("document is not indexed: {uri}"))?;
    if positions.is_empty() {
        return Ok(Vec::new());
    }

    let selection_tree = SelectionTree::build(document, cancel)?;
    let position_index = PositionIndex::new_with_cancel(&document.source, cancel)
        .map_err(|()| CANCELLATION_MESSAGE.to_string())?;
    let mut result = Vec::with_capacity(positions.len());
    for position in positions {
        check_cancel(cancel)?;
        let offset = text::position_to_offset(&document.source, *position).ok_or_else(|| {
            format!(
                "selection position ({}, {}) is not a valid UTF-16 source boundary",
                position.line, position.character
            )
        })?;
        result.push(selection_for_offset(
            &selection_tree,
            document,
            &position_index,
            offset,
            cancel,
        )?);
    }
    Ok(result)
}

impl SelectionTree {
    fn build(document: &Document, cancel: &AtomicBool) -> Result<Self, String> {
        let root = document.tree.root_node();
        let mut nodes = Vec::new();
        let mut pending = vec![(root, None, 0_usize)];
        let mut visited = 0_usize;

        while let Some((node, parent, depth)) = pending.pop() {
            check_cancel(cancel)?;
            visited = visited.saturating_add(1);
            if visited > MAX_SELECTION_TRAVERSAL_NODES {
                return Err(format!(
                    "selection syntax traversal exceeds the {MAX_SELECTION_TRAVERSAL_NODES}-node limit"
                ));
            }
            if depth > MAX_SELECTION_DEPTH {
                return Err(format!(
                    "selection syntax hierarchy exceeds the {MAX_SELECTION_DEPTH}-level depth limit"
                ));
            }

            let span = Span::from_node(node);
            let candidate = node == root || node.is_named() || node.is_error();
            let current = if candidate && (span.start < span.end || node == root) {
                let index = nodes.len();
                nodes.push(SelectionNode {
                    span,
                    parent,
                    children: Vec::new(),
                    depth,
                });
                if let Some(parent) = parent {
                    nodes[parent].children.push(index);
                }
                Some(index)
            } else {
                parent
            };

            let mut cursor = node.walk();
            let children = node.named_children(&mut cursor).collect::<Vec<_>>();
            pending.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| (child, current, depth.saturating_add(1))),
            );
        }

        let root = nodes
            .iter()
            .position(|node| node.parent.is_none())
            .ok_or_else(|| "selection syntax tree has no root node".to_string())?;
        Ok(Self { nodes, root })
    }

    fn deepest_containing(
        &self,
        offset: usize,
        work: &mut usize,
        cancel: &AtomicBool,
    ) -> Result<usize, String> {
        let mut current = self.root;
        loop {
            check_cancel(cancel)?;
            let mut selected: Option<usize> = None;
            for child in &self.nodes[current].children {
                *work = work.saturating_add(1);
                if *work > MAX_SELECTION_TRAVERSAL_NODES {
                    return Err(format!(
                        "selection syntax query exceeds the {MAX_SELECTION_TRAVERSAL_NODES}-node work limit"
                    ));
                }
                let candidate = &self.nodes[*child];
                if !candidate.span.contains_offset(offset) {
                    continue;
                }
                let replace = selected.is_none_or(|previous| {
                    let previous = &self.nodes[previous];
                    candidate.span.end.saturating_sub(candidate.span.start)
                        < previous.span.end.saturating_sub(previous.span.start)
                        || (candidate.span.end.saturating_sub(candidate.span.start)
                            == previous.span.end.saturating_sub(previous.span.start)
                            && candidate.depth > previous.depth)
                });
                if replace {
                    selected = Some(*child);
                }
            }
            let Some(child) = selected else {
                return Ok(current);
            };
            current = child;
        }
    }
}

fn selection_for_offset(
    tree: &SelectionTree,
    document: &Document,
    positions: &PositionIndex,
    offset: usize,
    cancel: &AtomicBool,
) -> Result<SelectionRange, String> {
    let mut work = 0_usize;
    let leaf = tree.deepest_containing(offset, &mut work, cancel)?;
    let mut spans = Vec::new();
    let mut current = Some(leaf);
    while let Some(index) = current {
        check_cancel(cancel)?;
        work = work.saturating_add(1);
        if work > MAX_SELECTION_TRAVERSAL_NODES {
            return Err(format!(
                "selection syntax query exceeds the {MAX_SELECTION_TRAVERSAL_NODES}-node work limit"
            ));
        }
        let span = tree.nodes[index].span;
        if safe_span(document, span)
            && spans
                .last()
                .is_none_or(|previous: &Span| *previous != span && span.contains(*previous))
        {
            spans.push(span);
        }
        current = tree.nodes[index].parent;
    }

    let root_span = tree.nodes[tree.root].span;
    if spans.last().copied() != Some(root_span)
        && spans
            .last()
            .is_none_or(|previous| root_span.contains(*previous) && root_span != *previous)
    {
        spans.push(root_span);
    }
    if spans.is_empty() {
        return Err("selection syntax tree produced no valid source range".to_string());
    }

    let mut parent = None;
    for span in spans.into_iter().rev() {
        let range = range_for_span(positions, &document.source, span)?;
        parent = Some(Box::new(SelectionRange { range, parent }));
    }
    parent
        .map(|selection| *selection)
        .ok_or_else(|| "selection range chain is empty".to_string())
}

fn safe_span(document: &Document, span: Span) -> bool {
    span.start <= span.end
        && span.end <= document.source.len()
        && document.source.is_char_boundary(span.start)
        && document.source.is_char_boundary(span.end)
}

fn range_for_span(positions: &PositionIndex, source: &str, span: Span) -> Result<Range, String> {
    if span.start > span.end {
        return Err("selection span has an inverted range".to_string());
    }
    let start = positions
        .offset_to_position(source, span.start)
        .ok_or_else(|| "selection start is not a valid UTF-8/UTF-16 boundary".to_string())?;
    let end = positions
        .offset_to_position(source, span.end)
        .ok_or_else(|| "selection end is not a valid UTF-8/UTF-16 boundary".to_string())?;
    Ok(Range { start, end })
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err(CANCELLATION_MESSAGE.to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_SELECTION_POSITIONS, selection_ranges_with_cancel};
    use crate::NavigationIndex;
    use crate::text::offset_to_position;
    use lsp_types::Position;
    use std::sync::atomic::AtomicBool;

    fn indexed(source: &str) -> (NavigationIndex, lsp_types::Url) {
        let uri = lsp_types::Url::parse("file:///selection-test.pas").expect("selection URI");
        let mut index = NavigationIndex::new();
        index
            .update(uri.clone(), source.to_string())
            .expect("selection source parses");
        (index, uri)
    }

    #[test]
    fn selection_ranges_use_utf16_positions_across_crlf_and_non_bmp_text() {
        let source = "unit 😀Selection;\r\ninterface\r\nimplementation\r\nprocedure Run;\r\nbegin\r\n  Value := 1;\r\nend.\r\n";
        let (index, uri) = indexed(source);
        let value = source.find("Value").expect("value identifier");
        let position = offset_to_position(source, value).expect("value position");
        let cancel = AtomicBool::new(false);

        let ranges = selection_ranges_with_cancel(&index, &uri, &[position], &cancel)
            .expect("selection ranges");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].range.start, position);
        assert_eq!(
            ranges[0].range.end,
            Position::new(position.line, position.character + 5)
        );
        assert_eq!(
            ranges
                .last()
                .expect("innermost range")
                .parent
                .as_ref()
                .expect("structural parent")
                .range
                .start,
            Position::new(5, 2)
        );
    }

    #[test]
    fn selection_ranges_return_an_empty_document_range_for_an_empty_source() {
        let (index, uri) = indexed("");
        let cancel = AtomicBool::new(false);
        let ranges = selection_ranges_with_cancel(&index, &uri, &[Position::new(0, 0)], &cancel)
            .expect("empty source selection range");

        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].range.start, Position::new(0, 0));
        assert_eq!(ranges[0].range.end, Position::new(0, 0));
        assert!(ranges[0].parent.is_none());
    }

    #[test]
    fn selection_ranges_reject_utf16_surrogate_boundaries() {
        let source = "unit 😀Selection;\n";
        let (index, uri) = indexed(source);
        let cancel = AtomicBool::new(false);
        let error = selection_ranges_with_cancel(&index, &uri, &[Position::new(0, 6)], &cancel)
            .expect_err("the middle of a surrogate pair must be rejected");

        assert!(error.contains("valid UTF-16 source boundary"), "{error}");
    }

    #[test]
    fn selection_ranges_reject_oversized_batches_before_tree_work() {
        let (index, uri) = indexed("unit Selection;\n");
        let positions = vec![Position::new(0, 0); MAX_SELECTION_POSITIONS + 1];
        let cancel = AtomicBool::new(false);
        let error = selection_ranges_with_cancel(&index, &uri, &positions, &cancel)
            .expect_err("oversized selection request");

        assert!(error.contains("more than 256 positions"), "{error}");
    }

    #[test]
    fn selection_ranges_honor_cancellation_before_source_traversal() {
        let (index, uri) = indexed("unit Selection;\n");
        let cancel = AtomicBool::new(true);
        let error = selection_ranges_with_cancel(&index, &uri, &[Position::new(0, 0)], &cancel)
            .expect_err("cancelled selection request");

        assert_eq!(error, "request cancelled");
    }

    #[test]
    fn selection_ranges_reject_excessive_syntax_depth() {
        let mut source = String::from(
            "unit SelectionDepth;\ninterface\nimplementation\nprocedure Run;\nbegin\n",
        );
        for _ in 0..160 {
            source.push_str("if True then begin\n");
        }
        for _ in 0..160 {
            source.push_str("end;\n");
        }
        source.push_str("end;\nend.\n");
        let (index, uri) = indexed(&source);
        let position = offset_to_position(&source, source.find("True").expect("nested condition"))
            .expect("nested condition position");
        let cancel = AtomicBool::new(false);

        let error = selection_ranges_with_cancel(&index, &uri, &[position], &cancel)
            .expect_err("excessive selection depth must fail closed");
        assert!(error.contains("selection syntax hierarchy"), "{error}");
    }
}
