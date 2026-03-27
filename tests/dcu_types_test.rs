use lint4d::dcu::header::parse_unit_header;
use lint4d::dcu::types::parse_dcu;
use lint4d::dcu::{DcuPlatform, DcuVersion, MethodKind, TypeKind};
use std::fs;
use std::path::PathBuf;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dcu/d13_win64/Win64/Debug")
        .join(name)
}

fn d2010_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dcu/d2010_win32/Win32/Debug")
        .join(name)
}

fn xe3_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dcu/xe3_win32/Win32/Debug")
        .join(name)
}

#[test]
fn parse_header_reads_unit_name_interfaces() {
    let data = fs::read(fixture_path("Lint4dFixture.Interfaces.dcu")).unwrap();
    let header = parse_unit_header(&data).unwrap();
    assert_eq!(header.version, DcuVersion::D13);
    assert_eq!(header.platform, DcuPlatform::Win64);
    assert_eq!(header.name, "Lint4dFixture.Interfaces");
}

#[test]
fn parse_header_reads_unit_name_classes() {
    let data = fs::read(fixture_path("Lint4dFixture.Classes.dcu")).unwrap();
    let header = parse_unit_header(&data).unwrap();
    assert_eq!(header.version, DcuVersion::D13);
    assert_eq!(header.name, "Lint4dFixture.Classes");
}

