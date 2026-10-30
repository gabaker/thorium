//! Tools for decoding, transforming, searching, and hashing raw bytes.
//!
//! Malware hides its strings and payloads behind a small set of standardized
//! transforms -- base64 wrappers, RC4, single-byte XOR/ADD/SUB loops, pairwise
//! subtraction, self-keyed XOR blobs, API-name hashing -- and these tools
//! replay those transforms natively so no agent ever hand-writes one of the
//! standardized algorithms in Rhai. Scripts are for *binary-specific* glue
//! (walking a config layout, chaining per-sample constants), not for
//! reimplementing base64 or RC4.
//!
//! Every tool here is pure static computation: bytes come from an inline
//! `data` payload or from a file window (the ORIGINAL sample or an extracted
//! file via `target`, exactly like the file tools), get transformed in
//! memory, and are optionally saved into the session's extracted workspace
//! (via `name`) where every file tool can address the result.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use md5::Md5;
use rig::tool::{ToolContext, ToolExecutionError};
use serde::Serialize;
use sha1::Sha1;
// the trait is imported anonymously so rig's generated `Digest` tool struct
// (PascalCase of the `digest` tool fn) does not collide with it
use sha2::{Digest as _, Sha256};
use tracing::instrument;

use crate::thorpal::detect::decode::apihash::{self, MAX_HASH_MATCHES, NameVariants};
use crate::thorpal::detect::decode::{arithmetic, base64, pairwise, rc4, recover, xor};
use crate::thorpal::detect::fileinfo::hex;
use crate::thorpal::store::SessionStore;
use crate::thorpal::tools::containers::{self, ExtractedFile, MAX_TOTAL_EXTRACTED, resolve_target};
use crate::thorpal::tools::file::{self, FileStrings, SCAN_WINDOW};

/// The most input bytes one decode/transform call may consume (16 MiB)
pub(crate) const MAX_DECODE_INPUT: usize = 16 * 1024 * 1024;

/// The most steps one decode pipeline may chain
pub(crate) const MAX_PIPELINE_STEPS: usize = 8;

/// The most input bytes a brute-force sweep scores per key (1 MiB)
pub(crate) const MAX_BRUTE_INPUT: usize = 1024 * 1024;

/// The default number of brute-force candidates returned
pub(crate) const DEFAULT_BRUTE_TOP: usize = 5;

/// The most brute-force candidates one call may return
pub(crate) const MAX_BRUTE_TOP: usize = 16;

/// The most keywords one grep call may search for
pub(crate) const MAX_KEYWORDS: usize = 32;

/// The most bytes one grep keyword may hold
pub(crate) const MAX_KEYWORD_LEN: usize = 128;

/// The default context bytes hexdumped around each grep match
pub(crate) const DEFAULT_CONTEXT: usize = 16;

/// The most context bytes hexdumped around each grep match
pub(crate) const MAX_CONTEXT: usize = 64;

/// The default number of grep matches returned
pub(crate) const DEFAULT_MATCHES: usize = 100;

/// The most grep matches one call may return
pub(crate) const MAX_MATCHES: usize = 500;

/// The most ranges one digest call may hash
pub(crate) const MAX_DIGEST_RANGES: usize = 64;

/// The most hash targets one api_hash_match call may resolve
pub(crate) const MAX_HASH_TARGETS: usize = 256;

/// The most characters of decoded text shown in a result preview
pub(crate) const TEXT_PREVIEW_CHARS: usize = 2048;

/// The most decoded bytes hexdumped in a result preview
pub(crate) const HEXDUMP_PREVIEW: usize = 256;

/// The most characters kept per brute-force candidate run
const BRUTE_RUN_CHARS: usize = 128;

/// The most runs kept per brute-force candidate
const BRUTE_RUNS: usize = 5;

/// The most decoded bytes a brute-force candidate preview renders
const BRUTE_PREVIEW_BYTES: usize = 64;

/// The most hidden runs one pairwise_sub call reports
const MAX_HIDDEN_RUNS: usize = 100;

/// The most characters kept per pairwise hidden run before truncation
const HIDDEN_RUN_CHARS: usize = 512;

/// The chunk size the digest pass streams ranges through (1 MiB)
const DIGEST_CHUNK: usize = 1024 * 1024;

// ===========================================================================
// Shared input resolution and result shaping
// ===========================================================================

/// Decode an inline `data` payload into bytes per its declared encoding
///
/// # Arguments
///
/// * `data` - The inline payload text
/// * `encoding` - The payload encoding: `hex` (default), `base64`, or `text`
pub(crate) fn decode_inline(data: &str, encoding: &str) -> Result<Vec<u8>, String> {
    // dispatch on the normalized encoding name
    match encoding.to_ascii_lowercase().as_str() {
        "hex" => parse_hex(data),
        "base64" => {
            base64::decode(data).ok_or_else(|| "the inline `data` is not valid base64".to_owned())
        }
        "text" => Ok(data.as_bytes().to_vec()),
        other => Err(format!(
            "unknown `encoding` '{other}'; supported: hex (default), base64, text"
        )),
    }
}

/// Parse a hex string into bytes, tolerating whitespace and a `0x` prefix
///
/// # Arguments
///
/// * `text` - The hex text to parse
pub(crate) fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    // strip whitespace and one leading 0x so pasted dumps parse
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let cleaned = cleaned
        .strip_prefix("0x")
        .or_else(|| cleaned.strip_prefix("0X"))
        .unwrap_or(&cleaned);
    // each byte takes two hex characters
    if !cleaned.len().is_multiple_of(2) {
        return Err("hex data has an odd number of characters".to_owned());
    }
    // decode each two-character chunk into a single byte
    let mut bytes = Vec::with_capacity(cleaned.len() / 2);
    for i in (0..cleaned.len()).step_by(2) {
        let byte = u8::from_str_radix(&cleaned[i..i + 2], 16)
            .map_err(|_| format!("invalid hex at position {i}"))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// Read one input window off the disk without blocking the runtime
///
/// # Arguments
///
/// * `path` - The file to read from
/// * `offset` - The file offset to start reading at
/// * `length` - The most bytes to read
#[instrument(skip_all, err(Debug))]
async fn read_input(path: PathBuf, offset: u64, length: usize) -> Result<(Vec<u8>, u64), String> {
    // file io is blocking, so hop onto a blocking thread for the read
    tokio::task::spawn_blocking(move || file::read_window_sync(&path, offset, length))
        .await
        .map_err(|err| format!("the input read task failed: {err}"))?
}

/// Resolve a decode tool's input bytes: inline `data` or a file window
///
/// Inline `data` (with its `encoding`) and a file window (`target`/`offset`/
/// `length`) are mutually exclusive; passing neither reads the ORIGINAL
/// sample from offset 0, like the file tools. The result is capped at
/// [`MAX_DECODE_INPUT`].
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `data` - The inline payload, when the bytes are supplied directly
/// * `encoding` - The inline payload's encoding (hex/base64/text)
/// * `target` - The extracted file to read, or `None`/`""` for the original sample
/// * `offset` - The file offset the window starts at
/// * `length` - The window's length (default/cap [`MAX_DECODE_INPUT`])
async fn resolve_input(
    context: &ToolContext,
    data: Option<String>,
    encoding: Option<String>,
    target: Option<String>,
    offset: Option<u64>,
    length: Option<usize>,
) -> Result<Vec<u8>, ToolExecutionError> {
    match data {
        Some(data) => {
            // inline data and a file window cannot both be the input; any
            // window knob alongside `data` would be silently ignored otherwise
            if target.as_deref().is_some_and(|name| !name.is_empty())
                || offset.is_some()
                || length.is_some()
            {
                return Err(ToolExecutionError::invalid_args(
                    "pass either inline `data` or a file window (`target`/`offset`/\
                     `length`), not both"
                        .to_owned(),
                ));
            }
            // decode the inline payload per its encoding
            let bytes = decode_inline(&data, encoding.as_deref().unwrap_or("hex"))
                .map_err(ToolExecutionError::invalid_args)?;
            // the cap applies to inline inputs too
            if bytes.len() > MAX_DECODE_INPUT {
                return Err(ToolExecutionError::invalid_args(format!(
                    "the inline data is {} bytes; decode tools accept at most \
                     {MAX_DECODE_INPUT} bytes per call",
                    bytes.len()
                )));
            }
            Ok(bytes)
        }
        None => {
            // resolve the original sample, or an extracted file when targeted
            let path = resolve_target(context, target.as_deref()).await?;
            let offset = offset.unwrap_or(0);
            let length = length.unwrap_or(MAX_DECODE_INPUT).min(MAX_DECODE_INPUT);
            // read the window off a blocking thread
            let (bytes, file_size) = read_input(path, offset, length)
                .await
                .map_err(ToolExecutionError::invalid_args)?;
            // an empty window has nothing to decode
            if bytes.is_empty() {
                return Err(ToolExecutionError::invalid_args(format!(
                    "offset {offset:#x} is at or past the end of the file \
                     ({file_size} bytes); nothing to read"
                )));
            }
            Ok(bytes)
        }
    }
}

/// Pulls the session store out of a tool context with a targeted error
///
/// Saving decode output needs the extracted workspace, so a store-less run
/// (persistence disabled) gets an error explaining exactly that.
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
fn store_from_context(
    context: &ToolContext,
) -> Result<std::sync::Arc<SessionStore>, ToolExecutionError> {
    // map an absent store onto the workspace-specific explanation
    SessionStore::try_from_context(context).ok_or_else(|| {
        ToolExecutionError::invalid_args(
            "saving output needs a session store, but this run has session \
             persistence disabled; drop `name` and work with the previews instead"
                .to_owned(),
        )
    })
}

/// Save one decode output into the session's extracted workspace
///
/// Follows the decompress tool's path exactly: the workspace lock serializes
/// writers, the output streams through the budget-capped atomic writer, and
/// the finished file is hashed and identified so the result self-describes.
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `name` - The output's workspace name
/// * `output` - The full decode output to save
#[instrument(skip_all, err(Debug))]
async fn save_output(
    context: &ToolContext,
    name: String,
    output: Vec<u8>,
) -> Result<ExtractedFile, ToolExecutionError> {
    // the output lands in the extracted workspace, so a store is required
    let store = store_from_context(context)?;
    // serialize workspace writes so concurrent saves can neither race the
    // same `.tmp` sibling nor leapfrog the total-budget check
    let _guard = store.lock_extracted().await;
    // resolve and validate the output's workspace name up front
    let dest = store
        .extracted_path(&name)
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    // budget the output against the workspace's remaining space
    let used = store
        .extracted_usage()
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    let cap = MAX_TOTAL_EXTRACTED.saturating_sub(used);
    // write and identify off a blocking thread, still under the lock
    tokio::task::spawn_blocking(move || {
        containers::write_capped(
            output.as_slice(),
            &dest,
            cap,
            &format!(
                "saving stopped: it would exceed the session's remaining extracted \
                 workspace budget of {cap} bytes; nothing was written"
            ),
        )?;
        containers::finish_extracted(name, &dest)
    })
    .await
    .map_err(|err| ToolExecutionError::invalid_args(format!("the save task failed: {err}")))?
    .map_err(ToolExecutionError::invalid_args)
}

/// A finished decode/transform: the output's shape plus bounded previews
#[derive(Debug, Clone, Serialize)]
pub struct DecodeResult {
    /// The steps that ran, in order, as human-readable labels
    pub steps: Vec<String>,
    /// The input length in bytes
    pub input_len: usize,
    /// The output length in bytes
    pub output_len: usize,
    /// The fraction of output bytes that are printable (0.0-1.0)
    pub printable_ratio: f32,
    /// A bounded text preview of the output (per the `emit` mode)
    pub text: String,
    /// A bounded hexdump preview of the output's head
    pub hexdump: String,
    /// The output saved into the extracted workspace, when `name` was given
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved: Option<ExtractedFile>,
    /// A note describing what the previews cut and how to get the rest
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// The fraction of bytes that are printable (strings(1) range plus tab)
///
/// # Arguments
///
/// * `bytes` - The bytes to measure
fn printable_ratio(bytes: &[u8]) -> f32 {
    // an empty buffer has no printable share to speak of
    if bytes.is_empty() {
        return 0.0;
    }
    let printable = bytes.iter().filter(|b| recover::is_printable(**b)).count();
    printable as f32 / bytes.len() as f32
}

/// Render bytes as a bounded text preview, dotting out control bytes
///
/// Newlines and tabs survive (decoded configs are often line-oriented);
/// every other non-printable byte renders as `.` like a hexdump gutter.
///
/// # Arguments
///
/// * `bytes` - The bytes to render
/// * `max_chars` - The most characters to keep
fn text_preview(bytes: &[u8], max_chars: usize) -> String {
    // map each byte to a displayable char, bounded up front
    bytes
        .iter()
        .take(max_chars)
        .map(|byte| match byte {
            b'\n' => '\n',
            b'\t' => '\t',
            byte if recover::is_printable(*byte) => *byte as char,
            _ => '.',
        })
        .collect()
}

/// Build a [`DecodeResult`] (sans `saved`) from one finished transform
///
/// # Arguments
///
/// * `steps` - The human-readable step labels that ran
/// * `input_len` - The input length in bytes
/// * `output` - The full output bytes
/// * `emit` - The preview mode: `strings`, `bytes`, or `hexdump`
fn build_result(steps: Vec<String>, input_len: usize, output: &[u8], emit: &str) -> DecodeResult {
    // render the text preview per the requested emit mode
    let text = match emit {
        // the raw-bytes mode shows the head as plain hex
        "bytes" => hex(&output[..output.len().min(TEXT_PREVIEW_CHARS / 2)]),
        // the hexdump mode leans on the hexdump field alone
        "hexdump" => String::new(),
        // the default strings mode shows printable text with dots
        _ => text_preview(output, TEXT_PREVIEW_CHARS),
    };
    // note what the previews cut so the model knows how to get the rest
    let cut = output.len() > TEXT_PREVIEW_CHARS || output.len() > HEXDUMP_PREVIEW;
    let truncated = cut.then(|| {
        format!(
            "the output is {} bytes; the previews show the head -- pass `name` to \
             save the full output into the extracted workspace where every file \
             tool can read it",
            output.len()
        )
    });
    DecodeResult {
        steps,
        input_len,
        output_len: output.len(),
        printable_ratio: printable_ratio(output),
        text,
        hexdump: file::hexdump(&output[..output.len().min(HEXDUMP_PREVIEW)], 0),
        saved: None,
        truncated,
    }
}

/// Parse and validate an `emit` preview mode
///
/// # Arguments
///
/// * `emit` - The requested mode, `None` for the default
fn parse_emit(emit: Option<&str>) -> Result<&'static str, String> {
    // dispatch on the normalized mode name, defaulting to strings
    match emit.map(str::to_ascii_lowercase).as_deref() {
        None | Some("strings") => Ok("strings"),
        Some("bytes") => Ok("bytes"),
        Some("hexdump") => Ok("hexdump"),
        Some(other) => Err(format!(
            "unknown `emit` mode '{other}'; supported: strings (default), bytes, hexdump"
        )),
    }
}

// ===========================================================================
// decode_bytes: the standardized-step pipeline
// ===========================================================================

/// One standardized step of a decode pipeline
#[derive(Debug)]
pub(crate) enum DecodeStep {
    /// Base64-decode (standard/URL-safe, or a custom/rotated alphabet)
    Base64 {
        /// A custom 64-symbol alphabet, when the sample carries one
        alphabet: Option<base64::Alphabet>,
        /// Rotate the alphabet left this many positions before decoding
        rotate: Option<usize>,
    },
    /// RC4-decrypt under a key
    Rc4 {
        /// The RC4 key bytes
        key: Vec<u8>,
    },
    /// XOR with a repeating key (single-byte keys are a one-byte key)
    Xor {
        /// The key bytes repeated across the data
        key: Vec<u8>,
    },
    /// Add a single byte to every input byte (wrapping)
    Add {
        /// The single-byte addend
        k: u8,
    },
    /// Subtract a single byte from every input byte (wrapping)
    Sub {
        /// The single-byte subtrahend
        k: u8,
    },
    /// Subtract every input byte from a single byte (`(K - b) & 0xff`)
    SubFromKey {
        /// The single-byte minuend
        k: u8,
    },
    /// XOR a self-keyed buffer whose first bytes are its own key
    SelfKeyXor {
        /// The number of leading bytes that are the key
        key_len: usize,
    },
}

impl DecodeStep {
    /// The human-readable label this step reports under
    fn label(&self) -> String {
        match self {
            DecodeStep::Base64 { alphabet, rotate } => match (alphabet, rotate) {
                (None, None) => "base64".to_owned(),
                (Some(_), None) => "base64(custom alphabet)".to_owned(),
                (None, Some(rotate)) => format!("base64(alphabet rotated {rotate})"),
                (Some(_), Some(rotate)) => {
                    format!("base64(custom alphabet rotated {rotate})")
                }
            },
            DecodeStep::Rc4 { key } => format!("rc4(key {} bytes)", key.len()),
            DecodeStep::Xor { key } => format!("xor(key {} bytes)", key.len()),
            DecodeStep::Add { k } => format!("add({k:#04x})"),
            DecodeStep::Sub { k } => format!("sub({k:#04x})"),
            DecodeStep::SubFromKey { k } => format!("sub_from_key({k:#04x})"),
            DecodeStep::SelfKeyXor { key_len } => format!("self_key_xor(key_len {key_len})"),
        }
    }

    /// Apply this step to one buffer
    ///
    /// # Arguments
    ///
    /// * `input` - The bytes the previous step produced
    fn apply(&self, input: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            DecodeStep::Base64 { alphabet, rotate } => {
                // resolve the alphabet: custom, rotated, or the shared decoder
                let resolved = match (alphabet, rotate) {
                    (None, None) => None,
                    (Some(alphabet), None) => Some(alphabet.clone()),
                    (Some(alphabet), Some(rotate)) => Some(alphabet.rotated(*rotate)),
                    (None, Some(rotate)) => Some(base64::Alphabet::standard().rotated(*rotate)),
                };
                match resolved {
                    // the shared decoder accepts standard and URL-safe at once
                    None => base64::decode(&String::from_utf8_lossy(input)).ok_or_else(|| {
                        "the input is not valid base64 (standard or URL-safe); a \
                         custom alphabet can be passed via the step's `alphabet`"
                            .to_owned()
                    }),
                    Some(alphabet) => base64::decode_with(&alphabet, input).ok_or_else(|| {
                        "the input does not decode under the given base64 alphabet".to_owned()
                    }),
                }
            }
            DecodeStep::Rc4 { key } => Ok(rc4::decrypt(key, input)),
            DecodeStep::Xor { key } => Ok(xor::keystream(key, input)),
            DecodeStep::Add { k } => Ok(arithmetic::add_byte(*k, input)),
            DecodeStep::Sub { k } => Ok(arithmetic::sub_byte(*k, input)),
            DecodeStep::SubFromKey { k } => Ok(arithmetic::sub_from_key(*k, input)),
            DecodeStep::SelfKeyXor { key_len } => {
                // an input no longer than its key carries no ciphertext
                if input.len() <= *key_len {
                    return Err(format!(
                        "self_key_xor needs more than key_len ({key_len}) bytes of \
                         input; got {}",
                        input.len()
                    ));
                }
                Ok(xor::self_keyed(*key_len, input))
            }
        }
    }
}

