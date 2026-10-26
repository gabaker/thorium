//! Tests for the container listing, extraction, and decompression tools.

use std::io::Cursor;
use std::path::Path;

use super::*;
use crate::thorpal::tools::ghidra::GhidraSettings;

/// A ZipCrypto-encrypted ZIP fixture holding one stored entry
///
/// `payload.bin` contains `ENCRYPTED PAYLOAD` encrypted with the password
/// `infected`, assembled by hand (zip 4's writer does not expose the legacy
/// ZipCrypto cipher) and verified round-trip against Python's `zipfile`.
const ENCRYPTED_ZIP: [u8; 149] = [
    0x50, 0x4b, 0x03, 0x04, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x00, 0x5b, 0x18,
    0x85, 0xeb, 0x1d, 0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x70, 0x61,
    0x79, 0x6c, 0x6f, 0x61, 0x64, 0x2e, 0x62, 0x69, 0x6e, 0x03, 0x47, 0x3a, 0x0e, 0xdb, 0xcf, 0x73,
    0x51, 0xb0, 0x58, 0x8e, 0xdb, 0x4f, 0xf2, 0x13, 0x44, 0xe6, 0x0f, 0xb1, 0x58, 0x75, 0x04, 0x59,
    0x9c, 0x1e, 0x1a, 0x9f, 0x11, 0x53, 0x50, 0x4b, 0x01, 0x02, 0x14, 0x00, 0x14, 0x00, 0x01, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x21, 0x00, 0x5b, 0x18, 0x85, 0xeb, 0x1d, 0x00, 0x00, 0x00, 0x11, 0x00,
    0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x70, 0x61, 0x79, 0x6c, 0x6f, 0x61, 0x64, 0x2e, 0x62, 0x69, 0x6e, 0x50,
    0x4b, 0x05, 0x06, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x39, 0x00, 0x00, 0x00, 0x46,
    0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Builds a tool context whose session store and sample point into a tempdir
///
/// Mirrors the file tests' context plus a scratch [`SessionStore`], exactly
/// like `ThorPal::tool_context` wires a persisted run.
///
/// # Arguments
///
/// * `dir` - The scratch dir the sessions root lives under
/// * `binary` - The path to the sample file the context points at
async fn context(dir: &Path, binary: &Path) -> ToolContext {
    // create a store so extractions have a workspace to land in
    let store = SessionStore::create(
        dir.join("sessions"),
        None,
        binary.to_path_buf(),
        "sample".to_owned(),
        false,
        Vec::new(),
    )
    .await
    .expect("store should create");
    // a session with no Ghidra install still carries the binary path
    let ghidra = GhidraSession::new(GhidraSettings::from_binary(binary, "projects", None));
    // seed the context exactly like ThorPal::tool_context does
    let mut context = ToolContext::new();
    context.insert(store);
    context.insert(ghidra);
    context
}

/// Writes a sample file into a scratch dir and returns its path
///
/// # Arguments
///
/// * `dir` - The scratch dir to write into
/// * `bytes` - The sample's contents
fn write_sample(dir: &Path, bytes: &[u8]) -> PathBuf {
    // materialize the fake sample the tools read
    let path = dir.join("sample.bin");
    std::fs::write(&path, bytes).expect("sample should write");
    path
}

/// Builds an in-memory ZIP archive from (name, data) pairs
///
/// # Arguments
///
/// * `entries` - The entry names and contents to store
fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    // write every entry with the default (deflate) options
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for (name, data) in entries {
        writer
            .start_file(name.to_string(), options)
            .expect("entry should start");
        writer.write_all(data).expect("entry should write");
    }
    writer.finish().expect("zip should finish").into_inner()
}

/// Builds an in-memory MSZIP-compressed CAB with one file
///
/// # Arguments
///
/// * `name` - The stored file's name
/// * `data` - The stored file's contents
fn build_cab(name: &str, data: &[u8]) -> Vec<u8> {
    // declare the folder and file, then stream the contents through
    let mut builder = cab::CabinetBuilder::new();
    builder
        .add_folder(cab::CompressionType::MsZip)
        .add_file(name);
    let mut writer = builder
        .build(Cursor::new(Vec::new()))
        .expect("cab should start");
    while let Some(mut file) = writer.next_file().expect("next file") {
        file.write_all(data).expect("file should write");
    }
    writer.finish().expect("cab should finish").into_inner()
}

/// Encodes a plain name into the MSI stream-name encoding
///
/// The in-test inverse of [`decode_msi_name`]: pairs of characters pack into
/// `0x3800 + a + (b << 6)` and a trailing odd character into `0x4800 + a`.
///
/// # Arguments
///
/// * `name` - The plain name to encode (charset characters only)
fn encode_msi_name(name: &str) -> String {
    // map a charset character back to its 6-bit index
    let index = |c: u8| {
        MSI_CHARSET
            .iter()
            .position(|&x| x == c)
            .expect("charset character") as u32
    };
    let bytes = name.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    // pack pairs of characters into the two-character range
    while i + 1 < bytes.len() {
        let packed = 0x3800 + index(bytes[i]) + (index(bytes[i + 1]) << 6);
        out.push(char::from_u32(packed).expect("valid code point"));
        i += 2;
    }
    // a trailing odd character lands in the one-character range
    if i < bytes.len() {
        out.push(char::from_u32(0x4800 + index(bytes[i])).expect("valid code point"));
    }
    out
}

/// Builds an in-memory CFB compound file with one MSI-encoded stream
///
/// # Arguments
///
/// * `decoded` - The stream's plain (decoded) name
/// * `data` - The stream's contents
fn build_cfb(decoded: &str, data: &[u8]) -> Vec<u8> {
    // create the compound file and one stream under the encoded name
    let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("cfb should create");
    let encoded = encode_msi_name(decoded);
    let mut stream = comp
        .create_stream(format!("/{encoded}"))
        .expect("stream should create");
    stream.write_all(data).expect("stream should write");
    drop(stream);
    comp.flush().expect("cfb should flush");
    comp.into_inner().into_inner()
}

/// Gzips a buffer in memory
///
/// # Arguments
///
/// * `data` - The bytes to compress
fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    // run the bytes through flate2's gzip encoder
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data).expect("gzip should write");
    encoder.finish().expect("gzip should finish")
}

