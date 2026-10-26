//! A pure-Rust LZNT1 decompressor ([MS-XCA] section 2.3/2.5).
//!
//! LZNT1 is the LZ77-style compression Windows uses for NTFS compressed files
//! and the `RtlCompressBuffer`/`RtlDecompressBuffer` APIs, which makes it a
//! staple of malware droppers that stash payloads compressed with the native
//! Windows routines. Compressed data is a sequence of chunks, each holding up
//! to 4096 bytes of uncompressed data: a 16-bit little-endian header (bit 15 =
//! compressed, bits 12-14 = signature `0b011`, bits 0-11 = payload size minus
//! one) followed by the payload. A compressed payload interleaves flag bytes
//! (one bit per token, LSB first: 0 = literal byte, 1 = 16-bit copy token)
//! whose copy tokens split into a displacement and a length; the split point
//! depends on how far into the chunk the output currently is.

/// The number of uncompressed bytes one LZNT1 chunk may hold.
const CHUNK_SIZE: usize = 4096;

/// The chunk-header signature [MS-XCA] fixes in bits 12-14 (`0b011`)
const HEADER_SIGNATURE: u16 = 0x3000;

/// Decompress an LZNT1-compressed buffer, bounded by an output cap
///
/// Walks the chunk sequence until the input ends or a zero header terminates
/// it, per [MS-XCA]. Every header's signature bits (12-14 must be `0b011`)
/// are validated: a bad signature (or a header whose payload runs past the
/// input) before any chunk decoded means the data is not LZNT1 at all and
/// errors, while after at least one good chunk it marks the end of the
/// compressed region -- a to-EOF window with trailing file bytes stops
/// cleanly instead of erroring or appending garbage. Exceeding `cap` output
/// bytes is a hard error naming the cap, so a decompression bomb can never
/// balloon memory.
///
/// # Arguments
///
/// * `input` - The LZNT1-compressed bytes
/// * `cap` - The most output bytes to produce before erroring
pub(crate) fn decompress(input: &[u8], cap: usize) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    let mut pos = 0;
    // count the good chunks so trailing non-LZNT1 bytes can stop cleanly
    let mut chunks = 0usize;
    // walk the chunk sequence until the input runs out
    while pos + 2 <= input.len() {
        // read the 16-bit little-endian chunk header
        let header = u16::from_le_bytes([input[pos], input[pos + 1]]);
        // an all-zero header terminates the chunk sequence
        if header == 0 {
            break;
        }
        // a header missing the fixed signature bits is not an LZNT1 chunk:
        // corrupt data up front, or trailing bytes after the real chunks
        if header & 0x7000 != HEADER_SIGNATURE {
            if chunks > 0 {
                break;
            }
            return Err(format!(
                "not LZNT1 data: the first chunk header {header:#06x} is missing the \
                 required signature bits"
            ));
        }
        pos += 2;
        // bits 0-11 encode the payload size minus one
        let size = usize::from(header & 0x0FFF) + 1;
        // a declared payload past the input is corrupt up front, or marks
        // where trailing non-LZNT1 bytes begin after the real chunks
        if pos + size > input.len() {
            if chunks > 0 {
                break;
            }
            return Err(format!(
                "corrupt LZNT1 data: a chunk declares {size} payload bytes but only \
                 {} remain",
                input.len() - pos
            ));
        }
        let payload = &input[pos..pos + size];
        pos += size;
        // bit 15 set means the payload is compressed; clear means raw bytes
        if header & 0x8000 != 0 {
            decompress_chunk(payload, &mut output, cap)?;
        } else {
            // enforce the cap on raw chunks too before copying them through
            if output.len() + size > cap {
                return Err(cap_error(cap));
            }
            output.extend_from_slice(payload);
        }
        chunks += 1;
    }
    Ok(output)
}

