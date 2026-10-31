//! Embedded foreign-format content inside the sample's raw bytes.
//!
//! A blinded evaluation run concluded a trojanized host was benign while a
//! PNG grafted into its `.rsrc` carried the implant's payload — a signal the
//! toolbox's binwalk had surfaced in one line. This detection is the native
//! equivalent: a magic scan over the raw file (binwalk-style) that validates
//! the structural header behind every magic before reporting it, plus a PNG
//! chunk/CRC validation (pngcheck-style) gated on a plausible first chunk, so
//! an embedded archive, executable, or structurally anomalous image is a
//! seeded finding instead of a missed lead while comparison immediates and
//! format-recognition constants in code never report. Validated archives and
//! executables are measured so one lead covers the whole artifact (members
//! included), and a flood of small clean resource images collapses into one
//! rollup lead. File-level findings are exempt from the runtime-noise gate by
//! construction.

use anyhow::Result;

use crate::thorpal::detect::context::AnalysisContext;
use crate::thorpal::detect::detections::{
    AttackTechnique, Detection, DetectionHit, EvidenceKind, HitSource, low, medium,
};
use crate::thorpal::detect::fileinfo::{FormatHeader, parse_header};

use super::T1027_009;

/// The most individually-rendered embedded-content hits emitted per run.
///
/// The clean-resource-image rollup rides outside this cap so an icon flood
/// never displaces executable or archive leads.
const MAX_EMBEDDED_HITS: usize = 16;

/// The most artifacts one scan collects across every kind.
///
/// Bounds the cap-exempt image collection on hostile files stuffed with tiny
/// valid images.
const MAX_SCANNED_ARTIFACTS: usize = 256;

/// Clean resource PNGs at least this large always stay individual leads.
const PNG_ROLLUP_CUTOFF: u64 = 16 * 1024;

/// The fewest clean resource PNGs that collapse into one rollup lead.
const PNG_ROLLUP_MIN: usize = 3;

/// The published COFF machine ids a real PE file header can carry.
///
/// In order: x86/x64, the ARM family (ARM, Thumb, ARMNT, ARM64, ARM64EC,
/// ARM64X), Itanium, EFI bytecode, the MIPS family (R4000, WCE MIPS v2,
/// MIPS16, MIPS FPU, MIPS16 FPU), PowerPC (plain and FPU), the `SuperH` family
/// (SH3, SH3 DSP, SH4, SH5), M32R, AM33, Alpha, RISC-V (32/64/128), and
/// `LoongArch` (32/64).
const KNOWN_COFF_MACHINES: [u16; 29] = [
    0x014c, 0x8664, 0x01c0, 0x01c2, 0x01c4, 0xaa64, 0xa641, 0xa64e, 0x0200, 0x0ebc, 0x0166, 0x0169,
    0x0266, 0x0366, 0x0466, 0x01f0, 0x01f1, 0x01a2, 0x01a3, 0x01a6, 0x01a8, 0x9041, 0x01d3, 0x0184,
    0x5032, 0x5064, 0x5128, 0x6232, 0x6264,
];

/// The embedded-content detection.
pub struct EmbeddedContent;

/// One embedded artifact found in the raw file.
struct Embedded {
    /// The file offset the magic was found at.
    offset: u64,
    /// What the magic identifies ("PNG image", "ZIP archive", ...).
    kind: &'static str,
    /// The measured or declared size in bytes, when the format's framing
    /// yields one.
    size: Option<u64>,
    /// The member count, when the artifact is an archive measured member by
    /// member.
    members: Option<usize>,
    /// Structural anomalies ("IDAT CRC mismatch", "trailing data after IEND").
    anomalies: Vec<String>,
}

impl Embedded {
    /// Whether this artifact has the shape the resource rollup collapses: a
    /// clean, measured PNG under the individual-lead size cutoff.
    ///
    /// The section check (the rollup takes only `.rsrc` images) happens at
    /// render time, where the parsed header is available.
    fn rollup_shape(&self) -> bool {
        // clean, measured, and small — anomalous or large images stay individual
        self.kind == "PNG image"
            && self.anomalies.is_empty()
            && self.size.is_some_and(|size| size < PNG_ROLLUP_CUTOFF)
    }
}

#[async_trait::async_trait]
impl Detection for EmbeddedContent {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "embedded_content"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1027_009
    }

    /// Scan the raw file for embedded foreign-format content.
    #[tracing::instrument(name = "EmbeddedContent::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // the scan works on the file as it exists on disk, not the loaded image
        let raw = ctx.raw_bytes().await.unwrap_or_default();
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        // the declared section table attributes each hit to a section by offset
        let header = parse_header(&raw, ctx.program.executable_format.as_deref());
        let found = scan_embedded(&raw);
        // render bounded individual hits plus the clean-resource-image rollup
        Ok(render_hits(header.as_ref(), found))
    }
}

/// Render scanned artifacts as detection hits: bounded individual leads plus
/// one rollup lead when small, clean resource images pile up.
///
/// # Arguments
///
/// * `header` - The parsed executable header, when one parsed
/// * `found` - The embedded artifacts the scan collected, in file order
fn render_hits(header: Option<&FormatHeader>, found: Vec<Embedded>) -> Vec<DetectionHit> {
    // a rollup candidate is a clean small PNG sitting in the resource section
    let eligible = |embedded: &Embedded| {
        embedded.rollup_shape()
            && containing_section(header, embedded.offset).as_deref() == Some(".rsrc")
    };
    // roll resource icons up only when enough pile up to drown a thread
    let candidates = found.iter().filter(|embedded| eligible(embedded)).count();
    let (rollup, individual): (Vec<Embedded>, Vec<Embedded>) = if candidates >= PNG_ROLLUP_MIN {
        found.into_iter().partition(eligible)
    } else {
        (Vec::new(), found)
    };
    // render bounded individual hits; the prelink-undo filter may drop some
    let mut hits: Vec<DetectionHit> = individual
        .iter()
        .take(MAX_EMBEDDED_HITS)
        .filter_map(|embedded| build_hit(header, embedded))
        .collect();
    // the rollup rides outside the cap so it never displaces other leads
    if !rollup.is_empty() {
        hits.push(build_png_rollup(&rollup));
    }
    hits
}

/// Render one embedded artifact as a file-level detection hit, when it
/// deserves one.
///
/// Severity policy: an embedded executable or archive is suspicious anywhere;
/// an image is suspicious only when structurally anomalous or sitting outside
/// the resource section (resources legitimately contain images). An ELF match
/// inside `.gnu.prelink_undo` is dropped entirely: prelink stores a legitimate
/// copy of the pre-link ELF image there by design.
///
/// # Arguments
///
/// * `header` - The parsed executable header, when one parsed
/// * `embedded` - The embedded artifact to render
fn build_hit(header: Option<&FormatHeader>, embedded: &Embedded) -> Option<DetectionHit> {
    // place the artifact against the declared section table
    let section = containing_section(header, embedded.offset);
    // prelink keeps the pre-link ELF image in .gnu.prelink_undo by design,
    // so an ELF magic there is tooling residue, not a payload
    if embedded.kind == "ELF executable" && section.as_deref() == Some(".gnu.prelink_undo") {
        return None;
    }
    let in_rsrc = section.as_deref().is_some_and(|name| name == ".rsrc");
    let is_image = embedded.kind == "PNG image";
    // images are normal in resources; everything else embedded is a lead —
    // an anomalous image reaching here walked a plausible IHDR and at least
    // two ASCII-typed chunks (see check_png), so its anomalies describe a
    // real-but-tampered stream, not a signature constant
    let severity = if is_image && embedded.anomalies.is_empty() && in_rsrc {
        low()
    } else {
        medium()
    };
    // compose the one-line observation the agents pivot on
    let location = match &section {
        Some(name) => format!("in {name}"),
        None => "outside every declared section".to_owned(),
    };
    // a measured archive names its member count so one lead covers them all
    let members = embedded.members.map_or_else(String::new, |count| {
        format!(", {count} member{},", if count == 1 { "" } else { "s" })
    });
    let size = embedded
        .size
        .map_or_else(String::new, |size| format!(" ({size} bytes)"));
    let anomalies = if embedded.anomalies.is_empty() {
        String::new()
    } else {
        format!("; {}", embedded.anomalies.join("; "))
    };
    let content = format!(
        "{}{members} embedded {location} @ file offset {:#x}{size}{anomalies}",
        embedded.kind, embedded.offset
    );
    let reasoning = if is_image && embedded.anomalies.is_empty() && in_rsrc {
        "Resources legitimately contain images — verify this one is consumed as an \
         image (icon/bitmap load) and not parsed as data; loaders hide configs and \
         payloads inside resource images."
            .to_owned()
    } else {
        "Foreign-format content inside an executable is how droppers and loaders \
         carry their next stage. Read it with the raw file tools (read_file at this \
         offset), extract it, and pursue what consumes it before concluding the \
         host is benign."
            .to_owned()
    };
    // file-level: no containing function, the whole file is the subject
    Some(DetectionHit {
        technique: T1027_009,
        address: embedded.offset,
        function: None,
        kind: EvidenceKind::ContextOnly,
        confidence: severity.0,
        suspicion: severity.1,
        content,
        name: None,
        reasoning,
        sites: Vec::new(),
        source: HitSource::Native,
        artifact: None,
        context: None,
        // unstamped here; the registry runner names native producers
        detection: None,
        // not an API-catalog hit: no catalog name or withheld severity
        api: None,
        catalog_severity: None,
        always_noise: false,
    })
}

