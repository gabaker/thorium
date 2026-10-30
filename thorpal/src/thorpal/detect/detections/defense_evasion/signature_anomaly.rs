//! Authenticode signatures that do not verify over the file they ride on.
//!
//! A certificate table copied from a legitimately signed binary onto a
//! modified host passes casual inspection ("it has a signature") while the
//! Authenticode digest proves the graft: the signed digest matches none of
//! the file's actual bytes. A blinded evaluation session saw exactly
//! this shape and still concluded "benign"; this detection turns the graft
//! into a seeded, named finding (signer vs. VersionInfo company) so the
//! benign-verdict directive has something concrete to demand pursuit of.

use anyhow::Result;

use crate::thorpal::detect::authenticode;
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
        // a verifying signature is a fact, not a suspicion — stay silent and
        // let the file-identity surface carry it. Everything else is noted:
        // a declared signature whose blob yields no recognizable digest at
        // all (sha1/256/384/512 are all scanned for) is itself anomalous, so
        // an unparseable blob earns the low-grade structural note whatever
        // the table's position — silencing it on layout would hand a trivial
        // suppression path to a lightly corrupted graft.
        if info.digest_matches == Some(true) {
            return Ok(Vec::new());
        }
        // name the parties: who signed the blob vs. who the file claims to be
        let signer = info
            .cert_names
            .first()
            .map_or("<no CN recovered>", String::as_str);
        let company = info.company_name.as_deref().unwrap_or("<none>");
        let tail = if info.table_is_tail {
            " The certificate table is the last bytes of the file, consistent with a \
             signature grafted onto a modified host."
        } else {
            ""
        };
        let hit = match info.digest_matches {
            Some(false) => DetectionHit {
                technique: T1553_002,
                address: info.table_offset,
                function: None,
                kind: EvidenceKind::ContextOnly,
                confidence: high().0,
                suspicion: high().1,
                content: format!(
                    "Authenticode signature present but the file's Authenticode digest \
                     does NOT match the signed digest — the binary was modified after \
                     signing (certificate names: {}; VersionInfo company: '{company}'; \
                     table @ {:#x}, {} bytes).{tail}",
                    if info.cert_names.is_empty() {
                        signer.to_owned()
                    } else {
                        info.cert_names.join(", ")
                    },
                    info.table_offset,
                    info.table_size,
                ),
                name: None,
                reasoning: "A signature that does not verify over the file it rides on \
                            means the bytes changed after signing: a tampered host, a \
                            grafted certificate table, or an implant. Treat the host as \
                            modified and pursue what was added — do not conclude benign \
                            because a signature exists."
                    .to_owned(),
                sites: Vec::new(),
                source: HitSource::Native,
                artifact: None,
                context: None,
                // unstamped here; the registry runner names native producers
                detection: None,
            },
            // no digest candidate parsed at all: structural fact, low grade
            _ => DetectionHit {
                technique: T1553_002,
                address: info.table_offset,
                function: None,
                kind: EvidenceKind::ContextOnly,
                confidence: low().0,
                suspicion: low().1,
                content: format!(
                    "A security directory is declared (table @ {:#x}, {} bytes; \
                     certificate names: {signer}) but no signed digest could be \
                     located to validate.{tail}",
                    info.table_offset, info.table_size,
                ),
                name: None,
                reasoning: "The signature blob did not parse far enough to verify; \
                            confirm it independently before treating the file as \
                            legitimately signed."
                    .to_owned(),
                sites: Vec::new(),
                source: HitSource::Native,
                artifact: None,
                context: None,
                // unstamped here; the registry runner names native producers
                detection: None,
            },
        };
        Ok(vec![hit])
    }
}
