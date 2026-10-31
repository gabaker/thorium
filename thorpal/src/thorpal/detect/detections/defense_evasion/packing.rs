//! Software packing / runtime obfuscation (MITRE ATT&CK **T1027.002**).
//!
//! A packed binary carries its real code as compressed or encrypted data and
//! reconstitutes it in memory at startup. Everything downstream of this — the
//! string list, the import table, the decompilation — describes the *stub*, not
//! the program, so an analyst who does not know the sample is packed will draw
//! conclusions about the wrong code. That makes this the one finding worth
//! reporting even when it is weak.
//!
//! # What is measured
//!
//! Seven structural signals, all read off the section table, the raw file, and
//! the import list — no family knowledge involved:
//!
//! 1. a mapped section that carries no bytes on disk, paired with a smaller
//!    high-entropy section that does (the unpack target and its source),
//! 2. a section that is both writable and executable,
//! 3. a substantial section whose entropy is near the theoretical maximum
//!    (executable, or large in absolute or relative terms — small compressed
//!    assets are routine and do not count),
//! 4. a thin import table consisting mostly of the dynamic-resolution and
//!    memory-permission calls an unpacking stub needs,
//! 5. an entry point in a writable section other than the first,
//! 6. a section-name set at odds with the conventions of the detected format, and
//! 7. high whole-file entropy, or a large high-entropy region appended past the
//!    last section (minus any Authenticode certificate table the header
//!    declares, and skipping the whole-file reading when one dominant data
//!    section already explains it).
//!
//! # Why this is scored conservatively
//!
//! Every one of those signals has a mundane cause. Installers and
//! self-extracting archives carry compressed payloads; commercial software ships
//! with license protection; games and media tools embed large compressed assets;
//! signed binaries carry an appended certificate table; JIT runtimes map
//! writable-executable memory. So a single signal is [`low`] and context-only,
//! and even several together only reach [`medium`] — enough to tell the analyst
//! that the visible code may not be the real code, without asserting intent.
//! And when the whole shape is a payload carrier — ordinary code and a full
//! import table beside one large compressed data blob — the finding says
//! exactly that (carve out and identify the cargo) instead of claiming the
//! visible code is packed.
//!
//! # Naming a family is a separate, stricter question
//!
//! Structure tells you *that* something is packed; it cannot tell you *what*
//! packed it. High entropy is high entropy no matter which tool produced it, and
//! a name asserted from shape alone sends an analyst looking for the wrong
//! unpacking procedure. So a family name comes only from [`PACKER_MARKERS`] — a
//! table of section-name sets and stub magic that packing tools write into the
//! image — and only a marker match may promote the finding to [`high`].
//!
//! Markers key on the section table and the stub, never on a banner string:
//! packers routinely offer to strip their banner, and repacking with a modified
//! banner is trivial, so a banner is the first thing to disappear from a sample
//! that has been through an operator's hands.

use std::borrow::Cow;
use std::collections::HashSet;

use anyhow::Result;

use crate::thorpal::detect::confidence::Confidence;
use crate::thorpal::detect::context::{AnalysisContext, Section, fmt_addr, parse_addr};
use crate::thorpal::detect::detections::{
    ArtifactClass, AttackTechnique, Detection, DetectionHit, EvidenceKind, HitArtifact, high, low,
    medium,
};
use crate::thorpal::detect::fileinfo::{FormatHeader, parse_header};

use super::T1027_002;

/// The Software Packing detection (ATT&CK T1027.002).
pub struct Packing;

/// The entropy at which a section's bytes are indistinguishable from random.
///
/// Compressed and encrypted data both sit just under the 8.0 ceiling. Ordinary
/// machine code lands around 6.0–6.5 and text-heavy data lower still, so 7.2
/// clears normal code comfortably while still catching a merely-compressed (not
/// encrypted) payload.
const HIGH_ENTROPY: f32 = 7.2;

/// The entropy at which the *whole file* looks compressed end to end.
///
/// Lower than [`HIGH_ENTROPY`] because a whole-file figure is diluted by the
/// header, the padding, and the import strings, which are all low-entropy.
const HIGH_FILE_ENTROPY: f32 = 7.0;

/// The entropy below which an executable section still reads as ordinary code.
///
/// Compiled machine code tops out around the mid-6 range; an executable section
/// reading past this is itself compressed or encrypted, so the image cannot be
/// described as a normal program merely carrying a payload.
const PLAIN_CODE_ENTROPY: f32 = 7.0;

/// The smallest region worth taking an entropy reading of, in bytes.
///
/// Shannon entropy over a short buffer is dominated by sampling noise — a
/// 64-byte buffer of distinct values scores 6.0 without being compressed at all.
const MIN_ENTROPY_LEN: u64 = 4096;

/// The size at which a high-entropy data section is big enough to be a packed
/// body or a carried payload rather than an ordinary embedded asset.
///
/// Compressed icons, images, and string tables live well under this; a packed
/// program body or a dropper's carried payload is rarely smaller.
const CARRIER_FLOOR: u64 = 64 * 1024;

/// How much larger a section's mapped size must be than its on-disk size before
/// the gap reads as "space reserved for something to be written into".
///
/// A `.bss`-style block legitimately reserves uninitialized space, so the ratio
/// alone is not the signal — signal 1 also requires a high-entropy section
/// elsewhere in the image to be the thing that gets written.
const UNPACK_TARGET_RATIO: u64 = 4;

/// The import count below which a program is doing almost everything by hand.
///
/// A program that genuinely uses the platform accumulates imports quickly; a
/// stub needs only enough to map memory and find the rest at runtime.
const THIN_IMPORT_TABLE: usize = 20;

/// Imports used to locate code at runtime rather than at link time.
const RESOLVER_IMPORTS: &[&str] = &[
    "loadlibrary",
    "getprocaddress",
    "getmodulehandle",
    "ldrloaddll",
    "ldrgetprocedureaddress",
    "dlopen",
    "dlsym",
];

/// Imports used to make writable memory executable (or to allocate memory that
/// already is).
const MEMORY_IMPORTS: &[&str] = &[
    "virtualalloc",
    "virtualprotect",
    "ntallocatevirtualmemory",
    "ntprotectvirtualmemory",
    "mmap",
    "mprotect",
];

/// Memory blocks Ghidra's loaders synthesize that are not section-table entries.
///
/// A platform fact about the loader, not about any sample: Ghidra maps a `Headers`
/// block over the file header of every PE and Mach-O it opens, and an `EXTERNAL`
/// block to hold thunks for imported symbols. Neither appears in any section table,
/// so counting them as sections both inflates the "unconventional name" ratio on
/// every image and lets a 4 KiB header block stand in for a compressed body. Shape
/// signals therefore skip them; the file-identity section table still lists them,
/// because there they are describing the memory map rather than the image.
const SYNTHETIC_BLOCKS: &[&str] = &["headers", "external", "tdb", "_elfsectionheaders"];

/// Section names that are conventional for a given executable format.
///
/// A platform fact, not an indicator list: these come from what linkers and
/// object formats emit, so the table changes when a toolchain ships a new
/// section convention, not when a new sample appears. Matching is
/// case-insensitive and prefix-based, so the many `.note.*`, `.rela.*`, and
/// `.debug_*` variants are covered by their stems.
const CONVENTIONAL_SECTIONS: &[(&str, &[&str])] = &[
    (
        "pe",
        &[
            ".text", ".rdata", ".data", ".rsrc", ".reloc", ".idata", ".edata", ".pdata", ".xdata",
            ".tls", ".bss", ".didat", ".didata", ".crt", ".debug", ".gfids", ".sxdata", ".00cfg",
            ".sdata", ".drectve", ".symtab", ".textbss", ".detourc", ".shared", ".rodata", ".init",
            "code", "data", "bss", "init", "page", ".arch", ".voltbl", ".giats", ".retplne",
        ],
    ),
    (
        "elf",
        &[
            ".text",
            ".data",
            ".rodata",
            ".bss",
            ".init",
            ".fini",
            ".plt",
            ".got",
            ".rela",
            ".rel",
            ".dynsym",
            ".dynstr",
            ".dynamic",
            ".interp",
            ".note",
            ".eh_frame",
            ".gnu",
            ".hash",
            ".comment",
            ".symtab",
            ".strtab",
            ".shstrtab",
            ".tbss",
            ".tdata",
            ".init_array",
            ".fini_array",
            ".preinit_array",
            ".gcc_except_table",
            ".ctors",
            ".dtors",
            ".jcr",
            ".debug",
            ".stab",
            ".tm_clone_table",
            ".sdata",
            ".sbss",
            ".lrodata",
            ".ldata",
            // the Go linker's own section set, emitted by every Go toolchain
            ".typelink",
            ".itablink",
            ".gosymtab",
            ".gopclntab",
            ".noptrdata",
            ".noptrbss",
            ".zdebug",
            ".go.buildinfo",
        ],
    ),
    // Mach-O sections are all `__`-prefixed by construction, so the prefix alone
    // is the convention
    ("mach-o", &["__"]),
];

