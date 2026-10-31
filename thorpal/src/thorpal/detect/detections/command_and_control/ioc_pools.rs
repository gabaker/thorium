//! Indicator pooling for the network-IOC pass (MITRE ATT&CK **T1071** family).
//!
//! A flat list of endpoints does not tell an analyst which of them the program actually
//! reaches for at request time. Two structural facts start to: indicators stored
//! **adjacent to one another** in a data region were laid down as one table by whatever
//! produced the binary, and indicators reached from **one common function** are
//! selected by that function's code. This module groups the indicators
//! [`super::network_comms::ioc_records`] recovered on those two bases, and reports for
//! each group the **selection idiom** visible in the referencing function's
//! disassembly: a call to a pseudo-random generator, a scaled-index load off a table
//! base, a back-edge that lets the function revisit the group, or a delay call of the
//! kind a retry or beacon interval uses.
//!
//! Pools are built from the *complete* record set, never the per-class-capped subset a
//! report shows — a six-member pool reported as a three-member one is worse than no
//! pool at all.
//!
//! # Member and pool quality gates
//!
//! Grouping junk only manufactures structure: three carve offsets inside one
//! ransom-note literal are not a table, and a statically linked stack's header
//! strings pooled under an unnamed library function imply a selection that is not
//! there. So membership excludes fragment-bearing URLs (reader-facing documentation
//! links) and literals owned by recognized runtime/library code, and a candidate pool
//! is only reported when it keeps at least one substantial member (a code xref or a
//! medium-grade class), a contiguous pool is backed by a code xref or a non-runtime
//! owner, an exclusively request-fabric pool is referenced from a function that also
//! does networking, and the members do not all sit inside one defined string.
//!
//! # Phrasing rule
//!
//! Everything here is reported as an **observed fact about structure**. A pool is never
//! labelled a decoy list and a member is never suppressed for looking incidental.
//! Which members are live is a dynamic question this pass cannot answer, and the two
//! ways of guessing at it both cause real harm: calling a live C2 a decoy walks an
//! analyst straight past the thing they were hunting, and calling an unrelated host a
//! C2 gets it blocked. So the finding says what is stored where, what references it,
//! and what the referencing code does — and stops there.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::thorpal::detect::confidence::Confidence;
use anyhow::Result;
use ghidra_cli::models::{Instruction, StringData};

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr, parse_addr};
use crate::thorpal::detect::detections::{
    AttackTechnique, DetectionHit, EvidenceKind, confidence_rank, low, medium,
};

use super::T1071;
use super::network_comms::{IocRecord, NetIoc, is_runtime_function};

/// The max gap (bytes) between two indicator strings that still counts as adjacency.
///
/// Wide enough to span the strings themselves plus the alignment padding a linker
/// inserts between them, narrow enough that two unrelated regions of a data block do
/// not merge into one "pool".
const POOL_GAP: u64 = 256;

/// The min members a group needs before it is reported as a pool.
///
/// Two adjacent indicators is a coincidence of layout; three is a table.
const MIN_POOL_MEMBERS: usize = 3;

/// The max pools reported (bounds a binary that stores hundreds of indicators).
const MAX_POOLS: usize = 12;

/// The max members listed inside one pool's content block.
const MAX_MEMBERS_SHOWN: usize = 40;

/// Pseudo-random generator entry points, by name.
///
/// A platform fact — the list grows when a new standard library ships, not when a new
/// sample appears. Matched case-insensitively as a substring of a call operand, so the
/// mangled and namespaced forms (`_ZN4rand…`, `math/rand.Intn`) are covered.
const RANDOM_APIS: &[&str] = &[
    "rand",
    "random",
    "getrandom",
    "arc4random",
    "rtlrandom",
    "cryptgenrandom",
    "bcryptgenrandom",
    "systemfunction036",
    "rdrand",
];

/// Sleep/delay entry points, by name — the shape of a retry or beacon interval.
const DELAY_APIS: &[&str] = &[
    "sleep",
    "nanosleep",
    "waitforsingleobject",
    "waitformultipleobjects",
    "settimer",
];

/// Why a set of indicators is treated as one pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolBasis {
    /// The indicators are stored adjacent to one another in a data region.
    Contiguous,
    /// The indicators are all referenced from one function.
    SharedFunction,
}

impl PoolBasis {
    /// The noun this basis is reported under.
    fn label(self) -> &'static str {
        match self {
            Self::Contiguous => "contiguous data region",
            Self::SharedFunction => "shared referencing function",
        }
    }

    /// The observation the grouping rests on, in a sentence.
    fn observation(self) -> &'static str {
        match self {
            Self::Contiguous => {
                "These indicators are stored next to each other in one data region, so whatever \
                 produced the binary emitted them as a single table rather than as unrelated \
                 literals scattered through the image."
            }
            Self::SharedFunction => {
                "These indicators are all referenced from the same function, so that function is \
                 where the program chooses between them."
            }
        }
    }
}

