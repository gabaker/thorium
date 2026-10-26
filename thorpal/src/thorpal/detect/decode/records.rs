//! Structural parsing of a recovered buffer into a configuration record.
//!
//! A decode that succeeds hands back a buffer, and the report shows it as an opaque
//! string. But the buffer a program bothered to hide is usually not prose — it is its
//! configuration, laid out in one of a handful of shapes that have been reinvented
//! independently for decades:
//!
//! * `key=value` pairs joined by a separator,
//! * fields joined by a single delimiter, positional rather than named,
//! * a length-prefixed sequence of strings,
//! * a fixed-stride table of NUL-padded strings.
//!
//! Recognizing the shape turns "here is 200 bytes of text" into a table of fields with
//! each one classified — which of them is the endpoint, which is the beacon interval,
//! which is the campaign identifier. That is the difference between an analyst reading
//! the blob and an analyst *using* it.
//!
//! # What keeps this from firing on prose
//!
//! Any text can be split on commas, so a shape match alone means nothing. A parse is
//! only accepted when the split produces a **consistent** tiling — every field
//! non-empty, every pair carrying a plausible identifier as its key, a length walk
//! that lands exactly on the end of the buffer, a stride that divides it exactly — and
//! when at least one field classifies as something a configuration would actually
//! hold: an endpoint, a URL, an address, a port, an interval, or a path. A sentence
//! with commas in it satisfies the first condition and fails the second.
//!
//! A failed parse is not a finding and not a score change; the caller falls back to
//! presenting the buffer as it already would.

use std::fmt::Write as _;

use crate::thorpal::detect::indicators;

/// The minimum `key=value` pairs for the pair grammar to be a record.
const MIN_PAIRS: usize = 2;

/// The minimum fields for a positional delimiter split to be a record.
const MIN_FIELDS: usize = 3;

/// The max fields parsed out of one record (bounds a runaway split).
const MAX_FIELDS: usize = 64;

/// The max length of a single field value; past this the "field" is really a blob.
const MAX_FIELD_LEN: usize = 512;

/// The max length of a `key=value` key before it stops looking like an identifier.
const MAX_KEY_LEN: usize = 32;

/// Separators between `key=value` pairs, most distinctive first.
const PAIR_SEPARATORS: &[u8] = &[b';', b'&', b'\n', b'|', b',', 0x00];

/// Characters that assign a value to a key.
const ASSIGNERS: &[u8] = b"=:";

/// Delimiters for a positional field split.
///
/// Deliberately excludes `:` and `/`, which appear inside the URLs and paths a config
/// is made of — splitting on those would shred the very fields worth reading.
const FIELD_DELIMITERS: &[u8] = &[b'|', b';', b'\n', b'\t', b',', 0x00, 0x01, 0x1f];

/// Strides tried for a fixed-width string table.
const STRIDES: &[usize] = &[8, 16, 24, 32, 48, 64, 96, 128, 256];

/// Key substrings that mark a numeric value as a network port.
const PORT_KEYS: &[&str] = &["port", "prt"];

/// Key substrings that mark a numeric value as a time interval.
const INTERVAL_KEYS: &[&str] = &[
    "interval", "sleep", "delay", "timeout", "beacon", "jitter", "wait", "period", "poll",
];

/// Values that read as a boolean switch.
const FLAG_VALUES: &[&str] = &[
    "true", "false", "yes", "no", "on", "off", "enabled", "disabled",
];

/// The layout a record was parsed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// `key=value` pairs joined by a separator.
    KeyValue {
        /// The byte separating one pair from the next.
        separator: u8,
        /// The byte assigning a value to its key.
        assigner: u8,
    },
    /// Positional fields joined by a single delimiter.
    Delimited {
        /// The byte separating one field from the next.
        delimiter: u8,
    },
    /// A sequence of strings, each preceded by its length.
    LengthPrefixed {
        /// The width in bytes of each length prefix (1 or 2, little-endian).
        width: usize,
    },
    /// A table of NUL-padded strings at a fixed stride.
    FixedStride {
        /// The stride in bytes.
        stride: usize,
    },
}

