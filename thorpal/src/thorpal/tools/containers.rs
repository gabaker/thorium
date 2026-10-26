//! Tools for statically listing, extracting, and decompressing containers.
//!
//! Malware routinely ships inside containers -- an MSI (OLE compound file)
//! carrying a CAB carrying the PE, a ZIP with the "infected" password, a
//! gzip-wrapped payload -- and these tools unpack that nesting natively so no
//! agent ever hand-writes a decompressor for a standardized format. Everything
//! here is static analysis only: bytes are parsed and copied, and nothing is
//! ever executed.
//!
//! Extracted children land in the session store's `extracted/` workspace (see
//! [`SessionStore::extracted_path`]), where every file tool can address them
//! through its `target` argument -- so an extracted CAB can itself be listed
//! and extracted, nesting as deep as the sample does. Raw compressed streams
//! (gzip/zlib/deflate/lzma/xz/lznt1/aplib) decompress through [`decompress`],
//! either from a file window or in-memory from scripts.

pub(crate) mod aplib;
pub(crate) mod lznt1;
pub(crate) mod office;

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rig::tool::{ToolContext, ToolExecutionError};
use serde::Serialize;
use tracing::instrument;

use crate::thorpal::store::SessionStore;
use crate::thorpal::tools::file::{self, SampleInfo};
use crate::thorpal::tools::ghidra::GhidraSession;

/// The most bytes a single extracted entry may hold (256 MiB)
pub(crate) const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// The most total bytes a session's extracted workspace may hold (1 GiB)
pub(crate) const MAX_TOTAL_EXTRACTED: u64 = 1024 * 1024 * 1024;

/// The most bytes one decompression may produce (64 MiB)
pub(crate) const MAX_DECOMPRESS_BYTES: usize = 64 * 1024 * 1024;

/// The most input bytes one decompression may consume (64 MiB)
pub(crate) const MAX_DECOMPRESS_INPUT: usize = 64 * 1024 * 1024;

/// The default number of entries one `list_container` call returns
pub(crate) const DEFAULT_ENTRIES: usize = 200;

/// The most entries one `list_container` call may return
pub(crate) const MAX_ENTRIES: usize = 500;

/// The number of head bytes hexdumped as an extracted file's preview
const PREVIEW_BYTES: usize = 64;

/// The memory liblzma's xz decoder may allocate (dictionary included)
///
/// Bounds a hostile xz header's declared dictionary (up to 1.5 GiB) while
/// leaving comfortable headroom over the 64 MiB output cap for the
/// dictionaries real-world xz files use.
const XZ_MEMLIMIT: u64 = 256 * 1024 * 1024;

/// The most candidate entry names quoted in an unknown-entry error
const MAX_CANDIDATES: usize = 30;

/// The container formats the native tools can list and extract
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainerKind {
    /// A Microsoft Cabinet (MSCF) archive
    Cab,
    /// A ZIP archive (also OOXML/JAR/APK)
    Zip,
    /// An OLE Compound Document File (MSI installer or legacy Office)
    Cfb,
}

/// Identify a supported container format from its leading byte signature
///
/// Matches the same magics as [`file::identify_format`], so the two views of a
/// file can never disagree about what it is.
///
/// # Arguments
///
/// * `head` - The file's head bytes to match the signatures against
pub(crate) fn detect_container(head: &[u8]) -> Option<ContainerKind> {
    // match the container magics in the same order the format table uses
    if head.starts_with(b"MSCF") {
        Some(ContainerKind::Cab)
    } else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Some(ContainerKind::Zip)
    } else if head.starts_with(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1") {
        Some(ContainerKind::Cfb)
    } else {
        None
    }
}

/// A Read+Seek view of a file starting at a fixed byte offset
///
/// Lets the container parsers open a container embedded mid-file (e.g. a CAB
/// appended to a dropper) by translating every seek -- including the
/// `SeekFrom::End` seeks a ZIP end-of-central-directory scan makes -- onto the
/// embedded region.
struct OffsetReader<R: Read + Seek> {
    /// The underlying reader positioned within the whole file
    inner: R,
    /// The file offset the embedded region starts at
    base: u64,
    /// The length of the embedded region (to the end of the file)
    len: u64,
}

impl<R: Read + Seek> OffsetReader<R> {
    /// Creates a view of `inner` starting at `base`, running to its end
    ///
    /// # Arguments
    ///
    /// * `inner` - The reader over the whole file
    /// * `base` - The file offset the embedded region starts at
    fn new(mut inner: R, base: u64) -> Result<Self, String> {
        // measure the whole file so End seeks can be translated
        let total = inner
            .seek(SeekFrom::End(0))
            .map_err(|err| format!("failed to measure the file: {err}"))?;
        // an offset past EOF leaves nothing to parse
        if base > total {
            return Err(format!(
                "offset {base:#x} is past the end of the file ({total} bytes)"
            ));
        }
        // start positioned at the region's beginning
        inner
            .seek(SeekFrom::Start(base))
            .map_err(|err| format!("failed to seek to offset {base:#x}: {err}"))?;
        Ok(OffsetReader {
            inner,
            base,
            len: total - base,
        })
    }
}

impl<R: Read + Seek> Read for OffsetReader<R> {
    /// Reads pass straight through; EOF falls where the file itself ends
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Seek> Seek for OffsetReader<R> {
    /// Translates virtual positions within the region onto the whole file
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        // compute the virtual target position within the embedded region
        let target = match pos {
            SeekFrom::Start(forward) => i128::from(forward),
            SeekFrom::End(delta) => i128::from(self.len) + i128::from(delta),
            SeekFrom::Current(delta) => {
                // the current virtual position is the inner position less base
                let current = self.inner.stream_position()? - self.base;
                i128::from(current) + i128::from(delta)
            }
        };
        // a seek before the region start is invalid, like any pre-zero seek
        let target = u64::try_from(target).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek before the start of the embedded region",
            )
        })?;
        // refuse a target so large the underlying position would overflow
        // (hostile zip64 offsets), matching the pre-region error path
        let absolute = self.base.checked_add(target).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek past the addressable range of the embedded region",
            )
        })?;
        // apply the translated seek and report the virtual position back
        self.inner.seek(SeekFrom::Start(absolute))?;
        Ok(target)
    }
}

/// The 64-character alphabet MSI stream names are encoded with
const MSI_CHARSET: &[u8; 64] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz._";

/// Decode an MSI-encoded CFB stream name back to its readable form
///
/// MSI files store stream names in a private UTF-16 encoding packing two
/// base64-style characters per code point: `0x3800..0x4800` carries two
/// characters, `0x4800..0x4840` carries one, and `0x4840` is the leading `!`
/// marker on system streams. Returns `None` when the name is not MSI-encoded
/// (any code point outside those ranges), in which case the raw name stands.
///
/// # Arguments
///
/// * `raw` - The CFB stream name as stored
pub(crate) fn decode_msi_name(raw: &str) -> Option<String> {
    let mut decoded = String::new();
    // walk the code points, decoding each per its range
    for c in raw.chars() {
        let code = c as u32;
        match code {
            // the system-stream marker decodes to a leading '!'
            0x4840 => decoded.push('!'),
            // this range packs two charset indices into one code point
            0x3800..=0x47FF => {
                let packed = code - 0x3800;
                decoded.push(MSI_CHARSET[(packed & 0x3F) as usize] as char);
                decoded.push(MSI_CHARSET[(packed >> 6) as usize] as char);
            }
            // this range carries a single trailing charset index
            0x4800..=0x483F => {
                decoded.push(MSI_CHARSET[(code - 0x4800) as usize] as char);
            }
            // any other code point means the name was never MSI-encoded
            _ => return None,
        }
    }
    // an empty name decodes to nothing worth showing
    (!decoded.is_empty()).then_some(decoded)
}

/// Map a container entry name onto a safe extracted file name
///
/// Every character outside `[A-Za-z0-9._-]` (path separators included)
/// becomes an underscore, runs collapse, leading dots/hyphens/underscores are
/// trimmed, and the result is capped at the store's name limit -- so a hostile
/// entry name (`../../evil`) can never traverse out of the workspace. A name
/// that sanitizes to nothing usable falls back to `entry-<index>`.
///
/// # Arguments
///
/// * `name` - The container entry name as stored
/// * `index` - The entry's index, used for the fallback name
pub(crate) fn sanitize_entry_name(name: &str, index: usize) -> String {
    // map unsafe characters to underscores, collapsing runs
    let mut out = String::with_capacity(name.len());
    let mut mapped = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
            out.push(c);
            mapped = false;
        } else if !mapped {
            // collapse runs of unsafe characters into a single underscore
            out.push('_');
            mapped = true;
        }
    }
    // trim the leading characters the validator refuses
    let mut safe: String = out.trim_start_matches(['.', '-', '_']).to_owned();
    // cap the length, keeping the head
    safe.truncate(128);
    // keep clear of the atomic writer's `.tmp` sibling convention
    if safe.ends_with(".tmp") {
        safe.push('_');
    }
    // anything still unusable falls back to a stable indexed name
    if crate::thorpal::store::validate_extracted_name(&safe).is_err() {
        return format!("entry-{index}");
    }
    safe
}

