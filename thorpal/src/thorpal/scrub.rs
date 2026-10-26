//! Defangs IOCs for transit and scrubs completion requests past content filters.
//!
//! Two consumers share the transforms in this module. thorctl's `--defang`
//! flag rewrites report copies written to the operator's path into the
//! conventional non-clickable forms (`hxxp[://]`, `1[.]2[.]3[.]4`,
//! `evil[.]com`); [`refang`] is its exact inverse so a defanged model reply
//! can be restored to pristine form client-side. The retry layer (see
//! [`crate::thorpal::retry`]) additionally uses [`scrub_request`] to rewrite a
//! completion request whose payload a content-filtering proxy 403-killed:
//! evaluation runs lost many sub-agents to a Zscaler filter reacting to raw IOC and
//! malware text transiting in tool results, and a scrubbed retry keeps the
//! agent alive where the identical payload would die forever.
//!
//! The defang transform is deliberately conservative and lossy: schemes are
//! broken, IPv4s with in-range octets are bracketed, and bare domains are
//! bracketed only against a small TLD allowlist, so file names like
//! `kernel32.dll` are never touched while version-number strings that look
//! like addresses may be. Every pass is idempotent — defanging a defanged
//! report is a no-op. Scrubbed requests are built fresh from the pristine
//! request per attempt and are never persisted: session history and the
//! session store always keep the pristine text.

use std::sync::OnceLock;

use regex::{Captures, Regex};
use rig::completion::CompletionRequest;
use rig::message::{AssistantContent, Message, ReasoningContent, ToolResultContent, UserContent};

/// The TLDs the bare-domain pass defangs
///
/// Deliberately small: common legitimate TLDs plus the ones malware reports
/// actually name. File extensions (`dll`, `exe`, `dat`, …) are excluded by
/// construction, so Windows module names never match.
const DEFANG_TLDS: &str = "com|net|org|info|biz|io|ru|cn|su|top|xyz|online|site|club|onion";

/// The byte budget a tool result's text keeps under a deep scrub.
///
/// Oversized tool results (raw string dumps, decoded buffers) are the bulk
/// payload a content filter keys on; past this budget a deep scrub keeps the
/// head and tail and drops the middle with a marker note. Tunable against
/// evaluation observations.
const TOOL_RESULT_TEXT_CAP: usize = 16 * 1024;

/// The minimum run of hex characters a deep scrub replaces with a digest note.
///
/// Long enough that real registers, hashes, and addresses (at most 64 chars
/// for a sha256) always survive; shellcode and config dumps do not.
const HEX_BLOB_MIN: usize = 256;

/// The minimum run of base64 characters a deep scrub replaces with a digest note.
///
/// Longer than the hex floor since prose and identifiers sit inside the
/// base64 alphabet; half a KiB of unbroken base64 is a payload, not a word.
const B64_BLOB_MIN: usize = 512;

/// The compiled scheme-breaking pattern
static SCHEMES: OnceLock<Regex> = OnceLock::new();

/// The compiled defanged-URL authority pattern
static AUTHORITIES: OnceLock<Regex> = OnceLock::new();

/// The compiled IPv4 pattern
static IPS: OnceLock<Regex> = OnceLock::new();

/// The compiled bare-domain pattern
static DOMAINS: OnceLock<Regex> = OnceLock::new();

/// The compiled defanged-scheme restore pattern
static REFANG_SCHEMES: OnceLock<Regex> = OnceLock::new();

/// The compiled hex-blob pattern
static HEX_BLOBS: OnceLock<Regex> = OnceLock::new();

/// The compiled base64-blob pattern
static B64_BLOBS: OnceLock<Regex> = OnceLock::new();

/// Fetch or lazily compile one of the defang patterns
///
/// # Arguments
///
/// * `cell` - The cache slot holding the compiled pattern
/// * `pattern` - The pattern source to compile on first use
fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    // the patterns are static strings, so compilation can never fail at runtime
    cell.get_or_init(|| Regex::new(pattern).expect("static defang pattern compiles"))
}

