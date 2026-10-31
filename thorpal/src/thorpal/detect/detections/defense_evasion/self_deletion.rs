//! Self-deletion / Indicator Removal: File Deletion (MITRE ATT&CK **T1070.004**).
//!
//! Detects malware that erases itself after running so an analyst cannot recover
//! the sample from an infected host. Two tells:
//!
//! * **Overwrite-then-delete / delete command lines** — the anti-forensics idiom
//!   of zeroing or randomizing the file's bytes *before* unlinking it (Windows
//!   `fsutil file setZeroData … & Del /f /q <self>`, Linux `dd if=/dev/urandom
//!   of=<self> … && rm <self>`) and the bare delete one-liners (`del /f /q`,
//!   `rm -f`). Found via defined-string artifacts; a string matching both
//!   catalogs is emitted once (the overwrite reasoning wins), and a matched
//!   string whose recorded references never load its base address is weakened
//!   as a constant-propagation artifact.
//! * **Self-delete APIs, gated on co-evidence** — the catalogued APIs are
//!   ubiquitous dual-use imports, so presence alone never claims T1070.004. An
//!   own-path lookup (`GetModuleFileName`) seeds per function only when that
//!   function also holds a deletion sink (a `DeleteFile`/`MoveFileEx` call or a
//!   delete/overwrite command-string reference); unpaired lookups collapse into
//!   one weak file-level line (or nothing, when the binary imports neither a
//!   deletion nor an exec sink). An exec sink (`ShellExecute`,
//!   `ShellExecuteEx`, `WinExec`, `system`) seeds only beside a same-function
//!   own-path lookup or a delete-command string somewhere in the binary, with
//!   the own-path-elsewhere residue folded into one weak line and bare calls
//!   dropped — a bare exec API is not deletion evidence. `MoveFileEx`'s
//!   `MOVEFILE_DELAY_UNTIL_REBOOT` flag is recovered per call site by the
//!   shared validation pass (`crate::thorpal::detect::validate`), which
//!   confirms or refutes the delayed-delete claim.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ghidra_cli::models::{Instruction, StringData};

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr, parse_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, MatchOptions, api_call_hits,
    low, medium, needle_matches, normalize, string_artifact_hits,
};
use crate::thorpal::detect::op_str;

use super::T1070_004;

/// The Self-deletion / Indicator Removal detection (ATT&CK T1070.004).
pub struct SelfDeletion;

/// Overwrite-before-delete artifact strings (the strong anti-forensics tell).
const OVERWRITE_STRINGS: &[&str] = &["setzerodata", "dd if=/dev/urandom", "shred -"];

/// Self-delete command-line artifact strings (shell one-liners that unlink self).
const DELETE_CMD_STRINGS: &[&str] = &["del /f", "/f /q", "&& rm ", "rm -f ", "rm -rf "];

/// The analyst-facing reasoning for an overwrite-before-delete command string.
const OVERWRITE_REASONING: &str = "Contains an overwrite-before-delete artifact (fsutil setZeroData / dd if=/dev/urandom / \
     shred). Malware zeroes or randomizes its own bytes before unlinking so the sample cannot \
     be carved back from disk.";

/// The analyst-facing reasoning for a self-delete shell one-liner string.
const DELETE_CMD_REASONING: &str = "Contains a file-deletion shell command (Windows 'del /f /q' or Linux 'rm -f'); malware \
     runs these against its own path to remove the sample after execution.";

/// The normalized catalog key of the own-path lookup API.
const GMF_API: &str = "GetModuleFileName";

/// The deletion-sink APIs whose same-function call pairs an own-path lookup.
const DELETION_SINK_APIS: &[&str] = &["DeleteFile", "MoveFileEx"];

/// The exec-sink APIs a shell-mediated self-delete launches its one-liner with.
const EXEC_SINK_APIS: &[&str] = &["ShellExecute", "ShellExecuteEx", "WinExec", "system"];

/// The max addresses listed in a combined file-level line's content.
const MAX_COMBINED_SITES: usize = 6;

/// The fewest hex digits an address needle must have for the direct-reference
/// check to be distinctive; below this the check is treated as indeterminate.
const MIN_ADDR_NEEDLE: usize = 4;

