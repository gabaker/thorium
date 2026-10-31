//! Runs the static detectors and seeds the forum with points of interest.
//!
//! Before the agent loop starts, this runs the ported detection engine
//! ([`crate::thorpal::detect`]) over the target binary and turns each finding
//! into a forum post/comment. Findings are grouped by the function they land in
//! (file-level findings split per producing detection), so the agents open with
//! concrete points of interest — a function, the evidence found there, and why
//! it matters — instead of discovering them from scratch. Each detection seeds
//! at most a budgeted number of finding-shape comments; the overflow stays
//! itemized and re-raisable in a per-detection census post.
//!
//! On top of the per-function statement posts, the strongest findings also derive
//! a small set of targeted open questions (is it packed? what does this host
//! check gate?), so the main agent's question loop opens with focused work to
//! delegate instead of exploring the binary cold.

use std::collections::BTreeMap;
use std::path::Path;

use ghidra_cli::Ghidra;

use crate::thorpal::detect::artifact_gates::artifact_is_validated;
use crate::thorpal::detect::confidence::Confidence;
use crate::thorpal::detect::context::{AnalysisContext, fmt_addr};
use crate::thorpal::detect::detections::{
    self, ArtifactClass, DetectionHit, DetectorRun, EvidenceKind, confidence_rank,
};
use crate::thorpal::detect::emulation;
use crate::thorpal::detect::provenance_types::CallContext;
use crate::thorpal::detect::script_detections::{self, ScriptDetector};
use crate::thorpal::detect::toolchain::Toolchain;
use crate::thorpal::detect::yara::corroborated_family;
use crate::thorpal::tools::forum::{
    FindingProvenance, ForumState, Kind, ParentRef, QuestionStatus, Stance,
};
use crate::thorpal::tools::scripts::ScriptHost;

/// The author every seeded post and comment is attributed to.
///
/// Shared with the forum's report appendix renderer (see
/// [`crate::thorpal::tools::forum::ForumState::report_appendix`]), which elides
/// detector-authored bodies while keeping agent findings verbatim, and
/// exported (as `DETECTOR_AUTHOR`) so offline telemetry can identify
/// detector-seeded forum records by the same reserved name.
pub const SEED_AUTHOR: &str = "detector";

/// The greatest number of backing sites listed in a comment before truncating.
const MAX_SITES_LISTED: usize = 8;

/// The greatest number of derived questions seeded after the statement posts.
///
/// Every open question forces the main agent to delegate and resolve it, so an
/// unbounded queue would spend the very tokens the up-front static work is meant
/// to save. The statement posts remain the exhaustive record; questions are the
/// focused cut.
const MAX_SEED_QUESTIONS: usize = 6;

/// The greatest number of completion-gate tasks seeded under one question.
///
/// Every seeded task is a hard gate on marking its question solved — the
/// delegated agent must satisfy and complete each one — so this cap keeps the
/// forced per-question work (and the run's cost) cheap.
pub(crate) const MAX_SEED_TASKS_PER_QUESTION: usize = 4;

/// The greatest number of host-check gate questions derived per run.
const MAX_GATE_QUESTIONS: usize = 2;

/// The greatest number of recovered endpoint values quoted in the C2 question.
const MAX_QUESTION_ENDPOINTS: usize = 5;

/// The greatest number of statement posts cited as evidence in one question.
const MAX_QUESTION_CITES: usize = 5;

/// The hard budget for the whole YARA compile-plus-scan task at seed time.
const YARA_SEED_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// The per-sample namespace of a multi-sample session's seeding pass.
///
/// The seeding path uses deterministic post names (`file-identity`,
/// `file-level` and its `file-level-<detection>` splits, `runtime-noise`,
/// `census-<detection>`, the `q-*` questions), and `seed_post` refuses
/// duplicate names — so a second sample's pass must prefix every name it
/// seeds. The prefix is order-derived (`s2`, `s3`, …) and the program name
/// suffixes titles so threads read unambiguously. The main sample seeds with
/// no scope and keeps its historical names byte-identical.
#[derive(Debug, Clone)]
pub struct SampleScope {
    /// The short post-name prefix (e.g. `s2`)
    pub prefix: String,
    /// The Ghidra program name the scoped posts describe
    pub program: String,
}

/// Applies a sample scope's prefix to a seeded post name.
///
/// # Arguments
///
/// * `scope` - The sample scope, or `None` for the main sample's pass
/// * `base` - The unscoped post name
fn scoped_name(scope: Option<&SampleScope>, base: &str) -> String {
    // the main pass keeps its historical names byte-identical
    match scope {
        Some(scope) => format!("{}-{base}", scope.prefix),
        None => base.to_owned(),
    }
}

/// Applies a sample scope's program suffix to a seeded post title.
///
/// # Arguments
///
/// * `scope` - The sample scope, or `None` for the main sample's pass
/// * `base` - The unscoped post title
fn scoped_title(scope: Option<&SampleScope>, base: String) -> String {
    // the main pass keeps its historical titles byte-identical
    match scope {
        Some(scope) => format!("{base} — {}", scope.program),
        None => base,
    }
}

/// Run the static detectors over `binary` and seed the forum with their findings.
///
/// Builds an [`AnalysisContext`] over the shared Ghidra connection, runs every
/// detection ([`detections::run_all`], which never fails per-detector) plus any
/// detector-flagged library scripts (see
/// [`script_detections::run_script_detectors`], equally failure-isolated),
/// validates the evidence, budgets each detection's admitted findings (see
/// [`apply_admission_budget`]; the overflow seeds per-detection census posts),
/// groups the kept hits into buckets (per function, with file-level findings
/// split per producing detection), and creates one post per bucket with one
/// comment per finding. When `seed_questions` is set, the script-emitted
/// questions plus the strongest findings' derived questions (see
/// [`derive_questions`]) are seeded as a bounded set of targeted open
/// questions.
///
/// # Arguments
///
/// * `ghidra` - The shared, already-connected Ghidra handle (pinned to the
///   sample's program when seeding a multi-sample session's sibling)
/// * `forum` - The instance's forum state to seed the findings onto
/// * `binary` - The path to the target binary being analyzed
/// * `seed_questions` - Whether to also seed derived questions after the posts
/// * `script_detectors` - The detector-flagged library scripts to run too
/// * `host` - The host the scripts' bindings call into, when any scripts run
/// * `yara_rules` - The operator's YARA rules to scan the sample with, if any
/// * `scope` - The sample's post-name namespace in a multi-sample session, or
///   `None` for the main sample's (historically named) pass
///
/// # Errors
///
/// Returns an error only when the analysis context cannot be built or the
/// detection driver hard-fails; seeding is best-effort and the caller treats a
/// failure as non-fatal.
#[tracing::instrument(skip_all, err(Debug))]
pub async fn seed_forum_from_detectors(
    ghidra: &Ghidra,
    forum: &ForumState,
    binary: &Path,
    seed_questions: bool,
    script_detectors: Vec<ScriptDetector>,
    host: Option<ScriptHost>,
    yara_rules: Option<&Path>,
    scope: Option<&SampleScope>,
) -> anyhow::Result<SeedReport> {
    // build the analysis context over a cheap clone of the shared connection so the
    // agent's later tool calls keep reusing the same analyzed database
    let ctx = AnalysisContext::build(ghidra.clone(), binary).await?;
    // run every registered detection; this logs and skips per-detector failures
    let (mut hits, roster) = detections::run_all_with_roster(&ctx).await?;
    // scan with the operator's YARA rules when configured; the budgeted,
    // failure-isolated scan (see yara_seed_hits) degrades to no hits
    if let Some(rules) = yara_rules {
        hits.extend(yara_seed_hits(rules, binary).await);
    }
    // emulate the decode-candidate functions the earlier detections flagged and
    // seed whatever runtime-built strings they produce. The stage awaits bridge
    // I/O (not in-process CPU), so it runs inline under one overall budget; the
    // per-run deadline lives in the bridge's Java step loop, which is what
    // actually frees the single-threaded bridge when this await is abandoned.
    // Any failure inside the stage degrades to a warning, never sinks seeding.
    match tokio::time::timeout(
        emulation::EMULATION_SEED_BUDGET,
        emulation::emulation_hits(&ctx, &hits),
    )
    .await
    {
        Ok(emulated) => hits.extend(emulated),
        Err(_) => {
            tracing::warn!("emulation seed stage timed out; continuing without it");
        }
    }
    // run the detector-flagged library scripts too, with the same per-detector
    // failure isolation; their hits merge into the same buckets below
    let mut script_questions = Vec::new();
    if let Some(host) = host.filter(|_| !script_detectors.is_empty()) {
        let (script_hits, questions) =
            script_detections::run_script_detectors(&ctx, &host, &script_detectors).await;
        hits.extend(script_hits);
        script_questions = questions;
    }
    // nothing to seed means the agent simply starts cold, as before -- the
    // roster still reports, since zero-hit detections are telemetry too
    if hits.is_empty() && script_questions.is_empty() {
        tracing::info!("no detector findings; forum left unseeded");
        return Ok(SeedReport { roster });
    }
    // validate the evidence against the code around it before any noise
    // classification: verified result gates and recovered arguments re-earn
    // their catalog severity, refuted claims are reworded and demoted
    crate::thorpal::detect::validate::apply(&ctx, &mut hits).await;
    // classify runtime-library noise once over every source's hits: a hit in
    // recognizable runtime code is weakened a step and gains the runtime note
    detections::apply_runtime_context(&ctx, &mut hits);
    // apply the per-detection admission budget: verified gates and validated
    // artifacts always seed, the strongest-ranked hits seed while they fit,
    // and the overflow lands itemized in the census posts seeded below
    let (hits, census) = apply_admission_budget(hits);
    // group the admitted hits into their statement-post buckets, each sorted
    // strongest-first (per-function, with file-level split per detection)
    let buckets = bucket_hits(hits);
    // split pure noise buckets out — a fingerprinted toolchain's stdlib, or
    // weak artifact-less matches in any binary — so they cannot flood the
    // forum; the rollup post below keeps the record
    let (buckets, suppressed) = partition_suppressed(ctx.toolchain, buckets);
    if !suppressed.is_empty() {
        // one post summarizes everything suppressed, so the forum stays honest
        if let Err(error) = seed_suppressed_rollup(forum, ctx.toolchain, &suppressed, scope).await {
            // a failed rollup must not sink the real seeding below
            tracing::warn!(?error, "failed to seed the runtime-noise rollup post");
        }
    }
    // seed one census post per over-budget detection so every held-back
    // finding stays itemized and re-raisable
    for (detection, overflow) in &census {
        if let Err(error) = seed_census_post(forum, detection, overflow, scope).await {
            // a failed census must not sink the real seeding below
            tracing::warn!(
                detection = %detection,
                ?error,
                "failed to seed a census post; skipping"
            );
        }
    }
    // seed one statement post per bucket, then one comment per finding under it
    for (key, hits) in &buckets {
        // seed the post for this bucket and, on success, its per-finding comments
        if let Err(error) = seed_bucket(Some(&ctx), forum, key, hits, scope).await {
            // one bad bucket must not sink the rest of the seeding
            tracing::warn!(?key, ?error, "failed to seed a detector bucket; skipping");
        }
    }
    // derive and seed the targeted follow-up questions when enabled; the
    // script-emitted questions take priority within the shared cap since a
    // promoted detector asked for them explicitly
    if seed_questions {
        let questions = merge_questions(script_questions, derive_questions(&buckets));
        seed_question_posts(forum, questions, scope).await;
    }
    Ok(SeedReport { roster })
}

/// What one seeding pass reports back to its caller
///
/// Seeding itself is best-effort and logs its own failures; the report exists
/// so the caller can persist the pass's detector roster as telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedReport {
    /// Every registry detection's outcome, zero-hit detections included
    pub roster: Vec<DetectorRun>,
}

/// Scan a sample with the operator's YARA rules under the seed budget.
///
/// Compile+scan is CPU work, so it runs on a blocking thread under a hard
/// overall budget (a pathological pack can stall compilation, which the
/// scanner's own timeout does not cover), and any failure degrades to a
/// warning and an empty hit list rather than sinking the caller's seeding.
/// The scan reads the file itself -- never a capped raw-bytes cache and never
/// Ghidra -- so rules keyed on filesize or late-file content judge the real
/// sample and the hits stay correctly attributed no matter what the bridge
/// holds.
///
/// # Arguments
///
/// * `rules` - The operator-configured rules file or directory
/// * `binary` - The path to the sample to scan
async fn yara_seed_hits(rules: &Path, binary: &Path) -> Vec<DetectionHit> {
    // own the paths so the scan can move onto a blocking thread
    let rules = rules.to_path_buf();
    let target = binary.to_path_buf();
    // run the scan off the async runtime under the overall seed budget
    let scanned = tokio::time::timeout(
        YARA_SEED_BUDGET,
        tokio::task::spawn_blocking(move || {
            crate::thorpal::detect::yara::scan_file(&rules, &target)
        }),
    )
    .await;
    // unpack the three failure layers, degrading each to a warning
    match scanned {
        Ok(Ok(Ok(yara_hits))) => yara_hits,
        Ok(Ok(Err(error))) => {
            tracing::warn!(?error, "yara seed scan failed; continuing without it");
            Vec::new()
        }
        Ok(Err(error)) => {
            tracing::warn!(?error, "yara seed scan task died; continuing without it");
            Vec::new()
        }
        // the orphaned blocking thread finishes on its own; nothing
        // downstream waits on it
        Err(_) => {
            tracing::warn!("yara seed scan timed out; continuing without it");
            Vec::new()
        }
    }
}

/// Seed only a sample's YARA findings, with no Ghidra context at all.
///
/// Used for a primary sample whose Ghidra import failed: the full detector
/// pass would read whichever sibling program the bridge holds and
/// mis-attribute that program's code to the primary, but the YARA scan reads
/// the on-disk bytes directly (see [`yara_seed_hits`]), so its findings stay
/// correctly attributed regardless of Ghidra's state. YARA hits are
/// file-level by construction, so no analysis context is needed to name their
/// buckets; no questions are derived (derivation leans on Ghidra context).
///
/// # Arguments
///
/// * `forum` - The instance's forum state to seed the findings onto
/// * `binary` - The path to the sample to scan
/// * `yara_rules` - The operator's YARA rules to scan the sample with
/// * `scope` - The sample's post-name namespace, or `None` for the main pass
#[tracing::instrument(skip_all, err(Debug))]
pub(crate) async fn seed_yara_only(
    forum: &ForumState,
    binary: &Path,
    yara_rules: &Path,
    scope: Option<&SampleScope>,
) -> anyhow::Result<()> {
    // scan the raw file bytes under the usual seed budget
    let hits = yara_seed_hits(yara_rules, binary).await;
    // nothing matched simply means the forum stays cold for this sample
    if hits.is_empty() {
        return Ok(());
    }
    // group into buckets for shape parity with the main pass; YARA hits are
    // file-level and all one detection, so this keeps the fixed `file-level`
    // bucket in practice
    let buckets = bucket_hits(hits);
    // seed one post per bucket, with no context to resolve function names from
    for (key, hits) in &buckets {
        if let Err(error) = seed_bucket(None, forum, key, hits, scope).await {
            // one bad bucket must not sink the rest of the seeding
            tracing::warn!(?key, ?error, "failed to seed a yara bucket; skipping");
        }
    }
    Ok(())
}

/// Seed the annotation naming a primary sample whose Ghidra import failed.
///
/// A failed primary still gets its identity post and the YARA-only pass, but
/// its Ghidra-context detector pass is skipped (see
/// [`crate::thorpal::ThorPal::seed_detectors`]), so this post is what tells
/// the model the primary was never statically seeded: the detector threads on
/// the forum describe other programs, and the primary's bytes are reachable
/// through the raw file tools only. Phrased to match the sample-set map's
/// per-sample import notes.
///
/// # Arguments
///
/// * `forum` - The instance's forum state to seed the annotation onto
/// * `program` - The primary's program (domain file) name
/// * `reason` - The recorded import failure for the primary
#[tracing::instrument(skip_all, err(Debug))]
pub(crate) async fn seed_failed_primary_annotation(
    forum: &ForumState,
    program: &str,
    reason: &str,
) -> anyhow::Result<()> {
    // render the record honestly: a gated member was never attempted at all
    let described = describe_import_failure(reason);
    let body = format!(
        "The primary sample '{program}' is NOT loaded in Ghidra {described}, \
         so no static detector findings were seeded for it -- any detector \
         threads on this forum describe the other programs in the project, \
         not the primary. Use the raw file tools -- file_info, read_text, \
         read_file, strings_file -- with target='{program}' to examine its \
         bytes instead; the Ghidra tools (and import_child of the same file) \
         will keep failing the same way."
    );
    // the annotation is an asserted fact about the seeding, so a statement
    forum
        .seed_detector_post(
            SEED_AUTHOR.to_string(),
            "primary-not-loaded".to_string(),
            format!("Primary sample '{program}' is not loaded in Ghidra"),
            body,
            Kind::Statement,
            "annotation",
            None,
            Vec::new(),
        )
        .await?;
    Ok(())
}