/// A ZIP lists its entries and extracts one with real hashes.
#[tokio::test]
async fn zip_lists_and_extracts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = build_zip(&[("payload.bin", b"abcd"), ("docs/readme.txt", b"hello")]);
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    // the listing names both entries with their sizes and methods
    let listing = list_container(&mut context, None, None, None, None)
        .await
        .expect("the listing should succeed");
    assert_eq!(listing.kind, "zip");
    assert_eq!(listing.total_entries, 2);
    assert_eq!(listing.entries[0].name, "payload.bin");
    assert_eq!(listing.entries[0].size, 4);
    assert!(!listing.entries[0].encrypted);
    assert!(listing.truncated.is_none());
    // the filter narrows the listing case-insensitively
    let filtered = list_container(&mut context, None, None, None, Some("README".to_owned()))
        .await
        .expect("the filtered listing should succeed");
    assert_eq!(filtered.total_entries, 1);
    assert_eq!(filtered.entries[0].name, "docs/readme.txt");
    // extraction lands the entry in the workspace with the known md5 of "abcd"
    let extracted = extract_file(&mut context, "payload.bin".to_owned(), None, None, None)
        .await
        .expect("the extraction should succeed");
    assert_eq!(extracted.name, "payload.bin");
    assert_eq!(extracted.info.size, 4);
    assert_eq!(extracted.info.md5, "e2fc714c4727ee9395f324cd2e7f331f");
    assert!(extracted.preview.contains("|abcd|"));
    // the subdirectory entry sanitizes its separators into the flat workspace
    let nested = extract_file(&mut context, "docs/readme.txt".to_owned(), None, None, None)
        .await
        .expect("the nested extraction should succeed");
    assert_eq!(nested.name, "docs_readme.txt");
    // an unknown entry errors listing the real candidates
    let error = extract_file(&mut context, "ghost.bin".to_owned(), None, None, None)
        .await
        .expect_err("an unknown entry should fail");
    let rendered = error.to_string();
    assert!(
        rendered.contains("payload.bin"),
        "no candidates: {rendered}"
    );
    assert!(
        rendered.contains("docs/readme.txt"),
        "no candidates: {rendered}"
    );
}

/// A CAB lists its MSZIP entries and extracts one intact.
#[tokio::test]
async fn cab_lists_and_extracts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = build_cab("dropper.dll", b"CAB PAYLOAD BYTES");
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    // the listing names the entry with the folder's compression method
    let listing = list_container(&mut context, None, None, None, None)
        .await
        .expect("the listing should succeed");
    assert_eq!(listing.kind, "cab");
    assert_eq!(listing.total_entries, 1);
    assert_eq!(listing.entries[0].name, "dropper.dll");
    assert_eq!(listing.entries[0].size, 17);
    assert_eq!(listing.entries[0].method, "MsZip");
    // extraction decompresses the MSZIP folder back to the original bytes
    let extracted = extract_file(&mut context, "dropper.dll".to_owned(), None, None, None)
        .await
        .expect("the extraction should succeed");
    assert_eq!(extracted.name, "dropper.dll");
    assert_eq!(extracted.info.size, 17);
    assert!(extracted.preview.contains("|CAB PAYLOAD BYTE"));
    // a password on a CAB is refused rather than silently ignored
    let error = extract_file(
        &mut context,
        "dropper.dll".to_owned(),
        None,
        None,
        Some("infected".to_owned()),
    )
    .await
    .expect_err("a CAB password should be refused");
    assert!(error.to_string().contains("ZIP"));
}

/// A CFB lists MSI-decoded stream names and extracts by either form.
#[tokio::test]
async fn cfb_lists_decoded_names_and_extracts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = build_cfb("payload.cab", b"INNER STREAM");
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    // the listing shows the decoded name, not the raw encoded one
    let listing = list_container(&mut context, None, None, None, None)
        .await
        .expect("the listing should succeed");
    assert_eq!(listing.kind, "cfb");
    assert_eq!(listing.total_entries, 1);
    assert_eq!(listing.entries[0].name, "payload.cab");
    assert_eq!(listing.entries[0].size, 12);
    assert!(!listing.entries[0].is_dir);
    // extraction matches the decoded name straight off the listing
    let extracted = extract_file(&mut context, "payload.cab".to_owned(), None, None, None)
        .await
        .expect("the extraction should succeed");
    assert_eq!(extracted.name, "payload.cab");
    assert_eq!(extracted.info.size, 12);
    assert!(extracted.preview.contains("|INNER STREAM|"));
    // the raw encoded name extracts too
    let encoded = encode_msi_name("payload.cab");
    let raw = extract_file(&mut context, encoded, None, None, None)
        .await
        .expect("the raw-name extraction should succeed");
    assert_eq!(raw.info.size, 12);
}

/// Serializes one dir-stream record as `id, size, data`
///
/// # Arguments
///
/// * `id` - The record id
/// * `data` - The record payload
fn dir_record(id: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// Builds an in-memory CFB maldoc: a VBA project plus an XLM macro sheet
///
/// The layout mirrors a Word maldoc (`Macros/VBA/...` with a compressed
/// `dir` and module stream, a plaintext `PROJECT`) with a `Workbook` stream
/// carrying one BIFF8 Excel 4.0 macro BOUNDSHEET, so one fixture exercises
/// both extraction paths.
///
/// # Arguments
///
/// * `source` - The VBA module source to compress into the module stream
fn build_maldoc(source: &[u8]) -> Vec<u8> {
    // create the compound file with the Word-style storage layout
    let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("cfb should create");
    comp.create_storage("/Macros").expect("storage");
    comp.create_storage("/Macros/VBA").expect("storage");
    // the dir stream declares the project and its one module, compressed
    let mut dir = Vec::new();
    dir.extend_from_slice(&dir_record(0x0004, b"VBAProject"));
    dir.extend_from_slice(&dir_record(0x0019, b"Module1"));
    dir.extend_from_slice(&dir_record(0x001A, b"Module1"));
    dir.extend_from_slice(&dir_record(0x0031, &0u32.to_le_bytes()));
    dir.extend_from_slice(&dir_record(0x0010, b""));
    for (name, data) in [
        ("/Macros/VBA/dir", office::compress_all_literals(&dir)),
        ("/Macros/VBA/_VBA_PROJECT", b"\xcc\x61".to_vec()),
        ("/Macros/VBA/Module1", office::compress_all_literals(source)),
        (
            "/Macros/PROJECT",
            b"ID=\"{00}\"\r\nModule=Module1\r\n".to_vec(),
        ),
    ] {
        let mut stream = comp.create_stream(name).expect("stream should create");
        stream.write_all(&data).expect("stream should write");
    }
    // a Workbook stream with a BIFF8 BOF and one very-hidden macro sheet
    let mut workbook = Vec::new();
    workbook.extend_from_slice(&[0x09, 0x08, 0x04, 0x00, 0x00, 0x06, 0x05, 0x00]);
    let name = b"MacroSheet";
    workbook.extend_from_slice(&[0x85, 0x00, (8 + name.len()) as u8, 0x00]);
    workbook.extend_from_slice(&[0, 0, 0, 0, 0x02, 0x01, name.len() as u8, 0x00]);
    workbook.extend_from_slice(name);
    let mut stream = comp
        .create_stream("/Workbook")
        .expect("stream should create");
    stream.write_all(&workbook).expect("stream should write");
    drop(stream);
    comp.flush().expect("cfb should flush");
    comp.into_inner().into_inner()
}

/// A maldoc's VBA source extracts decompressed and its XLM sheet is flagged.
#[tokio::test]
async fn maldoc_macros_extract_and_flag_xlm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source =
        b"Attribute VB_Name = \"Module1\"\r\nSub AutoOpen()\r\n  Shell \"cmd\"\r\nEnd Sub\r\n";
    let bytes = build_maldoc(source);
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    let report = extract_macros(&mut context, None, None)
        .await
        .expect("the macro extraction should succeed");
    // the VBA project is found by shape with its declared name
    assert_eq!(report.vba.len(), 1);
    let project = &report.vba[0];
    assert_eq!(project.project_name.as_deref(), Some("VBAProject"));
    assert!(project.storage.contains("VBA"));
    // the PROJECT properties stream rides along as a bounded preview
    assert!(
        project
            .project_properties
            .as_deref()
            .is_some_and(|properties| properties.contains("Module=Module1"))
    );
    // the module's source decompressed, previewed, and landed in the workspace
    assert_eq!(project.modules.len(), 1);
    let module = &project.modules[0];
    assert_eq!(module.name, "Module1");
    assert_eq!(module.source_bytes, source.len() as u64);
    assert!(module.preview.contains("AutoOpen"));
    let workspace = module.workspace_name.as_deref().expect("source written");
    // the written file holds the full decompressed source, byte for byte
    let session = std::fs::read_dir(dir.path().join("sessions"))
        .expect("sessions root")
        .next()
        .expect("one session")
        .expect("entry")
        .path();
    let written = std::fs::read(session.join("extracted").join(workspace))
        .expect("the extracted source should exist");
    assert_eq!(written, source);
    // the XLM scan names the very-hidden macro sheet
    assert_eq!(report.xlm.len(), 1, "the workbook should be scanned");
    let xlm = &report.xlm[0];
    assert_eq!(xlm.stream, "/Workbook");
    assert_eq!(xlm.biff, "biff8");
    assert_eq!(xlm.total_sheets, 1);
    assert_eq!(xlm.macro_sheets.len(), 1);
    assert_eq!(xlm.macro_sheets[0].name, "MacroSheet");
    assert_eq!(xlm.macro_sheets[0].visibility, "very-hidden");
}

