//! Seed-time YARA scanning over the raw sample (improvement 0015).
//!
//! When the operator configures a rules path, the sample's on-disk bytes are
//! scanned in full with yara-x (the official Rust engine, which handles the
//! modern rule packs the shipped 4.2 CLI could not) and every matching rule
//! becomes a seeded, file-level finding. Rule hits that name malware families are
//! content-derived evidence — the rule matched the bytes — so they are fully
//! compatible with blinded evaluation runs. Rules are operator-supplied and
//! never vendored; a missing or unparseable pack degrades to a warning, not
//! a failed run.

use std::path::Path;

use crate::thorpal::detect::detections::{
    AttackTactic, AttackTechnique, DetectionHit, EvidenceKind, HitSource, medium,
};
use std::borrow::Cow;

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
/// back as file-level [`DetectionHit`]s carrying the rule identifier and its
/// first match sites.
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
        let name = file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "rules".to_owned());
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
    // render each matching rule (bounded) as one file-level hit
    let mut hits = Vec::new();
    for rule in results.matching_rules().take(MAX_YARA_HITS) {
        // collect the first match sites across the rule's patterns
        let mut sites: Vec<u64> = Vec::new();
        for pattern in rule.patterns() {
            for found in pattern.matches() {
                if sites.len() >= MAX_SITES {
                    break;
                }
                sites.push(found.range().start as u64);
            }
            if sites.len() >= MAX_SITES {
                break;
            }
        }
        let first = sites.first().copied().unwrap_or(0);
        let shown = sites
            .iter()
            .map(|site| format!("{site:#x}"))
            .collect::<Vec<_>>()
            .join(", ");
        let severity = medium();
        hits.push(DetectionHit {
            technique: YARA_SIGNATURE,
            address: first,
            function: None,
            kind: EvidenceKind::ContextOnly,
            confidence: severity.0,
            suspicion: severity.1,
            content: format!(
                "YARA rule {}:{} matched the raw file (match offsets: {})",
                rule.namespace(),
                rule.identifier(),
                if shown.is_empty() {
                    "none recorded"
                } else {
                    &shown
                },
            ),
            name: None,
            reasoning: "A curated signature matched the sample's bytes. The rule name \
                        is content-derived evidence — treat it as an identification \
                        lead and verify the behaviors it implies against the binary."
                .to_owned(),
            sites,
            source: HitSource::Yara,
            artifact: None,
            context: None,
        });
    }
    tracing::info!(
        rules = loaded,
        matches = hits.len(),
        "yara seed scan complete"
    );
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(hits[0].sites.len(), 2);
        assert_eq!(hits[0].address, 4);
        assert!(matches!(hits[0].source, HitSource::Yara));
        // a non-matching buffer yields no hits
        let none = scan(&pack, b"nothing here").expect("scan should succeed");
        assert!(none.is_empty());
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
        // print each hit's content line: rule name + offsets only
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
}
