//! Runs detector-flagged library Rhai scripts as detections at seed time.
//!
//! Library scripts promoted with `detector: true` (see
//! [`ScriptLibrary`](crate::thorpal::store::library::ScriptLibrary)) execute
//! automatically while the native Rust detections run, so a scan an analyst's
//! past run proved useful keeps paying off on every new sample. Each script
//! runs sandboxed exactly like `run_script` -- every registered read-only Rhai
//! binding, the shared operation cap, and a wall-clock deadline -- and its
//! final expression is converted into [`DetectionHit`]s (tagged
//! [`HitSource::Script`]) that flow through the same forum seeding and
//! derived-question pipeline as the native hits.
//!
//! The expected return contract (documented to the model by `promote_script`):
//! either a bare array of hit maps, or a map `#{ hits: [...], questions:
//! [...] }` whose questions are seeded directly as open question posts. A hit
//! map requires `address`, `content`, and `reasoning`; `confidence`
//! (low/medium/high), `technique` + `technique_name`, `tactic`, `function`,
//! and `sites` are optional. A question map requires `name`, `title`, and
//! `body`, plus an optional `tasks` array of non-empty strings (at most
//! [`MAX_SEED_TASKS_PER_QUESTION`] kept) seeded as the question's
//! completion-gate checklist. Failures are logged and skipped per script, like
//! [`detections::run_all`](super::detections::run_all), so one broken script
//! never sinks the seed pass.

use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::thorpal::detect::context::AnalysisContext;
use crate::thorpal::detect::detections::{
    AttackTactic, AttackTechnique, DetectionHit, HitSource, high, low, medium,
};
use crate::thorpal::seed::{MAX_SEED_TASKS_PER_QUESTION, SeedQuestion};
use crate::thorpal::tools::registry;
use crate::thorpal::tools::scripts::{
    DEFAULT_TIMEOUT_SECS, MAX_OPERATIONS, ScriptHost, execute_script,
};

/// The greatest number of detector scripts run per session start.
///
/// Detector scripts run sequentially before the agent loop, each bounded by
/// [`DETECTOR_TIMEOUT_SECS`], so this caps the worst-case seed latency a
/// crowded library can impose.
const MAX_DETECTOR_SCRIPTS: usize = 8;

/// The per-script wall-clock deadline, matching `run_script`'s default.
const DETECTOR_TIMEOUT_SECS: u64 = DEFAULT_TIMEOUT_SECS;

/// The greatest number of seeded questions one detector script may emit.
///
/// Every open question forces the main agent to delegate work, so a single
/// over-eager script must not flood the question queue the derived questions
/// also share.
const MAX_SCRIPT_QUESTIONS: usize = 2;

/// The fallback technique for script hits that name no ATT&CK technique.
const SCRIPT_TECHNIQUE: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("SCRIPT"),
    name: Cow::Borrowed("Library Script Finding"),
    tactic: AttackTactic::Discovery,
};

/// A detector-flagged library script ready to run at seed time.
pub struct ScriptDetector {
    /// The library script's name as the run renders it (used for logging and
    /// hit provenance; blinded runs supply the neutral id here)
    pub name: String,
    /// The script's full source, doc header included (headers are comments)
    pub source: String,
    /// The blinding redactor the script's returned values render through, if
    /// any -- a library detector's own hardcoded text rides a library surface
    pub redactor: Option<std::sync::Arc<crate::thorpal::store::redact::LibraryRedactor>>,
}

