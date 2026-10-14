use pascal_core::node_kind as K;
use tree_sitter::Node;

/// Source-ordered trivia and non-extra leaves shared by formatter maps.
pub(crate) struct TriviaIndex<'tree> {
    pub(crate) leaves: Vec<Node<'tree>>,
    pub(crate) comments: Vec<(Node<'tree>, String)>,
    pub(crate) directives: Vec<(Node<'tree>, String)>,
}

#[derive(Clone, Copy)]
struct Collect {
    comments: bool,
    directives: bool,
    leaves: bool,
}

impl<'tree> TriviaIndex<'tree> {
    pub(crate) fn build(root: Node<'tree>, source: &[u8]) -> Self {
        #[cfg(test)]
        crate::trivia_test_support::record_leaf_index_build();

        let mut index = Self {
            leaves: Vec::new(),
            comments: Vec::new(),
            directives: Vec::new(),
        };
        Self::collect(
            root,
            source,
            &mut index,
            Collect {
                comments: true,
                directives: true,
                leaves: true,
            },
        );
        index
    }

    fn collect(node: Node<'tree>, source: &[u8], index: &mut Self, collect: Collect) {
        let is_extra = node.is_extra();
        let mut descend = collect;

        if is_extra && collect.comments && node.kind() == K::COMMENT {
            let text = pascal_core::decode_bytes(&source[node.start_byte()..node.end_byte()])
                .replace('\r', "");
            index.comments.push((node, text));
            descend.comments = false;
        }

        if is_extra && collect.directives && node.kind() == K::PP_DIRECTIVE {
            let text = pascal_core::decode_bytes(&source[node.start_byte()..node.end_byte()])
                .replace('\r', "");
            index.directives.push((node, text));
            descend.directives = false;
        }

        if is_extra {
            descend.leaves = false;
        } else if collect.leaves && node.child_count() == 0 {
            index.leaves.push(node);
        }

        if !descend.comments && !descend.directives && !descend.leaves {
            return;
        }

        for child in node.children(&mut node.walk()) {
            Self::collect(child, source, index, descend);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TriviaIndex;
    use std::path::PathBuf;

    #[test]
    fn shared_index_keeps_each_collection_in_source_order_and_strips_cr() {
        let source =
            b"unit Test;\r\n// comment\r\n{$DEFINE X}\r\ninterface\r\nimplementation\r\nend.\r\n";
        let info = pascal_core::FileInfo::new(PathBuf::from("test.pas"));
        let (tree, diagnostics) = pascal_core::parser::parse_file(&info, source).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");

        let index = TriviaIndex::build(tree.root_node(), source);

        assert!(
            index
                .leaves
                .windows(2)
                .all(|pair| pair[0].start_byte() <= pair[1].start_byte())
        );
        assert_eq!(index.comments.len(), 1);
        assert_eq!(index.comments[0].1, "// comment");
        assert_eq!(index.directives.len(), 1);
        assert_eq!(index.directives[0].1, "{$DEFINE X}");
        assert!(index.comments[0].0.start_byte() < index.directives[0].0.start_byte());
    }
}