/// A selection idiom observed in the disassembly of a function that reaches a pool.
///
/// Each variant is a directly observable instruction-level fact, not an inference about
/// intent: the function *contains* a call to a randomizer, *contains* a scaled-index
/// load, and so on. What the program does with them at run time is beyond a static pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Idiom {
    /// The function calls a pseudo-random number generator.
    Randomizer,
    /// The function loads through a scaled index (`[base + i*n]`), the shape of a
    /// lookup into an array of pointers.
    IndexedLoad,
    /// The function contains a back-edge, so it can revisit the pool.
    Loop,
    /// The function calls a sleep/delay API.
    Delay,
}

impl Idiom {
    /// How this idiom is described in the finding.
    fn label(self) -> &'static str {
        match self {
            Self::Randomizer => {
                "calls a pseudo-random number generator, so the member used is not fixed at build \
                 time"
            }
            Self::IndexedLoad => {
                "loads through a scaled index (`[base + i*n]`), the shape of a lookup into an \
                 array of pointers"
            }
            Self::Loop => {
                "contains a back-edge, so it can revisit the group rather than use one \
                           member once"
            }
            Self::Delay => "calls a sleep/delay API, the shape of a retry or a beacon interval",
        }
    }
}

/// One group of indicators reported as a pool.
struct Pool {
    /// Why these indicators are grouped.
    basis: PoolBasis,
    /// The members, in the order they were recovered.
    members: Vec<IocRecord>,
    /// The function that references the pool, when one was resolved.
    function: Option<u64>,
    /// The selection idioms observed in that function's disassembly.
    idioms: BTreeSet<Idiom>,
}

impl Pool {
    /// The address the finding is anchored at.
    ///
    /// The referencing function when there is one — that is the code an analyst opens —
    /// and otherwise the first member's storage address.
    fn address(&self) -> u64 {
        self.function
            .unwrap_or_else(|| self.members.first().map_or(0, |member| member.string_addr))
    }

    /// The ATT&CK technique the pool evidences.
    ///
    /// The members' own technique when they agree, and the T1071 parent when a shared
    /// function pulls together more than one kind of indicator.
    fn technique(&self) -> AttackTechnique {
        let mut techniques = self.members.iter().map(|member| member.ioc.technique());
        let Some(first) = techniques.next() else {
            return T1071;
        };
        if techniques.all(|technique| technique.id == first.id) {
            first
        } else {
            T1071
        }
    }

    /// The `(confidence, suspicion)` severity for this pool.
    ///
    /// Structure alone is `low()`: a table of hosts is how a CDN list, a mirror list,
    /// and an endpoint pool all look. An observed selection idiom in the referencing
    /// code is what raises it, because that is evidence the group is *used* as a group.
    fn severity(&self) -> (Confidence, i64) {
        if self.idioms.is_empty() {
            low()
        } else {
            medium()
        }
    }
}

/// Whether an instruction is a call.
///
/// # Arguments
///
/// * `insn` - The instruction to test
fn is_call(insn: &Instruction) -> bool {
    // CALL on x86, BL/BLR on ARM — enough for the architectures the bridge reports
    let mnemonic = insn.mnemonic.to_ascii_uppercase();
    mnemonic == "CALL" || mnemonic == "BL" || mnemonic == "BLR"
}

/// Whether an instruction is a jump to a lower address (a loop back-edge).
///
/// # Arguments
///
/// * `insn` - The instruction to test
fn is_backward_branch(insn: &Instruction) -> bool {
    // any jump mnemonic (JMP or a Jcc) can close a loop
    if !insn.mnemonic.to_ascii_uppercase().starts_with('J') {
        return false;
    }
    // a jump to a lower address is a back-edge
    let ops = crate::thorpal::detect::op_str(insn);
    match (parse_addr(ops.trim()), parse_addr(&insn.address)) {
        (Some(target), Some(here)) => target < here,
        _ => false,
    }
}

/// Whether an operand text loads through a scaled index.
///
/// Ghidra renders a scaled memory operand as `[base + index*0xN]`, so the scale marker
/// is what identifies an array lookup as opposed to a fixed offset into a struct.
///
/// # Arguments
///
/// * `operands` - The instruction's operand text, lower-cased
fn is_scaled_index(operands: &str) -> bool {
    // the scale is always rendered as a hex multiplier inside the brackets
    operands.contains('[')
        && ["*0x1", "*0x2", "*0x4", "*0x8"]
            .iter()
            .any(|scale| operands.contains(scale))
}

