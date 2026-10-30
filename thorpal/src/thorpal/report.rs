//! Generates markdown reports over a ThorPal session's accumulated state.
//!
//! A report is produced by a bare one-shot agent (no tools, no memory): the
//! session's conversation transcript and rendered forum are embedded into a
//! prompt alongside a markdown template describing the report's structure, and
//! the model writes the report following that template. A machine-generated
//! forum-evidence appendix (see
//! [`crate::thorpal::tools::forum::ForumState::report_appendix`]) is then
//! attached below the written report, so every concrete artifact posted on the
//! forum survives verbatim even where the narrative summarizes. Callers go
//! through [`crate::Session::generate_report`], which also persists the result
//! into the session store as `report.md`.
//!
//! Behind a content-filtering proxy the one-shot request is hazardous by
//! construction -- it embeds the raw transcript and forum, which is exactly
//! the bulk IOC text such filters kill (an evaluation run lost every report
//! attempt for some families this way). Generation therefore
//! climbs a reactive ladder: the pristine one-shot first (byte-identical to
//! the pre-ladder path when nothing blocks), then the same one-shot with the
//! embedded context defanged at render time and the model instructed to keep
//! the defanged forms (the reply is refanged client-side so the stored report
//! stays pristine-form), then per-section generation where a section that
//! still cannot transit degrades to a placeholder instead of killing the
//! whole report. The report agent rides the pause gate with the
//! [`ScrubPolicy::Surface`] policy so a content-filter kill surfaces here --
//! where the ladder knows how to rebuild the request -- instead of being
//! scrubbed generically at the retry layer.

use std::future::Future;

use rig::completion::{Prompt, PromptError};
use tracing::instrument;

use super::events::ThorPalEvent;
use super::resume::TranscriptEntry;
use super::scrub::{self, ScrubPolicy};
use super::{REPORT_MAX_TOKENS, ThorPal, retry};

/// The builtin default report template used when no override is supplied.
const BUILTIN_REPORT_TEMPLATE: &str = include_str!("../../report_template.md");

/// The prompt note defanged rungs carry so indicators survive verbatim.
///
/// Without it the model tends to re-arm `hxxp`/`[.]` forms back to live ones,
/// which both re-triggers the proxy on the response path and breaks the
/// client-side refang's byte-exact restoration.
const DEFANG_PROMPT_NOTE: &str = " IOC text in the context below has been \
defanged for transit (hxxp, [.]): reproduce every indicator exactly as \
written and do not re-arm defanged forms back to live ones.";

/// The placeholder body a section that cannot transit the proxy degrades to.
const SECTION_PLACEHOLDER: &str =
    "*[section unavailable: the completion request was blocked by a content-filtering proxy]*";

