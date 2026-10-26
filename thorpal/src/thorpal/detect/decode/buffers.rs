//! Recovering the static data buffers a function references.
//!
//! A decoder loop reads its ciphertext (and sometimes its key) from a fixed data
//! address loaded with `lea`/`mov`. To decode statically we parse those addresses
//! out of the function's operands ([`data_addrs`]) and slice the containing
//! section's bytes ([`slice_at`]) — no extra Ghidra query, since the section bytes
//! are already cached by [`AnalysisContext::initialized_bytes`].
//!
//! [`AnalysisContext::initialized_bytes`]: crate::thorpal::detect::context::AnalysisContext::initialized_bytes

use ghidra_cli::models::Instruction;

use crate::thorpal::detect::context::SectionBytes;

/// The smallest value treated as a candidate address (below this it is an immediate,
/// not a pointer into a loaded section).
const MIN_ADDR: u64 = 0x1000;

/// The largest number of hex digits parsed as an address (64-bit).
const MAX_HEX_DIGITS: usize = 16;

/// Parse the distinct static data addresses referenced by a function's operands.
///
/// Recognizes `0x…` immediates/displacements, Ghidra data labels whose suffix is an
/// address (`DAT_00102010`, `s_msg_00102010`, `PTR_…_00102010`), and bare hex
/// displacements inside `[…]`. Candidates below [`MIN_ADDR`] are dropped as
/// immediates; the caller confirms each falls in a real section via [`slice_at`].
///
/// # Arguments
///
/// * `insns` - The function's disassembled instructions
#[must_use]
pub fn data_addrs(insns: &[Instruction]) -> Vec<u64> {
    let mut out = Vec::new();
    for insn in insns {
        // only operands can carry a data reference (empty string tokenizes to nothing)
        let ops = crate::thorpal::detect::op_str(insn);
        for token in tokenize(&ops) {
            if let Some(addr) = token_addr(token) {
                // keep plausible pointers only; small values are immediates
                if addr >= MIN_ADDR && !out.contains(&addr) {
                    out.push(addr);
                }
            }
        }
    }
    out
}

/// Split an operand string into `[0-9A-Za-z_]` tokens (label/number atoms).
///
/// # Arguments
///
/// * `ops` - The operand string
fn tokenize(ops: &str) -> impl Iterator<Item = &str> {
    ops.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty())
}

/// Interpret one operand token as a candidate address, if it looks like one.
///
/// # Arguments
///
/// * `token` - A single operand atom (e.g. `"0x102010"`, `"DAT_00102010"`)
fn token_addr(token: &str) -> Option<u64> {
    // 1) an explicit hex immediate/displacement: 0x…
    if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        return parse_hex(hex);
    }
    // 2) a Ghidra label whose final `_`-segment is an address (DAT_…, s_…_…, PTR_…)
    if let Some(idx) = token.rfind('_') {
        if let Some(addr) = parse_hex(&token[idx + 1..]) {
            return Some(addr);
        }
    }
    // 3) a bare hex atom (e.g. a raw displacement `[00102010]`)
    parse_hex(token)
}