/// Unicode stream names resolve and a lying dir record cannot hide the rest.
#[tokio::test]
async fn maldoc_unicode_names_and_hostile_dir_still_extract() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = b"Attribute VB_Name = \"\xD0\x9C\xD0\xBE\xD0\xB4\xD1\x83\xD0\xBB\xD1\x8C1\"\r\nSub AutoOpen()\r\nEnd Sub\r\n";
    // the module's name in cp1251 bytes (mangles under a lossy UTF-8 decode)
    // and in the authoritative UTF-16 spelling the 0x0047/0x0032 records carry
    let mbcs = [0xCCu8, 0xEE, 0xE4, 0xF3, 0xEB, 0xFC, b'1'];
    let utf16: Vec<u8> = "Модуль1"
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    // one good module, then a record lying about its size (an evasion that
    // used to abort the whole extraction)
    let mut records = Vec::new();
    records.extend_from_slice(&dir_record(0x0019, &mbcs));
    records.extend_from_slice(&dir_record(0x0047, &utf16));
    records.extend_from_slice(&dir_record(0x001A, &mbcs));
    records.extend_from_slice(&dir_record(0x0032, &utf16));
    records.extend_from_slice(&dir_record(0x0031, &0u32.to_le_bytes()));
    records.extend_from_slice(&0x0019u16.to_le_bytes());
    records.extend_from_slice(&4000u32.to_le_bytes());
    records.extend_from_slice(b"short");
    // assemble the compound file: the VBA project plus a macro workbook
    let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("cfb should create");
    comp.create_storage("/Macros").expect("storage");
    comp.create_storage("/Macros/VBA").expect("storage");
    for (name, data) in [
        ("/Macros/VBA/dir", office::compress_all_literals(&records)),
        ("/Macros/VBA/_VBA_PROJECT", b"\xcc\x61".to_vec()),
        ("/Macros/VBA/Модуль1", office::compress_all_literals(source)),
    ] {
        let mut stream = comp.create_stream(name).expect("stream should create");
        stream.write_all(&data).expect("stream should write");
    }
    let mut workbook = Vec::new();
    workbook.extend_from_slice(&[0x09, 0x08, 0x04, 0x00, 0x00, 0x06, 0x05, 0x00]);
    workbook.extend_from_slice(&[0x85, 0x00, 0x0E, 0x00]);
    workbook.extend_from_slice(&[0, 0, 0, 0, 0x01, 0x01, 6, 0x00]);
    workbook.extend_from_slice(b"Macro1");
    let mut stream = comp
        .create_stream("/Workbook")
        .expect("stream should create");
    stream.write_all(&workbook).expect("stream should write");
    drop(stream);
    comp.flush().expect("cfb should flush");
    let bytes = comp.into_inner().into_inner();
    // run the tool: the unicode-named module must extract and the lying
    // record must surface as a caveat without hiding the XLM scan
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    let report = extract_macros(&mut context, None, None)
        .await
        .expect("a hostile dir stream must not abort the extraction");
    assert_eq!(report.vba.len(), 1);
    let module = &report.vba[0].modules[0];
    assert_eq!(module.name, "Модуль1");
    assert_eq!(module.source_bytes, source.len() as u64);
    assert!(module.workspace_name.is_some());
    assert!(module.preview.contains("AutoOpen"));
    // the lie is surfaced, not fatal
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("declares 4000 bytes"))
    );
    // the XLM scan still ran despite the damaged VBA project
    assert_eq!(report.xlm.len(), 1, "the workbook must still be scanned");
    let xlm = &report.xlm[0];
    assert_eq!(xlm.macro_sheets.len(), 1);
    assert_eq!(xlm.macro_sheets[0].name, "Macro1");
}