impl Shape {
    /// A one-line description of the layout for the report.
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::KeyValue {
                separator,
                assigner,
            } => format!(
                "`key{}value` pairs separated by {}",
                render_byte(assigner),
                render_byte(separator)
            ),
            Self::Delimited { delimiter } => {
                format!("positional fields separated by {}", render_byte(delimiter))
            }
            Self::LengthPrefixed { width } => {
                format!("length-prefixed sequence ({width}-byte little-endian lengths)")
            }
            Self::FixedStride { stride } => {
                format!("fixed {stride}-byte stride table of NUL-padded strings")
            }
        }
    }
}

/// Render a delimiter byte readably, since several of them are not printable.
///
/// # Arguments
///
/// * `byte` - The delimiter byte
fn render_byte(byte: u8) -> String {
    match byte {
        0x00 => "`NUL`".to_string(),
        b'\n' => "`\\n`".to_string(),
        b'\t' => "`\\t`".to_string(),
        b if b.is_ascii_graphic() => format!("`{}`", b as char),
        b => format!("`0x{b:02x}`"),
    }
}

/// What a parsed field's value looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// A full URL.
    Url,
    /// A `host:port` pair.
    Endpoint,
    /// A bare IPv4/IPv6 literal.
    IpAddress,
    /// A bare domain name.
    Domain,
    /// A network port number.
    Port,
    /// A time interval (beacon/sleep/timeout).
    Interval,
    /// A filesystem or URI path.
    Path,
    /// A boolean switch.
    Flag,
    /// A number with no further meaning attached.
    Number,
    /// Anything else — an identifier, a name, free text.
    Text,
}

impl FieldKind {
    /// The label this kind is rendered under.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Url => "URL",
            Self::Endpoint => "endpoint",
            Self::IpAddress => "IP address",
            Self::Domain => "domain",
            Self::Port => "port",
            Self::Interval => "interval",
            Self::Path => "path",
            Self::Flag => "flag",
            Self::Number => "number",
            Self::Text => "text",
        }
    }

    /// Whether this kind is something a configuration would hold — the evidence that
    /// makes a shape match believable rather than coincidental.
    #[must_use]
    pub fn is_notable(self) -> bool {
        matches!(
            self,
            Self::Url
                | Self::Endpoint
                | Self::IpAddress
                | Self::Domain
                | Self::Port
                | Self::Interval
                | Self::Path
        )
    }
}

/// One parsed field.
#[derive(Debug, Clone)]
pub struct Field {
    /// The field's name, when the layout names its fields.
    pub key: Option<String>,
    /// The field's value.
    pub value: String,
    /// What the value looks like.
    pub kind: FieldKind,
}

/// A configuration record parsed out of a recovered buffer.
#[derive(Debug, Clone)]
pub struct Record {
    /// The layout it was parsed under.
    pub shape: Shape,
    /// The fields, in the order they appear.
    pub fields: Vec<Field>,
}

impl Record {
    /// The fields whose kind is worth pivoting on.
    #[must_use]
    pub fn notable(&self) -> Vec<&Field> {
        self.fields
            .iter()
            .filter(|field| field.kind.is_notable())
            .collect()
    }

    /// Whether this parse is believable: a consistent tiling is not enough on its own,
    /// so at least one field has to look like configuration.
    fn is_credible(&self) -> bool {
        !self.notable().is_empty()
    }
}