/// Run every detector script and collect the hits and questions they emit.
///
/// Scripts run sequentially on blocking threads (rhai values are `!Send`),
/// each with every registered Rhai binding, the shared operation cap, and a
/// wall-clock deadline. A script that fails, times out, or returns a value
/// outside the contract is logged at ERROR and skipped -- mirroring the native
/// driver's per-detection failure isolation -- and malformed individual
/// entries are warned about and dropped without discarding the script's other
/// results.
///
/// # Arguments
///
/// * `ctx` - The shared analysis context (used to resolve containing functions)
/// * `host` - The host handle the scripts' Ghidra bindings call back into
/// * `detectors` - The detector-flagged library scripts to run
#[tracing::instrument(name = "script_detections::run_script_detectors", skip_all)]
pub async fn run_script_detectors(
    ctx: &AnalysisContext,
    host: &ScriptHost,
    detectors: &[ScriptDetector],
) -> (Vec<DetectionHit>, Vec<SeedQuestion>) {
    // cap how many scripts run so a crowded library cannot stall the seed pass
    if detectors.len() > MAX_DETECTOR_SCRIPTS {
        tracing::warn!(
            total = detectors.len(),
            cap = MAX_DETECTOR_SCRIPTS,
            "too many detector scripts; running only the first ones"
        );
    }
    // every registered binding is safe here: the registry guarantees mutating
    // tools never carry a Rhai binding, so detector scripts stay read-only
    let bindings: Vec<fn(&mut rhai::Engine, &ScriptHost)> = registry::all()
        .iter()
        .filter_map(|entry| entry.rhai_bind)
        .collect();
    // tell the scripts they are running as detectors, not under an agent
    let args = serde_json::json!({ "mode": "detector" });
    // accumulate every script's hits and questions in library order
    let mut hits = Vec::new();
    let mut questions = Vec::new();
    for detector in detectors.iter().take(MAX_DETECTOR_SCRIPTS) {
        // run the script on a blocking thread exactly like run_script does
        let source = detector.source.clone();
        let args = args.clone();
        let bindings = bindings.clone();
        let host = host.clone();
        let deadline = Instant::now() + Duration::from_secs(DETECTOR_TIMEOUT_SECS);
        // print output streams live under the rhai_script tracing target; the
        // captured buffer is dropped since no tool result carries it here
        let output = Arc::new(Mutex::new(Vec::new()));
        let evaled = tokio::task::spawn_blocking(move || {
            execute_script(
                &source,
                &args,
                &bindings,
                &host,
                deadline,
                MAX_OPERATIONS,
                &output,
            )
        })
        .await;
        // unwrap the run, logging and skipping any failure like run_all does
        let mut value = match evaled {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                tracing::error!(
                    script = %detector.name,
                    %error,
                    "detector script failed; skipping and continuing"
                );
                continue;
            }
            Err(error) => {
                tracing::error!(
                    script = %detector.name,
                    %error,
                    "detector script crashed its thread; skipping and continuing"
                );
                continue;
            }
        };
        // scrub deny-listed tokens out of the script's returned strings under
        // blinding, before any of them become forum-visible hit or question
        // text (sample strings stay readable through the agent's own tools)
        if let Some(redactor) = &detector.redactor {
            redactor.redact_json(&mut value);
        }
        // convert the returned value into hits/questions under the contract
        match outcome_from_value(&value, |addr| ctx.containing_function(addr), &detector.name) {
            Ok((found_hits, found_questions)) => {
                // log one concise line with the script's yield, like run_all
                tracing::info!(
                    script = %detector.name,
                    hits = found_hits.len(),
                    questions = found_questions.len(),
                    "detector script complete"
                );
                hits.extend(found_hits);
                questions.extend(found_questions);
            }
            Err(error) => {
                tracing::error!(
                    script = %detector.name,
                    %error,
                    "detector script returned a value outside the contract; skipping"
                );
            }
        }
    }
    (hits, questions)
}