/// Render a recorded import failure for display, honest about gated skips
///
/// A classification-gated record (a reason starting `not attempted:`) never
/// ran an import at all, so it renders as "(not attempted: ...)"; everything
/// else really paid a failed import and renders as "(import failed: ...)".
/// The rendered chain is flattened and capped so it stays scannable; the full
/// reason still answers any tool call against the program's name.
///
/// # Arguments
///
/// * `reason` - The recorded import-failure reason
pub(crate) fn describe_import_failure(reason: &str) -> String {
    // keep the single-line rendered chain short enough to scan
    let flat: String = reason.replace('\n', " ").chars().take(160).collect();
    // a gated record already says "not attempted"; do not call it a failure
    if flat.starts_with("not attempted:") {
        format!("({flat})")
    } else {
        format!("(import failed: {flat})")
    }
}

/// The key one seeded statement-post bucket is grouped and ordered by.
///
/// Function buckets stay shared across detections — several detections'
/// evidence co-locating in one function is genuine corroboration — but
/// file-level buckets split per producing detection, so one detection's
/// disputed file-level thread can never sweep another detection's unrelated
/// lead. `FileLevel` sorts before `Function`, keeping the historical
/// file-first seeding order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum BucketKey {
    /// A file-level bucket: `Some` names the producing detection, and `None`
    /// is the single-detection case keeping the fixed `file-level` post name
    /// (forum continuity for the common one-detection run).
    FileLevel(Option<String>),
    /// A function bucket, keyed by the containing function's entry address.
    Function(u64),
}

/// The stable per-detection grouping key for a hit.
///
/// Prefers the stamped producing-detection name and falls back to the
/// producing source's key (`yara`, `emulation`, …) for hits no runner
/// stamped, so budgeting and file-level bucketing always have a name.
///
/// # Arguments
///
/// * `hit` - The hit to key
fn detection_key(hit: &DetectionHit) -> &str {
    // the stamped detection name wins; the source key is the fallback
    match &hit.detection {
        Some(name) => name.as_str(),
        None => hit.source.key(),
    }
}

/// Sanitize a detection name into a space-free post-name component.
///
/// Post names are cited as `#name` tokens, so every character outside one
/// safe set is replaced rather than breaking the reference.
///
/// # Arguments
///
/// * `name` - The detection name to sanitize
fn name_component(name: &str) -> String {
    // keep the registry's snake_case names byte-identical; map the rest
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The bucket key a hit lands in.
///
/// # Arguments
///
/// * `hit` - The hit to key
fn hit_bucket_key(hit: &DetectionHit) -> BucketKey {
    // function hits share one bucket per function; file-level hits split per
    // producing detection
    match hit.function {
        Some(entry) => BucketKey::Function(entry),
        None => BucketKey::FileLevel(Some(detection_key(hit).to_owned())),
    }
}

/// Group hits into their seeded buckets, each sorted strongest-first.
///
/// Function hits group by containing function entry; file-level hits split
/// per producing detection, except that a run where exactly one detection
/// produced file-level hits keeps the single fixed `file-level` bucket (and
/// post name) for forum continuity. Each bucket is sorted strongest, most
/// suspicious first so threads read top-down and question derivation sees
/// every bucket pre-ranked.
///
/// # Arguments
///
/// * `hits` - The hits to group
fn bucket_hits(hits: Vec<DetectionHit>) -> BTreeMap<BucketKey, Vec<DetectionHit>> {
    // group the hits under their bucket keys in stable order
    let mut buckets: BTreeMap<BucketKey, Vec<DetectionHit>> = BTreeMap::new();
    for hit in hits {
        buckets.entry(hit_bucket_key(&hit)).or_default().push(hit);
    }
    // a lone file-level detection keeps the fixed `file-level` post name
    let file_level: Vec<BucketKey> = buckets
        .keys()
        .filter(|key| matches!(key, BucketKey::FileLevel(_)))
        .cloned()
        .collect();
    if let [only] = file_level.as_slice()
        && let Some(hits) = buckets.remove(only)
    {
        buckets.insert(BucketKey::FileLevel(None), hits);
    }
    // strongest, most suspicious findings first within every bucket
    for hits in buckets.values_mut() {
        hits.sort_by(|a, b| {
            confidence_rank(b.confidence)
                .cmp(&confidence_rank(a.confidence))
                .then(b.suspicion.cmp(&a.suspicion))
        });
    }
    buckets
}

/// The default greatest number of budgeted finding-shape hits one detection seeds.
///
/// Verified-use hits ([`CallContext::GatesBranch`]) and hits carrying a
/// class-validated artifact bypass the budget entirely; the rest of a
/// detection's hits seed as individual finding comments only while they fit
/// this budget, and the overflow lands itemized (and re-raisable) in that
/// detection's census post. Fleet telemetry puts confirmed threads at well
/// under one per detection per session, so the budget costs essentially no
/// confirmed mass while cutting the ignore flood.
pub const DETECTOR_FINDING_BUDGET: usize = 10;

/// Per-detection overrides of [`DETECTOR_FINDING_BUDGET`].
///
/// The flood-prone string detectors keep a few more presence-grade findings
/// per sample: their strongest leads are artifact-validated (and bypass the
/// budget anyway), and their mid-tier volume still earns engagement.
const DETECTOR_BUDGET_OVERRIDES: &[(&str, usize)] =
    &[("encoded_strings", 15), ("network_comms", 15)];

/// The admission budget for one detection's finding-shape hits.
///
/// # Arguments
///
/// * `detection` - The producing detection's name
fn detection_budget(detection: &str) -> usize {
    // overrides first, then the shared default
    DETECTOR_BUDGET_OVERRIDES
        .iter()
        .find(|(name, _)| *name == detection)
        .map_or(DETECTOR_FINDING_BUDGET, |(_, budget)| *budget)
}

/// Whether a hit bypasses the admission budget entirely.
///
/// Verified result use (the validation pass set [`CallContext::GatesBranch`])
/// and class-validated artifacts are the two evidence shapes whose confident
/// claims survive scrutiny, so they always seed as findings and never compete
/// for the budget.
///
/// # Arguments
///
/// * `hit` - The hit to judge
fn bypasses_budget(hit: &DetectionHit) -> bool {
    // verified gates and validated recoveries always seed
    matches!(hit.context, Some(CallContext::GatesBranch)) || artifact_is_validated(hit)
}

/// Apply the per-detection admission budget over the validated hit list.
///
/// Groups the hits by producing detection and ranks each detection's hits
/// validated-artifact first, then confidence and suspicion descending. Hits
/// that bypass the budget (see [`bypasses_budget`]) always stay finding-shape;
/// of the rest, the top [`detection_budget`] stay and the overflow is returned
/// per detection, still ranked, for the census posts — demoted, itemized, and
/// re-raisable, never dropped.
///
/// # Arguments
///
/// * `hits` - The full validated hit list
fn apply_admission_budget(
    hits: Vec<DetectionHit>,
) -> (Vec<DetectionHit>, BTreeMap<String, Vec<DetectionHit>>) {
    // group the hits under their producing detection's name
    let mut per_detection: BTreeMap<String, Vec<DetectionHit>> = BTreeMap::new();
    for hit in hits {
        per_detection
            .entry(detection_key(&hit).to_owned())
            .or_default()
            .push(hit);
    }
    // admit each detection's ranked hits against its budget
    let mut kept = Vec::new();
    let mut census: BTreeMap<String, Vec<DetectionHit>> = BTreeMap::new();
    for (detection, mut hits) in per_detection {
        // rank validated recoveries first, then the usual severity order
        hits.sort_by_cached_key(|hit| {
            (
                std::cmp::Reverse(artifact_is_validated(hit)),
                std::cmp::Reverse(confidence_rank(hit.confidence)),
                std::cmp::Reverse(hit.suspicion),
            )
        });
        // bypass hits are exempt; the rest consume the budget in ranked order
        let budget = detection_budget(&detection);
        let mut admitted = 0;
        let mut overflow = Vec::new();
        for hit in hits {
            if bypasses_budget(&hit) {
                kept.push(hit);
            } else if admitted < budget {
                admitted += 1;
                kept.push(hit);
            } else {
                overflow.push(hit);
            }
        }
        // only an over-budget detection earns a census post
        if !overflow.is_empty() {
            census.insert(detection, overflow);
        }
    }
    (kept, census)
}

/// Split the buckets into kept buckets and low-signal noise.
///
/// Two criteria suppress a bucket, and both demote rather than drop (the
/// rollup post records every suppressed hit):
///
/// - **Toolchain-attributed noise** (needs a fingerprinted toolchain): every
///   hit is `Untrusted` outright (the ubiquitous-API grade that also fires on
///   benign code), catalog-asserted inherent noise, or `Unsure` while sitting
///   in classified runtime code.
/// - **Weak and artifact-less** (no fingerprint needed): every hit is
///   `Untrusted` or inherent noise AND recovered nothing passing its artifact
///   class's validity gate — the stripped-binary flood, where name-based
///   runtime recognition is blind.
///
/// File-level buckets are never suppressed, and a bucket carrying any
/// class-validated pivot artifact (configs, endpoints, keys, certificates,
/// host identifiers) is always kept — a real recovery stays loud wherever it
/// sits, while a fabricated junk value that fails its validity gate no longer
/// shields its bucket.
///
/// # Arguments
///
/// * `toolchain` - The fingerprinted toolchain, if any
/// * `buckets` - The hit buckets, strongest-first
fn partition_suppressed(
    toolchain: Option<Toolchain>,
    buckets: BTreeMap<BucketKey, Vec<DetectionHit>>,
) -> (BTreeMap<BucketKey, Vec<DetectionHit>>, Vec<DetectionHit>) {
    // a single hit is toolchain-attributable noise when weak outright,
    // self-asserted inherent noise, or runtime-located and unsure
    let is_noise = |hit: &DetectionHit| {
        hit.always_noise
            || matches!(hit.confidence, Confidence::Untrusted)
            || (matches!(hit.confidence, Confidence::Unsure)
                && matches!(hit.context, Some(CallContext::Runtime(_))))
    };
    // a hit is suppressible with no fingerprint only when it is weak-grade
    // (or inherent noise) and recovered nothing passing its validity gate
    let is_weak_unvalidated = |hit: &DetectionHit| {
        (hit.always_noise || matches!(hit.confidence, Confidence::Untrusted))
            && !artifact_is_validated(hit)
    };
    // validated pivot-class artifacts veto suppression for their whole
    // bucket; a value failing its class's gate earns no shield
    let has_protected = |hits: &[DetectionHit]| {
        hits.iter().any(|hit| {
            hit.artifact.as_ref().is_some_and(|artifact| {
                matches!(
                    artifact.class,
                    ArtifactClass::ConfigRecord
                        | ArtifactClass::NetworkEndpoint
                        | ArtifactClass::SymmetricKey
                        | ArtifactClass::AsymmetricPrivateKey
                        | ArtifactClass::AsymmetricPublicKey
                        | ArtifactClass::Certificate
                        | ArtifactClass::HostIdentifier
                )
            }) && artifact_is_validated(hit)
        })
    };
    // keep or suppress whole buckets so threads never show half their findings
    let mut kept: BTreeMap<BucketKey, Vec<DetectionHit>> = BTreeMap::new();
    let mut suppressed: Vec<DetectionHit> = Vec::new();
    for (key, hits) in buckets {
        // the toolchain criterion attributes noise to a fingerprinted runtime
        let runtime_noise = toolchain.is_some() && hits.iter().all(is_noise);
        // the fingerprint-free criterion demands every hit weak and unvalidated
        let weak_unvalidated = hits.iter().all(is_weak_unvalidated);
        // file-level findings and validated artifacts are always kept
        let suppress = matches!(key, BucketKey::Function(_))
            && !has_protected(&hits)
            && (runtime_noise || weak_unvalidated);
        if suppress {
            suppressed.extend(hits);
        } else {
            kept.insert(key, hits);
        }
    }
    (kept, suppressed)
}

/// The most findings itemized in a rollup-shape post's body (the runtime-noise
/// rollup and the per-detection census posts).
const MAX_ROLLUP_LINES: usize = 40;

/// The most findings recorded structurally on one rollup-shape post.
///
/// Bounded well past the body's line cap, so telemetry sees what the rendered
/// text truncates.
const MAX_ROLLUP_PROVENANCE: usize = 500;

/// Itemize hits into compact rollup lines, bounded with a counted tail.
///
/// # Arguments
///
/// * `hits` - The hits to itemize, strongest-first
/// * `tail` - What the tail line calls the entries the cap cut
fn rollup_lines(hits: &[DetectionHit], tail: &str) -> String {
    // one compact line per hit: technique, site, and the content's first line
    let mut lines = Vec::new();
    for hit in hits.iter().take(MAX_ROLLUP_LINES) {
        let content = hit.content.lines().next().unwrap_or_default();
        lines.push(format!(
            "- {} @ {}: {}",
            hit.technique.id,
            fmt_addr(hit.address),
            content
        ));
    }
    // note what the cap cut so the counts still add up
    if hits.len() > MAX_ROLLUP_LINES {
        lines.push(format!(
            "- … and {} more {tail}",
            hits.len() - MAX_ROLLUP_LINES
        ));
    }
    lines.join("\n")
}

/// Seed the single rollup post recording the suppressed low-signal findings.
///
/// The wording follows the enabling criterion: with a fingerprinted toolchain
/// the post names its vocabulary (statically linked or imported — never a
/// static-linking claim) and splits the counts into runtime-located vs
/// weak-grade; without one it claims only weak grade, never a runtime.
///
/// # Arguments
///
/// * `forum` - The instance's forum state to seed the post onto
/// * `toolchain` - The fingerprinted toolchain, when one enabled suppression
/// * `suppressed` - The suppressed hits, itemized (bounded) in the body
/// * `scope` - The sample's post-name namespace, or `None` for the main pass
#[tracing::instrument(skip_all, err(Debug))]
async fn seed_suppressed_rollup(
    forum: &ForumState,
    toolchain: Option<Toolchain>,
    suppressed: &[DetectionHit],
    scope: Option<&SampleScope>,
) -> anyhow::Result<()> {
    // split the two suppression classes so the post never mislabels noise
    // that merely co-resides with a fingerprinted runtime
    let runtime_located = suppressed
        .iter()
        .filter(|hit| matches!(hit.context, Some(CallContext::Runtime(_))))
        .count();
    let low_grade = suppressed.len() - runtime_located;
    // word the title and the lead sentence for what the counts actually are:
    // a fingerprinted toolchain's vocabulary, or plain weak-grade matches
    let (title, lead) = match toolchain {
        Some(toolchain) => (
            format!(
                "{} low-signal detector findings suppressed (recognized {} binary)",
                suppressed.len(),
                toolchain.label()
            ),
            format!(
                "This binary carries the vocabulary of a recognized {} toolchain \
                 or runtime (statically linked or imported): {runtime_located} \
                 findings sit inside recognized runtime/support code, and \
                 {low_grade} are weak-grade matches on APIs and byte patterns \
                 that benign code also contains.",
                toolchain.label()
            ),
        ),
        None => (
            format!(
                "{} low-signal detector findings suppressed (weak-grade matches)",
                suppressed.len()
            ),
            format!(
                "These {} detector findings are weak-grade matches on APIs and \
                 byte patterns that benign code also contains, and none of them \
                 passed an artifact validity gate.",
                suppressed.len()
            ),
        ),
    };
    // itemize each suppressed finding (bounded) so nothing is silently lost
    let body = format!(
        "{lead} The findings were collapsed here instead of seeding \
         per-function threads. Re-raise any entry below as a question if the \
         host turns out to be suspect.\n{}",
        rollup_lines(suppressed, "of the same grades"),
    );
    // the rollup is an asserted fact about the seeding, so it is a statement
    forum
        .seed_detector_post(
            SEED_AUTHOR.to_string(),
            scoped_name(scope, "runtime-noise"),
            scoped_title(scope, title),
            body,
            Kind::Statement,
            "rollup",
            None,
            // record every suppressed finding structurally
            suppressed
                .iter()
                .take(MAX_ROLLUP_PROVENANCE)
                .map(|hit| finding_provenance(hit, true))
                .collect(),
        )
        .await?;
    Ok(())
}

/// Seed one detection's census post itemizing its budget-overflow findings.
///
/// Reuses the suppressed-rollup rendering shape — a bounded itemized body
/// plus capped structured provenance — and stays honest that nothing was
/// discarded: every held-back entry is listed (or counted) and re-raisable.
///
/// # Arguments
///
/// * `forum` - The instance's forum state to seed the post onto
/// * `detection` - The producing detection's name
/// * `overflow` - The held-back hits, ranked strongest-first
/// * `scope` - The sample's post-name namespace, or `None` for the main pass
#[tracing::instrument(skip_all, err(Debug))]
async fn seed_census_post(
    forum: &ForumState,
    detection: &str,
    overflow: &[DetectionHit],
    scope: Option<&SampleScope>,
) -> anyhow::Result<()> {
    // name the post per detection so reruns and resumes never collide
    let name = scoped_name(scope, &format!("census-{}", name_component(detection)));
    let title = format!(
        "{} low-priority {detection} findings held back (admission budget)",
        overflow.len()
    );
    // itemize each held-back finding (bounded) so nothing is silently lost
    let body = format!(
        "The {detection} detection produced more findings than its per-sample \
         admission budget seeds as individual leads; the {} lowest-ranked are \
         recorded here instead (ranking favors verified use and validated \
         artifacts, then confidence and suspicion). Nothing was discarded — \
         re-raise any entry as a question if it bears on your analysis.\n{}",
        overflow.len(),
        rollup_lines(overflow, "held back by the same budget"),
    );
    // the census is an asserted fact about the seeding, so it is a statement
    forum
        .seed_detector_post(
            SEED_AUTHOR.to_string(),
            name,
            scoped_title(scope, title),
            body,
            Kind::Statement,
            "rollup",
            None,
            // record every held-back finding structurally
            overflow
                .iter()
                .take(MAX_ROLLUP_PROVENANCE)
                .map(|hit| finding_provenance(hit, true))
                .collect(),
        )
        .await?;
    Ok(())
}

/// Seed one function's (or a file-level) post plus a comment per finding.
///
/// # Arguments
///
/// * `ctx` - The analysis context (used to resolve the function name), or
///   `None` for a Ghidra-less pass whose hits are file-level anyway
/// * `forum` - The instance's forum state to seed the post and comments onto
/// * `key` - The bucket's key (function entry, or a file-level detection)
/// * `hits` - The findings in this bucket, already sorted strongest-first
/// * `scope` - The sample's post-name namespace, or `None` for the main pass
#[tracing::instrument(skip_all, err(Debug))]
async fn seed_bucket(
    ctx: Option<&AnalysisContext>,
    forum: &ForumState,
    key: &BucketKey,
    hits: &[DetectionHit],
    scope: Option<&SampleScope>,
) -> anyhow::Result<()> {
    // derive a unique, space-free post name distinct from any LLM-chosen name
    let name = scoped_name(scope, &bucket_name(key));
    // build the human-readable title: function label + address + finding count
    let title = match key {
        BucketKey::Function(entry) => {
            // resolve the function name, falling back to the raw address
            let label = ctx
                .and_then(|ctx| ctx.function_name(*entry))
                .unwrap_or("(unnamed)");
            format!(
                "{label} @ {} — {} detector finding(s)",
                fmt_addr(*entry),
                hits.len()
            )
        }
        BucketKey::FileLevel(Some(detection)) => {
            format!(
                "File-level ({detection}) — {} detector finding(s)",
                hits.len()
            )
        }
        BucketKey::FileLevel(None) => format!("File-level — {} detector finding(s)", hits.len()),
    };
    // roll the distinct techniques up into the post body, plus the max confidence
    let body = post_body(hits);
    // seed the post; a duplicate name (shouldn't happen with the fn-/file- prefixes)
    // surfaces as an error the caller logs and skips. Detector findings are
    // asserted facts, so every evidence post is a statement — the targeted
    // questions derived from the strongest findings are seeded separately.
    forum
        .seed_detector_post(
            SEED_AUTHOR.to_string(),
            name.clone(),
            scoped_title(scope, title),
            body,
            Kind::Statement,
            "bucket",
            None,
            // the post's provenance rolls up every finding it fronts
            hits.iter()
                .map(|hit| finding_provenance(hit, false))
                .collect(),
        )
        .await?;
    // attach one comment per finding under the freshly-created post
    for hit in hits {
        // detectors assert evidence; they take no position on a parent claim, and
        // their findings are statements of fact rather than questions
        forum
            .seed_detector_comment(
                SEED_AUTHOR.to_string(),
                ParentRef::Post(name.clone()),
                comment_body(hit),
                Stance::Neutral,
                Kind::Statement,
                "finding",
                vec![finding_provenance(hit, false)],
            )
            .await?;
    }
    Ok(())
}

/// The unique, space-free statement post name for a bucket.
///
/// # Arguments
///
/// * `key` - The bucket's key (function entry, or a file-level detection)
fn bucket_name(key: &BucketKey) -> String {
    // function buckets name by entry address; split file-level buckets carry
    // their detection, and the single-detection case keeps the fixed name
    match key {
        BucketKey::Function(entry) => format!("fn-{entry:08x}"),
        BucketKey::FileLevel(Some(detection)) => {
            format!("file-level-{}", name_component(detection))
        }
        BucketKey::FileLevel(None) => "file-level".to_string(),
    }
}

/// Build a post body summarizing a bucket's findings.
///
/// Lists the distinct techniques observed in the function and the highest
/// confidence any finding reached, so the post header reads as a quick rollup
/// before the per-finding comments.
///
/// # Arguments
///
/// * `hits` - The findings in this bucket
fn post_body(hits: &[DetectionHit]) -> String {
    // collect the distinct technique names in first-seen order
    let mut techniques: Vec<&str> = Vec::new();
    for hit in hits {
        let technique = hit.technique.name.as_ref();
        if !techniques.contains(&technique) {
            techniques.push(technique);
        }
    }
    // the strongest confidence any finding here reached (hits are sorted, so the
    // first is the max, but recompute to stay robust to caller changes)
    let max_confidence = hits
        .iter()
        .map(|hit| confidence_rank(hit.confidence))
        .max()
        .map(detections::confidence_from_rank)
        .map(confidence_label)
        .unwrap_or("unknown");
    // assemble the rollup line
    format!(
        "Static detectors flagged this function. Techniques: {}. Max confidence: {}.",
        techniques.join(", "),
        max_confidence,
    )
}

/// Build a comment body describing a single finding.
///
/// # Arguments
///
/// * `hit` - The finding to render
fn comment_body(hit: &DetectionHit) -> String {
    // start with the technique id/name and the specific characteristic observed;
    // the labeled reasoning block gives the report appendix's line filter an
    // anchor so evidence lines survive elision while the prose does not
    let mut body = format!(
        "[{} {}] {}\nReasoning: {}",
        hit.technique.id, hit.technique.name, hit.content, hit.reasoning
    );
    // record where the evidence sits and how confident/suspicious it is
    body.push_str(&format!(
        "\nSite: {} | confidence: {} | suspicion: {}",
        fmt_addr(hit.address),
        confidence_label(hit.confidence),
        hit.suspicion,
    ));
    // list the backing sites (capped) when the finding spans more than its primary
    if !hit.sites.is_empty() {
        // render a bounded, comma-separated list of the backing addresses
        let shown: Vec<String> = hit
            .sites
            .iter()
            .take(MAX_SITES_LISTED)
            .map(|a| fmt_addr(*a))
            .collect();
        body.push_str(&format!(
            "\nSites ({}): {}",
            hit.sites.len(),
            shown.join(", ")
        ));
        // note when the list was truncated so the count still adds up
        if hit.sites.len() > MAX_SITES_LISTED {
            body.push_str(", …");
        }
    }
    // surface any recovered artifact value the agent can pivot on
    if let Some(artifact) = &hit.artifact {
        body.push_str(&format!(
            "\nArtifact ({}): {}",
            artifact.class.label(),
            artifact.value
        ));
    }
    // append the surrounding-code context sentence when the provenance pass set it
    if let Some(context) = &hit.context {
        body.push('\n');
        body.push_str(&context.note());
    }
    body
}

/// The lowercase label for a confidence level (the enum has no `Display`).
///
/// # Arguments
///
/// * `confidence` - The confidence level to label
fn confidence_label(confidence: Confidence) -> &'static str {
    // map each variant to a short, analyst-facing word
    match confidence {
        Confidence::Fact => "fact",
        Confidence::Likely => "likely",
        Confidence::Unsure => "unsure",
        Confidence::Untrusted => "untrusted",
    }
}