/// Read one step's key bytes from its JSON object
///
/// Accepts `key_hex` (a hex string) or `key` (a string taken as raw bytes),
/// but never both.
///
/// # Arguments
///
/// * `step` - The step's JSON object
fn step_key(step: &serde_json::Value) -> Result<Option<Vec<u8>>, String> {
    // the two spellings are mutually exclusive to keep intent unambiguous
    let key_hex = step.get("key_hex");
    let key = step.get("key");
    match (key_hex, key) {
        (Some(_), Some(_)) => Err("pass `key_hex` or `key`, not both".to_owned()),
        (Some(value), None) => {
            let text = value
                .as_str()
                .ok_or_else(|| "`key_hex` must be a hex string".to_owned())?;
            Ok(Some(parse_hex(text)?))
        }
        (None, Some(value)) => {
            let text = value
                .as_str()
                .ok_or_else(|| "`key` must be a string (its bytes are the key)".to_owned())?;
            Ok(Some(text.as_bytes().to_vec()))
        }
        (None, None) => Ok(None),
    }
}

/// Read one step's single-byte `k` from its JSON object
///
/// # Arguments
///
/// * `step` - The step's JSON object
/// * `op` - The op name, for error messages
fn step_k(step: &serde_json::Value, op: &str) -> Result<u8, String> {
    // the byte may be spelled `k` (an int) or `key` (an int)
    let value = step
        .get("k")
        .or_else(|| step.get("key"))
        .ok_or_else(|| format!("op '{op}' needs `k` (an integer 0-255)"))?;
    value
        .as_u64()
        .and_then(|k| u8::try_from(k).ok())
        .ok_or_else(|| format!("op '{op}' needs `k` to be an integer 0-255"))
}

/// Parse a pipeline's raw JSON steps into typed [`DecodeStep`]s
///
/// # Arguments
///
/// * `raw` - The raw step objects, in pipeline order
pub(crate) fn parse_steps(raw: &[serde_json::Value]) -> Result<Vec<DecodeStep>, String> {
    // an empty pipeline would be a no-op worth flagging
    if raw.is_empty() {
        return Err("`steps` must hold at least one step".to_owned());
    }
    // bound the chain so a degenerate pipeline cannot spin
    if raw.len() > MAX_PIPELINE_STEPS {
        return Err(format!(
            "`steps` holds {} steps; at most {MAX_PIPELINE_STEPS} may be chained",
            raw.len()
        ));
    }
    let mut steps = Vec::with_capacity(raw.len());
    for (index, step) in raw.iter().enumerate() {
        // every step is an object naming its op
        let op = step
            .get("op")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("step {index} must be an object with an `op` string"))?;
        // dispatch on the normalized op name
        let parsed = match op.to_ascii_lowercase().as_str() {
            "base64" => {
                // an optional custom alphabet, 64 distinct symbols
                let alphabet = match step.get("alphabet") {
                    None => None,
                    Some(value) => {
                        let text = value
                            .as_str()
                            .ok_or_else(|| "`alphabet` must be a 64-character string".to_owned())?;
                        if text.len() != base64::ALPHABET_LEN || !text.is_ascii() {
                            return Err("`alphabet` must be exactly 64 ASCII characters".to_owned());
                        }
                        // the decoder skips '=' (padding) and whitespace before
                        // the symbol lookup, so a table containing either would
                        // silently mis-decode -- refuse it instead
                        if text.contains('=') || text.contains(|c: char| c.is_ascii_whitespace()) {
                            return Err("`alphabet` must not contain '=' or whitespace: the \
                                 decoder treats those as padding/formatting, so such \
                                 a table would silently mis-decode"
                                .to_owned());
                        }
                        let mut table = [0u8; base64::ALPHABET_LEN];
                        table.copy_from_slice(text.as_bytes());
                        Some(
                            base64::Alphabet::new(table, Some(b'='), "custom").ok_or_else(
                                || {
                                    "`alphabet` repeats a symbol; the 64 must be distinct"
                                        .to_owned()
                                },
                            )?,
                        )
                    }
                };
                // an optional rotation, 1..=63 positions
                let rotate = match step.get("rotate") {
                    None => None,
                    Some(value) => {
                        let rotate = value.as_u64().and_then(|r| usize::try_from(r).ok());
                        match rotate {
                            Some(rotate) if (1..base64::ALPHABET_LEN).contains(&rotate) => {
                                Some(rotate)
                            }
                            _ => {
                                return Err("`rotate` must be an integer 1-63".to_owned());
                            }
                        }
                    }
                };
                DecodeStep::Base64 { alphabet, rotate }
            }
            "rc4" => {
                let key = step_key(step)?
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| "op 'rc4' needs a non-empty `key_hex` or `key`".to_owned())?;
                DecodeStep::Rc4 { key }
            }
            "xor" => {
                let key = step_key(step)?
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| "op 'xor' needs a non-empty `key_hex` or `key`".to_owned())?;
                DecodeStep::Xor { key }
            }
            "add" => DecodeStep::Add {
                k: step_k(step, "add")?,
            },
            "sub" => DecodeStep::Sub {
                k: step_k(step, "sub")?,
            },
            "sub_from_key" => DecodeStep::SubFromKey {
                k: step_k(step, "sub_from_key")?,
            },
            "self_key_xor" => {
                let key_len = step
                    .get("key_len")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|len| usize::try_from(len).ok())
                    .filter(|len| *len > 0)
                    .ok_or_else(|| {
                        "op 'self_key_xor' needs `key_len` (a positive integer; \
                         LummaC2 uses 32)"
                            .to_owned()
                    })?;
                DecodeStep::SelfKeyXor { key_len }
            }
            other => {
                return Err(format!(
                    "unknown op '{other}' in step {index}; supported ops: base64, rc4, \
                     xor, add, sub, sub_from_key, self_key_xor"
                ));
            }
        };
        steps.push(parsed);
    }
    Ok(steps)
}

/// Run a parsed pipeline over one input buffer
///
/// Returns the final output plus the labels of the steps that ran, so the
/// result records exactly what was replayed.
///
/// # Arguments
///
/// * `input` - The bytes to decode
/// * `steps` - The parsed pipeline steps, in order
pub(crate) fn run_pipeline(
    input: &[u8],
    steps: &[DecodeStep],
) -> Result<(Vec<u8>, Vec<String>), String> {
    // thread the buffer through each step, failing with the step named
    let mut buffer = input.to_vec();
    let mut labels = Vec::with_capacity(steps.len());
    for (index, step) in steps.iter().enumerate() {
        let label = step.label();
        buffer = step
            .apply(&buffer)
            .map_err(|err| format!("step {index} ({label}) failed: {err}"))?;
        labels.push(label);
    }
    Ok((buffer, labels))
}

/// Decodes bytes through a pipeline of standardized steps
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `steps` - The pipeline steps, each `{op, ...}` (see the description)
/// * `target` - The file to read, or `None`/`""` for the original sample
/// * `offset` - The file offset the input window starts at
/// * `length` - The input window's length (default/cap 16 MiB)
/// * `data` - Inline input bytes instead of a file window
/// * `encoding` - The inline data's encoding: hex (default), base64, text
/// * `name` - Save the full output under this workspace name
#[allow(clippy::too_many_arguments)]
#[thorium_derive::thorpal_tool(
    group = "decode",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_decode_bytes",
    rhai_doc = "decode_bytes(data: blob, steps: array) -> blob -- run a standardized decode pipeline | save_bytes(name: string, data: blob) -> map -- save a script-built payload into the extracted workspace",
    rhai_reference = "Runs a pipeline of standardized decode steps over a blob in memory and
returns the decoded blob.

`steps` is an array of maps, each with an `op` plus its parameters:
`#{ op: \"base64\" }` (optional `alphabet`: a 64-char string, `rotate`: 1-63),
`#{ op: \"rc4\", key: \"...\" }` or `key_hex`, `#{ op: \"xor\", key_hex:
\"5a\" }` (any key length), `#{ op: \"add\", k: 4 }`, `#{ op: \"sub\", k: 4
}`, `#{ op: \"sub_from_key\", k: 0x21 }` ((K - b) & 0xff), and `#{ op:
\"self_key_xor\", key_len: 32 }` (the first key_len bytes are the key for the
rest -- the LummaC2 shape). At most 8 steps chain.

NEVER hand-write base64, RC4, or these byte loops in Rhai -- this native
binding replays them exactly; scripts are for binary-specific glue around it.

`save_bytes(name, data)` saves a payload the script built in memory into the
extracted workspace (256 MiB/entry, 1 GiB/session) and returns the saved
file's identity map. The saved name works as every file tool's `target` and
as import_child's target -- so a script-decoded PE/ELF/Mach-O payload gets
carved and imported for full analysis instead of dying in script output."
)]
#[rig::tool_macro(
    description = "Decode bytes through a pipeline of standardized steps -- never hand-write these standardized algorithms in Rhai, use this native tool. Each step is {op, ...}: {op:\"base64\"} (optional alphabet: 64-char custom table, rotate: 1-63), {op:\"rc4\", key:\"CgeOvEO7hu\"} (or key_hex), {op:\"xor\", key_hex:\"5a13\"} (repeating key of any length), {op:\"add\"|\"sub\"|\"sub_from_key\", k: 4} (single-byte wrapping loops; sub_from_key is (K - b) & 0xff, the Lumma loader shape), {op:\"self_key_xor\", key_len: 32} (buffer carries its own key up front -- the LummaC2 shape). Up to 8 steps chain, e.g. [{op:\"base64\"},{op:\"rc4\",key:...}] for Stealc strings. Input comes inline via `data` (+`encoding`: hex default/base64/text) or from a file window (`target`/`offset`/`length`, 16 MiB cap). Returns bounded text and hexdump previews; pass `name` to save the full output into the extracted workspace. A saved output that identifies as PE/ELF/Mach-O can be imported with import_child and analyzed like any program -- the standard carve: decode -> save with `name` -> import_child."
)]
pub async fn decode_bytes(
    #[rig(context)] context: &mut ToolContext,
    steps: Vec<serde_json::Value>,
    target: Option<String>,
    offset: Option<u64>,
    length: Option<usize>,
    data: Option<String>,
    encoding: Option<String>,
    name: Option<String>,
) -> Result<DecodeResult, ToolExecutionError> {
    // parse the pipeline before touching any input
    let parsed = parse_steps(&steps).map_err(ToolExecutionError::invalid_args)?;
    // resolve the input bytes: inline data or a file window
    let input = resolve_input(context, data, encoding, target, offset, length).await?;
    // run the pipeline in memory (bounded by the input cap, steps never grow)
    let (output, labels) =
        run_pipeline(&input, &parsed).map_err(ToolExecutionError::invalid_args)?;
    // build the bounded result, then save the full output when asked
    let mut result = build_result(labels, input.len(), &output, "strings");
    if let Some(name) = name {
        result.saved = Some(save_output(context, name, output).await?);
    }
    Ok(result)
}

// ===========================================================================
// byte_transform: single-op transforms and key brute-forcing
// ===========================================================================

/// Rotate every byte left by `bits` (an 8-bit `rol`)
///
/// # Arguments
///
/// * `bits` - The rotation amount (masked to 0-7)
/// * `data` - The bytes to rotate
fn rol_byte(bits: u32, data: &[u8]) -> Vec<u8> {
    // rotate within the byte, like the 8-bit register op
    data.iter().map(|b| b.rotate_left(bits & 7)).collect()
}

/// Apply one byte transform under a key
///
/// Ops: `xor` (any key length), `add`/`sub`/`sub_from_key` (one key byte),
/// and `rol` (one key byte holding the bit count 1-7; decode a `ror` loop by
/// rotating left the same amount).
///
/// # Arguments
///
/// * `op` - The transform op name
/// * `key` - The key bytes
/// * `data` - The bytes to transform
pub(crate) fn transform(op: &str, key: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    // the single-byte ops share one key check
    let single = |op: &str| -> Result<u8, String> {
        match key {
            [k] => Ok(*k),
            _ => Err(format!(
                "op '{op}' takes exactly one key byte; got {} bytes",
                key.len()
            )),
        }
    };
    // dispatch on the normalized op name
    match op.to_ascii_lowercase().as_str() {
        "xor" => {
            if key.is_empty() {
                return Err("op 'xor' needs a non-empty key".to_owned());
            }
            Ok(xor::keystream(key, data))
        }
        "add" => Ok(arithmetic::add_byte(single("add")?, data)),
        "sub" => Ok(arithmetic::sub_byte(single("sub")?, data)),
        "sub_from_key" => Ok(arithmetic::sub_from_key(single("sub_from_key")?, data)),
        "rol" => {
            let bits = single("rol")?;
            // zero and 8+ rotations are the identity; demand a real amount
            if !(1..=7).contains(&bits) {
                return Err("op 'rol' needs a key byte of 1-7 (the bit count)".to_owned());
            }
            Ok(rol_byte(u32::from(bits), data))
        }
        other => Err(format!(
            "unknown transform op '{other}'; supported: xor, add, sub, sub_from_key, rol"
        )),
    }
}

/// One scored candidate from a brute-force key sweep
#[derive(Debug, Clone, Serialize)]
pub struct BruteCandidate {
    /// The key byte (or `rol` bit count) this candidate decoded under
    pub key: u8,
    /// The total characters across the quality runs the decode made readable
    pub score: usize,
    /// The count of lowercase letters and spaces in the decode (the tiebreak:
    /// an off-by-one key keeps bytes printable but loses exactly these)
    pub text_score: usize,
    /// The first readable runs the decode produced, clipped
    pub runs: Vec<String>,
    /// A bounded preview of the decoded head (per the `emit` mode)
    pub preview: String,
}

