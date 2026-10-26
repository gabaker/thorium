//! Tools for raw access to the ORIGINAL sample file on disk.
//!
//! The Ghidra tools all operate on Ghidra's loaded, virtual view of the
//! sample, which only exists when the import succeeded. These tools read the
//! original file's bytes directly instead, so they work when the import fails
//! (an unrecognized format, "No load spec found") and complement Ghidra when
//! it succeeds: the format header, the overlay, and anything else Ghidra does
//! not map are only visible here.
//!
//! This is static analysis only: every tool reads bytes off the disk and
//! nothing here ever executes the sample (no process is spawned; the format is
//! identified purely by byte signatures).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use md5::Md5;
use rig::tool::{ToolContext, ToolExecutionError};
use serde::Serialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use tracing::instrument;

use crate::thorpal::detect::fileinfo::hex;
use crate::thorpal::tools::containers::resolve_target;

/// The default number of bytes one `read_file` call returns
const DEFAULT_READ_LENGTH: usize = 256;

/// The most bytes one `read_file` call may return
const MAX_READ_LENGTH: usize = 4096;

/// The default minimum printable run length counted as a string
const DEFAULT_MIN_STRING: usize = 4;

/// The default number of strings one `strings_file` call returns
pub(crate) const DEFAULT_STRINGS: usize = 200;

/// The most strings one `strings_file` call may return
pub(crate) const MAX_STRINGS: usize = 500;

/// The most characters kept per extracted string before truncation
const MAX_STRING_CHARS: usize = 512;

/// The default number of text lines one `read_text` call returns
const DEFAULT_TEXT_LINES: usize = 100;

/// The most text lines one `read_text` call may return
pub(crate) const MAX_TEXT_LINES: usize = 500;

/// The default total character budget one `read_text` call returns
const DEFAULT_TEXT_CHARS: usize = 8192;

/// The most total characters one `read_text` call may return
pub(crate) const MAX_TEXT_CHARS: usize = 16384;

/// The most characters kept per text line before truncation
const MAX_TEXT_LINE_CHARS: usize = 512;

/// The number of raw bytes one `strings_file` call scans (8 MiB)
pub(crate) const SCAN_WINDOW: usize = 8 * 1024 * 1024;

/// The most bytes a script's `read_file` binding may read at once (16 MiB)
pub(crate) const MAX_SCRIPT_READ: usize = 16 * 1024 * 1024;

/// The chunk size the hashing pass streams the file through (1 MiB)
const HASH_CHUNK: usize = 1024 * 1024;

/// The number of head bytes kept for byte-signature format identification
///
/// Sized to cover an ordinary PE's COFF header, optional header, data
/// directories, and section table (for the build-timestamp read), not just
/// the magic window the format signatures need.
pub(crate) const HEAD_LENGTH: usize = 4096;

/// One printable string extracted from the raw sample file
#[derive(Debug, Clone, Serialize)]
pub struct FileString {
    /// The file offset the string starts at, as a `0x`-prefixed hex string
    pub offset: String,
    /// The encoding the string was decoded from (`ascii` or `utf16le`)
    pub encoding: &'static str,
    /// The decoded text, truncated to [`MAX_STRING_CHARS`] characters
    pub text: String,
}

