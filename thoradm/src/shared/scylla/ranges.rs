//! Splitting Scylla's token ring into ranges for workers to read

/// Splits the whole token ring, `i64::MIN` to `i64::MAX`, into contiguous ranges
///
/// Each range is `(start, end)`, both inclusive, and starts one token after the previous one
/// ends, so every token is in exactly one range. The ranges are returned in ring order and
/// differ in size by at most one token. Zero chunks is treated as one, the whole ring.
///
/// # Arguments
///
/// * `chunks` - How many ranges to split the ring into
#[must_use]
pub fn build_token_ranges(chunks: u64) -> Vec<(i64, i64)> {
    // the ring holds 2^64 tokens; split them as evenly as whole tokens allow
    let chunks = u128::from(chunks.max(1));
    let ring: u128 = 1 << 64;
    let (size, extra) = (ring / chunks, ring % chunks);
    // the first `extra` ranges take one more token than the rest, so every token is used
    let mut ranges = Vec::with_capacity(usize::try_from(chunks).unwrap_or(0));
    let mut start: u128 = 0;
    for index in 0..chunks {
        let len = size + u128::from(index < extra);
        let end = start + len - 1;
        ranges.push((ring_token(start), ring_token(end)));
        start = end + 1;
    }
    ranges
}

/// Converts a position on the ring, counted from its first token, into the token itself
///
/// # Arguments
///
/// * `position` - The position, from 0 (`i64::MIN`) to 2^64 - 1 (`i64::MAX`)
fn ring_token(position: u128) -> i64 {
    // positions count up from i64::MIN, so shift them down by 2^63
    let token = i128::try_from(position).unwrap_or(i128::MAX) + i128::from(i64::MIN);
    i64::try_from(token).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::build_token_ranges;

    /// The ranges cover the whole ring in order, each starting one token after the last ends
    #[test]
    fn ranges_cover_the_ring_contiguously_with_no_overlap() {
        for chunks in [1_u64, 2, 3, 10, 1000] {
            let ranges = build_token_ranges(chunks);
            // one range per chunk, from the first token to the last
            assert_eq!(ranges.len() as u64, chunks, "{chunks} chunks");
            assert_eq!(ranges.first().map(|range| range.0), Some(i64::MIN));
            assert_eq!(ranges.last().map(|range| range.1), Some(i64::MAX));
            // every range is non-empty and the next one starts right after it
            for (index, (start, end)) in ranges.iter().enumerate() {
                assert!(start <= end, "{chunks} chunks: range {index} is backwards");
                if let Some((next, _)) = ranges.get(index + 1) {
                    assert_eq!(
                        i128::from(*next),
                        i128::from(*end) + 1,
                        "{chunks} chunks: range {} doesn't start after range {index}",
                        index + 1
                    );
                }
            }
            // the ranges hold every token of the ring exactly once
            let tokens: i128 = ranges
                .iter()
                .map(|(start, end)| i128::from(*end) - i128::from(*start) + 1)
                .sum();
            assert_eq!(tokens, 1_i128 << 64, "{chunks} chunks");
        }
    }

    /// Zero chunks is the whole ring rather than a division by zero
    #[test]
    fn zero_chunks_is_the_whole_ring() {
        assert_eq!(build_token_ranges(0), vec![(i64::MIN, i64::MAX)]);
    }
}
