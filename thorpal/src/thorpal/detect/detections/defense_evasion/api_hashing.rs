//! API hashing / dynamic API resolution (MITRE ATT&CK **T1027.007**).
//!
//! Loaders and shellcode resolve imports by comparing hashes of export names
//! instead of carrying the strings, so the binary shows no IAT entry and no
//! name for what it calls. The hashes cannot be inverted, but they can be
//! *matched*: this detection collects candidate 32-bit hash values (compare
//! operands, hash-argument immediates, immediate tables in data) and replays
//! the curated name dictionary over them via
//! [`crate::thorpal::detect::decode::apihash`].
//!
//! Three passes, built to hold a near-zero false-positive rate:
//!
//! 1. **Constants** — the known hash-algorithm constants (the `ROR 13` idiom,
//!    the CRC-32 polynomial `0xEDB88320` and its standard table, the FNV-1a
//!    offset basis `0x811C9DC5` and prime `0x01000193`, the `djb2` seed 5381).
//!    These are ubiquitous in benign code (every zlib carries a CRC table), so
//!    a bare constant is **never a standalone finding**: it is reported at
//!    `low()` only when it corroborates a resolution finding in the same
//!    function, and dropped silently otherwise.
//! 2. **Resolution** (the self-validating core) — a finding fires only when at
//!    least [`MIN_RESOLVED`] distinct values resolve to real export names
//!    under ONE algorithm. Each family's hash table covers roughly `10^5` of
//!    the `2^32` possible values, so a random immediate matches at about
//!    `1e-5` and three matches in one function under one family is
//!    practically impossible by chance — the resolved names validate
//!    themselves, and an import-less binary gains named capabilities before
//!    the agents' first tool call.
//! 3. **Idioms** — a PEB loader-list walk or an export-directory walk in the
//!    same function is named in the reasoning as corroboration; its absence
//!    never blocks a finding that cleared the resolution bar.
//!
//! Findings group per resolver function (one finding naming every resolved
//! import, not one per name), cap at [`MAX_RESOLVER_FINDINGS`] per program,
//! and overflow into a single summary hit. The parameterized `mul_rolling`
//! family (unknown seed/multiplier) and the weak `byte_sum` family (its
//! hashes crowd a tiny range, so small immediates collide constantly) are
//! deliberately not replayed here.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use anyhow::Result;
use ghidra_cli::models::Instruction;

use crate::thorpal::detect::context::{AnalysisContext, SectionBytes, fmt_addr};
use crate::thorpal::detect::decode::apihash::{HashFamily, HashMatch, NameVariants, match_hashes};
use crate::thorpal::detect::decode::buffers::referencing_function;
use crate::thorpal::detect::detections::{
    ArtifactClass, AttackTechnique, Detection, DetectionHit, EvidenceKind, HitArtifact, high, low,
    medium, parse_imm,
};

use super::T1027_007;

/// The hash families replayed against candidate values.
///
/// `byte_sum` is excluded (its hashes crowd a tiny range, so ordinary small
/// immediates collide constantly) and `mul_rolling` is excluded (it is
/// parameterized by a seed/multiplier this static pass does not know).
const STRONG_FAMILIES: [HashFamily; 5] = [
    HashFamily::Fnv1a,
    HashFamily::Ror13,
    HashFamily::Crc32,
    HashFamily::Djb2Add,
    HashFamily::Djb2Xor,
];

/// The distinct values that must resolve under ONE family before a finding fires.
///
/// One family's table holds roughly `10^5` hashes out of `2^32`, so a random
/// value matches at about `1e-5`; requiring three distinct matches in one
/// function under one family pushes the chance of a coincidence below any
/// realistic program's noise floor.
const MIN_RESOLVED: usize = 3;

/// The max byte gap between resolving data words that still cluster as one table.
///
/// Generous enough for `{hash, ordinal}` pairs and 16-byte stride structs, small
/// enough that stray matches scattered through a section never aggregate.
const TABLE_GAP: u64 = 64;

/// The max distinct data words fed to the matcher (bounds memory on huge samples).
const MAX_DATA_WORDS: usize = 2_000_000;

/// The max detailed resolver findings per program; the rest roll into one summary.
const MAX_RESOLVER_FINDINGS: usize = 4;

/// The max resolved names listed in a finding's content line.
const MAX_NAMED: usize = 16;

/// The max resolved names carried in an artifact value.
const MAX_ARTIFACT_NAMES: usize = 40;

/// The max `hash=name` pairs listed in an artifact's detail.
const MAX_DETAIL_PAIRS: usize = 24;

/// The max evidence addresses listed in a content line.
const MAX_SITES: usize = 12;

/// The window for attributing a data table to the function referencing it.
///
/// Matches the data-reference window the shared detection helpers use, so data
/// attribution stays consistent across the tool.
const REF_WINDOW: u64 = 4096;

/// Hash-algorithm seed/polynomial values excluded from candidate targets.
///
/// Every FNV resolver loads `0x811C9DC5` and every bit-loop CRC carries
/// `0xEDB88320`; letting those count as hash targets would systematically
/// self-corroborate, so they are only ever constants (pass 1), never targets.
const ALGORITHM_SEEDS: [u32; 3] = [0xEDB8_8320, 0x811C_9DC5, 0x0100_0193];

/// The little-endian bytes of the reflected CRC-32 table entries 1-3
/// (`0x77073096`, `0xEE0E612C`, `0x990951BA`): the standard table's fingerprint.
const CRC32_TABLE_SIG: [u8; 12] = [
    0x96, 0x30, 0x07, 0x77, 0x2c, 0x61, 0x0e, 0xee, 0xba, 0x51, 0x09, 0x99,
];

/// The PEB-walk corroboration phrase (slotted after "The same function also ").
const IDIOM_PEB: &str = "walks the PEB loader module list via FS:[0x30]/GS:[0x60] — the \
                         module-base half of a hash resolver";

/// The export-directory-walk corroboration phrase (same slot as [`IDIOM_PEB`]).
const IDIOM_EXPORT: &str = "parses a PE export directory in memory (`e_lfanew` at +0x3c plus \
                            the export-directory offset) — the export-enumeration half of a \
                            hash resolver";

/// The API-hashing detection (ATT&CK T1027.007).
pub struct ApiHashing;

/// The resolved names (and the matched variant of each) for one hash value.
type ResolvedNames = Vec<(String, String)>;

/// The matcher output indexed family → hash → the names it resolves to.
type NameIndex = HashMap<&'static str, BTreeMap<u32, ResolvedNames>>;

/// Qualifying table clusters attributed to a function, keyed `(entry, family)`.
type AttributedClusters = BTreeMap<(u64, &'static str), Vec<TableCluster>>;

/// A hash-algorithm constant recognized by the constants pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ConstKind {
    /// The IEEE CRC-32 polynomial `0xEDB88320` as an instruction immediate.
    Crc32Poly,
    /// The standard reflected CRC-32 lookup table found in a data section.
    Crc32Table,
    /// The FNV-1a 32-bit offset basis `0x811C9DC5`.
    FnvOffset,
    /// The FNV-1a 32-bit prime `0x01000193`.
    FnvPrime,
    /// The `djb2` seed 5381 loaded into a register.
    Djb2Seed,
    /// A `ROR reg, 13` rotate — the classic Metasploit-style hash loop step.
    Ror13Idiom,
}

