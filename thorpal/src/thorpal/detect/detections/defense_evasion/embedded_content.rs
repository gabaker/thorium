//! Embedded foreign-format content inside the sample's raw bytes.
//!
//! A blinded evaluation run concluded a trojanized host was benign while a
//! PNG grafted into its `.rsrc` carried the implant's payload — a signal the
//! toolbox's binwalk had surfaced in one line. This detection is the native
//! equivalent: a magic scan over the raw file (binwalk-style) plus a PNG
//! chunk/CRC validation (pngcheck-style), so an embedded archive, executable,
//! or structurally anomalous image is a seeded finding instead of a missed
//! lead. File-level findings are exempt from the runtime-noise gate by
//! construction.

use anyhow::Result;

use crate::thorpal::detect::context::AnalysisContext;
use crate::thorpal::detect::detections::{
    AttackTechnique, Detection, DetectionHit, EvidenceKind, HitSource, low, medium,
};
use crate::thorpal::detect::fileinfo::{FormatHeader, parse_header};

use super::T1027_009;

/// The most embedded-content hits emitted per run.
const MAX_EMBEDDED_HITS: usize = 16;

/// The embedded-content detection.
pub struct EmbeddedContent;

/// One embedded artifact found in the raw file.
struct Embedded {
    /// The file offset the magic was found at.
    offset: u64,
    /// What the magic identifies ("PNG image", "ZIP archive", ...).
    kind: &'static str,
    /// The parsed size in bytes, when the format's framing declares one.
    size: Option<u64>,
    /// Structural anomalies ("IDAT CRC mismatch", "trailing data after IEND").
    anomalies: Vec<String>,
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
        // render one file-level hit per embedded artifact, bounded
        let mut hits = Vec::new();
        for embedded in found.into_iter().take(MAX_EMBEDDED_HITS) {
            hits.push(build_hit(header.as_ref(), embedded));
        }
        Ok(hits)
    }
}

/// Render one embedded artifact as a file-level detection hit.
///
/// Severity policy: an embedded executable or archive is suspicious anywhere;
/// an image is suspicious only when structurally anomalous or sitting outside
/// the resource section (resources legitimately contain images).
///
/// # Arguments
///
/// * `header` - The parsed executable header, when one parsed
/// * `embedded` - The embedded artifact to render
fn build_hit(header: Option<&FormatHeader>, embedded: Embedded) -> DetectionHit {
    // place the artifact against the declared section table
    let section = containing_section(header, embedded.offset);
    let in_rsrc = section.as_deref().is_some_and(|name| name == ".rsrc");
    let is_image = embedded.kind == "PNG image";
    // images are normal in resources; everything else embedded is a lead
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
    let size = embedded
        .size
        .map_or_else(String::new, |size| format!(" ({size} bytes)"));
    let anomalies = if embedded.anomalies.is_empty() {
        String::new()
    } else {
        format!("; {}", embedded.anomalies.join("; "))
    };
    let content = format!(
        "{} embedded {location} @ file offset {:#x}{size}{anomalies}",
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
    DetectionHit {
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
    }
}

/// Scan raw bytes for embedded file magics past offset 0.
///
/// Matches PNG, ZIP, CAB, gzip, ELF, and MZ+PE magics; offset 0 (the host
/// itself) is excluded, and matches inside an already-measured span dedupe.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
fn scan_embedded(raw: &[u8]) -> Vec<Embedded> {
    let mut out: Vec<Embedded> = Vec::new();
    // walk the file once; magic sets are tiny so a linear scan is fine
    let mut at = 1usize;
    while at + 2 <= raw.len() {
        // skip offsets inside the previously measured artifact — but never
        // honor a declared size reaching past the file, so a hostile header
        // with a huge size field cannot blind the scan to what follows it
        if let Some(last) = out.last() {
            if let Some(size) = last.size {
                let end = last.offset.saturating_add(size);
                if end <= raw.len() as u64 && (at as u64) < end {
                    at = end as usize;
                    continue;
                }
            }
        }
        let window = &raw[at..];
        let found = if window.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(check_png(raw, at))
        } else if window.starts_with(b"PK\x03\x04") {
            Some(Embedded {
                offset: at as u64,
                kind: "ZIP archive",
                size: None,
                anomalies: Vec::new(),
            })
        } else if window.starts_with(b"MSCF") {
            // the cab header declares its total size at offset 8; a size that
            // overruns the file is attacker noise and is reported unsized
            let size = read_le_u32(raw, at + 8)
                .map(u64::from)
                .filter(|size| at as u64 + size <= raw.len() as u64);
            Some(Embedded {
                offset: at as u64,
                kind: "CAB archive",
                size,
                anomalies: Vec::new(),
            })
        } else if window.starts_with(b"\x1f\x8b\x08")
            && window.get(3).is_some_and(|flg| *flg <= 0x1f)
        {
            // the FLG sanity check thins the 3-byte magic's false positives
            Some(Embedded {
                offset: at as u64,
                kind: "gzip stream",
                size: None,
                anomalies: Vec::new(),
            })
        } else if window.starts_with(b"\x7fELF") {
            Some(Embedded {
                offset: at as u64,
                kind: "ELF executable",
                size: None,
                anomalies: Vec::new(),
            })
        } else if window.starts_with(b"MZ") && embedded_pe_at(raw, at) {
            Some(Embedded {
                offset: at as u64,
                kind: "PE executable",
                size: None,
                anomalies: Vec::new(),
            })
        } else {
            None
        };
        match found {
            Some(embedded) => {
                at = embedded.offset as usize + 1;
                out.push(embedded);
                // the cap bounds hostile files stuffed with magic bytes
                if out.len() >= MAX_EMBEDDED_HITS {
                    break;
                }
            }
            None => at += 1,
        }
    }
    out
}

