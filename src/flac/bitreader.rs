use std::io::Read;

use super::error::{Result, error};

const INITIAL_CAPACITY: usize = 64 * 1024;
const MAX_BUFFER_BYTES: usize = 16 * 1024 * 1024;

/// A byte buffer that retains the current frame so its CRC can be computed
/// after the subframes have been decoded.
pub(crate) struct ByteSource<R: Read> {
    inner: R,
    buffer: Vec<u8>,
    position: usize,
    length: usize,
    eof: bool,
}

impl<R: Read> ByteSource<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self {
            inner,
            buffer: vec![0; INITIAL_CAPACITY],
            position: 0,
            length: 0,
            eof: false,
        }
    }

    pub(crate) fn start_frame(&mut self) {
        if self.position > 0 {
            self.buffer.copy_within(self.position..self.length, 0);
            self.length -= self.position;
            self.position = 0;
        }
    }

    fn refill(&mut self) -> Result<()> {
        if self.eof {
            return error("unexpected end of FLAC stream");
        }
        if self.length == self.buffer.len() {
            if self.buffer.len() >= MAX_BUFFER_BYTES {
                return error("FLAC frame exceeds the decoder buffer limit");
            }
            let new_len = (self.buffer.len() * 2).min(MAX_BUFFER_BYTES);
            self.buffer.resize(new_len, 0);
        }
        let read = self
            .inner
            .read(&mut self.buffer[self.length..])
            .map_err(|e| super::error::Error::new(format!("read: {e}")))?;
        if read == 0 {
            self.eof = true;
            return error("unexpected end of FLAC stream");
        }
        self.length += read;
        Ok(())
    }

    #[inline]
    pub(crate) fn ensure(&mut self, count: usize) -> Result<()> {
        if count > MAX_BUFFER_BYTES {
            return error("FLAC field exceeds the decoder buffer limit");
        }
        while self.length - self.position < count {
            self.refill()?;
        }
        Ok(())
    }

    /// Ensures `count` bytes are available. Returns false only when the stream
    /// ended at a clean byte boundary before any byte was requested.
    pub(crate) fn ensure_optional(&mut self, count: usize) -> Result<bool> {
        while self.length - self.position < count {
            if self.eof {
                return if self.length == self.position {
                    Ok(false)
                } else {
                    error("unexpected end of FLAC stream")
                };
            }
            if self.length == self.buffer.len() {
                if self.buffer.len() >= MAX_BUFFER_BYTES {
                    return error("FLAC frame exceeds the decoder buffer limit");
                }
                let new_len = (self.buffer.len() * 2).min(MAX_BUFFER_BYTES);
                self.buffer.resize(new_len, 0);
            }
            let read = self
                .inner
                .read(&mut self.buffer[self.length..])
                .map_err(|e| super::error::Error::new(format!("read: {e}")))?;
            if read == 0 {
                self.eof = true;
            } else {
                self.length += read;
            }
        }
        Ok(true)
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8> {
        self.ensure(1)?;
        let byte = self.buffer[self.position];
        self.position += 1;
        Ok(byte)
    }

    pub(crate) fn read_be_u16(&mut self) -> Result<u16> {
        self.ensure(2)?;
        let value =
            u16::from_be_bytes([self.buffer[self.position], self.buffer[self.position + 1]]);
        self.position += 2;
        Ok(value)
    }

    pub(crate) fn read_be_u24(&mut self) -> Result<u32> {
        self.ensure(3)?;
        let value = ((self.buffer[self.position] as u32) << 16)
            | ((self.buffer[self.position + 1] as u32) << 8)
            | self.buffer[self.position + 2] as u32;
        self.position += 3;
        Ok(value)
    }

    pub(crate) fn read_into(&mut self, out: &mut [u8]) -> Result<()> {
        self.ensure(out.len())?;
        out.copy_from_slice(&self.buffer[self.position..self.position + out.len()]);
        self.position += out.len();
        Ok(())
    }

    pub(crate) fn skip(&mut self, mut count: usize) -> Result<()> {
        while count > 0 {
            if self.length == self.position {
                self.refill()?;
            }
            let available = self.length - self.position;
            let take = count.min(available);
            self.position += take;
            count -= take;
        }
        Ok(())
    }

    pub(crate) fn position(&self) -> usize {
        self.position
    }

    pub(crate) fn crc_bytes(&self, end: usize) -> &[u8] {
        &self.buffer[..end]
    }
}