/// The strings extracted from one scanned window of the raw sample file
#[derive(Debug, Clone, Serialize)]
pub struct FileStrings {
    /// The strings found in the scanned window, in file order
    pub strings: Vec<FileString>,
    /// The file offset the scan started at, as a `0x`-prefixed hex string
    pub scanned_from: String,
    /// The file offset the scan stopped at, as a `0x`-prefixed hex string
    pub scanned_to: String,
    /// The file's total size in bytes
    pub file_size: u64,
    /// A note describing anything left unscanned and how to continue
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// One decoded, numbered line of text from the raw sample file
#[derive(Debug, Clone, Serialize)]
pub struct TextLine {
    /// The 1-based line number within the decoded window
    pub number: usize,
    /// The decoded text, truncated to [`MAX_TEXT_LINE_CHARS`] characters
    pub text: String,
}

/// The decoded text read from one window of the raw sample file
#[derive(Debug, Clone, Serialize)]
pub struct TextWindow {
    /// The encoding the text was decoded from
    pub encoding: &'static str,
    /// The decoded lines, in file order
    pub lines: Vec<TextLine>,
    /// The 1-based number of the first returned line (0 when none matched)
    pub from_line: usize,
    /// The 1-based number of the last returned line (0 when none matched)
    pub to_line: usize,
    /// The file's total size in bytes
    pub file_size: u64,
    /// A note describing anything left unread and how to continue
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// The raw sample file's identity: size, hashes, entropy, and format
#[derive(Debug, Clone, Serialize)]
pub struct SampleInfo {
    /// The sample's file name
    pub file_name: String,
    /// The file size in bytes
    pub size: u64,
    /// The MD5 of the file bytes
    pub md5: String,
    /// The SHA-1 of the file bytes
    pub sha1: String,
    /// The SHA-256 of the file bytes
    pub sha256: String,
    /// The Shannon entropy of the whole file (bits per byte, 0.0-8.0)
    pub entropy: f32,
    /// The format identified from the file's leading byte signature
    pub format: String,
    /// The build timestamp the executable header declares (UTC), when readable
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_timestamp: Option<String>,
    /// Why the declared timestamp cannot be taken at face value, when it can't
    /// (a reproducible-build hash, a future date, a zeroed field, pre-1995)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_anomaly: Option<String>,
    /// Why this file cannot be imported into Ghidra, when it can't
    /// (the identified format has no Ghidra loader)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ghidra_import: Option<String>,
}

/// Read one window of raw bytes from a file synchronously
///
/// Returns the bytes actually read (shorter than `length` at EOF, empty when
/// `offset` is at or past EOF) plus the file's total size. Shared by the
/// async tool path and the Rhai bindings so their semantics never drift.
///
/// # Arguments
///
/// * `path` - The file to read from
/// * `offset` - The file offset to start reading at
/// * `length` - The most bytes to read
pub(crate) fn read_window_sync(
    path: &Path,
    offset: u64,
    length: usize,
) -> Result<(Vec<u8>, u64), String> {
    // open the file and read its total size up front
    let mut file = std::fs::File::open(path)
        .map_err(|err| format!("failed to open the sample file '{}': {err}", path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|err| format!("failed to stat the sample file '{}': {err}", path.display()))?
        .len();
    // an offset at or past EOF reads nothing, which the caller reports
    if offset >= file_size {
        return Ok((Vec::new(), file_size));
    }
    // seek to the requested offset and read up to length bytes
    file.seek(SeekFrom::Start(offset)).map_err(|err| {
        format!(
            "failed to seek in the sample file '{}': {err}",
            path.display()
        )
    })?;
    let want = length.min(usize::try_from(file_size - offset).unwrap_or(usize::MAX));
    let mut bytes = vec![0u8; want];
    file.read_exact(&mut bytes)
        .map_err(|err| format!("failed to read the sample file '{}': {err}", path.display()))?;
    Ok((bytes, file_size))
}

/// Read one window of raw bytes from a file without blocking the runtime
///
/// # Arguments
///
/// * `path` - The file to read from
/// * `offset` - The file offset to start reading at
/// * `length` - The most bytes to read
#[instrument(skip_all, err(Debug))]
async fn read_window(path: PathBuf, offset: u64, length: usize) -> Result<(Vec<u8>, u64), String> {
    // file io is blocking, so hop onto a blocking thread for the read
    tokio::task::spawn_blocking(move || read_window_sync(&path, offset, length))
        .await
        .map_err(|err| format!("the file read task failed: {err}"))?
}

/// Render bytes as a 16-byte-per-row hexdump with an ASCII gutter
///
/// # Arguments
///
/// * `bytes` - The bytes to render
/// * `base` - The file offset the first byte sits at
pub(crate) fn hexdump(bytes: &[u8], base: u64) -> String {
    // each byte costs ~4 output characters across the hex and ascii columns
    let mut out = String::with_capacity(bytes.len() * 4 + 16);
    for (row, chunk) in bytes.chunks(16).enumerate() {
        // the offset column for this row
        out.push_str(&format!("{:08x}  ", base + (row * 16) as u64));
        // the hex column, padded so the ascii gutter always lines up
        for i in 0..16 {
            match chunk.get(i) {
                Some(byte) => out.push_str(&format!("{byte:02x} ")),
                None => out.push_str("   "),
            }
            // a mid-row gap makes the 8-byte halves easy to scan
            if i == 7 {
                out.push(' ');
            }
        }
        // the ascii gutter, with non-printable bytes rendered as dots
        out.push('|');
        for byte in chunk {
            out.push(if (0x20..0x7f).contains(byte) {
                *byte as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}

/// The 20-byte Windows shell-link signature: header size then the LNK CLSID
const LNK_MAGIC: [u8; 20] = [
    0x4c, 0x00, 0x00, 0x00, 0x01, 0x14, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x46,
];

/// Identify a file format from its leading byte signature
///
/// A first-match table of magic prefixes; purely static (nothing is parsed
/// beyond the prefix) and deliberately honest about ambiguity, e.g. a ZIP may
/// really be an OOXML document, a JAR, or an APK.
///
/// # Arguments
///
/// * `bytes` - The file's head bytes to match the signatures against
pub(crate) fn identify_format(bytes: &[u8]) -> &'static str {
    // the signature table; the first matching prefix wins
    let table: &[(&[u8], &'static str)] = &[
        (&LNK_MAGIC, "Windows shortcut (LNK)"),
        (
            b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1",
            "Composite Document File (OLE/CDF: MSI installer or legacy Office)",
        ),
        (b"\xfd7zXZ\x00", "xz compressed data"),
        (b"7z\xbc\xaf\x27\x1c", "7-Zip archive"),
        (b"Rar!\x1a\x07", "RAR archive"),
        (b"MZ", "MZ executable (Windows PE or MS-DOS)"),
        (b"\x7fELF", "ELF executable"),
        (
            b"\xfe\xed\xfa\xce",
            "Mach-O executable (32-bit, big-endian)",
        ),
        (
            b"\xfe\xed\xfa\xcf",
            "Mach-O executable (64-bit, big-endian)",
        ),
        (b"\xce\xfa\xed\xfe", "Mach-O executable (32-bit)"),
        (b"\xcf\xfa\xed\xfe", "Mach-O executable (64-bit)"),
        (
            b"\xca\xfe\xba\xbe",
            "Mach-O universal (fat) binary or Java class",
        ),
        (
            b"PK\x03\x04",
            "ZIP archive (possibly an OOXML document, JAR, or APK)",
        ),
        (
            b"PK\x05\x06",
            "ZIP archive (empty: end-of-central-directory only)",
        ),
        (b"\x1f\x8b", "gzip compressed data"),
        (b"BZh", "bzip2 compressed data"),
        (b"MSCF", "Microsoft Cabinet (CAB) archive"),
        (b"ITSF", "Compiled HTML Help (CHM)"),
        (b"%PDF", "PDF document"),
        (b"\x00asm", "WebAssembly binary"),
        (b"dex\n", "Android DEX bytecode"),
        (b"{\\rtf", "RTF document"),
        (b"\x89PNG\r\n\x1a\n", "PNG image"),
        (b"\xff\xd8\xff", "JPEG image"),
        (b"GIF87a", "GIF image"),
        (b"GIF89a", "GIF image"),
        (b"#!", "script with a shebang line"),
        (b"\xef\xbb\xbf", "UTF-8 text (BOM)"),
        (b"\xff\xfe", "UTF-16LE text (BOM)"),
        (b"\xfe\xff", "UTF-16BE text (BOM)"),
    ];
    // scan the table in order so longer, more specific prefixes win
    for (magic, name) in table {
        if bytes.starts_with(magic) {
            return name;
        }
    }
    UNKNOWN_FORMAT
}

/// The label `identify_format` returns when no byte signature matches
pub(crate) const UNKNOWN_FORMAT: &str = "unknown (no recognized magic)";

/// Whether Ghidra's best-guess import can plausibly load a file
///
/// Derived purely from the file's head bytes: Ghidra's `AutoImporter` picks a
/// loader by content, never falling back to the untargeted raw loader, so a
/// format no loader recognizes always fails with "No load spec found" after a
/// full (and expensive) headless import attempt. This classification lets
/// callers skip attempts that are certain to fail that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Loadability {
    /// A format Ghidra ships a loader for; import it normally
    Loadable,
    /// A format Ghidra demonstrably cannot load; never pay a JVM attempt
    Unloadable,
    /// No magic matched and no heuristic fired; attempt the import (it could
    /// be COFF, NE, Intel HEX, or another format a Ghidra loader accepts)
    Unknown,
}

/// A file format assessment derived from the head bytes
#[derive(Debug, Clone)]
pub(crate) struct FormatAssessment {
    /// The model-facing format label (a magic name, possibly refined with a
    /// script-language hint, or a hedged heuristic label)
    pub label: String,
    /// Whether Ghidra can plausibly import a file of this format
    pub loadability: Loadability,
}

/// Map a magic-table label onto its Ghidra loadability
///
/// Every native-executable magic Ghidra ships a loader for is `Loadable`;
/// every other recognized signature (archives, documents, images, text) is a
/// format Ghidra's best-guess import always rejects, so it is `Unloadable`.
///
/// # Arguments
///
/// * `label` - The magic-table label returned by `identify_format`
fn magic_loadability(label: &str) -> Loadability {
    // the native-executable formats Ghidra ships loaders for; the CAFEBABE
    // label ("Mach-O universal (fat) binary or Java class") starts with
    // "Mach-O" and both of its readings are loadable
    if label.starts_with("MZ executable")
        || label.starts_with("ELF executable")
        || label.starts_with("Mach-O")
        || label.starts_with("WebAssembly binary")
        || label.starts_with("Android DEX")
    {
        return Loadability::Loadable;
    }
    // an unrecognized magic stays unknown so the real importer gets a shot
    if label == UNKNOWN_FORMAT {
        return Loadability::Unknown;
    }
    // every other recognized signature has no Ghidra loader
    Loadability::Unloadable
}

/// Whether a byte reads as text (printable ASCII or common whitespace)
///
/// # Arguments
///
/// * `byte` - The byte to check
fn is_text_byte(byte: u8) -> bool {
    // the strings(1) printable range plus tab/carriage-return/newline
    is_printable(byte) || byte == b'\t' || byte == b'\r' || byte == b'\n'
}

/// Whether a BOM-less head matches the UTF-16LE text pattern
///
/// # Arguments
///
/// * `head` - The file's head bytes to test
pub(crate) fn sniff_utf16le_text(head: &[u8]) -> bool {
    // too short a head cannot establish the alternating pattern
    if head.len() < 16 {
        return false;
    }
    // count text bytes at even indexes and zero bytes at odd indexes
    let pairs = head.len() / 2;
    let text_evens = head
        .iter()
        .step_by(2)
        .filter(|byte| is_text_byte(**byte))
        .count();
    let zero_odds = head
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|byte| **byte == 0)
        .count();
    // require both halves of the pattern at 90% to call it UTF-16LE text
    text_evens * 10 >= pairs * 9 && zero_odds * 10 >= pairs * 9
}

/// The fraction of a head's bytes that read as text
///
/// # Arguments
///
/// * `head` - The file's head bytes to measure
fn printable_ratio(head: &[u8]) -> f32 {
    // an empty head carries no signal; let the caller handle it
    if head.is_empty() {
        return 0.0;
    }
    // the text-byte share of the whole head
    head.iter().filter(|byte| is_text_byte(**byte)).count() as f32 / head.len() as f32
}

/// Guess a script language from decoded text by distinct keyword hits
///
/// Deliberately conservative: a language is named only when at least two of
/// its distinct markers appear and it strictly out-scores every other
/// language, so ambiguous text stays unlabeled rather than mislabeled.
///
/// # Arguments
///
/// * `text` - The decoded text to score
fn script_language_hint(text: &str) -> Option<&'static str> {
    // score against lowercased text so casing never hides a marker
    let haystack = text.to_lowercase();
    // per-language distinct marker lists; markers are chosen to be rare
    // outside their language (e.g. "end sub" for VBScript, "invoke-" for
    // PowerShell) rather than exhaustive
    let languages: &[(&'static str, &[&str])] = &[
        (
            "VBScript",
            &[
                "dim ",
                "createobject(",
                "on error resume next",
                "end sub",
                "end function",
                "wscript.",
                "msgbox ",
                "vbcrlf",
            ],
        ),
        (
            "JavaScript",
            &[
                "function ",
                "var ",
                "eval(",
                "activexobject",
                "charcodeat",
                "fromcharcode",
                "=>",
                "const ",
                "document.",
            ],
        ),
        (
            "PowerShell",
            &[
                "invoke-",
                "new-object",
                "-join",
                "[system.",
                "iex ",
                "$env:",
                "param(",
                "write-host",
                "-encodedcommand",
                "foreach-object",
            ],
        ),
        (
            "batch script",
            &["@echo off", "goto ", "setlocal", "%~", "errorlevel"],
        ),
        (
            "HTML/HTA",
            &[
                "<script",
                "<html",
                "<hta:application",
                "<!doctype",
                "<head>",
                "<body",
            ],
        ),
    ];
    // count the distinct markers each language lands in the text
    let mut best: Option<(&'static str, usize)> = None;
    let mut tied = false;
    for (language, markers) in languages {
        let hits = markers
            .iter()
            .filter(|marker| haystack.contains(**marker))
            .count();
        // track the strict winner, remembering when the lead is shared
        match &best {
            Some((_, top)) if hits == *top => tied = true,
            Some((_, top)) if hits > *top => {
                best = Some((language, hits));
                tied = false;
            }
            None => best = Some((language, hits)),
            _ => {}
        }
    }
    // require at least two distinct markers and a strict winner
    match best {
        Some((language, hits)) if hits >= 2 && !tied => Some(language),
        _ => None,
    }
}

/// Decode UTF-16 bytes into text, tolerating invalid units
///
/// # Arguments
///
/// * `bytes` - The raw UTF-16 bytes to decode
/// * `big_endian` - Whether the units are big-endian (false means little)
fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    // reassemble the code units in the requested byte order
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| {
            if big_endian {
                u16::from_be_bytes([pair[0], pair[1]])
            } else {
                u16::from_le_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    // lossy decoding keeps the text view answering on malformed input
    String::from_utf16_lossy(&units)
}

/// Whether bytes are valid UTF-8, optionally tolerating a cut trailing sequence
///
/// # Arguments
///
/// * `head` - The bytes to validate
/// * `allow_cut_tail` - Tolerate an incomplete trailing multi-byte sequence
///   (right for a fixed-size window that may cut mid-character; wrong at a
///   real end of file, where the cut is genuine corruption)
fn is_utf8_head(head: &[u8], allow_cut_tail: bool) -> bool {
    match std::str::from_utf8(head) {
        Ok(_) => true,
        // an error with no length means the input ends mid-sequence, which a
        // fixed-size window routinely does at its boundary
        Err(error) => allow_cut_tail && error.error_len().is_none(),
    }
}

/// Detect hex-record object text (Intel HEX / Motorola S-records)
///
/// These are pure-printable text formats Ghidra ships loaders for, so the
/// text heuristic must not gate them: a head whose lines overwhelmingly match
/// the record grammar is handed back to the importer as attempt-worthy.
///
/// # Arguments
///
/// * `text` - The decoded head text to test
fn hex_record_format(text: &str) -> Option<&'static str> {
    // collect the non-empty lines the head holds
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    // too few lines cannot establish the record grammar
    if lines.len() < 4 {
        return None;
    }
    // an Intel HEX record is ':' followed by hex digits (count/addr/type/data/sum)
    let intel = |line: &str| {
        line.len() >= 11
            && line.starts_with(':')
            && line[1..].trim_end().chars().all(|c| c.is_ascii_hexdigit())
    };
    // a Motorola S-record is 'S', a digit, then hex digits
    let srec = |line: &str| {
        line.len() >= 10
            && line.starts_with('S')
            && line[1..2].chars().all(|c| c.is_ascii_digit())
            && line[2..].trim_end().chars().all(|c| c.is_ascii_hexdigit())
    };
    // ignore the final line, which a fixed window may have cut mid-record
    let whole = &lines[..lines.len() - 1];
    let matched_intel = whole.iter().filter(|line| intel(line)).count();
    let matched_srec = whole.iter().filter(|line| srec(line)).count();
    // require 90% of the whole lines to match one grammar
    if matched_intel * 10 >= whole.len() * 9 {
        return Some("Intel HEX object text");
    }
    if matched_srec * 10 >= whole.len() * 9 {
        return Some("Motorola S-record object text");
    }
    None
}

/// Refine a text label with a script-language hint when one fires
///
/// # Arguments
///
/// * `base` - The base label (e.g. "ASCII text")
/// * `qualifier` - A parenthesized qualifier to carry (e.g. "no BOM"), if any
/// * `text` - The decoded text to score for a language
fn text_label(base: &str, qualifier: Option<&str>, text: &str) -> String {
    // assemble the parenthetical from the qualifier and the language hint
    let hint = script_language_hint(text);
    match (qualifier, hint) {
        (Some(qualifier), Some(language)) => format!("{base} ({qualifier}, likely {language})"),
        (Some(qualifier), None) => format!("{base} ({qualifier})"),
        (None, Some(language)) => format!("{base} (likely {language})"),
        (None, None) => base.to_owned(),
    }
}

/// Assess a file's format and Ghidra loadability from its head bytes
///
/// Layered over [`identify_format`]: a recognized byte signature settles both
/// fields (text-ish signatures get a script-language refinement), and
/// signature-less heads fall through content heuristics — empty, all-zero,
/// BOM-less UTF-16LE, and mostly-printable ASCII/UTF-8 text — before landing
/// on `Unknown`, which callers treat as "let the real importer decide".
///
/// # Arguments
///
/// * `head` - The file's head bytes (up to [`HEAD_LENGTH`]) to assess
pub(crate) fn classify_format(head: &[u8]) -> FormatAssessment {
    // a recognized byte signature settles the label and loadability
    let magic = identify_format(head);
    if magic != UNKNOWN_FORMAT {
        // refine the text-ish signatures with a script-language hint
        let label = if magic == "script with a shebang line" {
            // quote the interpreter line itself; it is the best hint there is
            let line: String = head
                .iter()
                .take_while(|byte| **byte != b'\n' && **byte != b'\r')
                .take(64)
                .filter(|byte| is_printable(**byte))
                .map(|byte| *byte as char)
                .collect();
            format!("script with a shebang line ({line})")
        } else if magic == "UTF-8 text (BOM)" {
            text_label(
                "UTF-8 text",
                Some("BOM"),
                &String::from_utf8_lossy(&head[3..]),
            )
        } else if magic == "UTF-16LE text (BOM)" {
            text_label(
                "UTF-16LE text",
                Some("BOM"),
                &decode_utf16(&head[2..], false),
            )
        } else if magic == "UTF-16BE text (BOM)" {
            text_label(
                "UTF-16BE text",
                Some("BOM"),
                &decode_utf16(&head[2..], true),
            )
        } else {
            magic.to_owned()
        };
        return FormatAssessment {
            label,
            loadability: magic_loadability(magic),
        };
    }
    // an empty file has nothing any loader could map
    if head.is_empty() {
        return FormatAssessment {
            label: "empty file".to_owned(),
            loadability: Loadability::Unloadable,
        };
    }
    // an all-zero head is filler data, not a mappable format
    if head.iter().all(|byte| *byte == 0) {
        return FormatAssessment {
            label: format!("data (the first {} bytes are all zero)", head.len()),
            loadability: Loadability::Unloadable,
        };
    }
    // BOM-less UTF-16LE text (the Windows-script default encoding)
    if sniff_utf16le_text(head) {
        return FormatAssessment {
            label: text_label("UTF-16LE text", Some("no BOM"), &decode_utf16(head, false)),
            loadability: Loadability::Unloadable,
        };
    }
    // mostly-printable heads are text; require clean UTF-8 so binary data
    // with a printable run never misclassifies as unloadable (the head is a
    // fixed window, so a cut trailing sequence is tolerated)
    if head.len() >= 8 && printable_ratio(head) >= 0.95 && is_utf8_head(head, true) {
        let text = String::from_utf8_lossy(head);
        // hex-record object formats (Intel HEX, S-records) are text Ghidra
        // CAN load; hand them to the importer instead of gating them
        if let Some(label) = hex_record_format(&text) {
            return FormatAssessment {
                label: label.to_owned(),
                loadability: Loadability::Unknown,
            };
        }
        // call out non-ASCII bytes honestly; both decode the same way here
        let base = if head.is_ascii() {
            "ASCII text"
        } else {
            "UTF-8 text"
        };
        return FormatAssessment {
            label: text_label(base, None, &text),
            loadability: Loadability::Unloadable,
        };
    }
    // nothing matched; let the real importer make the call
    FormatAssessment {
        label: UNKNOWN_FORMAT.to_owned(),
        loadability: Loadability::Unknown,
    }
}

/// Whether a byte is printable ASCII for string extraction
///
/// # Arguments
///
/// * `byte` - The byte to check
fn is_printable(byte: u8) -> bool {
    // space through tilde, the classic strings(1) printable range
    (0x20..0x7f).contains(&byte)
}

/// Append one extracted string if it passes the filter, truncating long text
///
/// # Arguments
///
/// * `found` - The accumulated strings to append to
/// * `needle` - The lowercased case-insensitive substring filter, if any
/// * `offset` - The file offset the string starts at
/// * `encoding` - The encoding the string was decoded from
/// * `text` - The decoded (pure-ASCII) text
fn push_string(
    found: &mut Vec<FileString>,
    needle: Option<&str>,
    offset: u64,
    encoding: &'static str,
    mut text: String,
) {
    // drop strings the case-insensitive filter does not match
    if let Some(needle) = needle
        && !text.to_lowercase().contains(needle)
    {
        return;
    }
    // keep a long string's head rather than flooding the result; the text is
    // pure ASCII so a byte-indexed truncation always lands on a char boundary
    if text.len() > MAX_STRING_CHARS {
        text.truncate(MAX_STRING_CHARS);
    }
    found.push(FileString {
        offset: format!("{offset:#x}"),
        encoding,
        text,
    });
}

/// Extract printable-ASCII and UTF-16LE strings from a window of raw bytes
///
/// The end index of the UTF-16LE run starting at `start` (printable low bytes
/// paired with zero high bytes)
///
/// # Arguments
///
/// * `bytes` - The window of raw bytes to scan
/// * `start` - The index the candidate run starts at
fn wide_run_end(bytes: &[u8], start: usize) -> usize {
    // extend over printable-low/zero-high pairs until the pattern breaks
    let mut end = start;
    while end + 1 < bytes.len() && is_printable(bytes[end]) && bytes[end + 1] == 0 {
        end += 2;
    }
    end
}

/// The most candidate runs one scanning pass collects before stopping
///
/// Protects memory on pathological windows (millions of tiny runs) while
/// staying far above `MAX_STRINGS`, so a filtered scan can still look deep
/// into the window before the caller has to page.
const CANDIDATE_CAP: usize = 10_000;

/// One candidate printable run found by a scanning pass
struct Run {
    /// The window index the run starts at
    start: usize,
    /// The window index one past the run's last byte
    end: usize,
    /// Whether the run is UTF-16LE (false means plain ASCII)
    wide: bool,
}

/// Scans one encoding across the window, like one pass of GNU `strings`
///
/// Returns the qualifying runs, the window index the pass stopped at, and
/// whether the candidate cap stopped it early. Unless `at_eof` is set, a run
/// still open when the window ends is withheld and the stop index backs up to
/// its start, so a caller paging from the stop re-reads the run whole instead
/// of splitting it at the window boundary (a run spanning the entire window is
/// emitted as-is to guarantee progress).
///
/// # Arguments
///
/// * `bytes` - The window of raw bytes to scan
/// * `min_length` - The minimum run length (in characters) that qualifies
/// * `wide` - Scan UTF-16LE pairs instead of plain ASCII bytes
/// * `at_eof` - Whether the window ends at the end of the file itself
fn scan_runs(bytes: &[u8], min_length: usize, wide: bool, at_eof: bool) -> (Vec<Run>, usize, bool) {
    let mut runs = Vec::new();
    let mut i = 0;
    // walk the window collecting maximal runs of this pass's encoding
    while i < bytes.len() {
        // stop early when the candidate cap is reached, reporting where
        if runs.len() >= CANDIDATE_CAP {
            return (runs, i, true);
        }
        // measure the maximal run starting here, if any
        let end = if wide {
            // a wide run needs a printable low byte paired with a zero high
            if i + 1 >= bytes.len() || !is_printable(bytes[i]) || bytes[i + 1] != 0 {
                i += 1;
                continue;
            }
            wide_run_end(bytes, i)
        } else {
            if !is_printable(bytes[i]) {
                i += 1;
                continue;
            }
            // extend over every printable byte
            let mut end = i;
            while end < bytes.len() && is_printable(bytes[end]) {
                end += 1;
            }
            end
        };
        // a run still open at the window edge is withheld — even one shorter
        // than the minimum, since it may continue past the edge — so the next
        // page re-reads it whole (unless it spans the whole window)
        let chars = if wide { (end - i) / 2 } else { end - i };
        let open = if wide {
            end + 1 >= bytes.len()
        } else {
            end >= bytes.len()
        };
        if open && !at_eof && i > 0 {
            return (runs, i, false);
        }
        // record the run when it is long enough to count as a string
        if chars >= min_length {
            runs.push(Run {
                start: i,
                end,
                wide,
            });
        }
        i = end.max(i + 1);
    }
    (runs, bytes.len(), false)
}

/// Extracts printable strings from a window of raw bytes
///
/// Runs two independent passes — plain ASCII and UTF-16LE — and merges their
/// runs in offset order, the same convention as GNU `strings` scanning both
/// encodings: a wide string keeps its first character even when printable
/// ASCII directly precedes it (the two readings may then overlap by one byte,
/// which is reported in both). Returns the strings found, the file offset the
/// scan stopped at, and whether a limit stopped it early; the stop offset is
/// always safe to resume from without skipping or duplicating strings, and a
/// run cut by the window edge is withheld (backed up over) unless `at_eof`.
///
/// # Arguments
///
/// * `bytes` - The window of raw bytes to scan
/// * `base` - The file offset the window starts at
/// * `min_length` - The minimum run length (in characters) counted as a string
/// * `limit` - The most strings to return
/// * `filter` - An optional case-insensitive substring filter
/// * `at_eof` - Whether the window ends at the end of the file itself
pub(crate) fn extract_strings(
    bytes: &[u8],
    base: u64,
    min_length: usize,
    limit: usize,
    filter: Option<&str>,
    at_eof: bool,
) -> (Vec<FileString>, u64, bool) {
    // lowercase the filter once for case-insensitive matching
    let needle = filter.map(str::to_lowercase);
    // run the two encoding passes independently over the same window
    let (ascii_runs, ascii_stop, ascii_capped) = scan_runs(bytes, min_length, false, at_eof);
    let (wide_runs, wide_stop, wide_capped) = scan_runs(bytes, min_length, true, at_eof);
    // the scan is only complete up to the earlier of the two pass stops
    let mut stop = ascii_stop.min(wide_stop);
    let mut hit_limit = ascii_capped || wide_capped;
    // merge the passes in offset order, dropping runs past the stop so a
    // resume from `stop` never duplicates an already-returned string
    let mut merged: Vec<Run> = ascii_runs
        .into_iter()
        .chain(wide_runs)
        .filter(|run| run.start < stop)
        .collect();
    merged.sort_by_key(|run| run.start);
    // decode, filter, and cap the merged runs
    let mut found = Vec::new();
    for run in merged {
        // stop at the limit; everything from this run on is left for paging
        if found.len() >= limit {
            stop = run.start;
            hit_limit = true;
            break;
        }
        // decode the run's text per its encoding
        let text: String = if run.wide {
            bytes[run.start..run.end]
                .iter()
                .step_by(2)
                .map(|byte| *byte as char)
                .collect()
        } else {
            bytes[run.start..run.end]
                .iter()
                .map(|byte| *byte as char)
                .collect()
        };
        let encoding = if run.wide { "utf16le" } else { "ascii" };
        push_string(
            &mut found,
            needle.as_deref(),
            base + run.start as u64,
            encoding,
            text,
        );
    }
    // report where the scan stopped so a limited scan can be continued
    (found, base + stop as u64, hit_limit)
}

/// The Shannon entropy (bits per byte) from a streamed byte histogram
///
/// The streamed twin of [`crate::thorpal::detect::decode::shannon_entropy`]
/// (same formula, pinned equivalent by a test), computed from counts
/// accumulated chunk by chunk so the whole file never has to sit in memory.
///
/// # Arguments
///
/// * `counts` - The per-byte-value occurrence counts
/// * `total` - The total number of bytes counted
fn entropy_from_counts(counts: &[u64; 256], total: u64) -> f32 {
    // an empty file carries no information
    if total == 0 {
        return 0.0;
    }
    let len = total as f32;
    // -sum(p * log2(p)) over observed symbols
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = count as f32 / len;
            -p * p.log2()
        })
        .sum()
}

/// Compute the raw sample file's identity synchronously
///
/// Streams the whole file through MD5/SHA-1/SHA-256 and an entropy histogram
/// in [`HASH_CHUNK`] chunks, keeping only the head bytes the byte-signature
/// format table reads, so arbitrarily large samples never sit in memory.
/// Shared by the async tool path and the Rhai binding.
///
/// # Arguments
///
/// * `path` - The sample file to identify
pub(crate) fn file_identity_sync(path: &Path) -> Result<SampleInfo, String> {
    // open the file for the streamed hashing pass
    let mut file = std::fs::File::open(path)
        .map_err(|err| format!("failed to open the sample file '{}': {err}", path.display()))?;
    // one hasher per digest, all fed the same chunks
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    // a byte histogram for the whole-file entropy
    let mut counts = [0u64; 256];
    // the head bytes format identification matches its signatures against
    let mut head: Vec<u8> = Vec::with_capacity(HEAD_LENGTH);
    let mut size = 0u64;
    let mut chunk = vec![0u8; HASH_CHUNK];
    loop {
        // read the next chunk, stopping cleanly at EOF
        let read = file
            .read(&mut chunk)
            .map_err(|err| format!("failed to read the sample file '{}': {err}", path.display()))?;
        if read == 0 {
            break;
        }
        let bytes = &chunk[..read];
        // feed every digest and the entropy histogram
        md5.update(bytes);
        sha1.update(bytes);
        sha256.update(bytes);
        for byte in bytes {
            counts[*byte as usize] += 1;
        }
        // keep the head bytes until the signature window is full
        if head.len() < HEAD_LENGTH {
            let want = (HEAD_LENGTH - head.len()).min(bytes.len());
            head.extend_from_slice(&bytes[..want]);
        }
        size += read as u64;
    }
    // read the PE build timestamp off the head, following the debug directory
    // with one bounded seek+read to spot a reproducible-build marker
    let stamp = crate::thorpal::detect::fileinfo::pe_timestamp(&head, |offset, len| {
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).ok()?;
        Some(buf)
    });
    let (build_timestamp, timestamp_anomaly) = match &stamp {
        Some(stamp) => {
            // classify against the wall clock so future-dating is callable
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            crate::thorpal::detect::fileinfo::describe_timestamp(stamp, now)
        }
        // non-PE formats declare no build time this surface reads
        None => (None, None),
    };
    // assess the format and its Ghidra loadability off the head bytes
    let assessment = classify_format(&head);
    // an unloadable format gets a model-facing hint that pre-empts a doomed
    // import_child call: the right move is raw triage or extraction, not Ghidra
    let ghidra_import = match assessment.loadability {
        // an OLE compound file additionally points at the macro extractor,
        // since a legacy Office document's logic lives in macros, not streams
        Loadability::Unloadable if assessment.label.starts_with("Composite Document File") => {
            Some(format!(
                "not importable: {} has no Ghidra loader -- enumerate it with \
                 list_container/extract_file, and if it is an Office document \
                 (Macros/VBA, WordDocument, or Workbook streams) run extract_macros \
                 for its VBA source and XLM macro sheets; extract or decode an \
                 embedded executable and import that instead",
                assessment.label
            ))
        }
        Loadability::Unloadable => Some(format!(
            "not importable: {} has no Ghidra loader -- analyze it with read_text, \
             read_file, and strings_file (and the container/decode tools); extract \
             or decode an embedded executable and import that instead",
            assessment.label
        )),
        // loadable and unknown formats are the importer's call, not ours
        Loadability::Loadable | Loadability::Unknown => None,
    };
    // assemble the identity from the streamed facts
    Ok(SampleInfo {
        // name the file itself; a path with no final component (never produced
        // by the session wiring) degrades to a neutral literal, never the full
        // path, so an anonymized run cannot leak its original location here
        file_name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_owned()),
        size,
        md5: hex(md5.finalize().as_slice()),
        sha1: hex(sha1.finalize().as_slice()),
        sha256: hex(sha256.finalize().as_slice()),
        entropy: entropy_from_counts(&counts, size),
        format: assessment.label,
        build_timestamp,
        timestamp_anomaly,
        ghidra_import,
    })
}

/// Compute the raw sample file's identity without blocking the runtime
///
/// # Arguments
///
/// * `path` - The sample file to identify
#[instrument(skip_all, err(Debug))]
pub(crate) async fn file_identity(path: &Path) -> Result<SampleInfo, String> {
    // file io is blocking, so hop onto a blocking thread for the pass
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || file_identity_sync(&path))
        .await
        .map_err(|err| format!("the file identity task failed: {err}"))?
}

#[thorium_derive::thorpal_tool(
    group = "file",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_read_file",
    rhai_doc = "read_file(offset: int, size: int) | read_file(target: string, offset, size) -> blob -- read raw file bytes",
    rhai_reference = "Reads raw bytes of the ORIGINAL sample file on disk, independent of
Ghidra's loaded view (works even when the import failed), or of an extracted
file when the `read_file(target: string, offset: int, size: int)` overload
names one (see list_container/extract_file; `\"\"` means the original sample).

`offset` is the file offset to start at; `size` is the byte count (capped at
16 MiB per call). A read past EOF comes back shorter (or empty).

Returns a blob (byte array): index it for ints (`data[0]`), slice-parse ints
(`data.parse_le_int(0..4)`), or reinterpret it (`data.as_string()`,
`data.to_array()`)."
)]
#[rig::tool_macro(
    description = "Read raw bytes of the ORIGINAL sample file on disk as a hexdump, or of an extracted file when `target` names one (see list_container/extract_file) -- in a multi-sample session `target` also addresses a sibling sample by its file name. Works even when Ghidra cannot import the file, and sees bytes Ghidra does not map (format headers, overlays, appended data). offset defaults to 0; length defaults to 256 and caps at 4096 per call."
)]
pub async fn read_file(
    #[rig(context)] context: &mut ToolContext,
    offset: Option<u64>,
    length: Option<usize>,
    target: Option<String>,
) -> Result<String, rig::tool::ToolExecutionError> {
    // resolve the original sample, or an extracted file when targeted
    let path = resolve_target(context, target.as_deref()).await?;
    // clamp the window to the per-call cap, defaulting to a small read
    let offset = offset.unwrap_or(0);
    let length = length
        .unwrap_or(DEFAULT_READ_LENGTH)
        .clamp(1, MAX_READ_LENGTH);
    // read the window off a blocking thread; the error already names the path
    let (bytes, file_size) = read_window(path, offset, length)
        .await
        .map_err(ToolExecutionError::invalid_args)?;
    // an offset past EOF is an answer, not an error
    if bytes.is_empty() {
        return Ok(format!(
            "offset {offset:#x} is at or past the end of the file ({file_size} bytes); \
             nothing to read"
        ));
    }
    // render the window as a hexdump under a short context header
    Ok(format!(
        "file size: {file_size} bytes; showing {} bytes at offset {offset:#x}\n{}",
        bytes.len(),
        hexdump(&bytes, offset),
    ))
}