/// Case-flipped trees still detect, and a decoy Book cannot shadow /Workbook.
#[tokio::test]
async fn maldoc_case_flips_and_decoy_workbooks_cannot_evade() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = b"Attribute VB_Name = \"Mod1\"\r\nSub AutoOpen()\r\nEnd Sub\r\n";
    // the dir stream declares the module under a case-flipped stream name
    let mut records = Vec::new();
    records.extend_from_slice(&dir_record(0x0019, b"Mod1"));
    records.extend_from_slice(&dir_record(0x001A, b"MOD1"));
    records.extend_from_slice(&dir_record(0x0031, &0u32.to_le_bytes()));
    records.extend_from_slice(&dir_record(0x0010, b""));
    // a case-flipped project tree: Office resolves CFB names
    // case-insensitively, so this document loads and runs normally
    let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("cfb should create");
    comp.create_storage("/MACROS").expect("storage");
    comp.create_storage("/MACROS/vba").expect("storage");
    for (name, data) in [
        ("/MACROS/vba/DIR", office::compress_all_literals(&records)),
        ("/MACROS/vba/_vba_project", b"\xcc\x61".to_vec()),
        ("/MACROS/vba/mod1", office::compress_all_literals(source)),
    ] {
        let mut stream = comp.create_stream(name).expect("stream should create");
        stream.write_all(&data).expect("stream should write");
    }
    // a clean decoy /Book (shortlex-first) must not shadow the real
    // /Workbook carrying the very-hidden macro sheet
    let mut decoy = Vec::new();
    decoy.extend_from_slice(&[0x09, 0x08, 0x04, 0x00, 0x00, 0x06, 0x05, 0x00]);
    decoy.extend_from_slice(&[0x85, 0x00, 0x0E, 0x00]);
    decoy.extend_from_slice(&[0, 0, 0, 0, 0x00, 0x00, 6, 0x00]);
    decoy.extend_from_slice(b"Sheet1");
    let mut real = Vec::new();
    real.extend_from_slice(&[0x09, 0x08, 0x04, 0x00, 0x00, 0x06, 0x05, 0x00]);
    real.extend_from_slice(&[0x85, 0x00, 0x0E, 0x00]);
    real.extend_from_slice(&[0, 0, 0, 0, 0x02, 0x01, 6, 0x00]);
    real.extend_from_slice(b"Macro1");
    for (name, data) in [("/Book", decoy), ("/Workbook", real)] {
        let mut stream = comp.create_stream(name).expect("stream should create");
        stream.write_all(&data).expect("stream should write");
    }
    comp.flush().expect("cfb should flush");
    let bytes = comp.into_inner().into_inner();
    let path = write_sample(dir.path(), &bytes);
    let mut flipped_context = context(dir.path(), &path).await;
    let report = extract_macros(&mut flipped_context, None, None)
        .await
        .expect("the case-flipped maldoc should extract");
    // the case-flipped project was found and its module extracted
    assert_eq!(report.vba.len(), 1);
    assert_eq!(report.vba[0].modules.len(), 1);
    assert_eq!(report.vba[0].modules[0].source_bytes, source.len() as u64);
    // BOTH workbook streams were scanned; the real one carries the macros
    assert_eq!(report.xlm.len(), 2);
    let real_scan = report
        .xlm
        .iter()
        .find(|scan| scan.stream == "/Workbook")
        .expect("the real workbook must be scanned");
    assert_eq!(real_scan.macro_sheets.len(), 1);
    assert_eq!(real_scan.macro_sheets[0].name, "Macro1");
    assert_eq!(real_scan.macro_sheets[0].visibility, "very-hidden");
    let decoy_scan = report
        .xlm
        .iter()
        .find(|scan| scan.stream == "/Book")
        .expect("the decoy is scanned too");
    assert!(decoy_scan.macro_sheets.is_empty());
    // the Unicode class: CFB uppercases U+0131 (dotless i) to I, so a dir
    // stream spelled "dır" IS "dir" to Office -- detection must agree
    let dir = tempfile::tempdir().expect("tempdir");
    let mut records = Vec::new();
    records.extend_from_slice(&dir_record(0x0019, b"Mod1"));
    records.extend_from_slice(&dir_record(0x001A, b"Mod1"));
    records.extend_from_slice(&dir_record(0x0031, &0u32.to_le_bytes()));
    records.extend_from_slice(&dir_record(0x0010, b""));
    let mut comp = cfb::CompoundFile::create(Cursor::new(Vec::new())).expect("cfb should create");
    comp.create_storage("/M").expect("storage");
    comp.create_storage("/M/VBA").expect("storage");
    for (name, data) in [
        ("/M/VBA/dır", office::compress_all_literals(&records)),
        ("/M/VBA/_VBA_PROJECT", b"\xcc\x61".to_vec()),
        ("/M/VBA/Mod1", office::compress_all_literals(source)),
    ] {
        let mut stream = comp.create_stream(name).expect("stream should create");
        stream.write_all(&data).expect("stream should write");
    }
    comp.flush().expect("cfb should flush");
    let bytes = comp.into_inner().into_inner();
    let path = write_sample(dir.path(), &bytes);
    let mut unicode_context = context(dir.path(), &path).await;
    let report = extract_macros(&mut unicode_context, None, None)
        .await
        .expect("the dotless-i spelling should extract");
    assert_eq!(report.vba.len(), 1, "the d\u{131}r project must be found");
    assert_eq!(report.vba[0].modules.len(), 1);
}

/// Macro-less compound files and ZIPs answer with guidance, not findings.
#[tokio::test]
async fn macro_extraction_handles_non_maldocs() {
    let dir = tempfile::tempdir().expect("tempdir");
    // a plain single-stream CFB (an MSI shape) reports no macro content
    let bytes = build_cfb("payload.cab", b"INNER STREAM");
    let path = write_sample(dir.path(), &bytes);
    let mut msi_context = context(dir.path(), &path).await;
    let report = extract_macros(&mut msi_context, None, None)
        .await
        .expect("a macro-less compound file is an answer, not an error");
    assert!(report.vba.is_empty());
    assert!(report.xlm.is_empty());
    assert!(
        report
            .notes
            .first()
            .is_some_and(|note| note.contains("no detectable macro content"))
    );
    // a ZIP (OOXML) routes the caller through extract_file for vbaProject.bin
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = build_zip(&[("word/vbaProject.bin", b"not really")]);
    let path = write_sample(dir.path(), &bytes);
    let mut zip_context = context(dir.path(), &path).await;
    let err = extract_macros(&mut zip_context, None, None)
        .await
        .expect_err("a ZIP must redirect to extract_file");
    assert!(err.to_string().contains("vbaProject.bin"));
}

/// A container nested in a container unpacks all the way down.
#[tokio::test]
async fn nested_containers_unpack_recursively() {
    let dir = tempfile::tempdir().expect("tempdir");
    // a zip holding a gzip member holding the real payload
    let inner = gzip_bytes(b"FINAL PAYLOAD");
    let bytes = build_zip(&[("inner.gz", &inner)]);
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    // extract the gzip member out of the zip
    let extracted = extract_file(&mut context, "inner.gz".to_owned(), None, None, None)
        .await
        .expect("the extraction should succeed");
    assert_eq!(extracted.name, "inner.gz");
    assert_eq!(extracted.info.format, "gzip compressed data");
    // listing the extracted member reports the single-stream note
    let listing = list_container(&mut context, Some("inner.gz".to_owned()), None, None, None)
        .await
        .expect("the nested listing should succeed");
    assert_eq!(listing.kind, "gzip");
    assert!(listing.entries.is_empty());
    assert!(
        listing
            .truncated
            .as_deref()
            .unwrap_or("")
            .contains("decompress"),
        "missing note: {:?}",
        listing.truncated
    );
    // decompress the extracted member into the workspace and read it back
    let inflated = decompress(
        &mut context,
        "gzip".to_owned(),
        None,
        None,
        Some("inner.gz".to_owned()),
        Some("final.bin".to_owned()),
        None,
    )
    .await
    .expect("the decompression should succeed");
    assert_eq!(inflated.name, "final.bin");
    assert_eq!(inflated.info.size, 13);
    assert!(inflated.preview.contains("|FINAL PAYLOAD|"));
    // the inflated child is addressable like any extracted file
    let store = SessionStore::from_context(&context).expect("store");
    let final_path = store.extracted_path("final.bin").await.expect("path");
    assert_eq!(
        tokio::fs::read(&final_path).await.expect("read"),
        b"FINAL PAYLOAD"
    );
}

/// A container embedded mid-file parses through the offset view.
#[tokio::test]
async fn embedded_containers_parse_at_an_offset() {
    let dir = tempfile::tempdir().expect("tempdir");
    // prepend garbage so the zip starts at offset 16 (EOCD scans need End seeks)
    let mut bytes = b"GARBAGEPREFIX!!!".to_vec();
    bytes.extend_from_slice(&build_zip(&[("hidden.bin", b"EMBEDDED")]));
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    // the listing only works through the offset
    let listing = list_container(&mut context, None, Some(16), None, None)
        .await
        .expect("the offset listing should succeed");
    assert_eq!(listing.entries[0].name, "hidden.bin");
    // and so does the extraction
    let extracted = extract_file(&mut context, "hidden.bin".to_owned(), None, Some(16), None)
        .await
        .expect("the offset extraction should succeed");
    assert_eq!(extracted.info.size, 8);
    assert!(extracted.preview.contains("|EMBEDDED|"));
}

