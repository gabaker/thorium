//! Authenticode presence/consistency facts read off the raw PE bytes.
//!
//! A grafted signature — a certificate table copied from a signed binary onto
//! a modified host — is a strong implant signal that an analysis session easily
//! under-weights. This module verifies the one thing a graft cannot
//! fake: the Authenticode digest. The digest is computed per the spec (the
//! whole file minus the CheckSum field, the security data-directory entry,
//! and the certificate table itself) and compared against the EARLIEST
//! `DigestInfo`-shaped value inside the PKCS#7 blob — in a well-formed blob
//! that is `SpcIndirectDataContent`'s `messageDigest`, the only digest the
//! file digest is ever equal to; a signed file whose bytes were later
//! modified does not match it. Signer names are recovered with a bounded
//! byte scan (no ASN.1 dependency): heuristic, and labeled as such by the
//! caller.

use sha1::{Digest, Sha1};
use sha2::{Sha256, Sha384, Sha512};

use super::fileinfo::FormatHeader;

/// The most certificate common names collected from one signature blob.
const MAX_CERT_NAMES: usize = 8;

/// What the security directory held and whether its digest checks out.
#[derive(Debug, Clone)]
pub struct AuthenticodeInfo {
    /// The certificate table's file offset.
    pub table_offset: u64,
    /// The certificate table's size in bytes.
    pub table_size: u64,
    /// Whether the table runs exactly to the end of the file (the shape a
    /// signature grafted onto a modified host usually takes).
    pub table_is_tail: bool,
    /// Whether any `DigestInfo` inside the blob matches the file's computed
    /// Authenticode digest; `None` when no candidate digest was found at all.
    pub digest_matches: Option<bool>,
    /// Certificate common names scanned out of the blob (heuristic, deduped).
    pub cert_names: Vec<String>,
    /// The VersionInfo CompanyName recovered from the raw bytes, when present.
    pub company_name: Option<String>,
}

/// Read and judge the PE's Authenticode signature, when one is declared.
///
/// Returns `None` when the header declares no security directory or the
/// declared range does not fit the file.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `header` - The parsed PE header carrying the directory offsets
pub fn inspect(raw: &[u8], header: &FormatHeader) -> Option<AuthenticodeInfo> {
    // the security directory names the certificate table by FILE offset
    let (offset, size) = header.security_dir?;
    let start = usize::try_from(offset).ok()?;
    let end = start.checked_add(usize::try_from(size).ok()?)?;
    if end > raw.len() || size < 8 {
        return None;
    }
    // the WIN_CERTIFICATE header wraps the PKCS#7 blob (dwLength at +0,
    // wRevision at +4, wCertificateType at +6, data at +8)
    let blob = &raw[start + 8..end];
    // compute every supported Authenticode digest and judge the blob's
    // earliest DigestInfo against them (see `digest_verdict` for why only
    // the earliest counts)
    let computed = authenticode_digest(raw, header, start, end);
    let candidates = digest_candidates(blob);
    let digest_matches = digest_verdict(&candidates, &computed);
    Some(AuthenticodeInfo {
        table_offset: offset,
        table_size: size,
        table_is_tail: end == raw.len(),
        digest_matches,
        cert_names: cert_names(blob),
        company_name: company_name(raw, header),
    })
}

/// The file's computed Authenticode digests, one per supported algorithm.
struct ComputedDigests {
    /// The SHA-1 Authenticode digest.
    sha1: [u8; 20],
    /// The SHA-256 Authenticode digest.
    sha256: [u8; 32],
    /// The SHA-384 Authenticode digest.
    sha384: [u8; 48],
    /// The SHA-512 Authenticode digest.
    sha512: [u8; 64],
}

/// Judge the computed digests against the earliest `DigestInfo` candidate.
///
/// Only the FIRST candidate in blob order counts: in a well-formed
/// Authenticode blob that is `SpcIndirectDataContent`'s `messageDigest` — the
/// only `DigestInfo` the file digest is ever equal to — and it precedes the
/// certificates, the signer attributes, and any table slack. A digest planted
/// later in the (digest-excluded) table therefore cannot flip a mismatch into
/// a match, in any algorithm or encoding. This stays a heuristic structural
/// scan, not an ASN.1 verification: a mismatch is the strong signal; a match
/// is "consistent", never an attestation.
///
/// # Arguments
///
/// * `candidates` - The `(blob offset, digest)` candidates, in blob order
/// * `computed` - The file's computed Authenticode digests
fn digest_verdict(candidates: &[(usize, Vec<u8>)], computed: &ComputedDigests) -> Option<bool> {
    // the earliest candidate in blob order is the SPC content digest
    let (_, earliest) = candidates.first()?;
    // compare against the computed digest of the algorithm the blob declares
    Some(match earliest.len() {
        20 => earliest[..] == computed.sha1[..],
        32 => earliest[..] == computed.sha256[..],
        48 => earliest[..] == computed.sha384[..],
        64 => earliest[..] == computed.sha512[..],
        _ => false,
    })
}

