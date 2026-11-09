//! Checksum helpers. ReefDB uses CRC32 (IEEE, via `crc32fast`) for WAL frames
//! and segment blocks: cheap to compute, hardware-accelerated on common CPUs,
//! and strong enough to detect the torn/bit-rotted objects we care about.
//! End-to-end content trust is the object store's job (ETag/SSE); CRC here
//! protects against incomplete uploads and our own encoding bugs.

/// Compute the CRC32 (IEEE) checksum of a byte slice.
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // Standard CRC32 check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn detects_change() {
        let a = crc32(b"hello world");
        let b = crc32(b"hello worle");
        assert_ne!(a, b);
        assert_eq!(crc32(b""), 0);
    }
}