/// Encrypted ZIP entries need the right password and fail cleanly otherwise.
#[tokio::test]
async fn encrypted_zip_entries_enforce_the_password() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_sample(dir.path(), &ENCRYPTED_ZIP);
    let mut context = context(dir.path(), &path).await;
    // the listing marks the entry encrypted without any password
    let listing = list_container(&mut context, None, None, None, None)
        .await
        .expect("the listing should succeed");
    assert!(listing.entries[0].encrypted);
    // a missing password errors with the "infected" convention hinted
    let error = extract_file(&mut context, "payload.bin".to_owned(), None, None, None)
        .await
        .expect_err("a missing password should fail");
    assert!(error.to_string().contains("infected"));
    // a wrong password errors cleanly
    let error = extract_file(
        &mut context,
        "payload.bin".to_owned(),
        None,
        None,
        Some("wrong".to_owned()),
    )
    .await
    .expect_err("a wrong password should fail");
    assert!(error.to_string().contains("password"));
    // the right password decrypts the stored bytes exactly
    let extracted = extract_file(
        &mut context,
        "payload.bin".to_owned(),
        None,
        None,
        Some("infected".to_owned()),
    )
    .await
    .expect("the decryption should succeed");
    assert_eq!(extracted.info.size, 17);
    // the hexdump wraps at 16 bytes, so the 17-byte payload spans two rows
    assert!(extracted.preview.contains("|ENCRYPTED PAYLOA|"));
    assert!(extracted.preview.contains("|D|"));
}

/// Declared sizes past a cap error before any bytes are written.
#[test]
fn declared_sizes_past_the_cap_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = build_zip(&[("big.bin", b"0123456789")]);
    let path = write_sample(dir.path(), &bytes);
    let dest = dir.path().join("out");
    std::fs::create_dir(&dest).expect("dest should create");
    // a per-entry cap below the declared size refuses up front
    let error = extract_entry_sync(
        &path,
        0,
        "big.bin",
        None,
        &dest,
        ExtractLimits {
            entry: 4,
            total: 1000,
        },
    )
    .expect_err("the entry cap should refuse");
    assert!(error.contains("per-entry cap"), "unexpected error: {error}");
    // a total budget below the declared size names the workspace budget
    let error = extract_entry_sync(
        &path,
        0,
        "big.bin",
        None,
        &dest,
        ExtractLimits {
            entry: 1000,
            total: 4,
        },
    )
    .expect_err("the total cap should refuse");
    assert!(
        error.contains("workspace budget"),
        "unexpected error: {error}"
    );
    // nothing landed in the destination either way
    assert_eq!(std::fs::read_dir(&dest).expect("read dir").count(), 0);
}

/// A stream longer than its declared size stops at the cap, tmp cleaned.
#[test]
fn lying_streams_stop_at_the_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("liar.bin");
    // a reader that never ends stands in for a lying declared size
    let endless = std::io::repeat(0x41);
    let error = write_capped(endless, &dest, 1024, "stopped at the cap")
        .expect_err("the cap should stop the stream");
    assert_eq!(error, "stopped at the cap");
    // neither the final file nor the torn tmp sibling survives
    assert!(!dest.exists());
    assert!(!dir.path().join("liar.bin.tmp").exists());
    // an honest stream lands atomically with its byte count reported
    let written = write_capped(&b"abcd"[..], &dest, 1024, "unused").expect("write");
    assert_eq!(written, 4);
    assert_eq!(std::fs::read(&dest).expect("read"), b"abcd");
}

/// The decompression formats round-trip through real encoders.
#[test]
fn decompress_round_trips() {
    let data: Vec<u8> = b"the quick brown fox jumps over the lazy dog".repeat(20);
    // gzip
    let out =
        decompress_bytes(&gzip_bytes(&data), "gzip", None, MAX_DECOMPRESS_BYTES).expect("gzip");
    assert_eq!(out, data);
    // zlib
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&data).expect("zlib write");
    let zlib = encoder.finish().expect("zlib finish");
    let out = decompress_bytes(&zlib, "zlib", None, MAX_DECOMPRESS_BYTES).expect("zlib");
    assert_eq!(out, data);
    // raw deflate
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&data).expect("deflate write");
    let deflate = encoder.finish().expect("deflate finish");
    let out = decompress_bytes(&deflate, "deflate", None, MAX_DECOMPRESS_BYTES).expect("deflate");
    assert_eq!(out, data);
    // headered lzma
    let mut lzma = Vec::new();
    lzma_rs::lzma_compress(&mut &data[..], &mut lzma).expect("lzma compress");
    let out = decompress_bytes(&lzma, "lzma", None, MAX_DECOMPRESS_BYTES).expect("lzma");
    assert_eq!(out, data);
    // xz
    let mut xz = Vec::new();
    lzma_rs::xz_compress(&mut &data[..], &mut xz).expect("xz compress");
    let out = decompress_bytes(&xz, "xz", None, MAX_DECOMPRESS_BYTES).expect("xz");
    assert_eq!(out, data);
    // aplib: a hand-built literal stream dispatches through the new arm
    // (first raw literal 'H', literal code 0 + 'I', then the end marker)
    let aplib = [b'H', 0b0110_0000, b'I', 0x00];
    let out = decompress_bytes(&aplib, "aplib", None, MAX_DECOMPRESS_BYTES).expect("aplib");
    assert_eq!(out, b"HI");
    // the format name is case-insensitive
    let out = decompress_bytes(&gzip_bytes(&data), "GZIP", None, MAX_DECOMPRESS_BYTES)
        .expect("uppercase gzip");
    assert_eq!(out, data);
    // an unknown format errors naming the supported set and the LZX exclusion
    let error = decompress_bytes(&data, "lzx", None, MAX_DECOMPRESS_BYTES)
        .expect_err("lzx should be refused");
    assert!(error.contains("LZX"), "unexpected error: {error}");
    assert!(error.contains("aplib"), "unexpected error: {error}");
}

/// A headerless raw LZMA1 stream decompresses through explicit props.
#[test]
fn headerless_lzma_decompresses_with_props() {
    let data: Vec<u8> = b"KEENTROLL RAW LZMA1 CASE ".repeat(40);
    // compress headered, then strip the 13-byte header to make a raw stream
    let mut lzma = Vec::new();
    lzma_rs::lzma_compress(&mut &data[..], &mut lzma).expect("lzma compress");
    let dict_size = u32::from_le_bytes(lzma[1..5].try_into().expect("dict size"));
    let raw = &lzma[13..];
    // the raw stream fails without props, with the props hint in the error
    let error = decompress_bytes(raw, "lzma", None, MAX_DECOMPRESS_BYTES)
        .expect_err("a raw stream should fail headered");
    assert!(error.contains("props"), "unexpected error: {error}");
    // with the parameters supplied it decompresses exactly
    let props = serde_json::json!({
        "lc": 3, "lp": 0, "pb": 2,
        "dict_size": dict_size,
        "unpacked_size": data.len(),
    });
    let out = decompress_bytes(raw, "lzma", Some(&props), MAX_DECOMPRESS_BYTES).expect("raw lzma");
    assert_eq!(out, data);
    // out-of-range props error instead of panicking the decoder
    let bad = serde_json::json!({ "lc": 9 });
    let error = decompress_bytes(raw, "lzma", Some(&bad), MAX_DECOMPRESS_BYTES)
        .expect_err("bad props should be refused");
    assert!(error.contains("lc"), "unexpected error: {error}");
    // props on a non-lzma format are refused
    let error = decompress_bytes(raw, "gzip", Some(&props), MAX_DECOMPRESS_BYTES)
        .expect_err("props on gzip should be refused");
    assert!(error.contains("lzma"), "unexpected error: {error}");
}

