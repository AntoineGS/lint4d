use crate::dcu::reader::DcuReader;
use crate::dcu::tags::DcuError;
use crate::dcu::{DcuPlatform, DcuVersion};

const MAGIC_D13_WIN32: u32 = 0x2500_034D;
const MAGIC_D13_WIN64: u32 = 0x2500_234D;

pub fn parse_magic(data: &[u8]) -> Result<(DcuVersion, DcuPlatform), DcuError> {
    let mut reader = DcuReader::new(data);
    let magic = reader.read_u32()?;
    match magic {
        MAGIC_D13_WIN32 => Ok((DcuVersion::D13, DcuPlatform::Win32)),
        MAGIC_D13_WIN64 => Ok((DcuVersion::D13, DcuPlatform::Win64)),
        _ => Err(DcuError::UnsupportedVersion { magic }),
    }
}