/// Render a flood of small, clean resource PNGs as one low-severity lead.
///
/// The body lists every offset/size pair so each rolled-up image stays
/// individually extractable.
///
/// # Arguments
///
/// * `rollup` - The clean resource PNGs being rolled up, in file order
fn build_png_rollup(rollup: &[Embedded]) -> DetectionHit {
    // clean resource images keep the severity they carry individually
    let severity = low();
    // summarize the size spread across the rolled-up images
    let smallest = rollup.iter().filter_map(|png| png.size).min().unwrap_or(0);
    let largest = rollup.iter().filter_map(|png| png.size).max().unwrap_or(0);
    let range = if smallest == largest {
        format!("{smallest} bytes")
    } else {
        format!("{smallest}–{largest} bytes")
    };
    // list every offset/size pair so each image stays extractable
    let listed = rollup
        .iter()
        .map(|png| format!("@{:#x} ({} bytes)", png.offset, png.size.unwrap_or(0)))
        .collect::<Vec<String>>()
        .join(", ");
    let content = format!(
        "{} PNG resources, {range}: {listed} — verify these are consumed as images",
        rollup.len()
    );
    // file-level: no containing function, the resource section is the subject
    DetectionHit {
        technique: T1027_009,
        address: rollup.first().map_or(0, |png| png.offset),
        function: None,
        kind: EvidenceKind::ContextOnly,
        confidence: severity.0,
        suspicion: severity.1,
        content,
        name: None,
        reasoning: "Resources legitimately contain images — verify these are consumed \
             as images (icon/bitmap loads) and not parsed as data; loaders hide \
             configs and payloads inside resource images."
            .to_owned(),
        // every rolled-up image is a backing site for the one lead
        sites: rollup.iter().map(|png| png.offset).collect(),
        source: HitSource::Native,
        artifact: None,
        context: None,
        // unstamped here; the registry runner names native producers
        detection: None,
        // not an API-catalog hit: no catalog name or withheld severity
        api: None,
        catalog_severity: None,
        always_noise: false,
    }
}

/// Scan raw bytes for embedded file magics past offset 0.
///
/// Matches PNG, ZIP, CAB, gzip, ELF, and MZ+PE magics, and keeps a match only
/// when the structural header behind the magic validates — comparison
/// immediates in format-handling code and recognition constants in data fail
/// the fields that must follow the magic. Offset 0 (the host itself) is
/// excluded, and matches inside an already-measured span dedupe into the
/// artifact that owns them.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
fn scan_embedded(raw: &[u8]) -> Vec<Embedded> {
    let mut out: Vec<Embedded> = Vec::new();
    // rollup-shaped images are exempt from the hit cap, so count the
    // individually-rendered artifacts separately
    let mut capped = 0usize;
    // walk the file once; magic sets are tiny so a linear scan is fine
    let mut at = 1usize;
    while at + 2 <= raw.len() {
        // skip offsets inside the previously measured artifact — but never
        // honor a declared size reaching past the file, so a hostile header
        // with a huge size field cannot blind the scan to what follows it
        let skip_to = out.last().and_then(|last| {
            let end = usize::try_from(last.offset.saturating_add(last.size?)).ok()?;
            (end <= raw.len() && at < end).then_some(end)
        });
        if let Some(end) = skip_to {
            at = end;
            continue;
        }
        let window = &raw[at..];
        let found = if window.starts_with(b"\x89PNG\r\n\x1a\n") {
            check_png(raw, at)
        } else if window.starts_with(b"PK\x03\x04") && valid_zip_lfh(raw, at) {
            // measure the whole archive so one lead covers every member
            let (size, members) = zip_span(raw, at);
            Some(Embedded {
                offset: at as u64,
                kind: "ZIP archive",
                size,
                // a walk that measured no members leaves the count unsaid
                members: (members > 0).then_some(members),
                anomalies: Vec::new(),
            })
        } else if window.starts_with(b"MSCF") {
            // the fixed cabinet-header fields must hold — a cmp immediate
            // against 'MSCF' carries code bytes where reserved1 belongs
            cab_size(raw, at).map(|size| Embedded {
                offset: at as u64,
                kind: "CAB archive",
                size: Some(size),
                members: None,
                anomalies: Vec::new(),
            })
        } else if window.starts_with(b"\x1f\x8b\x08") && valid_gzip_at(raw, at) {
            Some(Embedded {
                offset: at as u64,
                kind: "gzip stream",
                size: None,
                members: None,
                anomalies: Vec::new(),
            })
        } else if window.starts_with(b"\x7fELF") && valid_elf_at(raw, at) {
            Some(Embedded {
                offset: at as u64,
                kind: "ELF executable",
                // the section-header table customarily closes an ELF image
                size: elf_span(raw, at),
                members: None,
                anomalies: Vec::new(),
            })
        } else if window.starts_with(b"MZ") && embedded_pe_at(raw, at) {
            Some(Embedded {
                offset: at as u64,
                kind: "PE executable",
                // the section table's raw extents bound the on-disk image
                size: pe_raw_span(raw, at),
                members: None,
                anomalies: Vec::new(),
            })
        } else {
            None
        };
        match found {
            Some(embedded) => {
                // every artifact anchors at the current scan offset, so resume
                // one byte past it; the measured-span skip advances further
                at += 1;
                // rollup-shaped images never count against the hit cap, so an
                // icon flood cannot displace executable or archive hits
                if !embedded.rollup_shape() {
                    capped += 1;
                }
                out.push(embedded);
                // the caps bound hostile files stuffed with magic bytes
                if capped >= MAX_EMBEDDED_HITS || out.len() >= MAX_SCANNED_ARTIFACTS {
                    break;
                }
            }
            None => at += 1,
        }
    }
    out
}

/// Whether a structurally valid ZIP local-file-header sits at `at`.
///
/// Bare `PK\x03\x04` immediates in archive-handling code carry code bytes
/// where the header fields belong; a real member header has a sane
/// version-needed, a method from APPNOTE's closed set, and a printable member
/// name that fits inside the file.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `at` - The offset the `PK\x03\x04` magic sits at
fn valid_zip_lfh(raw: &[u8], at: usize) -> bool {
    // version-needed-to-extract stays small in every real archive
    if read_le_u16(raw, at + 4).is_none_or(|version| version > 100) {
        return false;
    }
    // the compression method comes from APPNOTE's closed set
    let Some(method) = read_le_u16(raw, at + 8) else {
        return false;
    };
    if !matches!(method, 0 | 8 | 9 | 12 | 14 | 93..=99) {
        return false;
    }
    // the member name is 1..=512 bytes and must fit inside the file
    let Some(name_len) = read_le_u16(raw, at + 26) else {
        return false;
    };
    if !(1..=512).contains(&name_len) {
        return false;
    }
    let Some(name) = raw.get(at + 30..at + 30 + usize::from(name_len)) else {
        return false;
    };
    // member names are paths: printable bytes only (UTF-8 names stay allowed)
    name.iter().all(|byte| !byte.is_ascii_control())
}