/// Decompression bombs error at the cap instead of ballooning memory.
///
/// The lzma path pins the memlimit wired into lzma-rs (whose internal
/// dictionary buffer would otherwise grow to the full output before the
/// capped writer saw a byte) and the xz path pins liblzma's pull-based
/// streaming through the capped reader.
#[test]
fn decompression_bombs_error_at_the_cap() {
    // a highly compressible payload well past the 1 MiB test cap
    let data = vec![0u8; 8 << 20];
    let cap = 1 << 20;
    // a headered lzma bomb trips lzma-rs's memory limit at the cap
    let mut lzma = Vec::new();
    lzma_rs::lzma_compress(&mut &data[..], &mut lzma).expect("lzma compress");
    let error =
        decompress_bytes(&lzma, "lzma", None, cap).expect_err("the lzma bomb should trip the cap");
    assert!(
        error.contains("memory limit") || error.contains("cap"),
        "unexpected error: {error}"
    );
    // a raw lzma bomb trips the same limit through the props path
    let props = serde_json::json!({ "unpacked_size": data.len() });
    let error = decompress_bytes(&lzma[13..], "lzma", Some(&props), cap)
        .expect_err("the raw lzma bomb should trip the cap");
    assert!(
        error.contains("memory limit") || error.contains("cap"),
        "unexpected error: {error}"
    );
    // an xz bomb streams through liblzma and trips the capped reader
    let mut xz = Vec::new();
    lzma_rs::xz_compress(&mut &data[..], &mut xz).expect("xz compress");
    let error =
        decompress_bytes(&xz, "xz", None, cap).expect_err("the xz bomb should trip the cap");
    assert!(error.contains("cap"), "unexpected error: {error}");
}

/// A tiny raw-lzma dict_size is clamped instead of panicking the decoder.
#[test]
fn tiny_lzma_dict_sizes_are_clamped() {
    let data: Vec<u8> = b"CLAMPED DICT ".repeat(10);
    // compress headered, then strip the header to make a raw stream
    let mut lzma = Vec::new();
    lzma_rs::lzma_compress(&mut &data[..], &mut lzma).expect("lzma compress");
    // dict_size 0 would panic lzma-rs's circular buffer without the clamp
    let props = serde_json::json!({ "dict_size": 0, "unpacked_size": data.len() });
    let out = decompress_bytes(&lzma[13..], "lzma", Some(&props), MAX_DECOMPRESS_BYTES)
        .expect("the clamped dict should decode");
    assert_eq!(out, data);
}

/// Concatenated gzip members decode fully, not just the first.
#[test]
fn concatenated_gzip_members_decode_fully() {
    // two independent gzip members back to back
    let mut packed = gzip_bytes(b"FIRST MEMBER ");
    packed.extend_from_slice(&gzip_bytes(b"SECOND MEMBER"));
    let out = decompress_bytes(&packed, "gzip", None, MAX_DECOMPRESS_BYTES)
        .expect("the concatenated stream should decode");
    assert_eq!(out, b"FIRST MEMBER SECOND MEMBER");
}

/// LZNT1 decompresses through the shared dispatch and caps enforce.
#[test]
fn lznt1_and_caps_through_the_dispatch() {
    // the hand-assembled spec vector from the lznt1 module's tests
    let compressed = [0x03, 0xB0, 0x02, 0x41, 0x0C, 0x00];
    let out = decompress_bytes(&compressed, "lznt1", None, MAX_DECOMPRESS_BYTES).expect("lznt1");
    assert_eq!(out, vec![b'A'; 16]);
    // the output cap trips on every format, gzip shown here
    let big = gzip_bytes(&vec![0u8; 4096]);
    let error = decompress_bytes(&big, "gzip", None, 100).expect_err("the cap should trip");
    assert!(error.contains("100"), "unexpected error: {error}");
}

/// The total-workspace budget blocks extraction once spent.
#[tokio::test]
async fn total_budget_blocks_extraction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = build_zip(&[("payload.bin", b"0123456789")]);
    let path = write_sample(dir.path(), &bytes);
    let dest = dir.path().join("out");
    std::fs::create_dir(&dest).expect("dest should create");
    // a spent budget refuses even a tiny entry
    let error = extract_entry_sync(
        &path,
        0,
        "payload.bin",
        None,
        &dest,
        ExtractLimits {
            entry: MAX_ENTRY_BYTES,
            total: 2,
        },
    )
    .expect_err("the spent budget should refuse");
    assert!(
        error.contains("workspace budget"),
        "unexpected error: {error}"
    );
}

/// Colliding sanitized names uniquify instead of silently overwriting.
#[tokio::test]
async fn colliding_names_uniquify() {
    let dir = tempfile::tempdir().expect("tempdir");
    // two distinct entries whose names sanitize to the same workspace name
    let bytes = build_zip(&[
        ("dir/payload.bin", b"FIRST"),
        ("dir_payload.bin", b"SECOND"),
    ]);
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    // the first extraction takes the plain sanitized name
    let first = extract_file(&mut context, "dir/payload.bin".to_owned(), None, None, None)
        .await
        .expect("the first extraction should succeed");
    assert_eq!(first.name, "dir_payload.bin");
    // the second gets a counter spliced in ahead of the extension
    let second = extract_file(&mut context, "dir_payload.bin".to_owned(), None, None, None)
        .await
        .expect("the second extraction should succeed");
    assert_eq!(second.name, "dir_payload-2.bin");
    // both files survive with their own contents
    let store = SessionStore::from_context(&context).expect("store");
    let first_path = store.extracted_path(&first.name).await.expect("path");
    let second_path = store.extracted_path(&second.name).await.expect("path");
    assert_eq!(tokio::fs::read(&first_path).await.expect("read"), b"FIRST");
    assert_eq!(
        tokio::fs::read(&second_path).await.expect("read"),
        b"SECOND"
    );
    // re-extracting an entry keeps the earlier copy too
    let again = extract_file(&mut context, "dir_payload.bin".to_owned(), None, None, None)
        .await
        .expect("the re-extraction should succeed");
    assert_eq!(again.name, "dir_payload-3.bin");
}

