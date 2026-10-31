//! Readable-string recovery from decoded bytes.
//!
//! After a candidate decode, these helpers pull out the runs that actually look like
//! text — long enough, majority letters, diverse, and closing the brackets they open —
//! and, for the section brute-force, require that the *source* bytes both looked encoded
//! (mostly non-printable) and carried no layout of their own, so we surface genuinely
//! hidden strings rather than re-cased plaintext or a decoded table of numbers. Shared
//! by the encoded-string decoder loops and the crypto (RC4/base64) passes.

use std::collections::HashSet;

/// The minimum length of a recovered printable run to surface it.
pub const MIN_DECODED_LEN: usize = 10;

/// The minimum total recovered characters for a decode to count as a *real* recovery
/// (see [`is_strong_recovery`]).
pub const MIN_RECOVERY_CHARS: usize = 24;

/// The max length of a single embedded PEM block to extract (bounds a runaway scan when
/// a `-----BEGIN` marker has no nearby closing boundary). Also the window a caller should
/// slice from a boundary's address when recovering its full block (see
/// [`crate::thorpal::detect::detections::impact`]'s PEM pass).
pub const MAX_PEM_BLOCK: usize = 16 * 1024;

/// Whether a byte is printable ASCII (or tab) for the decoded-run test.
///
/// # Arguments
///
/// * `b` - The byte to classify
#[must_use]
pub fn is_printable(b: u8) -> bool {
    (0x20..=0x7e).contains(&b) || b == b'\t'
}

/// The digits leetspeak prose substitutes for letters.
///
/// The confirmed `GandCrab` ransom note spells `ATTENTION` as `A44EN4ION` and
/// `PREVENT` as `02E6EN4`, so a majority-of-letters bar starves on text an analyst
/// absolutely wants. These are the digits with a conventional letter reading
/// (0→o, 1→i, 2→r, 3→e, 4→t, 5→s, 7→w, 9→y); `6` and `8` are deliberately absent —
/// no leet word relies on them, and leaving them out keeps digit-bearing junk (the
/// AES T-table's `=&&jL66Zl??A~`) from gaining anything.
const LEET_DIGITS: &[u8] = b"01234579";

/// Whether a decoded run looks like real text: majority ASCII letters and enough
/// character diversity to reject runs like `"AAAA…"` (e.g. a zero region XORed with
/// a printable key).
///
/// Digit-substituted (leetspeak) prose gets a second chance at the letter-majority
/// bar: when the plain count falls short, the digits in [`LEET_DIGITS`] count as the
/// letters they stand for and the bar is retried. The second chance is held to the
/// shape leetspeak actually takes — prose: a run of at least [`MIN_DECODED_LEN`]
/// carrying a space (words), with real letters still a third of it — so the
/// single-token shapes that also mix letters and digits (a base64/hex run, emulated
/// counter output like `3g1>2O7`, a digits-only table) can never talk their way in.
/// The second chance only ever admits runs, so everything wordy today stays wordy.
///
/// # Arguments
///
/// * `run` - The candidate decoded bytes
#[must_use]
pub fn is_wordy(run: &[u8]) -> bool {
    // require character diversity (a constant/near-constant run carries no signal)
    let distinct: HashSet<u8> = run.iter().copied().collect();
    if distinct.len() < 5 {
        return false;
    }
    // a majority of ASCII letters passes outright
    let alpha = run.iter().filter(|b| b.is_ascii_alphabetic()).count();
    if alpha * 2 >= run.len() {
        return true;
    }
    // the second chance is for prose: long enough to mean something, and spaced
    // into words — the one shape a letter-and-digit token (base64, counter
    // output) never has
    if run.len() < MIN_DECODED_LEN || !run.contains(&b' ') {
        return false;
    }
    // leetspeak prose: the digits that stand for letters make up the majority,
    // provided real letters still carry a third of the run
    let leet = run.iter().filter(|b| LEET_DIGITS.contains(b)).count();
    alpha * 3 >= run.len() && (alpha + leet) * 2 >= run.len()
}

/// The bracket pairs whose closing half only ever follows its own opener in real text.
///
/// Every construct a recovery is likely to be — a format template, a JSON body, an
/// argument list, an index expression — closes what it opens, and does it in order. A
/// wrong key has no such discipline: it draws each of these six characters
/// independently, so it produces closers with nothing to close about as often as
/// anything else.
const BRACKET_PAIRS: &[(u8, u8)] = &[(b'(', b')'), (b'[', b']'), (b'{', b'}')];

/// Whether a recovered run reads as text rather than as the wrong key's output.
///
/// [`is_wordy`] asks whether the run is made of the right *kinds* of character; this
/// asks whether they are arranged like something a program would have hidden. A wrong
/// key clears the wordiness bar routinely — half of the printable range is letters, so
/// half of random printable output is too — and the tell is what it does with the other
/// half: it strews closing brackets that have nothing to close (see [`BRACKET_PAIRS`]).
///
/// Only that direction is tested. An *unclosed* opener is not evidence of anything,
/// because a genuine recovery is routinely truncated by the end of the buffer it was
/// carved from, and a run that stops mid-template is still a run worth reporting.
///
/// Unusual punctuation — `|`, `` ` ``, `~`, `^` — is deliberately not scored, in any
/// form. It is the obvious second signal and it does not survive contact with the
/// domain: `|` is the field separator in a large share of C2 configs, and `` ` `` and
/// `^` are the PowerShell and `cmd.exe` escape characters, so the runs that use them
/// most heavily are precisely the ones worth recovering. Measured over the fabrications
/// this module has actually produced, no rate or vocabulary bar on those four marks
/// catches any of them, while every bar tried rejects real recovered configs. The runs
/// they were reached for are caught properly by [`is_structured_source`], which looks at
/// the bytes the key was applied to instead of guessing from the output.
///
/// # Arguments
///
/// * `run` - The candidate recovered bytes
#[must_use]
pub fn reads_as_text(run: &[u8]) -> bool {
    let mut open: Vec<u8> = Vec::new();
    for byte in run {
        // an opener is pushed, recording the closer it is now waiting for
        if let Some((_, close)) = BRACKET_PAIRS.iter().find(|(o, _)| o == byte) {
            open.push(*close);
            continue;
        }
        // ...and a closer has to be the one the innermost opener is waiting for
        if BRACKET_PAIRS.iter().any(|(_, c)| c == byte) && open.pop() != Some(*byte) {
            return false;
        }
    }
    true
}

/// Whether a recovered run climbs (or falls) throughout, and so is a table read
/// through a decode rather than text.
///
/// [`is_structured_source`] catches a layout by looking at the bytes a key was applied
/// to. That is the better place to catch one, because it does not depend on how the
/// decode happened to come out — but it needs the source, and the schemes whose output
/// is not index-aligned with their input (pairwise SUB, base64) cannot supply it. For
/// those, the layout has to be read off the output instead, and order survives the
/// decode: a key that adds or subtracts a constant shifts a ramp without flattening it,
/// and subtracting adjacent elements of one ramp yields another.
///
/// Text of the length this is applied to ([`MIN_DECODED_LEN`] and up) is never in
/// order — a sorted ten characters of English is a coincidence with about a one in a
/// million rate, and the runs that reach here have already been filtered to the
/// printable range where an accidental ordering is rarer still.
///
/// # Why the bar is a share and not "fully ordered"
///
/// It was fully ordered, on the reasoning that a table is ordered exactly and the
/// partially-ordered cases could be left to the source tests. They cannot: the schemes
/// that most need this one are precisely the schemes with no index-aligned source, and
/// the tables they produce are ramps with periodic interruptions rather than clean
/// ones. A character table interleaved with punctuation — `` !"#$%&9'():*+,-./0;<1=>?``
/// — climbs on nine pairs in ten and is not claimed by an exact test at all.
///
/// [`ORDERED_SHARE`] is what the interruption tolerance costs, measured. Over the
/// 13,188 wordy, text-reading plaintext runs in the sample binaries' data sections it
/// claims 21 (0.159%). Sixteen of the twenty-one are character tables — base64
/// alphabets, hex-digit tables, printable-ASCII ramps — which is the thing being looked
/// for. The other five are short strings whose letters happen to climb (`LastAccess`,
/// `Label loop`, `BIO_accept_ex`). The share's distribution over that corpus puts its
/// 99th percentile at 75% and its 99.9th at 88%, and the highest any of the 13 genuine
/// recoveries in the same corpus reaches is 72%, so the bar sits in the tail rather
/// than inside the body of real text.
///
/// Both directions count: a table read backwards is the same table.
///
/// # Arguments
///
/// * `run` - The candidate recovered bytes
#[must_use]
pub fn is_ordered_run(run: &[u8]) -> bool {
    // too short for an ordering to be anything but chance (and an empty run orders
    // vacuously, which is not an answer worth returning)
    if run.len() < MIN_DECODED_LEN {
        return false;
    }
    // equal neighbours belong to both orderings — a table with repeated entries is
    // still a table — so counting the rises also counts them, and the falls are the
    // remainder
    let pairs = run.len() - 1;
    let rising = run.windows(2).filter(|pair| pair[0] <= pair[1]).count();
    rising.max(pairs - rising) * 100 >= pairs * ORDERED_SHARE
}

/// The share of a run's adjacent pairs that must agree on a direction before the run is
/// a table rather than text, in percent.
///
/// Measured on [`is_ordered_run`], which is also where what it costs is recorded.
const ORDERED_SHARE: usize = 85;

/// The largest stride a repeating run is looked for on.
///
/// Eight is where the catch saturates. Swept over the sample binaries' data sections under
/// every scheme the section brute-force uses, sixteen finds the same 52 fabrications as
/// eight and only widens what the bar claims out of real text; four misses eleven of them.
/// The tables that get this far repeat on an element width — three, four, six and eight
/// bytes are what the corpus actually contains — so there is nothing above eight to find.
const MAX_PERIOD: usize = 8;

/// The number of times a stride has to repeat before the repetition is a table rather
/// than a word that happens to contain itself.
///
/// This is the constant that makes the test affordable. A great deal of ordinary text
/// repeats *once*: a Go type name is its own package name doubled (`*hmac.hmac`,
/// `*context.Context`, `*regexp.Regexp`), and one repetition matches on almost every
/// position it is measured over. Without a floor those clear a 70% bar and the test claims
/// eight real plaintext runs; with it, two. A doubling is a word. Three in a row is a
/// layout.
const MIN_PERIODS: usize = 3;

/// The share of a run's positions that must match the position one stride back before the
/// run is a repeating table rather than text, in percent.
///
/// [`is_ordered_run`] catches the tables that climb. The ones left over are the tables that
/// *cycle* — a range table read through a byte key produces a fixed element with one
/// varying column, so it comes out as `l4i$n l3i$n l4i$n` or `8KDF9LDF9KDF9KDF`: not in
/// order, not dominated by any one byte, and wordy by every measure available.
///
/// Measured on the sample binaries' data sections, swept under pairwise SUB, XOR
/// `0x80..=0xFF` and SUB/ADD `0x01..=0xFF` with the rest of the chain in place — 700
/// fabrications gathered, 15 genuine recoveries, 13,217 plaintext runs — this bar rejects
/// 52 of the 700, none of the 15, and claims 2 of the 13,217 (0.015%). The genuine
/// recoveries top out at 36%, so the bar is not near them.
///
/// Seventy is where the two populations stop overlapping in the direction that matters.
/// Real text runs as high as 64% (`PROXY %s %s %s %i %i` — a request template repeating
/// its conversion specifiers) and the fabricated tables start at 72%. Dropping to 60%
/// buys seven more fabrications and costs that template, which is the kind of recovery the
/// sweep exists to produce. The two plaintext runs above the bar are one more character
/// table and `err:%lx:%lx:%lx:%lx`.
///
/// A repeating *column* was tried first and is subsumed by this. Requiring four identical
/// bytes on a stride caught one additional fabrication at 0.161% of plaintext, and letting
/// the column start anywhere in the run caught three but cost a genuine recovery. The share
/// asks the same question without insisting the repetition be exact, which is what the real
/// tables are: `8KDF9LDF9KDF9KDF8LDF8KDF8K` keeps three of its four columns and matches on
/// 72% of positions.
///
/// One caveat on all of the above: those populations are what the sweep *reports*, and a
/// run filter changes that set by more than the runs it removes. The first measurement of
/// this bar looked like a straight loss because the removals let the section sweep's
/// ambiguity test promote a fresh crop of tables in their place — an accounting problem
/// in that test rather than anything about the bar, since fixed, but the reason a filter
/// here has to be judged on a whole run of the tool and not on the population alone.
const PERIODIC_SHARE: usize = 70;

