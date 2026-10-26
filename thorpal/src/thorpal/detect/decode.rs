//! Static decode/decrypt engine for obfuscated data.
//!
//! Detections that flag encoded/encrypted data ([`crate::thorpal::detect::detections::defense_evasion::encoded_strings`],
//! [`crate::thorpal::detect::detections::impact::crypto_capability`]) use this to actually **recover
//! the plaintext and the key statically**, then present each recovered item as its own
//! flag: a typed, snippet-bearing name ([`recovered_name`]), an organized markdown body
//! with the payload in a fenced code block ([`recovered_content`]), and a reasoning
//! block that explains *how* it was recovered ([`recovery_reasoning`]).
//!
//! One decoder/decrypter per file:
//! * [`xor`] — single-byte and repeating/key-buffer XOR,
//! * [`arithmetic`] — single-byte ADD / SUB,
//! * [`pairwise`] — the pairwise SUB,
//! * [`rc4`] — the RC4 stream cipher,
//! * [`rc6`] — the RC6 block cipher (ECB / CBC),
//! * [`base64`] — base64 unwrapping,
//! * [`apihash`] — API-hash matching against the curated [`apihash_names`] dictionary.
//!
//! Shared infrastructure: [`recover`] (readable-run extraction + structured item
//! extraction) and [`buffers`] (finding the static data a function references).

use crate::thorpal::detect::confidence::Confidence;
use crate::thorpal::detect::detections::ArtifactClass;

use crate::thorpal::detect::detections::confidence_rank;

pub(crate) mod apihash;
pub(crate) mod apihash_names;
pub mod arithmetic;
pub mod base64;
pub mod buffers;
pub mod chain;
pub mod der;
pub mod pairwise;
pub mod rc4;
pub mod rc6;
pub mod records;
pub mod recover;
pub mod xor;

// re-export the buffer helpers so callers can write `decode::data_addrs(…)`;
// the cipher and `recover` helpers are reached through their submodule paths.
pub use buffers::{data_addrs, slice_at};

/// The max characters shown in a flag-name snippet for a generic string.
const SNIPPET_CHARS: usize = 24;
/// The max characters shown in a flag-name snippet for a PEM body.
const PEM_SNIPPET_CHARS: usize = 16;
/// The max characters of recovered text embedded in a flag's fenced code block.
const MAX_BLOCK_CHARS: usize = 4096;

/// The best-effort classified kind of a recovered string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveredKind {
    /// A PEM public key (`-----BEGIN PUBLIC KEY-----`).
    PublicKey,
    /// A PEM private key.
    PrivateKey,
    /// A PEM certificate.
    Certificate,
    /// A symmetric (secret) key — the same value both encrypts and decrypts.
    ///
    /// Distinguished from the asymmetric classes because the consequence differs:
    /// a recovered symmetric key decrypts the program's own data outright.
    SymmetricKey,
    /// An email address.
    Email,
    /// A URL.
    Url,
    /// Anything else (the fallback).
    String,
}

impl RecoveredKind {
    /// A human label for the kind, used in the flag name and content title.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            RecoveredKind::PublicKey => "Public Key",
            RecoveredKind::PrivateKey => "Private Key",
            RecoveredKind::Certificate => "Certificate",
            RecoveredKind::SymmetricKey => "Symmetric Key",
            RecoveredKind::Email => "Email",
            RecoveredKind::Url => "URL",
            RecoveredKind::String => "String",
        }
    }
}

/// The artifact class a recovered kind lands under in the report inventory.
///
/// Shared by every recovery path (static decode loops, blob decodes, and the
/// emulation stage) so identical plaintext always files under one class.
///
/// # Arguments
///
/// * `kind` - The classified kind of the recovered text
#[must_use]
pub fn artifact_class(kind: RecoveredKind) -> ArtifactClass {
    match kind {
        // network endpoints drive the C2 question deriver
        RecoveredKind::Url => ArtifactClass::NetworkEndpoint,
        // key material keeps its typed inventory sections
        RecoveredKind::PublicKey => ArtifactClass::AsymmetricPublicKey,
        RecoveredKind::PrivateKey => ArtifactClass::AsymmetricPrivateKey,
        RecoveredKind::Certificate => ArtifactClass::Certificate,
        RecoveredKind::SymmetricKey => ArtifactClass::SymmetricKey,
        // everything else is decoded data (drives the decoded-config question)
        RecoveredKind::Email | RecoveredKind::String => ArtifactClass::DecodedData,
    }
}

