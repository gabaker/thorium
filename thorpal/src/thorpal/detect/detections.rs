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
//! (decoded-instruction opcode/operand scanning), [`group_scan_hits`] (the
//! per-function grouping and per-program caps the instruction-scan detectors
//! apply to their output), and [`string_artifact_hits`] (defined-string
//! matches, with opt-in boundary/shape matching via [`MatchOptions`]). Keeping
//! the mechanics here lets each detection file read as a focused,
//! well-commented *catalog* of what it looks for and why.

pub mod command_and_control;
pub mod defense_evasion;
pub mod execution;
pub mod impact;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};

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
    /// Parse a tactic from its canonical ATT&CK display name.
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

impl HitSource {
    /// Every hit source, so readers can gate persisted keys against the
    /// crate's own closed vocabulary instead of a hand-copied list
    pub const ALL: [HitSource; 6] = [
        Self::Native,
        Self::Capa,
        Self::Llm,
        Self::Script,
        Self::Yara,
        Self::Emulation,
    ];

    /// The stable snake_case key naming this source in persisted provenance
    pub fn key(self) -> &'static str {
        // one fixed key per producer; persisted, so never reworded
        match self {
            Self::Native => "native",
            Self::Capa => "capa",
            Self::Llm => "llm",
            Self::Script => "script",
            Self::Yara => "yara",
            Self::Emulation => "emulation",
        }
    }
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
    /// Every artifact class, so readers can gate persisted keys and parsed
    /// labels against the crate's own closed vocabulary instead of a
    /// hand-copied list
    pub const ALL: [ArtifactClass; 12] = [
        Self::NetworkEndpoint,
        Self::UriPath,
        Self::UserAgent,
        Self::HttpHeader,
        Self::HostIdentifier,
        Self::SymmetricKey,
        Self::AsymmetricPrivateKey,
        Self::AsymmetricPublicKey,
        Self::Certificate,
        Self::DecodedData,
        Self::ConfigRecord,
        Self::Packer,
    ];

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

    /// The stable snake_case key naming this class in persisted provenance
    pub fn key(self) -> &'static str {
        // one fixed key per class; persisted, so never reworded
        match self {
            Self::NetworkEndpoint => "network_endpoint",
            Self::UriPath => "uri_path",
            Self::UserAgent => "user_agent",
            Self::HttpHeader => "http_header",
            Self::HostIdentifier => "host_identifier",
            Self::SymmetricKey => "symmetric_key",
            Self::AsymmetricPrivateKey => "asymmetric_private_key",
            Self::AsymmetricPublicKey => "asymmetric_public_key",
            Self::Certificate => "certificate",
            Self::DecodedData => "decoded_data",
            Self::ConfigRecord => "config_record",
            Self::Packer => "packer",
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
    /// The producing detection's stable name, when one stamped it: a native
    /// registry name (see [`Detection::name`]), or the library script name
    /// for script hits. `None` for producers that have no per-detection name
    /// (capa imports, the LLM pass) and for hits that predate stamping.
    pub detection: Option<String>,
    /// The normalized catalog API name behind this hit, when an API catalog
    /// produced it (set by `build_api_hit` from the matched spec's key).
    ///
    /// Lets the post-detection validator (`crate::thorpal::detect::validate`)
    /// identify the API robustly instead of re-parsing the rendered content.
    pub api: Option<&'static str>,
    /// The catalog's asserted `(confidence, suspicion)` severity, kept aside
    /// when the presence-only clamp in `build_api_hit` seeded this hit below
    /// it, so the validation pass can restore it once use is verified (a
    /// result that gates a branch, or a recovered in-set argument).
    pub catalog_severity: Option<(Confidence, i64)>,
    /// Whether this hit is inherent noise wherever it sits: the seeder treats
    /// `always_noise` hits as rollup-eligible noise regardless of toolchain
    /// (set from the producing catalog entry's assertion, e.g. a base64
    /// alphabet constant every encoder carries).
    pub always_noise: bool,
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
            // unstamped until the registry runner names its producer
            detection: None,
            // not produced by an API catalog unless build_api_hit says so
            api: None,
            // no clamped-away catalog severity unless the presence clamp sets one
            catalog_severity: None,
            // ordinary signal by default; noise-asserting catalog entries flip this
            always_noise: false,
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

    /// Record the normalized catalog API name behind this hit (builder style).
    ///
    /// # Arguments
    ///
    /// * `api` - The matched catalog entry's normalized API name
    pub fn api(mut self, api: &'static str) -> Self {
        // record the catalog API name and return self for chaining
        self.api = Some(api);
        self
    }

    /// Record the catalog severity the presence-only clamp held back (builder style).
    ///
    /// # Arguments
    ///
    /// * `severity` - The catalog's asserted `(confidence, suspicion)` pair
    pub fn catalog_severity(mut self, severity: (Confidence, i64)) -> Self {
        // record the withheld severity and return self for chaining
        self.catalog_severity = Some(severity);
        self
    }

    /// Mark this hit as inherent noise the seeder may roll up regardless of
    /// toolchain (builder style).
    pub fn always_noise(mut self) -> Self {
        // record the noise assertion and return self for chaining
        self.always_noise = true;
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

/// One registry detection's outcome over a single seeding pass
///
/// The full roster -- including detections that produced zero hits -- is what
/// makes "detectors that never fired" answerable offline: the registry
/// compiled into the recording binary may differ from any later reader's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectorRun {
    /// The detection's stable registry name (see [`Detection::name`])
    pub name: &'static str,
    /// The ATT&CK technique id the detection evidences
    pub technique: String,
    /// How many hits the detection produced
    pub hits: u32,
    /// Whether the detection failed instead of completing
    pub failed: bool,
}

/// Runs every registered detection, reporting the roster that ran
///
/// A detection that fails is logged at ERROR (naming the detection and
/// technique), recorded as failed on the roster, and skipped; the remaining
/// detections still run, so one broken Ghidra query or parse never sinks the
/// whole report. Every returned hit is stamped with its producing
/// detection's name. This never returns an error for a detection failure.
///
/// # Arguments
///
/// * `ctx` - The shared analysis context
#[instrument(name = "detections::run_all_with_roster", skip_all)]
pub async fn run_all_with_roster(
    ctx: &AnalysisContext,
) -> anyhow::Result<(Vec<DetectionHit>, Vec<DetectorRun>)> {
    // accumulate hits from each detection in registry order
    let mut hits = Vec::new();
    // record every detection's outcome, hit or miss, for the roster
    let mut roster = Vec::new();
    for detection in all() {
        // run the detection; on failure, log and continue with the rest so a single
        // broken detection never aborts the whole tool
        match detection.run(ctx).await {
            Ok(mut found) => {
                // stamp the producer onto every hit it returned
                stamp_detection(detection.name(), &mut found);
                // log one concise line with the detection's hit count
                tracing::info!(
                    detection = detection.name(),
                    technique = %detection.technique().id,
                    count = found.len(),
                    "detection complete"
                );
                roster.push(DetectorRun {
                    name: detection.name(),
                    technique: detection.technique().id.to_string(),
                    hits: found.len() as u32,
                    failed: false,
                });
                hits.extend(found);
            }
            Err(error) => {
                // record the failure and keep going
                roster.push(DetectorRun {
                    name: detection.name(),
                    technique: detection.technique().id.to_string(),
                    hits: 0,
                    failed: true,
                });
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
    let failed = roster.iter().filter(|run| run.failed).count();
    if failed > 0 {
        tracing::warn!(failed, "some detections failed; report is partial");
    }
    Ok((hits, roster))
}

/// Stamps the producing detection's name onto hits that lack one
///
/// # Arguments
///
/// * `name` - The detection's stable registry name
/// * `hits` - The hits the detection just produced
fn stamp_detection(name: &'static str, hits: &mut [DetectionHit]) {
    // never overwrite a name a detection chose to set itself
    for hit in hits {
        if hit.detection.is_none() {
            hit.detection = Some(name.to_owned());
        }
    }
}

// ===========================================================================
// Shared detection helpers
// ===========================================================================

/// A catalog entry describing one imported API to flag.
#[derive(Debug, Clone, Copy)]
pub struct ApiSpec {
    /// The normalized API name to match (e.g. `"IsDebuggerPresent"`).
    pub key: &'static str,
    /// Whether calls to this API are inherent noise: hits built from this entry
    /// carry [`DetectionHit::always_noise`], which the seeder treats as
    /// rollup-eligible noise regardless of toolchain.
    pub always_noise: bool,
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
/// containing function's name matches the runtime/library marker set or the
/// fingerprinted toolchain's runtime markers — unless it already carries a
/// context, or it carries a *validated* pivot-class artifact (configs,
/// endpoints, keys, certificates, host identifiers — see
/// [`super::artifact_gates`]): a real recovery stays loud wherever it sits,
/// while a fabricated value (a carved degenerate MAC, junk "recovered" text)
/// no longer shields runtime noise.
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
    // validated pivot-class artifacts are never written off as runtime noise
    if has_validated_pivot_artifact(hit) {
        return None;
    }
    // only hits inside a named function can be attributed to a runtime
    let name = fn_name?;
    // the shared runtime/library marker set catches C/C++ support code in
    // every toolchain
    if is_library_function(name) {
        return Some(CallContext::Runtime(format!(
            "{name}, C/C++ runtime support"
        )));
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

/// Whether a hit carries a pivot-class artifact that passed its class's gate.
///
/// The artifact veto that keeps real recoveries loud is gated per class (see
/// [`super::artifact_gates::artifact_is_validated`]): a fabricated value — a
/// carved degenerate MAC, junk "recovered" plaintext — earns no runtime-noise
/// immunity, so the hit it rides on is judged like any other.
///
/// # Arguments
///
/// * `hit` - The hit whose artifact to judge
fn has_validated_pivot_artifact(hit: &DetectionHit) -> bool {
    // only pivot classes (values an analyst can act on) can earn the veto
    let Some(artifact) = &hit.artifact else {
        return false;
    };
    let pivot = matches!(
        artifact.class,
        ArtifactClass::ConfigRecord
            | ArtifactClass::NetworkEndpoint
            | ArtifactClass::SymmetricKey
            | ArtifactClass::AsymmetricPrivateKey
            | ArtifactClass::AsymmetricPublicKey
            | ArtifactClass::Certificate
            | ArtifactClass::HostIdentifier
    );
    // the per-class gate decides whether the recovered value earns the privilege
    pivot && super::artifact_gates::artifact_is_validated(hit)
}

/// Classify hits sitting in recognizable runtime code and weaken them once.
///
/// The ported engine never carried the source project's provenance walk, so
/// `DetectionHit::context` was never set (see [`super::provenance_types`]).
/// This is the bounded replacement the `build_api_hit` tombstone reserved: a
/// name-based check of every hit's containing function against the
/// runtime/library marker set and the fingerprinted toolchain's runtime
/// markers, applied once over the merged hit list so every detection path is
/// judged by the same rule. Where the name check comes up empty (stripped
/// binaries), the name-free structural recognizers (see
/// [`structural_runtime_label`]) judge the hit by its containing function's
/// catalog-call set, derived from the API hits themselves — no bridge
/// queries. A classified hit is weakened one step and gains
/// [`CallContext::Runtime`], which the forum seeder renders as the existing
/// runtime note. Hits that already weakened themselves to the floor stay
/// there (`weaken` saturates), so double application is harmless.
///
/// # Arguments
///
/// * `ctx` - The analysis context (toolchain + function names)
/// * `hits` - The hits to classify in place
pub fn apply_runtime_context(ctx: &AnalysisContext, hits: &mut [DetectionHit]) {
    // derive each function's catalog-call set from the API hits themselves —
    // the cheap, bridge-free map the structural recognizers run on
    let calls = function_call_map(hits);
    // judge every hit by the shared rules, resolving names through the context
    for hit in hits.iter_mut() {
        // resolve the containing function's name, when the hit sits in one
        let name = hit.function.and_then(|entry| ctx.function_name(entry));
        // the name-based rule first; the name-free structural recognizers
        // catch stripped runtime code the name check is blind to
        let context = runtime_classification(ctx.toolchain, name, hit)
            .or_else(|| structural_classification(&calls, hit));
        if let Some(context) = context {
            // runtime-located evidence is one confidence step weaker, and the
            // note below tells the agents why
            hit.weaken();
            hit.context = Some(context);
        }
    }
}

/// Build each function's set of catalog API calls from the merged hits.
///
/// The cheap per-function call map available at [`apply_runtime_context`]'s
/// call site: every API-catalog hit records its normalized API name
/// ([`DetectionHit::api`]) and calling function, so the merged list already
/// says which catalog APIs each function calls — with no bridge queries.
/// Coverage is bounded by the catalogs: an imported API no catalog lists
/// never appears here, so the structural recognizers fire only when enough
/// cluster members are themselves catalogued (see the integration note on
/// [`structural_runtime_label`]).
///
/// # Arguments
///
/// * `hits` - The merged detection hits
fn function_call_map(hits: &[DetectionHit]) -> HashMap<u64, Vec<&'static str>> {
    // collect each function's distinct catalog API names
    let mut map: HashMap<u64, Vec<&'static str>> = HashMap::new();
    for hit in hits {
        if let (Some(function), Some(api)) = (hit.function, hit.api) {
            // record each API once per function
            let calls = map.entry(function).or_default();
            if !calls.contains(&api) {
                calls.push(api);
            }
        }
    }
    map
}

/// The imported APIs whose co-location marks MSVC's fail-fast/report cluster.
///
/// `__report_gsfailure` / `__raise_securityfailure` / the fail-fast filter
/// call these together from one small cluster of CRT functions; three or more
/// of them resolved from one function is the structural signature of that
/// failure-reporting code even when every name is stripped.
const FAILFAST_CLUSTER: &[&str] = &[
    "IsDebuggerPresent",
    "RtlCaptureContext",
    "SetUnhandledExceptionFilter",
    "UnhandledExceptionFilter",
    "TerminateProcess",
    "GetCurrentProcess",
];

/// The entropy-source APIs MSVC's `__security_init_cookie` mixes into the
/// /GS stack cookie.
///
/// The canonical cookie init reads the system time, the thread and process
/// ids, the performance counter, and the tick counter and XORs them into one
/// global. `GetTickCount64` counts as the tick-counter member (newer CRTs
/// call it instead of `GetTickCount`).
const GS_COOKIE_CLUSTER: &[&str] = &[
    "GetSystemTimeAsFileTime",
    "GetCurrentThreadId",
    "GetCurrentProcessId",
    "QueryPerformanceCounter",
    "GetTickCount",
];

/// How many distinct cluster members a function must call to be recognized.
///
/// Three is the precision bar: a sample's own SEH anti-debug typically pairs
/// two of the fail-fast APIs, and a timing check pairs two of the cookie
/// entropy sources, but only the CRT's own helpers co-locate three or more.
const CLUSTER_MIN: usize = 3;

/// Recognize compiler-runtime helpers by call-set shape, with no names.
///
/// The structural complement to [`is_library_function`] for stripped
/// binaries, where the name check is blind: (a) the MSVC fail-fast/report
/// cluster, and (b) the /GS security-cookie init cluster. Only the call-set
/// test runs — the cookie recognizer does not verify the XOR-into-one-global
/// write (that needs the function's disassembly, which this pure check does
/// not take), trading a sliver of precision for zero extra queries: a
/// sample's own code calling three distinct cookie-entropy APIs from one
/// function is rare, and a match costs weaken-plus-note, never a drop.
///
/// Integration: [`apply_runtime_context`] feeds this the per-function
/// catalog-call sets derived from the hits themselves (see
/// `function_call_map`). That map covers the /GS cluster today (three of its
/// timing members are catalogued) but cannot reach three fail-fast members
/// (only `IsDebuggerPresent` is catalogued), so the fail-fast recognizer
/// only becomes effective once a later pass supplies the full per-function
/// imported-call map — e.g. built from the call sites `api_call_hits`
/// already resolves, or from the cached whole-program disassembly. That pass
/// should call this with the richer set; the recognizer itself needs no
/// change.
///
/// # Arguments
///
/// * `calls` - The distinct imported APIs the function's resolved call set
///   includes (normalized names, see [`normalize`])
pub fn structural_runtime_label<'a, I>(calls: I) -> Option<&'static str>
where
    I: IntoIterator<Item = &'a str>,
{
    // tally the distinct members of each cluster present in the call set
    let mut failfast: Vec<&'static str> = Vec::new();
    let mut cookie: Vec<&'static str> = Vec::new();
    for call in calls {
        // fold the 64-bit tick counter onto its cluster member
        let call = if call == "GetTickCount64" {
            "GetTickCount"
        } else {
            call
        };
        // record each matched member once, in whichever cluster it belongs to
        if let Some(&member) = FAILFAST_CLUSTER.iter().find(|member| **member == call) {
            if !failfast.contains(&member) {
                failfast.push(member);
            }
        }
        if let Some(&member) = GS_COOKIE_CLUSTER.iter().find(|member| **member == call) {
            if !cookie.contains(&member) {
                cookie.push(member);
            }
        }
    }
    // three or more co-located members is the structural signature
    if failfast.len() >= CLUSTER_MIN {
        return Some("MSVC fail-fast/report support (structural call-set match)");
    }
    if cookie.len() >= CLUSTER_MIN {
        return Some("MSVC /GS security-cookie init (structural call-set match)");
    }
    None
}

/// Classify a hit the name rule could not attribute, by call-set shape.
///
/// Applies the same exemptions as [`runtime_classification`] (an existing
/// context, a validated pivot-class artifact), then matches the containing
/// function's catalog-call set against the structural clusters.
///
/// # Arguments
///
/// * `calls` - Each function's distinct catalog API calls (see `function_call_map`)
/// * `hit` - The hit to judge
fn structural_classification(
    calls: &HashMap<u64, Vec<&'static str>>,
    hit: &DetectionHit,
) -> Option<CallContext> {
    // an already-classified hit was judged by an earlier pass; leave it alone
    if hit.context.is_some() {
        return None;
    }
    // validated pivot-class artifacts are never written off as runtime noise
    if has_validated_pivot_artifact(hit) {
        return None;
    }
    // only hits inside a known function have a call set to judge
    let entry = hit.function?;
    let called = calls.get(&entry)?;
    // a cluster match classifies the whole function as CRT support code
    let label = structural_runtime_label(called.iter().copied())?;
    Some(CallContext::Runtime(label.to_string()))
}

/// Heuristically recognize C/C++ runtime and common-library function names.
///
/// Covers the MSVC/CRT support helpers plus the marker families of the other
/// runtimes that flood stripped-adjacent binaries: mingw, libstdc++/libsupc++,
/// glibc (loader, resolver, NSS, libc internals, stdio), musl, and OpenSSL.
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
    // markers matched anywhere in the (lowercased) name
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
        // mingw decorates its runtime helpers with the toolchain's own name
        // (`__mingw_CRTStartup`, `mingw_getsp`), which no other code carries
        "mingw",
        // the Itanium C++ ABI runtime entry points libstdc++/libsupc++ export
        // (`__cxa_throw`, `__cxa_guard_acquire`)
        "__cxa_",
        // libstdc++'s extension namespace (`__gnu_cxx::...`) in demangled names
        "__gnu_cxx",
        // glibc's resolver (`__res_vinit`), NSS (`__nss_database_lookup`), and
        // libc-internal (`__libc_start_main`) families: the markers' own
        // double-underscore prefix is implementation-reserved and acts as the
        // token boundary, so a bare `contains` cannot fire inside ordinary
        // identifiers like `resolver` (no underscores)
        "__res_",
        "__nss_",
        "__libc",
    ];
    // families whose marker is a reserved name *prefix*: anchoring at the
    // start keeps `_dl_` from firing inside user names like `my_dll_main` or
    // `handle_dl_open`, and `_io_` out of names like `my_io_handler`
    const PREFIX_MARKERS: &[&str] = &[
        // the glibc dynamic loader (`_dl_relocate_object`)
        "_dl_",
        // glibc stdio internals (`_IO_file_xsputn`), lowercased like the rest
        "_io_",
        // musl's stdio and syscall internals (`__stdio_write`, `__syscall_cp`)
        "__stdio_",
        "__syscall",
        // OpenSSL's own helpers (`OPENSSL_cleanse`, `OPENSSL_init_crypto`)
        "openssl_",
        // the C++ standard library, mangled (`_ZNSt…`) and demangled (`std::…`)
        "_znst",
        "std::",
    ];
    // lowercase once, then check for the contains- and prefix-anchored markers
    let n = name.to_ascii_lowercase();
    MARKERS.iter().any(|m| n.contains(m)) || PREFIX_MARKERS.iter().any(|m| n.starts_with(m))
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

/// How [`api_call_hits_with`] reports catalog APIs that are imported but never
/// referenced from code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportCensus {
    /// Fold every matched-but-unreferenced import into one weak file-level
    /// census hit naming them all. This is the default: an uncalled import
    /// says almost nothing on its own, but a block of them is the decoy
    /// import-table shape crypters pad an IAT with.
    Combined,
    /// Emit one weak hit per matched-but-unreferenced import, for detectors
    /// whose uncalled imports carry a different prior (e.g. crypto capability,
    /// where an uncalled `CryptEncrypt` is itself notable).
    // deliberate opt-out API surface: no detector opts out today (crypto routes
    // its uncalled imports through the combined census plus the always-noise
    // flag instead), but the mode is the documented escape hatch
    #[allow(dead_code)]
    PerImport,
}

/// Emit hits for the call sites of any imported API matching the given specs.
///
/// For each imported symbol whose normalized name matches a spec: resolve its call
/// sites (xrefs) and emit **one hit per calling function**, naming every call it makes
/// — an API a function calls ten times is one behavior repeated, not ten findings (the
/// repetition itself is corroboration, and the validator reads it off the site list).
/// APIs that are imported but have no resolved call site are folded into one weak
/// file-level census hit ([`ImportCensus::Combined`]); detectors that want a hit per
/// unreferenced import call [`api_call_hits_with`] with [`ImportCensus::PerImport`].
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
pub async fn api_call_hits(
    ctx: &AnalysisContext,
    technique: AttackTechnique,
    specs: &[ApiSpec],
    kind: EvidenceKind,
) -> anyhow::Result<Vec<DetectionHit>> {
    // delegate with the combined census default every catalog detector shares
    api_call_hits_with(ctx, technique, specs, kind, ImportCensus::Combined).await
}

/// Emit hits for the call sites of any imported API matching the given specs,
/// with an explicit policy for imported-but-unreferenced APIs.
///
/// See [`api_call_hits`] (the [`ImportCensus::Combined`] wrapper) for the
/// call-site grouping rules; `census` only decides how matched imports with no
/// resolved call site are reported.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `technique` - The ATT&CK technique these APIs evidence
/// * `specs` - The per-API catalog
/// * `kind` - The evidence kind applied to each resolved call-site hit (drives
///   how the validator confirms/refutes it, e.g. [`EvidenceKind::ApiResultCheck`])
/// * `census` - How matched-but-unreferenced imports are reported
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
#[instrument(name = "detections::api_call_hits", skip_all, err(Debug))]
pub async fn api_call_hits_with(
    ctx: &AnalysisContext,
    technique: AttackTechnique,
    specs: &[ApiSpec],
    kind: EvidenceKind,
    census: ImportCensus,
) -> anyhow::Result<Vec<DetectionHit>> {
    let mut hits = Vec::new();
    // the matched imports with no resolved reference, for the census hit below
    let mut unreferenced: Vec<&'static str> = Vec::new();
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
            // imported but unreferenced: report it the way the census mode asks
            match census {
                ImportCensus::Combined => {
                    // collect the miss for the one file-level census hit below
                    unreferenced.push(spec.key);
                }
                ImportCensus::PerImport => {
                    // a weak per-import signal, for catalogs where it means more
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
                }
            }
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
    // fold the unreferenced imports into one weak file-level census hit, when any
    if let Some(hit) = import_census_hit(technique, &unreferenced) {
        hits.push(hit);
    }
    Ok(hits)
}

/// The max unreferenced import names listed in the census hit's content.
const MAX_CENSUS_NAMES: usize = 8;

/// Build the one file-level census hit for catalog APIs that are imported but
/// never referenced from code.
///
/// Returns `None` when every matched import had a resolved reference. The hit
/// is file-level (no containing function, address 0) and weak: a single
/// uncalled import carries almost no signal, but a block of catalog APIs the
/// code never touches is the decoy import-table shape, so the census line
/// preserves that signal without asserting any single API's claim.
///
/// # Arguments
///
/// * `technique` - The ATT&CK technique the catalog evidences
/// * `unreferenced` - The matched-but-unreferenced imports' normalized names
fn import_census_hit(
    technique: AttackTechnique,
    unreferenced: &[&'static str],
) -> Option<DetectionHit> {
    // nothing unreferenced means nothing to report
    if unreferenced.is_empty() {
        return None;
    }
    // de-duplicate while keeping the import-table order (ANSI/Wide twins both
    // normalize to one catalog key and would otherwise list twice)
    let mut names: Vec<&'static str> = Vec::new();
    for name in unreferenced {
        if !names.contains(name) {
            names.push(name);
        }
    }
    // list the first few APIs and elide the rest so the line stays readable
    let mut listed = names
        .iter()
        .take(MAX_CENSUS_NAMES)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > MAX_CENSUS_NAMES {
        listed.push_str(", …");
    }
    // pluralize the count so a lone unreferenced import still reads correctly
    let plural = if names.len() == 1 { "API" } else { "APIs" };
    let content = format!(
        "{} catalog {plural} imported but never referenced from code: {listed}",
        names.len()
    );
    // one census line, worded so the decoy-table signal survives while every
    // per-API behavior claim is explicitly left unasserted
    let reasoning = "These APIs appear in the import table, but no call site or code \
                     reference to any of them was resolved. An import table padded with \
                     suspicious-looking APIs the code never calls is a known crypter/packer \
                     decoy pattern, so the block of them is itself a mild signal — but each \
                     individual API's catalog claim is unverified here, and none of these \
                     sightings is evidence the named behavior exists."
        .to_string();
    Some(DetectionHit::new(
        technique,
        0,
        None,
        low(),
        content,
        reasoning,
    ))
}

/// Build one imported-API call-site hit.
///
/// Centralizes the presence-only severity clamp, the thunk-aware
/// content/reasoning, and attaching the full site list for the validator. The
/// catalog's asserted severity is clamped to at most [`medium`]: the mere
/// presence of a dual-use API call is a context-dependent claim however strong
/// the catalog's prior, so the hit seeds at medium and carries the catalog
/// severity aside, for the validation pass
/// (`crate::thorpal::detect::validate`) to restore once the call's result is
/// seen gating a branch or a recovered argument confirms the claimed use.
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
    // presence of a dual-use API is never more than a medium-grade claim on its
    // own: clamp the catalog's asserted severity to at most medium() and keep
    // the original aside so the validation pass can restore it on verified use
    let clamp = medium();
    let severity = (
        confidence_from_rank(confidence_rank(spec.severity.0).min(confidence_rank(clamp.0))),
        spec.severity.1.min(clamp.1),
    );
    // remember the catalog severity only when the clamp actually lowered it
    let clamped = confidence_rank(spec.severity.0) > confidence_rank(severity.0)
        || spec.severity.1 > severity.1;
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
    let mut hit = DetectionHit::new(technique, primary, function, severity, content, reasoning)
        .kind(kind)
        .api(spec.key);
    // carry the catalog's noise assertion so the seeder can roll the hit up
    if spec.always_noise {
        hit = hit.always_noise();
    }
    // keep the catalog's asserted severity for the validator when it was clamped
    if clamped {
        hit = hit.catalog_severity(spec.severity);
    }
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

/// The detection-specific class of a hit's content: the text before the first
/// site rendering.
///
/// The shared content shapes put the per-site rendering after `" @ "` (a lone
/// site: `"RDTSC timing read @ 0x401000"`) or `" — "` (a grouped count:
/// `"Sleep — 3 calls @ …"`), so the class is everything before whichever
/// separator comes first. Two hits with equal classes are the same observation
/// at different sites. A content with neither separator is its own class.
///
/// # Arguments
///
/// * `content` - The hit content to classify
pub fn content_class(content: &str) -> &str {
    // locate each separator; absent ones push past the end so min() picks the
    // first one actually present
    let at = content.find(" @ ").unwrap_or(content.len());
    let dash = content.find(" — ").unwrap_or(content.len());
    &content[..at.min(dash)]
}

/// Group instruction-scan hits per function and cap generic tells per program.
///
/// The shared post-pass the per-instruction detectors apply to their
/// [`scan_instructions`] output, mirroring the grouping the API/literal/
/// signature helpers already do (see [`api_call_hits`] and
/// [`referenced_literal_hit`]). Two passes run:
///
/// 1. **Per-function grouping** — hits with the same producing detection, the
///    same [`content_class`], and the same containing function merge into a
///    single hit carrying every site (first/lowest site primary, the count
///    rendered in the content the way `build_api_hit` does, the strongest
///    `(confidence, suspicion)` in the group kept so grouping never weakens
///    evidence). A tell repeated ten times in one function is one behavior
///    repeated, not ten findings.
/// 2. **Per-program cap** — hits the caller marks generic (unclassified tells:
///    bare PEB reads, 0xCC compares, lone RDTSC/SYSCALL opcodes) are capped:
///    the `cap` strongest (confidence, then suspicion, then site count) stay
///    individual findings and the overflow collapses into one file-level
///    summary hit naming how many tells, sites, and functions were elided.
///
/// Hits the predicate calls classified are exempt from the cap, and
/// artifact-carrying hits are exempt from both passes — each recovered value
/// is its own finding.
///
/// # Arguments
///
/// * `hits` - One detector's instruction-scan hits
/// * `cap` - The most generic (predicate-true) hits kept individually per program
/// * `is_generic` - Marks the hits that count against the per-program cap
pub fn group_scan_hits<F>(hits: Vec<DetectionHit>, cap: usize, is_generic: F) -> Vec<DetectionHit>
where
    F: Fn(&DetectionHit) -> bool,
{
    // bucket hits by producer + content class + containing function, keeping
    // first-encounter order so the output stays deterministic
    let mut order: Vec<Vec<DetectionHit>> = Vec::new();
    let mut groups: HashMap<(Option<String>, String, Option<u64>), usize> = HashMap::new();
    for hit in hits {
        // a recovered value is its own finding: never merge artifact carriers
        if hit.artifact.is_some() {
            order.push(vec![hit]);
            continue;
        }
        // group by the same-observation key
        let key = (
            hit.detection.clone(),
            content_class(&hit.content).to_string(),
            hit.function,
        );
        match groups.get(&key) {
            Some(idx) => order[*idx].push(hit),
            None => {
                groups.insert(key, order.len());
                order.push(vec![hit]);
            }
        }
    }
    // collapse each group to one hit naming every site
    let merged: Vec<DetectionHit> = order.into_iter().filter_map(merge_scan_group).collect();
    // split the classified hits (cap-exempt) from the generic tells
    let mut kept: Vec<DetectionHit> = Vec::new();
    let mut generic: Vec<DetectionHit> = Vec::new();
    for hit in merged {
        if is_generic(&hit) {
            generic.push(hit);
        } else {
            kept.push(hit);
        }
    }
    // under the cap every generic tell still seeds individually
    if generic.len() <= cap {
        kept.extend(generic);
        return kept;
    }
    // keep the strongest `cap` generic tells as individual findings
    generic.sort_by(|a, b| {
        confidence_rank(b.confidence)
            .cmp(&confidence_rank(a.confidence))
            .then(b.suspicion.cmp(&a.suspicion))
            .then(b.sites.len().cmp(&a.sites.len()))
            .then(a.address.cmp(&b.address))
    });
    let overflow = generic.split_off(cap);
    kept.extend(generic);
    // collapse the overflow into one file-level summary so the elided mass
    // stays counted instead of silently vanishing
    if let Some(summary) = scan_overflow_summary(&overflow, cap) {
        kept.push(summary);
    }
    kept
}

/// Collapse one per-function group of same-class scan hits into a single hit.
///
/// The merged hit is based on the group's lowest-addressed member: it keeps
/// that hit's technique, kind, and reasoning, carries every member's sites
/// (first/lowest primary), renders the site count in the content, and keeps
/// the strongest `(confidence, suspicion)` in the group so grouping never
/// weakens evidence. A lone hit passes through unchanged, and an empty group
/// merges to nothing.
///
/// # Arguments
///
/// * `group` - The same-detection, same-class, same-function hits to collapse
fn merge_scan_group(mut group: Vec<DetectionHit>) -> Option<DetectionHit> {
    // a lone hit needs no merging (and an empty group merges to nothing)
    if group.len() <= 1 {
        return group.pop();
    }
    // gather every backing site (a hit with no list backs just its address)
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
    // the strongest grade among the members survives the merge
    let strongest = group
        .iter()
        .map(|hit| confidence_rank(hit.confidence))
        .max()
        .unwrap_or(0);
    let suspicion = group.iter().map(|hit| hit.suspicion).max().unwrap_or(0);
    // base the merged hit on the lowest-addressed member
    group.sort_by_key(|hit| hit.address);
    let mut base = group.swap_remove(0);
    base.confidence = confidence_from_rank(strongest);
    base.suspicion = suspicion;
    // the first (lowest) site is the primary address
    base.address = sites.first().copied().unwrap_or(base.address);
    // rewrite the content as one class observed at N sites
    let class = content_class(&base.content).to_string();
    base.content = format!("{class} — {} sites @ {}", sites.len(), render_sites(&sites));
    // every site rides along for the validator and the markup pass
    base.sites = sites;
    Some(base)
}

/// The max distinct content classes named in the overflow summary hit.
const MAX_SUMMARY_CLASSES: usize = 4;

/// Build the one file-level summary hit for capped generic scan tells.
///
/// Names how many grouped tells, sites, and functions were elided, plus the
/// first few content classes among them, so the count survives the cap
/// without a flood of identical findings. Returns `None` when nothing was
/// elided.
///
/// # Arguments
///
/// * `overflow` - The generic hits elided by the per-program cap
/// * `cap` - The cap that elided them
fn scan_overflow_summary(overflow: &[DetectionHit], cap: usize) -> Option<DetectionHit> {
    // nothing elided means nothing to summarize
    let first = overflow.first()?;
    // count the elided sites and the distinct functions they sit in
    let sites: usize = overflow.iter().map(|hit| hit.sites.len().max(1)).sum();
    let mut functions: Vec<Option<u64>> = overflow.iter().map(|hit| hit.function).collect();
    functions.sort_unstable();
    functions.dedup();
    // name the first few distinct classes so the summary stays actionable
    let mut classes: Vec<&str> = Vec::new();
    for hit in overflow {
        let class = content_class(&hit.content);
        if !classes.contains(&class) {
            classes.push(class);
        }
    }
    let mut listed = classes
        .iter()
        .take(MAX_SUMMARY_CLASSES)
        .copied()
        .collect::<Vec<_>>()
        .join("; ");
    if classes.len() > MAX_SUMMARY_CLASSES {
        listed.push_str("; …");
    }
    let content = format!(
        "{} generic tells elided (per-program cap {cap}): {sites} sites across {} functions — {listed}",
        overflow.len(),
        functions.len()
    );
    let reasoning = format!(
        "The per-program cap keeps the {cap} strongest generic tells as individual findings; \
         these {} grouped tells were folded into this summary instead of seeding one finding \
         each. Generic tells (bare PEB reads, 0xCC compares, lone timing/syscall opcodes) \
         repeat near-identically across a binary, so past the strongest few the count carries \
         the signal. Nothing was dropped: every elided tell is counted here.",
        overflow.len()
    );
    // file-level and weak: the summary asserts a count, not any single claim
    let mut summary =
        DetectionHit::new(first.technique.clone(), 0, None, low(), content, reasoning);
    // keep the producer stamp so the seeder attributes the line to its detector
    summary.detection.clone_from(&first.detection);
    Some(summary)
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

/// How a string catalog's needles are matched against a defined string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// Bare case-insensitive substring containment — the historical default
    /// every existing catalog uses.
    #[default]
    Substring,
    /// Substring containment, but only where the match is not flanked by an
    /// ASCII letter on either side. Kills tool names sliced out of longer
    /// letter runs (`"idaq"` inside an RSA key's base64) while keeping
    /// delimited uses (`"wireshark"` inside `"WIRESHARK.EXE"` — `.` is not a
    /// letter).
    LetterBounded,
}

/// A contextual shape a needle demands around its match site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchShape {
    /// The needle (`".onion"`) must be immediately preceded by a 16- or
    /// 56-character base32 (`[a-z2-7]`) label — the v2/v3 onion-address
    /// forms. Keeps library guard strings like `"/path.onion-guard"` from
    /// matching while real hidden-service addresses still do.
    OnionAddress,
}

/// Per-catalog options controlling how string needles match.
///
/// The default (`Substring`, no vetoes, no shape) reproduces the historical
/// bare-containment behavior, so [`string_artifact_hits`] callers are
/// unaffected until a catalog opts in via [`string_artifact_hits_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct MatchOptions {
    /// The boundary mode needles are matched under.
    pub mode: MatchMode,
    /// Reject a match sitting inside a contiguous base64-charset run of
    /// [`BASE64_RUN_MIN`] or more characters — key material and encoded blobs
    /// coincidentally spell short tool names.
    pub base64_run_veto: bool,
    /// A shape every needle in the call must find around its match, for
    /// catalogs whose needles are structured values rather than names.
    pub shape: Option<MatchShape>,
}

/// The shortest contiguous base64-charset run treated as encoded data.
///
/// Below this a run is an ordinary word; at 24+ characters (18 encoded bytes)
/// it is the inside of a key/blob, where a tool-name coincidence means nothing.
pub const BASE64_RUN_MIN: usize = 24;

/// Whether `needle` matches anywhere in `hay` under the given options.
///
/// The pure matcher behind [`string_artifact_hits_with`]: every occurrence of
/// the needle is tested against the boundary mode, the base64-run veto, and
/// the shape requirement, and one passing occurrence is a match. Both inputs
/// are expected lower-cased (the caller lower-cases the haystack once).
///
/// # Arguments
///
/// * `hay` - The lower-cased string value to search
/// * `needle` - The lower-cased needle
/// * `options` - The matching options in force for this catalog
pub fn needle_matches(hay: &str, needle: &str, options: MatchOptions) -> bool {
    // an empty needle matches nothing rather than everything
    if needle.is_empty() {
        return false;
    }
    // test every occurrence; one passing occurrence is a match
    hay.match_indices(needle)
        .any(|(at, matched)| occurrence_passes(hay, at, matched.len(), options))
}

/// Whether one occurrence of a needle passes the options' context checks.
///
/// # Arguments
///
/// * `hay` - The full string the occurrence was found in
/// * `at` - The byte offset the occurrence starts at
/// * `len` - The occurrence's byte length
/// * `options` - The matching options in force
fn occurrence_passes(hay: &str, at: usize, len: usize, options: MatchOptions) -> bool {
    let bytes = hay.as_bytes();
    let end = at + len;
    // letter-bounded: a match flanked by a letter is a slice of a longer run
    if options.mode == MatchMode::LetterBounded {
        if at > 0 && bytes[at - 1].is_ascii_alphabetic() {
            return false;
        }
        if end < bytes.len() && bytes[end].is_ascii_alphabetic() {
            return false;
        }
    }
    // the base64-run veto: inside encoded data a name coincidence is noise
    if options.base64_run_veto && in_base64_run(bytes, at, end) {
        return false;
    }
    // the shape requirement, when the catalog's needles demand one
    match options.shape {
        Some(MatchShape::OnionAddress) => onion_label_precedes(bytes, at),
        None => true,
    }
}

/// Whether a byte belongs to the base64 character set (`[A-Za-z0-9+/=]`).
///
/// # Arguments
///
/// * `b` - The byte to test
fn is_base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')
}

/// Whether a match sits inside a base64-charset run of veto length.
///
/// # Arguments
///
/// * `bytes` - The haystack bytes
/// * `at` - The match's start offset
/// * `end` - The match's (exclusive) end offset
fn in_base64_run(bytes: &[u8], at: usize, end: usize) -> bool {
    // a match containing a non-base64 byte cannot sit inside a base64 run
    if !bytes[at..end].iter().copied().all(is_base64_byte) {
        return false;
    }
    // expand to the maximal base64-charset run containing the match
    let mut start = at;
    while start > 0 && is_base64_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut stop = end;
    while stop < bytes.len() && is_base64_byte(bytes[stop]) {
        stop += 1;
    }
    // only a run long enough to be encoded data (not a word) vetoes
    stop - start >= BASE64_RUN_MIN
}

/// Whether a 16- or 56-character base32 label immediately precedes `at`.
///
/// The two label lengths are the v2 and v3 onion-address forms; a real
/// address is delimited on its left (a scheme separator, quote, or space ends
/// the maximal run exactly at the label), while a path or identifier that
/// merely contains `".onion"` carries the wrong-length run.
///
/// # Arguments
///
/// * `bytes` - The haystack bytes
/// * `at` - The match's start offset (where `".onion"` begins)
fn onion_label_precedes(bytes: &[u8], at: usize) -> bool {
    // the base32 alphabet onion labels are drawn from
    let is_base32 = |b: u8| matches!(b, b'a'..=b'z' | b'2'..=b'7');
    // measure the maximal base32 run ending right where the match starts
    let mut start = at;
    while start > 0 && is_base32(bytes[start - 1]) {
        start -= 1;
    }
    // only the exact v2/v3 label lengths name a hidden service
    matches!(at - start, 16 | 56)
}

/// Emit hits for defined strings containing any of the given (lower-case) needles.
///
/// The historical bare-substring entry point: delegates to
/// [`string_artifact_hits_with`] with [`MatchOptions::default`], so every
/// existing catalog keeps its behavior while catalogs that need boundary or
/// shape matching opt in through the `_with` variant.
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
pub async fn string_artifact_hits(
    ctx: &AnalysisContext,
    technique: AttackTechnique,
    needles: &[&str],
    severity: (Confidence, i64),
    reasoning: &str,
    excludes: &[&str],
) -> anyhow::Result<Vec<DetectionHit>> {
    // delegate with the bare-substring default every existing catalog uses
    string_artifact_hits_with(
        ctx,
        technique,
        needles,
        severity,
        reasoning,
        excludes,
        MatchOptions::default(),
    )
    .await
}

/// Emit hits for defined strings matching the given needles under explicit
/// matching options.
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
/// Needles are matched per `options` (see [`needle_matches`]); exclude terms
/// stay bare substrings, because a veto should fire as broadly as possible.
///
/// # Arguments
///
/// * `ctx` - The analysis context
/// * `technique` - The ATT&CK technique these artifacts evidence
/// * `needles` - Lower-case needles to match against string values
/// * `severity` - The `(confidence, suspicion)` severity for a referenced string
/// * `reasoning` - The analyst-facing explanation
/// * `excludes` - Lower-case terms that veto an otherwise-matching string
/// * `options` - The matching options in force for this catalog
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
#[instrument(name = "detections::string_artifact_hits", skip_all, err(Debug))]
pub async fn string_artifact_hits_with(
    ctx: &AnalysisContext,
    technique: AttackTechnique,
    needles: &[&str],
    severity: (Confidence, i64),
    reasoning: &str,
    excludes: &[&str],
    options: MatchOptions,
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
        if !needles.iter().any(|n| needle_matches(&hay, n, options)) {
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
    /// Whether matches of this constant are inherent noise: hits built from this
    /// entry carry [`DetectionHit::always_noise`], which the seeder treats as
    /// rollup-eligible noise regardless of toolchain.
    pub always_noise: bool,
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

/// Return the entry of a function that references an address inside a matched
/// signature's bytes, when the reference index holds one.
///
/// Unlike the proximity fallback in [`referencing_function`], this attributes
/// only on a reference landing within `[addr, addr + len)` — a function provably
/// reading the matched constant itself, not merely data stored near it.
///
/// # Arguments
///
/// * `index` - The sorted reference index (data address → referencing function)
/// * `addr` - The match's start address
/// * `len` - The matched signature's byte length
fn referenced_in_range(index: &[(u64, u64)], addr: u64, len: usize) -> Option<u64> {
    // the first index entry at or past the match start (the index is sorted)
    let pos = index.partition_point(|(data_addr, _)| *data_addr < addr);
    // attribute only when that entry still lands inside the matched bytes
    index
        .get(pos)
        .filter(|(data_addr, _)| *data_addr < addr.saturating_add(len as u64))
        .map(|(_, entry)| *entry)
}

/// Emit hits for any of the given byte-signatures found in initialized sections.
///
/// Scans every initialized memory block (via
/// [`AnalysisContext::initialized_bytes`]) for each signature's byte sequence. Each
/// match is attributed to a function so the flag links there rather than dangling on
/// the sample: its containing function when the constant sits inside a code section,
/// else — for data-section matches — the function the cached data-reference index
/// proves references an address inside the matched bytes themselves. A data match
/// nothing references stays sample-scoped; when some function's referenced data
/// merely sits nearby, that neighbourhood is named in the reasoning as unverified
/// rather than used for attribution, because proximity routinely crosses from one
/// object's data into the next.
///
/// Matches that share a signature, a section, and an attributed function (or, for
/// sample-scoped matches, the same unverified neighbour) are then collapsed into
/// **one** hit naming every address, because a table stored twice back-to-back is
/// one implementation seen twice, not two findings.
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
        let mut grouped: BTreeMap<(usize, Option<u64>, Option<u64>), Vec<u64>> = BTreeMap::new();
        for (idx, sig) in sigs.iter().enumerate() {
            // record every non-overlapping occurrence of the signature
            for offset in find_all(bytes, sig.bytes) {
                // map the in-section offset back to an absolute address
                let addr = base.saturating_add(offset as u64);
                // attribute the match: its containing function (code section), else the
                // function proven to reference the matched bytes, else None (sample-scoped)
                let function = ctx
                    .containing_function(addr)
                    .or_else(|| referenced_in_range(&index, addr, sig.bytes.len()));
                // for a sample-scoped match, keep the nearest referenced buffer's
                // function aside as an unverified neighbourhood for the reasoning
                let near = match function {
                    Some(_) => None,
                    None => referencing_function(&index, addr, DATA_REF_WINDOW),
                };
                grouped.entry((idx, function, near)).or_default().push(addr);
            }
        }
        // one hit per (signature, attributed function, neighbour) group in this section
        for ((idx, function, near), addrs) in grouped {
            hits.push(build_signature_hit(
                technique.clone(),
                kind,
                &sigs[idx],
                name,
                function,
                near,
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
/// * `near` - For a sample-scoped match, the function whose referenced data sits
///   nearest; named in the reasoning as unverified, never used for attribution
/// * `addrs` - Every address the signature matched at, in this section
/// * `severity` - The `(confidence, suspicion)` severity for a match
// eight parameters are over the lint's bar, and the alternative is a struct that
// exists only to be destructured at the single call site
#[allow(clippy::too_many_arguments)]
fn build_signature_hit(
    technique: AttackTechnique,
    kind: EvidenceKind,
    sig: &ByteSig,
    section: &str,
    function: Option<u64>,
    near: Option<u64>,
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
    // name the unverified neighbourhood when a sample-scoped match merely sits
    // near some function's referenced data (proximity is not attribution)
    let near_note = match near {
        Some(entry) => {
            format!(" — no code reference found; located near FUN_{entry:x}'s data (unverified)")
        }
        None => String::new(),
    };
    // assemble the hit, attaching every match address for the validator
    let mut hit = DetectionHit::new(
        technique,
        primary,
        function,
        severity,
        format!(
            "{} constant @ {} (section {section})",
            sig.name,
            render_sites(&addrs)
        ),
        format!("{}{repeat_note}{near_note}", sig.reasoning),
    )
    .kind(kind)
    .sites(addrs);
    // carry the catalog's noise assertion so the seeder can roll the hit up
    if sig.always_noise {
        hit = hit.always_noise();
    }
    hit
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry's detector names are stable, unique, and nonempty
    #[test]
    fn registry_names_are_unique() {
        let detections = all();
        assert!(!detections.is_empty());
        let mut names: Vec<&'static str> = detections.iter().map(|d| d.name()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "detector names must be unique");
        assert!(names.iter().all(|name| !name.is_empty()));
    }

    /// Stamping names a hit's producer without overwriting an explicit one
    #[test]
    fn stamping_names_producers_without_overwriting() {
        // one unstamped hit and one a detection named itself (a script hit)
        let mut hits = vec![
            DetectionHit::new(
                AttackTechnique {
                    id: "T1622".into(),
                    name: "Debugger Evasion".into(),
                    tactic: AttackTactic::DefenseEvasion,
                },
                0x1000,
                None,
                (Confidence::Likely, 40),
                "test",
                "test",
            ),
            {
                let mut named = DetectionHit::new(
                    AttackTechnique {
                        id: "T1622".into(),
                        name: "Debugger Evasion".into(),
                        tactic: AttackTactic::DefenseEvasion,
                    },
                    0x2000,
                    None,
                    (Confidence::Likely, 40),
                    "test",
                    "test",
                );
                named.detection = Some("my_script".to_owned());
                named
            },
        ];
        stamp_detection("debugger_evasion", &mut hits);
        assert_eq!(hits[0].detection.as_deref(), Some("debugger_evasion"));
        // an explicit name a producer set is never overwritten
        assert_eq!(hits[1].detection.as_deref(), Some("my_script"));
    }

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
            // unstamped here; the registry runner names native producers
            detection: None,
            // not an API-catalog hit: no catalog name or withheld severity
            api: None,
            catalog_severity: None,
            always_noise: false,
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
        // a validated pivot artifact vetoes classification wherever it sits
        hit.artifact = Some(HitArtifact {
            class: ArtifactClass::ConfigRecord,
            value: "host=10.20.30.40; retry interval 3600 seconds".to_owned(),
            role: None,
            detail: None,
        });
        assert!(
            runtime_classification(Some(Toolchain::Rust), Some("core::fmt::write"), &hit).is_none()
        );
        // junk "recovered" text earns no veto: the hit is judged like any other
        hit.artifact = Some(HitArtifact {
            class: ArtifactClass::ConfigRecord,
            value: "~~|~||z~||z|zzx".to_owned(),
            role: None,
            detail: None,
        });
        assert!(
            runtime_classification(Some(Toolchain::Rust), Some("core::fmt::write"), &hit).is_some()
        );
        // an existing context is never overwritten
        hit.artifact = None;
        hit.context = Some(CallContext::Runtime("preexisting".to_owned()));
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
            always_noise: false,
            bytes: b"expand 32-byte k",
            reasoning: "the ChaCha20 sigma constant.",
        };
        let hit = build_signature_hit(
            TECH,
            EvidenceKind::ContextOnly,
            &sig,
            ".rdata",
            Some(0x401000),
            None,
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
            always_noise: false,
            bytes: b"\x63\x7c\x77\x7b",
            reasoning: "the AES S-box.",
        };
        let hit = build_signature_hit(
            TECH,
            EvidenceKind::ContextOnly,
            &sig,
            ".rodata",
            None,
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

    /// A data match is attributed only through a reference into its own bytes.
    #[test]
    fn referenced_in_range_requires_a_reference_into_the_match() {
        // one function references the match start; another references data before it
        let index = vec![(0x1b_4260, 0x40_2000), (0x1b_42a0, 0x40_1000)];
        // a reference at the match start attributes to its function
        assert_eq!(referenced_in_range(&index, 0x1b_42a0, 256), Some(0x40_1000));
        // a reference into the middle of the matched bytes attributes too
        assert_eq!(referenced_in_range(&index, 0x1b_4290, 256), Some(0x40_1000));
        // a reference merely preceding the match (the old proximity case) does not
        assert_eq!(referenced_in_range(&index, 0x1b_42b0, 16), None);
        // a reference at or past the match end does not (the range end is exclusive)
        assert_eq!(referenced_in_range(&index, 0x1b_4280, 0x20), None);
    }

    /// An unreferenced data match seeds file-level, naming its neighbour as unverified.
    #[test]
    fn signature_hit_names_an_unverified_neighbour_for_file_level_matches() {
        let sig = ByteSig {
            name: "AES forward S-box",
            always_noise: false,
            bytes: b"\x63\x7c\x77\x7b",
            reasoning: "the AES S-box.",
        };
        let hit = build_signature_hit(
            TECH,
            EvidenceKind::ContextOnly,
            &sig,
            ".rodata",
            None,
            Some(0x40_1b60),
            vec![0x1b_42a0],
            medium(),
        );
        // the match stays sample-scoped: proximity alone earns no attribution
        assert_eq!(hit.function, None);
        // the nearest function's data is named as an unverified neighbourhood only
        assert!(
            hit.reasoning
                .contains("no code reference found; located near FUN_401b60's data (unverified)"),
            "{}",
            hit.reasoning
        );
    }

    /// A noise-asserting signature stamps its hits as always-noise.
    #[test]
    fn signature_hit_carries_the_noise_assertion() {
        let sig = ByteSig {
            name: "base64 standard alphabet",
            always_noise: true,
            bytes: b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
            reasoning: "the standard base64 alphabet.",
        };
        let hit = build_signature_hit(
            TECH,
            EvidenceKind::ContextOnly,
            &sig,
            ".rodata",
            Some(0x40_1000),
            None,
            vec![0x1b_42a0],
            medium(),
        );
        // the seeder rolls these hits up as noise regardless of toolchain
        assert!(hit.always_noise);
    }

    /// A noise-asserting API spec stamps its hits as always-noise.
    #[test]
    fn api_hit_carries_the_noise_assertion() {
        let spec = ApiSpec {
            key: "CryptAcquireContext",
            always_noise: true,
            severity: low(),
            reasoning: "opens a CryptoAPI provider handle.",
        };
        let hit = build_api_hit(
            TECH,
            EvidenceKind::ContextOnly,
            &spec,
            "CryptAcquireContext",
            Some(0x40_1000),
            &[0x40_1010],
            None,
        );
        // the seeder rolls these hits up as noise regardless of toolchain
        assert!(hit.always_noise);
    }

    /// Several direct calls from one function are one hit naming every call.
    #[test]
    fn api_hit_groups_direct_calls() {
        let spec = ApiSpec {
            key: "OutputDebugString",
            always_noise: false,
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
        // the validator identifies the API by the stamped catalog key
        assert_eq!(hit.api, Some("OutputDebugString"));
    }

    /// A catalog high() seeds clamped to medium, with the catalog pair kept aside.
    #[test]
    fn api_hit_clamps_presence_only_severity() {
        let spec = ApiSpec {
            key: "NtQueryInformationProcess",
            always_noise: false,
            severity: high(),
            reasoning: "queries debug-class process information.",
        };
        let hit = build_api_hit(
            TECH,
            EvidenceKind::ApiResultCheck,
            &spec,
            "NtQueryInformationProcess",
            Some(0x401000),
            &[0x401010],
            None,
        );
        // presence alone seeds at medium however strong the catalog's prior
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, 40);
        // the catalog severity rides along for the validation pass to restore
        let (confidence, suspicion) = hit.catalog_severity.expect("the clamp keeps the catalog");
        assert!(matches!(confidence, Confidence::Likely));
        assert_eq!(suspicion, 75);
    }

    /// A catalog severity at or below medium passes through unclamped.
    #[test]
    fn api_hit_leaves_sub_medium_severity_alone() {
        let spec = ApiSpec {
            key: "OutputDebugString",
            always_noise: false,
            severity: low(),
            reasoning: "writes to an attached debugger.",
        };
        let hit = build_api_hit(
            TECH,
            EvidenceKind::ContextOnly,
            &spec,
            "OutputDebugString",
            Some(0x401000),
            &[0x401010],
            None,
        );
        // nothing to clamp: the hit keeps the catalog's own low grade
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 15);
        // and no withheld severity exists for the validator to restore
        assert!(hit.catalog_severity.is_none());
    }

    /// Unreferenced imports fold into one file-level census hit naming them.
    #[test]
    fn import_census_collapses_unreferenced_imports() {
        let hit = import_census_hit(TECH, &["IsDebuggerPresent", "NtQueryInformationProcess"])
            .expect("unreferenced imports produce a census hit");
        assert_eq!(
            hit.content,
            "2 catalog APIs imported but never referenced from code: IsDebuggerPresent, \
             NtQueryInformationProcess"
        );
        // the census is file-level and weak: no function, address 0, low grade
        assert_eq!(hit.address, 0);
        assert_eq!(hit.function, None);
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert!(matches!(hit.kind, EvidenceKind::ContextOnly));
        // the wording keeps the decoy-table signal without asserting behaviors
        assert!(hit.reasoning.contains("decoy"), "{}", hit.reasoning);
        assert!(hit.reasoning.contains("unverified"), "{}", hit.reasoning);
    }

    /// The per-import census mode is constructible and distinct from Combined.
    #[test]
    fn per_import_census_mode_is_available() {
        // the opt-out variant exists for detectors whose uncalled imports are a
        // meaningful prior on their own; assert it stays a distinct mode
        assert!(!matches!(ImportCensus::PerImport, ImportCensus::Combined));
    }

    /// The census hit lists a bounded number of names and elides the rest.
    #[test]
    fn import_census_caps_the_listed_names() {
        // ten names, with one duplicate pair that must collapse to one entry
        let names = [
            "A1", "A2", "A3", "A4", "A5", "A6", "A7", "A8", "A9", "A10", "A1",
        ];
        let hit = import_census_hit(TECH, &names).expect("a census hit");
        // the duplicate is dropped from the count and the list caps at eight
        assert!(
            hit.content
                .starts_with("10 catalog APIs imported but never referenced from code: A1, A2,"),
            "{}",
            hit.content
        );
        assert!(hit.content.ends_with(", …"), "{}", hit.content);
        assert!(!hit.content.contains("A9"), "{}", hit.content);
    }

    /// With every matched import referenced there is no census to report.
    #[test]
    fn import_census_reports_nothing_without_misses() {
        assert!(import_census_hit(TECH, &[]).is_none());
    }

    /// A lone direct call keeps the singular wording and no site list.
    #[test]
    fn api_hit_keeps_a_lone_direct_call_singular() {
        let spec = ApiSpec {
            key: "IsDebuggerPresent",
            always_noise: false,
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

    /// Build a single-site scan hit for the grouping tests.
    ///
    /// # Arguments
    ///
    /// * `class` - The content class (the text before the site rendering)
    /// * `address` - The evidence site address
    /// * `function` - The containing function entry, if any
    fn scan_hit(class: &str, address: u64, function: Option<u64>) -> DetectionHit {
        DetectionHit::new(
            TECH,
            address,
            function,
            medium(),
            format!("{class} @ {}", fmt_addr(address)),
            "scan reasoning",
        )
    }

    /// Ten same-class tells in one function merge into one hit naming every site.
    #[test]
    fn group_scan_hits_merges_per_function() {
        // the per-instruction flood shape: one tell repeated through a function
        let hits: Vec<DetectionHit> = (0..10u64)
            .map(|i| {
                scan_hit(
                    "PEB BeingDebugged read",
                    0x401000 + i * 0x10,
                    Some(0x401000),
                )
            })
            .collect();
        let grouped = group_scan_hits(hits, 8, |_| false);
        // one behavior repeated is one finding carrying every site
        assert_eq!(grouped.len(), 1);
        let hit = &grouped[0];
        assert_eq!(hit.sites.len(), 10);
        // the first (lowest) site is primary and the count rides in the content
        assert_eq!(hit.address, 0x401000);
        assert!(
            hit.content
                .starts_with("PEB BeingDebugged read — 10 sites @ 0x401000"),
            "{}",
            hit.content
        );
        assert_eq!(hit.function, Some(0x401000));
    }

    /// Generic tells past the per-program cap collapse into one summary line.
    #[test]
    fn group_scan_hits_caps_generic_tells() {
        // twelve single-site generic tells, each in its own function
        let hits: Vec<DetectionHit> = (0..12u64)
            .map(|i| {
                scan_hit(
                    "bare RDTSC read",
                    0x401000 + i * 0x100,
                    Some(0x401000 + i * 0x100),
                )
            })
            .collect();
        let grouped = group_scan_hits(hits, 8, |_| true);
        // eight kept individually plus the one file-level summary
        assert_eq!(grouped.len(), 9);
        let summary = grouped.last().expect("a summary hit exists");
        assert_eq!(summary.address, 0);
        assert_eq!(summary.function, None);
        assert!(matches!(summary.confidence, Confidence::Untrusted));
        // the summary counts what was elided and names the class
        assert!(
            summary.content.starts_with(
                "4 generic tells elided (per-program cap 8): 4 sites across 4 functions"
            ),
            "{}",
            summary.content
        );
        assert!(
            summary.content.contains("bare RDTSC read"),
            "{}",
            summary.content
        );
    }

    /// Classified (non-generic) hits are exempt from the per-program cap.
    #[test]
    fn group_scan_hits_exempts_classified_hits() {
        // twelve classified tells across twelve functions, over an 8 cap
        let hits: Vec<DetectionHit> = (0..12u64)
            .map(|i| {
                scan_hit(
                    "Hell's Gate syscall stub",
                    0x401000 + i * 0x100,
                    Some(0x401000 + i * 0x100),
                )
            })
            .collect();
        let grouped = group_scan_hits(hits, 8, |_| false);
        // every classified tell still seeds individually, with no summary
        assert_eq!(grouped.len(), 12);
        assert!(grouped.iter().all(|hit| !hit.content.contains("elided")));
    }

    /// The content class is the text before the first site rendering.
    #[test]
    fn content_class_stops_at_the_site_rendering() {
        // the lone-site and grouped-count shapes, plus a separator-free content
        assert_eq!(
            content_class("RDTSC timing read @ 0x401000"),
            "RDTSC timing read"
        );
        assert_eq!(content_class("Sleep — 3 calls @ 0x1000, 0x2000"), "Sleep");
        assert_eq!(content_class("no separators here"), "no separators here");
    }

    /// The extended marker families match at their anchors and nowhere else.
    #[test]
    fn library_markers_match_new_families_conservatively() {
        // glibc resolver / loader / stdio / NSS / libc families (case-insensitive)
        assert!(is_library_function("__res_vinit"));
        assert!(is_library_function("_dl_discover_osversion"));
        assert!(is_library_function("_IO_file_xsputn"));
        assert!(is_library_function("__nss_database_lookup"));
        assert!(is_library_function("__libc_start_main"));
        // mingw / libstdc++ / openssl / musl markers
        assert!(is_library_function("__mingw_CRTStartup"));
        assert!(is_library_function("__cxa_throw"));
        assert!(is_library_function("std::basic_string<char>::append"));
        assert!(is_library_function("OPENSSL_cleanse"));
        assert!(is_library_function("__stdio_write"));
        // prefix anchoring: user names embedding similar fragments stay user code
        assert!(!is_library_function("my_dll_main"));
        assert!(!is_library_function("resolver"));
        assert!(!is_library_function("my_io_handler"));
        assert!(!is_library_function("handle_dl_open"));
    }

    /// The structural recognizers fire on 3+ cluster members, not on near-misses.
    #[test]
    fn structural_recognizers_need_three_members() {
        // the fail-fast cluster at four of six members
        let failfast = [
            "IsDebuggerPresent",
            "RtlCaptureContext",
            "SetUnhandledExceptionFilter",
            "TerminateProcess",
        ];
        let label = structural_runtime_label(failfast.iter().copied())
            .expect("four fail-fast members recognize");
        assert!(label.contains("fail-fast"), "{label}");
        // the /GS cookie cluster at three of five, via the 64-bit tick alias
        let cookie = [
            "GetSystemTimeAsFileTime",
            "QueryPerformanceCounter",
            "GetTickCount64",
        ];
        let label = structural_runtime_label(cookie.iter().copied())
            .expect("three cookie-entropy members recognize");
        assert!(label.contains("security-cookie"), "{label}");
        // a 2-of-6 near miss (a sample's own SEH anti-debug pair) stays unrecognized
        let near = ["IsDebuggerPresent", "SetUnhandledExceptionFilter"];
        assert!(structural_runtime_label(near.iter().copied()).is_none());
        // duplicate members count once (the tick counter and its 64-bit alias)
        let dupes = ["GetTickCount", "GetTickCount64", "QueryPerformanceCounter"];
        assert!(structural_runtime_label(dupes.iter().copied()).is_none());
    }

    /// A stripped cookie-init function's hits classify from the catalog-call
    /// map, and a validated artifact still shields its hit.
    #[test]
    fn structural_classification_reads_the_call_map() {
        // three cookie-entropy catalog hits sitting in one stripped function
        let mk = |api: &'static str| {
            let mut hit = scan_hit(api, 0x401010, Some(0x401000));
            hit.api = Some(api);
            hit
        };
        let hits = vec![
            mk("GetSystemTimeAsFileTime"),
            mk("QueryPerformanceCounter"),
            mk("GetTickCount"),
        ];
        let calls = function_call_map(&hits);
        // the cluster classifies the function's hits as runtime support
        let classified = structural_classification(&calls, &hits[0]);
        assert!(
            matches!(&classified, Some(CallContext::Runtime(label)) if label.contains("security-cookie")),
            "{classified:?}"
        );
        // the same hit carrying a validated endpoint artifact keeps its voice
        let mut shielded = hits[1].clone();
        shielded.artifact = Some(HitArtifact::new(
            ArtifactClass::NetworkEndpoint,
            "https://blog.yahoo.com/beacon",
        ));
        assert!(structural_classification(&calls, &shielded).is_none());
    }

    /// Letter-bounded matching plus the base64 veto kills in-blob tool names
    /// and keeps real filenames.
    #[test]
    fn needle_matching_letter_bounded_and_base64_veto() {
        let opts = MatchOptions {
            mode: MatchMode::LetterBounded,
            base64_run_veto: true,
            shape: None,
        };
        // the RSA public-exponent base64 run every PEM key carries ("…IDAQAB…")
        let blob = "miieowibaakcaqeaidaqabmiibcgkcaqea";
        assert!(!needle_matches(blob, "idaq", opts));
        // the tool's real filename still matches ('.' is not a letter)
        assert!(needle_matches("c:\\tools\\idaq.exe", "idaq", opts));
        // a bounded name inside a short run still matches ("wireshark.exe")
        assert!(needle_matches("wireshark.exe detected", "wireshark", opts));
        // the historical default keeps matching bare substrings
        assert!(needle_matches(blob, "idaq", MatchOptions::default()));
    }

    /// The onion shape demands a 16/56-character base32 label before the TLD.
    #[test]
    fn needle_matching_onion_shape() {
        let opts = MatchOptions {
            mode: MatchMode::Substring,
            base64_run_veto: false,
            shape: Some(MatchShape::OnionAddress),
        };
        // the c-ares-style guard substring is not an address
        assert!(!needle_matches("/path.onion-guard", ".onion", opts));
        // a real v3 (56-character) onion label passes
        let label56 = format!("{}b", "a".repeat(55));
        let v3 = format!("http://{label56}.onion/gate");
        assert!(needle_matches(&v3, ".onion", opts));
        // and the old v2 (16-character) form
        assert!(needle_matches("expyuzz4wqqyqhjn.onion", ".onion", opts));
        // a label of the wrong length fails
        assert!(!needle_matches("short3.onion", ".onion", opts));
    }
}