/// One entry in a listed container
#[derive(Debug, Clone, Serialize)]
pub struct ContainerEntry {
    /// The entry's name as stored (MSI stream names are shown decoded)
    pub name: String,
    /// The entry's uncompressed size in bytes
    pub size: u64,
    /// The entry's compressed size in bytes, when the format records one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compressed_size: Option<u64>,
    /// The compression method the entry is stored with
    pub method: String,
    /// The entry's recorded modification time, when one exists
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// Whether the entry is encrypted (ZIP only; pass a password to extract)
    pub encrypted: bool,
    /// Whether the entry is a directory/storage rather than a file
    pub is_dir: bool,
}

/// The listing of one container's entries
#[derive(Debug, Clone, Serialize)]
pub struct ContainerListing {
    /// The container's identified format description
    pub format: String,
    /// The short container kind: `zip`, `cab`, `cfb`, `gzip`, `xz`, or `bzip2`
    pub kind: &'static str,
    /// The entries listed, in container order
    pub entries: Vec<ContainerEntry>,
    /// The total number of matching entries in the container
    pub total_entries: usize,
    /// A note describing truncation or how to handle a single-stream format
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// A file extracted (or decompressed) into the session's workspace
#[derive(Debug, Clone, Serialize)]
pub struct ExtractedFile {
    /// The extracted file's name, usable as every file tool's `target`
    pub name: String,
    /// The extracted file's identity: size, hashes, entropy, and format
    pub info: SampleInfo,
    /// A hexdump of the extracted file's first bytes
    pub preview: String,
    /// A deterministic next-step note (e.g. an importable PE/ELF/Mach-O child)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The size budget one extraction runs under
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtractLimits {
    /// The most bytes this single entry may extract to
    pub entry: u64,
    /// The remaining total workspace budget across every extracted file
    pub total: u64,
}

/// Opens a file and reads the head bytes of the container at an offset
///
/// # Arguments
///
/// * `path` - The file holding the container
/// * `offset` - The byte offset the container starts at within the file
fn open_at(path: &Path, offset: u64) -> Result<(OffsetReader<std::fs::File>, Vec<u8>), String> {
    // open the file read-only and view it from the requested offset
    let file = std::fs::File::open(path)
        .map_err(|err| format!("failed to open '{}': {err}", path.display()))?;
    let mut reader = OffsetReader::new(file, offset)?;
    // read the head bytes the magic tables match against
    let mut head = vec![0u8; 8];
    let read = reader
        .read(&mut head)
        .map_err(|err| format!("failed to read '{}': {err}", path.display()))?;
    head.truncate(read);
    // rewind so the parser sees the container from its first byte
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|err| format!("failed to rewind '{}': {err}", path.display()))?;
    Ok((reader, head))
}

/// Whether an entry name passes a case-insensitive substring filter
///
/// # Arguments
///
/// * `name` - The entry name to test
/// * `filter` - The lowercased filter, or `None` to match everything
fn matches_filter(name: &str, filter: Option<&str>) -> bool {
    // an absent filter matches every entry
    match filter {
        Some(needle) => name.to_lowercase().contains(needle),
        None => true,
    }
}

/// List a container's entries synchronously
///
/// Single-stream compression formats (gzip/xz/bzip2) have no entries to list
/// and come back with a note pointing at the right next step instead;
/// recognized-but-unsupported archives (7-Zip, RAR) error with a clear
/// message. Shared by the async tool path and the Rhai binding.
///
/// # Arguments
///
/// * `path` - The file holding the container
/// * `offset` - The byte offset the container starts at within the file
/// * `limit` - The most entries to return
/// * `filter` - An optional case-insensitive substring filter on entry names
pub(crate) fn list_container_sync(
    path: &Path,
    offset: u64,
    limit: usize,
    filter: Option<&str>,
) -> Result<ContainerListing, String> {
    // open the container's region and identify it from its head bytes
    let (reader, head) = open_at(path, offset)?;
    let format = file::identify_format(&head).to_string();
    // lowercase the filter once for case-insensitive matching
    let needle = filter.map(str::to_lowercase);
    let needle = needle.as_deref();
    // dispatch on the supported container kinds first
    match detect_container(&head) {
        Some(ContainerKind::Zip) => list_zip(reader, format, limit, needle),
        Some(ContainerKind::Cab) => list_cab(reader, format, limit, needle),
        Some(ContainerKind::Cfb) => list_cfb(reader, format, limit, needle),
        None => {
            // single-stream formats carry one payload, not entries
            let single = if head.starts_with(b"\x1f\x8b") {
                Some((
                    "gzip",
                    "use the decompress tool with format \"gzip\" to inflate it",
                ))
            } else if head.starts_with(b"\xfd7zXZ\x00") {
                Some((
                    "xz",
                    "use the decompress tool with format \"xz\" to inflate it",
                ))
            } else if head.starts_with(b"BZh") {
                Some((
                    "bzip2",
                    "native bzip2 decompression is not supported; note the limitation \
                     in your findings",
                ))
            } else {
                None
            };
            if let Some((kind, note)) = single {
                return Ok(ContainerListing {
                    format,
                    kind,
                    entries: Vec::new(),
                    total_entries: 0,
                    truncated: Some(format!(
                        "this is a single {kind} compressed stream, not an entry \
                         container; {note}"
                    )),
                });
            }
            // recognized-but-unsupported archive formats get an honest error
            if head.starts_with(b"7z\xbc\xaf\x27\x1c") {
                return Err(
                    "this is a 7-Zip archive; native 7z listing/extraction is not \
                     supported (ZIP, CAB, and OLE/CFB containers are)"
                        .to_owned(),
                );
            }
            if head.starts_with(b"Rar!\x1a\x07") {
                return Err(
                    "this is a RAR archive; native RAR listing/extraction is not \
                     supported (ZIP, CAB, and OLE/CFB containers are)"
                        .to_owned(),
                );
            }
            Err(format!(
                "no supported container found at offset {offset:#x} (identified: \
                 {format}); supported containers are ZIP, CAB, and OLE/CFB"
            ))
        }
    }
}

/// List a ZIP archive's entries
///
/// # Arguments
///
/// * `reader` - The reader positioned over the archive
/// * `format` - The identified format description
/// * `limit` - The most entries to return
/// * `filter` - The lowercased name filter, if any
fn list_zip<R: Read + Seek>(
    reader: R,
    format: String,
    limit: usize,
    filter: Option<&str>,
) -> Result<ContainerListing, String> {
    // parse the central directory without touching any entry data
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|err| format!("failed to parse the ZIP: {err}"))?;
    let mut entries = Vec::new();
    let mut matched = 0usize;
    for index in 0..archive.len() {
        // the raw accessor reads metadata without decompressing (or decrypting)
        let entry = archive
            .by_index_raw(index)
            .map_err(|err| format!("failed to read ZIP entry {index}: {err}"))?;
        // apply the name filter before counting the entry
        if !matches_filter(entry.name(), filter) {
            continue;
        }
        matched += 1;
        // keep collecting until the page limit fills
        if entries.len() < limit {
            entries.push(ContainerEntry {
                name: entry.name().to_owned(),
                size: entry.size(),
                compressed_size: Some(entry.compressed_size()),
                method: entry.compression().to_string(),
                modified: entry.last_modified().map(|dt| dt.to_string()),
                encrypted: entry.encrypted(),
                is_dir: entry.is_dir(),
            });
        }
    }
    Ok(finish_listing(format, "zip", entries, matched))
}

/// List a CAB archive's entries
///
/// # Arguments
///
/// * `reader` - The reader positioned over the archive
/// * `format` - The identified format description
/// * `limit` - The most entries to return
/// * `filter` - The lowercased name filter, if any
fn list_cab<R: Read + Seek>(
    reader: R,
    format: String,
    limit: usize,
    filter: Option<&str>,
) -> Result<ContainerListing, String> {
    // parse the cabinet's folder and file tables
    let cabinet =
        cab::Cabinet::new(reader).map_err(|err| format!("failed to parse the CAB: {err}"))?;
    let mut entries = Vec::new();
    let mut matched = 0usize;
    // files live under folders, which carry the compression method
    for folder in cabinet.folder_entries() {
        let method = format!("{:?}", folder.compression_type());
        for entry in folder.file_entries() {
            // apply the name filter before counting the entry
            if !matches_filter(entry.name(), filter) {
                continue;
            }
            matched += 1;
            // keep collecting until the page limit fills
            if entries.len() < limit {
                entries.push(ContainerEntry {
                    name: entry.name().to_owned(),
                    size: u64::from(entry.uncompressed_size()),
                    compressed_size: None,
                    method: method.clone(),
                    modified: entry.datetime().map(|dt| dt.to_string()),
                    encrypted: false,
                    is_dir: false,
                });
            }
        }
    }
    Ok(finish_listing(format, "cab", entries, matched))
}

/// List an OLE compound file's storages and streams
///
/// Stream names are shown MSI-decoded when they decode (see
/// [`decode_msi_name`]); extraction matches either form.
///
/// # Arguments
///
/// * `reader` - The reader positioned over the compound file
/// * `format` - The identified format description
/// * `limit` - The most entries to return
/// * `filter` - The lowercased name filter, if any
fn list_cfb<R: Read + Seek>(
    reader: R,
    format: String,
    limit: usize,
    filter: Option<&str>,
) -> Result<ContainerListing, String> {
    // parse the compound file's directory tree
    let comp = cfb::CompoundFile::open(reader)
        .map_err(|err| format!("failed to parse the compound file: {err}"))?;
    let mut entries = Vec::new();
    let mut matched = 0usize;
    for entry in comp.walk() {
        // the root storage is the container itself, not an entry
        if entry.path() == Path::new("/") {
            continue;
        }
        // show MSI-encoded stream names decoded; raw names stand otherwise
        let name = decode_msi_name(entry.name()).unwrap_or_else(|| entry.name().to_owned());
        // apply the name filter before counting the entry
        if !matches_filter(&name, filter) {
            continue;
        }
        matched += 1;
        // keep collecting until the page limit fills
        if entries.len() < limit {
            entries.push(ContainerEntry {
                name,
                size: entry.len(),
                compressed_size: None,
                method: "stored".to_owned(),
                modified: None,
                encrypted: false,
                is_dir: !entry.is_stream(),
            });
        }
    }
    Ok(finish_listing(format, "cfb", entries, matched))
}