/// Convert a detector script's returned JSON into hits and seeded questions.
///
/// Pure (no Ghidra access) so the contract is unit-testable: the containing
/// function resolution is injected. Accepts either a bare array of hit maps or
/// a `{ hits: [...], questions: [...] }` map; anything else is an error naming
/// the contract. Malformed entries are warned about and skipped individually.
///
/// # Arguments
///
/// * `value` - The script's final expression, serialized to JSON
/// * `resolve_function` - Resolves an address to its containing function entry
/// * `script` - The script's name, stamped into hit reasoning and warnings
fn outcome_from_value(
    value: &serde_json::Value,
    resolve_function: impl Fn(u64) -> Option<u64>,
    script: &str,
) -> Result<(Vec<DetectionHit>, Vec<SeedQuestion>), String> {
    // split the two accepted shapes into their raw hit/question lists
    let (raw_hits, raw_questions) = match value {
        // a bare array is the hits-only shorthand
        serde_json::Value::Array(hits) => (hits.as_slice(), [].as_slice()),
        // the map shape carries optional hits and questions arrays
        serde_json::Value::Object(map) => {
            let hits = match map.get("hits") {
                Some(serde_json::Value::Array(hits)) => hits.as_slice(),
                None => [].as_slice(),
                Some(_) => return Err("`hits` must be an array of hit maps".to_owned()),
            };
            let questions = match map.get("questions") {
                Some(serde_json::Value::Array(questions)) => questions.as_slice(),
                None => [].as_slice(),
                Some(_) => return Err("`questions` must be an array of question maps".to_owned()),
            };
            (hits, questions)
        }
        // anything else is outside the contract entirely
        _ => {
            return Err(
                "a detector script's final expression must be an array of hit maps \
                 (each with address, content, reasoning) or a map #{ hits: [...], \
                 questions: [...] }"
                    .to_owned(),
            );
        }
    };
    // parse each hit, warning on and skipping malformed entries individually
    let mut hits = Vec::new();
    for raw in raw_hits {
        match parse_hit(raw, &resolve_function, script) {
            Ok(hit) => hits.push(hit),
            Err(error) => {
                tracing::warn!(script, %error, "skipping a malformed detector script hit");
            }
        }
    }
    // parse each question the same way, bounded per script
    let mut questions = Vec::new();
    for raw in raw_questions {
        // enforce the per-script question cap before parsing further entries
        if questions.len() == MAX_SCRIPT_QUESTIONS {
            tracing::warn!(
                script,
                cap = MAX_SCRIPT_QUESTIONS,
                "detector script emitted too many questions; dropping the rest"
            );
            break;
        }
        match parse_question(raw) {
            Ok(question) => questions.push(question),
            Err(error) => {
                tracing::warn!(script, %error, "skipping a malformed detector script question");
            }
        }
    }
    Ok((hits, questions))
}

/// Parse one hit map from a detector script's results.
///
/// # Arguments
///
/// * `raw` - The hit's JSON map
/// * `resolve_function` - Resolves an address to its containing function entry
/// * `script` - The script's name, suffixed onto the hit's reasoning
fn parse_hit(
    raw: &serde_json::Value,
    resolve_function: impl Fn(u64) -> Option<u64>,
    script: &str,
) -> Result<DetectionHit, String> {
    // every hit is a map; anything else cannot carry the required fields
    let map = raw.as_object().ok_or("a hit must be a map")?;
    // the three required fields: where, what, and why it matters
    let address = parse_address(map.get("address").ok_or("a hit needs an `address`")?)?;
    let content = required_str(map, "content")?;
    let reasoning = required_str(map, "reasoning")?;
    // the optional severity defaults to low, like a weak native indicator
    let severity = match map.get("confidence").and_then(|value| value.as_str()) {
        None | Some("low") => low(),
        Some("medium") => medium(),
        Some("high") => high(),
        Some(other) => {
            return Err(format!(
                "unknown confidence '{other}'; use \"low\", \"medium\", or \"high\""
            ));
        }
    };
    // an explicit technique needs both halves; otherwise the shared fallback
    let technique = match (
        map.get("technique").and_then(|value| value.as_str()),
        map.get("technique_name").and_then(|value| value.as_str()),
    ) {
        (Some(id), Some(name)) => {
            // the optional tactic string maps through the ATT&CK name parser
            let tactic = map
                .get("tactic")
                .and_then(|value| value.as_str())
                .and_then(AttackTactic::from_name)
                .unwrap_or(AttackTactic::Discovery);
            AttackTechnique::new(tactic, id.to_owned(), name.to_owned())
        }
        (None, _) => SCRIPT_TECHNIQUE,
        (Some(_), None) => {
            return Err("a hit with a `technique` also needs a `technique_name`".to_owned());
        }
    };
    // use the explicit containing function or resolve it from the address
    let function = match map.get("function") {
        Some(value) => Some(parse_address(value)?),
        None => resolve_function(address),
    };
    // collect the optional backing sites list
    let sites = match map.get("sites") {
        Some(serde_json::Value::Array(sites)) => sites
            .iter()
            .map(parse_address)
            .collect::<Result<Vec<u64>, String>>()?,
        None => Vec::new(),
        Some(_) => return Err("`sites` must be an array of addresses".to_owned()),
    };
    // stamp the producing script into the reasoning so the forum shows provenance
    let reasoning = format!("{reasoning} (library script `{script}`)");
    // assemble the hit, tagged as script-produced
    Ok(
        DetectionHit::new(technique, address, function, severity, content, reasoning)
            .source(HitSource::Script)
            .sites(sites),
    )
}

