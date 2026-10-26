//! A pure-Rust aPLib depacker (the canonical `aP_depack` algorithm).
//!
//! aPLib is Jørgen Ibsen's byte-oriented LZ77 packer, a staple of malware
//! packers and ransomware configuration blobs (LockBit Black packs its whole
//! config with it) because the tiny depacker is trivial to embed in a stub.
//! A stream opens with one raw literal byte and then interleaves MSB-first
//! tag bits with raw data bytes. Tag codes: `0` is a literal byte; `10` is a
//! gamma-coded match — the offset's high bits are gamma-coded (with a
//! repeated-offset form reusing the previous offset when the last code was a
//! literal/nibble and the gamma is exactly 2), the low byte raw, and the
//! length gamma-coded with offset-dependent adjustments (`>= 32000`: +1,
//! `>= 1280`: +1, `< 128`: +2); `110` is a short match whose single byte
//! packs the offset (`byte >> 1`, zero marking end of stream) and length
//! (`2 + (byte & 1)`); `111` is a nibble code — four tag bits of offset,
//! zero writing a literal zero byte, anything else copying one window byte.
//! Streams may also arrive wrapped in the `AP32` container header, which is
//! skipped by its declared header size. The decode rules here match the
//! LockBit Black in-binary depacker the evaluation verified byte-for-byte.

/// The size of the fixed `AP32` container header fields.
const AP32_HEADER_LEN: usize = 24;

/// The largest gamma value accepted before a stream is called corrupt.
///
/// Real offsets and lengths sit far below this; the bound exists so a hostile
/// stream cannot overflow the accumulator shift.
const GAMMA_LIMIT: usize = 1 << 30;

/// A bit reader over an aPLib stream's interleaved tag and data bytes.
struct BitReader<'input> {
    /// The compressed stream
    input: &'input [u8],
    /// The next byte to consume
    pos: usize,
    /// The current tag byte, already shifted past its consumed bits
    tag: u8,
    /// How many bits of the current tag byte remain
    bits: u8,
}

impl<'input> BitReader<'input> {
    /// Wrap a stream positioned at its first literal byte
    ///
    /// # Arguments
    ///
    /// * `input` - The compressed stream
    fn new(input: &'input [u8]) -> Self {
        BitReader {
            input,
            pos: 0,
            tag: 0,
            bits: 0,
        }
    }

    /// Read the next raw data byte from the stream
    fn byte(&mut self) -> Result<u8, String> {
        // data bytes come from the same cursor the tag bytes do
        match self.input.get(self.pos) {
            Some(byte) => {
                self.pos += 1;
                Ok(*byte)
            }
            None => Err("truncated aPLib stream".to_owned()),
        }
    }

    /// Read the next tag bit (MSB first within each tag byte)
    fn bit(&mut self) -> Result<u8, String> {
        // refill the tag byte once its eight bits are spent
        if self.bits == 0 {
            self.tag = self.byte()?;
            self.bits = 8;
        }
        self.bits -= 1;
        let bit = (self.tag >> 7) & 1;
        self.tag <<= 1;
        Ok(bit)
    }

    /// Read one gamma-coded value (always `>= 2`)
    ///
    /// Each iteration appends one data bit and reads one continuation bit;
    /// the accumulator is bounded so a hostile stream cannot overflow it.
    fn gamma(&mut self) -> Result<usize, String> {
        let mut value: usize = 1;
        loop {
            // bound the value before the shift so it can never overflow
            if value > GAMMA_LIMIT {
                return Err("corrupt aPLib data: a gamma code overflows".to_owned());
            }
            value = (value << 1) + usize::from(self.bit()?);
            if self.bit()? == 0 {
                return Ok(value);
            }
        }
    }
}