/// A finished brute-force sweep over one op's key space
#[derive(Debug, Clone, Serialize)]
pub struct BruteResult {
    /// The op the sweep ran
    pub op: String,
    /// The number of keys tried
    pub tried: usize,
    /// The best candidates, strongest first
    pub candidates: Vec<BruteCandidate>,
    /// A note describing any input clamping
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// Score one decoded buffer by the readable text it produced
///
/// The score is the total characters across the quality runs the decode made
/// readable ([`recover::printable_claim_runs`]: long, wordy, reads-as-text).
/// The text score breaks ties between neighbouring keys: an off-by-one
/// additive key keeps every byte printable (a shifted space is `!`, a shifted
/// letter is usually still a letter), so the printable total alone cannot
/// separate it from the true key -- but it loses exactly the spaces and
/// lowercase letters real text is made of, so those are counted directly.
///
/// # Arguments
///
/// * `decoded` - The decoded bytes to score
fn score_decode(decoded: &[u8]) -> (usize, usize, Vec<String>) {
    // the claims are every quality run, tables included
    let claims = recover::printable_claim_runs(decoded);
    let score = claims.iter().map(|(_, run)| run.len()).sum();
    // the tiebreak: how much of the decode is lowercase letters and spaces
    let text_score = decoded
        .iter()
        .filter(|byte| **byte == b' ' || byte.is_ascii_lowercase())
        .count();
    // the reportable runs drop the decoded tables; they feed the candidate
    let kept = claims
        .iter()
        .filter(|(_, run)| !recover::is_table_run(run.as_bytes()))
        .map(|(_, run)| run)
        .take(BRUTE_RUNS)
        .map(|run| {
            // clip on a char boundary; runs are ascii so bytes == chars
            let mut clipped = run.clone();
            clipped.truncate(BRUTE_RUN_CHARS);
            clipped
        })
        .collect();
    (score, text_score, kept)
}

/// Render one brute candidate's preview per the emit mode
///
/// # Arguments
///
/// * `decoded` - The decoded bytes
/// * `emit` - The preview mode: `strings`, `bytes`, or `hexdump`
fn brute_preview(decoded: &[u8], emit: &str) -> String {
    let head = &decoded[..decoded.len().min(BRUTE_PREVIEW_BYTES)];
    match emit {
        "bytes" => hex(head),
        "hexdump" => file::hexdump(head, 0),
        _ => text_preview(head, BRUTE_PREVIEW_BYTES),
    }
}

/// Brute-force one op's key space over a buffer, scoring each key
///
/// Keys 0-255 for `xor`/`add`/`sub`/`sub_from_key`, bit counts 1-7 for
/// `rol`. The input is clamped to [`MAX_BRUTE_INPUT`] with a note.
///
/// # Arguments
///
/// * `op` - The transform op to sweep
/// * `data` - The bytes to decode under each key
/// * `top` - The number of candidates to return
/// * `emit` - The preview mode for each candidate
pub(crate) fn brute(op: &str, data: &[u8], top: usize, emit: &str) -> Result<BruteResult, String> {
    // resolve the key space for this op (also validates the op name)
    let keys: Vec<u8> = match op.to_ascii_lowercase().as_str() {
        "xor" | "add" | "sub" | "sub_from_key" => (0..=255).collect(),
        "rol" => (1..=7).collect(),
        other => {
            return Err(format!(
                "unknown transform op '{other}'; supported: xor, add, sub, \
                 sub_from_key, rol"
            ));
        }
    };
    // clamp the input so a 16 MiB window does not cost 256 passes over it
    let (data, truncated) = if data.len() > MAX_BRUTE_INPUT {
        (
            &data[..MAX_BRUTE_INPUT],
            Some(format!(
                "the input was clamped to the first {MAX_BRUTE_INPUT} bytes for the \
                 sweep; rerun on a narrower window to score a later region"
            )),
        )
    } else {
        (data, None)
    };
    // decode and score under every key
    let mut candidates: Vec<BruteCandidate> = keys
        .iter()
        .map(|key| {
            // transform never fails here: the op and key shape are both valid
            let decoded = transform(op, &[*key], data).unwrap_or_default();
            let (score, text_score, runs) = score_decode(&decoded);
            BruteCandidate {
                key: *key,
                score,
                text_score,
                runs,
                preview: brute_preview(&decoded, emit),
            }
        })
        .collect();
    // strongest first: score, then reportable text, then the smaller key
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.text_score.cmp(&a.text_score))
            .then(a.key.cmp(&b.key))
    });
    let tried = candidates.len();
    candidates.truncate(top);
    Ok(BruteResult {
        op: op.to_ascii_lowercase(),
        tried,
        candidates,
        truncated,
    })
}

/// Parse a byte_transform `key` argument: an int byte or a hex string
///
/// # Arguments
///
/// * `key` - The raw key value the model passed
fn parse_transform_key(key: &serde_json::Value) -> Result<Vec<u8>, String> {
    match key {
        // an integer is a single key byte
        serde_json::Value::Number(_) => {
            let byte = key
                .as_u64()
                .and_then(|k| u8::try_from(k).ok())
                .ok_or_else(|| "an integer `key` must be 0-255".to_owned())?;
            Ok(vec![byte])
        }
        // a string is hex key bytes
        serde_json::Value::String(text) => {
            let bytes = parse_hex(text)?;
            if bytes.is_empty() {
                return Err("`key` decodes to zero bytes".to_owned());
            }
            Ok(bytes)
        }
        _ => Err("`key` must be an integer (0-255) or a hex string".to_owned()),
    }
}

/// Applies one byte transform, or brute-forces its key space
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `op` - The transform: xor, add, sub, sub_from_key, or rol
/// * `key` - The key: an integer byte or a hex string (omit to brute-force)
/// * `brute` - Force a brute-force sweep over the op's key space
/// * `top` - The number of brute candidates to return (default 5, max 16)
/// * `emit` - The preview mode: strings (default), bytes, or hexdump
/// * `target` - The file to read, or `None`/`""` for the original sample
/// * `offset` - The file offset the input window starts at
/// * `length` - The input window's length
/// * `data` - Inline input bytes instead of a file window
/// * `encoding` - The inline data's encoding: hex (default), base64, text
/// * `name` - Save the full (non-brute) output under this workspace name
#[allow(clippy::too_many_arguments)]
#[thorium_derive::thorpal_tool(
    group = "decode",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_byte_transform",
    rhai_doc = "byte_transform(data: blob, op: string, key: int|blob) -> blob | byte_transform(data, op) -> map (brute) -- single-byte transforms",
    rhai_reference = "Applies one byte transform to a blob in memory, or brute-forces its key.

Ops: `\"xor\"` (key may be a multi-byte blob, repeating), `\"add\"`, `\"sub\"`,
`\"sub_from_key\"` ((K - b) & 0xff), `\"rol\"` (key is the bit count 1-7).

Overloads: `byte_transform(data, op, key: int)` and `byte_transform(data, op,
key: blob)` return the transformed blob; `byte_transform(data, op)` with no
key brute-forces the op's key space (0-255, or 1-7 for rol) and returns a map
with `op`, `tried`, `candidates` (an array of candidate maps -- `key`,
`score`, `text_score`, `runs`, `preview` -- strongest first), and an optional
`truncated` note when the input was clamped for the sweep.

NEVER hand-write these byte loops (or a key brute-force) in Rhai -- this
native binding replays them exactly."
)]
#[rig::tool_macro(
    description = "Apply one byte transform (xor with a 1-byte or repeating multi-byte key, add, sub, sub_from_key ((K - b) & 0xff), rol with a 1-7 bit count) to bytes, or brute-force the key -- never hand-write these standardized byte loops or a key sweep in Rhai, use this native tool. Pass `key` as an integer (0-255) or a hex string; omit `key` (or set brute: true) to sweep the op's whole key space, scoring each key by the readable text it recovers and returning the top candidates (default 5, max 16) with their first recovered runs and a preview. Input comes inline via `data` (+`encoding`) or from a file window (`target`/`offset`/`length`); brute input is clamped to 1 MiB. `emit` picks the preview style: strings (default), bytes, or hexdump. Pass `name` to save a keyed transform's full output into the extracted workspace; a saved output that identifies as PE/ELF/Mach-O can be imported with import_child -- the standard carve: decode -> save with `name` -> import_child."
)]
pub async fn byte_transform(
    #[rig(context)] context: &mut ToolContext,
    op: String,
    key: Option<serde_json::Value>,
    brute: Option<bool>,
    top: Option<usize>,
    emit: Option<String>,
    target: Option<String>,
    offset: Option<u64>,
    length: Option<usize>,
    data: Option<String>,
    encoding: Option<String>,
    name: Option<String>,
) -> Result<serde_json::Value, ToolExecutionError> {
    // resolve the preview mode and the input bytes
    let emit = parse_emit(emit.as_deref()).map_err(ToolExecutionError::invalid_args)?;
    let input = resolve_input(context, data, encoding, target, offset, length).await?;
    // no key (or an explicit brute) sweeps the key space
    if brute.unwrap_or(false) || key.is_none() {
        // a key alongside brute is ambiguous; refuse rather than guess
        if key.is_some() {
            return Err(ToolExecutionError::invalid_args(
                "`brute` sweeps the whole key space; drop `key` (or drop `brute` to \
                 apply the key)"
                    .to_owned(),
            ));
        }
        // a sweep produces candidates, not one output -- a `name` would be
        // silently dropped, so refuse it with the path forward named
        if name.is_some() {
            return Err(ToolExecutionError::invalid_args(
                "brute mode does not save output; rerun without brute, passing the \
                 winning candidate's key, to save the decoded bytes under `name`"
                    .to_owned(),
            ));
        }
        let top = top.unwrap_or(DEFAULT_BRUTE_TOP).clamp(1, MAX_BRUTE_TOP);
        // the sweep is pure CPU over up to 1 MiB x 256 keys; hop off the runtime
        let op = op.clone();
        let result = tokio::task::spawn_blocking(move || self::brute(&op, &input, top, emit))
            .await
            .map_err(|err| {
                ToolExecutionError::invalid_args(format!("the brute task failed: {err}"))
            })?
            .map_err(ToolExecutionError::invalid_args)?;
        return serde_json::to_value(&result).map_err(|err| {
            ToolExecutionError::invalid_args(format!("failed to serialize the result: {err}"))
        });
    }
    // a keyed transform runs once and returns a DecodeResult
    let key = parse_transform_key(key.as_ref().expect("key presence checked above"))
        .map_err(ToolExecutionError::invalid_args)?;
    let output = transform(&op, &key, &input).map_err(ToolExecutionError::invalid_args)?;
    let label = format!("{}(key {} bytes)", op.to_ascii_lowercase(), key.len());
    let mut result = build_result(vec![label], input.len(), &output, emit);
    if let Some(name) = name {
        result.saved = Some(save_output(context, name, output).await?);
    }
    serde_json::to_value(&result).map_err(|err| {
        ToolExecutionError::invalid_args(format!("failed to serialize the result: {err}"))
    })
}

// ===========================================================================
// pairwise_sub: the per-character stack-string scheme
// ===========================================================================

/// One readable run the pairwise decode surfaced, at its source offset
#[derive(Debug, Clone, Serialize)]
pub struct HiddenRun {
    /// The run's source offset as a `0x`-prefixed hex string
    pub offset: String,
    /// The recovered text, truncated to [`HIDDEN_RUN_CHARS`] characters
    /// (a cut run ends with a `… (truncated)` marker; save the full decode
    /// via `name` to read the rest)
    pub text: String,
}

/// A finished pairwise-SUB decode: the output plus its readable runs
#[derive(Debug, Clone, Serialize)]
pub struct PairwiseResult {
    /// The decode result over the whole window
    #[serde(flatten)]
    pub decode: DecodeResult,
    /// The readable runs the decode surfaced, wide-string rejected and
    /// clustered (see the pairwise decoder's own filtering)
    pub hidden: Vec<HiddenRun>,
}

/// Decodes a pairwise-SUB buffer (`out[i] = in[2i] - in[2i+1]`)
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `parity` - Which byte starts a pair: 0 (default) or 1 (skip one byte)
/// * `target` - The file to read, or `None`/`""` for the original sample
/// * `offset` - The file offset the input window starts at
/// * `length` - The input window's length
/// * `data` - Inline input bytes instead of a file window
/// * `encoding` - The inline data's encoding: hex (default), base64, text
/// * `name` - Save the full decoded output under this workspace name
#[allow(clippy::too_many_arguments)]
#[thorium_derive::thorpal_tool(
    group = "decode",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_pairwise_sub",
    rhai_doc = "pairwise_sub(data: blob) | pairwise_sub(data, parity: int) -> blob -- decode a pairwise-SUB buffer",
    rhai_reference = "Decodes a pairwise-SUB buffer in memory: each output byte is
`data[2i] - data[2i+1]` (wrapping), the per-character stack-string scheme
that stores each plaintext byte as a `(char + k, k)` pair.

`parity` (0 or 1, default 0) picks which byte starts a pair; pass 1 when the
encoded region starts one byte into the window.

NEVER hand-write this standardized scheme in Rhai -- this native binding
replays it exactly."
)]
#[rig::tool_macro(
    description = "Decode a pairwise-SUB buffer: each output byte is data[2i] - data[2i+1] (wrapping), the per-character stack-string scheme that stores each plaintext byte as a (char + k, k) pair with a free-running k -- never hand-write this standardized scheme in Rhai, use this native tool. `parity` (0 default, or 1) picks which byte starts a pair. Returns the decoded window's previews plus `hidden`: the readable runs the decode surfaced (wide-string rejected, density-clustered) with their source offsets. Input comes inline via `data` (+`encoding`) or from a file window (`target`/`offset`/`length`); pass `name` to save the full decoded output into the extracted workspace, where import_child can analyze an executable payload -- the standard carve: decode -> save with `name` -> import_child."
)]
pub async fn pairwise_sub(
    #[rig(context)] context: &mut ToolContext,
    parity: Option<u8>,
    target: Option<String>,
    offset: Option<u64>,
    length: Option<usize>,
    data: Option<String>,
    encoding: Option<String>,
    name: Option<String>,
) -> Result<PairwiseResult, ToolExecutionError> {
    // the parity picks which byte starts a pair
    let parity = parity.unwrap_or(0);
    if parity > 1 {
        return Err(ToolExecutionError::invalid_args(
            "`parity` must be 0 (pairs start at the first byte) or 1 (skip one byte)".to_owned(),
        ));
    }
    // the hidden-run offsets are rebased onto the file when file-sourced
    let base = if data.is_none() {
        offset.unwrap_or(0)
    } else {
        0
    };
    let input = resolve_input(context, data, encoding, target, offset, length).await?;
    // shift the window by the parity, then decode the pairs
    let shifted = &input[usize::from(parity).min(input.len())..];
    let output = pairwise::decode(shifted);
    // surface the readable runs with their source offsets rebased
    let mut hidden: Vec<HiddenRun> = pairwise::hidden_runs(shifted)
        .into_iter()
        .map(|(src_offset, mut text)| {
            // keep a long run's head rather than ballooning the result; the
            // runs are pure printable ASCII, so a byte cut is a char cut
            if text.len() > HIDDEN_RUN_CHARS {
                text.truncate(HIDDEN_RUN_CHARS);
                text.push_str("… (truncated)");
            }
            HiddenRun {
                offset: format!("{:#x}", base + u64::from(parity) + src_offset as u64),
                text,
            }
        })
        .collect();
    // bound the run list; the decoded output itself carries the rest
    let mut result = build_result(
        vec![format!("pairwise_sub(parity {parity})")],
        input.len(),
        &output,
        "strings",
    );
    if hidden.len() > MAX_HIDDEN_RUNS {
        let total = hidden.len();
        hidden.truncate(MAX_HIDDEN_RUNS);
        result.truncated = Some(match result.truncated {
            Some(note) => {
                format!("{note}; hidden runs truncated to the first {MAX_HIDDEN_RUNS} of {total}")
            }
            None => format!("hidden runs truncated to the first {MAX_HIDDEN_RUNS} of {total}"),
        });
    }
    if let Some(name) = name {
        result.saved = Some(save_output(context, name, output).await?);
    }
    Ok(PairwiseResult {
        decode: result,
        hidden,
    })
}

// ===========================================================================
// api_hash_match: resolve import hashes against the curated dictionary
// ===========================================================================

/// One resolved hash in a tool result, with the hash rendered as hex
#[derive(Debug, Clone, Serialize)]
pub struct MatchedHash {
    /// The target hash, as a `0x`-prefixed hex string
    pub hash: String,
    /// The name whose variant hashes to the target
    pub name: String,
    /// The hash family the match was found under
    pub family: &'static str,
    /// The variant of the name that produced the hash
    pub variant: String,
}