/// A packing tool's structural fingerprint.
///
/// Each entry names *what the tool writes into the image*, which is the only
/// evidence that can carry a product name. Adding an entry is a statement about
/// a packing tool's output format — a platform fact with the same lifetime as
/// the tool — not a statement about any sample.
struct PackerMarker {
    /// The product name to report.
    name: &'static str,
    /// Section names that must **all** be present for the marker to match
    /// (case-insensitive, matched as prefixes so numbered variants are covered).
    sections: &'static [&'static str],
    /// A byte string the tool's stub or header embeds, when it has a distinctive
    /// one. Matching this alone is enough, since these are not sequences that
    /// occur by accident.
    magic: Option<&'static [u8]>,
}

/// Structural fingerprints of common packing and protection tools.
///
/// Deliberately short and deliberately specific. A marker that could match a
/// legitimately-built binary does not belong here, because the whole purpose of
/// the table is to be the one piece of evidence trusted enough to put a product
/// name in front of an analyst.
const PACKER_MARKERS: &[PackerMarker] = &[
    PackerMarker {
        name: "UPX",
        sections: &["upx0", "upx1"],
        magic: Some(b"UPX!"),
    },
    PackerMarker {
        name: "ASPack",
        sections: &[".aspack"],
        magic: None,
    },
    PackerMarker {
        name: "ASProtect",
        sections: &[".aspr"],
        magic: None,
    },
    PackerMarker {
        name: "FSG",
        sections: &[],
        magic: Some(b"FSG!"),
    },
    PackerMarker {
        name: "PECompact",
        sections: &["pec"],
        magic: Some(b"PEC2"),
    },
    PackerMarker {
        name: "Themida/WinLicense",
        sections: &[".themida"],
        magic: None,
    },
    PackerMarker {
        name: "Themida/WinLicense",
        sections: &[".winlice"],
        magic: None,
    },
    PackerMarker {
        name: "VMProtect",
        sections: &[".vmp0"],
        magic: None,
    },
    PackerMarker {
        name: "MPRESS",
        sections: &[".mpress1"],
        magic: None,
    },
    PackerMarker {
        name: "Petite",
        sections: &[".petite"],
        magic: None,
    },
    PackerMarker {
        name: "Enigma Protector",
        sections: &[".enigma1"],
        magic: None,
    },
    PackerMarker {
        name: "NsPack",
        sections: &["nsp0", "nsp1"],
        magic: None,
    },
    PackerMarker {
        name: "Obsidium",
        sections: &[".obsidium"],
        magic: None,
    },
    PackerMarker {
        name: "kkrunchy",
        sections: &["kkrunchy"],
        magic: None,
    },
    PackerMarker {
        name: "RLPack",
        sections: &[".rlpack"],
        magic: None,
    },
    PackerMarker {
        name: "Upack",
        sections: &[".upack"],
        magic: None,
    },
    PackerMarker {
        name: "Y0da Crypter",
        sections: &[".y0da"],
        magic: None,
    },
];

/// Signal 1's headline label: the reserved unpack target and its source.
const UNPACK_TARGET_LABEL: &str = "reserved unpack target";

/// Signal 2's headline label: a writable and executable section.
const WRITABLE_EXECUTABLE_LABEL: &str = "writable executable section";

/// Signal 3's headline label: a substantial high-entropy section.
const HIGH_ENTROPY_LABEL: &str = "high-entropy section";

/// Signal 4's headline label: an import table shaped like a loader's.
const THIN_IMPORTS_LABEL: &str = "loader-shaped import table";

/// Signal 5's headline label: an entry point in a writable section.
const WRITABLE_ENTRY_LABEL: &str = "entry point in a writable section";

/// Signal 6's headline label: a section-name set no linker emits.
const NAMING_LABEL: &str = "unconventional section names";

/// Signal 7's headline label: a compressed file body or appended region.
const FILE_ENTROPY_LABEL: &str = "compressed file body";

/// The signals that put the unpacking stub itself in evidence: the space it
/// reserves, the permissions it needs, the entry arrangement it uses, or the
/// import table it resolves the rest of the program with.
const STUB_SIGNALS: &[&str] = &[
    UNPACK_TARGET_LABEL,
    WRITABLE_EXECUTABLE_LABEL,
    THIN_IMPORTS_LABEL,
    WRITABLE_ENTRY_LABEL,
];

/// One structural observation supporting a packing conclusion.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Signal {
    /// A short label naming the observation, for the finding's headline.
    label: &'static str,
    /// The measured specifics, for the analyst-facing reasoning.
    detail: String,
}

impl Signal {
    /// Build a signal.
    ///
    /// # Arguments
    ///
    /// * `label` - The short label naming the observation
    /// * `detail` - The measured specifics
    fn new(label: &'static str, detail: impl Into<String>) -> Self {
        Signal {
            label,
            detail: detail.into(),
        }
    }
}

#[async_trait::async_trait]
impl Detection for Packing {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "packing"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1027_002
    }

    /// Measure the image's structure and report if it looks packed.
    #[tracing::instrument(name = "Packing::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // the loaded view (permissions, real bytes) and the raw file (the header,
        // the on-disk extents, the overlay) — both optional enrichment
        let sections = ctx.sections().await.unwrap_or_else(|error| {
            tracing::warn!(?error, "memory map unavailable; skipping packing analysis");
            Vec::new()
        });
        let raw = ctx.raw_bytes().await.unwrap_or_default();
        let header = parse_header(&raw, ctx.program.executable_format.as_deref());
        // with neither view there is nothing to measure
        if sections.is_empty() && header.is_none() {
            return Ok(Vec::new());
        }
        // gather every structural observation, then decide once
        let signals = collect_signals(ctx, &sections, &raw, header.as_ref());
        if signals.is_empty() {
            return Ok(Vec::new());
        }
        // a marker match is the only thing that may put a product name on this
        let family = match_marker(&sections, header.as_ref(), &raw);
        Ok(vec![build_hit(
            ctx,
            &sections,
            &raw,
            header.as_ref(),
            &signals,
            family,
        )])
    }
}