/// Compute the file's Authenticode digests (SHA-1/-256/-384/-512).
///
/// Per the Authenticode spec: hash the whole file excluding the optional
/// header's 4-byte CheckSum, the 8-byte security data-directory entry, and
/// the certificate table bytes themselves.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `header` - The parsed PE header carrying the excluded-field offsets
/// * `table_start` - The certificate table's start offset
/// * `table_end` - The certificate table's end offset
fn authenticode_digest(
    raw: &[u8],
    header: &FormatHeader,
    table_start: usize,
    table_end: usize,
) -> ComputedDigests {
    // collect the excluded ranges, clamped to the file and sorted
    let mut skips: Vec<(usize, usize)> = Vec::new();
    if let Some(checksum) = header.checksum_offset {
        let at = checksum as usize;
        skips.push((at, (at + 4).min(raw.len())));
    }
    if let Some(entry) = header.cert_dir_entry_offset {
        let at = entry as usize;
        skips.push((at, (at + 8).min(raw.len())));
    }
    skips.push((table_start, table_end));
    skips.sort_unstable();
    // feed every non-excluded span to all four hashers in file order
    let mut sha1 = Sha1::new();
    let mut sha256 = Sha256::new();
    let mut sha384 = Sha384::new();
    let mut sha512 = Sha512::new();
    let mut update = |span: &[u8]| {
        sha1.update(span);
        sha256.update(span);
        sha384.update(span);
        sha512.update(span);
    };
    let mut cursor = 0usize;
    for (from, to) in skips {
        if from > cursor {
            update(&raw[cursor..from.min(raw.len())]);
        }
        cursor = cursor.max(to);
    }
    if cursor < raw.len() {
        update(&raw[cursor..]);
    }
    drop(update);
    ComputedDigests {
        sha1: sha1.finalize().into(),
        sha256: sha256.finalize().into(),
        sha384: sha384.finalize().into(),
        sha512: sha512.finalize().into(),
    }
}

/// Scan a PKCS#7 blob for `DigestInfo`-shaped digest values, in blob order.
///
/// Matches the exact DER byte shapes `30 ..` is not needed for: a digest OID
/// (SHA-1 `06 05 2b 0e 03 02 1a`, or SHA-256/-384/-512 `06 09 60 86 48 01 65
/// 03 04 02 01|02|03`), optionally followed by `05 00` (NULL params, which
/// DER allows omitting), then an OCTET STRING of the digest length. Every
/// occurrence is collected with its blob offset and the result is sorted by
/// offset, because the caller judges only the EARLIEST candidate (see
/// [`digest_verdict`]) — pattern-scan order must not leak into that choice.
///
/// # Arguments
///
/// * `blob` - The PKCS#7 signature bytes
fn digest_candidates(blob: &[u8]) -> Vec<(usize, Vec<u8>)> {
    /// The DER tails that announce a digest value: the digest OID, optionally
    /// NULL parameters, then the OCTET STRING.
    const PATTERNS: &[(&[u8], usize)] = &[
        // sha1 OID 1.3.14.3.2.26 (+NULL) + OCTET STRING of 20
        (
            &[
                0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14,
            ],
            20,
        ),
        (&[0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x04, 0x14], 20),
        // sha256 OID 2.16.840.1.101.3.4.2.1 (+NULL) + OCTET STRING of 32
        (
            &[
                0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04,
                0x20,
            ],
            32,
        ),
        (
            &[
                0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x04, 0x20,
            ],
            32,
        ),
        // sha384 OID 2.16.840.1.101.3.4.2.2 (+NULL) + OCTET STRING of 48
        (
            &[
                0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04,
                0x30,
            ],
            48,
        ),
        (
            &[
                0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x04, 0x30,
            ],
            48,
        ),
        // sha512 OID 2.16.840.1.101.3.4.2.3 (+NULL) + OCTET STRING of 64
        (
            &[
                0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04,
                0x40,
            ],
            64,
        ),
        (
            &[
                0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x04, 0x40,
            ],
            64,
        ),
    ];
    let mut out = Vec::new();
    for (pattern, digest_len) in PATTERNS {
        let mut from = 0usize;
        // collect every occurrence; the blob is small (tens of KB)
        while let Some(at) = find(blob, pattern, from) {
            let start = at + pattern.len();
            if let Some(digest) = blob.get(start..start + digest_len) {
                out.push((at, digest.to_vec()));
            }
            from = at + 1;
        }
    }
    // order by blob offset so "first" means first in the blob, not first in
    // the pattern table
    out.sort_unstable_by_key(|(at, _)| *at);
    out
}

