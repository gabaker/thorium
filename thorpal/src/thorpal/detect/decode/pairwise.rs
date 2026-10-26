//! Pairwise-SUB decoder (`out[i] = in[2i] − in[2i+1]`).
//!
//! A common per-character stack-string scheme hides each plaintext byte as a pair
//! `(plain + k, k)` and recovers it by subtracting the two, with `k` varying freely from
//! character to character so no single key is ever stored. This module decodes that
//! pairing and, for the blob scan, extracts the readable runs while rejecting UTF-16LE
//! wide strings (whose zero subtrahend bytes make the SUB a plaintext no-op).

use super::recover::{
    MAX_RECOVERY_GAP, MIN_RECOVERY_CHARS, RunsAndClaims, dense_clusters, is_table_run,
    printable_claim_runs,
};

/// Decode `data` with the pairwise SUB scheme: each output byte is
/// `data[2i].wrapping_sub(data[2i+1])`. A trailing odd byte is ignored.
///
/// # Arguments
///
/// * `data` - The encoded bytes (interleaved value/key pairs)
#[must_use]
pub fn decode(data: &[u8]) -> Vec<u8> {
    // subtract each pair down to one output byte (wrapping, like the 8-bit op)
    data.chunks_exact(2)
        .map(|p| p[0].wrapping_sub(p[1]))
        .collect()
}

/// Decode `src` with pairwise SUB and return the printable, wordy runs as
/// `(source offset, string)` — **excluding UTF-16LE wide strings**.
///
/// A wide ASCII string is `char, 0x00, char, 0x00, …`; its zero subtrahend bytes
/// make the SUB a no-op that would otherwise "recover" ordinary plaintext. Genuine
/// pairwise-SUB encoding uses non-zero key bytes, so a run whose odd (subtrahend)
/// source bytes are mostly zero is rejected.
///
/// What survives that is then required to be part of a dense recovery
/// ([`dense_clusters`]) rather than an isolated hit, because this is applied to whole
/// sections: over a quarter of a megabyte, a scheme with no key to be wrong about will
/// turn up a handful of readable runs by chance no matter what the section holds.
///
/// # Arguments
///
/// * `src` - The section bytes to decode
#[must_use]
pub fn hidden_runs(src: &[u8]) -> Vec<(usize, String)> {
    hidden_runs_and_claims(src).0
}

/// [`hidden_runs`], paired with every run the scheme made readable **including the
/// decoded tables** it rejects.
///
/// The section sweep needs both: it reports the first and counts the second, because a
/// table it never reports is still this scheme's claim on the address it came from (see
/// [`printable_claim_runs`]).
///
/// Both halves are `(source offset, string)` pairs.
///
/// # Arguments
///
/// * `src` - The section bytes to decode
#[must_use]
pub fn hidden_runs_and_claims(src: &[u8]) -> RunsAndClaims {
    // decode every pair, then pull every readable run out of the result
    let claimed: Vec<(usize, String)> = printable_claim_runs(&decode(src))
        .into_iter()
        // reject wide strings: the run's odd (subtrahend) source bytes are ~all zero
        .filter(|(j, s)| {
            let zero = (*j..*j + s.len())
                .filter(|&k| src.get(2 * k + 1).copied() == Some(0))
                .count();
            zero * 2 < s.len()
        })
        .collect();
    // what may be reported is that, less the runs that are a decoded table
    let readable: Vec<(usize, String)> = claimed
        .iter()
        .filter(|(_, s)| !is_table_run(s.as_bytes()))
        .cloned()
        .collect();
    (cluster(readable), cluster(claimed))
}

/// Drop the scattered runs and map the survivors back to source offsets.
///
/// The clustering has to happen while the offsets are still decoded indices —
/// clustering them in source offsets would double every gap and split tables that are
/// packed — so the mapping is done after it, at two bytes of source per character.
///
/// # Arguments
///
/// * `runs` - The `(decoded index, string)` runs to cluster
fn cluster(runs: Vec<(usize, String)>) -> Vec<(usize, String)> {
    dense_clusters(runs, MAX_RECOVERY_GAP, MIN_RECOVERY_CHARS)
        .into_iter()
        .map(|(j, s)| (j * 2, s))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `text` the way the scheme does, as `(char + k, k)` pairs.
    ///
    /// # Arguments
    ///
    /// * `text` - The plaintext to encode
    /// * `key` - The subtrahend to pair each character with
    fn encode(text: &[u8], key: u8) -> Vec<u8> {
        text.iter()
            .flat_map(|c| [c.wrapping_add(key), key])
            .collect()
    }

    /// Pairwise SUB recovers a genuinely-encoded string but rejects UTF-16LE wide
    /// strings (whose zero subtrahend bytes make the decode a plaintext no-op).
    #[test]
    fn hidden_runs_rejects_wide_strings() {
        let text = b"kernel32.dll and advapi32.dll";
        // genuine encoding: (char + 7, 7) per char — a non-zero subtrahend "key"
        let got = hidden_runs(&encode(text, 7));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "kernel32.dll and advapi32.dll");
        assert_eq!(got[0].0, 0);
        // UTF-16LE wide string: (char, 0x00) per char — must be rejected
        let mut wide = Vec::new();
        for &c in text {
            wide.push(c);
            wide.push(0);
        }
        assert!(hidden_runs(&wide).is_empty());
    }

    /// The scattered runs a section-wide sweep turns up by chance are dropped, and the
    /// packed ones it exists to find are not.
    #[test]
    fn hidden_runs_drops_the_scattered_recoveries() {
        // one short readable run, alone in a kilobyte of nothing: the shape a sweep
        // produces over a section that is not hiding anything
        let mut lone = vec![0u8; 512];
        lone.splice(200..200, encode(b"CreateFile", 9));
        assert!(hidden_runs(&lone).is_empty());
        // a table of the same short entries, packed one after another: the shape a real
        // encoded string table has
        let mut table = vec![0u8; 512];
        let mut packed = Vec::new();
        for entry in [&b"CreateFile"[..], b"WriteFileEx", b"DeleteFileW"] {
            packed.extend(encode(entry, 9));
            // the terminator each entry is stored with, which the decode renders as a
            // hole between two runs
            packed.extend([0x00, 0x00]);
        }
        table.splice(200..200, packed);
        let got: Vec<String> = hidden_runs(&table).into_iter().map(|(_, s)| s).collect();
        assert_eq!(got, ["CreateFile", "WriteFileEx", "DeleteFileW"]);
    }
}
