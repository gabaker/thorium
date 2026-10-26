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
//! 3. a substantial section whose entropy is near the theoretical maximum,
//! 4. a thin import table consisting mostly of the dynamic-resolution and
//!    memory-permission calls an unpacking stub needs,
//! 5. an entry point in a writable section other than the first,
//! 6. a section-name set at odds with the conventions of the detected format, and
//! 7. high whole-file entropy, or a large high-entropy region appended past the
//!    last section.
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

use std::collections::HashSet;

use anyhow::Result;

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

/// The smallest region worth taking an entropy reading of, in bytes.
///
/// Shannon entropy over a short buffer is dominated by sampling noise — a
/// 64-byte buffer of distinct values scores 6.0 without being compressed at all.
const MIN_ENTROPY_LEN: u64 = 4096;

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
        Ok(vec![build_hit(ctx, &sections, &signals, family)])
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
    // each check is independent and contributes at most one signal
    [
        unpack_target_signal(sections, header),
        writable_executable_signal(sections),
        high_entropy_section_signal(sections),
        thin_import_signal(ctx),
        entry_point_signal(sections, header),
        section_naming_signal(ctx, sections, header),
        file_entropy_signal(raw, header),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Signal 1 — a section reserved to be written into, paired with the compressed
/// bytes that would fill it.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `header` - The parsed format header, when one could be read
fn unpack_target_signal(sections: &[Section], header: Option<&FormatHeader>) -> Option<Signal> {
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
    Some(Signal::new(
        "reserved unpack target",
        format!(
            "section `{}` is mapped {} bytes but stores only {} on disk, while `{}` holds {} bytes \
             at entropy {:.2} — the shape of a region reserved for code to be written into, and a \
             compressed body small enough to be its source",
            target.name,
            target.virtual_size,
            target.raw_size,
            source.name.trim(),
            source.size,
            source.entropy().unwrap_or_default(),
        ),
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
        "writable executable section",
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
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
fn high_entropy_section_signal(sections: &[Section]) -> Option<Signal> {
    // the highest-entropy section large enough for the reading to mean something
    let section = sections
        .iter()
        .filter(|section| !is_synthetic_block(&section.name))
        .filter(|section| section.size >= MIN_ENTROPY_LEN)
        .filter(|section| section.entropy().is_some_and(|value| value >= HIGH_ENTROPY))
        .max_by(|a, b| {
            a.entropy()
                .unwrap_or_default()
                .total_cmp(&b.entropy().unwrap_or_default())
        })?;
    Some(Signal::new(
        "high-entropy section",
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
        "loader-shaped import table",
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
        "entry point in a writable section",
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
/// * `ctx` - The analysis context (for the detected format)
/// * `sections` - Ghidra's loaded memory map
/// * `header` - The parsed format header, when one could be read
fn section_naming_signal(
    ctx: &AnalysisContext,
    sections: &[Section],
    header: Option<&FormatHeader>,
) -> Option<Signal> {
    // the header's names are authoritative; fall back to the loaded block names
    let names = section_names(sections, header);
    if names.is_empty() {
        return None;
    }
    // without a known format there is no convention to compare against
    let conventional = conventional_names(ctx.program.executable_format.as_deref())?;
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
        "unconventional section names",
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
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `header` - The parsed format header, when one could be read
fn file_entropy_signal(raw: &[u8], header: Option<&FormatHeader>) -> Option<Signal> {
    // too small for the reading to be meaningful
    if (raw.len() as u64) < MIN_ENTROPY_LEN {
        return None;
    }
    let mut parts = Vec::new();
    // the whole file, which the header and the padding would ordinarily dilute
    let whole = crate::thorpal::detect::decode::shannon_entropy(raw);
    if whole >= HIGH_FILE_ENTROPY {
        parts.push(format!(
            "the file as a whole reads at entropy {whole:.2}, leaving no room for the low-entropy \
             header, padding, and string data every image carries"
        ));
    }
    // anything appended past the last section the header declares
    if let Some(end) = header.and_then(FormatHeader::image_end) {
        let overlay = raw.get(end as usize..).unwrap_or_default();
        let entropy = crate::thorpal::detect::decode::shannon_entropy(overlay);
        if overlay.len() as u64 >= MIN_ENTROPY_LEN && entropy >= HIGH_ENTROPY {
            parts.push(format!(
                "{} bytes are appended past the last declared section at {} with entropy \
                 {entropy:.2}",
                overlay.len(),
                fmt_addr(end),
            ));
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(Signal::new("compressed file body", parts.join("; ")))
}

/// Assemble the finding from the signals that fired.
///
/// # Arguments
///
/// * `ctx` - The analysis context (for entry-point attribution)
/// * `sections` - Ghidra's loaded memory map
/// * `signals` - The structural observations that fired
/// * `family` - The packing tool named by a marker match, if any
fn build_hit(
    ctx: &AnalysisContext,
    sections: &[Section],
    signals: &[Signal],
    family: Option<&'static str>,
) -> DetectionHit {
    // a named family is the only evidence strong enough for `high`; otherwise the
    // score follows how many independent structural signals agree
    let severity = match (family, signals.len()) {
        (Some(_), _) => high(),
        (None, 1) => low(),
        (None, _) => medium(),
    };
    // the unpacking stub runs from the entry point, so that is both the most
    // useful site for the finding and its natural function attribution
    let (address, function, role) = attribute(ctx, sections);
    // the headline: what was concluded, and on what basis
    let content = match family {
        Some(name) => format!(
            "Packed with {name} — {} structural signal(s) plus a {name} marker",
            signals.len()
        ),
        None => format!(
            "Packed or obfuscated image — {} structural signal(s)",
            signals.len()
        ),
    };
    // the reasoning: every measurement, then the caveat the score depends on
    let mut reasoning = String::new();
    match family {
        Some(name) => reasoning.push_str(&format!(
            "The image carries {name}'s own section-table and stub markers, so the packing tool is \
             identified rather than inferred. Supporting structure: "
        )),
        None => reasoning.push_str(
            "The image's structure indicates its executable code is stored compressed or \
             encrypted and reconstituted at runtime. No packing tool could be identified — the \
             conclusion rests on structure alone, so no product name is asserted. Observed: ",
        ),
    }
    reasoning.push_str(
        &signals
            .iter()
            .map(|signal| format!("{} — {}", signal.label, signal.detail))
            .collect::<Vec<_>>()
            .join("; "),
    );
    reasoning.push_str(
        ". Note that compressed payloads are also entirely normal in installers, self-extracting \
         archives, commercially protected software, and binaries with large embedded assets; this \
         finding says the code on disk is not the code that runs, not that the program is \
         malicious.",
    );
    // the practical consequence, which is what the analyst actually acts on
    reasoning.push_str(
        " Treat every other finding in this report as describing the visible code only: the \
         strings, imports, and control flow of the packed body are not present in this image and \
         must be recovered by unpacking or dynamic analysis.",
    );
    // the artifact: the value is the conclusion, the detail is the measurement
    let artifact = HitArtifact::new(
        ArtifactClass::Packer,
        family.unwrap_or("unidentified packer"),
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

/// Locate the unpacking stub: the entry-point function, when Ghidra found one.
///
/// Returns the site address, the containing function, and the role phrase
/// describing why that function is the relevant one. Falls back to the
/// highest-entropy section's base when there is no recognizable entry function —
/// the compressed body is then the only concrete thing to point at.
///
/// # Arguments
///
/// * `ctx` - The analysis context (function list + containment lookup)
/// * `sections` - Ghidra's loaded memory map
fn attribute(
    ctx: &AnalysisContext,
    sections: &[Section],
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
    let entry = ctx
        .functions
        .iter()
        .filter(|function| ENTRY_NAMES.contains(&function.name.as_str()))
        .find_map(|function| {
            parse_addr(&function.entry_point).or_else(|| parse_addr(&function.address))
        });
    if let Some(entry) = entry {
        // the entry function is the stub: it is what runs before the real code exists
        return (
            entry,
            Some(entry),
            Some("entry point — the code that reconstitutes the packed body before it runs"),
        );
    }
    // no entry function: point at the compressed body itself, and let the caller
    // record that no function could be attributed
    let body = sections
        .iter()
        .filter(|section| section.size >= MIN_ENTROPY_LEN)
        .max_by(|a, b| {
            a.entropy()
                .unwrap_or_default()
                .total_cmp(&b.entropy().unwrap_or_default())
        });
    match body {
        Some(section) => (section.base, ctx.containing_function(section.base), None),
        // nothing mapped at all; anchor at the image base so the hit still has a site
        None => (0, None, None),
    }
}

/// Render the section table as the artifact's supporting detail.
///
/// # Arguments
///
/// * `sections` - Ghidra's loaded memory map
/// * `unattributed` - Whether function attribution failed, which must be said
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
            ". No entry-point function was recognized, so the unpacking stub could not be \
             attributed to a function — the address above is the compressed body, not the code \
             that decompresses it.",
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
    // header, but Ghidra sometimes renames or merges them in the loaded image
    let names: Vec<String> = section_names(sections, header)
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    PACKER_MARKERS
        .iter()
        .find(|marker| {
            // every named section must be present, as a prefix match so numbered
            // variants (`upx0`/`upx1`, `.vmp0`/`.vmp1`) are covered by one entry
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
    // the header is the authoritative table; Ghidra may split or synthesize blocks
    let mut names: Vec<String> = header
        .map(|header| {
            header
                .sections
                .iter()
                .map(|section| section.name.clone())
                .collect()
        })
        .unwrap_or_default();
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
        assert!(high_entropy_section_signal(&short).is_none());
        // the same distribution over a real section is a genuine reading
        let long = vec![section(".text", 0x40_1000, "r-x", flat(0x8000))];
        let signal = high_entropy_section_signal(&long).expect("a large flat section reads high");
        assert!(signal.detail.contains("entropy 8.00"));
        // ordinary code does not trip it
        let code = vec![section(".text", 0x40_1000, "r-x", low_entropy(0x8000))];
        assert!(high_entropy_section_signal(&code).is_none());
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
        let signal = unpack_target_signal(&loaded, Some(&header)).expect("the unpack shape");
        assert!(signal.detail.contains("`UPX1`"), "{}", signal.detail);
        assert!(!signal.detail.contains("Headers"), "{}", signal.detail);
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
}
