//! File identity — what the sample **is**, as opposed to what it does.
//!
//! Everything in this module is a *fact* read off the bytes: hashes, size,
//! entropy, the section table, the import hash, and whatever the executable
//! header declares. None of it carries suspicion and none of it produces a
//! finding. That separation matters: a high-entropy section or a zeroed build
//! timestamp is a measurement, and turning a measurement into an accusation is
//! how a report ends up asserting things it cannot support. The detections that
//! *do* draw conclusions (see [`crate::thorpal::detect::detections::defense_evasion`]) read these
//! same facts and combine several of them before saying anything.
//!
//! Two sources are combined:
//!
//! * **Ghidra's memory map** ([`AnalysisContext::sections`]) — the loaded,
//!   virtual view. Format-agnostic, and the only place permissions and real
//!   loaded bytes (hence per-section entropy) come from.
//! * **The raw file** ([`AnalysisContext::raw_bytes`]) — what Ghidra's loaded
//!   image is not. Ghidra drops the format header, drops anything appended past
//!   the last mapped section, and re-lays sections at their virtual addresses, so
//!   the declared build timestamp, the on-disk section extents, the import table
//!   order, and the overlay are only visible here.
//!
//! Header parsing is a direct read of the handful of fields that matter, gated on
//! the format Ghidra detected — deliberately not a full format library. It is
//! written to give up (return `None`) the moment anything is inconsistent rather
//! than to guess, because a malformed header is itself common in this corpus and
//! a wrong answer is worse than no answer.

use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

use crate::thorpal::detect::context::{AnalysisContext, Section};
use crate::thorpal::detect::decode::shannon_entropy;

/// The maximum number of section-table entries parsed out of a header.
///
/// A well-formed image has a few dozen; a corrupt or hostile header can declare
/// 65535, each costing a bounds-checked read.
const MAX_HEADER_SECTIONS: usize = 256;

/// The maximum number of import-table entries folded into the import hash.
///
/// Bounds a malformed import directory that claims an unterminated thunk array.
const MAX_IMPORTS: usize = 4096;

/// One section as the report presents it: the loaded view, plus its on-disk
/// extent when the header could be read.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SectionSummary {
    /// The block name (`.text`, `__TEXT`, …).
    pub name: String,
    /// The virtual address the block is loaded at.
    pub address: String,
    /// The number of bytes the block occupies in memory.
    pub virtual_size: u64,
    /// The number of bytes the block occupies in the file, when the header
    /// declared one for a section of this name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_size: Option<u64>,
    /// The block's permissions as Ghidra reports them (e.g. `rwx`).
    pub permissions: String,
    /// Whether the block carries bytes on disk (an uninitialized block reserves a
    /// virtual range and nothing else).
    pub initialized: bool,
    /// The Shannon entropy of the block's bytes, for initialized blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entropy: Option<f32>,
    /// Whether the entropy above was measured over only the leading part of the
    /// block, because reading it hit the per-block byte cap.
    ///
    /// Worth stating rather than hiding: an entropy figure over a prefix answers a
    /// different question than one over the whole block, and a packed payload is
    /// exactly the case where the block is large enough for the cap to bite.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub entropy_partial: bool,
}

/// Data appended to the file past the end of every section the header declares.
///
/// Entirely ordinary on its own — installers, self-extracting archives, and
/// signed binaries all carry one — but it is where an appended payload lives, so
/// its size and entropy are worth stating.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OverlayInfo {
    /// The file offset the overlay begins at.
    pub offset: u64,
    /// The number of bytes past that offset.
    pub size: u64,
    /// The Shannon entropy of those bytes.
    pub entropy: f32,
}

/// Everything the report knows about the sample as a file.
///
/// [`Default`] yields the empty identity — every hash blank and no sections — for
/// the case where the sample's bytes could not be read at all.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct FileIdentity {
    /// The file size in bytes.
    pub size: u64,
    /// The MD5 of the file bytes.
    pub md5: String,
    /// The SHA-1 of the file bytes.
    pub sha1: String,
    /// The SHA-256 of the file bytes.
    pub sha256: String,
    /// The SHA-512 of the file bytes.
    pub sha512: String,
    /// The Shannon entropy of the whole file (bits per byte, 0.0–8.0).
    pub entropy: f32,
    /// The build timestamp the header declares, as an ISO-8601 UTC string.
    ///
    /// Absent when the format carries no timestamp, when the header could not be
    /// read, or when the field is zero — reproducible-build toolchains zero it
    /// deliberately, so an absent timestamp is not by itself an anomaly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_timestamp: Option<String>,
    /// The import hash: MD5 over the ordered, lower-cased `library.function` list
    /// from the import table.
    ///
    /// The standard construction, so it is comparable with the value other tools
    /// report for the same file. Absent for formats with no import directory and
    /// for images whose import table could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imphash: Option<String>,
    /// Data appended past the last section, when there is any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay: Option<OverlayInfo>,
    /// The section table, in load order.
    pub sections: Vec<SectionSummary>,
}

/// One section exactly as the executable header declares it.
///
/// The *declared* view, deliberately kept separate from Ghidra's loaded view
/// ([`crate::thorpal::detect::context::Section`]). The two disagree in exactly the cases worth
/// noticing: a section whose declared on-disk size is zero but whose virtual size
/// is megabytes is the shape a runtime unpacker leaves behind, and the loaded view
/// shows only the megabytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderSection {
    /// The declared name, with any padding trimmed.
    pub name: String,
    /// The address the section is mapped at, in whatever space the format uses
    /// (a PE RVA, an ELF virtual address) — consistent with
    /// [`FormatHeader::entry_point`] for the same file.
    pub address: u64,
    /// The number of bytes the section occupies once mapped.
    pub virtual_size: u64,
    /// The file offset the section's bytes start at.
    pub raw_offset: u64,
    /// The number of bytes the section occupies in the file. Zero means the
    /// section is mapped but carries nothing on disk.
    pub raw_size: u64,
    /// The format's section flags (PE characteristics), or zero where the format
    /// has no equivalent.
    pub flags: u32,
}