/// APIs used to delete the sample itself or launch a self-delete command.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "MoveFileEx",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls MoveFileEx; with dwFlags containing MOVEFILE_DELAY_UNTIL_REBOOT (0x4) \
                    and a NULL destination this schedules a file for reboot deletion — a \
                    self-removal idiom, though updaters renaming with MOVEFILE_REPLACE_EXISTING \
                    and C2-driven uninstalls of other files use the same call, so the recovered \
                    flags and target decide the claim.",
    },
    ApiSpec {
        key: "DeleteFile",
        always_noise: false,
        severity: low(),
        reasoning: "Calls DeleteFile; file deletion is self-removal only when the deleted target \
                    is the binary's own path (e.g. one recovered via GetModuleFileName). Deleting \
                    temp output files, dropper artifacts, or a self-update's renamed '.old' \
                    previous version are the common other shapes, so the target decides the \
                    claim.",
    },
    ApiSpec {
        key: "GetModuleFileName",
        always_noise: false,
        severity: low(),
        reasoning: "Calls GetModuleFileName — an own-path lookup; self-deletion only if the \
                    recovered path is fed to a deletion sink (DeleteFile/MoveFileEx or a shell \
                    delete command). The lookup alone is dual-use: self-updaters, installers, \
                    config/log path building, and process-hollowing loaders all resolve their \
                    own path.",
    },
    ApiSpec {
        key: "ShellExecute",
        always_noise: false,
        severity: low(),
        reasoning: "Calls ShellExecute; the classic self-delete launches a detached shell \
                    one-liner ('cmd /c ping -n 5 > NUL & del <self>') through it after exit. On \
                    its own the call only proves program launching, so it seeds here only beside \
                    an own-path lookup in the same function or a delete-command string in the \
                    binary.",
    },
    ApiSpec {
        key: "ShellExecuteEx",
        always_noise: false,
        severity: low(),
        reasoning: "Calls ShellExecuteEx; used by the same shell-mediated self-delete idiom as \
                    ShellExecute (spawn the delete one-liner, exit before it runs). On its own \
                    the call only proves program launching, so it seeds here only beside an \
                    own-path lookup in the same function or a delete-command string in the \
                    binary.",
    },
    ApiSpec {
        key: "WinExec",
        always_noise: false,
        severity: low(),
        reasoning: "Calls WinExec; a compact way to run a cmd self-delete one-liner (e.g. \
                    'cmd /c ... & del <self>'). On its own the call only proves program \
                    launching, so it seeds here only beside an own-path lookup in the same \
                    function or a delete-command string in the binary.",
    },
    ApiSpec {
        key: "system",
        always_noise: false,
        severity: low(),
        reasoning: "Calls system(); on Linux the self-delete one-liner idiom is 'sleep 5 && rm \
                    <self>' (or an overwrite-then-rm chain). On its own the call only proves \
                    command execution, so it seeds here only beside an own-path lookup in the \
                    same function or a delete-command string in the binary.",
    },
];

/// Whether any import's normalized name matches one of the given catalog keys.
///
/// # Arguments
///
/// * `ctx` - The analysis context carrying the import table
/// * `keys` - The normalized catalog keys to test membership against
fn imports_any(ctx: &AnalysisContext, keys: &[&str]) -> bool {
    // normalize each import the way the catalog matcher does and test membership
    ctx.imports
        .iter()
        .any(|import| keys.contains(&normalize(&import.name).as_str()))
}

/// The base addresses of the defined strings a catalog pass matches, in order.
///
/// Mirrors the string pass's own matching (default bare-substring options plus
/// the bare-substring vetoes) so the result lines up one-to-one with the hits
/// the pass emitted; an unparseable string address records as 0, which the
/// short-needle guard later treats as indeterminate.
///
/// # Arguments
///
/// * `strings` - The program's defined strings
/// * `needles` - The catalog's lower-case needles
/// * `excludes` - The catalog's lower-case veto terms
fn matched_string_addrs(strings: &[StringData], needles: &[&str], excludes: &[&str]) -> Vec<u64> {
    // collect each matching string's base address in definition order
    let mut addrs = Vec::new();
    for string in strings {
        // lower-case once for the case-insensitive needle/veto tests
        let hay = string.value.to_ascii_lowercase();
        // same needle test as the pass (default bare-substring options)
        if !needles
            .iter()
            .any(|needle| needle_matches(&hay, needle, MatchOptions::default()))
        {
            continue;
        }
        // same veto test as the pass
        if excludes.iter().any(|exclude| hay.contains(exclude)) {
            continue;
        }
        // record the string's base address for the direct-reference check
        addrs.push(parse_addr(&string.address).unwrap_or(0));
    }
    addrs
}

/// Whether rendered operand text encodes `base` directly.
///
/// Ghidra renders a direct use of a defined string as a `0x…` immediate or an
/// `s_…_<addr>` label, both of which carry the address's bare hex digits, so a
/// substring test on the unpadded lowercase hex covers every rendering. An
/// address too short to be distinctive counts as encoded (fail open: the check
/// may only ever weaken proven-computed references, never rescue junk).
///
/// # Arguments
///
/// * `ops` - The instruction's rendered operand text
/// * `base` - The string's base address
fn ops_encode_address(ops: &str, base: u64) -> bool {
    // render the address as bare lowercase hex (contained by padded forms too)
    let needle = format!("{base:x}");
    // a tiny needle matches everywhere and proves nothing: treat as encoded
    if needle.len() < MIN_ADDR_NEEDLE {
        return true;
    }
    // one lowercase pass over the operands covers Ghidra's case variants
    ops.to_ascii_lowercase().contains(&needle)
}

/// Judge whether the referencing instruction at `site` encodes `base` directly.
///
/// Returns `None` when the site cannot be located in the listing (an
/// indeterminate verdict that blocks the weaken), else whether the operands
/// encode the string's base address.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `site` - The referencing instruction's address
/// * `base` - The referenced string's base address
fn site_encodes_base(insns: &[Instruction], site: u64, base: u64) -> Option<bool> {
    // locate the referencing instruction in the containing function's listing
    let insn = insns
        .iter()
        .find(|insn| parse_addr(&insn.address) == Some(site))?;
    // a direct use renders the string's base address in the operands
    Some(ops_encode_address(&op_str(insn), base))
}

/// Weaken a string hit whose every located reference is computed off another
/// base, appending the computed-references marker.
///
/// One directly-encoding site proves a real use and an unlocatable site proves
/// nothing, so either leaves the hit at strength; only a fully-judged,
/// all-computed site set (the NUL-overrun copy-loop artifact shape) weakens —
/// never drops — the hit.
///
/// # Arguments
///
/// * `hit` - The string hit to adjust in place
/// * `verdicts` - One [`site_encodes_base`] verdict per referencing site
fn apply_reference_verdicts(hit: &mut DetectionHit, verdicts: &[Option<bool>]) {
    // any directly-encoding site proves a real use: leave the hit at strength
    if verdicts.contains(&Some(true)) {
        return;
    }
    // an unlocatable (or absent) site is indeterminate and blocks the weaken
    if verdicts.is_empty() || verdicts.iter().any(Option::is_none) {
        return;
    }
    // every reference is computed off another base: weaken and say why
    hit.weaken();
    hit.content.push_str(" (indirect/computed references only)");
}