/// Renders a saved conversation transcript as plain markdown
///
/// Shared with the side-conversation summarizer (see
/// [`crate::thorpal::sides`]), which renders a side's history the same way.
///
/// # Arguments
///
/// * `entries` - The transcript entries to render
pub(crate) fn render_transcript(entries: &[TranscriptEntry]) -> String {
    // render each entry under a speaker heading so the model can follow turns
    entries
        .iter()
        .map(|entry| match entry {
            TranscriptEntry::User(text) => format!("## User:\n{text}"),
            TranscriptEntry::Assistant(text) => format!("## Assistant:\n{text}"),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Builds the one-shot report prompt from a template and the session's state
///
/// # Arguments
///
/// * `template` - The markdown template describing the report's structure
/// * `transcript` - The rendered conversation transcript
/// * `forum` - The rendered forum discussion
/// * `note` - An extra instruction sentence, or `""` for the pristine form
fn build_prompt(template: &str, transcript: &str, forum: &str, note: &str) -> String {
    format!(
        "You are writing the final analysis report for a completed binary \
         analysis session. Follow the template's structure and sections \
         exactly, filling each section in with the concrete findings and \
         evidence from the conversation transcript and forum discussion \
         below. Output only the finished markdown report with no preamble \
         or commentary. Concrete artifacts are never optional: every decoded \
         string, IOC, cryptographic key, file or registry path, and constant \
         that appears in the transcript or forum must appear verbatim \
         somewhere in the report.{note}\n\n\
         <template>\n{template}\n</template>\n\n\
         <transcript>\n{transcript}\n</transcript>\n\n\
         <forum>\n{forum}\n</forum>"
    )
}

/// Builds the prompt asking for exactly one template section of the report
///
/// Always carries the defang note: per-section generation only runs after the
/// defanged one-shot was itself blocked, so the context is always defanged.
///
/// # Arguments
///
/// * `section` - The template section (heading plus guidance) to write
/// * `transcript` - The defanged conversation transcript
/// * `forum` - The defanged forum discussion
fn build_section_prompt(section: &str, transcript: &str, forum: &str) -> String {
    format!(
        "You are writing ONE section of the final analysis report for a \
         completed binary analysis session. Write only the template section \
         below, filling it in with the concrete findings and evidence from \
         the conversation transcript and forum discussion. Output only that \
         finished markdown section with no preamble or commentary. Concrete \
         artifacts are never optional: every decoded string, IOC, \
         cryptographic key, file or registry path, and constant relevant to \
         this section must appear verbatim in it.{DEFANG_PROMPT_NOTE}\n\n\
         <template_section>\n{section}\n</template_section>\n\n\
         <transcript>\n{transcript}\n</transcript>\n\n\
         <forum>\n{forum}\n</forum>"
    )
}

/// Splits a report template into its `## `-headed sections
///
/// A new section starts at every line opening with `## `; any text before the
/// first heading joins the first section so nothing is lost. The sections
/// concatenate back to the template byte-exactly.
///
/// # Arguments
///
/// * `template` - The markdown template to split
fn split_sections(template: &str) -> Vec<String> {
    // cut a new section at each heading; split_inclusive keeps line endings
    // so the concatenation of the sections round-trips the template
    let mut sections: Vec<String> = Vec::new();
    let mut seen_heading = false;
    for line in template.split_inclusive('\n') {
        if line.starts_with("## ") {
            // the first heading absorbs any preamble gathered so far
            if seen_heading || sections.is_empty() {
                sections.push(String::new());
            }
            seen_heading = true;
        } else if sections.is_empty() {
            // preamble text before any heading opens the first section
            sections.push(String::new());
        }
        // append the line to the section being gathered
        sections
            .last_mut()
            .expect("a section was just ensured")
            .push_str(line);
    }
    sections
}

/// Renders the degraded form of a section whose request could not transit
///
/// # Arguments
///
/// * `section` - The template section that could not be generated
fn section_placeholder(section: &str) -> String {
    // keep the section's heading so the report structure survives the loss
    let heading = section
        .lines()
        .find(|line| line.starts_with("## "))
        .unwrap_or("## Section");
    format!("{heading}\n\n{SECTION_PLACEHOLDER}")
}

/// Whether a prompt failure was a content-filtering proxy kill
///
/// # Arguments
///
/// * `err` - The prompt error to classify
fn is_content_filtered(err: &PromptError) -> bool {
    // only a completion failure carries the provider response to classify
    match err {
        PromptError::CompletionError(inner) => retry::is_content_filter_error(inner),
        _ => false,
    }
}

/// A short single-line summary of a prompt failure for a degradation note
///
/// A content-filter kill's raw Display carries the whole preserved block page
/// (multi-KB HTML), which would bloat the stderr line, the TUI bubble, and
/// the durable transcript marker; every sibling surface (retry lines, pause
/// banners) summarizes instead, so degradation notes do too.
///
/// # Arguments
///
/// * `err` - The prompt error to summarize
fn summarize_prompt_error(err: &PromptError) -> String {
    match err {
        // completion failures reuse the retry layer's status-line summary,
        // suffixed with the provider request id when the error preserved one
        // (degradation notes are rare enough that the id rides inline)
        PromptError::CompletionError(inner) => match retry::request_id(inner) {
            Some(id) => format!("{} (request id: {id})", retry::error_summary(inner)),
            None => retry::error_summary(inner),
        },
        // anything else flattens onto one bounded line
        other => {
            let text = other.to_string().replace('\n', " ");
            if text.chars().count() > 80 {
                let truncated: String = text.chars().take(77).collect();
                format!("{truncated}...")
            } else {
                text
            }
        }
    }
}

/// One degradation the report ladder recorded while generating
#[derive(Debug)]
pub(crate) struct ReportFallbackNote {
    /// The ladder rung this degradation escalated to (1 = defanged one-shot,
    /// 2 = per-section generation; a placeholder section also records 2)
    rung: u8,
    /// What degraded and why, for the operator-facing event
    detail: String,
}

/// Attaches the machine-generated forum appendix below a finished report
///
/// # Arguments
///
/// * `report` - The report the one-shot agent wrote
/// * `appendix` - The rendered forum-evidence appendix, if anything qualified
fn attach_appendix(report: String, appendix: Option<String>) -> String {
    // append the appendix after a blank-line separator, or pass through unchanged
    match appendix {
        Some(appendix) => format!("{report}\n\n{appendix}"),
        None => report,
    }
}

/// Drives the content-filter report ladder over an abstract prompt function
///
/// Rung 0 sends the pristine one-shot prompt (byte-identical to the
/// pre-ladder path). A content-filter kill escalates to rung 1 -- the same
/// one-shot with the embedded context defanged and the model instructed to
/// keep the defanged forms, whose reply is refanged client-side -- and a
/// second kill escalates to rung 2, generating the report one template
/// section at a time with a per-section placeholder for anything that still
/// cannot transit. Any non-filter error (credential expiry, exhausted
/// retries) propagates unchanged at every rung -- only filter-classified
/// kills ever degrade -- so a failing provider still fails the report
/// instead of silently producing a placeholder shell.
///
/// # Arguments
///
/// * `prompt_fn` - Sends one report prompt and returns the model's reply
/// * `template` - The markdown template describing the report's structure
/// * `transcript` - The rendered (pristine) conversation transcript
/// * `forum` - The rendered (pristine) forum discussion
async fn generate_with<F, Fut>(
    prompt_fn: F,
    template: &str,
    transcript: &str,
    forum: &str,
) -> Result<(String, Vec<ReportFallbackNote>), anyhow::Error>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<String, PromptError>>,
{
    // the degradations recorded on the way down the ladder
    let mut notes = Vec::new();
    // rung 0: the pristine one-shot, byte-identical to the pre-ladder path
    match prompt_fn(build_prompt(template, transcript, forum, "")).await {
        Ok(report) => return Ok((report, notes)),
        // a content-filter kill steps down the ladder instead of failing
        Err(err) if is_content_filtered(&err) => notes.push(ReportFallbackNote {
            rung: 1,
            detail: format!(
                "the pristine report request was blocked ({}); retrying defanged",
                summarize_prompt_error(&err)
            ),
        }),
        // every other failure keeps the pre-ladder behavior
        Err(err) => return Err(err.into()),
    }
    // rung 1: defang the embedded context once for every remaining rung
    let transcript = scrub::defang(transcript);
    let forum = scrub::defang(forum);
    match prompt_fn(build_prompt(
        template,
        &transcript,
        &forum,
        DEFANG_PROMPT_NOTE,
    ))
    .await
    {
        // refang the reply so the stored report keeps pristine indicators
        Ok(report) => return Ok((scrub::refang(&report), notes)),
        // a second kill escalates to per-section generation
        Err(err) if is_content_filtered(&err) => notes.push(ReportFallbackNote {
            rung: 2,
            detail: format!(
                "the defanged report request was blocked too ({}); generating per section",
                summarize_prompt_error(&err)
            ),
        }),
        Err(err) => return Err(err.into()),
    }
    // rung 2: one request per template section, degrading only what the
    // filter still blocks
    let sections = split_sections(template);
    let mut assembled = Vec::with_capacity(sections.len());
    for section in &sections {
        match prompt_fn(build_section_prompt(section, &transcript, &forum)).await {
            // refang each finished section like the one-shot reply
            Ok(text) => assembled.push(scrub::refang(&text)),
            // a section that still cannot transit becomes a placeholder: the
            // other sections' finished text is worth keeping
            Err(err) if is_content_filtered(&err) => {
                notes.push(ReportFallbackNote {
                    rung: 2,
                    detail: format!(
                        "a report section was blocked ({}); it was replaced \
                         with a placeholder",
                        summarize_prompt_error(&err)
                    ),
                });
                assembled.push(section_placeholder(section));
            }
            // any other failure (credential expiry, exhausted retries) must
            // keep the pre-ladder contract: fail the report so the run exits
            // nonzero/75 and a scheduler retries, instead of silently
            // overwriting the stored report with an all-placeholder shell
            Err(err) => return Err(err.into()),
        }
    }
    Ok((assembled.join("\n\n"), notes))
}

/// The most field conflicts one reconciliation pass lists.
const MAX_FIELD_CONFLICTS: usize = 12;

/// The most absolute-verdict flags one calibration pass lists.
const MAX_VERDICT_FLAGS: usize = 8;

/// Find absolute benign/incapable verdict phrases in a report body
///
/// The dev-10 validation run showed the preamble calibration rule alone does
/// not reach the report writer: URLZone's "proven incapable" decoy verdict
/// survived a third run and Keentroll minted a fresh absolute negative even
/// as its old one was fixed. This scan backs the rule with the same shape
/// that killed the field-conflict class (0059): detect the claim, spend one
/// bounded turn on it before the report freezes. The phrase list is the
/// observed wrong-claim vocabulary — absolute capability-absence and
/// benign/decoy verdicts — and deliberately EXCLUDES wall-proof language
/// ("statically unreachable") which has its own posted-attempts gate and has
/// scored correct when asserted with evidence.
///
/// # Arguments
///
/// * `body` - The report body to scan
fn absolute_verdicts(body: &str) -> Vec<String> {
    let pattern = concat!(
        "proven (incapable|absent|benign|harmless)",
        "|provably (benign|harmless|inert)",
        "|no (functional )?malicious (capability|functionality|code)",
        "|genuine (utility|tool|application)",
        "|exfil-only",
        "|no tasking",
        "|cannot supply commands",
        "|incapable of",
        "|benign decoy",
        "|decoy host",
    );
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .expect("a valid verdict regex");
    let mut flags: Vec<String> = Vec::new();
    for found in re.find_iter(body) {
        // quote the phrase inside a window of surrounding prose so the
        // calibration turn can locate the exact sentence
        let start = body[..found.start()]
            .char_indices()
            .rev()
            .take(60)
            .last()
            .map_or(found.start(), |(idx, _)| idx);
        let end = body[found.end()..]
            .char_indices()
            .take(61)
            .last()
            .map_or(found.end(), |(idx, _)| found.end() + idx);
        let context = body[start..end].replace('\n', " ");
        // one flag per phrase occurrence, bounded so the prompt stays small
        flags.push(format!("\"{}\" in: …{}…", found.as_str(), context.trim()));
        if flags.len() >= MAX_VERDICT_FLAGS {
            break;
        }
    }
    flags
}

/// Canonicalize one extracted field value for conflict comparison
///
/// An audit recount showed every false conflict fire was a formatting
/// variant of agreeing values — `0xB5297A4D` vs `0xb5297a4d`, `0x3E8(1000`
/// vs `0x3e8`, `0x00402480}` vs `0x402480}`, `−20` vs `-20` — each burning
/// the single reconciliation turn on a non-conflict. Canonicalization keeps
/// the comparison about the claimed value, not its rendering: parenthetical
/// glosses and trailing prose brackets are stripped, case is folded, the
/// unicode minus maps to ASCII, and pure numerics are radix-normalized so a
/// hex and a decimal rendering of the same number agree.
///
/// # Arguments
///
/// * `raw` - The raw value text captured after the `=`
fn canonical_field_value(raw: &str) -> String {
    // a truncated render (ellipsis) is not a comparable value at all
    if raw.contains('…') {
        return String::new();
    }
    // a parenthetical or bracketed gloss glued to a value belongs to the
    // prose, as does an index/range suffix (`dk[16..23` vs `dk[16..24` is a
    // notation variant, not two values)
    let raw = raw.split(['(', '[']).next().unwrap_or(raw);
    // sentence punctuation or a stray closing bracket after a value is prose
    let raw = raw.trim_end_matches(['.', ':', '!', '?', '}', ']']);
    // markdown emphasis around a value is rendering, not the value (the
    // appendix bolds table values: `recv` vs `**recv**` must agree)
    let raw = raw.trim_matches('*');
    // fold case and map the unicode minus onto ASCII before comparing
    let lowered = raw.to_lowercase().replace('\u{2212}', "-");
    // radix-normalize pure numerics so 0x3E8, 0x3e8, and 1000 all agree
    if let Some(hex) = lowered.strip_prefix("0x") {
        if (1..=32).contains(&hex.len()) && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(value) = u128::from_str_radix(hex, 16) {
                return value.to_string();
            }
        }
    }
    if (1..=32).contains(&lowered.len()) && lowered.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(value) = lowered.parse::<u128>() {
            return value.to_string();
        }
    }
    lowered
}

/// Extract `key=value` claims from report text
///
/// Keys are lowercased and values canonicalized (case, radix, glued prose)
/// before comparison; keys carrying more than four distinct values in one
/// text are dropped as enumerations (option lists, protocol grammars) rather
/// than claims.
///
/// # Arguments
///
/// * `text` - The report body or appendix text
fn field_claims(
    text: &str,
) -> std::collections::HashMap<String, std::collections::HashSet<String>> {
    // the value may not START with `=` so comparison syntax (`a==b`) never
    // reads as an assignment claim of `=b`; interior/trailing `=` stays legal
    // because base64 values carry padding
    let re =
        regex::Regex::new(r#"([A-Za-z_][A-Za-z0-9_-]{1,24})=([^\s,;)&"'`=][^\s,;)&"'`]{0,63})"#)
            .expect("a valid field regex");
    let mut claims: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for caps in re.captures_iter(text) {
        // compare the claimed value, not its formatting
        let value = canonical_field_value(&caps[2]);
        if value.is_empty() {
            continue;
        }
        claims
            .entry(caps[1].to_lowercase())
            .or_default()
            .insert(value);
    }
    // an enumeration is vocabulary, not a claim about one field's value
    claims.retain(|_, values| values.len() <= 4);
    claims
}

/// The body-vs-appendix field conflicts worth a reconciliation pass
///
/// A key claimed in BOTH texts whose value sets are disjoint is a conflict:
/// the deterministic appendix carries machine-rendered evidence, so a body
/// that asserts a different value for the same field is either wrong or
/// under-explained (a report shipped `login=1606` in a body whose own appendix
/// said `login=<botid>`, and the wrong version reached the executive
/// summary and IOC table).
///
/// # Arguments
///
/// * `body` - The model-written report body
/// * `appendix` - The deterministic forum-evidence appendix
fn field_conflicts(body: &str, appendix: &str) -> Vec<String> {
    let body_claims = field_claims(body);
    let appendix_claims = field_claims(appendix);
    let mut conflicts: Vec<String> = body_claims
        .iter()
        .filter_map(|(key, body_values)| {
            let appendix_values = appendix_claims.get(key)?;
            // any shared value means the body agrees with the evidence
            if body_values.is_disjoint(appendix_values) {
                let mut body_list: Vec<&str> = body_values.iter().map(String::as_str).collect();
                let mut appendix_list: Vec<&str> =
                    appendix_values.iter().map(String::as_str).collect();
                body_list.sort_unstable();
                appendix_list.sort_unstable();
                Some(format!(
                    "`{key}`: the body says {} but the appendix evidence says {}",
                    body_list.join(" / "),
                    appendix_list.join(" / ")
                ))
            } else {
                None
            }
        })
        .collect();
    conflicts.sort();
    conflicts.truncate(MAX_FIELD_CONFLICTS);
    conflicts
}

/// Generates a markdown report over a ThorPal instance's session state
///
/// Runs a bare one-shot agent (no tools, no preamble, no memory) over the
/// session's conversation transcript and rendered forum, structured by the
/// given template or the builtin default when none is supplied, then attaches
/// the deterministic forum-evidence appendix below whatever the model wrote.
/// Behind a content-filtering proxy the generation climbs the reactive ladder
/// (see [`generate_with`]); each ladder request bills its usage under the
/// `report` ledger key and each degradation is announced as a
/// [`ThorPalEvent::ReportDegraded`].
///
/// # Arguments
///
/// * `thorpal` - The ThorPal instance whose state the report covers
/// * `template` - A template overriding the builtin default, if any
#[instrument(name = "report::generate", skip_all, err(Debug))]
pub(crate) async fn generate(
    thorpal: &ThorPal,
    template: Option<&str>,
) -> Result<String, anyhow::Error> {
    // fall back to the builtin template when no override was supplied
    let template = template.unwrap_or(BUILTIN_REPORT_TEMPLATE);
    // render the conversation history as a plain transcript
    let transcript = render_transcript(&thorpal.snapshot().await.transcript());
    // render the whole forum so the report can draw on the discussion
    let forum = thorpal.render_forum().await;
    // a builder with no tools added yields an agent that can only reply with
    // text; build it through the retrying wrapper so a throttle at the very
    // end of a run paces the request instead of losing the whole report. The
    // Surface policy keeps content-filter kills out of the retry-level scrub
    // ladder: this path owns the smarter recovery
    let agent = thorpal
        .gate
        .retrying_agent_with_policy(thorpal.provider.model(), "report", ScrubPolicy::Surface)
        .max_tokens(REPORT_MAX_TOKENS)
        .build();
    // each ladder request runs through the same one-shot agent; extended
    // details carry the usage so every rung bills under the report's own key
    let agent = &agent;
    let prompt_fn = move |prompt: String| async move {
        // one completion round trip per ladder request
        let response = agent.prompt(prompt).extended_details().await?;
        // fold this request's usage into the shared ledger as it lands
        thorpal.usage.record("report", response.usage).await;
        // mirror the usage onto the event sink so live readouts track it
        if let Some(tx) = &thorpal.events {
            let _ = tx.send(ThorPalEvent::UsageUpdated {
                agent: "report".to_owned(),
                input: response.usage.input_tokens,
                output: response.usage.output_tokens,
                cached: response.usage.cached_input_tokens,
                cache_creation: response.usage.cache_creation_input_tokens,
                total: response.usage.total_tokens,
            });
        }
        Ok(response.output)
    };
    // climb the ladder as far as the proxy forces, keeping every degradation
    let (report, notes) = generate_with(&prompt_fn, template, &transcript, &forum).await?;
    // announce each degradation so operators and the transcript can see what
    // the written report lost (and audits can count the class mechanically)
    for note in notes {
        // one warning per degradation for the file log
        tracing::warn!(rung = note.rung, detail = %note.detail, "report generation degraded");
        if let Some(tx) = &thorpal.events {
            let _ = tx.send(ThorPalEvent::ReportDegraded {
                rung: note.rung,
                detail: note.detail,
            });
        }
    }
    // render the mechanical forum-evidence appendix over the live forum
    let appendix = thorpal.forum().report_appendix().await;
    // pre-freeze contradiction gate: a body field=value claim the appendix
    // evidence contradicts gets exactly one reconciliation turn before the
    // report freezes (the `login=1606` class once survived into the executive
    // summary because body and appendix were never cross-checked)
    let report = match &appendix {
        Some(appendix_text) => {
            let conflicts = field_conflicts(&report, appendix_text);
            if conflicts.is_empty() {
                report
            } else {
                tracing::warn!(
                    conflicts = conflicts.len(),
                    "reconciling body-vs-appendix field conflicts before the report freezes"
                );
                let reconcile = format!(
                    "The draft report below contradicts its own machine-generated \
                     evidence appendix on these field values:\n- {}\n\nRewrite the \
                     FULL report body, correcting each conflicted claim against the \
                     appendix evidence (the appendix is machine-rendered from the \
                     session's forum). Where both values are genuinely correct in \
                     different contexts, keep both and disambiguate the wording. \
                     Change nothing else; return only the corrected report body.\n\n\
                     --- DRAFT REPORT ---\n{report}\n\n--- APPENDIX EVIDENCE \
                     (relevant excerpts are inside) ---\n{appendix_text}",
                    conflicts.join("\n- ")
                );
                match prompt_fn(reconcile).await {
                    Ok(fixed) => fixed,
                    // a failed reconciliation keeps the draft: a report with a
                    // flagged conflict beats no report at the end of a run
                    Err(err) => {
                        tracing::warn!(
                            error = %summarize_prompt_error(&err),
                            "the reconciliation pass failed; keeping the draft report"
                        );
                        report
                    }
                }
            }
        }
        None => report,
    };
    // verdict-calibration gate: an absolute benign/incapable verdict gets
    // exactly one draft-only calibration turn before the report freezes
    // (dev-10 measured the preamble rule alone failing to reach the writer:
    // the "proven incapable" class survived a third run)
    let flags = absolute_verdicts(&report);
    let report = if flags.is_empty() {
        report
    } else {
        tracing::warn!(
            flags = flags.len(),
            "calibrating absolute verdict language before the report freezes"
        );
        let calibrate = format!(
            "The draft report below asserts absolute benign/incapable verdict \
             phrases:\n- {}\n\n\"Not proven present\" and \"proven absent\" are \
             different claims. For each flagged phrase, EITHER keep it — only \
             if the draft already contains its exclusion accounting (what was \
             ruled out capability-by-capability, how, and the strongest \
             alternative the evidence does NOT exclude) adjacent to or clearly \
             referenced by the claim — OR rewrite that sentence to the \
             calibrated form (\"no X demonstrated\", \"X not proven present\") \
             naming what remains unexcluded. Never weaken any claim whose \
             exclusion accounting is already present, never touch positive \
             findings or wall proofs that cite posted attempts, and never \
             invent evidence not in the draft. Change nothing else; return \
             only the corrected full report body.\n\n--- DRAFT REPORT ---\n\
             {report}",
            flags.join("\n- ")
        );
        match prompt_fn(calibrate).await {
            Ok(fixed) => fixed,
            // a failed calibration keeps the draft: a report with a flagged
            // verdict beats no report at the end of a run
            Err(err) => {
                tracing::warn!(
                    error = %summarize_prompt_error(&err),
                    "the verdict-calibration pass failed; keeping the draft report"
                );
                report
            }
        }
    };
    // attach the appendix (if anything qualified) below the written report
    Ok(attach_appendix(report, appendix))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rig::completion::CompletionError;

    use super::*;

    /// An appendix is attached after a blank-line separator and a missing one
    /// leaves the report untouched
    #[test]
    fn attach_appendix_separates_report_and_appendix() {
        // an appendix lands below the report after exactly one blank line
        let joined = attach_appendix(
            "report body".to_string(),
            Some("---\n\n## Appendix".to_string()),
        );
        assert_eq!(joined, "report body\n\n---\n\n## Appendix");
        // no appendix leaves the report exactly as the model wrote it
        let untouched = attach_appendix("report body".to_string(), None);
        assert_eq!(untouched, "report body");
    }

    /// The builtin template splits on `## ` headings and reassembles byte-exact
    #[test]
    fn split_sections_round_trips_the_builtin_template() {
        // the builtin template is the shape rung 2 will actually split
        let sections = split_sections(BUILTIN_REPORT_TEMPLATE);
        assert!(
            sections.len() >= 2,
            "the builtin template must have multiple sections, got {}",
            sections.len()
        );
        // every section after the first opens with a heading line
        for section in &sections[1..] {
            assert!(
                section.starts_with("## "),
                "section must open with a heading"
            );
        }
        // the concatenation round-trips the template byte-exactly
        assert_eq!(sections.concat(), BUILTIN_REPORT_TEMPLATE);
        // preamble text before the first heading joins the first section
        let with_preamble = "intro text\n## A\nbody\n## B\nbody\n";
        let sections = split_sections(with_preamble);
        assert_eq!(sections[0], "intro text\n## A\nbody\n");
        assert_eq!(sections[1], "## B\nbody\n");
        assert_eq!(sections.concat(), with_preamble);
    }

    /// A scripted prompt function popping one result per ladder request
    ///
    /// # Arguments
    ///
    /// * `results` - The result each successive request observes
    /// * `prompts` - The log every observed prompt is pushed onto
    fn scripted(
        results: Vec<Result<String, PromptError>>,
        prompts: &Mutex<Vec<String>>,
    ) -> impl Fn(String) -> std::future::Ready<Result<String, PromptError>> + '_ {
        // pop results front-first as the ladder sends requests
        let script = Mutex::new(results);
        move |prompt: String| {
            // record what the ladder actually sent
            prompts.lock().unwrap().push(prompt);
            // hand out the next scripted result
            std::future::ready(script.lock().unwrap().remove(0))
        }
    }

    /// A content-filter prompt error with an observed block-page shape
    fn blocked() -> PromptError {
        PromptError::CompletionError(CompletionError::ProviderResponse(
            rig::ProviderResponseError::new(
                http::StatusCode::FORBIDDEN,
                "<!--# nonce-->\n<!DOCTYPE HTML PUBLIC><html><title>Zscaler</title></html>",
            ),
        ))
    }

    /// A content-filter kill defangs the request and refangs the reply
    #[tokio::test]
    async fn ladder_defangs_then_refangs_on_a_content_filter_block() {
        let prompts = Mutex::new(Vec::new());
        // the pristine one-shot is blocked; the defanged retry succeeds with a
        // reply that keeps the defanged forms (as the note instructs)
        let prompt_fn = scripted(
            vec![
                Err(blocked()),
                Ok("c2 is hxxp[://]evil[.]com and 10[.]0[.]0[.]1".to_owned()),
            ],
            &prompts,
        );
        let (report, notes) = generate_with(
            prompt_fn,
            "## Findings\n",
            "saw http://evil.com",
            "ip 10.0.0.1",
        )
        .await
        .expect("the ladder should recover");
        // the stored report carries the refanged (pristine-form) indicators
        assert_eq!(report, "c2 is http://evil.com and 10.0.0.1");
        // one degradation to rung 1 was recorded
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].rung, 1);
        // the first prompt was pristine, the second defanged and instructed
        let prompts = prompts.lock().unwrap();
        assert!(prompts[0].contains("http://evil.com"));
        assert!(!prompts[0].contains("defanged for transit"));
        assert!(prompts[1].contains("hxxp[://]evil[.]com"));
        assert!(prompts[1].contains("10[.]0[.]0[.]1"));
        assert!(prompts[1].contains("defanged for transit"));
    }

    /// Two kills escalate to per-section generation with placeholders
    #[tokio::test]
    async fn ladder_degrades_to_sections_with_placeholders() {
        let prompts = Mutex::new(Vec::new());
        // both one-shots are blocked; section A succeeds, section B is still
        // blocked, section C succeeds
        let template = "## A\nguide a\n## B\nguide b\n## C\nguide c\n";
        let prompt_fn = scripted(
            vec![
                Err(blocked()),
                Err(blocked()),
                Ok("## A\n\nfound hxxp[://]evil[.]com".to_owned()),
                Err(blocked()),
                Ok("## C\n\nall clear".to_owned()),
            ],
            &prompts,
        );
        let (report, notes) = generate_with(prompt_fn, template, "transcript", "forum")
            .await
            .expect("the ladder should assemble a report");
        // the sections land in template order, refanged, with B degraded
        assert_eq!(
            report,
            "## A\n\nfound http://evil.com\n\n## B\n\n*[section unavailable: the \
             completion request was blocked by a content-filtering proxy]*\n\n\
             ## C\n\nall clear"
        );
        // two escalations plus one placeholder were recorded
        let rungs: Vec<u8> = notes.iter().map(|note| note.rung).collect();
        assert_eq!(rungs, vec![1, 2, 2]);
        // five requests total: two one-shots and three sections
        let prompts = prompts.lock().unwrap();
        assert_eq!(prompts.len(), 5);
        assert!(prompts[2].contains("ONE section"));
        assert!(prompts[3].contains("## B\nguide b\n"));
    }

    /// A non-filter failure mid-rung-2 fails the report, never a placeholder
    #[tokio::test]
    async fn rung_two_non_filter_errors_fail_the_report() {
        let prompts = Mutex::new(Vec::new());
        // both one-shots are blocked; section A succeeds, then the provider
        // dies with a non-filter fault (e.g. an aborted auth pause)
        let prompt_fn = scripted(
            vec![
                Err(blocked()),
                Err(blocked()),
                Ok("## A\n\nfine".to_owned()),
                Err(PromptError::CompletionError(
                    CompletionError::ProviderError("aborted".to_owned()),
                )),
            ],
            &prompts,
        );
        let err = generate_with(prompt_fn, "## A\nga\n## B\ngb\n", "transcript", "forum")
            .await
            .expect_err("a dead provider must fail the report, not placeholder it");
        // the original fault surfaces so the run exits nonzero/75 and retries
        assert!(err.to_string().contains("aborted"));
        // nothing was sent past the failing section
        assert_eq!(prompts.lock().unwrap().len(), 4);
    }

    /// A non-filter failure propagates unchanged without entering the ladder
    #[tokio::test]
    async fn non_filter_errors_propagate_unladdered() {
        let prompts = Mutex::new(Vec::new());
        // a plain provider fault on the pristine one-shot
        let prompt_fn = scripted(
            vec![Err(PromptError::CompletionError(
                CompletionError::ProviderError("boom".to_owned()),
            ))],
            &prompts,
        );
        let err = generate_with(prompt_fn, "## A\n", "transcript", "forum")
            .await
            .expect_err("a non-filter failure must propagate");
        // the error is the original fault and only one request was sent
        assert!(err.to_string().contains("boom"));
        assert_eq!(prompts.lock().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod field_conflict_tests {
    use super::*;

    /// Disjoint body-vs-appendix values conflict; agreement and noise do not
    #[test]
    fn conflicts_flag_disjoint_claims_only() {
        // the observed class: the body asserts a value the appendix contradicts
        let body = "The beacon is cmd=getload&login=1606 with sel=1606.";
        let appendix = "Evidence: cmd=getload&login=<botid> and sel=1606 at !33.";
        let conflicts = field_conflicts(body, appendix);
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
        assert!(conflicts[0].starts_with("`login`"), "{conflicts:?}");
        // agreement on every shared key means no conflicts at all
        assert!(field_conflicts(body, body).is_empty());
        // an enumeration-heavy key is vocabulary, not a claim
        let noisy = "ops: op=a op=b op=c op=d op=e";
        assert!(field_conflicts(noisy, "op=z").is_empty());
        // a key present on only one side cannot conflict
        assert!(field_conflicts("alpha=1", "beta=2").is_empty());
    }

    /// Pins the observed false-fire shapes: formatting variants of agreeing
    /// values must not conflict, while genuinely different values still do
    #[test]
    fn conflicts_ignore_case_radix_and_gloss_variants() {
        // hex case variants of the same constant agree
        assert!(field_conflicts("key k1=0xB5297A4D here", "k1=0xb5297a4d at !7").is_empty());
        // a parenthetical gloss glued to the value is prose, not a new value
        assert!(field_conflicts("nid=0x3E8(1000 decoded", "nid=0x3e8 raw").is_empty());
        // leading zeros and a stray closing brace are formatting
        assert!(field_conflicts("{pfn=0x00402480} table", "{pfn=0x402480} dump").is_empty());
        // hex and decimal renderings of one number agree
        assert!(field_conflicts("port=0x3e8 beacon", "port=1000 beacon").is_empty());
        // the unicode minus renders identically to ASCII in terminals
        assert!(field_conflicts("gwl_exstyle=\u{2212}20 set", "gwl_exstyle=-20 set").is_empty());
        // an index/range suffix is notation, not a second value
        assert!(field_conflicts("iv=dk[16..23 slice", "iv=dk[16..24 and iv=dk[16:24").is_empty());
        // comparison syntax is not an assignment claim of `=b`
        assert!(field_conflicts("branch if len==size holds", "len=7 bytes read").is_empty());
        // an ellipsis-truncated render is not a comparable value
        assert!(field_conflicts("key=d0eb…-1 shown", "key=d0eb213e1cf14854dbe1 full").is_empty());
        // markdown-bold table values are rendering, not disagreement
        assert!(field_conflicts("ord16=recv in body", "| ord16=**recv** | table").is_empty());
        // canonicalization must not hide a real value disagreement
        let conflicts = field_conflicts("login=0x646 beacon", "login=0x647 beacon");
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");
    }

    /// Pins the dev-10 absolute-verdict vocabulary: observed wrong-claim
    /// phrases flag with context, wall-proof language and clean prose do not
    #[test]
    fn absolute_verdicts_flag_observed_phrases_only() {
        // the three dev-10 survivors flag, case-insensitively, with context
        let body = "The host is PROVEN INCAPABLE of harm.\nThe sibling is a \
                    functioning genuine utility.\nThe channel is exfil-only.";
        let flags = absolute_verdicts(body);
        assert_eq!(flags.len(), 3, "{flags:?}");
        assert!(flags[0].contains("PROVEN INCAPABLE"), "{flags:?}");
        assert!(flags[1].contains("genuine utility"), "{flags:?}");
        // wall-proof language is deliberately out of scope: it has its own
        // posted-attempts gate and has scored correct when evidence-backed
        let wall = "The payload is statically unreachable: entry-stub \
                    disassembly and trial decodes are posted at !12.";
        assert!(absolute_verdicts(wall).is_empty());
        // ordinary capability prose never fires
        let clean = "The bot downloads modules over HTTPS and persists via a \
                     Run key; no decoy content was found in the overlay.";
        assert!(absolute_verdicts(clean).is_empty());
        // the flag count is bounded for the calibration prompt
        let noisy = "decoy host. ".repeat(40);
        assert_eq!(absolute_verdicts(&noisy).len(), MAX_VERDICT_FLAGS);
    }
}
