//! Static extraction of Office macro content from OLE compound files.
//!
//! Maldocs carry their logic in two places a Ghidra importer can never reach:
//! VBA macro source (stored MS-OVBA-compressed inside a `VBA` storage) and
//! Excel 4.0 (XLM) macro sheets (BIFF records inside the `Workbook`/`Book`
//! stream). This module parses both statically -- bytes are read and copied,
//! nothing is ever executed -- so an agent gets decompressed macro source in
//! the extracted workspace instead of staring at compressed stream dumps.
//!
//! Scope is deliberately olevba-class, not a full BIFF parser: VBA modules
//! are fully decompressed and extracted; XLM is detected (macro sheets named
//! with their visibility, every workbook stream scanned) with the stream
//! dump left to `extract_file`.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::{ExtractLimits, open_at, sanitize_entry_name, uniquify_name, write_capped};

/// The most bytes read from any single CFB stream during macro extraction
const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;

/// The most decompressed bytes one VBA source stream may produce
const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;

/// The decompressed size every MS-OVBA chunk is bounded by (per spec)
const CHUNK_BYTES: usize = 4096;

/// The most modules one dir stream may declare before the parse stops
const MAX_MODULES: usize = 1024;

/// The most characters any declared name (module, stream, project) keeps
///
/// Compound-file stream names are capped at 31 UTF-16 units by the format,
/// so no legitimate name is ever truncated -- this bounds only hostile dir
/// streams, whose multi-megabyte "names" would otherwise flow into the
/// report and blow the analyzing agent's context.
const MAX_NAME_CHARS: usize = 128;

/// The most modules the report carries full entries for
///
/// Extraction itself continues past this (sources still land in the
/// workspace under the shared budget); only the report entries stop, so a
/// hostile thousand-module dir stream cannot inflate the tool result.
const MAX_REPORT_MODULES: usize = 64;

/// The most caveat notes one report carries before suppressing the rest
const MAX_NOTES: usize = 24;

/// The most VBA project roots one extraction walks
///
/// Real documents carry one; a hostile compound file declaring hundreds of
/// project storages cannot inflate the report past this.
const MAX_ROOTS: usize = 16;

/// The most workbook streams one extraction scans for XLM macro sheets
const MAX_WORKBOOK_SCANS: usize = 8;

/// The most decompressed bytes one extraction produces in total
///
/// The per-stream caps bound memory, but their product with the module and
/// root caps is CPU a crafted module flood can grind through while holding
/// the session-wide extraction lock -- this bounds the WORK of one call. No
/// real project comes near it.
const MAX_TOTAL_SOURCE_BYTES: u64 = 256 * 1024 * 1024;

/// Append a caveat note, suppressing past the cap
///
/// Per-item skip notes echo attacker-influenced names, so a hostile document
/// could otherwise inflate the report through its own failure notes; the
/// first suppressed note is replaced by a marker and the rest are dropped.
///
/// # Arguments
///
/// * `notes` - The report's notes list
/// * `note` - The note to append
fn push_note(notes: &mut Vec<String>, note: String) {
    // append under the cap; the first overflow leaves a marker instead
    if notes.len() < MAX_NOTES {
        notes.push(note);
    } else if notes.len() == MAX_NOTES {
        notes.push(
            "further notes suppressed (the report caps its own size against hostile input)"
                .to_owned(),
        );
    }
}

/// The most characters of a module's source quoted inline as its preview
const MODULE_PREVIEW_CHARS: usize = 600;

/// The most characters of the PROJECT stream quoted into the report
const PROJECT_PREVIEW_CHARS: usize = 500;

/// A module declared by a VBA project's `dir` stream
#[derive(Debug, Clone, Default)]
struct DirModule {
    /// The module's display name (MODULENAME, or its Unicode variant)
    name: String,
    /// The stream the module's source lives in, as code-page text
    /// (MODULESTREAMNAME, decoded lossily)
    stream_name: String,
    /// The stream name's authoritative UTF-16 spelling (the record's
    /// Unicode half) -- compound-file names ARE UTF-16, so this is the
    /// one that matches non-ASCII stream names the lossy decode mangles
    stream_name_unicode: String,
    /// The byte offset the compressed source starts at (MODULEOFFSET)
    text_offset: Option<u32>,
}

/// The best-effort outcome of parsing one `dir` stream
///
/// The parse never fails outright: a malformed record stops the walk with a
/// caveat instead of hiding every module (and the sibling projects and XLM
/// scan) behind one 3-byte lie, so everything declared before the damage is
/// still extracted.
#[derive(Debug, Clone, Default)]
struct DirParse {
    /// The modules declared before the walk ended
    modules: Vec<DirModule>,
    /// The project's declared name, when the stream carries one
    project_name: Option<String>,
    /// Why the walk stopped early, when it did
    caveat: Option<String>,
}

/// Decode code-page record text lossily, bounded to the name cap
///
/// # Arguments
///
/// * `data` - The record's raw code-page bytes
fn decode_name(data: &[u8]) -> String {
    // the code page is not tracked; a lossy decode is fine for triage
    bound_name(&String::from_utf8_lossy(data))
}

/// Decode a UTF-16LE record lossily, bounded to the name cap
///
/// # Arguments
///
/// * `data` - The record's raw UTF-16LE bytes
fn decode_name_utf16(data: &[u8]) -> String {
    // reassemble the little-endian code units, dropping a dangling byte
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    bound_name(&String::from_utf16_lossy(&units))
}

/// Truncate a decoded name to the cap, marking the cut
///
/// # Arguments
///
/// * `name` - The decoded name to bound
fn bound_name(name: &str) -> String {
    // cut on a char boundary so multi-byte names stay valid
    match name.char_indices().nth(MAX_NAME_CHARS) {
        Some((cut, _)) => format!("{}…", &name[..cut]),
        None => name.to_owned(),
    }
}

/// One extracted VBA module
#[derive(Debug, Clone, Serialize)]
pub struct MacroModule {
    /// The module's name as the project declares it
    pub name: String,
    /// The CFB stream the module's source was read from
    pub stream: String,
    /// The extracted source's workspace name (usable as every file tool's
    /// `target`), or `None` when extraction was skipped (a note says why)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    /// The decompressed source's size in bytes
    pub source_bytes: u64,
    /// The opening lines of the decompressed source
    pub preview: String,
}