/// Assemble a listing, noting when the page limit truncated it
///
/// # Arguments
///
/// * `format` - The identified format description
/// * `kind` - The short container kind label
/// * `entries` - The entries collected under the page limit
/// * `matched` - The total number of entries that matched the filter
fn finish_listing(
    format: String,
    kind: &'static str,
    entries: Vec<ContainerEntry>,
    matched: usize,
) -> ContainerListing {
    // note when the limit cut the listing so the model knows to filter
    let truncated = (entries.len() < matched).then(|| {
        format!(
            "showing the first {} of {matched} matching entries; narrow with \
             `filter` or raise `limit` (max {MAX_ENTRIES})",
            entries.len()
        )
    });
    ContainerListing {
        format,
        kind,
        entries,
        total_entries: matched,
        truncated,
    }
}

/// Streams a reader into a file under a byte cap, atomically
///
/// The bytes land in a `.tmp` sibling first; on success it is renamed over the
/// final path, and on any failure -- the cap included -- the sibling is
/// deleted, so a truncated extract can never masquerade as the real entry.
///
/// # Arguments
///
/// * `reader` - The decompressed entry stream to copy
/// * `dest` - The final path to write
/// * `cap` - The most bytes to accept
/// * `cap_error` - The error to report when the cap is breached
pub(crate) fn write_capped(
    mut reader: impl Read,
    dest: &Path,
    cap: u64,
    cap_error: &str,
) -> Result<u64, String> {
    // write into the temp sibling so a failure never leaves a partial file
    let tmp = dest.with_file_name(format!(
        "{}.tmp",
        dest.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("extract")
    ));
    // the inner copy loop, split out so every failure path cleans the sibling
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|err| format!("failed to create '{}': {err}", tmp.display()))?;
        let mut written = 0u64;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            // pull the next chunk off the (decompressing) entry stream
            let read = reader
                .read(&mut buffer)
                .map_err(|err| format!("failed to read the entry's data: {err}"))?;
            if read == 0 {
                break;
            }
            // enforce the cap on the real byte count, not the declared size
            written += read as u64;
            if written > cap {
                return Err(cap_error.to_owned());
            }
            file.write_all(&buffer[..read])
                .map_err(|err| format!("failed to write '{}': {err}", tmp.display()))?;
        }
        Ok(written)
    })();
    match result {
        Ok(written) => {
            // atomically move the finished file over the final path; a failed
            // rename cleans the sibling too, so no stray `.tmp` ever lingers
            if let Err(err) = std::fs::rename(&tmp, dest) {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("failed to finalize '{}': {err}", dest.display()));
            }
            Ok(written)
        }
        Err(error) => {
            // delete the torn sibling; a failed cleanup is not worth masking the error
            let _ = std::fs::remove_file(&tmp);
            Err(error)
        }
    }
}

/// Picks a workspace name that does not collide with an existing file
///
/// Re-extracting an entry (or two entries whose names sanitize identically)
/// must never silently overwrite an earlier extraction, so a taken name gets
/// a `-2`, `-3`, ... counter spliced in ahead of its extension, trimming the
/// stem when needed so the result stays within the store's name cap.
///
/// # Arguments
///
/// * `dest_dir` - The extracted workspace directory
/// * `name` - The sanitized name the entry wants
fn uniquify_name(dest_dir: &Path, name: &str) -> String {
    // an untaken name is used as-is
    if !dest_dir.join(name).exists() {
        return name.to_owned();
    }
    // split the extension off so the counter lands ahead of it
    let (stem, ext) = match name.rfind('.') {
        Some(index) if index > 0 => name.split_at(index),
        _ => (name, ""),
    };
    // count upward until a free name appears
    for counter in 2u64.. {
        let suffix = format!("-{counter}");
        // trim the stem so the result stays within the validator's 128 cap
        // (the name is sanitized ascii, so byte slicing is char safe)
        let keep = stem
            .len()
            .min(128usize.saturating_sub(ext.len() + suffix.len()));
        let candidate = format!("{}{suffix}{ext}", &stem[..keep]);
        if !dest_dir.join(&candidate).exists() {
            return candidate;
        }
    }
    // the counter is unbounded, so a free name is always found above
    unreachable!("the uniquify counter never exhausts")
}

/// Builds the error reported when an extraction would breach a size cap
///
/// # Arguments
///
/// * `limits` - The budget the extraction ran under
fn cap_error(limits: &ExtractLimits) -> String {
    // name whichever cap is the binding one so the model knows what tripped
    if limits.total < limits.entry {
        format!(
            "extraction stopped: it would exceed the session's remaining extracted \
             workspace budget of {} bytes (total cap {MAX_TOTAL_EXTRACTED} bytes); \
             nothing was written",
            limits.total
        )
    } else {
        format!(
            "extraction stopped: the entry exceeds the per-entry cap of {} bytes; \
             nothing was written",
            limits.entry
        )
    }
}

/// Finalize an extracted file: identify it and build its preview
///
/// # Arguments
///
/// * `name` - The extracted file's workspace name
/// * `path` - The extracted file's full path
pub(crate) fn finish_extracted(name: String, path: &Path) -> Result<ExtractedFile, String> {
    // hash and identify the finished file so the result is self-describing
    let info = file::file_identity_sync(path)?;
    // hexdump the head so the model sees what landed without another call
    let (head, _) = file::read_window_sync(path, 0, PREVIEW_BYTES)?;
    let preview = file::hexdump(&head, 0);
    // a carved executable deserves real analysis: point at import_child at
    // the exact moment it lands so a decoded payload is never examined
    // through previews alone (every carve path funnels through here)
    let executable = ["MZ executable", "ELF executable", "Mach-O"]
        .iter()
        .any(|prefix| info.format.starts_with(prefix));
    let note = executable.then(|| {
        format!(
            "this is a {}; import it for full Ghidra analysis with \
             import_child (target: \"{name}\")",
            info.format
        )
    });
    Ok(ExtractedFile {
        name,
        info,
        preview,
        note,
    })
}

/// Extract one container entry into a destination directory, synchronously
///
/// The entry's declared size is checked against the limits first, then the
/// real stream is copied through a cap-enforcing writer (a lying size still
/// stops at the cap, with the partial file deleted). `password` applies to ZIP
/// only -- the "infected" convention -- and errors on other formats. Shared by
/// the async tool path and the Rhai binding.
///
/// # Arguments
///
/// * `path` - The file holding the container
/// * `offset` - The byte offset the container starts at within the file
/// * `entry` - The entry name to extract (CFB matches decoded or raw names)
/// * `password` - The ZIP password, when the archive needs one
/// * `dest_dir` - The directory to extract into (the session's `extracted/`)
/// * `limits` - The size budget this extraction runs under
pub(crate) fn extract_entry_sync(
    path: &Path,
    offset: u64,
    entry: &str,
    password: Option<&str>,
    dest_dir: &Path,
    limits: ExtractLimits,
) -> Result<ExtractedFile, String> {
    // open the container's region and identify its kind
    let (reader, head) = open_at(path, offset)?;
    let kind = detect_container(&head).ok_or_else(|| {
        format!(
            "no supported container found at offset {offset:#x} (identified: {}); \
             supported containers are ZIP, CAB, and OLE/CFB -- single compressed \
             streams go through the decompress tool instead",
            file::identify_format(&head)
        )
    })?;
    // passwords are a ZIP concept; refusing elsewhere beats silently ignoring
    if password.is_some() && kind != ContainerKind::Zip {
        return Err(
            "`password` only applies to ZIP archives; CAB and OLE/CFB entries are \
             never password protected"
                .to_owned(),
        );
    }
    // dispatch to the per-format extractor
    match kind {
        ContainerKind::Zip => extract_zip(reader, entry, password, dest_dir, limits),
        ContainerKind::Cab => extract_cab(reader, entry, dest_dir, limits),
        ContainerKind::Cfb => extract_cfb(reader, entry, dest_dir, limits),
    }
}

/// Builds the unknown-entry error, listing the candidate names
///
/// # Arguments
///
/// * `entry` - The entry name that missed
/// * `candidates` - The names the container actually holds
fn unknown_entry(entry: &str, candidates: &[String]) -> String {
    // quote a bounded list of real names so the model can correct itself
    let shown: Vec<&str> = candidates
        .iter()
        .take(MAX_CANDIDATES)
        .map(String::as_str)
        .collect();
    let suffix = if candidates.len() > MAX_CANDIDATES {
        ", …"
    } else {
        ""
    };
    format!(
        "no entry named '{entry}' exists in this container; entries: {}{suffix}",
        shown.join(", ")
    )
}