/// The subset of the executable header this tool reads.
///
/// Built by [`parse_header`]; shared with the packing detection, which draws
/// conclusions from the same fields.
#[derive(Debug, Clone, Default)]
pub struct FormatHeader {
    /// The declared build timestamp (Unix epoch seconds), when the format has one
    /// and it is non-zero.
    pub timestamp: Option<u32>,
    /// The declared entry point, in the same address space as
    /// [`HeaderSection::address`].
    pub entry_point: Option<u64>,
    /// The section table exactly as declared, in table order.
    pub sections: Vec<HeaderSection>,
    /// The ordered, lower-cased `library.function` import list backing the import
    /// hash. Empty when the format has no import directory.
    pub imports: Vec<String>,
    /// The security directory's `(file offset, size)`, when a PE declares one.
    ///
    /// Unlike every other data directory, entry 4 holds a FILE OFFSET, not an
    /// RVA — the certificate table is never mapped.
    pub security_dir: Option<(u64, u64)>,
    /// The file offset of the PE optional header's CheckSum field.
    pub checksum_offset: Option<u64>,
    /// The file offset of the security data-directory entry itself (8 bytes).
    ///
    /// The Authenticode digest excludes the checksum, this entry, and the
    /// certificate table; keeping all three offsets here spares consumers a
    /// re-parse.
    pub cert_dir_entry_offset: Option<u64>,
    /// The debug directory's `(file offset, size)`, resolved from its RVA.
    pub debug_dir: Option<(u64, u64)>,
}

impl FormatHeader {
    /// The file offset one byte past the last declared section's on-disk extent.
    ///
    /// Returns `None` when the header declared no sections with any content.
    pub fn image_end(&self) -> Option<u64> {
        // the largest offset+size over every declared section with real content
        self.sections
            .iter()
            .filter(|section| section.raw_size > 0)
            .map(|section| section.raw_offset.saturating_add(section.raw_size))
            .max()
    }

    /// The section containing the declared entry point, if the header declares
    /// both and they are consistent.
    pub fn entry_section(&self) -> Option<&HeaderSection> {
        let entry = self.entry_point?;
        // sections do not overlap, so the first containing entry is the answer
        self.sections.iter().find(|section| {
            entry >= section.address && entry < section.address.saturating_add(section.virtual_size)
        })
    }

    /// The import hash over [`imports`](FormatHeader::imports), when there is one.
    pub fn imphash(&self) -> Option<String> {
        // no import directory (or an empty one) means no import hash
        if self.imports.is_empty() {
            return None;
        }
        // the standard construction: MD5 over the comma-joined entries
        Some(hex(
            Md5::digest(self.imports.join(",").as_bytes()).as_slice()
        ))
    }
}

/// Collect the sample's file identity.
///
/// Infallible by design: every input is optional enrichment, so a missing memory
/// map or an unreadable header costs the corresponding fields and nothing else.
/// The hashes and entropy are the only part that can fail outright, and only if
/// the sample cannot be read at all — in which case the caller already has bigger
/// problems and gets zeroed hashes plus a warning.
///
/// # Arguments
///
/// * `ctx` - The analysis context (raw bytes + memory map)
#[tracing::instrument(name = "fileinfo::collect", skip_all)]
pub async fn collect(ctx: &AnalysisContext) -> FileIdentity {
    // the raw file: hashes, entropy, header, overlay
    let raw = match ctx.raw_bytes().await {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(?error, "sample bytes unavailable; file identity is empty");
            Default::default()
        }
    };
    // the loaded view: names, permissions, per-section entropy
    let sections = match ctx.sections().await {
        Ok(sections) => sections,
        Err(error) => {
            tracing::warn!(?error, "memory map unavailable; omitting the section table");
            Vec::new()
        }
    };
    // read whatever the format header declares (None when it cannot be trusted)
    let header = parse_header(&raw, ctx.program.executable_format.as_deref());
    build(&raw, &sections, header.as_ref())
}

/// Assemble the identity from already-gathered inputs.
///
/// Split out from [`collect`] so it can be unit-tested without a Ghidra bridge.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `sections` - The loaded memory map
/// * `header` - The parsed format header, when one could be read
fn build(raw: &[u8], sections: &[Section], header: Option<&FormatHeader>) -> FileIdentity {
    // index the header's declared on-disk extents by section name so the loaded
    // blocks below can pick up their raw sizes
    let raw_sizes: Vec<(&str, u64)> = header
        .map(|header| {
            header
                .sections
                .iter()
                .map(|section| (section.name.as_str(), section.raw_size))
                .collect()
        })
        .unwrap_or_default();
    // render each loaded block, pairing it with its declared on-disk extent
    let sections = sections
        .iter()
        .map(|section| SectionSummary {
            name: section.name.clone(),
            address: crate::thorpal::detect::context::fmt_addr(section.base),
            virtual_size: section.size,
            // match on the trimmed name: header names are NUL-padded fixed fields
            raw_size: raw_sizes
                .iter()
                .find(|(name, _)| *name == section.name.trim())
                .map(|(_, size)| *size),
            permissions: section.permissions.clone(),
            initialized: section.is_initialized,
            entropy: section.entropy(),
            entropy_partial: section.truncated,
        })
        .collect();
    FileIdentity {
        size: raw.len() as u64,
        md5: hex(Md5::digest(raw).as_slice()),
        sha1: hex(Sha1::digest(raw).as_slice()),
        sha256: hex(Sha256::digest(raw).as_slice()),
        sha512: hex(Sha512::digest(raw).as_slice()),
        entropy: shannon_entropy(raw),
        build_timestamp: header.and_then(|h| h.timestamp).map(format_timestamp),
        imphash: header.and_then(FormatHeader::imphash),
        overlay: header.and_then(|h| overlay(raw, h)),
        sections,
    }
}

