use lint4d::dcu::header::parse_unit_header;
use lint4d::dcu::types::parse_dcu;
use lint4d::dcu::{DcuPlatform, DcuVersion, MethodKind, TypeKind};
use std::fs;
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dcu/d13_win64")
        .join(name)
}

#[test]
fn parse_header_reads_unit_name_types() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.Types.dcu")).unwrap();
    let header = parse_unit_header(&data).unwrap();
    assert_eq!(header.version, DcuVersion::D13);
    assert_eq!(header.platform, DcuPlatform::Win64);
    assert_eq!(header.name, "CDAPI.Adapter.Data.Types");
}

#[test]
fn parse_header_reads_unit_name_data() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.dcu")).unwrap();
    let header = parse_unit_header(&data).unwrap();
    assert_eq!(header.version, DcuVersion::D13);
    assert_eq!(header.name, "CDAPI.Adapter.Data");
}

#[test]
fn parse_dcu_extracts_imported_units() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.Types.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "CDAPI.Adapter.Data.Types");
    assert!(
        unit.imported_units.iter().any(|u| u == "System"),
        "Expected 'System' in imports, got: {:?}",
        unit.imported_units
    );
}

#[test]
fn parse_dcu_extracts_version_and_platform() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.Types.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.version, DcuVersion::D13);
    assert_eq!(unit.platform, DcuPlatform::Win64);
}

#[test]
fn parse_dcu_data_unit() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "CDAPI.Adapter.Data");

    let expected = [
        "System",
        "SysInit",
        "System.SysUtils",
        "CDAPI.Adapter.Data.Types",
        "mormot.core.data",
    ];
    for name in &expected {
        assert!(
            unit.imported_units.iter().any(|u| u == name),
            "Expected '{}' in imports, got: {:?}",
            name,
            unit.imported_units
        );
    }

    assert_eq!(
        unit.imported_units.len(),
        19,
        "Expected 19 imports, got: {:?}",
        unit.imported_units
    );
}

#[test]
fn parse_dcu_types_has_expected_imports() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.Types.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();

    // Verify specific imports we know should exist based on the hex dump
    let expected = ["System", "SysInit", "mormot.core.base", "mormot.core.rtti"];
    for name in &expected {
        assert!(
            unit.imported_units.iter().any(|u| u == name),
            "Expected '{}' in imports, got: {:?}",
            name,
            unit.imported_units
        );
    }

    // Verify we got a reasonable number of imports
    assert!(
        unit.imported_units.len() >= 4,
        "Expected at least 4 imports, got {}",
        unit.imported_units.len()
    );
}

#[test]
fn parse_dcu_extracts_type_names() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.Types.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert!(
        !unit.types.is_empty(),
        "Expected at least one type definition, got none"
    );
    for ty in &unit.types {
        assert!(!ty.name.is_empty(), "Found type with empty name");
    }
    // Print what we found for debugging
    for ty in &unit.types {
        eprintln!("  Type: {} ({:?})", ty.name, ty.kind);
    }
}

#[test]
fn parse_dcu_extracts_types_from_data() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert!(
        !unit.types.is_empty(),
        "Expected at least one type definition, got none"
    );
    // Print for debugging
    for ty in &unit.types {
        eprintln!("  Type: {} ({:?})", ty.name, ty.kind);
    }
}

#[test]
fn parse_dcu_extracts_class_fields() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();

    let class_types: Vec<_> = unit.types.iter()
        .filter(|t| t.kind == TypeKind::Class)
        .collect();

    assert!(!class_types.is_empty(), "Expected at least one class type");

    let has_fields = class_types.iter().any(|t| !t.fields.is_empty());
    assert!(has_fields, "Expected at least one class with fields, classes: {:?}",
        class_types.iter().map(|t| (&t.name, t.fields.len())).collect::<Vec<_>>());

    // Print what we found for debugging
    for ty in &class_types {
        eprintln!("  Class: {} -- {} fields, {} methods", ty.name, ty.fields.len(), ty.methods.len());
        for f in &ty.fields {
            eprintln!("    field: {} ({:?})", f.name, f.type_ref);
        }
        for m in &ty.methods {
            eprintln!("    method: {} ({:?})", m.name, m.kind);
        }
    }
}

#[test]
fn parse_dcu_class_has_constructor_or_destructor() {
    let data = fs::read(fixture_path("CDAPI.Adapter.Data.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();

    // TDataAdapter is a class -- it should have a constructor
    let adapter = unit.types.iter()
        .find(|t| t.name == "TDataAdapter")
        .expect("TDataAdapter not found");

    assert_eq!(adapter.kind, TypeKind::Class);

    let has_ctor_or_dtor = adapter.methods.iter()
        .any(|m| m.kind == MethodKind::Constructor || m.kind == MethodKind::Destructor);

    // Print methods for debugging even if assertion passes
    for m in &adapter.methods {
        eprintln!("  TDataAdapter method: {} ({:?})", m.name, m.kind);
    }

    assert!(has_ctor_or_dtor,
        "Expected TDataAdapter to have a constructor or destructor, methods: {:?}",
        adapter.methods.iter().map(|m| (&m.name, &m.kind)).collect::<Vec<_>>());
}

