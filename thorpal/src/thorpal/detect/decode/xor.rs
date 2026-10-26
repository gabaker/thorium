//! XOR decoders: single-byte and repeating/key-buffer keystream XOR.
//!
//! XOR is the most common string/payload obfuscation in malware because it is
//! trivial and self-inverting. A single-byte key XORs every byte with one value; a
//! keystream (repeating key or a key buffer) XORs byte `i` with `key[i % len]`.

/// XOR every byte of `data` with a single-byte `key`.
///
/// # Arguments
///
/// * `key` - The single-byte XOR key
/// * `data` - The bytes to transform
#[must_use]
pub fn single_byte(key: u8, data: &[u8]) -> Vec<u8> {
    // XOR is self-inverting, so this both encodes and decodes
    data.iter().map(|b| b ^ key).collect()
}

/// XOR `data` with a repeating multi-byte `key` (byte `i` ⊕ `key[i % key.len()]`).
///
/// Covers both a short repeating key and a full key buffer the same length as the
/// data. An empty key is a no-op (returns the data unchanged).
///
/// # Arguments
///
/// * `key` - The key bytes to repeat across the data
/// * `data` - The bytes to transform
#[must_use]
pub fn keystream(key: &[u8], data: &[u8]) -> Vec<u8> {
    // an empty key cannot transform anything — return the input verbatim
    if key.is_empty() {
        return data.to_vec();
    }
    // XOR each byte with the corresponding (wrapping) key byte
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ key[i % key.len()])
        .collect()
}

/// XOR a self-keyed buffer: the first `key_len` bytes are the key, and the
/// rest is the ciphertext XORed with it (`out[i] = data[key_len + i] ^
/// key[i % key_len]`).
///
/// This is the LummaC2 shape: a decoded blob carries its own key up front
/// (commonly 32 bytes) followed by the keystream-XORed plaintext. An input
/// shorter than (or equal to) the key -- or a `key_len` of zero -- has no
/// ciphertext to decode and returns an empty buffer.
///
/// # Arguments
///
/// * `key_len` - The number of leading bytes that are the key
/// * `data` - The self-keyed buffer (key prefix followed by ciphertext)
#[must_use]
pub fn self_keyed(key_len: usize, data: &[u8]) -> Vec<u8> {
    // a zero-length key or an input with no ciphertext decodes to nothing
    if key_len == 0 || data.len() <= key_len {
        return Vec::new();
    }
    // split the leading key off and keystream the remainder with it
    let (key, body) = data.split_at(key_len);
    keystream(key, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-byte XOR round-trips (XOR with the same key twice is the identity).
    #[test]
    fn single_byte_round_trips() {
        let plain = b"http://evil.example/gate";
        let enc = single_byte(0x2a, plain);
        assert_ne!(&enc, plain);
        assert_eq!(single_byte(0x2a, &enc), plain);
    }

    /// Keystream XOR repeats the key and round-trips.
    #[test]
    fn keystream_repeats_and_round_trips() {
        let plain = b"SOFTWARE\\Microsoft\\Windows";
        let key = b"key";
        let enc = keystream(key, plain);
        // byte 0 uses key[0], byte 3 wraps back to key[0]
        assert_eq!(enc[0], plain[0] ^ b'k');
        assert_eq!(enc[3], plain[3] ^ b'k');
        assert_eq!(keystream(key, &enc), plain);
    }

    /// An empty key leaves the data unchanged.
    #[test]
    fn empty_key_is_noop() {
        assert_eq!(keystream(&[], b"abc"), b"abc");
    }

    /// A self-keyed buffer (the LummaC2 shape) round-trips: a 32-byte key
    /// prefix followed by `keystream(key, plaintext)` decodes to the plaintext.
    #[test]
    fn self_keyed_round_trips_the_lummac2_shape() {
        let plain = b"https://lumma-c2.example/api|build42|logs";
        // a 32-byte key prefix, exactly the shape the decoded blob carries
        let key: Vec<u8> = (0u8..32).map(|i| i.wrapping_mul(7) ^ 0x5a).collect();
        let mut buffer = key.clone();
        buffer.extend_from_slice(&keystream(&key, plain));
        assert_eq!(self_keyed(32, &buffer), plain);
    }

    /// Degenerate inputs decode to nothing instead of panicking.
    #[test]
    fn self_keyed_handles_short_inputs() {
        // a zero key length has no key to strip
        assert!(self_keyed(0, b"abc").is_empty());
        // an input no longer than the key carries no ciphertext
        assert!(self_keyed(4, b"abcd").is_empty());
        assert!(self_keyed(8, b"abcd").is_empty());
    }
}