/// Measure a validated embedded ZIP archive: its byte span and member count.
///
/// Walks the member chain by each local-file-header's declared extents, then
/// closes the span at the end-of-central-directory record. Streaming members
/// (general-purpose bit 3) keep their sizes in trailing data descriptors and
/// break the walk, so the EOCD span covers the members past the break.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `start` - The offset the archive's first local-file-header sits at
fn zip_span(raw: &[u8], start: usize) -> (Option<u64>, usize) {
    // walk consecutive local-file-headers by their declared extents
    let mut at = start;
    let mut members = 0usize;
    let mut streaming = false;
    loop {
        // each iteration stands on a candidate local-file-header
        if raw
            .get(at..at + 4)
            .is_none_or(|magic| magic != b"PK\x03\x04")
            || !valid_zip_lfh(raw, at)
        {
            break;
        }
        // bit 3 keeps the sizes in a data descriptor after the member data:
        // the member is real but the walk cannot chain past it
        if read_le_u16(raw, at + 6).unwrap_or(0) & 0x0008 != 0 {
            members += 1;
            streaming = true;
            break;
        }
        // the next header sits past the fixed header, name, extra, and data
        let compressed = read_le_u32(raw, at + 18).unwrap_or(0) as usize;
        let name_len = usize::from(read_le_u16(raw, at + 26).unwrap_or(0));
        let extra_len = usize::from(read_le_u16(raw, at + 28).unwrap_or(0));
        let Some(next) = at.checked_add(30 + name_len + extra_len + compressed) else {
            break;
        };
        // a body running past the file leaves the member outside the span
        if next > raw.len() {
            break;
        }
        members += 1;
        at = next;
    }
    // the EOCD record closes the archive, central directory included
    match find_eocd(raw, at) {
        Some((end, total)) => {
            // the walked count is ground truth when the walk completed; the
            // EOCD total covers members a streaming break hid from the walk
            let count = if streaming {
                total.max(members)
            } else {
                members
            };
            (Some((end - start) as u64), count)
        }
        None => {
            // without an EOCD the span still covers every walked member so
            // none of them re-flags as its own archive
            let span = (at - start) as u64;
            ((span > 0).then_some(span), members)
        }
    }
}

/// Find the end-of-central-directory record closing an archive.
///
/// Returns the file offset one past the record (declared comment included)
/// and the total entry count the record declares.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `from` - The offset to start searching at
fn find_eocd(raw: &[u8], from: usize) -> Option<(usize, usize)> {
    // the fixed EOCD record is 22 bytes plus a declared trailing comment
    let mut at = from;
    while at + 22 <= raw.len() {
        if raw[at..].starts_with(b"PK\x05\x06") {
            // the declared comment must fit for this to be the real record
            let comment = usize::from(read_le_u16(raw, at + 20).unwrap_or(0));
            let end = at + 22 + comment;
            if end <= raw.len() {
                let total = usize::from(read_le_u16(raw, at + 10).unwrap_or(0));
                return Some((end, total));
            }
        }
        at += 1;
    }
    None
}

/// Validate a CAB header at `at` and return its declared cabinet size.
///
/// A cmp immediate against `MSCF` in format-handling code carries code bytes
/// where the fixed header fields belong, so the reserved word, the version
/// pair, and the size fields must all hold before the match is a cabinet.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `at` - The offset the `MSCF` magic sits at
fn cab_size(raw: &[u8], at: usize) -> Option<u64> {
    // reserved1 is always zero in a real cabinet
    if read_le_u32(raw, at + 4)? != 0 {
        return None;
    }
    // the format version has been 1.3 since the format shipped
    if raw.get(at + 24).copied()? != 3 || raw.get(at + 25).copied()? != 1 {
        return None;
    }
    // the declared size must cover the fixed header and fit the file
    let size = u64::from(read_le_u32(raw, at + 8)?);
    let remaining = (raw.len() - at) as u64;
    if size < 36 || size > remaining {
        return None;
    }
    // the first-file offset points inside the cabinet
    let coff_files = u64::from(read_le_u32(raw, at + 16)?);
    (coff_files < size).then_some(size)
}

/// Whether a gzip member header at `at` carries plausible fixed fields.
///
/// The 3-byte magic plus FLG sanity still fires by chance inside encrypted
/// or high-entropy blobs; a real member also keeps XFL and OS in their
/// published value sets.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `at` - The offset the gzip magic sits at
fn valid_gzip_at(raw: &[u8], at: usize) -> bool {
    // the FLG sanity check thins the 3-byte magic's false positives
    if raw.get(at + 3).is_none_or(|flg| *flg > 0x1f) {
        return false;
    }
    // deflate uses only the two published XFL hints (or none at all)
    if !matches!(raw.get(at + 8).copied(), Some(0 | 2 | 4)) {
        return false;
    }
    // the OS byte comes from RFC 1952's closed table (255 = unknown)
    matches!(raw.get(at + 9).copied(), Some(0..=13 | 255))
}

/// Whether a structurally valid ELF header sits at `at`.
///
/// Magic-compare strings and `e_ident` template constants in loader code
/// carry nothing valid past the magic, so the identification bytes, object
/// type, version, and declared header size must all hold.
/// `EI_OSABI`/`EI_ABIVERSION` stay unconstrained — real Linux ELFs carry
/// OSABI 3.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `at` - The offset the `\x7fELF` magic sits at
fn valid_elf_at(raw: &[u8], at: usize) -> bool {
    // EI_CLASS and EI_DATA come from their two-value sets
    let Some(class) = raw.get(at + 4).copied() else {
        return false;
    };
    if !matches!(class, 1 | 2) {
        return false;
    }
    let Some(data) = raw.get(at + 5).copied() else {
        return false;
    };
    if !matches!(data, 1 | 2) {
        return false;
    }
    // EI_VERSION is always 1
    if raw.get(at + 6).copied() != Some(1) {
        return false;
    }
    // the e_ident padding (bytes 9..16) is always zero
    if !raw
        .get(at + 9..at + 16)
        .is_some_and(|pad| pad.iter().all(|byte| *byte == 0))
    {
        return false;
    }
    // the fixed header must fit: 52 bytes for ELF32, 64 for ELF64
    let (ehsize, ehsize_at) = if class == 1 { (52u16, 40) } else { (64u16, 52) };
    if at + usize::from(ehsize) > raw.len() {
        return false;
    }
    let big = data == 2;
    // e_type comes from the core object kinds (REL/EXEC/DYN/CORE)
    if !read_u16_at(raw, at + 16, big).is_some_and(|e_type| (1..=4).contains(&e_type)) {
        return false;
    }
    // e_version repeats EI_VERSION as a 32-bit field
    if read_u32_at(raw, at + 20, big) != Some(1) {
        return false;
    }
    // e_ehsize must declare the class's fixed header size
    read_u16_at(raw, at + ehsize_at, big) == Some(ehsize)
}

/// Measure a validated embedded ELF through its section-header table.
///
/// The table customarily closes an ELF image, so `e_shoff + e_shnum *
/// e_shentsize` bounds the artifact when the fields are sane; an image with a
/// stripped or implausible table stays unsized.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `at` - The offset the validated ELF header sits at
fn elf_span(raw: &[u8], at: usize) -> Option<u64> {
    // class picks the field offsets, data picks the byte order
    let class = raw.get(at + 4).copied()?;
    let big = raw.get(at + 5).copied()? == 2;
    // read the table offset, entry size, and entry count for the class
    let (header, shoff, entsize, count) = if class == 1 {
        (
            52u64,
            u64::from(read_u32_at(raw, at + 32, big)?),
            read_u16_at(raw, at + 46, big)?,
            read_u16_at(raw, at + 48, big)?,
        )
    } else {
        (
            64u64,
            read_u64_at(raw, at + 40, big)?,
            read_u16_at(raw, at + 58, big)?,
            read_u16_at(raw, at + 60, big)?,
        )
    };
    // sane means entries exist at the class's fixed entry size, past the header
    let expected = if class == 1 { 40 } else { 64 };
    if count == 0 || entsize != expected || shoff < header {
        return None;
    }
    // the table must close inside the bytes that remain
    let end = shoff.checked_add(u64::from(count) * u64::from(entsize))?;
    (end <= (raw.len() - at) as u64).then_some(end)
}