/// Concurrent extractions serialize on the workspace lock and both land.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_extractions_serialize() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = build_zip(&[("payload.bin", b"RACE PAYLOAD")]);
    let path = write_sample(dir.path(), &bytes);
    // two contexts sharing one store, like two sub-agents in one run
    let store = SessionStore::create(
        dir.path().join("sessions"),
        None,
        path.clone(),
        "sample".to_owned(),
        false,
        Vec::new(),
    )
    .await
    .expect("store should create");
    let mut contexts = Vec::new();
    for _ in 0..2 {
        let ghidra = GhidraSession::new(GhidraSettings::from_binary(&path, "projects", None));
        let mut context = ToolContext::new();
        context.insert(std::sync::Arc::clone(&store));
        context.insert(ghidra);
        contexts.push(context);
    }
    let mut second = contexts.pop().expect("context");
    let mut first = contexts.pop().expect("context");
    // race two extractions of the same entry against each other
    let (a, b) = tokio::join!(
        extract_file(&mut first, "payload.bin".to_owned(), None, None, None),
        extract_file(&mut second, "payload.bin".to_owned(), None, None, None),
    );
    let a = a.expect("the first racer should succeed");
    let b = b.expect("the second racer should succeed");
    // the lock serialized them onto distinct names with no torn leftovers
    // (sorted: '-' orders ahead of '.')
    let mut names = vec![a.name, b.name];
    names.sort();
    assert_eq!(
        names,
        ["payload-2.bin", "payload.bin"].map(str::to_owned).to_vec()
    );
    assert_eq!(
        store.list_extracted().await.expect("list"),
        ["payload-2.bin", "payload.bin"]
    );
}

/// A failed finalize rename cleans its tmp sibling too.
#[test]
fn failed_finalize_cleans_the_tmp() {
    let dir = tempfile::tempdir().expect("tempdir");
    // a directory squatting on the destination makes the rename fail
    let dest = dir.path().join("blocked.bin");
    std::fs::create_dir(&dest).expect("squatter should create");
    let error = write_capped(&b"abcd"[..], &dest, 1024, "unused")
        .expect_err("the squatted rename should fail");
    assert!(error.contains("finalize"), "unexpected error: {error}");
    // the tmp sibling was cleaned up instead of lingering forever
    assert!(!dir.path().join("blocked.bin.tmp").exists());
}

/// An empty-string target means the original sample, like the Rhai docs say.
#[tokio::test]
async fn empty_target_means_the_original_sample() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = build_zip(&[("payload.bin", b"abcd")]);
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    // "" routes to the original sample instead of a name-length error
    let listing = list_container(&mut context, Some(String::new()), None, None, None)
        .await
        .expect("the empty target should list the sample");
    assert_eq!(listing.total_entries, 1);
    assert_eq!(listing.entries[0].name, "payload.bin");
}

/// Extraction without a session store errors with the persistence hint.
#[tokio::test]
async fn extraction_without_a_store_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bytes = build_zip(&[("payload.bin", b"abcd")]);
    let path = write_sample(dir.path(), &bytes);
    // a context with only the Ghidra session, like a store-less run
    let ghidra = GhidraSession::new(GhidraSettings::from_binary(&path, "projects", None));
    let mut context = ToolContext::new();
    context.insert(ghidra);
    // listing still works (it needs no workspace)
    let listing = list_container(&mut context, None, None, None, None)
        .await
        .expect("the listing should succeed");
    assert_eq!(listing.total_entries, 1);
    // extraction refuses with the persistence explanation
    let error = extract_file(&mut context, "payload.bin".to_owned(), None, None, None)
        .await
        .expect_err("a store-less extraction should fail");
    assert!(error.to_string().contains("persistence"));
}

/// Non-containers and unsupported archives answer honestly.
#[tokio::test]
async fn non_containers_answer_honestly() {
    let dir = tempfile::tempdir().expect("tempdir");
    // a PE is not a container at all
    let path = write_sample(dir.path(), b"MZ\x90\x00PAYLOAD");
    let mut context = context(dir.path(), &path).await;
    let error = list_container(&mut context, None, None, None, None)
        .await
        .expect_err("a PE should not list");
    assert!(error.to_string().contains("MZ executable"));
    // a 7z archive is recognized but unsupported
    let seven = dir.path().join("seven.7z");
    std::fs::write(&seven, b"7z\xbc\xaf\x27\x1cRESTOFHEADER").expect("write");
    let error =
        list_container_sync(&seven, 0, DEFAULT_ENTRIES, None).expect_err("7z should be refused");
    assert!(error.contains("7-Zip"), "unexpected error: {error}");
}

/// MSI stream names decode exactly and non-encoded names pass through.
#[test]
fn msi_names_decode() {
    // even- and odd-length names survive an encode/decode round trip
    for name in ["payload.cab", "Binary.stage2", "a", "ab", "A1_z."] {
        let encoded = encode_msi_name(name);
        assert_eq!(
            decode_msi_name(&encoded).as_deref(),
            Some(name),
            "{name} should round-trip"
        );
    }
    // the system-stream marker decodes to the ! prefix
    let mut encoded = String::from('\u{4840}');
    encoded.push_str(&encode_msi_name("Validation"));
    assert_eq!(decode_msi_name(&encoded).as_deref(), Some("!Validation"));
    // ordinary names are not MSI-encoded and pass through as None
    assert_eq!(decode_msi_name("SummaryInformation"), None);
    assert_eq!(decode_msi_name(""), None);
}

/// Hostile and awkward entry names sanitize into safe workspace names.
#[test]
fn entry_names_sanitize() {
    // traversal collapses into a plain safe name
    assert_eq!(sanitize_entry_name("../../etc/passwd", 0), "etc_passwd");
    // separators map to underscores without collapsing the real name
    assert_eq!(
        sanitize_entry_name("dir/sub/payload.exe", 0),
        "dir_sub_payload.exe"
    );
    // leading dots are trimmed so names can never hide
    assert_eq!(sanitize_entry_name(".hidden", 0), "hidden");
    // the tmp suffix is kept clear of the atomic writer's convention
    assert_eq!(sanitize_entry_name("x.tmp", 0), "x.tmp_");
    // a name with nothing usable falls back to the indexed form
    assert_eq!(sanitize_entry_name("///", 7), "entry-7");
    assert_eq!(sanitize_entry_name("", 3), "entry-3");
    // overlong names cap at the store's limit
    assert_eq!(sanitize_entry_name(&"a".repeat(300), 0).len(), 128);
}

/// The offset reader translates Start, End, and Current seeks.
#[test]
fn offset_reader_translates_seeks() {
    // a 16-byte prefix ahead of an 8-byte region
    let mut bytes = vec![0xEE; 16];
    bytes.extend_from_slice(b"ABCDEFGH");
    let mut reader = OffsetReader::new(Cursor::new(bytes), 16).expect("the view should open");
    // reads start at the region's first byte
    let mut buf = [0u8; 2];
    reader.read_exact(&mut buf).expect("read");
    assert_eq!(&buf, b"AB");
    // an End seek lands relative to the region's end
    assert_eq!(reader.seek(SeekFrom::End(-2)).expect("seek"), 6);
    reader.read_exact(&mut buf).expect("read");
    assert_eq!(&buf, b"GH");
    // a Current seek moves relative to the virtual position
    reader.seek(SeekFrom::Start(4)).expect("seek");
    assert_eq!(reader.seek(SeekFrom::Current(-2)).expect("seek"), 2);
    reader.read_exact(&mut buf).expect("read");
    assert_eq!(&buf, b"CD");
    // a seek before the region start is refused
    assert!(reader.seek(SeekFrom::Current(-100)).is_err());
    // a hostile target that would overflow the base errors instead of panicking
    assert!(reader.seek(SeekFrom::Start(u64::MAX)).is_err());
    // an offset past the end of the file never opens
    assert!(OffsetReader::new(Cursor::new(vec![0u8; 4]), 10).is_err());
}