/// Parse a recovered buffer as a configuration record.
///
/// Shapes are tried most-specific first: a named-pair grammar carries the most
/// structure, then a positional split, then the two binary layouts. Returns `None`
/// when no shape produces a credible parse — the caller then presents the buffer
/// exactly as it would have without this module.
///
/// # Arguments
///
/// * `bytes` - The recovered buffer
#[must_use]
pub fn parse(bytes: &[u8]) -> Option<Record> {
    // credibility is checked per shape, not once at the end, so a buffer that tiles
    // uninterestingly under one layout can still be read under the next
    [
        parse_key_value,
        parse_delimited,
        parse_length_prefixed,
        parse_fixed_stride,
    ]
    .into_iter()
    .find_map(|attempt| attempt(bytes).filter(Record::is_credible))
}

/// Try to read the buffer as `key=value` pairs.
///
/// Every token must be a pair with a plausible identifier key — one bare token is
/// enough to reject the split, because a real pair grammar does not have stragglers.
///
/// # Arguments
///
/// * `bytes` - The recovered buffer
fn parse_key_value(bytes: &[u8]) -> Option<Record> {
    for &separator in PAIR_SEPARATORS {
        let Some(tokens) = split_tokens(bytes, separator) else {
            continue;
        };
        if tokens.len() < MIN_PAIRS {
            continue;
        }
        for &assigner in ASSIGNERS {
            let mut fields = Vec::with_capacity(tokens.len());
            for token in &tokens {
                // every token must split into a plausible key and a non-empty value
                let Some((key, value)) = token.split_once(assigner as char) else {
                    fields.clear();
                    break;
                };
                let (key, value) = (key.trim(), value.trim());
                if !is_identifier(key) || value.is_empty() || value.len() > MAX_FIELD_LEN {
                    fields.clear();
                    break;
                }
                let kind = classify(Some(key), value);
                fields.push(Field {
                    key: Some(key.to_string()),
                    value: value.to_string(),
                    kind,
                });
            }
            if fields.len() >= MIN_PAIRS {
                return Some(Record {
                    shape: Shape::KeyValue {
                        separator,
                        assigner,
                    },
                    fields,
                });
            }
        }
    }
    None
}

/// Try to read the buffer as positional fields joined by one delimiter.
///
/// # Arguments
///
/// * `bytes` - The recovered buffer
fn parse_delimited(bytes: &[u8]) -> Option<Record> {
    for &delimiter in FIELD_DELIMITERS {
        let Some(tokens) = split_tokens(bytes, delimiter) else {
            continue;
        };
        if tokens.len() < MIN_FIELDS {
            continue;
        }
        // every field must be non-empty and of a sane length; one ragged field means
        // the delimiter is incidental punctuation rather than the record's structure
        if tokens
            .iter()
            .any(|token| token.trim().is_empty() || token.len() > MAX_FIELD_LEN)
        {
            continue;
        }
        let fields = tokens
            .iter()
            .map(|token| {
                let value = token.trim().to_string();
                let kind = classify(None, &value);
                Field {
                    key: None,
                    value,
                    kind,
                }
            })
            .collect::<Vec<_>>();
        let record = Record {
            shape: Shape::Delimited { delimiter },
            fields,
        };
        // prefer the first delimiter that yields a *credible* split, not merely a
        // well-formed one, so a comma inside prose never wins over the real structure
        if record.is_credible() {
            return Some(record);
        }
    }
    None
}

/// Split a buffer on a delimiter into printable tokens.
///
/// Returns `None` when the buffer is not printable enough to be a text record, or when
/// the split produces more than [`MAX_FIELDS`] tokens. A single trailing delimiter is
/// tolerated (records are routinely terminated rather than separated).
///
/// # Arguments
///
/// * `bytes` - The recovered buffer
/// * `delimiter` - The byte to split on
fn split_tokens(bytes: &[u8], delimiter: u8) -> Option<Vec<String>> {
    // the record's own delimiter aside, everything must be printable text
    if !bytes
        .iter()
        .all(|&b| b == delimiter || b.is_ascii_graphic() || b == b' ' || b == b'\t')
    {
        return None;
    }
    let trimmed = bytes.strip_suffix(&[delimiter]).unwrap_or(bytes);
    let tokens: Vec<String> = trimmed
        .split(|&b| b == delimiter)
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect();
    (tokens.len() <= MAX_FIELDS).then_some(tokens)
}

