//! Bounded chained decoding — recovering plaintext through more than one layer.
//!
//! A single decode primitive only reaches data hidden one layer deep. Obfuscation
//! routinely stacks layers instead: a payload is encrypted and then base64-armored so
//! it survives being stored as a string, or a base64 blob is XORed so it does not look
//! like base64. Either shape is invisible to a one-shot decoder even though both
//! layers are individually trivial.
//!
//! This module applies the [`crate::thorpal::detect::decode`] primitives in sequence, up to
//! [`MAX_DECODE_DEPTH`] layers, and reports the chain that recovered the text.
//!
//! # Why depth is dangerous, and what holds it in check
//!
//! Every extra layer is another few hundred chances for random bytes to come out
//! looking printable, so a naive recursive brute force converges on "everything
//! decodes to something". Four separate limits keep that from happening:
//!
//! 1. **Layers are typed.** A *transport* layer (base64 under an alphabet the binary
//!    demonstrably uses, or hex) is structurally self-checking: the whole run has to
//!    be well-formed under that alphabet, which is strong evidence on its own, so its
//!    output may be recursed into even when it is binary. A *cipher* layer
//!    (XOR/SUB/ADD/pairwise) has no such check, so it is only ever accepted — or
//!    recursed into — when its own output is already a strong recovery. A
//!    *speculative* layer (a systematically permuted alphabet, tried on no evidence
//!    beyond the shape of the input) is accepted on output strength alone and is
//!    never recursed into at all.
//! 2. **The bar rises with depth.** A chain of `n` layers must recover
//!    `n × MIN_RECOVERY_CHARS` characters, so a three-layer chain has to produce three
//!    times as much readable text as a one-layer chain before it is believed.
//! 3. **Chains are ranked, not raced.** Several keys can turn the same buffer into
//!    something printable — XOR in particular maps large parts of ASCII back into
//!    ASCII, so a wrong key routinely yields text-shaped output. Reporting whichever
//!    was tried first would then be arbitrary, so every surviving chain is ranked:
//!    a chain that finished decoding beats one whose output is still an encoding,
//!    then fewer layers win, then the output that scores better against ASCII letter
//!    frequencies (see [`text_score`]) wins.
//! 4. **A hard attempt budget** bounds the total work regardless of shape.
//!
//! Run offsets in a [`Chain`] are relative to the **final** decode, not to the
//! original buffer: a base64 layer is 4:3 and a pairwise layer is 2:1, so offsets do
//! not survive translation back through the chain. Callers anchor every recovered
//! item at the address of the input buffer as a whole.

use std::cmp::Ordering;

use super::{arithmetic, base64, pairwise, recover, xor};

/// The max decode layers applied to one buffer.
pub const MAX_DECODE_DEPTH: usize = 3;

/// The minimum input length worth handing to another layer.
const MIN_LAYER_INPUT: usize = 16;

/// The max input a cipher layer is brute-forced over.
///
/// A cipher layer costs ~640 full-buffer decodes; transport layers cost one each, so
/// only the cipher side needs a size limit.
const MAX_CIPHER_INPUT: usize = 16 * 1024;

/// The minimum fraction of non-printable source bytes before cipher layers are worth
/// attempting.
///
/// [`recover::hidden_runs`] only keeps a run whose *source* bytes were mostly
/// non-printable, so an all-printable buffer (an armored blob, say) can never yield
/// one. Skipping the brute force there is a pure saving, not a coverage loss.
const MIN_CIPHER_NONPRINTABLE: f32 = 0.1;

/// The minimum hex digits before a run is treated as hex-encoded.
const MIN_HEX_DIGITS: usize = 32;

/// The hard cap on decode attempts for one input buffer.
const MAX_CHAIN_ATTEMPTS: usize = 20_000;

/// One decode layer applied during a chained recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer {
    /// The scheme label (e.g. `"base64"`, `"XOR"`).
    pub scheme: String,
    /// The key rendered for display, when the scheme uses one.
    pub key: Option<String>,
}

impl Layer {
    /// Build a layer description.
    ///
    /// # Arguments
    ///
    /// * `scheme` - The scheme label
    /// * `key` - The key rendered for display, if any
    fn new(scheme: impl Into<String>, key: Option<String>) -> Self {
        Self {
            scheme: scheme.into(),
            key,
        }
    }