/// Decompress one compressed chunk's payload onto the output
///
/// # Arguments
///
/// * `payload` - The chunk's compressed payload (flag bytes and tokens)
/// * `output` - The accumulated output across every chunk so far
/// * `cap` - The most total output bytes to produce before erroring
fn decompress_chunk(payload: &[u8], output: &mut Vec<u8>, cap: usize) -> Result<(), String> {
    // copy tokens may only reach back to the start of this chunk
    let chunk_start = output.len();
    let mut pos = 0;
    // walk the payload one flag byte (eight tokens) at a time; the label lets
    // a payload ending mid-group fall through to the chunk-size sanity check
    'groups: while pos < payload.len() {
        let flags = payload[pos];
        pos += 1;
        // each flag bit (LSB first) says whether its token is a copy or literal
        for bit in 0..8 {
            // the payload may end mid-group, which simply ends the chunk
            if pos >= payload.len() {
                break 'groups;
            }
            if flags >> bit & 1 == 0 {
                // a literal token is a single raw byte
                if output.len() >= cap {
                    return Err(cap_error(cap));
                }
                output.push(payload[pos]);
                pos += 1;
            } else {
                // a copy token is a 16-bit little-endian displacement/length pair
                if pos + 2 > payload.len() {
                    return Err("corrupt LZNT1 data: a copy token is cut short".to_owned());
                }
                let token = u16::from_le_bytes([payload[pos], payload[pos + 1]]);
                pos += 2;
                // the split between the fields depends on the chunk position:
                // the displacement field grows (and the length field shrinks)
                // as the window of reachable output grows
                let done = output.len() - chunk_start;
                if done == 0 {
                    return Err(
                        "corrupt LZNT1 data: a copy token appears before any output".to_owned()
                    );
                }
                let mut len_bits = 12u32;
                let mut back = done - 1;
                while back >= 0x10 {
                    len_bits -= 1;
                    back >>= 1;
                }
                let length = usize::from(token & ((1 << len_bits) - 1)) + 3;
                let displacement = usize::from(token >> len_bits) + 1;
                // the displacement may not reach back past the chunk start
                if displacement > done {
                    return Err(format!(
                        "corrupt LZNT1 data: a copy token reaches {displacement} bytes \
                         back with only {done} bytes in the chunk"
                    ));
                }
                // copy byte by byte so overlapping (run-length) copies work
                for _ in 0..length {
                    if output.len() >= cap {
                        return Err(cap_error(cap));
                    }
                    let byte = output[output.len() - displacement];
                    output.push(byte);
                }
            }
        }
    }
    // a chunk may not produce more than its 4 KiB of uncompressed data, but a
    // short final chunk is fine, so only the upper bound is checked
    if output.len() - chunk_start > CHUNK_SIZE {
        return Err("corrupt LZNT1 data: a chunk decompressed past 4096 bytes".to_owned());
    }
    Ok(())
}