/// Run every structural check and collect the ones that fired.
///
/// # Arguments
///
/// * `ctx` - The analysis context (for the import list)
/// * `sections` - Ghidra's loaded memory map
/// * `raw` - The sample's file bytes
/// * `header` - The parsed format header, when one could be read
fn collect_signals(
    ctx: &AnalysisContext,
    sections: &[Section],
    raw: &[u8],
    header: Option<&FormatHeader>,
) -> Vec<Signal> {
    // the unpack-target pair goes first so its source section can be excluded
    // from the high-entropy check — one set of bytes must not score twice
    let unpack = unpack_target_signal(sections, header);
    let counted = unpack.as_ref().map(|(_, source)| *source);
    // each remaining check is independent and contributes at most one signal
    [
        unpack.map(|(signal, _)| signal),
        writable_executable_signal(sections),
        high_entropy_section_signal(sections, raw.len() as u64, counted),
        thin_import_signal(ctx),
        entry_point_signal(sections, header),
        section_naming_signal(ctx.program.executable_format.as_deref(), sections, header),
        file_entropy_signal(raw, sections, header),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Signal 1 — a section reserved to be written into, paired with the compressed
/// bytes that would fill it.
///
/// Returns the signal plus the source section's base address, so the caller can
/// keep the high-entropy check from counting the same section a second time.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `header` - The parsed format header, when one could be read
fn unpack_target_signal(
    sections: &[Section],
    header: Option<&FormatHeader>,
) -> Option<(Signal, u64)> {
    let header = header?;
    // a section mapped far larger than it is stored, or stored not at all
    let target = header.sections.iter().find(|section| {
        section.virtual_size >= MIN_ENTROPY_LEN
            && section.virtual_size / UNPACK_TARGET_RATIO > section.raw_size
    })?;
    // ...and, elsewhere, compressed-looking bytes small enough to be its source.
    // Largest wins rather than first: the compressed body is the bulk of the file,
    // and taking the first match in address order hands the role to whatever small
    // high-entropy block happens to sit lowest in the map.
    let source = sections
        .iter()
        .filter(|section| {
            !is_synthetic_block(&section.name)
                && section.name.trim() != target.name
                && section.size >= MIN_ENTROPY_LEN
                && section.size < target.virtual_size
                && section.entropy().is_some_and(|value| value >= HIGH_ENTROPY)
        })
        .max_by_key(|section| section.size)?;
    Some((
        Signal::new(
            UNPACK_TARGET_LABEL,
            format!(
                "section `{}` is mapped {} bytes but stores only {} on disk, while `{}` holds {} \
                 bytes at entropy {:.2} — the shape of a region reserved for code to be written \
                 into, and a compressed body small enough to be its source",
                target.name,
                target.virtual_size,
                target.raw_size,
                source.name.trim(),
                source.size,
                source.entropy().unwrap_or_default(),
            ),
        ),
        source.base,
    ))
}

/// Signal 2 — a section that can be both modified and run.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
fn writable_executable_signal(sections: &[Section]) -> Option<Signal> {
    // a section the loader maps writable *and* executable needs no VirtualProtect
    let section = sections.iter().find(|section| {
        !is_synthetic_block(&section.name) && section.is_writable() && section.is_executable()
    })?;
    Some(Signal::new(
        WRITABLE_EXECUTABLE_LABEL,
        format!(
            "section `{}` is mapped `{}` — code can be rewritten in place and run without ever \
             changing page permissions",
            section.name.trim(),
            section.permissions,
        ),
    ))
}

/// Signal 3 — a substantial section whose bytes look compressed or encrypted.
///
/// A compressed icon, image, or string table is routine, so a non-executable
/// section only counts when it is large in absolute terms or relative to the
/// file — big enough to be a packed body or a carried payload.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `raw_len` - The raw file's length in bytes (zero when unavailable)
/// * `counted` - The base of a section another signal already measured, if any
fn high_entropy_section_signal(
    sections: &[Section],
    raw_len: u64,
    counted: Option<u64>,
) -> Option<Signal> {
    // the highest-entropy section large enough for the reading to mean something
    let section = sections
        .iter()
        .filter(|section| !is_synthetic_block(&section.name))
        // the unpack-target signal's source is the same observation, not a
        // second one
        .filter(|section| Some(section.base) != counted)
        .filter(|section| section.size >= MIN_ENTROPY_LEN)
        // packed code is executable; a data section must be sizeable before a
        // compressed asset reads as a packing signal
        .filter(|section| {
            section.is_executable()
                || section.size >= CARRIER_FLOOR
                || (raw_len > 0 && section.size.saturating_mul(5) >= raw_len)
        })
        .filter(|section| section.entropy().is_some_and(|value| value >= HIGH_ENTROPY))
        .max_by(|a, b| {
            a.entropy()
                .unwrap_or_default()
                .total_cmp(&b.entropy().unwrap_or_default())
        })?;
    Some(Signal::new(
        HIGH_ENTROPY_LABEL,
        format!(
            "section `{}` holds {} bytes at entropy {:.2} of a possible 8.00 — compressed or \
             encrypted rather than compiled",
            section.name.trim(),
            section.size,
            section.entropy().unwrap_or_default(),
        ),
    ))
}

/// Signal 4 — an import table that describes a loader rather than a program.
///
/// # Arguments
///
/// * `ctx` - The analysis context (for the import list)
fn thin_import_signal(ctx: &AnalysisContext) -> Option<Signal> {
    // an empty import list usually means Ghidra could not read one, which is not
    // the same as the program not having one
    if ctx.imports.is_empty() || ctx.imports.len() >= THIN_IMPORT_TABLE {
        return None;
    }
    // classify the few imports there are
    let names: Vec<String> = ctx
        .imports
        .iter()
        .map(|symbol| symbol.name.to_ascii_lowercase())
        .collect();
    let resolvers = matching(&names, RESOLVER_IMPORTS);
    let memory = matching(&names, MEMORY_IMPORTS);
    // the stub's minimum toolkit: find code at runtime, and make memory runnable
    if resolvers.len() < 2 || memory.is_empty() {
        return None;
    }
    Some(Signal::new(
        THIN_IMPORTS_LABEL,
        format!(
            "only {} import(s), of which {} resolve code at runtime ({}) and {} change or allocate \
             executable memory ({}) — the minimum a program needs to reconstitute its own code, \
             and far less than one needs to do anything else",
            ctx.imports.len(),
            resolvers.len(),
            resolvers.join(", "),
            memory.len(),
            memory.join(", "),
        ),
    ))
}

/// Signal 5 — execution starts in a section that can rewrite itself.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `header` - The parsed format header, when one could be read
fn entry_point_signal(sections: &[Section], header: Option<&FormatHeader>) -> Option<Signal> {
    let header = header?;
    let entry = header.entry_section()?;
    // starting outside the first section is unusual on its own but not damning
    let first = header.sections.first()?;
    if entry.name == first.name {
        return None;
    }
    // ...it is the combination with writability that says "decompress in place"
    let loaded = sections
        .iter()
        .find(|section| section.name.trim() == entry.name)?;
    if !loaded.is_writable() {
        return None;
    }
    // The header states the entry relative to the image base, which is not the
    // address anything else in the report is written in. Rebasing it through the
    // section that contains it — same offset, actual mapped base — is what makes
    // it the address an analyst can go look at.
    let entry_point = header.entry_point.unwrap_or_default();
    let mapped = loaded
        .base
        .saturating_add(entry_point.saturating_sub(entry.address));
    Some(Signal::new(
        WRITABLE_ENTRY_LABEL,
        format!(
            "execution starts at {} in section `{}` (mapped `{}`), not in the image's first \
             section `{}` — the arrangement a stub uses to decompress into the space it is \
             running from",
            fmt_addr(mapped),
            entry.name,
            loaded.permissions,
            first.name,
        ),
    ))
}

/// Signal 6 — a section-name set no linker for this format would produce.
///
/// # Arguments
///
/// * `format` - Ghidra's `executable_format` string, when known
/// * `sections` - Ghidra's loaded memory map
/// * `header` - The parsed format header, when one could be read
fn section_naming_signal(
    format: Option<&str>,
    sections: &[Section],
    header: Option<&FormatHeader>,
) -> Option<Signal> {
    // the header's names are authoritative; fall back to the loaded block names
    let mut names = section_names(sections, header);
    // for an image that declares no section table, Ghidra invents `segment_N`
    // block names — loader inventory, not evidence the table was rewritten
    if header.is_none_or(|header| header.sections.is_empty()) {
        names.retain(|name| !is_loader_segment(name));
    }
    if names.is_empty() {
        return None;
    }
    // without a known format there is no convention to compare against
    let conventional = conventional_names(format)?;
    // split the table into names the format explains and names it does not
    let unusual: Vec<&String> = names
        .iter()
        .filter(|name| !is_conventional(name, conventional))
        .collect();
    // one odd section is a custom `#pragma section` or a linker script; a table
    // that is *mostly* odd is a different image than the one that was compiled
    if unusual.is_empty() || unusual.len() * 2 < names.len() {
        return None;
    }
    Some(Signal::new(
        NAMING_LABEL,
        format!(
            "{} of {} sections carry names no linker for this format emits ({}) — the section \
             table was rewritten after the program was built",
            unusual.len(),
            names.len(),
            unusual
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", "),
        ),
    ))
}

/// Signal 7 — the file as a whole, or a region appended to it, looks compressed.
///
/// Two exclusions keep this an independent reading: a whole-file figure
/// explained by one dominant non-executable section is the high-entropy-section
/// measurement again (so it is skipped), and an Authenticode certificate table
/// the header declares is a signed binary's routine tail (so it is carved out
/// of the overlay before measuring).
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `sections` - Ghidra's loaded memory map
/// * `header` - The parsed format header, when one could be read
fn file_entropy_signal(
    raw: &[u8],
    sections: &[Section],
    header: Option<&FormatHeader>,
) -> Option<Signal> {
    // too small for the reading to be meaningful
    if (raw.len() as u64) < MIN_ENTROPY_LEN {
        return None;
    }
    let mut parts = Vec::new();
    // a whole-file figure dominated by one large non-executable section is that
    // section's own reading diluted, not a second observation; an executable
    // dominator (packed code itself) keeps the reading
    let dominated = dominant_section(sections).is_some_and(|section| {
        !section.is_executable()
            && section.size.saturating_mul(5) >= (raw.len() as u64).saturating_mul(3)
    });
    // the whole file, which the header and the padding would ordinarily dilute
    let whole = crate::thorpal::detect::decode::shannon_entropy(raw);
    if !dominated && whole >= HIGH_FILE_ENTROPY {
        parts.push(format!(
            "the file as a whole reads at entropy {whole:.2}, leaving no room for the low-entropy \
             header, padding, and string data every image carries"
        ));
    }
    // anything appended past the last section the header declares, minus the
    // certificate table a signed binary legitimately appends there
    if let Some(end) = header.and_then(FormatHeader::image_end) {
        let cert = header.and_then(|header| header.security_dir);
        let (overlay, start, past_cert) = overlay_without_cert(raw, end, cert);
        let entropy = crate::thorpal::detect::decode::shannon_entropy(&overlay);
        if overlay.len() as u64 >= MIN_ENTROPY_LEN && entropy >= HIGH_ENTROPY {
            // name the cert exclusion so the measurement is reproducible
            let past = if past_cert {
                "past the last declared section and the Authenticode certificate table"
            } else {
                "past the last declared section"
            };
            parts.push(format!(
                "{} bytes are appended {past} at {} with entropy {entropy:.2}",
                overlay.len(),
                fmt_addr(start),
            ));
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(Signal::new(FILE_ENTROPY_LABEL, parts.join("; ")))
}

/// The overlay bytes to measure, with any declared certificate table carved out.
///
/// A signed PE's Authenticode certificate table is appended past the image and
/// reads as high-entropy DER, so the security data directory's declared range
/// is subtracted before the overlay is measured. Returns the bytes to measure,
/// the file offset of their first byte, and whether a cert range was excluded.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `end` - The file offset the overlay begins at
/// * `security_dir` - The declared security directory `(offset, size)`, if any
fn overlay_without_cert(
    raw: &[u8],
    end: u64,
    security_dir: Option<(u64, u64)>,
) -> (Cow<'_, [u8]>, u64, bool) {
    // index form of the overlay start; an offset past the address space simply
    // reads as "no overlay" on the slice lookups below
    let end_idx = usize::try_from(end).unwrap_or(usize::MAX);
    // the overlay as declared: everything past the last section's on-disk bytes
    let overlay = raw.get(end_idx..).unwrap_or_default();
    // with no declared cert range the whole overlay is the measurement
    let Some((cert_offset, cert_size)) = security_dir else {
        return (Cow::Borrowed(overlay), end, false);
    };
    // clamp the declared range to the file and to the overlay itself, so a
    // header that lies about its extent cannot void the measurement
    let len = raw.len() as u64;
    let cert_start = cert_offset.min(len).max(end);
    let cert_end = cert_offset.saturating_add(cert_size).min(len);
    if cert_end <= cert_start {
        return (Cow::Borrowed(overlay), end, false);
    }
    // stitch together the bytes around the cert table: appended payload can sit
    // on either side of a grafted signature (both bounds are clamped to the file
    // length, so the index conversions cannot actually fail)
    let cert_start_idx = usize::try_from(cert_start).unwrap_or(usize::MAX);
    let cert_end_idx = usize::try_from(cert_end).unwrap_or(usize::MAX);
    let before = raw.get(end_idx..cert_start_idx).unwrap_or_default();
    let after = raw.get(cert_end_idx..).unwrap_or_default();
    let mut rest = Vec::with_capacity(before.len() + after.len());
    rest.extend_from_slice(before);
    rest.extend_from_slice(after);
    // point the detail at the first byte that is not the certificate table
    let start = if before.is_empty() { cert_end } else { end };
    (Cow::Owned(rest), start, true)
}

/// Assemble the finding from the signals that fired.
///
/// # Arguments
///
/// * `ctx` - The analysis context (import count + entry-point attribution)
/// * `sections` - Ghidra's loaded memory map
/// * `raw` - The sample's file bytes
/// * `header` - The parsed format header, when one could be read
/// * `signals` - The structural observations that fired
/// * `family` - The packing tool named by a marker match, if any
fn build_hit(
    ctx: &AnalysisContext,
    sections: &[Section],
    raw: &[u8],
    header: Option<&FormatHeader>,
    signals: &[Signal],
    family: Option<&'static str>,
) -> DetectionHit {
    // the payload-carrier shape: compressed cargo inside an otherwise ordinary
    // program, which must be worded as cargo rather than as packed code
    let carrier = family.is_none() && carrier_shape(ctx.imports.len(), sections, signals);
    // grade the finding: a marker is identification, a carrier is weighed by its
    // cargo, and bare structure is weighed by how many signals agree
    let severity = severity_for(
        family,
        carrier,
        substantial_carrier(sections, raw.len() as u64),
        signals.len(),
    );
    // place the finding: the entry stub only when the evidence points at one,
    // otherwise the compressed body itself at file level
    let (address, function, role) = attribute(ctx, sections, raw, header, signals, family);
    // the headline and the reasoning: what was concluded, and on what basis
    let (content, reasoning) = compose_claim(family, carrier, signals);
    // the artifact: the value is the conclusion, the detail is the measurement
    let artifact = HitArtifact::new(
        ArtifactClass::Packer,
        family.unwrap_or(if carrier {
            "payload carrier"
        } else {
            "unidentified packer"
        }),
    )
    .detail(section_inventory(sections, role.is_none()));
    let artifact = match role {
        Some(role) => artifact.role(role),
        None => artifact,
    };
    DetectionHit::new(T1027_002, address, function, severity, content, reasoning)
        .kind(EvidenceKind::ContextOnly)
        .artifact(artifact)
}

/// The severity for a packing conclusion.
///
/// A named family is the only evidence strong enough for [`high`]. A payload
/// carrier is graded by its cargo: a substantial carrier keeps [`medium`] so
/// the unpack question still derives from it, a small one is context-only.
/// Bare structure follows how many independent signals agree.
///
/// # Arguments
///
/// * `family` - The packing tool named by a marker match, if any
/// * `carrier` - Whether the finding is the payload-carrier shape
/// * `substantial` - Whether the carrier's cargo is substantial
/// * `count` - How many structural signals fired
fn severity_for(
    family: Option<&'static str>,
    carrier: bool,
    substantial: bool,
    count: usize,
) -> (Confidence, i64) {
    // decide once over the whole evidence shape
    match (family, carrier, count) {
        // a marker identifies the tool outright
        (Some(_), _, _) => high(),
        // a substantial carrier warrants the unpack question even on one
        // signal; a small one is ordinary embedded data
        (None, true, _) => {
            if substantial {
                medium()
            } else {
                low()
            }
        }
        // bare structure: one signal is context, several agreeing are a finding
        (None, false, 1) => low(),
        (None, false, _) => medium(),
    }
}

/// Whether the fired signals describe a payload carrier rather than packed code.
///
/// The carrier shape: every signal is an entropy reading, the hottest section
/// is data rather than code, the import table is a full program's rather than a
/// stub's, and the visible executable sections read as ordinary compiled code.
/// The compressed bytes are then cargo the program carries, not the program's
/// own code hidden from view.
///
/// # Arguments
///
/// * `import_count` - How many imports the program declares
/// * `sections` - Ghidra's loaded memory map
/// * `signals` - The structural observations that fired
fn carrier_shape(import_count: usize, sections: &[Section], signals: &[Signal]) -> bool {
    // any non-entropy signal is structural evidence about the image itself, and
    // the packed-code claim must stand
    let entropy_only = signals
        .iter()
        .all(|signal| signal.label == HIGH_ENTROPY_LABEL || signal.label == FILE_ENTROPY_LABEL);
    if !entropy_only {
        return false;
    }
    // the hottest substantial section must be data: hot executable bytes are
    // packed code, not cargo
    if dominant_section(sections).is_none_or(Section::is_executable) {
        return false;
    }
    // a stub strips its import table down to a loader's; a full table says the
    // visible program is a real one
    if import_count < THIN_IMPORT_TABLE {
        return false;
    }
    // the visible code must exist and read as ordinary compiled code
    let readings: Vec<f32> = sections
        .iter()
        .filter(|section| !is_synthetic_block(&section.name) && section.is_executable())
        .filter_map(Section::entropy)
        .collect();
    !readings.is_empty() && readings.iter().all(|value| *value < PLAIN_CODE_ENTROPY)
}

/// Whether a carrier's cargo is big enough to be worth the unpack question.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `raw_len` - The raw file's length in bytes
fn substantial_carrier(sections: &[Section], raw_len: u64) -> bool {
    // the cargo is the hottest substantial section; 64 KiB of compressed data,
    // or a quarter of the file, is a payload rather than an icon
    dominant_section(sections).is_some_and(|section| {
        section.size >= CARRIER_FLOOR || section.size.saturating_mul(4) >= raw_len
    })
}

/// Compose the finding's headline and analyst-facing reasoning.
///
/// # Arguments
///
/// * `family` - The packing tool named by a marker match, if any
/// * `carrier` - Whether the finding is the payload-carrier shape
/// * `signals` - The structural observations that fired
fn compose_claim(
    family: Option<&'static str>,
    carrier: bool,
    signals: &[Signal],
) -> (String, String) {
    // the headline: what was concluded, and on what basis
    let content = match family {
        Some(name) => format!(
            "Packed with {name} — {} structural signal(s) plus a {name} marker",
            signals.len()
        ),
        None if carrier => format!(
            "Embedded high-entropy payload carrier — {} structural signal(s)",
            signals.len()
        ),
        None => format!(
            "Packed or obfuscated image — {} structural signal(s)",
            signals.len()
        ),
    };
    // the reasoning: the conclusion, every measurement, then the caveat the
    // score depends on
    let mut reasoning = match family {
        Some(name) => format!(
            "The image carries {name}'s own section-table and stub markers, so the packing tool is \
             identified rather than inferred. Supporting structure: "
        ),
        None if carrier => String::from(
            "The image carries large high-entropy embedded data — a resource or overlay payload \
             carrier. The visible code appears to be the code that runs: the import table is a \
             full program's and the executable sections read as ordinary compiled code, so carve \
             out and identify the payload rather than trying to unpack the program. Observed: ",
        ),
        None => String::from(
            "The image's structure indicates its executable code is stored compressed or \
             encrypted and reconstituted at runtime. No packing tool could be identified — the \
             conclusion rests on structure alone, so no product name is asserted. Observed: ",
        ),
    };
    reasoning.push_str(
        &signals
            .iter()
            .map(|signal| format!("{} — {}", signal.label, signal.detail))
            .collect::<Vec<_>>()
            .join("; "),
    );
    // the caveat and the practical consequence differ by claim: carried cargo
    // leaves the visible findings valid, packed code invalidates them
    if carrier {
        reasoning.push_str(
            ". Note that compressed payloads are entirely normal in installers, self-extracting \
             archives, commercially protected software, and binaries with large embedded assets; \
             this finding points at embedded data worth identifying, not at malice.",
        );
        reasoning.push_str(
            " The strings, imports, and control flow in this report describe the program itself \
             and remain valid; the embedded data is cargo to carve out and identify separately.",
        );
    } else {
        reasoning.push_str(
            ". Note that compressed payloads are also entirely normal in installers, \
             self-extracting archives, commercially protected software, and binaries with large \
             embedded assets; this finding says the code on disk is not the code that runs, not \
             that the program is malicious.",
        );
        reasoning.push_str(
            " Treat every other finding in this report as describing the visible code only: the \
             strings, imports, and control flow of the packed body are not present in this image \
             and must be recovered by unpacking or dynamic analysis.",
        );
    }
    (content, reasoning)
}

/// The hottest substantial section — the bytes a packing conclusion is about.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
fn dominant_section(sections: &[Section]) -> Option<&Section> {
    // the same pool the high-entropy signal reads: real sections, large enough
    // for an entropy reading to mean something
    sections
        .iter()
        .filter(|section| !is_synthetic_block(&section.name))
        .filter(|section| section.size >= MIN_ENTROPY_LEN)
        .filter(|section| section.entropy().is_some())
        .max_by(|a, b| {
            a.entropy()
                .unwrap_or_default()
                .total_cmp(&b.entropy().unwrap_or_default())
        })
}

/// Whether the evidence points at an unpacking stub rather than carried data.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `signals` - The structural observations that fired
/// * `family` - The packing tool named by a marker match, if any
fn stub_shaped(sections: &[Section], signals: &[Signal], family: Option<&'static str>) -> bool {
    // a marker, a signal describing the stub's own arrangements, or packed code
    // itself (an executable hottest section) all put a stub in evidence
    family.is_some()
        || signals
            .iter()
            .any(|signal| STUB_SIGNALS.contains(&signal.label))
        || dominant_section(sections).is_some_and(Section::is_executable)
}

/// Locate the finding's site: the entry stub when the evidence points at one,
/// else the compressed body itself.
///
/// Returns the site address, the containing function, and the role phrase
/// describing why that function is the relevant one. The entry function and the
/// stub role are claimed only on stub-shaped evidence (see [`stub_shaped`]) — a
/// bare entropy reading anchors file-level on the compressed bytes instead, so
/// a stock CRT entry is never presented as a packer stub.
///
/// # Arguments
///
/// * `ctx` - The analysis context (function list + containment lookup)
/// * `sections` - Ghidra's loaded memory map
/// * `raw` - The sample's file bytes
/// * `header` - The parsed format header, when one could be read
/// * `signals` - The structural observations that fired
/// * `family` - The packing tool named by a marker match, if any
fn attribute(
    ctx: &AnalysisContext,
    sections: &[Section],
    raw: &[u8],
    header: Option<&FormatHeader>,
    signals: &[Signal],
    family: Option<&'static str>,
) -> (u64, Option<u64>, Option<&'static str>) {
    // the names loaders and toolchains give the program's first instruction;
    // asking Ghidra beats recomputing the entry from the header, which would
    // need per-format rebasing to line up with the loaded image
    const ENTRY_NAMES: &[&str] = &[
        "entry",
        "_start",
        "start",
        "mainCRTStartup",
        "wmainCRTStartup",
        "WinMainCRTStartup",
        "wWinMainCRTStartup",
        "_mainCRTStartup",
    ];
    // only stub-shaped evidence may put the stub role on the entry function
    let stub = stub_shaped(sections, signals, family);
    if stub {
        // the entry function is the stub: it is what runs before the real code
        // exists
        let entry = ctx
            .functions
            .iter()
            .filter(|function| ENTRY_NAMES.contains(&function.name.as_str()))
            .find_map(|function| {
                parse_addr(&function.entry_point).or_else(|| parse_addr(&function.address))
            });
        if let Some(entry) = entry {
            return (
                entry,
                Some(entry),
                Some("entry point — the code that reconstitutes the packed body before it runs"),
            );
        }
    }
    // point at the compressed body itself: the hottest section when one cleared
    // the entropy bar
    let body = dominant_section(sections)
        .filter(|section| section.entropy().is_some_and(|value| value >= HIGH_ENTROPY));
    if let Some(section) = body {
        // only stub-shaped evidence may blame the code around the body; a bare
        // entropy reading stays file-level so no function is accused
        let function = if stub {
            ctx.containing_function(section.base)
        } else {
            None
        };
        return (section.base, function, None);
    }
    // the appended data, when the compressed thing is the overlay
    if let Some(end) = header.and_then(FormatHeader::image_end)
        && (raw.len() as u64) > end
    {
        return (end, None, None);
    }
    // nothing cleared the bar: the hottest section regardless, else the image base
    match dominant_section(sections) {
        Some(section) => {
            // the same file-level rule applies to this weaker anchor
            let function = if stub {
                ctx.containing_function(section.base)
            } else {
                None
            };
            (section.base, function, None)
        }
        None => (0, None, None),
    }
}

/// Render the section table as the artifact's supporting detail.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `unattributed` - Whether the hit names no stub function, which must be said
///   explicitly rather than left to look like an omission
fn section_inventory(sections: &[Section], unattributed: bool) -> String {
    // one compact entry per section: where it is, how big, and how compressed
    let table = sections
        .iter()
        .map(|section| {
            let entropy = section.entropy().map_or_else(
                || "no bytes".to_string(),
                |value| format!("entropy {value:.2}"),
            );
            format!(
                "{} @ {} ({} bytes, {}, {})",
                section.name.trim(),
                fmt_addr(section.base),
                section.size,
                section.permissions,
                entropy,
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let mut detail = format!("Section entropy: {table}");
    if unattributed {
        detail.push_str(
            ". No function is attributed for an unpacking stub — the address above is the \
             compressed body or appended data, not code shown to decompress it.",
        );
    }
    detail
}

/// Name the packing tool, but only on a real marker match.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `header` - The parsed format header, when one could be read
/// * `raw` - The sample's file bytes, searched for stub magic
fn match_marker(
    sections: &[Section],
    header: Option<&FormatHeader>,
    raw: &[u8],
) -> Option<&'static str> {
    // compare against the union of both views: a packer's sections show up in the
    // header, but Ghidra sometimes renames or merges them in the loaded image.
    // Names shorter than three characters or carrying non-printable bytes are
    // discarded first: below that floor a truncated or garbage name (a NUL-led
    // header field most of all) satisfies the reversed prefix test for
    // essentially every marker in the table
    let names: Vec<String> = section_names(sections, header)
        .iter()
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| name.len() >= 3 && name.bytes().all(|byte| byte.is_ascii_graphic()))
        .collect();
    PACKER_MARKERS
        .iter()
        .find(|marker| {
            // every named section must be present, as a prefix match so numbered
            // variants (`upx0`/`upx1`, `.vmp0`/`.vmp1`) are covered by one entry;
            // the reversed leg serves truncated names (`upx`), kept honest by the
            // three-character floor above
            let by_section = !marker.sections.is_empty()
                && marker.sections.iter().all(|wanted| {
                    names
                        .iter()
                        .any(|name| name.starts_with(wanted) || wanted.starts_with(name.as_str()))
                });
            // stub magic is distinctive enough to stand alone
            let by_magic = marker.magic.is_some_and(|magic| contains(raw, magic));
            by_section || by_magic
        })
        .map(|marker| marker.name)
}

/// The section names to reason about: the header's when available, else Ghidra's.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `header` - The parsed format header, when one could be read
fn section_names(sections: &[Section], header: Option<&FormatHeader>) -> Vec<String> {
    // the header is the authoritative table; Ghidra may split or synthesize
    // blocks. A NUL-led or binary-garbage name field is discarded outright: the
    // empty string it decodes to is not a name anything wrote on purpose
    let mut names: Vec<String> = header.map_or_else(Vec::new, |header| {
        header
            .sections
            .iter()
            .map(|section| section.name.trim().to_string())
            .filter(|name| is_printable_name(name))
            .collect()
    });
    // fold in any loaded block the header did not account for, skipping the ones
    // the loader made up — they are in the memory map but not in the image, so
    // counting them puts a name the format never emits into every single table
    let seen: HashSet<String> = names.iter().cloned().collect();
    names.extend(
        sections
            .iter()
            .map(|section| section.name.trim().to_string())
            .filter(|name| !name.is_empty() && !seen.contains(name))
            .filter(|name| !is_synthetic_block(name)),
    );
    // the synthetic entry the ELF parse appends is not a real section name
    names.retain(|name| !name.starts_with('<'));
    names
}

/// The conventional section-name set for a detected executable format.
///
/// # Arguments
///
/// * `format` - Ghidra's `executable_format` string, when known
fn conventional_names(format: Option<&str>) -> Option<&'static [&'static str]> {
    // match the format string against the table's keys, loosest first
    let format = format?.to_ascii_lowercase();
    CONVENTIONAL_SECTIONS
        .iter()
        .find(|(key, _)| match *key {
            // Ghidra spells this "Portable Executable (PE)"
            "pe" => format.contains("portable executable") || format.contains("pe "),
            key => format.contains(key),
        })
        .map(|(_, names)| *names)
}

/// Whether a loaded block is one Ghidra's loader synthesized rather than one the
/// image declares.
///
/// # Arguments
///
/// * `name` - The loaded block's name
fn is_synthetic_block(name: &str) -> bool {
    // exact match, not a prefix: a packer is free to name a real section `tdb0`
    let name = name.trim().to_ascii_lowercase();
    SYNTHETIC_BLOCKS.contains(&name.as_str())
}

/// Whether a declared section name is real text rather than a garbage field.
///
/// # Arguments
///
/// * `name` - The trimmed name to test
fn is_printable_name(name: &str) -> bool {
    // a NUL-led PE name field decodes to the empty string and a corrupted one
    // to control or non-ASCII bytes; neither is a name a toolchain wrote
    !name.is_empty() && name.bytes().all(|byte| (0x20..0x7f).contains(&byte))
}

/// Whether a block name is a `segment_N` placeholder Ghidra's loader invents
/// for an image that declares no section table.
///
/// # Arguments
///
/// * `name` - The block name to test
fn is_loader_segment(name: &str) -> bool {
    // the loader numbers its invented blocks `segment_0`, `segment_1`, ...
    let name = name.trim().to_ascii_lowercase();
    name.strip_prefix("segment_").is_some_and(|digits| {
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
    })
}

/// Whether a section name is one the format conventionally produces.
///
/// # Arguments
///
/// * `name` - The section name to test
/// * `conventional` - The format's conventional name stems
fn is_conventional(name: &str, conventional: &[&str]) -> bool {
    // stems, so `.note.gnu.build-id` matches `.note` and `.rela.plt` matches `.rela`
    let name = name.trim().to_ascii_lowercase();
    conventional.iter().any(|stem| name.starts_with(stem))
}

/// The subset of `names` containing any of the given substrings.
///
/// # Arguments
///
/// * `names` - The lower-cased import names
/// * `wanted` - The lower-cased substrings to look for
fn matching(names: &[String], wanted: &[&str]) -> Vec<String> {
    // substring rather than equality: the same call ships as `LoadLibraryA`,
    // `LoadLibraryExW`, and `__imp_LoadLibraryW`
    let mut found: Vec<String> = names
        .iter()
        .filter(|name| wanted.iter().any(|needle| name.contains(needle)))
        .cloned()
        .collect();
    found.sort();
    found.dedup();
    found
}

/// Whether `haystack` contains `needle`.
///
/// # Arguments
///
/// * `haystack` - The bytes to search
/// * `needle` - The byte string to find
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    // a plain window scan; the needles are 4 bytes and the files are small enough
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::fileinfo::HeaderSection;

    /// Build a loaded section for the tests.
    ///
    /// # Arguments
    ///
    /// * `name` - The block name
    /// * `base` - The block's base address
    /// * `permissions` - The permission string
    /// * `bytes` - The block's bytes
    fn section(name: &str, base: u64, permissions: &str, bytes: Vec<u8>) -> Section {
        Section {
            name: name.to_string(),
            base,
            end: base + bytes.len() as u64,
            size: bytes.len() as u64,
            permissions: permissions.to_string(),
            is_initialized: !bytes.is_empty(),
            is_loaded: true,
            bytes,
            truncated: false,
        }
    }

    /// Bytes that score near maximum entropy, without a random source.
    ///
    /// A repeating 0..=255 ramp has a perfectly flat byte histogram, which is
    /// exactly what Shannon entropy measures — so it reads 8.00 the same way
    /// compressed data does.
    ///
    /// # Arguments
    ///
    /// * `len` - How many bytes to produce
    fn flat(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 256) as u8).collect()
    }

    /// Bytes with a skewed histogram, standing in for ordinary code or text.
    ///
    /// # Arguments
    ///
    /// * `len` - How many bytes to produce
    fn low_entropy(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| if i % 8 == 0 { 0x55 } else { 0x00 })
            .collect()
    }

    /// A declared section for the tests.
    ///
    /// # Arguments
    ///
    /// * `name` - The section name
    /// * `address` - The mapped address
    /// * `virtual_size` - The mapped size
    /// * `raw_size` - The on-disk size
    fn declared(name: &str, address: u64, virtual_size: u64, raw_size: u64) -> HeaderSection {
        HeaderSection {
            name: name.to_string(),
            address,
            virtual_size,
            raw_offset: 0x400,
            raw_size,
            flags: 0,
        }
    }

    /// A section mapped far larger than it is stored, next to a compressed body,
    /// is the unpack-target shape.
    #[test]
    fn unpack_target_needs_both_the_space_and_the_source() {
        let header = FormatHeader {
            sections: vec![
                declared("UPX0", 0x1000, 0x40000, 0),
                declared("UPX1", 0x41000, 0x8000, 0x8000),
            ],
            ..Default::default()
        };
        let packed = vec![
            section("UPX0", 0x40_1000, "rwx", Vec::new()),
            section("UPX1", 0x44_1000, "rwx", flat(0x8000)),
        ];
        assert!(unpack_target_signal(&packed, Some(&header)).is_some());
        // the same reserved space with nothing compressed to fill it is just .bss
        let benign = vec![
            section("UPX0", 0x40_1000, "rw-", Vec::new()),
            section("UPX1", 0x44_1000, "r-x", low_entropy(0x8000)),
        ];
        assert!(unpack_target_signal(&benign, Some(&header)).is_none());
        // and with no header there are no declared on-disk sizes to compare
        assert!(unpack_target_signal(&packed, None).is_none());
    }

    /// Entropy signals need a section large enough for the reading to mean
    /// something.
    #[test]
    fn high_entropy_signal_ignores_short_sections() {
        // a short buffer scores high on noise alone
        let short = vec![section(".text", 0x40_1000, "r-x", flat(256))];
        assert!(high_entropy_section_signal(&short, 0x8000, None).is_none());
        // the same distribution over a real section is a genuine reading
        let long = vec![section(".text", 0x40_1000, "r-x", flat(0x8000))];
        let signal = high_entropy_section_signal(&long, 0x8000, None)
            .expect("a large flat section reads high");
        assert!(signal.detail.contains("entropy 8.00"));
        // ordinary code does not trip it
        let code = vec![section(".text", 0x40_1000, "r-x", low_entropy(0x8000))];
        assert!(high_entropy_section_signal(&code, 0x8000, None).is_none());
    }

    /// Blocks the loader synthesizes are not sections, and must not stand in for
    /// one in any shape signal.
    ///
    /// Ghidra maps a `Headers` block over every PE it opens. Left in, it is a
    /// small high-entropy block that outranks nothing but sorts first, so it wins
    /// the "compressed body" role in signal 1 and adds a permanent unconventional
    /// name to signal 6 — on every image, packed or not.
    #[test]
    fn synthetic_loader_blocks_are_not_sections() {
        let header = FormatHeader {
            sections: vec![
                declared("UPX0", 0x1000, 0x40000, 0),
                declared("UPX1", 0x41000, 0x8000, 0x8000),
            ],
            ..Default::default()
        };
        let loaded = vec![
            section("Headers", 0x40_0000, "r--", flat(0x1000)),
            section("UPX0", 0x40_1000, "rwx", Vec::new()),
            section("UPX1", 0x44_1000, "rwx", flat(0x8000)),
        ];
        // signal 1 names the real compressed body, not the header block
        let (signal, source) =
            unpack_target_signal(&loaded, Some(&header)).expect("the unpack shape");
        assert!(signal.detail.contains("`UPX1`"), "{}", signal.detail);
        assert!(!signal.detail.contains("Headers"), "{}", signal.detail);
        assert_eq!(source, 0x44_1000);
        // and the header block is not counted among the image's section names
        let names = section_names(&loaded, Some(&header));
        assert_eq!(names, vec!["UPX0".to_string(), "UPX1".to_string()]);
        assert!(is_synthetic_block("Headers") && is_synthetic_block("EXTERNAL"));
        // a real section is never mistaken for one on a prefix match
        assert!(!is_synthetic_block("tdb0") && !is_synthetic_block(".text"));
    }

    /// A writable+executable section is one signal; a conventionally-permissioned
    /// image is none.
    #[test]
    fn writable_executable_signal_needs_both_permissions() {
        let packed = vec![section("UPX1", 0x40_1000, "rwx", flat(64))];
        assert!(writable_executable_signal(&packed).is_some());
        let benign = vec![
            section(".text", 0x40_1000, "r-x", flat(64)),
            section(".data", 0x40_2000, "rw-", flat(64)),
        ];
        assert!(writable_executable_signal(&benign).is_none());
    }

    /// The entry-point signal needs the entry to be both outside the first section
    /// and in one that can be rewritten.
    #[test]
    fn entry_point_signal_requires_a_writable_non_first_section() {
        let header = FormatHeader {
            entry_point: Some(0x41100),
            sections: vec![
                declared("UPX0", 0x1000, 0x40000, 0),
                declared("UPX1", 0x41000, 0x8000, 0x8000),
            ],
            ..Default::default()
        };
        let writable = vec![section("UPX1", 0x44_1000, "rwx", flat(0x100))];
        let signal = entry_point_signal(&writable, Some(&header)).expect("a writable entry");
        // the header states 0x41100 relative to the image base; the report has to
        // name the address the entry is actually mapped at, or it points nowhere
        assert!(
            signal.detail.contains("0x441100"),
            "entry should be rebased through its section: {}",
            signal.detail
        );
        // a read-execute second section is an ordinary layout, not a stub
        let read_only = vec![section("UPX1", 0x44_1000, "r-x", flat(0x100))];
        assert!(entry_point_signal(&read_only, Some(&header)).is_none());
        // an entry in the first section is the normal arrangement
        let first = FormatHeader {
            entry_point: Some(0x1100),
            sections: header.sections.clone(),
            ..Default::default()
        };
        assert!(entry_point_signal(&writable, Some(&first)).is_none());
    }

    /// Section naming fires only when the table is *mostly* unconventional, so a
    /// single custom section in an otherwise normal image stays quiet.
    #[test]
    fn section_naming_needs_a_mostly_rewritten_table() {
        // stems cover the many suffixed variants each format produces
        let elf = conventional_names(Some("Executable and Linking Format (ELF)")).expect("known");
        assert!(is_conventional(".note.gnu.build-id", elf));
        assert!(is_conventional(".rela.plt", elf));
        assert!(!is_conventional("UPX1", elf));
        // an unknown format has no convention to judge against
        assert!(conventional_names(Some("Raw Binary")).is_none());
    }

    /// The import-table signal needs both halves of the stub's toolkit.
    #[test]
    fn thin_imports_need_resolvers_and_memory_permissions() {
        let names: Vec<String> = ["loadlibrarya", "getprocaddress", "virtualprotect"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(matching(&names, RESOLVER_IMPORTS).len(), 2);
        assert_eq!(matching(&names, MEMORY_IMPORTS), vec!["virtualprotect"]);
        // resolvers with no way to make memory executable is an ordinary plugin host
        let host: Vec<String> = ["loadlibrarya", "getprocaddress", "createfilew"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(matching(&host, MEMORY_IMPORTS).is_empty());
    }

    /// A family is named only on a marker match, never on shape.
    #[test]
    fn markers_name_a_family_only_on_a_real_match() {
        let header = FormatHeader {
            sections: vec![
                declared("UPX0", 0x1000, 0x40000, 0),
                declared("UPX1", 0x41000, 0x8000, 0x8000),
            ],
            ..Default::default()
        };
        assert_eq!(match_marker(&[], Some(&header), b""), Some("UPX"));
        // the stub magic alone is enough
        assert_eq!(match_marker(&[], None, b"....UPX!...."), Some("UPX"));
        // a packed-looking image with ordinary names gets no name at all
        let anonymous = FormatHeader {
            sections: vec![
                declared(".text", 0x1000, 0x40000, 0),
                declared(".data", 0x41000, 0x8000, 0x8000),
            ],
            ..Default::default()
        };
        assert_eq!(match_marker(&[], Some(&anonymous), b"\x00\x01\x02"), None);
        // a partial section set does not match either
        let partial = FormatHeader {
            sections: vec![declared("UPX0", 0x1000, 0x40000, 0)],
            ..Default::default()
        };
        assert_eq!(match_marker(&[], Some(&partial), b""), None);
    }

    /// The synthetic ELF section-header entry is not treated as a section name.
    #[test]
    fn section_names_drops_the_synthetic_entry() {
        let header = FormatHeader {
            sections: vec![
                declared(".text", 0x1000, 0x100, 0x100),
                declared("<section headers>", 0, 0, 0x280),
            ],
            ..Default::default()
        };
        let names = section_names(
            &[section(".text", 0x40_1000, "r-x", flat(64))],
            Some(&header),
        );
        assert_eq!(names, vec![".text".to_string()]);
    }

    /// A declared Authenticode certificate table is not a compressed overlay
    /// (7000 bytes at 0xF5200, declared by security directory 4).
    #[test]
    fn cert_table_overlay_is_not_a_compressed_body() {
        // the image: ordinary bytes up to 0xF5200, then the appended cert table
        let mut raw = low_entropy(0xF5200);
        raw.extend(flat(0x1B58));
        let mut header = FormatHeader {
            sections: vec![declared(".text", 0x1000, 0xF4E00, 0xF4E00)],
            ..Default::default()
        };
        // without the declared cert range the appended bytes read as a signal
        assert!(file_entropy_signal(&raw, &[], Some(&header)).is_some());
        // the security directory declares exactly that range as the cert table
        header.security_dir = Some((0xF5200, 0x1B58));
        assert!(file_entropy_signal(&raw, &[], Some(&header)).is_none());
    }

    /// Appended cargo with no declared certificate table keeps firing
    /// (5654 bytes at 0x8400, confirmed as real appended data).
    #[test]
    fn appended_cargo_without_a_declared_cert_still_fires() {
        let mut raw = low_entropy(0x8400);
        raw.extend(flat(5654));
        let header = FormatHeader {
            sections: vec![declared(".text", 0x1000, 0x8000, 0x8000)],
            ..Default::default()
        };
        let signal = file_entropy_signal(&raw, &[], Some(&header)).expect("cargo fires");
        assert!(
            signal.detail.contains("5654 bytes are appended"),
            "{}",
            signal.detail
        );
        assert!(!signal.detail.contains("certificate"), "{}", signal.detail);
    }

    /// Cargo grafted past a certificate table still fires, with the cert range
    /// excluded from the measurement (the shape of a signed dropper).
    #[test]
    fn cargo_grafted_past_a_cert_table_still_fires() {
        // 0x1000 of declared cert bytes, then 0x2000 of compressed cargo
        let mut raw = low_entropy(0x8400);
        raw.extend(flat(0x3000));
        let header = FormatHeader {
            sections: vec![declared(".text", 0x1000, 0x8000, 0x8000)],
            security_dir: Some((0x8400, 0x1000)),
            ..Default::default()
        };
        let signal = file_entropy_signal(&raw, &[], Some(&header)).expect("cargo fires");
        // the measurement covers only the cargo and says the cert was excluded
        assert!(
            signal.detail.contains("8192 bytes are appended"),
            "{}",
            signal.detail
        );
        assert!(
            signal.detail.contains("Authenticode certificate table"),
            "{}",
            signal.detail
        );
        assert!(signal.detail.contains("0x9400"), "{}", signal.detail);
    }

    /// A whole-file figure explained by one dominant data section is not an
    /// independent signal (`.rsrc` at 94.5% of the file, entropy 8.00).
    #[test]
    fn dominant_data_section_is_not_counted_again_as_whole_file_entropy() {
        // the file: a small code part and a dominant high-entropy resource
        let mut raw = low_entropy(0x2000);
        raw.extend(flat(0x8000));
        // the loaded view: ordinary code next to a resource that is 80% of the file
        let data_heavy = vec![
            section(".text", 0x40_1000, "r-x", low_entropy(0x2000)),
            section(".rsrc", 0x40_3000, "rw-", flat(0x8000)),
        ];
        assert!(file_entropy_signal(&raw, &data_heavy, None).is_none());
        // the same bytes behind an executable section are the packed-code shape,
        // which keeps the whole-file reading
        let packed_text = vec![section(".text", 0x40_1000, "r-x", flat(0x8000))];
        assert!(file_entropy_signal(&raw, &packed_text, None).is_some());
    }

    /// The unpack-target's source section is not scored again as the
    /// high-entropy section (`.bss` paired with an 11 KB `.rodata`).
    #[test]
    fn unpack_source_is_not_scored_again_as_high_entropy() {
        let header = FormatHeader {
            sections: vec![
                declared(".bss", 0x1000, 0x40000, 0),
                declared(".rodata", 0x41000, 11332, 11332),
            ],
            ..Default::default()
        };
        let sections = vec![section(".rodata", 0x44_1000, "r--", flat(11332))];
        // the pair fires and names `.rodata` as the source
        let (signal, source) = unpack_target_signal(&sections, Some(&header)).expect("the pair");
        assert!(signal.detail.contains("`.rodata`"), "{}", signal.detail);
        // the high-entropy check skips that same section...
        assert!(high_entropy_section_signal(&sections, 20_000, Some(source)).is_none());
        // ...and reads it normally when nothing else counted it
        assert!(high_entropy_section_signal(&sections, 20_000, None).is_some());
    }

    /// Small compressed assets do not clear the section-entropy floor; packed
    /// bodies and substantial carriers do (a 36 KB `.rsrc` vs the
    /// confirmed 307200-byte `.data` and a 48 KB resource at a fifth of its file).
    #[test]
    fn small_data_assets_do_not_clear_the_section_entropy_floor() {
        // a 36 KB resource in a 400 KB file is an asset, not a packed body
        let small = vec![section(".rsrc", 0x40_1000, "rw-", flat(36864))];
        assert!(high_entropy_section_signal(&small, 400_000, None).is_none());
        // a 307200-byte data section is big enough in any file
        let big = vec![section(".data", 0x40_1000, "rw-", flat(307_200))];
        assert!(high_entropy_section_signal(&big, 4_000_000, None).is_some());
        // a 48 KB resource that is a fifth of its file counts by proportion
        let fifth = vec![section(".rsrc", 0x40_1000, "rw-", flat(49_152))];
        assert!(high_entropy_section_signal(&fifth, 200_000, None).is_some());
        // and executable sections count at any size above the measurement floor
        let packed = vec![section(".text", 0x40_1000, "r-x", flat(0x2000))];
        assert!(high_entropy_section_signal(&packed, 400_000, None).is_some());
    }

    /// Empty or garbage header names never satisfy a marker (one case: a
    /// NUL-led name list naming UPX on a `ConfuserEx` sample; another: five
    /// empty names naming UPX on Enigma Protector).
    #[test]
    fn empty_header_names_never_name_a_marker() {
        // the ConfuserEx name list: garbage names plus the empty string
        let confuser = FormatHeader {
            sections: vec![
                declared("MDSK{F", 0x1000, 0x1000, 0x1000),
                declared("", 0x2000, 0x1000, 0x1000),
                declared("MDSK{_F", 0x3000, 0x1000, 0x1000),
                declared("SECTION.4", 0x4000, 0x1000, 0x1000),
            ],
            ..Default::default()
        };
        assert_eq!(match_marker(&[], Some(&confuser), b"\x00\x01"), None);
        // the Enigma name list: nothing but empty names
        let enigma = FormatHeader {
            sections: (0..5u64)
                .map(|i| declared("", 0x1000 * (i + 1), 0x1000, 0x1000))
                .collect(),
            ..Default::default()
        };
        assert_eq!(match_marker(&[], Some(&enigma), b"\x00\x01"), None);
        // a truncated three-character name still serves the reversed leg...
        let truncated = FormatHeader {
            sections: vec![declared("upx", 0x1000, 0x1000, 0x1000)],
            ..Default::default()
        };
        assert_eq!(match_marker(&[], Some(&truncated), b""), Some("UPX"));
        // ...but anything shorter is discarded rather than matched
        let shorter = FormatHeader {
            sections: vec![declared("up", 0x1000, 0x1000, 0x1000)],
            ..Default::default()
        };
        assert_eq!(match_marker(&[], Some(&shorter), b""), None);
        // the confirmed VMProtect family still matches forward on `.vmp0`
        let vmp = FormatHeader {
            sections: vec![
                declared(".text", 0x1000, 0x1000, 0x1000),
                declared(".vmp0", 0x2000, 0x1000, 0x1000),
            ],
            ..Default::default()
        };
        assert_eq!(match_marker(&[], Some(&vmp), b""), Some("VMProtect"));
    }

    /// Garbage header names are dropped from the reasoning name set, while real
    /// unconventional names stay.
    #[test]
    fn section_names_drops_garbage_header_names() {
        let header = FormatHeader {
            sections: vec![
                declared("", 0x1000, 0x1000, 0x1000),
                declared("\u{1}\u{2}bad", 0x2000, 0x1000, 0x1000),
                declared("MDSK{F", 0x3000, 0x1000, 0x1000),
            ],
            ..Default::default()
        };
        assert_eq!(
            section_names(&[], Some(&header)),
            vec!["MDSK{F".to_string()]
        );
    }

    /// Ghidra's `segment_N` placeholder names for header-less images are not
    /// naming evidence ('1 of 1 sections ... (`segment_0`)').
    #[test]
    fn loader_segment_names_are_not_naming_evidence() {
        let format = Some("Executable and Linking Format (ELF)");
        // a stripped image: no parsed header, loader-invented block names
        let stripped = vec![
            section("segment_0", 0x1000, "r-x", flat(0x1000)),
            section("segment_1", 0x3000, "rw-", flat(0x1000)),
        ];
        assert!(section_naming_signal(format, &stripped, None).is_none());
        // genuinely rewritten names on a header-less image still read as evidence
        let rewritten = vec![
            section("qxz0", 0x1000, "r-x", flat(0x1000)),
            section("qxz1", 0x3000, "rw-", flat(0x1000)),
        ];
        assert!(section_naming_signal(format, &rewritten, None).is_some());
        // with a declared section table, `segment_N` names count like any other
        let header = FormatHeader {
            sections: vec![
                declared("segment_0", 0x1000, 0x1000, 0x1000),
                declared("segment_1", 0x2000, 0x1000, 0x1000),
            ],
            ..Default::default()
        };
        assert!(section_naming_signal(format, &stripped, Some(&header)).is_some());
    }

    /// Go linker section names are conventional for ELF images (an analysis once
    /// read `.typelink` and friends as a rewritten section table).
    #[test]
    fn go_linker_sections_read_as_conventional() {
        let elf = conventional_names(Some("Executable and Linking Format (ELF)")).expect("known");
        // the Go runtime's section set is what the Go linker emits
        for name in [
            ".typelink",
            ".itablink",
            ".gosymtab",
            ".gopclntab",
            ".noptrdata",
            ".noptrbss",
            ".zdebug_info",
            ".go.buildinfo",
        ] {
            assert!(is_conventional(name, elf), "{name}");
        }
        // packer names stay unconventional
        assert!(!is_conventional("upx1", elf));
        // a Go-shaped table no longer reads as mostly rewritten
        let go_header = FormatHeader {
            sections: vec![
                declared(".text", 0x1000, 0x1000, 0x1000),
                declared(".rodata", 0x2000, 0x1000, 0x1000),
                declared(".typelink", 0x3000, 0x1000, 0x1000),
                declared(".gopclntab", 0x4000, 0x1000, 0x1000),
                declared(".noptrdata", 0x5000, 0x1000, 0x1000),
                declared(".go.buildinfo", 0x6000, 0x1000, 0x1000),
            ],
            ..Default::default()
        };
        let format = Some("Executable and Linking Format (ELF)");
        assert!(section_naming_signal(format, &[], Some(&go_header)).is_none());
    }

    /// The carrier shape needs entropy-only signals, a data-section dominator,
    /// a full import table, and plain-reading code (a dropper shape).
    #[test]
    fn carrier_shape_requires_plain_code_beside_the_cargo() {
        // ordinary code next to a dominant compressed resource
        let dropper = vec![
            section(".text", 0x40_1000, "r-x", low_entropy(0x4000)),
            section(".rsrc", 0x40_6000, "rw-", flat(0x20000)),
        ];
        let entropy_only = vec![Signal::new(HIGH_ENTROPY_LABEL, "the resource")];
        assert!(carrier_shape(60, &dropper, &entropy_only));
        // a thin import table is a stub's, not a program's
        assert!(!carrier_shape(6, &dropper, &entropy_only));
        // a stub-shaped signal keeps the packed-code claim
        let with_stub = vec![
            Signal::new(HIGH_ENTROPY_LABEL, "the resource"),
            Signal::new(UNPACK_TARGET_LABEL, "the pair"),
        ];
        assert!(!carrier_shape(60, &dropper, &with_stub));
        // a hot executable section is packed code, not cargo
        let packed = vec![
            section(".text", 0x40_1000, "r-x", flat(0x20000)),
            section(".data", 0x42_2000, "rw-", low_entropy(0x4000)),
        ];
        assert!(!carrier_shape(60, &packed, &entropy_only));
    }

    /// A carrier is substantial at 64 KB or a quarter of the file.
    #[test]
    fn substantial_carrier_needs_real_cargo() {
        // 300 KB of cargo is substantial in any file
        let big = vec![section(".data", 0x40_1000, "rw-", flat(307_200))];
        assert!(substantial_carrier(&big, 4_000_000));
        // a 32 KB blob in a large file is not
        let small = vec![section(".rsrc", 0x40_1000, "rw-", flat(0x8000))];
        assert!(!substantial_carrier(&small, 1_000_000));
        // ...but the same blob at a third of its file is
        assert!(substantial_carrier(&small, 0x8000 * 3));
    }

    /// Carrier severity: a substantial carrier keeps the unpack question alive
    /// at medium, a small one is context-only, and markers stay high.
    #[test]
    fn carrier_severity_tracks_the_cargo_size() {
        // a substantial carrier grades medium even on one signal
        assert!(matches!(
            severity_for(None, true, true, 1),
            (Confidence::Unsure, 40)
        ));
        // a small carrier is context-only
        assert!(matches!(
            severity_for(None, true, false, 2),
            (Confidence::Untrusted, 15)
        ));
        // a marker-named family stays high regardless of shape
        assert!(matches!(
            severity_for(Some("UPX"), false, false, 3),
            (Confidence::Likely, 75)
        ));
        // bare structure keeps the count-based grades
        assert!(matches!(
            severity_for(None, false, false, 1),
            (Confidence::Untrusted, 15)
        ));
        assert!(matches!(
            severity_for(None, false, false, 2),
            (Confidence::Unsure, 40)
        ));
    }

    /// The carrier claim speaks of cargo to carve, not of packed code; the
    /// structural and marker claims keep their packed-code wording.
    #[test]
    fn carrier_claim_speaks_of_cargo_not_packed_code() {
        let signals = vec![Signal::new(
            HIGH_ENTROPY_LABEL,
            "section `.rsrc` holds bytes",
        )];
        let (content, reasoning) = compose_claim(None, true, &signals);
        assert!(content.contains("payload carrier"), "{content}");
        assert!(
            reasoning.contains("carve out and identify the payload"),
            "{reasoning}"
        );
        assert!(
            !reasoning.contains("stored compressed or encrypted"),
            "{reasoning}"
        );
        // the packed-code claim is unchanged for non-carrier shapes
        let (content, reasoning) = compose_claim(None, false, &signals);
        assert!(content.contains("Packed or obfuscated image"), "{content}");
        assert!(
            reasoning.contains("stored compressed or encrypted"),
            "{reasoning}"
        );
        // a named family keeps its identification claim
        let (content, _) = compose_claim(Some("UPX"), false, &signals);
        assert!(content.contains("Packed with UPX"), "{content}");
    }

    /// Entry attribution is gated on stub-shaped evidence: entropy-only readings
    /// over a data section must not blame the CRT entry.
    #[test]
    fn stub_evidence_gates_the_entry_attribution() {
        // the disputed shape: a hot resource and nothing stub-shaped
        let dropper = vec![
            section(".text", 0x40_1000, "r-x", low_entropy(0x4000)),
            section(".rsrc", 0x40_6000, "rw-", flat(0x20000)),
        ];
        let entropy_only = vec![Signal::new(FILE_ENTROPY_LABEL, "the overlay")];
        assert!(!stub_shaped(&dropper, &entropy_only, None));
        // a marker is stub evidence on its own
        assert!(stub_shaped(&dropper, &entropy_only, Some("UPX")));
        // so is any stub-shaped signal
        let with_stub = vec![Signal::new(THIN_IMPORTS_LABEL, "the import table")];
        assert!(stub_shaped(&dropper, &with_stub, None));
        // and so is packed code itself: a hot executable dominator
        let packed = vec![section(".text", 0x40_1000, "r-x", flat(0x20000))];
        assert!(stub_shaped(&packed, &entropy_only, None));
    }
}
