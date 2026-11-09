//! Compact binary codecs for persistent index artifacts.
//!
//! Every on-disk / on-object-storage index file produced by `shoal-query`
//! (inverted indexes, attribute indexes, sparse postings, …) is encoded with
//! the primitives in this module so that the format rules live in one place:
//!
//! * fixed-width integers and floats are **little-endian**;
//! * variable-length unsigned integers use **LEB128** (7 bits per byte, high
//!   bit = continuation);
//! * signed varints use **zig-zag** encoding on top of LEB128;
//! * sorted, strictly-increasing `u32` id lists are **delta-encoded** (first
//!   value absolute, then gaps), which keeps posting lists small and cheap to
//!   range-fetch;
//! * every artifact starts with a 4-byte magic and a `u32` version so that
//!   corrupted or foreign files are rejected early.
//!
//! The decoder is defensive: it never panics on malformed input and returns
//! a [`CodecError`] instead, because index bytes ultimately come from object
//! storage and may be truncated or corrupted.

use std::fmt;

/// Errors produced while decoding a persistent artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// The input ended before a value could be fully read.
    UnexpectedEof {
        /// Byte offset at which the read was attempted.
        offset: usize,
        /// Number of additional bytes that were required.
        needed: usize,
    },
    /// A varint did not terminate within 64 bits.
    VarintOverflow,
    /// An encoded string was not valid UTF-8.
    InvalidUtf8,
    /// The artifact's magic bytes did not match the expected format.
    BadMagic {
        /// Magic the caller expected.
        expected: [u8; 4],
        /// Magic actually found in the input.
        found: [u8; 4],
    },
    /// The artifact's version is newer than this build understands (or zero).
    UnsupportedVersion {
        /// Version found in the input.
        found: u32,
        /// Maximum version this build supports.
        max: u32,
    },
    /// Structurally invalid data (e.g. non-increasing id list).
    Corrupt(String),
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::UnexpectedEof { offset, needed } => write!(
                f,
                "unexpected end of input at offset {offset} (needed {needed} more bytes)"
            ),
            CodecError::VarintOverflow => write!(f, "varint exceeds 64 bits"),
            CodecError::InvalidUtf8 => write!(f, "invalid UTF-8 in encoded string"),
            CodecError::BadMagic { expected, found } => write!(
                f,
                "bad magic: expected {:?}, found {:?}",
                String::from_utf8_lossy(expected),
                String::from_utf8_lossy(found)
            ),
            CodecError::UnsupportedVersion { found, max } => write!(
                f,
                "unsupported format version {found} (max supported {max})"
            ),
            CodecError::Corrupt(msg) => write!(f, "corrupt data: {msg}"),
        }
    }
}

impl std::error::Error for CodecError {}

