//! The detection framework.
//!
//! Detections are organized **by MITRE ATT&CK tactic** (one submodule folder per
//! tactic; e.g. [`defense_evasion`]). Every concrete detection implements the
//! [`Detection`] trait and returns a list of [`DetectionHit`]s, each tagged with
//! the ATT&CK technique it evidences. The [`run_all`] driver executes every
//! registered detection against a shared [`AnalysisContext`] and collects the
//! hits, which are later converted into Thorium `thorium::models::Flag`
//! entities by the report builder.
//!
//! This module also provides the reusable building blocks the detections share:
//! [`api_call_hits`] (imported-API call sites), [`scan_instructions`]
//! (decoded-instruction opcode/operand scanning), and [`string_artifact_hits`]
//! (defined-string matches). Keeping the mechanics here lets each detection file
//! read as a focused, well-commented *catalog* of what it looks for and why.

pub mod command_and_control;
pub mod defense_evasion;
pub mod execution;
pub mod impact;

use std::borrow::Cow;
use std::collections::BTreeMap;

use crate::thorpal::detect::confidence::Confidence;
use anyhow::Context;
use ghidra_cli::models::{Instruction, XRef};
use tracing::instrument;

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr, parse_addr};
use crate::thorpal::detect::decode::buffers::referencing_function;
use crate::thorpal::detect::provenance_types::CallContext;

/// The max distance (bytes) from a referenced data address at which a data-anchored
/// finding (a crypto constant, an unreferenced string) is still attributed to the
/// function that references the buffer. Matches the crypto detection's own
/// PEM/embedded-key windows so data attribution is consistent across the tool.
const DATA_REF_WINDOW: u64 = 4096;

// ===========================================================================
// MITRE ATT&CK taxonomy
// ===========================================================================

/// A MITRE ATT&CK Enterprise tactic (the top-level detection category).
///
/// The full matrix is enumerated so new detection families slot in by tactic
/// without renaming existing categories. Today [`DefenseEvasion`], [`Execution`],
/// [`Impact`], and [`CommandAndControl`] have detections implemented.
///
/// [`DefenseEvasion`]: AttackTactic::DefenseEvasion
/// [`Execution`]: AttackTactic::Execution
/// [`Impact`]: AttackTactic::Impact
/// [`CommandAndControl`]: AttackTactic::CommandAndControl
// The full ATT&CK matrix is enumerated for future detection families; only some
// variants are currently constructed, so allow the others to be unused.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AttackTactic {
    /// TA0043 — gathering information to plan future operations.
    Reconnaissance,
    /// TA0042 — establishing resources to support operations.
    ResourceDevelopment,
    /// TA0001 — getting into the network/host.
    InitialAccess,
    /// TA0002 — running attacker-controlled code.
    Execution,
    /// TA0003 — maintaining a foothold across restarts.
    Persistence,
    /// TA0004 — gaining higher-level permissions.
    PrivilegeEscalation,
    /// TA0005 — avoiding detection (includes anti-analysis / anti-debug / anti-VM).
    DefenseEvasion,
    /// TA0006 — stealing account names and secrets.
    CredentialAccess,
    /// TA0007 — figuring out the environment.
    Discovery,
    /// TA0008 — moving through the environment.
    LateralMovement,
    /// TA0009 — gathering data of interest.
    Collection,
    /// TA0011 — communicating with compromised systems (C2).
    CommandAndControl,
    /// TA0010 — stealing data.
    Exfiltration,
    /// TA0040 — manipulating, interrupting, or destroying systems/data.
    Impact,
}

impl AttackTactic {
    /// The ATT&CK tactic id (e.g. `"TA0005"`).
    pub fn id(&self) -> &'static str {
        // map each tactic to its canonical ATT&CK id
        match self {
            AttackTactic::Reconnaissance => "TA0043",
            AttackTactic::ResourceDevelopment => "TA0042",
            AttackTactic::InitialAccess => "TA0001",
            AttackTactic::Execution => "TA0002",
            AttackTactic::Persistence => "TA0003",
            AttackTactic::PrivilegeEscalation => "TA0004",
            AttackTactic::DefenseEvasion => "TA0005",
            AttackTactic::CredentialAccess => "TA0006",
            AttackTactic::Discovery => "TA0007",
            AttackTactic::LateralMovement => "TA0008",
            AttackTactic::Collection => "TA0009",
            AttackTactic::CommandAndControl => "TA0011",
            AttackTactic::Exfiltration => "TA0010",
            AttackTactic::Impact => "TA0040",
        }
    }

    /// The human-readable tactic name (e.g. `"Defense Evasion"`).
    pub fn name(&self) -> &'static str {
        // map each tactic to its display name
        match self {
            AttackTactic::Reconnaissance => "Reconnaissance",
            AttackTactic::ResourceDevelopment => "Resource Development",
            AttackTactic::InitialAccess => "Initial Access",
            AttackTactic::Execution => "Execution",
            AttackTactic::Persistence => "Persistence",
            AttackTactic::PrivilegeEscalation => "Privilege Escalation",
            AttackTactic::DefenseEvasion => "Defense Evasion",
            AttackTactic::CredentialAccess => "Credential Access",
            AttackTactic::Discovery => "Discovery",
            AttackTactic::LateralMovement => "Lateral Movement",
            AttackTactic::Collection => "Collection",
            AttackTactic::CommandAndControl => "Command and Control",
            AttackTactic::Exfiltration => "Exfiltration",
            AttackTactic::Impact => "Impact",
        }
    }

    /// Parse a tactic from its canonical ATT&CK display name (the inverse of
    /// [`name`](AttackTactic::name)).
    ///
    /// The comparison is case-insensitive and whitespace-trimmed so a value from
    /// an LLM (see `crate::llm`) or capa maps cleanly. Returns `None` for an
    /// unrecognized name so the caller can fall back to a neutral tactic.
    ///
    /// # Arguments
    ///
    /// * `name` - The tactic display name (e.g. `"Defense Evasion"`)
    pub fn from_name(name: &str) -> Option<AttackTactic> {
        // normalize once so casing/padding differences do not miss a match
        let normalized = name.trim().to_ascii_lowercase();
        // map each canonical display name back to its tactic
        let tactic = match normalized.as_str() {
            "reconnaissance" => AttackTactic::Reconnaissance,
            "resource development" => AttackTactic::ResourceDevelopment,
            "initial access" => AttackTactic::InitialAccess,
            "execution" => AttackTactic::Execution,
            "persistence" => AttackTactic::Persistence,
            "privilege escalation" => AttackTactic::PrivilegeEscalation,
            "defense evasion" => AttackTactic::DefenseEvasion,
            "credential access" => AttackTactic::CredentialAccess,
            "discovery" => AttackTactic::Discovery,
            "lateral movement" => AttackTactic::LateralMovement,
            "collection" => AttackTactic::Collection,
            "command and control" => AttackTactic::CommandAndControl,
            "exfiltration" => AttackTactic::Exfiltration,
            "impact" => AttackTactic::Impact,
            _ => return None,
        };
        Some(tactic)
    }
}

/// A specific MITRE ATT&CK technique/sub-technique within a [`AttackTactic`].
///
/// The `id`/`name` are [`Cow`]s so a technique can be either a compile-time
/// constant (the native detections' `const T1622: AttackTechnique = …` use
/// [`Cow::Borrowed`]) or built at runtime from capa's self-describing taxonomy
/// (see `crate::capa`, which uses [`Cow::Owned`]). This is why the type is
/// `Clone` but not `Copy`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AttackTechnique {
    /// The ATT&CK technique id (e.g. `"T1497.003"`).
    pub id: Cow<'static, str>,
    /// The technique name (e.g. `"Time Based Evasion"`).
    pub name: Cow<'static, str>,
    /// The tactic this technique belongs to.
    pub tactic: AttackTactic,
}

impl AttackTechnique {
    /// Build a technique from runtime-owned (or borrowed) id/name strings.
    ///
    /// Used by the capa integration, whose technique ids/names come from the capa
    /// results at runtime rather than from a `const`.
    ///
    /// # Arguments
    ///
    /// * `tactic` - The ATT&CK tactic this technique belongs to
    /// * `id` - The technique id (e.g. `"T1057"` or `"MBC C0042"`)
    /// * `name` - The technique display name
    pub fn new(
        tactic: AttackTactic,
        id: impl Into<Cow<'static, str>>,
        name: impl Into<Cow<'static, str>>,
    ) -> Self {
        // wrap the caller's strings, deferring the borrowed/owned choice to them
        Self {
            id: id.into(),
            name: name.into(),
            tactic,
        }
    }
}

// ===========================================================================
// Severity helpers (map calibrated confidence to Thorium's Confidence + suspicion)
// ===========================================================================

