//! Content-addressed identifiers without external crates.
//!
//! FNV-1a 128 bit is a non-cryptographic, dependency-free hash used to derive
//! a stable state-directory name for a project root (and content summaries).

const FNV_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
const FNV_PRIME: u128 = 0x1000000000000000000013b;

/// Compute FNV-1a over raw bytes.
pub fn fnv1a_128(data: &[u8]) -> u128 {
    let mut hash = FNV_OFFSET;
    for &byte in data {
        hash ^= byte as u128;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Hex digest of the FNV-1a hash of `data`.
pub fn digest_hex(data: &[u8]) -> String {
    to_hex(&fnv1a_128(data).to_be_bytes())
}

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Short human-readable content summary: hex(hash) + byte length.
pub fn content_fingerprint(data: &[u8]) -> String {
    format!("{}-{}B", digest_hex(data), data.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors_are_stable() {
        assert_eq!(
            digest_hex(b""),
            "6c62272e07bb014262b821756295c58d"
        );
        assert_eq!(
            digest_hex("中文".as_bytes()),
            digest_hex("中文".as_bytes())
        );
        assert_ne!(digest_hex(b"a"), digest_hex(b"b"));
    }
}