/// Describe the bytes past the last declared section, when there are any.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `header` - The parsed format header
fn overlay(raw: &[u8], header: &FormatHeader) -> Option<OverlayInfo> {
    let end = header.image_end()?;
    // nothing past the image (or a header that declares more than the file holds,
    // which is a malformed image rather than an overlay)
    if end >= raw.len() as u64 {
        return None;
    }
    let bytes = &raw[end as usize..];
    Some(OverlayInfo {
        offset: end,
        size: bytes.len() as u64,
        entropy: shannon_entropy(bytes),
    })
}

/// Parse the executable header for the fields this tool reads.
///
/// Dispatches on the format Ghidra detected and returns `None` for formats with
/// no header worth reading (or when the header is inconsistent with the bytes).
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `format` - Ghidra's `executable_format` string, when known
pub fn parse_header(raw: &[u8], format: Option<&str>) -> Option<FormatHeader> {
    // normalize the format string once for the substring tests below
    let format = format?.to_ascii_lowercase();
    // PE and ELF are the two formats whose headers carry fields worth reading;
    // Mach-O and the raw-binary loader fall through to the loaded view alone
    if format.contains("portable executable") || format.contains("pe ") {
        return parse_pe(raw);
    }
    if format.contains("elf") {
        return parse_elf(raw);
    }
    None
}

// ===========================================================================
// PE
// ===========================================================================

/// Parse the PE fields this tool reads: build timestamp, section extents, imports.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
fn parse_pe(raw: &[u8]) -> Option<FormatHeader> {
    // the DOS stub points at the PE signature
    if read_u16(raw, 0)? != 0x5a4d {
        return None;
    }
    let pe = read_u32(raw, 0x3c)? as usize;
    if read_u32(raw, pe)? != 0x0000_4550 {
        return None;
    }
    // COFF header: section count, timestamp, and the size of the optional header
    let coff = pe.checked_add(4)?;
    let section_count = read_u16(raw, coff + 2)? as usize;
    let timestamp = read_u32(raw, coff + 4)?;
    let opt_size = read_u16(raw, coff + 16)? as usize;
    // the optional header's magic distinguishes PE32 from PE32+, which shifts
    // every field after the (32-bit-only) BaseOfData slot
    let opt = coff.checked_add(20)?;
    let plus = match read_u16(raw, opt)? {
        0x10b => false,
        0x20b => true,
        // ROM images and anything else are not worth guessing at
        _ => return None,
    };
    // the entry point sits at a fixed offset in both optional-header flavours
    let entry_point = read_u32(raw, opt + 16)?;
    // the section table follows the optional header
    let table = opt.checked_add(opt_size)?;
    let sections = parse_pe_sections(raw, table, section_count);
    // the import directory is entry 1 of the data directories, which start after
    // the fixed part of the optional header (112 bytes for PE32, 128 for PE32+)
    let dirs = opt + if plus { 112 } else { 96 };
    let import_rva = read_u32(raw, dirs + 8).unwrap_or(0);
    let imports = if import_rva == 0 {
        Vec::new()
    } else {
        parse_pe_imports(raw, &sections, import_rva, plus)
    };
    // how many data directories the header actually declares bounds the reads
    // below (entry 4 = security, entry 6 = debug)
    let dir_count = read_u32(raw, opt + if plus { 108 } else { 92 }).unwrap_or(0) as usize;
    // the security entry holds a FILE OFFSET (the cert table is never mapped)
    let security_dir = (dir_count > 4)
        .then(|| {
            let offset = read_u32(raw, dirs + 32)?;
            let size = read_u32(raw, dirs + 36)?;
            (offset != 0 && size != 0).then_some((u64::from(offset), u64::from(size)))
        })
        .flatten();
    // the debug entry is RVA-based like the rest and must be resolved
    let debug_dir = (dir_count > 6)
        .then(|| {
            let rva = read_u32(raw, dirs + 48)?;
            let size = read_u32(raw, dirs + 52)?;
            if rva == 0 || size == 0 {
                return None;
            }
            let offset = rva_to_offset(&sections, rva)?;
            Some((offset as u64, u64::from(size)))
        })
        .flatten();
    Some(FormatHeader {
        // a zero timestamp is "not declared", not "the epoch"
        timestamp: (timestamp != 0).then_some(timestamp),
        // likewise a zero entry point (a DLL may legitimately have none)
        entry_point: (entry_point != 0).then_some(u64::from(entry_point)),
        sections,
        imports,
        security_dir,
        // the checksum sits at a fixed offset in both optional-header flavours
        checksum_offset: Some((opt + 64) as u64),
        cert_dir_entry_offset: (dir_count > 4).then_some((dirs + 32) as u64),
        debug_dir,
    })
}

/// The PE build-timestamp facts the file-identity surface reports.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PeTimestamp {
    /// The COFF `TimeDateStamp`, when non-zero.
    pub declared: Option<u32>,
    /// Whether the debug directory marks the stamp as a reproducible-build hash.
    pub repro: bool,
}