/// A strong, actively-exploited primitive (e.g. a resolved PEB debugger check).
///
/// Maps to Thorium `Confidence::Likely` with a high suspicion weight.
pub const fn high() -> (Confidence, i64) {
    (Confidence::Likely, 75)
}

/// A meaningful but context-dependent indicator (e.g. an anti-debug API call).
///
/// Maps to Thorium `Confidence::Unsure` with a moderate suspicion weight.
pub const fn medium() -> (Confidence, i64) {
    (Confidence::Unsure, 40)
}

/// A weak/ubiquitous indicator that also appears in benign code.
///
/// Maps to Thorium `Confidence::Untrusted` with a low suspicion weight.
pub const fn low() -> (Confidence, i64) {
    (Confidence::Untrusted, 15)
}

// ===========================================================================
// Detection hit + trait + driver
// ===========================================================================

/// How a hit should be validated by the post-detection pass (`crate::validate`).
///
/// Tells the validator what control-flow/context check confirms or refutes the
/// evidence. Defaults to [`ContextOnly`](EvidenceKind::ContextOnly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    /// An imported-API/syscall whose RETURN VALUE is meant to gate behavior
    /// (validate: the return is tested and feeds a conditional branch).
    ApiResultCheck,
    /// A timing primitive (validate: a paired read + delta subtraction/compare).
    Timing,
    /// An opcode/read whose result should feed a branch (CPUID hypervisor, PEB
    /// read, INT 0x2D, trap flag, 0xCC scan): validate a nearby conditional jump.
    ControlFlowGated,
    /// Context-only evidence (strings, sleeps, imported-but-uncalled): validated
    /// by reachability + clustering only.
    ContextOnly,
}

/// Which analysis produced a hit.
///
/// Distinguishes ghidra-detect's own static detections from findings imported
/// from a capa prior result (see `crate::capa`). The report uses this to label
/// and tally findings by source, and the corroboration pass uses it to know which
/// hits to cross-reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum HitSource {
    /// Produced by a ghidra-detect [`Detection`].
    Native,
    /// Imported from a capa prior result.
    Capa,
    /// Produced by the LLM enrichment pass (see `crate::llm`).
    Llm,
    /// Produced by a detector-flagged library Rhai script (see
    /// `crate::thorpal::detect::script_detections`).
    Script,
    /// Produced by the operator-configured seed-time YARA scan (see
    /// `crate::thorpal::detect::yara`).
    Yara,
    /// Produced by the seed-time p-code emulation stage (see
    /// `crate::thorpal::detect::emulation`).
    Emulation,
}

/// What kind of concrete artifact a hit recovered from the sample.
///
/// A detection hit answers *"what does this binary do?"*; an artifact answers
/// *"what is in it?"* — a value an analyst can pivot on. Tagging a hit with a
/// class lifts its recovered value into the report's artifact inventory (see
/// `crate::report::ExtractedArtifact`) alongside the address, the section, and
/// the function that uses it — all of which the Thorium `Flag` boundary drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub enum ArtifactClass {
    /// A URL, domain, or `host:port` the program talks to.
    NetworkEndpoint,
    /// A URI path or request template used against an endpoint.
    UriPath,
    /// A User-Agent the program presents.
    UserAgent,
    /// An HTTP header name, value, or hand-built header template.
    HttpHeader,
    /// A host-identity value the program is keyed to (MAC, GUID, serial, name).
    HostIdentifier,
    /// A recovered symmetric key or key-derivation input.
    SymmetricKey,
    /// A recovered asymmetric private key.
    AsymmetricPrivateKey,
    /// A recovered asymmetric public key.
    AsymmetricPublicKey,
    /// A recovered X.509 certificate.
    Certificate,
    /// A buffer the decode engine recovered plaintext from.
    DecodedData,
    /// A structured configuration record parsed out of a recovered buffer.
    ConfigRecord,
    /// Packing/runtime-obfuscation evidence and the section table backing it.
    Packer,
}

impl ArtifactClass {
    /// The section heading this class is rendered under in the report.
    pub fn label(self) -> &'static str {
        match self {
            Self::NetworkEndpoint => "Network Endpoints",
            Self::UriPath => "URI Paths & Request Templates",
            Self::UserAgent => "User Agents",
            Self::HttpHeader => "HTTP Headers",
            Self::HostIdentifier => "Host Identifiers",
            Self::SymmetricKey => "Symmetric Keys",
            Self::AsymmetricPrivateKey => "Asymmetric Private Keys",
            Self::AsymmetricPublicKey => "Asymmetric Public Keys",
            Self::Certificate => "Certificates",
            Self::DecodedData => "Decoded Data",
            Self::ConfigRecord => "Configuration Records",
            Self::Packer => "Packing",
        }
    }
}

/// The artifact payload a hit carries into the report's artifact inventory.
///
/// Held separately from the hit's `content` (a markdown block written for a
/// human reading the findings) so the inventory can carry the bare value and its
/// attribution in machine-readable form.
#[derive(Debug, Clone)]
pub struct HitArtifact {
    /// What kind of artifact this is.
    pub class: ArtifactClass,
    /// The recovered value itself, unadorned (the thing an analyst pivots on).
    pub value: String,
    /// The observed relationship between the artifact and the function that uses
    /// it — e.g. `"XOR decode loop that consumes this key"`. Derived from the
    /// evidence that produced the hit, never guessed; `None` when attribution
    /// failed, in which case [`detail`](HitArtifact::detail) must say so.
    pub role: Option<String>,
    /// Anything else worth recording: additional referencing functions, the
    /// encoding chain that produced the value, or why attribution failed.
    pub detail: Option<String>,
}

impl HitArtifact {
    /// Build an artifact payload with no attribution yet.
    ///
    /// # Arguments
    ///
    /// * `class` - The artifact class
    /// * `value` - The recovered value
    pub fn new(class: ArtifactClass, value: impl Into<String>) -> Self {
        Self {
            class,
            value: value.into(),
            // callers attach the role/detail through the builders below
            role: None,
            detail: None,
        }
    }

    /// Record the observed relationship to the using function (builder style).
    ///
    /// # Arguments
    ///
    /// * `role` - A short phrase naming what the function does with the artifact
    pub fn role(mut self, role: impl Into<String>) -> Self {
        // record the attribution phrase and return self for chaining
        self.role = Some(role.into());
        self
    }

    /// Record supplementary context (builder style).
    ///
    /// # Arguments
    ///
    /// * `detail` - Extra referencing functions, the encoding chain, or why
    ///   attribution failed
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        // record the supplementary context and return self for chaining
        self.detail = Some(detail.into());
        self
    }
}

/// One concrete piece of evidence produced by a detection.
///
/// Converted into a Thorium `thorium::models::Flag` by the report builder; the
/// containing [`function`](DetectionHit::function) drives which functions get
/// their disassembly/decompilation attached to the report.
#[derive(Debug, Clone)]
pub struct DetectionHit {
    /// The ATT&CK technique this hit evidences.
    pub technique: AttackTechnique,
    /// The address of the evidence (call site, opcode, or string use site).
    pub address: u64,
    /// The entry address of the containing function, when the site is in one.
    pub function: Option<u64>,
    /// How the post-detection validator should confirm/refute this hit.
    pub kind: EvidenceKind,
    /// How confident/reliable this hit is (Thorium confidence scale).
    pub confidence: Confidence,
    /// How suspicious this hit is (higher = more suspicious).
    pub suspicion: i64,
    /// The specific characteristic observed (becomes the flag's `content`).
    pub content: String,
    /// An explicit entity name for this hit's flag. When `None`, the submit/report
    /// paths fall back to the first line of `content`. Set for recovered-data flags so
    /// the entity gets a typed, snippet-bearing name (e.g. `Public Key - "MIIB…" @ …`)
    /// even though `content` is a multi-line markdown block.
    pub name: Option<String>,
    /// Why this is suspicious + how it thwarts analysis (becomes the reasoning).
    pub reasoning: String,
    /// Every address backing this finding — the call sites of a thunked import
    /// within one function, the instructions referencing a literal, or the
    /// addresses a constant was found at. Empty means "just use `address`"; the
    /// validator uses these to check control flow at each site, and the markup
    /// pass writes a note at each one.
    pub sites: Vec<u64>,
    /// Which analysis produced this hit (native detection vs. capa import).
    pub source: HitSource,
    /// The concrete artifact this hit recovered, when it recovered one.
    ///
    /// Present only on hits that extracted a pivot-worthy *value* (a key, an
    /// endpoint, a host identifier). Behavior findings leave this `None`. The
    /// report builder lifts these into its artifact inventory.
    pub artifact: Option<HitArtifact>,
    /// What the surrounding code actually does with this evidence, once the
    /// provenance pass has classified it (see `crate::provenance`).
    ///
    /// `None` until that pass runs (and on any hit it could not classify). A hit
    /// whose evidence is only *present* — an API the compiler's runtime calls, a
    /// counter read that seeds a PRNG — is a very different claim from one whose
    /// result steers the sample's own control flow, and this is what records the
    /// difference for the validator, the report, and the LLM.
    pub context: Option<CallContext>,
}

