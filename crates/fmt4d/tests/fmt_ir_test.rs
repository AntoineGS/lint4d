mod common;
use common::format_source;

#[test]
fn ir_simple_unit() {
    let result = format_source("unit Test;\ninterface\nimplementation\nend.\n");
    assert!(!result.is_empty());
}