/// Check an entry's declared size against the extraction budget
///
/// # Arguments
///
/// * `declared` - The entry's declared uncompressed size
/// * `limits` - The size budget this extraction runs under
fn check_declared(declared: u64, limits: &ExtractLimits) -> Result<(), String> {
    // refuse up front when the declared size alone already breaches a cap
    if declared > limits.entry || declared > limits.total {
        return Err(cap_error(limits));
    }
    Ok(())
}

/// Extract one entry from a ZIP archive
///
/// # Arguments
///
/// * `reader` - The reader positioned over the archive
/// * `entry` - The entry name to extract
/// * `password` - The ZIP password, when the archive needs one
/// * `dest_dir` - The directory to extract into
/// * `limits` - The size budget this extraction runs under
fn extract_zip<R: Read + Seek>(
    reader: R,
    entry: &str,
    password: Option<&str>,
    dest_dir: &Path,
    limits: ExtractLimits,
) -> Result<ExtractedFile, String> {
    // parse the central directory
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|err| format!("failed to parse the ZIP: {err}"))?;
    // find the entry by its exact stored name
    let Some(index) = (0..archive.len()).find(|index| {
        archive
            .by_index_raw(*index)
            .is_ok_and(|candidate| candidate.name() == entry)
    }) else {
        // a miss lists the real names so the model can correct itself
        let candidates: Vec<String> = archive.file_names().map(str::to_owned).collect();
        return Err(unknown_entry(entry, &candidates));
    };
    // check the declared size and encryption up front, without decompressing
    let (declared, encrypted, sanitized) = {
        let raw = archive
            .by_index_raw(index)
            .map_err(|err| format!("failed to read ZIP entry '{entry}': {err}"))?;
        if raw.is_dir() {
            return Err(format!("'{entry}' is a directory, not an extractable file"));
        }
        (
            raw.size(),
            raw.encrypted(),
            sanitize_entry_name(entry, index),
        )
    };
    // never overwrite an earlier extraction that landed on the same name
    let sanitized = uniquify_name(dest_dir, &sanitized);
    check_declared(declared, &limits)?;
    if encrypted && password.is_none() {
        return Err(format!(
            "entry '{entry}' is encrypted; pass `password` (malware archives \
             conventionally use \"infected\")"
        ));
    }
    // open the entry for reading, decrypting when a password was given
    let file = match password {
        Some(password) => archive
            .by_index_decrypt(index, password.as_bytes())
            .map_err(|err| match err {
                zip::result::ZipError::InvalidPassword => {
                    format!("wrong password for entry '{entry}' (or it is not encrypted)")
                }
                other => format!("failed to open ZIP entry '{entry}': {other}"),
            })?,
        None => archive
            .by_index(index)
            .map_err(|err| format!("failed to open ZIP entry '{entry}': {err}"))?,
    };
    // stream the entry through the cap-enforcing writer and finalize it
    let dest = dest_dir.join(&sanitized);
    write_capped(
        file,
        &dest,
        limits.entry.min(limits.total),
        &cap_error(&limits),
    )?;
    finish_extracted(sanitized, &dest)
}

/// Extract one entry from a CAB archive
///
/// # Arguments
///
/// * `reader` - The reader positioned over the archive
/// * `entry` - The entry name to extract
/// * `dest_dir` - The directory to extract into
/// * `limits` - The size budget this extraction runs under
fn extract_cab<R: Read + Seek>(
    reader: R,
    entry: &str,
    dest_dir: &Path,
    limits: ExtractLimits,
) -> Result<ExtractedFile, String> {
    // parse the cabinet's tables
    let mut cabinet =
        cab::Cabinet::new(reader).map_err(|err| format!("failed to parse the CAB: {err}"))?;
    // find the entry and check its declared size before decompressing
    let Some(found) = cabinet.get_file_entry(entry) else {
        // a miss lists the real names so the model can correct itself
        let candidates: Vec<String> = cabinet
            .folder_entries()
            .flat_map(|folder| folder.file_entries())
            .map(|file| file.name().to_owned())
            .collect();
        return Err(unknown_entry(entry, &candidates));
    };
    check_declared(u64::from(found.uncompressed_size()), &limits)?;
    // never overwrite an earlier extraction that landed on the same name
    let sanitized = uniquify_name(dest_dir, &sanitize_entry_name(entry, 0));
    // stream the decompressed entry through the cap-enforcing writer
    let file = cabinet
        .read_file(entry)
        .map_err(|err| format!("failed to open CAB entry '{entry}': {err}"))?;
    let dest = dest_dir.join(&sanitized);
    write_capped(
        file,
        &dest,
        limits.entry.min(limits.total),
        &cap_error(&limits),
    )?;
    finish_extracted(sanitized, &dest)
}

/// Extract one stream from an OLE compound file
///
/// The entry matches by decoded MSI name, raw stored name, or full path.
///
/// # Arguments
///
/// * `reader` - The reader positioned over the compound file
/// * `entry` - The stream name to extract
/// * `dest_dir` - The directory to extract into
/// * `limits` - The size budget this extraction runs under
fn extract_cfb<R: Read + Seek>(
    reader: R,
    entry: &str,
    dest_dir: &Path,
    limits: ExtractLimits,
) -> Result<ExtractedFile, String> {
    // parse the compound file's directory tree
    let mut comp = cfb::CompoundFile::open(reader)
        .map_err(|err| format!("failed to parse the compound file: {err}"))?;
    // find the stream by decoded name, raw name, or full path
    let mut found: Option<(PathBuf, String, u64)> = None;
    let mut candidates = Vec::new();
    for candidate in comp.walk() {
        // only streams hold data; storages are just directories
        if !candidate.is_stream() {
            continue;
        }
        let decoded =
            decode_msi_name(candidate.name()).unwrap_or_else(|| candidate.name().to_owned());
        candidates.push(decoded.clone());
        // match the decoded display name, the raw stored name, or the path
        if decoded == entry || candidate.name() == entry || candidate.path() == Path::new(entry) {
            found = Some((candidate.path().to_path_buf(), decoded, candidate.len()));
            break;
        }
    }
    let Some((stream_path, decoded, declared)) = found else {
        // a miss lists the decoded names so the model can correct itself
        return Err(unknown_entry(entry, &candidates));
    };
    check_declared(declared, &limits)?;
    // never overwrite an earlier extraction that landed on the same name
    let sanitized = uniquify_name(dest_dir, &sanitize_entry_name(&decoded, 0));
    // stream the entry through the cap-enforcing writer and finalize it
    let stream = comp
        .open_stream(&stream_path)
        .map_err(|err| format!("failed to open stream '{entry}': {err}"))?;
    let dest = dest_dir.join(&sanitized);
    write_capped(
        stream,
        &dest,
        limits.entry.min(limits.total),
        &cap_error(&limits),
    )?;
    finish_extracted(sanitized, &dest)
}

/// A `Write` sink that errors once a byte cap is exceeded
///
/// Backs the writer-driven decompressors (lzma/xz), so a decompression bomb
/// fails with the cap named instead of ballooning memory.
struct CappedVecWriter {
    /// The decompressed bytes accepted so far
    buf: Vec<u8>,
    /// The most bytes to accept
    cap: usize,
}

impl Write for CappedVecWriter {
    /// Accepts bytes until the cap, then errors naming it
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        // refuse the write that would carry the buffer past the cap
        if self.buf.len() + data.len() > self.cap {
            return Err(std::io::Error::other(format!(
                "decompressed output exceeded the {} byte cap",
                self.cap
            )));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    /// Nothing is buffered beyond the vec itself
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reads a decompressor to its end under a byte cap
///
/// # Arguments
///
/// * `reader` - The decompressing reader to drain
/// * `cap` - The most output bytes to accept
/// * `format` - The format name for error messages
fn read_capped(mut reader: impl Read, cap: usize, format: &str) -> Result<Vec<u8>, String> {
    // read one byte past the cap so an exact overrun is detectable
    let mut out = Vec::new();
    let mut limited = (&mut reader).take(cap as u64 + 1);
    limited
        .read_to_end(&mut out)
        .map_err(|err| format!("failed to decompress as {format}: {err}"))?;
    // anything past the cap is a hard error, never a silent truncation
    if out.len() > cap {
        return Err(format!(
            "decompressed {format} output exceeded the {cap} byte cap"
        ));
    }
    Ok(out)
}

/// Reads one `u32` field out of a JSON props object with a default
///
/// # Arguments
///
/// * `props` - The raw LZMA props object
/// * `key` - The field to read
/// * `default` - The value an absent field falls back to
fn prop_u32(props: &serde_json::Value, key: &str, default: u32) -> Result<u32, String> {
    // an absent field falls back; a present one must be a sane integer
    match props.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| format!("lzma props field `{key}` must be a non-negative integer")),
    }
}