/// Decode one raw window into text, sniffing the encoding
///
/// A BOM settles the encoding outright; a BOM-less window whose head matches
/// the UTF-16LE text pattern decodes as UTF-16LE; everything else decodes as
/// UTF-8, lossily when it must, so the text view always answers.
///
/// # Arguments
///
/// * `bytes` - The raw window to decode
/// * `at_eof` - Whether the window ends at the end of the file itself (a cut
///   trailing UTF-8 sequence is then real corruption, not a window artifact)
pub(crate) fn decode_text(bytes: &[u8], at_eof: bool) -> (&'static str, String) {
    // a BOM settles the encoding outright
    if let Some(rest) = bytes.strip_prefix(b"\xef\xbb\xbf") {
        // call out replacement honestly when the body is not clean UTF-8
        let label = if is_utf8_head(rest, !at_eof) {
            "utf-8"
        } else {
            "utf-8 (lossy)"
        };
        return (label, String::from_utf8_lossy(rest).into_owned());
    }
    if let Some(rest) = bytes.strip_prefix(b"\xff\xfe") {
        return ("utf-16le", decode_utf16(rest, false));
    }
    if let Some(rest) = bytes.strip_prefix(b"\xfe\xff") {
        return ("utf-16be", decode_utf16(rest, true));
    }
    // a BOM-less head matching the UTF-16LE text pattern decodes as such
    if sniff_utf16le_text(&bytes[..bytes.len().min(HEAD_LENGTH)]) {
        return ("utf-16le", decode_utf16(bytes, false));
    }
    // everything else decodes as UTF-8, lossily when it must
    let label = if is_utf8_head(bytes, !at_eof) {
        "utf-8"
    } else {
        "utf-8 (lossy)"
    };
    (label, String::from_utf8_lossy(bytes).into_owned())
}

