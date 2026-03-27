use lint4d::dcu::header::parse_magic;
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