/// Best-effort classify a recovered string by its content.
///
/// Recognizes PEM armor (public/private key, certificate) by its `-----BEGIN` marker,
/// then a URL or email *token* anywhere in the text (recovered strings often carry
/// surrounding junk like a `>>` prefix), and otherwise falls back to `String`.
///
/// # Arguments
///
/// * `text` - The recovered plaintext
#[must_use]
pub fn classify(text: &str) -> RecoveredKind {
    let lower = text.to_ascii_lowercase();
    if lower.contains("-----begin") {
        if lower.contains("public key") {
            return RecoveredKind::PublicKey;
        }
        if lower.contains("private key") {
            return RecoveredKind::PrivateKey;
        }
        if lower.contains("certificate") {
            return RecoveredKind::Certificate;
        }
    }
    if find_url(text).is_some() {
        return RecoveredKind::Url;
    }
    if find_email(text).is_some() {
        return RecoveredKind::Email;
    }
    RecoveredKind::String
}

/// The first whitespace-delimited token that looks like a URL, if any.
///
/// # Arguments
///
/// * `text` - The recovered plaintext
fn find_url(text: &str) -> Option<&str> {
    text.split_whitespace().find(|t| {
        let l = t.to_ascii_lowercase();
        l.starts_with("http://") || l.starts_with("https://") || l.starts_with("ftp://")
    })
}

/// The first whitespace-delimited token that looks like an email address, if any.
///
/// # Arguments
///
/// * `text` - The recovered plaintext
fn find_email(text: &str) -> Option<&str> {
    text.split_whitespace().find(|t| is_email_token(t))
}

/// Whether a single token is a plausible `local@domain.tld` email address.
///
/// # Arguments
///
/// * `t` - The token to test
fn is_email_token(t: &str) -> bool {
    // exactly one '@', a dotted domain, and only email-safe characters
    if t.matches('@').count() != 1 {
        return false;
    }
    let Some((local, domain)) = t.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@'))
}

/// A short, single-line snippet of the recovered text for the flag name.
///
/// For a key/certificate it is the start of the **body** (the first non-armor line, so
/// a public key reads `"MIIBIjANBgkq…"`); for an email/URL it is the matching token;
/// otherwise it is the start of the (whitespace-collapsed) text.
///
/// # Arguments
///
/// * `kind` - The classified kind
/// * `text` - The recovered plaintext
#[must_use]
pub fn snippet(kind: RecoveredKind, text: &str) -> String {
    match kind {
        RecoveredKind::PublicKey | RecoveredKind::PrivateKey | RecoveredKind::Certificate => {
            // the first line that is not PEM armor and not blank — the key/cert body
            let body = text
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with("-----"))
                .unwrap_or("");
            clip(body, PEM_SNIPPET_CHARS)
        }
        RecoveredKind::Email => clip(find_email(text).unwrap_or(text), SNIPPET_CHARS),
        RecoveredKind::Url => clip(find_url(text).unwrap_or(text), SNIPPET_CHARS),
        // a symmetric key is short already, but is rendered as encoded/hex text with
        // no internal whitespace, so the leading characters are the identifying part
        RecoveredKind::SymmetricKey => clip(text, PEM_SNIPPET_CHARS),
        // collapse whitespace so a multi-space / newline string reads as one line
        RecoveredKind::String => {
            let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
            clip(&collapsed, SNIPPET_CHARS)
        }
    }
}

/// Clip a string to `max` characters (adding `…`) and neutralize quotes/controls so it
/// is safe inside a `"…"`-quoted flag name.
///
/// # Arguments
///
/// * `s` - The string to clip
/// * `max` - The maximum number of characters
fn clip(s: &str, max: usize) -> String {
    // single-line, no embedded quotes/backticks that would confuse the name
    let cleaned: String = s
        .trim()
        .chars()
        .map(|c| match c {
            '"' | '`' => '\'',
            '\n' | '\r' | '\t' => ' ',
            _ => c,
        })
        .collect();
    if cleaned.chars().count() > max {
        let head: String = cleaned.chars().take(max).collect();
        format!("{head}…")
    } else {
        cleaned
    }
}