/// One VBA project found in the compound file
#[derive(Debug, Clone, Serialize)]
pub struct VbaExtract {
    /// The storage path the project lives under (e.g. `/Macros/VBA`)
    pub storage: String,
    /// The project's declared name, when the dir stream carries one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// The opening of the plaintext PROJECT properties stream, when present
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_properties: Option<String>,
    /// The project's modules with their extracted source
    pub modules: Vec<MacroModule>,
}

/// One sheet the XLM scan flagged as an Excel 4.0 macro sheet
#[derive(Debug, Clone, Serialize)]
pub struct XlmSheet {
    /// The sheet's name as the BOUNDSHEET record declares it
    pub name: String,
    /// The sheet's visibility: `visible`, `hidden`, or `very-hidden`
    pub visibility: String,
}

/// The result of scanning a workbook stream for Excel 4.0 macro sheets
#[derive(Debug, Clone, Serialize)]
pub struct XlmScan {
    /// The full path of the workbook stream that was scanned
    pub stream: String,
    /// The BIFF version the stream's BOF record declares
    pub biff: String,
    /// How many sheets the workbook declares in total
    pub total_sheets: usize,
    /// The sheets typed as Excel 4.0 macro sheets
    pub macro_sheets: Vec<XlmSheet>,
    /// A next-step or caveat note, when one applies
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The full macro-extraction report for one OLE compound file
#[derive(Debug, Clone, Serialize)]
pub struct MacroReport {
    /// The VBA projects found, with their modules extracted
    pub vba: Vec<VbaExtract>,
    /// The XLM scan results, one per workbook stream found (root first)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub xlm: Vec<XlmScan>,
    /// Caveats and skips the extraction wants the caller to know about
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// Decompress an MS-OVBA CompressedContainer ([MS-OVBA] 2.4.1)
///
/// The container is a 0x01 signature byte followed by chunks: a 2-byte
/// little-endian header (bits 0-11 the chunk size minus 3, bits 12-14 the
/// 0b011 signature, bit 15 the compressed flag) and then either 4096 literal
/// bytes (raw chunk) or token groups -- a flag byte whose bits select, per
/// token, a literal byte or a 2-byte back-reference copy token whose
/// offset/length split widens as the chunk fills. Every parse step is
/// bounds-checked so a truncated or hostile stream errors instead of
/// panicking or ballooning memory.
///
/// # Arguments
///
/// * `data` - The compressed container, starting at its signature byte
/// * `cap` - The most decompressed bytes to produce
/// * `work` - A cross-call work meter the bytes PRODUCED are billed into as
///   they are produced -- a bomb that errors at the cap has still done the
///   capped work, and only incremental billing accounts it honestly (a
///   blanket charge would let instant failures exhaust a caller's budget,
///   and no charge would let partial bombs grind unbilled)
pub(crate) fn decompress_container(
    data: &[u8],
    cap: usize,
    work: &mut u64,
) -> Result<Vec<u8>, String> {
    // the container must open with its signature byte
    if data.first() != Some(&0x01) {
        return Err(
            "not an MS-OVBA compressed container (missing the 0x01 signature byte)".to_owned(),
        );
    }
    let mut out = Vec::new();
    let mut pos = 1usize;
    // walk the chunks back to back until the input is exhausted
    while pos < data.len() {
        // every chunk opens with its 2-byte header
        if pos + 2 > data.len() {
            return Err(format!("truncated chunk header at byte {pos}"));
        }
        let header = u16::from_le_bytes([data[pos], data[pos + 1]]);
        // bits 12-14 are a fixed signature; refusing a bad one catches
        // desynced parses early instead of emitting garbage
        if header & 0x7000 != 0x3000 {
            return Err(format!("bad chunk signature in header at byte {pos}"));
        }
        // bits 0-11 encode the whole chunk's size (header included) minus 3
        let total = (header & 0x0FFF) as usize + 3;
        let data_end = pos + total;
        if data_end > data.len() {
            return Err(format!(
                "truncated chunk at byte {pos}: header declares {total} bytes but \
                 only {} remain",
                data.len() - pos
            ));
        }
        let chunk = &data[pos + 2..data_end];
        let before = out.len();
        // bit 15 separates stored-raw chunks from token-compressed ones
        let decompressed = if header & 0x8000 == 0 {
            // a raw chunk is literal bytes, copied as-is
            out.extend_from_slice(chunk);
            Ok(())
        } else {
            decompress_chunk(chunk, &mut out)
        };
        // bill the chunk's produced bytes before propagating any error: a
        // chunk that errored mid-way still did that work
        *work = work.saturating_add((out.len() - before) as u64);
        decompressed?;
        // enforce the output cap between chunks so a bomb stops early
        if out.len() > cap {
            return Err(format!("decompressed output exceeded the {cap} byte cap"));
        }
        pos = data_end;
    }
    Ok(out)
}

/// Decompress one token-compressed chunk, appending to the output
///
/// # Arguments
///
/// * `chunk` - The chunk's data bytes (header already consumed)
/// * `out` - The container-wide output the chunk appends to
fn decompress_chunk(chunk: &[u8], out: &mut Vec<u8>) -> Result<(), String> {
    // copy tokens reference the current chunk's output only
    let chunk_start = out.len();
    let mut i = 0usize;
    // each flag byte governs the next eight tokens
    while i < chunk.len() {
        let flags = chunk[i];
        i += 1;
        for bit in 0..8 {
            // the final flag group may cover fewer than eight tokens
            if i >= chunk.len() {
                break;
            }
            if flags & (1 << bit) == 0 {
                // a clear bit is a single literal byte
                out.push(chunk[i]);
                i += 1;
            } else {
                // a set bit is a 2-byte back-reference copy token
                if i + 2 > chunk.len() {
                    return Err("truncated copy token at the end of a chunk".to_owned());
                }
                let token = u16::from_le_bytes([chunk[i], chunk[i + 1]]);
                i += 2;
                let produced = out.len() - chunk_start;
                // a copy before any output has nothing to reference
                if produced == 0 {
                    return Err("copy token before any decompressed output".to_owned());
                }
                // the offset/length split widens as the chunk fills: the
                // smallest bit count >= 4 whose range covers the output so far
                let mut bit_count = 4u32;
                while (1usize << bit_count) < produced {
                    bit_count += 1;
                }
                let length_mask = 0xFFFFu16 >> bit_count;
                let length = (token & length_mask) as usize + 3;
                let offset = ((token & !length_mask) >> (16 - bit_count)) as usize + 1;
                // the reference must stay inside this chunk's output
                if offset > produced {
                    return Err(format!(
                        "copy token reaches {offset} bytes back with only {produced} \
                         decompressed"
                    ));
                }
                // byte-wise copy: overlapping references repeat recent output
                for _ in 0..length {
                    let byte = out[out.len() - offset];
                    out.push(byte);
                }
            }
            // a chunk may never decompress past the spec's 4096 bytes
            if out.len() - chunk_start > CHUNK_BYTES {
                return Err(format!(
                    "chunk decompressed past the {CHUNK_BYTES} byte chunk bound"
                ));
            }
        }
    }
    Ok(())
}

/// Parse a decompressed `dir` stream into its modules ([MS-OVBA] 2.3.4.2)
///
/// The stream is a flat sequence of `u16 id, u32 size, bytes` records, with
/// one documented irregularity: PROJECTVERSION (0x0009) stores a reserved
/// value where its size would be and always carries 6 data bytes. Only the
/// records that locate module source are interpreted; everything else is
/// skipped by size, bounds-checked. The parse is best-effort by design: a
/// record lying about its size (or a module flood) stops the walk with a
/// caveat but keeps everything already declared -- maldocs are hostile
/// input, and one bad record must not hide the modules before it.
///
/// # Arguments
///
/// * `dir` - The decompressed dir stream
fn parse_dir_stream(dir: &[u8]) -> DirParse {
    let mut parse = DirParse::default();
    let mut pos = 0usize;
    // walk the record sequence until the terminator or the end of the stream
    while pos + 6 <= dir.len() {
        let id = u16::from_le_bytes([dir[pos], dir[pos + 1]]);
        let size =
            u32::from_le_bytes([dir[pos + 2], dir[pos + 3], dir[pos + 4], dir[pos + 5]]) as usize;
        pos += 6;
        // PROJECTVERSION's size field is a reserved constant, not a size: the
        // record always carries 4 + 2 version bytes
        if id == 0x0009 {
            pos = pos.saturating_add(6);
            continue;
        }
        // the terminator ends the meaningful records
        if id == 0x0010 {
            break;
        }
        // a record lying about its size desyncs everything after it: stop
        // the walk with a caveat, keeping the modules declared before it
        if pos + size > dir.len() {
            parse.caveat = Some(format!(
                "dir record {id:#06x} at byte {} declares {size} bytes but only {} remain; \
                 the walk stopped there and later module declarations (if any) are not \
                 listed -- extract the storage's raw streams with extract_file to inspect \
                 what it hides",
                pos - 6,
                dir.len() - pos
            ));
            break;
        }
        let data = &dir[pos..pos + size];
        match id {
            // PROJECTNAME names the project (code-page text, decoded lossily)
            0x0004 => parse.project_name = Some(decode_name(data)),
            // MODULENAME starts the next module's record group
            0x0019 => {
                if parse.modules.len() >= MAX_MODULES {
                    parse.caveat = Some(format!(
                        "dir stream declares more than {MAX_MODULES} modules; the walk \
                         stopped at the cap"
                    ));
                    break;
                }
                parse.modules.push(DirModule {
                    name: decode_name(data),
                    ..DirModule::default()
                });
            }
            // MODULENAMEUNICODE is the display name's authoritative UTF-16
            // spelling; prefer it over the lossy code-page decode
            0x0047 => {
                if let Some(module) = parse.modules.last_mut() {
                    let unicode = decode_name_utf16(data);
                    if !unicode.is_empty() {
                        module.name = unicode;
                    }
                }
            }
            // MODULESTREAMNAME names the stream the source lives in, as
            // code-page text (its UTF-16 half follows as the 0x0032 record)
            0x001A => {
                if let Some(module) = parse.modules.last_mut() {
                    module.stream_name = decode_name(data);
                }
            }
            // the stream name's UTF-16 half: compound-file names ARE UTF-16,
            // so this is the spelling that actually matches non-ASCII streams
            0x0032 => {
                if let Some(module) = parse.modules.last_mut() {
                    module.stream_name_unicode = decode_name_utf16(data);
                }
            }
            // MODULEOFFSET locates the compressed source within that stream
            0x0031 => {
                if let Some(module) = parse.modules.last_mut() {
                    if size >= 4 {
                        module.text_offset =
                            Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
                    }
                }
            }
            // every other record is metadata the extraction does not need
            _ => {}
        }
        pos += size;
    }
    parse
}

/// The BIFF sheet-visibility states a BOUNDSHEET record can declare
///
/// # Arguments
///
/// * `state` - The record's visibility bits (already masked to 0..=3)
fn visibility_label(state: u8) -> &'static str {
    // 0 is visible; 1 hides from the tab bar; 2 hides from the UI entirely
    match state {
        0 => "visible",
        1 => "hidden",
        2 => "very-hidden",
        _ => "unknown",
    }
}

/// Scan a workbook stream's BOUNDSHEET records for Excel 4.0 macro sheets
///
/// Walks the BIFF record sequence (`u16 opcode, u16 size, bytes`) collecting
/// every BOUNDSHEET (0x0085) entry's sheet type, visibility, and name. The
/// name decodes per the BOF-declared BIFF version (byte string in BIFF5,
/// flagged 8/16-bit in BIFF8), lossily. A malformed record ends the scan with
/// a note instead of erroring the whole report -- detection is best-effort by
/// design.
///
/// # Arguments
///
/// * `stream` - The workbook stream's full path, for attribution
/// * `data` - The workbook stream's bytes
fn scan_workbook(stream: &str, data: &[u8]) -> XlmScan {
    let mut scan = XlmScan {
        stream: stream.to_owned(),
        biff: "unknown".to_owned(),
        total_sheets: 0,
        macro_sheets: Vec::new(),
        note: None,
    };
    let mut pos = 0usize;
    // walk the record stream; headers are 4 bytes, data follows
    while pos + 4 <= data.len() {
        let opcode = u16::from_le_bytes([data[pos], data[pos + 1]]);
        let size = u16::from_le_bytes([data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if pos + size > data.len() {
            scan.note = Some(format!(
                "the record walk stopped at a truncated record ({opcode:#06x}) at \
                 byte {}; counts may be incomplete",
                pos - 4
            ));
            break;
        }
        let record = &data[pos..pos + size];
        pos += size;
        match opcode {
            // BOF declares the substream's BIFF version
            0x0809 if record.len() >= 2 => {
                let version = u16::from_le_bytes([record[0], record[1]]);
                scan.biff = match version {
                    0x0600 => "biff8".to_owned(),
                    0x0500 => "biff5".to_owned(),
                    other => format!("{other:#06x}"),
                };
            }
            // BOUNDSHEET declares one sheet: position, state, type, and name
            0x0085 if record.len() >= 7 => {
                scan.total_sheets += 1;
                let state = record[4] & 0x03;
                let sheet_type = record[5];
                // only Excel 4.0 macro sheets (type 0x01) are of interest
                if sheet_type != 0x01 {
                    continue;
                }
                let cch = record[6] as usize;
                // BIFF8 names carry a flags byte; BIFF5 names are byte strings
                let name = if scan.biff == "biff5" {
                    let end = (7 + cch).min(record.len());
                    String::from_utf8_lossy(&record[7..end]).into_owned()
                } else if record.len() >= 8 && record[7] & 0x01 != 0 {
                    // 16-bit characters: cch UTF-16LE code units follow
                    let end = (8 + cch * 2).min(record.len());
                    let units: Vec<u16> = record[8..end]
                        .chunks_exact(2)
                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                        .collect();
                    String::from_utf16_lossy(&units)
                } else {
                    // 8-bit (compressed) characters follow the flags byte
                    let end = (8 + cch).min(record.len());
                    String::from_utf8_lossy(&record[8.min(record.len())..end]).into_owned()
                };
                scan.macro_sheets.push(XlmSheet {
                    name,
                    visibility: visibility_label(state).to_owned(),
                });
            }
            // every other record is irrelevant to sheet discovery
            _ => {}
        }
    }
    // point the caller at the content once macro sheets are confirmed,
    // keeping any truncation caveat the walk already recorded
    if !scan.macro_sheets.is_empty() {
        let guidance = format!(
            "Excel 4.0 macro sheets confirmed -- their formulas live in the \
             '{stream}' stream; extract it with extract_file and grep the dump \
             (strings_file/read_file) for the macro logic",
        );
        scan.note = Some(match scan.note.take() {
            Some(existing) => format!("{existing}; {guidance}"),
            None => guidance,
        });
    }
    scan
}

/// Read one CFB stream fully, bounded by a byte cap
///
/// # Arguments
///
/// * `comp` - The parsed compound file
/// * `path` - The stream's full path within the compound file
/// * `cap` - The most bytes to read before refusing
fn read_stream_capped<R: Read + std::io::Seek>(
    comp: &mut cfb::CompoundFile<R>,
    path: &Path,
    cap: usize,
) -> Result<Vec<u8>, String> {
    // open the stream and read one byte past the cap so an overrun is visible
    let stream = comp
        .open_stream(path)
        .map_err(|err| format!("failed to open stream '{}': {err}", path.display()))?;
    let mut data = Vec::new();
    stream
        .take(cap as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|err| format!("failed to read stream '{}': {err}", path.display()))?;
    if data.len() > cap {
        return Err(format!(
            "stream '{}' exceeds the {cap} byte macro-extraction cap",
            path.display()
        ));
    }
    Ok(data)
}

/// Build a bounded, line-oriented preview of decoded text
///
/// # Arguments
///
/// * `text` - The decoded text to preview
/// * `cap` - The most characters to quote
fn preview_text(text: &str, cap: usize) -> String {
    // quote the head on a char boundary and mark the cut when one happened
    let mut preview: String = text.chars().take(cap).collect();
    if text.chars().nth(cap).is_some() {
        preview.push_str("\n…(truncated; read the extracted file for the rest)");
    }
    preview
}

/// Extract every VBA module and XLM indicator from an OLE compound file
///
/// Finds VBA projects by shape rather than by fixed path -- any storage
/// holding both a `dir` and a `_VBA_PROJECT` stream is a project root (which
/// covers Word's `Macros/VBA`, Excel's `_VBA_PROJECT_CUR/VBA`, and an
/// extracted OOXML `vbaProject.bin` whose root storage is the project) -- and
/// scans every `Workbook`/`Book` stream (root first) for Excel 4.0 macro
/// sheets. All name matching is case-insensitive, exactly like MS-CFB's own
/// resolution, so a case-flipped directory tree cannot evade detection.
/// Module source is decompressed and written raw into the extracted
/// workspace under the shared size budget, with per-call caps on roots,
/// report entries, notes, and total decompression work -- maldocs are
/// hostile input and must not be able to grind or flood the session.
///
/// # Arguments
///
/// * `path` - The file holding the compound file
/// * `offset` - The byte offset the compound file starts at within the file
/// * `dest_dir` - The directory extracted module source lands in
/// * `limits` - The size budget the extraction runs under
pub(crate) fn extract_macros_sync(
    path: &Path,
    offset: u64,
    dest_dir: &Path,
    limits: ExtractLimits,
) -> Result<MacroReport, String> {
    // open the file at the requested offset and sniff the container magic
    let (reader, head) = open_at(path, offset)?;
    match super::detect_container(&head) {
        Some(super::ContainerKind::Cfb) => {}
        // OOXML macros live in an embedded vbaProject.bin, which is itself a
        // compound file: route the caller through extract_file first
        Some(super::ContainerKind::Zip) => {
            return Err(
                "this is a ZIP (OOXML documents are ZIPs): macro-carrying OOXML \
                 files embed a 'vbaProject.bin' compound file -- extract it with \
                 extract_file (word/vbaProject.bin, xl/vbaProject.bin, or \
                 ppt/vbaProject.bin) and run extract_macros on that extracted \
                 file via `target`"
                    .to_owned(),
            );
        }
        _ => {
            return Err(
                "not an OLE compound file: extract_macros reads legacy Office \
                 documents (CDF/OLE, magic D0 CF 11 E0) and extracted \
                 vbaProject.bin files"
                    .to_owned(),
            );
        }
    }
    // parse the compound file's directory tree once, up front
    let mut comp = cfb::CompoundFile::open(reader)
        .map_err(|err| format!("failed to parse the compound file: {err}"))?;
    // snapshot the entries so stream reads (which need &mut) can follow
    let entries: Vec<(PathBuf, bool)> = comp
        .walk()
        .map(|entry| (entry.path().to_path_buf(), entry.is_stream()))
        .collect();
    // MS-CFB name resolution is case-insensitive (ole32 and the cfb crate's
    // own open_stream both uppercase-compare), so every lookup here must be
    // too -- a case-flipped `DIR` tree still loads and auto-runs in Office
    // and must not evade detection. The uppercased-path index resolves a
    // wanted path back to its real on-disk spelling.
    let streams: Vec<(String, PathBuf)> = entries
        .iter()
        .filter(|(_, is_stream)| *is_stream)
        .map(|(path, _)| (path.to_string_lossy().to_uppercase(), path.clone()))
        .collect();
    let resolve_stream = |path: &Path| -> Option<&PathBuf> {
        // compare the uppercased spellings, exactly like a CFB lookup
        let wanted = path.to_string_lossy().to_uppercase();
        streams
            .iter()
            .find(|(upper, _)| *upper == wanted)
            .map(|(_, real)| real)
    };
    // a VBA project root is any storage holding both `dir` and `_VBA_PROJECT`
    // (matched case-insensitively); keep the dir stream's REAL path alongside
    let is_name = |path: &Path, wanted: &str| {
        // CFB compares names by Unicode uppercase, not ASCII: U+0131 (dotless
        // i) uppercases to I, so a stream spelled "d\u{131}r" IS "dir" to
        // Office and must be to this detection too
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.to_uppercase() == wanted.to_uppercase())
    };
    let mut roots: Vec<(PathBuf, PathBuf)> = entries
        .iter()
        .filter(|(entry, is_stream)| {
            *is_stream
                && is_name(entry, "dir")
                && entry
                    .parent()
                    .is_some_and(|parent| resolve_stream(&parent.join("_VBA_PROJECT")).is_some())
        })
        .filter_map(|(entry, _)| {
            entry
                .parent()
                .map(|parent| (parent.to_path_buf(), entry.clone()))
        })
        .collect();
    roots.dedup();
    // bound the project walk: real documents carry one project, so a flood
    // of project storages is hostile shaping, not content
    let total_roots = roots.len();
    roots.truncate(MAX_ROOTS);
    let mut report = MacroReport {
        vba: Vec::new(),
        xlm: Vec::new(),
        notes: Vec::new(),
    };
    // track the workspace budget across every module written, plus the
    // report-entry, suppressed-module, duplicate, and decompression-work
    // tallies the size and CPU caps need
    let mut written_total = 0u64;
    let mut written_index = 0usize;
    let mut reported_modules = 0usize;
    let mut unreported_modules = 0usize;
    let mut duplicate_modules = 0usize;
    let mut decompressed_total = 0u64;
    let mut work_exhausted = false;
    let mut extracted_streams: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    'roots: for (root, dir_path) in roots {
        // the dir stream is itself MS-OVBA compressed; decode it first --
        // any failure quarantines THIS project only, never the whole report
        let dir_raw = match read_stream_capped(&mut comp, &dir_path, MAX_STREAM_BYTES) {
            Ok(raw) => raw,
            Err(err) => {
                push_note(
                    &mut report.notes,
                    format!(
                        "the VBA project at '{}' has an unreadable dir stream ({err}); \
                         its modules were skipped",
                        root.display()
                    ),
                );
                continue;
            }
        };
        // the stream read itself is work the budget must see: a flood of
        // modules sharing one 64 MiB chain grinds reads long before any
        // output exists to bill
        decompressed_total = decompressed_total.saturating_add(dir_raw.len() as u64);
        let dir = match decompress_container(&dir_raw, MAX_SOURCE_BYTES, &mut decompressed_total) {
            Ok(dir) => dir,
            Err(err) => {
                push_note(
                    &mut report.notes,
                    format!(
                        "the VBA project at '{}' has an undecodable dir stream ({err}); \
                         its modules were skipped",
                        root.display()
                    ),
                );
                continue;
            }
        };
        // the parse is best-effort: a malformed record surfaces as a caveat
        // while everything declared before it still extracts below
        let parsed = parse_dir_stream(&dir);
        if let Some(caveat) = &parsed.caveat {
            push_note(
                &mut report.notes,
                format!("the VBA project at '{}': {caveat}", root.display()),
            );
        }
        // the plaintext PROJECT properties stream lives beside the project
        let project_properties = root
            .parent()
            .and_then(|parent| resolve_stream(&parent.join("PROJECT")))
            .cloned()
            .and_then(|path| read_stream_capped(&mut comp, &path, MAX_STREAM_BYTES).ok())
            .map(|data| preview_text(&String::from_utf8_lossy(&data), PROJECT_PREVIEW_CHARS));
        let mut extract = VbaExtract {
            storage: root.display().to_string(),
            project_name: parsed.project_name,
            project_properties,
            modules: Vec::new(),
        };
        for module in parsed.modules {
            // decompression work is CPU the whole session's extraction lock
            // waits behind: once the per-call work budget is spent, stop
            // extracting entirely rather than grinding a crafted flood
            if decompressed_total >= MAX_TOTAL_SOURCE_BYTES {
                work_exhausted = true;
                report.vba.push(extract);
                break 'roots;
            }
            // resolve the module's source stream: the UTF-16 spelling is
            // authoritative (compound-file names are UTF-16, so it matches
            // the non-ASCII names the lossy code-page decode mangles), the
            // code-page decode is the fallback, and a module with no
            // stream-name record falls back to its display name; resolution
            // is case-insensitive, exactly like a CFB lookup
            let candidates = [
                module.stream_name_unicode.as_str(),
                module.stream_name.as_str(),
                module.name.as_str(),
            ];
            let found = candidates
                .iter()
                .filter(|name| !name.is_empty())
                .find_map(|name| resolve_stream(&root.join(name)));
            let Some(stream_path) = found.cloned() else {
                // name what was declared so the miss is diagnosable
                let declared = candidates
                    .iter()
                    .copied()
                    .find(|name| !name.is_empty())
                    .unwrap_or("<unnamed>");
                push_note(
                    &mut report.notes,
                    format!(
                        "module '{}' names stream '{declared}' which does not exist; \
                         skipped",
                        module.name
                    ),
                );
                continue;
            };
            // one stream decompresses once: a flood of modules all naming the
            // same stream is hostile shaping, and the first extraction
            // already made the content addressable
            if !extracted_streams.insert(stream_path.clone()) {
                duplicate_modules += 1;
                continue;
            }
            let stream_name = stream_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| stream_path.display().to_string());
            let raw = match read_stream_capped(&mut comp, &stream_path, MAX_STREAM_BYTES) {
                Ok(raw) => raw,
                Err(err) => {
                    push_note(
                        &mut report.notes,
                        format!("module '{}' was unreadable ({err}); skipped", module.name),
                    );
                    continue;
                }
            };
            // the read is work even when nothing decompresses from it
            decompressed_total = decompressed_total.saturating_add(raw.len() as u64);
            // the compressed source starts at the dir-declared text offset;
            // the bytes before it are a performance cache the parse skips
            let text_offset = module.text_offset.unwrap_or(0) as usize;
            if text_offset >= raw.len() {
                push_note(
                    &mut report.notes,
                    format!(
                        "module '{}' declares text offset {text_offset} past its \
                         {}-byte stream; skipped",
                        module.name,
                        raw.len()
                    ),
                );
                continue;
            }
            // the work meter bills produced bytes incrementally inside the
            // decompressor, so bombs pay for exactly the work they did and
            // instant failures pay nothing extra beyond their read
            let source = match decompress_container(
                &raw[text_offset..],
                MAX_SOURCE_BYTES,
                &mut decompressed_total,
            ) {
                Ok(source) => source,
                Err(err) => {
                    push_note(
                        &mut report.notes,
                        format!(
                            "module '{}' failed to decompress ({err}); extract stream \
                             '{stream_name}' with extract_file to inspect it raw",
                            module.name
                        ),
                    );
                    continue;
                }
            };
            // land the raw decompressed source in the extracted workspace,
            // still under the session's shared size budget
            let remaining = limits.total.saturating_sub(written_total);
            let cap = limits.entry.min(remaining);
            let base = sanitize_entry_name(&format!("{}.vba", module.name), written_index);
            let workspace_name = uniquify_name(dest_dir, &base);
            written_index += 1;
            let dest = dest_dir.join(&workspace_name);
            let workspace_name = match write_capped(
                source.as_slice(),
                &dest,
                cap,
                &format!(
                    "extraction stopped: module '{}' would exceed the remaining \
                     workspace budget of {cap} bytes",
                    module.name
                ),
            ) {
                Ok(written) => {
                    written_total += written;
                    Some(workspace_name)
                }
                Err(err) => {
                    push_note(
                        &mut report.notes,
                        format!("module '{}' was not written: {err}", module.name),
                    );
                    None
                }
            };
            // the report carries full entries for the first modules only; a
            // hostile thousand-module dir cannot inflate the tool result,
            // while the sources above still landed in the workspace
            if reported_modules < MAX_REPORT_MODULES {
                reported_modules += 1;
                extract.modules.push(MacroModule {
                    name: module.name,
                    stream: stream_path.display().to_string(),
                    workspace_name,
                    source_bytes: source.len() as u64,
                    preview: preview_text(&String::from_utf8_lossy(&source), MODULE_PREVIEW_CHARS),
                });
            } else {
                unreported_modules += 1;
            }
        }
        report.vba.push(extract);
    }
    // the coverage notes below are single bounded lines that must survive
    // even when hostile skip notes already hit the suppression cap, so they
    // append directly
    if unreported_modules > 0 {
        report.notes.push(format!(
            "{unreported_modules} further module(s) were extracted into the workspace \
             without report entries (the report lists at most {MAX_REPORT_MODULES}); \
             address them with read_text -- a bad `target` lists every extracted name"
        ));
    }
    if total_roots > MAX_ROOTS {
        report.notes.push(format!(
            "{} further VBA project storage(s) were not walked (the report caps at \
             {MAX_ROOTS}); list and extract their streams with list_container/extract_file",
            total_roots - MAX_ROOTS
        ));
    }
    if duplicate_modules > 0 {
        report.notes.push(format!(
            "{duplicate_modules} module declaration(s) named a stream already extracted \
             for an earlier module and were skipped as duplicates"
        ));
    }
    if work_exhausted {
        report.notes.push(format!(
            "macro decompression stopped at the per-call work budget \
             ({MAX_TOTAL_SOURCE_BYTES} bytes of decompressed output): the remaining \
             declared modules were not extracted -- a real project never reaches this; \
             treat the document as hostile shaping and extract specific streams with \
             extract_file instead"
        ));
    }
    // scan EVERY workbook stream (either BIFF spelling, matched
    // case-insensitively) for XLM macro sheets, root-level streams first:
    // the root stream is the one Excel actually opens, and matching only
    // the first name anywhere would let a decoy `Book` shadow the real
    // `/Workbook`; embedded-object workbooks deeper in the tree are scanned
    // too, attributed by their full path
    let mut workbooks: Vec<PathBuf> = entries
        .iter()
        .filter(|(entry, is_stream)| {
            *is_stream && (is_name(entry, "Workbook") || is_name(entry, "Book"))
        })
        .map(|(entry, _)| entry.clone())
        .collect();
    workbooks.sort_by_key(|path| path.components().count());
    let total_workbooks = workbooks.len();
    workbooks.truncate(MAX_WORKBOOK_SCANS);
    for path in workbooks {
        let name = path.display().to_string();
        match read_stream_capped(&mut comp, &path, MAX_STREAM_BYTES) {
            Ok(data) => report.xlm.push(scan_workbook(&name, &data)),
            Err(err) => push_note(
                &mut report.notes,
                format!("the '{name}' stream was unreadable ({err}); XLM not scanned"),
            ),
        }
    }
    if total_workbooks > MAX_WORKBOOK_SCANS {
        report.notes.push(format!(
            "{} further workbook stream(s) were not scanned (the report caps at \
             {MAX_WORKBOOK_SCANS}); extract them with extract_file to inspect the rest",
            total_workbooks - MAX_WORKBOOK_SCANS
        ));
    }
    // an empty result is an answer, not an error: say what that means
    if report.vba.is_empty() && report.xlm.is_empty() {
        report.notes.push(
            "no VBA project storage and no Workbook/Book stream: this compound \
             file carries no detectable macro content (an MSI installer or a \
             macro-less document); list_container/extract_file remain the right \
             tools for its streams"
                .to_owned(),
        );
    }
    Ok(report)
}