/// Parse a hex string to `u64`, rejecting empty/oversized/non-hex input.
///
/// # Arguments
///
/// * `hex` - The hex digits (no `0x` prefix)
fn parse_hex(hex: &str) -> Option<u64> {
    // bound the length so a long token can't be a huge/garbage "address"
    if hex.is_empty() || hex.len() > MAX_HEX_DIGITS || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

/// Return the section name and bytes starting at `addr` (bounded to `max`), if
/// `addr` falls inside an initialized section.
///
/// # Arguments
///
/// * `sections` - The initialized section bytes (`AnalysisContext::initialized_bytes`)
/// * `addr` - The absolute address to slice from
/// * `max` - The maximum number of bytes to return
#[must_use]
pub fn slice_at(sections: &[SectionBytes], addr: u64, max: usize) -> Option<(&str, &[u8])> {
    for (name, base, bytes) in sections {
        // is addr within [base, base + len)?
        let end = base.saturating_add(bytes.len() as u64);
        if addr >= *base && addr < end {
            // slice from the address to the (bounded) end of the section
            let off = (addr - base) as usize;
            let stop = (off + max).min(bytes.len());
            return Some((name.as_str(), &bytes[off..stop]));
        }
    }
    None
}

/// Whether a section name is data-like (holds constants/strings/ciphertext) rather
/// than code or a pointer/linkage section.
///
/// Shared by the decode passes and the reference index; matches `.data`, `.rodata`,
/// `.rdata`, `.data.rel.ro`, etc.
///
/// # Arguments
///
/// * `name` - The section name
#[must_use]
pub fn is_data_section(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.contains("data") || l.contains("rodata") || l.contains("rdata")
}

/// Build the reverse of [`data_addrs`]: a map from each referenced **data** address to
/// the entry of a function that references it.
///
/// This lets a data-derived finding (a decoded buffer in `.rodata`) be attributed to
/// the function that decodes it. Only addresses that land in a data-like section are
/// indexed, so a code target (e.g. a jump address) can never mis-attribute a buffer.
/// The result is sorted by data address for [`referencing_function`]'s binary search.
///
/// # Arguments
///
/// * `all` - Every function's `(entry, instructions)` (from `all_function_disassembly`)
/// * `sections` - The initialized section bytes (to confirm an address is data)
#[must_use]
pub fn reference_index(
    all: &[(u64, Vec<Instruction>)],
    sections: &[SectionBytes],
) -> Vec<(u64, u64)> {
    let mut index: Vec<(u64, u64)> = Vec::new();
    for (entry, insns) in all {
        // every static data address this function references...
        for addr in data_addrs(insns) {
            // ...that actually lands in a data-like section
            if let Some((name, _)) = slice_at(sections, addr, 1) {
                if is_data_section(name) {
                    index.push((addr, *entry));
                }
            }
        }
    }
    // sort by data address so `referencing_function` can binary-search
    index.sort_by_key(|(addr, _)| *addr);
    index
}

/// Return the entry of a function that references the buffer containing `addr`.
///
/// Finds the greatest referenced data address `<= addr` (the base of the buffer the
/// address falls in) and returns its function, provided `addr` is within `window`
/// bytes of that base. A run recovered mid-buffer is thus attributed to the function
/// that referenced the buffer's start. Returns `None` when nothing references a nearby
/// address (the caller then falls back to the sample).
///
/// # Arguments
///
/// * `index` - The sorted reference index from [`reference_index`]
/// * `addr` - The (data) address to attribute
/// * `window` - The maximum distance from a referenced base to still attribute
#[must_use]
pub fn referencing_function(index: &[(u64, u64)], addr: u64, window: u64) -> Option<u64> {
    // the number of entries whose data address is <= addr (index is sorted)
    let pos = index.partition_point(|(data_addr, _)| *data_addr <= addr);
    if pos == 0 {
        // addr precedes every referenced data address → no container
        return None;
    }
    // the closest referenced address at or before addr is the candidate buffer base
    let (base, entry) = index[pos - 1];
    (addr.saturating_sub(base) <= window).then_some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an instruction with the given operands for parsing tests.
    fn insn(operands: &str) -> Instruction {
        Instruction {
            address: "1000".into(),
            mnemonic: "LEA".into(),
            operands: vec![operands.into()],
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// `data_addrs` parses 0x…, DAT_…, and bare-hex references and drops immediates.
    #[test]
    fn data_addrs_parses_all_forms() {
        let insns = vec![
            insn("RAX, [0x00102010]"),
            insn("RCX, [DAT_00103040]"),
            insn("RDX, [s_note_00104050]"),
            insn("AL, 0x2a"), // small immediate → dropped
        ];
        let mut got = data_addrs(&insns);
        got.sort_unstable();
        assert_eq!(got, vec![0x102010, 0x103040, 0x104050]);
    }

    /// `slice_at` returns the bytes from an address inside a section, bounded.
    #[test]
    fn slice_at_finds_and_bounds() {
        let sections = vec![(
            ".data".to_string(),
            0x1000u64,
            (0..32u8).collect::<Vec<u8>>(),
        )];
        let (name, bytes) = slice_at(&sections, 0x1010, 4).expect("in range");
        assert_eq!(name, ".data");
        assert_eq!(bytes, &[16, 17, 18, 19]);
        // an address outside every section yields nothing
        assert!(slice_at(&sections, 0x9999, 4).is_none());
    }

    /// `is_data_section` accepts data-like names and rejects code/linkage.
    #[test]
    fn is_data_section_classifies() {
        assert!(is_data_section(".data"));
        assert!(is_data_section(".rodata"));
        assert!(is_data_section(".data.rel.ro"));
        assert!(!is_data_section(".text"));
        assert!(!is_data_section(".got"));
    }

    /// `reference_index` maps each referenced data address to its function, skipping
    /// references that don't land in a data section.
    #[test]
    fn reference_index_maps_data_refs_to_functions() {
        // fn 0x400000 references a .rodata buffer; fn 0x400100 references a .text
        // address (must be excluded) and another .rodata buffer
        let all = vec![
            (0x400000u64, vec![insn("RSI, [0x00102000]")]),
            (
                0x400100u64,
                vec![insn("RDI, [0x00401234]"), insn("RCX, [0x00102100]")],
            ),
        ];
        let sections = vec![
            (".rodata".to_string(), 0x00102000u64, vec![0u8; 0x400]),
            (".text".to_string(), 0x00400000u64, vec![0u8; 0x2000]),
        ];
        let index = reference_index(&all, &sections);
        // only the two .rodata references are indexed (the .text ref is dropped)
        assert_eq!(index, vec![(0x102000, 0x400000), (0x102100, 0x400100)]);
    }

    /// `referencing_function` attributes an address to the nearest preceding
    /// referenced base within the window, and returns None otherwise.
    #[test]
    fn referencing_function_nearest_within_window() {
        let index = vec![(0x102000u64, 0x400000u64), (0x102100u64, 0x400100u64)];
        // exact base hit
        assert_eq!(
            referencing_function(&index, 0x102000, 0x1000),
            Some(0x400000)
        );
        // mid-buffer: attributed to the preceding base 0x102000
        assert_eq!(
            referencing_function(&index, 0x102050, 0x1000),
            Some(0x400000)
        );
        // just past the second base
        assert_eq!(
            referencing_function(&index, 0x102110, 0x1000),
            Some(0x400100)
        );
        // before every reference → None
        assert_eq!(referencing_function(&index, 0x101000, 0x1000), None);
        // beyond the window from the nearest base → None
        assert_eq!(
            referencing_function(&index, 0x102100 + 0x5000, 0x1000),
            None
        );
    }
}