/// The selection idioms present in a function's disassembly.
///
/// Purely a description of what the instructions contain. Nothing here decides whether
/// the function selects from *this* pool — the caller already knows the function
/// references it — and nothing here judges the members.
///
/// # Arguments
///
/// * `insns` - The function's instructions, in program order
fn idioms(insns: &[Instruction]) -> BTreeSet<Idiom> {
    let mut found = BTreeSet::new();
    for insn in insns {
        let operands = crate::thorpal::detect::op_str(insn).to_ascii_lowercase();
        // a call into a randomizer or a delay routine, matched on the callee's name
        if is_call(insn) {
            if RANDOM_APIS.iter().any(|api| operands.contains(api)) {
                found.insert(Idiom::Randomizer);
            }
            if DELAY_APIS.iter().any(|api| operands.contains(api)) {
                found.insert(Idiom::Delay);
            }
        }
        // an array lookup through a scaled index
        if is_scaled_index(&operands) {
            found.insert(Idiom::IndexedLoad);
        }
        // a back-edge, so the function can come back around to the group
        if is_backward_branch(insn) {
            found.insert(Idiom::Loop);
        }
    }
    found
}

/// Whether a record is eligible to be a pool member at all.
///
/// Two kinds of record can never justify grouping: a fragment-bearing URL (a
/// reader-facing documentation link that is never transmitted) and a literal owned by
/// recognized runtime/library code (a statically linked stack's own string table is a
/// table, but not an attacker's).
///
/// # Arguments
///
/// * `record` - The candidate member
/// * `name` - The attributed function's name, when it has one
fn poolable_member(record: &IocRecord, name: Option<&str>) -> bool {
    // a URL nobody transmits groups nothing
    if matches!(&record.ioc, NetIoc::Url(url) if url.has_fragment()) {
        return false;
    }
    // a runtime-owned literal ships with the linked stack, not with the sample
    !is_runtime_function(name)
}

/// Whether a member carries enough weight to substantiate a pool.
///
/// A code xref is direct evidence something reaches the member; failing that, a
/// medium-or-better class (a URL, an endpoint, a User-Agent, a template) carries its
/// own weight, while the weak classes (bare IPs, URI paths, header names) do not.
///
/// # Arguments
///
/// * `record` - The member to weigh
fn member_substance(record: &IocRecord) -> bool {
    // a referenced member is substantial however weak its class is
    !record.sites.is_empty()
        || confidence_rank(record.ioc.severity().0) >= confidence_rank(medium().0)
}

/// The `[start, end)` byte ranges of the defined strings, for the one-string test.
///
/// The span is the decoded value's byte length — exact for the ASCII blobs the carve
/// sweep reads, and an under-estimate (never an over-estimate) for wide strings, so
/// the test can only miss, not misfire.
///
/// # Arguments
///
/// * `strings` - The defined strings
fn defined_string_ranges(strings: &[StringData]) -> Vec<(u64, u64)> {
    strings
        .iter()
        .filter_map(|string| {
            // locate the string and span it by its decoded byte length
            let start = parse_addr(&string.address)?;
            let len = string.value.len() as u64;
            // a span of one byte can never hold two members, so empties are useless
            (len > 1).then_some((start, start + len))
        })
        .collect()
}

/// Whether every member's storage address sits inside one defined string.
///
/// Three "members" at offsets inside a single literal (a ransom note carrying several
/// URLs in its prose) are one string the carve sweep read three times, not a table.
///
/// # Arguments
///
/// * `members` - The pool's members
/// * `ranges` - The defined strings' `[start, end)` ranges
fn within_one_string(members: &[IocRecord], ranges: &[(u64, u64)]) -> bool {
    // the pool's storage footprint; an empty pool sits inside nothing
    let Some(min) = members.iter().map(|member| member.string_addr).min() else {
        return false;
    };
    let max = members
        .iter()
        .map(|member| member.string_addr)
        .max()
        .unwrap_or(min);
    // one range holding the whole footprint means one literal holds every member
    ranges
        .iter()
        .any(|(start, end)| *start <= min && max < *end && min != max)
}

/// Whether a candidate pool clears the member-substance and junk gates.
///
/// # Arguments
///
/// * `pool` - The candidate pool
/// * `network_funcs` - Entries of functions carrying a networking API/symbol hit
/// * `ranges` - The defined strings' `[start, end)` ranges
/// * `function_is_runtime` - Whether the pool's attributed function is runtime code
fn pool_substantiated(
    pool: &Pool,
    network_funcs: &HashSet<u64>,
    ranges: &[(u64, u64)],
    function_is_runtime: bool,
) -> bool {
    // a pool with no substantial member is junk grouped by shape
    if !pool.members.iter().any(member_substance) {
        return false;
    }
    // a contiguous table additionally needs a code xref or a non-runtime owner, or
    // nothing was ever seen to reach it
    if pool.basis == PoolBasis::Contiguous {
        let referenced = pool.members.iter().any(|member| !member.sites.is_empty());
        let owned = pool.function.is_some() && !function_is_runtime;
        if !referenced && !owned {
            return false;
        }
    }
    // a pool made entirely of request fabric is a string table unless the function
    // selecting from it also does networking
    let all_fabric = pool
        .members
        .iter()
        .all(|member| matches!(member.ioc, NetIoc::Http { .. }));
    if all_fabric
        && !pool
            .function
            .is_some_and(|entry| network_funcs.contains(&entry))
    {
        return false;
    }
    // members that all sit inside one defined string are carve offsets, not a table
    !within_one_string(&pool.members, ranges)
}