/// The entity name for a recovered item: `{Kind} - "{snippet}" @ {section} @ 0x{addr}`.
///
/// # Arguments
///
/// * `kind` - The classified kind
/// * `text` - The recovered plaintext
/// * `section` - The section the bytes came from (e.g. `.rodata`)
/// * `addr` - The address the encoded bytes start at
#[must_use]
pub fn recovered_name(kind: RecoveredKind, text: &str, section: &str, addr: u64) -> String {
    format!(
        "{} - \"{}\" @ {} @ 0x{:x}",
        kind.label(),
        snippet(kind, text),
        section,
        addr
    )
}

/// Build the flag `content` markdown for one recovered item: a titled section, the
/// payload in a fenced code block (newlines preserved), and a metadata list.
///
/// # Arguments
///
/// * `kind` - The classified kind (titles the section)
/// * `text` - The recovered plaintext
/// * `section` - The section the bytes came from (e.g. `.rodata`)
/// * `addr` - The address the encoded bytes start at
/// * `scheme` - The decode/decrypt scheme label (e.g. `"pairwise SUB"`, `"RC4"`)
/// * `key` - The recovered key rendered for display, if the scheme uses one
#[must_use]
pub fn recovered_content(
    kind: RecoveredKind,
    text: &str,
    section: &str,
    addr: u64,
    scheme: &str,
    key: Option<&str>,
) -> String {
    // bound the embedded payload and keep newlines; strip other control bytes
    let body = sanitize_block(text);
    // choose a fence longer than any backtick run in the body so it can't be broken
    let fence = pick_fence(&body);

    let mut out = String::new();
    out.push_str(&format!("### Recovered {}\n\n", kind.label()));
    out.push_str(&fence);
    out.push('\n');
    out.push_str(&body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&fence);
    out.push('\n');
    out.push_str("\n### Metadata\n");
    out.push_str(&format!("- **Section:** {section}\n"));
    out.push_str(&format!("- **Address:** 0x{addr:x}\n"));
    match key {
        Some(k) => out.push_str(&format!("- **Scheme:** {scheme} (key {k})\n")),
        None => out.push_str(&format!("- **Scheme:** {scheme}\n")),
    }
    out
}

/// Append a "How it was recovered" section (explaining the specific scheme) to a base
/// analyst explanation, producing the flag `reasoning`.
///
/// # Arguments
///
/// * `base` - The base analyst-facing explanation (what/why)
/// * `scheme` - The decode/decrypt scheme label
/// * `key` - The recovered key rendered for display, if any
#[must_use]
pub fn recovery_reasoning(base: &str, scheme: &str, key: Option<&str>) -> String {
    format!(
        "{base}\n\n### How it was recovered\n{}",
        method_sentence(scheme, key)
    )
}

/// Append a content-[`assess`]ment's notes to a reasoning block as an `### Assessment`
/// markdown section (the report demotes the header when nesting). No-op when empty.
///
/// # Arguments
///
/// * `reasoning` - The base reasoning (e.g. from [`recovery_reasoning`])
/// * `notes` - The assessment rationale sentences (from [`Assessment::notes`])
#[must_use]
pub fn append_assessment(mut reasoning: String, notes: &[String]) -> String {
    if !notes.is_empty() {
        reasoning.push_str("\n\n### Assessment\n");
        // one bullet per driver so the confidence/suspicion rationale reads cleanly
        for note in notes {
            reasoning.push_str(&format!("- {note}\n"));
        }
    }
    reasoning
}

/// A one-sentence description of how a scheme recovered the plaintext.
///
/// # Arguments
///
/// * `scheme` - The decode/decrypt scheme label
/// * `key` - The recovered key rendered for display, if any
fn method_sentence(scheme: &str, key: Option<&str>) -> String {
    let keyed = key.map(|k| format!(" with key `{k}`")).unwrap_or_default();
    match scheme {
        "pairwise SUB" => {
            "Recovered with a pairwise-subtraction scheme, where each plaintext byte is \
             `data[2i] - data[2i+1]` over the otherwise non-printable source bytes."
                .to_string()
        }
        "RC4" => format!(
            "Decrypted with the RC4 stream cipher{keyed} (an embedded candidate key), which turns \
             the non-printable ciphertext into readable text."
        ),
        "RC6" => format!(
            "Decrypted with the RC6 block cipher in ECB mode{keyed} (an embedded candidate key), \
             which turns the non-printable ciphertext into readable text."
        ),
        "RC6-CBC" => format!(
            "Decrypted with the RC6 block cipher in CBC mode (zero IV){keyed} (an embedded \
             candidate key), which turns the non-printable ciphertext into readable text."
        ),
        "repeating XOR" => format!(
            "Decrypted with a repeating-key XOR{keyed} (an embedded candidate key) over the \
             ciphertext buffer."
        ),
        s if s.contains("XOR") => {
            format!("Recovered by XOR-ing the non-printable source bytes{keyed}.")
        }
        s if s.contains("SUB") => format!(
            "Recovered by subtracting a single-byte key{keyed} from the non-printable source bytes."
        ),
        s if s.contains("ADD") => format!(
            "Recovered by adding a single-byte key{keyed} to the non-printable source bytes."
        ),
        other => format!("Recovered by static {other} decoding{keyed}."),
    }
}