/// Read the PE build timestamp (and its REPRO marker) from a head window.
///
/// Reuses [`parse_pe`] over the leading bytes (its import walk degrades to
/// empty when the directories sit past the head, which is fine here), then
/// follows the already-resolved debug directory with one bounded targeted
/// read to look for `IMAGE_DEBUG_TYPE_REPRO`. Returns `None` when the head
/// is not a PE.
///
/// # Arguments
///
/// * `head` - The file's leading bytes (must cover the section table)
/// * `read_at` - A bounded reader for the debug directory's file range
pub(crate) fn pe_timestamp(
    head: &[u8],
    read_at: impl FnOnce(u64, usize) -> Option<Vec<u8>>,
) -> Option<PeTimestamp> {
    // one shared parser: the header already carries the resolved debug range
    let header = parse_pe(head)?;
    let repro = header
        .debug_dir
        .filter(|(_, size)| *size <= 4096)
        .and_then(|(offset, size)| {
            // one targeted read of the (small) debug directory itself
            let entries = read_at(offset, size as usize)?;
            Some(debug_dir_has_repro(&entries))
        })
        .unwrap_or(false);
    Some(PeTimestamp {
        declared: header.timestamp,
        repro,
    })
}

/// Render a PE timestamp as a display string plus an anomaly note, when any.
///
/// The anomaly classes answer the misreads the evaluation kept hitting: a
/// reproducible-build hash read as a date, a future-dated (timestomped or
/// post-dated) stamp, a zeroed stamp, and an implausibly ancient one.
///
/// # Arguments
///
/// * `stamp` - The parsed timestamp facts
/// * `now` - The current time, as seconds since the Unix epoch
pub(crate) fn describe_timestamp(
    stamp: &PeTimestamp,
    now: u64,
) -> (Option<String>, Option<String>) {
    // a REPRO stamp is a content hash whatever its numeric value decodes to
    if stamp.repro {
        let shown = stamp.declared.map(format_timestamp);
        return (
            shown,
            Some(
                "reproducible-build hash (IMAGE_DEBUG_TYPE_REPRO), not a wall-clock \
                 time — do not read it as a date"
                    .to_owned(),
            ),
        );
    }
    match stamp.declared {
        None => (None, Some("not declared (zeroed header field)".to_owned())),
        Some(seconds) => {
            let shown = Some(format_timestamp(seconds));
            // allow a little clock skew before calling a stamp future-dated
            let anomaly = if u64::from(seconds) > now.saturating_add(172_800) {
                Some(
                    "dated in the future — a timestomp or a post-dated/REPRO-style \
                     build"
                        .to_owned(),
                )
            } else if seconds < 788_918_400 {
                Some("pre-dates 1995 — implausible for a modern binary".to_owned())
            } else {
                None
            };
            (shown, anomaly)
        }
    }
}

/// Whether a PE's debug directory declares a reproducible-build entry.
///
/// Walks the 28-byte `IMAGE_DEBUG_DIRECTORY` entries looking for type 16
/// (`IMAGE_DEBUG_TYPE_REPRO`), which marks the COFF timestamp as a content
/// hash rather than a wall-clock time (the deterministic-build convention).
///
/// # Arguments
///
/// * `entries` - The debug directory's raw bytes
pub(crate) fn debug_dir_has_repro(entries: &[u8]) -> bool {
    // each entry is 28 bytes with the Type field at offset 12
    entries
        .chunks_exact(28)
        .any(|entry| read_u32(entry, 12) == Some(16))
}

/// Read the PE section table.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `table` - The file offset of the section table
/// * `count` - The declared number of sections
fn parse_pe_sections(raw: &[u8], table: usize, count: usize) -> Vec<HeaderSection> {
    let mut out = Vec::new();
    // bound the walk: a hostile header can declare far more sections than exist
    for i in 0..count.min(MAX_HEADER_SECTIONS) {
        let entry = table + i * 40;
        // each entry is an 8-byte NUL-padded name followed by the size/address fields
        let Some(name) = raw.get(entry..entry + 8).map(section_name) else {
            break;
        };
        let (Some(vsize), Some(vaddr), Some(rsize), Some(roff), Some(flags)) = (
            read_u32(raw, entry + 8),
            read_u32(raw, entry + 12),
            read_u32(raw, entry + 16),
            read_u32(raw, entry + 20),
            read_u32(raw, entry + 36),
        ) else {
            break;
        };
        out.push(HeaderSection {
            name,
            address: u64::from(vaddr),
            // an object-file section may declare a zero virtual size and rely on
            // the raw size, so a section spans whichever is larger
            virtual_size: u64::from(vsize.max(rsize)),
            raw_offset: u64::from(roff),
            raw_size: u64::from(rsize),
            flags,
        });
    }
    out
}

/// Translate a relative virtual address to a file offset using the section table.
///
/// # Arguments
///
/// * `sections` - The declared section table
/// * `rva` - The relative virtual address to translate
fn rva_to_offset(sections: &[HeaderSection], rva: u32) -> Option<usize> {
    // find the section the RVA falls in and re-base it onto that section's file offset
    let rva = u64::from(rva);
    sections
        .iter()
        .find(|section| {
            rva >= section.address && rva < section.address.saturating_add(section.virtual_size)
        })
        .map(|section| (section.raw_offset + (rva - section.address)) as usize)
}