/// Scan a PKCS#7 blob for certificate common names (heuristic).
///
/// Looks for the X.509 commonName OID (`06 03 55 04 03`) followed by a
/// printable/UTF-8 string tag and collects the values, deduped and bounded.
/// Good enough to say whose certificate was grafted; not a parser.
///
/// # Arguments
///
/// * `blob` - The PKCS#7 signature bytes
fn cert_names(blob: &[u8]) -> Vec<String> {
    const CN_OID: &[u8] = &[0x06, 0x03, 0x55, 0x04, 0x03];
    let mut out: Vec<String> = Vec::new();
    let mut from = 0usize;
    while let Some(at) = find(blob, CN_OID, from) {
        from = at + 1;
        // the value follows as PrintableString (0x13) or UTF8String (0x0c)
        let tag_at = at + CN_OID.len();
        let Some(&tag) = blob.get(tag_at) else { break };
        if tag != 0x13 && tag != 0x0c {
            continue;
        }
        // short-form lengths only; CN values are short in practice
        let Some(&len) = blob.get(tag_at + 1) else {
            break;
        };
        let len = len as usize;
        if len == 0 || len > 96 {
            continue;
        }
        let Some(value) = blob.get(tag_at + 2..tag_at + 2 + len) else {
            continue;
        };
        let Ok(name) = std::str::from_utf8(value) else {
            continue;
        };
        // dedupe while preserving the blob's order (issuer chains repeat names)
        if !out.iter().any(|existing| existing == name) {
            out.push(name.to_owned());
            if out.len() >= MAX_CERT_NAMES {
                break;
            }
        }
    }
    out
}

/// Recover the VersionInfo `CompanyName` value from the raw bytes, if present.
///
/// Anchors on the `VS_VERSION_INFO` resource marker, trying occurrences
/// inside the declared `.rsrc` section first (the host's own version
/// resources live there) and the rest in file order only as a fallback, and
/// returns the first anchor whose following window actually carries a
/// UTF-16LE `CompanyName` value. The preference keeps a decoy planted
/// elsewhere in the file (an embedded second binary, a loose string) from
/// speaking for the host; trying every anchor keeps a bare marker (a
/// VERSIONINFO with no StringFileInfo, a stray string) from blocking the real
/// resource behind it. A decoy embedded *inside* `.rsrc` data can still win,
/// so this stays a heuristic (no resource-tree walk) and the caller labels it
/// as such.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `header` - The parsed PE header carrying the section table
fn company_name(raw: &[u8], header: &FormatHeader) -> Option<String> {
    /// The most anchor occurrences collected before settling for what was seen.
    const MAX_ANCHORS: usize = 64;
    // anchor on the version resource so the scan is scoped and cheap
    let anchor: Vec<u8> = "VS_VERSION_INFO"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    // the resource section's on-disk extent, when the table declares one
    let rsrc = header
        .sections
        .iter()
        .find(|section| section.name == ".rsrc" && section.raw_size > 0)
        .map(|section| {
            (
                section.raw_offset,
                section.raw_offset.saturating_add(section.raw_size),
            )
        });
    // collect the anchor occurrences, bounded against hostile marker floods
    let mut anchors: Vec<usize> = Vec::new();
    let mut from = 0usize;
    for _ in 0..MAX_ANCHORS {
        let Some(at) = find(raw, &anchor, from) else {
            break;
        };
        anchors.push(at);
        from = at + 1;
    }
    // partition into in-.rsrc anchors (preferred) and the rest, each in
    // file order
    let (preferred, rest): (Vec<usize>, Vec<usize>) = anchors.into_iter().partition(|at| {
        rsrc.is_some_and(|(start, end)| (*at as u64) >= start && (*at as u64) < end)
    });
    // return the first anchor that actually yields a CompanyName value
    preferred
        .into_iter()
        .chain(rest)
        .find_map(|at| read_company_value(raw, at))
}

