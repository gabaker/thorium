//! RC6 block cipher (RC6-32/20/b decryption).
//!
//! RC6 is a symmetric 128-bit block cipher (a former AES finalist) built on RC5. It
//! operates on four 32-bit words (a 16-byte block) over 20 rounds, driving its key
//! schedule from the same magic constants as RC5: `P = 0xB7E15163` (derived from *e*)
//! and `Q = 0x9E3779B9` (the golden ratio). Malware uses it — like RC4 — with an
//! embedded key to encrypt payloads/config; when both the key and the ciphertext are
//! statically recoverable we can replay the decryption to recover the plaintext.
//!
//! Only decryption is implemented (recovery is one-directional). The word packing is
//! little-endian, matching the RC6 reference implementation and the official test
//! vectors (see the unit tests).

/// The RC6 key-schedule magic constant `P` (derived from the base of natural logs, *e*).
const P: u32 = 0xB7E1_5163;
/// The RC6 key-schedule magic constant `Q` (derived from the golden ratio, φ).
const Q: u32 = 0x9E37_79B9;
/// The number of rounds for the standard RC6-32/20/b parameterization.
const ROUNDS: usize = 20;
/// The number of round-key words the schedule produces (`2r + 4`).
const SCHEDULE_WORDS: usize = 2 * ROUNDS + 4;
/// The RC6 block size in bytes (four 32-bit words).
const BLOCK: usize = 16;
/// `lg(w)` for a 32-bit word — the fixed rotation amount in the round function.
const LGW: u32 = 5;

/// Expand an RC6 key into its `2r + 4` round-key words (the array `S`).
///
/// Runs the standard RC5/RC6 key schedule: pack the key into little-endian words `L`
/// (zero-padding the final partial word), seed `S` with `S[0] = P`, `S[i] = S[i-1] + Q`,
/// then mix `L` into `S` over `3·max(c, 2r+4)` iterations.
///
/// # Arguments
///
/// * `key` - The RC6 key bytes (any non-zero length; padded up to a word boundary)
fn key_schedule(key: &[u8]) -> Vec<u32> {
    // pack the key into c little-endian words (c >= 1), zero-padding a partial tail word
    let c = key.len().div_ceil(4).max(1);
    let mut l = vec![0u32; c];
    for (idx, chunk) in key.chunks(4).enumerate() {
        // copy the (possibly short) chunk into a 4-byte buffer, leaving the rest zero
        let mut buf = [0u8; 4];
        buf[..chunk.len()].copy_from_slice(chunk);
        l[idx] = u32::from_le_bytes(buf);
    }
    // seed S: S[0] = P, S[i] = S[i-1] + Q (mod 2^32)
    let mut s = vec![0u32; SCHEDULE_WORDS];
    s[0] = P;
    for i in 1..SCHEDULE_WORDS {
        s[i] = s[i - 1].wrapping_add(Q);
    }
    // mix the key words L into the schedule S over 3·max(c, 2r+4) iterations
    let (mut a, mut b, mut i, mut j) = (0u32, 0u32, 0usize, 0usize);
    for _ in 0..(3 * c.max(SCHEDULE_WORDS)) {
        // A = S[i] = (S[i] + A + B) <<< 3
        a = s[i].wrapping_add(a).wrapping_add(b).rotate_left(3);
        s[i] = a;
        // B = L[j] = (L[j] + A + B) <<< (A + B)
        let ab = a.wrapping_add(b);
        b = l[j].wrapping_add(ab).rotate_left(ab);
        l[j] = b;
        // advance the two ring indices
        i = (i + 1) % SCHEDULE_WORDS;
        j = (j + 1) % c;
    }
    s
}