/// Bracket every alphanumeric-bounded dot in a span
///
/// Only a dot with an ASCII alphanumeric on both sides brackets -- exactly
/// the forms [`refang`] restores -- so a trailing sentence period after a
/// URL (or a dot against a closing bracket) stays a plain dot and the
/// defang/refang pair stays an exact inverse. Already-bracketed `[.]` forms
/// are not alphanumeric-bounded, which keeps the pass idempotent.
///
/// # Arguments
///
/// * `span` - The text span to bracket
fn bracket_dots(span: &str) -> String {
    // walk bytes so the neighbor checks mirror refang's restore rule
    let bytes = span.as_bytes();
    let mut out = String::with_capacity(span.len() + 8);
    let mut index = 0;
    while index < bytes.len() {
        // copy the full UTF-8 char so multi-byte text survives the walk
        let ch = span[index..].chars().next().expect("in-bounds char");
        if ch == '.'
            && index > 0
            && bytes[index - 1].is_ascii_alphanumeric()
            && bytes.get(index + 1).is_some_and(u8::is_ascii_alphanumeric)
        {
            out.push_str("[.]");
        } else {
            out.push(ch);
        }
        index += ch.len_utf8();
    }
    out
}

/// Defang a report or request text's IOCs for transit
///
/// Applies four ordered, idempotent passes: URL schemes become `hxxp[://]`
/// forms, the defanged URLs' authority components get their dots bracketed,
/// in-range IPv4 addresses get `[.]` separators, and bare domains matching
/// the [`DEFANG_TLDS`] allowlist get their dots bracketed. Markdown is not
/// exempted anywhere — IOC tables and code spans are exactly where the live
/// indicators sit.
///
/// # Arguments
///
/// * `report` - The rendered text to defang
pub fn defang(report: &str) -> String {
    // break URL schemes first so the authority pass can key on the result
    let schemes = regex(&SCHEMES, r"(?i)\b(https?|ftp)://");
    let report = schemes.replace_all(report, |caps: &Captures| {
        // normalize every case spelling to the conventional lowercase forms
        match caps[1].to_ascii_lowercase().as_str() {
            "https" => "hxxps[://]",
            "http" => "hxxp[://]",
            _ => "fxp[://]",
        }
    });
    // bracket every dot in the defanged URLs' authority component (up to the
    // first path, query, fragment, or whitespace boundary)
    let authorities = regex(&AUTHORITIES, r"(?i)\b(hxxps?|fxp)\[://\]([^/\s?#]+)");
    let report = authorities.replace_all(&report, |caps: &Captures| {
        format!("{}[://]{}", &caps[1], bracket_dots(&caps[2]))
    });
    // bracket IPv4 separators when every octet is actually in range
    let ips = regex(&IPS, r"\b(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})\b");
    let report = ips.replace_all(&report, |caps: &Captures| {
        // leave out-of-range shapes (version-ish numbers like 1.2.3.300) alone
        let in_range = (1..=4).all(|index| {
            caps[index]
                .parse::<u16>()
                .map(|octet| octet <= 255)
                .unwrap_or(false)
        });
        if in_range {
            format!("{}[.]{}[.]{}[.]{}", &caps[1], &caps[2], &caps[3], &caps[4])
        } else {
            caps[0].to_owned()
        }
    });
    // bracket bare domains against the conservative TLD allowlist; already
    // defanged text has no dots left in its labels, so this pass is idempotent
    let pattern = format!(r"(?i)\b(?:[a-z0-9-]{{1,63}}\.)+(?:{DEFANG_TLDS})\b");
    let domains = regex(&DOMAINS, &pattern);
    let report = domains.replace_all(&report, |caps: &Captures| bracket_dots(&caps[0]));
    report.into_owned()
}

/// Restore defanged text to its pristine fanged form
///
/// The exact inverse of the forms [`defang`] emits, and nothing else: the
/// broken schemes are restored and a `[.]` becomes `.` only when bounded by
/// an alphanumeric on both sides, so prose brackets, checkbox markers, and
/// spaced `[.]` sequences are never touched. Text that never carried a
/// defanged form passes through byte-exact.
///
/// # Arguments
///
/// * `text` - The defanged text to restore
pub fn refang(text: &str) -> String {
    // restore the broken schemes first, accepting any case spelling
    let schemes = regex(&REFANG_SCHEMES, r"(?i)\b(hxxps|hxxp|fxp)\[://\]");
    let text = schemes.replace_all(text, |caps: &Captures| {
        // map each defanged scheme back to the live protocol prefix
        match caps[1].to_ascii_lowercase().as_str() {
            "hxxps" => "https://",
            "hxxp" => "http://",
            _ => "ftp://",
        }
    });
    // un-bracket dots with a char walk: a regex replace would consume the
    // trailing alphanumeric and skip every second `[.]` in `a[.]b[.]c` runs
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        // only a literal `[.]` bounded by alphanumerics on both sides restores
        if bytes[index..].starts_with(b"[.]")
            && index > 0
            && bytes[index - 1].is_ascii_alphanumeric()
            && bytes.get(index + 3).is_some_and(u8::is_ascii_alphanumeric)
        {
            out.push('.');
            index += 3;
        } else {
            // copy the full UTF-8 char so multi-byte text survives the walk
            let ch = text[index..].chars().next().expect("in-bounds char");
            out.push(ch);
            index += ch.len_utf8();
        }
    }
    out
}