/// Try to read the buffer as a length-prefixed sequence of strings.
///
/// The length walk has to land exactly on the end of the buffer: a sequence that runs
/// over or stops short is not a sequence, it is two bytes that happened to look like a
/// length.
///
/// # Arguments
///
/// * `bytes` - The recovered buffer
fn parse_length_prefixed(bytes: &[u8]) -> Option<Record> {
    for width in [1usize, 2] {
        let mut fields = Vec::new();
        let mut at = 0usize;
        let walked = loop {
            if at == bytes.len() {
                break true;
            }
            // read the prefix, then the payload it claims
            let Some(prefix) = bytes.get(at..at + width) else {
                break false;
            };
            let len = match width {
                1 => usize::from(prefix[0]),
                _ => usize::from(u16::from_le_bytes([prefix[0], prefix[1]])),
            };
            at += width;
            let Some(payload) = bytes.get(at..at + len) else {
                break false;
            };
            // an empty or unreadable element means this is not a string sequence
            if len == 0 || len > MAX_FIELD_LEN || !is_text(payload) {
                break false;
            }
            at += len;
            let value = String::from_utf8_lossy(payload).trim().to_string();
            let kind = classify(None, &value);
            fields.push(Field {
                key: None,
                value,
                kind,
            });
            if fields.len() > MAX_FIELDS {
                break false;
            }
        };
        if walked && fields.len() >= MIN_FIELDS {
            return Some(Record {
                shape: Shape::LengthPrefixed { width },
                fields,
            });
        }
    }
    None
}

/// Try to read the buffer as a fixed-stride table of NUL-padded strings.
///
/// Requires the stride to divide the buffer exactly and at least one element to
/// actually be padded — an unpadded "table" is just a blob cut into equal pieces, and
/// any blob can be cut into equal pieces.
///
/// # Arguments
///
/// * `bytes` - The recovered buffer
fn parse_fixed_stride(bytes: &[u8]) -> Option<Record> {
    for &stride in STRIDES {
        if bytes.is_empty()
            || !bytes.len().is_multiple_of(stride)
            || bytes.len() / stride < MIN_FIELDS
        {
            continue;
        }
        if bytes.len() / stride > MAX_FIELDS {
            continue;
        }
        let mut fields = Vec::new();
        let mut padded = false;
        let mut ok = true;
        for slot in bytes.chunks_exact(stride) {
            // the element runs up to its first NUL; the rest must be padding
            let end = slot.iter().position(|&b| b == 0).unwrap_or(stride);
            let (text, tail) = slot.split_at(end);
            if text.is_empty() || !is_text(text) || tail.iter().any(|&b| b != 0) {
                ok = false;
                break;
            }
            padded |= !tail.is_empty();
            let value = String::from_utf8_lossy(text).trim().to_string();
            let kind = classify(None, &value);
            fields.push(Field {
                key: None,
                value,
                kind,
            });
        }
        if ok && padded && fields.len() >= MIN_FIELDS {
            return Some(Record {
                shape: Shape::FixedStride { stride },
                fields,
            });
        }
    }
    None
}

/// Whether a slice reads as printable text.
///
/// # Arguments
///
/// * `bytes` - The candidate element bytes
fn is_text(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes
            .iter()
            .all(|&b| b.is_ascii_graphic() || b == b' ' || b == b'\t')
}