/// Read one window of a file as decoded, line-numbered text synchronously
///
/// Reads one [`SCAN_WINDOW`] window at `offset`, decodes it (BOM, UTF-16LE
/// heuristic, else UTF-8 with lossy replacement), splits it into 1-based
/// numbered lines (relative to the window anchor), and pages them by
/// `start_line` under the `max_lines`/`max_chars` budgets. Shared by the
/// async tool path, the Rhai binding, and the seed-time preview so their
/// semantics never drift.
///
/// # Arguments
///
/// * `path` - The file to read from
/// * `offset` - The file offset the window is anchored at
/// * `start_line` - The 1-based line (within the window) to start from
/// * `max_lines` - The most lines to return
/// * `max_chars` - The total character budget across returned lines
/// * `filter` - An optional case-insensitive substring filter on lines
pub(crate) fn read_text_sync(
    path: &Path,
    offset: u64,
    start_line: usize,
    max_lines: usize,
    max_chars: usize,
    filter: Option<&str>,
) -> Result<TextWindow, String> {
    // read one scan window at the requested byte anchor
    let (bytes, file_size) = read_window_sync(path, offset, SCAN_WINDOW)?;
    let at_eof = offset + bytes.len() as u64 >= file_size;
    // an offset past EOF is an answer, not an error
    if bytes.is_empty() {
        return Ok(TextWindow {
            encoding: "utf-8",
            lines: Vec::new(),
            from_line: 0,
            to_line: 0,
            file_size,
            truncated: Some(format!(
                "offset {offset:#x} is at or past the end of the file ({file_size} \
                 bytes); nothing to read"
            )),
        });
    }
    // decode the window and normalize the three newline conventions
    let (encoding, text) = decode_text(&bytes, at_eof);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    // split into lines; a trailing newline is a terminator, not an empty line
    let mut raw_lines: Vec<&str> = normalized.split('\n').collect();
    if normalized.ends_with('\n') {
        raw_lines.pop();
    }
    let total_lines = raw_lines.len();
    // lowercase the filter once for case-insensitive matching
    let needle = filter.map(str::to_lowercase);
    // walk the lines from the requested start under both budgets
    let start_line = start_line.max(1);
    let mut lines = Vec::new();
    let mut budget = max_chars;
    let mut resume_line = None;
    for (index, raw) in raw_lines.iter().enumerate().skip(start_line - 1) {
        let number = index + 1;
        // the filter keeps only matching lines, numbers preserved
        if let Some(needle) = &needle
            && !raw.to_lowercase().contains(needle.as_str())
        {
            continue;
        }
        // stop at the line budget, remembering where to resume
        if lines.len() >= max_lines {
            resume_line = Some(number);
            break;
        }
        // keep the line's head under the per-line cap and the char budget;
        // a first line too long for the whole budget is cut (not skipped)
        // so a pathological file still makes progress
        let keep = raw.chars().count().min(MAX_TEXT_LINE_CHARS);
        let keep = if keep > budget {
            if lines.is_empty() { budget } else { 0 }
        } else {
            keep
        };
        if keep == 0 && !raw.is_empty() {
            resume_line = Some(number);
            break;
        }
        // truncated lines carry a marker so the cut is visible
        let mut kept: String = raw.chars().take(keep).collect();
        if keep < raw.chars().count() {
            kept.push('…');
        }
        budget -= keep;
        lines.push(TextLine { number, text: kept });
    }
    // report the returned range off the emitted lines themselves
    let from_line = lines.first().map(|line| line.number).unwrap_or(0);
    let to_line = lines.last().map(|line| line.number).unwrap_or(0);
    // note anything left unread and the exact way to continue
    let truncated = if let Some(resume) = resume_line {
        Some(format!(
            "line/character budget hit; rerun with line = {resume} (window \
             anchored at offset {offset:#x}) to continue"
        ))
    } else if !at_eof {
        Some(format!(
            "decoded one {} MiB window of {file_size} bytes; rerun with offset = \
             {:#x} to continue (line numbers restart at the new anchor)",
            SCAN_WINDOW / (1024 * 1024),
            offset + bytes.len() as u64,
        ))
    } else if start_line > total_lines {
        Some(format!(
            "the window holds {total_lines} lines; line {start_line} is past its end"
        ))
    } else if lines.is_empty() && needle.is_some() {
        Some("no lines matched the filter in this window".to_owned())
    } else {
        None
    };
    Ok(TextWindow {
        encoding,
        lines,
        from_line,
        to_line,
        file_size,
        truncated,
    })
}