/// Reduces one detection hit to its structured provenance record
///
/// The record mirrors exactly what the seeded template text flattens --
/// technique, confidence, suspicion, source, sites, artifact class -- so
/// offline telemetry reads these facts instead of parsing them back out.
///
/// # Arguments
///
/// * `hit` - The hit to record
/// * `suppressed` - Whether the hit was demoted into a rollup-shape post (the
///   runtime-noise rollup or a census post) instead of a finding comment
fn finding_provenance(hit: &DetectionHit, suppressed: bool) -> FindingProvenance {
    FindingProvenance {
        technique: hit.technique.id.to_string(),
        confidence: confidence_label(hit.confidence).to_owned(),
        suspicion: hit.suspicion,
        source: hit.source.key().to_owned(),
        detection: hit.detection.clone(),
        sites: hit.sites.len() as u64,
        // only the class key is recorded, never the recovered value
        artifact_class: hit
            .artifact
            .as_ref()
            .map(|artifact| artifact.class.key().to_owned()),
        suppressed,
    }
}

// ===========================================================================
// Derived questions (the hybrid half of seeding)
// ===========================================================================

/// A targeted follow-up question derived from the detectors' strongest findings
/// or emitted directly by a detector-flagged library script.
///
/// Seeded onto the forum as an open question post
/// ([`Kind::Question`]/[`QuestionStatus::Unsolved`]), so the main agent's
/// question loop picks it up and delegates it like any question it raised itself.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SeedQuestion {
    /// The unique, space-free post name (e.g. `q-unpack`)
    pub(crate) name: String,
    /// The short human-readable question title
    pub(crate) title: String,
    /// The full question text, citing the backing statement posts
    pub(crate) body: String,
    /// The completion-gate check tasks seeded under this question, possibly empty
    pub(crate) tasks: Vec<String>,
    /// What derived this question, recorded as provenance: `derived` for the
    /// finding-driven questions, `format` for file-format triage leads,
    /// `script` for promoted detector scripts, `prior_toolkit` for prior
    /// toolkit matches
    pub(crate) origin: &'static str,
}

/// The most container entries quoted in the container-payload question body.
const MAX_QUESTION_ENTRIES: usize = 20;

/// The check tasks for an archive the native container tools can enumerate.
fn archive_extraction_tasks() -> Vec<String> {
    vec![
        "Enumerate the container's entries with list_container and post the listing as \
         evidence"
            .to_string(),
        "Extract the embedded payload, identify it (hashes, format), and post its identity \
         as evidence"
            .to_string(),
    ]
}

/// The check tasks for a single compressed stream the decompress tool inflates.
fn stream_extraction_tasks() -> Vec<String> {
    vec![
        "Decompress the embedded stream with the decompress tool and post the result as \
         evidence"
            .to_string(),
        "Identify the decompressed payload (hashes, format) and post its identity as \
         evidence"
            .to_string(),
    ]
}

/// The check task for an archive format the native tools cannot extract.
fn raw_triage_tasks() -> Vec<String> {
    vec![
        "Triage the raw archive bytes with read_file/strings_file and post what the archive \
         carries as evidence"
            .to_string(),
    ]
}

/// The check tasks for an Office document whose streams flag macro content.
fn maldoc_extraction_tasks() -> Vec<String> {
    vec![
        "Run extract_macros and post the recovered VBA module source and XLM macro-sheet \
         findings as evidence"
            .to_string(),
        "Analyze the macro logic (decoded strings, payload URLs/paths, the execution \
         chain) and post what the document does as evidence"
            .to_string(),
    ]
}

/// The stream/storage names that mark a CDF as an Office document.
///
/// An OLE compound file is either an MSI installer or a legacy Office
/// document; these names only ever appear in the document shape -- the one
/// `extract_macros` reads -- so their presence steers the seeded lead from
/// generic stream extraction to macro extraction.
const MALDOC_INDICATORS: [&str; 6] = [
    "Macros",
    "VBA",
    "_VBA_PROJECT",
    "WordDocument",
    "Workbook",
    "Book",
];

/// Find the first Office-document indicator in a CFB listing, if any.
///
/// # Arguments
///
/// * `listing` - A best-effort listing of the container's entries, if one ran
fn maldoc_indicator<'a>(
    listing: Option<&'a crate::thorpal::tools::containers::ContainerListing>,
) -> Option<&'a str> {
    // match entry names case-insensitively (MS-CFB resolution is, so Office
    // loads a case-flipped tree normally); storages and streams both count
    listing?
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .find(|name| {
            MALDOC_INDICATORS
                .iter()
                .any(|indicator| indicator.eq_ignore_ascii_case(name))
        })
}

/// Derive the container-payload question for a container-format sample.
///
/// Runs before any Ghidra work (a container rarely imports), so a sample that
/// is really a wrapper -- an MSI, CAB, ZIP, or compressed stream -- opens with
/// the unpacking lead instead of the agents staring at the wrapper bytes.
/// Returns `None` for non-container formats (PE, ELF, unknown, ...).
///
/// # Arguments
///
/// * `format` - The sample's identified format (see `file::identify_format`)
/// * `listing` - A best-effort listing of the container's entries, if one ran
pub(crate) fn container_question(
    format: &str,
    listing: Option<&crate::thorpal::tools::containers::ContainerListing>,
) -> Option<SeedQuestion> {
    // an OLE compound file whose listing shows a document shape opens with
    // the macro-extraction lead instead of generic stream extraction
    let maldoc = format
        .starts_with("Composite Document File")
        .then(|| maldoc_indicator(listing))
        .flatten();
    // classify the identified format into the advice the tools can back up,
    // paired with the completion-gate check tasks that advice makes concrete
    let (advice, tasks) = if let Some(indicator) = maldoc {
        (
            format!(
                "This sample is an OLE compound DOCUMENT (its '{indicator}' entry \
                 marks a legacy Office shape, not an MSI) -- run extract_macros to \
                 statically decompress every VBA module's source into the extracted \
                 workspace (read the modules with read_text) and to flag Excel 4.0 \
                 (XLM) macro sheets with their hidden state; list_container and \
                 extract_file still reach every other stream, and an extracted OOXML \
                 vbaProject.bin can be fed back through extract_macros via `target`."
            ),
            maldoc_extraction_tasks(),
        )
    } else if format.starts_with("Composite Document File") {
        (
            "This sample is an OLE compound file (MSI installer or legacy Office \
             document) -- enumerate it with list_container (MSI stream names are \
             shown decoded) and extract the embedded payload with extract_file; \
             analyze any extracted executables (extracted files are addressable by \
             every file tool via the `target` argument, nested containers extract \
             all the way down, and a PE/ELF child can be imported for full Ghidra \
             analysis with import_child). If the listing shows Office document \
             streams (Macros/VBA, WordDocument, Workbook), run extract_macros for \
             the macro content."
                .to_string(),
            archive_extraction_tasks(),
        )
    } else if format.starts_with("ZIP archive") {
        (
            "This sample is a ZIP archive -- enumerate it with list_container and \
             extract the embedded payload with extract_file (encrypted malware \
             archives conventionally use the password \"infected\"); analyze any \
             extracted executables (extracted files are addressable by every file \
             tool via the `target` argument, and a PE/ELF child can be imported \
             for full Ghidra analysis with import_child)."
                .to_string(),
            archive_extraction_tasks(),
        )
    } else if format.starts_with("Microsoft Cabinet") {
        (
            "This sample is a CAB archive -- enumerate it with list_container and \
             extract the embedded payload with extract_file; analyze any extracted \
             executables (extracted files are addressable by every file tool via \
             the `target` argument, and a PE/ELF child can be imported for full \
             Ghidra analysis with import_child)."
                .to_string(),
            archive_extraction_tasks(),
        )
    } else if format.starts_with("gzip compressed data") {
        (
            "This sample is a single gzip compressed stream -- inflate it with the \
             decompress tool (format \"gzip\") and analyze the decompressed payload \
             (it lands in the extracted workspace, addressable by every file tool \
             via the `target` argument)."
                .to_string(),
            stream_extraction_tasks(),
        )
    } else if format.starts_with("xz compressed data") {
        (
            "This sample is a single xz compressed stream -- inflate it with the \
             decompress tool (format \"xz\") and analyze the decompressed payload \
             (it lands in the extracted workspace, addressable by every file tool \
             via the `target` argument)."
                .to_string(),
            stream_extraction_tasks(),
        )
    } else if format.starts_with("bzip2 compressed data") {
        (
            "This sample is a single bzip2 compressed stream; native bzip2 \
             decompression is not supported, so triage the raw bytes with \
             read_file/strings_file and note the limitation in your findings."
                .to_string(),
            raw_triage_tasks(),
        )
    } else if format.starts_with("7-Zip archive") || format.starts_with("RAR archive") {
        (
            "This sample is an archive format the native container tools do not \
             support (only ZIP, CAB, and OLE/CFB extract natively), so triage the \
             raw bytes with read_file/strings_file and note the limitation in your \
             findings."
                .to_string(),
            raw_triage_tasks(),
        )
    } else {
        // every other format is a payload in its own right, not a wrapper
        return None;
    };
    // open the body with the format fact, then the tool-backed advice
    let mut body = format!("Identified format: {format}.\n{advice}");
    // quote a bounded sample of the entries when a listing was gathered
    if let Some(listing) = listing
        && !listing.entries.is_empty()
    {
        let shown: Vec<String> = listing
            .entries
            .iter()
            .take(MAX_QUESTION_ENTRIES)
            .map(|entry| format!("- {} ({} bytes)", entry.name, entry.size))
            .collect();
        body.push_str(&format!(
            "\nEntries ({} total):\n{}",
            listing.total_entries,
            shown.join("\n")
        ));
        // note when the list was truncated so the count still adds up
        if listing.total_entries > shown.len() {
            body.push_str("\n- …");
        }
    }
    // the maldoc lead carries its own name and title; wrappers keep the
    // generic payload-extraction framing
    let (name, title) = match maldoc {
        Some(_) => (
            "q-maldoc-macros",
            "Extract and analyze the document's macro content",
        ),
        None => (
            "q-container-payload",
            "Extract and analyze the container's embedded payload",
        ),
    };
    Some(SeedQuestion {
        name: name.to_string(),
        title: title.to_string(),
        body,
        tasks,
        origin: "format",
    })
}