    /// The layer rendered as `scheme` or `scheme(key)`.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.key {
            Some(key) => format!("{}({key})", self.scheme),
            None => self.scheme.clone(),
        }
    }
}

/// How a layer's output relates to its input, which decides how the output is judged
/// and whether the chain may continue through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// A structurally self-checking re-encoding (base64 under a grounded alphabet,
    /// hex). Recursable without a strong recovery of its own.
    Transport,
    /// A byte cipher whose output is index-aligned with its input. Accepted and
    /// recursed into only on a strong recovery.
    Aligned,
    /// A decode tried on no evidence beyond the input's shape. Accepted only on a
    /// strong recovery and never recursed into.
    Speculative,
}

/// A chained recovery: the layers applied, in order, and what they recovered.
#[derive(Debug, Clone)]
pub struct Chain {
    /// The decode layers, outermost first.
    pub layers: Vec<Layer>,
    /// The recovered `(offset, string)` runs, offsets relative to the final decode.
    pub runs: Vec<(usize, String)>,
    /// The final decode's full output, for callers that need to parse it structurally
    /// rather than read it (the runs drop whatever fell between them).
    pub decoded: Vec<u8>,
    /// Whether the output is *itself* still well-formed under a transport encoding —
    /// i.e. this chain stopped one layer short of the plaintext.
    pub still_encoded: bool,
    /// How closely the recovered text tracks ASCII letter frequencies (higher is
    /// better); the tiebreak between two decodes that both look printable.
    pub score: f32,
}

impl Chain {
    /// The chain rendered as a scheme label, e.g. `"base64 → XOR(0x5a)"`.
    #[must_use]
    pub fn scheme(&self) -> String {
        self.layers
            .iter()
            .map(Layer::describe)
            .collect::<Vec<_>>()
            .join(" → ")
    }

    /// The innermost layer's key, which is the one that actually decrypted the text.
    #[must_use]
    pub fn key(&self) -> Option<&str> {
        self.layers
            .iter()
            .rev()
            .find_map(|layer| layer.key.as_deref())
    }

    /// The total recovered characters across all runs.
    #[must_use]
    pub fn recovered_chars(&self) -> usize {
        self.runs.iter().map(|(_, text)| text.len()).sum()
    }

    /// Whether this chain applied more than one layer.
    #[must_use]
    pub fn is_nested(&self) -> bool {
        self.layers.len() > 1
    }

    /// Whether every layer was structurally self-checking, i.e. no brute-forced key
    /// and no speculative alphabet stood between the stored bytes and the plaintext.
    #[must_use]
    pub fn is_structural(&self) -> bool {
        self.layers.iter().all(|layer| layer.key.is_none())
    }

    /// Whether this chain is a better result than `other`.
    ///
    /// In priority order: a chain that finished decoding beats one whose output is
    /// still an encoding; then fewer layers; then a better letter-frequency score;
    /// then more recovered text.
    ///
    /// # Arguments
    ///
    /// * `other` - The incumbent best chain, if any
    fn beats(&self, other: Option<&Chain>) -> bool {
        let Some(best) = other else {
            return true;
        };
        // finishing the job beats stopping one layer short of it
        match best.still_encoded.cmp(&self.still_encoded) {
            Ordering::Greater => return true,
            Ordering::Less => return false,
            Ordering::Equal => {}
        }
        // the simpler explanation wins
        match self.layers.len().cmp(&best.layers.len()) {
            Ordering::Less => return true,
            Ordering::Greater => return false,
            Ordering::Equal => {}
        }
        // among equally deep chains, the one whose output reads more like real text
        match self
            .score
            .partial_cmp(&best.score)
            .unwrap_or(Ordering::Equal)
        {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => self.recovered_chars() > best.recovered_chars(),
        }
    }
}

/// Relative frequency of each ASCII letter in English-language text, in percent.
///
/// Malware configuration text is not prose, but it is built out of English-derived
/// tokens (`http`, `update`, `sleep`, `retry`, host names, command names), so it
/// tracks these frequencies far more closely than the output of a wrong key does.
const LETTER_FREQ: [f32; 26] = [
    8.167, 1.492, 2.782, 4.253, 12.702, 2.228, 2.015, 6.094, 6.966, 0.153, 0.772, 4.025, 2.406,
    6.749, 7.507, 1.929, 0.095, 5.987, 6.327, 9.056, 2.758, 0.978, 2.360, 0.150, 1.974, 0.074,
];

