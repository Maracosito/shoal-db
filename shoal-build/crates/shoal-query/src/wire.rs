//! Compact binary primitives shared by the persistent index formats in this
//! crate (inverted text segments, sparse-vector segments).
//!
//! Everything here is plain little-endian fixed-width integers plus LEB128
//! unsigned varints. The helpers are deliberately dependency-free and fully
//! bounds-checked on the read side, because these bytes are fetched from
//! object storage and may be truncated or corrupted in transit.
//!
//! Note: `codec.rs` contains the WAL/segment record codec from milestone #2;
//! this module is intentionally separate and self-contained so the query-side
//! index formats do not couple to the write-path record framing.

use std::fmt;

/// Errors produced while decoding wire data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The buffer ended before a value could be fully decoded.
    UnexpectedEof {
        /// How many bytes the decoder needed.
        wanted: usize,
        /// How many bytes were actually left.
        remaining: usize,
    },
    /// A varint did not terminate within 64 bits.
    VarintOverflow,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::UnexpectedEof { wanted, remaining } => write!(
                f,
                "unexpected end of buffer: wanted {wanted} byte(s), {remaining} remaining"
            ),
            WireError::VarintOverflow => write!(f, "varint exceeds 64 bits"),
        }
    }
}

impl std::error::Error for WireError {}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// Append a `u16` in little-endian order.
pub fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Append a `u32` in little-endian order.
pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Append a `u64` in little-endian order.
pub fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Append an `f32` in little-endian order.
pub fn put_f32(buf: &mut Vec<u8>, v: f32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Append an unsigned LEB128 varint.
pub fn put_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            buf.push(byte | 0x80);
        } else {
            buf.push(byte);
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

/// A bounds-checked sequential reader over a byte slice.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Create a reader positioned at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Current byte offset.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Bytes remaining.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// True when all bytes have been consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Take the next `n` bytes as a slice.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.remaining() < n {
            return Err(WireError::UnexpectedEof {
                wanted: n,
                remaining: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Read a little-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16, WireError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Read a little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, WireError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a little-endian `u64`.
    pub fn read_u64(&mut self) -> Result<u64, WireError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Read a little-endian `f32`.
    pub fn read_f32(&mut self) -> Result<f32, WireError> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read an unsigned LEB128 varint as `u64`.
    pub fn read_uvarint(&mut self) -> Result<u64, WireError> {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            if self.pos >= self.buf.len() {
                return Err(WireError::UnexpectedEof {
                    wanted: 1,
                    remaining: 0,
                });
            }
            if shift >= 64 {
                return Err(WireError::VarintOverflow);
            }
            let byte = self.buf[self.pos];
            self.pos += 1;
            let low = (byte & 0x7f) as u64;
            if shift == 63 && low > 1 {
                return Err(WireError::VarintOverflow);
            }
            result |= low << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    /// Read an unsigned varint and require that it fits in `u32`.
    pub fn read_uvarint_u32(&mut self) -> Result<u32, WireError> {
        let v = self.read_uvarint()?;
        if v > u32::MAX as u64 {
            return Err(WireError::VarintOverflow);
        }
        Ok(v as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        let values: &[u64] = &[
            0,
            1,
            127,
            128,
            255,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX / 2,
            u64::MAX,
        ];
        let mut buf = Vec::new();
        for &v in values {
            put_uvarint(&mut buf, v);
        }
        let mut r = Reader::new(&buf);
        for &v in values {
            assert_eq!(r.read_uvarint().unwrap(), v);
        }
        assert!(r.is_empty());
    }

    #[test]
    fn fixed_width_roundtrip() {
        let mut buf = Vec::new();
        put_u16(&mut buf, 0xBEEF);
        put_u32(&mut buf, 0xDEAD_BEEF);
        put_u64(&mut buf, 0x0123_4567_89AB_CDEF);
        put_f32(&mut buf, -3.5);
        let mut r = Reader::new(&buf);
        assert_eq!(r.read_u16().unwrap(), 0xBEEF);
        assert_eq!(r.read_u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.read_u64().unwrap(), 0x0123_4567_89AB_CDEF);
        assert_eq!(r.read_f32().unwrap(), -3.5);
        assert!(r.is_empty());
    }

    #[test]
    fn eof_is_error_not_panic() {
        let buf = [0x01u8, 0x02];
        let mut r = Reader::new(&buf);
        assert!(matches!(
            r.read_u32(),
            Err(WireError::UnexpectedEof { wanted: 4, .. })
        ));
        // Truncated varint (continuation bit set at end of buffer).
        let buf = [0x80u8];
        let mut r = Reader::new(&buf);
        assert!(matches!(
            r.read_uvarint(),
            Err(WireError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn varint_overflow_is_error() {
        // 11 bytes of continuation: more than 64 bits.
        let buf = [0xFFu8; 11];
        let mut r = Reader::new(&buf);
        assert_eq!(r.read_uvarint(), Err(WireError::VarintOverflow));
    }

    #[test]
    fn uvarint_u32_rejects_large() {
        let mut buf = Vec::new();
        put_uvarint(&mut buf, u32::MAX as u64 + 1);
        let mut r = Reader::new(&buf);
        assert_eq!(r.read_uvarint_u32(), Err(WireError::VarintOverflow));
    }
}