/// The check tasks for a script/text sample triaged without Ghidra.
fn script_triage_tasks() -> Vec<String> {
    vec![
        "Identify the script's language and entry behavior, citing read_text line \
         numbers as evidence"
            .to_string(),
        "Decode the obfuscation layer(s) with the decode tools or a developer script \
         and post the decoded content as evidence"
            .to_string(),
        "Extract the IOCs (URLs, domains, IPs, paths, registry keys) and post them \
         as evidence"
            .to_string(),
    ]
}

/// The check tasks for an RTF document's embedded objects.
fn rtf_extraction_tasks() -> Vec<String> {
    vec![
        "Locate the embedded object data (grep_bytes for objdata/objupdate/\\bin \
         markers) and post the hits as evidence"
            .to_string(),
        "Decode the embedded payload, identify it (hashes, format), and post its \
         identity as evidence"
            .to_string(),
    ]
}

/// Derive the script-triage question for a text/script-format sample.
///
/// The text analogue of [`container_question`], running before any Ghidra
/// work: a text or script sample can never be imported (the loadability gate
/// refuses it without an attempt), so it opens with the read_text-first
/// triage lead instead of the agents discovering the limitation through
/// failed imports. Returns `None` for every non-text format — wrappers get
/// the container question, binaries and unknowns get no lead — so the two
/// leads are mutually exclusive by construction.
///
/// # Arguments
///
/// * `format` - The sample's identified format (see `file::classify_format`)
pub(crate) fn script_question(format: &str) -> Option<SeedQuestion> {
    // classify the identified format into the advice the tools can back up,
    // paired with the completion-gate check tasks that advice makes concrete
    let (name, title, advice, tasks) = if format.starts_with("ASCII text")
        || format.starts_with("UTF-8 text")
        || format.starts_with("UTF-16LE text")
        || format.starts_with("UTF-16BE text")
        || format.starts_with("script with a shebang line")
    {
        (
            "q-script-triage",
            "Identify and deobfuscate the script sample",
            "This sample is a text/script file -- Ghidra does not apply to it and \
             import_child will refuse it; do not retry Ghidra tools against it. \
             Read it with read_text (decoded, line-numbered text; strings_file and \
             grep_bytes for sweeps), identify the language and its entry behavior, \
             peel obfuscation layers with the decode tools (decode_bytes/\
             byte_transform) or a developer script, and when a decoded layer \
             embeds an executable payload, carve it into the workspace with a \
             `name` and import THAT with import_child.",
            script_triage_tasks(),
        )
    } else if format.starts_with("RTF document") {
        (
            "q-rtf-embedded",
            "Locate and extract the RTF's embedded objects",
            "This sample is an RTF document -- Ghidra does not apply to it and \
             import_child will refuse it. Malicious RTFs carry exploit payloads as \
             embedded OLE objects: grep_bytes for the objdata/objupdate/\\bin \
             markers, read the surrounding content with read_text/read_file, \
             decode the embedded hex into bytes, and identify the carved payload \
             (an executable child can be imported with import_child).",
            rtf_extraction_tasks(),
        )
    } else {
        // every other format is either a wrapper (the container question's
        // territory) or a binary payload with no triage lead to seed
        return None;
    };
    // open the body with the format fact, then the tool-backed advice
    Some(SeedQuestion {
        name: name.to_string(),
        title: title.to_string(),
        body: format!("Identified format: {format}.\n{advice}"),
        tasks,
        origin: "format",
    })
}

/// Merge the script-emitted questions with the derived ones under the cap.
///
/// Script questions come first -- a promoted detector asked for them explicitly,
/// so they outrank the generic derived themes -- then duplicates by name are
/// dropped and the whole queue is truncated to [`MAX_SEED_QUESTIONS`] so seeding
/// never floods the delegation loop.
///
/// # Arguments
///
/// * `script` - The questions the detector scripts emitted, in script order
/// * `derived` - The derived questions, in priority order
fn merge_questions(script: Vec<SeedQuestion>, derived: Vec<SeedQuestion>) -> Vec<SeedQuestion> {
    // keep the explicit script questions ahead of the derived themes
    let mut questions = script;
    questions.extend(derived);
    // drop later duplicates so two sources can never seed the same post name
    let mut seen = std::collections::BTreeSet::new();
    questions.retain(|question| seen.insert(question.name.clone()));
    // bound the queue so seeding never floods the delegation loop
    questions.truncate(MAX_SEED_QUESTIONS);
    questions
}

/// Derive the targeted questions the detector findings justify.
///
/// Walks the already-sorted kept buckets (never census or suppressed hits) and
/// emits at most [`MAX_SEED_QUESTIONS`] questions, highest-leverage theme
/// first: unpacking blocks every other analysis angle, a gated host check
/// decides whether the payload runs at all, and so on down to the
/// cross-cutting evasion rollup. Only strong or verified signals qualify — a
/// run whose findings are all weak seeds no questions — so a mostly-benign
/// binary leaves the question queue empty.
///
/// # Arguments
///
/// * `buckets` - The kept findings grouped by bucket key, each sorted
///   strongest-first
fn derive_questions(buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>) -> Vec<SeedQuestion> {
    // collect candidates in fixed priority order so the cap keeps the best
    let mut questions = Vec::new();
    // unpacking first since packed code blocks every other analysis angle
    questions.extend(unpack_question(buckets));
    // host/environment gates decide whether the payload executes at all
    questions.extend(gate_questions(buckets));
    // statically decoded strings/config want their runtime usage confirmed
    questions.extend(decoded_config_question(buckets));
    // recovered key material wants the data it protects identified
    questions.extend(crypto_key_question(buckets));
    // recovered endpoints want the channel behind them characterized
    questions.extend(c2_question(buckets));
    // enumeration + crypto co-occurrence wants a ransomware verdict
    questions.extend(ransomware_question(buckets));
    // a strong signed-digest mismatch wants the uncovered bytes identified
    questions.extend(signature_question(buckets));
    // a corroborated YARA family claim wants code-level verification
    questions.extend(yara_identity_question(buckets));
    // a native-API section-map pair wants its mapped payload traced
    questions.extend(section_map_question(buckets));
    // the cross-function evasion rollup reads the whole gauntlet as one question
    questions.extend(evasion_question(buckets));
    // bound the queue so seeding never floods the delegation loop
    questions.truncate(MAX_SEED_QUESTIONS);
    questions
}

/// Iterate a bucket map as `(bucket key, hit)` pairs in bucket order.
///
/// The derivers below pair every hit with the key of the bucket it seeded
/// under, so their evidence lines cite post names that actually exist.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn bucket_hit_pairs(
    buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>,
) -> impl Iterator<Item = (&BucketKey, &DetectionHit)> {
    // flatten each bucket into key-tagged hits, preserving the map order
    buckets
        .iter()
        .flat_map(|(key, hits)| hits.iter().map(move |hit| (key, hit)))
}

/// Render a capped `Evidence: see #post, …` line citing the backing buckets.
///
/// Citing the statement posts lets the delegated agent jump straight to the
/// evidence (sites, confidence, recovered artifacts) instead of re-deriving it.
///
/// # Arguments
///
/// * `keys` - The bucket keys of the backing findings, strongest first
///   (duplicates are collapsed); the rendered names are the seeded post names
fn evidence_line<'a>(keys: impl IntoIterator<Item = &'a BucketKey>) -> String {
    // dedupe while preserving the caller's strongest-first order
    let mut names: Vec<String> = Vec::new();
    for key in keys {
        let name = bucket_name(key);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    // cap the citation list so one broad theme cannot bloat the question body
    let shown: Vec<String> = names
        .iter()
        .take(MAX_QUESTION_CITES)
        .map(|name| format!("#{name}"))
        .collect();
    let mut line = format!("Evidence: see {}", shown.join(", "));
    // note when the list was truncated so the reader knows there is more
    if names.len() > MAX_QUESTION_CITES {
        line.push_str(", …");
    }
    line
}

/// Derive the unpack question when the packing evidence is strong enough.
///
/// Packing is scored conservatively (a single structural signal is only `low`),
/// so this requires at least a medium-confidence finding before asking the agent
/// to spend a delegation on unpacking.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn unpack_question(buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // collect the packing findings that clear the strength bar
    let cited: Vec<&BucketKey> = bucket_hit_pairs(buckets)
        .filter(|(_, hit)| hit.technique.id.as_ref() == "T1027.002")
        .filter(|(_, hit)| confidence_rank(hit.confidence) >= confidence_rank(Confidence::Unsure))
        .map(|(key, _)| key)
        .collect();
    // weak-or-no packing evidence seeds no question
    if cited.is_empty() {
        return None;
    }
    Some(SeedQuestion {
        name: "q-unpack".to_string(),
        title: "Is this binary packed, and what does the unpacked payload do?".to_string(),
        body: format!(
            "Static detectors found structural packing/obfuscation evidence in this binary. \
             Identify the packer or protector, recover the unpacked payload, and determine \
             what the unpacked code actually does — the packed view may misrepresent the \
             sample's real capabilities.\n{}",
            evidence_line(cited),
        ),
        // the gate's checks mirror the two halves of the question: name the
        // protection, then surface what it hides -- and the third makes an
        // unproven wall claim unable to close the question (an evaluation diagnosis:
        // a spoofed VMProtect wall was accepted with no entry-stub read and
        // no library-decoder trial while a working static unpacker existed)
        tasks: vec![
            "Identify the packer or protection and post the supporting evidence".to_string(),
            "Recover the hidden payload and post its identity as evidence -- an embedded \
             PE you have identified in place is NOT recovered until it is carved \
             (decode/save_bytes) and imported (import_child); characterization alone \
             counts only for a layer proven unreachable under the wall-proof task below; \
             for a terminal payload under ~128 KB (or any fully-decompilable managed \
             payload), complete a function census before closing (every function \
             decompiled, every recovered string/constant xref'd, strings re-swept at \
             min length 3)"
                .to_string(),
            "If any layer seems statically unreachable, post the proof of the wall before \
             concluding: the entry-stub disassembly you read, a search_scripts pass for \
             decoders matching the suspected protection with each match's run output, and \
             your trial decodes — an untested wall claim does not close this question"
                .to_string(),
        ],
        origin: "derived",
    })
}

/// The most prior-toolkit scripts listed in the seeded lead's body.
const MAX_TOOLKIT_SCRIPTS: usize = 6;

/// Derive the prior-toolkit lead when library scripts reference this sample.
///
/// A script whose description, source, or docs embed the sample's SHA-256 (or
/// its 16-hex stem, the convention session artifacts name programs with) was
/// written by a prior analysis of this exact binary — usually a working
/// decoder, unpacker, or carver with the sample's own keys and offsets baked
/// in. Surfacing those as an open question makes reuse the default instead of
/// an unprompted library search: evaluation families re-derived or abandoned
/// work the library already held for the same bytes.
///
/// # Arguments
///
/// * `matches` - The matching scripts as (rendered name, rendered description)
pub(crate) fn prior_toolkit_question(matches: &[(String, String)]) -> Option<SeedQuestion> {
    // no matching prior work means no lead
    if matches.is_empty() {
        return None;
    }
    // list a bounded cut of the matched scripts, descriptions where present
    let listed: Vec<String> = matches
        .iter()
        .take(MAX_TOOLKIT_SCRIPTS)
        .map(|(name, desc)| {
            if desc.is_empty() {
                format!("- `{name}`")
            } else {
                format!("- `{name}` — {desc}")
            }
        })
        .collect();
    let mut body = format!(
        "The persistent script library holds {} script(s) whose text references this \
         binary's SHA-256: a prior analysis of this exact sample produced working tooling \
         (decoders, unpackers, carvers), likely with this binary's own keys and offsets \
         baked in. Review and reuse it before building anything from scratch — and before \
         declaring any layer unreachable:\n{}",
        matches.len(),
        listed.join("\n"),
    );
    // note when the list was truncated so the reader knows there is more
    if matches.len() > MAX_TOOLKIT_SCRIPTS {
        body.push_str("\n(more matches exist: search_scripts for this sample's SHA-256)");
    }
    Some(SeedQuestion {
        name: "q-prior-toolkit".to_string(),
        title: "Prior-session tooling exists for this exact binary — review and reuse it"
            .to_string(),
        body,
        // the gate's checks force the review and the validated reuse
        tasks: vec![
            "Read each listed script (and its docs) and post what it recovers for this \
             binary"
                .to_string(),
            "Run the applicable scripts, validate their outputs against the binary, and \
             post the recovered artifacts as evidence"
                .to_string(),
        ],
        origin: "prior_toolkit",
    })
}

/// Derive one question per decompiler-confirmed host-identifier gate.
///
/// A comparison against a recovered MAC/GUID/serial/name that steers a branch is
/// the single highest-value lead a detector can hand the agent: it decides
/// whether the payload runs at all and may tie the sample to a specific victim.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn gate_questions(buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>) -> Vec<SeedQuestion> {
    let mut questions = Vec::new();
    for (key, hit) in bucket_hit_pairs(buckets) {
        // only decompiler-confirmed comparisons that steer control flow qualify
        if hit.kind != EvidenceKind::ControlFlowGated {
            continue;
        }
        // only hits that recovered the concrete host identifier are actionable
        let Some(artifact) = &hit.artifact else {
            continue;
        };
        if artifact.class != ArtifactClass::HostIdentifier {
            continue;
        }
        // name the question after the gate's site so multiple gates stay distinct
        let site = fmt_addr(hit.address);
        questions.push(SeedQuestion {
            name: format!("q-host-gate-{:08x}", hit.address),
            title: format!("What does the host-identity check at {site} gate?"),
            body: format!(
                "The detectors observed a comparison against the host identifier \
                 `{}` steering control flow at {site}. Determine which execution path \
                 this check guards, what the sample does when the check fails, and \
                 whether the value ties the sample to a specific victim environment.\n{}",
                artifact.value,
                evidence_line([key]),
            ),
            // the gate's checks pin the concrete site so the delegated agent
            // must ground both halves of the answer in the decompiled code
            tasks: vec![
                format!(
                    "Decompile the function containing the check at {site} and post the \
                     gated comparison as evidence"
                ),
                format!(
                    "Determine what executes when the check at {site} fails and cite the \
                     decompiled path"
                ),
            ],
            origin: "derived",
        });
        // a couple of gates are plenty; the statement posts carry the rest
        if questions.len() == MAX_GATE_QUESTIONS {
            break;
        }
    }
    questions
}

/// Derive the decoded-data verification question when any decode pass recovered
/// plaintext or structured config.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn decoded_config_question(
    buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>,
) -> Option<SeedQuestion> {
    // cite every bucket that carries recovered plaintext or a parsed config record
    let cited: Vec<&BucketKey> = bucket_hit_pairs(buckets)
        .filter(|(_, hit)| {
            matches!(
                hit.artifact.as_ref().map(|artifact| artifact.class),
                Some(ArtifactClass::DecodedData | ArtifactClass::ConfigRecord)
            )
        })
        .map(|(key, _)| key)
        .collect();
    // no recovered plaintext means nothing to verify
    if cited.is_empty() {
        return None;
    }
    Some(SeedQuestion {
        name: "q-decoded-config".to_string(),
        title: "How is the recovered decoded data used at runtime?".to_string(),
        body: format!(
            "The detectors recovered decoded strings and/or configuration records \
             from this binary (statically, or by budgeted emulation of the sample's \
             own decode routines — each finding's recovery route is in its body). \
             Verify the recovered plaintext is real rather than a coincidental \
             decode, then determine how the decoded values are used at runtime — \
             which functions consume them and what behavior they configure.\n{}",
            evidence_line(cited),
        ),
        // the gate's checks mirror the question: prove the decode, then its use
        tasks: vec![
            "Verify each decoded value by an independent route (a raw-byte decode, or \
             reproducing the emulation that recovered it) and post the verification \
             as evidence"
                .to_string(),
            "Identify the function(s) that consume the decoded data and post them as \
             evidence"
                .to_string(),
        ],
        origin: "derived",
    })
}