/// Score how closely `text` tracks ASCII letter frequencies — higher is better.
///
/// The classic chi-squared fit used to pick a single-byte key out of its near misses,
/// negated so that larger means more plausible and normalized by letter count so a
/// long decode is not penalized for being long. Text with no letters at all scores
/// worst: it carries no evidence either way.
///
/// This only ever chooses *between* decodes that have already passed
/// [`recover::is_strong_recovery`]; it never admits one on its own.
///
/// # Arguments
///
/// * `text` - The recovered text to score
#[must_use]
pub fn text_score(text: &str) -> f32 {
    // histogram the letters, case-folded
    let mut counts = [0f32; 26];
    let mut total = 0f32;
    for byte in text.bytes().filter(u8::is_ascii_alphabetic) {
        counts[(byte.to_ascii_lowercase() - b'a') as usize] += 1.0;
        total += 1.0;
    }
    if total == 0.0 {
        return f32::MIN;
    }
    // chi-squared against the expected counts for this many letters
    let chi2: f32 = counts
        .iter()
        .zip(LETTER_FREQ)
        .map(|(observed, freq)| {
            let expected = total * freq / 100.0;
            (observed - expected).powi(2) / expected
        })
        .sum();
    -chi2 / total
}

/// The chained decoder, parameterized by the base64 alphabets in play.
pub struct Engine {
    /// Alphabets with evidence behind them: the two standard ones, plus any table
    /// found stored in the binary. A run that is well-formed under one of these is
    /// treated as genuinely transport-encoded.
    grounded: Vec<base64::Alphabet>,
    /// Systematically permuted alphabets, tried on shape alone.
    speculative: Vec<base64::Alphabet>,
}

impl Engine {
    /// Build an engine over the standard alphabets plus any tables recovered from the
    /// binary (see [`base64::find_alphabets`]).
    ///
    /// # Arguments
    ///
    /// * `discovered` - Alphabet tables found stored in the sample
    #[must_use]
    pub fn new(discovered: Vec<base64::Alphabet>) -> Self {
        // the two RFC alphabets plus anything the binary carries its own table for
        let mut grounded = vec![base64::Alphabet::standard(), base64::Alphabet::url_safe()];
        grounded.extend(discovered);
        // every rotation of the standard alphabet: the cheapest way to make base64
        // unreadable, and a bounded family to sweep
        let standard = base64::Alphabet::standard();
        let speculative = (1..base64::ALPHABET_LEN)
            .map(|n| standard.rotated(n))
            .collect();
        Self {
            grounded,
            speculative,
        }
    }

    /// Recover plaintext from `input` through up to [`MAX_DECODE_DEPTH`] layers.
    ///
    /// Returns the shortest chain that produced a strong recovery, or `None` when no
    /// chain did.
    ///
    /// # Arguments
    ///
    /// * `input` - The stored (encoded) bytes
    #[must_use]
    pub fn recover(&self, input: &[u8]) -> Option<Chain> {
        let mut best = None;
        let mut budget = MAX_CHAIN_ATTEMPTS;
        self.walk(input, &mut Vec::new(), &mut best, &mut budget);
        best
    }

