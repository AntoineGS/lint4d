use std::collections::HashSet;

use tree_sitter::{Node, Tree};

use crate::constructs::node_text;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Detect factory functions across a set of pre-parsed Pascal files.
///
/// A *factory function* is a standalone (non-method) function whose body
/// ultimately returns a freshly-constructed object, either directly via
/// `Result := TFoo.Create(...)` or indirectly via another factory function.
///
/// Each entry in `files` is `(unit_name_hint, tree, source_bytes)`.
/// `unit_name_hint` is used as a fallback if the unit name cannot be extracted
/// from the AST.
///
/// Returns a set of `(unit_name_lowercase, function_name_lowercase)` pairs.
pub fn detect_factories(files: &[(&str, &Tree, &[u8])]) -> HashSet<(String, String)> {
    let mut all_metas: Vec<FuncMeta> = Vec::new();

    // Phase 1: extract metadata from every file.
    for &(hint, tree, source) in files {
        let root = tree.root_node();
        let unit_name = extract_unit_name(root, source)
            .unwrap_or_else(|| hint.to_string())
            .to_lowercase();
        let uses = extract_uses_clauses(root, source);

        collect_func_metas(root, source, &unit_name, &uses, &mut all_metas);
    }

    // Phase 2: seed factories from direct constructors.
    let mut factory_set: HashSet<(String, String)> = HashSet::new();
    for meta in &all_metas {
        if meta.has_direct_constructor {
            factory_set.insert((meta.unit.clone(), meta.name.clone()));
        }
    }

    // Phase 3: fixed-point expansion for indirect factories (max 20 rounds).
    for _round in 0..20 {
        let mut changed = false;
        for meta in &all_metas {
            let key = (meta.unit.clone(), meta.name.clone());
            if factory_set.contains(&key) {
                continue;
            }
            for callee in &meta.result_callees {
                // Check the function's own unit first.
                if factory_set.contains(&(meta.unit.clone(), callee.clone())) {
                    factory_set.insert(key.clone());
                    changed = true;
                    break;
                }
                // Then check uses clauses in reverse order (Delphi semantics).
                let mut found = false;
                for used_unit in meta.uses.iter().rev() {
                    if factory_set.contains(&(used_unit.to_lowercase(), callee.clone())) {
                        factory_set.insert(key.clone());
                        changed = true;
                        found = true;
                        break;
                    }
                }
                if found {
                    break;
                }
            }
        }
        if !changed {
            break;
        }
    }

    factory_set
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

/// Metadata extracted from a single function/procedure definition.
struct FuncMeta {
    /// Lower-cased unit name where the function lives.
    unit: String,
    /// Lower-cased unqualified function name.
    name: String,
    /// Units referenced via `uses` clauses (original casing stored; compared
    /// with `.to_lowercase()` during resolution).
    uses: Vec<String>,
    /// `true` when the body contains `Result := TFoo.Create(...)`.
    has_direct_constructor: bool,
    /// Lower-cased names of functions assigned to `Result` via
    /// `Result := SomeFunc(...)` (candidate indirect factories).
    result_callees: Vec<String>,
}

// ---------------------------------------------------------------------------
// AST helpers — unit/uses extraction
// ---------------------------------------------------------------------------

fn extract_unit_name(root: Node, source: &[u8]) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "unit" | "program" | "library" => {
                let mut inner = child.walk();
                for grandchild in child.children(&mut inner) {
                    if grandchild.kind() == "moduleName" {
                        return Some(node_text(grandchild, source));
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_uses_clauses(root: Node, source: &[u8]) -> Vec<String> {
    let mut units = Vec::new();
    collect_uses_recursive(root, source, &mut units);
    units
}

fn collect_uses_recursive(node: Node, source: &[u8], units: &mut Vec<String>) {
    if node.kind() == "declUses" {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "moduleName" {
                units.push(node_text(child, source));
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_uses_recursive(child, source, units);
    }
}

// ---------------------------------------------------------------------------
// AST helpers — function metadata extraction
// ---------------------------------------------------------------------------

fn collect_func_metas(
    node: Node,
    source: &[u8],
    unit_name: &str,
    uses: &[String],
    out: &mut Vec<FuncMeta>,
) {
    if node.kind() == "defProc" {
        if let Some(meta) = extract_func_meta(node, source, unit_name, uses) {
            out.push(meta);
        }
        // Recurse into nested procedures inside this defProc.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            collect_func_metas(child, source, unit_name, uses, out);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_func_metas(child, source, unit_name, uses, out);
    }
}

fn extract_func_meta(
    def_proc: Node,
    source: &[u8],
    unit_name: &str,
    uses: &[String],
) -> Option<FuncMeta> {
    let proc_name = extract_proc_name(def_proc, source)?;

    // Only standalone functions are eligible factories; skip class methods.
    if proc_name.contains('.') {
        return None;
    }

    let block = def_proc
        .children(&mut def_proc.walk())
        .find(|c| c.kind() == "block")?;

    let mut has_direct_constructor = false;
    let mut result_callees: Vec<String> = Vec::new();

    scan_result_assignments(
        block,
        source,
        &mut has_direct_constructor,
        &mut result_callees,
    );

    Some(FuncMeta {
        unit: unit_name.to_string(),
        name: proc_name.to_lowercase(),
        uses: uses.to_vec(),
        has_direct_constructor,
        result_callees,
    })
}

/// Extract the unqualified or qualified name from a `defProc` node.
///
/// Returns e.g. `"Bar"` for standalone `Bar` and `"TFoo.Bar"` for methods.
fn extract_proc_name(def_proc: Node, source: &[u8]) -> Option<String> {
    let decl_proc = def_proc
        .children(&mut def_proc.walk())
        .find(|c| c.kind() == "declProc")?;

    // Try the `name` field first.
    if let Some(name_node) = decl_proc.child_by_field_name("name") {
        return Some(node_text(name_node, source));
    }

    // Fall back to looking for genericDot / exprDot (qualified name).
    let mut cursor = decl_proc.walk();
    for child in decl_proc.children(&mut cursor) {
        match child.kind() {
            "genericDot" | "exprDot" => {
                let idents: Vec<Node> = child
                    .children(&mut child.walk())
                    .filter(|c| c.kind() == "identifier")
                    .collect();
                if idents.len() >= 2 {
                    return Some(format!(
                        "{}.{}",
                        node_text(idents[0], source),
                        node_text(idents[1], source)
                    ));
                }
                if let Some(last) = idents.last() {
                    return Some(node_text(*last, source));
                }
            }
            "identifier" => {
                return Some(node_text(child, source));
            }
            _ => {}
        }
    }

    None
}

/// Recursively scan `node` for `Result := ...` assignment patterns.
fn scan_result_assignments(
    node: Node,
    source: &[u8],
    has_direct_constructor: &mut bool,
    result_callees: &mut Vec<String>,
) {
    if node.kind() == "assignment" {
        if let Some(lhs) = node.child_by_field_name("lhs") {
            let lhs_text = node_text(lhs, source);
            if lhs_text.eq_ignore_ascii_case("result") {
                if let Some(rhs) = node.child_by_field_name("rhs") {
                    if is_constructor_call(rhs, source) {
                        *has_direct_constructor = true;
                    } else if let Some(callee) = extract_callee_name(rhs, source) {
                        result_callees.push(callee.to_lowercase());
                    }
                }
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        scan_result_assignments(child, source, has_direct_constructor, result_callees);
    }
}

/// Return `true` when `node` represents a constructor call (`TFoo.Create` or
/// `TFoo.Create(...)`).
fn is_constructor_call(node: Node, source: &[u8]) -> bool {
    match node.kind() {
        "exprDot" => {
            let rhs = match node.child_by_field_name("rhs") {
                Some(r) => r,
                None => return false,
            };
            node_text(rhs, source).eq_ignore_ascii_case("create")
        }
        "exprCall" => {
            let entity = match node.child_by_field_name("entity") {
                Some(e) => e,
                None => return false,
            };
            is_constructor_call(entity, source)
        }
        _ => false,
    }
}

/// Extract the simple function/callee name from a RHS expression.
fn extract_callee_name(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => Some(node_text(node, source)),
        "exprCall" => {
            let entity = node.child_by_field_name("entity")?;
            extract_callee_name(entity, source)
        }
        "exprDot" => {
            let lhs = node.child_by_field_name("lhs")?;
            let rhs = node.child_by_field_name("rhs")?;
            if lhs.kind() == "identifier" && rhs.kind() == "identifier" {
                // e.g. `Unit.Func` — return just the function name for resolution.
                Some(node_text(rhs, source))
            } else {
                None
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(source: &[u8]) -> Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_pascal::LANGUAGE.into())
            .expect("failed to set Pascal language");
        parser.parse(source, None).expect("parse failed")
    }

    #[test]
    fn direct_constructor_is_factory() {
        let source = br#"
unit MyUnit;

interface
function MakeObj: TObject;

implementation

function MakeObj: TObject;
begin
  Result := TObject.Create;
end;

end.
"#;
        let tree = parse(source);
        let factories = detect_factories(&[("myunit", &tree, source)]);
        assert!(
            factories.contains(&("myunit".to_string(), "makeobj".to_string())),
            "MakeObj should be detected as a factory; got: {:?}",
            factories
        );
    }

    #[test]
    fn non_factory_not_detected() {
        let source = br#"
unit MyUnit;

interface
function Add(A, B: Integer): Integer;

implementation

function Add(A, B: Integer): Integer;
begin
  Result := A + B;
end;

end.
"#;
        let tree = parse(source);
        let factories = detect_factories(&[("myunit", &tree, source)]);
        assert!(
            !factories.contains(&("myunit".to_string(), "add".to_string())),
            "Add should NOT be a factory"
        );
    }

    #[test]
    fn indirect_factory_via_callee() {
        let source = br#"
unit MyUnit;

interface
function MakeBase: TObject;
function MakeWrapped: TObject;

implementation

function MakeBase: TObject;
begin
  Result := TObject.Create;
end;

function MakeWrapped: TObject;
begin
  Result := MakeBase;
end;

end.
"#;
        let tree = parse(source);
        let factories = detect_factories(&[("myunit", &tree, source)]);
        assert!(
            factories.contains(&("myunit".to_string(), "makebase".to_string())),
            "MakeBase should be a factory"
        );
        assert!(
            factories.contains(&("myunit".to_string(), "makewrapped".to_string())),
            "MakeWrapped should be an indirect factory; got: {:?}",
            factories
        );
    }

    #[test]
    fn class_method_not_factory() {
        // TMyClass.Create is a constructor method — the defProc has a qualified
        // name and must not be registered as a factory.
        let source = br#"
unit MyUnit;

interface
type TMyClass = class
  constructor Create;
end;

implementation

constructor TMyClass.Create;
begin
  inherited Create;
  Result := TObject.Create;
end;

end.
"#;
        let tree = parse(source);
        let factories = detect_factories(&[("myunit", &tree, source)]);
        // The qualified name "tmyclass.create" should NOT appear as a factory.
        let has_method = factories
            .iter()
            .any(|(_, name)| name.contains('.'));
        assert!(
            !has_method,
            "class methods should not be registered as factories; got: {:?}",
            factories
        );
    }
}