/// Decompress one raw buffer in a given format, bounded by an output cap
///
/// Formats: `gzip`, `zlib`, `deflate` (raw), `lzma` (headered by default, or
/// headerless when `props` supplies `{lc, lp, pb, dict_size, unpacked_size}`),
/// `xz`, `lznt1`, and `aplib` (raw or `AP32`-headered). Raw LZX is
/// deliberately excluded: LZX only appears inside CAB folders, which the
/// extraction path decompresses natively. Shared by the async tool path and
/// the Rhai binding.
///
/// # Arguments
///
/// * `input` - The compressed bytes
/// * `format` - The compression format name
/// * `props` - The raw-LZMA properties, for headerless LZMA1 streams
/// * `cap` - The most output bytes to produce
pub(crate) fn decompress_bytes(
    input: &[u8],
    format: &str,
    props: Option<&serde_json::Value>,
    cap: usize,
) -> Result<Vec<u8>, String> {
    // bound the input so a script can never feed an absurd buffer through
    if input.len() > MAX_DECOMPRESS_INPUT {
        return Err(format!(
            "the input is {} bytes; decompression accepts at most \
             {MAX_DECOMPRESS_INPUT} bytes per call",
            input.len()
        ));
    }
    // props only mean something to headerless lzma
    if props.is_some() && !format.eq_ignore_ascii_case("lzma") {
        return Err("`props` only applies to format \"lzma\" (headerless LZMA1)".to_owned());
    }
    // dispatch on the normalized format name
    match format.to_ascii_lowercase().as_str() {
        // the multi-member decoder keeps reading across concatenated gzip
        // streams instead of silently truncating after the first member
        "gzip" => read_capped(flate2::read::MultiGzDecoder::new(input), cap, "gzip"),
        "zlib" => read_capped(flate2::read::ZlibDecoder::new(input), cap, "zlib"),
        "deflate" => read_capped(flate2::read::DeflateDecoder::new(input), cap, "deflate"),
        "xz" => {
            // liblzma streams pull-based under a memlimit (lzma-rs's xz path
            // would buffer the whole output in memory before the cap could
            // trip), so the dictionary and the output are both bounded
            let stream = liblzma::stream::Stream::new_stream_decoder(XZ_MEMLIMIT, 0)
                .map_err(|err| format!("failed to build the xz decoder: {err}"))?;
            let decoder = liblzma::read::XzDecoder::new_stream(input, stream);
            read_capped(decoder, cap, "xz")
        }
        "lzma" => match props {
            // headered lzma: the 13-byte header carries the parameters
            None => {
                let mut writer = CappedVecWriter {
                    buf: Vec::new(),
                    cap,
                };
                let mut reader = input;
                // the memlimit bounds lzma-rs's internal dictionary buffer,
                // which otherwise balloons to the header's declared dict_size
                // before a single byte reaches the capped writer
                let options = lzma_rs::decompress::Options {
                    memlimit: Some(cap),
                    ..Default::default()
                };
                lzma_rs::lzma_decompress_with_options(&mut reader, &mut writer, &options).map_err(
                    |err| {
                        format!(
                            "failed to decompress as headered lzma: {err}; a headerless \
                             LZMA1 stream needs `props` with lc/lp/pb/dict_size (and \
                             optionally unpacked_size)"
                        )
                    },
                )?;
                Ok(writer.buf)
            }
            // headerless lzma: the caller supplies the parameters directly
            Some(props) => {
                // read each parameter with the classic lzma defaults
                let lc = prop_u32(props, "lc", 3)?;
                let lp = prop_u32(props, "lp", 0)?;
                let pb = prop_u32(props, "pb", 2)?;
                // clamp tiny dictionaries up to 0x1000 exactly like lzma-rs's
                // own headered path does; below that its circular buffer panics
                let dict_size = prop_u32(props, "dict_size", 1 << 24)?.max(0x1000);
                let unpacked_size = match props.get("unpacked_size") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(value) => Some(value.as_u64().ok_or_else(|| {
                        "lzma props field `unpacked_size` must be a non-negative integer".to_owned()
                    })?),
                };
                // validate the ranges here; the decoder would panic on them
                if lc > 8 || lp > 4 || pb > 4 {
                    return Err(
                        "lzma props out of range: lc must be <= 8 and lp/pb <= 4".to_owned()
                    );
                }
                // build the raw decoder with the memory bounded by the cap
                let params = lzma_rs::decompress::raw::LzmaParams::new(
                    lzma_rs::decompress::raw::LzmaProperties { lc, lp, pb },
                    dict_size,
                    unpacked_size,
                );
                let mut decoder = lzma_rs::decompress::raw::LzmaDecoder::new(params, Some(cap))
                    .map_err(|err| format!("invalid raw lzma parameters: {err}"))?;
                let mut writer = CappedVecWriter {
                    buf: Vec::new(),
                    cap,
                };
                let mut reader = input;
                decoder
                    .decompress(&mut reader, &mut writer)
                    .map_err(|err| format!("failed to decompress as raw lzma: {err}"))?;
                Ok(writer.buf)
            }
        },
        "lznt1" => lznt1::decompress(input, cap),
        "aplib" => aplib::decompress(input, cap),
        other => Err(format!(
            "unsupported decompression format '{other}'; supported: gzip, zlib, \
             deflate, lzma (headered, or headerless via `props`), xz, lznt1, \
             aplib (raw or AP32-headered). Raw \
             LZX is not exposed -- LZX only appears inside CAB folders, which \
             extract_file decompresses natively"
        )),
    }
}

/// Resolve the file a container/file tool should operate on
///
/// `None` -- or an empty string, matching the Rhai bindings' `""`-means-the-
/// original-sample convention -- targets the ORIGINAL sample; a name targets
/// that file in the session's extracted workspace, falling back to a sibling
/// sample of a multi-sample session by its file name (the workspace keeps
/// precedence so extracted names resolve exactly as before), with a miss
/// hinting at the known names.
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `target` - The extracted file name, or `None`/`""` for the original sample
#[instrument(skip_all, err(Debug))]
pub(crate) async fn resolve_target(
    context: &ToolContext,
    target: Option<&str>,
) -> Result<PathBuf, ToolExecutionError> {
    match target {
        // no target -- or the bindings' empty-string convention -- means the
        // original sample the run was built around
        None | Some("") => Ok(GhidraSession::from_context(context)?.binary().to_path_buf()),
        Some(name) => {
            // extracted files only exist when the run has a session store
            let store = store_from_context(context)?;
            // validate and resolve the name inside the extracted workspace
            let path = store
                .extracted_path(name)
                .await
                .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
            // a missing file gets a hint listing what actually exists
            if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                // fall back to a sibling sample addressed by its file name
                // (after the workspace miss, so extracted names keep winning)
                let session = GhidraSession::from_context(context)?;
                if let Some(extra) = session
                    .extra_binaries()
                    .iter()
                    .find(|extra| extra.file_name().is_some_and(|file| file == name))
                {
                    return Ok(extra.clone());
                }
                let known = store.list_extracted().await.unwrap_or_default();
                let mut hint = if known.is_empty() {
                    "nothing has been extracted yet (see list_container/extract_file)".to_owned()
                } else {
                    format!("known extracted files: {}", known.join(", "))
                };
                // name the sibling samples too so a near-miss self-corrects
                let siblings: Vec<String> = session
                    .extra_binaries()
                    .iter()
                    .filter_map(|extra| extra.file_name())
                    .map(|file| file.to_string_lossy().into_owned())
                    .collect();
                if !siblings.is_empty() {
                    hint = format!("{hint}; sibling samples: {}", siblings.join(", "));
                }
                return Err(ToolExecutionError::invalid_args(format!(
                    "no extracted file named '{name}' exists; {hint}"
                )));
            }
            Ok(path)
        }
    }
}

/// Pulls the session store out of a tool context with a targeted error
///
/// The container tools need the store for the extracted workspace, so a
/// store-less run (persistence disabled) gets an error explaining exactly
/// that instead of the generic wiring message.
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
fn store_from_context(
    context: &ToolContext,
) -> Result<std::sync::Arc<SessionStore>, ToolExecutionError> {
    // map an absent store onto the extraction-specific explanation
    SessionStore::try_from_context(context).ok_or_else(|| {
        ToolExecutionError::invalid_args(
            "extracted files need a session store, but this run has session \
             persistence disabled; only the original sample can be operated on"
                .to_owned(),
        )
    })
}

/// List a container's entries without blocking the runtime
///
/// # Arguments
///
/// * `path` - The file holding the container
/// * `offset` - The byte offset the container starts at within the file
/// * `limit` - The most entries to return
/// * `filter` - An optional case-insensitive substring filter on entry names
#[instrument(skip_all, err(Debug))]
async fn list_container_at(
    path: PathBuf,
    offset: u64,
    limit: usize,
    filter: Option<String>,
) -> Result<ContainerListing, String> {
    // container parsing is blocking io, so hop onto a blocking thread
    tokio::task::spawn_blocking(move || {
        list_container_sync(&path, offset, limit, filter.as_deref())
    })
    .await
    .map_err(|err| format!("the listing task failed: {err}"))?
}

/// Extract one container entry without blocking the runtime
///
/// # Arguments
///
/// * `path` - The file holding the container
/// * `offset` - The byte offset the container starts at within the file
/// * `entry` - The entry name to extract
/// * `password` - The ZIP password, when the archive needs one
/// * `dest_dir` - The directory to extract into
/// * `limits` - The size budget this extraction runs under
#[instrument(skip_all, err(Debug))]
async fn extract_entry(
    path: PathBuf,
    offset: u64,
    entry: String,
    password: Option<String>,
    dest_dir: PathBuf,
    limits: ExtractLimits,
) -> Result<ExtractedFile, String> {
    // extraction is blocking io, so hop onto a blocking thread
    tokio::task::spawn_blocking(move || {
        extract_entry_sync(
            &path,
            offset,
            &entry,
            password.as_deref(),
            &dest_dir,
            limits,
        )
    })
    .await
    .map_err(|err| format!("the extraction task failed: {err}"))?
}