#[test]
fn parse_dcu_extracts_imported_units() {
    let data = fs::read(fixture_path("Lint4dFixture.Interfaces.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Interfaces");
    assert!(
        unit.imported_units.iter().any(|u| u == "System"),
        "Expected 'System' in imports, got: {:?}",
        unit.imported_units
    );
}

#[test]
fn parse_dcu_extracts_version_and_platform() {
    let data = fs::read(fixture_path("Lint4dFixture.Interfaces.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.version, DcuVersion::D13);
    assert_eq!(unit.platform, DcuPlatform::Win64);
}

#[test]
fn parse_dcu_classes_unit() {
    let data = fs::read(fixture_path("Lint4dFixture.Classes.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Classes");

    let expected = ["System", "SysInit", "System.SysUtils", "System.Classes"];
    for name in &expected {
        assert!(
            unit.imported_units.iter().any(|u| u == name),
            "Expected '{}' in imports, got: {:?}",
            name,
            unit.imported_units
        );
    }

    assert!(
        unit.imported_units.len() >= 4,
        "Expected at least 4 imports, got: {:?}",
        unit.imported_units
    );
}

#[test]
fn parse_dcu_interfaces_has_expected_imports() {
    let data = fs::read(fixture_path("Lint4dFixture.Interfaces.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();

    let expected = ["System", "SysInit", "Lint4dFixture.Classes"];
    for name in &expected {
        assert!(
            unit.imported_units.iter().any(|u| u == name),
            "Expected '{}' in imports, got: {:?}",
            name,
            unit.imported_units
        );
    }

    assert!(
        unit.imported_units.len() >= 3,
        "Expected at least 3 imports, got {}",
        unit.imported_units.len()
    );
}

#[test]
fn parse_dcu_extracts_type_names() {
    let data = fs::read(fixture_path("Lint4dFixture.Interfaces.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert!(
        !unit.types.is_empty(),
        "Expected at least one type definition, got none"
    );
    for ty in &unit.types {
        assert!(!ty.name.is_empty(), "Found type with empty name");
    }
}

#[test]
fn parse_dcu_extracts_types_from_classes() {
    let data = fs::read(fixture_path("Lint4dFixture.Classes.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert!(
        !unit.types.is_empty(),
        "Expected at least one type definition, got none"
    );
}

#[test]
fn parse_dcu_extracts_class_fields() {
    let data = fs::read(fixture_path("Lint4dFixture.Classes.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();

    let class_types: Vec<_> = unit
        .types
        .iter()
        .filter(|t| t.kind == TypeKind::Class)
        .collect();

    assert!(!class_types.is_empty(), "Expected at least one class type");

    let has_fields = class_types.iter().any(|t| !t.fields.is_empty());
    assert!(
        has_fields,
        "Expected at least one class with fields, classes: {:?}",
        class_types
            .iter()
            .map(|t| (&t.name, t.fields.len()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn parse_dcu_class_has_constructor_or_destructor() {
    let data = fs::read(fixture_path("Lint4dFixture.Classes.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();

    let adapter = unit
        .types
        .iter()
        .find(|t| t.name == "TDataAdapter")
        .expect("TDataAdapter not found");

    assert_eq!(adapter.kind, TypeKind::Class);

    let has_ctor_or_dtor = adapter
        .methods
        .iter()
        .any(|m| m.kind == MethodKind::Constructor || m.kind == MethodKind::Destructor);

    assert!(
        has_ctor_or_dtor,
        "Expected TDataAdapter to have a constructor or destructor, methods: {:?}",
        adapter
            .methods
            .iter()
            .map(|m| (&m.name, &m.kind))
            .collect::<Vec<_>>()
    );
}

#[test]
fn parse_dcu_records_unit() {
    let data = fs::read(fixture_path("Lint4dFixture.Records.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Records");
    assert!(
        !unit.types.is_empty(),
        "Expected type definitions in Records unit"
    );
}

#[test]
fn parse_dcu_enums_unit() {
    let data = fs::read(fixture_path("Lint4dFixture.Enums.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Enums");
    assert!(
        !unit.types.is_empty(),
        "Expected type definitions in Enums unit"
    );
}

#[test]
#[ignore] // DCU parser hits unknown tag 0x74 in generics-heavy units
fn parse_dcu_generics_unit() {
    let data = fs::read(fixture_path("Lint4dFixture.Generics.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Generics");
    assert!(
        !unit.types.is_empty(),
        "Expected type definitions in Generics unit"
    );
}

#[test]
fn parse_dcu_inheritance_unit() {
    let data = fs::read(fixture_path("Lint4dFixture.Inheritance.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Inheritance");
    assert!(
        unit.imported_units
            .iter()
            .any(|u| u == "Lint4dFixture.Classes"),
        "Expected 'Lint4dFixture.Classes' in imports, got: {:?}",
        unit.imported_units
    );
}

#[test]
#[ignore] // DCU parser does not yet extract fields from complex classes
fn parse_dcu_torture_many_fields() {
    let data = fs::read(fixture_path("Lint4dFixture.Torture.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Torture");

    let mega = unit
        .types
        .iter()
        .find(|t| t.name == "TMegaClass")
        .expect("TMegaClass not found");

    assert!(
        mega.fields.len() >= 20,
        "Expected TMegaClass to have 20+ fields, got {}",
        mega.fields.len()
    );
}

#[test]
fn parse_dcu_torture_overloaded_methods() {
    let data = fs::read(fixture_path("Lint4dFixture.Torture.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();

    let mega = unit
        .types
        .iter()
        .find(|t| t.name == "TMegaClass")
        .expect("TMegaClass not found");

    let overloaded_count = mega
        .methods
        .iter()
        .filter(|m| m.name == "Overloaded")
        .count();

    assert!(
        overloaded_count >= 2,
        "Expected multiple 'Overloaded' methods, got {}",
        overloaded_count
    );
}

#[test]
fn parse_dcu_torture_imports() {
    let data = fs::read(fixture_path("Lint4dFixture.Torture.dcu")).unwrap();
    let unit = parse_dcu(&data).unwrap();

    let expected = [
        "Lint4dFixture.Classes",
        "Lint4dFixture.Interfaces",
        "Lint4dFixture.Enums",
    ];
    for name in &expected {
        assert!(
            unit.imported_units.iter().any(|u| u == name),
            "Expected '{}' in Torture imports, got: {:?}",
            name,
            unit.imported_units
        );
    }
}

// ── D2010 (Win32) multi-version tests ──────────────────────────────────────

#[test]
fn parse_dcu_d2010_classes() {
    let data = fs::read(d2010_fixture_path("Lint4dFixture.Classes.dcu"))
        .expect("D2010 fixture not found — run make in d2010_win32/");
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Classes");
    assert_eq!(unit.version, DcuVersion::D2010);
    assert_eq!(unit.platform, DcuPlatform::Win32);
    assert!(
        !unit.types.is_empty(),
        "Expected types in D2010 Classes unit"
    );
    let classes: Vec<_> = unit
        .types
        .iter()
        .filter(|t| t.kind == TypeKind::Class)
        .collect();
    assert!(
        !classes.is_empty(),
        "Expected class types in D2010 Classes unit"
    );
}

#[test]
fn parse_dcu_d2010_interfaces() {
    let data = fs::read(d2010_fixture_path("Lint4dFixture.Interfaces.dcu"))
        .expect("D2010 fixture not found — run make in d2010_win32/");
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Interfaces");
    assert_eq!(unit.version, DcuVersion::D2010);
}

#[test]
fn parse_dcu_d2010_records() {
    let data = fs::read(d2010_fixture_path("Lint4dFixture.Records.dcu"))
        .expect("D2010 fixture not found — run make in d2010_win32/");
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Records");
    assert_eq!(unit.version, DcuVersion::D2010);
}

#[test]
fn parse_dcu_d2010_inheritance() {
    let data = fs::read(d2010_fixture_path("Lint4dFixture.Inheritance.dcu"))
        .expect("D2010 fixture not found — run make in d2010_win32/");
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Inheritance");
    assert!(
        unit.imported_units
            .iter()
            .any(|u| u == "Lint4dFixture.Classes"),
        "Expected 'Lint4dFixture.Classes' in imports"
    );
}

#[test]
fn parse_dcu_d2010_class_has_fields() {
    let data = fs::read(d2010_fixture_path("Lint4dFixture.Classes.dcu"))
        .expect("D2010 fixture not found — run make in d2010_win32/");
    let unit = parse_dcu(&data).unwrap();
    let class_types: Vec<_> = unit
        .types
        .iter()
        .filter(|t| t.kind == TypeKind::Class)
        .collect();
    let has_fields = class_types.iter().any(|t| !t.fields.is_empty());
    assert!(
        has_fields,
        "Expected at least one D2010 class with fields, classes: {:?}",
        class_types
            .iter()
            .map(|t| (&t.name, t.fields.len()))
            .collect::<Vec<_>>()
    );
}

// ── XE3 (Win32) multi-version tests ───────────────────────────────────────

#[test]
fn parse_dcu_xe3_classes() {
    let data = fs::read(xe3_fixture_path("Lint4dFixture.Classes.dcu"))
        .expect("XE3 fixture not found — run make in xe3_win32/");
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Classes");
    assert_eq!(unit.version, DcuVersion::DXE3);
    assert_eq!(unit.platform, DcuPlatform::Win32);
    assert!(
        !unit.types.is_empty(),
        "Expected types in XE3 Classes unit"
    );
    let classes: Vec<_> = unit
        .types
        .iter()
        .filter(|t| t.kind == TypeKind::Class)
        .collect();
    assert!(
        !classes.is_empty(),
        "Expected class types in XE3 Classes unit"
    );
}

#[test]
fn parse_dcu_xe3_interfaces() {
    let data = fs::read(xe3_fixture_path("Lint4dFixture.Interfaces.dcu"))
        .expect("XE3 fixture not found — run make in xe3_win32/");
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Interfaces");
    assert_eq!(unit.version, DcuVersion::DXE3);
}

#[test]
fn parse_dcu_xe3_records() {
    let data = fs::read(xe3_fixture_path("Lint4dFixture.Records.dcu"))
        .expect("XE3 fixture not found — run make in xe3_win32/");
    let unit = parse_dcu(&data).unwrap();
    assert_eq!(unit.name, "Lint4dFixture.Records");
    assert_eq!(unit.version, DcuVersion::DXE3);
}

#[test]
fn parse_dcu_xe3_class_has_constructor() {
    let data = fs::read(xe3_fixture_path("Lint4dFixture.Classes.dcu"))
        .expect("XE3 fixture not found — run make in xe3_win32/");
    let unit = parse_dcu(&data).unwrap();
    let adapter = unit
        .types
        .iter()
        .find(|t| t.name == "TDataAdapter")
        .expect("TDataAdapter not found in XE3 fixture");
    assert_eq!(adapter.kind, TypeKind::Class);
    let has_ctor = adapter
        .methods
        .iter()
        .any(|m| m.kind == MethodKind::Constructor);
    assert!(
        has_ctor,
        "Expected TDataAdapter to have a constructor in XE3"
    );
}