impl ConstKind {
    /// The short display label used in content and corroboration notes.
    fn label(self) -> &'static str {
        // one fixed label per kind, reused by content and reasoning alike
        match self {
            ConstKind::Crc32Poly => "IEEE CRC-32 polynomial 0xedb88320",
            ConstKind::Crc32Table => "standard CRC-32 lookup table",
            ConstKind::FnvOffset => "FNV-1a offset basis 0x811c9dc5",
            ConstKind::FnvPrime => "FNV-1a prime 0x01000193",
            ConstKind::Djb2Seed => "djb2 seed 5381",
            ConstKind::Ror13Idiom => "ROR-13 rotate idiom",
        }
    }

    /// The matcher family labels this constant seeds (and so corroborates).
    fn families(self) -> &'static [&'static str] {
        // a constant only ever corroborates the algorithm it parameterizes
        match self {
            ConstKind::Crc32Poly | ConstKind::Crc32Table => &["crc32"],
            ConstKind::FnvOffset | ConstKind::FnvPrime => &["fnv1a"],
            ConstKind::Djb2Seed => &["djb2_add", "djb2_xor"],
            ConstKind::Ror13Idiom => &["ror13"],
        }
    }

    /// The reasoning lead-in: what the constant is and why it proves nothing alone.
    fn blurb(self) -> &'static str {
        // every blurb names the ubiquity out loud, per the near-zero-FP contract
        match self {
            ConstKind::Crc32Poly => {
                "The IEEE CRC-32 polynomial seeds the crc32 hash family, but CRC-32 \
                 polynomials and tables are ubiquitous in benign code (checksums, zlib, PNG)."
            }
            ConstKind::Crc32Table => {
                "The standard reflected CRC-32 lookup table drives table-based crc32 \
                 hashing, but CRC-32 tables are ubiquitous in benign code (checksums, \
                 zlib, PNG)."
            }
            ConstKind::FnvOffset => {
                "The FNV-1a 32-bit offset basis seeds the fnv1a hash family, but the FNV \
                 constants appear in countless benign hash maps."
            }
            ConstKind::FnvPrime => {
                "The FNV-1a 32-bit prime drives the fnv1a multiply step, but the FNV \
                 constants appear in countless benign hash maps."
            }
            ConstKind::Djb2Seed => {
                "5381 seeds the djb2 string-hash family, but the same seed appears in \
                 countless benign string-hash implementations."
            }
            ConstKind::Ror13Idiom => {
                "A rotate-right-by-13 accumulate step is the classic Metasploit-style \
                 export-name hash loop, but rotate-and-add also occurs in benign hashing."
            }
        }
    }
}

/// Everything the instruction pass learns about one function.
#[derive(Debug, Default)]
struct FunctionFacts {
    /// Candidate hash values: value → the sites observing it.
    immediates: BTreeMap<u32, Vec<u64>>,
    /// Hash-algorithm constants: kind → the sites carrying it.
    constants: BTreeMap<ConstKind, Vec<u64>>,
    /// Resolver idioms seen in the body (full corroboration phrases).
    idioms: Vec<&'static str>,
}

impl FunctionFacts {
    /// Whether the pass learned nothing about the function.
    fn is_empty(&self) -> bool {
        // a function is kept only when at least one fact list is populated
        self.immediates.is_empty() && self.constants.is_empty() && self.idioms.is_empty()
    }
}

/// One contiguous run of resolving table words in a data section.
#[derive(Debug, Clone)]
struct TableCluster {
    /// The matched words in the run: `(address, value)`, ascending by address.
    entries: Vec<(u64, u32)>,
}

impl TableCluster {
    /// The number of distinct values in the run (its resolution count).
    fn distinct_values(&self) -> usize {
        // runs are short, so a throwaway set is the simplest dedupe
        self.entries
            .iter()
            .map(|(_, value)| *value)
            .collect::<HashSet<u32>>()
            .len()
    }
}

/// Per-family evidence collected for one resolver site (pre-finding form).
struct FamilyEvidence {
    /// The family label the values resolved under.
    family: &'static str,
    /// Resolved hash → the `(name, variant)` pairs it maps to.
    resolved: BTreeMap<u32, ResolvedNames>,
    /// The code sites of the resolving immediates.
    insn_sites: Vec<u64>,
    /// The data addresses of the resolving table words.
    table_addrs: Vec<u64>,
}

/// One qualifying resolver, ready to render as a finding.
struct ResolverFinding {
    /// The resolver function's entry, when attributed (`None` = a bare table).
    function: Option<u64>,
    /// The anchor address: the first resolving code site, else the table start.
    anchor: u64,
    /// The winning family label (most resolutions).
    family: &'static str,
    /// Other families that also cleared the bar here: `(label, count)`.
    extra_families: Vec<(&'static str, usize)>,
    /// Resolved hash → the `(name, variant)` pairs it maps to.
    resolved: BTreeMap<u32, ResolvedNames>,
    /// Every evidence address (immediate sites plus table words), ascending.
    sites: Vec<u64>,
    /// Whether instruction immediates contributed resolutions.
    via_insns: bool,
    /// Whether an immediate table contributed resolutions.
    via_table: bool,
    /// Corroborating resolver idioms seen in the function body.
    idioms: Vec<&'static str>,
    /// Matching-family constant labels seen in the function body.
    constants: Vec<&'static str>,
}

/// Keep a candidate value only when it could plausibly be an API-name hash.
///
/// The floor removes small integers, flags, and classic 32-bit image addresses
/// (`0x004xxxxx`); repeated-byte fillers and the algorithm seeds are refused
/// outright. The filter costs under half a percent of true hashes per name
/// (those with a zero top byte), which the [`MIN_RESOLVED`] bar absorbs.
///
/// # Arguments
///
/// * `imm` - The raw immediate (or data word) to judge
fn plausible_hash(imm: u64) -> Option<u32> {
    // a hash target is a 32-bit value; wider immediates are pointers/qwords
    let value = u32::try_from(imm).ok()?;
    // the floor removes small integers and 32-bit image addresses; -1 is filler
    if value <= 0x00FF_FFFF || value == u32::MAX {
        return None;
    }
    // repeated-byte fillers (0x01010101, 0xcccccccc, ...) are padding, not hashes
    let bytes = value.to_le_bytes();
    if bytes.iter().all(|byte| *byte == bytes[0]) {
        return None;
    }
    // the seeds would self-corroborate (every FNV resolver loads its basis)
    if ALGORITHM_SEEDS.contains(&value) {
        return None;
    }
    Some(value)
}

/// Collect the candidate hash immediates a function's body carries.
///
/// Only data-carrying mnemonics are read: a resolver compares a computed hash
/// against an immediate (`CMP`/`SUB`), loads one into a register (`MOV`), or
/// passes one as an argument (`PUSH`/`MOV`). `CALL`/jump immediates are code
/// addresses, never hash values, so those mnemonics are skipped entirely.
///
/// # Arguments
///
/// * `insns` - The function's decoded instructions
fn insn_candidates(insns: &[Instruction]) -> Vec<(u64, u32)> {
    // collect (site, value) pairs for immediates that could be API-name hashes
    let mut out = Vec::new();
    for insn in insns {
        // only data-carrying mnemonics: call/jump immediates are code addresses
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        if !matches!(mnemonic.as_str(), "CMP" | "SUB" | "MOV" | "PUSH") {
            continue;
        }
        // the site this immediate is observed at; skip unparseable addresses
        let Some(site) = crate::thorpal::detect::context::parse_addr(&insn.address) else {
            continue;
        };
        // scan every comma-separated operand token for a plausible hash value
        let ops = crate::thorpal::detect::op_str(insn);
        for token in ops.split(',') {
            if let Some(value) = parse_imm(token).and_then(plausible_hash) {
                out.push((site, value));
            }
        }
    }
    out
}

/// Collect the hash-algorithm constants a function's body carries.
///
/// # Arguments
///
/// * `insns` - The function's decoded instructions
fn constant_sites(insns: &[Instruction]) -> Vec<(ConstKind, u64)> {
    // collect (kind, site) pairs for the known seeds, polynomials, and rotates
    let mut out = Vec::new();
    for insn in insns {
        // the mnemonic routes the rotate idiom and gates the djb2 seed
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        // the site this constant is observed at; skip unparseable addresses
        let Some(site) = crate::thorpal::detect::context::parse_addr(&insn.address) else {
            continue;
        };
        let ops = crate::thorpal::detect::op_str(insn);
        // the ROR-13 idiom is a mnemonic+rotate-count shape, not a bare value
        if mnemonic == "ROR" && ops.split(',').next_back().and_then(parse_imm) == Some(13) {
            out.push((ConstKind::Ror13Idiom, site));
            continue;
        }
        // match each immediate token against the known seeds and polynomials
        for token in ops.split(',') {
            match parse_imm(token) {
                Some(0xEDB8_8320) => out.push((ConstKind::Crc32Poly, site)),
                Some(0x811C_9DC5) => out.push((ConstKind::FnvOffset, site)),
                Some(0x0100_0193) => out.push((ConstKind::FnvPrime, site)),
                // 5381 is small enough to appear incidentally, so it only
                // counts where a hash seed is actually loaded
                Some(5381) if mnemonic == "MOV" => out.push((ConstKind::Djb2Seed, site)),
                _ => {}
            }
        }
    }
    out
}

/// Recognize the resolver idioms (pass 3 corroboration) in a function body.
///
/// # Arguments
///
/// * `insns` - The function's decoded instructions
fn resolver_idioms(insns: &[Instruction]) -> Vec<&'static str> {
    // each idiom needs its structural parts seen anywhere in the body
    let mut peb = false;
    let mut lfanew = false;
    let mut export_dir = false;
    for insn in insns {
        // lower the operand text once per instruction for substring tests
        let ops = crate::thorpal::detect::op_str(insn).to_ascii_lowercase();
        // the PEB lives at a fixed TEB offset reachable only via FS/GS
        if ops.contains("fs:[0x30") || ops.contains("gs:[0x60") {
            peb = true;
        }
        // e_lfanew at +0x3c locates the PE header inside a mapped module
        if ops.contains("0x3c]") {
            lfanew = true;
        }
        // the export-directory entry sits at +0x78 (PE32) / +0x88 (PE32+)
        if ops.contains("0x78]") || ops.contains("0x88]") {
            export_dir = true;
        }
    }
    // emit the phrase for each idiom whose parts were all observed
    let mut idioms = Vec::new();
    if peb {
        idioms.push(IDIOM_PEB);
    }
    if lfanew && export_dir {
        idioms.push(IDIOM_EXPORT);
    }
    idioms
}