/// A finished API-hash matching pass
#[derive(Debug, Clone, Serialize)]
pub struct HashMatchResult {
    /// The resolved hashes with their names, families, and variants
    pub matched: Vec<MatchedHash>,
    /// The targets nothing resolved, as `0x`-prefixed hex strings
    pub unmatched: Vec<String>,
    /// The number of names tried (curated dictionary plus extras)
    pub names_tried: usize,
    /// A note about the dictionary and any caps that applied
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Parse one hash target: an unsigned integer or a hex string
///
/// # Arguments
///
/// * `value` - The raw target value the caller passed
pub(crate) fn parse_hash_target(value: &serde_json::Value) -> Result<u32, String> {
    match value {
        // an integer must fit a u32
        serde_json::Value::Number(_) => value
            .as_u64()
            .and_then(|hash| u32::try_from(hash).ok())
            .ok_or_else(|| format!("hash target {value} is not a 32-bit unsigned integer")),
        // a string is hex, with or without the 0x prefix
        serde_json::Value::String(text) => {
            let cleaned = text.trim();
            let cleaned = cleaned
                .strip_prefix("0x")
                .or_else(|| cleaned.strip_prefix("0X"))
                .unwrap_or(cleaned);
            u32::from_str_radix(cleaned, 16)
                .map_err(|_| format!("hash target '{text}' is not a hex u32"))
        }
        _ => Err(format!(
            "hash target {value} must be an unsigned integer or a hex string"
        )),
    }
}

/// Parse a salt position name into whether the salt suffixes (the default)
///
/// Shared by the tool path and the Rhai binding so the two can never drift
/// on casing or spelling.
///
/// # Arguments
///
/// * `position` - The requested position, `None` for the suffix default
pub(crate) fn parse_salt_position(position: Option<&str>) -> Result<bool, String> {
    // match case-insensitively; the default is a suffix salt
    match position {
        None => Ok(true),
        Some(name) if name.eq_ignore_ascii_case("suffix") => Ok(true),
        Some(name) if name.eq_ignore_ascii_case("prefix") => Ok(false),
        Some(other) => Err(format!(
            "unknown `salt_position` '{other}'; supported: prefix, suffix (default)"
        )),
    }
}

/// Parse the optional `variants` JSON object into [`NameVariants`]
///
/// # Arguments
///
/// * `value` - The raw variants object, `None` for all-on defaults
pub(crate) fn parse_variants(value: Option<&serde_json::Value>) -> Result<NameVariants, String> {
    // the default tries every variant
    let mut variants = NameVariants::default();
    let Some(value) = value else {
        return Ok(variants);
    };
    let object = value
        .as_object()
        .ok_or_else(|| "`variants` must be an object of booleans".to_owned())?;
    // apply each known flag, rejecting typos
    for (key, flag) in object {
        let flag = flag
            .as_bool()
            .ok_or_else(|| format!("variants field `{key}` must be a boolean"))?;
        match key.as_str() {
            "wide" => variants.wide = flag,
            "lowercase" => variants.lowercase = flag,
            "uppercase" => variants.uppercase = flag,
            "nt_zw_twins" => variants.nt_zw_twins = flag,
            "strip_ext" => variants.strip_ext = flag,
            other => {
                return Err(format!(
                    "unknown variants field `{other}`; supported: wide, lowercase, \
                     uppercase, nt_zw_twins, strip_ext"
                ));
            }
        }
    }
    Ok(variants)
}

/// Match hash targets against the dictionary, synchronously
///
/// Shared by the async tool path and the Rhai binding.
///
/// # Arguments
///
/// * `targets` - The parsed hash targets
/// * `family` - The family name (`all` tries every parameterless family)
/// * `seed` - The `mul_rolling` seed, when that family is in play
/// * `multiplier` - The `mul_rolling` multiplier
/// * `salt` - Optional salt bytes and whether they suffix (true) or prefix
/// * `variants` - The name variants to try
/// * `extra_names` - Caller-supplied names tried beyond the dictionary
pub(crate) fn api_hash_match_sync(
    targets: &[u32],
    family: &str,
    seed: Option<u64>,
    multiplier: Option<u64>,
    salt: Option<(Vec<u8>, bool)>,
    variants: &NameVariants,
    extra_names: &[String],
) -> Result<HashMatchResult, String> {
    // bound the target list so one call stays cheap
    if targets.is_empty() {
        return Err("`targets` must hold at least one hash".to_owned());
    }
    if targets.len() > MAX_HASH_TARGETS {
        return Err(format!(
            "`targets` holds {} hashes; at most {MAX_HASH_TARGETS} resolve per call",
            targets.len()
        ));
    }
    // narrow the rolling-hash parameters to u32 with honest errors
    let narrow = |value: Option<u64>, what: &str| -> Result<Option<u32>, String> {
        value
            .map(|value| {
                u32::try_from(value)
                    .map_err(|_| format!("`{what}` must fit a 32-bit unsigned integer"))
            })
            .transpose()
    };
    let seed = narrow(seed, "seed")?;
    let multiplier = narrow(multiplier, "multiplier")?;
    let families = apihash::families_from_str(family, seed, multiplier)?;
    // walk the dictionary once
    let salt_ref = salt
        .as_ref()
        .map(|(bytes, suffix)| (bytes.as_slice(), *suffix));
    let (matches, unmatched) =
        apihash::match_hashes(targets, &families, variants, salt_ref, extra_names);
    // assemble the model-facing result with hex-rendered hashes
    let capped = matches.len() >= MAX_HASH_MATCHES;
    let matched: Vec<MatchedHash> = matches
        .into_iter()
        .map(|hit| MatchedHash {
            hash: format!("{:#010x}", hit.hash),
            name: hit.name,
            family: hit.family,
            variant: hit.variant,
        })
        .collect();
    let unmatched: Vec<String> = unmatched
        .into_iter()
        .map(|hash| format!("{hash:#010x}"))
        .collect();
    // the note explains the dictionary and what a miss means
    let mut notes = vec![format!(
        "matched against a curated dictionary of {} well-known export/DLL/process \
         names (not a full export table)",
        apihash::names_tried(&[])
    )];
    if !unmatched.is_empty() {
        notes.push(
            "unmatched hashes may use a salt, a different family/seed, or an \
             uncommon export; pass `extra_names` to extend the dictionary"
                .to_owned(),
        );
    }
    if capped {
        notes.push(format!(
            "matches capped at {MAX_HASH_MATCHES}; narrow the targets or families"
        ));
    }
    Ok(HashMatchResult {
        matched,
        unmatched,
        names_tried: apihash::names_tried(extra_names),
        note: Some(notes.join("; ")),
    })
}

/// Resolves API-name hashes back to the names they hash
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `targets` - The hashes to resolve: integers or hex strings (max 256)
/// * `family` - The hash family (default `all`)
/// * `seed` - The `mul_rolling` seed (give with `multiplier`)
/// * `multiplier` - The `mul_rolling` multiplier
/// * `salt_hex` - Salt bytes (hex) mixed into every hashed name
/// * `salt_position` - Where the salt goes: `prefix` or `suffix` (default)
/// * `variants` - Booleans toggling name variants (see the description)
/// * `extra_names` - Extra candidate names tried beyond the dictionary
#[allow(clippy::too_many_arguments)]
#[thorium_derive::thorpal_tool(
    group = "decode",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_api_hash_match",
    rhai_doc = "api_hash_match(targets: array, family: string) | api_hash_match(targets, family, opts: map) -> map -- resolve API-name hashes",
    rhai_reference = "Resolves API-name hashes back to the export/DLL/process names they hash,
by hashing a curated dictionary (~2,400 well-known names) under the requested
families and name variants.

`targets` is an array of ints and/or hex strings (max 256). `family` is one
of `fnv1a`, `mul_rolling`, `ror13`, `crc32`, `djb2_add`, `djb2_xor`,
`byte_sum`, or `all`. The optional `opts` map takes: `seed` + `multiplier`
(ints, parameterizing mul_rolling), `salt_hex` (string) + `salt_position`
(`\"prefix\"`/`\"suffix\"`), `variants` (a map of booleans: `wide`,
`lowercase`, `uppercase`, `nt_zw_twins`, `strip_ext`), and `extra_names`
(an array of strings tried beyond the dictionary).

Returns a map with `matched` (array of `hash`/`name`/`family`/`variant`
maps), `unmatched` (hex strings), `names_tried`, and a `note`.

NEVER hand-write an API-hash brute force in Rhai -- this native binding
replays every family and variant at once."
)]
#[rig::tool_macro(
    description = "Resolve API-name hashes (shellcode/loader import hashing) back to the names they hash -- never hand-write an API-hash brute force in Rhai, use this native tool. Hashes a curated dictionary of well-known Windows export, DLL, and process names (plus your `extra_names`) under the requested family and reports every name whose hash hits a target. `targets` takes integers and/or hex strings (max 256). `family`: fnv1a, mul_rolling (needs `seed` + `multiplier`, e.g. SwitchBack's 0xC00018BE/0x21), ror13 (the Metasploit resolver), crc32, djb2_add, djb2_xor, byte_sum, or all (default). Name variants tried per entry (toggle via `variants` booleans): wide (UTF-16LE), lowercase, uppercase, nt_zw_twins (Zw twins of Nt names), strip_ext (kernel32.dll -> kernel32), each with and without a trailing NUL; `salt_hex` + `salt_position` (prefix|suffix) mix a salt in. Returns matched (name/family/variant per hash), unmatched, and names_tried."
)]
pub async fn api_hash_match(
    #[rig(context)] _context: &mut ToolContext,
    targets: Vec<serde_json::Value>,
    family: Option<String>,
    seed: Option<u64>,
    multiplier: Option<u64>,
    salt_hex: Option<String>,
    salt_position: Option<String>,
    variants: Option<serde_json::Value>,
    extra_names: Option<Vec<String>>,
) -> Result<HashMatchResult, ToolExecutionError> {
    // coerce every target up front so a typo fails before any hashing
    let targets: Vec<u32> = targets
        .iter()
        .map(parse_hash_target)
        .collect::<Result<_, _>>()
        .map_err(ToolExecutionError::invalid_args)?;
    // resolve the salt bytes and their position
    let salt = match salt_hex {
        None => {
            // a position without salt bytes is a mistake worth naming
            if salt_position.is_some() {
                return Err(ToolExecutionError::invalid_args(
                    "`salt_position` needs `salt_hex`".to_owned(),
                ));
            }
            None
        }
        Some(text) => {
            let bytes = parse_hex(&text).map_err(ToolExecutionError::invalid_args)?;
            // the shared parser keeps the tool and the binding in lockstep
            let suffix = parse_salt_position(salt_position.as_deref())
                .map_err(ToolExecutionError::invalid_args)?;
            Some((bytes, suffix))
        }
    };
    let variants = parse_variants(variants.as_ref()).map_err(ToolExecutionError::invalid_args)?;
    let family = family.unwrap_or_else(|| "all".to_owned());
    let extra_names = extra_names.unwrap_or_default();
    // the walk is pure CPU over a few hundred thousand hashes; hop off the runtime
    tokio::task::spawn_blocking(move || {
        api_hash_match_sync(
            &targets,
            &family,
            seed,
            multiplier,
            salt,
            &variants,
            &extra_names,
        )
    })
    .await
    .map_err(|err| ToolExecutionError::invalid_args(format!("the match task failed: {err}")))?
    .map_err(ToolExecutionError::invalid_args)
}

// ===========================================================================
// grep_bytes: keyword search over raw bytes
// ===========================================================================

/// Which encodings a grep searches for its keywords in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrepEncodings {
    /// Plain ASCII bytes only
    Ascii,
    /// UTF-16LE (interleaved zero high bytes) only
    Utf16le,
    /// Both encodings at once
    Both,
}

impl GrepEncodings {
    /// The model-facing name this encodings selection renders as
    pub(crate) fn label(self) -> &'static str {
        match self {
            GrepEncodings::Ascii => "ascii",
            GrepEncodings::Utf16le => "utf16le",
            GrepEncodings::Both => "ascii+utf16le",
        }
    }

    /// Parse the model-facing encodings name
    ///
    /// # Arguments
    ///
    /// * `name` - The requested encodings, `None` for both
    pub(crate) fn parse(name: Option<&str>) -> Result<Self, String> {
        match name.map(str::to_ascii_lowercase).as_deref() {
            None | Some("both") => Ok(GrepEncodings::Both),
            Some("ascii") => Ok(GrepEncodings::Ascii),
            Some("utf16le") => Ok(GrepEncodings::Utf16le),
            Some(other) => Err(format!(
                "unknown `encodings` '{other}'; supported: ascii, utf16le, both (default)"
            )),
        }
    }
}

/// One keyword hit in a scanned window
#[derive(Debug, Clone, Serialize)]
pub struct GrepMatch {
    /// The file offset the match starts at, as a `0x`-prefixed hex string
    pub offset: String,
    /// The keyword that matched
    pub keyword: String,
    /// The encoding the keyword matched in (`ascii` or `utf16le`)
    pub encoding: &'static str,
    /// A hexdump of the match with its surrounding context bytes
    pub context: String,
}

/// A finished grep over one window of a file
#[derive(Debug, Clone, Serialize)]
pub struct GrepResult {
    /// The matches found, in offset order
    pub matches: Vec<GrepMatch>,
    /// The file offset the scan started at, as a `0x`-prefixed hex string
    pub scanned_from: String,
    /// The file offset the scan stopped at (safe to resume from)
    pub scanned_to: String,
    /// The file's total size in bytes
    pub file_size: u64,
    /// A note describing anything left unscanned and how to continue
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
    /// A zero-match coverage statement scoping what the scan proved
    ///
    /// A negative scan is evidence only about the window and encodings it
    /// covered: an evaluation minted a wrong absence claim from a partial check, so
    /// every zero-match result now states its own coverage explicitly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<String>,
}

/// Find every start position of `needle` in `haystack`
///
/// A first-byte-skip scan: cheap enough for one 8 MiB window times a bounded
/// keyword list without pulling in a search crate.
///
/// # Arguments
///
/// * `haystack` - The bytes to search
/// * `needle` - The byte pattern to find
fn find_all(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    let mut positions = Vec::new();
    // an empty or oversized needle can never match
    if needle.is_empty() || needle.len() > haystack.len() {
        return positions;
    }
    let first = needle[0];
    let mut index = 0;
    // skip to each candidate first byte, then compare the rest
    while index + needle.len() <= haystack.len() {
        match haystack[index..=haystack.len() - needle.len()]
            .iter()
            .position(|byte| *byte == first)
        {
            Some(skip) => {
                let at = index + skip;
                if haystack[at..at + needle.len()] == *needle {
                    positions.push(at);
                }
                index = at + 1;
            }
            None => break,
        }
    }
    positions
}