/// Append-only binary writer.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Creates an empty writer.
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Creates a writer with pre-allocated capacity.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Number of bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Consumes the writer, returning the encoded bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    /// Borrows the bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Writes a single byte.
    pub fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Writes a little-endian `u32`.
    pub fn put_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes a little-endian `u64`.
    pub fn put_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes a little-endian `f32`.
    pub fn put_f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes a little-endian `f64`.
    pub fn put_f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes an LEB128 varint.
    pub fn put_varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                self.buf.push(byte | 0x80);
            } else {
                self.buf.push(byte);
                break;
            }
        }
    }

    /// Writes a zig-zag-encoded signed varint.
    pub fn put_signed_varint(&mut self, v: i64) {
        let zz = ((v << 1) ^ (v >> 63)) as u64;
        self.put_varint(zz);
    }

    /// Writes a length-prefixed byte slice.
    pub fn put_bytes(&mut self, b: &[u8]) {
        self.put_varint(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    /// Writes a length-prefixed UTF-8 string.
    pub fn put_str(&mut self, s: &str) {
        self.put_bytes(s.as_bytes());
    }

    /// Writes a 4-byte magic followed by a `u32` version.
    pub fn put_header(&mut self, magic: &[u8; 4], version: u32) {
        self.buf.extend_from_slice(magic);
        self.put_u32(version);
    }

    /// Delta-encodes a strictly-increasing sorted `u32` list:
    /// `varint(len)`, then `varint(ids[0])`, then `varint(ids[i] - ids[i-1])`.
    pub fn put_u32_deltas(&mut self, ids: &[u32]) {
        self.put_varint(ids.len() as u64);
        let mut prev: u32 = 0;
        for (i, &id) in ids.iter().enumerate() {
            if i == 0 {
                self.put_varint(id as u64);
            } else {
                debug_assert!(id > prev, "ids must be strictly increasing");
                self.put_varint((id - prev) as u64);
            }
            prev = id;
        }
    }
}

/// Zero-copy binary reader over a byte slice.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Creates a reader over `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Current byte offset.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes remaining to be read.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether the reader is exhausted.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], CodecError> {
        if self.remaining() < n {
            return Err(CodecError::UnexpectedEof {
                offset: self.pos,
                needed: n - self.remaining(),
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Reads a single byte.
    pub fn get_u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a little-endian `u32`.
    pub fn get_u32(&mut self) -> Result<u32, CodecError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a little-endian `u64`.
    pub fn get_u64(&mut self) -> Result<u64, CodecError> {
        let b = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(b);
        Ok(u64::from_le_bytes(arr))
    }

    /// Reads a little-endian `f32`.
    pub fn get_f32(&mut self) -> Result<f32, CodecError> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a little-endian `f64`.
    pub fn get_f64(&mut self) -> Result<f64, CodecError> {
        let b = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(b);
        Ok(f64::from_le_bytes(arr))
    }

    /// Reads an LEB128 varint.
    pub fn get_varint(&mut self) -> Result<u64, CodecError> {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.get_u8()?;
            let low = (byte & 0x7f) as u64;
            if shift > 63 || (shift == 63 && low > 1) {
                return Err(CodecError::VarintOverflow);
            }
            result |= low << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    /// Reads a zig-zag-encoded signed varint.
    pub fn get_signed_varint(&mut self) -> Result<i64, CodecError> {
        let zz = self.get_varint()?;
        Ok(((zz >> 1) as i64) ^ -((zz & 1) as i64))
    }

    /// Reads a length-prefixed byte slice.
    pub fn get_bytes(&mut self) -> Result<&'a [u8], CodecError> {
        let len = self.get_varint()? as usize;
        self.take(len)
    }

    /// Reads a length-prefixed UTF-8 string.
    pub fn get_str(&mut self) -> Result<String, CodecError> {
        let bytes = self.get_bytes()?;
        std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .map_err(|_| CodecError::InvalidUtf8)
    }

    /// Validates a 4-byte magic + `u32` version header and returns the
    /// version. Versions of zero or greater than `max_version` are rejected.
    pub fn check_header(
        &mut self,
        magic: &[u8; 4],
        max_version: u32,
    ) -> Result<u32, CodecError> {
        let found = self.take(4)?;
        let mut m = [0u8; 4];
        m.copy_from_slice(found);
        if &m != magic {
            return Err(CodecError::BadMagic {
                expected: *magic,
                found: m,
            });
        }
        let v = self.get_u32()?;
        if v == 0 || v > max_version {
            return Err(CodecError::UnsupportedVersion {
                found: v,
                max: max_version,
            });
        }
        Ok(v)
    }

    /// Decodes a delta-encoded sorted `u32` list (see
    /// [`Writer::put_u32_deltas`]). Validates strict monotonicity.
    pub fn get_u32_deltas(&mut self) -> Result<Vec<u32>, CodecError> {
        let len = self.get_varint()? as usize;
        // Each entry takes at least one byte; reject absurd lengths early so
        // a corrupted length cannot trigger a huge allocation.
        if len > self.remaining() {
            return Err(CodecError::Corrupt(format!(
                "id list claims {len} entries but only {} bytes remain",
                self.remaining()
            )));
        }
        let mut out = Vec::with_capacity(len);
        let mut prev: u64 = 0;
        for i in 0..len {
            let d = self.get_varint()?;
            let v = if i == 0 {
                d
            } else {
                if d == 0 {
                    return Err(CodecError::Corrupt(
                        "id list is not strictly increasing".to_string(),
                    ));
                }
                prev + d
            };
            if v > u32::MAX as u64 {
                return Err(CodecError::Corrupt("id exceeds u32::MAX".to_string()));
            }
            out.push(v as u32);
            prev = v;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_width_roundtrip() {
        let mut w = Writer::new();
        w.put_u8(0xAB);
        w.put_u32(0xDEAD_BEEF);
        w.put_u64(0x0123_4567_89AB_CDEF);
        w.put_f32(3.5);
        w.put_f64(-2.25);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.get_u8().unwrap(), 0xAB);
        assert_eq!(r.get_u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.get_u64().unwrap(), 0x0123_4567_89AB_CDEF);
        assert_eq!(r.get_f32().unwrap(), 3.5);
        assert_eq!(r.get_f64().unwrap(), -2.25);
        assert!(r.is_empty());
    }

    #[test]
    fn varint_roundtrip_boundaries() {
        let values = [
            0u64,
            1,
            127,
            128,
            255,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX - 1,
            u64::MAX,
        ];
        let mut w = Writer::new();
        for &v in &values {
            w.put_varint(v);
        }
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        for &v in &values {
            assert_eq!(r.get_varint().unwrap(), v);
        }
        assert!(r.is_empty());
    }

    #[test]
    fn signed_varint_roundtrip() {
        let values = [0i64, 1, -1, 63, -64, 1_000_000, -1_000_000, i64::MAX, i64::MIN];
        let mut w = Writer::new();
        for &v in &values {
            w.put_signed_varint(v);
        }
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        for &v in &values {
            assert_eq!(r.get_signed_varint().unwrap(), v);
        }
    }

    #[test]
    fn varint_overflow_rejected() {
        let bytes = [0xFFu8; 10];
        let mut r = Reader::new(&bytes);
        assert_eq!(r.get_varint(), Err(CodecError::VarintOverflow));
    }

    #[test]
    fn truncated_varint_is_eof() {
        let bytes = [0x80u8];
        let mut r = Reader::new(&bytes);
        assert!(matches!(
            r.get_varint(),
            Err(CodecError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn string_and_bytes_roundtrip() {
        let mut w = Writer::new();
        w.put_str("héllo wörld");
        w.put_bytes(&[1, 2, 3]);
        w.put_str("");
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.get_str().unwrap(), "héllo wörld");
        assert_eq!(r.get_bytes().unwrap(), &[1, 2, 3]);
        assert_eq!(r.get_str().unwrap(), "");
    }

    #[test]
    fn invalid_utf8_rejected() {
        let mut w = Writer::new();
        w.put_bytes(&[0xFF, 0xFE]);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.get_str(), Err(CodecError::InvalidUtf8));
    }

    #[test]
    fn header_roundtrip_and_mismatch() {
        let mut w = Writer::new();
        w.put_header(b"SHQI", 2);
        let bytes = w.into_bytes();

        let mut ok = Reader::new(&bytes);
        assert_eq!(ok.check_header(b"SHQI", 3).unwrap(), 2);

        let mut bad_magic = Reader::new(&bytes);
        assert!(matches!(
            bad_magic.check_header(b"XXXX", 3),
            Err(CodecError::BadMagic { .. })
        ));

        let mut too_old_reader = Reader::new(&bytes);
        assert!(matches!(
            too_old_reader.check_header(b"SHQI", 1),
            Err(CodecError::UnsupportedVersion { found: 2, max: 1 })
        ));
    }

    #[test]
    fn delta_list_roundtrip() {
        for ids in [
            vec![],
            vec![0u32],
            vec![5],
            vec![0, 1, 2, 3],
            vec![10, 100, 1_000, 1_000_000, u32::MAX],
        ] {
            let mut w = Writer::new();
            w.put_u32_deltas(&ids);
            let bytes = w.into_bytes();
            let mut r = Reader::new(&bytes);
            assert_eq!(r.get_u32_deltas().unwrap(), ids);
            assert!(r.is_empty());
        }
    }

    #[test]
    fn delta_list_rejects_non_increasing() {
        // len=2, first=5, delta=0 -> non-increasing.
        let mut w = Writer::new();
        w.put_varint(2);
        w.put_varint(5);
        w.put_varint(0);
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert!(matches!(r.get_u32_deltas(), Err(CodecError::Corrupt(_))));
    }

    #[test]
    fn delta_list_rejects_absurd_length() {
        let mut w = Writer::new();
        w.put_varint(1_000_000); // claims a million entries, but no data follows
        let bytes = w.into_bytes();
        let mut r = Reader::new(&bytes);
        assert!(matches!(r.get_u32_deltas(), Err(CodecError::Corrupt(_))));
    }
}
