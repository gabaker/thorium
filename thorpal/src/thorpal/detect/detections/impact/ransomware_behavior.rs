//! Ransomware behavior around encryption (MITRE ATT&CK **T1486**).
//!
//! Where `crypto_capability` proves a sample *can* encrypt, this flags the
//! surrounding ransomware workflow that turns crypto into impact:
//!
//! * **Target discovery** — enumerating drives (`A:`–`Z:`) and walking the
//!   filesystem to find victim files (every drive, recursing writable directories).
//! * **Rename / extension rewrite** — appending a marker extension to each
//!   encrypted file.
//! * **Ransom notes** — dropping a note with payment/recovery instructions.
//!
//! Individually these are weak; co-located with `crypto_capability` (the
//! validator's clustering bumps such groups) they are a strong ransomware signal.
//! The discovery/rename APIs are dual-use — file managers, installers, backup
//! tools, and RAT file-manager commands call the same ones — so each catalog
//! reasoning claims the capability and names the crypto/ransom-note corroboration
//! a T1486 claim needs, one function's catalog hits merge into a single
//! enumeration-surface finding, and `.onion` matches only as a shaped v2/v3
//! onion address rather than a bare TLD substring.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, MatchMode, MatchOptions,
    MatchShape, api_call_hits, confidence_from_rank, confidence_rank, low, medium,
    string_artifact_hits, string_artifact_hits_with,
};

use super::T1486;

/// The Ransomware behavior detection (ATT&CK T1486).
pub struct RansomwareBehavior;

/// Drive/file enumeration and rename APIs a ransomware uses to find and mark
/// victim files.
///
/// Every entry is dual-use — file managers, installers, backup/sync tools, and
/// RAT file-manager/recon commands call the same APIs — so each reasoning
/// claims the capability and names the corroboration a T1486 claim needs:
/// co-located crypto evidence or a ransom note. One function's hits are
/// collapsed into a single enumeration-surface finding by
/// [`merge_enumeration_hits`].
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "GetLogicalDriveStrings",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls GetLogicalDriveStrings to enumerate every mounted drive. The sweep is a \
                    discovery capability file managers, backup tools, and RAT 'list drives' \
                    commands perform as routinely as ransomware; it evidences T1486 \
                    encryption-for-impact only when co-located crypto evidence or a ransom note \
                    ties the sweep to an encryption workflow.",
    },
    ApiSpec {
        key: "GetLogicalDrives",
        always_noise: false,
        severity: low(),
        reasoning: "Calls GetLogicalDrives to list drive letters (A:-Z:) — a drive-presence check \
                    shared by file managers, installers, and recon listings. On its own this is \
                    enumeration capability; a T1486 claim needs co-located crypto evidence or a \
                    ransom note.",
    },
    ApiSpec {
        key: "FindFirstFile",
        always_noise: false,
        severity: low(),
        reasoning: "Calls FindFirstFile to begin a directory walk — the primitive behind \
                    file-manager listings, installer and backup enumeration, and recon-for-exfil \
                    directory sweeps as much as ransomware target discovery. The walk evidences \
                    T1486 only with co-located crypto evidence or a ransom note tying it to \
                    encryption.",
    },
    ApiSpec {
        key: "FindNextFile",
        always_noise: false,
        severity: low(),
        reasoning: "Calls FindNextFile to continue a directory walk over enumerated files — \
                    ubiquitous in file managers, installers, and backup or recon listings. \
                    Without co-located crypto evidence or a ransom note this is enumeration \
                    capability, not T1486 encryption-for-impact.",
    },
    ApiSpec {
        key: "GetDriveType",
        always_noise: false,
        severity: low(),
        reasoning: "Calls GetDriveType to classify each enumerated drive \
                    (fixed/removable/network). File-manager drive lists and installer target \
                    checks classify drives the same way; it speaks to selecting volumes to \
                    encrypt (T1486) only when co-located crypto evidence or a ransom note is \
                    present in the binary.",
    },
    ApiSpec {
        key: "SetFileAttributes",
        always_noise: false,
        severity: low(),
        reasoning: "Calls SetFileAttributes to rewrite file attributes (e.g. clearing read-only \
                    before modifying a file) — routine in installers, unpackers, and \
                    self-updaters. As part of a T1486 encryption workflow it is evidenced only \
                    alongside co-located crypto evidence or a ransom note.",
    },
    ApiSpec {
        key: "MoveFile",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls MoveFile to rename or move files. Self-update relaunches, staging \
                    moves, and file-manager commands rename files as often as ransomware appends \
                    a marker extension to each encrypted one; the T1486 extension-rewrite \
                    reading needs co-located crypto evidence or a ransom note.",
    },
    ApiSpec {
        key: "GetVolumeInformation",
        always_noise: false,
        severity: low(),
        reasoning: "Calls GetVolumeInformation to read a volume's name/serial/filesystem — \
                    widely used to derive a volume-serial host/bot ID or to inspect a drive \
                    before use. That is discovery, not encryption; a T1486 reading requires \
                    co-located crypto evidence or a ransom note.",
    },
];