impl DetectionHit {
    /// Build a hit, resolving the containing function from the site address.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The analysis context (used to resolve the containing function)
    /// * `technique` - The ATT&CK technique evidenced
    /// * `address` - The evidence site address
    /// * `severity` - The `(confidence, suspicion)` pair (see [`high`]/[`medium`]/[`low`])
    /// * `content` - The specific characteristic observed
    /// * `reasoning` - The analyst-facing explanation
    pub fn at(
        ctx: &AnalysisContext,
        technique: AttackTechnique,
        address: u64,
        severity: (Confidence, i64),
        content: impl Into<String>,
        reasoning: impl Into<String>,
    ) -> Self {
        // resolve the containing function once, up front
        let function = ctx.containing_function(address);
        // reuse the explicit constructor with the resolved function
        Self::new(technique, address, function, severity, content, reasoning)
    }

    /// Build a hit with an explicitly known containing function.
    ///
    /// Used by the instruction scanner, which already knows the function it is
    /// iterating (no lookup needed).
    ///
    /// # Arguments
    ///
    /// * `technique` - The ATT&CK technique evidenced
    /// * `address` - The evidence site address
    /// * `function` - The containing function entry address, if any
    /// * `severity` - The `(confidence, suspicion)` pair (see [`high`]/[`medium`]/[`low`])
    /// * `content` - The specific characteristic observed
    /// * `reasoning` - The analyst-facing explanation
    pub fn new(
        technique: AttackTechnique,
        address: u64,
        function: Option<u64>,
        severity: (Confidence, i64),
        content: impl Into<String>,
        reasoning: impl Into<String>,
    ) -> Self {
        // destructure the (confidence, suspicion) severity pair
        let (confidence, suspicion) = severity;
        Self {
            technique,
            address,
            function,
            // default to context-only validation; detections override via `kind()`
            kind: EvidenceKind::ContextOnly,
            confidence,
            suspicion,
            content: content.into(),
            // no explicit name by default; submit/report fall back to content's first line
            name: None,
            reasoning: reasoning.into(),
            // by default a hit has just its primary `address`
            sites: Vec::new(),
            // detections build native hits; the capa importer overrides this
            source: HitSource::Native,
            // a behavior finding carries no artifact; extractors attach one
            artifact: None,
            // unclassified until the provenance pass reads the surrounding code
            context: None,
        }
    }

    /// Set an explicit entity name for this hit's flag (builder style).
    ///
    /// # Arguments
    ///
    /// * `name` - The entity name (e.g. `Public Key - "MIIB…" @ .rodata @ 0x1b42a0`)
    pub fn name(mut self, name: impl Into<String>) -> Self {
        // record the explicit name and return self for chaining
        self.name = Some(name.into());
        self
    }

    /// Set which analysis produced this hit (builder style).
    ///
    /// # Arguments
    ///
    /// * `source` - The producing analysis (native detection vs. capa import)
    pub fn source(mut self, source: HitSource) -> Self {
        // record the source and return self for chaining
        self.source = source;
        self
    }

    /// Set how this hit should be validated (builder style).
    ///
    /// # Arguments
    ///
    /// * `kind` - The evidence kind used to route validation
    pub fn kind(mut self, kind: EvidenceKind) -> Self {
        // record the routing kind and return self for chaining
        self.kind = kind;
        self
    }

    /// Attach the concrete artifact this hit recovered (builder style).
    ///
    /// # Arguments
    ///
    /// * `artifact` - The recovered value plus its attribution (see [`HitArtifact`])
    pub fn artifact(mut self, artifact: HitArtifact) -> Self {
        // record the artifact payload and return self for chaining
        self.artifact = Some(artifact);
        self
    }

    /// Attach every address backing this finding (builder style).
    ///
    /// # Arguments
    ///
    /// * `sites` - All evidence sites (e.g. every call of a thunked import within
    ///   the containing function, or every instruction referencing a literal)
    pub fn sites(mut self, sites: Vec<u64>) -> Self {
        // record the full site list and return self for chaining
        self.sites = sites;
        self
    }

    /// Raise this hit one confidence step (and its suspicion); returns whether it
    /// moved.
    ///
    /// Steps up the `Untrusted < Unsure < Likely < Fact` scale, saturating at
    /// `Fact`. Shared by the post-detection validator (branch-confirmed checks) and
    /// the capa corroboration pass (agreement between capa and a native detection) —
    /// both of which speak to the hit itself. Corroboration that only speaks to the
    /// surrounding function goes through [`DetectionHit::strengthen_to`] instead.
    pub fn strengthen(&mut self) -> bool {
        let r = confidence_rank(self.confidence);
        // already at the ceiling — nothing to do
        if r >= 3 {
            return false;
        }
        self.confidence = confidence_from_rank(r + 1);
        self.suspicion = (self.suspicion + 15).min(95);
        true
    }

    /// Raise this hit one confidence step, but no further than `ceiling`; returns
    /// whether it moved.
    ///
    /// For corroboration that is about a hit's *neighbourhood* rather than the hit
    /// itself. "The function this sits in is packed with evasion indicators" is good
    /// reason to believe an indicator in it is real, so it earns a step; it is not
    /// reason to call the indicator a fact, because it is evidence about the company
    /// the hit keeps and not about the hit. `Fact` is for evidence that bears on the
    /// claim directly — an independent tool asserting the same thing, a decode that
    /// validates itself — so a neighbourhood signal is capped below it. Without the
    /// cap a heuristic recovery lands at the top of the scale for no reason beyond
    /// its address.
    ///
    /// # Arguments
    ///
    /// * `ceiling` - The highest confidence this corroboration may reach
    pub fn strengthen_to(&mut self, ceiling: Confidence) -> bool {
        // already at or past what this class of corroboration can justify
        if confidence_rank(self.confidence) >= confidence_rank(ceiling) {
            return false;
        }
        self.strengthen()
    }

    /// Lower this hit one confidence step (and its suspicion); returns whether it
    /// moved.
    ///
    /// Steps down the confidence scale, saturating at `Untrusted`.
    pub fn weaken(&mut self) -> bool {
        let r = confidence_rank(self.confidence);
        // already at the floor — nothing to do
        if r == 0 {
            return false;
        }
        self.confidence = confidence_from_rank(r - 1);
        self.suspicion = (self.suspicion - 15).max(5);
        true
    }
}

/// Rank a confidence level (0 = least, 3 = most confident).
///
/// # Arguments
///
/// * `confidence` - The confidence level to rank
pub fn confidence_rank(confidence: Confidence) -> u8 {
    match confidence {
        Confidence::Untrusted => 0,
        Confidence::Unsure => 1,
        Confidence::Likely => 2,
        Confidence::Fact => 3,
    }
}

/// Build a confidence level from its rank (see [`confidence_rank`]).
///
/// # Arguments
///
/// * `rank` - The rank to convert (values above 3 saturate at `Fact`)
pub fn confidence_from_rank(rank: u8) -> Confidence {
    match rank {
        0 => Confidence::Untrusted,
        1 => Confidence::Unsure,
        2 => Confidence::Likely,
        _ => Confidence::Fact,
    }
}

/// A single anti-analysis / capability detection.
#[async_trait::async_trait]
pub trait Detection: Send + Sync {
    /// A short, stable name for logs (e.g. `"debugger_evasion"`).
    fn name(&self) -> &'static str;

    /// The primary ATT&CK technique this detection reports.
    fn technique(&self) -> AttackTechnique;

    /// Run the detection against the shared context, returning any hits.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The shared analysis context
    ///
    /// # Errors
    ///
    /// Returns an error only for hard failures; a detection that simply finds
    /// nothing returns an empty vector.
    async fn run(&self, ctx: &AnalysisContext) -> anyhow::Result<Vec<DetectionHit>>;
}

/// The full registry of detections, across all ATT&CK tactics.
pub fn all() -> Vec<Box<dyn Detection>> {
    // each ATT&CK tactic module contributes its own `detections()`
    let mut detections = Vec::new();
    detections.extend(command_and_control::detections());
    detections.extend(defense_evasion::detections());
    detections.extend(execution::detections());
    detections.extend(impact::detections());
    detections
}