/// Grep one window of bytes for keywords in one or both encodings
///
/// Mirrors the strings tool's resume contract: unless `at_eof`, the scan
/// stops `max keyword length - 1` bytes short of the window edge (so a
/// keyword straddling the edge is found by the next page), and hitting the
/// match limit stops at the first undelivered match -- with every hit tied
/// at that offset delivered too (the limit may overshoot by the ties), so
/// the returned stop offset is always safe to resume from without skipping,
/// duplicating, or stalling.
///
/// # Arguments
///
/// * `bytes` - The window of raw bytes to scan
/// * `base` - The file offset the window starts at
/// * `keywords` - The keywords to search for
/// * `encodings` - Which encodings to search in
/// * `case_insensitive` - Match case-insensitively (ASCII-only case folding;
///   non-ASCII characters always match their exact bytes)
/// * `context` - The context bytes hexdumped around each match
/// * `limit` - The most matches to return
/// * `at_eof` - Whether the window ends at the end of the file itself
#[allow(clippy::too_many_arguments)]
pub(crate) fn grep_window(
    bytes: &[u8],
    base: u64,
    keywords: &[String],
    encodings: GrepEncodings,
    case_insensitive: bool,
    context: usize,
    limit: usize,
    at_eof: bool,
) -> Result<(Vec<GrepMatch>, u64, bool), String> {
    // validate the keyword list before any scanning
    if keywords.is_empty() {
        return Err("`keywords` must hold at least one keyword".to_owned());
    }
    if keywords.len() > MAX_KEYWORDS {
        return Err(format!(
            "`keywords` holds {} keywords; at most {MAX_KEYWORDS} search per call",
            keywords.len()
        ));
    }
    for keyword in keywords {
        if keyword.is_empty() {
            return Err("keywords must not be empty".to_owned());
        }
        if keyword.len() > MAX_KEYWORD_LEN {
            // clip on characters, not bytes: a multi-byte char at the cut
            // would panic a byte slice
            let head: String = keyword.chars().take(16).collect();
            return Err(format!(
                "keyword '{head}…' is {} bytes; keywords cap at {MAX_KEYWORD_LEN}",
                keyword.len()
            ));
        }
    }
    // one lowering pass over the window makes every match case-insensitive
    let haystack: Vec<u8> = if case_insensitive {
        bytes.to_ascii_lowercase()
    } else {
        bytes.to_vec()
    };
    // build the needles: ascii and/or interleaved utf16le per keyword
    let mut needles: Vec<(usize, &'static str, Vec<u8>)> = Vec::new();
    for (index, keyword) in keywords.iter().enumerate() {
        // fold ASCII-only, matching the haystack's fold exactly: a Unicode
        // fold here would turn 'É' into a byte sequence the haystack never
        // contains, making non-ASCII keywords miss even exact-case bytes
        let folded = if case_insensitive {
            keyword.to_ascii_lowercase()
        } else {
            keyword.clone()
        };
        if encodings != GrepEncodings::Utf16le {
            needles.push((index, "ascii", folded.as_bytes().to_vec()));
        }
        if encodings != GrepEncodings::Ascii {
            let wide: Vec<u8> = folded.encode_utf16().flat_map(u16::to_le_bytes).collect();
            needles.push((index, "utf16le", wide));
        }
    }
    // the scan is only complete short of the edge, so a straddling keyword
    // is re-read whole by the next page (unless the file itself ends here)
    let max_len = needles.iter().map(|(_, _, n)| n.len()).max().unwrap_or(1);
    let backed = haystack.len().saturating_sub(max_len.saturating_sub(1));
    let stop = if at_eof || backed == 0 {
        haystack.len()
    } else {
        backed
    };
    // collect every hit starting before the stop, in offset order
    let mut hits: Vec<(usize, usize, &'static str, usize)> = Vec::new();
    for (keyword_index, encoding, needle) in &needles {
        for position in find_all(&haystack, needle) {
            if position < stop {
                hits.push((position, *keyword_index, encoding, needle.len()));
            }
        }
    }
    hits.sort_unstable_by_key(|(position, keyword, _, _)| (*position, *keyword));
    // apply the limit: the stop backs up to the first undelivered match --
    // unless hits tie at that offset, in which case every tied hit is
    // delivered (overshooting the limit) and the stop lands just past it, so
    // a resume can neither re-deliver a tied hit nor stall at zero progress
    let mut hit_limit = false;
    let mut scanned_to = base + stop as u64;
    if hits.len() > limit {
        hit_limit = true;
        // the first undelivered match's offset is the candidate boundary
        let boundary = hits[limit].0;
        let before = hits.iter().take_while(|hit| hit.0 < boundary).count();
        if before == limit {
            // a clean cut: everything before the boundary is delivered and
            // the resume re-finds the boundary match first
            hits.truncate(limit);
            scanned_to = base + boundary as u64;
        } else {
            // ties straddle the cut: deliver every hit at the boundary too
            let keep = hits.iter().take_while(|hit| hit.0 <= boundary).count();
            hits.truncate(keep);
            scanned_to = base + boundary as u64 + 1;
        }
    }
    // render each hit with its context hexdump
    let matches = hits
        .into_iter()
        .map(|(position, keyword_index, encoding, needle_len)| {
            let start = position.saturating_sub(context);
            let end = (position + needle_len + context).min(bytes.len());
            GrepMatch {
                offset: format!("{:#x}", base + position as u64),
                keyword: keywords[keyword_index].clone(),
                encoding,
                context: file::hexdump(&bytes[start..end], base + start as u64),
            }
        })
        .collect();
    Ok((matches, scanned_to, hit_limit))
}

/// Searches raw file bytes for keywords in ASCII and/or UTF-16LE
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `keywords` - The keywords to search for (max 32, 128 bytes each)
/// * `encodings` - Which encodings to search: ascii, utf16le, both (default)
/// * `case_insensitive` - Match case-insensitively (default true)
/// * `context_bytes` - Context bytes hexdumped around each match (default 16)
/// * `limit` - The most matches to return (default 100, max 500)
/// * `target` - The file to read, or `None`/`""` for the original sample
/// * `offset` - The file offset the scan window starts at
/// * `length` - The scan window's length (default/cap one 8 MiB window)
#[allow(clippy::too_many_arguments)]
#[thorium_derive::thorpal_tool(
    group = "decode",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_grep_bytes",
    rhai_doc = "grep_bytes(data: blob, keywords: array) | grep_bytes(data, keywords, opts: map) -> map -- keyword search over bytes",
    rhai_reference = "Searches a blob for keywords in ASCII and/or UTF-16LE and returns a map
with `matches` (an array of match maps, each with `offset` -- a hex string
relative to the blob -- `keyword`, `encoding` (`\"ascii\"`/`\"utf16le\"`), and
`context`, a hexdump around the match), `hit_limit` (bool: whether the match
limit cut the scan short), and `scanned_to` (int: the blob offset the scan
stopped at, safe to resume a sliced blob from).

The optional `opts` map takes: `encodings` (`\"ascii\"`/`\"utf16le\"`/
`\"both\"`, default both), `case_insensitive` (bool, default true; the fold
is ASCII-only, so non-ASCII bytes match exactly), `context` (int, context
bytes around each match), and `limit` (int, max matches).

NEVER hand-write a byte scanner for plain keywords in Rhai -- this native
binding scans both encodings at once."
)]
#[rig::tool_macro(
    description = "Search raw file bytes for keywords in ASCII and UTF-16LE at once, case-insensitively by default (ASCII-only folding; non-ASCII characters match their exact bytes) -- never hand-write this standardized byte scan in Rhai, use this native tool. Each match reports its file offset, the keyword, the encoding it matched in, and a hexdump of the surrounding context (default 16 bytes each side, max 64). Up to 32 keywords (128 bytes each) per call; `limit` defaults to 100 (max 500). Scans one window (default 8 MiB) from `offset` of the ORIGINAL sample or an extracted file (`target`); the result's scanned_to is always safe to resume from (a keyword cut by the window edge is found by the next page)."
)]
pub async fn grep_bytes(
    #[rig(context)] context: &mut ToolContext,
    keywords: Vec<String>,
    encodings: Option<String>,
    case_insensitive: Option<bool>,
    context_bytes: Option<usize>,
    limit: Option<usize>,
    target: Option<String>,
    offset: Option<u64>,
    length: Option<usize>,
) -> Result<GrepResult, ToolExecutionError> {
    // clamp the knobs to usable ranges
    let encodings =
        GrepEncodings::parse(encodings.as_deref()).map_err(ToolExecutionError::invalid_args)?;
    let case_insensitive = case_insensitive.unwrap_or(true);
    let context_window = context_bytes.unwrap_or(DEFAULT_CONTEXT).min(MAX_CONTEXT);
    let limit = limit.unwrap_or(DEFAULT_MATCHES).clamp(1, MAX_MATCHES);
    // resolve the file and read one scan window
    let path = resolve_target(context, target.as_deref()).await?;
    let offset = offset.unwrap_or(0);
    let window = length.unwrap_or(SCAN_WINDOW).min(SCAN_WINDOW);
    let (bytes, file_size) = read_input(path, offset, window)
        .await
        .map_err(ToolExecutionError::invalid_args)?;
    // the window edge is only final when it is the end of the file
    let at_eof = offset + bytes.len() as u64 >= file_size;
    // scan on a blocking thread: 8 MiB x 32 keywords is real CPU work
    let (matches, scanned_to, hit_limit) = tokio::task::spawn_blocking(move || {
        grep_window(
            &bytes,
            offset,
            &keywords,
            encodings,
            case_insensitive,
            context_window,
            limit,
            at_eof,
        )
    })
    .await
    .map_err(|err| ToolExecutionError::invalid_args(format!("the grep task failed: {err}")))?
    .map_err(ToolExecutionError::invalid_args)?;
    // note anything left unscanned; scanned_to is always the resume offset
    let note = if hit_limit {
        Some(format!(
            "match limit of {limit} hit at {scanned_to:#x}; rerun with offset = \
             {scanned_to:#x} to continue"
        ))
    } else if scanned_to < file_size {
        Some(format!(
            "scanned up to {scanned_to:#x} of {file_size} bytes; rerun with offset = \
             {scanned_to:#x} to continue"
        ))
    } else {
        None
    };
    // scope what a negative result actually proved: window + encodings only
    let coverage = matches.is_empty().then(|| {
        format!(
            "0 matches over bytes {offset:#x}..{scanned_to:#x} of {file_size}              (encodings: {}, case_insensitive: {case_insensitive}); this scan              proves absence only within that window and those encodings",
            encodings.label(),
        )
    });
    Ok(GrepResult {
        matches,
        scanned_from: format!("{offset:#x}"),
        scanned_to: format!("{scanned_to:#x}"),
        file_size,
        truncated: note,
        coverage,
    })
}

// ===========================================================================
// strings_range: bounded string extraction
// ===========================================================================

/// Parse the strings_range `encodings` post-filter
///
/// # Arguments
///
/// * `name` - The requested filter, `None` for both encodings
fn parse_string_encodings(name: Option<&str>) -> Result<Option<&'static str>, String> {
    // the filter keeps one encoding; None keeps both
    match name.map(str::to_ascii_lowercase).as_deref() {
        None | Some("both") => Ok(None),
        Some("ascii") => Ok(Some("ascii")),
        Some("utf16le") => Ok(Some("utf16le")),
        Some(other) => Err(format!(
            "unknown `encodings` '{other}'; supported: ascii, utf16le, both (default)"
        )),
    }
}

/// Extracts strings from an explicit byte range of a file
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `offset` - The file offset the range starts at
/// * `length` - The range's length (clamped to one 8 MiB window)
/// * `min_length` - The minimum run length counted as a string (default 4)
/// * `limit` - The most strings to return (default 200, max 500)
/// * `encodings` - Keep one encoding: ascii, utf16le, or both (default)
/// * `filter` - An optional case-insensitive substring filter
/// * `target` - The file to read, or `None`/`""` for the original sample
#[allow(clippy::too_many_arguments)]
#[thorium_derive::thorpal_tool(
    group = "decode",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_strings_range",
    rhai_doc = "strings_range(data: blob, min_len: int) | strings_range(data, min_len, limit) -> array -- extract strings from a blob",
    rhai_reference = "Extracts printable-ASCII and UTF-16LE strings from a blob in memory --
the in-memory twin of strings_file, for scanning a buffer a script just
decoded.

Overloads: `strings_range(data: blob, min_len: int)` and `strings_range(data,
min_len, limit: int)`. Returns an array of maps, each with `offset`
(hex string, relative to the blob), `encoding` (`\"ascii\"`/`\"utf16le\"`),
and `text`.

