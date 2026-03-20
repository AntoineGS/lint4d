use lint4d::dcu::header::parse_unit_header;
use lint4d::dcu::types::parse_dcu;
use lint4d::dcu::{DcuPlatform, DcuVersion};
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

