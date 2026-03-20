use crate::dcu::tags::DcuError;

pub struct DcuReader<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) pos: usize,
}