/// Whether a token reads as a field name rather than as free text.
///
/// # Arguments
///
/// * `key` - The candidate key
fn is_identifier(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_LEN
        && key.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Classify a field value, using its key as a hint where the layout names its fields.
///
/// The key hint only decides between readings that are otherwise ambiguous — a bare
/// integer is a port, an interval, or just a number depending entirely on what it is
/// called. Everything else is decided by the value's own shape via
/// [`crate::thorpal::detect::indicators`], so a positional record classifies just as well as a named one.
///
/// # Arguments
///
/// * `key` - The field's name, when it has one
/// * `value` - The field's value
#[must_use]
pub fn classify(key: Option<&str>, value: &str) -> FieldKind {
    // shape-decided readings first: these need no hint and cannot be confused
    if indicators::parse_url(value).is_some() {
        return FieldKind::Url;
    }
    if indicators::parse_ipv4_literal(value).is_some()
        || indicators::parse_ipv6_literal(value).is_some()
    {
        return FieldKind::IpAddress;
    }
    if indicators::parse_endpoint(value).is_some() {
        return FieldKind::Endpoint;
    }
    let lower = value.to_ascii_lowercase();
    if FLAG_VALUES.contains(&lower.as_str()) {
        return FieldKind::Flag;
    }
    // a bare integer means nothing without a name for it
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        let hint = key.unwrap_or_default().to_ascii_lowercase();
        if PORT_KEYS.iter().any(|k| hint.contains(k)) && indicators::parse_port(value).is_some() {
            return FieldKind::Port;
        }
        if INTERVAL_KEYS.iter().any(|k| hint.contains(k)) {
            return FieldKind::Interval;
        }
        return FieldKind::Number;
    }
    // a path is a leading slash plus a segment, or any Windows-style path
    if (value.starts_with('/') && value.len() > 1) || value.contains('\\') {
        return FieldKind::Path;
    }
    if value.contains('.') && indicators::valid_url_host(value) {
        return FieldKind::Domain;
    }
    FieldKind::Text
}

/// Render a record as a markdown table for the flag content.
///
/// # Arguments
///
/// * `record` - The parsed record
#[must_use]
pub fn render(record: &Record) -> String {
    let mut out = String::new();
    out.push_str("### Parsed Configuration\n\n");
    let _ = writeln!(out, "Layout: {}\n", record.shape.label());
    out.push_str("| # | Field | Value | Reads as |\n|---|---|---|---|\n");
    for (idx, field) in record.fields.iter().enumerate() {
        let key = field.key.as_deref().unwrap_or("—");
        let _ = writeln!(
            out,
            "| {} | {} | `{}` | {} |",
            idx + 1,
            escape_cell(key),
            escape_cell(&field.value),
            field.kind.label()
        );
    }
    out
}