/// How aggressively a blocked request's text content is rewritten.
///
/// The retry layer climbs these levels in order on repeated content-filter
/// kills; each level is strictly more lossy than the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrubLevel {
    /// Defang IOCs (URLs, IPs, allowlisted domains) in every text part
    Defang,
    /// Defang plus neutralize bulk payload blobs and truncate oversized
    /// tool results — the heavier content a filter keys on
    Deep,
}

impl ScrubLevel {
    /// The numeric form events and transcript markers carry
    pub(crate) fn as_u8(self) -> u8 {
        // level 1 is the defang pass, level 2 the deep pass
        match self {
            ScrubLevel::Defang => 1,
            ScrubLevel::Deep => 2,
        }
    }
}

/// How a retrying model reacts to a content-filter kill.
///
/// Almost every agent wants the ladder: a scrubbed retry beats dying with
/// partial work. The report path opts out ([`Surface`](ScrubPolicy::Surface))
/// because it owns a smarter ladder of its own — defang-at-render with a
/// client-side refang, then per-section generation (see
/// [`crate::thorpal::report`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ScrubPolicy {
    /// Retry a content-filter 403 with scrubbed context, escalating
    /// [`ScrubLevel`]s before giving up
    #[default]
    Ladder,
    /// Return the classified content-filter error to the caller untouched
    Surface,
}

/// Scrub a text part at the given level
///
/// # Arguments
///
/// * `text` - The text to scrub
/// * `level` - How aggressively to rewrite it
pub(crate) fn scrub_text(text: &str, level: ScrubLevel) -> String {
    // every level starts by defanging the IOC forms filters key on
    let scrubbed = defang(text);
    // the defang level stops there
    if level == ScrubLevel::Defang {
        return scrubbed;
    }
    // a deep scrub also collapses bulk payload blobs into digest notes; hex
    // first (tighter alphabet), then whatever long base64 runs remain
    let hex = regex(&HEX_BLOBS, &format!("[0-9a-fA-F]{{{HEX_BLOB_MIN},}}"));
    let scrubbed = hex.replace_all(&scrubbed, blob_note);
    let b64 = regex(&B64_BLOBS, &format!(r"[A-Za-z0-9+/=]{{{B64_BLOB_MIN},}}"));
    let scrubbed = b64.replace_all(&scrubbed, blob_note);
    scrubbed.into_owned()
}

/// Render the digest note a neutralized blob is replaced with
///
/// # Arguments
///
/// * `caps` - The regex captures holding the matched blob
fn blob_note(caps: &Captures) -> String {
    // digest the blob so the pristine transcript can still be correlated
    let digest = <sha2::Sha256 as sha2::Digest>::digest(caps[0].as_bytes());
    let hex: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("[scrubbed {}-char blob sha256={hex}]", caps[0].len())
}

/// Truncate an oversized tool-result text to its head and tail
///
/// Keeps the first ~3/4 and last ~1/4 of the budget around a marker note so
/// the model still sees how the result started and ended. Splits are floored
/// to char boundaries so multi-byte text can never panic the slice.
///
/// # Arguments
///
/// * `text` - The tool-result text to bound
fn truncate_tool_text(text: &str) -> String {
    // short results pass through untouched
    if text.len() <= TOOL_RESULT_TEXT_CAP {
        return text.to_owned();
    }
    // floor both split points to char boundaries (stable std helper)
    let head_end = text.floor_char_boundary((TOOL_RESULT_TEXT_CAP / 4) * 3);
    let tail_start = text.floor_char_boundary(text.len() - TOOL_RESULT_TEXT_CAP / 4);
    let dropped = tail_start - head_end;
    // keep the head and tail around a marker naming what was dropped
    format!(
        "{}\n[... {dropped} bytes removed by the content-filter scrub; the full \
         result is in the persisted transcript ...]\n{}",
        &text[..head_end],
        &text[tail_start..]
    )
}