/// Parse one seeded question map from a detector script's results.
///
/// # Arguments
///
/// * `raw` - The question's JSON map
fn parse_question(raw: &serde_json::Value) -> Result<SeedQuestion, String> {
    // every question is a map; anything else cannot carry the required fields
    let map = raw.as_object().ok_or("a question must be a map")?;
    // all three fields are required: the reference name, title, and body
    let name = required_str(map, "name")?;
    let title = required_str(map, "title")?;
    let body = required_str(map, "body")?;
    // question names become forum post references, so they must stay short
    // and space-free exactly like script names
    crate::thorpal::store::validate_script_name(&name)
        .map_err(|error| format!("bad question name '{name}': {error}"))?;
    // the optional tasks array becomes the question's completion-gate checklist;
    // one bad entry rejects the whole question since every kept task hard-gates
    // marking it solved
    let mut tasks = match map.get("tasks") {
        Some(serde_json::Value::Array(entries)) => entries
            .iter()
            .map(|entry| {
                entry
                    .as_str()
                    .map(str::trim)
                    .filter(|task| !task.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| "`tasks` must be an array of non-empty strings".to_owned())
            })
            .collect::<Result<Vec<String>, String>>()?,
        None => Vec::new(),
        Some(_) => return Err("`tasks` must be an array of non-empty strings".to_owned()),
    };
    // enforce the shared per-question task cap (the seeding path truncates to
    // the same constant, so warning here is the only place a drop is visible)
    if tasks.len() > MAX_SEED_TASKS_PER_QUESTION {
        tracing::warn!(
            question = %name,
            cap = MAX_SEED_TASKS_PER_QUESTION,
            "detector script question carries too many tasks; dropping the rest"
        );
        tasks.truncate(MAX_SEED_TASKS_PER_QUESTION);
    }
    Ok(SeedQuestion {
        name,
        title,
        body,
        tasks,
    })
}

/// Pull a required non-empty string field out of a hit/question map.
///
/// # Arguments
///
/// * `map` - The JSON map to read the field from
/// * `key` - The required field's key
fn required_str(
    map: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String, String> {
    // the field must exist, be a string, and say something
    map.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("`{key}` must be a non-empty string"))
}

