use crate::dcu::tags::DcuError;

pub struct DcuReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> DcuReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn peek_at(&self, offset: usize) -> u8 {
        if offset < self.data.len() {
            self.data[offset]
        } else {
            0
        }
    }

    pub fn peek_bytes(&self, start: usize, end: usize) -> &[u8] {
        let s = std::cmp::min(start, self.data.len());
        let e = std::cmp::min(end, self.data.len());
        &self.data[s..e]
    }

    pub fn set_position(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn unread(&mut self, n: usize) {
        self.pos = self.pos.saturating_sub(n);
    }

    pub fn peek_byte(&self) -> Result<u8, DcuError> {
        if self.pos >= self.data.len() {
            return Err(DcuError::UnexpectedEof {
                context: "peek_byte",
            });
        }
        Ok(self.data[self.pos])
    }

    pub fn read_byte(&mut self) -> Result<u8, DcuError> {
        if self.pos >= self.data.len() {
            return Err(DcuError::UnexpectedEof {
                context: "read_byte",
            });
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    pub fn read_word(&mut self) -> Result<u16, DcuError> {
        if self.pos + 2 > self.data.len() {
            return Err(DcuError::UnexpectedEof {
                context: "read_word",
            });
        }
        let val = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(val)
    }

    pub fn read_u32(&mut self) -> Result<u32, DcuError> {
        if self.pos + 4 > self.data.len() {
            return Err(DcuError::UnexpectedEof {
                context: "read_u32",
            });
        }
        let val = u32::from_le_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        Ok(val)
    }

    pub fn skip(&mut self, n: usize) -> Result<(), DcuError> {
        if self.pos + n > self.data.len() {
            return Err(DcuError::UnexpectedEof { context: "skip" });
        }
        self.pos += n;
        Ok(())
    }

    /// DCU variable-length unsigned integer encoding.
    /// Low bits of first byte signal how many bytes follow.
    pub fn read_uindex(&mut self) -> Result<u32, DcuError> {
        let b0 = self.read_byte()? as u32;
        if b0 & 1 == 0 {
            return Ok(b0 >> 1);
        }
        if b0 & 2 == 0 {
            let b1 = self.read_byte()? as u32;
            let w = b0 | (b1 << 8);
            return Ok(w >> 2);
        }
        if b0 & 4 == 0 {
            let b1 = self.read_byte()? as u32;
            let b2 = self.read_byte()? as u32;
            let dw = b0 | (b1 << 8) | (b2 << 16);
            return Ok(dw >> 3);
        }
        if b0 & 8 == 0 {
            let b1 = self.read_byte()? as u32;
            let b2 = self.read_byte()? as u32;
            let b3 = self.read_byte()? as u32;
            let dw = b0 | (b1 << 8) | (b2 << 16) | (b3 << 24);
            return Ok(dw >> 4);
        }
        // 5-byte form: next 4 bytes are the raw value
        let val = self.read_u32()?;
        Ok(val)
    }

    /// Read a length-prefixed ANSI name string.
    /// If the first byte is 0xFF, the following 4 bytes are a u32 length.
    /// Otherwise the first byte is the length directly.
    pub fn read_name(&mut self) -> Result<String, DcuError> {
        let len_byte = self.read_byte()?;
        let len = if len_byte == 0xFF {
            self.read_u32()? as usize
        } else {
            len_byte as usize
        };
        if len == 0 {
            return Ok(String::new());
        }
        if self.pos + len > self.data.len() {
            return Err(DcuError::UnexpectedEof {
                context: "read_name",
            });
        }
        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;
        // ANSI bytes — treat as Latin-1 (superset of ASCII)
        let s: String = bytes.iter().map(|&b| b as char).collect();
        Ok(s)
    }

    /// DCU variable-length signed integer encoding.
    /// Same byte structure as read_uindex but with arithmetic (sign-extending) shift.
    pub fn read_index(&mut self) -> Result<i32, DcuError> {
        let b0 = self.read_byte()?;
        if b0 & 1 == 0 {
            return Ok((b0 as i8 as i32) >> 1);
        }
        if b0 & 2 == 0 {
            let b1 = self.read_byte()?;
            let w = (b0 as u16) | ((b1 as u16) << 8);
            return Ok((w as i16 as i32) >> 2);
        }
        if b0 & 4 == 0 {
            let b1 = self.read_byte()?;
            let b2 = self.read_byte()?;
            // Sign-extend b2 via i8→i32→u32 so bits 24-31 carry the sign
            // before the arithmetic right-shift recovers the 21-bit signed value.
            let dw = (b0 as u32) | ((b1 as u32) << 8) | ((b2 as i8 as i32 as u32) << 16);
            return Ok((dw as i32) >> 3);
        }
        if b0 & 8 == 0 {
            let b1 = self.read_byte()?;
            let b2 = self.read_byte()?;
            let b3 = self.read_byte()?;
            let dw = (b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16) | ((b3 as u32) << 24);
            return Ok((dw as i32) >> 4);
        }
        // 5-byte form
        let val = self.read_u32()? as i32;
        Ok(val)
    }
}