/// Decompress one file window into the workspace without blocking the runtime
///
/// # Arguments
///
/// * `path` - The file to read the compressed window from
/// * `offset` - The file offset the compressed window starts at
/// * `length` - The compressed window's length
/// * `format` - The compression format name
/// * `props` - The raw-LZMA properties, for headerless LZMA1 streams
/// * `cap` - The most output bytes to produce
/// * `dest` - The workspace path the output lands at
/// * `name` - The output's workspace name
#[allow(clippy::too_many_arguments)]
#[instrument(skip_all, err(Debug))]
async fn decompress_window(
    path: PathBuf,
    offset: u64,
    length: usize,
    format: String,
    props: Option<serde_json::Value>,
    cap: usize,
    dest: PathBuf,
    name: String,
) -> Result<ExtractedFile, String> {
    // the read/decompress/write pass is blocking io, so hop onto a blocking thread
    tokio::task::spawn_blocking(move || {
        // read the compressed window off the disk
        let (input, _) = file::read_window_sync(&path, offset, length)?;
        if input.is_empty() {
            return Err(format!(
                "offset {offset:#x} is at or past the end of the file; nothing to \
                 decompress"
            ));
        }
        // decompress in memory under the cap
        let output = decompress_bytes(&input, &format, props.as_ref(), cap)?;
        // land the output atomically in the extracted workspace
        write_capped(
            output.as_slice(),
            &dest,
            cap as u64,
            &format!("decompressed output exceeded the {cap} byte cap"),
        )?;
        finish_extracted(name, &dest)
    })
    .await
    .map_err(|err| format!("the decompression task failed: {err}"))?
}

/// Lists the entries of a container file (ZIP, CAB, or OLE/CFB)
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `target` - The extracted file to list, or `None`/`""` for the original sample
/// * `offset` - The byte offset the container starts at within the file
/// * `limit` - The most entries to return
/// * `filter` - An optional case-insensitive substring filter on entry names
#[thorium_derive::thorpal_tool(
    group = "containers",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_list_container",
    rhai_doc = "list_container() | list_container(target: string) -> map -- list a container's entries",
    rhai_reference = "Lists the entries of a container file -- the ORIGINAL sample when
called with no arguments, or an extracted file when `target` names one (\"\"
also means the original sample).

Supported containers: ZIP, CAB, and OLE/CFB compound files (MSI stream names
are shown decoded). Single-stream formats come back with no entries and a
note: use `decompress` on gzip/xz, while bzip2 is recognized but not natively
decompressible.

Returns a map with: `format` (string), `kind` (string), `entries` (array of
maps with `name`, `size`, `method`, `encrypted`, `is_dir`, and optionally
`compressed_size`/`modified`), `total_entries` (int), and an optional
`truncated` note."
)]
#[rig::tool_macro(
    description = "List the entries of a container file: ZIP, CAB, or OLE/CFB compound file (MSI/legacy Office; MSI stream names are shown decoded). Purely static -- nothing is executed. Operates on the ORIGINAL sample by default, or on an extracted file when `target` names one, so nested containers (a CAB inside an MSI) list all the way down. `offset` reads a container embedded mid-file. Single-stream formats (gzip/xz) are reported with a note pointing at the decompress tool. `filter` is a case-insensitive name substring; `limit` defaults to 200 (max 500)."
)]
pub async fn list_container(
    #[rig(context)] context: &mut ToolContext,
    target: Option<String>,
    offset: Option<u64>,
    limit: Option<usize>,
    filter: Option<String>,
) -> Result<ContainerListing, ToolExecutionError> {
    // resolve the file to list: the original sample or an extracted child
    let path = resolve_target(context, target.as_deref()).await?;
    // clamp the knobs to usable ranges
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(DEFAULT_ENTRIES).clamp(1, MAX_ENTRIES);
    // parse the container off a blocking thread
    list_container_at(path, offset, limit, filter)
        .await
        .map_err(ToolExecutionError::invalid_args)
}

/// Extracts one container entry into the session's extracted workspace
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `entry` - The entry name to extract (CFB streams match decoded or raw)
/// * `target` - The container to extract from, or `None`/`""` for the original sample
/// * `offset` - The byte offset the container starts at within the file
/// * `password` - The ZIP password, when the archive needs one
#[thorium_derive::thorpal_tool(
    group = "containers",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_extract_file",
    rhai_doc = "extract_file(entry) | extract_file(target, entry) | extract_file(target, entry, password) -> map -- extract a container entry",
    rhai_reference = "Extracts one entry from a container (ZIP, CAB, or OLE/CFB) into the
session's extracted workspace, where every file tool can address it by name.

Overloads: `extract_file(entry: string)` extracts from the ORIGINAL sample;
`extract_file(target: string, entry: string)` extracts from an extracted file
(pass `\"\"` as `target` for the original sample); `extract_file(target,
entry, password)` additionally decrypts a ZIP entry (`target` is required in
this form -- use `\"\"` for the original sample, since the two-argument form
always reads as (target, entry)).

Returns a map with: `name` (the workspace name usable as every file tool's
`target`), `info` (the extracted file's size/hashes/entropy/format), and
`preview` (a head hexdump). Fails on size-cap breaches (nothing is written)."
)]
#[rig::tool_macro(
    description = "Extract one entry from a container (ZIP, CAB, or OLE/CFB compound file) into the session's extracted workspace. The extracted file is addressable by every file tool via its `target` argument (read_file, strings_file, file_info, list_container, extract_file, decompress), so nested containers unpack all the way down. Purely static -- the extracted file is written to disk, hashed, and identified, never executed. `entry` must match a listed name exactly (CFB streams match decoded or raw MSI names). `password` applies to encrypted ZIP entries only (malware archives conventionally use \"infected\"). Caps: 256 MiB per entry, 1 GiB total per session; a breach extracts nothing."
)]
pub async fn extract_file(
    #[rig(context)] context: &mut ToolContext,
    entry: String,
    target: Option<String>,
    offset: Option<u64>,
    password: Option<String>,
) -> Result<ExtractedFile, ToolExecutionError> {
    // resolve the container to extract from and the workspace to extract into
    let path = resolve_target(context, target.as_deref()).await?;
    let store = store_from_context(context)?;
    // serialize workspace writes so concurrent extractions can neither race
    // the same `.tmp` sibling nor leapfrog the total-budget check
    let _guard = store.lock_extracted().await;
    let dest_dir = store
        .extracted_dir()
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    // budget this extraction against the workspace's remaining space
    let used = store
        .extracted_usage()
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    let limits = ExtractLimits {
        entry: MAX_ENTRY_BYTES,
        total: MAX_TOTAL_EXTRACTED.saturating_sub(used),
    };
    // extract off a blocking thread, still under the workspace lock
    let offset = offset.unwrap_or(0);
    extract_entry(path, offset, entry, password, dest_dir, limits)
        .await
        .map_err(ToolExecutionError::invalid_args)
}

/// Extract Office macro content without blocking the runtime
///
/// # Arguments
///
/// * `path` - The file holding the compound file
/// * `offset` - The byte offset the compound file starts at within the file
/// * `dest_dir` - The directory extracted module source lands in
/// * `limits` - The size budget this extraction runs under
#[instrument(skip_all, err(Debug))]
async fn extract_macros_at(
    path: PathBuf,
    offset: u64,
    dest_dir: PathBuf,
    limits: ExtractLimits,
) -> Result<office::MacroReport, String> {
    // parsing and decompression are blocking io, so hop onto a blocking thread
    tokio::task::spawn_blocking(move || {
        office::extract_macros_sync(&path, offset, &dest_dir, limits)
    })
    .await
    .map_err(|err| format!("the macro extraction task failed: {err}"))?
}

/// Extracts VBA macro source and XLM indicators from an Office document
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `target` - The document to read, or `None`/`""` for the original sample
/// * `offset` - The byte offset the compound file starts at within the file
#[thorium_derive::thorpal_tool(
    group = "containers",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_extract_macros",
    rhai_doc = "extract_macros() | extract_macros(target: string) -> map -- extract VBA macro source and XLM indicators",
    rhai_reference = "Statically extracts Office macro content from an OLE compound file --