/// Read one window of a file as decoded text without blocking the runtime
///
/// # Arguments
///
/// * `path` - The file to read from
/// * `offset` - The file offset the window is anchored at
/// * `start_line` - The 1-based line (within the window) to start from
/// * `max_lines` - The most lines to return
/// * `max_chars` - The total character budget across returned lines
/// * `filter` - An optional case-insensitive substring filter on lines
#[instrument(skip_all, err(Debug))]
pub(crate) async fn read_text_window(
    path: PathBuf,
    offset: u64,
    start_line: usize,
    max_lines: usize,
    max_chars: usize,
    filter: Option<String>,
) -> Result<TextWindow, String> {
    // file io is blocking, so hop onto a blocking thread for the read
    tokio::task::spawn_blocking(move || {
        read_text_sync(
            &path,
            offset,
            start_line,
            max_lines,
            max_chars,
            filter.as_deref(),
        )
    })
    .await
    .map_err(|err| format!("the text read task failed: {err}"))?
}

#[thorium_derive::thorpal_tool(
    group = "file",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_read_text",
    rhai_doc = "read_text(line: int) | read_text(line, max_lines: int) | read_text(target: string, line[, max_lines]) -> array -- read a raw file as line-numbered text",
    rhai_reference = "Reads the ORIGINAL sample file on disk as decoded, line-numbered text
(one 8 MiB window from the start), independent of Ghidra's view, or an
extracted file when the string-first overloads name one (see
list_container/extract_file; `\"\"` means the original sample).

Overloads: `read_text(line: int)`, `read_text(line: int, max_lines: int)`,
`read_text(target: string, line: int)`, and `read_text(target: string, line:
int, max_lines: int)`. `line` is the 1-based line to start from; `max_lines`
caps how many come back (500 max). The encoding is sniffed (BOM, UTF-16LE
heuristic, else UTF-8 with lossy replacement).

A 16384-character total budget also applies, so fewer than `max_lines` lines
can come back with more still unread -- to read everything, page by `line =
last returned number + 1` until the result is empty, never by comparing the
count against `max_lines`.

Returns an array of maps, each with: `number` (1-based int), `text` (string,
truncated past 512 characters)."
)]
#[rig::tool_macro(
    description = "Read the ORIGINAL sample file on disk as decoded, line-numbered TEXT -- the text twin of read_file's hexdump -- or an extracted file / sibling sample when `target` names one (see list_container/extract_file). Auto-detects UTF-8 and UTF-16 (BOM or heuristic) and replaces non-text bytes lossily, so it works on any file Ghidra cannot import: scripts (VBScript/JavaScript/PowerShell/batch/HTA), configs, logs, and other text. Pages by `line` (1-based, default 1) with `max_lines` (default 100, max 500) under a `max_chars` total budget (default 8192, max 16384); `filter` keeps only lines containing a case-insensitive substring (numbers preserved); `offset` re-anchors the 8 MiB decode window for files larger than it (line numbers restart at the anchor)."
)]
pub async fn read_text(
    #[rig(context)] context: &mut ToolContext,
    line: Option<usize>,
    max_lines: Option<usize>,
    max_chars: Option<usize>,
    filter: Option<String>,
    offset: Option<u64>,
    target: Option<String>,
) -> Result<TextWindow, rig::tool::ToolExecutionError> {
    // resolve the original sample, or an extracted file when targeted
    let path = resolve_target(context, target.as_deref()).await?;
    // clamp the knobs to usable ranges
    let start_line = line.unwrap_or(1).max(1);
    let max_lines = max_lines
        .unwrap_or(DEFAULT_TEXT_LINES)
        .clamp(1, MAX_TEXT_LINES);
    let max_chars = max_chars
        .unwrap_or(DEFAULT_TEXT_CHARS)
        .clamp(1, MAX_TEXT_CHARS);
    let offset = offset.unwrap_or(0);
    // decode the window off a blocking thread; the error already names the path
    read_text_window(path, offset, start_line, max_lines, max_chars, filter)
        .await
        .map_err(rig::tool::ToolExecutionError::invalid_args)
}

#[thorium_derive::thorpal_tool(
    group = "file",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_strings_file",
    rhai_doc = "strings_file(min_len) | strings_file(min_len, limit) | strings_file(target: string, min_len[, limit]) -> array -- extract strings from a raw file",
    rhai_reference = "Extracts printable-ASCII and UTF-16LE strings from the raw sample
file on disk (one 8 MiB window from the start), independent of Ghidra's view,
or from an extracted file when the string-first overloads name one (see
list_container/extract_file; `\"\"` means the original sample).

Overloads: `strings_file(min_len: int)`, `strings_file(min_len: int, limit:
int)`, `strings_file(target: string, min_len: int)`, and
`strings_file(target: string, min_len: int, limit: int)`.
`min_len` is the minimum run length in characters; `limit` caps how many come
back (500 max).

Returns an array of maps, each with: `offset` (0x-prefixed hex string),
`encoding` (`\"ascii\"` or `\"utf16le\"`), `text` (string)."
)]
#[rig::tool_macro(
    description = "Extract printable-ASCII and UTF-16LE strings from the ORIGINAL sample file on disk, or from an extracted file when `target` names one (see list_container/extract_file; in a multi-sample session `target` also addresses a sibling sample by its file name), scanning one 8 MiB window from offset (default 0). Works even when Ghidra cannot import the file and sees strings in regions Ghidra does not map. Supports a minimum length (default 4), a result limit (default 200, max 500), and a case-insensitive substring filter."
)]
pub async fn strings_file(
    #[rig(context)] context: &mut ToolContext,
    min_length: Option<usize>,
    offset: Option<u64>,
    limit: Option<usize>,
    filter: Option<String>,
    target: Option<String>,
) -> Result<FileStrings, rig::tool::ToolExecutionError> {
    // resolve the original sample, or an extracted file when targeted
    let path = resolve_target(context, target.as_deref()).await?;
    // clamp the knobs to usable ranges
    let min_length = min_length.unwrap_or(DEFAULT_MIN_STRING).max(2);
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(DEFAULT_STRINGS).clamp(1, MAX_STRINGS);
    // read one scan window off a blocking thread
    let (bytes, file_size) = read_window(path, offset, SCAN_WINDOW)
        .await
        .map_err(ToolExecutionError::invalid_args)?;
    // extract the printable runs from the window; the extractor backs the stop
    // offset up over any run cut by the window edge unless this is the file end
    let at_eof = offset + bytes.len() as u64 >= file_size;
    let (strings, scanned_to, hit_limit) =
        extract_strings(&bytes, offset, min_length, limit, filter.as_deref(), at_eof);
    // note anything left unscanned; the resume offset is always scanned_to, so
    // paging never skips bytes the limit or the window edge left unscanned
    let note = if hit_limit {
        Some(format!(
            "string limit of {limit} hit at {scanned_to:#x}; rerun with offset = \
             {scanned_to:#x} to continue"
        ))
    } else if scanned_to < file_size {
        Some(format!(
            "scanned one {} MiB window up to {scanned_to:#x} of {file_size} bytes; \
             rerun with offset = {scanned_to:#x} to continue",
            SCAN_WINDOW / (1024 * 1024),
        ))
    } else {
        None
    };
    Ok(FileStrings {
        strings,
        scanned_from: format!("{offset:#x}"),
        scanned_to: format!("{scanned_to:#x}"),
        file_size,
        truncated: note,
    })
}

#[thorium_derive::thorpal_tool(
    group = "file",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_file_info",
    rhai_doc = "file_info() | file_info(target: string) -> map -- a file's size, hashes, entropy, and identified format",
    rhai_reference = "Gets the ORIGINAL sample file's identity, read off the raw bytes on
disk (works even when Ghidra could not import the file), or an extracted
file's when the `file_info(target: string)` overload names one (see
list_container/extract_file; `\"\"` means the original sample).

