//! Seed-time YARA scanning over the raw sample (improvement 0015).
//!
//! When the operator configures a rules path, the sample's on-disk bytes are
//! scanned in full with yara-x (the official Rust engine, which handles the
//! modern rule packs the shipped 4.2 CLI could not) and the matching rules
//! become seeded, file-level findings. The raw matches are post-processed
//! before seeding: rules naming the same family across sources cluster into
//! one corroborated lead, match offsets are attributed to the file's
//! structure (sections, version resources, embedded PEs, the certificate
//! table), matched bytes are quoted so leads self-evidence, self-declared
//! weak rule classes (`SUSP_`/`Generic_Threat`/`_Unknown_`/packer) rank below
//! handcrafted family signatures, and leads past a per-sample cap fold into
//! one rollup. Rule hits that name malware families are content-derived
//! evidence — the rule matched the bytes, and so did the quoted excerpts — so
//! they are fully compatible with blinded evaluation runs. Rules are
//! operator-supplied and never vendored; a missing or unparseable pack
//! degrades to a warning, not a failed run.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

use crate::thorpal::detect::detections::{
    AttackTactic, AttackTechnique, DetectionHit, EvidenceKind, HitSource, confidence_rank, high,
    low, medium,
};
use crate::thorpal::detect::fileinfo;

/// The pseudo-technique YARA matches are reported under.
///
/// A signature match is identification evidence rather than a single ATT&CK
/// behavior; it is filed under Defense Evasion alongside the crate's other
/// obfuscation/identification findings so it renders like every other hit.
pub const YARA_SIGNATURE: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("YARA"),
    name: Cow::Borrowed("YARA signature match"),
    tactic: AttackTactic::DefenseEvasion,
};

/// The most matching rules reported per scan.
const MAX_YARA_HITS: usize = 32;

/// The most pattern-match sites recorded per rule.
const MAX_SITES: usize = 8;

/// The most family clusters rendered as individual leads per scan.
///
/// Clusters past this cut (sorted strongest-first) fold into one summary
/// rollup hit, so a flood of weak or duplicate rules cannot bury the forum
/// while every folded rule stays discoverable by name and first offset.
const MAX_RENDERED_LEADS: usize = 10;

/// The most per-site matched-byte excerpts quoted per rule.
const MAX_EXCERPTS: usize = 4;

/// The most matched bytes considered per excerpt.
const MAX_EXCERPT_BYTES: usize = 48;

/// The most bytes shown in a hex (non-text) excerpt.
const MAX_HEX_SHOWN: usize = 16;

/// The most `VS_VERSION_INFO` blocks mapped per file (hostile-flood bound).
const MAX_VERSION_BLOCKS: usize = 64;

/// The most embedded-PE regions mapped per file (hostile-flood bound).
const MAX_EMBEDDED_PES: usize = 16;

/// The most embedded-PE header parses attempted per file.
///
/// A bare `MZ` pair is rejected with two bounded reads, but a candidate with
/// a plausible PE signature costs a header parse; capping the parses keeps a
/// file stuffed with decoy headers from stalling the scan.
const MAX_EMBEDDED_CANDIDATES: usize = 64;

/// The entity-name prefix stamped on corroborated family-cluster leads.
///
/// [`corroborated_family`] strips it back off; the seeder keys its
/// family-identification question on that accessor rather than this string.
const FAMILY_LEAD_PREFIX: &str = "YARA family: ";

/// The known source/author prefixes operator packs carry, with the source
/// each one names.
///
/// Stripping these turns alias identifiers into comparable family tokens, and
/// counting distinct sources across a cluster is what "independent" means in
/// the corroboration upgrade. The elastic prefixes all map to one source, so
/// elastic's `Windows_Trojan_`/`Windows_Generic_` pairs never corroborate
/// themselves.
const SOURCE_PREFIXES: &[(&str, &str)] = &[
    ("MALPEDIA_", "malpedia"),
    ("SIGNATURE_BASE_", "signature_base"),
    ("DITEKSHEN_", "ditekshen"),
    ("CAPE_", "cape"),
    ("SEKOIA_", "sekoia"),
    ("ARKBIRD_", "arkbird"),
    ("R3C0NST_", "r3c0nst"),
    ("RUSSIANPANDA_", "russianpanda"),
    ("CRAIU_", "craiu"),
    ("WINDOWS_TROJAN_", "elastic"),
    ("LINUX_TROJAN_", "elastic"),
    ("MACOS_TROJAN_", "elastic"),
    ("WINDOWS_GENERIC_", "elastic"),
];

/// The time budget for the scan phase, per run.
///
/// Compilation runs unbounded here; the seed phase bounds the whole
/// compile-plus-scan task with its own timeout so a pathological pack cannot
/// stall seeding either way.
const SCAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Scan a sample file on disk with the operator's YARA rules.
///
/// Reads the file in full — never a capped analysis-context view — so rules
/// keyed on `filesize` or on content past any in-memory cap judge the real
/// file, then delegates to [`scan`].
///
/// # Arguments
///
/// * `rules_path` - The operator-configured rules file or directory
/// * `sample` - The path to the sample file to scan
pub fn scan_file(rules_path: &Path, sample: &Path) -> Result<Vec<DetectionHit>, anyhow::Error> {
    // read the true on-disk bytes; conditions like filesize depend on them
    let raw = std::fs::read(sample)
        .map_err(|error| anyhow::anyhow!("reading sample '{}': {error}", sample.display()))?;
    scan(rules_path, &raw)
}