/// Walk the PE import directory into the ordered `library.function` list the
/// import hash is computed over.
///
/// Follows the standard construction: the library name lower-cased with a
/// `.dll`/`.ocx`/`.sys` extension stripped, each imported symbol lower-cased, and
/// an ordinal import rendered as `ord<n>`. Order is the import table's own, which
/// is why this reads the file rather than Ghidra's (symbol-table-ordered) import
/// list — a differently-ordered list hashes to a value nothing else will match.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `sections` - The section table, for RVA translation
/// * `directory_rva` - The import directory's relative virtual address
/// * `plus` - Whether this is a PE32+ image (64-bit thunks)
fn parse_pe_imports(
    raw: &[u8],
    sections: &[HeaderSection],
    directory_rva: u32,
    plus: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    let Some(directory) = rva_to_offset(sections, directory_rva) else {
        return out;
    };
    // each descriptor is 20 bytes; an all-zero descriptor terminates the array
    for i in 0..MAX_IMPORTS {
        let entry = directory + i * 20;
        let (Some(lookup_rva), Some(name_rva), Some(thunk_rva)) = (
            read_u32(raw, entry),
            read_u32(raw, entry + 12),
            read_u32(raw, entry + 16),
        ) else {
            break;
        };
        // the terminating descriptor
        if name_rva == 0 && thunk_rva == 0 {
            break;
        }
        // the library name, lower-cased with its extension stripped
        let Some(library) = rva_to_offset(sections, name_rva).and_then(|at| cstring(raw, at))
        else {
            continue;
        };
        let library = strip_module_extension(&library.to_ascii_lowercase());
        // prefer the import lookup table (never overwritten by the loader) and
        // fall back to the address table for images that only have the latter
        let thunks = if lookup_rva != 0 {
            lookup_rva
        } else {
            thunk_rva
        };
        let Some(mut thunk) = rva_to_offset(sections, thunks) else {
            continue;
        };
        // walk the thunk array to its zero terminator
        let width = if plus { 8 } else { 4 };
        while out.len() < MAX_IMPORTS {
            let value = if plus {
                read_u64(raw, thunk)
            } else {
                read_u32(raw, thunk).map(u64::from)
            };
            let Some(value) = value.filter(|value| *value != 0) else {
                break;
            };
            // the high bit marks an import by ordinal; otherwise the value is an
            // RVA to a hint/name table entry (a 2-byte hint then the name)
            let ordinal_bit = if plus { 1 << 63 } else { 1 << 31 };
            let symbol = if value & ordinal_bit != 0 {
                format!("ord{}", value & 0xffff)
            } else {
                match rva_to_offset(sections, value as u32).and_then(|at| cstring(raw, at + 2)) {
                    Some(name) => name.to_ascii_lowercase(),
                    // an unresolvable name would silently shorten the list and
                    // change the hash, so abandon the whole import table instead
                    None => return Vec::new(),
                }
            };
            out.push(format!("{library}.{symbol}"));
            thunk += width;
        }
    }
    out
}

/// Strip a module filename's extension, for the import-hash construction.
///
/// # Arguments
///
/// * `library` - The lower-cased library name
fn strip_module_extension(library: &str) -> String {
    // only the three module extensions are stripped; anything else is part of the name
    match library.rsplit_once('.') {
        Some((stem, "dll" | "ocx" | "sys")) => stem.to_string(),
        _ => library.to_string(),
    }
}

// ===========================================================================
// ELF
// ===========================================================================

/// Parse the ELF fields this tool reads: the section extents.
///
/// ELF carries no build timestamp and no import directory, so the header only
/// contributes its section table (which is what the overlay calculation needs).
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
fn parse_elf(raw: &[u8]) -> Option<FormatHeader> {
    // the ELF magic, then the class (32/64-bit) and endianness that size the rest
    if raw.get(..4)? != b"\x7fELF" {
        return None;
    }
    let wide = match raw.get(4)? {
        1 => false,
        2 => true,
        _ => return None,
    };
    // only little-endian images are parsed; a big-endian one falls back to the
    // loaded view rather than being read with the wrong byte order
    if *raw.get(5)? != 1 {
        return None;
    }
    // the entry point, then the section header table's offset, entry size, and count
    let (entry_point, shoff, shentsize, shnum, shstrndx) = if wide {
        (
            read_u64(raw, 0x18)?,
            read_u64(raw, 0x28)? as usize,
            read_u16(raw, 0x3a)? as usize,
            read_u16(raw, 0x3c)? as usize,
            read_u16(raw, 0x3e)? as usize,
        )
    } else {
        (
            u64::from(read_u32(raw, 0x18)?),
            read_u32(raw, 0x20)? as usize,
            read_u16(raw, 0x2e)? as usize,
            read_u16(raw, 0x30)? as usize,
            read_u16(raw, 0x32)? as usize,
        )
    };
    // a stripped-of-sections image (shoff == 0) still has program headers, but
    // nothing this module reads, so treat it as no header
    if shoff == 0 || shentsize == 0 {
        return None;
    }
    // the section-name string table, needed to name each section
    let strtab = elf_section_offset(raw, shoff, shentsize, shstrndx, wide);
    let mut out = Vec::new();
    for i in 0..shnum.min(MAX_HEADER_SECTIONS) {
        let entry = shoff + i * shentsize;
        let Some(name_off) = read_u32(raw, entry) else {
            break;
        };
        // sh_type SHT_NOBITS (8) reserves space without occupying the file
        let (Some(kind), Some(flags)) = (read_u32(raw, entry + 4), read_u32(raw, entry + 8)) else {
            break;
        };
        let (addr, offset, size) = if wide {
            let (Some(addr), Some(offset), Some(size)) = (
                read_u64(raw, entry + 0x10),
                read_u64(raw, entry + 0x18),
                read_u64(raw, entry + 0x20),
            ) else {
                break;
            };
            (addr, offset, size)
        } else {
            let (Some(addr), Some(offset), Some(size)) = (
                read_u32(raw, entry + 0x0c),
                read_u32(raw, entry + 0x10),
                read_u32(raw, entry + 0x14),
            ) else {
                break;
            };
            (u64::from(addr), u64::from(offset), u64::from(size))
        };
        // resolve the name out of the string table, falling back to the index
        let name = strtab
            .and_then(|base| cstring(raw, base + name_off as usize))
            .unwrap_or_else(|| format!("sh{i}"));
        out.push(HeaderSection {
            name,
            address: addr,
            virtual_size: size,
            raw_offset: offset,
            // SHT_NOBITS occupies no file space regardless of its declared size
            raw_size: if kind == 8 { 0 } else { size },
            flags,
        });
    }
    // the section header table itself sits at the end of a conventional image, so
    // it counts toward the extent an overlay is measured against
    out.push(HeaderSection {
        name: "<section headers>".to_string(),
        address: 0,
        virtual_size: 0,
        raw_offset: shoff as u64,
        raw_size: (shnum.min(MAX_HEADER_SECTIONS) * shentsize) as u64,
        flags: 0,
    });
    Some(FormatHeader {
        // ELF declares no build time
        timestamp: None,
        entry_point: (entry_point != 0).then_some(entry_point),
        sections: out,
        imports: Vec::new(),
        // the PE-only directories have no ELF equivalent here
        security_dir: None,
        checksum_offset: None,
        cert_dir_entry_offset: None,
        debug_dir: None,
    })
}