/// Whether a recovered run repeats on a short stride, and so is a table read through a
/// decode rather than text.
///
/// Periodicity is invariant under the byte-key schemes — XOR, ADD and SUB shift every byte
/// the same way, so a stride that repeats in the source repeats in the output — which is
/// why this reads the output and covers the schemes [`is_structured_source`] cannot see the
/// source of as well as the ones it can.
///
/// # Arguments
///
/// * `run` - The candidate recovered bytes
#[must_use]
pub fn is_periodic_run(run: &[u8]) -> bool {
    (1..=MAX_PERIOD).any(|period| {
        // the stride has to fit enough times to be a layout rather than a doubled word
        if run.len() < period * MIN_PERIODS {
            return false;
        }
        // count the positions that match the one a stride back
        let positions = run.len() - period;
        let same = (0..positions)
            .filter(|i| run[*i] == run[i + period])
            .count();
        same * 100 >= positions * PERIODIC_SHARE
    })
}

/// The fewest bytes a column has to hold before the fact that it never changes says
/// anything.
///
/// Two bytes agreeing is a coincidence at any stride; three is the same floor
/// [`MIN_PERIODS`] puts on a repeat, and for the same reason.
const MIN_COLUMN_BYTES: usize = 3;

/// The share of a run's bytes that has to sit in a frozen column before the run is a
/// table of fixed-width records rather than text, in percent.
///
/// [`is_periodic_run`] catches the tables whose whole record repeats. What it leaves is
/// the table whose record *mostly* repeats — a three-byte motif on a four-byte stride
/// with the fourth byte counting through the entries, which decodes to `ovrtovrxRvri…`,
/// `ClipBliBCliMCli<Cli`, `1XZ16XZA6XZQ6XZq5XZ`. Half of every such run disagrees with
/// the position a stride back, so it lands at 50–67% against a 70% bar and every one of
/// them was reported. Reading the run as columns instead of as a lag asks the question
/// the layout actually answers: the motif is frozen, the counter is not.
///
/// Measured over every recovered string the tool reports across the corpus — 45 runs, the
/// genuine recoveries among them being ransom notes, PEM key blocks, a path list, a
/// Session URL, a destructive shell command and an email address — the genuine text
/// scores **0%** frozen and the decoded tables score **23–81%**. There is no distribution
/// between the two, so twenty sits well clear of both.
///
/// A single frozen column does not count, and that is the constant that keeps the
/// separator-delimited host identifiers the sweep is meant to recover: `1.2.3.4` freezes
/// its dot column, which is 42% of the run, and `00:11:22:33:44:55` freezes its colon
/// column at 29%. Both are one column, and it is punctuation. A record layout freezes the
/// motif, which is never only the separator. (An earlier attempt at a column test asked
/// for four identical bytes on one stride and cost a genuine recovery — this is why.)
///
/// Runs that repeat an octet, `1.1.1.1` and `10.10.10.10`, do clear this at 100%. They
/// also clear [`is_periodic_run`], so they were already lost before this bar existed.
const FROZEN_SHARE: usize = 20;

/// Whether a recovered run is a table of fixed-width records read through a decode: one
/// whose bytes, split into columns by the record stride, leave whole columns frozen at a
/// single value.
///
/// Like [`is_periodic_run`] this reads the output rather than the source, because the
/// byte-key schemes shift every byte the same way and so leave a column frozen if it was
/// frozen before the decode.
///
/// # Arguments
///
/// * `run` - The candidate recovered bytes
#[must_use]
pub fn is_columnar_run(run: &[u8]) -> bool {
    // try every stride a fixed-width record could plausibly be laid out on
    (2..=MAX_PERIOD).any(|stride| frozen_share(run, stride) >= FROZEN_SHARE)
}

/// The share of `run`'s bytes sitting in a column that `stride` holds frozen at one
/// value, or zero unless at least two columns are frozen.
///
/// # Arguments
///
/// * `run` - The candidate recovered bytes
/// * `stride` - The record width to split the run into columns on
fn frozen_share(run: &[u8], stride: usize) -> usize {
    // a run shorter than a column's worth of bytes has no layout to read
    if run.len() < MIN_COLUMN_BYTES {
        return 0;
    }
    // walk the columns: each is the bytes one record apart from a given offset
    let mut columns = 0;
    let mut bytes = 0;
    for start in 0..stride {
        // how many bytes this offset picks up before the run ends
        let len = run.len().saturating_sub(start).div_ceil(stride);
        // too short for its being constant to mean anything
        if len < MIN_COLUMN_BYTES {
            continue;
        }
        // frozen means every byte in the column is the one it starts with
        if run[start..]
            .iter()
            .step_by(stride)
            .all(|b| *b == run[start])
        {
            columns += 1;
            bytes += len;
        }
    }
    // one frozen column is a separator, not a record layout
    if columns < 2 {
        return 0;
    }
    bytes * 100 / run.len()
}

/// The share of adjacent byte pairs that may step by one before a region is a counter
/// rather than something with a value hidden inside it, in percent.
///
/// Two neighbouring random bytes differ by exactly one about twice in every 256 pairs,
/// so under a percent either way. Ordinary text under a byte key is barely worse —
/// consecutive letters one apart (`de`, `no`, `st`) are a few percent of English, and a
/// byte key shifts a pair without changing how far apart it is. A table that counts runs
/// at three-quarters and up. There is no distribution between the two to worry about
/// placing the bar inside of, so it sits far from both.
const STEPPED_SHARE: usize = 25;

/// Whether a region's bytes mostly step to their neighbour by one.
///
/// Counted in both directions: a table that counts down is the same table.
///
/// # Arguments
///
/// * `region` - The source bytes underlying a recovered run
#[must_use]
pub fn counts_up_or_down(region: &[u8]) -> bool {
    // a single byte has no neighbour to step to
    let pairs = region.len().saturating_sub(1);
    if pairs == 0 {
        return false;
    }
    // count the adjacent pairs one apart in either direction
    let stepped = region
        .windows(2)
        .filter(|pair| pair[0].abs_diff(pair[1]) == 1)
        .count();
    stepped * 100 >= pairs * STEPPED_SHARE
}

/// The share of adjacent byte pairs that must go the same way before a region is a
/// sorted table rather than something with a value hidden inside it, in percent.
///
/// Two random bytes are non-decreasing about half the time, so this is a bar on a coin
/// flip and the only question is how many flips it takes. At four in five over the
/// [`MIN_MONOTONE_PAIRS`] pairs required, a random region clears it about once in a
/// thousand; a sorted table clears it always. Text under a key sits where the coin does
/// — a key permutes byte values without ordering them — so the gap is the full distance
/// between the two.
const MONOTONE_SHARE: usize = 80;

/// The number of adjacent pairs a region needs before its ordering means anything.
///
/// Short runs go one way by chance constantly: eight of ten pairs ascending is a one in
/// fifty accident across nine pairs and a one in a thousand one across twenty. The floor
/// buys that back while leaving [`counts_up_or_down`] and [`is_wide_int_table`] to cover
/// the shorter tables, which they catch on layout rather than on order.
///
/// Swept over the sample binaries' data sections under every single-byte XOR/ADD/SUB
/// key, counting only the runs that already clear the other three guards, against 30,000
/// keyed real plaintexts:
///
/// | floor | fabrications caught | genuine plaintexts lost |
/// |-------|---------------------|-------------------------|
/// | 12    | 24,658              | 174 (0.580%)            |
/// | 16    | 20,199              | 48 (0.160%)             |
/// | 20    | 5,649               | 33 (0.110%)             |
///
/// Sixteen keeps four fifths of the catch for under a third of the cost, and what it
/// still loses is table-shaped anyway (`CCCAA@AAAAAA@@@AA`). Twenty gives up three
/// quarters of the catch to save almost nothing further, so the knee is at sixteen.
const MIN_MONOTONE_PAIRS: usize = 16;

/// Whether a region's bytes are sorted rather than hidden data.
///
/// The tables a compiler emits are very often built in order — Huffman code lengths,
/// codepoint ranges, sorted string offsets, the cumulative counts a lookup bisects —
/// and a sorted region of small values is the one shape left that walks past every
/// other test here. It does not step by one, so [`counts_up_or_down`] misses it; its
/// columns are not empty, so [`is_wide_int_table`] misses it; its values are spread
/// across a dozen or so, so [`is_dominated_by_one_byte`] misses it. Under a printable
/// key its small values land in the printable range together and produce a run that is
/// long, letter-heavy and diverse, which is to say wordy by every measure available.
///
/// Ciphertext is not sorted. Whatever a cipher's output looks like, it does not climb,
/// and a region that climbs was written by something that had a reason to put it in
/// order — which is a reason it was not encrypted.
///
/// Both directions count: a table read backwards is the same table.
///
/// # Arguments
///
/// * `region` - The source bytes underlying a recovered run
#[must_use]
pub fn is_monotone_table(region: &[u8]) -> bool {
    // too few pairs for an ordering to be anything but chance
    let pairs = region.len().saturating_sub(1);
    if pairs < MIN_MONOTONE_PAIRS {
        return false;
    }
    // count each direction separately, letting equal neighbours serve both — a run of
    // repeats breaks neither an ascending table nor a descending one
    let up = region.windows(2).filter(|pair| pair[0] <= pair[1]).count();
    let down = region.windows(2).filter(|pair| pair[0] >= pair[1]).count();
    up.max(down) * 100 >= pairs * MONOTONE_SHARE
}

/// The element widths a stored table of integers is laid out on.
///
/// Four and eight cover the offset, length, index and pointer tables a compiler emits;
/// anything narrower has no high byte to be empty, and anything wider is rare enough not
/// to be worth the extra chance of a coincidence.
const TABLE_STRIDES: &[usize] = &[4, 8];

/// The number of elements a stride has to span before an empty column means anything.
///
/// Two zeroes the same distance apart is a coincidence that happens constantly in real
/// data; three in a row at a fixed stride is a layout.
const MIN_TABLE_ELEMENTS: usize = 3;

/// Whether a region is a table of little-endian integers rather than hidden data.
///
/// This is the shape that survives every other filter. A compiler-emitted table of
/// offsets, lengths or slot numbers holds values far smaller than the width they are
/// stored in, so its high bytes are all zero — and a zero byte under any additive or
/// XOR key becomes the key itself, a printable character repeating on a fixed stride.
/// The rest of the element is small too, so it lands in the printable range alongside
/// it, and the result is a run that is long, letter-heavy, diverse and balanced: wordy
/// by every measure [`is_wordy`] and [`reads_as_text`] can take, and pure fabrication.
/// [`src_nonprintable_ratio`] does not object either, because a table of small integers
/// is exactly the mostly-non-printable source it is looking for.
///
/// The tell is the stride. Data that a program bothered to hide is a string, and a
/// string has no column that is always empty; a table of integers has one per element,
/// so finding the same offset zero three elements running is a layout rather than an
/// accident. The test walks each phase of each stride in [`TABLE_STRIDES`] and asks
/// whether that phase is empty everywhere it appears.
///
/// # Arguments
///
/// * `region` - The source bytes underlying a recovered run
#[must_use]
pub fn is_wide_int_table(region: &[u8]) -> bool {
    TABLE_STRIDES.iter().any(|stride| {
        // walk each byte position within the element
        (0..*stride).any(|phase| {
            // the number of elements this phase has a byte in
            let elements = region.len().saturating_sub(phase).div_ceil(*stride);
            // enough elements to be a layout, and the column empty in all of them
            elements >= MIN_TABLE_ELEMENTS
                && region
                    .iter()
                    .skip(phase)
                    .step_by(*stride)
                    .all(|byte| *byte == 0)
        })
    })
}