/// Scan a sample's raw bytes with the operator's YARA rules.
///
/// `rules_path` may be a single `.yar`/`.yara` file or a directory of them
/// (top-level files only — nested directories are not walked; each file
/// compiles into its own namespace, keyed by its full file name so `foo.yar`
/// and `foo.yara` never collide). Any per-file compile failure is logged and
/// skipped; a scan failure or timeout yields no hits. Matching rules come
/// back as file-level [`DetectionHit`]s: same-family alias rules are merged
/// into one lead (upgraded when independent sources corroborate it), match
/// offsets are attributed to the file structure, matched bytes are quoted,
/// and anything past the render cap folds into one rollup lead.
///
/// # Arguments
///
/// * `rules_path` - The operator-configured rules file or directory
/// * `raw` - The sample's file bytes
#[tracing::instrument(skip_all, err(Debug))]
pub fn scan(rules_path: &Path, raw: &[u8]) -> Result<Vec<DetectionHit>, anyhow::Error> {
    // gather the rule files: one path, or every .yar/.yara in a directory
    let mut files = Vec::new();
    if rules_path.is_dir() {
        for entry in std::fs::read_dir(rules_path)? {
            let path = entry?.path();
            let ext = path.extension().and_then(|ext| ext.to_str());
            if matches!(ext, Some("yar" | "yara")) {
                files.push(path);
            }
        }
        files.sort();
    } else {
        files.push(rules_path.to_path_buf());
    }
    if files.is_empty() {
        anyhow::bail!("no .yar/.yara rule files under '{}'", rules_path.display());
    }
    // compile every pack, one namespace per file so identifiers never collide;
    // relaxed regexp syntax accepts the 4.x-isms the public packs carry
    let mut compiler = yara_x::Compiler::new();
    compiler.relaxed_re_syntax(true);
    let mut loaded = 0usize;
    let mut first_error: Option<String> = None;
    for file in &files {
        let source = match std::fs::read(file) {
            Ok(source) => source,
            Err(error) => {
                // an unreadable pack is skipped, never fatal to seeding
                tracing::warn!(?error, pack = %file.display(), "skipping unreadable rule pack");
                first_error.get_or_insert_with(|| format!("{}: {error}", file.display()));
                continue;
            }
        };
        // namespace by the full file name (extension kept) so same-stem packs
        // like foo.yar and foo.yara never share a namespace and silently drop
        // one of them on duplicate rule identifiers
        let name = file.file_name().map_or_else(
            || "rules".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        compiler.new_namespace(&name);
        if let Err(error) = compiler.add_source(source.as_slice()) {
            // a pack that fails to parse is skipped, never fatal to seeding
            tracing::warn!(?error, pack = %file.display(), "skipping unparseable rule pack");
            first_error.get_or_insert_with(|| format!("{}: {error}", file.display()));
            continue;
        }
        loaded += 1;
    }
    if loaded == 0 {
        anyhow::bail!(
            "no rule pack compiled from '{}' (first error: {})",
            rules_path.display(),
            first_error.unwrap_or_else(|| "<none recorded>".to_owned()),
        );
    }
    let rules = compiler.build();
    // scan under a hard time budget so a pathological pack cannot stall seeding
    let mut scanner = yara_x::Scanner::new(&rules);
    scanner.set_timeout(SCAN_TIMEOUT);
    let results = scanner.scan(raw)?;
    // collect the matching rules (bounded) with their sites and excerpts
    let matches = collect_matches(&results, raw);
    let matched_count = matches.len();
    // cluster, tier, attribute, and cap the matches into seeded leads
    let hits = build_hits(matches, raw);
    tracing::info!(
        rules = loaded,
        matches = matched_count,
        leads = hits.len(),
        "yara seed scan complete"
    );
    Ok(hits)
}

/// Collect each matching rule (bounded) with its match sites and excerpts.
///
/// # Arguments
///
/// * `results` - The completed scan results
/// * `raw` - The scanned bytes, for excerpt rendering
fn collect_matches(results: &yara_x::ScanResults<'_, '_>, raw: &[u8]) -> Vec<RuleMatch> {
    // reduce each matching rule (bounded) to its clustering form
    let mut matches = Vec::new();
    for rule in results.matching_rules().take(MAX_YARA_HITS) {
        // collect the first match sites and excerpts across the rule's patterns
        let mut sites: Vec<u64> = Vec::new();
        let mut excerpts: Vec<String> = Vec::new();
        for pattern in rule.patterns() {
            for found in pattern.matches() {
                if sites.len() >= MAX_SITES {
                    break;
                }
                let range = found.range();
                // quote the matched bytes (capped) so the lead self-evidences
                if excerpts.len() < MAX_EXCERPTS
                    && let Some(data) = raw.get(range.clone())
                {
                    excerpts.push(format!("{:#x}: {}", range.start, render_excerpt(data)));
                }
                sites.push(range.start as u64);
            }
            if sites.len() >= MAX_SITES {
                break;
            }
        }
        matches.push(RuleMatch {
            namespace: rule.namespace().to_owned(),
            identifier: rule.identifier().to_owned(),
            sites,
            excerpts,
        });
    }
    matches
}

/// One matching rule as collected from the scanner, before clustering.
struct RuleMatch {
    /// The pack namespace the rule compiled under.
    namespace: String,
    /// The rule identifier as written in the pack.
    identifier: String,
    /// The first match-site file offsets (bounded by [`MAX_SITES`]).
    sites: Vec<u64>,
    /// Rendered `offset: excerpt` entries for the first sites (bounded by
    /// [`MAX_EXCERPTS`]).
    excerpts: Vec<String>,
}

/// A group of matching rules that reduce to the same family token.
struct FamilyCluster {
    /// The family token the members share (see [`family_token`]).
    token: String,
    /// The member rules, in scanner order.
    members: Vec<RuleMatch>,
}

impl FamilyCluster {
    /// Count the independent sources agreeing on this family.
    ///
    /// Distinct pack namespaces and distinct recognized source prefixes are
    /// both independence axes: a per-source pack layout varies namespaces
    /// while one combined pack varies only identifier prefixes, and either
    /// form of agreement counts.
    fn independent_sources(&self) -> usize {
        // distinct pack namespaces (one compiled namespace per rule file)
        let namespaces: HashSet<&str> = self
            .members
            .iter()
            .map(|member| member.namespace.as_str())
            .collect();
        // distinct recognized source prefixes within the identifiers
        let sources: HashSet<&str> = self
            .members
            .iter()
            .filter_map(|member| split_source(&member.identifier).0)
            .collect();
        namespaces.len().max(sources.len())
    }

    /// Whether independent sources corroborate this family.
    ///
    /// Requires two or more distinct rule identifiers (the same rule shipped
    /// in two pack files is one signature) agreeing across two or more
    /// independent sources.
    fn corroborated(&self) -> bool {
        // identical identifiers never corroborate each other
        let distinct: HashSet<&str> = self
            .members
            .iter()
            .map(|member| member.identifier.as_str())
            .collect();
        distinct.len() >= 2 && self.independent_sources() >= 2
    }

    /// The merged match sites across every member, deduped and bounded.
    fn sites(&self) -> Vec<u64> {
        // preserve member order, dropping offsets shared across alias rules
        let mut sites: Vec<u64> = Vec::new();
        for member in &self.members {
            for site in &member.sites {
                if sites.len() >= MAX_SITES {
                    return sites;
                }
                if !sites.contains(site) {
                    sites.push(*site);
                }
            }
        }
        sites
    }
}

/// Post-process the matching rules into seeded leads.
///
/// Clusters same-family alias rules into one lead each, tiers severity by
/// rule class and corroboration, attributes match offsets to the file's
/// structure, and caps the rendered leads with a rollup for the tail.
///
/// # Arguments
///
/// * `matches` - The matching rules collected from the scanner
/// * `raw` - The sample's file bytes, for structural attribution
fn build_hits(matches: Vec<RuleMatch>, raw: &[u8]) -> Vec<DetectionHit> {
    // map the file's structure once; every cluster classifies against it
    let regions = file_regions(raw);
    // group the rules by family token, preserving scanner order
    let mut clusters: Vec<FamilyCluster> = Vec::new();
    for rule_match in matches {
        // reduce the identifier to its family token for alias grouping
        let token = family_token(&rule_match.identifier);
        // extend the family's cluster, or open one on first sight
        if let Some(cluster) = clusters.iter_mut().find(|cluster| cluster.token == token) {
            cluster.members.push(rule_match);
        } else {
            clusters.push(FamilyCluster {
                token,
                members: vec![rule_match],
            });
        }
    }
    // render one lead per cluster, keeping the cluster for the sort/rollup
    let mut leads: Vec<(&FamilyCluster, DetectionHit)> = clusters
        .iter()
        .map(|cluster| (cluster, cluster_hit(cluster, &regions)))
        .collect();
    // strongest leads first: confidence, suspicion, member count, then token
    leads.sort_by(|(a_cluster, a_hit), (b_cluster, b_hit)| {
        confidence_rank(b_hit.confidence)
            .cmp(&confidence_rank(a_hit.confidence))
            .then(b_hit.suspicion.cmp(&a_hit.suspicion))
            .then(b_cluster.members.len().cmp(&a_cluster.members.len()))
            .then(a_cluster.token.cmp(&b_cluster.token))
    });
    // emit the strongest leads individually and set aside the tail
    let mut hits: Vec<DetectionHit> = Vec::new();
    let mut tail: Vec<(&FamilyCluster, DetectionHit)> = Vec::new();
    for (cluster, hit) in leads {
        if hits.len() < MAX_RENDERED_LEADS {
            hits.push(hit);
        } else {
            tail.push((cluster, hit));
        }
    }
    // fold a multi-lead tail into one rollup; a tail of one just renders
    if tail.len() == 1 {
        hits.push(tail.remove(0).1);
    } else if !tail.is_empty() {
        hits.push(rollup_hit(&tail));
    }
    hits
}

/// Build the seeded lead for one family cluster.
///
/// # Arguments
///
/// * `cluster` - The family cluster to render
/// * `regions` - The mapped file regions for offset attribution
fn cluster_hit(cluster: &FamilyCluster, regions: &[FileRegion]) -> DetectionHit {
    // the merged, deduped match sites across every member
    let sites = cluster.sites();
    // classify each site against the file map (most specific region wins)
    let placements: Vec<Option<&FileRegion>> =
        sites.iter().map(|site| locate(regions, *site)).collect();
    // the structural facts the severity and reasoning rules below key on
    let all_versioninfo = !sites.is_empty()
        && placements
            .iter()
            .all(|placed| matches!(placed, Some(region) if region.kind == RegionKind::VersionInfo));
    let embedded = placements
        .iter()
        .filter(|placed| matches!(placed, Some(region) if region.kind == RegionKind::EmbeddedPe))
        .count();
    // class-tier the base severity: self-declared weak classes rank low
    let weak_only = cluster
        .members
        .iter()
        .all(|member| weak_rule_class(&member.identifier));
    // corroboration upgrades a family-class lead that independent sources
    // agree on — never a weak-only cluster or version-metadata-only matches
    let corroborated = !weak_only && !all_versioninfo && cluster.corroborated();
    // settle the final severity from those facts
    let severity = if all_versioninfo {
        low()
    } else if corroborated {
        high()
    } else if weak_only {
        low()
    } else {
        medium()
    };
    // every lead keeps the identification-lead framing
    let mut reasoning = String::from(
        "A curated signature matched the sample's bytes. The rule name is \
         content-derived evidence — treat it as an identification lead and \
         verify the behaviors it implies against the binary.",
    );
    // corroborated families carry the independent-agreement line
    if corroborated {
        let _ = write!(
            reasoning,
            " {} independent signatures agree on family '{}', which makes a \
             coincidental match unlikely.",
            cluster.independent_sources(),
            cluster.token,
        );
    }
    // self-declared weak classes warn the reader off over-trusting the name
    if weak_only {
        reasoning.push_str(
            " Heuristic/cluster rule — weak family evidence: this rule class \
             (SUSP_/Generic_Threat/_Unknown_/packer) also fires on benign or \
             generic content.",
        );
    }
    // matches living entirely in version metadata are generic strings, not code
    if all_versioninfo {
        reasoning.push_str(
            " Every match offset falls inside version-resource metadata \
             (VERSIONINFO strings) — generic version text rather than \
             family-specific code, so this identification is unsupported \
             without other evidence.",
        );
    }
    // embedded-PE matches describe the embedded object, not the host's code
    if embedded > 0 {
        reasoning.push_str(if embedded == sites.len() {
            " All matches land inside an embedded PE object — verify that \
             embedded object (carve and analyze it) rather than attributing \
             the identity to the host's own code."
        } else {
            " Some matches land inside an embedded PE object — for those, \
             verify the embedded object (carve and analyze it) rather than \
             attributing the identity to the host's own code."
        });
    }
    // render the lead body with per-member offsets, excerpts, and location
    let location = location_summary(&placements);
    let content = cluster_content(cluster, location.as_deref());
    // the corroborated marker doubles as the lead's entity name; the seeder's
    // question derivation reads it back through corroborated_family
    let name = corroborated.then(|| format!("{FAMILY_LEAD_PREFIX}{}", cluster.token));
    DetectionHit {
        technique: YARA_SIGNATURE,
        address: sites.first().copied().unwrap_or(0),
        function: None,
        kind: EvidenceKind::ContextOnly,
        confidence: severity.0,
        suspicion: severity.1,
        content,
        name,
        reasoning,
        sites,
        source: HitSource::Yara,
        artifact: None,
        context: None,
        // unstamped here; the registry runner names native producers
        detection: None,
        // not an API-catalog hit: no catalog name or withheld severity
        api: None,
        catalog_severity: None,
        always_noise: false,
    }
}

/// Build the lead body for one family cluster.
///
/// # Arguments
///
/// * `cluster` - The family cluster to render
/// * `location` - The structural location line, when the offsets mapped
fn cluster_content(cluster: &FamilyCluster, location: Option<&str>) -> String {
    // header line: single rules keep the familiar shape; merges name the family
    let mut content = if let [member] = cluster.members.as_slice() {
        format!(
            "YARA rule {}:{} matched the raw file (match offsets: {})",
            member.namespace,
            member.identifier,
            fmt_offsets(&member.sites),
        )
    } else {
        // count the independent sources once for the header grammar
        let sources = cluster.independent_sources();
        format!(
            "YARA family '{}': {} rules from {} independent {} matched the raw file",
            cluster.token,
            cluster.members.len(),
            sources,
            if sources == 1 { "source" } else { "sources" },
        )
    };
    // a merged lead lists every member rule with its own offsets and excerpts
    if cluster.members.len() > 1 {
        for member in &cluster.members {
            let _ = write!(
                content,
                "\n- {}:{} (offsets: {})",
                member.namespace,
                member.identifier,
                fmt_offsets(&member.sites),
            );
            if !member.excerpts.is_empty() {
                let _ = write!(content, "\n  Matched: {}", member.excerpts.join(" | "));
            }
        }
    } else if let Some(member) = cluster.members.first()
        && !member.excerpts.is_empty()
    {
        // a single rule carries its excerpts directly under the header
        let _ = write!(content, "\nMatched: {}", member.excerpts.join(" | "));
    }
    // the structural location line, when the offsets mapped to anything
    if let Some(location) = location {
        let _ = write!(content, "\nLocation: {location}");
    }
    content
}

/// Render a bounded offset list, or `none recorded` for a site-less rule.
///
/// # Arguments
///
/// * `sites` - The match-site file offsets to render
fn fmt_offsets(sites: &[u64]) -> String {
    // a condition-only rule (filesize, module checks) records no sites
    if sites.is_empty() {
        return "none recorded".to_owned();
    }
    // lowercase 0x-prefixed hex, matching the crate's address rendering
    sites
        .iter()
        .map(|site| format!("{site:#x}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Fold the leads past the render cap into one summary hit.
///
/// # Arguments
///
/// * `tail` - The folded clusters with their rendered leads, strongest first
fn rollup_hit(tail: &[(&FamilyCluster, DetectionHit)]) -> DetectionHit {
    // name every folded rule with its first match offset so nothing is lost
    let mut named: Vec<String> = Vec::new();
    let mut sites: Vec<u64> = Vec::new();
    for (cluster, _) in tail {
        for member in &cluster.members {
            // the first offset anchors the name; site-less rules show bare
            let first = member.sites.first().copied();
            named.push(match first {
                Some(offset) => format!("{}:{}@{offset:#x}", member.namespace, member.identifier),
                None => format!("{}:{}", member.namespace, member.identifier),
            });
            // carry a bounded site list so the rollup still points into the file
            if let Some(offset) = first
                && sites.len() < MAX_SITES
                && !sites.contains(&offset)
            {
                sites.push(offset);
            }
        }
    }
    // the tail is sorted strongest-first, so its head bounds the severity
    let strongest = &tail[0].1;
    DetectionHit {
        technique: YARA_SIGNATURE,
        address: sites.first().copied().unwrap_or(0),
        function: None,
        kind: EvidenceKind::ContextOnly,
        confidence: strongest.confidence,
        suspicion: strongest.suspicion,
        content: format!(
            "{} more YARA {} matched: {}",
            named.len(),
            if named.len() == 1 { "rule" } else { "rules" },
            named.join(", "),
        ),
        name: None,
        reasoning: "Lower-ranked signature matches folded into one summary lead so the \
                    strongest identifications stay readable. Each folded rule is named \
                    with its first match offset; engage one only if its name matters to \
                    the investigation."
            .to_owned(),
        sites,
        source: HitSource::Yara,
        artifact: None,
        context: None,
        // unstamped here; the registry runner names native producers
        detection: None,
        // not an API-catalog hit: no catalog name or withheld severity
        api: None,
        catalog_severity: None,
        always_noise: false,
    }
}

/// The family token of a corroborated YARA cluster lead, when the hit is one.
///
/// The seeder's question derivation keys on this accessor to spot the merged
/// multi-source family leads worth one identification question per sample.
/// The marker lives in the hit's entity name (stamped only on corroborated,
/// `Likely`-upgraded clusters), and this is the only reader of that encoding.
///
/// # Arguments
///
/// * `hit` - The detection hit to inspect
// The accessor exists for the seeder's question derivation; nothing in the
// detect pass itself reads the marker back, so allow it to be unused here.
#[allow(dead_code)]
#[must_use]
pub fn corroborated_family(hit: &DetectionHit) -> Option<&str> {
    // only the yara detector stamps the marker
    if !matches!(hit.source, HitSource::Yara) {
        return None;
    }
    // the marker is the entity-name prefix on corroborated cluster leads
    hit.name.as_deref()?.strip_prefix(FAMILY_LEAD_PREFIX)
}

/// Split a rule identifier into its recognized source label and remainder.
///
/// # Arguments
///
/// * `identifier` - The rule identifier to split
fn split_source(identifier: &str) -> (Option<&'static str>, &str) {
    // compare case-insensitively; packs vary identifier casing by author
    let upper = identifier.to_ascii_uppercase();
    for (prefix, label) in SOURCE_PREFIXES.iter().copied() {
        if upper.starts_with(prefix) {
            return (Some(label), &identifier[prefix.len()..]);
        }
    }
    (None, identifier)
}

/// Reduce a rule identifier to its salient family token.
///
/// Strips the recognized source/author prefix, drops generic platform/kind
/// words, and peels trailing qualifier tokens (`_Auto`, 8-hex rule ids, date
/// stamps like `_Feb24_1`, versions like `_V2`), then lowercases what
/// remains, so alias rules for one family reduce to the same token.
///
/// # Arguments
///
/// * `identifier` - The rule identifier to reduce
fn family_token(identifier: &str) -> String {
    // strip the recognized source/author prefix first
    let (_, rest) = split_source(identifier);
    // split the remainder into underscore-separated tokens
    let mut tokens: Vec<&str> = rest.split('_').filter(|token| !token.is_empty()).collect();
    // drop the generic platform/kind words wherever they appear
    tokens.retain(|token| !is_generic_token(token));
    // peel trailing qualifiers, always keeping at least one salient token
    while tokens.len() > 1 && is_trailing_qualifier(tokens[tokens.len() - 1]) {
        tokens.pop();
    }
    // an identifier that was all noise falls back to its lowercased self
    if tokens.is_empty() {
        return identifier.to_ascii_lowercase();
    }
    // the lowercased salient remainder is the family token
    tokens.join("_").to_ascii_lowercase()
}

/// Whether a token is a generic platform/kind word carrying no family identity.
///
/// # Arguments
///
/// * `token` - The identifier token to test
fn is_generic_token(token: &str) -> bool {
    // compare case-insensitively; packs vary casing by author
    matches!(
        token.to_ascii_uppercase().as_str(),
        "WIN"
            | "WIN32"
            | "WIN64"
            | "WINDOWS"
            | "LNX"
            | "LINUX"
            | "MAC"
            | "MACOS"
            | "OSX"
            | "MAL"
            | "MALWARE"
            | "HKTL"
            | "IMPLANT"
            | "BACKDOOR"
    )
}

/// Whether a token is a trailing qualifier rather than part of a family name.
///
/// # Arguments
///
/// * `token` - The identifier token to test
fn is_trailing_qualifier(token: &str) -> bool {
    // the MALPEDIA machine-generated n-gram marker
    if token.eq_ignore_ascii_case("auto") {
        return true;
    }
    // elastic's 8-hex rule ids (e.g. 41db1d4d)
    if token.len() == 8 && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return true;
    }
    // version suffixes like V2
    if let Some(digits) = token.strip_prefix(['v', 'V'])
        && !digits.is_empty()
        && digits.len() <= 3
        && digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return true;
    }
    // short counters like the 1 in _Feb24_1 (longer numbers such as the 044
    // in Unidentified_044 are family-distinguishing and must survive)
    if !token.is_empty() && token.len() <= 2 && token.bytes().all(|byte| byte.is_ascii_digit()) {
        return true;
    }
    // month+year date stamps like Feb24 or Feb2024
    if token.is_ascii() && (5..=7).contains(&token.len()) {
        let (month, year) = token.split_at(3);
        let month_known = matches!(
            month.to_ascii_uppercase().as_str(),
            "JAN"
                | "FEB"
                | "MAR"
                | "APR"
                | "MAY"
                | "JUN"
                | "JUL"
                | "AUG"
                | "SEP"
                | "OCT"
                | "NOV"
                | "DEC"
        );
        let year_digits =
            (year.len() == 2 || year.len() == 4) && year.bytes().all(|byte| byte.is_ascii_digit());
        if month_known && year_digits {
            return true;
        }
    }
    false
}

/// Whether a rule identifier self-declares a weak, low-precision class.
///
/// Matches `SUSP_` heuristics, elastic `Generic_Threat` cluster rules,
/// unattributed `_Unknown_` cluster rules, and packer/UPX structural rules
/// (which duplicate the native packing detector). Checked against every
/// confirmed identifier in the telemetry pack: none matches —
/// `Windows_Generic_MalCert` is `Generic_MalCert`, not `Generic_Threat`, and
/// `_Auto` rules deliberately stay medium (Stealc, Tidepool, and
/// `Unidentified_044` were all confirmed from that class).
///
/// # Arguments
///
/// * `identifier` - The rule identifier to classify
fn weak_rule_class(identifier: &str) -> bool {
    // token the identifier so substrings of family names cannot false-match
    let upper = identifier.to_ascii_uppercase();
    let tokens: Vec<&str> = upper.split('_').filter(|token| !token.is_empty()).collect();
    // SUSP marks a suspicious-trait heuristic, not a family signature
    if tokens.contains(&"SUSP") {
        return true;
    }
    // Unknown marks an unattributed cluster rule
    if tokens.contains(&"UNKNOWN") {
        return true;
    }
    // packer/UPX structural rules duplicate the native packing detector
    if tokens.contains(&"PACKER") || tokens.contains(&"UPX") {
        return true;
    }
    // elastic's Generic_Threat clusters (Generic alone is not enough: the
    // confirmed Generic_MalCert class must stay medium)
    if tokens
        .windows(2)
        .any(|pair| matches!(pair, ["GENERIC", "THREAT"]))
    {
        return true;
    }
    false
}

/// Render matched bytes as a short, sanitized excerpt.
///
/// Printable UTF-16LE runs render as `u"…"` quoted text, printable ASCII as
/// quoted text, and anything else as a short hex snippet; input is capped at
/// [`MAX_EXCERPT_BYTES`] (hex display at [`MAX_HEX_SHOWN`] bytes) so a giant
/// match cannot bloat the lead body.
///
/// # Arguments
///
/// * `data` - The matched bytes to render
fn render_excerpt(data: &[u8]) -> String {
    // cap the bytes considered and remember whether the match kept going
    let truncated = data.len() > MAX_EXCERPT_BYTES;
    let data = &data[..data.len().min(MAX_EXCERPT_BYTES)];
    let suffix = if truncated { "…" } else { "" };
    // printable UTF-16LE text renders as a quoted string with a utf16 marker
    if let Some(text) = utf16_text(data) {
        return format!("u\"{}\"{suffix}", escape_quotes(&text));
    }
    // printable ASCII renders as a plain quoted string
    if data.len() >= 2 && data.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
        let text: String = data.iter().map(|&byte| byte as char).collect();
        return format!("\"{}\"{suffix}", escape_quotes(&text));
    }
    // everything else renders as a bounded hex snippet
    let shown = &data[..data.len().min(MAX_HEX_SHOWN)];
    let hex: Vec<String> = shown.iter().map(|byte| format!("{byte:02x}")).collect();
    let more = if truncated || data.len() > MAX_HEX_SHOWN {
        "…"
    } else {
        ""
    };
    format!("hex {}{more}", hex.join(" "))
}

/// Decode a buffer as printable UTF-16LE text, if that is what it holds.
///
/// # Arguments
///
/// * `data` - The bytes to decode
fn utf16_text(data: &[u8]) -> Option<String> {
    // utf-16 text needs at least two full code units, in whole pairs
    if data.len() < 4 || data.len() % 2 != 0 {
        return None;
    }
    // decode pairwise: printable ASCII low bytes under zero high bytes only
    let (pairs, _) = data.as_chunks::<2>();
    let mut text = String::new();
    for pair in pairs {
        if pair[1] != 0 || !(0x20..=0x7e).contains(&pair[0]) {
            return None;
        }
        text.push(pair[0] as char);
    }
    Some(text)
}

/// Escape quotes and backslashes so an excerpt cannot break its quoting.
///
/// # Arguments
///
/// * `text` - The text to escape
fn escape_quotes(text: &str) -> String {
    // backslashes first so freshly escaped quotes do not double-escape
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A classified byte range of the raw file a match offset can land in.
struct FileRegion {
    /// The region's start file offset (inclusive).
    start: u64,
    /// The region's end file offset (exclusive).
    end: u64,
    /// What the range is, structurally.
    kind: RegionKind,
}

/// The structural classes a file range can belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RegionKind {
    /// A `VS_VERSION_INFO` resource block (the host's or an embedded object's).
    VersionInfo,
    /// An embedded foreign PE image (never the host's own image at offset 0).
    EmbeddedPe,
    /// The Authenticode certificate table the security directory declares.
    CertTable,
    /// A declared section's on-disk extent.
    Section(String),
    /// Bytes past every declared section (the overlay).
    Overlay,
    /// The header region before the first declared section.
    Header,
}

impl RegionKind {
    /// The classification priority when regions overlap (lower wins).
    ///
    /// Version blocks sit inside sections (and inside embedded images), and
    /// embedded images sit inside sections or the overlay, so the most
    /// specific claim must win the lookup.
    fn priority(&self) -> u8 {
        // most specific first: metadata block, embedded image, cert table,
        // declared section, then the structural catch-alls
        match self {
            RegionKind::VersionInfo => 0,
            RegionKind::EmbeddedPe => 1,
            RegionKind::CertTable => 2,
            RegionKind::Section(_) => 3,
            RegionKind::Overlay => 4,
            RegionKind::Header => 5,
        }
    }
}

impl FileRegion {
    /// Whether a file offset falls inside this region.
    ///
    /// # Arguments
    ///
    /// * `offset` - The file offset to test
    fn contains(&self, offset: u64) -> bool {
        // half-open interval, matching every extent above
        offset >= self.start && offset < self.end
    }

    /// The human-readable label location lines use for this region.
    fn label(&self) -> String {
        // name the region with its extent where the extent adds information
        match &self.kind {
            RegionKind::VersionInfo => format!(
                "version-resource metadata (VERSIONINFO @{:#x}-{:#x})",
                self.start, self.end
            ),
            RegionKind::EmbeddedPe => format!("embedded PE @{:#x}-{:#x}", self.start, self.end),
            RegionKind::CertTable => {
                format!("the certificate table @{:#x}-{:#x}", self.start, self.end)
            }
            RegionKind::Section(name) => format!("section {name}"),
            RegionKind::Overlay => format!("the overlay @{:#x}-{:#x}", self.start, self.end),
            RegionKind::Header => "the file header".to_owned(),
        }
    }
}

/// Map the raw file into classified regions for match-offset attribution.
///
/// Version blocks and embedded PE images are swept format-independently (they
/// matter wherever they sit); the host's declared layout joins the map when
/// its header parses. The result is sorted most-specific-first so [`locate`]
/// resolves overlaps with one linear scan.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
fn file_regions(raw: &[u8]) -> Vec<FileRegion> {
    // collect regions from every mapper, specific metadata first
    let mut regions: Vec<FileRegion> = Vec::new();
    // version-resource blocks (the host's own or an embedded object's)
    for (start, end) in version_info_blocks(raw) {
        regions.push(FileRegion {
            start,
            end,
            kind: RegionKind::VersionInfo,
        });
    }
    // embedded foreign PE images, wherever they sit
    for (start, end) in embedded_pe_ranges(raw) {
        regions.push(FileRegion {
            start,
            end,
            kind: RegionKind::EmbeddedPe,
        });
    }
    // the host's declared layout, when its header parses
    if let Some(header) = host_header(raw) {
        // each declared section's on-disk extent
        for section in &header.sections {
            if section.raw_size > 0 {
                regions.push(FileRegion {
                    start: section.raw_offset,
                    end: section.raw_offset.saturating_add(section.raw_size),
                    kind: RegionKind::Section(section.name.clone()),
                });
            }
        }
        // the certificate table, when the security directory declares one
        if let Some((offset, size)) = header.security_dir {
            regions.push(FileRegion {
                start: offset,
                end: offset.saturating_add(size),
                kind: RegionKind::CertTable,
            });
        }
        // the overlay: bytes past every declared section
        if let Some(image_end) = header.image_end()
            && image_end < raw.len() as u64
        {
            regions.push(FileRegion {
                start: image_end,
                end: raw.len() as u64,
                kind: RegionKind::Overlay,
            });
        }
        // the header region before the first declared section's bytes
        let first_section = header
            .sections
            .iter()
            .filter(|section| section.raw_size > 0)
            .map(|section| section.raw_offset)
            .min();
        if let Some(first) = first_section
            && first > 0
        {
            regions.push(FileRegion {
                start: 0,
                end: first,
                kind: RegionKind::Header,
            });
        }
    }
    // most-specific regions first so overlap resolution is one linear find
    regions.sort_by_key(|region| (region.kind.priority(), region.start));
    regions
}

/// Parse the host executable header by sniffing the magic bytes.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
fn host_header(raw: &[u8]) -> Option<fileinfo::FormatHeader> {
    // PE hosts start with the DOS magic
    if raw.get(..2) == Some(b"MZ".as_slice()) {
        return fileinfo::parse_header(raw, Some("portable executable"));
    }
    // ELF hosts start with the ELF magic
    if raw.get(..4) == Some(b"\x7fELF".as_slice()) {
        return fileinfo::parse_header(raw, Some("elf"));
    }
    None
}

/// Sweep the raw bytes for `VS_VERSION_INFO` block extents.
///
/// Each block's declared length (the `u16` six bytes before the UTF-16LE key)
/// covers the whole block including its string tables, which is exactly the
/// extent version-metadata matches should be attributed to.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
fn version_info_blocks(raw: &[u8]) -> Vec<(u64, u64)> {
    // the UTF-16LE key every version block carries
    let anchor: Vec<u8> = "VS_VERSION_INFO"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    // collect bounded block extents from each anchor occurrence
    let mut blocks: Vec<(u64, u64)> = Vec::new();
    let mut from = 0usize;
    while blocks.len() < MAX_VERSION_BLOCKS {
        // find the next anchor, stopping at end of file
        let Some(at) = find(raw, &anchor, from) else {
            break;
        };
        from = at + 1;
        // the block header (length/value-length/type words) precedes the key
        let Some(start) = at.checked_sub(6) else {
            continue;
        };
        let Some(bytes) = raw.get(start..start + 2) else {
            continue;
        };
        let length = usize::from(u16::from_le_bytes([bytes[0], bytes[1]]));
        // reject lengths too short to even cover the header and key
        if length < 6 + anchor.len() {
            continue;
        }
        // clamp the declared extent to the file
        let end = (start + length).min(raw.len());
        blocks.push((start as u64, end as u64));
    }
    blocks
}

/// Sweep the raw bytes for embedded foreign PE images and their extents.
///
/// A candidate is an `MZ` pair past offset 0 whose `e_lfanew` points at a
/// `PE\0\0` signature; its extent is the declared end of its section table
/// (the same measure [`fileinfo::FormatHeader::image_end`] gives the host).
/// Candidates inside an already-mapped image are skipped so one dropper
/// payload maps once.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
fn embedded_pe_ranges(raw: &[u8]) -> Vec<(u64, u64)> {
    // walk MZ candidates in file order under both bounds
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut from = 1usize;
    let mut parses = 0usize;
    while ranges.len() < MAX_EMBEDDED_PES && parses < MAX_EMBEDDED_CANDIDATES {
        // find the next DOS magic past the host's own header
        let Some(at) = find(raw, b"MZ", from) else {
            break;
        };
        from = at + 1;
        // skip candidates inside an already-mapped embedded image
        if ranges
            .iter()
            .any(|(start, end)| (at as u64) >= *start && (at as u64) < *end)
        {
            continue;
        }
        // a real DOS header points at a PE signature inside the file
        let Some(pe_at) = read_u32(raw, at + 0x3c) else {
            continue;
        };
        let Ok(pe_at) = usize::try_from(pe_at) else {
            continue;
        };
        let Some(sig_at) = at.checked_add(pe_at) else {
            continue;
        };
        if raw.get(sig_at..sig_at + 4) != Some(b"PE\0\0".as_slice()) {
            continue;
        }
        // parse the candidate like a file of its own to learn its extent
        parses += 1;
        let Some(header) = fileinfo::parse_header(&raw[at..], Some("portable executable")) else {
            continue;
        };
        let Some(image_end) = header.image_end() else {
            continue;
        };
        // re-base the extent onto the host file and clamp it to the file
        let end = (at as u64).saturating_add(image_end).min(raw.len() as u64);
        if end > at as u64 {
            ranges.push((at as u64, end));
        }
    }
    ranges
}

/// Classify one match offset against the mapped regions.
///
/// # Arguments
///
/// * `regions` - The mapped regions, sorted most-specific-first
/// * `offset` - The match-site file offset to classify
fn locate(regions: &[FileRegion], offset: u64) -> Option<&FileRegion> {
    // regions are pre-sorted most-specific-first, so first hit wins
    regions.iter().find(|region| region.contains(offset))
}

/// Render the location line summarizing where a lead's matches sit.
///
/// Returns `None` when no site mapped to any structure (a bare-offset lead
/// stays bare rather than claiming an empty location).
///
/// # Arguments
///
/// * `placements` - Each site's classified region, in site order
fn location_summary(placements: &[Option<&FileRegion>]) -> Option<String> {
    // no structural info at all means no line
    if placements.iter().all(Option::is_none) {
        return None;
    }
    // bucket the sites per label, preserving first-seen order
    let mut labels: Vec<(String, usize)> = Vec::new();
    for placement in placements {
        let label =
            placement.map_or_else(|| "unattributed file ranges".to_owned(), FileRegion::label);
        match labels.iter_mut().find(|(existing, _)| *existing == label) {
            Some((_, count)) => *count += 1,
            None => labels.push((label, 1)),
        }
    }
    // a single shared label reads as one claim about every match
    if let [(label, count)] = labels.as_slice() {
        return Some(if *count == 1 {
            format!("match inside {label}")
        } else {
            format!("all {count} matches inside {label}")
        });
    }
    // mixed placements list per-label counts in first-seen order
    let parts: Vec<String> = labels
        .iter()
        .map(|(label, count)| format!("{count} in {label}"))
        .collect();
    Some(format!("matches: {}", parts.join(", ")))
}

/// Find a byte pattern in a haystack, starting from an offset.
///
/// # Arguments
///
/// * `haystack` - The bytes to search
/// * `needle` - The pattern to find
/// * `from` - The offset to start searching at
fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    // a plain windows scan; every caller's needle is short
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
}

