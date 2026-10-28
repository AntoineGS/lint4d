//! Bounded decoding of canonical encoding v1: every length is checked
//! against what remains before anything is taken or allocated.

use crate::Digest;
use std::path::PathBuf;

pub(crate) type Decoded<T> = Result<T, &'static str>;

pub(crate) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    pub(crate) fn take(&mut self, count: usize) -> Decoded<&'a [u8]> {
        if count > self.0.len() {
            return Err("truncated");
        }
        let (head, tail) = self.0.split_at(count);
        self.0 = tail;
        Ok(head)
    }

    pub(crate) fn u8(&mut self) -> Decoded<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Decoded<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("two bytes"),
        ))
    }

    pub(crate) fn u64(&mut self) -> Decoded<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight bytes"),
        ))
    }

    pub(crate) fn length(&mut self) -> Decoded<usize> {
        usize::try_from(self.u64()?).map_err(|_| "length overflows")
    }

    pub(crate) fn field(&mut self, tag: u16) -> Decoded<()> {
        if self.u16()? == tag {
            Ok(())
        } else {
            Err("unexpected field")
        }
    }

    pub(crate) fn bool(&mut self) -> Decoded<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("invalid bool"),
        }
    }

    /// An `Option`: its 0/1 tag, then the value if present.
    pub(crate) fn optional<T>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Decoded<T>,
    ) -> Decoded<Option<T>> {
        if self.bool()? {
            read(self).map(Some)
        } else {
            Ok(None)
        }
    }

    pub(crate) fn digest(&mut self) -> Decoded<Digest> {
        Ok(Digest::from_bytes(
            self.take(32)?.try_into().expect("32 bytes"),
        ))
    }

    pub(crate) fn str(&mut self) -> Decoded<String> {
        let length = self.length()?;
        std::str::from_utf8(self.take(length)?)
            .map(str::to_owned)
            .map_err(|_| "invalid UTF-8")
    }

    #[cfg(unix)]
    pub(crate) fn path(&mut self) -> Decoded<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        if self.u8()? != 1 {
            return Err("foreign path encoding");
        }
        let length = self.length()?;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(
            self.take(length)?,
        )))
    }

    #[cfg(windows)]
    pub(crate) fn path(&mut self) -> Decoded<PathBuf> {
        use std::os::windows::ffi::OsStringExt;
        if self.u8()? != 2 {
            return Err("foreign path encoding");
        }
        let units = self.length()?;
        let bytes = self.take(units.checked_mul(2).ok_or("length overflows")?)?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect();
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&units)))
    }

    /// Nothing may follow a complete value.
    pub(crate) fn finish(&self) -> Decoded<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err("trailing bytes")
        }
    }
}