/// Whether an `MZ` at `at` opens a structurally valid embedded PE.
///
/// Beyond the `PE\0\0` signature behind `e_lfanew`, a real header carries a
/// published COFF machine id and opens its optional header with the PE32 or
/// PE32+ magic — fields an incidental `MZ` in code or data never satisfies.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `at` - The offset the `MZ` sits at
fn embedded_pe_at(raw: &[u8], at: usize) -> bool {
    // follow e_lfanew; real headers keep it small, a huge value is noise
    let Some(e_lfanew) = read_le_u32(raw, at + 0x3c) else {
        return false;
    };
    if e_lfanew > 0x10000 {
        return false;
    }
    let Some(pe) = at.checked_add(e_lfanew as usize) else {
        return false;
    };
    // the PE\0\0 signature must really be there
    if !matches!(raw.get(pe..pe + 4), Some(sig) if sig == b"PE\0\0") {
        return false;
    }
    // the COFF machine must be a published architecture id
    if !read_le_u16(raw, pe + 4).is_some_and(|machine| KNOWN_COFF_MACHINES.contains(&machine)) {
        return false;
    }
    // the optional header opens with the PE32/PE32+ magic
    matches!(read_le_u16(raw, pe + 24), Some(0x10b | 0x20b))
}

/// Measure a validated embedded PE's on-disk extent from its section table.
///
/// The raw extent is the furthest `PointerToRawData + SizeOfRawData` any
/// section declares — `SizeOfImage` is virtual-aligned and overshoots raw
/// spans, which could skip a separate artifact sitting right after the PE —
/// clamped to the bytes actually remaining in the file.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `at` - The offset the validated `MZ` header sits at
fn pe_raw_span(raw: &[u8], at: usize) -> Option<u64> {
    /// The most section headers walked; real tables stay well under this.
    const MAX_PE_SECTIONS: u16 = 96;
    // re-resolve the COFF header the validation proved present
    let pe = at.checked_add(read_le_u32(raw, at + 0x3c)? as usize)?;
    let sections = read_le_u16(raw, pe + 6)?;
    let optional = read_le_u16(raw, pe + 20)?;
    // the section table opens right after the optional header
    let table = pe + 24 + usize::from(optional);
    let mut end = 0u64;
    for index in 0..usize::from(sections.min(MAX_PE_SECTIONS)) {
        // each 40-byte header declares the section's raw pointer and size
        let base = table + index * 40;
        let Some(size) = read_le_u32(raw, base + 16) else {
            break;
        };
        let Some(pointer) = read_le_u32(raw, base + 20) else {
            break;
        };
        // only sections with on-disk bytes extend the raw image
        if size > 0 {
            end = end.max(u64::from(pointer) + u64::from(size));
        }
    }
    // clamp to the remaining bytes so the span never claims past the file
    (end > 0).then(|| end.min((raw.len() - at) as u64))
}

/// Validate a PNG at `offset`: gate on a plausible IHDR, then walk chunks.
///
/// The pngcheck-equivalent with an admission gate: the first chunk must be a
/// structurally plausible IHDR (graphics libraries store the bare 8-byte
/// signature as a recognition constant, which never carries one), and the
/// walk must cover at least two ASCII-typed chunks before an anomalous stream
/// is worth reporting — a walk that dies on its first chunk or hits a
/// non-ASCII chunk type is evidence against a PNG, not an anomalous image.
/// Past the gate it CRC-checks every chunk and measures the stream to IEND;
/// CRC-only tampering (the IDAT-steganography shape) still reports, since
/// field checks rather than CRCs gate emission. Bytes after IEND belong to
/// the surrounding file and are not judged here.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `offset` - The offset the PNG signature sits at
fn check_png(raw: &[u8], offset: usize) -> Option<Embedded> {
    /// The most anomaly strings recorded; further anomalies are counted into
    /// one bounded summary line, never accumulated as strings.
    const MAX_PNG_ANOMALIES: usize = 8;
    /// The most chunks walked before declaring the stream unbounded garbage.
    const MAX_PNG_CHUNKS: usize = 4096;
    // chunks start right after the 8-byte signature
    let first = offset + 8;
    // gate: a real PNG opens with a 13-byte IHDR chunk
    if read_be_u32(raw, first)? != 13 || raw.get(first + 4..first + 8)? != b"IHDR" {
        return None;
    }
    // gate: the dimensions must be plausible image geometry
    let width = read_be_u32(raw, first + 8)?;
    let height = read_be_u32(raw, first + 12)?;
    if !(1..0x0100_0000).contains(&width) || !(1..0x0100_0000).contains(&height) {
        return None;
    }
    // gate: bit depth and color type come from the spec's closed sets
    if !matches!(*raw.get(first + 16)?, 1 | 2 | 4 | 8 | 16) {
        return None;
    }
    if !matches!(*raw.get(first + 17)?, 0 | 2 | 3 | 4 | 6) {
        return None;
    }
    let mut anomalies: Vec<String> = Vec::new();
    // anomalies past the record cap are counted, not stored, so an anomalous
    // but real stream (the IDAT-steganography shape) still walks to IEND and
    // keeps its measured size
    let mut overflowed = 0usize;
    let mut at = first;
    let mut size = None;
    let mut walked = 0usize;
    // count chunks whose type bytes are letters: junk behind a real-looking
    // IHDR dies before two of them, real streams run on them to IEND
    let mut ascii_chunks = 0usize;
    loop {
        // bail out of garbage that parses as endless tiny chunks; a real
        // stream reaches IEND long before this bound
        walked += 1;
        if walked > MAX_PNG_CHUNKS {
            anomalies.push("chunk walk abandoned (unbounded chunk count)".to_owned());
            break;
        }
        // each chunk: u32 length, 4-byte type, data, u32 CRC over type+data
        let Some(length) = read_be_u32(raw, at) else {
            anomalies.push("truncated chunk header".to_owned());
            break;
        };
        let Some(kind) = raw.get(at + 4..at + 8) else {
            anomalies.push("truncated chunk type".to_owned());
            break;
        };
        // a chunk type outside the spec's letters-only alphabet is proof of
        // a non-PNG, never an anomaly worth seeding
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        ascii_chunks += 1;
        let data_end = at + 8 + length as usize;
        let Some(declared) = read_be_u32(raw, data_end) else {
            anomalies.push("truncated chunk data".to_owned());
            break;
        };
        // CRC spans the type and data bytes
        let computed = crc32fast::hash(&raw[at + 4..data_end]);
        if computed != declared {
            // record up to the cap; count the rest for the summary line
            if anomalies.len() < MAX_PNG_ANOMALIES {
                anomalies.push(format!(
                    "{} chunk CRC mismatch",
                    String::from_utf8_lossy(kind)
                ));
            } else {
                overflowed += 1;
            }
        }
        let next = data_end + 4;
        if kind == b"IEND" {
            size = Some((next - offset) as u64);
            break;
        }
        at = next;
    }
    // a walk that dies before covering two ASCII-typed chunks found a
    // signature constant, not an image
    if ascii_chunks < 2 {
        return None;
    }
    // fold the uncounted anomalies into one bounded line
    if overflowed > 0 {
        anomalies.push(format!("…and {overflowed} more chunk anomalies"));
    }
    Some(Embedded {
        offset: offset as u64,
        kind: "PNG image",
        size,
        members: None,
        anomalies,
    })
}