/// Ransom-note / extortion phrases (distinctive enough for medium confidence).
///
/// The bare `"your files"` needle is omitted as fully shadowed: notes that say
/// it also match `"all your files"` or a decrypt/recover phrase, while the bare
/// form matches benign UI text. `.onion` is not a phrase — it is matched by the
/// shape-gated onion-address pass in [`RansomwareBehavior::run`].
const NOTE_STRINGS: &[&str] = &[
    "have been encrypted",
    "all your files",
    "how to decrypt",
    "how to recover",
    "recover your file",
    "decrypt your file",
];

/// The Tor hidden-service TLD needle for the shape-gated onion-address pass.
///
/// Matched under [`ONION_OPTIONS`], so a real v2/v3 address
/// (`34r6hq26q2h4jkzj.onion`) matches while a bare `".onion"`/`".onion."`
/// suffix-guard constant (c-ares' RFC 7686 refusal strings) does not.
const ONION_NEEDLES: &[&str] = &[".onion"];

/// The matching options for the onion-address pass.
///
/// [`MatchShape::OnionAddress`] requires a 16- or 56-character base32 label
/// immediately before the matched TLD — the v2/v3 onion-address forms — so
/// only a concrete hidden-service address matches, never the TLD alone.
const ONION_OPTIONS: MatchOptions = MatchOptions {
    mode: MatchMode::Substring,
    base64_run_veto: false,
    shape: Some(MatchShape::OnionAddress),
};

/// The catalog APIs that rename files or rewrite their attributes.
///
/// Used to word a merged per-function finding: enumeration co-located with a
/// rename/attribute rewrite is the walk-and-mark loop shape, which the combined
/// content and reasoning call out.
const RENAME_APIS: &[&str] = &["MoveFile", "SetFileAttributes"];

/// The max call sites named in a merged enumeration-surface finding's content.
///
/// Matches the shared helpers' cap: enough to spot-check where the calls are;
/// past that the count says more than the addresses do.
const MAX_MERGED_SITES: usize = 12;

/// Render a merged finding's call sites, naming the first few and counting the
/// rest.
///
/// # Arguments
///
/// * `sites` - The merged call sites, in ascending address order
fn render_merged_sites(sites: &[u64]) -> String {
    // name the first few sites in the shared 0x… rendering
    let listed = sites
        .iter()
        .take(MAX_MERGED_SITES)
        .copied()
        .map(fmt_addr)
        .collect::<Vec<_>>()
        .join(", ");
    // no silent truncation: say how many sites were left out
    match sites
        .len()
        .checked_sub(MAX_MERGED_SITES)
        .filter(|extra| *extra > 0)
    {
        Some(extra) => format!("{listed} (+{extra} more; {} sites in total)", sites.len()),
        None => listed,
    }
}

/// Merge one function's catalog API hits into a single enumeration-surface
/// finding.
///
/// `api_call_hits` emits one hit per (function x API x thunkness), so a single
/// file-manager-shaped function draws up to eight separate T1486 findings an
/// agent must refute one by one. This groups the call-site hits by containing
/// function and collapses each group into one finding naming every matched API
/// with the union of their call sites. Lone hits keep their per-API content and
/// reasoning, and hits without a catalog key (the file-level import-census
/// line) pass through untouched.
///
/// # Arguments
///
/// * `hits` - The catalog's `api_call_hits` output
fn merge_enumeration_hits(hits: Vec<DetectionHit>) -> Vec<DetectionHit> {
    // collect pass-through hits and bucket catalog call-site hits by function;
    // a BTreeMap keys on the function entry so output order is deterministic
    let mut merged: Vec<DetectionHit> = Vec::new();
    let mut groups: BTreeMap<Option<u64>, Vec<DetectionHit>> = BTreeMap::new();
    for hit in hits {
        // only catalog call-site hits merge; the census line carries no API key
        if hit.api.is_none() {
            merged.push(hit);
            continue;
        }
        // bucket the call-site hit under its containing function
        groups.entry(hit.function).or_default().push(hit);
    }
    // collapse each function's bucket into one combined finding
    merged.extend(groups.into_values().filter_map(merge_function_group));
    merged
}

