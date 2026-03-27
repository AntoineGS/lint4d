use lint4d::dcu::reader::DcuReader;
use lint4d::dcu::DcuVersion;

#[test]
fn read_byte_returns_byte_and_advances() {
    let data = [0x42, 0xFF];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_byte().unwrap(), 0x42);
    assert_eq!(r.position(), 1);
    assert_eq!(r.read_byte().unwrap(), 0xFF);
    assert_eq!(r.position(), 2);
}

#[test]
fn read_byte_eof() {
    let data = [];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert!(r.read_byte().is_err());
}

#[test]
fn read_word_little_endian() {
    let data = [0x34, 0x12];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_word().unwrap(), 0x1234);
}

#[test]
fn read_u32_little_endian() {
    let data = [0x78, 0x56, 0x34, 0x12];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_u32().unwrap(), 0x12345678);
}

// ReadUIndex tests
#[test]
fn read_uindex_1byte() {
    let data = [0x24];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_uindex().unwrap(), 18);
    assert_eq!(r.position(), 1);
}

#[test]
fn read_uindex_1byte_zero() {
    let data = [0x00];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_uindex().unwrap(), 0);
}

#[test]
fn read_uindex_2bytes() {
    let data = [0x0D, 0x49];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_uindex().unwrap(), 0x1243);
    assert_eq!(r.position(), 2);
}

#[test]
fn read_uindex_3bytes() {
    let data = [0x23, 0x03, 0x00];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_uindex().unwrap(), 100);
    assert_eq!(r.position(), 3);
}

#[test]
fn read_uindex_4bytes() {
    let data = [0x87, 0x0C, 0x00, 0x00];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_uindex().unwrap(), 200);
    assert_eq!(r.position(), 4);
}

#[test]
fn read_uindex_5bytes() {
    let data = [0x0F, 0x78, 0x56, 0x34, 0x12];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_uindex().unwrap(), 0x12345678);
    assert_eq!(r.position(), 5);
}

// ReadIndex (signed) tests
#[test]
fn read_index_positive_1byte() {
    let data = [0x24];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_index().unwrap(), 18);
}

#[test]
fn read_index_negative_1byte() {
    let data = [0xFE];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_index().unwrap(), -1);
}

#[test]
fn read_index_zero() {
    let data = [0x00];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_index().unwrap(), 0);
}

#[test]
fn read_index_negative_2byte() {
    // value = -2: encoded as (-2 << 2) | 0b01 = -7 as i16 = 0xFFF9
    let data = [0xF9, 0xFF];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_index().unwrap(), -2);
}

#[test]
fn read_name_short() {
    let data = [0x06, b'S', b'y', b's', b't', b'e', b'm'];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_name().unwrap(), "System");
    assert_eq!(r.position(), 7);
}

#[test]
fn read_name_empty() {
    let data = [0x00];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_name().unwrap(), "");
    assert_eq!(r.position(), 1);
}

#[test]
fn read_name_long_format() {
    let data = [0xFF, 0x03, 0x00, 0x00, 0x00, b'F', b'o', b'o'];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.read_name().unwrap(), "Foo");
    assert_eq!(r.position(), 8);
}

#[test]
fn unread_moves_position_back() {
    let data = [0x01, 0x02, 0x03, 0x04];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    r.read_byte().unwrap();
    r.read_byte().unwrap();
    assert_eq!(r.position(), 2);
    r.unread(1);
    assert_eq!(r.position(), 1);
    assert_eq!(r.read_byte().unwrap(), 0x02);
}

#[test]
fn unread_saturates_at_zero() {
    let data = [0x01, 0x02];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    r.read_byte().unwrap();
    r.unread(5);
    assert_eq!(r.position(), 0);
}

#[test]
fn peek_byte_does_not_advance() {
    let data = [0xAB, 0xCD];
    let r = DcuReader::new(&data, DcuVersion::D13);
    assert_eq!(r.peek_byte().unwrap(), 0xAB);
    assert_eq!(r.position(), 0);
}

#[test]
fn peek_byte_eof() {
    let data = [];
    let r = DcuReader::new(&data, DcuVersion::D13);
    assert!(r.peek_byte().is_err());
}

#[test]
fn set_position_moves_reader() {
    let data = [0x01, 0x02, 0x03, 0x04];
    let mut r = DcuReader::new(&data, DcuVersion::D13);
    r.set_position(2);
    assert_eq!(r.position(), 2);
    assert_eq!(r.read_byte().unwrap(), 0x03);
}