/// Map a file offset onto the declared section containing it, when one does.
///
/// # Arguments
///
/// * `header` - The parsed executable header, when one parsed
/// * `offset` - The file offset to attribute
fn containing_section(header: Option<&FormatHeader>, offset: u64) -> Option<String> {
    // walk the declared on-disk extents; sections do not overlap
    header?.sections.iter().find_map(|section| {
        let start = section.raw_offset;
        let end = start.checked_add(section.raw_size)?;
        (section.raw_size > 0 && offset >= start && offset < end).then(|| section.name.clone())
    })
}

/// Read a little-endian u16, when the range fits.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
fn read_le_u16(raw: &[u8], at: usize) -> Option<u16> {
    // ZIP, CAB, and PE fields are always little-endian
    read_u16_at(raw, at, false)
}

/// Read a little-endian u32, when the range fits.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
fn read_le_u32(raw: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(raw.get(at..at + 4)?.try_into().ok()?))
}

/// Read a big-endian u32, when the range fits.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
fn read_be_u32(raw: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(raw.get(at..at + 4)?.try_into().ok()?))
}

/// Read a u16 in the byte order a format declares, when the range fits.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
/// * `big` - Whether the bytes are big-endian
fn read_u16_at(raw: &[u8], at: usize, big: bool) -> Option<u16> {
    // the same two bytes, ordered by the declared endianness
    let bytes = raw.get(at..at + 2)?.try_into().ok()?;
    Some(if big {
        u16::from_be_bytes(bytes)
    } else {
        u16::from_le_bytes(bytes)
    })
}

/// Read a u32 in the byte order a format declares, when the range fits.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
/// * `big` - Whether the bytes are big-endian
fn read_u32_at(raw: &[u8], at: usize, big: bool) -> Option<u32> {
    // the same four bytes, ordered by the declared endianness
    let bytes = raw.get(at..at + 4)?.try_into().ok()?;
    Some(if big {
        u32::from_be_bytes(bytes)
    } else {
        u32::from_le_bytes(bytes)
    })
}