/// Group indicators that are stored next to each other in one data region.
///
/// Members are grouped by class as well as by adjacency: a URL sitting between two
/// unrelated header strings is a layout accident, while five URLs in a row is a table.
///
/// # Arguments
///
/// * `records` - Every recovered indicator
fn contiguous_pools(records: &[IocRecord]) -> Vec<Vec<IocRecord>> {
    // bucket by class first so adjacency is only ever tested within one class
    let mut by_class: BTreeMap<&'static str, Vec<IocRecord>> = BTreeMap::new();
    for record in records {
        // a record with no resolved storage address cannot be placed in a layout
        if record.string_addr == 0 {
            continue;
        }
        by_class
            .entry(record.ioc.kind())
            .or_default()
            .push(record.clone());
    }
    let mut pools = Vec::new();
    for (_, mut class) in by_class {
        class.sort_by_key(|record| record.string_addr);
        // walk the sorted addresses, breaking the run wherever the gap is too wide
        let mut run: Vec<IocRecord> = Vec::new();
        for record in class {
            // measure the gap from the *end* of the previous literal, not its start, so
            // a run of long URLs is not broken up by its own members' lengths
            let contiguous = run.last().is_some_and(|prev| {
                let prev_end = prev.string_addr + prev.ioc.value().len() as u64;
                record.string_addr.saturating_sub(prev_end) <= POOL_GAP
            });
            if !contiguous && !run.is_empty() {
                // the run ended; keep it only if it is big enough to be a table
                if run.len() >= MIN_POOL_MEMBERS {
                    pools.push(std::mem::take(&mut run));
                } else {
                    run.clear();
                }
            }
            run.push(record);
        }
        if run.len() >= MIN_POOL_MEMBERS {
            pools.push(run);
        }
    }
    pools
}

/// Group indicators that are all referenced from one function.
///
/// # Arguments
///
/// * `records` - Every recovered indicator
fn shared_function_pools(records: &[IocRecord]) -> Vec<(u64, Vec<IocRecord>)> {
    // bucket by the attributed function, in address order for a stable report
    let mut by_function: BTreeMap<u64, Vec<IocRecord>> = BTreeMap::new();
    for record in records {
        let Some(entry) = record.function else {
            continue;
        };
        by_function.entry(entry).or_default().push(record.clone());
    }
    by_function
        .into_iter()
        .filter(|(_, members)| members.len() >= MIN_POOL_MEMBERS)
        .collect()
}

/// A stable identity for a pool's membership, for deduplicating the two bases.
///
/// A table of endpoints referenced from one function is found twice — once by layout
/// and once by reference — and is worth reporting once.
///
/// # Arguments
///
/// * `members` - The pool's members
fn membership_key(members: &[IocRecord]) -> String {
    let mut keys: Vec<String> = members
        .iter()
        .map(|member| format!("{}|{}", member.ioc.kind(), member.ioc.value()))
        .collect();
    keys.sort();
    keys.join("\u{1}")
}

/// Render a pool's members, the referencing function, and the observed idioms.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `pool` - The pool to render
fn pool_content(ctx: &AnalysisContext, pool: &Pool) -> String {
    let mut out = format!("### Indicator Pool — {}\n\n", pool.basis.label());
    out.push_str(&format!(
        "{} indicators grouped by {}.\n",
        pool.members.len(),
        pool.basis.label()
    ));
    // the members themselves, each with where it is stored
    out.push_str("\n### Members\n");
    for member in pool.members.iter().take(MAX_MEMBERS_SHOWN) {
        out.push_str(&format!(
            "- `{}` — {} @ {}\n",
            member.ioc.value(),
            member.ioc.kind(),
            fmt_addr(member.string_addr)
        ));
    }
    // never truncate silently, in the report either
    if pool.members.len() > MAX_MEMBERS_SHOWN {
        out.push_str(&format!(
            "- … and {} more not listed here\n",
            pool.members.len() - MAX_MEMBERS_SHOWN
        ));
    }
    // who reaches the pool
    out.push_str("\n### Referenced From\n");
    match pool.function {
        Some(entry) => {
            let name = ctx.function_name(entry).unwrap_or("(unnamed)");
            out.push_str(&format!("- **Function:** `{name}` @ {}\n", fmt_addr(entry)));
        }
        None => out.push_str("- **Function:** (no single function references the whole group)\n"),
    }
    // what that code was observed doing, with no claim about which member is live
    out.push_str("\n### Selection Idiom\n");
    if pool.idioms.is_empty() {
        out.push_str(
            "- No randomizer call, scaled-index load, back-edge, or delay call was observed in \
             the referencing function, so how the program picks between these members is not \
             visible statically.\n",
        );
    } else {
        for idiom in &pool.idioms {
            out.push_str(&format!("- The referencing function {}.\n", idiom.label()));
        }
    }
    out
}

