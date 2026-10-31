//! Authenticode signatures that do not verify over the file they ride on.
//!
//! A certificate table copied from a legitimately signed binary onto a
//! modified host passes casual inspection ("it has a signature") while the
//! Authenticode digest proves the graft: the signed digest matches none of
//! the file's actual bytes. A blinded evaluation session saw exactly
//! this shape and still concluded "benign"; this detection turns the graft
//! into a seeded, named finding (signer vs. VersionInfo company) so the
//! benign-verdict directive has something concrete to demand pursuit of.
//! A file whose digest verifies once the span trailing the declared
//! certificate table is excluded is a different anomaly — a signature genuine
//! over the signed scope with data appended after signing — and is reported
//! as exactly that, with the appendix range to carve, never as a digest
//! mismatch or a grafted table.

use anyhow::Result;

use crate::thorpal::detect::authenticode::{self, AuthenticodeInfo};
use crate::thorpal::detect::confidence::Confidence;
use crate::thorpal::detect::context::AnalysisContext;
use crate::thorpal::detect::detections::{
    AttackTechnique, Detection, DetectionHit, EvidenceKind, HitSource, high, low,
};
use crate::thorpal::detect::fileinfo::parse_header;

use super::T1553_002;

/// The signature-anomaly detection.
pub struct SignatureAnomaly;

#[async_trait::async_trait]
impl Detection for SignatureAnomaly {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "signature_anomaly"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1553_002
    }

    /// Verify the Authenticode digest and report a signature that fails it.
    #[tracing::instrument(name = "SignatureAnomaly::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // everything here reads the file as it exists on disk
        let raw = ctx.raw_bytes().await.unwrap_or_default();
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        // only a parsed PE header carries the security directory offsets
        let Some(header) = parse_header(&raw, ctx.program.executable_format.as_deref()) else {
            return Ok(Vec::new());
        };
        let Some(info) = authenticode::inspect(&raw, &header) else {
            // no declared signature is the common case and not a finding
            return Ok(Vec::new());
        };
        // judge the inspection and seed the single hit it earns, when it does
        Ok(build_hit(&info).into_iter().collect())
    }
}

/// Judge one inspected signature and build the hit it earns, when it earns one.
///
/// A verifying signature returns `None`: it is a fact, not a suspicion, and
/// the file-identity surface carries it. Everything else earns exactly one
/// hit — the arm (post-signing appendix, failed digest, unparseable blob)
/// chooses the claim's wording, never whether to stay silent. A declared
/// signature whose blob yields no recognizable digest at all (sha1/256/384/512
/// are all scanned for) is itself anomalous, so an unparseable blob earns the
/// low-grade structural note whatever the table's position — silencing it on
/// layout would hand a trivial suppression path to a lightly corrupted graft.
///
/// # Arguments
///
/// * `info` - The Authenticode facts read off the raw file bytes
fn build_hit(info: &AuthenticodeInfo) -> Option<DetectionHit> {
    // a verifying signature is a fact, not a suspicion — stay silent and
    // let the file-identity surface carry it
    if info.digest_matches == Some(true) {
        return None;
    }
    // name the parties: who signed the blob vs. who the file claims to be
    let signer = info
        .cert_names
        .first()
        .map_or("<no CN recovered>", String::as_str);
    let company = info.company_name.as_deref().unwrap_or("<none>");
    let names = if info.cert_names.is_empty() {
        signer.to_owned()
    } else {
        info.cert_names.join(", ")
    };
    let tail = if info.table_is_tail {
        " The certificate table is the last bytes of the file, consistent with a \
         signature grafted onto a modified host."
    } else {
        ""
    };
    // the appendix span after the declared table, for the arms that name it
    let appendix_start = info.table_offset + info.table_size;
    let appendix_end = appendix_start + info.trailing_bytes;
    // a trailing span is a concrete carve target whichever arm fires; name it
    // so an engaging agent has bytes to pursue, not just a verdict
    let trailing = if info.trailing_bytes > 0 {
        format!(
            " {} bytes trail the declared certificate table \
             (@{appendix_start:#x}-{appendix_end:#x}).",
            info.trailing_bytes,
        )
    } else {
        String::new()
    };
    // a short recovered chain is worth noting: real signing identities carry
    // issuer chains, decoys usually do not. The CN scan is a bounded heuristic
    // that can under-recover, so this stays subordinate to the arm's claim
    let chain = if info.cert_names.len() <= 1 {
        " At most one CN, no issuer chain recovered — consistent with a \
         self-signed decoy certificate rather than a stolen/grafted identity."
    } else {
        ""
    };
    let hit = match (info.digest_matches, info.signed_scope_matches) {
        // the digest verifies once the trailing span is excluded: the
        // signature is genuine over the signed scope and the anomaly is a
        // post-signing appendix — word it as that, not as a digest mismatch
        (Some(false), Some(true)) => assemble_hit(
            info,
            high(),
            format!(
                "Authenticode signature present and it verifies over the signed \
                 scope, but {} bytes were appended after the declared certificate \
                 table (@{appendix_start:#x}-{appendix_end:#x}); carve and \
                 identify the appendix (a secondary signature blob, implant, or \
                 payload). Certificate names: {names}; VersionInfo company: \
                 '{company}'; table @ {:#x}, {} bytes.{chain}",
                info.trailing_bytes, info.table_offset, info.table_size,
            ),
            "A digest that verifies only once the trailing span is excluded \
             proves those bytes were absent at signing time: the file was \
             extended after signing, and real verifiers still reject it. The \
             appendix is the addition — a secondary signature blob, an implant, \
             or a payload read back at runtime — and the signature itself may \
             ride a stolen key. Do not conclude benign because the signed scope \
             verifies; carve the appendix and identify what it contains.",
        ),
        // the digest fails even over the signed scope: a tampered host, a
        // grafted certificate table, or an implant
        (Some(false), _) => assemble_hit(
            info,
            high(),
            format!(
                "Authenticode signature present but the file's Authenticode digest \
                 does NOT match the signed digest — the binary was modified after \
                 signing (certificate names: {names}; VersionInfo company: \
                 '{company}'; table @ {:#x}, {} bytes).{tail}{trailing}{}{chain}",
                info.table_offset,
                info.table_size,
                // a trailing span cannot explain this arm's mismatch: the
                // scoped re-check already excluded it and still failed
                if info.trailing_bytes > 0 {
                    " The digest fails even with that trailing span excluded, \
                     so the signed region itself was modified."
                } else {
                    ""
                },
            ),
            "A signature that does not verify over the file it rides on \
             means the bytes changed after signing: a tampered host, a \
             grafted certificate table, or an implant. Treat the host as \
             modified and pursue what was added — do not conclude benign \
             because a signature exists.",
        ),
        // no digest candidate parsed at all: structural fact, low grade
        _ => assemble_hit(
            info,
            low(),
            format!(
                "A security directory is declared (table @ {:#x}, {} bytes; \
                 certificate names: {signer}) but no signed digest could be \
                 located to validate.{tail}{trailing}{chain}",
                info.table_offset, info.table_size,
            ),
            "The signature blob did not parse far enough to verify; \
             confirm it independently before treating the file as \
             legitimately signed.",
        ),
    };
    Some(hit)
}