/// Keep `\n` but drop other control characters, and bound the length so a huge blob
/// cannot balloon a flag.
///
/// # Arguments
///
/// * `s` - The recovered text
fn sanitize_block(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect();
    if cleaned.chars().count() > MAX_BLOCK_CHARS {
        let head: String = cleaned.chars().take(MAX_BLOCK_CHARS).collect();
        format!("{head}\n… (truncated)")
    } else {
        cleaned
    }
}

/// A backtick fence at least one longer than the longest backtick run in `body` (min
/// three), so the body can never prematurely close the fenced code block.
///
/// # Arguments
///
/// * `body` - The text that will go inside the fence
fn pick_fence(body: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for c in body.chars() {
        if c == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    // at least ``` , and always longer than any run inside the body
    "`".repeat(longest.max(2) + 1)
}

// ===========================================================================
// Content-driven scoring (confidence + suspicion)
// ===========================================================================

/// Privacy/anonymity email providers favored by extortion operators — used to raise
/// the *suspicion* of a recovered email.
const ANON_EMAIL_DOMAINS: &[&str] = &[
    "proton.me",
    "protonmail.com",
    "protonmail.ch",
    "pm.me",
    "tutanota.com",
    "tutanota.de",
    "tuta.io",
    "tutamail.com",
    "keemail.me",
    "cock.li",
    "onionmail.org",
    "onionmail.com",
    "mail2tor.com",
    "dnmx.org",
    "elude.in",
    "secmail.pro",
    "guerrillamail.com",
    "mailfence.com",
    "msgsafe.io",
];

/// Ransom/extortion wording that makes a recovered string notable to an analyst.
const EXTORTION_KEYWORDS: &[&str] = &[
    "encrypt",
    "decrypt",
    "bitcoin",
    "monero",
    "ransom",
    "payment",
    "your files",
    ".onion",
    "recover your",
];

/// Destructive / anti-forensic / persistence / download commands — a recovered
/// command containing one of these is highly notable (self-wipe, shadow-copy
/// deletion, log clearing, downloaders, persistence).
const SUSPICIOUS_COMMANDS: &[&str] = &[
    // destruction / anti-forensics (unix)
    "rm -rf",
    "rm -f",
    "shred",
    "mkfs",
    "/dev/urandom",
    "history -c",
    "chattr",
    // destruction / anti-forensics (windows)
    "vssadmin",
    "bcdedit",
    "wbadmin",
    "cipher /w",
    "wevtutil",
    "del /",
    "format ",
    "fsutil",
    "clear-eventlog",
    // persistence / privilege
    "schtasks",
    "reg add",
    "reg delete",
    "crontab",
    "sc stop",
    "net stop",
    "taskkill",
    "icacls",
    "attrib ",
    "sc config",
    // execution / download / LOLBins
    "powershell",
    "cmd /c",
    "cmd.exe",
    "certutil",
    "bitsadmin",
    "rundll32",
    "regsvr32",
    "mshta",
    "wmic",
    "curl ",
    "wget ",
    "base64 -d",
    "nohup ",
    // a self-wipe overwrites before deleting, usually with dd
    "dd if=",
];

/// Shell operators + common command prefixes that mark a recovered string as a
/// command line (raises confidence: a valid command is unlikely to decode by chance).
const COMMAND_MARKERS: &[&str] = &[
    "&&",
    "||",
    " | ",
    "; ",
    " > ",
    " >> ",
    "2>&1",
    ">/dev/",
    "$(",
    "${",
    "/bin/",
    "/usr/bin/",
    "sh -c",
    "bash ",
    "cmd /c",
    "powershell",
    "cscript",
    "wscript",
    "sleep ",
    "echo ",
];

/// The minimum host length + Shannon entropy (bits/char) for a URL host to look
/// random/DGA-generated (normal domains sit ~2.5–3.5; hex/base32 hosts ~4+).
const HIGH_ENTROPY_MIN_LEN: usize = 10;
const HIGH_ENTROPY_BITS: f32 = 3.8;

/// The Shannon entropy (bits per symbol) of a byte slice.
///
/// # Arguments
///
/// * `bytes` - The bytes to measure
#[must_use]
pub fn shannon_entropy(bytes: &[u8]) -> f32 {
    if bytes.is_empty() {
        return 0.0;
    }
    // histogram of byte values
    let mut counts = [0u32; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f32;
    // -Σ p·log2(p) over observed symbols
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f32 / len;
            -p * p.log2()
        })
        .sum()
}