NEVER hand-write a printable-run scanner in Rhai -- this native binding runs
the same extractor as the strings tools."
)]
#[rig::tool_macro(
    description = "Extract printable-ASCII and UTF-16LE strings from an explicit byte range of the ORIGINAL sample or an extracted file (`target`) -- the range-bounded twin of strings_file for scanning exactly one region (a decrypted blob you saved, an overlay, one section). `offset` + `length` bound the range (length clamps to one 8 MiB window); `min_length` (default 4), `limit` (default 200, max 500), and `filter` (case-insensitive substring) narrow the result, and `encodings` keeps just ascii or utf16le strings. Never hand-write a printable-run scanner in Rhai -- this native tool (and its blob binding) already does it."
)]
pub async fn strings_range(
    #[rig(context)] context: &mut ToolContext,
    offset: u64,
    length: Option<usize>,
    min_length: Option<usize>,
    limit: Option<usize>,
    encodings: Option<String>,
    filter: Option<String>,
    target: Option<String>,
) -> Result<FileStrings, ToolExecutionError> {
    // clamp the knobs to the same ranges strings_file enforces
    let keep =
        parse_string_encodings(encodings.as_deref()).map_err(ToolExecutionError::invalid_args)?;
    let min_length = min_length.unwrap_or(4).max(2);
    let limit = limit
        .unwrap_or(file::DEFAULT_STRINGS)
        .clamp(1, file::MAX_STRINGS);
    let window = length.unwrap_or(SCAN_WINDOW).min(SCAN_WINDOW);
    // resolve the file and read the requested range
    let path = resolve_target(context, target.as_deref()).await?;
    let (bytes, file_size) = read_input(path, offset, window)
        .await
        .map_err(ToolExecutionError::invalid_args)?;
    // the range edge is only final when it is the end of the file
    let at_eof = offset + bytes.len() as u64 >= file_size;
    // reuse the strings tool's extractor so semantics never drift
    let (strings, scanned_to, hit_limit) =
        file::extract_strings(&bytes, offset, min_length, limit, filter.as_deref(), at_eof);
    // apply the encoding post-filter over the merged runs
    let strings: Vec<file::FileString> = match keep {
        Some(encoding) => strings
            .into_iter()
            .filter(|string| string.encoding == encoding)
            .collect(),
        None => strings,
    };
    // note anything left unscanned within the requested range
    let requested_end = offset.saturating_add(window as u64).min(file_size);
    let note = if hit_limit {
        Some(format!(
            "string limit of {limit} hit at {scanned_to:#x}; rerun with offset = \
             {scanned_to:#x} to continue"
        ))
    } else if scanned_to < requested_end {
        Some(format!(
            "scanned the range up to {scanned_to:#x}; rerun with offset = \
             {scanned_to:#x} to continue"
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

// ===========================================================================
// digest: hash whole files or byte ranges
// ===========================================================================

/// The digest algorithms the digest tool can run
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DigestAlgo {
    /// MD5 (the triage-lookup classic)
    Md5,
    /// SHA-1
    Sha1,
    /// SHA-256
    Sha256,
}

impl DigestAlgo {
    /// Parse the model-facing algorithm name
    ///
    /// # Arguments
    ///
    /// * `name` - The requested algorithm
    pub(crate) fn parse(name: &str) -> Result<Self, String> {
        match name.to_ascii_lowercase().as_str() {
            "md5" => Ok(DigestAlgo::Md5),
            "sha1" => Ok(DigestAlgo::Sha1),
            "sha256" => Ok(DigestAlgo::Sha256),
            other => Err(format!(
                "unknown digest algorithm '{other}'; supported: md5, sha1, sha256"
            )),
        }
    }

    /// The stable lowercase label this algorithm reports under
    fn label(self) -> &'static str {
        match self {
            DigestAlgo::Md5 => "md5",
            DigestAlgo::Sha1 => "sha1",
            DigestAlgo::Sha256 => "sha256",
        }
    }
}

/// One streaming hasher behind whichever algorithm was picked
enum AnyHasher {
    /// A running MD5
    Md5(Md5),
    /// A running SHA-1
    Sha1(Sha1),
    /// A running SHA-256
    Sha256(Sha256),
}

impl AnyHasher {
    /// Start a fresh hasher for an algorithm
    ///
    /// # Arguments
    ///
    /// * `algo` - The algorithm to hash under
    fn new(algo: DigestAlgo) -> Self {
        match algo {
            DigestAlgo::Md5 => AnyHasher::Md5(Md5::new()),
            DigestAlgo::Sha1 => AnyHasher::Sha1(Sha1::new()),
            DigestAlgo::Sha256 => AnyHasher::Sha256(Sha256::new()),
        }
    }

    /// Feed one chunk into the running hash
    ///
    /// # Arguments
    ///
    /// * `bytes` - The chunk to absorb
    fn update(&mut self, bytes: &[u8]) {
        match self {
            AnyHasher::Md5(hasher) => hasher.update(bytes),
            AnyHasher::Sha1(hasher) => hasher.update(bytes),
            AnyHasher::Sha256(hasher) => hasher.update(bytes),
        }
    }

    /// Finish the hash and render it as lowercase hex
    fn finish(self) -> String {
        match self {
            AnyHasher::Md5(hasher) => hex(hasher.finalize().as_slice()),
            AnyHasher::Sha1(hasher) => hex(hasher.finalize().as_slice()),
            AnyHasher::Sha256(hasher) => hex(hasher.finalize().as_slice()),
        }
    }
}

/// Digest one in-memory buffer (the Rhai binding's path)
///
/// # Arguments
///
/// * `algo` - The algorithm to hash under
/// * `bytes` - The bytes to hash
pub(crate) fn digest_bytes(algo: DigestAlgo, bytes: &[u8]) -> String {
    // one buffer, one hasher
    let mut hasher = AnyHasher::new(algo);
    hasher.update(bytes);
    hasher.finish()
}

/// One hashed range in a digest result
#[derive(Debug, Clone, Serialize)]
pub struct RangeDigest {
    /// The range's start offset, as a `0x`-prefixed hex string
    pub offset: String,
    /// The range's requested length in bytes
    pub length: u64,
    /// The bytes actually read (shorter than `length` at EOF)
    pub read: u64,
    /// The range's own digest, when `per_range` was set
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

/// A finished digest over a file or its ranges
#[derive(Debug, Clone, Serialize)]
pub struct DigestResult {
    /// The algorithm that ran
    pub algo: &'static str,
    /// The file's total size in bytes
    pub file_size: u64,
    /// The single digest over the ranges' bytes concatenated in order
    /// (absent when `per_range` was set)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// The ranges that were hashed, in order
    pub ranges: Vec<RangeDigest>,
    /// A note describing short reads at EOF
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Hash a file's byte ranges synchronously, streaming in 1 MiB chunks
///
/// An empty `ranges` hashes the whole file. By default the ranges' bytes are
/// concatenated in order into one digest; `per_range` digests each range on
/// its own instead. A range reaching past EOF is hashed short and reported.
///
/// # Arguments
///
/// * `path` - The file to hash
/// * `algo` - The algorithm to hash under
/// * `ranges` - The `(offset, length)` ranges, in hashing order
/// * `per_range` - Digest each range on its own instead of concatenated
pub(crate) fn digest_ranges_sync(
    path: &Path,
    algo: DigestAlgo,
    ranges: &[(u64, u64)],
    per_range: bool,
) -> Result<DigestResult, String> {
    // open the file once for every range
    let mut file = std::fs::File::open(path)
        .map_err(|err| format!("failed to open '{}': {err}", path.display()))?;
    let file_size = file
        .metadata()
        .map_err(|err| format!("failed to stat '{}': {err}", path.display()))?
        .len();
    // no ranges means the whole file, now that its size is known
    let ranges = if ranges.is_empty() {
        vec![(0, file_size)]
    } else {
        ranges.to_vec()
    };
    // one hasher across every range unless each range digests alone
    let mut combined = (!per_range).then(|| AnyHasher::new(algo));
    let mut out_ranges = Vec::with_capacity(ranges.len());
    let mut short_reads = Vec::new();
    let mut chunk = vec![0u8; DIGEST_CHUNK];
    for (offset, length) in &ranges {
        // a per-range digest gets its own hasher
        let mut own = per_range.then(|| AnyHasher::new(algo));
        let mut read_total = 0u64;
        // seek to the range start when any of it exists
        if *offset < file_size {
            file.seek(SeekFrom::Start(*offset))
                .map_err(|err| format!("failed to seek '{}': {err}", path.display()))?;
            // stream the range through in bounded chunks
            let mut remaining = (*length).min(file_size - offset);
            while remaining > 0 {
                let want =
                    usize::try_from(remaining.min(DIGEST_CHUNK as u64)).unwrap_or(DIGEST_CHUNK);
                let read = file
                    .read(&mut chunk[..want])
                    .map_err(|err| format!("failed to read '{}': {err}", path.display()))?;
                if read == 0 {
                    break;
                }
                // feed whichever hasher owns this range's bytes
                if let Some(hasher) = combined.as_mut() {
                    hasher.update(&chunk[..read]);
                }
                if let Some(hasher) = own.as_mut() {
                    hasher.update(&chunk[..read]);
                }
                read_total += read as u64;
                remaining -= read as u64;
            }
        }
        // record a short read so the digest's coverage is explicit
        if read_total < *length {
            short_reads.push(format!(
                "range {offset:#x}+{length} read only {read_total} bytes (EOF)"
            ));
        }
        out_ranges.push(RangeDigest {
            offset: format!("{offset:#x}"),
            length: *length,
            read: read_total,
            digest: own.map(AnyHasher::finish),
        });
    }
    Ok(DigestResult {
        algo: algo.label(),
        file_size,
        digest: combined.map(AnyHasher::finish),
        ranges: out_ranges,
        note: (!short_reads.is_empty()).then(|| short_reads.join("; ")),
    })
}

/// Digests a file (or byte ranges of it) with MD5/SHA-1/SHA-256
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `algo` - The algorithm: md5, sha1, or sha256
/// * `ranges` - `[offset, length]` pairs to hash (default: the whole file)
/// * `per_range` - Digest each range on its own instead of concatenated
/// * `target` - The file to hash, or `None`/`""` for the original sample
#[thorium_derive::thorpal_tool(
    group = "decode",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_digest",
    rhai_doc = "digest(data: blob, algo: string) -> string -- hash a blob to lowercase hex",
    rhai_reference = "Hashes a blob in memory and returns the digest as a lowercase hex
string. `algo` is `\"md5\"`, `\"sha1\"`, or `\"sha256\"`.

NEVER hand-write a hash function in Rhai -- this native binding runs the
real implementations."
)]
#[rig::tool_macro(
    description = "Digest a file -- or explicit [offset, length] byte ranges of it -- with md5, sha1, or sha256, streaming so size never matters; never hand-write a hash algorithm in Rhai, use this native tool. With no `ranges` the whole file is hashed (the ORIGINAL sample by default, or an extracted file via `target`). With `ranges`, the ranges' bytes are concatenated in order into one digest -- e.g. hash a PE's sections while skipping a patched region -- or digested one-by-one with `per_range: true` (up to 64 ranges). A range reaching past EOF is hashed short and reported, never silently padded."
)]
pub async fn digest(
    #[rig(context)] context: &mut ToolContext,
    algo: String,
    ranges: Option<Vec<Vec<u64>>>,
    per_range: Option<bool>,
    target: Option<String>,
) -> Result<DigestResult, ToolExecutionError> {
    // parse the algorithm before touching the disk
    let algo = DigestAlgo::parse(&algo).map_err(ToolExecutionError::invalid_args)?;
    // resolve the file to hash
    let path = resolve_target(context, target.as_deref()).await?;
    // parse the ranges; the empty list means the whole file to the sync core
    let ranges: Vec<(u64, u64)> = match ranges {
        None => Vec::new(),
        Some(ranges) => {
            if ranges.is_empty() {
                return Err(ToolExecutionError::invalid_args(
                    "`ranges` must hold at least one [offset, length] pair".to_owned(),
                ));
            }
            if ranges.len() > MAX_DIGEST_RANGES {
                return Err(ToolExecutionError::invalid_args(format!(
                    "`ranges` holds {} ranges; at most {MAX_DIGEST_RANGES} hash per call",
                    ranges.len()
                )));
            }
            ranges
                .iter()
                .map(|range| match range.as_slice() {
                    [offset, length] if *length > 0 => Ok((*offset, *length)),
                    [_, _] => Err("range lengths must be positive".to_owned()),
                    _ => Err("each range must be an [offset, length] pair".to_owned()),
                })
                .collect::<Result<_, _>>()
                .map_err(ToolExecutionError::invalid_args)?
        }
    };
    let per_range = per_range.unwrap_or(false);
    // stream the hash off a blocking thread
    tokio::task::spawn_blocking(move || digest_ranges_sync(&path, algo, &ranges, per_range))
        .await
        .map_err(|err| ToolExecutionError::invalid_args(format!("the digest task failed: {err}")))?
        .map_err(ToolExecutionError::invalid_args)
}

#[cfg(test)]
mod tests;

// ===========================================================================
// scan_refs: byte-level inbound-reference scan
// ===========================================================================

/// The most reference hits one scan returns.
const MAX_REF_HITS: usize = 500;

/// The default reference-hit cap.
const DEFAULT_REF_HITS: usize = 100;

/// The most file bytes one reference scan reads.
const REF_SCAN_CAP: usize = 32 * 1024 * 1024;

/// The widest target range a reference scan accepts, in bytes.
const MAX_REF_RANGE: u64 = 16 * 1024 * 1024;

/// One inbound reference to the target address range
#[derive(Debug, Clone, serde::Serialize)]
pub struct RefHit {
    /// The reference's encoding: `call_rel32`, `jmp_rel32`, `imm32`, or `imm64`
    pub kind: String,
    /// The virtual address of the referencing instruction or slot (hex)
    pub va: String,
    /// The file offset of the referencing bytes (hex)
    pub file_offset: String,
    /// The section the reference lives in (`flat` for headerless scans)
    pub section: String,
    /// The virtual address the reference resolves to, inside the target (hex)
    pub target: String,
    /// A short hexdump around the referencing bytes
    pub context: String,
}

/// The result of a byte-level inbound-reference scan
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScanRefsResult {
    /// The references found, in file order
    pub hits: Vec<RefHit>,
    /// The resolved target range the scan looked for, as `start..end` hex
    pub target_range: String,
    /// How the file's bytes were mapped: `pe` or `flat @ <image_base>`
    pub mapping: String,
    /// The regions scanned, as `name (file_off..end -> va)` lines
    pub regions: Vec<String>,
    /// A note when the hit cap or the read cap truncated the scan
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// One mapped region of the scanned file
struct RefRegion {
    /// The region's display name (a PE section name, or `flat`)
    name: String,
    /// The file offset the region's bytes start at
    file_offset: usize,
    /// The number of file bytes the region carries
    len: usize,
    /// The virtual address the region maps to
    va: u64,
}

/// Parse a PE's image base and mapped regions out of raw file bytes
///
/// A deliberately minimal reader (mirroring `detect::fileinfo`'s walk): DOS
/// `e_lfanew`, `PE\0\0` magic, PE32/PE32+ discrimination for the image-base
/// slot, then the section table's name/VA/raw-pointer/raw-size columns.
/// Returns `None` for anything that is not a parseable PE, which sends the
/// scan down the flat-mapping path instead.
///
/// # Arguments
///
/// * `raw` - The file bytes to parse
fn pe_regions(raw: &[u8]) -> Option<(u64, Vec<RefRegion>)> {
    // little helpers over the raw bytes; any short read fails the parse
    let u16_at = |off: usize| -> Option<u16> {
        raw.get(off..off + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |off: usize| -> Option<u32> {
        raw.get(off..off + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let u64_at = |off: usize| -> Option<u64> {
        raw.get(off..off + 8)
            .map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    };
    // DOS header magic plus e_lfanew into the PE signature
    if raw.get(..2) != Some(b"MZ".as_slice()) {
        return None;
    }
    let pe = u32_at(0x3c)? as usize;
    if u32_at(pe)? != 0x0000_4550 {
        return None;
    }
    // COFF header: section count and optional-header size
    let coff = pe.checked_add(4)?;
    let section_count = u16_at(coff + 2)? as usize;
    let opt_size = u16_at(coff + 16)? as usize;
    let opt = coff.checked_add(20)?;
    // the optional-header magic picks the image-base slot's offset and width
    let image_base = match u16_at(opt)? {
        0x10b => u64::from(u32_at(opt + 28)?),
        0x20b => u64_at(opt + 24)?,
        _ => return None,
    };
    // walk the section table (40-byte entries) into scannable regions
    let table = opt.checked_add(opt_size)?;
    let mut regions = Vec::new();
    for i in 0..section_count.min(96) {
        let entry = table + i * 40;
        let name_bytes = raw.get(entry..entry + 8)?;
        let name = String::from_utf8_lossy(name_bytes)
            .trim_end_matches('\0')
            .to_string();
        let va = u64::from(u32_at(entry + 12)?);
        let raw_size = u32_at(entry + 16)? as usize;
        let raw_off = u32_at(entry + 20)? as usize;
        // only sections with on-disk bytes can carry scannable references
        if raw_size == 0 || raw_off >= raw.len() {
            continue;
        }
        regions.push(RefRegion {
            name: if name.is_empty() {
                format!("sect{i}")
            } else {
                name
            },
            file_offset: raw_off,
            len: raw_size.min(raw.len() - raw_off),
            va: image_base + va,
        });
    }
    (!regions.is_empty()).then_some((image_base, regions))
}

/// Scan one mapped region's bytes for references into the target range
///
/// # Arguments
///
/// * `raw` - The whole file's bytes
/// * `region` - The mapped region to scan
/// * `target` - The target VA range as `start..end` (half-open)
/// * `wide` - Whether to also match 8-byte absolute slots (PE32+)
/// * `hits` - The hit accumulator, capped by the caller's limit
/// * `limit` - The most hits to accumulate overall
fn scan_region(
    raw: &[u8],
    region: &RefRegion,
    target: (u64, u64),
    wide: bool,
    hits: &mut Vec<RefHit>,
    limit: usize,
) {
    let bytes = &raw[region.file_offset..region.file_offset + region.len];
    let in_target = |va: u64| va >= target.0 && va < target.1;
    // a short context window around each referencing site
    let context = |at: usize| -> String {
        let lo = at.saturating_sub(8);
        let hi = (at + 12).min(bytes.len());
        crate::thorpal::tools::file::hexdump(&bytes[lo..hi], region.va + lo as u64)
    };
    for i in 0..bytes.len() {
        if hits.len() >= limit {
            return;
        }
        let va_here = region.va + i as u64;
        // E8/E9: call/jmp rel32, resolved from the end of the instruction
        if i + 5 <= bytes.len() && (bytes[i] == 0xE8 || bytes[i] == 0xE9) {
            let rel = i32::from_le_bytes([bytes[i + 1], bytes[i + 2], bytes[i + 3], bytes[i + 4]]);
            let dst = (va_here + 5).wrapping_add_signed(i64::from(rel));
            if in_target(dst) {
                hits.push(RefHit {
                    kind: if bytes[i] == 0xE8 {
                        "call_rel32"
                    } else {
                        "jmp_rel32"
                    }
                    .to_string(),
                    va: format!("{va_here:#x}"),
                    file_offset: format!("{:#x}", region.file_offset + i),
                    section: region.name.clone(),
                    target: format!("{dst:#x}"),
                    context: context(i),
                });
                continue;
            }
        }
        // 4-byte absolute slot: an immediate, pointer-table entry, or operand
        if i + 4 <= bytes.len() {
            let v = u64::from(u32::from_le_bytes([
                bytes[i],
                bytes[i + 1],
                bytes[i + 2],
                bytes[i + 3],
            ]));
            if in_target(v) {
                hits.push(RefHit {
                    kind: "imm32".to_string(),
                    va: format!("{va_here:#x}"),
                    file_offset: format!("{:#x}", region.file_offset + i),
                    section: region.name.clone(),
                    target: format!("{v:#x}"),
                    context: context(i),
                });
                continue;
            }
        }
        // 8-byte absolute slot, for PE32+ pointer tables
        if wide && i + 8 <= bytes.len() {
            let v = u64::from_le_bytes([
                bytes[i],
                bytes[i + 1],
                bytes[i + 2],
                bytes[i + 3],
                bytes[i + 4],
                bytes[i + 5],
                bytes[i + 6],
                bytes[i + 7],
            ]);
            if in_target(v) {
                hits.push(RefHit {
                    kind: "imm64".to_string(),
                    va: format!("{va_here:#x}"),
                    file_offset: format!("{:#x}", region.file_offset + i),
                    section: region.name.clone(),
                    target: format!("{v:#x}"),
                    context: context(i),
                });
            }
        }
    }
}

/// Parse a hex-or-decimal address argument
///
/// # Arguments
///
/// * `text` - The address text (`0x`-prefixed hex, bare hex, or decimal)
fn parse_va(text: &str) -> Result<u64, String> {
    let t = text.trim();
    let parsed = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16)
    } else if t.chars().all(|c| c.is_ascii_digit()) {
        t.parse::<u64>()
    } else {
        u64::from_str_radix(t, 16)
    };
    parsed.map_err(|err| format!("'{text}' is not a hex or decimal address: {err}"))
}

/// Scan file bytes for inbound references to a virtual-address range
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `target_va` - The target virtual address (hex or decimal)
/// * `range_length` - Widen the target to `[va, va+range_length)` (default 1)
/// * `image_base` - The flat-mapping base for headerless/carved inputs
/// * `target` - The file to scan, or `None`/`""` for the original sample
/// * `limit` - The most hits to return (default 100, max 500)
#[thorium_derive::thorpal_tool(group = "decode")]
#[rig::tool_macro(
    description = "Find WHO REFERENCES a virtual address by scanning raw file bytes for the reference encodings themselves -- E8/E9 call/jmp rel32 (resolved per-site), 4-byte absolute slots (push/mov immediates and pointer-table entries, e.g. CRT initializer/atexit tables), and 8-byte slots on PE32+ -- independent of any decompiler's model, so it works where Ghidra xrefs come back empty: carved payloads, data-section pointer tables, and code the auto-analyzer never typed. PEs map through their section table automatically; anything else scans flat with `image_base` (default 0; set it to the base you loaded a carve at). `target_va` is hex or decimal; `range_length` widens the target to a half-open range (e.g. a whole function or table). Scans the ORIGINAL sample or an extracted file (`target`); each hit reports kind, the referencing site's VA/file offset/section, the resolved target, and a short hexdump. Expect occasional coincidental 4-byte matches on small addresses -- corroborate imm32 hits before concluding."
)]
pub async fn scan_refs(
    #[rig(context)] context: &mut ToolContext,
    target_va: String,
    range_length: Option<u64>,
    image_base: Option<String>,
    target: Option<String>,
    limit: Option<usize>,
) -> Result<ScanRefsResult, ToolExecutionError> {
    // resolve the target range up front so arg errors are instant
    let start = parse_va(&target_va).map_err(ToolExecutionError::invalid_args)?;
    let span = range_length.unwrap_or(1).clamp(1, MAX_REF_RANGE);
    let end = start.saturating_add(span);
    let flat_base = match image_base.as_deref() {
        Some(text) => parse_va(text).map_err(ToolExecutionError::invalid_args)?,
        None => 0,
    };
    let limit = limit.unwrap_or(DEFAULT_REF_HITS).clamp(1, MAX_REF_HITS);
    // read the file head up to the scan cap
    let path = resolve_target(context, target.as_deref()).await?;
    let (raw, file_size) = read_input(path, 0, REF_SCAN_CAP)
        .await
        .map_err(ToolExecutionError::invalid_args)?;
    // scan on a blocking thread: tens of MiB of per-byte work is real CPU
    let (hits, mapping, regions, read_truncated) = tokio::task::spawn_blocking(move || {
        // a parseable PE maps by section; everything else scans flat
        let (mapping, wide, regions) = match pe_regions(&raw) {
            Some((base, regions)) => {
                // PE32+ bases past 4 GiB imply 8-byte pointer slots too
                (
                    format!("pe @ {base:#x}"),
                    base > u64::from(u32::MAX) || end > u64::from(u32::MAX),
                    regions,
                )
            }
            None => (
                format!("flat @ {flat_base:#x}"),
                end > u64::from(u32::MAX),
                vec![RefRegion {
                    name: "flat".to_string(),
                    file_offset: 0,
                    len: raw.len(),
                    va: flat_base,
                }],
            ),
        };
        let mut hits = Vec::new();
        for region in &regions {
            scan_region(&raw, region, (start, end), wide, &mut hits, limit);
        }
        let described: Vec<String> = regions
            .iter()
            .map(|r| {
                format!(
                    "{} ({:#x}..{:#x} -> {:#x})",
                    r.name,
                    r.file_offset,
                    r.file_offset + r.len,
                    r.va
                )
            })
            .collect();
        (hits, mapping, described, raw.len())
    })
    .await
    .map_err(|err| ToolExecutionError::invalid_args(format!("the scan task failed: {err}")))?;
    // note truncation by either the hit cap or the read cap
    let mut notes = Vec::new();
    if hits.len() >= limit {
        notes.push(format!(
            "hit cap of {limit} reached; narrow the target range"
        ));
    }
    if (read_truncated as u64) < file_size {
        notes.push(format!(
            "scanned the first {read_truncated} of {file_size} bytes (read cap)"
        ));
    }
    Ok(ScanRefsResult {
        hits,
        target_range: format!("{start:#x}..{end:#x}"),
        mapping,
        regions,
        truncated: (!notes.is_empty()).then(|| notes.join("; ")),
    })
}

#[cfg(test)]
mod scan_refs_tests {
    use super::*;

    /// Flat-mode scans resolve rel32 and absolute slots against the base
    #[test]
    fn flat_scan_finds_rel32_and_imm32() {
        // a flat image at base 0x400000 with three planted references to
        // 0x401000: E8 (call rel32), E9 (jmp rel32), and an absolute dword
        let base = 0x40_0000u64;
        let mut raw = vec![0x90u8; 0x60];
        // call at +0x10: dst = base+0x10+5+rel = 0x401000 -> rel = 0xFEB
        raw[0x10] = 0xE8;
        raw[0x11..0x15].copy_from_slice(&0xFEBi32.to_le_bytes());
        // jmp at +0x20: rel = 0x401000 - (base+0x20+5) = 0xFDB
        raw[0x20] = 0xE9;
        raw[0x21..0x25].copy_from_slice(&0xFDBi32.to_le_bytes());
        // absolute pointer-table slot at +0x30
        raw[0x30..0x34].copy_from_slice(&0x40_1000u32.to_le_bytes());
        let region = RefRegion {
            name: "flat".to_string(),
            file_offset: 0,
            len: raw.len(),
            va: base,
        };
        let mut hits = Vec::new();
        scan_region(&raw, &region, (0x40_1000, 0x40_1001), false, &mut hits, 100);
        let kinds: Vec<&str> = hits.iter().map(|h| h.kind.as_str()).collect();
        assert_eq!(kinds, ["call_rel32", "jmp_rel32", "imm32"]);
        assert_eq!(hits[0].va, "0x400010");
        assert_eq!(hits[1].va, "0x400020");
        assert_eq!(hits[2].va, "0x400030");
        // every hit resolved into the target
        assert!(hits.iter().all(|h| h.target == "0x401000"));
        // a scan for a different target finds nothing
        let mut none = Vec::new();
        scan_region(&raw, &region, (0x50_0000, 0x50_0001), false, &mut none, 100);
        assert!(none.is_empty());
    }

    /// A minimal PE32 maps its section and the image base drives VA math
    #[test]
    fn pe_mapping_resolves_section_references() {
        // build the smallest PE32 this parser accepts: DOS stub, PE sig,
        // COFF (1 section), PE32 optional header, one .text section whose
        // raw bytes hold a call into the section's own VA range
        let mut raw = vec![0u8; 0x400];
        raw[0..2].copy_from_slice(b"MZ");
        raw[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        raw[0x80..0x84].copy_from_slice(&0x0000_4550u32.to_le_bytes());
        let coff = 0x84;
        raw[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        raw[coff + 16..coff + 18].copy_from_slice(&0xE0u16.to_le_bytes());
        let opt = coff + 20;
        raw[opt..opt + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        raw[opt + 28..opt + 32].copy_from_slice(&0x40_0000u32.to_le_bytes());
        let table = opt + 0xE0;
        raw[table..table + 5].copy_from_slice(b".text");
        raw[table + 12..table + 16].copy_from_slice(&0x1000u32.to_le_bytes());
        raw[table + 16..table + 20].copy_from_slice(&0x40u32.to_le_bytes());
        raw[table + 20..table + 24].copy_from_slice(&0x200u32.to_le_bytes());
        // the section's bytes: a call at VA 0x401000 to VA 0x401010
        raw[0x200] = 0xE8;
        raw[0x201..0x205].copy_from_slice(&0xBi32.to_le_bytes());
        let (base, regions) = pe_regions(&raw).expect("a parseable PE");
        assert_eq!(base, 0x40_0000);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].name, ".text");
        assert_eq!(regions[0].va, 0x40_1000);
        let mut hits = Vec::new();
        scan_region(
            &raw,
            &regions[0],
            (0x40_1010, 0x40_1011),
            false,
            &mut hits,
            100,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, "call_rel32");
        assert_eq!(hits[0].va, "0x401000");
        assert_eq!(hits[0].section, ".text");
        // junk that is not a PE falls back to the flat path
        assert!(pe_regions(b"not a pe at all").is_none());
    }
}

// ===========================================================================
// entropy_map: per-region entropy mapping
// ===========================================================================

/// The default entropy window size, in bytes.
const ENTROPY_WINDOW: usize = 4096;

/// The default hot-region entropy threshold.
const ENTROPY_HOT: f64 = 7.5;

/// The most hot regions one map returns.
const MAX_HOT_REGIONS: usize = 100;

/// One contiguous high-entropy region of the mapped file
#[derive(Debug, Clone, serde::Serialize)]
pub struct HotRegion {
    /// The region (PE section, `overlay`, or `flat`) the bytes live in
    pub section: String,
    /// The file offset the hot run starts at (hex)
    pub file_offset: String,
    /// The virtual address the hot run maps to, when the file maps (hex)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub va: Option<String>,
    /// The hot run's length in bytes
    pub length: usize,
    /// The hot run's measured entropy (bits/byte)
    pub entropy: f64,
}

/// One mapped region's whole-range entropy summary
#[derive(Debug, Clone, serde::Serialize)]
pub struct RegionEntropy {
    /// The region's name (a PE section name, `overlay`, or `flat`)
    pub section: String,
    /// The region's file range as `start..end` hex
    pub range: String,
    /// The region's whole-range entropy (bits/byte)
    pub entropy: f64,
}

/// The result of an entropy map over one file
#[derive(Debug, Clone, serde::Serialize)]
pub struct EntropyMapResult {
    /// The whole file's entropy (bits/byte) — averages cargo away; use the
    /// regions and hot runs for verdict-grade evidence
    pub file_entropy: f64,
    /// Each mapped region's whole-range entropy
    pub regions: Vec<RegionEntropy>,
    /// Contiguous windows at or above the threshold, strongest first
    pub hot_regions: Vec<HotRegion>,
    /// The window size the map scanned with
    pub window: usize,
    /// The hot threshold the map applied
    pub threshold: f64,
    /// A note when the hot cap or the read cap truncated the map
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// Shannon entropy of a byte slice, in bits per byte
///
/// # Arguments
///
/// * `bytes` - The bytes to measure
fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    // one pass to the byte histogram, then the usual -sum(p log2 p)
    let mut counts = [0u64; 256];
    for byte in bytes {
        counts[*byte as usize] += 1;
    }
    let total = bytes.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = *count as f64 / total;
            -p * p.log2()
        })
        .sum()
}

/// Collect a region's hot windows into merged contiguous runs
///
/// # Arguments
///
/// * `raw` - The whole file's bytes
/// * `region` - The mapped region to scan
/// * `mapped` - Whether the region's VA is meaningful (a parsed PE)
/// * `window` - The scan window size
/// * `threshold` - The hot entropy threshold
/// * `hot` - The hot-run accumulator
fn hot_runs(
    raw: &[u8],
    region: &RefRegion,
    mapped: bool,
    window: usize,
    threshold: f64,
    hot: &mut Vec<HotRegion>,
) {
    let bytes = &raw[region.file_offset..region.file_offset + region.len];
    let mut run_start: Option<usize> = None;
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + window).min(bytes.len());
        let is_hot = shannon_entropy(&bytes[offset..end]) >= threshold;
        match (is_hot, run_start) {
            // a hot window extends or starts the current run
            (true, None) => run_start = Some(offset),
            (true, Some(_)) => {}
            // a cold window closes any open run
            (false, Some(start)) => {
                push_hot_run(bytes, region, mapped, start, offset, hot);
                run_start = None;
            }
            (false, None) => {}
        }
        offset = end;
    }
    // close a run that reaches the region's end
    if let Some(start) = run_start {
        push_hot_run(bytes, region, mapped, start, bytes.len(), hot);
    }
}