/// The share of a region a single byte value may account for before the region is
/// padding rather than something with a value hidden inside it, in percent.
///
/// Measured over six thousand real printable runs from compiled binaries, each put
/// under a spread of single-byte keys: 99.2% of them have no byte value holding as much
/// as half the run, and the most common byte of ordinary text — the space, or whatever
/// the key turns it into — lands between a tenth and a third. The runs above the bar are
/// rules of `=` and blocks of padding, which are not text worth recovering even when
/// they are genuine. Sparse compiler tables sit far on the other side at two-thirds and
/// up, where the zero holding the empty slots is most of the region.
const DOMINANT_SHARE: usize = 50;

/// Whether one byte value accounts for most of a region.
///
/// # Arguments
///
/// * `region` - The source bytes underlying a recovered run
#[must_use]
pub fn is_dominated_by_one_byte(region: &[u8]) -> bool {
    if region.is_empty() {
        return false;
    }
    // tally the region by value; 256 counters is cheaper than any map
    let mut counts = [0usize; 256];
    for byte in region {
        counts[*byte as usize] += 1;
    }
    // the most common value against the bar
    counts
        .iter()
        .max()
        .is_some_and(|top| top * 100 >= region.len() * DOMINANT_SHARE)
}

/// The fewest same-phase steps a paired-delta table needs before the constant
/// step is a layout rather than a coincidence.
///
/// A fixed step recurring on alternate positions is drawn from 256 values each
/// time it recurs, so eight in a row is a one-in-256⁷ accident against text or
/// ciphertext — while a table built in adjacent pairs produces it from one end
/// to the other.
const MIN_PAIRED_STEPS: usize = 8;

/// Whether a run's bytes step by one constant on alternate positions, and so are
/// a table built in adjacent pairs rather than text.
///
/// This is the period-2 shape the stride tests cannot see. The popcount-family
/// table (`b[i] = 16 − 2·popcount(i)`, an MSVC STL staple) pairs every even
/// element with that element minus two, so its *values* never repeat on any
/// stride ([`is_periodic_run`] needs equality), climb on only half their pairs
/// ([`is_ordered_run`] needs 85%), and freeze no column — but its delta sequence
/// is constant on one phase. The byte-key schemes shift every byte alike and so
/// leave neighbour deltas unchanged, which lets this read source and output
/// equally well; both phases are tried because a run may be carved from either
/// half of a pair.
///
/// Text never does this: a constant alternate-position step means every other
/// character is derived from its neighbour at one fixed distance, and the
/// interleaved shapes that come close (`%s%s%s…`) freeze *both* phases and are
/// already claimed by [`is_periodic_run`].
///
/// # Arguments
///
/// * `bytes` - The candidate run, or the source bytes underlying one
#[must_use]
pub fn is_paired_delta_table(bytes: &[u8]) -> bool {
    // the wrapping neighbour deltas, which the byte-key decodes leave unchanged
    let deltas: Vec<u8> = bytes
        .windows(2)
        .map(|pair| pair[1].wrapping_sub(pair[0]))
        .collect();
    // try both phases: a run may start on either half of a pair
    (0..2).any(|phase| {
        // the phase needs a first step and enough exact repeats to be a layout
        let mut steps = deltas.iter().skip(phase).step_by(2);
        let Some(first) = steps.next() else {
            return false;
        };
        let mut count = 1;
        for step in steps {
            if step != first {
                return false;
            }
            count += 1;
        }
        count >= MIN_PAIRED_STEPS
    })
}

/// Whether a region's bytes all share one parity, and so are a table of even (or
/// odd) machine values rather than hidden data.
///
/// Every neighbour delta of such a region is even — the popcount family again
/// (`16 − 2·popcount(i)` only ever produces even values), and more broadly any
/// table of doubled counts, halved sizes or aligned offsets. Chance hands a
/// region one parity across [`MIN_MONOTONE_PAIRS`] pairs about once in 65,000,
/// and text and ciphertext mix parities: letters, digits and punctuation start
/// mixed, and a single-byte XOR, ADD or SUB shifts every byte's parity the same
/// way, so mixed stays mixed.
///
/// # Arguments
///
/// * `region` - The source bytes underlying a recovered run
#[must_use]
pub fn is_even_delta_table(region: &[u8]) -> bool {
    // too few pairs for a shared parity to be anything but chance
    let pairs = region.len().saturating_sub(1);
    if pairs < MIN_MONOTONE_PAIRS {
        return false;
    }
    // one parity throughout — equivalently, every neighbour delta is even
    region.windows(2).all(|pair| (pair[0] ^ pair[1]) & 1 == 0)
}

/// The highest value a byte may hold for a region to be a small-alphabet table.
///
/// Printable text starts at `0x20`, and the values compiler-emitted index,
/// count and popcount tables are made of end below it: a region that never
/// leaves the control range holds small machine numbers, where the ciphertext
/// of a string under a byte key spreads across the byte range.
const SMALL_ALPHABET_CEILING: u8 = 0x1f;

/// The most distinct values a small-alphabet region may hold, in absolute terms
/// (a longer region is instead allowed one distinct value per
/// [`SMALL_ALPHABET_SPREAD`] bytes, whichever is larger).
const SMALL_ALPHABET_DISTINCT: usize = 12;

/// The bytes of region length that buy a small-alphabet table one more distinct
/// value.
const SMALL_ALPHABET_SPREAD: usize = 8;

/// The fewest bytes a region needs before its alphabet means anything.
const MIN_SMALL_ALPHABET_BYTES: usize = 8;

/// Whether a region is a table of small integers rather than hidden data.
///
/// This is the alphabet read behind the layout reads, and it is what catches the
/// mixed small-int table — values scattered over `0x00..=0x1f` from a dozen-odd
/// distinct entries, in no order, with no dominant byte, no empty stride column
/// and nothing stepping by one — which walks past every other source test and
/// decodes to wordy junk under a whole band of additive keys. Encoded *text*
/// cannot look like this: a byte key maps a string's spread of letters, digits
/// and punctuation onto an equally spread set of ciphertext values, so a region
/// confined to the control range and spelled from a handful of values was never
/// holding a string.
///
/// Deliberately **not** part of [`is_structured_source`]: the crypto pass shares
/// that test and must keep accepting narrow-range sources (lowercase plaintext
/// under a lowercase repeating key lands every byte under `0x20`), so this read
/// belongs only to the single-byte-key paths — [`hidden_claim_runs`] applies it.
///
/// # Arguments
///
/// * `region` - The source bytes underlying a recovered run
#[must_use]
pub fn is_small_alphabet_table(region: &[u8]) -> bool {
    // too short for an alphabet to mean anything
    if region.len() < MIN_SMALL_ALPHABET_BYTES {
        return false;
    }
    // every byte must be a small machine number
    if region.iter().any(|byte| *byte > SMALL_ALPHABET_CEILING) {
        return false;
    }
    // ...drawn from a small set of distinct values (scaled for long regions)
    let distinct: HashSet<u8> = region.iter().copied().collect();
    distinct.len() <= SMALL_ALPHABET_DISTINCT.max(region.len() / SMALL_ALPHABET_SPREAD)
}

/// Whether a region's bytes carry a structure of their own rather than hidden data.
///
/// A decode is only evidence when its input could plausibly have been holding
/// something. Six shapes cannot: bytes that step through consecutive values
/// ([`counts_up_or_down`]), bytes laid out as fixed-width integers
/// ([`is_wide_int_table`]), bytes that are mostly one repeated value
/// ([`is_dominated_by_one_byte`]), bytes that are sorted ([`is_monotone_table`]),
/// bytes built in adjacent pairs ([`is_paired_delta_table`]), and bytes of one
/// parity ([`is_even_delta_table`]). All six are ordinary compiler output, all six
/// are far from random, and all six fabricate readable text under a key that never
/// encoded them — the first because a run of small consecutive numbers shifts into
/// a run of consecutive letters, the second because its empty high bytes hand back
/// the key, the third because a repeated byte hands back the same character over
/// and over, and the last three because any additive key walks a region of small,
/// regularly-built values into the printable range all together.
///
/// They are complementary rather than redundant, and each covers a table the others
/// miss. A sparse table is only caught by the third: it has no column empty in every
/// element, so it is not a wide-int table, and its zeroes do not step, so it does not
/// count — but the zero filling its unused slots is most of its bytes. A table of code
/// lengths or ranges is only caught by the fourth: it steps by two and three as often as
/// by one, it is stored a byte per element so it has no empty column, and its values are
/// spread widely enough that none of them dominates — but it was built in order. The
/// popcount-family table (`b[i] = 16 − 2·popcount(i)`) is only caught by the last two:
/// its even-only deltas never step by one, its modal byte holds 27% of it, it rises as
/// often as it falls and it has no empty column — but every even element is paired with
/// itself minus two, and no byte of it is ever odd.
///
/// # Arguments
///
/// * `region` - The source bytes underlying a recovered run
#[must_use]
pub fn is_structured_source(region: &[u8]) -> bool {
    counts_up_or_down(region)
        || is_wide_int_table(region)
        || is_dominated_by_one_byte(region)
        || is_monotone_table(region)
        || is_paired_delta_table(region)
        || is_even_delta_table(region)
}

/// The fraction of `src` bytes that are non-printable (i.e. looked encoded).
///
/// # Arguments
///
/// * `src` - The original (pre-decode) bytes over a candidate run
#[must_use]
pub fn src_nonprintable_ratio(src: &[u8]) -> f32 {
    if src.is_empty() {
        return 0.0;
    }
    let np = src.iter().filter(|b| !is_printable(**b)).count();
    np as f32 / src.len() as f32
}

/// What one decode contributes to a sweep: the runs that may be reported, and every run
/// it made readable including the decoded tables it would never report.
///
/// The two differ, and the difference matters to any caller weighing one decode against
/// another — see [`hidden_claim_runs`].
pub type RunsAndClaims = (Vec<(usize, String)>, Vec<(usize, String)>);

/// Whether a recovered run is a decoded table rather than text, by any of the four
/// shapes a table takes: one that climbs ([`is_ordered_run`]), one that cycles
/// ([`is_periodic_run`]), one whose records hold whole byte columns still
/// ([`is_columnar_run`]), and one built in adjacent pairs
/// ([`is_paired_delta_table`] — the popcount-family fractal, whose values neither
/// repeat on a stride nor agree on a direction, so the first three never see it).
///
/// # Arguments
///
/// * `run` - The candidate recovered bytes
#[must_use]
pub fn is_table_run(run: &[u8]) -> bool {
    is_ordered_run(run)
        || is_periodic_run(run)
        || is_columnar_run(run)
        || is_paired_delta_table(run)
}