/// The file offset of an ELF section header's content.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `shoff` - The section header table's file offset
/// * `shentsize` - The size of one section header entry
/// * `index` - The section index to resolve
/// * `wide` - Whether this is a 64-bit image
fn elf_section_offset(
    raw: &[u8],
    shoff: usize,
    shentsize: usize,
    index: usize,
    wide: bool,
) -> Option<usize> {
    // sh_offset sits at 0x18 in a 64-bit entry and 0x10 in a 32-bit one
    let entry = shoff.checked_add(index.checked_mul(shentsize)?)?;
    if wide {
        read_u64(raw, entry + 0x18).map(|off| off as usize)
    } else {
        read_u32(raw, entry + 0x10).map(|off| off as usize)
    }
}

// ===========================================================================
// Byte-level helpers
// ===========================================================================

/// Read a little-endian `u16` at `at`, or `None` if it runs past the end.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
fn read_u16(raw: &[u8], at: usize) -> Option<u16> {
    // a bounds-checked slice keeps every header walk total
    raw.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

/// Read a little-endian `u32` at `at`, or `None` if it runs past the end.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
fn read_u32(raw: &[u8], at: usize) -> Option<u32> {
    raw.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Read a little-endian `u64` at `at`, or `None` if it runs past the end.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
fn read_u64(raw: &[u8], at: usize) -> Option<u64> {
    raw.get(at..at + 8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap_or_default()))
}

/// Read a NUL-terminated ASCII string at `at`.
///
/// Bounded to a sane name length and rejects anything with a non-printable byte,
/// so a mis-translated offset yields `None` rather than a garbage name.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset the string starts at
fn cstring(raw: &[u8], at: usize) -> Option<String> {
    // library and symbol names are short; anything longer is a bad offset
    let window = raw.get(at..(at + 512).min(raw.len()))?;
    let end = window.iter().position(|b| *b == 0)?;
    let text = &window[..end];
    // an empty or non-printable "name" means the offset did not land on one
    if text.is_empty() || text.iter().any(|b| !(0x20..0x7f).contains(b)) {
        return None;
    }
    Some(String::from_utf8_lossy(text).into_owned())
}

/// Decode a fixed-width, NUL-padded section name field.
///
/// # Arguments
///
/// * `field` - The raw name field
fn section_name(field: &[u8]) -> String {
    // trim the NUL padding, then render the remainder leniently
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Render bytes as a lower-case hex string.
///
/// Shared with the raw-file tools (see [`crate::thorpal::tools::file`]), which
/// render their streamed digests the same way.
///
/// # Arguments
///
/// * `bytes` - The digest bytes
pub(crate) fn hex(bytes: &[u8]) -> String {
    // two lower-case hex digits per byte, the form every tool reports hashes in
    bytes.iter().fold(String::new(), |mut out, b| {
        out.push_str(&format!("{b:02x}"));
        out
    })
}

/// Render a Unix epoch timestamp as an ISO-8601 UTC string.
///
/// # Arguments
///
/// * `seconds` - Seconds since the Unix epoch
fn format_timestamp(seconds: u32) -> String {
    // days since the epoch, and the time of day within the final day
    let (days, rem) = (i64::from(seconds) / 86_400, i64::from(seconds) % 86_400);
    let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil-from-days: shift the year to start in March so leap days land last
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    // undo the March-first shift
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = era * 400 + yoe + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but well-formed PE32 image with two sections and one import.
    ///
    /// Built byte-by-byte so the parser is exercised against a real layout rather
    /// than a mock: the assertions below would not catch a field-offset error if
    /// the fixture were produced by the same code that reads it.
    fn pe_fixture() -> Vec<u8> {
        let mut raw = vec![0u8; 0x600];
        // DOS header: "MZ" plus the e_lfanew pointer to the PE signature
        raw[0..2].copy_from_slice(b"MZ");
        raw[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        // PE signature + COFF header (2 sections, a timestamp, a 224-byte optional header)
        raw[0x80..0x84].copy_from_slice(b"PE\0\0");
        raw[0x86..0x88].copy_from_slice(&2u16.to_le_bytes());
        raw[0x88..0x8c].copy_from_slice(&1_600_000_000u32.to_le_bytes());
        raw[0x94..0x96].copy_from_slice(&224u16.to_le_bytes());
        // optional header: PE32 magic, the entry point, and the import directory
        let opt = 0x98;
        raw[opt..opt + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        raw[opt + 16..opt + 20].copy_from_slice(&0x1020u32.to_le_bytes());
        let dirs = opt + 96;
        raw[dirs + 8..dirs + 12].copy_from_slice(&0x2000u32.to_le_bytes());
        // section table: .text (RVA 0x1000, file 0x400) and .rdata (RVA 0x2000, file 0x500)
        let table = opt + 224;
        for (i, (name, vaddr, roff, flags)) in [
            (b".text\0\0\0", 0x1000u32, 0x400u32, 0x6000_0020u32),
            (b".rdata\0\0", 0x2000, 0x500, 0x4000_0040),
        ]
        .into_iter()
        .enumerate()
        {
            let entry = table + i * 40;
            raw[entry..entry + 8].copy_from_slice(name);
            raw[entry + 8..entry + 12].copy_from_slice(&0x100u32.to_le_bytes());
            raw[entry + 12..entry + 16].copy_from_slice(&vaddr.to_le_bytes());
            raw[entry + 16..entry + 20].copy_from_slice(&0x100u32.to_le_bytes());
            raw[entry + 20..entry + 24].copy_from_slice(&roff.to_le_bytes());
            raw[entry + 36..entry + 40].copy_from_slice(&flags.to_le_bytes());
        }
        // .rdata contents (file 0x500 = RVA 0x2000): one import descriptor naming
        // KERNEL32.dll, a lookup table with one named thunk, then the terminator
        let put32 = |raw: &mut Vec<u8>, at: usize, v: u32| {
            raw[at..at + 4].copy_from_slice(&v.to_le_bytes())
        };
        put32(&mut raw, 0x500, 0x2040); // OriginalFirstThunk -> lookup table
        put32(&mut raw, 0x50c, 0x2060); // Name -> "KERNEL32.dll"
        put32(&mut raw, 0x510, 0x2050); // FirstThunk
        // the lookup table at RVA 0x2040 (file 0x540): one entry, then the terminator
        put32(&mut raw, 0x540, 0x2080);
        // the hint/name entry at RVA 0x2080 (file 0x580): a 2-byte hint then the name
        raw[0x582..0x582 + 13].copy_from_slice(b"GetProcAddre\0");
        // the library name at RVA 0x2060 (file 0x560)
        raw[0x560..0x560 + 13].copy_from_slice(b"KERNEL32.dll\0");
        raw
    }

    /// The PE parser reads the timestamp, the section extents, and the imports in
    /// import-table order.
    #[test]
    fn parse_pe_reads_header_fields() {
        let header = parse_header(&pe_fixture(), Some("Portable Executable (PE)"))
            .expect("the fixture parses");
        assert_eq!(header.timestamp, Some(1_600_000_000));
        assert_eq!(
            header.sections,
            vec![
                HeaderSection {
                    name: ".text".to_string(),
                    address: 0x1000,
                    virtual_size: 0x100,
                    raw_offset: 0x400,
                    raw_size: 0x100,
                    flags: 0x6000_0020,
                },
                HeaderSection {
                    name: ".rdata".to_string(),
                    address: 0x2000,
                    virtual_size: 0x100,
                    raw_offset: 0x500,
                    raw_size: 0x100,
                    flags: 0x4000_0040,
                },
            ]
        );
        // the entry point resolves into the code section, not past the image
        assert_eq!(header.entry_point, Some(0x1020));
        assert_eq!(
            header.entry_section().map(|s| s.name.as_str()),
            Some(".text")
        );
        // the library keeps its lower-cased stem, the symbol its lower-cased name
        assert_eq!(header.imports, vec!["kernel32.getprocaddre".to_string()]);
        // the import hash is MD5 over the comma-joined list
        assert_eq!(
            header.imphash(),
            Some(hex(Md5::digest(b"kernel32.getprocaddre").as_slice()))
        );
    }

    /// A zero timestamp reads as "not declared" — reproducible-build toolchains
    /// zero it deliberately, so it must not surface as an epoch date.
    #[test]
    fn parse_pe_treats_zero_timestamp_as_absent() {
        let mut raw = pe_fixture();
        raw[0x88..0x8c].copy_from_slice(&0u32.to_le_bytes());
        let header = parse_header(&raw, Some("Portable Executable (PE)")).expect("parses");
        assert_eq!(header.timestamp, None);
    }

    /// A truncated or non-PE image yields no header rather than a guess.
    #[test]
    fn parse_pe_rejects_malformed_images() {
        // no MZ magic
        assert!(
            parse_header(
                b"not an executable at all",
                Some("Portable Executable (PE)")
            )
            .is_none()
        );
        // MZ but the e_lfanew pointer runs past the end
        let mut raw = vec![0u8; 0x40];
        raw[0..2].copy_from_slice(b"MZ");
        raw[0x3c..0x40].copy_from_slice(&0xffff_0000u32.to_le_bytes());
        assert!(parse_header(&raw, Some("Portable Executable (PE)")).is_none());
        // a format with no header worth reading
        assert!(parse_header(&pe_fixture(), Some("Mac OS X Mach-O")).is_none());
        assert!(parse_header(&pe_fixture(), None).is_none());
    }

    /// The import hash matches the standard construction, including the extension
    /// strip and the `ord<n>` rendering.
    #[test]
    fn imphash_follows_the_standard_construction() {
        let header = FormatHeader {
            imports: vec![
                "ws2_32.ord23".to_string(),
                "kernel32.loadlibrarya".to_string(),
            ],
            ..Default::default()
        };
        assert_eq!(
            header.imphash(),
            Some(hex(
                Md5::digest(b"ws2_32.ord23,kernel32.loadlibrarya").as_slice()
            ))
        );
        // no import table means no import hash, rather than the MD5 of nothing
        assert_eq!(FormatHeader::default().imphash(), None);
    }

    /// Module extensions are stripped for the hash; other trailing text is not.
    #[test]
    fn strip_module_extension_only_strips_module_extensions() {
        assert_eq!(strip_module_extension("kernel32.dll"), "kernel32");
        assert_eq!(strip_module_extension("comctl32.ocx"), "comctl32");
        assert_eq!(strip_module_extension("null.sys"), "null");
        // a version-suffixed shared object is not a module extension
        assert_eq!(strip_module_extension("libc.so.6"), "libc.so.6");
        assert_eq!(strip_module_extension("api-ms-win-crt"), "api-ms-win-crt");
    }

    /// The overlay is the run of bytes past the last declared section.
    #[test]
    fn overlay_measures_bytes_past_the_last_section() {
        let header = FormatHeader {
            sections: vec![HeaderSection {
                name: ".text".to_string(),
                address: 0x1000,
                virtual_size: 0x100,
                raw_offset: 0x400,
                raw_size: 0x100,
                flags: 0,
            }],
            ..Default::default()
        };
        // a file that ends exactly at the image end has no overlay
        assert!(overlay(&vec![0u8; 0x500], &header).is_none());
        // ...and one that runs past it reports the remainder
        let found = overlay(&vec![0u8; 0x600], &header).expect("an overlay is present");
        assert_eq!(found.offset, 0x500);
        assert_eq!(found.size, 0x100);
        // a header declaring more than the file holds is malformed, not an overlay
        assert!(overlay(&vec![0u8; 0x100], &header).is_none());
    }

    /// Timestamp anomalies classify REPRO, future, zero, and ancient stamps.
    #[test]
    fn describe_timestamp_classifies_the_misread_classes() {
        // a REPRO stamp is a hash whatever it decodes to
        let repro = PeTimestamp {
            declared: Some(0x82F3_9FFA),
            repro: true,
        };
        let (shown, anomaly) = describe_timestamp(&repro, 1_760_000_000);
        assert!(shown.is_some());
        assert!(anomaly.unwrap().contains("reproducible-build"));
        // a stamp past now (+ skew) is future-dated
        let future = PeTimestamp {
            declared: Some(2_000_000_000),
            repro: false,
        };
        let (_, anomaly) = describe_timestamp(&future, 1_700_000_000);
        assert!(anomaly.unwrap().contains("future"));
        // a zeroed stamp is "not declared"
        let zeroed = PeTimestamp {
            declared: None,
            repro: false,
        };
        let (shown, anomaly) = describe_timestamp(&zeroed, 1_700_000_000);
        assert!(shown.is_none());
        assert!(anomaly.unwrap().contains("not declared"));
        // an ordinary past stamp carries no anomaly
        let normal = PeTimestamp {
            declared: Some(1_600_000_000),
            repro: false,
        };
        let (shown, anomaly) = describe_timestamp(&normal, 1_700_000_000);
        assert!(shown.is_some());
        assert!(anomaly.is_none());
    }

    /// The REPRO walk recognizes type-16 debug entries and nothing else.
    #[test]
    fn debug_dir_repro_walk() {
        // two 28-byte entries: a CODEVIEW (type 2) then a REPRO (type 16)
        let mut entries = vec![0u8; 56];
        entries[12] = 2;
        entries[28 + 12] = 16;
        assert!(debug_dir_has_repro(&entries));
        // a lone CODEVIEW entry is not REPRO
        assert!(!debug_dir_has_repro(&entries[..28]));
    }

    /// Timestamps render as ISO-8601 UTC, including across a leap day.
    #[test]
    fn format_timestamp_renders_iso_utc() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_timestamp(1_600_000_000), "2020-09-13T12:26:40Z");
        // 2020-02-29, the leap day the shifted-year arithmetic has to get right
        assert_eq!(format_timestamp(1_582_934_400), "2020-02-29T00:00:00Z");
        // the 32-bit ceiling
        assert_eq!(format_timestamp(u32::MAX), "2106-02-07T06:28:15Z");
    }

    /// `build` folds the loaded view and the header into one identity, pairing each
    /// block with its declared on-disk extent.
    #[test]
    fn build_pairs_loaded_blocks_with_declared_extents() {
        let sections = vec![
            Section {
                name: ".text".to_string(),
                base: 0x40_1000,
                end: 0x40_1100,
                size: 0x100,
                permissions: "r-x".to_string(),
                is_initialized: true,
                is_loaded: true,
                bytes: vec![0x90; 0x100],
                truncated: false,
            },
            Section {
                name: ".bss".to_string(),
                base: 0x40_2000,
                end: 0x40_3000,
                size: 0x1000,
                permissions: "rw-".to_string(),
                is_initialized: false,
                is_loaded: true,
                bytes: Vec::new(),
                truncated: false,
            },
        ];
        let header = FormatHeader {
            timestamp: Some(1_600_000_000),
            entry_point: Some(0x1000),
            sections: vec![HeaderSection {
                name: ".text".to_string(),
                address: 0x1000,
                virtual_size: 0x100,
                raw_offset: 0x400,
                raw_size: 0x100,
                flags: 0,
            }],
            imports: vec!["kernel32.loadlibrarya".to_string()],
            ..Default::default()
        };
        let identity = build(&vec![0u8; 0x500], &sections, Some(&header));
        assert_eq!(identity.size, 0x500);
        // every hash is present and the right width
        assert_eq!(identity.md5.len(), 32);
        assert_eq!(identity.sha1.len(), 40);
        assert_eq!(identity.sha256.len(), 64);
        assert_eq!(identity.sha512.len(), 128);
        assert_eq!(
            identity.build_timestamp.as_deref(),
            Some("2020-09-13T12:26:40Z")
        );
        assert!(identity.imphash.is_some());
        // the initialized block gets its declared raw size and an entropy reading
        assert_eq!(identity.sections[0].raw_size, Some(0x100));
        assert_eq!(identity.sections[0].entropy, Some(0.0));
        // the uninitialized block has neither — and is still reported, because a
        // block that reserves space without carrying bytes is itself a signal
        assert_eq!(identity.sections[1].raw_size, None);
        assert_eq!(identity.sections[1].entropy, None);
        assert!(!identity.sections[1].initialized);
    }

    /// With no header, the identity still carries the hashes and the loaded view.
    #[test]
    fn build_survives_without_a_header() {
        let identity = build(b"abcd", &[], None);
        assert_eq!(identity.size, 4);
        assert_eq!(identity.md5, "e2fc714c4727ee9395f324cd2e7f331f");
        assert_eq!(identity.build_timestamp, None);
        assert_eq!(identity.imphash, None);
        assert!(identity.overlay.is_none());
        assert!(identity.sections.is_empty());
    }
}
