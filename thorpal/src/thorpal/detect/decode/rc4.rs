//! RC4 stream cipher (KSA + PRGA).
//!
//! RC4 is symmetric and self-inverting for a given key, so the same routine both
//! encrypts and decrypts. Malware uses it with an embedded key to
//! encrypt payloads/config; when both the key and the ciphertext are statically
//! recoverable we can replay RC4 to recover the plaintext.

/// Decrypt (or encrypt) `data` with RC4 under `key`.
///
/// Runs the standard Key-Scheduling Algorithm (KSA) then the Pseudo-Random
/// Generation Algorithm (PRGA), XORing the keystream over the data. An empty key
/// returns the data unchanged (RC4 is undefined for a zero-length key).
///
/// # Arguments
///
/// * `key` - The RC4 key bytes
/// * `data` - The bytes to transform (ciphertext when decrypting)
#[must_use]
pub fn decrypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    // a zero-length key has no defined schedule — leave the data untouched
    if key.is_empty() {
        return data.to_vec();
    }
    // KSA: initialize the permutation S to the identity, then key-mix it
    let mut s: [u8; 256] = [0; 256];
    for (i, slot) in s.iter_mut().enumerate() {
        *slot = i as u8;
    }
    let mut j: u8 = 0;
    for i in 0..256 {
        // j += S[i] + key[i mod keylen]; then swap S[i], S[j]
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    // PRGA: generate the keystream and XOR it over the data
    let mut out = Vec::with_capacity(data.len());
    let mut i: u8 = 0;
    let mut j: u8 = 0;
    for &byte in data {
        // advance the two indices and swap
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        // the keystream byte is S[(S[i] + S[j]) mod 256]
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        out.push(byte ^ k);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical RC4 test vector (key "Key" → "Plaintext").
    #[test]
    fn known_vector() {
        // RFC/Wikipedia RC4 test vector
        let ciphertext = [0xBB, 0xF3, 0x16, 0xE8, 0xD9, 0x40, 0xAF, 0x0A, 0xD3];
        let recovered = decrypt(b"Key", &ciphertext);
        assert_eq!(recovered, b"Plaintext");
        // and it round-trips (RC4 is symmetric)
        assert_eq!(decrypt(b"Key", &recovered), ciphertext);
    }

    /// An empty key is a no-op.
    #[test]
    fn empty_key_is_noop() {
        assert_eq!(decrypt(&[], b"abc"), b"abc");
    }
}