/// The analyst-facing explanation for a pool.
///
/// # Arguments
///
/// * `pool` - The pool being explained
fn pool_reasoning(pool: &Pool) -> String {
    let mut out = format!(
        "{} indicators of the same kind are grouped here by {}. {}",
        pool.members.len(),
        pool.basis.label(),
        pool.basis.observation()
    );
    // the idioms, phrased as what was seen rather than what it means
    if pool.idioms.is_empty() {
        out.push_str(
            " No selection idiom was observed in the referencing code, so this is reported as \
             layout structure only.",
        );
    } else {
        out.push_str(" The referencing function's disassembly shows how the group is reached: it ");
        let described: Vec<&str> = pool.idioms.iter().map(|idiom| idiom.label()).collect();
        out.push_str(&described.join("; it "));
        out.push('.');
    }
    // the phrasing rule, made explicit in the finding itself
    out.push_str(
        " Which members are contacted at run time is not determinable statically: every member \
         is reported, none is called a decoy, and none is ranked above another. Treat the group \
         as the set the program can choose from and pivot on all of it.",
    );
    out
}

/// Group the recovered indicators into pools and report each one.
///
/// Runs over the complete, uncapped record set from
/// [`super::network_comms::ioc_records`], minus the members that can never justify
/// grouping (see [`poolable_member`]). Pools found by both bases are reported once,
/// preferring the contiguous reading because a data-layout fact is stronger evidence of
/// a deliberate table than a shared reference is, and each candidate pool must clear
/// [`pool_substantiated`] before it is reported.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `records` - Every recovered indicator
/// * `network_funcs` - Entries of functions carrying a networking API/symbol hit
///
/// # Errors
///
/// Returns an error if disassembling a referencing function fails.
#[tracing::instrument(name = "ioc_pools::pool_hits", skip_all, err(Debug))]
pub(super) async fn pool_hits(
    ctx: &AnalysisContext,
    records: &[IocRecord],
    network_funcs: &HashSet<u64>,
) -> Result<Vec<DetectionHit>> {
    // exclude the records that can never justify grouping (fragment URLs and
    // runtime-owned literals) before any structure is read out of the set
    let records: Vec<IocRecord> = records
        .iter()
        .filter(|record| {
            // resolve the attributed function's name for the runtime-ownership test
            let name = record.function.and_then(|entry| ctx.function_name(entry));
            poolable_member(record, name)
        })
        .cloned()
        .collect();
    // the defined strings' spans, for the carve-offsets-in-one-literal test
    let ranges = defined_string_ranges(&ctx.strings);
    let mut pools: Vec<Pool> = Vec::new();
    // the membership sets already reported, so the two bases do not double-report
    let mut seen: BTreeSet<String> = BTreeSet::new();
    // 1) groups laid out as a table in one data region
    for members in contiguous_pools(&records) {
        if !seen.insert(membership_key(&members)) {
            continue;
        }
        // a contiguous table is attributed to a function only when all of it agrees
        let first = members.first().and_then(|member| member.function);
        let function =
            first.filter(|entry| members.iter().all(|member| member.function == Some(*entry)));
        pools.push(Pool {
            basis: PoolBasis::Contiguous,
            members,
            function,
            idioms: BTreeSet::new(),
        });
    }
    // 2) groups reached from one function, wherever they are stored
    for (entry, members) in shared_function_pools(&records) {
        if !seen.insert(membership_key(&members)) {
            continue;
        }
        pools.push(Pool {
            basis: PoolBasis::SharedFunction,
            members,
            function: Some(entry),
            idioms: BTreeSet::new(),
        });
    }
    // drop the candidates whose membership cannot substantiate a selection structure
    pools.retain(|pool| {
        // resolve the pool's attributed function name for the runtime test
        let name = pool.function.and_then(|entry| ctx.function_name(entry));
        pool_substantiated(pool, network_funcs, &ranges, is_runtime_function(name))
    });
    // the largest pools first, so the cap keeps the most informative ones
    pools.sort_by_key(|pool| std::cmp::Reverse(pool.members.len()));
    // never truncate silently
    if pools.len() > MAX_POOLS {
        tracing::warn!(
            cap = MAX_POOLS,
            dropped = pools.len() - MAX_POOLS,
            "indicator pool cap reached; smaller pools were not reported",
        );
        pools.truncate(MAX_POOLS);
    }
    let mut hits = Vec::with_capacity(pools.len());
    for mut pool in pools {
        // read the referencing function's (cached) disassembly for its selection idioms
        if let Some(entry) = pool.function {
            pool.idioms = idioms(&ctx.disasm_fn(entry).await?);
        }
        hits.push(
            DetectionHit::new(
                pool.technique(),
                pool.address(),
                pool.function,
                pool.severity(),
                pool_content(ctx, &pool),
                pool_reasoning(&pool),
            )
            .name(format!(
                "Indicator Pool - {} x {} @ {}",
                pool.members.len(),
                pool.members
                    .first()
                    .map_or("indicator", |member| member.ioc.kind()),
                fmt_addr(pool.address())
            ))
            .kind(EvidenceKind::ContextOnly),
        );
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::super::network_comms::NetIoc;
    use super::*;

    /// Build an indicator record for the grouping tests.
    ///
    /// # Arguments
    ///
    /// * `value` - The IP literal the record carries
    /// * `string_addr` - Where the literal is stored
    /// * `function` - The function it is attributed to
    fn record(value: &str, string_addr: u64, function: Option<u64>) -> IocRecord {
        IocRecord {
            ioc: NetIoc::Ip {
                value: value.to_string(),
                family: "IPv4",
            },
            string_addr,
            sites: Vec::new(),
            function,
        }
    }

    /// Build a minimal instruction for the idiom tests.
    ///
    /// # Arguments
    ///
    /// * `address` - The instruction's address
    /// * `mnemonic` - The instruction mnemonic
    /// * `operands` - The operand text
    fn insn(address: &str, mnemonic: &str, operands: &str) -> Instruction {
        Instruction {
            address: address.to_string(),
            mnemonic: mnemonic.to_string(),
            operands: vec![operands.to_string()],
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// Adjacent same-class indicators group; a wide gap splits the run.
    #[test]
    fn contiguous_pools_break_on_a_wide_gap() {
        let records = vec![
            record("10.0.0.1", 0x1000, None),
            record("10.0.0.2", 0x1010, None),
            record("10.0.0.3", 0x1020, None),
            // a long way further on: a separate region, and too small on its own
            record("10.0.0.4", 0x9000, None),
            record("10.0.0.5", 0x9010, None),
        ];
        let pools = contiguous_pools(&records);
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].len(), 3);
        assert_eq!(pools[0][0].ioc.value(), "10.0.0.1");
    }

    /// Two adjacent indicators are a layout coincidence, not a table.
    #[test]
    fn contiguous_pools_need_three_members() {
        let records = vec![
            record("10.0.0.1", 0x1000, None),
            record("10.0.0.2", 0x1010, None),
        ];
        assert!(contiguous_pools(&records).is_empty());
    }

    /// Indicators reached from one function group; an unattributed one does not.
    #[test]
    fn shared_function_pools_group_by_referencing_function() {
        let records = vec![
            record("10.0.0.1", 0x1000, Some(0x40_1000)),
            record("10.0.0.2", 0x2000, Some(0x40_1000)),
            record("10.0.0.3", 0x3000, Some(0x40_1000)),
            record("10.0.0.4", 0x4000, Some(0x40_2000)),
            record("10.0.0.5", 0x5000, None),
        ];
        let pools = shared_function_pools(&records);
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].0, 0x40_1000);
        assert_eq!(pools[0].1.len(), 3);
    }

    /// The same membership found by both bases is reported once.
    #[test]
    fn membership_key_ignores_member_order() {
        let a = vec![
            record("10.0.0.1", 0x1000, None),
            record("10.0.0.2", 0x1010, None),
        ];
        let b = vec![
            record("10.0.0.2", 0x2010, None),
            record("10.0.0.1", 0x2000, None),
        ];
        assert_eq!(membership_key(&a), membership_key(&b));
    }

    /// The idiom scan reports only what the instructions actually contain.
    #[test]
    fn idioms_report_observed_instructions_only() {
        let found = idioms(&[
            insn("401000", "CALL", "dword ptr [->MSVCRT.DLL::rand]"),
            insn("401006", "MOV", "RAX,qword ptr [RBX + RCX*0x8]"),
            insn("40100a", "JNZ", "0x401000"),
        ]);
        assert!(found.contains(&Idiom::Randomizer));
        assert!(found.contains(&Idiom::IndexedLoad));
        assert!(found.contains(&Idiom::Loop));
        assert!(!found.contains(&Idiom::Delay));
        // a forward branch and a fixed struct offset are neither a loop nor a lookup
        let none = idioms(&[
            insn("401000", "MOV", "RAX,qword ptr [RBX + 0x8]"),
            insn("401004", "JMP", "0x401100"),
        ]);
        assert!(none.is_empty());
    }

    /// A pool with no observed idiom stays at the structural floor.
    #[test]
    fn severity_rises_only_with_an_observed_idiom() {
        let mut pool = Pool {
            basis: PoolBasis::Contiguous,
            members: vec![record("10.0.0.1", 0x1000, None)],
            function: None,
            idioms: BTreeSet::new(),
        };
        assert_eq!(format!("{:?}", pool.severity().0), format!("{:?}", low().0));
        pool.idioms.insert(Idiom::Randomizer);
        assert_eq!(
            format!("{:?}", pool.severity().0),
            format!("{:?}", medium().0)
        );
    }

    /// The finding never ranks members or calls any of them a decoy.
    #[test]
    fn pool_reasoning_never_claims_a_decoy() {
        let pool = Pool {
            basis: PoolBasis::Contiguous,
            members: vec![
                record("10.0.0.1", 0x1000, None),
                record("10.0.0.2", 0x1010, None),
                record("10.0.0.3", 0x1020, None),
            ],
            function: None,
            idioms: BTreeSet::new(),
        };
        let reasoning = pool_reasoning(&pool);
        assert!(reasoning.contains("none is called a decoy"));
        assert!(!reasoning.to_ascii_lowercase().contains("likely the real"));
    }

    /// Build a URL-record pool member for the gate tests.
    ///
    /// # Arguments
    ///
    /// * `raw` - The URL as it appears in the binary
    /// * `string_addr` - Where the literal is stored
    /// * `sites` - The code sites referencing it
    fn url_record(raw: &str, string_addr: u64, sites: Vec<u64>) -> IocRecord {
        // parse the raw text so has_fragment reflects the real URL shape
        let url = crate::thorpal::detect::indicators::parse_url(raw).expect("URL parses");
        IocRecord {
            ioc: NetIoc::Url(url),
            string_addr,
            sites,
            function: None,
        }
    }

    /// Build a request-fabric pool member for the gate tests.
    ///
    /// # Arguments
    ///
    /// * `artifact` - Which piece of the fabric the literal is
    /// * `raw` - The literal as it appears in the binary
    /// * `string_addr` - Where the literal is stored
    /// * `sites` - The code sites referencing it
    /// * `function` - The function it is attributed to
    fn fabric_record(
        artifact: crate::thorpal::detect::indicators::HttpArtifact,
        raw: &str,
        string_addr: u64,
        sites: Vec<u64>,
        function: Option<u64>,
    ) -> IocRecord {
        IocRecord {
            ioc: NetIoc::Http {
                artifact,
                raw: raw.to_string(),
            },
            string_addr,
            sites,
            function,
        }
    }

    /// Build a pool over the given members for the gate tests.
    ///
    /// # Arguments
    ///
    /// * `basis` - Why the members are grouped
    /// * `members` - The pool's members
    /// * `function` - The function referencing the pool, when one was resolved
    fn pool(basis: PoolBasis, members: Vec<IocRecord>, function: Option<u64>) -> Pool {
        Pool {
            basis,
            members,
            function,
            idioms: BTreeSet::new(),
        }
    }

    /// Fragment URLs and runtime-owned literals are never pool members.
    #[test]
    fn poolable_member_excludes_fragments_and_runtime_literals() {
        // the getrandom documentation link groups nothing
        let doc = url_record(
            "https://docs.rs/getrandom#nodejs-es-module-support",
            0x1000,
            Vec::new(),
        );
        assert!(!poolable_member(&doc, None));
        // a literal owned by a recognized runtime function ships with the stack
        let owned = record("10.0.0.1", 0x2000, Some(0x40_1000));
        assert!(!poolable_member(&owned, Some("net/http.(*Client).Do")));
        // an ordinary program-owned member stays eligible
        assert!(poolable_member(&owned, Some("FUN_00401000")));
    }

    /// Substance needs a code xref or a medium-grade class.
    #[test]
    fn member_substance_needs_an_xref_or_a_weighty_class() {
        // a bare, unreferenced IP is exactly the version-number shape
        assert!(!member_substance(&record("10.0.0.1", 0x1000, None)));
        // a code xref makes any class substantial
        let mut referenced = record("10.0.0.1", 0x1000, None);
        referenced.sites.push(0x40_1000);
        assert!(member_substance(&referenced));
        // a URL-class member carries its own weight even unreferenced
        assert!(member_substance(&url_record(
            "https://c2.example.com/panel",
            0x1000,
            Vec::new()
        )));
    }

    /// Defined strings span their decoded byte length; malformed addresses drop out.
    #[test]
    fn defined_string_ranges_span_decoded_bytes() {
        let strings = vec![
            StringData {
                address: "4f62b0".to_string(),
                value: "x".repeat(0x200),
                length: 0x200,
                encoding: None,
                references: Vec::new(),
            },
            StringData {
                address: "not-an-address".to_string(),
                value: "bad".to_string(),
                length: 3,
                encoding: None,
                references: Vec::new(),
            },
        ];
        assert_eq!(
            defined_string_ranges(&strings),
            vec![(0x4f_62b0, 0x4f_64b0)]
        );
    }

    /// Carve offsets inside one ransom-note literal never report as a pool.
    #[test]
    fn pools_inside_one_defined_string_are_skipped() {
        // three "members" carved at offsets inside the one literal at 0x4f62b0
        let note = pool(
            PoolBasis::SharedFunction,
            vec![
                url_record(
                    "https://restore-a.example.com/help",
                    0x4f_62b0,
                    vec![0x40_1000],
                ),
                url_record("https://restore-b.example.com/help", 0x4f_62f0, Vec::new()),
                url_record("https://restore-c.example.com/help", 0x4f_6330, Vec::new()),
            ],
            Some(0x40_0f00),
        );
        // the whole ransom note is one defined string spanning every member
        let ranges = vec![(0x4f_62b0, 0x4f_62b0 + 0x200)];
        assert!(!pool_substantiated(&note, &HashSet::new(), &ranges, false));
        // the same members strung across separate literals keep their pool
        assert!(pool_substantiated(&note, &HashSet::new(), &[], false));
    }

    /// A contiguous table nothing references and nothing owns is not reported.
    #[test]
    fn contiguous_pools_need_a_reference_or_an_owner() {
        // three URL members with no xrefs: substantial by class, but nothing was
        // ever seen to reach the table
        let members = vec![
            url_record("https://a.example.com/gate", 0x1000, Vec::new()),
            url_record("https://b.example.com/gate", 0x1040, Vec::new()),
            url_record("https://c.example.com/gate", 0x1080, Vec::new()),
        ];
        let unreferenced = pool(PoolBasis::Contiguous, members.clone(), None);
        assert!(!pool_substantiated(
            &unreferenced,
            &HashSet::new(),
            &[],
            false
        ));
        // the same table attributed to a program-owned function is reported
        let owned = pool(PoolBasis::Contiguous, members.clone(), Some(0x40_1000));
        assert!(pool_substantiated(&owned, &HashSet::new(), &[], false));
        // ...while a runtime owner is not an owner for this purpose
        let runtime = pool(PoolBasis::Contiguous, members, Some(0x40_1000));
        assert!(!pool_substantiated(&runtime, &HashSet::new(), &[], true));
    }

    /// Junk fabric pools are suppressed; the ironstorm-shaped confirmed pool passes.
    #[test]
    fn fabric_pools_need_a_networking_function() {
        use crate::thorpal::detect::indicators::HttpArtifact;
        // the macOS framework paths pooled as 'URI Path @ contiguous data region':
        // weak class, no xrefs, no owner — no substance at all
        let dylib = pool(
            PoolBasis::Contiguous,
            vec![
                fabric_record(
                    HttpArtifact::UriPath,
                    "/System/Library/Frameworks/IOKit.framework/Versions/A/IOKit",
                    0x1000,
                    Vec::new(),
                    None,
                ),
                fabric_record(
                    HttpArtifact::UriPath,
                    "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation",
                    0x1080,
                    Vec::new(),
                    None,
                ),
                fabric_record(
                    HttpArtifact::UriPath,
                    "/System/Library/Frameworks/Security.framework/Security",
                    0x1100,
                    Vec::new(),
                    None,
                ),
            ],
            None,
        );
        assert!(!pool_substantiated(&dylib, &HashSet::new(), &[], false));
        // OpenSSL's asn_mime literals under an unnamed library function: referenced,
        // but exclusively request fabric with no networking in the function
        let asn_mime = pool(
            PoolBasis::SharedFunction,
            vec![
                fabric_record(
                    HttpArtifact::HeaderTemplate,
                    "Content-Type: %smime;",
                    0x2000,
                    vec![0x50_1000],
                    Some(0x50_0f00),
                ),
                fabric_record(
                    HttpArtifact::ContentType,
                    "multipart/signed",
                    0x2040,
                    vec![0x50_1010],
                    Some(0x50_0f00),
                ),
                fabric_record(
                    HttpArtifact::ContentType,
                    "application/pkcs7-mime",
                    0x2080,
                    vec![0x50_1020],
                    Some(0x50_0f00),
                ),
            ],
            Some(0x50_0f00),
        );
        assert!(!pool_substantiated(&asn_mime, &HashSet::new(), &[], false));
        // the ironstorm shape: the same three-member fabric table (one low-grade
        // Content-Type member kept) selected by the function carrying WinINet hits
        let wininet: HashSet<u64> = [0x50_0f00].into_iter().collect();
        assert!(pool_substantiated(&asn_mime, &wininet, &[], false));
    }
}