/// Reads bits MSB-first from the current frame buffer.
///
/// `cache` holds `cache_bits` unread bits at its most-significant end.
/// `source.position` is the first byte represented by the cache; bytes may be
/// loaded ahead, but the position advances only for fully consumed bytes.
/// `align_to_byte` drops the lookahead bits and leaves them in the source
/// buffer, so the next frame starts at `source.position`.
pub(crate) struct BitReader<'a, R: Read> {
    source: &'a mut ByteSource<R>,
    cache: u64,
    cache_bits: u32,
}

impl<'a, R: Read> BitReader<'a, R> {
    pub(crate) fn new(source: &'a mut ByteSource<R>) -> Self {
        Self {
            source,
            cache: 0,
            cache_bits: 0,
        }
    }

    #[inline(always)]
    fn refill(&mut self) -> Result<()> {
        debug_assert!(self.cache_bits <= 56);
        let loaded_bytes = (self.cache_bits as usize + 7) / 8;
        let index = self.source.position + loaded_bytes;
        self.source.ensure(index - self.source.position + 1)?;
        let byte = self.source.buffer[index] as u64;
        let free = 64 - self.cache_bits;
        self.cache |= byte << (free - 8);
        self.cache_bits += 8;
        Ok(())
    }

    #[inline(always)]
    fn advance(&mut self, count: u32) {
        let offset = (64 - self.cache_bits) % 8;
        self.source.position += ((offset + count) / 8) as usize;
        if count == 64 {
            self.cache = 0;
        } else {
            self.cache <<= count;
        }
        self.cache_bits -= count;
    }

    #[inline(always)]
    pub(crate) fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read_bits(1)? != 0)
    }

    #[inline(always)]
    pub(crate) fn read_bits(&mut self, count: u32) -> Result<u64> {
        if count == 0 {
            return Ok(0);
        }
        while self.cache_bits < count {
            self.refill()?;
        }
        let value = self.cache >> (64 - count);
        self.advance(count);
        Ok(value)
    }

    #[inline(always)]
    pub(crate) fn read_signed(&mut self, count: u32) -> Result<i64> {
        if count == 0 {
            return Ok(0);
        }
        let value = self.read_bits(count)?;
        if count < 64 && value & (1u64 << (count - 1)) != 0 {
            Ok(value as i64 - (1i64 << count))
        } else {
            Ok(value as i64)
        }
    }

    #[inline(always)]
    pub(crate) fn read_unary(&mut self) -> Result<u64> {
        let mut zeros = 0u64;
        loop {
            if self.cache_bits == 0 {
                self.refill()?;
            }
            let zero_count = self.cache.leading_zeros();
            if zero_count < self.cache_bits {
                let consumed = zero_count + 1;
                zeros += zero_count as u64;
                self.advance(consumed);
                return Ok(zeros);
            }
            zeros += self.cache_bits as u64;
            self.advance(self.cache_bits);
        }
    }

    pub(crate) fn align_to_byte(&mut self) -> Result<()> {
        let offset = (64 - self.cache_bits) % 8;
        let padding = (8 - offset) % 8;
        if padding > 0 && self.read_bits(padding)? != 0 {
            return error("nonzero frame padding");
        }
        // Discard lookahead bits. `source.position` is now the next frame's
        // first byte; the bytes themselves remain in the source buffer.
        self.cache = 0;
        self.cache_bits = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{BitReader, ByteSource};

    #[test]
    fn reads_bits_and_unary() {
        let mut source = ByteSource::new(Cursor::new(vec![0b1011_0010, 0b0000_0011]));
        source.start_frame();
        let mut bits = BitReader::new(&mut source);
        assert_eq!(bits.read_bits(1).unwrap(), 1);
        assert_eq!(bits.read_bits(3).unwrap(), 0b011);
        assert_eq!(bits.read_unary().unwrap(), 2);
        assert_eq!(bits.read_bits(2).unwrap(), 0);
        assert_eq!(bits.read_unary().unwrap(), 5);
        assert_eq!(bits.read_bits(1).unwrap(), 1);
        bits.align_to_byte().unwrap();
    }

    #[test]
    fn long_unary_run_spans_cache() {
        let mut data = vec![0u8; 9];
        data.push(0x01);
        let mut source = ByteSource::new(Cursor::new(data));
        source.start_frame();
        let mut bits = BitReader::new(&mut source);
        assert_eq!(bits.read_unary().unwrap(), 79);
    }

    #[test]
    fn signed_values() {
        let mut source = ByteSource::new(Cursor::new(vec![0b1111_0000]));
        source.start_frame();
        let mut bits = BitReader::new(&mut source);
        assert_eq!(bits.read_signed(4).unwrap(), -1);
        assert_eq!(bits.read_signed(4).unwrap(), 0);
    }
}
