use lint4d::dcu::header::parse_magic;
use lint4d::dcu::tags::DcuError;
use lint4d::dcu::{DcuPlatform, DcuVersion};

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