/// Run every registered detection and collect all hits.
///
/// A detection that fails is logged at ERROR (naming the detection + technique)
/// and skipped, and the remaining detections still run: one broken Ghidra query
/// or parse must not sink the whole report, so the tool flags everything it can
/// still analyze. When at least one detection failed, a summary line notes that
/// the report is partial. This never returns an error.
///
/// # Arguments
///
/// * `ctx` - The shared analysis context
#[instrument(name = "detections::run_all", skip_all)]
pub async fn run_all(ctx: &AnalysisContext) -> anyhow::Result<Vec<DetectionHit>> {
    // accumulate hits from each detection in registry order
    let mut hits = Vec::new();
    // count detections that failed so we can note a partial report at the end
    let mut failed = 0usize;
    for detection in all() {
        // run the detection; on failure, log and continue with the rest so a single
        // broken detection never aborts the whole tool
        match detection.run(ctx).await {
            Ok(found) => {
                // log one concise line with the detection's hit count
                tracing::info!(
                    detection = detection.name(),
                    technique = %detection.technique().id,
                    count = found.len(),
                    "detection complete"
                );
                hits.extend(found);
            }
            Err(error) => {
                // record the failure and keep going
                failed += 1;
                tracing::error!(
                    detection = detection.name(),
                    technique = %detection.technique().id,
                    ?error,
                    "detection failed; skipping and continuing",
                );
            }
        }
    }
    // note when the report is partial so the operator sees the degraded run
    if failed > 0 {
        tracing::warn!(failed, "some detections failed; report is partial");
    }
    Ok(hits)
}

// ===========================================================================
// Shared detection helpers
// ===========================================================================

/// A catalog entry describing one imported API to flag.
#[derive(Debug, Clone, Copy)]
pub struct ApiSpec {
    /// The normalized API name to match (e.g. `"IsDebuggerPresent"`).
    pub key: &'static str,
    /// The `(confidence, suspicion)` severity for a resolved call site.
    pub severity: (Confidence, i64),
    /// The analyst-facing explanation (meaning + how it thwarts analysis).
    pub reasoning: &'static str,
}

/// Normalize an imported symbol name for catalog matching.
///
/// Strips import decorations (`__imp_`, leading `_`, stdcall `@N`) and a single
/// trailing ANSI/Wide `A`/`W` suffix so `GetUserNameA` matches a `GetUserName`
/// catalog key.
///
/// # Arguments
///
/// * `name` - The raw imported symbol name
pub fn normalize(name: &str) -> String {
    // drop the common IAT decoration and leading underscores
    let mut n = name.trim_start_matches("__imp_").trim_start_matches('_');
    // cut any stdcall arity suffix (`Foo@4`)
    if let Some(idx) = n.find('@') {
        n = &n[..idx];
    }
    // strip a single trailing A/W (ANSI/Wide variant) when it looks like a suffix
    let bytes = n.as_bytes();
    if n.len() > 4 && (bytes[n.len() - 1] == b'A' || bytes[n.len() - 1] == b'W') {
        return n[..n.len() - 1].to_string();
    }
    n.to_string()
}

/// Classify a hit sitting in recognizable runtime code, when it does.
///
/// The pure decision behind [`apply_runtime_context`], split out so it is
/// testable without a bridge. A hit is classified as runtime noise when its
/// containing function's name matches the MSVC marker set or the
/// fingerprinted toolchain's runtime markers — unless it already carries a
/// context, or it carries a pivot-class artifact (configs, endpoints, keys,
/// certificates, host identifiers): a real recovery stays loud wherever it
/// sits.
///
/// # Arguments
///
/// * `toolchain` - The fingerprinted toolchain, if any
/// * `fn_name` - The containing function's name, when the hit sits in one
/// * `hit` - The hit to judge
pub fn runtime_classification(
    toolchain: Option<super::toolchain::Toolchain>,
    fn_name: Option<&str>,
    hit: &DetectionHit,
) -> Option<CallContext> {
    // an already-classified hit was judged by an earlier pass; leave it alone
    if hit.context.is_some() {
        return None;
    }
    // pivot-class artifacts are never written off as runtime noise
    if let Some(artifact) = &hit.artifact {
        if matches!(
            artifact.class,
            ArtifactClass::ConfigRecord
                | ArtifactClass::NetworkEndpoint
                | ArtifactClass::SymmetricKey
                | ArtifactClass::AsymmetricPrivateKey
                | ArtifactClass::AsymmetricPublicKey
                | ArtifactClass::Certificate
                | ArtifactClass::HostIdentifier
        ) {
            return None;
        }
    }
    // only hits inside a named function can be attributed to a runtime
    let name = fn_name?;
    // the MSVC marker set catches CRT support code in every toolchain
    if is_library_function(name) {
        return Some(CallContext::Runtime(format!("{name}, MSVC/CRT support")));
    }
    // the fingerprinted toolchain's own runtime/stdlib markers catch the rest
    let toolchain = toolchain?;
    if super::toolchain::is_toolchain_runtime(toolchain, name) {
        return Some(CallContext::Runtime(format!(
            "{name}, {} runtime",
            toolchain.label()
        )));
    }
    None
}

/// Classify hits sitting in recognizable runtime code and weaken them once.
///
/// The ported engine never carried the source project's provenance walk, so
/// `DetectionHit::context` was never set (see [`super::provenance_types`]).
/// This is the bounded replacement the `build_api_hit` tombstone reserved: a
/// name-based check of every hit's containing function against the MSVC
/// marker set and the fingerprinted toolchain's runtime markers, applied once
/// over the merged hit list so every detection path is judged by the same
/// rule. A classified hit is weakened one step and gains
/// [`CallContext::Runtime`], which the forum seeder renders as the existing
/// runtime note. Hits that already weakened themselves to the floor stay
/// there (`weaken` saturates), so double application is harmless.
///
/// # Arguments
///
/// * `ctx` - The analysis context (toolchain + function names)
/// * `hits` - The hits to classify in place
pub fn apply_runtime_context(ctx: &AnalysisContext, hits: &mut [DetectionHit]) {
    // judge every hit by the one shared rule, resolving names through the context
    for hit in hits.iter_mut() {
        // resolve the containing function's name, when the hit sits in one
        let name = hit.function.and_then(|entry| ctx.function_name(entry));
        if let Some(context) = runtime_classification(ctx.toolchain, name, hit) {
            // runtime-located evidence is one confidence step weaker, and the
            // note below tells the agents why
            hit.weaken();
            hit.context = Some(context);
        }
    }
}

/// Heuristically recognize C-runtime / compiler-support function names.
///
/// The provenance pass (`crate::provenance`) uses this as the *seed* for its
/// caller-chain walk: a statically-linked runtime legitimately calls
/// `IsDebuggerPresent`, reads `NtGlobalFlag`, and reads the performance counter,
/// none of which is the sample's own behavior. Name matching alone is incomplete
/// (a runtime helper can be reached from a recognized parent under an
/// unrecognized name), so treat a `false` here as "unknown", not "user code" —
/// the walk is what closes that gap.
///
/// # Arguments
///
/// * `name` - The containing function name
pub fn is_library_function(name: &str) -> bool {
    // lowercase once, then check for well-known runtime markers
    let n = name.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "acrt",
        "scrt",
        "_rtc",
        "crt_",
        "report_gsfailure",
        "guard_check",
        "guard_dispatch",
        "exit_or_terminate",
        "_except_handler",
        "securecookie",
        "security_init_cookie",
        "_initterm",
        "_cinit",
        "__report",
        "_onexit",
        // MSVC failure/termination helpers: `__fastfail` and the `DllMain`
        // dispatcher are where the debugger and `NtGlobalFlag` checks an analyst
        // sees in a normal MSVC binary actually live
        "fastfail",
        "dllmain_dispatch",
        "terminate_process",
        "invalid_parameter",
        "purecall",
        "chkstk",
        "raise_securityfailure",
        "filter_x86_sse2",
        "is_c_termination",
    ];
    MARKERS.iter().any(|m| n.contains(m))
}

/// Truncate a string for embedding in flag content/reasoning.
///
/// # Arguments
///
/// * `s` - The string to shorten
/// * `max` - The maximum number of characters to keep
fn short(s: &str, max: usize) -> String {
    // return as-is when short enough, else clip with an ellipsis
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let clipped: String = s.chars().take(max.saturating_sub(3)).collect();
        format!("{clipped}...")
    }
}