/// Decrypt a single 16-byte RC6 block with an expanded schedule `s`.
///
/// The exact inverse of RC6 encryption: undo the post-whitening (`A -= S[2r+2]`,
/// `C -= S[2r+3]`), run the `r` rounds in reverse (un-rotating with the quadratic
/// `f(x) = x·(2x+1) <<< 5` values), then undo the pre-whitening (`B -= S[0]`,
/// `D -= S[1]`).
///
/// # Arguments
///
/// * `block` - The 16 ciphertext bytes
/// * `s` - The expanded round-key schedule from [`key_schedule`]
fn decrypt_block(block: [u8; BLOCK], s: &[u32]) -> [u8; BLOCK] {
    // load the four little-endian words A, B, C, D
    let mut a = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
    let mut b = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
    let mut c = u32::from_le_bytes([block[8], block[9], block[10], block[11]]);
    let mut d = u32::from_le_bytes([block[12], block[13], block[14], block[15]]);
    // undo the final post-whitening
    c = c.wrapping_sub(s[2 * ROUNDS + 3]);
    a = a.wrapping_sub(s[2 * ROUNDS + 2]);
    // run the rounds in reverse
    for i in (1..=ROUNDS).rev() {
        // reverse the end-of-round word rotation (A,B,C,D) = (B,C,D,A)
        let (na, nb, nc, nd) = (d, a, b, c);
        a = na;
        b = nb;
        c = nc;
        d = nd;
        // the two quadratic mixing values for this round: t = f(B), u = f(D),
        // where f(x) = (x·(2x+1)) <<< lg(w)
        let t = b
            .wrapping_mul(b.wrapping_mul(2).wrapping_add(1))
            .rotate_left(LGW);
        let u = d
            .wrapping_mul(d.wrapping_mul(2).wrapping_add(1))
            .rotate_left(LGW);
        // invert C = ((C ^ u) <<< t) + S[2i+1] and A = ((A ^ t) <<< u) + S[2i]
        c = c.wrapping_sub(s[2 * i + 1]).rotate_right(t) ^ u;
        a = a.wrapping_sub(s[2 * i]).rotate_right(u) ^ t;
    }
    // undo the initial pre-whitening
    d = d.wrapping_sub(s[1]);
    b = b.wrapping_sub(s[0]);
    // store the four words back out little-endian
    let mut out = [0u8; BLOCK];
    out[0..4].copy_from_slice(&a.to_le_bytes());
    out[4..8].copy_from_slice(&b.to_le_bytes());
    out[8..12].copy_from_slice(&c.to_le_bytes());
    out[12..16].copy_from_slice(&d.to_le_bytes());
    out
}

/// Decrypt `data` with RC6 in **ECB** mode under `key`.
///
/// Decrypts the whole-block prefix of `data` (any trailing partial block is ignored,
/// since RC6 only operates on full 16-byte blocks). An empty key returns the data
/// unchanged (RC6 has no defined schedule for a zero-length key).
///
/// # Arguments
///
/// * `key` - The RC6 key bytes
/// * `data` - The ciphertext to decrypt
#[must_use]
pub fn decrypt_ecb(key: &[u8], data: &[u8]) -> Vec<u8> {
    // a zero-length key has no defined schedule — leave the data untouched
    if key.is_empty() {
        return data.to_vec();
    }
    // expand the key once, then decrypt each full block independently
    let s = key_schedule(key);
    let mut out = Vec::with_capacity(data.len());
    for block in data.chunks_exact(BLOCK) {
        // chunks_exact guarantees a 16-byte slice, so the conversion cannot fail
        let arr: [u8; BLOCK] = block.try_into().expect("chunks_exact yields BLOCK bytes");
        out.extend_from_slice(&decrypt_block(arr, &s));
    }
    out
}