/// Push one closed hot run with its measured entropy
///
/// # Arguments
///
/// * `bytes` - The region's bytes
/// * `region` - The mapped region the run lives in
/// * `mapped` - Whether the region's VA is meaningful
/// * `start` - The run's start, region-relative
/// * `end` - The run's end, region-relative
/// * `hot` - The hot-run accumulator
fn push_hot_run(
    bytes: &[u8],
    region: &RefRegion,
    mapped: bool,
    start: usize,
    end: usize,
    hot: &mut Vec<HotRegion>,
) {
    hot.push(HotRegion {
        section: region.name.clone(),
        file_offset: format!("{:#x}", region.file_offset + start),
        va: mapped.then(|| format!("{:#x}", region.va + start as u64)),
        length: end - start,
        entropy: shannon_entropy(&bytes[start..end]),
    });
}

/// Map a file's entropy per region and surface its high-entropy runs
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `target` - The file to map, or `None`/`""` for the original sample
/// * `window` - The scan window size in bytes (default 4096)
/// * `threshold` - The hot threshold in bits/byte (default 7.5)
#[thorium_derive::thorpal_tool(group = "decode")]
#[rig::tool_macro(
    description = "Map a file's entropy PER REGION — each PE section, the overlay, and sliding windows inside them — and return every contiguous high-entropy run with its offsets, section, and VA. Whole-file entropy AVERAGES CARGO AWAY: a 38 KB encrypted payload hiding in a resource reads as unremarkable at file level while this map pins it to its exact range. Use it before any inert/decoy/benign verdict: every hot region (default threshold 7.5 bits/byte) must be accounted for — decoded, consumer-traced (scan_refs), or explicitly flagged as unexplained carried data. Non-PE and carved inputs map flat; `window` (default 4096) and `threshold` tune the scan; scans the ORIGINAL sample or an extracted file (`target`)."
)]
pub async fn entropy_map(
    #[rig(context)] context: &mut ToolContext,
    target: Option<String>,
    window: Option<usize>,
    threshold: Option<f64>,
) -> Result<EntropyMapResult, ToolExecutionError> {
    let window = window.unwrap_or(ENTROPY_WINDOW).clamp(256, 65536);
    let threshold = threshold.unwrap_or(ENTROPY_HOT).clamp(1.0, 8.0);
    // read the file head up to the scan cap
    let path = resolve_target(context, target.as_deref()).await?;
    let (raw, file_size) = read_input(path, 0, REF_SCAN_CAP)
        .await
        .map_err(ToolExecutionError::invalid_args)?;
    // the histogram passes are real CPU work on a blocking thread
    let (file_entropy, regions, mut hot, read_len) = tokio::task::spawn_blocking(move || {
        // map by PE sections (plus any overlay) or flat for everything else
        let (mapped, mut mapped_regions) = match pe_regions(&raw) {
            Some((_base, regions)) => (true, regions),
            None => (
                false,
                vec![RefRegion {
                    name: "flat".to_string(),
                    file_offset: 0,
                    len: raw.len(),
                    va: 0,
                }],
            ),
        };
        // bytes past the last mapped section are the overlay
        if mapped {
            let end = mapped_regions
                .iter()
                .map(|r| r.file_offset + r.len)
                .max()
                .unwrap_or(0);
            if end < raw.len() {
                mapped_regions.push(RefRegion {
                    name: "overlay".to_string(),
                    file_offset: end,
                    len: raw.len() - end,
                    va: 0,
                });
            }
        }
        let file_entropy = shannon_entropy(&raw);
        let mut summaries = Vec::new();
        let mut hot = Vec::new();
        for region in &mapped_regions {
            let bytes = &raw[region.file_offset..region.file_offset + region.len];
            summaries.push(RegionEntropy {
                section: region.name.clone(),
                range: format!(
                    "{:#x}..{:#x}",
                    region.file_offset,
                    region.file_offset + region.len
                ),
                entropy: shannon_entropy(bytes),
            });
            // the overlay maps to no VA even inside a parsed PE
            let has_va = mapped && region.name != "overlay";
            hot_runs(&raw, region, has_va, window, threshold, &mut hot);
        }
        (file_entropy, summaries, hot, raw.len())
    })
    .await
    .map_err(|err| ToolExecutionError::invalid_args(format!("the entropy task failed: {err}")))?;
    // strongest runs first, capped for the model
    hot.sort_by(|a, b| {
        b.entropy
            .partial_cmp(&a.entropy)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut notes = Vec::new();
    if hot.len() > MAX_HOT_REGIONS {
        notes.push(format!(
            "{} hot regions found; returning the strongest {MAX_HOT_REGIONS}",
            hot.len()
        ));
        hot.truncate(MAX_HOT_REGIONS);
    }
    if (read_len as u64) < file_size {
        notes.push(format!(
            "mapped the first {read_len} of {file_size} bytes (read cap)"
        ));
    }
    Ok(EntropyMapResult {
        file_entropy,
        regions,
        hot_regions: hot,
        window,
        threshold,
        truncated: (!notes.is_empty()).then(|| notes.join("; ")),
    })
}

#[cfg(test)]
mod entropy_map_tests {
    use super::*;

    /// A random block inside zeros maps to one hot run at the right offsets
    #[test]
    fn hot_runs_pin_the_cargo() {
        // 64 KiB of zeros with an 8 KiB pseudo-random block at 16 KiB
        let mut raw = vec![0u8; 64 * 1024];
        let mut state = 0x1234_5678u32;
        for byte in raw[16 * 1024..24 * 1024].iter_mut() {
            // a tiny LCG is random enough to saturate the byte histogram
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            *byte = (state >> 16) as u8;
        }
        let region = RefRegion {
            name: "flat".to_string(),
            file_offset: 0,
            len: raw.len(),
            va: 0,
        };
        let mut hot = Vec::new();
        hot_runs(&raw, &region, false, 4096, 7.5, &mut hot);
        // exactly one merged hot run covering the planted block
        assert_eq!(hot.len(), 1);
        assert_eq!(hot[0].file_offset, "0x4000");
        assert_eq!(hot[0].length, 8 * 1024);
        assert!(hot[0].entropy > 7.5);
        assert!(hot[0].va.is_none());
        // the whole-file average hides what the map pins down
        assert!(shannon_entropy(&raw) < 2.0);
        // zeros alone are never hot
        let mut none = Vec::new();
        let cold = RefRegion {
            name: "flat".to_string(),
            file_offset: 0,
            len: 16 * 1024,
            va: 0,
        };
        hot_runs(&raw, &cold, false, 4096, 7.5, &mut none);
        assert!(none.is_empty());
    }
}

// ===========================================================================
// stack_stores: reconstruct stack-built buffers from store-immediate runs
// ===========================================================================

/// The fewest recognized stores a run must have to be emitted.
const STACK_MIN_STORES: usize = 8;

/// The most reconstructed buffers one scan returns.
const MAX_STACK_BUFFERS: usize = 64;

/// The widest instruction gap (bytes) that keeps a store run alive.
const STACK_RUN_GAP: usize = 48;

/// One reconstructed stack-built buffer
#[derive(Debug, Clone, serde::Serialize)]
pub struct StackBuffer {
    /// The section the building code lives in (`flat` for headerless scans)
    pub section: String,
    /// The virtual address of the run's first store (hex)
    pub va: String,
    /// The file offset of the run's first store (hex)
    pub file_offset: String,
    /// The frame base the stores write through: `ebp` or `esp`
    pub base: String,
    /// How many stores the run folded in (immediate plus register-cached)
    pub stores: usize,
    /// The reconstructed buffer's length in bytes (displacement span)
    pub length: usize,
    /// The fraction of the span the stores actually covered (0-1)
    pub coverage: f64,
    /// The reconstructed bytes' entropy (bits/byte)
    pub entropy: f64,
    /// Printable runs recovered from the reconstruction, capped
    pub strings: Vec<String>,
    /// A short hexdump of the reconstruction's head
    pub head: String,
    /// The saved workspace file, when `save_prefix` was passed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved: Option<String>,
}

/// The result of a stack-store scan
#[derive(Debug, Clone, serde::Serialize)]
pub struct StackStoresResult {
    /// The reconstructed buffers, largest first
    pub buffers: Vec<StackBuffer>,
    /// How the file's bytes were mapped: `pe` or `flat`
    pub mapping: String,
    /// A note when a cap truncated the scan
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// One in-progress store run during the scan
struct StackRun {
    /// The region-relative offset of the run's first store
    start: usize,
    /// The region-relative offset just past the run's last store
    last_end: usize,
    /// The frame base register the run writes through (`true` = ebp)
    ebp_base: bool,
    /// The written bytes keyed by frame displacement
    writes: std::collections::BTreeMap<i64, u8>,
    /// How many stores folded in
    stores: usize,
}

/// Decode one store-ish instruction at `at`, folding it into `run`
///
/// Recognizes the stack-build idioms without a disassembler: direct
/// byte/dword immediate stores to `[ebp+disp]` / `[esp+disp]` (C6/C7 with
/// the /45,/85,/44 24,/84 24 forms), the register-cache idiom (`B0+r imm8`
/// then `88` register stores to the same frame forms), and `xor r8,r8`
/// zeroing. Returns the instruction length consumed, or `None` when the
/// bytes are not a recognized store.
///
/// # Arguments
///
/// * `bytes` - The region's bytes
/// * `at` - The offset to decode at
/// * `cache` - The per-register imm8 cache (`None` = unknown)
fn decode_stack_store(
    bytes: &[u8],
    at: usize,
    cache: &mut [Option<u8>; 8],
) -> Option<(usize, bool, i64, Vec<u8>)> {
    let b = &bytes[at..];
    // helpers over the modrm forms this scanner recognizes
    let disp8 = |x: u8| -> i64 { i64::from(x as i8) };
    let disp32 = |x: &[u8]| -> i64 { i64::from(i32::from_le_bytes([x[0], x[1], x[2], x[3]])) };
    match b {
        // mov byte [ebp+disp8], imm8
        [0xC6, 0x45, d, i, ..] => Some((4, true, disp8(*d), vec![*i])),
        // mov byte [ebp+disp32], imm8
        [0xC6, 0x85, ..] if b.len() >= 7 => Some((7, true, disp32(&b[2..6]), vec![b[6]])),
        // mov byte [esp+disp8], imm8 (SIB)
        [0xC6, 0x44, 0x24, d, i, ..] => Some((5, false, disp8(*d), vec![*i])),
        // mov dword [ebp+disp8], imm32
        [0xC7, 0x45, d, ..] if b.len() >= 7 => Some((7, true, disp8(*d), b[3..7].to_vec())),
        // mov dword [ebp+disp32], imm32
        [0xC7, 0x85, ..] if b.len() >= 10 => Some((10, true, disp32(&b[2..6]), b[6..10].to_vec())),
        // mov dword [esp+disp8], imm32 (SIB)
        [0xC7, 0x44, 0x24, d, ..] if b.len() >= 8 => Some((8, false, disp8(*d), b[4..8].to_vec())),
        // mov r8, imm8 caches a byte for the register-store idiom
        [op @ 0xB0..=0xB7, i, ..] => {
            cache[(op - 0xB0) as usize] = Some(*i);
            // a cache load is consumed but writes nothing itself
            Some((2, true, i64::MIN, Vec::new()))
        }
        // xor r8,r8 zeroes the cache (31 /r and 33 /r with reg == rm)
        [0x31 | 0x33, modrm, ..] if modrm >> 6 == 0b11 && (modrm >> 3) & 7 == modrm & 7 => {
            cache[(modrm & 7) as usize] = Some(0);
            Some((2, true, i64::MIN, Vec::new()))
        }
        // mov [ebp+disp8], r8 stores the cached byte
        [0x88, modrm @ 0x45..=0x7D, d, ..] if modrm & 0xC7 == 0x45 => {
            let reg = ((modrm >> 3) & 7) as usize;
            cache[reg].map(|value| (3, true, disp8(*d), vec![value]))
        }
        // mov [ebp+disp32], r8
        [0x88, modrm, ..] if modrm & 0xC7 == 0x85 && b.len() >= 6 => {
            let reg = ((modrm >> 3) & 7) as usize;
            cache[reg].map(|value| (6, true, disp32(&b[2..6]), vec![value]))
        }
        _ => None,
    }
}

/// Scan one region for stack-build runs and emit the reconstructions
///
/// # Arguments
///
/// * `raw` - The whole file's bytes
/// * `region` - The mapped region to scan
/// * `mapped` - Whether the region's VA is meaningful
/// * `min_stores` - The fewest stores an emitted run needs
/// * `out` - The reconstruction accumulator
fn scan_stack_region(
    raw: &[u8],
    region: &RefRegion,
    mapped: bool,
    min_stores: usize,
    out: &mut Vec<(StackBuffer, Vec<u8>)>,
) {
    let bytes = &raw[region.file_offset..region.file_offset + region.len];
    let mut cache: [Option<u8>; 8] = [None; 8];
    let mut runs: Vec<StackRun> = Vec::new();
    let mut offset = 0usize;
    let flush = |run: StackRun, out: &mut Vec<(StackBuffer, Vec<u8>)>| {
        // reconstruct the frame span the run covered
        if run.stores < min_stores || run.writes.is_empty() {
            return;
        }
        let lo = *run.writes.keys().next().unwrap();
        let hi = *run.writes.keys().next_back().unwrap();
        let length = usize::try_from(hi - lo + 1).unwrap_or(0);
        if length == 0 || length > 1 << 20 {
            return;
        }
        let mut buffer = vec![0u8; length];
        for (disp, value) in &run.writes {
            buffer[usize::try_from(disp - lo).unwrap()] = *value;
        }
        // printable runs, capped, so a stacked string is readable at a glance
        let mut strings: Vec<String> = Vec::new();
        let mut current = String::new();
        for byte in buffer.iter().chain(std::iter::once(&0)) {
            if (0x20..0x7f).contains(byte) {
                current.push(*byte as char);
            } else {
                if current.len() >= 4 && strings.len() < 8 {
                    strings.push(current.clone());
                }
                current.clear();
            }
        }
        out.push((
            StackBuffer {
                section: region.name.clone(),
                va: format!("{:#x}", region.va + run.start as u64),
                file_offset: format!("{:#x}", region.file_offset + run.start),
                base: if run.ebp_base { "ebp" } else { "esp" }.to_string(),
                stores: run.stores,
                length,
                coverage: run.writes.len() as f64 / length as f64,
                entropy: shannon_entropy(&buffer),
                strings,
                head: crate::thorpal::tools::file::hexdump(&buffer[..buffer.len().min(48)], 0),
                saved: None,
            },
            buffer,
        ));
        // VAs only mean something inside a mapped image
        if !mapped && let Some((last, _)) = out.last_mut() {
            last.va = "n/a".to_string();
        }
    };
    while offset < bytes.len() {
        match decode_stack_store(bytes, offset, &mut cache) {
            Some((len, ebp, disp, value)) => {
                // cache loads consume bytes but write nothing
                if !value.is_empty() {
                    // extend a live run on the same base, or start a new one
                    let extend = runs.last().is_some_and(|run| {
                        run.ebp_base == ebp && offset - run.last_end <= STACK_RUN_GAP
                    });
                    if !extend {
                        if let Some(done) = runs.pop() {
                            flush(done, out);
                        }
                        runs.push(StackRun {
                            start: offset,
                            last_end: offset + len,
                            ebp_base: ebp,
                            writes: std::collections::BTreeMap::new(),
                            stores: 0,
                        });
                    }
                    let run = runs.last_mut().unwrap();
                    for (i, byte) in value.iter().enumerate() {
                        run.writes.insert(disp + i as i64, *byte);
                    }
                    run.stores += 1;
                    run.last_end = offset + len;
                }
                offset += len;
            }
            None => {
                // a long gap past the window closes the live run
                if let Some(run) = runs.last()
                    && offset.saturating_sub(run.last_end) > STACK_RUN_GAP
                    && let Some(done) = runs.pop()
                {
                    flush(done, out);
                }
                offset += 1;
            }
        }
        if out.len() >= MAX_STACK_BUFFERS {
            return;
        }
    }
    if let Some(done) = runs.pop() {
        flush(done, out);
    }
}

/// Reconstruct stack-built buffers (keys, strings) from store-immediate runs
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `target` - The file to scan, or `None`/`""` for the original sample
/// * `min_stores` - The fewest stores an emitted run needs (default 8)
/// * `save_prefix` - Save each reconstruction into the extracted workspace
#[thorium_derive::thorpal_tool(group = "decode")]
#[rig::tool_macro(
    description = "Reconstruct STACK-BUILT buffers — keys and strings assembled at runtime by runs of mov-immediate stores to [ebp/esp+disp] — by scanning code bytes for the store idioms themselves (C6/C7 byte and dword immediate stores, plus the register-cache idiom: mov r8,imm8 / xor r8,r8 followed by mov [ebp+disp],r8). These values are INVISIBLE to every contiguous-byte search: one family carried a 512-byte log XOR key as ~484 single-byte stack stores that strings, grep_bytes, and x55/wide sweeps all missed, and the key's absence became a wrong claim. Each reconstruction reports its building code's VA/section, store count, displacement span, coverage, entropy, printable runs, and a head hexdump; pass `save_prefix` to land each buffer in the extracted workspace for decode_bytes/digest follow-ups. PE code sections map automatically; carves scan flat. Expect occasional coincidental runs — judge by coverage, entropy, and the strings preview."
)]
pub async fn stack_stores(
    #[rig(context)] context: &mut ToolContext,
    target: Option<String>,
    min_stores: Option<usize>,
    save_prefix: Option<String>,
) -> Result<StackStoresResult, ToolExecutionError> {
    let min_stores = min_stores.unwrap_or(STACK_MIN_STORES).clamp(3, 10_000);
    let path = resolve_target(context, target.as_deref()).await?;
    let (raw, file_size) = read_input(path, 0, REF_SCAN_CAP)
        .await
        .map_err(ToolExecutionError::invalid_args)?;
    // the per-byte decode walk is real CPU work on a blocking thread
    let (mut buffers, mapping, read_len) = tokio::task::spawn_blocking(move || {
        let (mapped, regions) = match pe_regions(&raw) {
            Some((base, regions)) => (true, {
                // only executable-ish sections build stacks; keep any section
                // with raw bytes but skip resources, which never hold code
                let _ = base;
                regions
                    .into_iter()
                    .filter(|r| r.name != ".rsrc")
                    .collect::<Vec<_>>()
            }),
            None => (
                false,
                vec![RefRegion {
                    name: "flat".to_string(),
                    file_offset: 0,
                    len: raw.len(),
                    va: 0,
                }],
            ),
        };
        let mut out = Vec::new();
        for region in &regions {
            scan_stack_region(&raw, region, mapped, min_stores, &mut out);
        }
        (
            out,
            if mapped { "pe" } else { "flat" }.to_string(),
            raw.len(),
        )
    })
    .await
    .map_err(|err| ToolExecutionError::invalid_args(format!("the scan task failed: {err}")))?;
    // largest reconstructions first; they are the key/string candidates
    buffers.sort_by(|a, b| b.0.length.cmp(&a.0.length));
    // land the top reconstructions in the workspace when the caller asked
    if let Some(prefix) = save_prefix {
        for (index, (entry, bytes)) in buffers.iter_mut().enumerate().take(8) {
            let name = format!("{prefix}_{index}");
            let saved = save_output(context, name, std::mem::take(bytes)).await?;
            entry.saved = Some(saved.name);
        }
    }
    let buffers: Vec<StackBuffer> = buffers.into_iter().map(|(entry, _)| entry).collect();
    let mut notes = Vec::new();
    if buffers.len() >= MAX_STACK_BUFFERS {
        notes.push(format!("buffer cap of {MAX_STACK_BUFFERS} reached"));
    }
    if (read_len as u64) < file_size {
        notes.push(format!(
            "scanned the first {read_len} of {file_size} bytes (read cap)"
        ));
    }
    Ok(StackStoresResult {
        buffers,
        mapping,
        truncated: (!notes.is_empty()).then(|| notes.join("; ")),
    })
}

#[cfg(test)]
mod stack_stores_tests {
    use super::*;

    /// Immediate and register-cached stores reconstruct one stacked buffer
    #[test]
    fn stacked_key_reconstructs_through_the_cache() {
        // hand-assembled x86: build 8 bytes at [ebp-0x10] via a dword store,
        // two byte stores, and the register-cache idiom (mov al / xor cl)
        let code: Vec<u8> = vec![
            // mov dword [ebp-0x10], 0x44434241  ("ABCD")
            0xC7, 0x45, 0xF0, 0x41, 0x42, 0x43, 0x44,
            // mov byte [ebp-0x0c], 0x45         ('E')
            0xC6, 0x45, 0xF4, 0x45, // mov al, 0x46 ; mov [ebp-0x0b], al ('F' via cache)
            0xB0, 0x46, 0x88, 0x45, 0xF5,
            // xor cl, cl ; mov [ebp-0x0a], cl   (zero via cache)
            0x31, 0xC9, 0x88, 0x4D, 0xF6, // mov byte [ebp-0x09], 0x47         ('G')
            0xC6, 0x45, 0xF7, 0x47, // trailing padding so the run flushes on the gap
            0x90, 0x90,
        ];
        let mut raw = code.clone();
        raw.extend(std::iter::repeat_n(0x90u8, 64));
        let region = RefRegion {
            name: "flat".to_string(),
            file_offset: 0,
            len: raw.len(),
            va: 0,
        };
        let mut out = Vec::new();
        scan_stack_region(&raw, &region, false, 3, &mut out);
        assert_eq!(out.len(), 1, "one run should flush");
        let (meta, bytes) = &out[0];
        // the span is [-0x10, -0x09] = 8 bytes, fully covered
        assert_eq!(meta.length, 8);
        assert_eq!(meta.stores, 5);
        assert!((meta.coverage - 1.0).abs() < 1e-9);
        assert_eq!(bytes, b"ABCDEF\0G");
        // the printable run surfaces for at-a-glance reading
        assert!(meta.strings.iter().any(|s| s == "ABCDEF"));
        assert_eq!(meta.base, "ebp");
        assert_eq!(meta.va, "n/a");
        // plain padding alone reconstructs nothing
        let mut none = Vec::new();
        let pad = RefRegion {
            name: "flat".to_string(),
            file_offset: code.len(),
            len: 64,
            va: 0,
        };
        scan_stack_region(&raw, &pad, false, 3, &mut none);
        assert!(none.is_empty());
    }
}