/// Compress bytes as all-literal MS-OVBA chunks (test fixture builder)
///
/// Emits token-compressed chunks of literal tokens only, which any
/// spec-conformant decompressor must accept. Inputs must stay small
/// enough that flags-plus-literals fit a chunk's 4096 data bytes.
///
/// # Arguments
///
/// * `data` - The plaintext to wrap
#[cfg(test)]
pub(crate) fn compress_all_literals(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x01];
    for chunk in data.chunks(3600) {
        // one clear flag byte ahead of every eight literal bytes
        let mut body = Vec::new();
        for group in chunk.chunks(8) {
            body.push(0u8);
            body.extend_from_slice(group);
        }
        assert!(body.len() <= 4096, "test chunks must fit the chunk bound");
        // header: size-minus-3 in the low bits, 0b011 signature, compressed
        let header = 0xB000u16 | ((body.len() + 2 - 3) as u16 & 0x0FFF);
        out.extend_from_slice(&header.to_le_bytes());
        out.extend_from_slice(&body);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Literal-only containers round-trip arbitrary bytes.
    #[test]
    fn literal_chunks_round_trip() {
        // a realistic module header plus some binary-ish content
        let source = b"Attribute VB_Name = \"Module1\"\r\nSub AutoOpen()\r\n  x = 1\r\nEnd Sub\r\n";
        let compressed = compress_all_literals(source);
        let out = decompress_container(&compressed, MAX_SOURCE_BYTES, &mut 0).expect("round trip");
        assert_eq!(out, source);
        // multi-chunk inputs reassemble in order
        let big: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
        let compressed = compress_all_literals(&big);
        let out = decompress_container(&compressed, MAX_SOURCE_BYTES, &mut 0).expect("multi chunk");
        assert_eq!(out, big);
    }

    /// Copy tokens expand back-references, including overlapping runs.
    #[test]
    fn copy_tokens_expand_references() {
        // literals a,b,c then a copy of (offset 3, length 3): "abcabc"
        // at 3 bytes produced the bit split is 4/12, so the token packs
        // (offset-1) << 12 | (length-3)
        let container = [0x01, 0x05, 0xB0, 0x08, b'a', b'b', b'c', 0x00, 0x20];
        let out = decompress_container(&container, 1024, &mut 0).expect("copy token");
        assert_eq!(out, b"abcabc");
        // literal 'a' then an overlapping copy (offset 1, length 10): 11 a's
        let container = [0x01, 0x03, 0xB0, 0x02, b'a', 0x07, 0x00];
        let out = decompress_container(&container, 1024, &mut 0).expect("overlapping copy");
        assert_eq!(out, b"aaaaaaaaaaa");
    }

    /// Raw (stored) chunks copy their literal bytes through unchanged.
    #[test]
    fn raw_chunks_pass_through() {
        // a full raw chunk is 4096 literal bytes behind a 0x3FFF header
        let payload = vec![0xAB; 4096];
        let mut container = vec![0x01, 0xFF, 0x3F];
        container.extend_from_slice(&payload);
        let out = decompress_container(&container, MAX_SOURCE_BYTES, &mut 0).expect("raw chunk");
        assert_eq!(out, payload);
    }

    /// The work meter bills exactly the bytes produced, bomb or not.
    #[test]
    fn work_meter_bills_produced_bytes_honestly() {
        // a literal round trip bills exactly its output
        let source = b"Sub AutoOpen()\r\nEnd Sub\r\n";
        let compressed = compress_all_literals(source);
        let mut work = 0u64;
        decompress_container(&compressed, MAX_SOURCE_BYTES, &mut work).expect("round trip");
        assert_eq!(work, source.len() as u64);
        // an instant failure (missing signature) bills nothing: sixteen
        // one-byte decoys must not be able to exhaust a caller's budget
        let mut work = 0u64;
        assert!(decompress_container(b"\x02junk", 1024, &mut work).is_err());
        assert_eq!(work, 0);
        // a bomb that errors mid-chunk bills the work it actually did: one
        // literal plus a 4098-byte overlapping copy breaches the chunk bound
        // only AFTER producing 4099 bytes
        let bomb = [0x01, 0x03, 0xB0, 0x02, b'a', 0xFF, 0x0F];
        let mut work = 0u64;
        assert!(decompress_container(&bomb, 1024, &mut work).is_err());
        assert_eq!(work, 4099);
    }

    /// Hostile and truncated containers error instead of panicking.
    #[test]
    fn malformed_containers_error() {
        // a missing signature byte is refused up front
        assert!(decompress_container(b"\x02junk", 1024, &mut 0).is_err());
        // a truncated chunk header is named
        assert!(decompress_container(&[0x01, 0x05], 1024, &mut 0).is_err());
        // a header promising more bytes than remain is refused
        assert!(decompress_container(&[0x01, 0xFF, 0xB0, 0x00], 1024, &mut 0).is_err());
        // a copy token with nothing produced yet is refused
        let container = [0x01, 0x02, 0xB0, 0x01, 0x00, 0x00];
        assert!(decompress_container(&container, 1024, &mut 0).is_err());
        // a truncated copy token is refused
        let container = [0x01, 0x02, 0xB0, 0x02, b'a', 0x07];
        assert!(decompress_container(&container, 1024, &mut 0).is_err());
        // the output cap stops a decompression early
        let big: Vec<u8> = vec![b'x'; 2048];
        let compressed = compress_all_literals(&big);
        assert!(decompress_container(&compressed, 100, &mut 0).is_err());
    }

    /// Build a minimal dir stream from (id, data) records.
    ///
    /// # Arguments
    ///
    /// * `records` - The records to serialize in order
    fn build_dir(records: &[(u16, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (id, data) in records {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
        }
        out
    }

    /// The dir parse collects module names, stream names, and offsets.
    #[test]
    fn dir_stream_parses_modules() {
        // PROJECTVERSION uses the irregular fixed-6-byte layout
        let mut dir = Vec::new();
        dir.extend_from_slice(&0x0009u16.to_le_bytes());
        dir.extend_from_slice(&4u32.to_le_bytes());
        dir.extend_from_slice(&[1, 0, 0, 0, 2, 0]);
        dir.extend_from_slice(&build_dir(&[
            (0x0004, b"EvilProject"),
            (0x0019, b"Module1"),
            (0x001A, b"Module1Stream"),
            (0x0031, &5u32.to_le_bytes()),
            (0x0019, b"ThisDocument"),
            (0x0031, &0u32.to_le_bytes()),
            (0x0010, b""),
        ]));
        let parsed = parse_dir_stream(&dir);
        assert!(parsed.caveat.is_none());
        assert_eq!(parsed.project_name.as_deref(), Some("EvilProject"));
        assert_eq!(parsed.modules.len(), 2);
        assert_eq!(parsed.modules[0].name, "Module1");
        assert_eq!(parsed.modules[0].stream_name, "Module1Stream");
        assert_eq!(parsed.modules[0].text_offset, Some(5));
        // the second module fell back to no stream name and offset 0
        assert_eq!(parsed.modules[1].name, "ThisDocument");
        assert!(parsed.modules[1].stream_name.is_empty());
        assert_eq!(parsed.modules[1].text_offset, Some(0));
    }

    /// The Unicode name records win over the lossy code-page decodes.
    #[test]
    fn dir_stream_prefers_unicode_names() {
        // the cp1251 bytes for a cyrillic name mangle under a UTF-8 decode;
        // the Unicode halves carry the authoritative UTF-16 spellings
        let mbcs = [0xCCu8, 0xEE, 0xE4, 0xF3, 0xEB, 0xFC];
        let name_utf16: Vec<u8> = "Модуль"
            .encode_utf16()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        let dir = build_dir(&[
            (0x0019, &mbcs),
            (0x0047, &name_utf16),
            (0x001A, &mbcs),
            (0x0032, &name_utf16),
            (0x0031, &0u32.to_le_bytes()),
            (0x0010, b""),
        ]);
        let parsed = parse_dir_stream(&dir);
        assert_eq!(parsed.modules.len(), 1);
        // the display name and the stream lookup both use the UTF-16 spelling
        assert_eq!(parsed.modules[0].name, "Модуль");
        assert_eq!(parsed.modules[0].stream_name_unicode, "Модуль");
        // the lossy code-page decode is kept as the fallback spelling
        assert!(parsed.modules[0].stream_name.contains('\u{FFFD}'));
    }

    /// Hostile multi-megabyte names are bounded at parse time.
    #[test]
    fn dir_stream_bounds_hostile_names() {
        // a 2 MB "name" must not flow into the report uncapped
        let giant = vec![b'A'; 2 * 1024 * 1024];
        let dir = build_dir(&[(0x0004, &giant), (0x0019, &giant), (0x0010, b"")]);
        let parsed = parse_dir_stream(&dir);
        let project = parsed.project_name.expect("project name");
        // the cap keeps the head plus the truncation marker
        assert_eq!(project.chars().count(), MAX_NAME_CHARS + 1);
        assert!(project.ends_with('…'));
        assert_eq!(parsed.modules[0].name.chars().count(), MAX_NAME_CHARS + 1);
    }

    /// A dir record lying about its size stops the walk but keeps the prefix.
    #[test]
    fn dir_stream_survives_lying_sizes() {
        // one good module, then a record declaring more bytes than remain
        let mut dir = build_dir(&[
            (0x0019, b"Module1"),
            (0x001A, b"Module1"),
            (0x0031, &0u32.to_le_bytes()),
        ]);
        dir.extend_from_slice(&0x0019u16.to_le_bytes());
        dir.extend_from_slice(&400u32.to_le_bytes());
        dir.extend_from_slice(b"short");
        let parsed = parse_dir_stream(&dir);
        // the module declared before the lie is kept, with the stop explained
        assert_eq!(parsed.modules.len(), 1);
        assert_eq!(parsed.modules[0].name, "Module1");
        let caveat = parsed.caveat.expect("the lie should be surfaced");
        assert!(caveat.contains("declares 400 bytes"));
        assert!(caveat.contains("extract_file"));
    }

    /// The note cap suppresses hostile floods but keeps a marker.
    #[test]
    fn push_note_caps_hostile_floods() {
        let mut notes = Vec::new();
        // flood far past the cap
        for index in 0..(MAX_NOTES * 3) {
            push_note(&mut notes, format!("note {index}"));
        }
        // the cap plus exactly one suppression marker survive
        assert_eq!(notes.len(), MAX_NOTES + 1);
        assert!(notes.last().expect("marker").contains("suppressed"));
    }

    /// Build one BIFF record from its opcode and data.
    ///
    /// # Arguments
    ///
    /// * `opcode` - The record opcode
    /// * `data` - The record payload
    fn biff_record(opcode: u16, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&opcode.to_le_bytes());
        out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    /// Build a BIFF8 BOUNDSHEET record.
    ///
    /// # Arguments
    ///
    /// * `state` - The visibility state (0/1/2)
    /// * `sheet_type` - The sheet type byte (0x01 = macro sheet)
    /// * `name` - The 8-bit sheet name
    fn boundsheet8(state: u8, sheet_type: u8, name: &str) -> Vec<u8> {
        let mut data = vec![0u8; 4];
        data.push(state);
        data.push(sheet_type);
        data.push(name.len() as u8);
        // the flags byte: 8-bit (compressed) characters
        data.push(0);
        data.extend_from_slice(name.as_bytes());
        biff_record(0x0085, &data)
    }

    /// The XLM scan names macro sheets with their visibility.
    #[test]
    fn workbook_scan_flags_macro_sheets() {
        // a BIFF8 BOF, a plain worksheet, and a very-hidden macro sheet
        let mut stream = biff_record(0x0809, &[0x00, 0x06, 0x05, 0x00]);
        stream.extend_from_slice(&boundsheet8(0, 0x00, "Sheet1"));
        stream.extend_from_slice(&boundsheet8(2, 0x01, "Macro1"));
        let scan = scan_workbook("Workbook", &stream);
        assert_eq!(scan.biff, "biff8");
        assert_eq!(scan.total_sheets, 2);
        assert_eq!(scan.macro_sheets.len(), 1);
        assert_eq!(scan.macro_sheets[0].name, "Macro1");
        assert_eq!(scan.macro_sheets[0].visibility, "very-hidden");
        assert!(
            scan.note
                .as_deref()
                .is_some_and(|note| note.contains("extract_file"))
        );
        // a macro-less workbook reports zero macro sheets and no note
        let plain = boundsheet8(0, 0x00, "Sheet1");
        let scan = scan_workbook("Workbook", &plain);
        assert!(scan.macro_sheets.is_empty());
        assert!(scan.note.is_none());
        // a truncated record ends the scan with a caveat, not a panic
        let mut broken = boundsheet8(0, 0x01, "Macro1");
        broken.extend_from_slice(&[0x85, 0x00, 0xFF, 0xFF, 0x01]);
        let scan = scan_workbook("Workbook", &broken);
        assert_eq!(scan.macro_sheets.len(), 1);
        assert!(
            scan.note
                .as_deref()
                .is_some_and(|note| note.contains("truncated"))
        );
    }
}