/// Scrub every string leaf of a JSON value in place
///
/// # Arguments
///
/// * `value` - The JSON value to scrub
/// * `level` - How aggressively to rewrite its strings
fn scrub_json_strings(value: &mut serde_json::Value, level: ScrubLevel) {
    match value {
        // rewrite string leaves through the text scrubber
        serde_json::Value::String(text) => *text = scrub_text(text, level),
        // recurse into containers; keys stay structural and untouched
        serde_json::Value::Array(items) => {
            for item in items {
                scrub_json_strings(item, level);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                scrub_json_strings(item, level);
            }
        }
        // numbers, bools, and nulls carry no text
        _ => (),
    }
}

/// Build a scrubbed copy of a completion request for a content-filter retry
///
/// Clones the pristine request and rewrites only its text content: the
/// preamble, system/user/assistant text parts, tool-result text (deep scrubs
/// also bound oversized results and rewrite JSON string leaves), unsigned
/// reasoning text, and string documents. Structure is never touched — tool
/// call/result correlation ids, names, images, tool definitions, and
/// provider params all pass through byte-exact, and provider-signed
/// reasoning blocks stay whole since rewriting signed text would invalidate
/// the signature and fail the request outright.
///
/// # Arguments
///
/// * `request` - The pristine request to scrub a copy of
/// * `level` - How aggressively to rewrite its text content
pub(crate) fn scrub_request(request: &CompletionRequest, level: ScrubLevel) -> CompletionRequest {
    // scrub a clone so the pristine request survives for later attempts
    let mut request = request.clone();
    // the preamble is thorpal-authored but defang is harmless and idempotent
    if let Some(preamble) = &mut request.preamble {
        *preamble = scrub_text(preamble, level);
    }
    // walk the conversation rewriting only text content
    for message in &mut request.chat_history {
        match message {
            // system instructions are plain text
            Message::System { content } => *content = scrub_text(content, level),
            // user turns carry prompt text and tool results
            Message::User { content } => {
                for part in content {
                    match part {
                        // plain prompt/feedback text
                        UserContent::Text(text) => {
                            text.text = scrub_text(&text.text, level);
                        }
                        // tool results are the bulk payload filters key on
                        UserContent::ToolResult(result) => {
                            for item in &mut result.content {
                                match item {
                                    ToolResultContent::Text(text) => {
                                        // deep scrubs also bound oversized results
                                        let scrubbed = scrub_text(&text.text, level);
                                        text.text = if level == ScrubLevel::Deep {
                                            truncate_tool_text(&scrubbed)
                                        } else {
                                            scrubbed
                                        };
                                    }
                                    // structured results only rewrite at depth
                                    ToolResultContent::Json { value } => {
                                        if level == ScrubLevel::Deep {
                                            scrub_json_strings(value, level);
                                        }
                                    }
                                    // images carry no text
                                    ToolResultContent::Image(_) => (),
                                }
                            }
                        }
                        // media parts carry no text to rewrite
                        _ => (),
                    }
                }
            }
            // assistant turns carry reply text, reasoning, and tool calls
            Message::Assistant { content, .. } => {
                for part in content {
                    match part {
                        // plain reply text
                        AssistantContent::Text(text) => {
                            text.text = scrub_text(&text.text, level);
                        }
                        // reasoning: only unsigned text may be rewritten
                        AssistantContent::Reasoning(reasoning) => {
                            for block in &mut reasoning.content {
                                match block {
                                    // a signed block must stay byte-exact or the
                                    // provider rejects the whole request
                                    ReasoningContent::Text { text, signature } => {
                                        if signature.is_none() {
                                            *text = scrub_text(text, level);
                                        }
                                    }
                                    // summaries are provider prose, safe to scrub
                                    ReasoningContent::Summary(text) => {
                                        *text = scrub_text(text, level);
                                    }
                                    // opaque provider payloads stay whole
                                    ReasoningContent::Encrypted(_)
                                    | ReasoningContent::Redacted { .. } => (),
                                }
                            }
                        }
                        // tool-call arguments only rewrite at depth: names and
                        // correlation ids stay structural
                        AssistantContent::ToolCall(call) => {
                            if level == ScrubLevel::Deep {
                                scrub_json_strings(&mut call.function.arguments, level);
                            }
                        }
                        // images carry no text
                        AssistantContent::Image(_) => (),
                    }
                }
            }
        }
    }
    // context documents (injected context like turn warnings) scrub too; a
    // deep scrub also covers their string metadata props
    for document in &mut request.documents {
        document.text = scrub_text(&document.text, level);
        if level == ScrubLevel::Deep {
            for prop in document.additional_props.values_mut() {
                *prop = scrub_text(prop, level);
            }
        }
    }
    request
}