/// Decrypt `data` with RC6 in **CBC** mode (zero IV) under `key`.
///
/// After decrypting each block, XOR it with the previous ciphertext block (the IV,
/// zero for the first block) — the common CBC construction. Decrypts the whole-block
/// prefix of `data`; an empty key returns the data unchanged.
///
/// # Arguments
///
/// * `key` - The RC6 key bytes
/// * `data` - The ciphertext to decrypt
#[must_use]
pub fn decrypt_cbc(key: &[u8], data: &[u8]) -> Vec<u8> {
    // a zero-length key has no defined schedule — leave the data untouched
    if key.is_empty() {
        return data.to_vec();
    }
    // expand the key once; the IV is the all-zero block
    let s = key_schedule(key);
    let mut out = Vec::with_capacity(data.len());
    let mut prev = [0u8; BLOCK];
    for block in data.chunks_exact(BLOCK) {
        let arr: [u8; BLOCK] = block.try_into().expect("chunks_exact yields BLOCK bytes");
        // decrypt then XOR with the previous ciphertext block (CBC chaining)
        let mut pt = decrypt_block(arr, &s);
        for (p, iv) in pt.iter_mut().zip(prev.iter()) {
            *p ^= *iv;
        }
        out.extend_from_slice(&pt);
        // this ciphertext block is the next block's chaining value
        prev = arr;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encrypt a single block — the exact inverse of [`decrypt_block`] — for the
    /// known-answer and round-trip tests.
    fn encrypt_block(block: [u8; BLOCK], s: &[u32]) -> [u8; BLOCK] {
        let mut a = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        let mut b = u32::from_le_bytes([block[4], block[5], block[6], block[7]]);
        let mut c = u32::from_le_bytes([block[8], block[9], block[10], block[11]]);
        let mut d = u32::from_le_bytes([block[12], block[13], block[14], block[15]]);
        // pre-whitening
        b = b.wrapping_add(s[0]);
        d = d.wrapping_add(s[1]);
        for i in 1..=ROUNDS {
            let t = b
                .wrapping_mul(b.wrapping_mul(2).wrapping_add(1))
                .rotate_left(LGW);
            let u = d
                .wrapping_mul(d.wrapping_mul(2).wrapping_add(1))
                .rotate_left(LGW);
            a = (a ^ t).rotate_left(u).wrapping_add(s[2 * i]);
            c = (c ^ u).rotate_left(t).wrapping_add(s[2 * i + 1]);
            // rotate the words (A,B,C,D) = (B,C,D,A)
            let (na, nb, nc, nd) = (b, c, d, a);
            a = na;
            b = nb;
            c = nc;
            d = nd;
        }
        // post-whitening
        a = a.wrapping_add(s[2 * ROUNDS + 2]);
        c = c.wrapping_add(s[2 * ROUNDS + 3]);
        let mut out = [0u8; BLOCK];
        out[0..4].copy_from_slice(&a.to_le_bytes());
        out[4..8].copy_from_slice(&b.to_le_bytes());
        out[8..12].copy_from_slice(&c.to_le_bytes());
        out[12..16].copy_from_slice(&d.to_le_bytes());
        out
    }

    /// The official RC6-32/20/16 test vectors (from the RC6 specification): a decrypt of
    /// the known ciphertext yields the known plaintext.
    #[test]
    fn known_vectors() {
        // vector 1: all-zero key and plaintext
        let key = [0u8; 16];
        let ct = hex("8fc3a53656b1f778c129df4e9848a41e");
        assert_eq!(decrypt_ecb(&key, &ct), vec![0u8; 16]);

        // vector 2: structured key and plaintext
        let key = hex("0123456789abcdef0112233445566778");
        let pt = hex("02132435465768798a9bacbdcedfe0f1");
        let ct = hex("524e192f4715c6231f51f6367ea43f18");
        assert_eq!(decrypt_ecb(&key, &ct), pt);
    }

    /// ECB encrypt→decrypt round-trips over multiple blocks (trailing partial ignored).
    #[test]
    fn ecb_round_trips_multiblock() {
        let key = hex("0123456789abcdef0112233445566778");
        let s = key_schedule(&key);
        // two full blocks of plaintext plus a trailing partial block
        let mut pt = Vec::new();
        pt.extend_from_slice(&encrypt_block(*b"whole-block AAAA", &s));
        pt.extend_from_slice(&encrypt_block(*b"whole-block BBBB", &s));
        pt.extend_from_slice(b"tail"); // partial block, must be dropped
        let dec = decrypt_ecb(&key, &pt);
        assert_eq!(dec.len(), 32);
        assert_eq!(&dec[0..16], b"whole-block AAAA");
        assert_eq!(&dec[16..32], b"whole-block BBBB");
    }

    /// CBC decryption inverts CBC encryption under a zero IV.
    #[test]
    fn cbc_round_trips() {
        let key = hex("0123456789abcdef0112233445566778");
        let s = key_schedule(&key);
        let plain = *b"CBC chaining test block padding!";
        // CBC-encrypt the two blocks with a zero IV
        let mut ct = Vec::new();
        let mut prev = [0u8; BLOCK];
        for block in plain.chunks_exact(BLOCK) {
            let mut b: [u8; BLOCK] = block.try_into().unwrap();
            for (x, iv) in b.iter_mut().zip(prev.iter()) {
                *x ^= *iv;
            }
            let c = encrypt_block(b, &s);
            ct.extend_from_slice(&c);
            prev = c;
        }
        assert_eq!(decrypt_cbc(&key, &ct), plain.to_vec());
    }

    /// An empty key is a no-op for both modes.
    #[test]
    fn empty_key_is_noop() {
        let data = b"some ciphertext!";
        assert_eq!(decrypt_ecb(&[], data), data);
        assert_eq!(decrypt_cbc(&[], data), data);
    }

    /// Decode a hex string to bytes (test helper).
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