/// Emit hits for the call sites of any imported API matching the given specs.
///
/// For each imported symbol whose normalized name matches a spec: resolve its call
/// sites (xrefs) and emit **one hit per calling function**, naming every call it makes
/// — an API a function calls ten times is one behavior repeated, not ten findings (the
/// repetition itself is corroboration, and the validator reads it off the site list).
/// If the API is imported but no call site resolves, emit a single weak "imported" hit.
/// Hits landing inside compiler/runtime support code are downgraded (or dropped) later,
/// by the provenance pass — see `crate::provenance`.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `technique` - The ATT&CK technique these APIs evidence
/// * `specs` - The per-API catalog
/// * `kind` - The evidence kind applied to each resolved call-site hit (drives
///   how the validator confirms/refutes it, e.g. [`EvidenceKind::ApiResultCheck`])
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
#[instrument(name = "detections::api_call_hits", skip_all, err(Debug))]
pub async fn api_call_hits(
    ctx: &AnalysisContext,
    technique: AttackTechnique,
    specs: &[ApiSpec],
    kind: EvidenceKind,
) -> anyhow::Result<Vec<DetectionHit>> {
    let mut hits = Vec::new();
    // walk the program's imports once, matching each against the catalog
    for import in &ctx.imports {
        // normalize the import name and look for a matching spec (exact match on key)
        let norm = normalize(&import.name);
        let Some(spec) = specs.iter().find(|s| s.key == norm) else {
            continue;
        };
        // resolve the call sites, transparently hopping import thunks so a flag is
        // attributed to the real caller (not the one-instruction JMP stub).
        let sites = ctx
            .resolve_call_sites(&import.address)
            .await
            .with_context(|| format!("resolving call sites of import '{}'", import.name))?;
        if sites.is_empty() {
            // imported but unreferenced: a weak signal on its own
            hits.push(DetectionHit::at(
                ctx,
                technique.clone(),
                0,
                low(),
                format!("{norm} imported"),
                format!(
                    "{} (imported, but no direct call site was resolved)",
                    spec.reasoning
                ),
            ));
            continue;
        }
        // thunked and direct sites are each grouped by their calling function into a
        // single hit listing every call: ten calls of one API from one function is one
        // behavior repeated, and ten identical flags for it bury everything else
        let mut thunked: BTreeMap<Option<u64>, Vec<u64>> = BTreeMap::new();
        let mut direct: BTreeMap<Option<u64>, Vec<u64>> = BTreeMap::new();
        let mut thunk_entry: Option<u64> = None;
        for cs in sites {
            if cs.via_thunk.is_some() {
                // remember the thunk (for the reasoning note) and collect the site
                thunk_entry = thunk_entry.or(cs.via_thunk);
                thunked.entry(cs.function).or_default().push(cs.site);
            } else {
                // ordinary direct call: collect it under its calling function
                direct.entry(cs.function).or_default().push(cs.site);
            }
        }
        // one hit per calling function, for the thunked then the direct call sites
        for (via_thunk, groups) in [(thunk_entry, thunked), (None, direct)] {
            for (function, mut group) in groups {
                // sort + dedupe the call addresses for stable, clean output
                group.sort_unstable();
                group.dedup();
                hits.push(build_api_hit(
                    technique.clone(),
                    kind,
                    spec,
                    &norm,
                    function,
                    &group,
                    via_thunk,
                ));
            }
        }
    }
    Ok(hits)
}

/// Build one imported-API call-site hit.
///
/// Centralizes the severity, the thunk-aware content/reasoning, and attaching the
/// full site list for the validator.
///
/// # Arguments
///
/// * `technique` - The ATT&CK technique evidenced
/// * `kind` - The evidence kind routing validation
/// * `spec` - The matched API catalog entry
/// * `norm` - The normalized import name
/// * `function` - The containing (calling) function entry, when known
/// * `sites` - Every call site of this API within that function
/// * `via_thunk` - The thunk entry the import was reached through, if any
// seven parameters is at the lint's bar, and the alternative is a struct that
// exists only to be destructured at the single call site
#[allow(clippy::too_many_arguments)]
fn build_api_hit(
    technique: AttackTechnique,
    kind: EvidenceKind,
    spec: &ApiSpec,
    norm: &str,
    function: Option<u64>,
    sites: &[u64],
    via_thunk: Option<u64>,
) -> DetectionHit {
    // the primary address is the first (lowest) call site
    let primary = sites.first().copied().unwrap_or(0);
    // the spec severity stands as detected: the runtime/library downgrade is the
    // provenance pass's job now (`crate::provenance::apply`), so every detection
    // path — this one, the instruction scanner, the string matcher, capa — is
    // judged by the same rule and the note is written exactly once
    let severity = spec.severity;
    // render the call addresses for the content, capping the list rather than
    // spilling dozens of addresses into one line
    let addrs = render_sites(sites);
    // make thunked calls explicit and list all call sites
    let content = match (via_thunk.is_some(), sites.len()) {
        (true, n) if n > 1 => format!("{norm} call (via import thunk) — {n} calls @ {addrs}"),
        (true, _) => format!("{norm} call (via import thunk) @ {addrs}"),
        (false, n) if n > 1 => format!("{norm} — {n} calls @ {addrs}"),
        (false, _) => format!("{norm} call @ {addrs}"),
    };
    // note the thunk indirection in the reasoning when applicable
    let thunk_note = if via_thunk.is_some() {
        " This import is invoked through a thunk stub, so this flag is attributed to \
         the calling function shown here (not the one-instruction jump stub)."
    } else {
        ""
    };
    let reasoning = format!("{}{thunk_note}", spec.reasoning);
    // assemble the hit; attach the full site list only when there is more than one
    let mut hit =
        DetectionHit::new(technique, primary, function, severity, content, reasoning).kind(kind);
    if sites.len() > 1 {
        hit = hit.sites(sites.to_vec());
    }
    hit
}

/// A cursor over one decoded instruction during an [`scan_instructions`] pass.
///
/// Gives the visited instruction plus its position within its function so a
/// detection can inspect neighbouring instructions (e.g. a CPUID's leaf, a
/// syscall's number, or a PEB load's field offset).
pub struct InsnCtx<'a> {
    /// The entry address of the function being scanned.
    pub func_entry: u64,
    /// The function's full (trimmed) instruction list.
    pub insns: &'a [Instruction],
    /// The index of the currently-visited instruction within `insns`.
    pub idx: usize,
}

impl InsnCtx<'_> {
    /// The currently-visited instruction.
    pub fn insn(&self) -> &Instruction {
        // index is always in-bounds (produced by the scanner)
        &self.insns[self.idx]
    }

    /// The visited instruction's address, parsed to `u64` (0 if malformed).
    pub fn addr(&self) -> u64 {
        parse_addr(&self.insn().address).unwrap_or(0)
    }

    /// The visited instruction's mnemonic, upper-cased for comparison.
    pub fn mnemonic(&self) -> String {
        self.insn().mnemonic.to_ascii_uppercase()
    }

    /// The visited instruction's operands, lower-cased (empty if none).
    pub fn operands(&self) -> String {
        crate::thorpal::detect::op_str(self.insn()).to_ascii_lowercase()
    }

    /// The up-to-`n` instructions immediately preceding the visited one.
    ///
    /// # Arguments
    ///
    /// * `n` - How many preceding instructions to return
    pub fn preceding(&self, n: usize) -> &[Instruction] {
        // clamp the window start to the function's beginning
        let start = self.idx.saturating_sub(n);
        &self.insns[start..self.idx]
    }

    /// The up-to-`n` instructions immediately following the visited one.
    ///
    /// # Arguments
    ///
    /// * `n` - How many following instructions to return
    pub fn following(&self, n: usize) -> &[Instruction] {
        // clamp the window end to the function's end
        let end = (self.idx + 1 + n).min(self.insns.len());
        &self.insns[self.idx + 1..end]
    }
}

/// Scan every decoded instruction in the program, collecting detection hits.
///
/// This disassembles all functions (via [`AnalysisContext::all_function_disassembly`],
/// cached) and invokes `visit` for each instruction; a returned `Some(hit)` is
/// collected. This is the accurate, decoded-instruction basis for all
/// opcode/operand detections (RDTSC, CPUID, INT, syscall, PEB reads, trap-flag).
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `visit` - Called per instruction; returns a hit to record, or `None`
///
/// # Errors
///
/// Returns an error if disassembling the program fails.
#[instrument(name = "detections::scan_instructions", skip_all, err(Debug))]
pub async fn scan_instructions<F>(
    ctx: &AnalysisContext,
    mut visit: F,
) -> anyhow::Result<Vec<DetectionHit>>
where
    F: FnMut(&InsnCtx) -> Option<DetectionHit>,
{
    // ensure the whole program is disassembled (cached across passes)
    let all = ctx.all_function_disassembly().await?;
    // visit every instruction of every function
    let mut hits = Vec::new();
    for (entry, insns) in &all {
        for idx in 0..insns.len() {
            let cursor = InsnCtx {
                func_entry: *entry,
                insns,
                idx,
            };
            if let Some(hit) = visit(&cursor) {
                hits.push(hit);
            }
        }
    }
    Ok(hits)
}