/// Derive the key-usage question when embedded key material was recovered.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn crypto_key_question(buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // cite every bucket that recovered symmetric or private key material
    let cited: Vec<&BucketKey> = bucket_hit_pairs(buckets)
        .filter(|(_, hit)| {
            matches!(
                hit.artifact.as_ref().map(|artifact| artifact.class),
                Some(ArtifactClass::SymmetricKey | ArtifactClass::AsymmetricPrivateKey)
            )
        })
        .map(|(key, _)| key)
        .collect();
    // no recovered keys means nothing to chase
    if cited.is_empty() {
        return None;
    }
    Some(SeedQuestion {
        name: "q-crypto-keys".to_string(),
        title: "What data do the recovered cryptographic keys protect?".to_string(),
        body: format!(
            "The detectors recovered embedded cryptographic key material from this \
             binary. Determine what the keys encrypt or decrypt — C2 traffic, an \
             embedded configuration, staged payloads, or victim files — and which \
             functions perform that crypto.\n{}",
            evidence_line(cited),
        ),
        // no seeded checks in v1: a wrong gate here is worse than no gate
        tasks: Vec::new(),
        origin: "derived",
    })
}

/// Derive the C2-characterization question when network endpoints were recovered.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn c2_question(buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // gather the distinct endpoint values and the buckets backing them
    let mut endpoints: Vec<&str> = Vec::new();
    let mut cited: Vec<&BucketKey> = Vec::new();
    for (key, hit) in bucket_hit_pairs(buckets) {
        // only hits that recovered a concrete endpoint value qualify
        let Some(artifact) = &hit.artifact else {
            continue;
        };
        if artifact.class != ArtifactClass::NetworkEndpoint {
            continue;
        }
        // dedupe the quoted values while keeping the strongest-first order
        if !endpoints.contains(&artifact.value.as_str()) {
            endpoints.push(artifact.value.as_str());
        }
        cited.push(key);
    }
    // no recovered endpoints means no channel to characterize
    if endpoints.is_empty() {
        return None;
    }
    // quote a bounded sample of the recovered endpoints in the body
    let shown: Vec<String> = endpoints
        .iter()
        .take(MAX_QUESTION_ENDPOINTS)
        .map(|endpoint| format!("`{endpoint}`"))
        .collect();
    let suffix = if endpoints.len() > MAX_QUESTION_ENDPOINTS {
        ", …"
    } else {
        ""
    };
    Some(SeedQuestion {
        name: "q-c2-channel".to_string(),
        title: "Characterize the C2 channel behind the recovered endpoints".to_string(),
        body: format!(
            "The detectors recovered network endpoints from this binary: {}{suffix}. \
             Characterize the command-and-control channel: how an endpoint is \
             selected, what protocol is spoken over it, and what data is sent or \
             received.\n{}",
            shown.join(", "),
            evidence_line(cited),
        ),
        // no seeded checks in v1: a wrong gate here is worse than no gate
        tasks: Vec::new(),
        origin: "derived",
    })
}

/// Derive the ransomware-verdict question when impact findings corroborate.
///
/// The ransomware-behavior detector is intentionally weak alone, so this only
/// asks for a verdict when impact (T1486) findings land in at least two distinct
/// buckets — e.g. crypto capability in one function and drive enumeration in
/// another.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn ransomware_question(buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // cite every bucket carrying an impact (data-encrypted-for-impact) finding
    let cited: Vec<&BucketKey> = bucket_hit_pairs(buckets)
        .filter(|(_, hit)| hit.technique.id.as_ref() == "T1486")
        .map(|(key, _)| key)
        .collect();
    // require corroboration across buckets before raising the heavyweight claim
    let distinct: std::collections::BTreeSet<&BucketKey> = cited.iter().copied().collect();
    if distinct.len() < 2 {
        return None;
    }
    Some(SeedQuestion {
        name: "q-ransomware".to_string(),
        title: "Confirm or refute ransomware behavior".to_string(),
        body: format!(
            "The detectors found both cryptographic capability and file/drive \
             enumeration consistent with ransomware (T1486). Confirm or refute: does \
             the file enumeration feed the encryption, is there a ransom note or an \
             extension rename, and which directories or file types are targeted?\n{}",
            evidence_line(cited),
        ),
        // no seeded checks in v1: a wrong gate here is worse than no gate
        tasks: Vec::new(),
        origin: "derived",
    })
}

/// Derive the signature-tamper question from a strong signed-digest mismatch.
///
/// The signature-anomaly detection emits at most one hit per binary, so one
/// strong (at least `Likely`) T1553.002 finding is the whole class: the code
/// signature fails to cover the file's on-disk bytes, and what the signed
/// digest misses is exactly where post-signing tampering lives.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn signature_question(buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // one strong signed-digest mismatch is the whole class (one hit per binary)
    let (key, _) = bucket_hit_pairs(buckets).find(|(_, hit)| {
        hit.technique.id.as_ref() == "T1553.002"
            && confidence_rank(hit.confidence) >= confidence_rank(Confidence::Likely)
    })?;
    Some(SeedQuestion {
        name: "q-signature".to_string(),
        title: "The code signature does not cover the file as it exists on disk — what was \
                added or changed after signing?"
            .to_string(),
        body: format!(
            "The detectors found a strong signed-digest mismatch (T1553.002): the file's \
             code signature does not cover its current on-disk bytes, so content was \
             added or changed after signing. Find the uncovered bytes and determine what \
             they carry and why they were introduced.\n{}",
            evidence_line([key]),
        ),
        // the single gate: the uncovered bytes must be identified with byte evidence
        tasks: vec![
            "Identify the bytes the signed digest fails to cover — tampered region, \
             grafted table, or appended data — and post byte evidence of what they \
             contain."
                .to_string(),
        ],
        origin: "derived",
    })
}

/// Derive the family-verification question from a corroborated YARA cluster.
///
/// Corroborating YARA rules naming one family (see [`corroborated_family`])
/// are a strong identity claim, but family rules also fire on shared code and
/// packer stubs — so this routes one delegation at verifying the implied
/// behaviors instead of letting the claim stand untested. At most one question
/// seeds per sample, on the first (strongest-ranked) cluster lead.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn yara_identity_question(
    buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>,
) -> Option<SeedQuestion> {
    // the first corroborated cluster lead names the family this question verifies
    let (key, family) = bucket_hit_pairs(buckets)
        .find_map(|(key, hit)| corroborated_family(hit).map(|family| (key, family)))?;
    Some(SeedQuestion {
        name: "q-yara-family".to_string(),
        title: format!(
            "Signatures identify this as {family} — verify the implied behaviors against \
             the binary"
        ),
        body: format!(
            "Corroborating YARA rules identify this sample as the `{family}` family. \
             Verify the identification against the code itself: tie the family's implied \
             behaviors (its C2 style, persistence, payload handling) to concrete \
             functions and data in this binary, or show the matches land in shared or \
             packer code the rules misread.\n{}",
            evidence_line([key]),
        ),
        // the single gate: the family claim must be confirmed or refuted in code
        tasks: vec![format!(
            "Confirm or refute the {family} identification with code-level evidence — \
             cite the functions, strings, or configuration that match or contradict the \
             family's known behaviors"
        )],
        origin: "derived",
    })
}

/// Derive the section-map trace question from a native-API section pair.
///
/// A T1106 hit whose content names both `NtCreateSection` and
/// `NtMapViewOfSection` (the native-API detection renders the pair into one
/// grouped hit — a stable content contract) is the classic section-map path
/// for manual mapping and self-injection.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn section_map_question(buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // cite every bucket whose T1106 content carries the section-map pair
    let cited: Vec<&BucketKey> = bucket_hit_pairs(buckets)
        .filter(|(_, hit)| {
            hit.technique.id.as_ref() == "T1106"
                && hit.content.contains("NtCreateSection")
                && hit.content.contains("NtMapViewOfSection")
        })
        .map(|(key, _)| key)
        .collect();
    // no paired section APIs means no mapping path to trace
    if cited.is_empty() {
        return None;
    }
    Some(SeedQuestion {
        name: "q-section-map".to_string(),
        title: "A section-map execution path (NtCreateSection+NtMapViewOfSection) is \
                present — trace what gets mapped and executed"
            .to_string(),
        body: format!(
            "The detectors found NtCreateSection and NtMapViewOfSection together — the \
             section-map path used for manual mapping and self-injection. Trace the \
             pair: what bytes fill the section, where the view is mapped, and whether \
             execution transfers into it.\n{}",
            evidence_line(cited),
        ),
        // no seeded checks in v1: a wrong gate here is worse than no gate
        tasks: Vec::new(),
        origin: "derived",
    })
}

/// Derive the evasion-gauntlet question from verified anti-analysis checks.
///
/// Gates on verified use, not raw confidence: a qualifying hit either carries
/// [`CallContext::GatesBranch`] (the validation pass proved its result steers
/// a branch) or is `Likely`-or-better while carrying a class-validated
/// artifact. Presence-only confident claims no longer qualify — that tier is
/// where the disputes live.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by bucket key
fn evasion_question(buckets: &BTreeMap<BucketKey, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // cite every bucket with a verified anti-debug or anti-sandbox finding
    let cited: Vec<&BucketKey> = bucket_hit_pairs(buckets)
        .filter(|(_, hit)| {
            hit.technique.id.as_ref() == "T1622" || hit.technique.id.as_ref().starts_with("T1497")
        })
        .filter(|(_, hit)| {
            matches!(hit.context, Some(CallContext::GatesBranch))
                || (confidence_rank(hit.confidence) >= confidence_rank(Confidence::Likely)
                    && artifact_is_validated(hit))
        })
        .map(|(key, _)| key)
        .collect();
    // unverified or absent evasion evidence seeds no question
    if cited.is_empty() {
        return None;
    }
    Some(SeedQuestion {
        name: "q-evasion-gauntlet".to_string(),
        title: "Map the anti-analysis gauntlet".to_string(),
        body: format!(
            "The detectors found verified anti-debug and/or anti-sandbox checks. Map \
             the gauntlet: which checks gate the real payload, where does the clean \
             (non-analysis) execution path diverge, and what does the sample do once \
             it believes it is unobserved?\n{}",
            evidence_line(cited),
        ),
        // no seeded checks in v1: a wrong gate here is worse than no gate
        tasks: Vec::new(),
        origin: "derived",
    })
}