/// Weaken command-string hits whose recorded references never load the string's
/// base address (constant-propagation / NUL-overrun artifacts), in place.
///
/// Re-derives the matched strings' base addresses with the same catalog
/// matching the pass used, pairs them with the hits one-to-one, and judges each
/// referenced hit's sites against the cached disassembly. Any pairing doubt (a
/// count mismatch) or unlocatable site degrades to a no-op, so the check can
/// only ever weaken proven-computed references.
///
/// # Arguments
///
/// * `ctx` - The analysis context (strings + cached disassembly)
/// * `needles` - The catalog needles the pass matched with
/// * `excludes` - The catalog vetoes the pass matched with
/// * `hits` - The pass's hits, adjusted in place
///
/// # Errors
///
/// Returns an error if a bridge disassembly query fails.
async fn verify_direct_references(
    ctx: &AnalysisContext,
    needles: &[&str],
    excludes: &[&str],
    hits: &mut [DetectionHit],
) -> Result<()> {
    // nothing matched means nothing to verify
    if hits.is_empty() {
        return Ok(());
    }
    // re-derive the matched strings' base addresses in catalog-match order
    let bases = matched_string_addrs(&ctx.strings, needles, excludes);
    // the pass emits at most one hit per matched string, in string order; any
    // count mismatch makes the pairing unsafe, so skip the check (fail open)
    if bases.len() != hits.len() {
        return Ok(());
    }
    // judge each referenced string's sites (unreferenced hits carry no sites)
    for (base, hit) in bases.into_iter().zip(hits.iter_mut()) {
        if hit.sites.is_empty() {
            continue;
        }
        // disassemble each referencing site's function (cached) and judge it
        let mut verdicts = Vec::with_capacity(hit.sites.len());
        for site in &hit.sites {
            let listing = ctx.disasm_containing(*site).await?;
            verdicts.push(
                listing
                    .as_ref()
                    .and_then(|(_, insns)| site_encodes_base(insns, *site, base)),
            );
        }
        // weaken-and-annotate only when every located site is computed-only
        apply_reference_verdicts(hit, &verdicts);
    }
    Ok(())
}

/// Map each deletion-sink-holding function to the label naming its sink.
///
/// A function holds a sink when it calls `DeleteFile`/`MoveFileEx` or
/// references a delete/overwrite command string; the label feeds the paired
/// own-path lookup's `— paired with <sink> in this function` note, with the
/// API name preferred over the string descriptions when both are present.
///
/// # Arguments
///
/// * `api_hits` - The API pass's per-function hits
/// * `overwrite_hits` - The overwrite-string pass's hits
/// * `delete_hits` - The delete-string pass's hits
fn sink_label_map(
    api_hits: &[DetectionHit],
    overwrite_hits: &[DetectionHit],
    delete_hits: &[DetectionHit],
) -> BTreeMap<u64, &'static str> {
    // deletion-sink API calls name the pairing most precisely, so they win
    let mut sinks: BTreeMap<u64, &'static str> = BTreeMap::new();
    for hit in api_hits {
        if let (Some(api), Some(entry)) = (hit.api, hit.function)
            && DELETION_SINK_APIS.contains(&api)
        {
            sinks.entry(entry).or_insert(api);
        }
    }
    // delete-command strings are sinks too (the shell-mediated idiom)
    for hit in delete_hits {
        if let Some(entry) = hit.function {
            sinks.entry(entry).or_insert("a delete-command string");
        }
    }
    // an overwrite-before-delete string implies the delete that follows it
    for hit in overwrite_hits {
        if let Some(entry) = hit.function {
            sinks
                .entry(entry)
                .or_insert("an overwrite-before-delete command string");
        }
    }
    sinks
}

/// Every address backing the given hits, ascending and de-duplicated.
///
/// # Arguments
///
/// * `hits` - The hits whose sites to gather
fn flatten_sites(hits: &[DetectionHit]) -> Vec<u64> {
    // a hit with no site list backs just its primary address
    let mut sites: Vec<u64> = hits
        .iter()
        .flat_map(|hit| {
            if hit.sites.is_empty() {
                vec![hit.address]
            } else {
                hit.sites.clone()
            }
        })
        .collect();
    // sort + dedupe so the rendered list is stable and free of repeats
    sites.sort_unstable();
    sites.dedup();
    sites
}

/// Count the distinct code loci the given hits spread across.
///
/// # Arguments
///
/// * `hits` - The hits whose containing functions to count
fn locus_count(hits: &[DetectionHit]) -> usize {
    // count distinct containing functions, plus each unattributed hit on its own
    let functions: BTreeSet<u64> = hits.iter().filter_map(|hit| hit.function).collect();
    let unattributed = hits.iter().filter(|hit| hit.function.is_none()).count();
    functions.len() + unattributed
}