/// Parse an immediate operand token (hex `0x..` or decimal) to `u64`.
///
/// # Arguments
///
/// * `text` - The operand token (already trimmed)
pub fn parse_imm(text: &str) -> Option<u64> {
    // hex immediates carry an 0x prefix; everything else is decimal
    let t = text.trim();
    if let Some(hex) = t.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u64>().ok()
    }
}

/// Recover a value moved into a register from the instructions preceding a site.
///
/// Handles `mov <reg>, <imm>` (accepting the 32/64-bit register pair, since
/// writing `eax` also sets `rax`) and `xor <reg>, <reg>` (which yields 0). Used
/// to recover CPUID leaves and syscall numbers from `eax`/`rax`.
///
/// # Arguments
///
/// * `preceding` - The instructions before the site, in program order
/// * `reg32` - The 32-bit register name to match (e.g. `"eax"`)
/// * `reg64` - The 64-bit register name to match (e.g. `"rax"`)
pub fn reg_value_before(preceding: &[Instruction], reg32: &str, reg64: &str) -> Option<u64> {
    // walk backwards from the site to the most recent write of the register
    for insn in preceding.iter().rev() {
        let m = insn.mnemonic.to_ascii_uppercase();
        let ops = crate::thorpal::detect::op_str(insn).to_ascii_lowercase();
        // `mov reg, imm` — the immediate is the value
        if m == "MOV"
            && (ops.starts_with(&format!("{reg32},")) || ops.starts_with(&format!("{reg64},")))
        {
            return ops.split(',').nth(1).and_then(parse_imm);
        }
        // `xor reg, reg` — zeroes the register
        if m == "XOR" {
            let same32 = ops == format!("{reg32},{reg32}") || ops == format!("{reg32}, {reg32}");
            let same64 = ops == format!("{reg64},{reg64}") || ops == format!("{reg64}, {reg64}");
            if same32 || same64 {
                return Some(0);
            }
        }
    }
    None
}

/// Emit hits for defined strings containing any of the given (lower-case) needles.
///
/// For each matching string, emit a single hit naming every code cross-reference to
/// it (see [`referenced_literal_hit`]), or a single low-confidence hit at the string
/// itself when it is unreferenced. An
/// unreferenced string is still attributed to the function that references its
/// buffer via the cached data-reference index, when one does, so the flag links
/// there rather than the sample; a string no function references stays sample-
/// scoped. The index is built lazily — only if an unreferenced match is met — so a
/// fully-referenced match set never forces a whole-program disassembly.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `technique` - The ATT&CK technique these artifacts evidence
/// * `needles` - Lower-case substrings to match against string values
/// * `severity` - The `(confidence, suspicion)` severity for a referenced string
/// * `reasoning` - The analyst-facing explanation
/// * `excludes` - Lower-case terms that veto an otherwise-matching string
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
#[instrument(name = "detections::string_artifact_hits", skip_all, err(Debug))]
pub async fn string_artifact_hits(
    ctx: &AnalysisContext,
    technique: AttackTechnique,
    needles: &[&str],
    severity: (Confidence, i64),
    reasoning: &str,
    excludes: &[&str],
) -> anyhow::Result<Vec<DetectionHit>> {
    let mut hits = Vec::new();
    // the reverse data-reference index, built lazily only when an unreferenced string
    // needs attributing (so a fully-referenced match set never forces a disassembly)
    let mut index: Option<Vec<(u64, u64)>> = None;
    // examine every defined string once
    for string in &ctx.strings {
        // lower-case the value for case-insensitive substring matching
        let hay = string.value.to_ascii_lowercase();
        // require a needle match and no excluded term (e.g. "frida" not in "friday")
        if !needles.iter().any(|n| hay.contains(n)) {
            continue;
        }
        if excludes.iter().any(|e| hay.contains(e)) {
            continue;
        }
        // a short, safe rendering of the matched string for the flag content
        let shown = short(&string.value, 80);
        // resolve the code cross-references to this string
        let xrefs = ctx
            .xrefs_to(&string.address)
            .await
            .with_context(|| format!("resolving references to string at {}", string.address))?;
        if xrefs.is_empty() {
            // unreferenced string: weaker signal, anchored at the string address
            let at = crate::thorpal::detect::context::parse_addr(&string.address).unwrap_or(0);
            // build the reference index on first need, then attribute the string to a
            // function that references its buffer (else leave it sample-scoped); bind
            // the freshly-built index directly so the lookup needs no unwrap
            let index = match &mut index {
                Some(index) => index,
                slot => slot.insert(ctx.reference_index().await?),
            };
            let function = ctx
                .containing_function(at)
                .or_else(|| referencing_function(index, at, DATA_REF_WINDOW));
            hits.push(DetectionHit::new(
                technique.clone(),
                at,
                function,
                low(),
                format!("string {shown:?}"),
                format!("{reasoning} (string not referenced by code in this analysis)"),
            ));
            continue;
        }
        // one hit per string, not per use site (see [`referenced_literal_hit`])
        let sites = xref_sites(&xrefs);
        if let Some(hit) = referenced_literal_hit(
            technique.clone(),
            &shown,
            sites,
            severity,
            reasoning,
            |site| ctx.containing_function(site),
        ) {
            hits.push(hit);
        }
    }
    Ok(hits)
}

/// The code sites referencing a target, in ascending address order and de-duplicated.
///
/// # Arguments
///
/// * `xrefs` - The cross-references resolved for the target address
pub fn xref_sites(xrefs: &[XRef]) -> Vec<u64> {
    // parse each reference's source address, dropping any the bridge did not render
    let mut sites: Vec<u64> = xrefs
        .iter()
        .filter_map(|xref| parse_addr(&xref.from))
        .collect();
    // sort + dedupe so the rendered site list is stable and free of repeats
    sites.sort_unstable();
    sites.dedup();
    sites
}

/// Collapse every reference to one literal into a single hit that names all of them.
///
/// A literal a program consults from a dozen places is one artifact used a dozen times,
/// not a dozen findings. Ghidra makes that distinction easy to lose: its constant
/// propagation records a data reference at *every* instruction the literal's address
/// flows through, so a single use routinely surfaces as five to ten cross-references.
/// Emitting one hit each buries every other finding under copies of the same sentence
/// and makes the containing function look busier than it is.
///
/// The hit is anchored at the first site that resolves to a containing function so the
/// flag links to code, falling back to the lowest site when none does (a data-section
/// reference sorting lowest must not sample-scope an otherwise well-attributed finding).
/// Every site is attached to the hit, so the validator can inspect each one and the
/// markup pass can leave a note at each one.
///
/// Returns `None` when there are no sites to report.
///
/// # Arguments
///
/// * `technique` - The ATT&CK technique the literal evidences
/// * `shown` - The short, quote-safe rendering of the literal for the content
/// * `sites` - The code sites referencing the literal, in address order
/// * `severity` - The `(confidence, suspicion)` severity for the collapsed hit
/// * `reasoning` - The analyst-facing explanation
/// * `containing` - Resolves a site to its containing function (normally
///   [`AnalysisContext::containing_function`])
pub fn referenced_literal_hit(
    technique: AttackTechnique,
    shown: &str,
    sites: Vec<u64>,
    severity: (Confidence, i64),
    reasoning: &str,
    containing: impl Fn(u64) -> Option<u64>,
) -> Option<DetectionHit> {
    // nothing referenced the literal: the caller handles that case on its own
    if sites.is_empty() {
        return None;
    }
    // anchor on the first site that sits in a function, else the lowest site
    let (anchor, function) = sites
        .iter()
        .find_map(|site| containing(*site).map(|entry| (*site, Some(entry))))
        .unwrap_or((sites[0], None));
    // one hit for the literal, naming every place it is referenced from
    let hit = DetectionHit::new(
        technique,
        anchor,
        function,
        severity,
        format!("string {shown:?} referenced @ {}", render_sites(&sites)),
        reasoning.to_string(),
    )
    .sites(sites);
    Some(hit)
}

/// The max reference sites named in a string-artifact hit's content.
///
/// Enough to spot-check where a string is used; past that the count says more than the
/// addresses do.
const MAX_LISTED_SITES: usize = 12;

/// Render a string's use sites, naming the first few and counting the rest.
///
/// # Arguments
///
/// * `sites` - The code sites referencing the string, in address order
fn render_sites(sites: &[u64]) -> String {
    let listed = sites
        .iter()
        .take(MAX_LISTED_SITES)
        .map(|site| fmt_addr(*site))
        .collect::<Vec<_>>()
        .join(", ");
    // no silent truncation: say how many sites were left out
    match sites
        .len()
        .checked_sub(MAX_LISTED_SITES)
        .filter(|extra| *extra > 0)
    {
        Some(extra) => format!("{listed} (+{extra} more; {} sites in total)", sites.len()),
        None => listed,
    }
}