/// Every printable, wordy run in `decoded` that reads as text, **including the decoded
/// tables** — what a decode made readable, before any judgement about what is worth
/// reporting.
///
/// A table is never worth reporting, but it is still the decode's claim on the bytes it
/// came out of, and a caller weighing one decode against another has to count it. The
/// section sweep is that caller: it drops any address more than one key made readable,
/// so silently discarding one key's table hands the address to the next key's and
/// promotes a fabrication that contention had been suppressing. Callers that only report
/// want [`printable_runs`], which is this less the tables.
///
/// Returns `(decoded index, string)` pairs.
///
/// # Arguments
///
/// * `decoded` - The decoded bytes
#[must_use]
pub fn printable_claim_runs(decoded: &[u8]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < decoded.len() {
        // skip non-printable bytes, then grow a maximal printable run
        if !is_printable(decoded[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < decoded.len() && is_printable(decoded[i]) {
            i += 1;
        }
        let run = &decoded[start..i];
        // keep only long, wordy runs that read as text
        if run.len() >= MIN_DECODED_LEN && is_wordy(run) && reads_as_text(run) {
            out.push((start, String::from_utf8_lossy(run).into_owned()));
        }
    }
    out
}

/// Extract maximal printable, wordy runs from `decoded` (length, wordiness,
/// [`reads_as_text`] and [`is_table_run`] — the source bytes are not consulted, so the
/// decoded output is the only layout signal available here).
///
/// Used where there is no index-aligned source to judge the decode against: pairwise
/// SUB, whose output is half the length of its input, and a re-encoding, whose input was
/// printable by construction. [`is_structured_source`] cannot run on either, so the
/// shapes it would have caught have to be caught on the output instead — without that,
/// any stride-2 table of small values (a `u16` codepoint array, a UTF-8 range table)
/// subtracts down to an ascending printable run and is reported as a recovery.
///
/// Returns `(decoded index, string)` pairs.
///
/// # Arguments
///
/// * `decoded` - The decoded bytes
#[must_use]
pub fn printable_runs(decoded: &[u8]) -> Vec<(usize, String)> {
    // the tables are what separates a claim from something worth reporting
    printable_claim_runs(decoded)
        .into_iter()
        .filter(|(_, text)| !is_table_run(text.as_bytes()))
        .collect()
}

/// Every run a byte-key decode made readable out of genuinely-encoded source bytes,
/// **including the decoded tables** — the byte-key counterpart of
/// [`printable_claim_runs`], and what a caller weighing one key against another has to
/// count. Callers that only report want [`hidden_runs`].
///
/// Returns `(index, string)` pairs.
///
/// # Arguments
///
/// * `decoded` - The decoded bytes
/// * `src` - The original bytes, aligned index-for-index with `decoded`
#[must_use]
pub fn hidden_claim_runs(decoded: &[u8], src: &[u8]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < decoded.len() {
        // skip non-printable decoded bytes, then grow a maximal printable run
        if !is_printable(decoded[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < decoded.len() && is_printable(decoded[i]) {
            i += 1;
        }
        let run = &decoded[start..i];
        let src_run = &src[start..i.min(src.len())];
        // require length, wordiness, that it reads as text, that the source actually
        // looked encoded, and that the source carried neither a layout nor a
        // small-integer alphabet of its own (the latter is a byte-key-only read —
        // see is_small_alphabet_table — so it sits beside the shared layout test)
        if run.len() >= MIN_DECODED_LEN
            && is_wordy(run)
            && reads_as_text(run)
            && src_nonprintable_ratio(src_run) >= 0.5
            && !is_structured_source(src_run)
            && !is_small_alphabet_table(src_run)
        {
            out.push((start, String::from_utf8_lossy(run).into_owned()));
        }
    }
    out
}

/// Extract printable, wordy runs from `decoded` (index-aligned to `src`), keeping
/// only runs that are not themselves a table ([`is_table_run`]) and whose original bytes
/// were mostly non-printable and carried neither a structure
/// ([`is_structured_source`]) nor a small-integer alphabet
/// ([`is_small_alphabet_table`]) of their own — so we recover genuinely hidden
/// strings, not merely re-cased plaintext or a decoded table of numbers.
///
/// Used for the byte-key schemes (XOR/SUB/ADD). Returns `(index, string)` pairs.
///
/// # Arguments
///
/// * `decoded` - The decoded bytes
/// * `src` - The original bytes, aligned index-for-index with `decoded`
#[must_use]
pub fn hidden_runs(decoded: &[u8], src: &[u8]) -> Vec<(usize, String)> {
    // the tables are what separates a claim from something worth reporting
    hidden_claim_runs(decoded, src)
        .into_iter()
        .filter(|(_, text)| !is_table_run(text.as_bytes()))
        .collect()
}

/// Readable runs from a decode whose key was *observed* in the program's own decoder
/// loop, rather than guessed.
///
/// The brute-force sweeps try keys, so [`hidden_runs`] holds their output to the full
/// gauntlet — including the output-shape table reads ([`is_table_run`]) whose job is
/// to kill what a wrong key fabricates. A key read out of the loop that references
/// the buffer is not a guess: the code performing the decode corroborates it, so its
/// runs skip the output-shape vetoes and are judged on the source instead. The source
/// must still have looked encoded and carried no layout of its own
/// ([`is_structured_source`], [`is_small_alphabet_table`]), which keeps an
/// observed-key decode of a compiler table — the popcount family, a suballocator
/// ramp — exactly as dead as the sweeps leave it.
///
/// Returns `(index, string)` pairs.
///
/// # Arguments
///
/// * `decoded` - The decoded bytes
/// * `src` - The original bytes, aligned index-for-index with `decoded`
#[must_use]
pub fn observed_key_runs(decoded: &[u8], src: &[u8]) -> Vec<(usize, String)> {
    // the claims extractor applies exactly the source-side gauntlet and no
    // output-shape veto, which is the contract an observed key earns
    hidden_claim_runs(decoded, src)
}

/// Readable runs from a decode corroborated *structurally* — the buffer itself
/// carries the observed key, as interleaved `(value, key)` pair storage.
///
/// Used by the keyed-pair recovery (`GandCrab`-style stack strings, where each
/// character is stored as the shifted byte followed by the key byte). The decode is
/// already corroborated twice over — the loop's immediate was observed, and the
/// buffer's own key column repeats it — so the source-looked-encoded ratio does not
/// apply: a small key leaves much of the shifted text printable (`A` + `0x3a` is
/// `{`), and demanding non-printability would starve exactly the uppercase leetspeak
/// prose this path exists to recover. The structural reads still veto a decoded
/// table: the value column must carry no layout or small-integer alphabet of its own.
///
/// Returns `(index, string)` pairs, indexed in value-column positions.
///
/// # Arguments
///
/// * `decoded` - The decoded value-column bytes
/// * `src` - The stored value-column bytes, aligned index-for-index with `decoded`
#[must_use]
pub fn keyed_pair_runs(decoded: &[u8], src: &[u8]) -> Vec<(usize, String)> {
    // the output-quality reads (length, wordiness, text shape) on the decoded bytes
    printable_claim_runs(decoded)
        .into_iter()
        // the source reads still veto decoded tables: the key column corroborates
        // the decode itself, not the idea that these bytes were hiding a string
        .filter(|(start, text)| {
            let src_run = &src[*start..(start + text.len()).min(src.len())];
            !is_structured_source(src_run) && !is_small_alphabet_table(src_run)
        })
        .collect()
}

/// Whether recovered runs look like a *real* decode rather than coincidental noise.
///
/// A genuine decrypt/decode yields readable text that **dominates a contiguous
/// region**: one long run, or several runs packed together (a string table). A wrong
/// key instead yields a few short runs scattered through otherwise-random bytes. This
/// requires the recovered text to total at least `min_chars` **and** to cover ≥60% of
/// the span from the first run to the last — which rejects the scattered pattern.
///
/// # Arguments
///
/// * `runs` - The recovered `(offset, string)` runs (in ascending offset order)
/// * `min_chars` - The minimum total recovered characters (e.g. [`MIN_RECOVERY_CHARS`])
#[must_use]
pub fn is_strong_recovery(runs: &[(usize, String)], min_chars: usize) -> bool {
    if runs.is_empty() {
        return false;
    }
    // the byte span the runs cover, and the total recovered text within it
    let first = runs.first().map(|(i, _)| *i).unwrap_or(0);
    let last = runs.last().map(|(i, s)| i + s.len()).unwrap_or(0);
    let span = last.saturating_sub(first).max(1);
    let total: usize = runs.iter().map(|(_, s)| s.len()).sum();
    // enough text overall, and it dominates its span (contiguous, not scattered)
    total >= min_chars && total * 100 >= span * 60
}

/// The largest hole between two recovered runs that still leaves them part of the same
/// recovery, in decoded bytes.
///
/// A decoded string table separates its entries with a terminator and occasionally some
/// alignment padding, so neighbouring entries sit a handful of bytes apart; sixteen is
/// comfortably above any of the paddings the sample binaries use and far below the
/// distances a wrong key's scattered hits fall at. It does not need to be tuned: the
/// filter's yield is flat from a gap of zero — runs that literally abut — all the way to
/// sixteen, rejecting the same 53 of the 69 fabrications the samples produce at every
/// value in between. Above that it stops being flat and starts being arbitrary (49 at 64,
/// 57 at 128, 65 at 1024, moving in both directions), which is the signature of a
/// constant that has begun fitting the corpus rather than describing the shape, so the
/// constant is pinned at the top of the plateau instead.
///
/// Measured on [`dense_clusters`], which is also where what it costs is recorded.
pub const MAX_RECOVERY_GAP: usize = 16;

/// Keep only the runs that belong to a dense recovery, dropping the ones that stand alone
/// in a sea of random bytes.
///
/// [`is_strong_recovery`] asks its question of a whole decode at once, which is the right
/// question when the decode covers one buffer: either the key was right and the buffer
/// reads, or it was not. It is the wrong question for a section sweep, where a single key
/// is applied to a quarter of a megabyte at a time. Over that much data a wrong key
/// produces a scatter of individually-plausible runs — ten to twenty characters each,
/// hundreds of kilobytes apart — and the span from the first to the last is so large that
/// the coverage test can neither confirm nor reject anything: it is one verdict for
/// findings that have nothing to do with each other.
///
/// So the runs are grouped first. Each group is a maximal chain of runs whose neighbours
/// sit within [`MAX_RECOVERY_GAP`] of one another, and [`is_strong_recovery`] is then
/// asked of each group on its own. A group survives if it carries `min_chars` of text
/// packed densely, which is what a decoded string table looks like; an isolated short run
/// is a group of one whose only way through is to be long enough on its own merits.
///
/// Over the sample binaries this rejects 53 of the 69 runs a section sweep fabricates and
/// none of the 13 it genuinely recovers — the real ones are all either long single
/// strings or the packed tables the grouping was written for.
///
/// The runs must be in ascending offset order and indexed in the same units, which is the
/// one thing a caller can get wrong: a scheme whose output is not index-aligned with its
/// input (pairwise SUB) has to be filtered in *decoded* offsets, before its indices are
/// mapped back onto the source.
///
/// # Arguments
///
/// * `runs` - The recovered `(offset, string)` runs, in ascending offset order
/// * `gap` - The largest hole between two runs of one recovery (e.g. [`MAX_RECOVERY_GAP`])
/// * `min_chars` - The minimum total characters in a group (e.g. [`MIN_RECOVERY_CHARS`])
#[must_use]
pub fn dense_clusters(
    runs: Vec<(usize, String)>,
    gap: usize,
    min_chars: usize,
) -> Vec<(usize, String)> {
    let mut kept = Vec::new();
    let mut cluster: Vec<(usize, String)> = Vec::new();
    // the end of the last run taken into the open cluster, which is what the next run's
    // distance is measured from
    let mut end: usize = 0;
    for run in runs {
        // a run further than `gap` past the previous one starts a new recovery, so the
        // open one is decided before it is dropped
        if !cluster.is_empty() && run.0 > end.saturating_add(gap) {
            if is_strong_recovery(&cluster, min_chars) {
                kept.append(&mut cluster);
            }
            cluster.clear();
        }
        end = run.0 + run.1.len();
        cluster.push(run);
    }
    // ...and the last cluster is decided the same way
    if is_strong_recovery(&cluster, min_chars) {
        kept.append(&mut cluster);
    }
    kept
}

/// Extract logical recovered items from runs, one `(offset, text)` per item.
///
/// `hidden_runs` splits a PEM armor into per-line runs (newlines are non-printable), so
/// this coalesces a `-----BEGIN … -----` … `-----END … -----` span back into one item
/// (lines joined with `\n`). Crucially it **stops at the end of the structure**: text
/// glued after the closing `-----END-----` marker (or before a `-----BEGIN`) inside the
/// same run is split off as its own item, so a key never bleeds into the next secret.
/// Every other run is its own item — distinct IOCs are never merged.
///
/// # Arguments
///
/// * `runs` - The `(offset, string)` runs in ascending-offset order
#[must_use]
pub fn extract_items(runs: Vec<(usize, String)>) -> Vec<(usize, String)> {
    let mut items: Vec<(usize, String)> = Vec::new();
    let mut i = 0;
    let n = runs.len();
    while i < n {
        let (off, s) = (runs[i].0, runs[i].1.clone());
        // a run that opens a PEM armor starts a structured item
        if let Some(bpos) = find_pem_begin(&s) {
            // any text before the BEGIN marker in this run is its own item
            if bpos > 0 {
                push_item(&mut items, off, &s[..bpos]);
            }
            let pem_off = off + bpos;
            let head = &s[bpos..];
            // case 1: BEGIN and the closing END are in this same run
            if let Some(end) = find_pem_end_close(head) {
                push_pem(&mut items, pem_off, &[head[..end].to_string()]);
                push_item(&mut items, pem_off + end, &head[end..]);
                i += 1;
                continue;
            }
            // case 2: scan following runs for the END line
            let mut lines = vec![head.to_string()];
            i += 1;
            let mut terminated = false;
            while i < n {
                let (roff, rs) = (runs[i].0, runs[i].1.clone());
                if let Some(end) = find_pem_end_close(&rs) {
                    lines.push(rs[..end].to_string());
                    push_pem(&mut items, pem_off, &lines);
                    // text after the closing marker is a separate item
                    push_item(&mut items, roff + end, &rs[end..]);
                    terminated = true;
                    i += 1;
                    break;
                }
                lines.push(rs);
                i += 1;
            }
            // an unterminated BEGIN: emit whatever we gathered as one item
            if !terminated {
                push_pem(&mut items, pem_off, &lines);
            }
            continue;
        }
        // an ordinary run: its own item
        items.push((off, s));
        i += 1;
    }
    items
}

/// Extract every complete PEM-armored block (`-----BEGIN … -----END …-----`) stored
/// verbatim in a raw byte buffer, returning each block's start offset and text.
///
/// Unlike [`extract_items`] (which coalesces per-line runs recovered from a *decode*),
/// this scans raw section bytes directly, so it surfaces a plaintext key/certificate the
/// disassembler never defined as a string — the common case in Go/packed binaries where
/// string data is one unterminated concatenated blob. Only blocks with a matching
/// `-----END …-----` close within [`MAX_PEM_BLOCK`] bytes are returned; an unterminated
/// `-----BEGIN` is skipped.
///
/// # Arguments
///
/// * `bytes` - The raw section bytes to scan
#[must_use]
pub fn find_pem_blocks(bytes: &[u8]) -> Vec<(usize, String)> {
    // the marker to seed each block on (lowercased; matched case-insensitively)
    const BEGIN: &[u8] = b"-----begin";
    let mut out = Vec::new();
    let mut i = 0;
    while i + BEGIN.len() <= bytes.len() {
        // fast path: a PEM armor always opens with a dash run, so skip the rest
        if bytes[i] != b'-' || !bytes[i..i + BEGIN.len()].eq_ignore_ascii_case(BEGIN) {
            i += 1;
            continue;
        }
        // bound the window and decode just this region to text. A real PEM block is pure
        // ASCII through its closing marker, so the lossy conversion is byte-exact there
        // (any trailing binary lies past the close, which `find_pem_end_close` stops at).
        let cap = (i + MAX_PEM_BLOCK).min(bytes.len());
        let region = String::from_utf8_lossy(&bytes[i..cap]);
        match find_pem_end_close(&region) {
            // a complete block: record it and resume scanning after its close
            Some(close) => {
                out.push((i, region[..close].to_string()));
                i += close;
            }
            // an unterminated `-----BEGIN`: skip past the marker and keep scanning
            None => i += BEGIN.len(),
        }
    }
    out
}

/// Push a candidate (leading/trailing) item, keeping only meaningful text.
///
/// # Arguments
///
/// * `items` - The accumulating item list
/// * `off` - The item's start offset
/// * `text` - The candidate text (trimmed before the length/wordiness check)
fn push_item(items: &mut Vec<(usize, String)>, off: usize, text: &str) {
    let trimmed = text.trim();
    // ignore stray fragments; only surface real recovered text
    if trimmed.len() >= MIN_DECODED_LEN && is_wordy(trimmed.as_bytes()) {
        items.push((off, trimmed.to_string()));
    }
}

/// Push a coalesced PEM armor item (its lines joined with newlines).
///
/// # Arguments
///
/// * `items` - The accumulating item list
/// * `off` - The armor's start offset
/// * `lines` - The armor's lines (BEGIN … body … END)
fn push_pem(items: &mut Vec<(usize, String)>, off: usize, lines: &[String]) {
    items.push((off, lines.join("\n")));
}

/// The byte index of a `-----BEGIN` PEM header in `s`, if present.
///
/// # Arguments
///
/// * `s` - The run text
fn find_pem_begin(s: &str) -> Option<usize> {
    // ASCII marker, so the lowercased index matches the original byte index
    s.to_ascii_lowercase().find("-----begin")
}

/// The byte index just past a closing `-----END…-----` marker in `s`, if present.
///
/// Tolerant of a merged decode: after `-----END` it accepts the next run of three or
/// more dashes as the close. Returns `s.len()` when `-----END` is present but no
/// closing dashes are (a truncated armor).
///
/// # Arguments
///
/// * `s` - The run text
fn find_pem_end_close(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    let start = lower.find("-----end")?;
    let bytes = s.as_bytes();
    let mut i = start + "-----end".len();
    // find the next run of >=3 dashes and return the index just after it
    while i < bytes.len() {
        if bytes[i] == b'-' {
            let dash_start = i;
            while i < bytes.len() && bytes[i] == b'-' {
                i += 1;
            }
            if i - dash_start >= 3 {
                return Some(i);
            }
        } else {
            i += 1;
        }
    }
    // "-----END" but no closing dashes: the whole run is armor
    Some(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `printable_runs` recovers a long wordy run and ignores non-printable bytes.
    #[test]
    fn printable_runs_recovers_text() {
        // a non-printable prefix, then a readable payload, then a NUL
        let mut buf = vec![0u8, 1, 2];
        buf.extend_from_slice(b"malicious payload");
        buf.push(0);
        let runs = printable_runs(&buf);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1, "malicious payload");
        assert_eq!(runs[0].0, 3);
    }

    /// A decode whose output climbs is a table that was decoded, not a string that
    /// was hidden.
    ///
    /// The schemes that cannot hand their source bytes to [`is_structured_source`]
    /// need the layout read off their output, and every one of these is a real run a
    /// pairwise-SUB sweep produced over a stride table in a `.data` block: long,
    /// letter-heavy, diverse, bracket-clean, and in order from end to end.
    #[test]
    fn printable_runs_rejects_a_decoded_table() {
        for table in [
            &b"HLPTX\\`dhlp"[..],
            b"*.2JNVjnrz~",
            b"7=CIOU[agmsy",
            b"\"&*.26:>BFJNRVZ^bjrz~",
            b"!!!!77::::==@@@@CCCCFFFFIIIILLLLOORRUUXX[[[[",
        ] {
            // each one clears every test that looks at the characters themselves...
            assert!(is_wordy(table), "{}", String::from_utf8_lossy(table));
            assert!(reads_as_text(table), "{}", String::from_utf8_lossy(table));
            // ...and is given away by the one that looks at their order
            assert!(is_ordered_run(table), "{}", String::from_utf8_lossy(table));
            let mut buffer = vec![0x00, 0x01];
            buffer.extend_from_slice(table);
            buffer.push(0x00);
            assert!(
                printable_runs(&buffer).is_empty(),
                "{}",
                String::from_utf8_lossy(table)
            );
        }
        // the strings a decode exists to find are not in order, and are still returned
        for text in [
            &b"kernel32.dll"[..],
            b"http://c2.example.com/gate.php",
            b"SeDebugPrivilege",
            b"Mozilla/5.0 (Windows NT 10.0)",
        ] {
            assert!(!is_ordered_run(text), "{}", String::from_utf8_lossy(text));
            let mut buffer = vec![0x00, 0x01];
            buffer.extend_from_slice(text);
            buffer.push(0x00);
            assert_eq!(
                printable_runs(&buffer).first().map(|(_, run)| run.as_str()),
                Some(String::from_utf8_lossy(text).as_ref()),
            );
        }
    }

    /// The tables the ordering test misses are the ones that cycle, and the periodicity
    /// test takes them.
    ///
    /// Real pairwise-SUB runs over the UTF-8 range tables in a `.data` block. Each keeps
    /// a fixed element with one varying column — `F` every fourth character, `xz` every
    /// third — which no ordering can see, and each matches itself on more than
    /// [`PERIODIC_SHARE`] of its positions.
    #[test]
    fn is_periodic_run_catches_the_cycling_tables() {
        for table in [
            &b"8KDF9LDF9KDF9KDF8LDF8KDF8K"[..],
            b"Gxz9;xz :xz :xz :xz :xz",
            b"t,q<v\tt8q<v\tt9q<v\tt+q<v\tt,q<v\tu:q<v",
        ] {
            // each clears every test that looks at the characters or their order...
            assert!(is_wordy(table), "{}", String::from_utf8_lossy(table));
            assert!(reads_as_text(table), "{}", String::from_utf8_lossy(table));
            assert!(!is_ordered_run(table), "{}", String::from_utf8_lossy(table));
            // ...and is given away by the one that looks at their stride
            assert!(is_periodic_run(table), "{}", String::from_utf8_lossy(table));
            let mut buffer = vec![0x00, 0x01];
            buffer.extend_from_slice(table);
            buffer.push(0x00);
            assert!(
                printable_runs(&buffer).is_empty(),
                "{}",
                String::from_utf8_lossy(table)
            );
        }
    }

    /// The tables the periodicity test misses are the ones that count, and the column
    /// test takes them.
    ///
    /// Real recoveries from the corpus, every one of them reported before this bar
    /// existed. Each is a three-byte motif on a four-byte stride with the fourth byte
    /// counting through the entries, so half of it disagrees with the position a stride
    /// back and [`PERIODIC_SHARE`] never reaches them — but the motif's two columns are
    /// frozen, which is the same layout read the way it is laid out.
    #[test]
    fn is_columnar_run_catches_the_counted_tables() {
        for table in [
            &b"ovrtovrxRvriovrxRvrCovr"[..],
            b"ClipBliBCliMCli<Cli",
            b"1XZ16XZA6XZQ6XZq5XZ",
            b"tnjVJnjhunjStnj#unj",
            b"T{y S{yNU{yOt{yfu{yWu{yEu{yOt{y",
            b"MlhaGlh Glh-Alh",
            b"tZ_qtZ_BuZ_",
        ] {
            // each clears every other test in the chain, including the two table tests...
            assert!(is_wordy(table), "{}", String::from_utf8_lossy(table));
            assert!(reads_as_text(table), "{}", String::from_utf8_lossy(table));
            assert!(!is_ordered_run(table), "{}", String::from_utf8_lossy(table));
            assert!(
                !is_periodic_run(table),
                "{}",
                String::from_utf8_lossy(table)
            );
            // ...and is given away by the one that reads it as columns
            assert!(is_columnar_run(table), "{}", String::from_utf8_lossy(table));
            let mut buffer = vec![0x00, 0x01];
            buffer.extend_from_slice(table);
            buffer.push(0x00);
            assert!(
                printable_runs(&buffer).is_empty(),
                "{}",
                String::from_utf8_lossy(table)
            );
        }
    }

    /// A literal held together by a separator is not a table of records.
    ///
    /// This is the population [`FROZEN_SHARE`]'s two-column floor was put there for: a
    /// dotted quad and a MAC address each freeze one column, and it is punctuation —
    /// 42% and 29% of the run respectively, both well over the bar had one column been
    /// enough. These are exactly the host identifiers the sweep exists to recover.
    #[test]
    fn is_columnar_run_spares_the_separated_identifiers() {
        for text in [
            &b"1.2.3.4"[..],
            b"00:11:22:33:44:55",
            b"192.168.1.100",
            b"00-1A-2B-3C-4D-5E",
        ] {
            assert!(!is_columnar_run(text), "{}", String::from_utf8_lossy(text));
        }
    }

    /// Nor is prose, however it is punctuated.
    ///
    /// The genuine recoveries the corpus produces, which the column test has to leave
    /// alone: they score zero frozen bytes at every stride, against the 23% the decoded
    /// tables start at.
    #[test]
    fn is_columnar_run_spares_real_recoveries() {
        for text in [
            &b"Session (https://getsession.org, copy and paste to send a friend request):"[..],
            b"Every hour of delay increases the risk of permanent data loss.",
            b"RECOVER-YOUR-FIL== IMPORTANT ==",
            b"-----BEGIN PUBLIC KEY-----",
            b"PROXY %s %s %s %i %i",
            b"MAIL FROM:%s%s%s%s%s%s",
        ] {
            assert!(!is_columnar_run(text), "{}", String::from_utf8_lossy(text));
            let mut buffer = vec![0x00, 0x01];
            buffer.extend_from_slice(text);
            buffer.push(0x00);
            assert_eq!(
                printable_runs(&buffer).first().map(|(_, run)| run.as_str()),
                Some(String::from_utf8_lossy(text).as_ref()),
            );
        }
    }

    /// The tables all three table tests miss are left to be missed, on purpose.
    ///
    /// These are real pairwise-SUB runs over the same range tables, and they are the two
    /// shapes the corpus contains that no test reaches: their repeating column is a
    /// single character in three *and* it starts one byte into the run, so they agree with
    /// themselves on only 30% of positions and no column anchored at the start of a record
    /// is frozen. The test pins the gap so that a later attempt to close it starts from the
    /// measurement rather than from the observation that these obviously look wrong.
    #[test]
    fn the_table_tests_leave_the_sparsely_repeating_tables() {
        for table in [&b"yyIx@GxG@xI>xN9xP5x[+xw"[..], b"jxXi@ViBTiDRiFPiH+im(iw"] {
            assert!(is_wordy(table), "{}", String::from_utf8_lossy(table));
            assert!(reads_as_text(table), "{}", String::from_utf8_lossy(table));
            assert!(!is_ordered_run(table), "{}", String::from_utf8_lossy(table));
            assert!(
                !is_periodic_run(table),
                "{}",
                String::from_utf8_lossy(table)
            );
            assert!(
                !is_columnar_run(table),
                "{}",
                String::from_utf8_lossy(table)
            );
            assert!(
                !is_paired_delta_table(table),
                "{}",
                String::from_utf8_lossy(table)
            );
        }
    }

    /// The MSVC STL popcount table never becomes a recovered string.
    ///
    /// `b[i] = 16 − 2·popcount(i)` walks past the four layout reads: its deltas
    /// are even so nothing steps by one, its modal byte holds 27% of it, it rises
    /// as often as it falls, and it has no empty stride column. Under ADD keys
    /// around 0x70 it decodes to a long, wordy, bracket-clean run — the demo
    /// corpus's `~~|~||z~||z|zzx…` fabrication — so the pair, parity and alphabet
    /// reads are what have to condemn it.
    #[test]
    fn hidden_runs_reject_the_popcount_table() {
        // the table exactly as the compiler emits it, carved from +0x01 the way
        // the sweep's printable window broke it
        let table: Vec<u8> = (0u32..256)
            .map(|i| 16 - 2 * u8::try_from(i.count_ones()).expect("popcount of a byte"))
            .collect();
        let region = &table[1..];
        // all three new source reads see the layout...
        assert!(is_small_alphabet_table(region));
        assert!(is_paired_delta_table(region));
        assert!(is_even_delta_table(region));
        // ...which none of the original four does
        assert!(!counts_up_or_down(region));
        assert!(!is_wide_int_table(region));
        assert!(!is_dominated_by_one_byte(region));
        assert!(!is_monotone_table(region));
        // the ADD 0x70 fabrication really is wordy, text-reading junk — the hazard
        let decoded: Vec<u8> = region.iter().map(|byte| byte.wrapping_add(0x70)).collect();
        assert!(decoded.starts_with(b"~~|~||z~||z|zzx"));
        assert!(is_wordy(&decoded));
        assert!(reads_as_text(&decoded));
        // the output-side pair read condemns it even where no source is in hand
        assert!(is_table_run(&decoded));
        // and the byte-key sweep path drops it end to end
        assert!(src_nonprintable_ratio(region) >= 0.5);
        assert!(hidden_runs(&decoded, region).is_empty());
    }

    /// An observed key skips the output-shape vetoes but never the source reads.
    ///
    /// The popcount table under its matching ADD 0x70 key decodes to wordy,
    /// text-reading junk — the exact fabrication the overhaul killed — so the
    /// observed-key path must still kill it on the source layout, while a keyed
    /// leetspeak ransom line (digit-substituted words and all) comes through.
    #[test]
    fn observed_key_runs_keep_the_source_vetoes() {
        // the popcount table, carved as the sweep's printable window broke it
        let table: Vec<u8> = (0u32..256)
            .map(|i| 16 - 2 * u8::try_from(i.count_ones()).expect("popcount of a byte"))
            .collect();
        let region = &table[1..];
        let decoded: Vec<u8> = region.iter().map(|byte| byte.wrapping_add(0x70)).collect();
        // the junk really is wordy and bracket-clean, so only the source condemns it
        assert!(decoded.starts_with(b"~~|~||z~||z|zzx"));
        assert!(observed_key_runs(&decoded, region).is_empty());
        // ...and the structural keyed-pair read kills it the same way
        assert!(keyed_pair_runs(&decoded, region).is_empty());
        // a PPMd-style suballocator ramp is monotone at the source, so it dies too
        let ramp: Vec<u8> = (0u8..64).map(|i| i * 4).collect();
        let ramp_decoded: Vec<u8> = ramp.iter().map(|byte| byte.wrapping_add(0x24)).collect();
        assert!(ramp_decoded.starts_with(b"$(,048<@"));
        assert!(observed_key_runs(&ramp_decoded, &ramp).is_empty());
        assert!(keyed_pair_runs(&ramp_decoded, &ramp).is_empty());
        // the confirmed leetspeak note line under a high-bit key still comes through
        let line = b"* DO NO4 MODIF9 ENC2904ED FILE3";
        let src: Vec<u8> = line.iter().map(|byte| byte ^ 0x9c).collect();
        assert_eq!(
            observed_key_runs(line, &src)
                .first()
                .map(|(_, run)| run.as_str()),
            Some("* DO NO4 MODIF9 ENC2904ED FILE3")
        );
    }

    /// The source reads never fire on the note's ciphertext shapes.
    ///
    /// The confirmed `GandCrab` lines reach the source tests two ways — the whole
    /// line under a byte key, and the value column of the `(value, key)` pair
    /// storage — and the alphabet, pair and parity reads (and the rest of the
    /// structured-source battery) must claim neither, or the recovery dies at the
    /// source for looking like the tables it is nothing like.
    #[test]
    fn note_ciphertext_trips_no_source_reads() {
        for line in [
            &b" GANDC2AB 6"[..],
            b"A44EN4ION!",
            b"IN O2DE2 4O 02E6EN4 DA4A DAMAGE",
            b"* DO NO4 MODIF9 ENC2904ED FILE3",
            b"* DO NO4 CHANGE DA4A BELO7",
            b"//WWW.ToRPRojecT.oRg/ ",
        ] {
            // the keyed contiguous shape: the line under a high-bit XOR key
            let keyed: Vec<u8> = line.iter().map(|byte| byte ^ 0x9c).collect();
            // the keyed-pair value column: the line shifted by the observed 0x3a
            let column: Vec<u8> = line.iter().map(|byte| byte.wrapping_add(0x3a)).collect();
            for src in [&keyed, &column] {
                let text = String::from_utf8_lossy(line);
                assert!(!is_small_alphabet_table(src), "{text}");
                assert!(!is_paired_delta_table(src), "{text}");
                assert!(!is_even_delta_table(src), "{text}");
                assert!(!is_structured_source(src), "{text}");
            }
        }
    }

    /// A mixed small-integer table is condemned by its alphabet alone.
    ///
    /// Values scattered over the control range from ten distinct entries dodge
    /// every layout read — mixed parity, varied deltas, no dominance, no order,
    /// no empty column — and under additive keys in the 0x40s they decode to
    /// wordy junk (the pack's ignored `?AFS@w@????T?LEAC` twin). The alphabet is
    /// the tell: encoded text spreads across the byte range, machine numbers stay
    /// under 0x20.
    #[test]
    fn small_alphabet_catches_the_mixed_small_int_table() {
        #[rustfmt::skip]
        let table: &[u8] = &[
            0x03, 0x15, 0x08, 0x1b, 0x05, 0x0e, 0x18, 0x01, 0x12, 0x0b, 0x1b, 0x05,
            0x15, 0x03, 0x0e, 0x08, 0x1b, 0x01, 0x12, 0x18, 0x0b, 0x05, 0x15, 0x0e,
        ];
        // none of the layout reads claims it...
        assert!(!counts_up_or_down(table));
        assert!(!is_wide_int_table(table));
        assert!(!is_dominated_by_one_byte(table));
        assert!(!is_monotone_table(table));
        assert!(!is_paired_delta_table(table));
        assert!(!is_even_delta_table(table));
        // ...the alphabet does
        assert!(is_small_alphabet_table(table));
        // and the byte-key sweep path drops what it would otherwise fabricate
        let decoded: Vec<u8> = table.iter().map(|byte| byte.wrapping_add(0x44)).collect();
        assert!(is_wordy(&decoded));
        assert!(reads_as_text(&decoded));
        assert!(src_nonprintable_ratio(table) >= 0.5);
        assert!(hidden_runs(&decoded, table).is_empty());
    }

    /// A table built in adjacent pairs is condemned by its frozen alternate step.
    #[test]
    fn paired_delta_catches_a_pair_table() {
        // nine (value, value + 0x33) pairs: distinct values, mixed parity, mixed
        // direction, nothing dominant — only the alternate-phase step is frozen
        #[rustfmt::skip]
        let table: &[u8] = &[
            0xb0, 0xe3, 0x40, 0x73, 0x90, 0xc3, 0x20, 0x53, 0x70, 0xa3,
            0x08, 0x3b, 0x60, 0x93, 0x18, 0x4b, 0x88, 0xbb,
        ];
        // the pair structure is invisible to every other read...
        assert!(!counts_up_or_down(table));
        assert!(!is_wide_int_table(table));
        assert!(!is_dominated_by_one_byte(table));
        assert!(!is_monotone_table(table));
        assert!(!is_even_delta_table(table));
        assert!(!is_small_alphabet_table(table));
        // ...and the frozen alternate step gives it away
        assert!(is_paired_delta_table(table));
        assert!(is_structured_source(table));
    }

    /// A table of one parity is a table even when its values leave the control range.
    #[test]
    fn even_delta_catches_a_same_parity_table() {
        // all-even values in no order, spread too wide for the alphabet read and
        // too irregular for the pair read
        #[rustfmt::skip]
        let table: &[u8] = &[
            0x22, 0x08, 0x30, 0x14, 0x3a, 0x02, 0x26, 0x0c, 0x38, 0x1e,
            0x04, 0x2c, 0x12, 0x36, 0x0a, 0x20, 0x16, 0x3c,
        ];
        // every other read passes it...
        assert!(!counts_up_or_down(table));
        assert!(!is_wide_int_table(table));
        assert!(!is_dominated_by_one_byte(table));
        assert!(!is_monotone_table(table));
        assert!(!is_paired_delta_table(table));
        assert!(!is_small_alphabet_table(table));
        // ...and the shared parity condemns it
        assert!(is_even_delta_table(table));
        assert!(is_structured_source(table));
    }

    /// The new table reads leave the confirmed recoveries' sources alone.
    ///
    /// The pack's real wins — the dead-drop URL, the proton.me contact, ransom
    /// prose, a PEM body — decode from high-distinct, mostly-printable-plaintext
    /// ciphertext, so none of the alphabet, pair or parity reads may claim a keyed
    /// real plaintext's source, and none of their decoded texts reads as a table.
    #[test]
    fn new_source_reads_spare_real_recoveries() {
        for (plain, key) in [
            (&b"http://blog.yahoo.com/dead-drop/post"[..], 0x9du8),
            (b"    >> 01Flip@proton.me", 0xa5),
            (b"Your files have been encrypted. Do not attempt", 0xd7),
            (b"MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA", 0xeb),
        ] {
            let src: Vec<u8> = plain.iter().map(|byte| byte ^ key).collect();
            let text = String::from_utf8_lossy(plain);
            assert!(!is_small_alphabet_table(&src), "{text}");
            assert!(!is_paired_delta_table(&src), "{text}");
            assert!(!is_even_delta_table(&src), "{text}");
            assert!(!is_structured_source(&src), "{text}");
            assert!(!is_table_run(plain), "{text}");
            // the decode still comes through the full byte-key path
            assert_eq!(
                hidden_runs(plain, &src)
                    .first()
                    .map(|(_, run)| run.as_str()),
                Some(text.as_ref()),
                "{text}"
            );
        }
    }

    /// A word that contains itself once is a word, not a table.
    ///
    /// [`MIN_PERIODS`] is what separates them, and this is the population it was measured
    /// against: Go type names, which are their own package name doubled and so match on
    /// nearly every position of the one stride they repeat on.
    #[test]
    fn is_periodic_run_spares_a_doubled_word() {
        for text in [
            &b"*context.Context"[..],
            b"*regexp.Regexp",
            b"*hmac.hmac",
            b"PROXY %s %s %s %i %i",
            b"MAIL FROM:%s%s%s%s%s%s",
        ] {
            assert!(!is_periodic_run(text), "{}", String::from_utf8_lossy(text));
        }
    }

    /// The recoveries a pairwise-SUB sweep exists to produce survive the table test.
    ///
    /// Every one of these is a real run recovered from a sample's encoded `.rodata`
    /// blob, and they are what bounds how far [`ORDERED_SHARE`] may be lowered: a ransom
    /// note and a PEM body are exactly the long, letter-heavy, partially-repetitive text
    /// a looser bar starts to claim. None of them agrees on a direction more than 62% of
    /// the time, so the 85% bar clears all five with room to spare.
    #[test]
    fn is_ordered_run_spares_real_recoveries() {
        for text in [
            &b"deskthemmsstylesthemepacdadiagradaschemasas"[..],
            b"MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA",
            b"Your files have been encrypted. Do not attempt",
            b"Session (https://getsession.org, copy and paste",
            b"    >> ${cipher_data}sleep 5 && dd if=/dev/urandom",
        ] {
            assert!(!is_ordered_run(text), "{}", String::from_utf8_lossy(text));
            let mut buffer = vec![0x00, 0x01];
            buffer.extend_from_slice(text);
            buffer.push(0x00);
            assert_eq!(
                printable_runs(&buffer).first().map(|(_, run)| run.as_str()),
                Some(String::from_utf8_lossy(text).as_ref()),
            );
        }
    }

    /// Build the `(offset, string)` pairs a decode would have produced.
    ///
    /// # Arguments
    ///
    /// * `runs` - The `(offset, text)` pairs to render
    fn runs(runs: &[(usize, &str)]) -> Vec<(usize, String)> {
        runs.iter().map(|(i, s)| (*i, (*s).to_string())).collect()
    }

    /// A packed group survives on its combined length; the same runs scattered do not.
    #[test]
    fn dense_clusters_keeps_the_packed_group() {
        // three short entries of a decoded string table, one terminator apart: no entry
        // reaches `MIN_RECOVERY_CHARS` alone, and together they plainly do
        let table = runs(&[
            (100, "CreateFile"),
            (111, "WriteFileEx"),
            (123, "DeleteFileW"),
        ]);
        assert_eq!(
            dense_clusters(table.clone(), MAX_RECOVERY_GAP, MIN_RECOVERY_CHARS),
            table
        );
        // the identical text, spread across a section: every run is now its own group,
        // and none of them is long enough to stand alone
        let scattered = runs(&[
            (100, "CreateFile"),
            (9_000, "WriteFileEx"),
            (70_000, "DeleteFileW"),
        ]);
        assert!(dense_clusters(scattered, MAX_RECOVERY_GAP, MIN_RECOVERY_CHARS).is_empty());
    }

    /// Each group is judged alone, so a real recovery is not saved or sunk by its
    /// neighbours.
    #[test]
    fn dense_clusters_judges_each_group_separately() {
        let mixed = runs(&[
            // an isolated short run — dropped
            (0, "GetTickCount"),
            // a genuine long one, far from anything — kept on its own length
            (5_000, "https://example.invalid/gate.php?id="),
            // a second isolated short run, which the long one must not carry
            (60_000, "RegOpenKeyEx"),
        ]);
        let kept = dense_clusters(mixed, MAX_RECOVERY_GAP, MIN_RECOVERY_CHARS);
        assert_eq!(
            kept,
            runs(&[(5_000, "https://example.invalid/gate.php?id=")])
        );
    }

    /// A group only counts when its text fills the span it covers.
    #[test]
    fn dense_clusters_requires_the_group_to_be_dense() {
        // four runs chained by gaps just inside `MAX_RECOVERY_GAP`: one group, plenty of
        // text, but more hole than text, which is the scattered shape at close range
        let sparse = runs(&[
            (0, "AAAbcdefgh"),
            (26, "BBBcdefghi"),
            (52, "CCCdefghij"),
            (78, "DDDefghijk"),
        ]);
        assert!(dense_clusters(sparse, MAX_RECOVERY_GAP, MIN_RECOVERY_CHARS).is_empty());
    }

    /// `is_wordy` rejects a constant run (no diversity) but accepts real text.
    #[test]
    fn is_wordy_requires_diversity() {
        assert!(!is_wordy(b"AAAAAAAAAA"));
        assert!(is_wordy(b"kernel32.dll"));
    }

    /// Digit-substituted (leetspeak) prose gets a second chance at wordiness.
    ///
    /// The confirmed `GandCrab` ransom note spells its words with digits —
    /// `A44EN4ION`, `02E6EN4` (prevent), `ENC2904ED` (encrypted) — so a run carved
    /// mid-word can fall under the letter-majority bar even though every digit stands
    /// for a letter. The second chance maps the leet digits to the letters they stand
    /// for and retries the majority test; it is a second chance only, so everything
    /// that passes today still passes, and junk gains nothing: the popcount
    /// fabrication has no digits at all, the AES T-table junk's `6` is deliberately
    /// unmapped (no leet word uses it), and a digits-only ramp has too few real
    /// letters to qualify.
    #[test]
    fn is_wordy_gives_digit_substituted_prose_a_second_chance() {
        // a truncated carve of the confirmed note line fails the plain letter
        // majority (9 letters over 19 bytes) and must survive on the leet map
        assert!(is_wordy(b"IN O2DE2 4O 02E6EN4"));
        // the confirmed note lines themselves stay wordy
        for line in [
            &b"A44EN4ION!"[..],
            b"IN O2DE2 4O 02E6EN4 DA4A DAMAGE",
            b"* DO NO4 MODIF9 ENC2904ED FILE3",
            b"* DO NO4 CHANGE DA4A BELO7",
            b"//WWW.ToRPRojecT.oRg/ ",
        ] {
            assert!(is_wordy(line), "{}", String::from_utf8_lossy(line));
        }
        // the overhaul's fabrication kills stay non-wordy: no digits to map
        assert!(!is_wordy(b"~~|~||z~||z|zzx"));
        // ...and `6` maps to nothing, so the T-table junk stays under the bar
        assert!(!is_wordy(b"=&&jL66Zl??A~"));
        // emulated table fragments are short, spaceless tokens: no second chance
        assert!(!is_wordy(b"3g1>2O7"));
        // a digits-only ramp has no letters: the second chance demands real ones
        assert!(!is_wordy(b"0123 4567 8901 2345"));
        // a letter-and-digit transport token (base64) is one word, not prose
        assert!(!is_wordy(b"3q1R5k9W2z4V7p0X8"));
    }

    /// `reads_as_text` drops a run whose punctuation answers to nothing, and keeps
    /// everything a decode is actually meant to surface.
    ///
    /// A wrong key draws from the whole printable range, so it produces `}` and `)` at
    /// the same rate it produces anything else, and it has no reason to have produced
    /// the opener first — while the constructs a recovery actually is (a format
    /// template, a JSON body, an argument list) close every bracket they open.
    #[test]
    fn reads_as_text_rejects_unmatched_closers() {
        // a real wrong-key run, verbatim: a `}` in the fourth character with no `{`
        assert!(!reads_as_text(br#""Ka}cd@kiny+Oeklgd`h&Wzfin"#));
        // ...and a crossed pair, which is the same failure one level in
        assert!(!reads_as_text(b"lookup [key}"));
        // the constructs a real recovery is made of all balance
        for good in [
            &b"{host}:{port}/{path}?id={id}"[..],
            br#"{"retry":[1,2,3],"jitter":{"min":5}}"#,
            b"GetProcAddress(hMod, name)",
            b"http://c2.example.com/gate.php",
            br"SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
            b"MIIEowIBAAKCAQEAsfPDvbbHZm9jK2xQ0lVpTt7yRmHnQ3wDcFgKpLoEuIvXyNaB",
        ] {
            assert!(reads_as_text(good), "{}", String::from_utf8_lossy(good));
        }
        // an unclosed opener is not evidence: a genuine recovery is routinely cut off
        // by the end of the buffer it was carved from
        assert!(reads_as_text(b"connect to (host, port"));
        // nor is stray punctuation on its own — a pipe is a C2 config's field
        // separator, and a backtick and a caret are shell escape characters
        assert!(reads_as_text(b"1.2.3.4|443|60|agent-01|none"));
        assert!(reads_as_text(b"powershell -c `whoami` ^& exit"));
    }

    /// A run that is wordy but reads as wrong-key output never becomes a recovery.
    #[test]
    fn run_extractors_drop_wrong_key_output() {
        let mut buf = vec![0u8];
        buf.extend_from_slice(b"yl}svzu|4u|pw{ul}{~zup0u||c");
        buf.push(0);
        // it clears every other bar — long enough, majority letters, diverse — so
        // only the text-quality gate stands between it and the report
        assert!(is_wordy(&buf[1..buf.len() - 1]));
        assert!(printable_runs(&buf).is_empty());
        assert!(hidden_runs(&buf, &vec![0x01u8; buf.len()]).is_empty());
    }

    /// `is_strong_recovery` accepts contiguous readable text but rejects scattered
    /// fragments and too-little text.
    #[test]
    fn is_strong_recovery_needs_dominant_text() {
        // one long contiguous run → real recovery
        assert!(is_strong_recovery(
            &[(0, "http://c2.example.com/gate".to_string())],
            24
        ));
        // several packed runs (a string table) → real recovery
        assert!(is_strong_recovery(
            &[
                (0, "kernel32.dll".to_string()),
                (13, "CreateProcessW".to_string())
            ],
            24
        ));
        // a long run plus a fragment scattered far away → rejected (low coverage)
        assert!(!is_strong_recovery(
            &[
                (0, "readable payload here!".to_string()),
                (900, "frag".to_string())
            ],
            24
        ));
        // too little text overall → rejected
        assert!(!is_strong_recovery(&[(0, "short".to_string())], 24));
    }

    /// `extract_items` merges a PEM armor into one item, splits text glued after the
    /// END marker into its own item, and keeps distinct secrets apart.
    #[test]
    fn extract_items_merges_pem_and_splits_trailing() {
        let runs = vec![
            (0, "sleep 5 && rm -rf ${self_name}".to_string()), // a command — its own item
            (64, "-----BEGIN PUBLIC KEY-----".to_string()),
            (96, "MIIBIjANBgkqhkiG9w0BAQEFAAOC".to_string()),
            // the END marker is glued to trailing ransom-note text (one run)
            (
                128,
                "-----END PUBLIC KEY-----RECOVER-YOUR-FILES IMPORTANT".to_string(),
            ),
            (256, "admin@evil.example".to_string()), // an email — its own item
        ];
        let items = extract_items(runs);
        // command, merged PEM, trailing note, email → four items
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].1, "sleep 5 && rm -rf ${self_name}");
        // the PEM item carries BEGIN … body … END and stops at the END marker
        assert_eq!(items[1].0, 64);
        assert!(items[1].1.starts_with("-----BEGIN PUBLIC KEY-----"));
        assert!(items[1].1.contains("MIIBIjANBgkqhkiG9w0BAQEFAAOC"));
        assert!(items[1].1.ends_with("-----END PUBLIC KEY-----"));
        assert!(!items[1].1.contains("RECOVER"));
        // the trailing note is split into its own item
        assert!(items[2].1.contains("RECOVER-YOUR-FILES"));
        assert_eq!(items[3].1, "admin@evil.example");
    }

    /// An unterminated `-----BEGIN` (no END) is emitted as a single item.
    #[test]
    fn extract_items_unterminated_begin() {
        let runs = vec![
            (0, "-----BEGIN PUBLIC KEY-----".to_string()),
            (32, "MIIBsomekeydatahere".to_string()),
        ];
        let items = extract_items(runs);
        assert_eq!(items.len(), 1);
        assert!(items[0].1.contains("-----BEGIN PUBLIC KEY-----"));
        assert!(items[0].1.contains("MIIBsomekeydatahere"));
    }

    /// `find_pem_blocks` extracts a full block from raw bytes surrounded by binary,
    /// starting exactly at the `-----BEGIN` marker.
    #[test]
    fn find_pem_blocks_extracts_from_raw_bytes() {
        let pem = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEF\n-----END PUBLIC KEY-----";
        // binary padding on both sides, mimicking a Go string blob in .rodata
        let mut buf = vec![0x00u8, 0xff, 0x80, b'9', b'9'];
        let start = buf.len();
        buf.extend_from_slice(pem.as_bytes());
        buf.extend_from_slice(&[0x00, 0x01, 0xfe]);
        let blocks = find_pem_blocks(&buf);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].0, start);
        assert_eq!(blocks[0].1, pem);
    }

    /// An unterminated `-----BEGIN` (no `-----END…-----` close) yields no block.
    #[test]
    fn find_pem_blocks_skips_unterminated() {
        let buf = b"junk-----BEGIN PUBLIC KEY-----\nMIIBnoendmarkerhere".to_vec();
        assert!(find_pem_blocks(&buf).is_empty());
    }

    /// Two blocks in one buffer are both extracted at their respective offsets.
    #[test]
    fn find_pem_blocks_extracts_multiple() {
        let a = "-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----";
        let b = "-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----";
        let mut buf = Vec::new();
        buf.extend_from_slice(a.as_bytes());
        buf.extend_from_slice(&[0x00, 0x00]);
        let b_start = buf.len();
        buf.extend_from_slice(b.as_bytes());
        let blocks = find_pem_blocks(&buf);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], (0, a.to_string()));
        assert_eq!(blocks[1], (b_start, b.to_string()));
    }

    /// `hidden_runs` requires the source to have looked encoded (mostly non-printable).
    #[test]
    fn hidden_runs_requires_encoded_source() {
        // decoded text whose source was all printable → rejected (not hidden)
        let decoded = b"plain readable text here";
        assert!(hidden_runs(decoded, decoded).is_empty());
        // same text but the source bytes were non-printable → surfaced
        let src: Vec<u8> = decoded.iter().map(|byte| byte ^ 0xa5).collect();
        assert_eq!(hidden_runs(decoded, &src).len(), 1);
    }

    /// A table of little-endian integers never becomes a recovered string.
    ///
    /// This is the shape that costs the most, taken verbatim from a real binary's
    /// `.rdata`: small offsets stored four bytes wide, so every fourth byte is empty.
    /// It clears every other bar — the source is three-quarters non-printable, and
    /// adding any byte key to it produces a long, letter-heavy, diverse, bracket-free
    /// run — which is why the guard has to be about the *layout* of the source rather
    /// than the quality of the output. Left unguarded, one such region reports under a
    /// dozen consecutive keys and fills the whole per-sample cap with fabrications.
    #[test]
    fn hidden_runs_rejects_a_wide_int_table() {
        #[rustfmt::skip]
        let table: &[u8] = &[
            0x3d, 0x02, 0x00, 0x10, 0x45, 0x1a, 0x00, 0x10, 0x45, 0x1a, 0x00, 0x10,
        ];
        // the fabricated output really is wordy and reads as text — that is the hazard
        let decoded: Vec<u8> = table.iter().map(|byte| byte.wrapping_add(0x31)).collect();
        assert!(is_wordy(&decoded), "{}", String::from_utf8_lossy(&decoded));
        assert!(reads_as_text(&decoded));
        assert!(src_nonprintable_ratio(table) >= 0.5);
        // ...so it is the empty column that has to condemn it
        assert!(is_wide_int_table(table));
        assert!(hidden_runs(&decoded, table).is_empty());
    }

    /// A sorted table of small values never becomes a recovered string.
    #[test]
    fn hidden_runs_rejects_a_sorted_table() {
        // a code-length table, verbatim from a real binary's `.rdata`: built in order,
        // in segments, with a small header between them
        #[rustfmt::skip]
        let table: &[u8] = &[
            0x0e, 0x10, 0x12, 0x12, 0x14, 0x14, 0x06, 0x02, 0x00, 0x00, 0x00, 0x02, 0x02,
            0x03, 0x03, 0x03, 0x06, 0x06, 0x07, 0x07, 0x08, 0x09, 0x0b, 0x0b, 0x0b,
        ];
        // replayed under a key lifted from elsewhere in the same binary
        let key = b"certs";
        let decoded: Vec<u8> = table
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ key[(i + 3) % key.len()])
            .collect();
        // the fabrication clears every test that looks at the output alone
        assert!(is_wordy(&decoded), "{}", String::from_utf8_lossy(&decoded));
        assert!(reads_as_text(&decoded));
        assert!(src_nonprintable_ratio(table) >= 0.5);
        // and every structural test but the one that reads its order
        assert!(!counts_up_or_down(table));
        assert!(!is_wide_int_table(table));
        assert!(!is_dominated_by_one_byte(table));
        assert!(is_monotone_table(table));
        assert!(hidden_runs(&decoded, table).is_empty());
    }

    /// Ordering only condemns a region when there is enough of it to mean something,
    /// and real ciphertext is not ordered.
    #[test]
    fn is_monotone_table_needs_length_and_spares_ciphertext() {
        // a short ascending run is a coincidence, not a layout
        assert!(!is_monotone_table(&[0x02, 0x05, 0x09, 0x0c, 0x11, 0x14]));
        // the same climb carried far enough is a table
        let table: Vec<u8> = (0..20u8).map(|i| i * 3).collect();
        assert!(is_monotone_table(&table));
        // and read backwards it is the same table
        let reversed: Vec<u8> = table.iter().rev().copied().collect();
        assert!(is_monotone_table(&reversed));
        // a real recovery's source — ordinary text under a word key — is not ordered
        let plain = b"http://c2.example.com/gate";
        let key = b"secret";
        let ct: Vec<u8> = plain
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ key[i % key.len()])
            .collect();
        assert!(!is_monotone_table(&ct));
        assert!(!is_structured_source(&ct));
    }

    /// A run of consecutive values never becomes a recovered string either, and a
    /// genuinely keyed source is left alone by both structural tests.
    #[test]
    fn is_structured_source_separates_tables_from_hidden_data() {
        // a length/index table that counts, verbatim from a real binary's `.rdata`
        #[rustfmt::skip]
        let counter: &[u8] = &[
            0x04, 0x05, 0x06, 0x08, 0x09, 0x0a, 0x0c, 0x0d, 0x0e, 0x10, 0x12, 0x12,
            0x14, 0x14, 0x06, 0x02, 0x00, 0x00, 0x00, 0x02, 0x02, 0x03, 0x03, 0x03, 0x06,
        ];
        assert!(counts_up_or_down(counter));
        assert!(is_structured_source(counter));
        // real hidden data trips neither test, including the awkward case: printable
        // plaintext under a printable key, which is neither uniform nor broad
        for (plain, key) in [
            (&b"http://c2.example.com/gate.php"[..], 0xa5u8),
            (br#"{"host":"10.1.2.3","port":443,"sleep":60}"#, 0x5c),
            (br"SOFTWARE\Microsoft\Windows\CurrentVersion\Run", 0x6b),
            (b"payloadconfigurationdata", 0x21),
        ] {
            let src: Vec<u8> = plain.iter().map(|byte| byte ^ key).collect();
            assert!(
                !is_structured_source(&src),
                "{}",
                String::from_utf8_lossy(plain)
            );
        }
    }

    /// A sparse table is rejected even though it is neither a counter nor a wide-int
    /// table — the zero filling its unused slots is most of it.
    #[test]
    fn is_structured_source_rejects_a_sparse_table() {
        // verbatim from a real binary's `.rdata`: mostly-empty slots with occasional
        // small values, which under an ADD key reads back as a wall of one letter
        #[rustfmt::skip]
        let sparse: &[u8] = &[
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x3d, 0x1b, 0x35, 0xff, 0x08, 0x08, 0x08,
            0x36, 0x38,
        ];
        // neither of the layout tests sees it: nothing steps, and no column is empty
        // in every element of a stride
        assert!(!counts_up_or_down(sparse));
        assert!(!is_wide_int_table(sparse));
        assert!(is_dominated_by_one_byte(sparse));
        assert!(is_structured_source(sparse));
        // and it would otherwise have been reported: it decodes to a readable-looking
        // run over bytes that all look encoded
        let decoded: Vec<u8> = sparse.iter().map(|byte| byte.wrapping_add(0x41)).collect();
        assert!(is_wordy(&decoded));
        assert!(src_nonprintable_ratio(sparse) >= 0.5);
        assert!(hidden_runs(&decoded, sparse).is_empty());
    }
}