/// Render a combined line's addresses, naming the first few and counting the rest.
///
/// # Arguments
///
/// * `sites` - The addresses to render, ascending
fn render_addrs(sites: &[u64]) -> String {
    // list a spot-checkable handful of addresses
    let listed = sites
        .iter()
        .take(MAX_COMBINED_SITES)
        .map(|site| fmt_addr(*site))
        .collect::<Vec<_>>()
        .join(", ");
    // no silent truncation: say how many sites were left out
    match sites
        .len()
        .checked_sub(MAX_COMBINED_SITES)
        .filter(|extra| *extra > 0)
    {
        Some(extra) => format!("{listed} (+{extra} more)"),
        None => listed,
    }
}

/// The bare pluralizing suffix for a rendered count.
///
/// # Arguments
///
/// * `count` - The count being rendered
fn plural(count: usize) -> &'static str {
    // only an exact one renders without the suffix
    if count == 1 { "" } else { "s" }
}

/// Collapse the unpaired own-path lookups into their one combined file-level
/// line, or nothing when the binary could not feed a lookup to any sink.
///
/// With a deletion sink imported the line notes that no function pairs the
/// two; with exec sinks only it notes that a shell-mediated self-delete needs
/// no deletion import; with neither imported the lookups are dropped outright
/// (the path has nowhere self-deleting to go).
///
/// # Arguments
///
/// * `unpaired` - The own-path lookup hits no sink-holding function claimed
/// * `imports_deletion_sink` - Whether the binary imports `DeleteFile`/`MoveFileEx`
/// * `imports_exec_sink` - Whether the binary imports any exec sink
fn collapse_unpaired_gmf(
    unpaired: &[DetectionHit],
    imports_deletion_sink: bool,
    imports_exec_sink: bool,
) -> Option<DetectionHit> {
    // nothing unpaired means nothing to collapse
    let first = unpaired.first()?;
    // with neither a deletion nor an exec sink imported the lookup can feed no
    // self-delete at all (the rdpwrap shape): emit nothing
    if !imports_deletion_sink && !imports_exec_sink {
        return None;
    }
    // gather every lookup site and the loci they spread across
    let sites = flatten_sites(unpaired);
    let loci = locus_count(unpaired);
    // word the one weak line per the binary's sink tier
    let (content, reasoning) = if imports_deletion_sink {
        (
            format!(
                "GetModuleFileName — {} own-path lookup{} across {} function{}, none with a \
                 same-function deletion sink @ {}",
                sites.len(),
                plural(sites.len()),
                loci,
                plural(loci),
                render_addrs(&sites)
            ),
            "The binary imports a deletion sink (DeleteFile/MoveFileEx), but no function pairs \
             its own-path lookup with one: own-path lookup is self-deletion evidence only if the \
             recovered path is fed to a deletion sink, and unpaired lookups are the dual-use \
             norm (self-update, config/log path building, loaders). All unpaired lookups are \
             folded into this one weak file-level line."
                .to_string(),
        )
    } else {
        (
            format!(
                "GetModuleFileName — {} own-path lookup{} across {} function{}; no deletion API \
                 imported, exec sinks only @ {}",
                sites.len(),
                plural(sites.len()),
                loci,
                plural(loci),
                render_addrs(&sites)
            ),
            "The binary resolves its own module path but imports no DeleteFile/MoveFileEx; only \
             exec sinks (ShellExecute/ShellExecuteEx/WinExec/system) are present. A \
             shell-mediated self-delete — an exec sink launching a runtime-built delete \
             one-liner fed the own path — needs no deletion import, so the lookups are kept as \
             this single weak file-level line rather than per-function findings."
                .to_string(),
        )
    };
    // one weak file-level hit carrying every lookup site for the markup pass
    Some(
        DetectionHit::new(first.technique.clone(), 0, None, low(), content, reasoning).sites(sites),
    )
}

/// Collapse the co-evidence-less exec-sink calls (own-path lookup elsewhere in
/// the binary) into their one combined file-level line.
///
/// # Arguments
///
/// * `unpaired` - The exec-sink hits kept only by the own-path-elsewhere fallback
fn collapse_unpaired_exec(unpaired: &[DetectionHit]) -> Option<DetectionHit> {
    // nothing to fold means no line
    let first = unpaired.first()?;
    // name the distinct exec APIs involved, in first-appearance order
    let mut apis: Vec<&'static str> = Vec::new();
    for hit in unpaired {
        if let Some(api) = hit.api
            && !apis.contains(&api)
        {
            apis.push(api);
        }
    }
    // gather every call site and the loci they spread across
    let sites = flatten_sites(unpaired);
    let loci = locus_count(unpaired);
    // one weak line naming the sinks and why they were not dropped outright
    let content = format!(
        "{} — {} exec-sink call{} across {} function{}, own-path lookup elsewhere in the binary \
         @ {}",
        apis.join("/"),
        sites.len(),
        plural(sites.len()),
        loci,
        plural(loci),
        render_addrs(&sites)
    );
    let reasoning = "These exec-capable calls have no same-function GetModuleFileName pairing \
                     and the binary embeds no delete-command string, but it does look up its own \
                     path elsewhere, which keeps a stack-built shell self-delete (e.g. a \
                     runtime-assembled '%ComSpec% /c del <self>' one-liner) possible. They are \
                     folded into this one weak file-level line instead of per-function T1070.004 \
                     findings."
        .to_string();
    // one weak file-level hit carrying every call site for the markup pass
    Some(
        DetectionHit::new(first.technique.clone(), 0, None, low(), content, reasoning).sites(sites),
    )
}

