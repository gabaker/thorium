//! Runs the static detectors and seeds the forum with points of interest.
//!
//! Before the agent loop starts, this runs the ported detection engine
//! ([`crate::thorpal::detect`]) over the target binary and turns each finding
//! into a forum post/comment. Findings are grouped by the function they land in,
//! so the agents open with concrete points of interest — a function, the evidence
//! found there, and why it matters — instead of discovering them from scratch.
//!
//! On top of the per-function statement posts, the strongest findings also derive
//! a small set of targeted open questions (is it packed? what does this host
//! check gate?), so the main agent's question loop opens with focused work to
//! delegate instead of exploring the binary cold.

use std::collections::BTreeMap;
use std::path::Path;

use ghidra_cli::Ghidra;

use crate::thorpal::detect::confidence::Confidence;
use crate::thorpal::detect::context::{AnalysisContext, fmt_addr};
use crate::thorpal::detect::detections::{
    self, ArtifactClass, DetectionHit, DetectorRun, EvidenceKind, confidence_rank,
};
use crate::thorpal::detect::emulation;
use crate::thorpal::detect::provenance_types::CallContext;
use crate::thorpal::detect::script_detections::{self, ScriptDetector};
use crate::thorpal::detect::toolchain::Toolchain;
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
/// The seeding path uses fixed post names (`file-identity`, `file-level`,
/// `runtime-noise`, the `q-*` questions), and `seed_post` refuses duplicate
/// names — so a second sample's pass must prefix every name it seeds. The
/// prefix is order-derived (`s2`, `s3`, …) and the program name suffixes
/// titles so threads read unambiguously. The main sample seeds with no scope
/// and keeps its historical names byte-identical.
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
/// groups the hits by the function each lands in (the `None` bucket is
/// file-level), and creates one post per function with one comment per finding.
/// When `seed_questions` is set, the script-emitted questions plus the strongest
/// findings' derived questions (see [`derive_questions`]) are seeded as a
/// bounded set of targeted open questions.
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
    // classify runtime-library noise once over every source's hits: a hit in
    // recognizable runtime code is weakened a step and gains the runtime note
    detections::apply_runtime_context(&ctx, &mut hits);
    // group the hits by their containing function entry (None = file-level); a
    // BTreeMap keeps the buckets in a stable address order across runs
    let mut buckets: BTreeMap<Option<u64>, Vec<DetectionHit>> = BTreeMap::new();
    for hit in hits {
        buckets.entry(hit.function).or_default().push(hit);
    }
    // strongest, most suspicious findings first so each thread reads top-down and
    // the question derivation below sees every bucket pre-ranked
    for hits in buckets.values_mut() {
        hits.sort_by(|a, b| {
            confidence_rank(b.confidence)
                .cmp(&confidence_rank(a.confidence))
                .then(b.suspicion.cmp(&a.suspicion))
        });
    }
    // split pure runtime-noise buckets out so a fingerprinted toolchain's
    // stdlib does not flood the forum; the rollup post below keeps the record
    let (buckets, suppressed) = partition_suppressed(ctx.toolchain, buckets);
    if !suppressed.is_empty() {
        // one post summarizes everything suppressed, so the forum stays honest
        if let Err(error) = seed_suppressed_rollup(
            forum,
            ctx.toolchain.expect("suppression requires a toolchain"),
            &suppressed,
            scope,
        )
        .await
        {
            // a failed rollup must not sink the real seeding below
            tracing::warn!(?error, "failed to seed the runtime-noise rollup post");
        }
    }
    // seed one statement post per bucket, then one comment per finding under it
    for (function, hits) in &buckets {
        // seed the post for this bucket and, on success, its per-finding comments
        if let Err(error) = seed_bucket(Some(&ctx), forum, *function, hits, scope).await {
            // one bad bucket must not sink the rest of the seeding
            tracing::warn!(
                ?function,
                ?error,
                "failed to seed a detector bucket; skipping"
            );
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
    // group by containing function for shape parity with the main pass; YARA
    // hits are file-level, so this is one `None` bucket in practice
    let mut buckets: BTreeMap<Option<u64>, Vec<DetectionHit>> = BTreeMap::new();
    for hit in hits {
        buckets.entry(hit.function).or_default().push(hit);
    }
    // strongest, most suspicious findings first, like the main pass
    for hits in buckets.values_mut() {
        hits.sort_by(|a, b| {
            confidence_rank(b.confidence)
                .cmp(&confidence_rank(a.confidence))
                .then(b.suspicion.cmp(&a.suspicion))
        });
    }
    // seed one post per bucket, with no context to resolve function names from
    for (function, hits) in &buckets {
        if let Err(error) = seed_bucket(None, forum, *function, hits, scope).await {
            // one bad bucket must not sink the rest of the seeding
            tracing::warn!(?function, ?error, "failed to seed a yara bucket; skipping");
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

/// Split the buckets into kept buckets and low-signal noise.
///
/// A bucket is suppressed only when a toolchain was fingerprinted (the
/// enabling condition: a statically linked runtime is what floods the forum)
/// AND every hit in it is noise under the policy: `Untrusted` outright (the
/// ubiquitous-API grade that also fires on benign code), or `Unsure` while
/// sitting in classified runtime code. The file-level bucket is never
/// suppressed, and a bucket carrying any pivot-class artifact (configs,
/// endpoints, keys, certificates, host identifiers) is always kept — a real
/// recovery stays loud wherever it sits. Suppressed hits are returned for the
/// single rollup post, which records each one, so nothing is silently lost.
///
/// # Arguments
///
/// * `toolchain` - The fingerprinted toolchain, if any
/// * `buckets` - The per-function hit buckets, strongest-first
fn partition_suppressed(
    toolchain: Option<Toolchain>,
    buckets: BTreeMap<Option<u64>, Vec<DetectionHit>>,
) -> (BTreeMap<Option<u64>, Vec<DetectionHit>>, Vec<DetectionHit>) {
    // without a fingerprint there is no runtime to attribute noise to
    if toolchain.is_none() {
        return (buckets, Vec::new());
    }
    // a single hit is noise when weak outright or runtime-located and unsure
    let is_noise = |hit: &DetectionHit| {
        matches!(hit.confidence, Confidence::Untrusted)
            || (matches!(hit.confidence, Confidence::Unsure)
                && matches!(hit.context, Some(CallContext::Runtime(_))))
    };
    // pivot-class artifacts veto suppression for their whole bucket
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
            })
        })
    };
    // keep or suppress whole buckets so threads never show half their findings
    let mut kept: BTreeMap<Option<u64>, Vec<DetectionHit>> = BTreeMap::new();
    let mut suppressed: Vec<DetectionHit> = Vec::new();
    for (function, hits) in buckets {
        // file-level findings and protected artifacts are always kept
        let suppress = function.is_some() && !has_protected(&hits) && hits.iter().all(is_noise);
        if suppress {
            suppressed.extend(hits);
        } else {
            kept.insert(function, hits);
        }
    }
    (kept, suppressed)
}