/// Seed an already-merged list of follow-up questions onto the forum.
///
/// Best-effort like the rest of seeding: one failed question is logged and
/// skipped rather than sinking the remainder. Also the single seeding path for
/// a question's completion-gate check tasks (capped per question), so every
/// seeded question — derived, script-emitted, or the pre-Ghidra container
/// question — attaches its checklist the same way.
///
/// # Arguments
///
/// * `forum` - The instance's forum state to seed the questions onto
/// * `questions` - The merged, capped questions to seed (see [`merge_questions`])
/// * `scope` - The sample's post-name namespace, or `None` for the main pass
#[tracing::instrument(skip_all)]
pub(crate) async fn seed_question_posts(
    forum: &ForumState,
    questions: Vec<SeedQuestion>,
    scope: Option<&SampleScope>,
) {
    // seed each question in its merged priority order
    for question in questions {
        // split the question up front so the task list survives the post
        // fields moving into the seed call below
        let SeedQuestion {
            name,
            title,
            body,
            mut tasks,
            origin,
        } = question;
        // namespace the fixed question names under the sample's scope so a
        // sibling's q-* posts cannot collide with the main sample's
        let name = scoped_name(scope, &name);
        // every derived question starts open so the question loop drives it
        if let Err(error) = forum
            .seed_detector_post(
                SEED_AUTHOR.to_string(),
                name.clone(),
                scoped_title(scope, title),
                body,
                Kind::Question(QuestionStatus::Unsolved),
                "question",
                Some(origin.to_owned()),
                Vec::new(),
            )
            .await
        {
            // one bad question must not sink the rest of the seeding
            tracing::warn!(
                name = %name,
                ?error,
                "failed to seed a derived question; skipping"
            );
            // a failed post has nothing to hang tasks under
            continue;
        }
        // most questions carry no check tasks; nothing more to attach then
        if tasks.is_empty() {
            continue;
        }
        // every seeded task is a hard solved-gate, so enforce the cap here too
        tasks.truncate(MAX_SEED_TASKS_PER_QUESTION);
        // attach the completion-gate checklist under the freshly seeded question
        if let Err(error) = forum
            .seed_tasks(
                SEED_AUTHOR.to_string(),
                ParentRef::Post(name.clone()),
                tasks,
            )
            .await
        {
            // one bad task list must not sink the rest of the seeding
            tracing::warn!(
                name = %name,
                ?error,
                "failed to seed a question's check tasks; skipping"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::thorpal::detect::detections::command_and_control::T1071;
    use crate::thorpal::detect::detections::defense_evasion::{
        T1027_002, T1480_001, T1553_002, T1622,
    };
    use crate::thorpal::detect::detections::execution::T1106;
    use crate::thorpal::detect::detections::impact::T1486;
    use crate::thorpal::detect::detections::{
        AttackTechnique, HitArtifact, HitSource, high, low, medium,
    };

    /// Builds a bare context-only test hit in the given bucket.
    ///
    /// # Arguments
    ///
    /// * `technique` - The ATT&CK technique the hit evidences
    /// * `function` - The containing function entry (`None` = file-level)
    /// * `severity` - The `(confidence, suspicion)` pair
    fn hit(
        technique: AttackTechnique,
        function: Option<u64>,
        severity: (Confidence, i64),
    ) -> DetectionHit {
        DetectionHit {
            technique,
            // place the evidence site a little inside the containing function
            address: function.unwrap_or(0x1000) + 0x10,
            function,
            kind: EvidenceKind::ContextOnly,
            confidence: severity.0,
            suspicion: severity.1,
            content: "test content".to_string(),
            name: None,
            reasoning: "test reasoning".to_string(),
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
        }
    }

    /// The seeded finding lines keep their exact shapes: thorctl's telemetry
    /// fold parses `Site: ... | confidence: ... | suspicion: ...` and
    /// `Sites (n):` out of legacy stores, so rewording them breaks it
    #[test]
    fn seeded_finding_lines_keep_their_parsed_shapes() {
        let mut strong = hit(T1486, Some(0x401000), high());
        strong.sites = vec![0x401010, 0x401020];
        let body = comment_body(&strong);
        // the single finding line: Site | confidence | suspicion
        let site_line = body
            .lines()
            .find(|line| line.starts_with("Site: "))
            .expect("a Site line");
        let shape = regex::Regex::new(
            r"^Site: \S+ \| confidence: (fact|likely|unsure|untrusted) \| suspicion: -?\d+$",
        )
        .expect("a valid shape regex");
        assert!(shape.is_match(site_line), "got {site_line:?}");
        // the backing-sites line: Sites (n):
        assert!(
            body.lines().any(|line| line.starts_with("Sites (2):")),
            "got {body:?}"
        );
    }

    /// Seeded buckets and questions record their structured provenance
    #[tokio::test]
    async fn seeding_records_structured_provenance() {
        let forum = ForumState::new();
        // one strong finding carrying sites, an artifact, and a producer name
        let mut strong = hit(T1486, Some(0x401000), high());
        strong.sites = vec![0x401010, 0x401020, 0x401030];
        strong.detection = Some("ransomware_behavior".to_owned());
        strong.artifact = Some(HitArtifact {
            class: ArtifactClass::SymmetricKey,
            value: "SECRET-VALUE-NEVER-RECORDED".to_owned(),
            role: None,
            detail: None,
        });
        // seed the bucket: one post plus one comment per finding
        seed_bucket(
            None,
            &forum,
            &BucketKey::Function(0x401000),
            &[strong.clone()],
            None,
        )
        .await
        .expect("the bucket should seed");
        // and one derived question with its origin
        seed_question_posts(
            &forum,
            vec![SeedQuestion {
                name: "q-unpack".to_owned(),
                title: "Can the payload be unpacked?".to_owned(),
                body: "See #fn-00401000.".to_owned(),
                tasks: Vec::new(),
                origin: "derived",
            }],
            None,
        )
        .await;
        // the snapshot carries structured records for every seeded node
        let snapshot = forum.snapshot().await;
        assert_eq!(snapshot.detector_provenance.len(), 3);
        // the bucket post's record fronts the finding's structured facts
        let post = snapshot
            .detector_provenance
            .iter()
            .find(|record| record.node == ParentRef::Post("fn-00401000".to_owned()))
            .expect("the bucket post's record");
        assert_eq!(post.shape, "bucket");
        assert_eq!(post.findings.len(), 1);
        let finding = &post.findings[0];
        assert_eq!(finding.technique, T1486.id);
        assert_eq!(finding.confidence, confidence_label(strong.confidence));
        assert_eq!(finding.suspicion, strong.suspicion);
        assert_eq!(finding.source, "native");
        assert_eq!(finding.detection.as_deref(), Some("ransomware_behavior"));
        assert_eq!(finding.sites, 3);
        // only the artifact's class key is recorded, never its value (the
        // value still lives in the comment BODY the analyst reads -- the
        // structured records themselves must never duplicate it)
        assert_eq!(finding.artifact_class.as_deref(), Some("symmetric_key"));
        let records = serde_json::to_string(&snapshot.detector_provenance)
            .expect("provenance should serialize");
        assert!(!records.contains("SECRET-VALUE-NEVER-RECORDED"));
        // the finding comment carries the same record under its own id
        let comment = snapshot
            .detector_provenance
            .iter()
            .find(|record| matches!(record.node, ParentRef::Comment(_)))
            .expect("the finding comment's record");
        assert_eq!(comment.shape, "finding");
        assert_eq!(comment.findings.len(), 1);
        // the question records its origin
        let question = snapshot
            .detector_provenance
            .iter()
            .find(|record| record.node == ParentRef::Post("q-unpack".to_owned()))
            .expect("the question's record");
        assert_eq!(question.shape, "question");
        assert_eq!(question.question_origin.as_deref(), Some("derived"));
    }

    /// Pure runtime-noise buckets suppress; signal, validated artifacts, and
    /// file-level buckets always stay.
    #[test]
    fn partition_suppressed_keeps_signal() {
        // a bucket of runtime-located noise, a strong bucket, a file-level
        // bucket, and a weak bucket shielded by a validated pivot artifact
        let mut noise = hit(T1622, Some(0x1000), low());
        noise.context = Some(CallContext::Runtime("rt".to_owned()));
        let mut unsure_runtime = hit(T1622, Some(0x1000), medium());
        unsure_runtime.context = Some(CallContext::Runtime("rt".to_owned()));
        let strong = hit(T1486, Some(0x2000), high());
        let file_level = hit(T1027_002, None, low());
        let mut shielded = hit(T1071, Some(0x3000), low());
        shielded.artifact = Some(HitArtifact::new(
            ArtifactClass::NetworkEndpoint,
            "https://evil.example/gate.php",
        ));
        let input = buckets(vec![noise, unsure_runtime, strong, file_level, shielded]);
        let (kept, suppressed) = partition_suppressed(Some(Toolchain::Rust), input.clone());
        // only the pure-noise bucket moved to the rollup
        assert_eq!(suppressed.len(), 2);
        assert!(!kept.contains_key(&BucketKey::Function(0x1000)));
        // the strong, file-level, and artifact-shielded buckets all stayed
        assert!(kept.contains_key(&BucketKey::Function(0x2000)));
        assert!(kept.contains_key(&BucketKey::FileLevel(None)));
        assert!(kept.contains_key(&BucketKey::Function(0x3000)));
        // without a toolchain the mixed-grade runtime bucket stays too: that
        // noise is attributable only to a fingerprinted runtime
        let (kept, suppressed) = partition_suppressed(None, input);
        assert!(suppressed.is_empty());
        assert_eq!(kept.len(), 4);
    }

    /// All-Untrusted, artifact-less buckets suppress with no toolchain; one
    /// stronger hit keeps its bucket and file-level buckets stay exempt.
    #[test]
    fn untrusted_artifactless_buckets_suppress_without_a_toolchain() {
        // a weak artifact-less bucket, a mixed-grade bucket, and a weak
        // file-level hit
        let weak_one = hit(T1622, Some(0x1000), low());
        let weak_two = hit(T1071, Some(0x1000), low());
        let mixed_weak = hit(T1622, Some(0x2000), low());
        let mixed_unsure = hit(T1622, Some(0x2000), medium());
        let file_level = hit(T1027_002, None, low());
        let input = buckets(vec![
            weak_one,
            weak_two,
            mixed_weak,
            mixed_unsure,
            file_level,
        ]);
        let (kept, suppressed) = partition_suppressed(None, input);
        // the all-Untrusted artifact-less bucket rolled up without a fingerprint
        assert_eq!(suppressed.len(), 2);
        assert!(!kept.contains_key(&BucketKey::Function(0x1000)));
        // the Unsure hit keeps its bucket, and file-level stays exempt
        assert!(kept.contains_key(&BucketKey::Function(0x2000)));
        assert!(kept.contains_key(&BucketKey::FileLevel(None)));
    }

    /// A junk pivot artifact no longer vetoes suppression; a valid GUID does.
    #[test]
    fn junk_artifacts_no_longer_veto_suppression() {
        // one weak bucket carrying a carved repeating-pattern MAC, one weak
        // bucket carrying a real author-chosen GUID
        let mut junk = hit(T1480_001, Some(0x1000), low());
        junk.artifact = Some(HitArtifact::new(
            ArtifactClass::HostIdentifier,
            "a2:c2:a2:c2:a2:c2",
        ));
        let mut guid = hit(T1480_001, Some(0x2000), low());
        guid.artifact = Some(HitArtifact::new(
            ArtifactClass::HostIdentifier,
            "AC634028-9BF2-4EC5-9D3B-2A5C8D1E7F10",
        ));
        let input = buckets(vec![junk.clone(), guid]);
        // under a fingerprint the junk value earns no shield; the GUID does
        let (kept, suppressed) = partition_suppressed(Some(Toolchain::Msvc), input);
        assert_eq!(suppressed.len(), 1);
        assert!(!kept.contains_key(&BucketKey::Function(0x1000)));
        assert!(kept.contains_key(&BucketKey::Function(0x2000)));
        // the junk-shielded weak bucket suppresses without a fingerprint too
        let (kept, suppressed) = partition_suppressed(None, buckets(vec![junk]));
        assert_eq!(suppressed.len(), 1);
        assert!(kept.is_empty());
    }

    /// Catalog-asserted inherent noise rolls up regardless of toolchain.
    #[test]
    fn always_noise_hits_suppress_without_a_toolchain() {
        // an Unsure hit the producing catalog marked as inherent noise
        let mut inherent = hit(T1622, Some(0x1000), medium());
        inherent.always_noise = true;
        let (kept, suppressed) = partition_suppressed(None, buckets(vec![inherent]));
        // the self-asserted noise rolled up with no fingerprint at all
        assert_eq!(suppressed.len(), 1);
        assert!(kept.is_empty());
    }

    /// The budget keeps each detection's strongest K; the overflow is returned
    /// for that detection's census, still ranked.
    #[test]
    fn admission_budget_keeps_the_strongest_and_overflows_to_census() {
        // thirteen rankable hits for one default-budget detection
        let mut all = Vec::new();
        for index in 0u64..13 {
            // suspicion descends with the index so the ranking is unambiguous
            let suspicion = 100 - i64::try_from(index).expect("a small index");
            let mut weak = hit(T1622, Some(0x1000), (Confidence::Unsure, suspicion));
            weak.detection = Some("debugger_evasion".to_owned());
            all.push(weak);
        }
        let (kept, census) = apply_admission_budget(all);
        // the top ten by rank stayed finding-shape
        assert_eq!(kept.len(), DETECTOR_FINDING_BUDGET);
        assert!(kept.iter().all(|hit| hit.suspicion >= 91));
        // the overflow landed under the detection's census, still ranked
        let overflow = census.get("debugger_evasion").expect("a census entry");
        assert_eq!(overflow.len(), 3);
        assert_eq!(
            overflow.iter().map(|hit| hit.suspicion).collect::<Vec<_>>(),
            vec![90, 89, 88]
        );
    }

    /// Verified gates and validated artifacts bypass the budget entirely.
    #[test]
    fn verified_and_validated_hits_bypass_the_budget() {
        // twelve budget-competing hits, one verified gate, one validated artifact
        let mut all = Vec::new();
        for _ in 0..12 {
            let mut weak = hit(T1622, Some(0x1000), medium());
            weak.detection = Some("debugger_evasion".to_owned());
            all.push(weak);
        }
        let mut gated = hit(T1622, Some(0x9000), low());
        gated.detection = Some("debugger_evasion".to_owned());
        gated.context = Some(CallContext::GatesBranch);
        all.push(gated);
        let mut recovered = hit(T1071, Some(0xa000), low());
        recovered.detection = Some("debugger_evasion".to_owned());
        recovered.artifact = Some(HitArtifact::new(
            ArtifactClass::NetworkEndpoint,
            "10.2.3.4:8443",
        ));
        all.push(recovered);
        let (kept, census) = apply_admission_budget(all);
        // both bypass hits seeded on top of the full budget
        assert_eq!(kept.len(), DETECTOR_FINDING_BUDGET + 2);
        assert!(
            kept.iter()
                .any(|hit| matches!(hit.context, Some(CallContext::GatesBranch)))
        );
        assert!(kept.iter().any(|hit| hit.artifact.is_some()));
        // exactly the two over-budget presence hits were held back
        let overflow = census.get("debugger_evasion").expect("a census entry");
        assert_eq!(overflow.len(), 2);
    }

    /// The override table raises the flood-prone string detectors' budgets.
    #[test]
    fn budget_overrides_apply_per_detection() {
        // the two overridden detections rank above the shared default
        assert_eq!(detection_budget("network_comms"), 15);
        assert_eq!(detection_budget("encoded_strings"), 15);
        assert_eq!(
            detection_budget("debugger_evasion"),
            DETECTOR_FINDING_BUDGET
        );
    }

    /// The census post itemizes the overflow with name, counts, and provenance.
    #[tokio::test]
    async fn census_post_itemizes_the_held_back_findings() {
        let forum = ForumState::new();
        // more overflow than the body's line cap to exercise the counted tail
        let overflow: Vec<DetectionHit> = (0..MAX_ROLLUP_LINES + 3)
            .map(|_| {
                let mut weak = hit(T1622, Some(0x1000), low());
                weak.detection = Some("debugger_evasion".to_owned());
                weak
            })
            .collect();
        seed_census_post(&forum, "debugger_evasion", &overflow, None)
            .await
            .expect("the census should seed");
        // the fixed per-detection name landed with the budget-framed title
        let view = forum.view().await;
        let thread = view
            .threads
            .iter()
            .find(|thread| thread.post.name == "census-debugger_evasion")
            .expect("the census post should exist");
        assert_eq!(
            thread.post.title,
            format!(
                "{} low-priority debugger_evasion findings held back (admission budget)",
                MAX_ROLLUP_LINES + 3
            )
        );
        // the body itemizes to the cap, counts the tail, and invites re-raising
        assert_eq!(
            thread.post.body.matches("- T1622 @").count(),
            MAX_ROLLUP_LINES
        );
        assert!(thread.post.body.contains("and 3 more"));
        assert!(thread.post.body.contains("re-raise any entry"));
        assert!(thread.post.body.contains("Nothing was discarded"));
        // the structured records carry every held-back finding as demoted
        let snapshot = forum.snapshot().await;
        let record = snapshot
            .detector_provenance
            .iter()
            .find(|record| record.node == ParentRef::Post("census-debugger_evasion".to_owned()))
            .expect("the census post's record");
        assert_eq!(record.shape, "rollup");
        assert_eq!(record.findings.len(), MAX_ROLLUP_LINES + 3);
        assert!(record.findings.iter().all(|finding| finding.suppressed));
    }

    /// The rollup wording matches its criterion: a fingerprinted toolchain's
    /// vocabulary (never a static-link claim), or plain weak-grade matches.
    #[tokio::test]
    async fn rollup_wording_matches_the_toolchain_split() {
        // one runtime-located hit and one weak-grade hit, as the rollup gets them
        let mut located = hit(T1622, Some(0x1000), low());
        located.context = Some(CallContext::Runtime("rt".to_owned()));
        let weak = hit(T1071, Some(0x2000), low());
        // the fingerprinted case names the toolchain's vocabulary and splits counts
        let forum = ForumState::new();
        seed_suppressed_rollup(&forum, Some(Toolchain::Mfc), &[located, weak.clone()], None)
            .await
            .expect("the rollup should seed");
        let view = forum.view().await;
        let thread = view
            .threads
            .iter()
            .find(|thread| thread.post.name == "runtime-noise")
            .expect("the rollup post should exist");
        assert!(thread.post.title.contains("recognized MFC binary"));
        assert!(thread.post.body.contains(
            "carries the vocabulary of a recognized MFC toolchain or runtime \
             (statically linked or imported)"
        ));
        assert!(
            thread
                .post
                .body
                .contains("1 findings sit inside recognized runtime/support code")
        );
        assert!(thread.post.body.contains("1 are weak-grade matches"));
        assert!(!thread.post.body.contains("statically links"));
        assert!(thread.post.body.contains("Re-raise any entry below"));
        // the no-fingerprint case claims only weak grade, never a runtime
        let bare = ForumState::new();
        seed_suppressed_rollup(&bare, None, &[weak], None)
            .await
            .expect("the rollup should seed");
        let view = bare.view().await;
        let thread = view
            .threads
            .iter()
            .find(|thread| thread.post.name == "runtime-noise")
            .expect("the rollup post should exist");
        assert!(thread.post.title.contains("(weak-grade matches)"));
        assert!(
            thread
                .post
                .body
                .contains("weak-grade matches on APIs and byte patterns")
        );
        assert!(!thread.post.body.contains("recognized"));
        assert!(!thread.post.body.contains("runtime"));
        assert!(thread.post.body.contains("Re-raise any entry below"));
    }

    /// File-level buckets split per producing detection, with the fixed name
    /// kept when exactly one detection produced file-level hits.
    #[test]
    fn file_level_buckets_split_per_detection() {
        // two detections with file-level hits split into named buckets
        let mut packing = hit(T1027_002, None, medium());
        packing.detection = Some("packing".to_owned());
        let mut family = hit(T1486, None, medium());
        family.detection = Some("yara".to_owned());
        let split = bucket_hits(vec![packing.clone(), family]);
        let names: Vec<String> = split.keys().map(bucket_name).collect();
        assert_eq!(names, vec!["file-level-packing", "file-level-yara"]);
        // a lone file-level detection keeps the fixed historical name
        let lone = bucket_hits(vec![packing]);
        let names: Vec<String> = lone.keys().map(bucket_name).collect();
        assert_eq!(names, vec!["file-level"]);
    }

    /// A split file-level post names its detection in name and title.
    #[tokio::test]
    async fn split_file_level_posts_name_their_detection() {
        let forum = ForumState::new();
        // one file-level packing hit seeded under a split (per-detection) key
        let mut flagged = hit(T1027_002, None, medium());
        flagged.detection = Some("packing".to_owned());
        seed_bucket(
            None,
            &forum,
            &BucketKey::FileLevel(Some("packing".to_owned())),
            &[flagged],
            None,
        )
        .await
        .expect("the bucket should seed");
        // the post name carries the detection and the title stays readable
        let view = forum.view().await;
        let thread = view
            .threads
            .iter()
            .find(|thread| thread.post.name == "file-level-packing")
            .expect("the split post should exist");
        assert!(thread.post.title.contains("File-level (packing)"));
    }

    /// Question evidence lines cite the actual split file-level post names.
    #[test]
    fn questions_cite_split_file_level_post_names() {
        // two detections' file-level hits force the split naming
        let mut packing = hit(T1027_002, None, medium());
        packing.detection = Some("packing".to_owned());
        let mut other = hit(T1486, None, medium());
        other.detection = Some("yara".to_owned());
        let buckets = buckets(vec![packing, other]);
        let questions = derive_questions(&buckets);
        // the unpack question cites the packing detection's own post
        let unpack = questions
            .iter()
            .find(|question| question.name == "q-unpack")
            .expect("the unpack question");
        assert!(unpack.body.ends_with("Evidence: see #file-level-packing"));
    }

    /// The runtime note renders into a seeded comment body when context is set.
    #[test]
    fn comment_body_renders_the_runtime_note() {
        // a classified hit carries the runtime context sentence
        let mut classified = hit(T1622, Some(0x1000), medium());
        classified.context = Some(CallContext::Runtime("core::fmt, Rust runtime".to_owned()));
        let body = comment_body(&classified);
        assert!(body.contains("compiler-generated runtime code"));
        assert!(body.contains("core::fmt, Rust runtime"));
        // an unclassified hit carries no context sentence
        let plain = hit(T1622, Some(0x1000), medium());
        assert!(!comment_body(&plain).contains("Context:"));
    }

    /// Env-gated live replay: run the detectors plus the runtime suppression
    /// over a real binary and print the counts, with no model calls.
    ///
    /// Set `THORPAL_REPLAY_BINARY` and `THORPAL_REPLAY_GHIDRA` (and optionally
    /// `THORPAL_REPLAY_PROJECTS`); an existing analyzed project for the binary
    /// is reused, so repeat replays are cheap.
    #[tokio::test]
    #[ignore = "needs a Ghidra install and a target binary; env-gated"]
    async fn detector_replay() {
        use crate::thorpal::tools::ghidra::{GhidraSession, GhidraSettings};
        // pull the replay inputs from the environment so CI never needs Ghidra
        let binary = std::env::var("THORPAL_REPLAY_BINARY").expect("set THORPAL_REPLAY_BINARY");
        let install = std::env::var("THORPAL_REPLAY_GHIDRA").expect("set THORPAL_REPLAY_GHIDRA");
        let projects = std::env::var("THORPAL_REPLAY_PROJECTS")
            .unwrap_or_else(|_| "/home/ubuntu/ghidra-projects".to_owned());
        // reuse the most recent analysis so repeat replays are cheap
        let settings = GhidraSettings::for_analysis(
            std::path::PathBuf::from(&binary),
            std::path::PathBuf::from(projects),
            Some(std::path::PathBuf::from(install)),
            false,
        )
        .expect("settings should resolve");
        let session = GhidraSession::new(settings);
        let ghidra = session.get().await.expect("bridge should connect").clone();
        // run the real seed-phase pipeline: context, detections, suppression
        let ctx = AnalysisContext::build(ghidra, std::path::Path::new(&binary))
            .await
            .expect("context should build");
        let (mut hits, _) = detections::run_all_with_roster(&ctx)
            .await
            .expect("detections should run");
        let total = hits.len();
        detections::apply_runtime_context(&ctx, &mut hits);
        let runtime = hits
            .iter()
            .filter(|hit| matches!(hit.context, Some(CallContext::Runtime(_))))
            .count();
        let grouped = bucket_hits(hits);
        let bucket_count = grouped.len();
        let (kept, suppressed) = partition_suppressed(ctx.toolchain, grouped);
        // print the before/after shape the validation plan records per sample
        println!("toolchain: {:?}", ctx.toolchain);
        println!(
            "hits: {total} (runtime-classified {runtime}); buckets: {bucket_count} -> kept {} \
             (+ rollup of {} suppressed hits)",
            kept.len(),
            suppressed.len()
        );
        let mut per_tech: BTreeMap<String, usize> = BTreeMap::new();
        for hit in &suppressed {
            *per_tech.entry(hit.technique.id.to_string()).or_default() += 1;
        }
        println!("suppressed per technique: {per_tech:?}");
        // the file-level buckets carry the implant-signal findings; print
        // each one so a replay shows what the forum would open with
        for (key, hits) in &kept {
            if matches!(key, BucketKey::FileLevel(_)) {
                for hit in hits {
                    let first = hit.content.lines().next().unwrap_or_default();
                    println!("{} [{}]: {first}", bucket_name(key), hit.technique.id);
                }
            }
        }
        // optionally exercise the seed-time YARA scan against a real pack,
        // reading the file in full exactly like the seeding path does
        if let Ok(rules) = std::env::var("THORPAL_REPLAY_YARA") {
            match crate::thorpal::detect::yara::scan_file(
                std::path::Path::new(&rules),
                std::path::Path::new(&binary),
            ) {
                Ok(yara_hits) => {
                    println!("yara matches: {}", yara_hits.len());
                    for hit in &yara_hits {
                        let first = hit.content.lines().next().unwrap_or_default();
                        println!("yara: {first}");
                    }
                }
                Err(error) => println!("yara scan failed: {error:#}"),
            }
        }
        session.shutdown().await.expect("shutdown should flush");
    }

    /// Groups test hits into the bucket map [`derive_questions`] expects.
    ///
    /// # Arguments
    ///
    /// * `hits` - The hits to group into buckets
    fn buckets(hits: Vec<DetectionHit>) -> BTreeMap<BucketKey, Vec<DetectionHit>> {
        // the driver's own grouping, so key normalization matches seeding
        bucket_hits(hits)
    }

    /// A medium-confidence packing finding derives the unpack question.
    #[test]
    fn packed_binary_seeds_an_unpack_question() {
        // one file-level packing hit above the strength bar
        let buckets = buckets(vec![hit(T1027_002, None, medium())]);
        let questions = derive_questions(&buckets);
        // exactly the unpack question, citing the file-level statement post
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-unpack");
        assert!(questions[0].body.contains("#file-level"));
    }

    /// The unpack question gates on a posted proof before any wall claim
    #[test]
    fn unpack_question_carries_the_wall_proof_gate() {
        // one file-level packing hit above the strength bar
        let buckets = buckets(vec![hit(T1027_002, None, medium())]);
        let questions = derive_questions(&buckets);
        // the third task is the wall-proof gate alongside the original two
        assert_eq!(questions[0].tasks.len(), 3);
        assert!(questions[0].tasks[2].contains("search_scripts"));
        assert!(questions[0].tasks[2].contains("entry-stub"));
    }

    /// Prior-toolkit matches seed the reuse lead; none seed nothing
    #[test]
    fn prior_toolkit_question_lists_matches() {
        // no matches means no lead
        assert!(prior_toolkit_question(&[]).is_none());
        // a match list renders the names, descriptions, and both gate tasks
        let matches = vec![
            (
                "lib-1285db75".to_string(),
                "wraps a blob as a PE".to_string(),
            ),
            ("unpack_vmp".to_string(), String::new()),
        ];
        let question = prior_toolkit_question(&matches).unwrap();
        assert_eq!(question.name, "q-prior-toolkit");
        assert!(question.body.contains("2 script(s)"));
        assert!(
            question
                .body
                .contains("`lib-1285db75` — wraps a blob as a PE")
        );
        assert!(question.body.contains("`unpack_vmp`"));
        assert!(!question.body.contains("more matches exist"));
        assert_eq!(question.tasks.len(), 2);
        // more matches than the cap keeps the body bounded and says so
        let many: Vec<(String, String)> = (0..8)
            .map(|i| (format!("script_{i}"), String::new()))
            .collect();
        let question = prior_toolkit_question(&many).unwrap();
        assert!(question.body.contains("8 script(s)"));
        assert!(question.body.contains("`script_5`"));
        assert!(!question.body.contains("`script_6`"));
        assert!(question.body.contains("more matches exist"));
    }

    /// A single weak packing signal is not worth a delegation.
    #[test]
    fn weak_packing_evidence_seeds_no_question() {
        // a lone low-confidence structural signal (e.g. an odd section name)
        let buckets = buckets(vec![hit(T1027_002, None, low())]);
        assert!(derive_questions(&buckets).is_empty());
    }

    /// A control-flow-gated host identifier derives a per-gate question.
    #[test]
    fn gated_host_check_seeds_a_gate_question() {
        // a decompiler-confirmed comparison against a recovered MAC address
        let mut gated = hit(T1480_001, Some(0x401000), high());
        gated.kind = EvidenceKind::ControlFlowGated;
        gated.artifact = Some(HitArtifact::new(
            ArtifactClass::HostIdentifier,
            "00:0c:29:aa:bb:cc",
        ));
        let buckets = buckets(vec![gated]);
        let questions = derive_questions(&buckets);
        // the gate question quotes the identifier and cites the function's post
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-host-gate-00401010");
        assert!(questions[0].body.contains("00:0c:29:aa:bb:cc"));
        assert!(questions[0].body.contains("#fn-00401000"));
    }

    /// A derived gate question carries the two site-anchored check tasks.
    #[test]
    fn gate_question_carries_check_tasks() {
        // a decompiler-confirmed comparison against a recovered host identifier
        let mut gated = hit(T1480_001, Some(0x401000), high());
        gated.kind = EvidenceKind::ControlFlowGated;
        gated.artifact = Some(HitArtifact::new(
            ArtifactClass::HostIdentifier,
            "00:0c:29:aa:bb:cc",
        ));
        let buckets = buckets(vec![gated]);
        let questions = derive_questions(&buckets);
        // the gate question carries exactly two tasks, each naming the site
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].tasks.len(), 2);
        assert!(questions[0].tasks[0].contains("0x401010"));
        assert!(questions[0].tasks[0].contains("Decompile"));
        assert!(questions[0].tasks[1].contains("0x401010"));
        assert!(questions[0].tasks[1].contains("fails"));
    }

    /// Recovered endpoints across functions roll up into one C2 question.
    #[test]
    fn endpoints_roll_up_into_one_c2_question() {
        // two endpoint artifacts recovered in two different functions
        let mut first = hit(T1071, Some(0x401000), medium());
        first.artifact = Some(HitArtifact::new(
            ArtifactClass::NetworkEndpoint,
            "https://evil.example/gate.php",
        ));
        let mut second = hit(T1071, Some(0x402000), medium());
        second.artifact = Some(HitArtifact::new(
            ArtifactClass::NetworkEndpoint,
            "10.1.2.3:4444",
        ));
        let buckets = buckets(vec![first, second]);
        let questions = derive_questions(&buckets);
        // one combined question quoting both endpoints and citing both posts
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-c2-channel");
        assert!(questions[0].body.contains("https://evil.example/gate.php"));
        assert!(questions[0].body.contains("10.1.2.3:4444"));
        assert!(questions[0].body.contains("#fn-00401000"));
        assert!(questions[0].body.contains("#fn-00402000"));
    }

    /// The ransomware verdict needs impact findings in two distinct buckets.
    #[test]
    fn ransomware_needs_cross_function_corroboration() {
        // a single bucket of impact findings is not corroboration
        let lone = buckets(vec![
            hit(T1486, Some(0x401000), medium()),
            hit(T1486, Some(0x401000), medium()),
        ]);
        assert!(derive_questions(&lone).is_empty());
        // the same findings across two functions clear the bar
        let corroborated = buckets(vec![
            hit(T1486, Some(0x401000), medium()),
            hit(T1486, Some(0x402000), medium()),
        ]);
        let questions = derive_questions(&corroborated);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-ransomware");
    }

    /// Only verified evasion findings derive the gauntlet question.
    #[test]
    fn evasion_gauntlet_requires_verified_findings() {
        // a raw Likely anti-debug claim no longer qualifies (the disputed tier)
        let raw = buckets(vec![hit(T1622, Some(0x401000), high())]);
        assert!(derive_questions(&raw).is_empty());
        // a verified result gate qualifies
        let mut gated = hit(T1622, Some(0x401000), high());
        gated.context = Some(CallContext::GatesBranch);
        let questions = derive_questions(&buckets(vec![gated]));
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-evasion-gauntlet");
        // a Likely claim carrying a validated artifact qualifies too
        let mut recovered = hit(T1622, Some(0x401000), high());
        recovered.artifact = Some(HitArtifact::new(
            ArtifactClass::HostIdentifier,
            "60:45:cb:9f:8e:21",
        ));
        let questions = derive_questions(&buckets(vec![recovered]));
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-evasion-gauntlet");
        // a junk artifact does not re-qualify a Likely claim
        let mut junk = hit(T1622, Some(0x401000), high());
        junk.artifact = Some(HitArtifact::new(
            ArtifactClass::HostIdentifier,
            "a2:c2:a2:c2:a2:c2",
        ));
        assert!(derive_questions(&buckets(vec![junk])).is_empty());
    }

    /// A strong signed-digest mismatch derives the signature question.
    #[test]
    fn signature_mismatch_seeds_the_signature_question() {
        // a Likely T1553.002 file-level finding (one hit per binary)
        let mut anomaly = hit(T1553_002, None, high());
        anomaly.detection = Some("signature_anomaly".to_owned());
        let questions = derive_questions(&buckets(vec![anomaly]));
        // the question carries the fixed title, one byte-evidence gate, and
        // cites the mismatch's own bucket post
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-signature");
        assert_eq!(
            questions[0].title,
            "The code signature does not cover the file as it exists on disk — what was \
             added or changed after signing?"
        );
        assert!(questions[0].body.contains("#file-level"));
        assert_eq!(questions[0].tasks.len(), 1);
        assert!(questions[0].tasks[0].contains("tampered region, grafted table, or appended data"));
        // below Likely the mismatch seeds no question
        let weak = buckets(vec![hit(T1553_002, None, medium())]);
        assert!(derive_questions(&weak).is_empty());
    }

    /// A corroborated YARA family cluster derives one identity question.
    #[test]
    fn corroborated_yara_family_seeds_one_identity_question() {
        // two corroborated cluster leads; at most one question seeds
        let mut first = hit(T1622, None, high());
        first.source = HitSource::Yara;
        first.name = Some("YARA family: redline".to_owned());
        let mut second = hit(T1622, None, high());
        second.source = HitSource::Yara;
        second.name = Some("YARA family: vidar".to_owned());
        let questions = derive_questions(&buckets(vec![first, second]));
        // one question on the first family, citing its bucket, with one gate
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-yara-family");
        assert!(questions[0].title.contains("redline"));
        assert!(questions[0].body.contains("#file-level"));
        assert_eq!(questions[0].tasks.len(), 1);
        assert!(questions[0].tasks[0].contains("redline"));
        // an uncorroborated yara lead (no family marker) seeds nothing
        let mut plain = hit(T1622, None, high());
        plain.source = HitSource::Yara;
        assert!(derive_questions(&buckets(vec![plain])).is_empty());
    }

    /// A native-API section pair derives the section-map trace question.
    #[test]
    fn section_map_pair_seeds_the_trace_question() {
        // one grouped T1106 hit naming both section APIs in its content
        let mut pair = hit(T1106, Some(0x401000), medium());
        pair.content = "Native API set in reach: NtCreateSection, NtMapViewOfSection".to_owned();
        let questions = derive_questions(&buckets(vec![pair]));
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-section-map");
        assert!(questions[0].body.contains("#fn-00401000"));
        // one API alone is not the mapping path
        let mut lone = hit(T1106, Some(0x401000), medium());
        lone.content = "Native API set in reach: NtCreateSection".to_owned();
        assert!(derive_questions(&buckets(vec![lone])).is_empty());
    }

    /// The new question tier slots between ransomware and the evasion rollup.
    #[test]
    fn new_questions_slot_between_ransomware_and_evasion() {
        // qualifying hits for ransomware, signature, family, section, evasion
        let mut all = vec![
            hit(T1486, Some(0x401000), medium()),
            hit(T1486, Some(0x402000), medium()),
        ];
        all.push(hit(T1553_002, Some(0x403000), high()));
        let mut family = hit(T1622, None, high());
        family.source = HitSource::Yara;
        family.name = Some("YARA family: redline".to_owned());
        all.push(family);
        let mut pair = hit(T1106, Some(0x404000), medium());
        pair.content = "NtCreateSection, NtMapViewOfSection".to_owned();
        all.push(pair);
        let mut gated = hit(T1622, Some(0x405000), high());
        gated.context = Some(CallContext::GatesBranch);
        all.push(gated);
        let questions = derive_questions(&buckets(all));
        // the fixed priority order holds under the cap
        let names: Vec<&str> = questions
            .iter()
            .map(|question| question.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "q-ransomware",
                "q-signature",
                "q-yara-family",
                "q-section-map",
                "q-evasion-gauntlet"
            ]
        );
    }

    /// The question queue is capped with the highest-leverage themes kept.
    #[test]
    fn the_question_queue_is_capped() {
        // build qualifying candidates for every theme (8 total, cap is 6)
        let mut all = vec![hit(T1027_002, None, medium())];
        // two distinct host gates
        for entry in [0x401000_u64, 0x402000] {
            let mut gated = hit(T1480_001, Some(entry), high());
            gated.kind = EvidenceKind::ControlFlowGated;
            gated.artifact = Some(HitArtifact::new(ArtifactClass::HostIdentifier, "GATE"));
            all.push(gated);
        }
        // a decoded config record and a recovered symmetric key
        let mut decoded = hit(T1027_002, Some(0x403000), medium());
        decoded.artifact = Some(HitArtifact::new(ArtifactClass::ConfigRecord, "cfg"));
        all.push(decoded);
        let mut key = hit(T1486, Some(0x404000), medium());
        key.artifact = Some(HitArtifact::new(ArtifactClass::SymmetricKey, "deadbeef"));
        all.push(key);
        // a recovered endpoint, corroborated impact findings, and a verified
        // evasion gate
        let mut endpoint = hit(T1071, Some(0x405000), medium());
        endpoint.artifact = Some(HitArtifact::new(ArtifactClass::NetworkEndpoint, "1.2.3.4"));
        all.push(endpoint);
        all.push(hit(T1486, Some(0x406000), medium()));
        let mut gauntlet = hit(T1622, Some(0x407000), high());
        gauntlet.context = Some(CallContext::GatesBranch);
        all.push(gauntlet);
        let buckets = buckets(all);
        let questions = derive_questions(&buckets);
        // the cap holds and the highest-leverage question survives at the front
        assert_eq!(questions.len(), MAX_SEED_QUESTIONS);
        assert_eq!(questions[0].name, "q-unpack");
        // the lowest-priority theme (the evasion rollup) is the one cut
        assert!(questions.iter().all(|q| q.name != "q-evasion-gauntlet"));
    }

    /// Container formats seed the payload question; payload formats do not.
    #[test]
    fn container_formats_seed_the_payload_question() {
        // MSI/OLE, ZIP, CAB, and gzip all classify as wrappers to unpack
        for (format, hint) in [
            (
                "Composite Document File (OLE/CDF: MSI installer or legacy Office)",
                "list_container",
            ),
            (
                "ZIP archive (possibly an OOXML document, JAR, or APK)",
                "extract_file",
            ),
            ("Microsoft Cabinet (CAB) archive", "list_container"),
            ("gzip compressed data", "decompress"),
        ] {
            let question = container_question(format, None)
                .unwrap_or_else(|| panic!("{format} should seed a question"));
            assert_eq!(question.name, "q-container-payload");
            assert!(
                question.body.contains(hint),
                "{format} body missing {hint}: {}",
                question.body
            );
        }
        // unsupported archives still seed, but say the tools cannot help
        let question = container_question("7-Zip archive", None).expect("7z should seed");
        assert!(question.body.contains("not support"));
        // real payload formats seed nothing at all
        for format in [
            "MZ executable (Windows PE or MS-DOS)",
            "ELF executable",
            "unknown (no recognized magic)",
        ] {
            assert!(
                container_question(format, None).is_none(),
                "{format} should not seed"
            );
        }
    }

    /// Container questions carry extraction check tasks matching their branch.
    #[test]
    fn container_question_carries_extraction_tasks() {
        // enumerable archives get the list_container + extract-and-identify pair
        for format in [
            "Composite Document File (OLE/CDF: MSI installer or legacy Office)",
            "ZIP archive (possibly an OOXML document, JAR, or APK)",
            "Microsoft Cabinet (CAB) archive",
        ] {
            let question = container_question(format, None)
                .unwrap_or_else(|| panic!("{format} should seed a question"));
            assert_eq!(question.tasks.len(), 2, "{format} should carry two tasks");
            assert!(question.tasks[0].contains("list_container"));
            assert!(question.tasks[1].contains("identify it (hashes, format)"));
        }
        // single compressed streams get the decompress + identify pair instead
        for format in ["gzip compressed data", "xz compressed data"] {
            let question = container_question(format, None)
                .unwrap_or_else(|| panic!("{format} should seed a question"));
            assert_eq!(question.tasks.len(), 2, "{format} should carry two tasks");
            assert!(question.tasks[0].contains("decompress tool"));
            assert!(question.tasks[1].contains("hashes, format"));
        }
        // unsupported archives get the single raw-triage task
        for format in [
            "bzip2 compressed data",
            "7-Zip archive",
            "RAR archive (v4 or v5)",
        ] {
            let question = container_question(format, None)
                .unwrap_or_else(|| panic!("{format} should seed a question"));
            assert_eq!(question.tasks.len(), 1, "{format} should carry one task");
            assert!(question.tasks[0].contains("read_file/strings_file"));
        }
    }

    /// A CDF whose listing shows document streams seeds the macro lead.
    #[test]
    fn cdf_with_document_streams_seeds_the_maldoc_lead() {
        use crate::thorpal::tools::containers::{ContainerEntry, ContainerListing};
        let cdf = "Composite Document File (OLE/CDF: MSI installer or legacy Office)";
        // a minimal entry constructor for the two listing shapes
        let entry = |name: &str, is_dir: bool| ContainerEntry {
            name: name.to_string(),
            size: 0,
            compressed_size: None,
            method: "stored".to_string(),
            modified: None,
            encrypted: false,
            is_dir,
        };
        let listing = |entries: Vec<ContainerEntry>| ContainerListing {
            format: cdf.to_string(),
            kind: "cfb",
            total_entries: entries.len(),
            entries,
            truncated: None,
        };
        // a Word maldoc shape steers the lead to extract_macros
        let word = listing(vec![entry("WordDocument", false), entry("Macros", true)]);
        let question = container_question(cdf, Some(&word)).expect("the maldoc should seed");
        assert_eq!(question.name, "q-maldoc-macros");
        assert!(question.body.contains("extract_macros"));
        assert!(question.tasks[0].contains("extract_macros"));
        // the entries preview still rides along under the specialized advice
        assert!(question.body.contains("WordDocument (0 bytes)"));
        // an Excel shape (Workbook stream) steers the same way
        let excel = listing(vec![entry("Workbook", false)]);
        let question = container_question(cdf, Some(&excel)).expect("the workbook should seed");
        assert_eq!(question.name, "q-maldoc-macros");
        // an MSI-shaped listing keeps the generic payload-extraction lead
        let msi = listing(vec![
            entry("payload.cab", false),
            entry("Binary.Data", false),
        ]);
        let question = container_question(cdf, Some(&msi)).expect("the MSI should seed");
        assert_eq!(question.name, "q-container-payload");
        assert!(question.tasks[0].contains("list_container"));
    }

    /// Text/script formats seed the triage question; everything else does not.
    #[test]
    fn script_formats_seed_the_triage_question() {
        // every text label class opens with the read_text-first triage lead
        for format in [
            "ASCII text",
            "ASCII text (likely VBScript)",
            "UTF-8 text (BOM, likely JavaScript)",
            "UTF-16LE text (no BOM, likely PowerShell)",
            "UTF-16BE text (BOM)",
            "script with a shebang line (#!/bin/sh)",
        ] {
            let question = script_question(format)
                .unwrap_or_else(|| panic!("{format} should seed a question"));
            assert_eq!(question.name, "q-script-triage");
            assert!(
                question.body.contains("read_text"),
                "{format} body missing read_text: {}",
                question.body
            );
            assert!(
                question.body.contains("Ghidra does not apply"),
                "{format} body missing the applicability fact: {}",
                question.body
            );
        }
        // an RTF document gets the embedded-object extraction lead instead
        let question = script_question("RTF document").expect("RTF should seed");
        assert_eq!(question.name, "q-rtf-embedded");
        assert!(question.body.contains("objdata"));
        // binaries, wrappers, images, and unknowns seed no triage lead
        for format in [
            "MZ executable (Windows PE or MS-DOS)",
            "ELF executable",
            "ZIP archive (possibly an OOXML document, JAR, or APK)",
            "PNG image",
            "PDF document",
            "unknown (no recognized magic)",
            "empty file",
        ] {
            assert!(
                script_question(format).is_none(),
                "{format} should not seed"
            );
        }
    }

    /// Script questions carry the triage check tasks matching their branch.
    #[test]
    fn script_question_carries_triage_tasks() {
        // the script lead gates on language, decoded layers, and IOCs
        let question = script_question("ASCII text (likely VBScript)").expect("should seed");
        assert_eq!(question.tasks.len(), 3);
        assert!(question.tasks[0].contains("read_text"));
        assert!(question.tasks[1].contains("decode"));
        assert!(question.tasks[2].contains("IOCs"));
        // the RTF lead gates on locating and identifying the embedded object
        let question = script_question("RTF document").expect("should seed");
        assert_eq!(question.tasks.len(), 2);
        assert!(question.tasks[0].contains("grep_bytes"));
        assert!(question.tasks[1].contains("hashes, format"));
    }

    /// At most one of the two format leads seeds for any label class.
    #[test]
    fn container_and_script_questions_are_mutually_exclusive() {
        // sweep one label from every class the classifier can produce
        for format in [
            "Composite Document File (OLE/CDF: MSI installer or legacy Office)",
            "ZIP archive (possibly an OOXML document, JAR, or APK)",
            "Microsoft Cabinet (CAB) archive",
            "gzip compressed data",
            "xz compressed data",
            "bzip2 compressed data",
            "7-Zip archive",
            "RAR archive",
            "ASCII text (likely VBScript)",
            "UTF-8 text (BOM)",
            "UTF-16LE text (no BOM)",
            "UTF-16BE text (BOM)",
            "script with a shebang line (#!/bin/sh)",
            "RTF document",
            "MZ executable (Windows PE or MS-DOS)",
            "ELF executable",
            "Mach-O universal (fat) binary or Java class",
            "WebAssembly binary",
            "Android DEX bytecode",
            "PNG image",
            "JPEG image",
            "GIF image",
            "Windows shortcut (LNK)",
            "PDF document",
            "Compiled HTML Help (CHM)",
            "unknown (no recognized magic)",
            "empty file",
            "data (the first 64 bytes are all zero)",
        ] {
            let seeded = usize::from(container_question(format, None).is_some())
                + usize::from(script_question(format).is_some());
            assert!(seeded <= 1, "{format} seeded both format leads");
        }
    }

    /// The import-failure renderer is honest about gated skips.
    #[test]
    fn describe_import_failure_is_honest_about_gated_skips() {
        // a classification-gated record never ran an import
        assert_eq!(
            describe_import_failure("not attempted: ASCII text has no Ghidra loader"),
            "(not attempted: ASCII text has no Ghidra loader)"
        );
        // a real import failure keeps the failed framing
        assert_eq!(
            describe_import_failure("No load spec found"),
            "(import failed: No load spec found)"
        );
        // multi-line chains flatten and cap so the note stays scannable
        let long = format!("line one\nline two {}", "x".repeat(200));
        let described = describe_import_failure(&long);
        assert!(!described.contains('\n'));
        assert!(described.len() <= "(import failed: )".len() + 160);
    }

    /// A gathered listing enriches the question body with entry names.
    #[test]
    fn container_question_quotes_listed_entries() {
        use crate::thorpal::tools::containers::{ContainerEntry, ContainerListing};
        // a listing with two entries and a larger total
        let listing = ContainerListing {
            format: "ZIP archive (possibly an OOXML document, JAR, or APK)".to_string(),
            kind: "zip",
            entries: vec![
                ContainerEntry {
                    name: "payload.exe".to_string(),
                    size: 1234,
                    compressed_size: Some(600),
                    method: "deflated".to_string(),
                    modified: None,
                    encrypted: false,
                    is_dir: false,
                },
                ContainerEntry {
                    name: "config.dat".to_string(),
                    size: 64,
                    compressed_size: Some(64),
                    method: "stored".to_string(),
                    modified: None,
                    encrypted: false,
                    is_dir: false,
                },
            ],
            total_entries: 30,
            truncated: None,
        };
        let question = container_question(
            "ZIP archive (possibly an OOXML document, JAR, or APK)",
            Some(&listing),
        )
        .expect("the zip should seed");
        // both entries are quoted with sizes and the truncation is noted
        assert!(question.body.contains("payload.exe (1234 bytes)"));
        assert!(question.body.contains("config.dat (64 bytes)"));
        assert!(question.body.contains("30 total"));
        assert!(question.body.contains('…'));
    }

    /// Builds a bare named question for the merge tests.
    ///
    /// # Arguments
    ///
    /// * `name` - The question's space-free post name
    fn question(name: &str) -> SeedQuestion {
        SeedQuestion {
            name: name.to_string(),
            title: format!("{name} title"),
            body: format!("{name} body"),
            tasks: Vec::new(),
            origin: "derived",
        }
    }

    /// Script questions outrank derived ones, dedupe by name, and stay capped.
    #[test]
    fn script_questions_merge_first_under_the_cap() {
        // two script questions, one colliding with a derived question's name
        let script = vec![question("q-script-a"), question("q-unpack")];
        // enough derived questions to overflow the cap after the merge
        let derived: Vec<SeedQuestion> = ["q-unpack", "q-b", "q-c", "q-d", "q-e", "q-f"]
            .iter()
            .map(|name| question(name))
            .collect();
        let merged = merge_questions(script, derived);
        // the cap holds, the script questions lead, and the duplicate is gone
        assert_eq!(merged.len(), MAX_SEED_QUESTIONS);
        assert_eq!(merged[0].name, "q-script-a");
        assert_eq!(merged[1].name, "q-unpack");
        assert_eq!(
            merged.iter().filter(|q| q.name == "q-unpack").count(),
            1,
            "the duplicate derived q-unpack must be dropped"
        );
        // the lowest-priority derived question is the one cut
        assert!(merged.iter().all(|q| q.name != "q-f"));
    }

    /// Seeded questions get their check tasks attached, capped per question.
    #[tokio::test]
    async fn seed_question_posts_seeds_and_caps_question_tasks() {
        // a question carrying more check tasks than the per-question cap allows
        let forum = ForumState::new();
        let mut tasked = question("q-tasked");
        tasked.tasks = (0..MAX_SEED_TASKS_PER_QUESTION + 2)
            .map(|index| format!("check {index}"))
            .collect();
        seed_question_posts(&forum, vec![tasked], None).await;
        // the post landed and carries exactly the capped detector checklist
        let view = forum.view().await;
        let thread = view
            .threads
            .iter()
            .find(|thread| thread.post.name == "q-tasked")
            .expect("the question should be seeded");
        assert_eq!(thread.tasks.len(), MAX_SEED_TASKS_PER_QUESTION);
        assert!(thread.tasks.iter().all(|task| task.author == "detector"));
    }
    /// Sample scopes prefix post names and suffix titles; `None` is identity.
    #[test]
    fn sample_scope_namespacing() {
        // a sibling's scope prefixes names and suffixes titles
        let scope = SampleScope {
            prefix: "s2".to_owned(),
            program: "payload.bin".to_owned(),
        };
        assert_eq!(scoped_name(Some(&scope), "file-level"), "s2-file-level");
        assert_eq!(
            scoped_title(Some(&scope), "Sample file identity".to_owned()),
            "Sample file identity \u{2014} payload.bin"
        );
        // the main pass keeps its historical names and titles byte-identical
        assert_eq!(scoped_name(None, "file-level"), "file-level");
        assert_eq!(scoped_title(None, "T".to_owned()), "T");
    }

    /// A failed primary's annotation names the program and steers to raw tools.
    #[tokio::test]
    async fn failed_primary_annotation_steers_to_raw_tools() {
        // seed the annotation for a primary that never imported
        let forum = ForumState::new();
        seed_failed_primary_annotation(&forum, "evil.doc", "No load spec found\nchain line")
            .await
            .expect("the annotation should seed");
        // the fixed-name post landed with the program named in its title
        let view = forum.view().await;
        let thread = view
            .threads
            .iter()
            .find(|thread| thread.post.name == "primary-not-loaded")
            .expect("the annotation post should exist");
        assert!(thread.post.title.contains("evil.doc"));
        // the body says the primary was never statically seeded ...
        assert!(thread.post.body.contains("NOT loaded in Ghidra"));
        assert!(thread.post.body.contains("no static detector findings"));
        // ... flattens the reason's rendered chain onto one line ...
        assert!(thread.post.body.contains("No load spec found chain line"));
        // ... and steers the model to the raw file tools on the right target
        assert!(thread.post.body.contains("target='evil.doc'"));
    }

    /// The yara-only pass seeds file-level findings with no Ghidra at all.
    #[tokio::test]
    async fn seed_yara_only_seeds_file_level_hits() {
        // a scratch sample plus one rule matching its bytes
        let dir = tempfile::tempdir().expect("tempdir");
        let sample = dir.path().join("payload.bin");
        std::fs::write(&sample, b"needle-for-yara-9000").expect("write sample");
        let rules = dir.path().join("rules.yar");
        std::fs::write(
            &rules,
            "rule seed_hit { strings: $a = \"needle-for-yara-9000\" condition: $a }",
        )
        .expect("write rules");
        // run the yara-only pass against an in-memory forum
        let forum = ForumState::new();
        seed_yara_only(&forum, &sample, &rules, None)
            .await
            .expect("the yara-only pass should seed");
        // the file-level bucket landed with the rule's finding under it
        let view = forum.view().await;
        let thread = view
            .threads
            .iter()
            .find(|thread| thread.post.name == "file-level")
            .expect("the file-level post should exist");
        assert!(thread.post.title.contains("File-level"));
        assert!(
            thread
                .comments
                .iter()
                .any(|node| node.comment.body.contains("seed_hit")),
            "the rule identifier should reach the seeded comment"
        );
        // a sample nothing matches seeds nothing (and still succeeds)
        let cold = ForumState::new();
        let misses = dir.path().join("misses.bin");
        std::fs::write(&misses, b"nothing to see").expect("write sample");
        seed_yara_only(&cold, &misses, &rules, None)
            .await
            .expect("a hit-less pass should succeed");
        assert!(cold.view().await.threads.is_empty());
    }
}