#[cfg(test)]
mod tests {
    use rig::completion::Document;
    use rig::message::{Text, ToolCall, ToolCallId, ToolFunction, ToolResult};

    use super::*;

    /// Builds a request touching every text-bearing content shape
    fn request_with_all_shapes() -> CompletionRequest {
        CompletionRequest {
            model: None,
            preamble: Some("analyze http://evil.com".to_owned()),
            chat_history: vec![
                Message::Assistant {
                    id: None,
                    content: vec![
                        AssistantContent::Text(Text::new("found evil.top")),
                        AssistantContent::ToolCall(ToolCall::new(
                            ToolCallId::new_or_mint("c1"),
                            ToolFunction::new(
                                "read_file".to_owned(),
                                serde_json::json!({"path": "a"}),
                            ),
                        )),
                    ],
                },
                Message::User {
                    content: vec![UserContent::ToolResult(ToolResult {
                        call: ToolCallId::new_or_mint("c1"),
                        provider: None,
                        name: "read_file".to_owned(),
                        content: vec![ToolResultContent::text("beacon to http://evil.com now")],
                    })],
                },
                Message::User {
                    content: vec![UserContent::text("see 10.0.0.1 live")],
                },
            ],
            documents: vec![Document {
                id: "warning".to_owned(),
                text: "warn: http://evil.com".to_owned(),
                additional_props: std::collections::HashMap::new(),
            }],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            tool_choice: None,
            additional_params: None,
            output_schema: None,
            record_telemetry_content: false,
        }
    }

    /// URL schemes break into the conventional forms, whatever their case.
    #[test]
    fn schemes_break_case_insensitively() {
        // each scheme spelling normalizes to its lowercase defanged form
        assert_eq!(
            defang("fetches HTTPS://evil.com/x then http://evil.com and FTP://evil.com"),
            "fetches hxxps[://]evil[.]com/x then hxxp[://]evil[.]com and fxp[://]evil[.]com"
        );
    }

    /// A defanged URL's authority is bracketed through ports and paths.
    #[test]
    fn url_authorities_bracket_dots() {
        // the port and path survive; only the host dots are bracketed
        assert_eq!(
            defang("c2 at http://a.b.evil.ru:8443/gate.php?x=1"),
            "c2 at hxxp[://]a[.]b[.]evil[.]ru:8443/gate.php?x=1"
        );
    }

    /// IPv4 addresses bracket only when every octet is in range.
    #[test]
    fn ips_bracket_only_in_range() {
        // a real address defangs; an out-of-range shape is left alone
        assert_eq!(defang("beacons to 10.2.3.4"), "beacons to 10[.]2[.]3[.]4");
        assert_eq!(defang("version 1.2.3.300"), "version 1.2.3.300");
    }

    /// Bare domains defang on the TLD allowlist; file names never do.
    #[test]
    fn domains_respect_the_allowlist() {
        // a listed TLD defangs even without a scheme
        assert_eq!(
            defang("resolves evil.top daily"),
            "resolves evil[.]top daily"
        );
        // module names and off-list extensions are untouched
        assert_eq!(
            defang("loads kernel32.dll and drops payload.dat"),
            "loads kernel32.dll and drops payload.dat"
        );
    }

    /// Defanging is idempotent: a second pass changes nothing.
    #[test]
    fn defang_is_idempotent() {
        // one pass over a mixed line, then prove the fixed point
        let input = "see https://a.evil.com/x, 10.0.0.1, and evil.xyz (plus 1.2.3.4)";
        let once = defang(input);
        assert_eq!(defang(&once), once, "second pass must be a no-op");
        // pre-defanged text is already at the fixed point
        let fanged = "hxxps[://]a[.]evil[.]com/x and 10[.]0[.]0[.]1";
        assert_eq!(defang(fanged), fanged);
    }