/// The most suppressed findings itemized in the rollup post's body.
const MAX_ROLLUP_LINES: usize = 40;

/// Seed the single rollup post recording the suppressed low-signal findings.
///
/// # Arguments
///
/// * `forum` - The instance's forum state to seed the post onto
/// * `toolchain` - The fingerprinted toolchain that enabled suppression
/// * `suppressed` - The suppressed hits, itemized (bounded) in the body
/// * `scope` - The sample's post-name namespace, or `None` for the main pass
#[tracing::instrument(skip_all, err(Debug))]
async fn seed_suppressed_rollup(
    forum: &ForumState,
    toolchain: Toolchain,
    suppressed: &[DetectionHit],
    scope: Option<&SampleScope>,
) -> anyhow::Result<()> {
    // split the two suppression classes so the post never mislabels noise
    // that merely co-resides with the fingerprinted runtime
    let runtime_located = suppressed
        .iter()
        .filter(|hit| matches!(hit.context, Some(CallContext::Runtime(_))))
        .count();
    let low_grade = suppressed.len() - runtime_located;
    // name the post statically so reruns and resumes never collide
    let title = format!(
        "{} low-signal detector findings suppressed (recognized {} binary)",
        suppressed.len(),
        toolchain.label()
    );
    // itemize each suppressed finding (bounded) so nothing is silently lost
    let mut lines = Vec::new();
    for hit in suppressed.iter().take(MAX_ROLLUP_LINES) {
        // one compact line: technique, site, and the content's first line
        let content = hit.content.lines().next().unwrap_or_default();
        lines.push(format!(
            "- {} @ {}: {}",
            hit.technique.id,
            fmt_addr(hit.address),
            content
        ));
    }
    if suppressed.len() > MAX_ROLLUP_LINES {
        lines.push(format!(
            "- … and {} more of the same grades",
            suppressed.len() - MAX_ROLLUP_LINES
        ));
    }
    let body = format!(
        "This binary statically links a recognized {} runtime, so the \
         low-signal detector findings were collapsed here instead of seeding \
         per-function threads: {runtime_located} sit inside recognized \
         runtime/CRT code, and {low_grade} are untrusted-grade matches on APIs \
         and patterns that benign code also contains. Re-raise any entry below \
         as a question if the host turns out to be suspect.\n{}",
        toolchain.label(),
        lines.join("\n"),
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
            // record every suppressed finding structurally (bounded well past
            // the body's line cap, so telemetry sees what the text truncates)
            suppressed
                .iter()
                .take(500)
                .map(|hit| finding_provenance(hit, true))
                .collect(),
        )
        .await?;
    Ok(())
}