/// Parse an address from a JSON integer or a hex string.
///
/// Strings parse as hex with an optional `0x` prefix (via the shared
/// [`parse_addr`](crate::thorpal::detect::context::parse_addr)), matching the
/// Ghidra bindings, which hand scripts addresses as bare hex strings like
/// `"0010c000"` -- so a hit can pass a binding's address field straight
/// through. Rhai ints are signed 64-bit, so addresses with the top bit set may
/// arrive as negative numbers and are reinterpreted bitwise.
///
/// # Arguments
///
/// * `value` - The JSON value to parse an address from
fn parse_address(value: &serde_json::Value) -> Result<u64, String> {
    match value {
        serde_json::Value::Number(number) => {
            // accept unsigned directly and reinterpret negative i64s bitwise
            if let Some(address) = number.as_u64() {
                Ok(address)
            } else if let Some(signed) = number.as_i64() {
                Ok(signed as u64)
            } else {
                Err(format!("'{number}' is not an integer address"))
            }
        }
        serde_json::Value::String(text) => {
            // strings are hex (optionally 0x-prefixed), exactly like the
            // addresses the Ghidra bindings hand back
            crate::thorpal::detect::context::parse_addr(text.trim())
                .ok_or_else(|| format!("'{text}' is not a hex address"))
        }
        other => Err(format!("'{other}' is not an address (int or hex string)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::confidence::Confidence;

    /// Resolves every test address into a fixed fake containing function.
    fn resolver(addr: u64) -> Option<u64> {
        // bucket by aligning down so the mapping is visibly derived
        Some(addr & !0xfff)
    }

    /// A bare array of hit maps parses as hits only.
    #[test]
    fn bare_array_parses_as_hits() {
        let value = serde_json::json!([
            {
                "address": 0x401010,
                "content": "tight XOR loop over a .data blob",
                "reasoning": "single-byte XOR decode before a call",
            },
        ]);
        let (hits, questions) = outcome_from_value(&value, resolver, "find-xor").expect("parse");
        assert!(questions.is_empty());
        assert_eq!(hits.len(), 1);
        // the required fields land with the defaults around them
        assert_eq!(hits[0].address, 0x401010);
        assert_eq!(hits[0].function, Some(0x401000));
        assert!(matches!(hits[0].confidence, Confidence::Untrusted));
        assert_eq!(hits[0].source, HitSource::Script);
        // the fallback technique and the provenance suffix are applied
        assert_eq!(hits[0].technique.id.as_ref(), "SCRIPT");
        assert!(hits[0].reasoning.contains("(library script `find-xor`)"));
    }

    /// The map shape parses hits and questions together.
    #[test]
    fn map_shape_parses_hits_and_questions() {
        let value = serde_json::json!({
            "hits": [{
                "address": "0x402000",
                "content": "c",
                "reasoning": "r",
                "confidence": "high",
                "technique": "T1027",
                "technique_name": "Obfuscated Files or Information",
                "tactic": "defense evasion",
                "function": "0x402000",
                "sites": [0x402000, "0x402010"],
            }],
            "questions": [{
                "name": "q-xor-blob",
                "title": "What does the XOR stub decode?",
                "body": "Decode the blob at 0x402000.",
            }],
        });
        let (hits, questions) = outcome_from_value(&value, resolver, "s").expect("parse");
        // the explicit technique, tactic, severity, and sites all land
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].technique.id.as_ref(), "T1027");
        assert_eq!(hits[0].technique.tactic, AttackTactic::DefenseEvasion);
        assert!(matches!(hits[0].confidence, Confidence::Likely));
        assert_eq!(hits[0].function, Some(0x402000));
        assert_eq!(hits[0].sites, [0x402000, 0x402010]);
        // the question comes through with its reference name
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-xor-blob");
    }

    /// Malformed entries are skipped without discarding the good ones.
    #[test]
    fn malformed_entries_are_skipped() {
        let value = serde_json::json!({
            "hits": [
                { "address": 0x1000, "content": "good", "reasoning": "r" },
                { "content": "missing address", "reasoning": "r" },
                { "address": 0x2000, "content": "bad confidence", "reasoning": "r",
                  "confidence": "certain" },
                { "address": 0x3000, "content": "half technique", "reasoning": "r",
                  "technique": "T1027" },
                "not a map",
            ],
            "questions": [
                { "name": "has spaces in it", "title": "t", "body": "b" },
                { "name": "q-ok", "title": "t", "body": "b" },
            ],
        });
        let (hits, questions) = outcome_from_value(&value, resolver, "s").expect("parse");
        // only the well-formed hit and question survive
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].address, 0x1000);
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-ok");
    }

    /// The per-script question cap drops the excess questions.
    #[test]
    fn question_cap_holds() {
        // one more question than the cap allows
        let raw: Vec<serde_json::Value> = (0..=MAX_SCRIPT_QUESTIONS)
            .map(|i| serde_json::json!({ "name": format!("q-{i}"), "title": "t", "body": "b" }))
            .collect();
        let value = serde_json::json!({ "questions": raw });
        let (hits, questions) = outcome_from_value(&value, resolver, "s").expect("parse");
        assert!(hits.is_empty());
        assert_eq!(questions.len(), MAX_SCRIPT_QUESTIONS);
    }

    /// A question's optional tasks array parses onto its checklist.
    #[test]
    fn map_shape_parses_question_tasks() {
        let value = serde_json::json!({
            "questions": [{
                "name": "q-xor-blob",
                "title": "What does the XOR stub decode?",
                "body": "Decode the blob at 0x402000.",
                "tasks": [
                    "Decode the blob at 0x402000 and post the plaintext as evidence",
                    "Identify the function that consumes the decoded bytes",
                ],
            }],
        });
        let (hits, questions) = outcome_from_value(&value, resolver, "s").expect("parse");
        assert!(hits.is_empty());
        // both tasks land on the parsed question in order
        assert_eq!(questions.len(), 1);
        assert_eq!(
            questions[0].tasks,
            [
                "Decode the blob at 0x402000 and post the plaintext as evidence",
                "Identify the function that consumes the decoded bytes",
            ]
        );
        // a question without the field simply carries no tasks
        let value = serde_json::json!({
            "questions": [{ "name": "q-bare", "title": "t", "body": "b" }],
        });
        let (_, questions) = outcome_from_value(&value, resolver, "s").expect("parse");
        assert!(questions[0].tasks.is_empty());
    }

    /// Bad task entries reject their whole question; excess tasks are capped.
    #[test]
    fn question_tasks_are_capped_and_validated() {
        // more valid tasks than the cap allows
        let over: Vec<String> = (0..MAX_SEED_TASKS_PER_QUESTION + 2)
            .map(|index| format!("check {index}"))
            .collect();
        let value = serde_json::json!({
            "questions": [
                // a non-string entry rejects the whole question
                { "name": "q-non-string", "title": "t", "body": "b", "tasks": ["ok", 42] },
                // a blank entry rejects the whole question too
                { "name": "q-blank", "title": "t", "body": "b", "tasks": ["  "] },
                // the over-long valid list is kept, truncated to the cap
                { "name": "q-capped", "title": "t", "body": "b", "tasks": over },
            ],
        });
        let (hits, questions) = outcome_from_value(&value, resolver, "s").expect("parse");
        assert!(hits.is_empty());
        // only the valid question survives, with its checklist capped
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].name, "q-capped");
        assert_eq!(questions[0].tasks.len(), MAX_SEED_TASKS_PER_QUESTION);
        assert_eq!(questions[0].tasks[0], "check 0");
    }

    /// A value outside the contract errors naming the expected shapes.
    #[test]
    fn wrong_shapes_error_with_the_contract() {
        // a scalar is outside the contract entirely
        let error = outcome_from_value(&serde_json::json!(42), resolver, "s")
            .expect_err("a scalar should be rejected");
        assert!(error.contains("array of hit maps"));
        // a map whose hits field is not an array is rejected too
        let error = outcome_from_value(&serde_json::json!({ "hits": 1 }), resolver, "s")
            .expect_err("a non-array hits field should be rejected");
        assert!(error.contains("`hits` must be an array"));
    }

    /// Addresses parse from ints, negative ints, and hex strings.
    #[test]
    fn address_forms_parse() {
        assert_eq!(parse_address(&serde_json::json!(0x1000)), Ok(0x1000));
        // a rhai int with the top bit set arrives negative and reinterprets
        assert_eq!(parse_address(&serde_json::json!(-1i64)), Ok(u64::MAX));
        assert_eq!(parse_address(&serde_json::json!("0x1000")), Ok(0x1000));
        // bare hex strings are exactly what the Ghidra bindings hand scripts
        assert_eq!(parse_address(&serde_json::json!("0010c000")), Ok(0x10c000));
        assert!(parse_address(&serde_json::json!("0xzz")).is_err());
        assert!(parse_address(&serde_json::json!(true)).is_err());
    }
}