the ORIGINAL sample when called with no arguments, or an extracted file when
`target` names one (\"\" also means the original sample).

VBA projects are found by shape (any storage holding `dir` plus
`_VBA_PROJECT`), each module's source is MS-OVBA-decompressed and written
into the extracted workspace (addressable by every file tool via `target`),
and the `Workbook`/`Book` stream is scanned for Excel 4.0 (XLM) macro sheets
with their visibility. OOXML (docm/xlsm) is a ZIP: extract its
`vbaProject.bin` with extract_file first and run this on that.

Returns a map with: `vba` (array of projects with `storage`, optional
`project_name`/`project_properties`, and `modules` -- each with `name`,
`stream`, `workspace_name`, `source_bytes`, `preview`), `xlm` (an array of
scans, one per workbook stream found, root first -- each with `stream`,
`biff`, `total_sheets`, `macro_sheets`, optional `note`), and `notes`
(caveats and skips)."
)]
#[rig::tool_macro(
    description = "Statically extract Office macro content from an OLE compound file (legacy .doc/.xls/.ppt, or an extracted vbaProject.bin). VBA macro source is MS-OVBA-decompressed per module and written into the extracted workspace, where read_text and every file tool can address it by name; every Workbook/Book stream is scanned for Excel 4.0 (XLM) macro sheets (named, with hidden/very-hidden visibility; root streams first). Purely static -- nothing is executed. Operates on the ORIGINAL sample by default, or on an extracted file when `target` names one. OOXML (.docm/.xlsm) is a ZIP: extract word/vbaProject.bin or xl/vbaProject.bin with extract_file, then run this on it. A compound file with no macro content (e.g. an MSI) reports that explicitly."
)]
pub async fn extract_macros(
    #[rig(context)] context: &mut ToolContext,
    target: Option<String>,
    offset: Option<u64>,
) -> Result<office::MacroReport, ToolExecutionError> {
    // resolve the document to read and the workspace the source lands in
    let path = resolve_target(context, target.as_deref()).await?;
    let store = store_from_context(context)?;
    // serialize workspace writes so concurrent extractions can neither race
    // the same `.tmp` sibling nor leapfrog the total-budget check
    let _guard = store.lock_extracted().await;
    let dest_dir = store
        .extracted_dir()
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    // budget the extracted module source against the workspace's remaining space
    let used = store
        .extracted_usage()
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    let limits = ExtractLimits {
        entry: MAX_ENTRY_BYTES,
        total: MAX_TOTAL_EXTRACTED.saturating_sub(used),
    };
    // extract off a blocking thread, still under the workspace lock
    let offset = offset.unwrap_or(0);
    extract_macros_at(path, offset, dest_dir, limits)
        .await
        .map_err(ToolExecutionError::invalid_args)
}

/// Decompresses a raw stream window into the session's extracted workspace
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `format` - The compression format (gzip/zlib/deflate/lzma/xz/lznt1/aplib)
/// * `offset` - The file offset the compressed window starts at
/// * `length` - The compressed window's length (default/cap 64 MiB)
/// * `target` - The file to read from, or `None`/`""` for the original sample
/// * `name` - The output's workspace name (default `decompressed-<offset>-<format>`)
/// * `props` - The raw-LZMA properties, for headerless LZMA1 streams
#[thorium_derive::thorpal_tool(
    group = "containers",
    rhai_bind = "crate::thorpal::tools::scripts::bindings::bind_decompress",
    rhai_doc = "decompress(data: blob, format: string) | decompress(data, format, props: map) -> blob -- decompress a raw stream in memory",
    rhai_reference = "Decompresses a blob of raw compressed data in memory and returns the
decompressed blob (nothing is written to disk from scripts).

Formats: `\"gzip\"`, `\"zlib\"`, `\"deflate\"` (raw), `\"lzma\"` (headered
13-byte-header streams by default), `\"xz\"`, `\"lznt1\"` (Windows
RtlCompressBuffer), `\"aplib\"` (raw or AP32-headered aPLib, the packer
ransomware configs favor). For a headerless raw LZMA1 stream pass a `props`
map: `#{ lc: 3, lp: 0, pb: 2, dict_size: 0x10000, unpacked_size: 1234 }` (all
fields optional; classic defaults shown, `unpacked_size` may be omitted).

Output is capped at 64 MiB; a cap breach is a catchable error. NEVER
hand-write a decompressor for one of these standardized formats in Rhai --
scripts are for binary-specific decode logic (XOR loops, custom alphabets).
A decompressed payload a script wants analyzed should be saved into the
workspace with `save_bytes` and imported with import_child."
)]
#[rig::tool_macro(
    description = "Decompress a raw compressed stream from a file window into the session's extracted workspace, where every file tool can address the result via `target`. Formats: gzip, zlib, deflate (raw), lzma (headered by default; pass `props` {lc, lp, pb, dict_size, unpacked_size} for a headerless raw LZMA1 stream), xz, lznt1 (Windows RtlCompressBuffer), and aplib (raw or AP32-headered aPLib). Raw LZX is not exposed: LZX only appears inside CAB folders, which extract_file decompresses natively. Reads from the ORIGINAL sample by default, or from an extracted file when `target` names one; `offset`/`length` select the compressed window (input and output are capped at 64 MiB). `name` overrides the output's workspace name. An output that identifies as PE/ELF/Mach-O can be imported with import_child and analyzed like any program -- the standard carve: decode -> save with `name` -> import_child."
)]
pub async fn decompress(
    #[rig(context)] context: &mut ToolContext,
    format: String,
    offset: Option<u64>,
    length: Option<usize>,
    target: Option<String>,
    name: Option<String>,
    props: Option<serde_json::Value>,
) -> Result<ExtractedFile, ToolExecutionError> {
    // resolve the file to read the compressed window from
    let path = resolve_target(context, target.as_deref()).await?;
    // the output lands in the extracted workspace, so a store is required
    let store = store_from_context(context)?;
    // serialize workspace writes so concurrent decompressions can neither
    // race the same `.tmp` sibling nor leapfrog the total-budget check
    let _guard = store.lock_extracted().await;
    // resolve and validate the output's workspace name up front
    let offset = offset.unwrap_or(0);
    let name = name.unwrap_or_else(|| format!("decompressed-{offset:#x}-{format}"));
    let dest = store
        .extracted_path(&name)
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    // budget the output against the workspace's remaining space
    let used = store
        .extracted_usage()
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    let cap = usize::try_from(MAX_TOTAL_EXTRACTED.saturating_sub(used))
        .unwrap_or(usize::MAX)
        .min(MAX_DECOMPRESS_BYTES);
    // read, decompress, write, and identify off a blocking thread, still
    // under the workspace lock
    let length = length
        .unwrap_or(MAX_DECOMPRESS_INPUT)
        .min(MAX_DECOMPRESS_INPUT);
    decompress_window(path, offset, length, format, props, cap, dest, name)
        .await
        .map_err(ToolExecutionError::invalid_args)
}

/// A decoded PNG pixel dump landed in the extracted workspace
#[derive(Serialize)]
pub struct PixelDump {
    /// The image width in pixels
    pub width: u32,
    /// The image height in pixels
    pub height: u32,
    /// The PNG bit depth per channel
    pub bit_depth: u8,
    /// The PNG color type (0 gray, 2 rgb, 3 palette, 4 gray+alpha, 6 rgba)
    pub color_type: u8,
    /// The channels per pixel the color type implies
    pub channels: u8,
    /// The distinct row-filter types the image used, in first-seen order
    pub filters_seen: Vec<u8>,
    /// A note when the IDAT stream held fewer rows than the header promised
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
    /// The saved pixel dump as an extracted-workspace file
    #[serde(flatten)]
    pub file: ExtractedFile,
}

/// The facts a PNG's IHDR chunk declares about its pixel stream
#[derive(Debug)]
struct PngHeader {
    /// The image width in pixels
    width: u32,
    /// The image height in pixels
    height: u32,
    /// The bit depth per channel
    bit_depth: u8,
    /// The color type (0 gray, 2 rgb, 3 palette, 4 gray+alpha, 6 rgba)
    color_type: u8,
}

impl PngHeader {
    /// The channels per pixel this color type implies
    fn channels(&self) -> Result<u8, String> {
        // the five legal PNG color types and their channel counts
        match self.color_type {
            0 => Ok(1),
            2 => Ok(3),
            // palette indices are returned raw: the index stream is where a
            // pixel-stego payload lives, and no PLTE lookup can hide it
            3 => Ok(1),
            4 => Ok(2),
            6 => Ok(4),
            other => Err(format!("invalid PNG color type {other}")),
        }
    }

    /// The byte length of one unfiltered pixel row
    fn row_bytes(&self, channels: u8) -> u64 {
        // sub-byte depths pack multiple pixels per byte, so round bits up
        (u64::from(self.width) * u64::from(channels) * u64::from(self.bit_depth)).div_ceil(8)
    }
}

/// Parse a PNG's header and concatenated IDAT stream without validating CRCs
///
/// Chunk CRCs are deliberately ignored: a malware-built PNG is under no
/// obligation to checksum correctly, and the pixel payload is what matters.
///
/// # Arguments
///
/// * `input` - The raw bytes starting at the PNG signature
fn parse_png(input: &[u8]) -> Result<(PngHeader, Vec<u8>), String> {
    // the fixed 8-byte PNG signature gates the whole parse
    if input.len() < 8 || &input[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err("no PNG signature at the given offset".to_owned());
    }
    let mut pos = 8usize;
    let mut header: Option<PngHeader> = None;
    let mut idat: Vec<u8> = Vec::new();
    // walk the chunk stream: 4-byte length, 4-byte type, data, 4-byte crc
    while pos + 8 <= input.len() {
        let length = u32::from_be_bytes(input[pos..pos + 4].try_into().expect("4 bytes")) as usize;
        let kind = &input[pos + 4..pos + 8];
        let data_start = pos + 8;
        // a chunk running past the input ends the walk (truncated file)
        let Some(data_end) = data_start
            .checked_add(length)
            .filter(|end| *end <= input.len())
        else {
            break;
        };
        match kind {
            b"IHDR" => {
                // the header must be long enough for the fields we read
                if length < 13 {
                    return Err("the PNG IHDR chunk is shorter than 13 bytes".to_owned());
                }
                let data = &input[data_start..data_end];
                // compression and filter methods have exactly one legal value
                if data[10] != 0 || data[11] != 0 {
                    return Err("unsupported PNG compression/filter method".to_owned());
                }
                // Adam7 would need a deinterlacing pass this tool does not do
                if data[12] != 0 {
                    return Err(
                        "Adam7-interlaced PNG; pixel extraction supports non-interlaced \
                         images only"
                            .to_owned(),
                    );
                }
                header = Some(PngHeader {
                    width: u32::from_be_bytes(data[0..4].try_into().expect("4 bytes")),
                    height: u32::from_be_bytes(data[4..8].try_into().expect("4 bytes")),
                    bit_depth: data[8],
                    color_type: data[9],
                });
            }
            // the pixel stream may be split across any number of IDAT chunks
            b"IDAT" => idat.extend_from_slice(&input[data_start..data_end]),
            b"IEND" => break,
            // every other chunk (PLTE, tEXt, ...) is irrelevant to the dump
            _ => {}
        }
        // step past the data and the 4-byte crc we never validate
        pos = data_end.saturating_add(4);
    }
    let header = header.ok_or_else(|| "the PNG has no IHDR chunk".to_owned())?;
    if idat.is_empty() {
        return Err("the PNG has no IDAT pixel data".to_owned());
    }
    Ok((header, idat))
}