/// Read a little-endian `u32` at `at`, or `None` if it runs past the end.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
fn read_u32(raw: &[u8], at: usize) -> Option<u32> {
    // a bounds-checked slice keeps the header sweeps total
    raw.get(at..at + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::confidence::Confidence;

    /// A tiny inline rule matches a fixture buffer and maps onto a hit.
    #[test]
    fn scan_matches_an_inline_rule() {
        // write a one-rule pack into a scratch dir
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = dir.path().join("test.yar");
        std::fs::write(
            &pack,
            "rule corn_marker { strings: $a = \"CORNFIELD\" condition: $a }",
        )
        .expect("pack should write");
        // the fixture buffer carries the marker twice
        let raw = b"....CORNFIELD....CORNFIELD....";
        let hits = scan(&pack, raw).expect("scan should succeed");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("corn_marker"));
        // the matched bytes are quoted in the lead body
        assert!(hits[0].content.contains("0x4: \"CORNFIELD\""));
        assert_eq!(hits[0].sites.len(), 2);
        assert_eq!(hits[0].address, 4);
        assert!(matches!(hits[0].source, HitSource::Yara));
        // a non-matching buffer yields no hits
        let none = scan(&pack, b"nothing here").expect("scan should succeed");
        assert!(none.is_empty());
    }

    /// Alias rules for one family in a single pack merge into one Likely lead.
    #[test]
    fn scan_merges_alias_rules_from_one_pack() {
        // two sources' alias rules for the same family, in one pack file
        let dir = tempfile::tempdir().expect("tempdir");
        let pack = dir.path().join("aliases.yar");
        std::fs::write(
            &pack,
            "rule MALPEDIA_Win_Cornfam_Auto { strings: $a = \"CORNFIELD\" condition: $a }\n\
             rule RUSSIANPANDA_Win_Mal_Cornfam_V2 { strings: $a = \"CORNFIELD\" condition: $a }",
        )
        .expect("pack should write");
        let raw = b"....CORNFIELD....";
        let hits = scan(&pack, raw).expect("scan should succeed");
        // one merged lead, corroborated across the two source prefixes
        assert_eq!(hits.len(), 1);
        assert!(matches!(hits[0].confidence, Confidence::Likely));
        assert_eq!(hits[0].suspicion, 75);
        assert!(hits[0].content.contains("MALPEDIA_Win_Cornfam_Auto"));
        assert!(hits[0].content.contains("RUSSIANPANDA_Win_Mal_Cornfam_V2"));
        assert_eq!(corroborated_family(&hits[0]), Some("cornfam"));
    }

    /// Family tokens strip source prefixes, generic words, and qualifiers.
    #[test]
    fn family_token_strips_source_and_generic_noise() {
        // the Prolock alias trio from three sources reduces to one token
        assert_eq!(family_token("R3C0NST_Prolock_Malware"), "prolock");
        assert_eq!(family_token("DITEKSHEN_MALWARE_Win_Prolock"), "prolock");
        assert_eq!(
            family_token("SIGNATURE_BASE_MAL_Prolock_Malware"),
            "prolock"
        );
        // the confirmed Stealc trio reduces to one token
        assert_eq!(family_token("MALPEDIA_Win_Stealc_Auto"), "stealc");
        assert_eq!(family_token("RUSSIANPANDA_Win_Mal_Stealc_V2"), "stealc");
        assert_eq!(family_token("Windows_Trojan_Stealc_41db1d4d"), "stealc");
        // numeric family designators survive qualifier peeling
        assert_eq!(
            family_token("MALPEDIA_Win_Unidentified_044_Auto"),
            "unidentified_044"
        );
        // date-stamped cluster rules peel the date and counter
        assert_eq!(
            family_token("SIGNATURE_BASE_MAL_Beacon_Unknown_Feb24_1"),
            "beacon_unknown"
        );
        // the confirmed cert rule keeps its salient token
        assert_eq!(family_token("Windows_Generic_MalCert_568023ba"), "malcert");
    }

    /// The confirmed Stealc alias trio merges into one corroborated lead.
    #[test]
    fn stealc_alias_trio_merges_into_one_corroborated_lead() {
        // the three confirmed alias rules, as one combined pack would ship them
        let matches = vec![
            rule("pack.yar", "MALPEDIA_Win_Stealc_Auto", &[0x1234, 0x5678]),
            rule("pack.yar", "RUSSIANPANDA_Win_Mal_Stealc_V2", &[0x1234]),
            rule("pack.yar", "Windows_Trojan_Stealc_41db1d4d", &[0x9abc]),
        ];
        let hits = build_hits(matches, &[]);
        // one merged lead carrying every member rule and its offsets
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("YARA family 'stealc'"));
        assert!(hits[0].content.contains("MALPEDIA_Win_Stealc_Auto"));
        assert!(hits[0].content.contains("RUSSIANPANDA_Win_Mal_Stealc_V2"));
        assert!(hits[0].content.contains("Windows_Trojan_Stealc_41db1d4d"));
        // corroboration upgrades the merged lead to Likely/75
        assert!(matches!(hits[0].confidence, Confidence::Likely));
        assert_eq!(hits[0].suspicion, 75);
        assert!(hits[0].reasoning.contains("independent signatures agree"));
        // the seeder's accessor reads the family marker back off the hit
        assert_eq!(corroborated_family(&hits[0]), Some("stealc"));
        // merged sites are deduped across the aliases
        assert_eq!(hits[0].sites, vec![0x1234, 0x5678, 0x9abc]);
    }

    /// Solo confirmed rules keep seeding unchanged at medium severity.
    #[test]
    fn solo_confirmed_rules_still_seed_at_medium() {
        // the confirmed single-rule patterns from the telemetry pack
        let matches = vec![
            rule("pack.yar", "MALPEDIA_Win_Tidepool_Auto", &[0x10]),
            rule("pack.yar", "MALPEDIA_Win_Unidentified_044_Auto", &[0x20]),
            rule(
                "pack.yar",
                "SIGNATURE_BASE_HKTL_Meterpreter_Inmemory",
                &[0x30],
            ),
            rule("pack.yar", "Windows_Generic_MalCert_568023ba", &[0x40]),
        ];
        let hits = build_hits(matches, &[]);
        // no false merging: each confirmed solo rule seeds its own lead
        assert_eq!(hits.len(), 4);
        for hit in &hits {
            // every solo lead stays at the medium tier with no marker
            assert!(matches!(hit.confidence, Confidence::Unsure));
            assert_eq!(hit.suspicion, 40);
            assert_eq!(corroborated_family(hit), None);
        }
    }

    /// Self-declared weak rule classes rank low with the class note.
    #[test]
    fn weak_rule_classes_rank_low_with_the_class_note() {
        // the disputed _Unknown_ cluster rule and friends from the weak classes
        let matches = vec![
            rule(
                "pack.yar",
                "SIGNATURE_BASE_MAL_Beacon_Unknown_Feb24_1",
                &[0x100],
            ),
            rule("pack.yar", "SIGNATURE_BASE_SUSP_XORed_URL", &[0x200]),
            rule("pack.yar", "Windows_Generic_Threat_2bb6f41d", &[0x300]),
            rule("pack.yar", "SIGNATURE_BASE_Packer_UPX_Generic", &[0x400]),
        ];
        let hits = build_hits(matches, &[]);
        assert_eq!(hits.len(), 4);
        for hit in &hits {
            // the weak classes demote to Untrusted/15 and say why
            assert!(matches!(hit.confidence, Confidence::Untrusted));
            assert_eq!(hit.suspicion, 15);
            assert!(hit.reasoning.contains("weak family evidence"));
        }
        // the confirmed MalCert class is Generic_MalCert, not Generic_Threat,
        // and must stay medium
        let kept = build_hits(
            vec![rule(
                "pack.yar",
                "Windows_Generic_MalCert_568023ba",
                &[0x40],
            )],
            &[],
        );
        assert!(matches!(kept[0].confidence, Confidence::Unsure));
        assert_eq!(kept[0].suspicion, 40);
    }

    /// Matches living entirely in version-resource metadata demote to low.
    #[test]
    fn version_metadata_only_matches_demote_to_low() {
        // a version block at the disputed Beacon lead's real geometry
        // (0x44ba6-0x44d1c) carrying the generic '1.0.0.0' version strings
        let mut raw = vec![0u8; 0x4_6000];
        write_version_block(&mut raw);
        // a family-class rule whose only matches are those version strings
        let matches = vec![rule(
            "pack.yar",
            "RUSSIANPANDA_Win_Mal_Fakefam_V2",
            &[0x44bf0, 0x44c84],
        )];
        let hits = build_hits(matches, &raw);
        assert_eq!(hits.len(), 1);
        // the all-in-VERSIONINFO demotion fires: Untrusted/15 plus the note
        assert!(matches!(hits[0].confidence, Confidence::Untrusted));
        assert_eq!(hits[0].suspicion, 15);
        assert!(hits[0].reasoning.contains("version-resource metadata"));
        // the location line attributes every match to the block's extent
        assert!(hits[0].content.contains(
            "all 2 matches inside version-resource metadata (VERSIONINFO @0x44ba6-0x44d1c)"
        ));
    }

    /// The real Beacon offsets annotate the version block without demoting.
    #[test]
    fn beacon_partial_versioninfo_matches_annotate_without_demoting() {
        // the disputed lead's real geometry: 2 of 8 offsets in the block
        let mut raw = vec![0u8; 0xa_0000];
        write_version_block(&mut raw);
        let matches = vec![rule(
            "pack.yar",
            "SIGNATURE_BASE_MAL_Beacon_Unknown_Feb24_1",
            &[
                0x44bf0, 0x44c84, 0x9d1f3, 0x9d287, 0x41f15, 0x4229d, 0x9a518, 0x9a8a0,
            ],
        )];
        let hits = build_hits(matches, &raw);
        assert_eq!(hits.len(), 1);
        // the version-metadata demotion must NOT fire (6 of 8 sit outside)
        assert!(
            !hits[0]
                .reasoning
                .contains("Every match offset falls inside")
        );
        // the _Unknown_ class demotion covers it instead
        assert!(matches!(hits[0].confidence, Confidence::Untrusted));
        assert!(hits[0].reasoning.contains("weak family evidence"));
        // the mixed location line shows the split the dispute had to prove
        assert!(hits[0].content.contains("2 in version-resource metadata"));
    }

    /// Matches inside an embedded PE keep severity and gain carve guidance.
    #[test]
    fn embedded_pe_matches_keep_severity_with_carve_guidance() {
        // the Mikoponi dispute's real geometry: an embedded PE spanning
        // 0x252c-0x2858c inside a larger host image
        let mut raw = build_pe(&[(".text", 0x400, 0x2fc00)], None, 0x3_0000);
        let embedded = build_pe(&[(".data", 0x400, 0x25c60)], None, 0x200);
        raw[0x252c..0x252c + embedded.len()].copy_from_slice(&embedded);
        // the embedded sweep maps the declared extent
        assert_eq!(embedded_pe_ranges(&raw), vec![(0x252c, 0x2858c)]);
        // the disputed lead's real 8 match offsets, all inside that extent
        let matches = vec![rule(
            "pack.yar",
            "MALPEDIA_Win_Mikoponi_Auto",
            &[
                0x50ac, 0x4a70, 0x42c4, 0xa2d9, 0x35d9, 0x4ee7, 0x2a5e, 0xa015,
            ],
        )];
        let hits = build_hits(matches, &raw);
        assert_eq!(hits.len(), 1);
        // embedded-PE attribution annotates but never demotes
        assert!(matches!(hits[0].confidence, Confidence::Unsure));
        assert_eq!(hits[0].suspicion, 40);
        assert!(
            hits[0]
                .content
                .contains("all 8 matches inside embedded PE @0x252c-0x2858c")
        );
        assert!(hits[0].reasoning.contains("embedded PE object"));
        assert!(hits[0].reasoning.contains("carve"));
    }

    /// Certificate-table matches stay medium and annotate the location only.
    #[test]
    fn certificate_table_matches_stay_medium_annotate_only() {
        // a host whose security directory declares a certificate table
        let raw = build_pe(&[(".text", 0x400, 0xc00)], Some((0x1000, 0x800)), 0x1800);
        // the confirmed MalCert pattern matches the certificate overlay by design
        let matches = vec![rule(
            "pack.yar",
            "Windows_Generic_MalCert_568023ba",
            &[0x1100, 0x1400],
        )];
        let hits = build_hits(matches, &raw);
        assert_eq!(hits.len(), 1);
        // annotate-only: severity stays medium
        assert!(matches!(hits[0].confidence, Confidence::Unsure));
        assert_eq!(hits[0].suspicion, 40);
        assert!(hits[0].content.contains("certificate table"));
    }

    /// Excerpts render ASCII, UTF-16LE, and binary matches distinctly.
    #[test]
    fn excerpts_quote_ascii_utf16_and_hex() {
        // printable ASCII renders quoted
        assert_eq!(render_excerpt(b"GET /gate.php"), "\"GET /gate.php\"");
        // UTF-16LE version strings render with the utf16 marker (the Beacon
        // dispute's generic '1.0.0.0' would have been self-triaging)
        let wide: Vec<u8> = "1.0.0.0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(render_excerpt(&wide), "u\"1.0.0.0\"");
        // binary matches render as a bounded hex snippet
        assert_eq!(
            render_excerpt(&[0x6a, 0x40, 0x68, 0x00, 0x30]),
            "hex 6a 40 68 00 30"
        );
        // oversized matches truncate at the excerpt cap
        let long = vec![b'A'; 100];
        let rendered = render_excerpt(&long);
        assert!(rendered.starts_with('"'));
        assert!(rendered.ends_with('…'));
        assert!(rendered.len() < 60);
        // quotes cannot break out of the excerpt quoting
        assert_eq!(render_excerpt(b"a\"b"), "\"a\\\"b\"");
    }

    /// Leads past the render cap fold into one named rollup hit.
    #[test]
    fn lead_flood_folds_into_a_rollup() {
        // one corroborated family, nine solo families, and four weak rules
        let mut matches = vec![
            rule("a.yar", "MALPEDIA_Win_Stealc_Auto", &[0x10]),
            rule("b.yar", "RUSSIANPANDA_Win_Mal_Stealc_V2", &[0x20]),
        ];
        for index in 0..9u64 {
            let identifier = format!("MALPEDIA_Win_Fam{index}_Auto");
            matches.push(rule("a.yar", &identifier, &[0x100 + index]));
        }
        matches.push(rule("a.yar", "SUSP_One", &[0x500]));
        matches.push(rule("a.yar", "SIGNATURE_BASE_Packer_UPX_A", &[0x600]));
        matches.push(rule("a.yar", "Windows_Generic_Threat_0a1b2c3d", &[0x700]));
        matches.push(rule(
            "a.yar",
            "SIGNATURE_BASE_MAL_X_Unknown_Jan23_1",
            &[0x800],
        ));
        let hits = build_hits(matches, &[]);
        // ten rendered leads plus one rollup for the four weak singles
        assert_eq!(hits.len(), 11);
        // the corroborated family sorts first
        assert!(matches!(hits[0].confidence, Confidence::Likely));
        assert_eq!(corroborated_family(&hits[0]), Some("stealc"));
        // the rollup names every folded rule with its first offset
        let rollup = &hits[10];
        assert!(rollup.content.starts_with("4 more YARA rules matched:"));
        assert!(rollup.content.contains("a.yar:SUSP_One@0x500"));
        assert!(
            rollup
                .content
                .contains("a.yar:SIGNATURE_BASE_Packer_UPX_A@0x600")
        );
        assert!(
            rollup
                .content
                .contains("a.yar:Windows_Generic_Threat_0a1b2c3d@0x700")
        );
        assert!(
            rollup
                .content
                .contains("a.yar:SIGNATURE_BASE_MAL_X_Unknown_Jan23_1@0x800")
        );
        // the folded tail was all weak-class, so the rollup carries that tier
        assert!(matches!(rollup.confidence, Confidence::Untrusted));
    }

    /// An operator-assembled rule pack compiles under the production engine.
    ///
    /// Engine-parity gate for packs assembled by
    /// `tools/scripts/fetch-elastic-yara-pack.py`: the pack named by the
    /// `THORPAL_YARA_PACK` env var is compiled exactly as seeding would
    /// (same yara-x version, module set, relaxed regexp syntax, and
    /// namespace-per-file convention). Point it at a single `.yar` file for
    /// the final gate, or at a directory of per-source files to enumerate
    /// every failing source file for the script's `--exclude` flags.
    ///
    /// Run with:
    /// `THORPAL_YARA_PACK=<path> cargo test -p thorpal --lib detect::yara -- --ignored --nocapture`
    #[test]
    #[ignore = "operator pack validation; needs THORPAL_YARA_PACK"]
    fn elastic_pack_compiles() {
        // resolve the pack path the operator wants validated
        let pack = std::path::PathBuf::from(
            std::env::var("THORPAL_YARA_PACK").expect("set THORPAL_YARA_PACK to the pack path"),
        );
        if pack.is_dir() {
            // enumeration mode: compile every source file on its own so the
            // operator sees the complete exclude list in one pass
            let mut good = 0usize;
            let mut bad = 0usize;
            for entry in std::fs::read_dir(&pack).expect("rules dir should list") {
                let path = entry.expect("dir entry should read").path();
                if !matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("yar" | "yara")
                ) {
                    continue;
                }
                // compile this file alone, mirroring scan()'s settings
                let mut compiler = yara_x::Compiler::new();
                compiler.relaxed_re_syntax(true);
                compiler.new_namespace(&path.file_name().unwrap().to_string_lossy());
                let source = std::fs::read(&path).expect("rule file should read");
                match compiler.add_source(source.as_slice()) {
                    Ok(_) => good += 1,
                    Err(error) => {
                        bad += 1;
                        println!(
                            "FAIL {}: {error}",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        );
                    }
                }
            }
            println!("per-file compile: {good} ok, {bad} failed");
            assert!(good > 0, "no rule file compiled from {}", pack.display());
        } else {
            // parity mode: run the exact production compile+scan path against
            // an empty buffer and time it (scan cost on real bytes is tiny
            // next to compile cost for packs this size)
            let rules = std::fs::read_to_string(&pack).expect("pack should read");
            let declared = rules.lines().filter(|line| is_rule_decl(line)).count();
            let started = std::time::Instant::now();
            let hits = scan(&pack, b"").expect("assembled pack should compile cleanly");
            println!(
                "pack {} compiled: {declared} declared rules, {} empty-buffer hits, {:?}",
                pack.display(),
                hits.len(),
                started.elapsed(),
            );
            assert!(declared > 0, "pack declares no rules");
        }
    }

    /// A pack (or rules directory) scans a real file and reports its hits.
    ///
    /// Operator re-check tool for new packs: scans the file named by
    /// `THORPAL_SCAN_SAMPLE` with the rules at `THORPAL_YARA_PACK` through
    /// the production [`scan_file`] path and prints each hit line (rule
    /// namespace, identifier, and match offsets). Zero hits is a valid,
    /// recorded outcome.
    #[test]
    #[ignore = "operator pack validation; needs THORPAL_YARA_PACK + THORPAL_SCAN_SAMPLE"]
    fn pack_scans_sample() {
        // resolve the rules path and the sample to judge them against
        let pack = std::path::PathBuf::from(
            std::env::var("THORPAL_YARA_PACK").expect("set THORPAL_YARA_PACK to the pack path"),
        );
        let sample = std::path::PathBuf::from(
            std::env::var("THORPAL_SCAN_SAMPLE")
                .expect("set THORPAL_SCAN_SAMPLE to the file to scan"),
        );
        // run the exact seed-time scan path under its usual budgets
        let started = std::time::Instant::now();
        let hits = scan_file(&pack, &sample).expect("scan should succeed");
        println!(
            "scanned {} with {}: {} hit(s) in {:?}",
            sample.display(),
            pack.display(),
            hits.len(),
            started.elapsed(),
        );
        // print each hit's full lead body: rules, offsets, excerpts, location
        for hit in &hits {
            println!("  {}", hit.content);
        }
    }

    /// Checks whether a pack line opens a YARA rule declaration.
    ///
    /// # Arguments
    ///
    /// * `line` - The pack source line to test
    fn is_rule_decl(line: &str) -> bool {
        // strip the modifiers a declaration may carry before the keyword
        let mut rest = line.trim_start();
        while let Some(stripped) = rest
            .strip_prefix("private ")
            .or_else(|| rest.strip_prefix("global "))
        {
            rest = stripped.trim_start();
        }
        // a declaration is the rule keyword followed by an identifier
        rest.starts_with("rule ") || rest.starts_with("rule\t")
    }

    /// Build a [`RuleMatch`] fixture for the post-processing tests.
    ///
    /// # Arguments
    ///
    /// * `namespace` - The pack namespace the rule compiled under
    /// * `identifier` - The rule identifier
    /// * `sites` - The match-site file offsets
    fn rule(namespace: &str, identifier: &str, sites: &[u64]) -> RuleMatch {
        // excerpts are rendered at collection time, so fixtures carry none
        RuleMatch {
            namespace: namespace.to_owned(),
            identifier: identifier.to_owned(),
            sites: sites.to_vec(),
            excerpts: Vec::new(),
        }
    }

    /// Build a minimal PE32 fixture with the given declared sections.
    ///
    /// # Arguments
    ///
    /// * `sections` - `(name, raw_offset, raw_size)` triples for the table
    /// * `cert` - The security directory's `(file offset, size)`, if declared
    /// * `total` - The fixture's total size (zero-padded)
    fn build_pe(sections: &[(&str, u32, u32)], cert: Option<(u32, u32)>, total: usize) -> Vec<u8> {
        // lay out DOS magic -> PE signature at 0x80 -> COFF -> PE32 header
        let mut raw = vec![0u8; total];
        raw[0] = b'M';
        raw[1] = b'Z';
        raw[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        raw[0x80..0x84].copy_from_slice(b"PE\0\0");
        // COFF: section count at +2, optional-header size at +16
        let coff = 0x84;
        let count = u16::try_from(sections.len()).expect("few sections");
        raw[coff + 2..coff + 4].copy_from_slice(&count.to_le_bytes());
        raw[coff + 16..coff + 18].copy_from_slice(&0xe0u16.to_le_bytes());
        // optional header: PE32 magic, entry point, data-directory count
        let opt = coff + 20;
        raw[opt..opt + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        raw[opt + 16..opt + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        raw[opt + 92..opt + 96].copy_from_slice(&16u32.to_le_bytes());
        // the security data-directory entry, when the fixture declares one
        if let Some((offset, size)) = cert {
            let dirs = opt + 96;
            raw[dirs + 32..dirs + 36].copy_from_slice(&offset.to_le_bytes());
            raw[dirs + 36..dirs + 40].copy_from_slice(&size.to_le_bytes());
        }
        // the section table follows the optional header
        let table = opt + 0xe0;
        for (index, (name, raw_offset, raw_size)) in sections.iter().enumerate() {
            let entry = table + index * 40;
            raw[entry..entry + name.len()].copy_from_slice(name.as_bytes());
            raw[entry + 8..entry + 12].copy_from_slice(&raw_size.to_le_bytes());
            let vaddr = 0x1000u32 * (u32::try_from(index).expect("few sections") + 1);
            raw[entry + 12..entry + 16].copy_from_slice(&vaddr.to_le_bytes());
            raw[entry + 16..entry + 20].copy_from_slice(&raw_size.to_le_bytes());
            raw[entry + 20..entry + 24].copy_from_slice(&raw_offset.to_le_bytes());
        }
        raw
    }

    /// Write a UTF-16LE string into a fixture at the given offset.
    ///
    /// # Arguments
    ///
    /// * `raw` - The fixture buffer to write into
    /// * `at` - The offset to write at
    /// * `text` - The text to encode
    fn put_utf16(raw: &mut [u8], at: usize, text: &str) {
        // encode and copy the little-endian pairs in place
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        raw[at..at + bytes.len()].copy_from_slice(&bytes);
    }

    /// Write the disputed Beacon lead's version block into a fixture.
    ///
    /// Reproduces the real geometry from the telemetry pack: a
    /// `VS_VERSION_INFO` block spanning 0x44ba6-0x44d1c whose `1.0.0.0`
    /// version strings sit at 0x44bf0 and 0x44c84.
    ///
    /// # Arguments
    ///
    /// * `raw` - The fixture buffer to write into (at least 0x44d1c long)
    fn write_version_block(raw: &mut [u8]) {
        // the block header: length 0x176 covers 0x44ba6..0x44d1c
        raw[0x44ba6..0x44ba8].copy_from_slice(&0x176u16.to_le_bytes());
        // the UTF-16LE key sits right after the three header words
        put_utf16(raw, 0x44bac, "VS_VERSION_INFO");
        // the generic version strings the disputed matches landed on
        put_utf16(raw, 0x44bf0, "1.0.0.0");
        put_utf16(raw, 0x44c84, "1.0.0.0");
    }
}