/// Gate the API pass's hits on co-evidence: pair own-path lookups with
/// deletion sinks, require delete-string or own-path co-evidence for exec
/// sinks, and collapse what fails each gate.
///
/// Deletion-sink hits, the import-census line, and any other non-catalog hit
/// pass through unchanged.
///
/// # Arguments
///
/// * `api_hits` - The API pass's hits, consumed and re-emitted gated
/// * `overwrite_hits` - The overwrite-string pass's hits
/// * `delete_hits` - The delete-string pass's hits
/// * `imports_deletion_sink` - Whether the binary imports `DeleteFile`/`MoveFileEx`
/// * `imports_exec_sink` - Whether the binary imports any exec sink
fn gate_api_hits(
    api_hits: Vec<DetectionHit>,
    overwrite_hits: &[DetectionHit],
    delete_hits: &[DetectionHit],
    imports_deletion_sink: bool,
    imports_exec_sink: bool,
) -> Vec<DetectionHit> {
    // the functions whose own-path lookup can pair an exec sink (gate b)
    let gmf_functions: BTreeSet<u64> = api_hits
        .iter()
        .filter(|hit| hit.api == Some(GMF_API))
        .filter_map(|hit| hit.function)
        .collect();
    // whether any own-path lookup call exists at all (the weak exec fallback)
    let gmf_anywhere = api_hits.iter().any(|hit| hit.api == Some(GMF_API));
    // the deletion-sink functions (API call or command string) with labels
    let sinks = sink_label_map(&api_hits, overwrite_hits, delete_hits);
    // whether any delete/overwrite command string exists in the binary (gate a)
    let strings_exist = !overwrite_hits.is_empty() || !delete_hits.is_empty();
    // judge every API hit by its catalog family
    let mut kept = Vec::new();
    let mut unpaired_gmf = Vec::new();
    let mut unpaired_exec = Vec::new();
    for mut hit in api_hits {
        match hit.api {
            // own-path lookups: keep only same-function-paired ones, annotated
            Some(api) if api == GMF_API => {
                match hit.function.and_then(|entry| sinks.get(&entry).copied()) {
                    Some(label) => {
                        // the lookup shares a function with a deletion sink
                        hit.content.push_str(" — paired with ");
                        hit.content.push_str(label);
                        hit.content.push_str(" in this function");
                        kept.push(hit);
                    }
                    None => unpaired_gmf.push(hit),
                }
            }
            // exec sinks: require co-evidence before claiming T1070.004
            Some(api) if EXEC_SINK_APIS.contains(&api) => {
                if hit
                    .function
                    .is_some_and(|entry| gmf_functions.contains(&entry))
                {
                    // gate (b): the calling function also looks up its own path
                    kept.push(hit);
                } else if strings_exist {
                    // gate (a): binary-level delete-string evidence, noted as such
                    hit.content
                        .push_str(" — delete command string present elsewhere in the binary");
                    kept.push(hit);
                } else if gmf_anywhere {
                    // weak fallback: an own-path lookup elsewhere keeps a
                    // stack-built delete one-liner possible, at combined grade only
                    unpaired_exec.push(hit);
                }
                // with no co-evidence at all the call is dropped: a bare exec
                // API is not deletion evidence
            }
            // deletion sinks, the census line, and anything else pass unchanged
            _ => kept.push(hit),
        }
    }
    // collapse the unpaired own-path lookups per the binary's sink imports
    if let Some(hit) =
        collapse_unpaired_gmf(&unpaired_gmf, imports_deletion_sink, imports_exec_sink)
    {
        kept.push(hit);
    }
    // collapse the co-evidence-less exec sinks into their one weak line
    if let Some(hit) = collapse_unpaired_exec(&unpaired_exec) {
        kept.push(hit);
    }
    kept
}