/// Whether an `MZ` at `at` points at a valid embedded PE signature.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `at` - The offset the `MZ` sits at
fn embedded_pe_at(raw: &[u8], at: usize) -> bool {
    // follow e_lfanew and require the PE\0\0 signature to really be there
    let Some(e_lfanew) = read_le_u32(raw, at + 0x3c) else {
        return false;
    };
    let Some(pe) = at.checked_add(e_lfanew as usize) else {
        return false;
    };
    matches!(raw.get(pe..pe + 4), Some(sig) if sig == b"PE\0\0")
}

/// Validate a PNG at `offset`: walk its chunks and flag anomalies.
///
/// The pngcheck-equivalent: CRC-checks every chunk, measures the stream to
/// IEND, and flags truncation and malformed chunk structure. Data placed
/// *inside* chunks (the IDAT-steganography shape) surfaces through the CRC
/// and size checks; bytes after IEND belong to the surrounding file and are
/// not judged here.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `offset` - The offset the PNG signature sits at
fn check_png(raw: &[u8], offset: usize) -> Embedded {
    /// The most anomaly strings recorded; further anomalies are counted into
    /// one bounded summary line, never accumulated as strings.
    const MAX_PNG_ANOMALIES: usize = 8;
    /// The most chunks walked before declaring the stream unbounded garbage.
    const MAX_PNG_CHUNKS: usize = 4096;
    let mut anomalies: Vec<String> = Vec::new();
    // anomalies past the record cap are counted, not stored, so an anomalous
    // but real stream (the IDAT-steganography shape) still walks to IEND and
    // keeps its measured size
    let mut overflowed = 0usize;
    // chunks start right after the 8-byte signature
    let mut at = offset + 8;
    let mut size = None;
    let mut walked = 0usize;
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
    // fold the uncounted anomalies into one bounded line
    if overflowed > 0 {
        anomalies.push(format!("…and {overflowed} more chunk anomalies"));
    }
    Embedded {
        offset: offset as u64,
        kind: "PNG image",
        size,
        anomalies,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal valid PNG (IHDR + IEND) with correct CRCs.
    fn tiny_png() -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        // a 13-byte IHDR for a 1x1 grayscale image
        let ihdr_data = [0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0];
        for (kind, data) in [(&b"IHDR"[..], &ihdr_data[..]), (&b"IEND"[..], &[][..])] {
            png.extend_from_slice(&(data.len() as u32).to_be_bytes());
            png.extend_from_slice(kind);
            png.extend_from_slice(data);
            let mut crc = Vec::from(kind);
            crc.extend_from_slice(data);
            png.extend_from_slice(&crc32fast::hash(&crc).to_be_bytes());
        }
        png
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
        assert!(found[0].anomalies.is_empty());
    }

    /// A corrupted chunk CRC is flagged as an anomaly.
    #[test]
    fn flags_a_png_crc_mismatch() {
        let mut raw = vec![0u8; 16];
        let mut png = tiny_png();
        // corrupt one IHDR data byte without fixing its CRC
        png[16] ^= 0xFF;
        raw.extend_from_slice(&png);
        let found = scan_embedded(&raw);
        assert_eq!(found.len(), 1);
        assert!(found[0].anomalies.iter().any(|a| a.contains("IHDR")));
    }

    /// An embedded MZ without a PE signature is not reported; one with is.
    #[test]
    fn embedded_pe_requires_the_signature() {
        // a bare MZ with garbage e_lfanew is ignored
        let mut raw = vec![0u8; 8];
        raw.extend_from_slice(b"MZ");
        raw.extend_from_slice(&[0u8; 128]);
        assert!(scan_embedded(&raw).is_empty());
        // a well-formed embedded PE stub is found
        let mut pe = vec![0u8; 8];
        pe.extend_from_slice(b"MZ");
        pe.extend_from_slice(&[0u8; 0x3a]);
        // e_lfanew points just past the stub at the PE signature
        let e_lfanew = 0x40u32;
        pe.extend_from_slice(&e_lfanew.to_le_bytes());
        pe.resize(8 + 0x40, 0);
        pe.extend_from_slice(b"PE\0\0");
        let found = scan_embedded(&pe);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "PE executable");
    }

    /// Many CRC mismatches cap the recorded notes but never abandon the walk:
    /// the stream is still measured to IEND and the overflow is summarized.
    #[test]
    fn png_anomaly_cap_does_not_abandon_the_walk() {
        let mut raw = vec![0u8; 4];
        let start = raw.len();
        raw.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        // ten tEXt chunks whose CRCs are all wrong
        for _ in 0..10 {
            raw.extend_from_slice(&1u32.to_be_bytes());
            raw.extend_from_slice(b"tEXt");
            raw.push(0x41);
            raw.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        }
        // a valid IEND chunk terminates the stream
        raw.extend_from_slice(&0u32.to_be_bytes());
        raw.extend_from_slice(b"IEND");
        raw.extend_from_slice(&crc32fast::hash(b"IEND").to_be_bytes());
        let total = raw.len() - start;
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
}