    /// Try every primitive against `input`, recording successes and recursing where
    /// the layer type allows it.
    ///
    /// # Arguments
    ///
    /// * `input` - The bytes at this depth
    /// * `layers` - The layers already applied (outermost first)
    /// * `best` - The best chain found so far
    /// * `budget` - The remaining decode attempts
    fn walk(
        &self,
        input: &[u8],
        layers: &mut Vec<Layer>,
        best: &mut Option<Chain>,
        budget: &mut usize,
    ) {
        // stop at the depth limit, on a buffer too short to carry anything, or when
        // the attempt budget for this input is spent
        if layers.len() >= MAX_DECODE_DEPTH || input.len() < MIN_LAYER_INPUT || *budget == 0 {
            return;
        }

        // --- transport layers: structurally self-checking, so recursable -----------
        for alphabet in &self.grounded {
            // the whole run must be well-formed under the alphabet
            if !base64::fits_alphabet(alphabet, input) {
                continue;
            }
            if let Some(decoded) = base64::decode_with(alphabet, input) {
                let layer = Layer::new(alphabet.label.clone(), None);
                self.consider(
                    layer,
                    &decoded,
                    input,
                    Mode::Transport,
                    layers,
                    best,
                    budget,
                );
            }
        }
        if let Some(decoded) = decode_hex(input) {
            let layer = Layer::new("hex", None);
            self.consider(
                layer,
                &decoded,
                input,
                Mode::Transport,
                layers,
                best,
                budget,
            );
        }

        // --- speculative alphabets: judged only by what they produce ---------------
        for alphabet in &self.speculative {
            if !base64::fits_alphabet(alphabet, input) {
                continue;
            }
            if let Some(decoded) = base64::decode_with(alphabet, input) {
                let layer = Layer::new(alphabet.label.clone(), None);
                self.consider(
                    layer,
                    &decoded,
                    input,
                    Mode::Speculative,
                    layers,
                    best,
                    budget,
                );
            }
        }

        // --- cipher layers: brute-forced, so bounded by size and by source shape ---
        if input.len() > MAX_CIPHER_INPUT
            || recover::src_nonprintable_ratio(input) < MIN_CIPHER_NONPRINTABLE
        {
            return;
        }
        // single-byte XOR, high-bit keys only — a low key maps ASCII back into the
        // printable range, which is indistinguishable from plaintext
        for key in 0x80u8..=0xff {
            let decoded = xor::single_byte(key, input);
            let layer = Layer::new("XOR", Some(format!("0x{key:02x}")));
            self.consider(layer, &decoded, input, Mode::Aligned, layers, best, budget);
        }
        // single-byte SUB/ADD
        for key in 0x01u8..=0xff {
            for (scheme, decoded) in [
                ("SUB", arithmetic::sub_byte(key, input)),
                ("ADD", arithmetic::add_byte(key, input)),
            ] {
                let layer = Layer::new(scheme, Some(format!("0x{key:02x}")));
                self.consider(layer, &decoded, input, Mode::Aligned, layers, best, budget);
            }
        }
        // pairwise SUB has its own run extractor (its output is half-length, so it is
        // not index-aligned) and its own wide-string guard
        if *budget > 0 {
            *budget -= 1;
            let runs = pairwise::hidden_runs(input);
            let decoded = pairwise::decode(input);
            let layer = Layer::new("pairwise SUB", None);
            self.record(
                layer,
                decoded,
                runs,
                Mode::Speculative,
                layers,
                best,
                budget,
            );
        }
    }

    /// Judge one layer's output and, where the layer type allows, continue through it.
    ///
    /// # Arguments
    ///
    /// * `layer` - The layer that produced `decoded`
    /// * `decoded` - The layer's output
    /// * `src` - The layer's input (needed to judge an index-aligned decode)
    /// * `mode` - How the output relates to the input
    /// * `layers` - The layers already applied
    /// * `best` - The best chain found so far
    /// * `budget` - The remaining decode attempts
    #[allow(clippy::too_many_arguments)]
    fn consider(
        &self,
        layer: Layer,
        decoded: &[u8],
        src: &[u8],
        mode: Mode,
        layers: &mut Vec<Layer>,
        best: &mut Option<Chain>,
        budget: &mut usize,
    ) {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        // an aligned decode is judged on whether it *uncovered* text (the source bytes
        // were non-printable); a re-encoding is judged on the output alone, since its
        // input was printable by construction
        let runs = match mode {
            Mode::Aligned => recover::hidden_runs(decoded, src),
            Mode::Transport | Mode::Speculative => recover::printable_runs(decoded),
        };
        self.record(layer, decoded.to_vec(), runs, mode, layers, best, budget);
    }