    /// Refanging inverts every form defang emits and nothing else.
    #[test]
    fn refang_inverts_defang_on_pristine_ioc_text() {
        // a mixed line survives the full round trip byte-exactly
        let input = "see https://a.evil.com:8443/x, ftp://e.evil.ru, 10.0.0.1, and evil.xyz";
        assert_eq!(refang(&defang(input)), input);
        // consecutive bracketed dots all restore (the regex-overlap trap)
        assert_eq!(refang("a[.]b[.]c[.]com"), "a.b.c.com");
        // text defang never touches passes through refang byte-exact
        let untouched = "loads kernel32.dll, marks [ ] boxes, and cites [.] alone";
        assert_eq!(refang(untouched), untouched);
        // multi-byte text around a bracketed dot survives the walk
        assert_eq!(refang("Ghidra’s c2 evil[.]com"), "Ghidra’s c2 evil.com");
        // trailing sentence punctuation never brackets, so the round trip
        // survives URLs at sentence/parenthesis/text ends too
        for input in [
            "beacons to http://evil.com. Next sentence",
            "see the c2 (http://evil.com.)",
            "ends with http://b.net.",
        ] {
            let defanged = defang(input);
            assert!(!defanged.contains("com[.] ") && !defanged.ends_with("[.]"));
            assert_eq!(refang(&defanged), input, "round trip of {input:?}");
        }
    }

    /// A defang-level scrub rewrites exactly the text parts of a request.
    #[test]
    fn scrub_request_defangs_only_text_parts() {
        // assemble a request touching every content shape
        let request = request_with_all_shapes();
        let scrubbed = scrub_request(&request, ScrubLevel::Defang);
        // the preamble and the context document defang
        assert_eq!(
            scrubbed.preamble.as_deref(),
            Some("analyze hxxp[://]evil[.]com")
        );
        assert_eq!(scrubbed.documents[0].text, "warn: hxxp[://]evil[.]com");
        // every text part defangs; find them by walking the history
        let rendered = format!("{:?}", scrubbed.chat_history);
        assert!(
            rendered.contains("found evil[.]top"),
            "assistant text defangs"
        );
        assert!(
            rendered.contains("hxxp[://]evil[.]com now"),
            "tool result defangs"
        );
        assert!(rendered.contains("10[.]0[.]0[.]1"), "prompt text defangs");
        // structural parts are untouched at the defang level
        assert!(rendered.contains("read_file"), "tool names survive");
        assert!(
            rendered.contains("\"a\""),
            "tool-call args survive a defang scrub"
        );
        // the pristine request is not mutated
        assert_eq!(request.preamble.as_deref(), Some("analyze http://evil.com"));
    }

    /// A deep scrub neutralizes payload blobs and bounds oversized results.
    #[test]
    fn deep_scrub_neutralizes_blobs_and_truncates_oversized_results() {
        // a long hex run collapses into a digest note
        let hex_blob = "41".repeat(HEX_BLOB_MIN);
        let scrubbed = scrub_text(&format!("dump: {hex_blob}"), ScrubLevel::Deep);
        assert!(
            scrubbed.contains("[scrubbed 512-char blob sha256="),
            "hex blob collapses: {scrubbed}"
        );
        // a long base64 run collapses too; a short one survives
        let b64_blob = "QUJD".repeat(B64_BLOB_MIN / 4 + 1);
        let scrubbed = scrub_text(&b64_blob, ScrubLevel::Deep);
        assert!(scrubbed.starts_with("[scrubbed"), "base64 blob collapses");
        assert_eq!(
            scrub_text("short QUJD run", ScrubLevel::Deep),
            "short QUJD run"
        );
        // an oversized tool result keeps its head and tail around the marker
        let oversized = "x".repeat(TOOL_RESULT_TEXT_CAP * 2);
        let bounded = truncate_tool_text(&oversized);
        assert!(bounded.len() < oversized.len(), "the result shrank");
        assert!(
            bounded.contains("removed by the content-filter scrub"),
            "the marker names the drop"
        );
        // a small result passes through byte-exact
        assert_eq!(truncate_tool_text("small"), "small");
        // truncation splits never land inside a multi-byte char
        let wide = "’".repeat(TOOL_RESULT_TEXT_CAP);
        let bounded = truncate_tool_text(&wide);
        assert!(bounded.contains("removed by the content-filter scrub"));
    }
}
