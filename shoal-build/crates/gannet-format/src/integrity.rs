//! Content integrity helpers: every immutable file referenced by a manifest
//! carries a SHA-256 digest, stored as lowercase hex. Readers verify on
//! download (cache fill), turning silent corruption or truncated uploads
//! into explicit, retryable errors.

use sha2::{Digest, Sha256};

/// SHA-256 of `data` as lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Constant-shape verification against an expected lowercase/uppercase hex
/// digest. Returns `false` on any mismatch, including malformed input.
pub fn verify_sha256_hex(data: &[u8], expected_hex: &str) -> bool {
    sha256_hex(data).eq_ignore_ascii_case(expected_hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verification() {
        let digest = sha256_hex(b"hello");
        assert!(verify_sha256_hex(b"hello", &digest));
        assert!(verify_sha256_hex(b"hello", &digest.to_uppercase()));
        assert!(!verify_sha256_hex(b"hellp", &digest));
        assert!(!verify_sha256_hex(b"hello", "not-hex"));
    }
}