/// Assemble the file-level signature hit every arm shares the shape of.
///
/// # Arguments
///
/// * `info` - The Authenticode facts the hit cites
/// * `severity` - The arm's `(confidence, suspicion)` pair
/// * `content` - The arm's rendered claim
/// * `reasoning` - The arm's reasoning text
fn assemble_hit(
    info: &AuthenticodeInfo,
    severity: (Confidence, i64),
    content: String,
    reasoning: &str,
) -> DetectionHit {
    // every arm emits one file-level, context-only hit at the table offset
    DetectionHit {
        technique: T1553_002,
        address: info.table_offset,
        function: None,
        kind: EvidenceKind::ContextOnly,
        confidence: severity.0,
        suspicion: severity.1,
        content,
        name: None,
        reasoning: reasoning.to_owned(),
        sites: Vec::new(),
        source: HitSource::Native,
        artifact: None,
        context: None,
        // unstamped here; the registry runner names native producers
        detection: None,
        // not an API-catalog hit: no catalog identity or withheld severity
        api: None,
        catalog_severity: None,
        always_noise: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an [`AuthenticodeInfo`] with the given verdicts and layout.
    ///
    /// # Arguments
    ///
    /// * `digest_matches` - The full-scope digest verdict
    /// * `signed_scope_matches` - The trailing-span-excluded digest verdict
    /// * `trailing_bytes` - How many bytes trail the declared table
    /// * `table_is_tail` - Whether the table runs exactly to EOF
    fn info(
        digest_matches: Option<bool>,
        signed_scope_matches: Option<bool>,
        trailing_bytes: u64,
        table_is_tail: bool,
    ) -> AuthenticodeInfo {
        AuthenticodeInfo {
            table_offset: 0x9c00,
            table_size: 6440,
            table_is_tail,
            digest_matches,
            signed_scope_matches,
            trailing_bytes,
            cert_names: vec!["Thawte".to_owned(), "VeriSignMPKI-2-8".to_owned()],
            company_name: None,
        }
    }

    /// An observed overlay shape (table @ 0x9c00, 6440 bytes, a 1044-byte
    /// second PKCS#7 fragment appended at 0xB528-0xB93C, signed scope
    /// verifying) takes the appendix arm: the carve cue replaces the
    /// digest-mismatch claim while the severity stays at the strong grade.
    #[test]
    fn appendix_arm_words_the_overlay_accurately() {
        // the disputed file's exact verdicts and layout
        let hit = build_hit(&info(Some(false), Some(true), 1044, false)).unwrap();
        // sensitivity is untouched: the appendix arm keeps the strong grade
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, high().1);
        assert_eq!(hit.technique.id, "T1553.002");
        // the claim is the one a scope-limited recomputation corroborates
        assert!(hit.content.contains("verifies over the signed scope"));
        assert!(
            hit.content
                .contains("1044 bytes were appended after the declared certificate table")
        );
        assert!(hit.content.contains("(@0xb528-0xb93c)"));
        assert!(hit.content.contains("carve and identify the appendix"));
        // the disproven maximal claims are gone from this arm
        assert!(!hit.content.contains("does NOT match"));
        assert!(!hit.content.contains("modified after signing"));
        assert!(!hit.content.contains("grafted"));
        assert!(!hit.reasoning.contains("grafted"));
    }

    /// The confirmed tail-graft pattern (`philandro Software GmbH` signer vs.
    /// `Macromedia, Inc.` `VersionInfo`, table at EOF) keeps the strong
    /// digest-mismatch arm with its tail sentence and names both parties.
    #[test]
    fn tail_graft_keeps_the_strong_arm() {
        // a tail graft has no trailing span, so both scopes fail alike
        let graft = AuthenticodeInfo {
            table_offset: 0x8000,
            table_size: 0x1800,
            table_is_tail: true,
            digest_matches: Some(false),
            signed_scope_matches: Some(false),
            trailing_bytes: 0,
            cert_names: vec![
                "philandro Software GmbH".to_owned(),
                "DigiCert SHA2 Assured ID Code Signing CA".to_owned(),
            ],
            company_name: Some("Macromedia, Inc.".to_owned()),
        };
        let hit = build_hit(&graft).unwrap();
        // the confirmed pattern still seeds at the strong grade
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, high().1);
        assert!(hit.content.contains("does NOT match"));
        assert!(hit.content.contains("philandro Software GmbH"));
        assert!(hit.content.contains("Macromedia, Inc."));
        // the tail-graft sentence still rides the strong claim
        assert!(hit.content.contains("last bytes of the file"));
        // no trailing span exists, so no appendix range is invented
        assert!(!hit.content.contains("trail the declared"));
    }

    /// A host modified inside the signed region keeps the strong arm even
    /// when an appendix also exists, and the body names the trailing range
    /// and says the scoped re-check failed too.
    #[test]
    fn tampered_host_with_appendix_keeps_the_strong_arm_and_names_the_span() {
        // both scopes fail: the trailing span does not explain the mismatch
        let hit = build_hit(&info(Some(false), Some(false), 1044, false)).unwrap();
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert!(hit.content.contains("does NOT match"));
        // the trailing range is named as a concrete pursue target
        assert!(
            hit.content
                .contains("1044 bytes trail the declared certificate table (@0xb528-0xb93c)")
        );
        // and the body preempts the scope-limited recheck an agent would run
        assert!(
            hit.content
                .contains("fails even with that trailing span excluded")
        );
    }

    /// The unparseable-blob arm keeps its low grade and gains the zero-CN
    /// chain-shape note and the trailing-range specifics.
    #[test]
    fn unparseable_blob_stays_low_and_carries_the_shape_notes() {
        // an unparseable shape: no digest candidate, no CN recovered
        let blob = AuthenticodeInfo {
            table_offset: 0x4000,
            table_size: 64,
            table_is_tail: false,
            digest_matches: None,
            signed_scope_matches: None,
            trailing_bytes: 32,
            cert_names: Vec::new(),
            company_name: None,
        };
        let hit = build_hit(&blob).unwrap();
        // the grade is unchanged: structural fact, bottom of the triage order
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, low().1);
        assert!(hit.content.contains("<no CN recovered>"));
        // the chain-shape wording covers the zero-CN case
        assert!(hit.content.contains("At most one CN"));
        assert!(hit.content.contains("self-signed decoy"));
        // the trailing range gives even this floor-grade lead a target
        assert!(
            hit.content
                .contains("32 bytes trail the declared certificate table")
        );
    }

    /// The chain-shape note stays subordinate: on a true graft whose chain
    /// under-recovered to one CN, the mismatch claim leads and the grade
    /// stays strong.
    #[test]
    fn chain_note_stays_subordinate_to_the_mismatch_claim() {
        // a tail graft with a single recovered CN (the bounded scan can
        // under-recover on real chains)
        let graft = AuthenticodeInfo {
            table_offset: 0x8000,
            table_size: 0x1000,
            table_is_tail: true,
            digest_matches: Some(false),
            signed_scope_matches: Some(false),
            trailing_bytes: 0,
            cert_names: vec!["Initech Software".to_owned()],
            company_name: None,
        };
        let hit = build_hit(&graft).unwrap();
        // the note never demotes the finding
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, high().1);
        // the decoy phrasing follows the mismatch claim, never replaces it
        let claim = hit.content.find("does NOT match").unwrap();
        let note = hit.content.find("self-signed decoy").unwrap();
        assert!(claim < note);
    }

    /// A verifying signature builds no hit at all.
    #[test]
    fn verifying_signature_builds_no_hit() {
        // both scopes verify: the signature is a fact, not a suspicion
        assert!(build_hit(&info(Some(true), Some(true), 0, true)).is_none());
    }
}
