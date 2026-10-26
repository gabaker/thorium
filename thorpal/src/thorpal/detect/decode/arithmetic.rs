//! Single-byte arithmetic decoders (ADD / SUB).
//!
//! A decoder loop that applies `sub al, k` or `add al, k` to each byte is a common
//! stack-string / string-decrypt idiom. These primitives **mirror the loop's own
//! operation** (we replay exactly what the malware does), so `sub_byte` subtracts
//! and `add_byte` adds — both wrapping, like the 8-bit register op.

/// Subtract a single-byte `key` from every byte of `data` (wrapping).
///
/// Mirrors a `sub <byte>, key` decoder loop.
///
/// # Arguments
///
/// * `key` - The single-byte subtrahend
/// * `data` - The bytes to transform
#[must_use]
pub fn sub_byte(key: u8, data: &[u8]) -> Vec<u8> {
    // wrapping subtraction matches the 8-bit register/memory op
    data.iter().map(|b| b.wrapping_sub(key)).collect()
}

/// Add a single-byte `key` to every byte of `data` (wrapping).
///
/// Mirrors an `add <byte>, key` decoder loop.
///
/// # Arguments
///
/// * `key` - The single-byte addend
/// * `data` - The bytes to transform
#[must_use]
pub fn add_byte(key: u8, data: &[u8]) -> Vec<u8> {
    // wrapping addition matches the 8-bit register/memory op
    data.iter().map(|b| b.wrapping_add(key)).collect()
}

/// Subtract every byte of `data` from a single-byte `key` (wrapping).
///
/// Mirrors a `mov al, key; sub al, <byte>` decoder loop -- `(K - b) & 0xff` --
/// which, unlike [`sub_byte`], is its own inverse: `K - (K - b) = b`. Seen in
/// the Lumma loader's string decode among others.
///
/// # Arguments
///
/// * `key` - The single-byte minuend
/// * `data` - The bytes to transform
#[must_use]
pub fn sub_from_key(key: u8, data: &[u8]) -> Vec<u8> {
    // wrapping subtraction from the key matches the 8-bit register op
    data.iter().map(|b| key.wrapping_sub(*b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SUB and ADD are inverses of each other with the same key.
    #[test]
    fn sub_and_add_are_inverses() {
        let plain = b"cmd.exe /c whoami";
        // encode by adding, decode by subtracting (and vice-versa)
        let enc = add_byte(0x11, plain);
        assert_ne!(&enc, plain);
        assert_eq!(sub_byte(0x11, &enc), plain);
    }

    /// Both ops wrap at the byte boundary.
    #[test]
    fn ops_wrap() {
        assert_eq!(add_byte(0x02, &[0xff]), vec![0x01]);
        assert_eq!(sub_byte(0x02, &[0x01]), vec![0xff]);
    }

    /// Subtract-from-key is its own inverse: `K - (K - b) = b`.
    #[test]
    fn sub_from_key_is_self_inverse() {
        let plain = b"http://evil.example/gate.php";
        // one application encodes, the second decodes
        let enc = sub_from_key(0x21, plain);
        assert_ne!(&enc, plain);
        assert_eq!(sub_from_key(0x21, &enc), plain);
        // the op wraps like the 8-bit register subtraction
        assert_eq!(sub_from_key(0x02, &[0x05]), vec![0xfd]);
    }

    /// The Lumma loader vector: `(0x21 - b) & 0xff` recovers the plaintext.
    #[test]
    fn sub_from_key_decodes_the_lumma_shape() {
        let plain = b"LoadLibraryA";
        // encode the way the loader stores it: each byte is `0x21 - plain`
        let enc: Vec<u8> = plain.iter().map(|b| 0x21u8.wrapping_sub(*b)).collect();
        assert_eq!(sub_from_key(0x21, &enc), plain);
    }
}