    /// Record a layer's result: accept it as a chain if strong enough, then recurse if
    /// the layer type permits.
    ///
    /// # Arguments
    ///
    /// * `layer` - The layer that produced these runs
    /// * `decoded` - The layer's output
    /// * `runs` - The runs recovered from the output
    /// * `mode` - How the output relates to the input
    /// * `layers` - The layers already applied
    /// * `best` - The best chain found so far
    /// * `budget` - The remaining decode attempts
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        layer: Layer,
        decoded: Vec<u8>,
        runs: Vec<(usize, String)>,
        mode: Mode,
        layers: &mut Vec<Layer>,
        best: &mut Option<Chain>,
        budget: &mut usize,
    ) {
        // the bar rises with depth: each extra layer must pay for the extra chances it
        // had to produce printable noise
        let depth = layers.len() + 1;
        let strong = recover::is_strong_recovery(&runs, recover::MIN_RECOVERY_CHARS * depth);
        if strong {
            let mut chain_layers = layers.clone();
            chain_layers.push(layer.clone());
            // score the recovered text, and note whether it is still an encoding —
            // a decode that lands on more base64 has not reached the plaintext
            let text: String = runs.iter().map(|(_, run)| run.as_str()).collect();
            let candidate = Chain {
                layers: chain_layers,
                runs,
                decoded: decoded.clone(),
                still_encoded: self.is_transport_encoded(&decoded),
                score: text_score(&text),
            };
            if candidate.beats(best.as_ref()) {
                *best = Some(candidate);
            }
        }
        // a transport layer carries its own structural evidence, so the chain may
        // continue through it even when this level read as binary; a cipher layer may
        // only be continued through once it has proven itself
        let recursable = match mode {
            Mode::Transport => true,
            Mode::Aligned => strong,
            Mode::Speculative => false,
        };
        if recursable {
            layers.push(layer);
            self.walk(&decoded, layers, best, budget);
            layers.pop();
        }
    }

    /// Whether `bytes` are themselves well-formed under a transport encoding, i.e.
    /// this decode landed on another encoded layer rather than on plaintext.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The decode output to test
    fn is_transport_encoded(&self, bytes: &[u8]) -> bool {
        // too short to be a meaningful encoded layer either way
        if bytes.len() < MIN_LAYER_INPUT {
            return false;
        }
        self.grounded
            .iter()
            .any(|alphabet| base64::fits_alphabet(alphabet, bytes))
            || decode_hex(bytes).is_some()
    }
}