/// Compose a merged finding's reasoning in capability-plus-condition form.
///
/// Claims the combined dual-use capability, calls out the walk-and-mark
/// co-occurrence when enumeration and rename/attribute APIs share the
/// function, names the crypto/ransom-note corroboration a T1486 claim needs,
/// and keeps the shared builder's thunk attribution note.
///
/// # Arguments
///
/// * `list` - The matched APIs, rendered as a comma-separated list
/// * `descriptor` - The lower-case description of the combined surface
/// * `in_function` - Whether the cluster sits inside a recovered function
/// * `co_occurs` - Whether enumeration and rename/attribute APIs co-occur
/// * `thunked` - Whether any member was reached through a thunk stub
fn merged_reasoning(
    list: &str,
    descriptor: &str,
    in_function: bool,
    co_occurs: bool,
    thunked: bool,
) -> String {
    // open with where the cluster sits, without asserting a containing
    // function the resolver did not find
    let lead = if in_function {
        format!("One function calls {list} together.")
    } else {
        format!("Call sites outside any recovered function combine {list}.")
    };
    // claim the combined capability in dual-use terms
    let mut reasoning = format!(
        "{lead} This cluster is a {descriptor} capability that file managers, installers, \
         backup/sync tools, and RAT file-manager/recon commands implement as routinely as \
         ransomware target discovery."
    );
    // name the walk-and-mark co-occurrence when both halves are present
    if co_occurs {
        reasoning.push_str(
            " Enumeration co-located with a rename or attribute rewrite in one function is the \
             walk-and-mark loop a ransomware runs over victim files — and equally a file \
             manager's move command or an installer's staging pass.",
        );
    }
    // name the corroboration a T1486 claim needs either way
    reasoning.push_str(
        " It evidences T1486 encryption-for-impact only when co-located crypto evidence \
         (crypto APIs or constants) or a ransom note ties this surface to an encryption \
         workflow.",
    );
    // keep the thunk attribution note when any member was reached via a stub
    if thunked {
        reasoning.push_str(
            " At least one of these imports is invoked through a thunk stub, so this flag is \
             attributed to the calling function shown here (not the one-instruction jump stub).",
        );
    }
    reasoning
}

/// Collapse one function's catalog hits into a single combined finding.
///
/// A lone hit passes through unchanged; two or more hits become one finding
/// naming the matched APIs, the union of their call sites (kept on the hit for
/// the validator and the markup pass), the strongest member severity (merging
/// never weakens evidence), and the thunk attribution note when any member was
/// reached through a thunk stub. When an enumeration API co-occurs with a
/// rename/attribute API the content and reasoning call out the walk-and-mark
/// shape, still conditioned on crypto/ransom-note corroboration.
///
/// # Arguments
///
/// * `group` - One function's catalog call-site hits
fn merge_function_group(mut group: Vec<DetectionHit>) -> Option<DetectionHit> {
    // a lone hit needs no merging (and an empty group merges to nothing)
    if group.len() <= 1 {
        return group.pop();
    }
    // gather every backing call site (a hit with no list backs just its address)
    let mut sites: Vec<u64> = group
        .iter()
        .flat_map(|hit| {
            if hit.sites.is_empty() {
                vec![hit.address]
            } else {
                hit.sites.clone()
            }
        })
        .collect();
    // sort + dedupe so the rendered site list is stable and free of repeats
    sites.sort_unstable();
    sites.dedup();
    // name each matched API once, alphabetically for a stable rendering
    let mut apis: Vec<&'static str> = group.iter().filter_map(|hit| hit.api).collect();
    apis.sort_unstable();
    apis.dedup();
    // the strongest member grade survives the merge (merging never weakens)
    let strongest = group
        .iter()
        .map(|hit| confidence_rank(hit.confidence))
        .max()
        .unwrap_or(0);
    let suspicion = group.iter().map(|hit| hit.suspicion).max().unwrap_or(0);
    // detect a thunked member by the shared builder's content marker, so the
    // merged reasoning keeps `build_api_hit`'s attribution note
    let thunked = group
        .iter()
        .any(|hit| hit.content.contains("via import thunk"));
    // split the matched APIs into the walk half and the mark half
    let renames = apis.iter().any(|api| RENAME_APIS.contains(api));
    let enumerates = apis.iter().any(|api| !RENAME_APIS.contains(api));
    // word the surface class for the walk / mark / walk-and-mark shapes
    let (surface, descriptor) = match (enumerates, renames) {
        (true, true) => (
            "Filesystem enumeration + rename/attribute surface",
            "filesystem enumeration plus file rename/attribute rewrite",
        ),
        (false, true) => (
            "File rename/attribute surface",
            "file rename/attribute rewrite",
        ),
        _ => (
            "Filesystem/drive enumeration surface",
            "filesystem/drive enumeration",
        ),
    };
    // render one content line naming every API and the union of call sites
    let list = apis.join(", ");
    let content = format!(
        "{surface} ({list}) — {} calls @ {}",
        sites.len(),
        render_merged_sites(&sites),
    );
    // compose the capability+condition reasoning for the combined finding
    let reasoning = merged_reasoning(
        &list,
        descriptor,
        group[0].function.is_some(),
        enumerates && renames,
        thunked,
    );
    // base the merged hit on the group's shared attributes and strongest grade
    let severity = (confidence_from_rank(strongest), suspicion);
    let mut hit = DetectionHit::new(
        group[0].technique.clone(),
        sites.first().copied().unwrap_or(0),
        group[0].function,
        severity,
        content,
        reasoning,
    )
    .kind(group[0].kind)
    .sites(sites);
    // keep the catalog key when the group is one API observed two ways, so the
    // validator can still identify it
    if let [only] = apis.as_slice() {
        hit = hit.api(only);
    }
    // a merged hit is only inherent noise when every member asserted it
    if group.iter().all(|member| member.always_noise) {
        hit = hit.always_noise();
    }
    // carry the strongest clamped-aside catalog severity so the validation
    // pass can still restore it on verified use
    let catalog = group
        .iter()
        .filter_map(|member| member.catalog_severity)
        .max_by_key(|(confidence, suspicion)| (confidence_rank(*confidence), *suspicion));
    if let Some(withheld) = catalog {
        hit = hit.catalog_severity(withheld);
    }
    Some(hit)
}