Returns a map with: `file_name` (string), `size` (int), `md5`/`sha1`/`sha256`
(lower-case hex strings), `entropy` (float, bits per byte 0.0-8.0), `format`
(string identified from the leading byte signature, refined with text/script
heuristics), and -- only when the format has no Ghidra loader --
`ghidra_import` (string explaining why an import would fail)."
)]
#[rig::tool_macro(
    description = "Get the ORIGINAL sample file's identity -- or an extracted file's when `target` names one (see list_container/extract_file; in a multi-sample session `target` also addresses a sibling sample by its file name): size, MD5/SHA1/SHA256, whole-file Shannon entropy, and the format identified from its byte signature. Works even when Ghidra cannot import the file -- use it (with read_file and strings_file) to triage an unimportable sample."
)]
pub async fn file_info(
    #[rig(context)] context: &mut ToolContext,
    target: Option<String>,
) -> Result<SampleInfo, rig::tool::ToolExecutionError> {
    // resolve the original sample, or an extracted file when targeted
    let path = resolve_target(context, target.as_deref()).await?;
    // stream the file through the hashing/entropy pass off a blocking thread
    file_identity(&path)
        .await
        .map_err(ToolExecutionError::invalid_args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::decode::shannon_entropy;
    use crate::thorpal::store::SessionStore;
    use crate::thorpal::tools::ghidra::{GhidraSession, GhidraSettings};

    /// Builds a tool context whose shared session points at a sample file
    ///
    /// Mirrors the scripts tests: a Ghidra session with no install configured
    /// still carries the binary path the file tools resolve.
    ///
    /// # Arguments
    ///
    /// * `binary` - The path to the sample file the context points at
    fn context(binary: &Path) -> ToolContext {
        // a session with no Ghidra install still carries the binary path
        let ghidra = GhidraSession::new(GhidraSettings::from_binary(binary, "projects", None));
        // seed the context exactly like ThorPal::tool_context does
        let mut context = ToolContext::new();
        context.insert(ghidra);
        context
    }

    /// Writes a sample file into a scratch dir and returns its path
    ///
    /// # Arguments
    ///
    /// * `dir` - The scratch dir to write into
    /// * `bytes` - The sample's contents
    fn write_sample(dir: &tempfile::TempDir, bytes: &[u8]) -> PathBuf {
        // materialize the fake sample the tools read
        let path = dir.path().join("sample.bin");
        std::fs::write(&path, bytes).expect("sample should write");
        path
    }

    /// The hexdump renders offsets, split hex columns, and the ascii gutter.
    #[test]
    fn hexdump_renders_the_classic_layout() {
        // one full row plus a short tail row, based at a non-zero offset
        let bytes = b"MZ\x90\x00ABCDEFGHIJKLend";
        let dump = hexdump(bytes, 0x10);
        let lines: Vec<&str> = dump.lines().collect();
        assert_eq!(
            lines[0],
            "00000010  4d 5a 90 00 41 42 43 44  45 46 47 48 49 4a 4b 4c |MZ..ABCDEFGHIJKL|"
        );
        // the tail row pads the hex column so the gutter still lines up
        assert!(lines[1].starts_with("00000020  65 6e 64 "));
        assert!(lines[1].ends_with("|end|"));
        assert_eq!(lines.len(), 2);
    }

    /// The signature table identifies the common formats and misses cleanly.
    #[test]
    fn identify_format_matches_known_magics() {
        // the CDF/OLE magic names the MSI/legacy-Office ambiguity
        assert_eq!(
            identify_format(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1corn"),
            "Composite Document File (OLE/CDF: MSI installer or legacy Office)"
        );
        assert_eq!(
            identify_format(b"MZ\x90\x00"),
            "MZ executable (Windows PE or MS-DOS)"
        );
        assert_eq!(identify_format(b"\x7fELF\x02\x01\x01"), "ELF executable");
        assert_eq!(
            identify_format(b"PK\x03\x04corn"),
            "ZIP archive (possibly an OOXML document, JAR, or APK)"
        );
        // the full 20-byte LNK signature is required, not just the header size
        assert_eq!(identify_format(&LNK_MAGIC), "Windows shortcut (LNK)");
        assert_eq!(
            identify_format(&LNK_MAGIC[..19]),
            "unknown (no recognized magic)"
        );
        assert_eq!(identify_format(b"corncob"), "unknown (no recognized magic)");
        assert_eq!(identify_format(b""), "unknown (no recognized magic)");
    }

    /// Builds the mixed ascii/utf16le buffer the extraction tests scan
    fn string_fixture() -> Vec<u8> {
        let mut bytes = Vec::new();
        // leading noise and a too-short ascii run
        bytes.extend_from_slice(b"\x00\x01hi\x00");
        // an ascii hit at offset 5
        bytes.extend_from_slice(b"corncob\x00");
        // a utf16le hit at offset 13
        for byte in b"wide" {
            bytes.push(*byte);
            bytes.push(0);
        }
        // trailing noise
        bytes.extend_from_slice(b"\xff\xfe");
        bytes
    }

    /// Both encodings extract with their offsets rebased onto the file.
    #[test]
    fn extract_strings_finds_ascii_and_utf16le() {
        let bytes = string_fixture();
        let (found, scanned_to, hit_limit) =
            extract_strings(&bytes, 0x100, 4, MAX_STRINGS, None, true);
        // the short run is dropped; the ascii and wide hits both land. the
        // wide pass reads corncob's final 'b' plus its NUL terminator as a
        // leading wide pair — the GNU strings convention for the two passes
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].offset, "0x105");
        assert_eq!(found[0].encoding, "ascii");
        assert_eq!(found[0].text, "corncob");
        assert_eq!(found[1].offset, "0x10b");
        assert_eq!(found[1].encoding, "utf16le");
        assert_eq!(found[1].text, "bwide");
        // the scan ran to the end of the window without hitting the limit
        assert_eq!(scanned_to, 0x100 + bytes.len() as u64);
        assert!(!hit_limit);
    }

    /// The minimum length, filter, and limit all narrow the extraction.
    #[test]
    fn extract_strings_honors_min_length_filter_and_limit() {
        let bytes = string_fixture();
        // a longer minimum drops the five-character wide string
        let (found, _, _) = extract_strings(&bytes, 0, 6, MAX_STRINGS, None, true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "corncob");
        // the filter matches case-insensitively against the text
        let (found, _, _) = extract_strings(&bytes, 0, 4, MAX_STRINGS, Some("CORN"), true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "corncob");
        // the limit stops the scan mid-window, reports where, and says why;
        // the resume offset is the start of the first undelivered string
        let (found, scanned_to, hit_limit) = extract_strings(&bytes, 0, 4, 1, None, true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "corncob");
        assert_eq!(scanned_to, 11);
        assert!(hit_limit);
    }

    /// Very long runs keep only their head, cut at the per-string cap.
    #[test]
    fn extract_strings_truncates_long_runs() {
        // a 600-character printable run exceeds the per-string cap
        let bytes = vec![b'A'; 600];
        let (found, _, _) = extract_strings(&bytes, 0, 4, MAX_STRINGS, None, true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text.len(), MAX_STRING_CHARS);
    }

    /// An ascii run never swallows the first low byte of an adjacent wide run.
    #[test]
    fn extract_strings_keeps_adjacent_wide_strings_whole() {
        // printable ascii immediately followed by a utf16le string
        let mut bytes = b"BAD!".to_vec();
        for byte in b"widestr" {
            bytes.push(*byte);
            bytes.push(0);
        }
        let (found, _, _) = extract_strings(&bytes, 0, 4, MAX_STRINGS, None, true);
        // the wide string lands whole at its true offset; the ascii pass reads
        // the overlapping first low byte too, per the two-pass convention
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].text.as_str(), found[0].offset.as_str()),
            ("BAD!w", "0x0")
        );
        assert_eq!(
            (found[1].text.as_str(), found[1].offset.as_str()),
            ("widestr", "0x4")
        );
        // a lone printable byte before a minimum-length wide run drops neither
        let mut bytes = vec![b'P'];
        for byte in b"wide" {
            bytes.push(*byte);
            bytes.push(0);
        }
        let (found, _, _) = extract_strings(&bytes, 0, 4, MAX_STRINGS, None, true);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].text.as_str(), found[0].offset.as_str()),
            ("wide", "0x1")
        );
    }

    /// A run cut by the window edge is withheld and the stop offset backs up.
    #[test]
    fn extract_strings_backs_up_over_window_cut_runs() {
        // noise, a complete string, then a run the window cuts mid-way
        let bytes = b"\x00\x01corncob\x00ABC";
        let (found, scanned_to, hit_limit) = extract_strings(bytes, 0, 4, MAX_STRINGS, None, false);
        // the cut trailing run is withheld; the resume offset is its start
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "corncob");
        assert_eq!(scanned_to, 10);
        assert!(!hit_limit);
        // the same bytes at the end of the file emit the trailing run normally
        let (found, scanned_to, _) = extract_strings(bytes, 0, 3, MAX_STRINGS, None, true);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].text, "ABC");
        assert_eq!(scanned_to, bytes.len() as u64);
        // a run spanning the whole window is emitted to guarantee progress
        let bytes = vec![b'A'; 32];
        let (found, scanned_to, _) = extract_strings(&bytes, 0, 4, MAX_STRINGS, None, false);
        assert_eq!(found.len(), 1);
        assert_eq!(scanned_to, 32);
    }

    /// The streamed histogram entropy matches the in-memory formula.
    #[test]
    fn entropy_from_counts_matches_shannon_entropy() {
        // a mixed buffer with an uneven byte distribution
        let bytes = b"the quick brown fox jumps over 13 lazy dogs \x00\x01\x02\xff";
        let mut counts = [0u64; 256];
        for byte in bytes {
            counts[*byte as usize] += 1;
        }
        let streamed = entropy_from_counts(&counts, bytes.len() as u64);
        assert!((streamed - shannon_entropy(bytes)).abs() < 1e-4);
        // an empty file reads as zero entropy, not NaN
        assert_eq!(entropy_from_counts(&[0u64; 256], 0), 0.0);
    }

    /// The identity pass hashes a real file to the known digests.
    #[tokio::test]
    async fn file_identity_hashes_a_tempfile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_sample(&dir, b"abcd");
        let info = file_identity(&path).await.expect("identity should compute");
        // the digests of "abcd" are well-known constants
        assert_eq!(info.file_name, "sample.bin");
        assert_eq!(info.size, 4);
        assert_eq!(info.md5, "e2fc714c4727ee9395f324cd2e7f331f");
        assert_eq!(info.sha1, "81fe8bfe87576c3ecb22426f8e57847382917acf");
        assert_eq!(
            info.sha256,
            "88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589"
        );
        // four distinct bytes carry exactly two bits each
        assert!((info.entropy - 2.0).abs() < 1e-6);
        assert_eq!(info.format, "unknown (no recognized magic)");
        // a missing file surfaces as an error naming the path
        let error = file_identity(&dir.path().join("ghost"))
            .await
            .expect_err("a missing file should fail");
        assert!(error.contains("ghost"));
    }

    /// The read_file tool hexdumps real bytes and answers past-EOF reads.
    #[tokio::test]
    async fn read_file_tool_dumps_the_raw_sample() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_sample(&dir, b"MZ\x90\x00CORNFIELD");
        // build the missing-sample context before the name is shadowed below
        let mut missing = context(&dir.path().join("ghost"));
        let mut context = context(&path);
        // a plain read dumps from the start with the size header
        let dump = read_file(&mut context, None, None, None)
            .await
            .expect("the read should succeed");
        assert!(dump.starts_with("file size: 13 bytes; showing 13 bytes at offset 0x0"));
        assert!(dump.contains("4d 5a 90 00"));
        assert!(dump.contains("|MZ..CORNFIELD|"));
        // an offset past EOF is a short answer, not an error
        let past = read_file(&mut context, Some(1000), None, None)
            .await
            .expect("a past-EOF read should still answer");
        assert!(past.contains("past the end"));
        // a missing sample surfaces as a tool error naming the path
        let error = read_file(&mut missing, None, None, None)
            .await
            .expect_err("a missing sample should fail");
        assert!(error.to_string().contains("ghost"));
    }

    /// The strings_file tool scans the raw sample and reports its window.
    #[tokio::test]
    async fn strings_file_tool_scans_the_raw_sample() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_sample(&dir, &string_fixture());
        let mut context = context(&path);
        // the default scan finds both encodings with the window reported
        let found = strings_file(&mut context, None, None, None, None, None)
            .await
            .expect("the scan should succeed");
        assert_eq!(found.file_size, string_fixture().len() as u64);
        assert_eq!(found.scanned_from, "0x0");
        assert_eq!(found.strings.len(), 2);
        assert_eq!(found.strings[0].text, "corncob");
        // the wide pass reads corncob's final byte + NUL as a leading pair,
        // per the two-pass GNU strings convention
        assert_eq!(found.strings[1].text, "bwide");
        assert!(found.truncated.is_none());
        // hitting the limit mid-window notes how to continue the scan at the
        // first undelivered string's offset
        let capped = strings_file(&mut context, None, None, Some(1), None, None)
            .await
            .expect("the capped scan should succeed");
        assert_eq!(capped.strings.len(), 1);
        let note = capped.truncated.expect("the cap should be noted");
        assert!(note.contains("offset = 0xb"), "unexpected note: {note}");
    }

    /// The file_info tool identifies the raw sample end to end.
    #[tokio::test]
    async fn file_info_tool_identifies_the_raw_sample() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_sample(&dir, b"MZ\x90\x00CORNFIELD");
        let mut context = context(&path);
        let info = file_info(&mut context, None)
            .await
            .expect("the identity should compute");
        // the format comes from the MZ magic and the size from the bytes
        assert_eq!(info.size, 13);
        assert_eq!(info.format, "MZ executable (Windows PE or MS-DOS)");
        assert_eq!(info.md5.len(), 32);
        assert_eq!(info.sha1.len(), 40);
        assert_eq!(info.sha256.len(), 64);
    }

    /// A target reads an extracted file when the run carries a session store.
    #[tokio::test]
    async fn target_reads_an_extracted_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_sample(&dir, b"ORIGINAL");
        // build a context that also carries a session store with one extract
        let store = SessionStore::create(
            dir.path().join("sessions"),
            None,
            path.clone(),
            "sample".to_owned(),
            false,
            Vec::new(),
        )
        .await
        .expect("store should create");
        let extracted = store.extracted_path("child.bin").await.expect("path");
        tokio::fs::write(&extracted, b"CHILD BYTES")
            .await
            .expect("write");
        let ghidra = GhidraSession::new(GhidraSettings::from_binary(&path, "projects", None));
        let mut context = ToolContext::new();
        context.insert(store);
        context.insert(ghidra);
        // no target still reads the original sample
        let dump = read_file(&mut context, None, None, None)
            .await
            .expect("the original read should succeed");
        assert!(dump.contains("|ORIGINAL|"));
        // an empty target means the original sample too (the "" convention)
        let dump = read_file(&mut context, None, None, Some(String::new()))
            .await
            .expect("the empty target should read the original");
        assert!(dump.contains("|ORIGINAL|"));
        // the target routes every tool at the extracted child instead
        let dump = read_file(&mut context, None, None, Some("child.bin".to_owned()))
            .await
            .expect("the targeted read should succeed");
        assert!(dump.contains("|CHILD BYTES|"));
        let info = file_info(&mut context, Some("child.bin".to_owned()))
            .await
            .expect("the targeted identity should compute");
        assert_eq!(info.file_name, "child.bin");
        assert_eq!(info.size, 11);
        let found = strings_file(
            &mut context,
            None,
            None,
            None,
            None,
            Some("child.bin".to_owned()),
        )
        .await
        .expect("the targeted scan should succeed");
        assert_eq!(found.strings[0].text, "CHILD BYTES");
        // an unknown target errors with the known names hinted
        let error = file_info(&mut context, Some("ghost.bin".to_owned()))
            .await
            .expect_err("an unknown target should fail");
        assert!(error.to_string().contains("child.bin"));
    }

    /// A target without a session store errors cleanly instead of panicking.
    #[tokio::test]
    async fn target_without_a_store_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_sample(&dir, b"ORIGINAL");
        let mut context = context(&path);
        // the store-less context refuses targets with the persistence hint
        let error = read_file(&mut context, None, None, Some("child.bin".to_owned()))
            .await
            .expect_err("a store-less target should fail");
        assert!(error.to_string().contains("persistence"));
    }

    /// The newly added signatures identify documents and images.
    #[test]
    fn identify_format_matches_new_magics() {
        assert_eq!(identify_format(b"{\\rtf1\\ansi"), "RTF document");
        assert_eq!(identify_format(b"\x89PNG\r\n\x1a\ncorn"), "PNG image");
        assert_eq!(identify_format(b"\xff\xd8\xff\xe0corn"), "JPEG image");
        assert_eq!(identify_format(b"GIF87acorn"), "GIF image");
        assert_eq!(identify_format(b"GIF89acorn"), "GIF image");
    }

    /// The loadability map keeps native executables and rejects the rest.
    #[test]
    fn classify_format_maps_loadability() {
        // the native-executable magics Ghidra ships loaders for
        for head in [
            b"MZ\x90\x00".as_slice(),
            b"\x7fELF\x02\x01\x01",
            b"\xcf\xfa\xed\xfe",
            b"\xca\xfe\xba\xbe\x00\x00\x00\x02",
            b"\x00asm\x01\x00\x00\x00",
            b"dex\n035\x00",
        ] {
            let assessment = classify_format(head);
            assert_eq!(
                assessment.loadability,
                Loadability::Loadable,
                "{} should be loadable",
                assessment.label
            );
        }
        // every other recognized signature has no Ghidra loader
        for head in [
            b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1corn".as_slice(),
            b"PK\x03\x04corn",
            b"\x1f\x8bcorn",
            b"MSCFcorn",
            b"ITSFcorn",
            b"%PDF-1.7",
            b"Rar!\x1a\x07",
            b"7z\xbc\xaf\x27\x1c",
            b"{\\rtf1\\ansi",
            b"\x89PNG\r\n\x1a\ncorn",
            b"\xff\xd8\xff\xe0corn",
            b"GIF89acorn",
        ] {
            let assessment = classify_format(head);
            assert_eq!(
                assessment.loadability,
                Loadability::Unloadable,
                "{} should be unloadable",
                assessment.label
            );
        }
        // a shebang script quotes its interpreter line in the label
        let assessment = classify_format(b"#!/bin/sh\necho hi\n");
        assert_eq!(assessment.label, "script with a shebang line (#!/bin/sh)");
        assert_eq!(assessment.loadability, Loadability::Unloadable);
    }

    /// Text heads classify as text, with honest script-language hints.
    #[test]
    fn classify_format_detects_text_and_languages() {
        // a BOM-less VBScript dropper reads as likely VBScript
        let vbs = b"On Error Resume Next\r\nDim shell\r\nSet shell = \
                    CreateObject(\"WScript.Shell\")\r\nMsgBox \"hi\"\r\n";
        let assessment = classify_format(vbs);
        assert_eq!(assessment.label, "ASCII text (likely VBScript)");
        assert_eq!(assessment.loadability, Loadability::Unloadable);
        // a JavaScript loader reads as likely JavaScript
        let js = b"function run() { var x = eval(code); }\n\
                   var y = new ActiveXObject(\"WScript.Shell\");\n";
        assert_eq!(classify_format(js).label, "ASCII text (likely JavaScript)");
        // a batch file reads as likely batch
        let bat = b"@echo off\r\nsetlocal\r\ngoto :start\r\n";
        assert_eq!(
            classify_format(bat).label,
            "ASCII text (likely batch script)"
        );
        // an HTA/HTML page reads as likely HTML/HTA
        let hta = b"<!DOCTYPE html><html><head><script>go()</script>";
        assert_eq!(classify_format(hta).label, "ASCII text (likely HTML/HTA)");
        // BOM-less UTF-16LE PowerShell is sniffed and hinted
        let ps = "$env:TEMP; Invoke-WebRequest $u; New-Object Net.WebClient\n";
        let wide: Vec<u8> = ps.bytes().flat_map(|byte| [byte, 0]).collect();
        let assessment = classify_format(&wide);
        assert_eq!(
            assessment.label,
            "UTF-16LE text (no BOM, likely PowerShell)"
        );
        assert_eq!(assessment.loadability, Loadability::Unloadable);
        // a BOM-carrying script keeps the BOM fact alongside the hint
        let mut bom = b"\xef\xbb\xbf".to_vec();
        bom.extend_from_slice(vbs);
        assert_eq!(
            classify_format(&bom).label,
            "UTF-8 text (BOM, likely VBScript)"
        );
        // markers tied across languages stay honestly unlabeled
        let tie = b"function f() {}\nvar x = 1\nend function\nend sub\n";
        assert_eq!(classify_format(tie).label, "ASCII text");
    }

    /// Empty and all-zero heads classify as unloadable filler.
    #[test]
    fn classify_format_flags_zero_and_empty_heads() {
        // an empty file has nothing to load
        let assessment = classify_format(b"");
        assert_eq!(assessment.label, "empty file");
        assert_eq!(assessment.loadability, Loadability::Unloadable);
        // an all-zero head is filler data (the gm2010 39 MB zero-file case)
        let assessment = classify_format(&vec![0u8; 64]);
        assert!(
            assessment.label.starts_with("data ("),
            "{}",
            assessment.label
        );
        assert_eq!(assessment.loadability, Loadability::Unloadable);
    }

    /// Hex-record object text stays attempt-worthy despite being pure text.
    #[test]
    fn classify_format_keeps_hex_records_importable() {
        // an Intel HEX fragment: every line is ':' + hex record
        let ihex = b":10010000214601360121470136007EFE09D2190140\n\
                     :100110002146017E17C20001FF5F16002148011928\n\
                     :10012000194E79234623965778239EDA3F01B2CAA7\n\
                     :100130003F0156702B5E712B722B732146013421C7\n\
                     :00000001FF\n";
        let assessment = classify_format(ihex);
        assert_eq!(assessment.label, "Intel HEX object text");
        assert_eq!(assessment.loadability, Loadability::Unknown);
        // a Motorola S-record fragment: every line is 'S' + digit + hex
        let srec = b"S00F000068656C6C6F202020202000003C\n\
                     S11F00007C0802A6900100049421FFF07C6C1B787C8C23783C600000386300002F\n\
                     S11F001C4BFFFFE5398000007D83637880010014382100107C0803A64E800020E9\n\
                     S111003848656C6C6F20776F726C642E0A0042\n\
                     S5030003F9\n\
                     S9030000FC\n";
        let assessment = classify_format(srec);
        assert_eq!(assessment.label, "Motorola S-record object text");
        assert_eq!(assessment.loadability, Loadability::Unknown);
        // ordinary prose is unaffected by the record check
        let vbs = b"On Error Resume Next\r\nDim x\r\nSet x = CreateObject(\"a\")\r\n";
        assert_eq!(classify_format(vbs).loadability, Loadability::Unloadable);
    }

    /// Signature-less binary heads stay unknown so the importer decides.
    #[test]
    fn classify_format_leaves_binaries_unknown() {
        // a binary head with no recognized magic (an MBR-style boot image)
        let mut boot = vec![0x33u8, 0xc0, 0x8e, 0xd0, 0xbc, 0x00, 0x7c, 0x8e];
        boot.extend_from_slice(&[0x05, 0x09, 0x11, 0x80, 0x9c, 0x02, 0x13, 0xee]);
        let assessment = classify_format(&boot);
        assert_eq!(assessment.label, UNKNOWN_FORMAT);
        assert_eq!(assessment.loadability, Loadability::Unknown);
        // a short printable head cannot establish text, so it stays unknown
        assert_eq!(
            classify_format(b"corncob").loadability,
            Loadability::Unknown
        );
        // mostly-printable but invalid UTF-8 is not called text
        let mut dirty = vec![b'A'; 100];
        dirty[50] = 0xc0;
        assert_eq!(classify_format(&dirty).loadability, Loadability::Unknown);
    }

    /// file_info hints that unloadable formats cannot be imported.
    #[tokio::test]
    async fn file_info_tool_hints_unimportable_formats() {
        let dir = tempfile::tempdir().expect("tempdir");
        // a ZIP head carries the no-loader hint naming the raw tools
        let path = write_sample(&dir, b"PK\x03\x04corncob");
        let mut zip_context = context(&path);
        let info = file_info(&mut zip_context, None)
            .await
            .expect("the identity should compute");
        let hint = info.ghidra_import.expect("a ZIP should carry the hint");
        assert!(hint.contains("no Ghidra loader"), "unexpected hint: {hint}");
        assert!(hint.contains("read_text"), "unexpected hint: {hint}");
        // a native executable carries no hint at all
        let path = write_sample(&dir, b"MZ\x90\x00CORNFIELD");
        let mut mz_context = context(&path);
        let info = file_info(&mut mz_context, None)
            .await
            .expect("the identity should compute");
        assert!(info.ghidra_import.is_none());
    }

    /// read_text pages decoded lines with stable 1-based numbering.
    #[tokio::test]
    async fn read_text_tool_pages_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_sample(&dir, b"alpha\nbravo\ncharlie\ndelta\n");
        let mut context = context(&path);
        // the default read returns every line, numbered from one
        let window = read_text(&mut context, None, None, None, None, None, None)
            .await
            .expect("the read should succeed");
        assert_eq!(window.encoding, "utf-8");
        assert_eq!(window.lines.len(), 4);
        assert_eq!((window.from_line, window.to_line), (1, 4));
        assert_eq!(window.lines[0].number, 1);
        assert_eq!(window.lines[0].text, "alpha");
        assert_eq!(window.lines[3].text, "delta");
        assert!(window.truncated.is_none());
        // paging starts mid-file with the numbering preserved
        let window = read_text(&mut context, Some(3), None, None, None, None, None)
            .await
            .expect("the paged read should succeed");
        assert_eq!(window.lines[0].number, 3);
        assert_eq!(window.lines[0].text, "charlie");
        // a line cap stops early and names the resume line
        let window = read_text(&mut context, None, Some(2), None, None, None, None)
            .await
            .expect("the capped read should succeed");
        assert_eq!(window.lines.len(), 2);
        let note = window.truncated.expect("the cap should be noted");
        assert!(note.contains("line = 3"), "unexpected note: {note}");
        // a start past the window's end is an answer, not an error
        let window = read_text(&mut context, Some(100), None, None, None, None, None)
            .await
            .expect("the past-end read should answer");
        assert!(window.lines.is_empty());
        let note = window.truncated.expect("the miss should be noted");
        assert!(note.contains("past its end"), "unexpected note: {note}");
        // an offset past EOF answers like read_file does
        let window = read_text(&mut context, None, None, None, None, Some(1000), None)
            .await
            .expect("the past-EOF read should answer");
        assert!(window.lines.is_empty());
        let note = window.truncated.expect("the EOF should be noted");
        assert!(note.contains("past the end"), "unexpected note: {note}");
    }

    /// read_text sniffs BOMs, BOM-less UTF-16LE, and lossy fallbacks.
    #[tokio::test]
    async fn read_text_tool_detects_encodings() {
        let dir = tempfile::tempdir().expect("tempdir");
        // a UTF-16LE BOM file decodes with its lines intact
        let mut wide = b"\xff\xfe".to_vec();
        wide.extend("wide\nlines\n".bytes().flat_map(|byte| [byte, 0]));
        let path = write_sample(&dir, &wide);
        let mut wide_context = context(&path);
        let window = read_text(&mut wide_context, None, None, None, None, None, None)
            .await
            .expect("the wide read should succeed");
        assert_eq!(window.encoding, "utf-16le");
        assert_eq!(window.lines[0].text, "wide");
        assert_eq!(window.lines[1].text, "lines");
        // a BOM-less UTF-16LE script is sniffed by its byte pattern
        let bare: Vec<u8> = "Invoke-WebRequest $u\nNew-Object x\n"
            .bytes()
            .flat_map(|byte| [byte, 0])
            .collect();
        let path = write_sample(&dir, &bare);
        let mut bare_context = context(&path);
        let window = read_text(&mut bare_context, None, None, None, None, None, None)
            .await
            .expect("the sniffed read should succeed");
        assert_eq!(window.encoding, "utf-16le");
        assert_eq!(window.lines[0].text, "Invoke-WebRequest $u");
        // invalid UTF-8 decodes lossily instead of failing
        let path = write_sample(&dir, b"hello\n\xffworld\n");
        let mut lossy_context = context(&path);
        let window = read_text(&mut lossy_context, None, None, None, None, None, None)
            .await
            .expect("the lossy read should succeed");
        assert_eq!(window.encoding, "utf-8 (lossy)");
        assert_eq!(window.lines[0].text, "hello");
        assert!(window.lines[1].text.contains('\u{fffd}'));
        // a multi-byte sequence cut at the REAL end of file is lossy too: the
        // cut-tail tolerance only applies at a window boundary, never at EOF
        let path = write_sample(&dir, b"hello\n\xe2\x82");
        let mut cut_context = context(&path);
        let window = read_text(&mut cut_context, None, None, None, None, None, None)
            .await
            .expect("the cut read should succeed");
        assert_eq!(window.encoding, "utf-8 (lossy)");
        assert!(window.lines[1].text.contains('\u{fffd}'));
    }

    /// read_text filters by substring and enforces its character budgets.
    #[tokio::test]
    async fn read_text_tool_filters_and_caps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_sample(&dir, b"alpha\nBRAVO run\ncharlie\n");
        let mut text_context = context(&path);
        // the filter keeps matching lines with their true numbers
        let window = read_text(
            &mut text_context,
            None,
            None,
            None,
            Some("bravo".to_owned()),
            None,
            None,
        )
        .await
        .expect("the filtered read should succeed");
        assert_eq!(window.lines.len(), 1);
        assert_eq!(window.lines[0].number, 2);
        assert_eq!(window.lines[0].text, "BRAVO run");
        // a filter with no matches answers with a note, not an error
        let window = read_text(
            &mut text_context,
            None,
            None,
            None,
            Some("missing".to_owned()),
            None,
            None,
        )
        .await
        .expect("the matchless read should answer");
        assert!(window.lines.is_empty());
        let note = window.truncated.expect("the miss should be noted");
        assert!(note.contains("filter"), "unexpected note: {note}");
        // the character budget stops between lines and names the resume
        let window = read_text(&mut text_context, None, None, Some(7), None, None, None)
            .await
            .expect("the budgeted read should succeed");
        assert_eq!(window.lines.len(), 1);
        assert_eq!(window.lines[0].text, "alpha");
        let note = window.truncated.expect("the budget should be noted");
        assert!(note.contains("line = 2"), "unexpected note: {note}");
        // a pathological line keeps only its head, with the cut marked
        let path = write_sample(&dir, &vec![b'A'; 600]);
        let mut long_context = context(&path);
        let window = read_text(&mut long_context, None, None, None, None, None, None)
            .await
            .expect("the long-line read should succeed");
        assert_eq!(
            window.lines[0].text.chars().count(),
            MAX_TEXT_LINE_CHARS + 1
        );
        assert!(window.lines[0].text.ends_with('…'));
    }

    /// read_text follows the target convention to extracted files.
    #[tokio::test]
    async fn read_text_tool_targets_extracted_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_sample(&dir, b"ORIGINAL\n");
        // build a context that also carries a session store with one extract
        let store = SessionStore::create(
            dir.path().join("sessions"),
            None,
            path.clone(),
            "sample".to_owned(),
            false,
            Vec::new(),
        )
        .await
        .expect("store should create");
        let extracted = store.extracted_path("child.vbs").await.expect("path");
        tokio::fs::write(&extracted, b"Dim x\nMsgBox \"corn\"\n")
            .await
            .expect("write");
        let ghidra = GhidraSession::new(GhidraSettings::from_binary(&path, "projects", None));
        let mut context = ToolContext::new();
        context.insert(store);
        context.insert(ghidra);
        // the target routes the text read at the extracted child
        let window = read_text(
            &mut context,
            None,
            None,
            None,
            None,
            None,
            Some("child.vbs".to_owned()),
        )
        .await
        .expect("the targeted read should succeed");
        assert_eq!(window.lines[0].text, "Dim x");
        assert_eq!(window.lines[1].number, 2);
    }
}