/// Seed one function's (or the file-level) post plus a comment per finding.
///
/// # Arguments
///
/// * `ctx` - The analysis context (used to resolve the function name), or
///   `None` for a Ghidra-less pass whose hits are file-level anyway
/// * `forum` - The instance's forum state to seed the post and comments onto
/// * `function` - The containing function entry, or `None` for file-level findings
/// * `hits` - The findings in this bucket, already sorted strongest-first
/// * `scope` - The sample's post-name namespace, or `None` for the main pass
#[tracing::instrument(skip_all, err(Debug))]
async fn seed_bucket(
    ctx: Option<&AnalysisContext>,
    forum: &ForumState,
    function: Option<u64>,
    hits: &[DetectionHit],
    scope: Option<&SampleScope>,
) -> anyhow::Result<()> {
    // derive a unique, space-free post name distinct from any LLM-chosen name
    let name = scoped_name(scope, &bucket_name(function));
    // build the human-readable title: function label + address + finding count
    let title = match function {
        Some(entry) => {
            // resolve the function name, falling back to the raw address
            let label = ctx
                .and_then(|ctx| ctx.function_name(entry))
                .unwrap_or("(unnamed)");
            format!(
                "{label} @ {} — {} detector finding(s)",
                fmt_addr(entry),
                hits.len()
            )
        }
        None => format!("File-level — {} detector finding(s)", hits.len()),
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
/// * `function` - The containing function entry, or `None` for file-level findings
fn bucket_name(function: Option<u64>) -> String {
    // key function buckets by entry address and the file bucket by a fixed name
    match function {
        Some(entry) => format!("fn-{entry:08x}"),
        None => "file-level".to_string(),
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
/// * `suppressed` - Whether the hit was suppressed into the noise rollup
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
/// Walks the already-sorted buckets and emits at most [`MAX_SEED_QUESTIONS`]
/// questions, highest-leverage theme first: unpacking blocks every other analysis
/// angle, a gated host check decides whether the payload runs at all, and so on
/// down to the cross-cutting evasion rollup. Only strong signals qualify — a run
/// whose findings are all weak seeds no questions — so a mostly-benign binary
/// leaves the question queue empty.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by containing function, each sorted
///   strongest-first
fn derive_questions(buckets: &BTreeMap<Option<u64>, Vec<DetectionHit>>) -> Vec<SeedQuestion> {
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
    // the cross-function evasion rollup reads the whole gauntlet as one question
    questions.extend(evasion_question(buckets));
    // bound the queue so seeding never floods the delegation loop
    questions.truncate(MAX_SEED_QUESTIONS);
    questions
}

/// Render a capped `Evidence: see #post, …` line citing the backing buckets.
///
/// Citing the statement posts lets the delegated agent jump straight to the
/// evidence (sites, confidence, recovered artifacts) instead of re-deriving it.
///
/// # Arguments
///
/// * `functions` - The containing functions of the backing findings, strongest
///   first (duplicates are collapsed)
fn evidence_line(functions: impl IntoIterator<Item = Option<u64>>) -> String {
    // dedupe while preserving the caller's strongest-first order
    let mut names: Vec<String> = Vec::new();
    for function in functions {
        let name = bucket_name(function);
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
/// * `buckets` - The findings grouped by containing function
fn unpack_question(buckets: &BTreeMap<Option<u64>, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // collect the packing findings that clear the strength bar
    let cited: Vec<Option<u64>> = buckets
        .values()
        .flatten()
        .filter(|hit| hit.technique.id.as_ref() == "T1027.002")
        .filter(|hit| confidence_rank(hit.confidence) >= confidence_rank(Confidence::Unsure))
        .map(|hit| hit.function)
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
/// * `buckets` - The findings grouped by containing function
fn gate_questions(buckets: &BTreeMap<Option<u64>, Vec<DetectionHit>>) -> Vec<SeedQuestion> {
    let mut questions = Vec::new();
    for hit in buckets.values().flatten() {
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
                evidence_line([hit.function]),
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
/// * `buckets` - The findings grouped by containing function
fn decoded_config_question(
    buckets: &BTreeMap<Option<u64>, Vec<DetectionHit>>,
) -> Option<SeedQuestion> {
    // cite every bucket that carries recovered plaintext or a parsed config record
    let cited: Vec<Option<u64>> = buckets
        .values()
        .flatten()
        .filter(|hit| {
            matches!(
                hit.artifact.as_ref().map(|artifact| artifact.class),
                Some(ArtifactClass::DecodedData | ArtifactClass::ConfigRecord)
            )
        })
        .map(|hit| hit.function)
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
/// * `buckets` - The findings grouped by containing function
fn crypto_key_question(buckets: &BTreeMap<Option<u64>, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // cite every bucket that recovered symmetric or private key material
    let cited: Vec<Option<u64>> = buckets
        .values()
        .flatten()
        .filter(|hit| {
            matches!(
                hit.artifact.as_ref().map(|artifact| artifact.class),
                Some(ArtifactClass::SymmetricKey | ArtifactClass::AsymmetricPrivateKey)
            )
        })
        .map(|hit| hit.function)
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
/// * `buckets` - The findings grouped by containing function
fn c2_question(buckets: &BTreeMap<Option<u64>, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // gather the distinct endpoint values and the buckets backing them
    let mut endpoints: Vec<&str> = Vec::new();
    let mut cited: Vec<Option<u64>> = Vec::new();
    for hit in buckets.values().flatten() {
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
        cited.push(hit.function);
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
/// * `buckets` - The findings grouped by containing function
fn ransomware_question(buckets: &BTreeMap<Option<u64>, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // cite every bucket carrying an impact (data-encrypted-for-impact) finding
    let cited: Vec<Option<u64>> = buckets
        .values()
        .flatten()
        .filter(|hit| hit.technique.id.as_ref() == "T1486")
        .map(|hit| hit.function)
        .collect();
    // require corroboration across functions before raising the heavyweight claim
    let distinct: std::collections::BTreeSet<Option<u64>> = cited.iter().copied().collect();
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

/// Derive the evasion-gauntlet question when strong anti-analysis checks exist.
///
/// # Arguments
///
/// * `buckets` - The findings grouped by containing function
fn evasion_question(buckets: &BTreeMap<Option<u64>, Vec<DetectionHit>>) -> Option<SeedQuestion> {
    // cite every bucket with a strong anti-debug or anti-sandbox finding
    let cited: Vec<Option<u64>> = buckets
        .values()
        .flatten()
        .filter(|hit| {
            hit.technique.id.as_ref() == "T1622" || hit.technique.id.as_ref().starts_with("T1497")
        })
        .filter(|hit| confidence_rank(hit.confidence) >= confidence_rank(Confidence::Likely))
        .map(|hit| hit.function)
        .collect();
    // weak or absent evasion evidence seeds no question
    if cited.is_empty() {
        return None;
    }
    Some(SeedQuestion {
        name: "q-evasion-gauntlet".to_string(),
        title: "Map the anti-analysis gauntlet".to_string(),
        body: format!(
            "The detectors found strong anti-debug and/or anti-sandbox checks. Map \
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
    use crate::thorpal::detect::detections::defense_evasion::{T1027_002, T1480_001, T1622};
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
        seed_bucket(None, &forum, Some(0x401000), &[strong.clone()], None)
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

    /// Pure runtime-noise buckets suppress; signal, artifacts, and the
    /// file-level bucket always stay.
    #[test]
    fn partition_suppressed_keeps_signal() {
        // a bucket of runtime-located noise, a strong bucket, the file bucket,
        // and a weak bucket shielded by a pivot-class artifact
        let mut noise = hit(T1622, Some(0x1000), low());
        noise.context = Some(CallContext::Runtime("rt".to_owned()));
        let mut unsure_runtime = hit(T1622, Some(0x1000), medium());
        unsure_runtime.context = Some(CallContext::Runtime("rt".to_owned()));
        let strong = hit(T1486, Some(0x2000), high());
        let file_level = hit(T1027_002, None, low());
        let mut shielded = hit(T1071, Some(0x3000), low());
        shielded.artifact = Some(HitArtifact {
            class: ArtifactClass::NetworkEndpoint,
            value: "endpoint".to_owned(),
            role: None,
            detail: None,
        });
        let mut input: BTreeMap<Option<u64>, Vec<DetectionHit>> = BTreeMap::new();
        input.insert(Some(0x1000), vec![noise, unsure_runtime]);
        input.insert(Some(0x2000), vec![strong]);
        input.insert(None, vec![file_level]);
        input.insert(Some(0x3000), vec![shielded]);
        let (kept, suppressed) = partition_suppressed(Some(Toolchain::Rust), input.clone());
        // only the pure-noise bucket moved to the rollup
        assert_eq!(suppressed.len(), 2);
        assert!(!kept.contains_key(&Some(0x1000)));
        // the strong, file-level, and artifact-shielded buckets all stayed
        assert!(kept.contains_key(&Some(0x2000)));
        assert!(kept.contains_key(&None));
        assert!(kept.contains_key(&Some(0x3000)));
        // without a toolchain fingerprint nothing is ever suppressed
        let (kept, suppressed) = partition_suppressed(None, input);
        assert!(suppressed.is_empty());
        assert_eq!(kept.len(), 4);
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
        let mut grouped: BTreeMap<Option<u64>, Vec<DetectionHit>> = BTreeMap::new();
        for hit in hits {
            grouped.entry(hit.function).or_default().push(hit);
        }
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
        // the file-level bucket carries the implant-signal findings; print
        // each one so a replay shows what the forum would open with
        if let Some(file_level) = kept.get(&None) {
            for hit in file_level {
                let first = hit.content.lines().next().unwrap_or_default();
                println!("file-level [{}]: {first}", hit.technique.id);
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
    /// * `hits` - The hits to group by containing function
    fn buckets(hits: Vec<DetectionHit>) -> BTreeMap<Option<u64>, Vec<DetectionHit>> {
        // mirror the driver's grouping without its sorting (irrelevant here)
        let mut buckets: BTreeMap<Option<u64>, Vec<DetectionHit>> = BTreeMap::new();
        for hit in hits {
            buckets.entry(hit.function).or_default().push(hit);
        }
        buckets
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

    /// Only strong evasion findings derive the gauntlet question.
    #[test]
    fn evasion_gauntlet_requires_strong_findings() {
        // a medium-confidence anti-debug API call alone is too weak
        let weak = buckets(vec![hit(T1622, Some(0x401000), medium())]);
        assert!(derive_questions(&weak).is_empty());
        // a high-confidence check (e.g. a resolved PEB read) clears the bar
        let strong = buckets(vec![hit(T1622, Some(0x401000), high())]);
        let questions = derive_questions(&strong);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-evasion-gauntlet");
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
        // a recovered endpoint, corroborated impact findings, and strong evasion
        let mut endpoint = hit(T1071, Some(0x405000), medium());
        endpoint.artifact = Some(HitArtifact::new(ArtifactClass::NetworkEndpoint, "1.2.3.4"));
        all.push(endpoint);
        all.push(hit(T1486, Some(0x406000), medium()));
        all.push(hit(T1622, Some(0x407000), high()));
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