/// The Paeth predictor from the PNG specification
///
/// # Arguments
///
/// * `a` - The byte to the left of the current position
/// * `b` - The byte above the current position
/// * `c` - The byte above-left of the current position
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    // the predictor picks whichever neighbor is closest to a+b-c
    let p = i32::from(a) + i32::from(b) - i32::from(c);
    let (pa, pb, pc) = (
        (p - i32::from(a)).abs(),
        (p - i32::from(b)).abs(),
        (p - i32::from(c)).abs(),
    );
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Decode a PNG into its raw unfiltered pixel bytes
///
/// Returns the header, the pixel buffer, the distinct filter types seen, and
/// how many whole rows decoded (fewer than the header's height when the IDAT
/// stream is truncated — a malware-built image earns a note, not a failure).
///
/// # Arguments
///
/// * `input` - The raw bytes starting at the PNG signature
/// * `cap` - The most pixel bytes the decode may produce
fn decode_png_pixels(
    input: &[u8],
    cap: usize,
) -> Result<(PngHeader, Vec<u8>, Vec<u8>, u32), String> {
    // parse the structure first so size math has real dimensions
    let (header, idat) = parse_png(input)?;
    let channels = header.channels()?;
    if header.width == 0 || header.height == 0 {
        return Err("the PNG declares a zero dimension".to_owned());
    }
    let row_bytes = header.row_bytes(channels);
    // bound the output before inflating anything
    let total = row_bytes
        .checked_mul(u64::from(header.height))
        .ok_or_else(|| "the PNG dimensions overflow".to_owned())?;
    if row_bytes == 0 || total > cap as u64 {
        return Err(format!(
            "the decoded pixel stream would be {total} bytes, over the {cap} byte cap"
        ));
    }
    let row_bytes = row_bytes as usize;
    // inflate the concatenated IDAT stream under the filtered-size bound
    let filtered_cap = (row_bytes + 1) * header.height as usize;
    let raw = read_capped(
        flate2::read::ZlibDecoder::new(idat.as_slice()),
        filtered_cap,
        "zlib (PNG IDAT)",
    )?;
    // reverse the per-row filters into the output buffer
    let bpp = ((usize::from(channels) * usize::from(header.bit_depth)) / 8).max(1);
    let rows_available = raw.len() / (row_bytes + 1);
    let rows = rows_available.min(header.height as usize);
    let mut pixels = vec![0u8; rows * row_bytes];
    let mut filters_seen: Vec<u8> = Vec::new();
    for row in 0..rows {
        let src = &raw[row * (row_bytes + 1)..(row + 1) * (row_bytes + 1)];
        let filter = src[0];
        if !filters_seen.contains(&filter) {
            filters_seen.push(filter);
        }
        for i in 0..row_bytes {
            let x = src[1 + i];
            // the left/up/up-left neighbors are zero outside the image
            let a = if i >= bpp {
                pixels[row * row_bytes + i - bpp]
            } else {
                0
            };
            let b = if row > 0 {
                pixels[(row - 1) * row_bytes + i]
            } else {
                0
            };
            let c = if row > 0 && i >= bpp {
                pixels[(row - 1) * row_bytes + i - bpp]
            } else {
                0
            };
            // the five standard PNG row filters, reversed
            let value = match filter {
                0 => x,
                1 => x.wrapping_add(a),
                2 => x.wrapping_add(b),
                3 => x.wrapping_add(((u16::from(a) + u16::from(b)) / 2) as u8),
                4 => x.wrapping_add(paeth(a, b, c)),
                other => return Err(format!("row {row} uses unknown PNG filter type {other}")),
            };
            pixels[row * row_bytes + i] = value;
        }
    }
    if rows == 0 {
        return Err("the IDAT stream holds less than one pixel row".to_owned());
    }
    Ok((header, pixels, filters_seen, rows as u32))
}

/// Reads, decodes, and lands a PNG pixel dump off a blocking thread
///
/// # Arguments
///
/// * `path` - The file to read the PNG from
/// * `offset` - The byte offset the PNG signature starts at
/// * `length` - The most input bytes to read
/// * `cap` - The most pixel bytes the decode may produce
/// * `dest` - The workspace path the pixel dump lands at
/// * `name` - The workspace name the dump is addressable by
async fn png_pixels_window(
    path: PathBuf,
    offset: u64,
    length: usize,
    cap: usize,
    dest: PathBuf,
    name: String,
) -> Result<PixelDump, String> {
    // the read/decode/write pass is blocking io, so hop onto a blocking thread
    tokio::task::spawn_blocking(move || {
        // read the PNG window off the disk
        let (input, _) = file::read_window_sync(&path, offset, length)?;
        if input.is_empty() {
            return Err(format!(
                "offset {offset:#x} is at or past the end of the file; nothing to decode"
            ));
        }
        // decode the pixel stream in memory under the cap
        let (header, pixels, filters_seen, rows) = decode_png_pixels(&input, cap)?;
        // land the dump atomically in the extracted workspace
        write_capped(
            pixels.as_slice(),
            &dest,
            cap as u64,
            &format!("the pixel dump exceeded the {cap} byte cap"),
        )?;
        let channels = header.channels()?;
        // a short-rowed stream is reported, never silently padded
        let truncated = (rows < header.height).then(|| {
            format!(
                "the IDAT stream held only {rows} of the declared {} rows",
                header.height
            )
        });
        let file = finish_extracted(name, &dest)?;
        Ok(PixelDump {
            width: header.width,
            height: header.height,
            bit_depth: header.bit_depth,
            color_type: header.color_type,
            channels,
            filters_seen,
            truncated,
            file,
        })
    })
    .await
    .map_err(|err| format!("the pixel decode task failed: {err}"))?
}

/// Decodes a PNG's raw pixel stream into the extracted workspace
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
/// * `offset` - The byte offset the PNG signature starts at
/// * `target` - The file holding the PNG, or `None`/`""` for the original sample
/// * `name` - The workspace name for the pixel dump
#[thorium_derive::thorpal_tool(group = "containers")]
#[rig::tool_macro(
    description = "Decode an embedded PNG into its RAW PIXEL BYTES in the extracted workspace, where every file tool can scan them. Image formats hide payloads behind the codec: byte-level scans of the FILE cannot see data carried in the decoded pixel stream (one dropper family stores its full XOR-encoded stage-2 in a decoy image's pixels behind a self-validating CRC32 header), so an oversized or out-of-place image in a suspected decoy or dropper deserves a pixel dump followed by the standard sweeps -- entropy_map, grep_bytes/strings, header scans, carve-and-import. Purely static: parses IHDR/IDAT (chunk CRCs are NOT validated -- malware images need not checksum), inflates, and reverses the five row filters; palette images return raw indices (no PLTE lookup); Adam7-interlaced images are not supported. Operates on the ORIGINAL sample by default, or an extracted file via `target`; `offset` locates a PNG embedded mid-file (point it at the 89 50 4E 47 signature). Returns the dimensions, bit depth, color type, channels, filters seen, and the saved dump's workspace name/identity; a truncated IDAT stream decodes the whole rows it holds and says so. Caps: 64 MiB in, 64 MiB of pixels out."
)]
pub async fn png_pixels(
    #[rig(context)] context: &mut ToolContext,
    offset: Option<u64>,
    target: Option<String>,
    name: Option<String>,
) -> Result<PixelDump, ToolExecutionError> {
    // resolve the file to read the PNG from
    let path = resolve_target(context, target.as_deref()).await?;
    // the output lands in the extracted workspace, so a store is required
    let store = store_from_context(context)?;
    // serialize workspace writes so concurrent dumps can neither race the
    // same `.tmp` sibling nor leapfrog the total-budget check
    let _guard = store.lock_extracted().await;
    // resolve and validate the output's workspace name up front
    let offset = offset.unwrap_or(0);
    let name = name.unwrap_or_else(|| format!("png-pixels-{offset:#x}"));
    let dest = store
        .extracted_path(&name)
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    // budget the output against the workspace's remaining space
    let used = store
        .extracted_usage()
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    let cap = usize::try_from(MAX_TOTAL_EXTRACTED.saturating_sub(used))
        .unwrap_or(usize::MAX)
        .min(MAX_DECOMPRESS_BYTES);
    // read, decode, write, and identify off a blocking thread, still under
    // the workspace lock
    png_pixels_window(path, offset, MAX_DECOMPRESS_INPUT, cap, dest, name)
        .await
        .map_err(ToolExecutionError::invalid_args)
}

#[cfg(test)]
mod tests;