// ===========================================================================
// Byte-signature scanning (findcrypt-style constant detection)
// ===========================================================================

/// A named byte-signature to scan the program's sections for.
///
/// Used by [`signature_hits`] to detect well-known constants (e.g. an AES S-box
/// or a ChaCha nonce) that betray a cryptographic implementation even in a
/// stripped binary with no crypto imports.
#[derive(Debug, Clone, Copy)]
pub struct ByteSig {
    /// A short label for the constant (e.g. `"AES forward S-box"`).
    pub name: &'static str,
    /// The literal bytes to search for in the program's initialized sections.
    pub bytes: &'static [u8],
    /// The analyst-facing explanation (what the constant proves + why it matters).
    pub reasoning: &'static str,
}

/// Return the start offsets of every non-overlapping occurrence of `needle`.
///
/// # Arguments
///
/// * `haystack` - The bytes to search within
/// * `needle` - The byte sequence to find
fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    // an empty needle (or one longer than the haystack) never matches
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    let mut offsets = Vec::new();
    let mut i = 0;
    // walk the haystack, jumping past each hit so occurrences stay disjoint
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            offsets.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    offsets
}

/// Emit hits for any of the given byte-signatures found in initialized sections.
///
/// Scans every initialized memory block (via
/// [`AnalysisContext::initialized_bytes`]) for each signature's byte sequence. Each
/// match is attributed to a function so the flag links there rather than dangling on
/// the sample: its containing function when the constant sits inside a code section,
/// else the function that references the constant's buffer (via the cached data-
/// reference index) — the same attribution the crypto detection already uses for
/// recovered key material. A constant no function references stays sample-scoped.
///
/// Matches that share a signature, a section, and an attributed function are then
/// collapsed into **one** hit naming every address, because a table stored twice
/// back-to-back is one implementation seen twice, not two findings.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `technique` - The ATT&CK technique these constants evidence
/// * `sigs` - The byte-signature catalog to scan for
/// * `severity` - The `(confidence, suspicion)` severity for a match
/// * `kind` - The evidence kind applied to each hit
///
/// # Errors
///
/// Returns an error if reading the program's section bytes / references fails.
#[instrument(name = "detections::signature_hits", skip_all, err(Debug))]
pub async fn signature_hits(
    ctx: &AnalysisContext,
    technique: AttackTechnique,
    sigs: &[ByteSig],
    severity: (Confidence, i64),
    kind: EvidenceKind,
) -> anyhow::Result<Vec<DetectionHit>> {
    let mut hits = Vec::new();
    // read (cached) the bytes of every initialized section once
    let sections = ctx.initialized_bytes().await?;
    // read (cached) the reverse data-reference index once, to attribute a constant in
    // a data section to the function that references it
    let index = ctx.reference_index().await?;
    // scan each section for each signature
    for (name, base, bytes) in &sections {
        // group each signature's matches by the function they attribute to: a constant
        // table the compiler stored twice back-to-back is one fact about the sample, and
        // emitting a flag per copy just duplicates the same sentence
        let mut grouped: BTreeMap<(usize, Option<u64>), Vec<u64>> = BTreeMap::new();
        for (idx, sig) in sigs.iter().enumerate() {
            // record every non-overlapping occurrence of the signature
            for offset in find_all(bytes, sig.bytes) {
                // map the in-section offset back to an absolute address
                let addr = base.saturating_add(offset as u64);
                // attribute the constant: its containing function (code section), else
                // the function that references its buffer, else None (sample-scoped)
                let function = ctx
                    .containing_function(addr)
                    .or_else(|| referencing_function(&index, addr, DATA_REF_WINDOW));
                grouped.entry((idx, function)).or_default().push(addr);
            }
        }
        // one hit per (signature, attributed function) group found in this section
        for ((idx, function), addrs) in grouped {
            hits.push(build_signature_hit(
                technique.clone(),
                kind,
                &sigs[idx],
                name,
                function,
                addrs,
                severity,
            ));
        }
    }
    Ok(hits)
}

