use crate::error::{FontError, Result};

/// Big-endian cursor over a byte slice; every read is bounds-checked.
#[derive(Clone, Copy)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    ctx: &'static str,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8], ctx: &'static str) -> Self {
        Reader { data, pos: 0, ctx }
    }

    pub(crate) fn at(data: &'a [u8], pos: usize, ctx: &'static str) -> Result<Self> {
        if pos > data.len() {
            return Err(FontError::Truncated(ctx));
        }
        Ok(Reader { data, pos, ctx })
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(FontError::Truncated(self.ctx))?;
        let out = self
            .data
            .get(self.pos..end)
            .ok_or(FontError::Truncated(self.ctx))?;
        self.pos = end;
        Ok(out)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self.bytes(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn i8(&mut self) -> Result<i8> {
        Ok(i8::from_be_bytes(self.array()?))
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    pub(crate) fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }
}

pub(crate) fn read_u16(data: &[u8], off: usize) -> Option<u16> {
    let bytes = data.get(off..off.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

pub(crate) fn read_i16(data: &[u8], off: usize) -> Option<i16> {
    read_u16(data, off).map(|v| v as i16)
}

pub(crate) fn read_u32(data: &[u8], off: usize) -> Option<u32> {
    let bytes = data.get(off..off.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}
