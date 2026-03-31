use std::fs;
use std::path::PathBuf;

fn print_tree(node: tree_sitter::Node, depth: usize, source: &[u8]) {
    let indent = "  ".repeat(depth);
    let start = node.start_byte();
    let end = (node.end_byte()).min(start + 50);
    let text = std::str::from_utf8(&source[start..end]).unwrap_or("?");
    let text = text.replace('\n', "\n");
    println!("{}[{}] {}: '{}'", indent, node.id(), node.kind(), text);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_extra() {
            print_tree(child, depth + 1, source);
        }
    }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/test_full.pas".to_string());
    let source = fs::read(&path).expect("read failed");
    let info = pascal_core::FileInfo::new(PathBuf::from(&path));
    let (tree, diags) = pascal_core::parser::parse_file(&info, &source).expect("parse failed");

    println!("Diagnostics: {}", diags.len());
    for d in &diags {
        println!("  {}: line {}: {}", d.rule_id, d.line, d.message);
    }

    println!("\nAST:");
    print_tree(tree.root_node(), 0, &source);
}