/// Build one hit for every match of a signature attributed to the same function.
///
/// # Arguments
///
/// * `technique` - The ATT&CK technique the constant evidences
/// * `kind` - The evidence kind routing validation
/// * `sig` - The matched byte-signature
/// * `section` - The name of the section the matches were found in
/// * `function` - The function the constant attributes to, when one does
/// * `addrs` - Every address the signature matched at, in this section
/// * `severity` - The `(confidence, suspicion)` severity for a match
// seven parameters is one over the lint's bar, and the alternative is a struct that
// exists only to be destructured at the single call site
#[allow(clippy::too_many_arguments)]
fn build_signature_hit(
    technique: AttackTechnique,
    kind: EvidenceKind,
    sig: &ByteSig,
    section: &str,
    function: Option<u64>,
    mut addrs: Vec<u64>,
    severity: (Confidence, i64),
) -> DetectionHit {
    // sort + dedupe the match addresses for stable, clean output
    addrs.sort_unstable();
    addrs.dedup();
    // anchor the hit at the first (lowest) occurrence
    let primary = addrs.first().copied().unwrap_or(0);
    // say out loud when the constant was found more than once, so collapsing never
    // reads as having dropped the other copies
    let repeat_note = match addrs.len() {
        0 | 1 => String::new(),
        n => format!(
            " The constant is stored {n} times here (copies of one table, not {n} separate \
             implementations)."
        ),
    };
    DetectionHit::new(
        technique,
        primary,
        function,
        severity,
        format!(
            "{} constant @ {} (section {section})",
            sig.name,
            render_sites(&addrs)
        ),
        format!("{}{repeat_note}", sig.reasoning),
    )
    .kind(kind)
    .sites(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal technique for building test hits.
    const TECH: AttackTechnique = AttackTechnique {
        id: Cow::Borrowed("T1486"),
        name: Cow::Borrowed("Data Encrypted for Impact"),
        tactic: AttackTactic::Impact,
    };

    /// Runtime classification weeds runtime code but never pivot artifacts.
    #[test]
    fn runtime_classification_vetoes_artifacts_and_contexts() {
        use crate::thorpal::detect::toolchain::Toolchain;
        // build a bare unsure hit sitting inside some function
        let mut hit = DetectionHit {
            technique: TECH,
            address: 0x1010,
            function: Some(0x1000),
            kind: EvidenceKind::ContextOnly,
            confidence: Confidence::Unsure,
            suspicion: 40,
            content: String::new(),
            name: None,
            reasoning: String::new(),
            sites: Vec::new(),
            source: HitSource::Native,
            artifact: None,
            context: None,
        };
        // a Rust runtime function classifies as runtime noise
        let classified =
            runtime_classification(Some(Toolchain::Rust), Some("core::fmt::write"), &hit);
        assert!(matches!(classified, Some(CallContext::Runtime(_))));
        // the sample's own function never classifies
        assert!(
            runtime_classification(Some(Toolchain::Rust), Some("decrypt_config"), &hit).is_none()
        );
        // MSVC CRT markers classify even with no toolchain fingerprint
        assert!(runtime_classification(None, Some("__scrt_fastfail"), &hit).is_some());
        // a hit outside any function cannot be attributed to a runtime
        assert!(runtime_classification(Some(Toolchain::Rust), None, &hit).is_none());
        // a protected artifact vetoes classification wherever it sits
        hit.artifact = Some(HitArtifact {
            class: ArtifactClass::ConfigRecord,
            value: "cfg".to_owned(),
            role: None,
            detail: None,
        });
        assert!(
            runtime_classification(Some(Toolchain::Rust), Some("core::fmt::write"), &hit).is_none()
        );
        // an existing context is never overwritten
        hit.artifact = None;
        hit.context = Some(CallContext::GatesBranch);
        assert!(
            runtime_classification(Some(Toolchain::Rust), Some("core::fmt::write"), &hit).is_none()
        );
    }

    /// Every reference to one literal collapses into a single hit naming them all.
    #[test]
    fn referenced_literal_collapses_to_one_hit() {
        // one string the constant propagator tracked through four instructions
        let sites = vec![0x53ab17, 0x53abd9, 0x53acad, 0x53b53d];
        let hit = referenced_literal_hit(
            TECH,
            "\n-----BEGIN ",
            sites.clone(),
            high(),
            "a PEM boundary",
            // every site sits in the same function
            |_| Some(0x53aad0),
        )
        .expect("a referenced literal produces a hit");
        // the content names every site, not just the anchor
        assert_eq!(
            hit.content,
            "string \"\\n-----BEGIN \" referenced @ 0x53ab17, 0x53abd9, 0x53acad, 0x53b53d"
        );
        // and every site rides along for the validator and the markup pass
        assert_eq!(hit.sites, sites);
        assert_eq!(hit.function, Some(0x53aad0));
    }

    /// The anchor prefers a site inside a function over a lower data reference.
    #[test]
    fn referenced_literal_anchors_on_a_site_in_a_function() {
        // the lowest site is a data reference; only the second sits in code
        let hit = referenced_literal_hit(
            TECH,
            "secret",
            vec![0x8e80f0, 0x53ab17],
            high(),
            "reason",
            |site| (site == 0x53ab17).then_some(0x53aad0),
        )
        .expect("a referenced literal produces a hit");
        // anchoring on the data reference would have sample-scoped the flag
        assert_eq!(hit.address, 0x53ab17);
        assert_eq!(hit.function, Some(0x53aad0));
    }

    /// With no site inside a function the hit falls back to the lowest address.
    #[test]
    fn referenced_literal_falls_back_to_the_lowest_site() {
        let hit = referenced_literal_hit(
            TECH,
            "secret",
            vec![0x8e80f0, 0x8e8100],
            high(),
            "reason",
            |_| None,
        )
        .expect("a referenced literal produces a hit");
        assert_eq!(hit.address, 0x8e80f0);
        assert_eq!(hit.function, None);
    }

    /// An unreferenced literal is the caller's problem, not a hit.
    #[test]
    fn referenced_literal_needs_a_site() {
        assert!(
            referenced_literal_hit(TECH, "secret", Vec::new(), high(), "reason", |_| None)
                .is_none()
        );
    }

    /// Build a test cross-reference from the given source address.
    ///
    /// # Arguments
    ///
    /// * `from` - The referencing address, as Ghidra renders it
    fn xref(from: &str) -> XRef {
        XRef {
            from: from.to_string(),
            to: "0x8e80f0".to_string(),
            ref_type: "DATA".to_string(),
            from_function: None,
            to_function: None,
            direction: None,
        }
    }

    /// `xref_sites` parses, sorts, and de-duplicates the reference addresses.
    #[test]
    fn xref_sites_sorts_and_dedupes() {
        // the same site reported twice, out of order, plus an unparseable address
        let xrefs = vec![
            xref("0x401020"),
            xref("0x401000"),
            xref("0x401020"),
            xref("EXTERNAL:00000001"),
        ];
        assert_eq!(xref_sites(&xrefs), vec![0x401000, 0x401020]);
    }

    /// A signature matched twice in one function is one hit naming both copies.
    #[test]
    fn signature_hit_collapses_repeated_matches() {
        let sig = ByteSig {
            name: "ChaCha/Salsa20 nonce",
            bytes: b"expand 32-byte k",
            reasoning: "the ChaCha20 sigma constant.",
        };
        let hit = build_signature_hit(
            TECH,
            EvidenceKind::ContextOnly,
            &sig,
            ".rdata",
            Some(0x401000),
            // the compiler emitted the table twice, back-to-back
            vec![0x7322b0, 0x7322a0],
            medium(),
        );
        assert_eq!(
            hit.content,
            "ChaCha/Salsa20 nonce constant @ 0x7322a0, 0x7322b0 (section .rdata)"
        );
        // the second copy is accounted for out loud, not silently dropped
        assert!(
            hit.reasoning.contains("stored 2 times here"),
            "{}",
            hit.reasoning
        );
        assert_eq!(hit.address, 0x7322a0);
        assert_eq!(hit.sites, vec![0x7322a0, 0x7322b0]);
    }

    /// A single match reads exactly as it did before matches were grouped.
    #[test]
    fn signature_hit_leaves_a_lone_match_alone() {
        let sig = ByteSig {
            name: "AES forward S-box",
            bytes: b"\x63\x7c\x77\x7b",
            reasoning: "the AES S-box.",
        };
        let hit = build_signature_hit(
            TECH,
            EvidenceKind::ContextOnly,
            &sig,
            ".rodata",
            None,
            vec![0x1b42a0],
            medium(),
        );
        assert_eq!(
            hit.content,
            "AES forward S-box constant @ 0x1b42a0 (section .rodata)"
        );
        // nothing to say about repeats when there are none
        assert_eq!(hit.reasoning, "the AES S-box.");
    }

    /// Several direct calls from one function are one hit naming every call.
    #[test]
    fn api_hit_groups_direct_calls() {
        let spec = ApiSpec {
            key: "OutputDebugString",
            severity: high(),
            reasoning: "writes to an attached debugger.",
        };
        let sites = vec![0x1400015a0, 0x1400015b8, 0x1400015d0];
        let hit = build_api_hit(
            TECH,
            EvidenceKind::ContextOnly,
            &spec,
            "OutputDebugString",
            Some(0x140001510),
            &sites,
            None,
        );
        assert_eq!(
            hit.content,
            "OutputDebugString — 3 calls @ 0x1400015a0, 0x1400015b8, 0x1400015d0"
        );
        // the site list is what earns the validator's repetition bump
        assert_eq!(hit.sites, sites);
        // a direct call says nothing about thunks
        assert!(!hit.reasoning.contains("thunk"), "{}", hit.reasoning);
    }

    /// A lone direct call keeps the singular wording and no site list.
    #[test]
    fn api_hit_keeps_a_lone_direct_call_singular() {
        let spec = ApiSpec {
            key: "IsDebuggerPresent",
            severity: high(),
            reasoning: "checks BeingDebugged.",
        };
        let hit = build_api_hit(
            TECH,
            EvidenceKind::ApiResultCheck,
            &spec,
            "IsDebuggerPresent",
            Some(0x401000),
            &[0x401010],
            None,
        );
        assert_eq!(hit.content, "IsDebuggerPresent call @ 0x401010");
        assert!(hit.sites.is_empty());
    }

    /// Use sites are listed up to the cap, and what was left off is stated.
    #[test]
    fn render_sites_names_a_few_and_counts_the_rest() {
        // a handful of sites is listed in full, with nothing appended
        assert_eq!(render_sites(&[0x401000, 0x401020]), "0x401000, 0x401020");
        // past the cap the addresses stop and the count takes over
        let many: Vec<u64> = (0..30).map(|i| 0x401000 + i * 0x10).collect();
        let rendered = render_sites(&many);
        assert!(rendered.starts_with("0x401000, 0x401010, "), "{rendered}");
        assert!(
            rendered.ends_with("(+18 more; 30 sites in total)"),
            "{rendered}"
        );
        assert_eq!(rendered.matches("0x").count(), MAX_LISTED_SITES);
    }

    /// `normalize` strips import decorations and a trailing ANSI/Wide suffix.
    #[test]
    fn normalize_strips_decorations() {
        assert_eq!(normalize("IsDebuggerPresent"), "IsDebuggerPresent");
        assert_eq!(normalize("GetUserNameA"), "GetUserName");
        assert_eq!(normalize("OutputDebugStringW"), "OutputDebugString");
        assert_eq!(normalize("__imp_Sleep"), "Sleep");
        assert_eq!(normalize("_ptrace"), "ptrace");
        assert_eq!(normalize("Foo@4"), "Foo");
    }

    /// `reg_value_before` recovers a `mov eax, imm` and an `xor eax, eax` (=0).
    #[test]
    fn reg_value_before_recovers_immediates() {
        let mov = Instruction {
            address: "1000".into(),
            mnemonic: "MOV".into(),
            operands: vec!["EAX, 0x40000000".into()],
            bytes: None,
            length: None,
            flow_type: None,
        };
        assert_eq!(reg_value_before(&[mov], "eax", "rax"), Some(0x4000_0000));
        let xor = Instruction {
            address: "1000".into(),
            mnemonic: "XOR".into(),
            operands: vec!["eax,eax".into()],
            bytes: None,
            length: None,
            flow_type: None,
        };
        assert_eq!(reg_value_before(&[xor], "eax", "rax"), Some(0));
    }

    /// `find_all` reports every non-overlapping occurrence and nothing spurious.
    #[test]
    fn find_all_reports_disjoint_occurrences() {
        // two disjoint matches of the needle
        assert_eq!(find_all(b"ab__ab__", b"ab"), vec![0, 4]);
        // overlapping candidates are counted only once (disjoint)
        assert_eq!(find_all(b"aaaa", b"aa"), vec![0, 2]);
        // no match / empty needle / oversized needle
        assert_eq!(find_all(b"abc", b"xyz"), Vec::<usize>::new());
        assert_eq!(find_all(b"abc", b""), Vec::<usize>::new());
        assert_eq!(find_all(b"a", b"abc"), Vec::<usize>::new());
    }
}