/// Run the three per-function instruction scans over one body.
///
/// # Arguments
///
/// * `insns` - The function's decoded instructions
fn function_facts(insns: &[Instruction]) -> FunctionFacts {
    // the candidate hash immediates, grouped by value
    let mut facts = FunctionFacts::default();
    for (site, value) in insn_candidates(insns) {
        facts.immediates.entry(value).or_default().push(site);
    }
    // the hash-algorithm constants, grouped by kind
    for (kind, site) in constant_sites(insns) {
        facts.constants.entry(kind).or_default().push(site);
    }
    // the resolver idioms, as ready-to-use corroboration phrases
    facts.idioms = resolver_idioms(insns);
    facts
}

/// Find the first occurrence of `needle` in `haystack`, if any.
///
/// # Arguments
///
/// * `haystack` - The section bytes to search
/// * `needle` - The byte signature to find
fn find_sig(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    // an empty or oversized needle can never match
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    // first occurrence only: one table is all the corroboration needs
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Pool every distinct candidate value: function immediates plus data words.
///
/// # Arguments
///
/// * `facts` - The per-function instruction facts
/// * `data_sections` - The non-executable initialized sections to scan
fn collect_targets(
    facts: &BTreeMap<u64, FunctionFacts>,
    data_sections: &[SectionBytes],
) -> Vec<u32> {
    // start from every candidate immediate any function carries
    let mut targets: HashSet<u32> = facts
        .values()
        .flat_map(|f| f.immediates.keys().copied())
        .collect();
    // add every plausible aligned word from the data sections, bounded
    'sections: for (_, _, bytes) in data_sections {
        let mut off = 0usize;
        while off + 4 <= bytes.len() {
            // read the aligned little-endian word at this offset
            let value =
                u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
            if plausible_hash(u64::from(value)).is_some() {
                targets.insert(value);
                // bound the matcher's working set on pathological samples
                if targets.len() >= MAX_DATA_WORDS {
                    break 'sections;
                }
            }
            off += 4;
        }
    }
    targets.into_iter().collect()
}

/// Index the matcher's output: family → hash → the names it resolves to.
///
/// # Arguments
///
/// * `matches` - The raw matches from the dictionary walk
fn family_index(matches: &[HashMatch]) -> NameIndex {
    // one name list per (family, hash); duplicate names are dropped
    let mut index: NameIndex = HashMap::new();
    for hit in matches {
        let names = index
            .entry(hit.family)
            .or_default()
            .entry(hit.hash)
            .or_default();
        if !names.iter().any(|(name, _)| *name == hit.name) {
            names.push((hit.name.clone(), hit.variant.clone()));
        }
    }
    index
}

/// Locate the resolving aligned words in one section's bytes.
///
/// # Arguments
///
/// * `base` - The section's base address
/// * `bytes` - The section's bytes
/// * `resolving` - Every value the dictionary walk resolved
fn matched_words(base: u64, bytes: &[u8], resolving: &HashSet<u32>) -> Vec<(u64, u32)> {
    // walk the section as aligned little-endian words, keeping resolving ones
    let mut words = Vec::new();
    let mut off = 0usize;
    while off + 4 <= bytes.len() {
        let value =
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        if resolving.contains(&value) {
            words.push((base.saturating_add(off as u64), value));
        }
        off += 4;
    }
    words
}

/// Group resolving words into contiguous clusters (candidate hash tables).
///
/// # Arguments
///
/// * `words` - The resolving `(address, value)` words of one section
/// * `gap` - The max byte gap between neighbours in one cluster
fn cluster_words(words: &[(u64, u32)], gap: u64) -> Vec<TableCluster> {
    // sort by address so adjacency reduces to a neighbour test
    let mut sorted: Vec<(u64, u32)> = words.to_vec();
    sorted.sort_unstable_by_key(|(addr, _)| *addr);
    let mut clusters: Vec<TableCluster> = Vec::new();
    for (addr, value) in sorted {
        // extend the open cluster while the gap to its last word stays small
        match clusters.last_mut() {
            Some(cluster)
                if cluster
                    .entries
                    .last()
                    .is_some_and(|(last, _)| addr.saturating_sub(*last) <= gap) =>
            {
                cluster.entries.push((addr, value));
            }
            _ => clusters.push(TableCluster {
                entries: vec![(addr, value)],
            }),
        }
    }
    clusters
}

