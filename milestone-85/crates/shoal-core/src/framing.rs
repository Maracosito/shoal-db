//! CRC-checked binary framing for WAL entries and segments.
//!
//! Layout: `[8-byte magic][u32 LE body length][u32 LE crc32(body)][body]`.

use crate::error::{Result, ShoalError};
use bytes::Bytes;

pub const HEADER_LEN: usize = 16;

pub fn encode_framed(magic: &[u8; 8], body: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(magic);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32fast::hash(body).to_le_bytes());
    out.extend_from_slice(body);
    Bytes::from(out)
}

pub fn decode_framed<'a>(magic: &[u8; 8], data: &'a [u8]) -> Result<&'a [u8]> {
    if data.len() < HEADER_LEN {
        return Err(ShoalError::Corrupt("frame too short".into()));
    }
    if &data[0..8] != magic {
        return Err(ShoalError::Corrupt("bad magic".into()));
    }
    let len = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let crc = u32::from_le_bytes(data[12..16].try_into().unwrap());
    if data.len() != HEADER_LEN + len {
        return Err(ShoalError::Corrupt(format!(
            "frame length mismatch: header says {}, have {}",
            len,
            data.len() - HEADER_LEN
        )));
    }
    let body = &data[HEADER_LEN..];
    if crc32fast::hash(body) != crc {
        return Err(ShoalError::Corrupt("crc mismatch".into()));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_corruption() {
        let magic = b"TESTMAG1";
        let framed = encode_framed(magic, b"hello world");
        assert_eq!(decode_framed(magic, &framed).unwrap(), b"hello world");
        let mut bad = framed.to_vec();
        let n = bad.len();
        bad[n - 1] ^= 0xff;
        assert!(decode_framed(magic, &bad).is_err());
        let mut short = framed.to_vec();
        short.truncate(n - 3);
        assert!(decode_framed(magic, &short).is_err());
    }
}
