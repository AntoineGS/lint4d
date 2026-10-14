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

#[test]
fn parse_magic_ignores_the_bits_the_compiler_masks() {
    // dcc accepts (magic & 0xff801fff) == 0x2500034d: bits 16-22 vary.
    let data = 0x257F_234Du32.to_le_bytes();
    assert_eq!(
        parse_magic(&data).unwrap(),
        (DcuVersion::D13, DcuPlatform::Win64)
    );
    let data = 0x2540_034Du32.to_le_bytes();
    assert_eq!(
        parse_magic(&data).unwrap(),
        (DcuVersion::D13, DcuPlatform::Win32)
    );
}

#[test]
fn parse_magic_accepts_the_alternate_d13_code_byte() {
    // dcc 13 expects 0x00 instead of 0x03 in bits 8-12 under one option.
    let data = 0x2500_004Du32.to_le_bytes();
    assert_eq!(
        parse_magic(&data).unwrap(),
        (DcuVersion::D13, DcuPlatform::Win32)
    );
}

#[test]
fn parse_magic_reports_an_unsupported_target_separately_from_the_version() {
    for code in 2..8u32 {
        let magic = 0x2500_034D | (code << 13);
        match parse_magic(&magic.to_le_bytes()) {
            Err(DcuError::UnsupportedPlatform {
                version,
                target_code,
                ..
            }) => {
                assert_eq!(version, DcuVersion::D13);
                assert_eq!(u32::from(target_code), code);
            }
            other => panic!("target code {code}: {other:?}"),
        }
    }
    // D2010 has no Win64 compiler.
    let magic = 0x1500_2045u32;
    assert!(matches!(
        parse_magic(&magic.to_le_bytes()),
        Err(DcuError::UnsupportedPlatform {
            version: DcuVersion::D2010,
            target_code: 1,
            ..
        })
    ));
}

#[test]
fn parse_magic_rejects_a_known_version_byte_with_a_foreign_signature() {
    for magic in [0x2500_034Cu32, 0x2580_034D, 0x2500_014D] {
        assert!(
            matches!(
                parse_magic(&magic.to_le_bytes()),
                Err(DcuError::UnsupportedVersion { .. })
            ),
            "{magic:#010x}"
        );
    }
}

#[test]
fn parse_magic_maps_every_previously_accepted_signature_unchanged() {
    use DcuPlatform::{Win32, Win64};
    use DcuVersion::*;
    let expected = [
        (0x1500_0045, D2010, Win32),
        (0x1600_034B, DXE, Win32),
        (0x1700_034B, DXE2, Win32),
        (0x1700_234B, DXE2, Win64),
        (0x1800_034B, DXE3, Win32),
        (0x1800_234B, DXE3, Win64),
        (0x1900_034B, DXE4, Win32),
        (0x1900_234B, DXE4, Win64),
        (0x1A00_034B, DXE5, Win32),
        (0x1A00_234B, DXE5, Win64),
        (0x1B00_034D, DXE6, Win32),
        (0x1B00_234D, DXE6, Win64),
        (0x1C00_034D, DXE7, Win32),
        (0x1C00_234D, DXE7, Win64),
        (0x1D00_034D, DXE8, Win32),
        (0x1D00_234D, DXE8, Win64),
        (0x1E00_034D, D10S, Win32),
        (0x1E00_234D, D10S, Win64),
        (0x1F00_034D, D101B, Win32),
        (0x1F00_234D, D101B, Win64),
        (0x2000_034D, D102T, Win32),
        (0x2000_234D, D102T, Win64),
        (0x2100_034D, D103R, Win32),
        (0x2100_234D, D103R, Win64),
        (0x2200_034D, D104S, Win32),
        (0x2200_234D, D104S, Win64),
        (0x2300_034D, D11A, Win32),
        (0x2300_234D, D11A, Win64),
        (0x2400_034D, D12A, Win32),
        (0x2400_234D, D12A, Win64),
        (0x2500_034D, D13, Win32),
        (0x2500_234D, D13, Win64),
    ];
    for (magic, version, platform) in expected {
        assert_eq!(
            parse_magic(&u32::to_le_bytes(magic)).unwrap(),
            (version, platform),
            "{magic:#010x}"
        );
    }
}
