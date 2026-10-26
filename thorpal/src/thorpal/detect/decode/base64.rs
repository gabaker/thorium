//! base64 decoder (standard and URL-safe alphabets).
//!
//! Malware frequently base64-wraps an encrypted key or payload (cipher data carried
//! in a dropped note, a config blob). Decoding the wrapper is the first step before a
//! second-stage XOR/RC4 decode, so this accepts either alphabet (standard `+`/`/`
//! and URL-safe `-`/`_`) and ignores surrounding whitespace and `=` padding.

/// The 0-63 value of a base64 character under either alphabet, if valid.
///
/// # Arguments
///
/// * `c` - The candidate base64 character
fn value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        // both alphabets share 0-61; only the last two symbols differ
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Decode a base64 string (standard **or** URL-safe), ignoring whitespace and `=`
/// padding. Returns `None` if a non-alphabet byte is encountered.
///
/// # Arguments
///
/// * `input` - The base64 text to decode
#[must_use]
pub fn decode(input: &str) -> Option<Vec<u8>> {
    // accumulate 6-bit groups into a bit buffer, emitting a byte per 8 bits
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &c in input.as_bytes() {
        // skip padding and any whitespace/formatting between groups
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        // reject anything outside both alphabets (not real base64)
        let v = value(c)? as u32;
        buf = (buf << 6) | v;
        bits += 6;
        // once we have a full byte, shift it out
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

/// The standard base64 alphabet, indexed by 6-bit value (the inverse of [`value`]).
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes as padded standard base64 (no line wrapping).
///
/// The counterpart to [`decode`], used to render recovered **binary** material in a
/// form an analyst can copy out of the report and feed to another tool — a key or
/// certificate stored as raw bytes has no textual representation of its own.
///
/// # Arguments
///
/// * `bytes` - The raw bytes to encode
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    // three input bytes become four output characters, padded to a 4-char boundary
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // pack the chunk into a 24-bit group, left-aligned so a short tail still
        // occupies the high bits
        let mut group: u32 = 0;
        for (idx, byte) in chunk.iter().enumerate() {
            group |= u32::from(*byte) << (16 - 8 * idx);
        }
        // emit one character per 6-bit field, then pad the fields the tail did not fill
        for idx in 0..=chunk.len() {
            let shift = 18 - 6 * idx;
            out.push(ALPHABET[((group >> shift) & 0x3f) as usize] as char);
        }
        for _ in chunk.len()..3 {
            out.push('=');
        }
    }
    out
}

/// Wrap base64 text at `width` characters per line, the layout PEM armor uses.
///
/// # Arguments
///
/// * `text` - The unwrapped base64 text
/// * `width` - The characters per line
#[must_use]
pub fn wrap(text: &str, width: usize) -> String {
    // a zero width would loop forever; treat it as "do not wrap"
    if width == 0 {
        return text.to_string();
    }
    // base64 is ASCII, so byte chunks are also character chunks
    text.as_bytes()
        .chunks(width)
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

// ===========================================================================
// Non-standard alphabets
// ===========================================================================

/// The number of symbols in a base64 alphabet.
pub const ALPHABET_LEN: usize = 64;

/// The minimum alphanumeric symbols a 64-byte run needs to read as an alphabet
/// table rather than as an incidental run of distinct bytes.
///
/// Every published base64 variant keeps `A-Za-z0-9` (62 symbols) and varies only the
/// last two; a permuted alphabet reorders them but keeps the multiset. Allowing a
/// little slack covers variants that also substitute a symbol or two.
const MIN_ALPHABET_ALNUM: usize = 56;

/// The max alphabet tables recovered from one binary (bounds the decode fan-out).
const MAX_ALPHABETS: usize = 8;

/// A base64 alphabet: the 64 symbols in value order, plus the padding character.
///
/// Programs that want their base64 to resist a casual `base64 -d` permute the
/// alphabet and sometimes swap the pad character. The permutation is not
/// recoverable from ciphertext alone (it is a 64! search), so the useful generic
/// move is to decode with alphabets that are either standard, a systematic variant,
/// or **read out of the binary itself** — a program using a custom alphabet has to
/// carry the table, and [`find_alphabets`] looks for exactly that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alphabet {
    /// The 64 symbols indexed by their 6-bit value.
    pub table: [u8; ALPHABET_LEN],
    /// The padding character, if this alphabet pads.
    pub pad: Option<u8>,
    /// A short label naming the alphabet for the report (e.g. `"base64"`).
    pub label: String,
}