/// Decode a run of ASCII hex digits, ignoring whitespace.
///
/// Returns `None` unless the run is entirely hex digits and whitespace and carries at
/// least [`MIN_HEX_DIGITS`] digits in whole byte pairs — a partial match means the run
/// is text that happens to start with hex characters.
///
/// # Arguments
///
/// * `input` - The candidate hex-encoded bytes
fn decode_hex(input: &[u8]) -> Option<Vec<u8>> {
    let mut nibbles = Vec::with_capacity(input.len());
    for &c in input {
        // formatting whitespace between digits is tolerated
        if c.is_ascii_whitespace() {
            continue;
        }
        nibbles.push(match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            // any non-hex byte means this run is not hex-encoded
            _ => return None,
        });
    }
    // whole bytes only, and long enough that the shape is not a coincidence
    if nibbles.len() < MIN_HEX_DIGITS || nibbles.len() % 2 != 0 {
        return None;
    }
    Some(
        nibbles
            .chunks_exact(2)
            .map(|pair| (pair[0] << 4) | pair[1])
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plaintext used across the chain tests: long enough to clear the
    /// depth-scaled recovery bar at every depth.
    const PLAIN: &[u8] =
        b"http://update.example.invalid/gate.php|sleep=300|id=agent-01|retry=5|proxy=none";

    /// A single base64 layer is recovered and reported as one transport layer.
    #[test]
    fn recovers_a_single_transport_layer() {
        let engine = Engine::new(Vec::new());
        let encoded = base64::encode(PLAIN);
        let chain = engine.recover(encoded.as_bytes()).expect("base64 layer");
        assert_eq!(chain.scheme(), "base64");
        assert!(!chain.is_nested());
        assert!(chain.is_structural());
        assert!(chain.runs.iter().any(|(_, text)| text.contains("gate.php")));
    }

    /// A payload XORed and then base64-armored is recovered through both layers, with
    /// the key of the inner layer reported.
    #[test]
    fn recovers_base64_over_xor() {
        let engine = Engine::new(Vec::new());
        let xored = xor::single_byte(0x9c, PLAIN);
        let encoded = base64::encode(&xored);
        let chain = engine.recover(encoded.as_bytes()).expect("two-layer chain");
        assert_eq!(chain.scheme(), "base64 → XOR(0x9c)");
        assert!(chain.is_nested());
        // a brute-forced key means the chain is not purely structural
        assert!(!chain.is_structural());
        assert_eq!(chain.key(), Some("0x9c"));
        assert!(chain.runs.iter().any(|(_, text)| text.contains("agent-01")));
    }

    /// A payload stored under a custom alphabet is only recovered once that
    /// alphabet's table is supplied — which is what finding it in the binary buys.
    #[test]
    fn recovers_under_a_discovered_alphabet() {
        // a permutation that is deliberately *not* a rotation, so the speculative
        // sweep cannot stumble onto it: adjacent symbols swapped
        let standard = base64::Alphabet::standard();
        let mut table = [0u8; base64::ALPHABET_LEN];
        for (idx, slot) in table.iter_mut().enumerate() {
            *slot = standard.table[idx ^ 1];
        }
        let custom = base64::Alphabet::new(table, Some(b'='), "base64 (custom)").unwrap();
        // encode under the custom table by remapping a standard encoding
        let encoded: Vec<u8> = base64::encode(PLAIN)
            .bytes()
            .map(|c| match standard.table.iter().position(|&s| s == c) {
                Some(value) => custom.table[value],
                None => c,
            })
            .collect();
        // without the table the standard alphabets produce nothing believable
        let blind = Engine::new(Vec::new()).recover(&encoded);
        assert!(
            blind
                .as_ref()
                .is_none_or(|chain| !chain.runs.iter().any(|(_, text)| text.contains("gate.php")))
        );
        // with it, the payload comes straight back
        let chain = Engine::new(vec![custom])
            .recover(&encoded)
            .expect("custom alphabet");
        assert_eq!(chain.scheme(), "base64 (custom)");
        assert!(chain.runs.iter().any(|(_, text)| text.contains("gate.php")));
    }

    /// Hex-encoded text is recovered, and a run that merely starts with hex digits is
    /// not mistaken for it.
    #[test]
    fn recovers_hex_and_rejects_near_misses() {
        let engine = Engine::new(Vec::new());
        let encoded: String = PLAIN.iter().map(|b| format!("{b:02x}")).collect();
        let chain = engine.recover(encoded.as_bytes()).expect("hex layer");
        assert_eq!(chain.scheme(), "hex");
        // a non-hex byte anywhere disqualifies the whole run
        assert!(decode_hex(b"deadbeefdeadbeefdeadbeefdeadbeefz").is_none());
        // an odd digit count is not whole bytes
        assert!(decode_hex(b"deadbeefdeadbeefdeadbeefdeadbeefa").is_none());
        // too short for the shape to mean anything
        assert!(decode_hex(b"deadbeef").is_none());
    }

    /// Random bytes and ordinary plaintext yield no chain: the depth-scaled recovery
    /// bar is what stops a deep search from "decoding" everything.
    #[test]
    fn rejects_noise_and_plaintext() {
        let engine = Engine::new(Vec::new());
        // a deterministic pseudo-random buffer (no RNG in the test harness)
        let noise: Vec<u8> = (0u32..512)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        assert!(engine.recover(&noise).is_none());
        // ordinary readable text is not "recovered" — nothing was hidden
        assert!(
            engine
                .recover(b"the quick brown fox jumps over the lazy dog again")
                .is_none()
        );
        // a buffer too short to carry a real payload is not searched at all
        assert!(engine.recover(b"aGVsbG8=").is_none());
    }

    /// Depth is bounded and the shorter of two viable chains wins.
    #[test]
    fn prefers_the_shortest_chain() {
        let engine = Engine::new(Vec::new());
        let encoded = base64::encode(PLAIN);
        let chain = engine.recover(encoded.as_bytes()).expect("base64 layer");
        // the engine can see deeper chains through the same buffer, but reports the
        // one-layer explanation
        assert_eq!(chain.layers.len(), 1);
        // three layers is the ceiling regardless of what the search finds
        let deep = base64::encode(base64::encode(&xor::single_byte(0x88, PLAIN)).as_bytes());
        let chain = engine.recover(deep.as_bytes()).expect("three-layer chain");
        assert_eq!(chain.layers.len(), MAX_DECODE_DEPTH);
        assert_eq!(chain.scheme(), "base64 → base64 → XOR(0x88)");
    }

    /// A layer's display form carries its key when it has one.
    #[test]
    fn layer_describes_itself() {
        assert_eq!(Layer::new("base64", None).describe(), "base64");
        assert_eq!(
            Layer::new("XOR", Some("0x41".into())).describe(),
            "XOR(0x41)"
        );
    }
}