#[async_trait::async_trait]
impl Detection for RansomwareBehavior {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "ransomware_behavior"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1486
    }

    /// Run the enumeration/rename API, ransom-note, and onion-address passes.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The analysis context to scan
    #[tracing::instrument(name = "RansomwareBehavior::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from each sub-pass
        let mut hits = Vec::new();
        // 1) drive/file enumeration + rename API call sites, merged so each
        //    function raises one combined enumeration-surface finding
        hits.extend(merge_enumeration_hits(
            api_call_hits(ctx, T1486, APIS, EvidenceKind::ContextOnly).await?,
        ));
        // 2) ransom-note / extortion phrases
        hits.extend(
            string_artifact_hits(
                ctx,
                T1486,
                NOTE_STRINGS,
                medium(),
                "Contains ransom-note / extortion wording (\"...have been encrypted\", \"how to \
                 decrypt/recover\"). This is the victim-facing note a ransomware drops with \
                 payment and recovery instructions.",
                &[],
            )
            .await?,
        );
        // 3) Tor hidden-service addresses, shape-gated so only a concrete
        //    v2/v3 onion address matches, never a bare ".onion" guard constant
        hits.extend(
            string_artifact_hits_with(
                ctx,
                T1486,
                ONION_NEEDLES,
                medium(),
                "References a Tor hidden-service address: a v2/v3 base32 onion label precedes \
                 \".onion\", so this is a concrete address — the payment/contact channel a \
                 ransom note points victims at (or Tor-based C2) — not a bare \".onion\" \
                 suffix-guard constant like DNS libraries carry.",
                &[],
                ONION_OPTIONS,
            )
            .await?,
        );
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::thorpal::detect::confidence::Confidence;
    use crate::thorpal::detect::detections::needle_matches;

    /// Build a catalog-shaped call-site hit the way `build_api_hit` renders one.
    ///
    /// # Arguments
    ///
    /// * `api` - The catalog API key
    /// * `function` - The containing function entry
    /// * `sites` - The call sites within that function (first one is primary)
    /// * `severity` - The seeded `(confidence, suspicion)` pair
    /// * `thunked` - Whether the import was reached through a thunk stub
    fn catalog_hit(
        api: &'static str,
        function: u64,
        sites: &[u64],
        severity: (Confidence, i64),
        thunked: bool,
    ) -> DetectionHit {
        // render the content the way the shared builder does for each shape
        let content = match (thunked, sites.len()) {
            (true, n) if n > 1 => format!("{api} call (via import thunk) — {n} calls @ …"),
            (true, _) => format!("{api} call (via import thunk) @ …"),
            (false, n) if n > 1 => format!("{api} — {n} calls @ …"),
            (false, _) => format!("{api} call @ …"),
        };
        // assemble the hit with the catalog key and the full site list
        let mut hit = DetectionHit::new(
            T1486,
            sites[0],
            Some(function),
            severity,
            content,
            "test reasoning",
        )
        .kind(EvidenceKind::ContextOnly)
        .api(api);
        if sites.len() > 1 {
            hit = hit.sites(sites.to_vec());
        }
        hit
    }

    /// Bare ".onion"/".onion." guard constants fail the shape; real onion addresses pass.
    #[test]
    fn onion_needle_requires_an_address_shape() {
        // c-ares' RFC 7686 refusal strings carry no preceding base32 label
        for guard in [".onion", ".onion."] {
            assert!(!needle_matches(guard, ".onion", ONION_OPTIONS), "{guard}");
        }
        // the pack's confirmed ransomware addresses carry exact 16-char labels
        for addr in [
            "34r6hq26q2h4jkzj.onion ",
            "http://4rhfxsrzmzilheyj.onion/get.php?a=",
        ] {
            assert!(needle_matches(addr, ".onion", ONION_OPTIONS), "{addr}");
        }
    }

    /// The pack's confirmed ransom-note strings still match the trimmed phrase list.
    #[test]
    fn confirmed_note_phrases_still_match() {
        // the confirmed notes, lower-cased like the pass does
        for note in [
            "the only copy of the private key, which will allow you to decrypt your files",
            "all your files stolen and encrypted",
        ] {
            assert!(
                NOTE_STRINGS.iter().any(|needle| needle_matches(
                    note,
                    needle,
                    MatchOptions::default()
                )),
                "{note}"
            );
        }
        // the shadowed bare needle and the bare TLD are no longer phrase needles
        assert!(!NOTE_STRINGS.contains(&"your files"));
        assert!(!NOTE_STRINGS.contains(&".onion"));
    }

    /// Every catalog reasoning claims the capability and names the T1486 corroboration condition.
    #[test]
    fn catalog_reasonings_claim_capability_not_intent() {
        for spec in APIS {
            let lower = spec.reasoning.to_ascii_lowercase();
            // the condition: a T1486 claim needs co-located crypto or a ransom note
            assert!(lower.contains("t1486"), "{} names the claim", spec.key);
            assert!(
                lower.contains("crypto evidence"),
                "{} names the crypto condition",
                spec.key
            );
            assert!(
                lower.contains("ransom note"),
                "{} names the note condition",
                spec.key
            );
            // the old asserted-intent and self-conceding wording is gone
            assert!(
                !lower.contains("recurses the tree to find victim files"),
                "{}",
                spec.key
            );
            assert!(!lower.contains("hence low confidence"), "{}", spec.key);
        }
    }

    /// One file-manager-shaped function's five per-API hits merge into a single finding.
    #[test]
    fn function_hits_merge_into_one_enumeration_surface() {
        // a disputed shape: five separate findings on one function
        let hits = vec![
            catalog_hit("FindFirstFile", 0x4000, &[0x4010], low(), false),
            catalog_hit("FindNextFile", 0x4000, &[0x4020, 0x4030], low(), false),
            catalog_hit("GetDriveType", 0x4000, &[0x4040], low(), false),
            catalog_hit("GetLogicalDriveStrings", 0x4000, &[0x4050], medium(), false),
            catalog_hit("GetVolumeInformation", 0x4000, &[0x4060], low(), false),
        ];
        let merged = merge_enumeration_hits(hits);
        assert_eq!(merged.len(), 1);
        let hit = &merged[0];
        // the combined content names every API and the class reads as a surface
        assert!(
            hit.content
                .starts_with("Filesystem/drive enumeration surface"),
            "{}",
            hit.content
        );
        for api in [
            "FindFirstFile",
            "FindNextFile",
            "GetDriveType",
            "GetLogicalDriveStrings",
            "GetVolumeInformation",
        ] {
            assert!(hit.content.contains(api), "{api}");
        }
        // every call site is kept for the validator and the markup pass
        assert_eq!(
            hit.sites,
            vec![0x4010, 0x4020, 0x4030, 0x4040, 0x4050, 0x4060]
        );
        assert_eq!(hit.address, 0x4010);
        assert_eq!(hit.function, Some(0x4000));
        // the strongest member grade survives the merge
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, medium().1);
        // the reasoning claims capability and names the corroboration condition
        assert!(hit.reasoning.contains("T1486"));
        assert!(hit.reasoning.contains("crypto evidence"));
    }

    /// Enumeration co-located with a rename API is called out as the walk-and-mark shape.
    #[test]
    fn enumeration_plus_rename_co_occurrence_is_named() {
        // a walk half plus a mark half in the same function
        let hits = vec![
            catalog_hit("FindFirstFile", 0x5000, &[0x5010], low(), false),
            catalog_hit("MoveFile", 0x5000, &[0x5020], medium(), false),
        ];
        let merged = merge_enumeration_hits(hits);
        assert_eq!(merged.len(), 1);
        let hit = &merged[0];
        // the surface class and the reasoning both call out the co-occurrence
        assert!(
            hit.content
                .starts_with("Filesystem enumeration + rename/attribute surface"),
            "{}",
            hit.content
        );
        assert!(hit.reasoning.contains("walk-and-mark"), "{}", hit.reasoning);
        // the rename member's medium grade survives
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, medium().1);
    }

    /// The Delphi-RTL dispute shape (thunked walk in one stripped helper) merges to one
    /// low-grade finding keeping the thunk note.
    #[test]
    fn thunked_delphi_walk_merges_and_keeps_the_thunk_note() {
        // a Delphi shape: FindFirstFile/FindNextFile via thunks in one
        // stripped SysUtils-style helper
        let hits = vec![
            catalog_hit("FindFirstFile", 0xbe90, &[0xbea4], low(), true),
            catalog_hit("FindNextFile", 0xbe90, &[0xbec0], low(), true),
        ];
        let merged = merge_enumeration_hits(hits);
        assert_eq!(merged.len(), 1);
        let hit = &merged[0];
        // one low-grade finding instead of two, still thunk-attributed
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, low().1);
        assert!(hit.reasoning.contains("thunk stub"), "{}", hit.reasoning);
        assert!(
            hit.content
                .starts_with("Filesystem/drive enumeration surface"),
            "{}",
            hit.content
        );
    }

    /// A lone `MoveFile` hit (the pack's confirmed ransomware rename) passes through unchanged.
    #[test]
    fn lone_confirmed_move_file_passes_through_unchanged() {
        // the confirmed shape: one MoveFile call in a true-ransomware function
        let hit = catalog_hit("MoveFile", 0x6000, &[0x6010], medium(), false);
        let content = hit.content.clone();
        let merged = merge_enumeration_hits(vec![hit]);
        assert_eq!(merged.len(), 1);
        // content, grade, and catalog key are untouched for the validator
        assert_eq!(merged[0].content, content);
        assert!(matches!(merged[0].confidence, Confidence::Unsure));
        assert_eq!(merged[0].suspicion, medium().1);
        assert_eq!(merged[0].api, Some("MoveFile"));
    }

    /// The file-level census line is never merged and separate functions stay separate.
    #[test]
    fn census_line_and_separate_functions_stay_separate() {
        // the census line carries no catalog key, no function, and address 0
        let census = DetectionHit::new(
            T1486,
            0,
            None,
            low(),
            "2 catalog APIs imported but never referenced from code: FindFirstFile, MoveFile",
            "census reasoning",
        );
        let hits = vec![
            census,
            catalog_hit("FindFirstFile", 0x4000, &[0x4010], low(), false),
            catalog_hit("FindFirstFile", 0x7000, &[0x7010], low(), false),
        ];
        let merged = merge_enumeration_hits(hits);
        // three findings survive: the census line plus one per function
        assert_eq!(merged.len(), 3);
    }

    /// Thunked and direct calls of one API in one function merge keeping the catalog key.
    #[test]
    fn same_api_thunked_and_direct_merge_keeps_the_api_key() {
        // the same import reached two ways from one function
        let hits = vec![
            catalog_hit("MoveFile", 0x8000, &[0x8010], medium(), true),
            catalog_hit("MoveFile", 0x8000, &[0x8020], medium(), false),
        ];
        let merged = merge_enumeration_hits(hits);
        assert_eq!(merged.len(), 1);
        let hit = &merged[0];
        // one API observed two ways keeps its key for the validator
        assert_eq!(hit.api, Some("MoveFile"));
        assert_eq!(hit.sites, vec![0x8010, 0x8020]);
        assert!(hit.reasoning.contains("thunk stub"), "{}", hit.reasoning);
    }
}