/// Read the UTF-16LE `CompanyName` value in the window after one anchor.
///
/// # Arguments
///
/// * `raw` - The sample's file bytes
/// * `resource` - The `VS_VERSION_INFO` anchor offset to read after
fn read_company_value(raw: &[u8], resource: usize) -> Option<String> {
    // only the window after the anchor can belong to this version resource
    let window_end = resource.saturating_add(8192).min(raw.len());
    let raw = &raw[resource..window_end];
    // encode the key as the UTF-16LE bytes it occupies in the resource
    let key: Vec<u8> = "CompanyName"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let at = find(raw, &key, 0)?;
    // skip the key's NUL terminator and the DWORD-alignment padding
    let mut cursor = at + key.len() + 2;
    while cursor + 1 < raw.len() && raw[cursor] == 0 && raw[cursor + 1] == 0 {
        cursor += 2;
    }
    // read UTF-16LE code units until the value's NUL, bounded
    let mut units = Vec::new();
    while units.len() < 128 {
        let unit = u16::from_le_bytes([*raw.get(cursor)?, *raw.get(cursor + 1)?]);
        if unit == 0 {
            break;
        }
        units.push(unit);
        cursor += 2;
    }
    let name = String::from_utf16(&units).ok()?;
    let trimmed = name.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// Find a byte pattern in a haystack, starting from an offset.
///
/// # Arguments
///
/// * `haystack` - The bytes to search
/// * `needle` - The pattern to find
/// * `from` - The offset to start searching at
fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    // a plain windows scan; every caller's needle is short
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| from + at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::fileinfo::HeaderSection;

    /// Digest candidates match the OID shapes and come back in blob order.
    #[test]
    fn digest_candidates_find_both_algorithms() {
        // a sha256 DigestInfo tail followed by a sha1 one: blob order must
        // win over the pattern table's order (sha1 patterns scan first)
        let mut blob = vec![0u8; 4];
        blob.extend_from_slice(&[
            0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04,
            0x20,
        ]);
        blob.extend_from_slice(&[0xBB; 32]);
        blob.extend_from_slice(&[
            0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14,
        ]);
        blob.extend_from_slice(&[0xAA; 20]);
        let found = digest_candidates(&blob);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0], (4, vec![0xBB; 32]));
        assert_eq!(found[1], (4 + 15 + 32, vec![0xAA; 20]));
    }

    /// Only the earliest candidate counts; a later planted match cannot flip
    /// a mismatch into a verification.
    #[test]
    fn digest_verdict_judges_only_the_earliest_candidate() {
        let computed = ComputedDigests {
            sha1: [0x11; 20],
            sha256: [0x22; 32],
            sha384: [0x33; 48],
            sha512: [0x44; 64],
        };
        // the real (earliest) sha1 digest mismatches; a planted sha256 later
        // in the blob equals the computed value and must not rescue it
        let planted = vec![(10usize, vec![0xAA; 20]), (500usize, vec![0x22; 32])];
        assert_eq!(digest_verdict(&planted, &computed), Some(false));
        // the earliest candidate matching its own algorithm verifies
        let genuine = vec![(10usize, vec![0x11; 20]), (500usize, vec![0xAA; 32])];
        assert_eq!(digest_verdict(&genuine, &computed), Some(true));
        // no candidates at all means there was nothing to validate
        assert_eq!(digest_verdict(&[], &computed), None);
    }

    /// Common names are collected after the CN OID and deduped in order.
    #[test]
    fn cert_names_scan_dedupes() {
        // two CN attributes, the first repeated (issuer + subject)
        let mut blob = Vec::new();
        for name in ["philandro", "philandro", "DigiCert"] {
            blob.extend_from_slice(&[0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, name.len() as u8]);
            blob.extend_from_slice(name.as_bytes());
        }
        assert_eq!(cert_names(&blob), vec!["philandro", "DigiCert"]);
    }

    /// The Authenticode digest skips the checksum, cert entry, and table.
    #[test]
    fn authenticode_digest_excludes_the_declared_ranges() {
        // a toy "file": 64 bytes, with fake field offsets inside it
        let raw: Vec<u8> = (0u8..64).collect();
        let header = FormatHeader {
            checksum_offset: Some(8),
            cert_dir_entry_offset: Some(16),
            ..Default::default()
        };
        let computed = authenticode_digest(&raw, &header, 32, 40);
        // the reference computation hashes the same spans straight-line
        let mut reference = Sha1::new();
        reference.update(&raw[0..8]);
        reference.update(&raw[12..16]);
        reference.update(&raw[24..32]);
        reference.update(&raw[40..]);
        assert_eq!(computed.sha1, <[u8; 20]>::from(reference.finalize()));
    }

    /// CompanyName is read as UTF-16LE inside the version-resource window.
    #[test]
    fn company_name_reads_utf16_value() {
        // a decoy CompanyName before the resource must not win
        let mut raw = vec![0u8; 8];
        raw.extend("CompanyName".encode_utf16().flat_map(u16::to_le_bytes));
        raw.extend_from_slice(&[0, 0, 0, 0]);
        raw.extend("Decoy Corp".encode_utf16().flat_map(u16::to_le_bytes));
        raw.extend_from_slice(&[0, 0]);
        // the real resource: anchor, key, NUL, pad, value, terminator
        raw.extend("VS_VERSION_INFO".encode_utf16().flat_map(u16::to_le_bytes));
        raw.extend_from_slice(&[0, 0]);
        raw.extend("CompanyName".encode_utf16().flat_map(u16::to_le_bytes));
        raw.extend_from_slice(&[0, 0, 0, 0]);
        raw.extend("Macromedia, Inc.".encode_utf16().flat_map(u16::to_le_bytes));
        raw.extend_from_slice(&[0, 0]);
        // no section table declared: the first anchored resource stands
        let header = FormatHeader::default();
        assert_eq!(
            company_name(&raw, &header).as_deref(),
            Some("Macromedia, Inc.")
        );
        // without the anchor there is no attributable company at all
        let bare: Vec<u8> = "CompanyName\0\0Evil"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(company_name(&bare, &header), None);
    }

    /// Appends one `anchor + CompanyName + value` version-resource shape.
    ///
    /// # Arguments
    ///
    /// * `raw` - The buffer to append the resource bytes onto
    /// * `company` - The CompanyName value the resource carries
    fn push_version_resource(raw: &mut Vec<u8>, company: &str) {
        raw.extend("VS_VERSION_INFO".encode_utf16().flat_map(u16::to_le_bytes));
        raw.extend_from_slice(&[0, 0]);
        raw.extend("CompanyName".encode_utf16().flat_map(u16::to_le_bytes));
        raw.extend_from_slice(&[0, 0, 0, 0]);
        raw.extend(company.encode_utf16().flat_map(u16::to_le_bytes));
        raw.extend_from_slice(&[0, 0]);
    }

    /// A full decoy resource outside `.rsrc` cannot speak for the host when
    /// the host's own resource section carries one, and a bare in-`.rsrc`
    /// marker with no CompanyName does not block the real resource behind it.
    #[test]
    fn company_name_prefers_the_resource_section() {
        // an embedded second binary's version resource, early in the file
        let mut raw = vec![0u8; 16];
        push_version_resource(&mut raw, "Microsoft Corporation");
        // the host's resource section opens with a bare marker (no
        // StringFileInfo) whose whole read window is padding...
        let rsrc_start = raw.len() as u64;
        raw.extend("VS_VERSION_INFO".encode_utf16().flat_map(u16::to_le_bytes));
        raw.extend_from_slice(&vec![0u8; 9000]);
        // ...followed by the host's real version resource
        push_version_resource(&mut raw, "philandro Software GmbH");
        let rsrc_size = raw.len() as u64 - rsrc_start;
        let header = FormatHeader {
            sections: vec![HeaderSection {
                name: ".rsrc".to_owned(),
                address: 0x4000,
                virtual_size: rsrc_size,
                raw_offset: rsrc_start,
                raw_size: rsrc_size,
                flags: 0,
            }],
            ..Default::default()
        };
        assert_eq!(
            company_name(&raw, &header).as_deref(),
            Some("philandro Software GmbH")
        );
    }
}