/// Read a u64 in the byte order a format declares, when the range fits.
///
/// # Arguments
///
/// * `raw` - The bytes to read from
/// * `at` - The offset to read at
/// * `big` - Whether the bytes are big-endian
fn read_u64_at(raw: &[u8], at: usize, big: bool) -> Option<u64> {
    // the same eight bytes, ordered by the declared endianness
    let bytes = raw.get(at..at + 8)?.try_into().ok()?;
    Some(if big {
        u64::from_be_bytes(bytes)
    } else {
        u64::from_le_bytes(bytes)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::fileinfo::HeaderSection;

    /// The 13 IHDR data bytes of a 1x1 8-bit grayscale image.
    const IHDR_1X1: [u8; 13] = [0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0];

    /// Appends one chunk (length, type, data, CRC) to a PNG under construction.
    ///
    /// # Arguments
    ///
    /// * `png` - The PNG bytes being built
    /// * `kind` - The 4-byte chunk type
    /// * `data` - The chunk data bytes
    fn push_chunk(png: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
        // each chunk: u32 length, 4-byte type, data, u32 CRC over type+data
        png.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let mut crc = Vec::from(kind);
        crc.extend_from_slice(data);
        png.extend_from_slice(&crc32fast::hash(&crc).to_be_bytes());
    }

    /// Builds a minimal valid PNG (IHDR + IEND) with correct CRCs.
    fn tiny_png() -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        // a 13-byte IHDR for a 1x1 grayscale image, then the closing IEND
        push_chunk(&mut png, b"IHDR", &IHDR_1X1);
        push_chunk(&mut png, b"IEND", &[]);
        png
    }

    /// Builds a valid PNG padded with one IDAT chunk to exactly `total` bytes.
    ///
    /// # Arguments
    ///
    /// * `total` - The total stream size to produce (at least 57 bytes)
    fn png_of_size(total: usize) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        push_chunk(&mut png, b"IHDR", &IHDR_1X1);
        // one IDAT chunk sized so the whole stream lands on the target
        push_chunk(&mut png, b"IDAT", &vec![0x5a; total - 57]);
        push_chunk(&mut png, b"IEND", &[]);
        png
    }

    /// Builds one stored (method 0) ZIP local member: header, name, data.
    ///
    /// # Arguments
    ///
    /// * `name` - The member's file name
    /// * `data` - The member's stored bytes
    fn zip_member(name: &str, data: &[u8]) -> Vec<u8> {
        let mut member = b"PK\x03\x04".to_vec();
        // version-needed 2.0, no flags, stored, zeroed time/date
        member.extend_from_slice(&20u16.to_le_bytes());
        member.extend_from_slice(&0u16.to_le_bytes());
        member.extend_from_slice(&0u16.to_le_bytes());
        member.extend_from_slice(&0u16.to_le_bytes());
        member.extend_from_slice(&0u16.to_le_bytes());
        // CRC and both sizes describe the stored data exactly
        member.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
        let size = u32::try_from(data.len()).unwrap();
        member.extend_from_slice(&size.to_le_bytes());
        member.extend_from_slice(&size.to_le_bytes());
        member.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
        member.extend_from_slice(&0u16.to_le_bytes());
        member.extend_from_slice(name.as_bytes());
        member.extend_from_slice(data);
        member
    }

    /// Builds a complete ZIP archive: members, central directory, and EOCD.
    ///
    /// # Arguments
    ///
    /// * `members` - The `(name, data)` pairs to store
    fn zip_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Vec::new();
        // the member chain, recording each local header's offset
        let mut offsets = Vec::new();
        for (name, data) in members {
            offsets.push(u32::try_from(archive.len()).unwrap());
            archive.extend_from_slice(&zip_member(name, data));
        }
        // the central directory mirrors every member
        let cd_start = u32::try_from(archive.len()).unwrap();
        for ((name, data), offset) in members.iter().zip(&offsets) {
            archive.extend_from_slice(b"PK\x01\x02");
            // version-made-by and version-needed
            archive.extend_from_slice(&20u16.to_le_bytes());
            archive.extend_from_slice(&20u16.to_le_bytes());
            // flags, method, time, date
            archive.extend_from_slice(&[0u8; 8]);
            archive.extend_from_slice(&crc32fast::hash(data).to_le_bytes());
            let size = u32::try_from(data.len()).unwrap();
            archive.extend_from_slice(&size.to_le_bytes());
            archive.extend_from_slice(&size.to_le_bytes());
            archive.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
            // extra/comment lengths, disk, internal and external attributes
            archive.extend_from_slice(&[0u8; 12]);
            archive.extend_from_slice(&offset.to_le_bytes());
            archive.extend_from_slice(name.as_bytes());
        }
        let cd_size = u32::try_from(archive.len()).unwrap() - cd_start;
        // the end-of-central-directory record closes the archive
        archive.extend_from_slice(b"PK\x05\x06");
        archive.extend_from_slice(&[0u8; 4]);
        let total = u16::try_from(members.len()).unwrap();
        archive.extend_from_slice(&total.to_le_bytes());
        archive.extend_from_slice(&total.to_le_bytes());
        archive.extend_from_slice(&cd_size.to_le_bytes());
        archive.extend_from_slice(&cd_start.to_le_bytes());
        archive.extend_from_slice(&0u16.to_le_bytes());
        archive
    }

    /// Builds a minimal valid little-endian ELF64 header (OSABI 3) with a
    /// zero-filled section-header table of `shnum` entries right after it.
    ///
    /// # Arguments
    ///
    /// * `shnum` - The section-header count to declare
    fn tiny_elf64(shnum: u16) -> Vec<u8> {
        // e_ident: magic, class 2, LSB, version 1, Linux OSABI, zero padding
        let mut elf = b"\x7fELF\x02\x01\x01\x03".to_vec();
        elf.extend_from_slice(&[0u8; 8]);
        // e_type EXEC, e_machine x86-64, e_version 1
        elf.extend_from_slice(&2u16.to_le_bytes());
        elf.extend_from_slice(&0x3eu16.to_le_bytes());
        elf.extend_from_slice(&1u32.to_le_bytes());
        // e_entry and e_phoff zeroed, the table right after the 64-byte header
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&0u64.to_le_bytes());
        elf.extend_from_slice(&64u64.to_le_bytes());
        // e_flags, e_ehsize, e_phentsize, e_phnum
        elf.extend_from_slice(&0u32.to_le_bytes());
        elf.extend_from_slice(&64u16.to_le_bytes());
        elf.extend_from_slice(&0u16.to_le_bytes());
        elf.extend_from_slice(&0u16.to_le_bytes());
        // e_shentsize, e_shnum, e_shstrndx
        elf.extend_from_slice(&64u16.to_le_bytes());
        elf.extend_from_slice(&shnum.to_le_bytes());
        elf.extend_from_slice(&0u16.to_le_bytes());
        // the section-header table itself, zero-filled
        elf.extend_from_slice(&vec![0u8; usize::from(shnum) * 64]);
        elf
    }

    /// Builds a minimal embedded PE: MZ stub, COFF header, optional-header
    /// magic, and the given `(PointerToRawData, SizeOfRawData)` sections.
    ///
    /// # Arguments
    ///
    /// * `sections` - The `(raw pointer, raw size)` pairs to declare
    fn tiny_pe(sections: &[(u32, u32)]) -> Vec<u8> {
        // the MZ stub carries only e_lfanew, pointing right past itself
        let mut pe = b"MZ".to_vec();
        pe.resize(0x3c, 0);
        pe.extend_from_slice(&0x40u32.to_le_bytes());
        // the COFF header: signature, AMD64 machine, section count
        pe.extend_from_slice(b"PE\0\0");
        pe.extend_from_slice(&0x8664u16.to_le_bytes());
        pe.extend_from_slice(&u16::try_from(sections.len()).unwrap().to_le_bytes());
        // timestamp, symbol table pointer, symbol count
        pe.extend_from_slice(&[0u8; 12]);
        // a 16-byte optional header holding just the PE32+ magic
        pe.extend_from_slice(&16u16.to_le_bytes());
        pe.extend_from_slice(&0u16.to_le_bytes());
        pe.extend_from_slice(&0x20bu16.to_le_bytes());
        pe.extend_from_slice(&[0u8; 14]);
        // each section header: raw size and pointer set, the rest zeroed
        for (pointer, size) in sections {
            let mut section = [0u8; 40];
            section[16..20].copy_from_slice(&size.to_le_bytes());
            section[20..24].copy_from_slice(&pointer.to_le_bytes());
            pe.extend_from_slice(&section);
        }
        pe
    }

    /// Builds a header declaring one named section over a raw-byte range.
    ///
    /// # Arguments
    ///
    /// * `name` - The section name
    /// * `raw_offset` - The section's file offset
    /// * `raw_size` - The section's on-disk size
    fn header_with(name: &str, raw_offset: u64, raw_size: u64) -> FormatHeader {
        FormatHeader {
            sections: vec![HeaderSection {
                name: name.to_owned(),
                address: raw_offset,
                virtual_size: raw_size,
                raw_offset,
                raw_size,
                flags: 0,
            }],
            ..Default::default()
        }
    }

    /// A clean embedded PNG is found, measured, and anomaly-free.
    #[test]
    fn finds_and_measures_an_embedded_png() {
        // bury a valid PNG inside filler bytes
        let mut raw = vec![0x41u8; 64];
        let png = tiny_png();
        raw.extend_from_slice(&png);
        raw.extend_from_slice(&[0x42u8; 32]);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].offset, 64);
        assert_eq!(found[0].kind, "PNG image");
        assert_eq!(found[0].size, Some(png.len() as u64));
        assert_eq!(found[0].anomalies, Vec::<String>::new());
    }

    /// A corrupted chunk CRC on a structurally valid stream is flagged as an
    /// anomaly (field tampering would gate the stream out instead).
    #[test]
    fn flags_a_png_crc_mismatch() {
        let mut raw = vec![0u8; 16];
        let mut png = tiny_png();
        // corrupt one IHDR CRC byte without touching the gated fields
        png[29] ^= 0xFF;
        raw.extend_from_slice(&png);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        assert!(found[0].anomalies.iter().any(|a| a.contains("IHDR")));
    }

    /// An embedded MZ without a real PE header is not reported; a validated
    /// MZ + COFF + optional-header stub is.
    #[test]
    fn embedded_pe_requires_a_real_header() {
        // a bare MZ with garbage e_lfanew is ignored
        let mut raw = vec![0u8; 8];
        raw.extend_from_slice(b"MZ");
        raw.extend_from_slice(&[0u8; 128]);
        assert!(scan_embedded(&raw).is_empty());
        // a well-formed embedded PE stub is found
        let mut full = vec![0u8; 8];
        full.extend_from_slice(&tiny_pe(&[]));
        full.extend_from_slice(&[0u8; 16]);
        let found = scan_embedded(&full);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "PE executable");
        // no section table means no measurable raw span
        assert_eq!(found[0].size, None);
    }

    /// An MZ + PE signature with an unpublished COFF machine is not reported.
    #[test]
    fn a_pe_with_an_unknown_machine_is_not_reported() {
        let mut raw = vec![0u8; 8];
        let mut pe = tiny_pe(&[]);
        // overwrite the machine field with an unpublished id
        let machine_at = 0x40 + 4;
        pe[machine_at..machine_at + 2].copy_from_slice(&0x1234u16.to_le_bytes());
        raw.extend_from_slice(&pe);
        raw.extend_from_slice(&[0u8; 16]);
        assert!(scan_embedded(&raw).is_empty());
    }

    /// A measured embedded PE subsumes magic bytes inside its span while a
    /// following artifact past the span is still found.
    #[test]
    fn a_pe_span_subsumes_nested_magics_but_not_followers() {
        let mut raw = vec![0u8; 16];
        let start = raw.len();
        // one section ending at raw offset 0x100 bounds the embedded image
        let mut pe = tiny_pe(&[(0x80, 0x80)]);
        pe.resize(0x100, 0);
        // a well-formed gzip header inside the measured span must not re-flag
        pe[0xc0..0xc4].copy_from_slice(b"\x1f\x8b\x08\x00");
        pe[0xc9] = 3;
        raw.extend_from_slice(&pe);
        // a second artifact right after the span is still scanned
        raw.extend_from_slice(b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03");
        raw.extend_from_slice(&[0u8; 8]);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].kind, "PE executable");
        assert_eq!(found[0].size, Some(0x100));
        assert_eq!(found[1].kind, "gzip stream");
        assert_eq!(found[1].offset, (start + 0x100) as u64);
    }

    /// Many CRC mismatches cap the recorded notes but never abandon the walk:
    /// the stream is still measured to IEND and the overflow is summarized.
    #[test]
    fn png_anomaly_cap_does_not_abandon_the_walk() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        // a valid IHDR admits the stream through the first-chunk gate
        push_chunk(&mut png, b"IHDR", &IHDR_1X1);
        // ten tEXt chunks whose CRCs are all wrong
        for _ in 0..10 {
            png.extend_from_slice(&1u32.to_be_bytes());
            png.extend_from_slice(b"tEXt");
            png.push(0x41);
            png.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        }
        // a valid IEND chunk terminates the stream
        push_chunk(&mut png, b"IEND", &[]);
        let total = png.len();
        let mut raw = vec![0u8; 4];
        raw.extend_from_slice(&png);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        // the size survives: IEND was reached despite the anomaly flood
        assert_eq!(found[0].size, Some(total as u64));
        // eight recorded mismatches plus the single overflow summary
        assert_eq!(found[0].anomalies.len(), 9);
        assert!(found[0].anomalies[8].contains("2 more"));
    }

    /// Offset 0 (the host's own magic) is never reported.
    #[test]
    fn the_host_magic_is_ignored() {
        let mut raw = tiny_png();
        raw.extend_from_slice(&[0u8; 32]);
        assert!(scan_embedded(&raw).is_empty());
    }

    /// The `SmartAssembly` `ldc.i4 0x04034B50` comparison immediate from
    /// a protected binary is not reported: its version-needed field reads 0x2a40.
    #[test]
    fn zip_magic_immediate_in_code_is_not_an_archive() {
        let mut raw = vec![0u8; 32];
        // ldloc, ldind, ldc.i4 <ZIP magic>, bne.un — managed ZIP-reader code
        raw.extend_from_slice(b"\x06\x4a\x20PK\x03\x04\x40\x2a");
        raw.extend_from_slice(&[0u8; 64]);
        assert!(scan_embedded(&raw).is_empty());
    }

    /// The `WinHTTP` downloader's `cmp dword ptr [esi], 'FCSM'` immediate from
    /// its code is not reported: its reserved1 field reads 0x4d8b1174.
    #[test]
    fn cab_compare_immediate_is_not_a_cabinet() {
        let mut raw = vec![0u8; 32];
        // cmp [esi], 'FCSM'; je; mov — buffer-validation code, not a cabinet
        raw.extend_from_slice(b"\x81\x3eMSCF\x74\x11\x8b\x4d");
        raw.extend_from_slice(&[0u8; 64]);
        assert!(scan_embedded(&raw).is_empty());
    }

    /// A structurally real cabinet header still seeds with its declared size.
    #[test]
    fn a_real_cab_header_still_seeds() {
        let mut raw = vec![0u8; 16];
        let start = raw.len();
        // signature, zero reserved1, size 100, zero reserved2, coffFiles 44
        raw.extend_from_slice(b"MSCF");
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&100u32.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());
        raw.extend_from_slice(&44u32.to_le_bytes());
        raw.extend_from_slice(&0u32.to_le_bytes());
        // version 1.3 as minor/major bytes, then the declared body
        raw.extend_from_slice(&[3, 1]);
        raw.resize(start + 100, 0);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "CAB archive");
        assert_eq!(found[0].size, Some(100));
    }

    /// The `is_infected_elf_arcsec_marker` magic-compare string from
    /// an 84-byte .rodata is not reported: nothing follows the magic.
    #[test]
    fn elf_magic_compare_string_is_not_an_executable() {
        // an 84-byte read-only blob holding only the marker string
        let mut raw = vec![0u8; 84];
        raw[10..15].copy_from_slice(b"\x7fELF\x00");
        assert!(scan_embedded(&raw).is_empty());
    }

    /// ld.so's adjacent `e_ident` template constants pass the
    /// `e_ident` checks but fail `e_type`, which reads the next template's 0x457f.
    #[test]
    fn elf_ident_templates_fail_the_type_check() {
        let mut raw = vec![0u8; 24];
        // two 16-byte e_ident templates back to back (class 2, LSB, OSABI 3)
        for _ in 0..2 {
            raw.extend_from_slice(b"\x7fELF\x02\x01\x01\x03");
            raw.extend_from_slice(&[0u8; 8]);
        }
        raw.extend_from_slice(&[0u8; 64]);
        assert!(scan_embedded(&raw).is_empty());
    }

    /// A structurally real embedded ELF — an observed .data payload
    /// shape — still seeds, measured through its section-header table.
    #[test]
    fn a_real_embedded_elf_is_measured() {
        let mut raw = vec![0u8; 0x4440];
        let elf = tiny_elf64(2);
        raw.extend_from_slice(&elf);
        raw.extend_from_slice(&[0u8; 32]);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "ELF executable");
        assert_eq!(found[0].offset, 0x4440);
        assert_eq!(found[0].size, Some(elf.len() as u64));
    }

    /// An XOR-keystream coincidence — gzip magic bytes inside
    /// ciphertext with impossible XFL/OS fields — is not reported.
    #[test]
    fn gzip_ciphertext_coincidence_is_not_a_stream() {
        let mut raw = vec![0u8; 32];
        // 1f 8b 08 with a small FLG, then keystream bytes where XFL/OS belong
        raw.extend_from_slice(b"\x1f\x8b\x08\x04\xaa\xbb\xcc\xdd\x37\x5a");
        raw.extend_from_slice(&[0u8; 32]);
        assert!(scan_embedded(&raw).is_empty());
    }

    /// A well-formed gzip member header still seeds.
    #[test]
    fn a_wellformed_gzip_header_still_seeds() {
        let mut raw = vec![0u8; 32];
        // magic, deflate, no flags, zero mtime, XFL 0, OS 3 (unix)
        raw.extend_from_slice(b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03");
        raw.extend_from_slice(&[0x5au8; 16]);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "gzip stream");
    }

    /// A graphics-library signature constant — the bare 8-byte
    /// PNG signature followed by small-integer struct fields — is not reported.
    #[test]
    fn png_signature_constant_is_not_an_image() {
        let mut raw = vec![0u8; 16];
        raw.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        // the codec-table struct fields that follow the recognition constant
        for field in [1u32, 2, 16, 8, 32, 64] {
            raw.extend_from_slice(&field.to_le_bytes());
        }
        raw.extend_from_slice(&[0u8; 32]);
        assert!(scan_embedded(&raw).is_empty());
    }

    /// A signature whose stream dies on its very first chunk (the Delphi
    /// graphics-unit constant) is not an anomalous image.
    #[test]
    fn png_dying_on_chunk_one_is_not_reported() {
        let mut raw = vec![0u8; 8];
        // the signature sits near the end of the file with a half-written IHDR
        raw.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        raw.extend_from_slice(&13u32.to_be_bytes());
        raw.extend_from_slice(b"IHDR");
        raw.extend_from_slice(&[0, 0]);
        assert!(scan_embedded(&raw).is_empty());
    }

    /// A non-ASCII chunk type behind a plausible IHDR is proof of a non-PNG
    /// (the ignored '<non-ASCII> chunk CRC mismatch' lead shape), not a lead.
    #[test]
    fn png_with_a_non_ascii_chunk_type_is_not_reported() {
        let mut raw = vec![0u8; 8];
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        push_chunk(&mut png, b"IHDR", &IHDR_1X1);
        // the next "chunk" carries garbage type bytes
        png.extend_from_slice(&4u32.to_be_bytes());
        png.extend_from_slice(&[0xC3, 0xA9, 0x01, 0x02]);
        png.extend_from_slice(&[0u8; 8]);
        raw.extend_from_slice(&png);
        raw.extend_from_slice(&[0u8; 16]);
        assert!(scan_embedded(&raw).is_empty());
    }

    /// A real stream truncated mid-IDAT — a grafted PNG cut short — still
    /// seeds as an anomalous image once two ASCII-typed chunks are covered.
    #[test]
    fn png_truncated_after_two_chunks_still_seeds() {
        let mut raw = vec![0u8; 8];
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        push_chunk(&mut png, b"IHDR", &IHDR_1X1);
        // an IDAT whose declared length runs past the end of the file
        png.extend_from_slice(&0x4000u32.to_be_bytes());
        png.extend_from_slice(b"IDAT");
        png.extend_from_slice(&[0x5a; 32]);
        raw.extend_from_slice(&png);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        assert!(found[0].anomalies.iter().any(|a| a.contains("truncated")));
        assert_eq!(found[0].size, None);
    }

    /// A 13-member resource-ZIP payload bundle seeds one measured
    /// lead carrying its member count instead of thirteen per-member leads.
    #[test]
    fn a_multi_member_zip_is_one_measured_lead() {
        // thirteen stored members mirror the observed payload bundle
        let members: Vec<(String, Vec<u8>)> = (0..13)
            .map(|index| (format!("member{index:02}.bin"), vec![0x61u8; 40 + index]))
            .collect();
        let pairs: Vec<(&str, &[u8])> = members
            .iter()
            .map(|(name, data)| (name.as_str(), data.as_slice()))
            .collect();
        let archive = zip_archive(&pairs);
        let mut raw = vec![0u8; 64];
        raw.extend_from_slice(&archive);
        raw.extend_from_slice(&[0u8; 32]);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "ZIP archive");
        assert_eq!(found[0].offset, 64);
        assert_eq!(found[0].members, Some(13));
        assert_eq!(found[0].size, Some(archive.len() as u64));
    }

    /// A streaming (bit-3) member breaks the walk and the EOCD span still
    /// covers every member past the break.
    #[test]
    fn a_streaming_zip_falls_back_to_the_eocd_span() {
        let mut raw = vec![0u8; 32];
        let start = raw.len();
        // first member: streaming flag set, sizes zeroed (descriptor follows)
        let mut streaming = zip_member("later.bin", b"");
        streaming[6] = 0x08;
        raw.extend_from_slice(&streaming);
        raw.extend_from_slice(b"streamed-data");
        // the data descriptor the streaming member defers its sizes to
        raw.extend_from_slice(b"PK\x07\x08");
        raw.extend_from_slice(&[0u8; 12]);
        // a second, normal member the broken walk can no longer reach
        raw.extend_from_slice(&zip_member("inner.txt", b"payload"));
        // the EOCD closes the archive and declares both members
        raw.extend_from_slice(b"PK\x05\x06");
        raw.extend_from_slice(&[0u8; 4]);
        raw.extend_from_slice(&2u16.to_le_bytes());
        raw.extend_from_slice(&2u16.to_le_bytes());
        raw.extend_from_slice(&[0u8; 8]);
        raw.extend_from_slice(&0u16.to_le_bytes());
        let end = raw.len();
        raw.extend_from_slice(&[0u8; 24]);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].members, Some(2));
        assert_eq!(found[0].size, Some((end - start) as u64));
    }

    /// A measured archive lead names its member count in the content line.
    #[test]
    fn a_measured_zip_lead_names_its_member_count() {
        let embedded = Embedded {
            offset: 0x40,
            kind: "ZIP archive",
            size: Some(2048),
            members: Some(13),
            anomalies: Vec::new(),
        };
        let hit = build_hit(None, &embedded).expect("archives always render");
        assert!(hit.content.contains("ZIP archive, 13 members"));
        assert!(hit.content.contains("(2048 bytes)"));
        assert_eq!(hit.suspicion, medium().1);
    }

    /// Fifteen small clean resource PNGs — an icon flood —
    /// collapse into one low-severity rollup lead listing every offset/size.
    #[test]
    fn small_clean_resource_pngs_roll_into_one_lead() {
        // fifteen icons spanning the pack's 337–449 byte spread
        let mut raw = vec![0u8; 0x400];
        let rsrc_start = raw.len() as u64;
        let mut offsets = Vec::new();
        for index in 0..15 {
            offsets.push(raw.len() as u64);
            let size = match index {
                0 => 337,
                1 => 449,
                _ => 400,
            };
            raw.extend_from_slice(&png_of_size(size));
        }
        let rsrc_size = raw.len() as u64 - rsrc_start;
        raw.extend_from_slice(&[0u8; 32]);
        let header = header_with(".rsrc", rsrc_start, rsrc_size);
        let hits = render_hits(Some(&header), scan_embedded(&raw));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].suspicion, low().1);
        assert!(hits[0].content.contains("15 PNG resources"));
        assert!(hits[0].content.contains("337–449 bytes"));
        // every rolled-up image stays listed and extractable
        assert_eq!(hits[0].sites.len(), 15);
        assert!(
            hits[0]
                .content
                .contains(&format!("@{:#x} (337 bytes)", offsets[0]))
        );
    }

    /// A confirmed 47325-byte resource PNG stays an individual
    /// lead under the large-image carve-out while its sibling icons roll up.
    #[test]
    fn a_large_resource_png_stays_individual() {
        let mut raw = vec![0u8; 64];
        let rsrc_start = raw.len() as u64;
        // three small icons roll up around the large payload-sized image
        for _ in 0..3 {
            raw.extend_from_slice(&png_of_size(400));
        }
        let big_at = raw.len() as u64;
        raw.extend_from_slice(&png_of_size(47325));
        let rsrc_size = raw.len() as u64 - rsrc_start;
        let header = header_with(".rsrc", rsrc_start, rsrc_size);
        let hits = render_hits(Some(&header), scan_embedded(&raw));
        assert_eq!(hits.len(), 2);
        // the individual lead keeps the confirmed artifact's offset and size
        assert!(
            hits[0]
                .content
                .contains(&format!("@ file offset {big_at:#x} (47325 bytes)"))
        );
        assert_eq!(hits[0].suspicion, low().1);
        // the rollup carries the three small icons
        assert!(hits[1].content.contains("3 PNG resources"));
    }

    /// Two clean resource icons stay individual leads — no rollup below three.
    #[test]
    fn two_clean_resource_pngs_stay_individual() {
        let mut raw = vec![0u8; 64];
        let rsrc_start = raw.len() as u64;
        raw.extend_from_slice(&png_of_size(337));
        raw.extend_from_slice(&png_of_size(449));
        let rsrc_size = raw.len() as u64 - rsrc_start;
        let header = header_with(".rsrc", rsrc_start, rsrc_size);
        let hits = render_hits(Some(&header), scan_embedded(&raw));
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.suspicion == low().1));
    }

    /// Clean PNGs grafted into .text — a confirmed 7449- and
    /// 10588-byte payload shape — never roll up and keep medium severity.
    #[test]
    fn clean_text_section_pngs_stay_full_strength() {
        let mut raw = vec![0u8; 64];
        let text_start = raw.len() as u64;
        raw.extend_from_slice(&png_of_size(7449));
        raw.extend_from_slice(&png_of_size(10588));
        raw.extend_from_slice(&png_of_size(400));
        let text_size = raw.len() as u64 - text_start;
        let header = header_with(".text", text_start, text_size);
        let hits = render_hits(Some(&header), scan_embedded(&raw));
        assert_eq!(hits.len(), 3);
        assert!(hits.iter().all(|hit| hit.suspicion == medium().1));
        assert!(hits[0].content.contains("(7449 bytes)"));
    }

    /// An icon flood cannot displace an executable lead: the rollup rides
    /// outside the per-run hit cap.
    #[test]
    fn the_rollup_never_displaces_an_executable_lead() {
        let mut raw = vec![0u8; 64];
        let rsrc_start = raw.len() as u64;
        // twenty icons would exhaust the old per-run cap on their own
        for _ in 0..20 {
            raw.extend_from_slice(&png_of_size(400));
        }
        let rsrc_size = raw.len() as u64 - rsrc_start;
        // the dropper PE sits after the flood — past the old 16-hit cap
        raw.extend_from_slice(&tiny_pe(&[]));
        raw.extend_from_slice(&[0u8; 16]);
        let header = header_with(".rsrc", rsrc_start, rsrc_size);
        let hits = render_hits(Some(&header), scan_embedded(&raw));
        assert_eq!(hits.len(), 2);
        assert!(hits[0].content.contains("PE executable"));
        assert!(hits[1].content.contains("20 PNG resources"));
    }

    /// An ELF match inside `.gnu.prelink_undo` is prelink residue, not a payload.
    #[test]
    fn elf_inside_prelink_undo_is_dropped() {
        let mut raw = vec![0u8; 0x200];
        let undo_start = raw.len() as u64;
        raw.extend_from_slice(&tiny_elf64(1));
        let undo_size = raw.len() as u64 - undo_start;
        raw.extend_from_slice(&[0u8; 16]);
        let header = header_with(".gnu.prelink_undo", undo_start, undo_size);
        assert!(render_hits(Some(&header), scan_embedded(&raw)).is_empty());
    }

    /// The same real ELF in .data — an affirmed payload shape — keeps
    /// its full-strength lead.
    #[test]
    fn a_real_elf_in_data_keeps_its_lead() {
        let mut raw = vec![0u8; 0x4440];
        raw.extend_from_slice(&tiny_elf64(1));
        let data_size = raw.len() as u64 - 0x4440;
        raw.extend_from_slice(&[0u8; 16]);
        let header = header_with(".data", 0x4440, data_size);
        let hits = render_hits(Some(&header), scan_embedded(&raw));
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0]
                .content
                .contains("ELF executable embedded in .data @ file offset 0x4440")
        );
        assert_eq!(hits[0].suspicion, medium().1);
    }
}