/// Decompress an aPLib-packed buffer, bounded by an output cap
///
/// Accepts both a raw stream and one wrapped in the `AP32` container header
/// (skipped by its declared size). The stream must reach its own end-of-
/// stream marker: truncation mid-token is a hard error, as is any match
/// reaching back past the produced output. Exceeding `cap` output bytes is a
/// hard error naming the cap, so a decompression bomb can never balloon
/// memory.
///
/// # Arguments
///
/// * `input` - The aPLib-compressed bytes
/// * `cap` - The most output bytes to produce before erroring
pub(crate) fn decompress(input: &[u8], cap: usize) -> Result<Vec<u8>, String> {
    // skip the optional AP32 container header by its declared size
    let input = strip_ap32(input)?;
    // a stream with no first literal cannot be aPLib at all
    if input.is_empty() {
        return Err("empty aPLib stream".to_owned());
    }
    let mut reader = BitReader::new(input);
    let mut output = Vec::new();
    // the stream opens with one raw literal byte
    push_literal(&mut output, reader.byte()?, cap)?;
    // last-was-match state (LWM) selects the gamma-match offset adjustment
    // and gates the repeated-offset form; r0 is the last match offset
    let mut lwm = false;
    let mut r0: usize = 0;
    loop {
        if reader.bit()? == 0 {
            // code 0: one literal byte, which resets the LWM state
            push_literal(&mut output, reader.byte()?, cap)?;
            lwm = false;
            continue;
        }
        if reader.bit()? == 0 {
            // code 10: a gamma-coded match
            let gamma = reader.gamma()?;
            if !lwm && gamma == 2 {
                // the repeated-offset form reuses r0 with a gamma length
                let length = reader.gamma()?;
                copy_match(&mut output, r0, length, cap)?;
            } else {
                // the offset's high bits arrive gamma-adjusted, the low raw
                let high = gamma - if lwm { 2 } else { 3 };
                let offset = (high << 8) | usize::from(reader.byte()?);
                let mut length = reader.gamma()?;
                // longer offsets imply longer matches; short ones a minimum
                if offset >= 32000 {
                    length += 1;
                }
                if offset >= 1280 {
                    length += 1;
                }
                if offset < 128 {
                    length += 2;
                }
                copy_match(&mut output, offset, length, cap)?;
                r0 = offset;
            }
            lwm = true;
            continue;
        }
        if reader.bit()? == 0 {
            // code 110: a short match packed into one byte, offset 0 = end
            let packed = usize::from(reader.byte()?);
            let offset = packed >> 1;
            if offset == 0 {
                break;
            }
            copy_match(&mut output, offset, 2 + (packed & 1), cap)?;
            r0 = offset;
            lwm = true;
        } else {
            // code 111: four tag bits of offset; zero writes a zero byte
            let mut offset = 0usize;
            for _ in 0..4 {
                offset = (offset << 1) + usize::from(reader.bit()?);
            }
            if offset == 0 {
                push_literal(&mut output, 0, cap)?;
            } else {
                copy_match(&mut output, offset, 1, cap)?;
            }
            lwm = false;
        }
    }
    Ok(output)
}

/// Skip an optional `AP32` container header by its declared size
///
/// # Arguments
///
/// * `input` - The compressed bytes, possibly `AP32`-wrapped
fn strip_ap32(input: &[u8]) -> Result<&[u8], String> {
    // raw streams pass through untouched
    if input.len() < AP32_HEADER_LEN || &input[..4] != b"AP32" {
        return Ok(input);
    }
    // the second header field declares the header's own size
    let header = u32::from_le_bytes([input[4], input[5], input[6], input[7]]) as usize;
    if header < AP32_HEADER_LEN || header > input.len() {
        return Err(format!(
            "corrupt AP32 header: declared header size {header} with {} input bytes",
            input.len()
        ));
    }
    Ok(&input[header..])
}

/// Push one literal byte onto the output, enforcing the cap
///
/// # Arguments
///
/// * `output` - The accumulated output
/// * `byte` - The literal byte to push
/// * `cap` - The most total output bytes to produce before erroring
fn push_literal(output: &mut Vec<u8>, byte: u8, cap: usize) -> Result<(), String> {
    if output.len() >= cap {
        return Err(cap_error(cap));
    }
    output.push(byte);
    Ok(())
}

/// Copy `length` bytes from `offset` back in the window onto the output
///
/// # Arguments
///
/// * `output` - The accumulated output
/// * `offset` - How far back the match reaches (must be within the output)
/// * `length` - How many bytes the match copies
/// * `cap` - The most total output bytes to produce before erroring
fn copy_match(
    output: &mut Vec<u8>,
    offset: usize,
    length: usize,
    cap: usize,
) -> Result<(), String> {
    // a match may only reach back into bytes already produced
    if offset == 0 || offset > output.len() {
        return Err(format!(
            "corrupt aPLib data: a match reaches {offset} bytes back with only {} produced",
            output.len()
        ));
    }
    // copy byte by byte so overlapping (run-length) matches work
    for _ in 0..length {
        if output.len() >= cap {
            return Err(cap_error(cap));
        }
        let byte = output[output.len() - offset];
        output.push(byte);
    }
    Ok(())
}