/// Whether `text` reads like a natural-language sentence (mild evidence a decode is
/// real): several words, mostly letters/spaces, at least one real word.
///
/// # Arguments
///
/// * `text` - The recovered text
#[must_use]
pub fn is_sentence(text: &str) -> bool {
    let trimmed = text.trim();
    // shell/code metacharacters mark a command or config, not prose
    if trimmed.contains(['{', '}', '$', '&', '|', '<', '>', '=', '\\', '`']) {
        return false;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    // several whitespace-separated tokens, one of which is a real (3+ letter) word
    if tokens.len() < 4
        || !tokens
            .iter()
            .any(|t| t.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 3)
    {
        return false;
    }
    // dominated by letters/spaces (rejects paths/identifiers full of punctuation)
    let letters_spaces = trimmed
        .chars()
        .filter(|c| c.is_ascii_alphabetic() || c.is_whitespace())
        .count();
    letters_spaces * 100 >= trimmed.chars().count() * 70
}

/// The anonymity/privacy email provider a recovered email uses, if any.
///
/// # Arguments
///
/// * `text` - The recovered text (an email token is located within it)
#[must_use]
pub fn anonymous_email_provider(text: &str) -> Option<&'static str> {
    let email = find_email(text)?;
    let domain = email.split_once('@')?.1.to_ascii_lowercase();
    // match the exact domain (an anon provider), tolerating a trailing punctuation
    let domain = domain.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    ANON_EMAIL_DOMAINS.iter().copied().find(|d| *d == domain)
}