#[async_trait::async_trait]
impl Detection for SelfDeletion {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "self_deletion"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1070_004
    }

    /// Run the string passes, then gate the API pass's hits on co-evidence.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The shared analysis context
    #[tracing::instrument(name = "SelfDeletion::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // 1) overwrite-before-delete command fragments (strong anti-forensics tell)
        let mut overwrite_hits = string_artifact_hits(
            ctx,
            T1070_004,
            OVERWRITE_STRINGS,
            medium(),
            OVERWRITE_REASONING,
            &[],
        )
        .await?;
        // 2) self-delete shell one-liners; strings the overwrite catalog already
        //    matched are vetoed here so a dual-needle string emits exactly once
        //    (the overwrite reasoning wins)
        let mut delete_hits = string_artifact_hits(
            ctx,
            T1070_004,
            DELETE_CMD_STRINGS,
            medium(),
            DELETE_CMD_REASONING,
            OVERWRITE_STRINGS,
        )
        .await?;
        // 3) weaken command strings whose recorded references never load their
        //    base address (constant-propagation / NUL-overrun artifacts)
        verify_direct_references(ctx, OVERWRITE_STRINGS, &[], &mut overwrite_hits).await?;
        verify_direct_references(ctx, DELETE_CMD_STRINGS, OVERWRITE_STRINGS, &mut delete_hits)
            .await?;
        // 4) self-delete / delayed-delete APIs, then the co-evidence gate: pair
        //    own-path lookups with sinks, gate exec sinks, collapse the rest
        let api_hits = api_call_hits(ctx, T1070_004, APIS, EvidenceKind::ContextOnly).await?;
        let gated = gate_api_hits(
            api_hits,
            &overwrite_hits,
            &delete_hits,
            imports_any(ctx, DELETION_SINK_APIS),
            imports_any(ctx, EXEC_SINK_APIS),
        );
        // merge the three sub-passes' surviving hits
        let mut hits = overwrite_hits;
        hits.extend(delete_hits);
        hits.extend(gated);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::thorpal::detect::confidence::Confidence;

    /// Build an API-catalog hit shaped like `build_api_hit`'s per-function output.
    ///
    /// # Arguments
    ///
    /// * `api` - The normalized catalog API name
    /// * `function` - The containing function entry, when known
    /// * `site` - The call-site address
    fn api_hit(api: &'static str, function: Option<u64>, site: u64) -> DetectionHit {
        DetectionHit::new(
            T1070_004,
            site,
            function,
            low(),
            format!("{api} call @ {}", fmt_addr(site)),
            "catalog reasoning",
        )
        .kind(EvidenceKind::ContextOnly)
        .api(api)
    }

    /// Build a referenced command-string hit as the string pass emits one.
    ///
    /// # Arguments
    ///
    /// * `function` - The containing function entry, when known
    /// * `site` - The referencing site address
    /// * `shown` - The rendered string value
    fn string_hit(function: Option<u64>, site: u64, shown: &str) -> DetectionHit {
        DetectionHit::new(
            T1070_004,
            site,
            function,
            medium(),
            format!("string {shown:?} referenced @ {}", fmt_addr(site)),
            "string reasoning",
        )
        .sites(vec![site])
    }

    /// Build a synthetic instruction for the reference-check fixtures.
    ///
    /// # Arguments
    ///
    /// * `address` - The instruction address
    /// * `mnemonic` - The instruction mnemonic
    /// * `operands` - The rendered operand list
    fn insn(address: u64, mnemonic: &str, operands: &[&str]) -> Instruction {
        Instruction {
            // the bridge emits instruction addresses as bare, unprefixed hex
            address: format!("{address:x}"),
            mnemonic: mnemonic.to_owned(),
            operands: operands.iter().map(|op| (*op).to_owned()).collect(),
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// Build a defined-string fixture for the catalog-matching helpers.
    ///
    /// # Arguments
    ///
    /// * `address` - The string's base address
    /// * `value` - The string's decoded value
    fn string_data(address: u64, value: &str) -> StringData {
        StringData {
            address: fmt_addr(address),
            value: value.to_owned(),
            length: value.len(),
            encoding: None,
            references: Vec::new(),
        }
    }

    /// The `MoriMuddyWater` self-update shape: a `GetModuleFileName` paired with a
    /// same-function `MoveFileEx` survives at low with the pairing note.
    #[test]
    fn gmf_paired_with_movefileex_survives_low_with_note() {
        // FUN_180028f30 looks up its own path and schedules the .old delete
        let hits = vec![
            api_hit(GMF_API, Some(0x1_8002_8f30), 0x1_8002_8f60),
            api_hit("MoveFileEx", Some(0x1_8002_8f30), 0x1_8002_8f90),
        ];
        let gated = gate_api_hits(hits, &[], &[], true, false);
        // both hits survive: the sink unchanged, the lookup annotated
        assert_eq!(gated.len(), 2);
        let gmf = gated
            .iter()
            .find(|hit| hit.api == Some(GMF_API))
            .expect("paired lookup kept");
        assert!(
            gmf.content
                .ends_with("— paired with MoveFileEx in this function"),
            "{}",
            gmf.content
        );
        // the pairing keeps the lookup at its catalog low(), never stronger
        assert!(matches!(gmf.confidence, Confidence::Untrusted));
        assert_eq!(gmf.suspicion, 15);
    }

    /// A delete-command string xref pairs a same-function `GetModuleFileName`
    /// (the confirmed del-one-liner survival shape).
    #[test]
    fn gmf_paired_with_delete_string_survives() {
        // the '/c timeout /t 5 & del /f /q' reference and the lookup co-locate
        let delete_hits = vec![string_hit(
            Some(0x1_4003_b8a0),
            0x1_4003_b8de,
            "/c timeout /t 5 & del /f /q",
        )];
        let hits = vec![api_hit(GMF_API, Some(0x1_4003_b8a0), 0x1_4003_b8c0)];
        let gated = gate_api_hits(hits, &[], &delete_hits, false, true);
        // the lookup survives as a per-function finding with the string label
        assert_eq!(gated.len(), 1);
        assert!(
            gated[0]
                .content
                .ends_with("— paired with a delete-command string in this function"),
            "{}",
            gated[0].content
        );
    }

    /// The `RunPE` shape: unpaired lookups with a deletion sink
    /// imported elsewhere collapse into one combined file-level line.
    #[test]
    fn unpaired_gmf_with_deletion_import_collapses_to_one_line() {
        // GMF @0x403609 feeds CreateProcess hollowing, not a deletion sink
        let hits = vec![
            api_hit(GMF_API, Some(0x40_35d0), 0x40_3609),
            api_hit(GMF_API, Some(0x40_1000), 0x40_1100).sites(vec![0x40_1100, 0x40_1180]),
        ];
        let gated = gate_api_hits(hits, &[], &[], true, true);
        // exactly one weak file-level line survives, carrying every site
        assert_eq!(gated.len(), 1);
        let line = &gated[0];
        assert_eq!(line.address, 0);
        assert!(line.function.is_none());
        assert!(matches!(line.confidence, Confidence::Untrusted));
        assert!(
            line.content
                .contains("none with a same-function deletion sink"),
            "{}",
            line.content
        );
        assert!(line.sites.contains(&0x40_3609));
        assert!(line.sites.contains(&0x40_1180));
    }

    /// The syssettings.ini / rdpwrap shape: with neither a deletion
    /// nor an exec sink imported, own-path lookups emit nothing.
    #[test]
    fn gmf_with_no_sink_imports_emits_nothing() {
        // two config/log path-building lookups, no sink import anywhere
        let hits = vec![
            api_hit(GMF_API, Some(0x40_2000), 0x40_2040),
            api_hit(GMF_API, Some(0x40_3000), 0x40_3040),
        ];
        let gated = gate_api_hits(hits, &[], &[], false, false);
        // no self-delete capability exists, so nothing seeds
        assert!(gated.is_empty());
    }

    /// With exec sinks imported but no deletion API, unpaired lookups keep one
    /// combined weak line (shell-mediated self-delete needs no deletion import).
    #[test]
    fn gmf_with_exec_imports_only_keeps_one_combined_line() {
        // one unpaired lookup in a binary importing only exec sinks
        let hits = vec![api_hit(GMF_API, Some(0x40_2000), 0x40_2040)];
        let gated = gate_api_hits(hits, &[], &[], false, true);
        // the single weak file-level line names the exec-only tier
        assert_eq!(gated.len(), 1);
        assert!(matches!(gated[0].confidence, Confidence::Untrusted));
        assert!(
            gated[0]
                .content
                .contains("no deletion API imported, exec sinks only"),
            "{}",
            gated[0].content
        );
    }

    /// The `%ComSpec%` stack-string self-delete: the `ShellExecute` in
    /// `self_delete_via_cmd_del` keeps seeding via its same-function lookup.
    #[test]
    fn exec_sink_paired_with_same_function_gmf_survives() {
        // GMF @0x404104 and ShellExecute @0x404194 share fn 0x4040e0
        let hits = vec![
            api_hit(GMF_API, Some(0x40_40e0), 0x40_4104),
            api_hit("ShellExecute", Some(0x40_40e0), 0x40_4194),
        ];
        let gated = gate_api_hits(hits, &[], &[], false, true);
        // the exec sink survives unchanged as a per-function finding
        let exec = gated
            .iter()
            .find(|hit| hit.api == Some("ShellExecute"))
            .expect("paired exec sink kept");
        assert_eq!(exec.content, "ShellExecute call @ 0x404194");
        assert!(matches!(exec.confidence, Confidence::Untrusted));
        // the lookup itself is unpaired (no deletion sink) and folds to a line
        assert_eq!(gated.len(), 2);
        assert!(gated.iter().any(|hit| hit.address == 0));
    }

    /// A delete-command string anywhere in the binary keeps an exec sink,
    /// annotated with the binary-level note.
    #[test]
    fn exec_sink_with_delete_string_elsewhere_keeps_with_note() {
        // the WinExec caller holds no lookup; the del string lives elsewhere
        let delete_hits = vec![string_hit(
            Some(0x40_5000),
            0x40_5040,
            "/c timeout /t 5 & del /f /q",
        )];
        let hits = vec![api_hit("WinExec", Some(0x40_2100), 0x40_2158)];
        let gated = gate_api_hits(hits, &[], &delete_hits, false, true);
        // the call survives with the delete-string note appended
        assert_eq!(gated.len(), 1);
        assert!(
            gated[0]
                .content
                .ends_with("— delete command string present elsewhere in the binary"),
            "{}",
            gated[0].content
        );
    }

    /// The esxcli shape: a `system()` call with no delete string and
    /// no own-path lookup anywhere drops from this detector.
    #[test]
    fn bare_exec_sink_without_co_evidence_drops() {
        // the service-stop pipeline runner in main, nothing self-delete-shaped
        let hits = vec![api_hit("system", Some(0x41_6900), 0x41_6a0a)];
        let gated = gate_api_hits(hits, &[], &[], false, true);
        // a bare exec API is not deletion evidence
        assert!(gated.is_empty());
    }

    /// The runas-elevation shape: exec sinks whose only
    /// co-evidence is an own-path lookup elsewhere fold into one weak line.
    #[test]
    fn exec_sinks_with_gmf_elsewhere_fold_into_one_combined_line() {
        // ShellExecuteEx @0x10004207 and a lookup in an unrelated function
        let hits = vec![
            api_hit("ShellExecuteEx", Some(0x1000_4100), 0x1000_4207),
            api_hit(GMF_API, Some(0x1000_0f00), 0x1000_1000),
        ];
        let gated = gate_api_hits(hits, &[], &[], false, true);
        // both families collapse: no per-function finding survives
        assert_eq!(gated.len(), 2);
        assert!(gated.iter().all(|hit| hit.address == 0));
        let exec = gated
            .iter()
            .find(|hit| hit.content.contains("ShellExecuteEx"))
            .expect("combined exec line present");
        assert!(
            exec.content
                .contains("own-path lookup elsewhere in the binary"),
            "{}",
            exec.content
        );
        assert!(exec.sites.contains(&0x1000_4207));
    }

    /// Deletion-sink hits and the import-census line pass the gate untouched.
    #[test]
    fn census_and_deletion_sink_hits_pass_the_gate_untouched() {
        // the ironstorm dropper-artifact DeleteFile plus a census line
        let census = DetectionHit::new(
            T1070_004,
            0,
            None,
            low(),
            "2 catalog APIs imported but never referenced from code: WinExec, system",
            "census reasoning",
        );
        let hits = vec![
            api_hit("DeleteFile", Some(0x1_8000_15c0), 0x1_8000_1603),
            census,
        ];
        let gated = gate_api_hits(hits, &[], &[], true, true);
        // both pass through with their content unchanged
        assert_eq!(gated.len(), 2);
        assert!(
            gated
                .iter()
                .any(|hit| hit.content == "DeleteFile call @ 0x180001603")
        );
        assert!(gated.iter().any(|hit| hit.content.starts_with("2 catalog")));
    }

    /// The `LockBit` shape: references computed off an adjacent base
    /// (never encoding the string's own address) weaken and annotate the hit.
    #[test]
    fn computed_only_references_weaken_and_annotate() {
        // the copy loop reads words off 0x403490; the fsutil string is 0x4034e8
        let insns = vec![insn(0x40_1500, "MOV", &["CX", "word ptr [EAX + 0x403490]"])];
        let verdict = site_encodes_base(&insns, 0x40_1500, 0x40_34e8);
        assert_eq!(verdict, Some(false));
        // every located reference is computed: the hit weakens, never drops
        let mut hit = string_hit(
            Some(0x40_1400),
            0x40_1500,
            "fsutil file setzerodata offset=0 length=524288",
        );
        apply_reference_verdicts(&mut hit, &[verdict]);
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 25);
        assert!(
            hit.content
                .ends_with(" (indirect/computed references only)"),
            "{}",
            hit.content
        );
    }

    /// A reference loading the string's base directly (immediate or Ghidra
    /// label rendering) keeps the hit at full strength.
    #[test]
    fn direct_base_reference_keeps_the_hit_at_strength() {
        // an immediate encoding of the confirmed del one-liner's base address
        let insns = vec![insn(0x1_4003_b8d0, "LEA", &["RDX", "[0x14003b8de]"])];
        assert_eq!(
            site_encodes_base(&insns, 0x1_4003_b8d0, 0x1_4003_b8de),
            Some(true)
        );
        // a Ghidra label rendering carries the same bare hex digits
        assert!(ops_encode_address(
            "RDX, [s_/c_timeout_/t_5_&_del_/f_/q_14003b8de]",
            0x1_4003_b8de
        ));
        // a direct verdict leaves the hit exactly as the pass seeded it
        let mut hit = string_hit(
            Some(0x1_4003_b8a0),
            0x1_4003_b8d0,
            "/c timeout /t 5 & del /f /q",
        );
        apply_reference_verdicts(&mut hit, &[Some(true)]);
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, 40);
        assert!(!hit.content.contains("indirect/computed"));
    }

    /// An unlocatable referencing site is indeterminate and blocks the weaken.
    #[test]
    fn unlocatable_reference_sites_block_the_weaken() {
        // one computed site plus one the disassembly could not locate
        let mut hit = string_hit(Some(0x40_1400), 0x40_1500, "del /f /q");
        apply_reference_verdicts(&mut hit, &[Some(false), None]);
        // nothing was proven about every reference, so nothing moves
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert!(!hit.content.contains("indirect/computed"));
    }

    /// The dual-needle `LockBit` string is vetoed from the delete pass
    /// (one emission, overwrite reasoning wins) while a plain del one-liner stays.
    #[test]
    fn dual_catalog_string_is_vetoed_from_the_delete_pass() {
        // the LockBit command carries both an overwrite and a delete needle
        let strings = vec![
            string_data(
                0x40_34e8,
                "/c fsutil file setZeroData offset=0 length=524288 \"%s\" & Del /f /q \"%s\"",
            ),
            string_data(0x1_4003_b8de, "/c timeout /t 5 & del /f /q"),
        ];
        // the overwrite pass claims the dual-needle string
        let overwrite = matched_string_addrs(&strings, OVERWRITE_STRINGS, &[]);
        assert_eq!(overwrite, vec![0x40_34e8]);
        // the delete pass skips it and keeps only the plain del one-liner
        let delete = matched_string_addrs(&strings, DELETE_CMD_STRINGS, OVERWRITE_STRINGS);
        assert_eq!(delete, vec![0x1_4003_b8de]);
    }

    /// The `MoveFileEx` catalog reasoning claims reboot-deletion scheduling in
    /// hypothesis form, naming the benign uses, instead of asserting self-removal.
    #[test]
    fn movefileex_reasoning_claims_scheduling_not_self_removal() {
        // the catalog entry is the template every MoveFileEx hit renders
        let spec = APIS
            .iter()
            .find(|spec| spec.key == "MoveFileEx")
            .expect("MoveFileEx catalogued");
        assert!(
            spec.reasoning
                .contains("schedules a file for reboot deletion")
        );
        assert!(spec.reasoning.contains("MOVEFILE_REPLACE_EXISTING"));
        assert!(!spec.reasoning.contains("the malware itself"));
    }

    /// The `DeleteFile` catalog reasoning is hypothesis-form, naming the benign
    /// temp-cleanup / dropper-artifact / '.old' self-update shapes.
    #[test]
    fn deletefile_reasoning_is_hypothesis_form() {
        // the catalog entry is the template every DeleteFile hit renders
        let spec = APIS
            .iter()
            .find(|spec| spec.key == "DeleteFile")
            .expect("DeleteFile catalogued");
        assert!(spec.reasoning.contains("only when"));
        assert!(spec.reasoning.contains("'.old'"));
        assert!(spec.reasoning.contains("temp output files"));
    }

    /// The `GetModuleFileName` catalog reasoning is the conditional own-path
    /// form, not an asserted self-delete chain.
    #[test]
    fn gmf_reasoning_is_conditional() {
        // the catalog entry is the template every paired lookup renders
        let spec = APIS
            .iter()
            .find(|spec| spec.key == GMF_API)
            .expect("GetModuleFileName catalogued");
        assert!(spec.reasoning.contains("own-path lookup"));
        assert!(spec.reasoning.contains("self-deletion only if"));
        assert!(
            !spec
                .reasoning
                .contains("the value a self-delete routine feeds")
        );
    }
}