/// The error produced when the output cap is exceeded
///
/// # Arguments
///
/// * `cap` - The output cap that was exceeded
fn cap_error(cap: usize) -> String {
    format!("LZNT1 output exceeded the {cap} byte decompression cap")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A literal plus an overlapping copy token expands to a run.
    ///
    /// Hand-assembled from the [MS-XCA] 2.3 rules: header 0xB003 (compressed,
    /// signature 3, payload size 4), flag byte 0b0000_0010 (literal then
    /// copy), literal 'A', then the copy token for displacement 1 / length 15.
    /// At chunk position 1 the split is 4 displacement bits / 12 length bits,
    /// so the token is ((1-1) << 12) | (15-3) = 0x000C.
    #[test]
    fn overlapping_copy_expands_a_run() {
        let compressed = [0x03, 0xB0, 0x02, 0x41, 0x0C, 0x00];
        let out = decompress(&compressed, 1 << 20).expect("decompress");
        assert_eq!(out, vec![b'A'; 16]);
    }

    /// A copy token past the 16-byte boundary uses the shifted field split.
    ///
    /// After 18 literals the split becomes 5 displacement bits / 11 length
    /// bits (position 17 shifts once), so displacement 18 / length 10 encodes
    /// as ((18-1) << 11) | (10-3) = 0x8807.
    #[test]
    fn copy_tokens_use_the_position_dependent_split() {
        // 18 literals need three flag bytes: 8 + 8 + (2 literals, 1 copy)
        let mut compressed = vec![0x00, 0x00]; // header patched below
        compressed.push(0x00);
        compressed.extend_from_slice(b"ABCDEFGH");
        compressed.push(0x00);
        compressed.extend_from_slice(b"IJKLMNOP");
        compressed.push(0b0000_0100);
        compressed.extend_from_slice(b"QR");
        compressed.extend_from_slice(&0x8807u16.to_le_bytes());
        // stamp the real header now that the payload size is known
        let size = (compressed.len() - 2) as u16;
        compressed[..2].copy_from_slice(&(0xB000 | (size - 1)).to_le_bytes());
        let out = decompress(&compressed, 1 << 20).expect("decompress");
        assert_eq!(out, b"ABCDEFGHIJKLMNOPQRABCDEFGHIJ");
    }

    /// Uncompressed chunks copy through raw and a zero header terminates.
    #[test]
    fn raw_chunks_and_termination() {
        // an uncompressed chunk: header 0x3003 then four raw bytes
        let mut compressed = vec![0x03, 0x30];
        compressed.extend_from_slice(b"MZ\x90\x00");
        // a zero header terminates before the trailing garbage
        compressed.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
        let out = decompress(&compressed, 1 << 20).expect("decompress");
        assert_eq!(out, b"MZ\x90\x00");
        // empty input decompresses to empty output
        assert!(decompress(&[], 1 << 20).expect("empty").is_empty());
    }

    /// Multiple chunks concatenate in order.
    #[test]
    fn chunks_concatenate() {
        // two raw chunks back to back
        let mut compressed = vec![0x02, 0x30];
        compressed.extend_from_slice(b"abc");
        compressed.extend_from_slice(&[0x02, 0x30]);
        compressed.extend_from_slice(b"def");
        let out = decompress(&compressed, 1 << 20).expect("decompress");
        assert_eq!(out, b"abcdef");
    }

    /// Trailing non-LZNT1 bytes after a good chunk stop the decode cleanly.
    ///
    /// A to-EOF window routinely carries the rest of the file behind the
    /// compressed region, so trailing bytes must neither error nor append
    /// fake raw chunks to the real output.
    #[test]
    fn trailing_bytes_stop_cleanly() {
        // the worked-example chunk followed by plain file bytes ("GA" reads
        // as header 0x4741, whose signature bits are wrong)
        let mut compressed = vec![0x03, 0xB0, 0x02, 0x41, 0x0C, 0x00];
        compressed.extend_from_slice(b"GARBAGE TRAILING FILE BYTES");
        let out = decompress(&compressed, 1 << 20).expect("decompress");
        assert_eq!(out, vec![b'A'; 16]);
        // a bit15-set garbage tail (header 0x8000, signature bits zero) too
        let mut compressed = vec![0x03, 0xB0, 0x02, 0x41, 0x0C, 0x00];
        compressed.extend_from_slice(&[0x00, 0x80, 0xDE, 0xAD]);
        let out = decompress(&compressed, 1 << 20).expect("decompress");
        assert_eq!(out, vec![b'A'; 16]);
        // a valid-signature tail whose declared payload runs past the input
        // also marks the end of the compressed region
        let mut compressed = vec![0x03, 0xB0, 0x02, 0x41, 0x0C, 0x00];
        compressed.extend_from_slice(&[0xFF, 0xB0]);
        let out = decompress(&compressed, 1 << 20).expect("decompress");
        assert_eq!(out, vec![b'A'; 16]);
    }

    /// Data whose first header lacks the signature is rejected up front.
    #[test]
    fn missing_signature_up_front_errors() {
        // header 0x8000: bit 15 set but the signature bits are zero
        let error = decompress(&[0x00, 0x80, 0x41], 1 << 20)
            .expect_err("a bad first signature should fail");
        assert!(error.contains("signature"), "unexpected error: {error}");
    }

    /// Corrupt streams error instead of panicking or looping.
    #[test]
    fn corrupt_streams_error() {
        // a chunk declaring more payload than remains
        assert!(decompress(&[0xFF, 0xB0, 0x00], 1 << 20).is_err());
        // a copy token before any literal has nothing to copy from
        assert!(decompress(&[0x02, 0xB0, 0x01, 0x0C, 0x00], 1 << 20).is_err());
        // a copy token cut short mid-stream
        assert!(decompress(&[0x02, 0xB0, 0x02, 0x41, 0x0C], 1 << 20).is_err());
    }

    /// Exceeding the output cap is a hard error naming the cap.
    #[test]
    fn output_cap_is_enforced() {
        // the 16-byte run from the first test against an 8-byte cap
        let compressed = [0x03, 0xB0, 0x02, 0x41, 0x0C, 0x00];
        let error = decompress(&compressed, 8).expect_err("the cap should trip");
        assert!(error.contains("8 byte"), "unexpected error: {error}");
    }
}