/// The host portion of a URL token (`scheme://HOST[/…]`), lowercased.
///
/// # Arguments
///
/// * `url` - A URL token
fn url_host(url: &str) -> Option<String> {
    let after = url.split("://").nth(1)?;
    // host ends at the first '/', ':' , or trailing punctuation
    let host: String = after
        .chars()
        .take_while(|c| *c != '/' && *c != ':')
        .collect();
    let host = host.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Whether the recovered text contains a `.onion` (Tor hidden-service) URL.
///
/// # Arguments
///
/// * `text` - The recovered text
#[must_use]
pub fn is_onion_url(text: &str) -> bool {
    find_url(text)
        .and_then(url_host)
        .is_some_and(|h| h.ends_with(".onion"))
}

/// Whether the recovered text contains a URL whose host looks random/DGA-generated
/// (high Shannon entropy). Excludes `.onion` (scored separately).
///
/// # Arguments
///
/// * `text` - The recovered text
#[must_use]
pub fn is_high_entropy_url(text: &str) -> bool {
    let Some(host) = find_url(text).and_then(url_host) else {
        return false;
    };
    !host.ends_with(".onion")
        && host.len() >= HIGH_ENTROPY_MIN_LEN
        && shannon_entropy(host.as_bytes()) >= HIGH_ENTROPY_BITS
}

/// Whether the recovered text carries ransom/extortion wording.
///
/// # Arguments
///
/// * `text` - The recovered text
#[must_use]
pub fn looks_extortion(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    EXTORTION_KEYWORDS.iter().any(|k| lower.contains(k))
}

/// The destructive/anti-forensic/persistence command a recovered string contains, if
/// any (e.g. `dd if=`, `vssadmin`, `rm -rf`). Highly notable to an analyst.
///
/// # Arguments
///
/// * `text` - The recovered text
#[must_use]
pub fn suspicious_command(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    SUSPICIOUS_COMMANDS
        .iter()
        .copied()
        .find(|c| lower.contains(c))
}

/// Whether the recovered text looks like a shell command line (a shell operator or a
/// known command prefix). A valid command is very unlikely to decode by chance, so
/// this is evidence the decode is real (raises confidence).
///
/// # Arguments
///
/// * `text` - The recovered text
#[must_use]
pub fn looks_shell_command(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    suspicious_command(text).is_some() || COMMAND_MARKERS.iter().any(|m| lower.contains(m))
}

/// A content-driven confidence + suspicion assessment for a recovered item, with a
/// rationale for each adjustment.
#[derive(Debug, Clone)]
pub struct Assessment {
    /// How accurate/true the recovery is (method reliability + content self-validation).
    pub confidence: Confidence,
    /// How important the content is to an analyst (independent of confidence).
    pub suspicion: i64,
    /// One rationale sentence per driver, for the reasoning's Assessment section.
    pub notes: Vec<String>,
}

/// Score a recovered item on two **independent** axes:
///
/// * **confidence** = `max(method_confidence, validity_floor)` — a self-validating
///   structure (key/cert/email/URL is essentially impossible to produce by brute-force
///   chance) raises confidence to `Likely`; a sentence to `Unsure`; otherwise the
///   method's own reliability stands. Never lowers.
/// * **suspicion** = how important the content is to an analyst, computed *fresh* from
///   the content (a ransom note, `.onion`, anonymous email, or embedded key is highly
///   suspicious even when decoded unreliably).
///
/// # Arguments
///
/// * `method_confidence` - The decode method's reliability (blind blob = `Untrusted`,
///   exact-key decoder-loop/crypto = `Likely`)
/// * `kind` - The classified kind of the recovered text
/// * `text` - The recovered text
#[must_use]
pub fn assess(method_confidence: Confidence, kind: RecoveredKind, text: &str) -> Assessment {
    let mut notes = Vec::new();

    // ---- confidence: method reliability, raised by content self-validation --------
    // a valid shell command (esp. a destructive one) is, like a key/email/sentence,
    // extremely unlikely to be produced by a wrong decode → evidence the decode is real
    let (validity_floor, string_reason) = match kind {
        RecoveredKind::PublicKey
        | RecoveredKind::PrivateKey
        | RecoveredKind::Certificate
        | RecoveredKind::Email
        | RecoveredKind::Url => (Confidence::Likely, ""),
        // a symmetric key is raw bytes with no self-validating structure, so the
        // decode method's own reliability is all there is to go on here
        RecoveredKind::SymmetricKey => (Confidence::Untrusted, ""),
        RecoveredKind::String if suspicious_command(text).is_some() => (
            Confidence::Likely,
            "a valid destructive shell command (could not decode by chance → confidence raised)",
        ),
        RecoveredKind::String if looks_shell_command(text) => (
            Confidence::Unsure,
            "looks like a shell command (a real decode → confidence raised)",
        ),
        RecoveredKind::String if is_sentence(text) => (
            Confidence::Unsure,
            "reads as natural-language text (a real decode → confidence raised)",
        ),
        RecoveredKind::String => (Confidence::Untrusted, ""),
    };
    let confidence = if confidence_rank(validity_floor) > confidence_rank(method_confidence) {
        notes.push(match kind {
            RecoveredKind::String => string_reason.to_string(),
            other => format!(
                "recognized {} — a structure a wrong decode could not produce by chance (confidence raised)",
                other.label().to_ascii_lowercase()
            ),
        });
        validity_floor
    } else {
        method_confidence
    };

    // ---- suspicion: importance to an analyst, independent of confidence -----------
    // collect (value, note) candidates; the final suspicion is the strongest signal
    let mut candidates: Vec<(i64, String)> = Vec::new();
    // the kind's base importance (a generic string carries no note)
    candidates.push(match kind {
        RecoveredKind::PrivateKey => (95, "recovered private key material".to_string()),
        RecoveredKind::PublicKey => (
            80,
            "embedded public key — a ransomware file-encryption key".to_string(),
        ),
        RecoveredKind::Certificate => (60, "recovered certificate".to_string()),
        RecoveredKind::SymmetricKey => (
            85,
            "recovered symmetric key — decrypts the program's own data".to_string(),
        ),
        RecoveredKind::Email => (70, "recovered email address".to_string()),
        RecoveredKind::Url => (65, "recovered URL".to_string()),
        RecoveredKind::String => (25, String::new()),
    });
    // content signals that make an item notable regardless of decode accuracy
    if looks_extortion(text) {
        candidates.push((90, "ransom/extortion wording".to_string()));
    }
    if let Some(provider) = anonymous_email_provider(text) {
        candidates.push((85, format!("anonymous email provider `{provider}`")));
    }
    if is_onion_url(text) {
        candidates.push((95, "`.onion` (Tor hidden-service) address".to_string()));
    } else if is_high_entropy_url(text) {
        candidates.push((80, "high-entropy URL host — likely C2/DGA".to_string()));
    }
    if let Some(cmd) = suspicious_command(text) {
        candidates.push((
            90,
            format!("destructive/anti-forensic shell command (`{cmd}`)"),
        ));
    } else if looks_shell_command(text) {
        candidates.push((50, "shell command".to_string()));
    }
    if matches!(kind, RecoveredKind::String) && is_sentence(text) {
        candidates.push((45, "natural-language text".to_string()));
    }
    // suspicion is the strongest candidate; record a note for each non-empty driver
    let suspicion = candidates.iter().map(|(v, _)| *v).max().unwrap_or(25);
    for (_, note) in &candidates {
        if !note.is_empty() {
            notes.push(note.clone());
        }
    }

    Assessment {
        confidence,
        suspicion,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `classify` recognizes PEM keys, emails, and URLs (even with surrounding junk).
    #[test]
    fn classify_recognizes_kinds() {
        assert_eq!(
            classify("-----BEGIN PUBLIC KEY-----\nMIIB...\n-----END PUBLIC KEY-----"),
            RecoveredKind::PublicKey
        );
        assert_eq!(
            classify("-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----"),
            RecoveredKind::Certificate
        );
        // recovered strings often carry a `>>` prefix — still classified by the token
        assert_eq!(classify("    >> 01Flip@proton.me"), RecoveredKind::Email);
        assert_eq!(
            classify("visit https://evil.example/x now"),
            RecoveredKind::Url
        );
        assert_eq!(classify("sleep 5 && rm -rf x"), RecoveredKind::String);
    }

    /// `recovered_name` types the flag and carries a meaningful snippet.
    #[test]
    fn recovered_name_is_typed_with_snippet() {
        let pem =
            "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOC\n-----END PUBLIC KEY-----";
        assert_eq!(
            recovered_name(RecoveredKind::PublicKey, pem, ".rodata", 0x1b42a0),
            "Public Key - \"MIIBIjANBgkqhkiG…\" @ .rodata @ 0x1b42a0"
        );
        assert_eq!(
            recovered_name(
                RecoveredKind::Email,
                "    >> 01Flip@proton.me",
                ".rodata",
                0x1b488c
            ),
            "Email - \"01Flip@proton.me\" @ .rodata @ 0x1b488c"
        );
    }

    /// `recovered_content` renders the payload in a fenced block plus a metadata list.
    #[test]
    fn recovered_content_fences_payload() {
        let pem = "-----BEGIN PUBLIC KEY-----\nMIIB\n-----END PUBLIC KEY-----";
        let md = recovered_content(
            RecoveredKind::PublicKey,
            pem,
            ".rodata",
            0x1b42a0,
            "pairwise SUB",
            None,
        );
        assert!(md.starts_with("### Recovered Public Key\n"));
        assert!(
            md.contains("```\n-----BEGIN PUBLIC KEY-----\nMIIB\n-----END PUBLIC KEY-----\n```")
        );
        assert!(md.contains("- **Section:** .rodata"));
        assert!(md.contains("- **Address:** 0x1b42a0"));
        assert!(md.contains("- **Scheme:** pairwise SUB"));
    }

    /// A payload containing a triple-backtick gets a longer fence so it can't break out.
    #[test]
    fn recovered_content_fence_is_safe() {
        let text = "before ``` after";
        let md = recovered_content(
            RecoveredKind::String,
            text,
            ".data",
            0x1000,
            "XOR",
            Some("0x2a"),
        );
        assert!(md.contains("````\n")); // 4-backtick fence around a 3-backtick body
    }

    /// `recovery_reasoning` adds a scheme-specific "How it was recovered" section.
    #[test]
    fn recovery_reasoning_explains_method() {
        let r = recovery_reasoning("Base explanation.", "XOR", Some("0x80"));
        assert!(r.contains("### How it was recovered"));
        assert!(r.contains("XOR-ing"));
        assert!(r.contains("`0x80`"));
        let p = recovery_reasoning("Base.", "pairwise SUB", None);
        assert!(p.contains("pairwise-subtraction"));
    }

    /// `shannon_entropy` is ~0 for a constant run and high for random-looking bytes.
    #[test]
    fn shannon_entropy_ranges() {
        assert!(shannon_entropy(b"aaaaaaaa") < 0.01);
        // a 16-hex-char host has ~4 bits/char
        assert!(shannon_entropy(b"3a9f2b7c1d8e4056") > 3.5);
    }

    /// `is_sentence` accepts natural language and rejects keys/commands.
    #[test]
    fn is_sentence_classifies_prose() {
        assert!(is_sentence(
            "Your files have been encrypted and cannot be recovered"
        ));
        assert!(!is_sentence("MIIBIjANBgkqhkiG9w0BAQEFAAOC"));
        assert!(!is_sentence(
            ">> ${cipher_data}sleep 5 && dd if=/dev/urandom of=x"
        ));
    }

    /// Anonymous providers are recognized; ordinary ones are not.
    #[test]
    fn anonymous_email_is_detected() {
        assert_eq!(
            anonymous_email_provider("    >> 01Flip@proton.me"),
            Some("proton.me")
        );
        assert_eq!(
            anonymous_email_provider("contact tuta.io? no: a@tutanota.com"),
            Some("tutanota.com")
        );
        assert_eq!(anonymous_email_provider("admin@gmail.com"), None);
    }

    /// `.onion` and high-entropy URLs are flagged; ordinary URLs are not.
    #[test]
    fn suspicious_urls_are_flagged() {
        assert!(is_onion_url("http://msydqstlz2kzerdg.onion/gate"));
        assert!(!is_onion_url("https://google.com"));
        assert!(is_high_entropy_url("http://k7f9x2a1q8z3b6w4.example/gate"));
        assert!(!is_high_entropy_url("https://google.com/search"));
        // an onion is not double-counted as a high-entropy URL
        assert!(!is_high_entropy_url("http://msydqstlz2kzerdg.onion/gate"));
    }

    /// Confidence and suspicion move on independent axes.
    #[test]
    fn assess_decouples_confidence_and_suspicion() {
        // a ransom-note sentence decoded by a shaky blob decode: low confidence, high suspicion
        let note = assess(
            Confidence::Untrusted,
            RecoveredKind::String,
            "Your files have been encrypted, pay to decrypt",
        );
        assert!(matches!(note.confidence, Confidence::Unsure)); // sentence → mild validity raise
        assert_eq!(note.suspicion, 90); // extortion wording → high importance

        // a boring string reliably decrypted: high confidence, low suspicion
        let boring = assess(Confidence::Likely, RecoveredKind::String, "kernel32.dll");
        assert!(matches!(boring.confidence, Confidence::Likely));
        assert_eq!(boring.suspicion, 25);

        // a public key recovered by blob: self-validating → confidence raised; notable
        let pem = assess(
            Confidence::Untrusted,
            RecoveredKind::PublicKey,
            "-----BEGIN PUBLIC KEY-----",
        );
        assert!(matches!(pem.confidence, Confidence::Likely));
        assert_eq!(pem.suspicion, 80);

        // an anonymous email: confidence raised (valid structure), suspicion raised
        let email = assess(
            Confidence::Untrusted,
            RecoveredKind::Email,
            "01Flip@proton.me",
        );
        assert!(matches!(email.confidence, Confidence::Likely));
        assert_eq!(email.suspicion, 85);

        // a .onion URL: strongest suspicion
        let onion = assess(
            Confidence::Untrusted,
            RecoveredKind::Url,
            "http://msydqstlz2kzerdg.onion/",
        );
        assert_eq!(onion.suspicion, 95);

        // a random generic string: unchanged, no notes
        let junk = assess(Confidence::Untrusted, RecoveredKind::String, "Xy9qZ");
        assert!(matches!(junk.confidence, Confidence::Untrusted));
        assert_eq!(junk.suspicion, 25);
        assert!(junk.notes.is_empty());

        // 01flip's self-wipe command via blob: valid destructive command → confidence
        // raised to Likely (can't decode by chance) and high suspicion
        let cmd = assess(
            Confidence::Untrusted,
            RecoveredKind::String,
            ">> ${cipher_data}sleep 5 && dd if=/dev/urandom of=x && rm ${self_name}",
        );
        assert!(matches!(cmd.confidence, Confidence::Likely));
        assert_eq!(cmd.suspicion, 90);
    }
}
