use lint4d::dcu::{DcuUnit, DcuVersion, DcuPlatform, TypeInfo, TypeKind, TypeRef, MethodInfo, MethodKind, FieldInfo, Visibility};
use lint4d::dcu::ProjectContext;

fn make_test_unit() -> DcuUnit {
    DcuUnit {
        name: "TestUnit".to_string(),
        version: DcuVersion::D13,
        platform: DcuPlatform::Win64,
        imported_units: vec![],
        types: vec![
            TypeInfo {
                name: "TMyClass".to_string(),
                kind: TypeKind::Class,
                parent: None,
                fields: vec![],
                methods: vec![
                    MethodInfo {
                        name: "Create".to_string(),
                        kind: MethodKind::Constructor,
                        params: vec![],
                        return_type: None,
                    },
                ],
                interface_guid: None,
            },
            TypeInfo {
                name: "IMyInterface".to_string(),
                kind: TypeKind::Interface,
                parent: None,
                fields: vec![],
                methods: vec![],
                interface_guid: None,
            },
        ],
    }
}

#[test]
fn project_context_resolves_class_type() {
    let unit = make_test_unit();
    let ctx = ProjectContext::from_units(vec![unit]);
    let uses = vec!["TestUnit".to_string()];
    assert_eq!(ctx.is_class_type("TMyClass", &uses), Some(true));
    assert_eq!(ctx.is_class_type("IMyInterface", &uses), Some(false));
}

#[test]
fn project_context_resolves_interface_type() {
    let unit = make_test_unit();
    let ctx = ProjectContext::from_units(vec![unit]);
    let uses = vec!["TestUnit".to_string()];
    assert_eq!(ctx.is_interface_type("IMyInterface", &uses), Some(true));
    assert_eq!(ctx.is_interface_type("TMyClass", &uses), Some(false));
}

#[test]
fn project_context_returns_none_for_unknown_type() {
    let unit = make_test_unit();
    let ctx = ProjectContext::from_units(vec![unit]);
    let uses = vec!["TestUnit".to_string()];
    assert_eq!(ctx.is_class_type("TUnknown", &uses), None);
}

#[test]
fn project_context_get_constructor() {
    let unit = make_test_unit();
    let ctx = ProjectContext::from_units(vec![unit]);
    let uses = vec!["TestUnit".to_string()];
    let ctor = ctx.get_constructor("TMyClass", &uses);
    assert!(ctor.is_some());
    assert_eq!(ctor.unwrap().kind, MethodKind::Constructor);
}

#[test]
fn project_context_from_dcu_paths() {
    use std::path::PathBuf;
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dcu/d13_win64/Win64/Debug");
    let ctx = ProjectContext::from_dcu_paths(&[path]).unwrap();
    assert!(ctx.unit_count() >= 7, "Expected at least 7 units, got {}", ctx.unit_count());
}