/// A one-line summary of a record for the artifact inventory.
///
/// # Arguments
///
/// * `record` - The parsed record
#[must_use]
pub fn summary(record: &Record) -> String {
    // lead with the fields worth pivoting on; fall back to the first few otherwise
    let notable = record.notable();
    let chosen: Vec<&Field> = if notable.is_empty() {
        record.fields.iter().take(4).collect()
    } else {
        notable.into_iter().take(4).collect()
    };
    chosen
        .iter()
        .map(|field| match &field.key {
            Some(key) => format!("{key}={}", field.value),
            None => field.value.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Neutralize a value so it cannot break out of a markdown table cell.
///
/// # Arguments
///
/// * `value` - The cell text
fn escape_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `key=value` config is parsed, its fields classified, and its endpoint,
    /// interval, and port picked out as the notable ones.
    #[test]
    fn parses_key_value_pairs() {
        let raw =
            b"url=http://c2.example.invalid/gate.php;sleep=600;port=8443;id=camp-7;debug=false";
        let record = parse(raw).expect("key=value record");
        assert_eq!(
            record.shape,
            Shape::KeyValue {
                separator: b';',
                assigner: b'='
            }
        );
        assert_eq!(record.fields.len(), 5);
        assert_eq!(record.fields[0].kind, FieldKind::Url);
        assert_eq!(record.fields[1].kind, FieldKind::Interval);
        assert_eq!(record.fields[2].kind, FieldKind::Port);
        // an identifier with no recognizable shape stays text, and a switch is a flag
        assert_eq!(record.fields[3].kind, FieldKind::Text);
        assert_eq!(record.fields[4].kind, FieldKind::Flag);
        assert_eq!(record.notable().len(), 3);
    }

    /// A positional record is parsed and classified without any field names.
    #[test]
    fn parses_positional_fields() {
        let raw = b"192.0.2.10:4444|/api/v2/checkin|30|agent-alpha";
        let record = parse(raw).expect("delimited record");
        assert_eq!(record.shape, Shape::Delimited { delimiter: b'|' });
        assert_eq!(record.fields[0].kind, FieldKind::Endpoint);
        assert_eq!(record.fields[1].kind, FieldKind::Path);
        // a bare integer with no key to name it is just a number
        assert_eq!(record.fields[2].kind, FieldKind::Number);
        assert_eq!(record.fields[3].kind, FieldKind::Text);
    }

    /// A length-prefixed string sequence is walked, and a walk that does not land
    /// exactly on the end of the buffer is rejected.
    #[test]
    fn parses_length_prefixed_sequence() {
        let parts: [&[u8]; 3] = [b"c2.example.invalid", b"/submit", b"443"];
        let mut raw = Vec::new();
        for part in parts {
            raw.push(part.len() as u8);
            raw.extend_from_slice(part);
        }
        let record = parse(&raw).expect("length-prefixed record");
        assert_eq!(record.shape, Shape::LengthPrefixed { width: 1 });
        assert_eq!(record.fields.len(), 3);
        assert_eq!(record.fields[0].kind, FieldKind::Domain);
        assert_eq!(record.fields[1].kind, FieldKind::Path);
        // one byte short of the declared payload and the walk no longer tiles
        raw.pop();
        assert!(parse_length_prefixed(&raw).is_none());
    }

    /// A fixed-stride table is parsed, and an unpadded blob of the same size is not.
    #[test]
    fn parses_fixed_stride_table() {
        let entries: [&[u8]; 3] = [b"cdn.example.invalid", b"/pixel.gif", b"10.0.0.9:8080"];
        let mut raw = Vec::new();
        for entry in entries {
            let mut slot = entry.to_vec();
            slot.resize(24, 0);
            raw.extend_from_slice(&slot);
        }
        let record = parse(&raw).expect("fixed-stride record");
        assert_eq!(record.shape, Shape::FixedStride { stride: 24 });
        assert_eq!(record.fields.len(), 3);
        assert_eq!(record.fields[2].kind, FieldKind::Endpoint);
        // the same length with no padding is just a blob cut into equal pieces
        assert!(parse_fixed_stride(&[b'A'; 72]).is_none());
    }

    /// Prose, a lone pair, and a ragged split are all rejected: a shape match without
    /// a configuration-looking field in it is not a record.
    #[test]
    fn rejects_text_that_merely_splits() {
        // a sentence splits on commas but holds nothing a config would hold
        assert!(parse(b"first, second, third, and then finally the fourth thing").is_none());
        // a single pair is not a grammar
        assert!(parse(b"host=c2.example.invalid").is_none());
        // an empty field means the delimiter is punctuation, not structure
        assert!(parse(b"http://a.example.invalid||30|x").is_none());
        // named fields but nothing notable in any of them
        assert!(parse(b"alpha=one;beta=two;gamma=three").is_none());
    }

    /// The rendered table names the layout and escapes a value that would otherwise
    /// break out of its cell.
    #[test]
    fn render_describes_the_layout() {
        let record = Record {
            shape: Shape::Delimited { delimiter: 0x00 },
            fields: vec![Field {
                key: None,
                value: "a|b".to_string(),
                kind: FieldKind::Text,
            }],
        };
        let table = render(&record);
        assert!(table.contains("separated by `NUL`"));
        assert!(table.contains("a\\|b"));
        // the summary falls back to the leading fields when nothing is notable
        assert_eq!(summary(&record), "a|b");
    }
}