/// A sibling sample resolves by file name, after the workspace misses.
#[tokio::test]
async fn resolve_target_falls_back_to_siblings() {
    let dir = tempfile::tempdir().expect("tempdir");
    // the primary sample plus a sibling file on disk
    let binary = write_sample(dir.path(), b"primary");
    let sibling = dir.path().join("payload.bin");
    std::fs::write(&sibling, b"sibling bytes").expect("sibling should write");
    // a context whose Ghidra settings carry the sibling, as a multi-sample
    // session's bootstrap would
    let store = SessionStore::create(
        dir.path().join("sessions"),
        None,
        binary.clone(),
        "sample".to_owned(),
        false,
        Vec::new(),
    )
    .await
    .expect("store should create");
    let mut settings = GhidraSettings::from_binary(&binary, "projects", None);
    settings.extra_binaries = vec![sibling.clone()];
    let ghidra = GhidraSession::new(settings);
    let mut context = ToolContext::new();
    context.insert(store);
    context.insert(ghidra);
    // the sibling's file name resolves to its on-disk path
    let path = resolve_target(&context, Some("payload.bin"))
        .await
        .expect("sibling should resolve");
    assert_eq!(path, sibling);
    // an extracted file with the same name would win: carve one and re-resolve
    let dest = {
        let store = context
            .get::<std::sync::Arc<SessionStore>>()
            .expect("store");
        let dest = store
            .extracted_path("payload.bin")
            .await
            .expect("workspace path");
        std::fs::write(&dest, b"carved").expect("carved should write");
        dest
    };
    let path = resolve_target(&context, Some("payload.bin"))
        .await
        .expect("workspace should win");
    assert_eq!(path, dest);
    // a miss still errors, now naming the sibling samples in the hint
    let error = resolve_target(&context, Some("nope.bin"))
        .await
        .expect_err("a miss should error");
    let rendered = error.to_string();
    assert!(
        rendered.contains("sibling samples: payload.bin"),
        "hint must name the siblings: {rendered}"
    );
}

/// Builds a PNG from pre-filtered rows (filter byte + row data per row)
///
/// Chunk CRCs are zeroed: the parser deliberately never validates them.
///
/// # Arguments
///
/// * `width` - The declared image width in pixels
/// * `height` - The declared image height in pixels
/// * `bit_depth` - The declared bit depth per channel
/// * `color_type` - The declared color type
/// * `filtered` - The raw filtered scanline stream to deflate into IDAT
fn build_png(width: u32, height: u32, bit_depth: u8, color_type: u8, filtered: &[u8]) -> Vec<u8> {
    // deflate the filtered scanlines into a zlib stream
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, filtered).expect("deflate");
    let idat = encoder.finish().expect("deflate finish");
    // assemble signature + IHDR + IDAT + IEND with zeroed CRCs
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[bit_depth, color_type, 0, 0, 0]);
    for (kind, data) in [
        (&b"IHDR"[..], ihdr.as_slice()),
        (b"IDAT", idat.as_slice()),
        (b"IEND", &[]),
    ] {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&[0, 0, 0, 0]);
    }
    png
}

/// All five row filters reverse to the exact pixel bytes
#[test]
fn png_pixels_reverse_every_filter() {
    // a 4x5 grayscale image: known pixels, one row per filter type
    let pixels: Vec<Vec<u8>> = vec![
        vec![10, 20, 30, 40],
        vec![15, 25, 35, 45],
        vec![17, 28, 39, 50],
        vec![20, 30, 40, 50],
        vec![25, 35, 45, 55],
    ];
    // forward-filter each row per the spec so the decoder must reverse it
    let mut filtered = Vec::new();
    for (row, data) in pixels.iter().enumerate() {
        let filter = row as u8;
        filtered.push(filter);
        for (i, &x) in data.iter().enumerate() {
            let a = if i >= 1 { data[i - 1] } else { 0 };
            let b = if row > 0 { pixels[row - 1][i] } else { 0 };
            let c = if row > 0 && i >= 1 {
                pixels[row - 1][i - 1]
            } else {
                0
            };
            let encoded = match filter {
                0 => x,
                1 => x.wrapping_sub(a),
                2 => x.wrapping_sub(b),
                3 => x.wrapping_sub(((u16::from(a) + u16::from(b)) / 2) as u8),
                4 => x.wrapping_sub(super::paeth(a, b, c)),
                _ => unreachable!(),
            };
            filtered.push(encoded);
        }
    }
    let png = build_png(4, 5, 8, 0, &filtered);
    let (header, decoded, filters_seen, rows) =
        super::decode_png_pixels(&png, 1 << 20).expect("decode");
    // every row decodes back to the exact pixel bytes
    let flat: Vec<u8> = pixels.into_iter().flatten().collect();
    assert_eq!(decoded, flat);
    assert_eq!((header.width, header.height, rows), (4, 5, 5));
    assert_eq!(filters_seen, vec![0, 1, 2, 3, 4]);
}

/// A payload hidden in the pixel stream surfaces in the dump
#[test]
fn png_pixels_surface_a_pixel_stream_payload() {
    // an RGB image whose pixel bytes carry a marker byte-scans cannot see
    // in the compressed file
    let payload = b"PIXEL-CARGO-MARKER:0xDEADBEEF";
    let mut flat = payload.to_vec();
    flat.resize(6 * 3 * 4, 0xAA);
    // filter 0 rows: 6 pixels wide, 4 rows, 3 channels
    let mut filtered = Vec::new();
    for row in flat.chunks(18) {
        filtered.push(0u8);
        filtered.extend_from_slice(row);
    }
    let png = build_png(6, 4, 8, 2, &filtered);
    // the compressed file does not contain the marker in the clear
    assert!(!png.windows(payload.len()).any(|w| w == payload));
    let (_, decoded, _, _) = super::decode_png_pixels(&png, 1 << 20).expect("decode");
    // the pixel dump does
    assert!(decoded.windows(payload.len()).any(|w| w == payload));
}

/// Interlaced and non-PNG inputs are refused with instructive errors
#[test]
fn png_pixels_reject_interlace_and_non_png() {
    // flipping the interlace byte on a valid image names Adam7
    let mut png = build_png(2, 1, 8, 0, &[0, 1, 2]);
    let ihdr_interlace = 8 + 8 + 12;
    png[ihdr_interlace] = 1;
    let err = super::decode_png_pixels(&png, 1 << 20).expect_err("interlace");
    assert!(err.contains("Adam7"), "{err}");
    // a non-PNG input names the missing signature
    let err = super::decode_png_pixels(b"MZ not a png", 1 << 20).expect_err("not png");
    assert!(err.contains("signature"), "{err}");
}
