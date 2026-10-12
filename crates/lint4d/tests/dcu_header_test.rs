use lint4d::dcu::header::{parse_magic, parse_unit_header};
use lint4d::dcu::tags::DcuError;
use lint4d::dcu::{DcuPlatform, DcuVersion};
use std::fs;
use std::path::PathBuf;

#[test]
fn parse_magic_d13_win64() {
    let data = [0x4D, 0x23, 0x00, 0x25];
    let (version, platform) = parse_magic(&data).unwrap();
    assert_eq!(version, DcuVersion::D13);
    assert_eq!(platform, DcuPlatform::Win64);
}

#[test]
fn parse_magic_d2010_win32() {
    let data = 0x1500_0045_u32.to_le_bytes();
    let (version, platform) = parse_magic(&data).unwrap();
    assert_eq!(version, DcuVersion::D2010);
    assert_eq!(platform, DcuPlatform::Win32);
}

#[test]
fn parse_magic_xe3_win32() {
    let data = 0x1800_034B_u32.to_le_bytes();
    let (version, platform) = parse_magic(&data).unwrap();
    assert_eq!(version, DcuVersion::DXE3);
    assert_eq!(platform, DcuPlatform::Win32);
}

#[test]
fn parse_magic_xe2_win64() {
    let data = 0x1700_234B_u32.to_le_bytes();
    let (version, platform) = parse_magic(&data).unwrap();
    assert_eq!(version, DcuVersion::DXE2);
    assert_eq!(platform, DcuPlatform::Win64);
}

#[test]
fn parse_magic_unknown() {
    let data = [0x00, 0x00, 0x00, 0x00];
    let result = parse_magic(&data);
    assert!(matches!(result, Err(DcuError::UnsupportedVersion { .. })));
}

#[test]
fn parse_magic_too_short() {
    let data = [0x4D, 0x23];
    let result = parse_magic(&data);
    assert!(matches!(result, Err(DcuError::UnexpectedEof { .. })));
}

#[test]
fn parse_magic_from_d2010_fixture() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dcu/d2010_win32/Win32/Debug/Lint4dFixture.Classes.dcu");
    let data = fs::read(&path).expect("D2010 fixture not found — run make in d2010_win32/");
    let (version, platform) = parse_magic(&data).unwrap();
    assert_eq!(version, DcuVersion::D2010);
    assert_eq!(platform, DcuPlatform::Win32);
}

#[test]
fn parse_magic_from_xe3_fixture() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dcu/xe3_win32/Win32/Debug/Lint4dFixture.Classes.dcu");
    let data = fs::read(&path).expect("XE3 fixture not found — run make in xe3_win32/");
    let (version, platform) = parse_magic(&data).unwrap();
    assert_eq!(version, DcuVersion::DXE3);
    assert_eq!(platform, DcuPlatform::Win32);
}

/// A D13 Win64 unit header: magic, size, two stamp dwords, an empty name,
/// unit index 1, the unit name, then `records` and the drSrc record.
fn synthetic_header(records: &[u8]) -> Vec<u8> {
    let mut data = vec![0x4D, 0x23, 0x00, 0x25, 0, 0, 0, 0];
    data.extend_from_slice(&[0x71, 0xA8, 0x7A, 0x5C, 0x8E, 0x44, 0x26, 0x08]);
    data.extend_from_slice(&[0x00, 0x02, 0x0D]);
    data.extend_from_slice(b"Lint4dFixture");
    data.extend_from_slice(records);
    data.push(0x70);
    data.push(25);
    data.extend_from_slice(b"Lint4dFixture.Classes.pas");
    data.extend_from_slice(&[0x54, 0xA7, 0x7A, 0x5C, 0x61]);
    let size = (data.len() as u32).to_le_bytes();
    data[4..8].copy_from_slice(&size);
    data
}

#[test]
fn unit_header_reads_tagged_records_containing_a_drsrc_byte() {
    let data = synthetic_header(&[0xFE, 0x27, 0xFE, 0xEF, 0x07, 0x96, 0x00, 0x06, 0x70]);
    let header = parse_unit_header(&data).expect("header");
    assert_eq!(header.name, "Lint4dFixture.Classes");
    assert_eq!(header.flags, 0x7E_FFE2);
}

#[test]
fn unit_header_rejects_a_size_that_does_not_match_the_input() {
    let mut data = synthetic_header(&[0xFE, 0x27, 0xFE, 0xEF, 0x07, 0x96, 0x00, 0x06, 0x3C]);
    let actual = data.len();
    data[4] = data[4].wrapping_add(1);
    match parse_unit_header(&data) {
        Err(DcuError::SizeMismatch {
            declared,
            actual: seen,
        }) => {
            assert_eq!(declared as usize, actual + 1);
            assert_eq!(seen, actual);
        }
        other => panic!("expected a size mismatch, got {:?}", other.map(|h| h.name)),
    }
}

#[test]
fn unit_header_rejects_an_unknown_record_before_the_source_name() {
    let data = synthetic_header(&[0xFE, 0x27, 0xFE, 0xEF, 0x07, 0x42, 0x96, 0x00, 0x06, 0x3C]);
    match parse_unit_header(&data) {
        Err(DcuError::UnknownTag { tag: 0x42, offset }) => assert_eq!(offset, 37),
        other => panic!("expected an unknown tag, got {:?}", other.map(|h| h.name)),
    }
}

#[test]
fn unit_header_reads_every_fixture() {
    for fixture in [
        "tests/fixtures/dcu/d13_win64/Win64/Debug/Lint4dFixture.Classes.dcu",
        "tests/fixtures/dcu/d2010_win32/Win32/Debug/Lint4dFixture.Classes.dcu",
        "tests/fixtures/dcu/xe3_win32/Win32/Debug/Lint4dFixture.Classes.dcu",
    ] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(fixture);
        let data = fs::read(&path).expect("fixture");
        let header = parse_unit_header(&data).expect(fixture);
        assert_eq!(header.name, "Lint4dFixture.Classes", "{fixture}");
        assert_eq!(header.stamp as usize, data.len(), "{fixture}");
    }
}