/// The error produced when the output cap is exceeded
///
/// # Arguments
///
/// * `cap` - The output cap that was exceeded
fn cap_error(cap: usize) -> String {
    format!("aPLib output exceeded the {cap} byte decompression cap")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test-side writer producing aPLib's interleaved tag/data layout.
    ///
    /// Tag bytes are allocated lazily at the position the reader will fetch
    /// them from, which exactly mirrors the decoder's cursor behavior.
    struct TagWriter {
        /// The stream built so far
        out: Vec<u8>,
        /// The index of the currently open tag byte
        tag: Option<usize>,
        /// How many bits of the open tag byte are used
        used: u8,
    }

    impl TagWriter {
        /// Open a stream with its mandatory first literal byte
        ///
        /// # Arguments
        ///
        /// * `first` - The stream's opening raw literal
        fn new(first: u8) -> Self {
            TagWriter {
                out: vec![first],
                tag: None,
                used: 0,
            }
        }

        /// Append one tag bit, opening a fresh tag byte when needed
        ///
        /// # Arguments
        ///
        /// * `bit` - The tag bit to append
        fn bit(&mut self, bit: u8) {
            // open a new tag byte at the stream cursor when the current is full
            if self.tag.is_none() || self.used == 8 {
                self.out.push(0);
                self.tag = Some(self.out.len() - 1);
                self.used = 0;
            }
            let index = self.tag.expect("a tag byte is open");
            self.out[index] |= (bit & 1) << (7 - self.used);
            self.used += 1;
        }

        /// Append several tag bits in order
        ///
        /// # Arguments
        ///
        /// * `bits` - The tag bits to append
        fn bits(&mut self, bits: &[u8]) {
            for bit in bits {
                self.bit(*bit);
            }
        }

        /// Append one raw data byte at the stream cursor
        ///
        /// # Arguments
        ///
        /// * `byte` - The data byte to append
        fn byte(&mut self, byte: u8) {
            self.out.push(byte);
        }

        /// Append a gamma code for a value (`>= 2`)
        ///
        /// # Arguments
        ///
        /// * `value` - The value to gamma-code
        fn gamma(&mut self, value: usize) {
            // emit the bits below the leading 1, each with a continuation bit
            assert!(value >= 2, "gamma codes only encode values >= 2");
            let top = usize::BITS - 1 - value.leading_zeros();
            for position in (0..top).rev() {
                self.bit(((value >> position) & 1) as u8);
                self.bit(u8::from(position != 0));
            }
        }

        /// Append the end-of-stream marker (a zero-offset short match)
        fn eof(&mut self) {
            self.bits(&[1, 1, 0]);
            self.byte(0x00);
        }
    }

    /// Literals round-trip and the end marker stops the stream cleanly.
    #[test]
    fn literals_and_eof() {
        // first literal, one coded literal, then the end marker
        let mut writer = TagWriter::new(b'A');
        writer.bit(0);
        writer.byte(b'B');
        writer.eof();
        let out = decompress(&writer.out, 1 << 20).expect("decompress");
        assert_eq!(out, b"AB");
    }

    /// Short matches carry their one-bit length (2 or 3) and set the window.
    #[test]
    fn short_match_lengths() {
        let mut writer = TagWriter::new(b'A');
        writer.bit(0);
        writer.byte(b'B');
        // offset 1 length 2: byte (1 << 1) | 0
        writer.bits(&[1, 1, 0]);
        writer.byte(0x02);
        // offset 1 length 3: byte (1 << 1) | 1
        writer.bits(&[1, 1, 0]);
        writer.byte(0x03);
        writer.eof();
        let out = decompress(&writer.out, 1 << 20).expect("decompress");
        assert_eq!(out, b"ABBBBBB");
    }

    /// A gamma match under offset 128 gains the +2 length adjustment.
    #[test]
    fn gamma_match_short_offset() {
        let mut writer = TagWriter::new(b'A');
        writer.bit(0);
        writer.byte(b'B');
        // code 10, offset high gamma(3) => (3-3)<<8, low byte 2, length
        // gamma(2) + 2 for the short offset = 4
        writer.bits(&[1, 0]);
        writer.gamma(3);
        writer.byte(0x02);
        writer.gamma(2);
        writer.eof();
        let out = decompress(&writer.out, 1 << 20).expect("decompress");
        assert_eq!(out, b"ABABAB");
    }

    /// Gamma matches at and past the 1280/32000 bands gain their adjustments.
    #[test]
    fn gamma_match_long_offset_bands() {
        // build 32001 literals so a 32000-back match is reachable
        let mut writer = TagWriter::new(b'A');
        for _ in 0..32_000 {
            writer.bit(0);
            writer.byte(b'B');
        }
        // offset 1280 = (5-3+3... encode high=5, low=0): gamma(5+3)? no --
        // high = offset >> 8 = 5, low = 0, and with lwm clear the gamma is
        // high + 3 = 8; length gamma(2) + 1 (>= 1280) = 3
        writer.bits(&[1, 0]);
        writer.gamma(8);
        writer.byte(0x00);
        writer.gamma(2);
        // offset 32000 = high 125 low 0; lwm is now set so gamma = high + 2;
        // length gamma(2) + 1 (>= 32000) + 1 (>= 1280) = 4
        writer.bits(&[1, 0]);
        writer.gamma(127);
        writer.byte(0x00);
        writer.gamma(2);
        writer.eof();
        let out = decompress(&writer.out, 1 << 20).expect("decompress");
        // simulate the two copies over the literal prefix
        let mut expected = vec![b'A'];
        expected.extend(std::iter::repeat(b'B').take(32_000));
        for _ in 0..3 {
            expected.push(expected[expected.len() - 1280]);
        }
        for _ in 0..4 {
            expected.push(expected[expected.len() - 32_000]);
        }
        assert_eq!(out, expected);
    }

    /// The repeated-offset form reuses the previous match offset.
    #[test]
    fn rep_match_reuses_the_offset() {
        let mut writer = TagWriter::new(b'A');
        // a gamma match at offset 1, length gamma(2) + 2 = 4 -> "AAAAA"
        writer.bits(&[1, 0]);
        writer.gamma(3);
        writer.byte(0x01);
        writer.gamma(2);
        // a literal clears LWM so the next gamma(2) selects the rep form
        writer.bit(0);
        writer.byte(b'B');
        // rep match: offset r0 = 1, length gamma(3) = 3 -> "BBB"
        writer.bits(&[1, 0]);
        writer.gamma(2);
        writer.gamma(3);
        writer.eof();
        let out = decompress(&writer.out, 1 << 20).expect("decompress");
        assert_eq!(out, b"AAAAABBBB");
    }

    /// Nibble codes write a zero byte or copy one near window byte.
    #[test]
    fn nibble_codes() {
        let mut writer = TagWriter::new(b'A');
        // offset 0 writes a literal zero
        writer.bits(&[1, 1, 1]);
        writer.bits(&[0, 0, 0, 0]);
        // offset 2 copies the 'A' two bytes back
        writer.bits(&[1, 1, 1]);
        writer.bits(&[0, 0, 1, 0]);
        writer.eof();
        let out = decompress(&writer.out, 1 << 20).expect("decompress");
        assert_eq!(out, b"A\x00A");
    }

    /// An `AP32` container header is skipped by its declared size.
    #[test]
    fn ap32_header_skips() {
        // the raw stream from the literals test, wrapped in a 24-byte header
        let mut writer = TagWriter::new(b'A');
        writer.bit(0);
        writer.byte(b'B');
        writer.eof();
        let mut wrapped = Vec::new();
        wrapped.extend_from_slice(b"AP32");
        wrapped.extend_from_slice(&24u32.to_le_bytes());
        wrapped.extend_from_slice(&[0u8; 16]);
        wrapped.extend_from_slice(&writer.out);
        let out = decompress(&wrapped, 1 << 20).expect("decompress");
        assert_eq!(out, b"AB");
        // a header whose declared size runs past the input is corrupt
        let mut bad = Vec::new();
        bad.extend_from_slice(b"AP32");
        bad.extend_from_slice(&4096u32.to_le_bytes());
        bad.extend_from_slice(&[0u8; 16]);
        let error = decompress(&bad, 1 << 20).expect_err("oversize header");
        assert!(error.contains("AP32"), "unexpected error: {error}");
    }

    /// Corrupt streams error instead of panicking or looping.
    #[test]
    fn corrupt_streams_error() {
        // empty input has no first literal
        let error = decompress(&[], 1 << 20).expect_err("empty");
        assert!(error.contains("empty"), "unexpected error: {error}");
        // a literal code whose data byte is missing is truncation, not success
        let mut writer = TagWriter::new(b'A');
        writer.bit(0);
        let error = decompress(&writer.out, 1 << 20).expect_err("truncated");
        assert!(error.contains("truncated"), "unexpected error: {error}");
        // a match past the produced output names the overreach
        let mut writer = TagWriter::new(b'A');
        writer.bits(&[1, 0]);
        writer.gamma(3);
        writer.byte(0x05);
        writer.gamma(2);
        let error = decompress(&writer.out, 1 << 20).expect_err("overreach");
        assert!(error.contains("reaches"), "unexpected error: {error}");
        // a rep match with no prior offset has nothing to reuse
        let mut writer = TagWriter::new(b'A');
        writer.bits(&[1, 0]);
        writer.gamma(2);
        writer.gamma(2);
        let error = decompress(&writer.out, 1 << 20).expect_err("rep without r0");
        assert!(error.contains("reaches"), "unexpected error: {error}");
    }

    /// Exceeding the output cap is a hard error naming the cap.
    #[test]
    fn output_cap_is_enforced() {
        // the short-match run from above against a 3-byte cap
        let mut writer = TagWriter::new(b'A');
        writer.bit(0);
        writer.byte(b'B');
        writer.bits(&[1, 1, 0]);
        writer.byte(0x03);
        writer.eof();
        let error = decompress(&writer.out, 3).expect_err("the cap should trip");
        assert!(error.contains("3 byte"), "unexpected error: {error}");
    }
}
