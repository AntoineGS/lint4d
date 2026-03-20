use lint4d::dcu::header::parse_unit_header;
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