impl Alphabet {
    /// Build an alphabet from a 64-symbol table.
    ///
    /// Returns `None` when the table has a repeated symbol — a decode table has to be
    /// a bijection or the mapping is ambiguous.
    ///
    /// # Arguments
    ///
    /// * `table` - The 64 symbols in value order
    /// * `pad` - The padding character, if any
    /// * `label` - The label to report this alphabet under
    #[must_use]
    pub fn new(
        table: [u8; ALPHABET_LEN],
        pad: Option<u8>,
        label: impl Into<String>,
    ) -> Option<Self> {
        // a repeated symbol makes the reverse mapping ambiguous
        let mut seen = [false; 256];
        for &symbol in &table {
            if std::mem::replace(&mut seen[symbol as usize], true) {
                return None;
            }
        }
        Some(Self {
            table,
            pad,
            label: label.into(),
        })
    }

    /// The standard RFC 4648 alphabet.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            table: *ALPHABET,
            pad: Some(b'='),
            label: "base64".to_string(),
        }
    }

    /// The RFC 4648 URL-safe alphabet (`-`/`_` for the last two symbols).
    #[must_use]
    pub fn url_safe() -> Self {
        let mut table = *ALPHABET;
        table[62] = b'-';
        table[63] = b'_';
        Self {
            table,
            pad: Some(b'='),
            label: "base64 (URL-safe)".to_string(),
        }
    }

    /// This alphabet rotated left by `n` positions.
    ///
    /// Rotation is the cheapest way to make base64 output unreadable while keeping
    /// the same symbol set, so the whole 64-rotation family is worth trying — but
    /// only as a *speculative* decode, accepted on the strength of its output rather
    /// than on any evidence in the binary (see [`crate::thorpal::detect::decode::chain`]).
    ///
    /// # Arguments
    ///
    /// * `n` - The number of positions to rotate left by
    #[must_use]
    pub fn rotated(&self, n: usize) -> Self {
        let mut table = [0u8; ALPHABET_LEN];
        for (idx, slot) in table.iter_mut().enumerate() {
            *slot = self.table[(idx + n) % ALPHABET_LEN];
        }
        Self {
            table,
            pad: self.pad,
            label: format!("base64 (alphabet rotated {n})"),
        }
    }

    /// The reverse mapping from symbol to 6-bit value.
    fn reverse(&self) -> [Option<u8>; 256] {
        let mut rev = [None; 256];
        for (value, &symbol) in self.table.iter().enumerate() {
            rev[symbol as usize] = Some(value as u8);
        }
        rev
    }

    /// Whether `byte` is one of this alphabet's symbols.
    ///
    /// # Arguments
    ///
    /// * `byte` - The byte to test
    #[must_use]
    pub fn contains(&self, byte: u8) -> bool {
        self.table.contains(&byte)
    }
}