/// Find and attribute every qualifying table cluster in the data sections.
///
/// Returns the clusters attributed to a referencing function, keyed by
/// `(function, family)`, plus the unattributed ones.
///
/// # Arguments
///
/// * `data_sections` - The non-executable initialized sections
/// * `resolving` - Every value the dictionary walk resolved
/// * `index` - The family → hash → names index
/// * `family_order` - The family labels in fixed report order
/// * `ref_index` - The reverse data-reference index for attribution
fn find_table_clusters(
    data_sections: &[SectionBytes],
    resolving: &HashSet<u32>,
    index: &NameIndex,
    family_order: &[&'static str],
    ref_index: &[(u64, u64)],
) -> (AttributedClusters, Vec<(&'static str, TableCluster)>) {
    // split the qualifying clusters into attributed and free-standing ones
    let mut attributed: AttributedClusters = BTreeMap::new();
    let mut free: Vec<(&'static str, TableCluster)> = Vec::new();
    for (_, base, bytes) in data_sections {
        // every resolving word in this section, with its address
        let words = matched_words(*base, bytes, resolving);
        if words.is_empty() {
            continue;
        }
        for family in family_order {
            // this family's resolving words, clustered into contiguous runs
            let Some(fam_map) = index.get(family) else {
                continue;
            };
            let fam_words: Vec<(u64, u32)> = words
                .iter()
                .copied()
                .filter(|(_, value)| fam_map.contains_key(value))
                .collect();
            for cluster in cluster_words(&fam_words, TABLE_GAP) {
                // only a run of three distinct resolutions reads as a hash table
                if cluster.distinct_values() < MIN_RESOLVED {
                    continue;
                }
                // attribute via the first entry a function's data references cover
                match cluster
                    .entries
                    .iter()
                    .find_map(|(addr, _)| referencing_function(ref_index, *addr, REF_WINDOW))
                {
                    Some(entry) => attributed.entry((entry, family)).or_default().push(cluster),
                    None => free.push((family, cluster)),
                }
            }
        }
    }
    (attributed, free)
}

/// Collect one function's evidence under one family, if it qualifies.
///
/// Qualification is per-algorithm: at least [`MIN_RESOLVED`] distinct resolving
/// immediates, or an attributed table cluster (which cleared the same bar at
/// build time). A qualifying function's below-bar immediates still ride along
/// in the finding — they resolved under the same family in the same function.
///
/// # Arguments
///
/// * `entry` - The function's entry address
/// * `family` - The family label to judge
/// * `facts` - The per-function instruction facts
/// * `fn_clusters` - The attributed table clusters, keyed by `(function, family)`
/// * `index` - The family → hash → names index
fn family_evidence(
    entry: u64,
    family: &'static str,
    facts: &BTreeMap<u64, FunctionFacts>,
    fn_clusters: &AttributedClusters,
    index: &NameIndex,
) -> Option<FamilyEvidence> {
    // the family's resolution table; absent means nothing resolved under it
    let fam_map = index.get(family)?;
    // the function's immediates that resolve under this family
    let mut resolved: BTreeMap<u32, ResolvedNames> = BTreeMap::new();
    let mut insn_sites: Vec<u64> = Vec::new();
    if let Some(f) = facts.get(&entry) {
        for (value, sites) in &f.immediates {
            if let Some(names) = fam_map.get(value) {
                resolved.insert(*value, names.clone());
                insn_sites.extend(sites.iter().copied());
            }
        }
    }
    // immediates qualify on their own only at the per-function threshold
    let insn_bar = resolved.len() >= MIN_RESOLVED;
    // attributed table clusters already cleared the threshold at build time
    let mut table_addrs: Vec<u64> = Vec::new();
    if let Some(clusters) = fn_clusters.get(&(entry, family)) {
        for cluster in clusters {
            for (addr, value) in &cluster.entries {
                if let Some(names) = fam_map.get(value) {
                    resolved.insert(*value, names.clone());
                    table_addrs.push(*addr);
                }
            }
        }
    }
    // qualify via either source: three immediates, or a qualifying table
    (insn_bar || !table_addrs.is_empty()).then_some(FamilyEvidence {
        family,
        resolved,
        insn_sites,
        table_addrs,
    })
}

/// Build the evidence record for one free-standing (unattributed) table.
///
/// # Arguments
///
/// * `family` - The family the cluster resolved under
/// * `cluster` - The qualifying cluster
/// * `index` - The family → hash → names index
fn cluster_evidence(
    family: &'static str,
    cluster: &TableCluster,
    index: &NameIndex,
) -> FamilyEvidence {
    // map every table word back to the names it resolves to
    let mut resolved: BTreeMap<u32, ResolvedNames> = BTreeMap::new();
    let mut table_addrs: Vec<u64> = Vec::new();
    if let Some(fam_map) = index.get(family) {
        for (addr, value) in &cluster.entries {
            if let Some(names) = fam_map.get(value) {
                resolved.insert(*value, names.clone());
                table_addrs.push(*addr);
            }
        }
    }
    FamilyEvidence {
        family,
        resolved,
        insn_sites: Vec::new(),
        table_addrs,
    }
}

/// The matching-family constant labels a resolver function carries.
///
/// # Arguments
///
/// * `facts` - The function's instruction facts
/// * `family` - The winning family label
fn matching_constants(facts: &FunctionFacts, family: &'static str) -> Vec<&'static str> {
    // keep each corroborating kind's label once, in kind order
    facts
        .constants
        .keys()
        .filter(|kind| kind.families().contains(&family))
        .map(|kind| kind.label())
        .collect()
}

/// Turn one site's best family evidence into a renderable finding.
///
/// # Arguments
///
/// * `function` - The resolver function's entry, when attributed
/// * `evidence` - The winning family's evidence
/// * `extra_families` - Other families that also cleared the bar: `(label, count)`
/// * `facts` - The function's instruction facts, when it has any
fn finding_from_evidence(
    function: Option<u64>,
    evidence: FamilyEvidence,
    extra_families: Vec<(&'static str, usize)>,
    facts: Option<&FunctionFacts>,
) -> ResolverFinding {
    // order and dedupe both site lists for stable rendering
    let mut insn_sites = evidence.insn_sites;
    insn_sites.sort_unstable();
    insn_sites.dedup();
    let mut table_addrs = evidence.table_addrs;
    table_addrs.sort_unstable();
    table_addrs.dedup();
    // anchor on code when any immediate resolved, else on the table itself
    let anchor = insn_sites
        .first()
        .copied()
        .or_else(|| table_addrs.first().copied())
        .unwrap_or_default();
    // the via flags drive the content wording
    let via_insns = !insn_sites.is_empty();
    let via_table = !table_addrs.is_empty();
    // the full evidence site list the validator and markup pass walk
    let mut sites: Vec<u64> = insn_sites.into_iter().chain(table_addrs).collect();
    sites.sort_unstable();
    sites.dedup();
    // name the matching-family constants and idioms seen in the function
    let constants = facts.map_or_else(Vec::new, |f| matching_constants(f, evidence.family));
    let idioms = facts.map_or_else(Vec::new, |f| f.idioms.clone());
    ResolverFinding {
        function,
        anchor,
        family: evidence.family,
        extra_families,
        resolved: evidence.resolved,
        sites,
        via_insns,
        via_table,
        idioms,
        constants,
    }
}

/// Assemble every qualifying resolver finding across the program.
///
/// # Arguments
///
/// * `facts` - The per-function instruction facts
/// * `fn_clusters` - The attributed table clusters, keyed by `(function, family)`
/// * `free_clusters` - The qualifying clusters no function references
/// * `index` - The family → hash → names index
/// * `family_order` - The family labels in fixed report order
fn assemble_findings(
    facts: &BTreeMap<u64, FunctionFacts>,
    fn_clusters: &AttributedClusters,
    free_clusters: Vec<(&'static str, TableCluster)>,
    index: &NameIndex,
    family_order: &[&'static str],
) -> Vec<ResolverFinding> {
    // every function with candidate immediates or an attributed table
    let mut involved: BTreeSet<u64> = facts.keys().copied().collect();
    involved.extend(fn_clusters.keys().map(|(entry, _)| *entry));
    let mut findings = Vec::new();
    for entry in involved {
        // judge each family independently: the threshold is per-algorithm
        let mut qualifying: Vec<FamilyEvidence> = Vec::new();
        for family in family_order {
            if let Some(evidence) = family_evidence(entry, family, facts, fn_clusters, index) {
                qualifying.push(evidence);
            }
        }
        // the family resolving the most values headlines the finding
        qualifying.sort_by_key(|evidence| std::cmp::Reverse(evidence.resolved.len()));
        let mut rest = qualifying.into_iter();
        let Some(best) = rest.next() else {
            continue;
        };
        // a second qualifying family is rare enough to be worth naming
        let extra_families: Vec<(&'static str, usize)> =
            rest.map(|e| (e.family, e.resolved.len())).collect();
        findings.push(finding_from_evidence(
            Some(entry),
            best,
            extra_families,
            facts.get(&entry),
        ));
    }
    // an unattributed table stands alone, anchored at its own words
    for (family, cluster) in free_clusters {
        let evidence = cluster_evidence(family, &cluster, index);
        findings.push(finding_from_evidence(None, evidence, Vec::new(), None));
    }
    findings
}

/// Order findings strongest-first and split them at the per-program cap.
///
/// # Arguments
///
/// * `findings` - Every qualifying resolver finding
fn cap_findings(
    mut findings: Vec<ResolverFinding>,
) -> (Vec<ResolverFinding>, Vec<ResolverFinding>) {
    // strongest first: most resolutions, then the lowest anchor for stability
    findings.sort_by(|a, b| {
        b.resolved
            .len()
            .cmp(&a.resolved.len())
            .then(a.anchor.cmp(&b.anchor))
    });
    // everything past the cap rolls into the summary hit
    let overflow = findings.split_off(MAX_RESOLVER_FINDINGS.min(findings.len()));
    (findings, overflow)
}

/// Render up to `cap` list items, counting the remainder out loud.
///
/// # Arguments
///
/// * `items` - The rendered items
/// * `cap` - The max items to list
fn cap_list(items: &[String], cap: usize) -> String {
    // list the head of the list, then say how many items were left out
    let shown = items
        .iter()
        .take(cap)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    match items.len().saturating_sub(cap) {
        0 => shown,
        extra => format!("{shown} (+{extra} more)"),
    }
}

/// Render up to `cap` addresses, counting the remainder out loud.
///
/// # Arguments
///
/// * `addrs` - The addresses, ascending
/// * `cap` - The max addresses to list
fn list_addrs(addrs: &[u64], cap: usize) -> String {
    // render each address the way the rest of the report does
    let rendered: Vec<String> = addrs.iter().map(|addr| fmt_addr(*addr)).collect();
    cap_list(&rendered, cap)
}

/// One display token per resolved hash: its dictionary names slash-joined.
///
/// # Arguments
///
/// * `resolved` - The resolved hash → names map
fn display_names(resolved: &BTreeMap<u32, ResolvedNames>) -> Vec<String> {
    // a hash resolving to several dictionary names shows them all, joined
    resolved
        .values()
        .map(|entries| {
            entries
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect()
}

/// The machine-pivotable `hash=name [variant]` map for an artifact detail.
///
/// # Arguments
///
/// * `resolved` - The resolved hash → names map
/// * `cap` - The max pairs to list
fn detail_pairs(resolved: &BTreeMap<u32, ResolvedNames>, cap: usize) -> String {
    // one pair per resolved value, naming the matched variant for each name
    let pairs: Vec<String> = resolved
        .iter()
        .take(cap)
        .map(|(hash, entries)| {
            let names = entries
                .iter()
                .map(|(name, variant)| format!("{name} [{variant}]"))
                .collect::<Vec<_>>()
                .join("/");
            format!("0x{hash:08x}={names}")
        })
        .collect();
    // count what the cap left out so nothing is silently dropped
    let shown = pairs.join("; ");
    match resolved.len().saturating_sub(cap) {
        0 => format!("hash-to-name map: {shown}"),
        extra => format!("hash-to-name map: {shown} (+{extra} more)"),
    }
}

/// Describe a function for prose: its Ghidra name (when known) plus address.
///
/// # Arguments
///
/// * `entry` - The function's entry address
/// * `fn_name` - Resolves an entry to the function's Ghidra name
fn describe_function<F>(entry: u64, fn_name: &F) -> String
where
    F: Fn(u64) -> Option<String>,
{
    // show the Ghidra name when one exists, always with the address
    match fn_name(entry) {
        Some(name) => format!("{name} ({})", fmt_addr(entry)),
        None => fmt_addr(entry),
    }
}

/// Build the high-severity hit for one resolver finding.
///
/// # Arguments
///
/// * `finding` - The qualifying resolver
/// * `fn_name` - Resolves an entry to the function's Ghidra name
fn resolver_hit<F>(finding: &ResolverFinding, fn_name: &F) -> DetectionHit
where
    F: Fn(u64) -> Option<String>,
{
    // the headline numbers: distinct resolutions and their display names
    let count = finding.resolved.len();
    let names = display_names(&finding.resolved);
    // the evidence-source phrasing for the content line
    let source = match (finding.via_insns, finding.via_table) {
        (true, true) => "hash immediates and immediate-table entries",
        (true, false) => "hash immediates",
        _ => "immediate-table entries",
    };
    let content = format!(
        "API hashing ({}): {count} {source} statically resolve to export names — {} @ {}",
        finding.family,
        cap_list(&names, MAX_NAMED),
        list_addrs(&finding.sites, MAX_SITES),
    );
    // where the evidence sits, for the opening claim
    let location = match finding.function {
        Some(entry) => format!("function {}", describe_function(entry, fn_name)),
        None => "an unreferenced data table".to_string(),
    };
    // the self-validation argument, written for an agent reader
    let mut reasoning = format!(
        "{count} distinct 32-bit values in {location} all hash to real Windows export/module \
         names under one algorithm ({}); random constants cannot clear that bar, so these are \
         precomputed API-name hashes — dynamic API resolution. The resolved names are the \
         effective import table this code gains with no IAT entry for an analyst or sandbox \
         to see: treat them as named capabilities of an otherwise import-less binary.",
        finding.family,
    );
    // corroborating resolver idioms observed in the same function, named
    for idiom in &finding.idioms {
        let _ = write!(reasoning, " The same function also {idiom}.");
    }
    // matching-family hash constants observed alongside, corroborating the call
    if !finding.constants.is_empty() {
        let _ = write!(
            reasoning,
            " It also carries the {} — corroborating the {} identification.",
            finding.constants.join(" and "),
            finding.family,
        );
    }
    // a second family independently clearing the bar is worth naming
    for (other, other_count) in &finding.extra_families {
        let _ = write!(
            reasoning,
            " A further {other_count} values at the same site resolve under {other} as well."
        );
    }
    // the one next action an agent should take from here
    match finding.function {
        Some(_) => reasoning.push_str(
            " Next action: cross-reference this resolver's callers to find the dynamic-import \
             consumers and tie each resolved name to the call site that uses it.",
        ),
        None => reasoning.push_str(
            " No code reference to the table was located in this analysis (common for \
             position-independent shellcode blobs). Next action: locate the code consuming \
             this table — search for instructions referencing its address range — then \
             cross-reference that resolver's callers to find the dynamic-import consumers.",
        ),
    }
    // the resolved-name list is the pivot artifact: named capability, no IAT
    let artifact = HitArtifact::new(
        ArtifactClass::DecodedData,
        cap_list(&names, MAX_ARTIFACT_NAMES),
    )
    .role(format!(
        "export names recovered by statically resolving API-name hashes under {}",
        finding.family
    ))
    .detail(detail_pairs(&finding.resolved, MAX_DETAIL_PAIRS));
    // context-only: the resolution itself is the validation, so no branch or
    // return-value check applies (reachability + clustering still do)
    let mut hit = DetectionHit::new(
        T1027_007,
        finding.anchor,
        finding.function,
        high(),
        content,
        reasoning,
    )
    .kind(EvidenceKind::ContextOnly)
    .name(format!(
        "API Hashing - {} resolves {count} names @ {}",
        finding.family,
        fmt_addr(finding.anchor)
    ))
    .artifact(artifact);
    // attach the full site list for the validator and the markup pass
    if finding.sites.len() > 1 {
        hit = hit.sites(finding.sites.clone());
    }
    hit
}

/// Build the low-severity corroboration hits for a resolver's constants.
///
/// Only constants seeding a family the finding qualified under are emitted —
/// a bare constant (or one for an algorithm nothing resolved under) is never
/// a finding of any kind.
///
/// # Arguments
///
/// * `entry` - The resolver function's entry address
/// * `finding` - The resolver finding these constants corroborate
/// * `constants` - The function's constants: kind → sites
fn constant_hits(
    entry: u64,
    finding: &ResolverFinding,
    constants: &BTreeMap<ConstKind, Vec<u64>>,
) -> Vec<DetectionHit> {
    // the family labels this finding qualified under (headline + extras)
    let families: Vec<&'static str> = std::iter::once(finding.family)
        .chain(finding.extra_families.iter().map(|(label, _)| *label))
        .collect();
    let mut hits = Vec::new();
    for (kind, kind_sites) in constants {
        // a constant corroborates only the algorithm family it seeds
        let Some(family) = kind
            .families()
            .iter()
            .find(|label| families.contains(label))
        else {
            continue;
        };
        // sorted, deduped sites for stable rendering
        let mut sites = kind_sites.clone();
        sites.sort_unstable();
        sites.dedup();
        let content = format!(
            "{} @ {} (hash-algorithm constant)",
            kind.label(),
            list_addrs(&sites, MAX_SITES)
        );
        let reasoning = format!(
            "{} A bare hash constant is never a standalone finding here: it is reported only \
             because hash values in the same function statically resolved to export names \
             under the {family} family (see the API hashing finding at {}), which this \
             constant corroborates.",
            kind.blurb(),
            fmt_addr(finding.anchor),
        );
        // context-only: a constant's weight comes from the company it keeps
        let anchor = sites.first().copied().unwrap_or(entry);
        let mut hit = DetectionHit::new(T1027_007, anchor, Some(entry), low(), content, reasoning)
            .kind(EvidenceKind::ContextOnly);
        if sites.len() > 1 {
            hit = hit.sites(sites);
        }
        hits.push(hit);
    }
    hits
}

/// Build the single rollup hit for findings past the per-program cap.
///
/// The rolled-up sites carry the same self-validating evidence as the detailed
/// findings; the summary is reported at `medium()` only so the capped detailed
/// findings stay the loudest entries in the report.
///
/// # Arguments
///
/// * `overflow` - The findings past the cap, strongest first
fn summary_hit(overflow: &[ResolverFinding]) -> DetectionHit {
    // name each rolled-up site with its family and resolution count
    let listed: Vec<String> = overflow
        .iter()
        .map(|f| {
            format!(
                "{} ({}, {} names)",
                fmt_addr(f.anchor),
                f.family,
                f.resolved.len()
            )
        })
        .collect();
    let content = format!(
        "API hashing: {} more resolver sites past the reporting cap — {}",
        overflow.len(),
        cap_list(&listed, MAX_SITES),
    );
    let reasoning = format!(
        "Each site here independently cleared the bar of {MIN_RESOLVED}+ distinct values \
         resolving to real export names under one hash algorithm — the same self-validating \
         evidence as the detailed API hashing findings — and is rolled up so the report \
         stays readable past {MAX_RESOLVER_FINDINGS} resolver findings. The artifact \
         carries the additional resolved names. Next action: treat every listed site as a \
         resolver and cross-reference its callers to find the dynamic-import consumers."
    );
    // pool the overflow's resolved names into one pivotable artifact, deduped
    let mut names: Vec<String> = Vec::new();
    for finding in overflow {
        for name in display_names(&finding.resolved) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    let artifact = HitArtifact::new(
        ArtifactClass::DecodedData,
        cap_list(&names, MAX_ARTIFACT_NAMES),
    )
    .role("export names resolved at resolver sites past the per-program reporting cap")
    .detail(format!(
        "pooled from {} resolver sites; per-site families and counts are in the finding \
         content",
        overflow.len()
    ));
    // anchor the rollup on the strongest overflowed site, carrying the rest
    let anchors: Vec<u64> = overflow.iter().map(|f| f.anchor).collect();
    let anchor = anchors.first().copied().unwrap_or_default();
    let mut hit = DetectionHit::new(T1027_007, anchor, None, medium(), content, reasoning)
        .kind(EvidenceKind::ContextOnly)
        .artifact(artifact);
    if anchors.len() > 1 {
        hit = hit.sites(anchors);
    }
    hit
}

/// The whole detection, computed from pre-fetched program facts.
///
/// The pure core of [`ApiHashing::run`], split out so the resolution rules,
/// grouping, and caps are testable against byte/instruction fixtures without a
/// Ghidra bridge.
///
/// # Arguments
///
/// * `all` - Every function's entry address and decoded instructions
/// * `data_sections` - The non-executable initialized sections (name, base, bytes)
/// * `ref_index` - The reverse data-reference index for table attribution
/// * `fn_name` - Resolves an entry to the function's Ghidra name
fn detect<F>(
    all: &[(u64, Vec<Instruction>)],
    data_sections: &[SectionBytes],
    ref_index: &[(u64, u64)],
    fn_name: &F,
) -> Vec<DetectionHit>
where
    F: Fn(u64) -> Option<String>,
{
    // scan every function once for immediates, constants, and idioms
    let mut facts: BTreeMap<u64, FunctionFacts> = BTreeMap::new();
    for (entry, insns) in all {
        let found = function_facts(insns);
        if !found.is_empty() {
            facts.insert(*entry, found);
        }
    }
    // a standard CRC-32 table in data corroborates a referencing resolver
    for (_, base, bytes) in data_sections {
        if let Some(off) = find_sig(bytes, &CRC32_TABLE_SIG) {
            let addr = base.saturating_add(off as u64);
            if let Some(entry) = referencing_function(ref_index, addr, REF_WINDOW) {
                facts
                    .entry(entry)
                    .or_default()
                    .constants
                    .entry(ConstKind::Crc32Table)
                    .or_default()
                    .push(addr);
            }
        }
    }
    // pool every distinct candidate value across code and data
    let targets = collect_targets(&facts, data_sections);
    // fewer candidates than the threshold can never qualify: skip the walk
    if targets.len() < MIN_RESOLVED {
        return Vec::new();
    }
    // one dictionary walk resolves every candidate under every strong family
    let (matches, _) = match_hashes(
        &targets,
        &STRONG_FAMILIES,
        &NameVariants::default(),
        None,
        &[],
    );
    if matches.is_empty() {
        return Vec::new();
    }
    // index the matches by family and remember every resolving value
    let index = family_index(&matches);
    let resolving: HashSet<u32> = matches.iter().map(|m| m.hash).collect();
    // the family labels in a fixed order, for deterministic output
    let family_order: Vec<&'static str> = STRONG_FAMILIES.iter().map(|f| f.label()).collect();
    // find and attribute the qualifying immediate tables in data
    let (fn_clusters, free_clusters) =
        find_table_clusters(data_sections, &resolving, &index, &family_order, ref_index);
    // assemble the qualifying resolver findings, then apply the program cap
    let findings = assemble_findings(&facts, &fn_clusters, free_clusters, &index, &family_order);
    let (kept, overflow) = cap_findings(findings);
    // emit each detailed finding followed by its corroborating constants
    let mut hits = Vec::new();
    for finding in &kept {
        hits.push(resolver_hit(finding, fn_name));
        if let Some(entry) = finding.function
            && let Some(found) = facts.get(&entry)
        {
            hits.extend(constant_hits(entry, finding, &found.constants));
        }
    }
    // everything past the cap rolls into one summary hit
    if !overflow.is_empty() {
        hits.push(summary_hit(&overflow));
    }
    hits
}

#[async_trait::async_trait]
impl Detection for ApiHashing {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "api_hashing"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1027_007
    }

    /// Resolve candidate hash immediates and tables, reporting the hits.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The shared analysis context
    #[tracing::instrument(name = "ApiHashing::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // decode every function once (cached): the immediates live in code
        let all = ctx.all_function_disassembly().await?;
        // non-executable initialized blocks are where immediate tables live;
        // code immediates already cover the executable blocks
        let data_sections: Vec<SectionBytes> = ctx
            .sections()
            .await?
            .into_iter()
            .filter(|s| s.is_initialized && !s.is_executable() && !s.bytes.is_empty())
            .map(|s| (s.name, s.base, s.bytes))
            .collect();
        // the reverse data-reference index attributes tables to their resolver
        let ref_index = ctx.reference_index().await?;
        // the pure core does the resolution, grouping, caps, and hit building
        let hits = detect(&all, &data_sections, &ref_index, &|entry| {
            ctx.function_name(entry).map(str::to_owned)
        });
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::confidence::Confidence;

    /// Build a minimal instruction for the fixture bodies.
    ///
    /// # Arguments
    ///
    /// * `mnemonic` - The instruction mnemonic
    /// * `operands` - The joined operand string
    /// * `address` - The instruction address
    fn insn(mnemonic: &str, operands: &str, address: u64) -> Instruction {
        Instruction {
            address: fmt_addr(address),
            mnemonic: mnemonic.into(),
            operands: vec![operands.into()],
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// Build a resolver-style body comparing EAX against each hash immediate.
    ///
    /// # Arguments
    ///
    /// * `base` - The address of the first compare
    /// * `hashes` - The hash immediates to compare against
    fn cmp_body(base: u64, hashes: &[u32]) -> Vec<Instruction> {
        // one compare per hash, eight bytes apart
        hashes
            .iter()
            .zip(0u64..)
            .map(|(hash, idx)| insn("CMP", &format!("eax, 0x{hash:x}"), base + idx * 8))
            .collect()
    }

    /// Pick `n` dictionary names whose hash under `family` passes the filter.
    ///
    /// # Arguments
    ///
    /// * `family` - The hash family to compute under
    /// * `n` - How many names to pick
    fn resolvable_names(family: HashFamily, n: usize) -> Vec<(&'static str, u32)> {
        // a pool of common dictionary names; the filter rejects ~0.4% per name
        [
            "LoadLibraryA",
            "GetProcAddress",
            "VirtualAlloc",
            "WinExec",
            "CreateFileA",
            "ExitProcess",
            "WSAStartup",
            "InternetOpenA",
        ]
        .iter()
        .filter_map(|name| {
            plausible_hash(u64::from(family.hash(name.as_bytes()))).map(|hash| (*name, hash))
        })
        .take(n)
        .collect()
    }

    /// Three immediates hashed with each strong family fire one high finding
    /// naming the resolved exports, with the artifact carrying the list.
    #[test]
    fn three_resolutions_fire_under_each_family() {
        for family in STRONG_FAMILIES {
            // hash three real dictionary names with the real implementation
            let picked = resolvable_names(family, 3);
            assert_eq!(picked.len(), 3, "{} fixture names", family.label());
            let hashes: Vec<u32> = picked.iter().map(|(_, hash)| *hash).collect();
            let all = vec![(0x40_1000u64, cmp_body(0x40_1000, &hashes))];
            let hits = detect(&all, &[], &[], &|_| None);
            assert_eq!(hits.len(), 1, "{}: {hits:?}", family.label());
            let hit = &hits[0];
            // the finding is high-severity, attributed, and context-only
            assert!(matches!(hit.confidence, Confidence::Likely));
            assert_eq!(hit.function, Some(0x40_1000));
            assert_eq!(hit.kind, EvidenceKind::ContextOnly);
            assert!(hit.content.contains(family.label()), "{}", hit.content);
            // every resolved name is in the content and the artifact value
            let artifact = hit.artifact.as_ref().expect("an artifact");
            assert_eq!(artifact.class, ArtifactClass::DecodedData);
            for (name, _) in &picked {
                assert!(hit.content.contains(name), "{}", hit.content);
                assert!(artifact.value.contains(name), "{}", artifact.value);
            }
            // the next action points the agent at the resolver's callers
            assert!(hit.reasoning.contains("callers"), "{}", hit.reasoning);
        }
    }

    /// Below the three-resolution bar nothing fires — real hashes or junk.
    #[test]
    fn fewer_than_three_resolutions_stay_silent() {
        // two genuine fnv1a hashes of dictionary names
        let picked = resolvable_names(HashFamily::Fnv1a, 2);
        let mut values: Vec<u32> = picked.iter().map(|(_, hash)| *hash).collect();
        // plus two junk immediates, pinned unresolved by the matcher itself
        for junk in [0xdead_beefu32, 0x0bad_f00d] {
            let (matches, unmatched) = match_hashes(
                &[junk],
                &STRONG_FAMILIES,
                &NameVariants::default(),
                None,
                &[],
            );
            assert!(
                matches.is_empty() && unmatched == [junk],
                "junk fixture 0x{junk:08x} resolves"
            );
            values.push(junk);
        }
        let all = vec![(0x40_1000u64, cmp_body(0x40_1000, &values))];
        assert!(detect(&all, &[], &[], &|_| None).is_empty());
    }

    /// Three values that each resolve under a different family never aggregate.
    #[test]
    fn split_family_resolutions_do_not_aggregate() {
        // one genuine hash per family — never three under ONE algorithm
        let fnv = resolvable_names(HashFamily::Fnv1a, 1)[0].1;
        let ror = resolvable_names(HashFamily::Ror13, 1)[0].1;
        let crc = resolvable_names(HashFamily::Crc32, 1)[0].1;
        let all = vec![(0x40_1000u64, cmp_body(0x40_1000, &[fnv, ror, crc]))];
        assert!(detect(&all, &[], &[], &|_| None).is_empty());
    }

    /// The CRC-32 polynomial alone — immediate and table — is never a finding.
    #[test]
    fn crc32_polynomial_alone_stays_silent() {
        // the exact ubiquitous constant, as an immediate in code...
        let body = vec![
            insn("MOV", "eax, 0xedb88320", 0x40_1000),
            insn("XOR", "eax, 0x1", 0x40_1004),
        ];
        // ...and as the standard table in data, referenced by the function
        let mut table = vec![0u8; 4];
        table.extend_from_slice(&CRC32_TABLE_SIG);
        let data = vec![(".rdata".to_string(), 0x0050_0000u64, table)];
        let ref_index = vec![(0x0050_0000u64, 0x40_1000u64)];
        let hits = detect(&[(0x40_1000, body)], &data, &ref_index, &|_| None);
        assert!(hits.is_empty(), "{hits:?}");
    }

    /// Matching-family constants surface as low corroboration beside the
    /// resolver; constants for other algorithms stay out entirely.
    #[test]
    fn constants_corroborate_a_resolver_finding() {
        // three fnv1a resolutions plus the FNV seed, prime, and a CRC-32 poly
        let picked = resolvable_names(HashFamily::Fnv1a, 3);
        let hashes: Vec<u32> = picked.iter().map(|(_, hash)| *hash).collect();
        let mut body = cmp_body(0x40_1000, &hashes);
        body.push(insn("MOV", "eax, 0x811c9dc5", 0x40_1100));
        body.push(insn("IMUL", "eax, eax, 0x1000193", 0x40_1108));
        body.push(insn("MOV", "edx, 0xedb88320", 0x40_1110));
        let hits = detect(&[(0x40_1000u64, body)], &[], &[], &|_| None);
        // one high resolver plus the two FNV constants; the CRC poly is silent
        assert_eq!(hits.len(), 3, "{hits:?}");
        assert!(
            hits[0].reasoning.contains("offset basis"),
            "{}",
            hits[0].reasoning
        );
        for hit in &hits[1..] {
            assert!(matches!(hit.confidence, Confidence::Untrusted));
            assert_eq!(hit.function, Some(0x40_1000));
            assert!(
                hit.reasoning.contains("never a standalone finding"),
                "{}",
                hit.reasoning
            );
        }
        assert!(
            hits.iter().all(|hit| !hit.content.contains("CRC-32")),
            "{hits:?}"
        );
    }

    /// An immediate table in data resolves, clusters, and attributes to the
    /// function that references it.
    #[test]
    fn data_table_resolves_and_attributes() {
        // the table: three djb2 hashes plus one non-resolving entry, packed LE
        let picked = resolvable_names(HashFamily::Djb2Add, 3);
        let mut bytes = Vec::new();
        for (_, hash) in &picked {
            bytes.extend_from_slice(&hash.to_le_bytes());
        }
        bytes.extend_from_slice(&0xdead_beefu32.to_le_bytes());
        let data = vec![(".rdata".to_string(), 0x0050_0000u64, bytes)];
        // the loop body references the table; it carries no immediates itself
        let body = vec![insn("LEA", "esi, [0x500000]", 0x40_1000)];
        let ref_index = vec![(0x0050_0000u64, 0x40_1000u64)];
        let hits = detect(&[(0x40_1000, body)], &data, &ref_index, &|_| None);
        assert_eq!(hits.len(), 1, "{hits:?}");
        let hit = &hits[0];
        // attributed to the referencing function, worded as a table resolution
        assert_eq!(hit.function, Some(0x40_1000));
        assert!(hit.content.contains("immediate-table"), "{}", hit.content);
        assert!(hit.sites.contains(&0x0050_0000), "{:?}", hit.sites);
        for (name, _) in &picked {
            assert!(hit.content.contains(name), "{}", hit.content);
        }
    }

    /// Resolving words too far apart never cluster into a table finding.
    #[test]
    fn scattered_data_matches_do_not_cluster() {
        // three genuine hashes spread a page apart in one section
        let picked = resolvable_names(HashFamily::Ror13, 3);
        let mut bytes = vec![0u8; 0x3000];
        for ((_, hash), slot) in picked.iter().zip([0x0usize, 0x1000, 0x2000]) {
            bytes[slot..slot + 4].copy_from_slice(&hash.to_le_bytes());
        }
        let data = vec![(".data".to_string(), 0x0050_0000u64, bytes)];
        assert!(detect(&[], &data, &[], &|_| None).is_empty());
    }

    /// Resolver findings cap at four, strongest first, with one summary rollup.
    #[test]
    fn resolver_findings_cap_with_a_summary_overflow() {
        // six resolver functions: one resolves four names, the rest three
        let four: Vec<u32> = resolvable_names(HashFamily::Fnv1a, 4)
            .iter()
            .map(|(_, hash)| *hash)
            .collect();
        assert_eq!(four.len(), 4, "fixture needs four resolvable names");
        let three = &four[..3];
        let mut all = vec![(0x40_0000u64, cmp_body(0x40_0000, &four))];
        for idx in 1u64..6 {
            let entry = 0x40_0000 + idx * 0x1000;
            all.push((entry, cmp_body(entry, three)));
        }
        let hits = detect(&all, &[], &[], &|_| None);
        // four detailed findings plus one rollup
        assert_eq!(hits.len(), 5, "{hits:?}");
        let detailed = &hits[..4];
        assert!(
            detailed
                .iter()
                .all(|hit| matches!(hit.confidence, Confidence::Likely))
        );
        // the strongest (four-name) resolver survives the cap
        assert!(detailed.iter().any(|hit| hit.function == Some(0x40_0000)));
        // the rollup names the two capped sites and carries their names
        let summary = &hits[4];
        assert!(matches!(summary.confidence, Confidence::Unsure));
        assert!(summary.content.contains("2 more"), "{}", summary.content);
        assert_eq!(summary.sites.len(), 2);
        assert!(summary.artifact.is_some());
    }

    /// A PEB walk in the resolver's body is named in the finding's reasoning,
    /// and a resolver without one says nothing about idioms it lacks.
    #[test]
    fn peb_walk_is_named_as_corroboration() {
        // the resolver with a PEB access alongside its compares
        let picked = resolvable_names(HashFamily::Ror13, 3);
        let hashes: Vec<u32> = picked.iter().map(|(_, hash)| *hash).collect();
        let mut body = cmp_body(0x40_1000, &hashes);
        body.push(insn("MOV", "eax, fs:[0x30]", 0x40_1100));
        let hits = detect(&[(0x40_1000u64, body)], &[], &[], &|_| None);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].reasoning.contains("PEB"), "{}", hits[0].reasoning);
        // the plain resolver carries no idiom sentence at all
        let plain = detect(
            &[(0x40_1000u64, cmp_body(0x40_1000, &hashes))],
            &[],
            &[],
            &|_| None,
        );
        assert!(
            !plain[0].reasoning.contains("PEB"),
            "{}",
            plain[0].reasoning
        );
    }

    /// The immediate filter keeps plausible hashes and drops addresses, seeds,
    /// fillers, and control-flow targets.
    #[test]
    fn candidate_immediates_filter_noise() {
        let insns = vec![
            // kept: the classic Metasploit kernel32!LoadLibraryA compare
            insn("CMP", "eax, 0x726774c", 0x1000),
            // dropped: below the range floor (small integers, flags)
            insn("MOV", "edx, 0x1505", 0x1008),
            // dropped: a classic 32-bit image address
            insn("PUSH", "0x405030", 0x1010),
            // dropped: repeated-byte filler
            insn("MOV", "ecx, 0x11111111", 0x1018),
            // dropped: an algorithm seed, never a target
            insn("MOV", "eax, 0x811c9dc5", 0x1020),
            // dropped: call targets are code addresses, not data immediates
            insn("CALL", "0x72677400", 0x1028),
            // dropped: wider than any 32-bit hash
            insn("MOV", "rax, 0x140001000", 0x1030),
        ];
        assert_eq!(insn_candidates(&insns), vec![(0x1000, 0x0726_774c)]);
    }

    /// Words cluster by gap: tight runs group, distant matches split.
    #[test]
    fn table_words_cluster_by_gap() {
        // two words 8 bytes apart, one 56 bytes further, one a page away
        let words = vec![(0x1000u64, 1u32), (0x1008, 2), (0x1040, 3), (0x2000, 4)];
        let clusters = cluster_words(&words, TABLE_GAP);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].entries.len(), 3);
        assert_eq!(clusters[1].entries.len(), 1);
    }
}