/// Decode base64 text under an arbitrary alphabet, ignoring whitespace and padding.
///
/// The alphabet-parameterized counterpart to [`decode`]. Returns `None` if a byte is
/// neither a symbol, the pad character, nor whitespace, or if the run carries no
/// complete byte.
///
/// # Arguments
///
/// * `alphabet` - The alphabet to decode under
/// * `input` - The encoded bytes
#[must_use]
pub fn decode_with(alphabet: &Alphabet, input: &[u8]) -> Option<Vec<u8>> {
    let rev = alphabet.reverse();
    // accumulate 6-bit groups into a bit buffer, emitting a byte per 8 bits
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &c in input {
        // skip padding and any whitespace/formatting between groups
        if Some(c) == alphabet.pad || c.is_ascii_whitespace() {
            continue;
        }
        // reject anything outside the alphabet (this run is not encoded under it)
        let v = u32::from(rev[c as usize]?);
        buf = (buf << 6) | v;
        bits += 6;
        // once we have a full byte, shift it out
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Whether every byte of `input` is a symbol of `alphabet`, its pad, or whitespace.
///
/// The precondition for treating a run as encoded under this alphabet: a partial
/// match means the run is something else that happens to share characters.
///
/// # Arguments
///
/// * `alphabet` - The alphabet to test against
/// * `input` - The candidate encoded bytes
#[must_use]
pub fn fits_alphabet(alphabet: &Alphabet, input: &[u8]) -> bool {
    input
        .iter()
        .all(|&c| alphabet.contains(c) || Some(c) == alphabet.pad || c.is_ascii_whitespace())
}

/// Recover base64 alphabet tables stored as literals in a byte buffer.
///
/// A program decoding with a custom alphabet has to carry the 64-symbol table, and a
/// run of 64 *distinct* graphic bytes that is mostly alphanumeric is what that table
/// looks like on disk. Requiring the run to be maximal (neither neighbour could join
/// it without repeating a symbol) keeps a longer distinct run from yielding 64
/// overlapping "alphabets".
///
/// Returns `(offset, alphabet)` per table found, capped at [`MAX_ALPHABETS`].
///
/// # Arguments
///
/// * `bytes` - The raw bytes to scan
#[must_use]
pub fn find_alphabets(bytes: &[u8]) -> Vec<(usize, Alphabet)> {
    let mut out: Vec<(usize, Alphabet)> = Vec::new();
    if bytes.len() < ALPHABET_LEN {
        return out;
    }
    for start in 0..=bytes.len() - ALPHABET_LEN {
        let window = &bytes[start..start + ALPHABET_LEN];
        // every symbol must be graphic and distinct, and the set mostly alphanumeric
        if !window.iter().all(u8::is_ascii_graphic) {
            continue;
        }
        if window.iter().filter(|b| b.is_ascii_alphanumeric()).count() < MIN_ALPHABET_ALNUM {
            continue;
        }
        let mut seen = [false; 256];
        if window
            .iter()
            .any(|&b| std::mem::replace(&mut seen[b as usize], true))
        {
            continue;
        }
        // the run must be maximal: a neighbour that could extend it means this window
        // is an arbitrary slice of a longer distinct run, not a delimited table
        let extends = |idx: Option<usize>| match idx.and_then(|i| bytes.get(i)) {
            Some(&b) => b.is_ascii_graphic() && !seen[b as usize],
            None => false,
        };
        if extends(start.checked_sub(1)) || extends(Some(start + ALPHABET_LEN)) {
            continue;
        }
        let mut table = [0u8; ALPHABET_LEN];
        table.copy_from_slice(window);
        // the standard/URL-safe tables are already tried directly; only variants add value
        if table == *ALPHABET || table == Alphabet::url_safe().table {
            continue;
        }
        if let Some(alphabet) = Alphabet::new(
            table,
            Some(b'='),
            format!("base64 (alphabet at 0x{start:x})"),
        ) {
            out.push((start, alphabet));
        }
        if out.len() >= MAX_ALPHABETS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Standard base64 decodes, tolerating padding and whitespace.
    #[test]
    fn decodes_standard() {
        assert_eq!(decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode("aGVs bG8=\n").unwrap(), b"hello");
    }

    /// URL-safe base64 decodes via the shared value table.
    #[test]
    fn decodes_url_safe() {
        // ">>>?" encodes to "Pj4+Pw==" (std) / "Pj4-Pw==" (url-safe)
        assert_eq!(decode("Pj4-Pw==").unwrap(), b">>>?");
    }

    /// A non-alphabet byte makes the decode fail.
    #[test]
    fn rejects_non_base64() {
        assert!(decode("not base64!@#$").is_none());
    }

    /// `encode` pads each tail length correctly and round-trips through `decode`.
    #[test]
    fn encodes_every_tail_length() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"h"), "aA==");
        assert_eq!(encode(b"he"), "aGU=");
        assert_eq!(encode(b"hel"), "aGVs");
        assert_eq!(encode(b"hello"), "aGVsbG8=");
        // every byte value round-trips, including the high-bit ones a key is full of
        let raw: Vec<u8> = (0u8..=255).collect();
        assert_eq!(decode(&encode(&raw)).unwrap(), raw);
    }

    /// `wrap` breaks base64 into PEM-width lines and leaves a short body alone.
    #[test]
    fn wrap_breaks_into_lines() {
        assert_eq!(wrap("aGVsbG8=", 4), "aGVs\nbG8=");
        assert_eq!(wrap("aGVsbG8=", 64), "aGVsbG8=");
        // a zero width is treated as "do not wrap" rather than looping forever
        assert_eq!(wrap("aGVsbG8=", 0), "aGVsbG8=");
    }

    /// Encode text under an arbitrary alphabet (test helper mirroring [`encode`]).
    fn encode_with(alphabet: &Alphabet, bytes: &[u8]) -> Vec<u8> {
        let std = Alphabet::standard();
        // encode normally, then map each standard symbol onto the target alphabet
        encode(bytes)
            .bytes()
            .map(|c| match std.table.iter().position(|&s| s == c) {
                Some(value) => alphabet.table[value],
                // the pad character carries through unchanged
                None => c,
            })
            .collect()
    }

    /// The standard alphabet decodes identically through both entry points, and a
    /// rotated alphabet round-trips only under the alphabet that produced it.
    #[test]
    fn decode_with_honors_the_alphabet() {
        let std = Alphabet::standard();
        assert_eq!(decode_with(&std, b"aGVsbG8=").unwrap(), b"hello");
        // the URL-safe symbols are rejected by the standard table and vice versa
        assert_eq!(
            decode_with(&Alphabet::url_safe(), b"Pj4-Pw==").unwrap(),
            b">>>?"
        );
        assert!(decode_with(&std, b"Pj4-Pw==").is_none());
        // a rotated alphabet is a different mapping, so the plaintext only comes back
        // under the alphabet that produced it
        let rotated = std.rotated(13);
        let encoded = encode_with(&rotated, b"the quick brown fox");
        assert_eq!(
            decode_with(&rotated, &encoded).unwrap(),
            b"the quick brown fox"
        );
        assert_ne!(decode_with(&std, &encoded).unwrap(), b"the quick brown fox");
        // an empty decode is a failure, not an empty success
        assert!(decode_with(&std, b"==").is_none());
    }

    /// A repeated symbol makes the reverse mapping ambiguous, so the table is rejected.
    #[test]
    fn alphabet_rejects_a_repeated_symbol() {
        let mut table = *ALPHABET;
        table[63] = table[0];
        assert!(Alphabet::new(table, Some(b'='), "broken").is_none());
        assert!(Alphabet::new(*ALPHABET, Some(b'='), "fine").is_some());
    }

    /// `fits_alphabet` is all-or-nothing: one foreign byte disqualifies the run.
    #[test]
    fn fits_alphabet_requires_every_byte() {
        let std = Alphabet::standard();
        assert!(fits_alphabet(&std, b"aGVsbG8=\n"));
        assert!(!fits_alphabet(&std, b"aGVsbG8-"));
    }

    /// A permuted alphabet stored as a delimited literal is recovered; the standard
    /// table, a short run, and a slice of a longer distinct run are not.
    #[test]
    fn find_alphabets_recovers_a_stored_table() {
        // a permuted table: the standard alphabet with its two halves swapped
        let mut table = [0u8; ALPHABET_LEN];
        table[..32].copy_from_slice(&ALPHABET[32..]);
        table[32..].copy_from_slice(&ALPHABET[..32]);
        let mut blob = b"\x00config\x00".to_vec();
        let at = blob.len();
        blob.extend_from_slice(&table);
        blob.extend_from_slice(b"\x00trailer\x00");
        let found = find_alphabets(&blob);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, at);
        assert_eq!(found[0].1.table, table);
        // the standard table is skipped — it is already tried directly
        let mut plain = b"\x00".to_vec();
        plain.extend_from_slice(ALPHABET);
        plain.push(0);
        assert!(find_alphabets(&plain).is_empty());
        // a graphic neighbour the window could have absorbed means this is a slice of
        // a longer run, not a delimited table
        let mut glued = b"!".to_vec();
        glued.extend_from_slice(&table);
        glued.push(0);
        assert!(find_alphabets(&glued).is_empty());
        // a run of mostly-punctuation distinct bytes is not an alphabet table
        let mut punct: Vec<u8> = (b'!'..=b'~').collect();
        punct.insert(0, 0);
        punct.push(0);
        assert!(find_alphabets(&punct).is_empty());
    }
}
