//! Tests for the native decode/transform/search/hash tools.

use super::*;
use crate::thorpal::tools::ghidra::{GhidraSession, GhidraSettings};

/// Builds a tool context whose session store and sample point into a tempdir
///
/// Mirrors the container tests' context: a scratch [`SessionStore`] plus an
/// install-less Ghidra session carrying the binary path, exactly like
/// `ThorPal::tool_context` wires a persisted run.
///
/// # Arguments
///
/// * `dir` - The scratch dir the sessions root lives under
/// * `binary` - The path to the sample file the context points at
async fn context(dir: &Path, binary: &Path) -> ToolContext {
    // create a store so saved outputs have a workspace to land in
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

/// Renders bytes as lowercase hex for inline `data` arguments
///
/// # Arguments
///
/// * `bytes` - The bytes to render
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The SwitchBack GT-8 resolver: mul_rolling(0xC00018BE, 0x21) resolves the
/// whole Hell's Gate set from the BUILT-IN dictionary.
#[tokio::test]
async fn switchback_gt8_hashes_resolve() {
    // the eight Nt* names the Hell's Gate loader resolves
    let gate = [
        "NtOpenProcess",
        "NtAllocateVirtualMemory",
        "NtWriteVirtualMemory",
        "NtProtectVirtualMemory",
        "NtFreeVirtualMemory",
        "NtCreateThreadEx",
        "NtClose",
        "NtQueryInformationProcess",
    ];
    // hash each name with an independent in-test reference loop
    let reference = |name: &str| -> u32 {
        let mut hash = 0xC000_18BEu32;
        for byte in name.bytes() {
            hash = hash.wrapping_mul(0x21).wrapping_add(u32::from(byte));
        }
        hash
    };
    let targets: Vec<u32> = gate.iter().map(|name| reference(name)).collect();
    // the sync core resolves all eight against the built-in dictionary
    let result = api_hash_match_sync(
        &targets,
        "mul_rolling",
        Some(0xC000_18BE),
        Some(0x21),
        None,
        &NameVariants::default(),
        &[],
    )
    .expect("the match should run");
    assert!(
        result.unmatched.is_empty(),
        "unmatched: {:?}",
        result.unmatched
    );
    for name in gate {
        assert!(
            result.matched.iter().any(|hit| hit.name == name),
            "{name} did not resolve"
        );
    }
    // the SwitchBack doc constants are those hashes verbatim; pin them
    for (hash, name) in [
        (0x6AAF_4EE5u32, "NtAllocateVirtualMemory"),
        (0x0F44_31CB, "NtWriteVirtualMemory"),
        (0x3826_76E9, "NtCreateThreadEx"),
    ] {
        assert_eq!(
            reference(name),
            hash,
            "{name} drifted from the doc constant"
        );
    }
    // the tool path coerces hex-string targets and resolves the same set
    let mut context = ToolContext::new();
    let string_targets: Vec<serde_json::Value> = targets
        .iter()
        .map(|hash| serde_json::json!(format!("{hash:#x}")))
        .collect();
    let result = api_hash_match(
        &mut context,
        string_targets,
        Some("mul_rolling".to_owned()),
        Some(0xC000_18BE),
        Some(0x21),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("the tool should match");
    assert!(result.unmatched.is_empty());
    assert!(result.matched.iter().any(|hit| hit.name == "NtOpenProcess"));
    assert!(result.note.unwrap_or_default().contains("curated"));
}

/// The IronSpine SUB-4 shape: `sub` with key 4 round-trips, and the brute
/// sweep ranks that key first on the text it recovers.
#[test]
fn ironspine_sub4_round_trips_and_brutes() {
    // the plaintext carries a URL so the right key clearly wins the sweep
    let plain = b"beacon to http://ironspine.example/gate.php then sleep 60 seconds";
    let enc = arithmetic::add_byte(4, plain);
    // the keyed transform replays the loop exactly
    let out = transform("sub", &[4], &enc).expect("sub should run");
    assert_eq!(out, plain);
    // the brute sweep tries all 256 keys and ranks 4 first
    let result = brute("sub", &enc, DEFAULT_BRUTE_TOP, "strings").expect("brute should run");
    assert_eq!(result.tried, 256);
    assert_eq!(result.op, "sub");
    let best = &result.candidates[0];
    assert_eq!(best.key, 4, "wrong winner: {best:?}");
    assert!(best.score >= plain.len() - 2);
    assert!(
        best.runs
            .iter()
            .any(|run| run.contains("ironspine.example"))
    );
    assert!(best.preview.contains("beacon"));
}

/// The Stealc string shape: base64 then RC4 recovers the plaintext.
#[tokio::test]
async fn stealc_pipeline_decodes_base64_rc4() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plain = b"https://stealc-panel.example/c2sock|build7";
    // encode the way the sample stores it: rc4 under the key, then base64
    let packed = base64::encode(&rc4::decrypt(b"CgeOvEO7hu", plain));
    let path = write_sample(dir.path(), b"unused");
    let mut context = context(dir.path(), &path).await;
    // the pipeline unwraps the base64 and replays rc4 in one call
    let result = decode_bytes(
        &mut context,
        vec![
            serde_json::json!({ "op": "base64" }),
            serde_json::json!({ "op": "rc4", "key": "CgeOvEO7hu" }),
        ],
        None,
        None,
        None,
        Some(packed),
        Some("text".to_owned()),
        None,
    )
    .await
    .expect("the pipeline should decode");
    assert_eq!(result.steps, ["base64", "rc4(key 10 bytes)"]);
    assert_eq!(result.output_len, plain.len());
    assert!(result.text.contains("stealc-panel.example"));
    assert!((result.printable_ratio - 1.0).abs() < 1e-6);
}

/// The LummaC2 shape: base64 over key(32) || keystream(key, pt) round-trips
/// through the self_key_xor step.
#[tokio::test]
async fn lummac2_self_keyed_pipeline_decodes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plain = b"lumma build: 9HxQz2|https://lumma-gate.example/api";
    // the decoded blob carries its own 32-byte key up front
    let key: Vec<u8> = (0u8..32).map(|i| i.wrapping_mul(13) ^ 0xa7).collect();
    let mut blob = key.clone();
    blob.extend_from_slice(&xor::keystream(&key, plain));
    let packed = base64::encode(&blob);
    let path = write_sample(dir.path(), b"unused");
    let mut context = context(dir.path(), &path).await;
    let result = decode_bytes(
        &mut context,
        vec![
            serde_json::json!({ "op": "base64" }),
            serde_json::json!({ "op": "self_key_xor", "key_len": 32 }),
        ],
        None,
        None,
        None,
        Some(packed),
        Some("text".to_owned()),
        None,
    )
    .await
    .expect("the pipeline should decode");
    assert_eq!(result.output_len, plain.len());
    assert!(result.text.contains("lumma-gate.example"));
}

/// Encode `text` the way the pairwise scheme does, as `(char + k, k)` pairs.
///
/// # Arguments
///
/// * `text` - The plaintext to encode
/// * `key` - The subtrahend to pair each character with
fn pairwise_encode(text: &[u8], key: u8) -> Vec<u8> {
    text.iter()
        .flat_map(|c| [c.wrapping_add(key), key])
        .collect()
}

/// Pairwise SUB round-trips through the tool, parity 1 included.
#[tokio::test]
async fn pairwise_sub_tool_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plain = b"kernel32.dll and advapi32.dll";
    // one junk byte up front puts the pairs on parity 1
    let mut enc = vec![0xEE];
    enc.extend(pairwise_encode(plain, 7));
    let path = write_sample(dir.path(), b"unused");
    let mut context = context(dir.path(), &path).await;
    let result = pairwise_sub(
        &mut context,
        Some(1),
        None,
        None,
        None,
        Some(to_hex(&enc)),
        None,
        None,
    )
    .await
    .expect("the decode should run");
    // the decoded window starts with the plaintext
    assert_eq!(result.decode.steps, ["pairwise_sub(parity 1)"]);
    assert!(
        result
            .decode
            .text
            .starts_with("kernel32.dll and advapi32.dll")
    );
    // the hidden runs surface the recovery at its source offset (parity included)
    assert_eq!(result.hidden.len(), 1);
    assert_eq!(result.hidden[0].offset, "0x1");
    assert_eq!(result.hidden[0].text, "kernel32.dll and advapi32.dll");
    // the wrong parity decodes garbage: no hidden runs survive the filters
    let wrong = pairwise_sub(
        &mut context,
        Some(0),
        None,
        None,
        None,
        Some(to_hex(&enc)),
        None,
        None,
    )
    .await
    .expect("the wrong-parity decode should still run");
    assert!(wrong.hidden.is_empty());
    // a parity past 1 is refused
    let error = pairwise_sub(
        &mut context,
        Some(2),
        None,
        None,
        None,
        Some(to_hex(&enc)),
        None,
        None,
    )
    .await
    .expect_err("parity 2 should be refused");
    assert!(error.to_string().contains("parity"));
}

/// A huge hidden run is clipped to the documented cap, never returned whole.
#[tokio::test]
async fn pairwise_sub_clips_huge_hidden_runs() {
    let dir = tempfile::tempdir().expect("tempdir");
    // one readable run far past the per-run cap
    let plain = "the quick brown fox jumps over the lazy dog ".repeat(50);
    let enc = pairwise_encode(plain.as_bytes(), 9);
    let path = write_sample(dir.path(), b"unused");
    let mut context = context(dir.path(), &path).await;
    let result = pairwise_sub(
        &mut context,
        None,
        None,
        None,
        None,
        Some(to_hex(&enc)),
        None,
        None,
    )
    .await
    .expect("the decode should run");
    // the run comes back clipped with the truncation marker attached
    assert_eq!(result.hidden.len(), 1);
    let text = &result.hidden[0].text;
    assert!(text.ends_with("… (truncated)"), "no marker: {text}");
    assert!(text.chars().count() < plain.len());
    assert!(text.starts_with("the quick brown fox"));
}

/// The digest tool hashes whole files and ranges with pinned constants.
#[tokio::test]
async fn digest_hashes_files_and_ranges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_sample(dir.path(), b"abcd");
    let mut context = context(dir.path(), &path).await;
    // the whole-file digests of "abcd" are well-known constants (see file.rs)
    let whole = digest(&mut context, "md5".to_owned(), None, None, None)
        .await
        .expect("the whole-file digest should run");
    assert_eq!(whole.algo, "md5");
    assert_eq!(whole.file_size, 4);
    assert_eq!(
        whole.digest.as_deref(),
        Some("e2fc714c4727ee9395f324cd2e7f331f")
    );
    assert_eq!(whole.ranges.len(), 1);
    assert_eq!(whole.ranges[0].read, 4);
    // the whole-file default is sized exactly, so no short-read note appears
    assert_eq!(whole.ranges[0].length, 4);
    assert!(whole.note.is_none());
    let sha = digest(&mut context, "sha256".to_owned(), None, None, None)
        .await
        .expect("sha256 should run");
    assert_eq!(
        sha.digest.as_deref(),
        Some("88d4266fd4e6338d13b845fcf289579d209c897823b9217da3e161936f031589")
    );
    // two ranges concatenate into one digest matching the in-memory twin
    let ranged = digest(
        &mut context,
        "sha1".to_owned(),
        Some(vec![vec![0, 2], vec![3, 1]]),
        None,
        None,
    )
    .await
    .expect("the ranged digest should run");
    assert_eq!(
        ranged.digest.as_deref(),
        Some(digest_bytes(DigestAlgo::Sha1, b"abd").as_str())
    );
    assert!(ranged.note.is_none());
    // per_range digests each range alone and drops the concatenated digest
    let per = digest(
        &mut context,
        "md5".to_owned(),
        Some(vec![vec![0, 2], vec![2, 2]]),
        Some(true),
        None,
    )
    .await
    .expect("the per-range digest should run");
    assert!(per.digest.is_none());
    assert_eq!(
        per.ranges[0].digest.as_deref(),
        Some(digest_bytes(DigestAlgo::Md5, b"ab").as_str())
    );
    assert_eq!(
        per.ranges[1].digest.as_deref(),
        Some(digest_bytes(DigestAlgo::Md5, b"cd").as_str())
    );
    // a range past EOF reads short and says so, never silently padding
    let short = digest(
        &mut context,
        "md5".to_owned(),
        Some(vec![vec![2, 100]]),
        None,
        None,
    )
    .await
    .expect("the short digest should run");
    assert_eq!(short.ranges[0].read, 2);
    assert_eq!(
        short.digest.as_deref(),
        Some(digest_bytes(DigestAlgo::Md5, b"cd").as_str())
    );
    assert!(short.note.unwrap_or_default().contains("EOF"));
    // an unknown algorithm errors naming the supported set
    let error = digest(&mut context, "sha512".to_owned(), None, None, None)
        .await
        .expect_err("sha512 should be refused");
    assert!(error.to_string().contains("sha256"));
}

/// Builds the mixed ascii/utf16le buffer the grep tests scan
fn grep_fixture() -> Vec<u8> {
    let mut bytes = b"\x00\x01noise CONFIG here\x00".to_vec();
    // a utf16le "gate" after the ascii region
    for byte in b"gate" {
        bytes.push(*byte);
        bytes.push(0);
    }
    bytes.extend_from_slice(b"\xff\xfetail config end");
    bytes
}

/// The grep core finds both encodings case-insensitively with offsets.
#[test]
fn grep_window_finds_both_encodings() {
    let bytes = grep_fixture();
    let (matches, scanned_to, hit_limit) = grep_window(
        &bytes,
        0x100,
        &["config".to_owned(), "GATE".to_owned()],
        GrepEncodings::Both,
        true,
        4,
        DEFAULT_MATCHES,
        true,
    )
    .expect("the grep should run");
    assert!(!hit_limit);
    assert_eq!(scanned_to, 0x100 + bytes.len() as u64);
    // ascii CONFIG, wide gate, and ascii config, in offset order
    assert_eq!(matches.len(), 3);
    assert_eq!(matches[0].offset, "0x108");
    assert_eq!(matches[0].encoding, "ascii");
    assert_eq!(matches[0].keyword, "config");
    assert_eq!(matches[1].encoding, "utf16le");
    assert_eq!(matches[1].keyword, "GATE");
    assert_eq!(matches[2].encoding, "ascii");
    // the context hexdump wraps the match with its surrounding bytes
    assert!(matches[0].context.contains("CONFIG"));
    // case-sensitive matching drops the lowercase hit and the wide one
    let (matches, _, _) = grep_window(
        &bytes,
        0x100,
        &["CONFIG".to_owned()],
        GrepEncodings::Ascii,
        false,
        4,
        DEFAULT_MATCHES,
        true,
    )
    .expect("the exact grep should run");
    assert_eq!(matches.len(), 1);
    // the encodings filter narrows to one pass
    let (matches, _, _) = grep_window(
        &bytes,
        0x100,
        &["gate".to_owned()],
        GrepEncodings::Utf16le,
        true,
        4,
        DEFAULT_MATCHES,
        true,
    )
    .expect("the wide-only grep should run");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].encoding, "utf16le");
}

/// An over-long keyword with a multi-byte char errors instead of panicking.
#[test]
fn grep_window_clips_long_keywords_char_safely() {
    // byte 16 sits inside the two-byte 'é', where a byte slice would panic
    let keyword = format!("{}é{}", "a".repeat(15), "x".repeat(120));
    assert!(keyword.len() > MAX_KEYWORD_LEN);
    let error = grep_window(
        b"anything",
        0,
        &[keyword],
        GrepEncodings::Both,
        true,
        4,
        DEFAULT_MATCHES,
        true,
    )
    .expect_err("the oversized keyword should be refused");
    assert!(error.contains("cap"), "unexpected error: {error}");
    assert!(
        error.contains("aaaaaaaaaaaaaaaé"),
        "unexpected clip: {error}"
    );
}

/// Case-insensitive matching folds ASCII only, so non-ASCII keywords still
/// match their exact bytes.
#[test]
fn grep_window_matches_non_ascii_keywords() {
    // the haystack carries CAFÉ in utf-8; É is non-ASCII and stays as-is
    let bytes = "xx CAFÉ yy".as_bytes().to_vec();
    for keyword in ["CAFÉ", "cafÉ"] {
        let (matches, _, _) = grep_window(
            &bytes,
            0,
            &[keyword.to_owned()],
            GrepEncodings::Ascii,
            true,
            4,
            DEFAULT_MATCHES,
            true,
        )
        .expect("the non-ascii grep should run");
        assert_eq!(matches.len(), 1, "{keyword} missed");
        assert_eq!(matches[0].offset, "0x3");
    }
}

/// Hits tied at the limit boundary all deliver, so a resume never stalls or
/// re-delivers.
#[test]
fn grep_window_delivers_boundary_ties() {
    // "a" and "ab" both hit offset 0; a naive cut would stall at 0 forever
    let bytes = b"ab tail".to_vec();
    let keywords = vec!["a".to_owned(), "ab".to_owned()];
    let (matches, scanned_to, hit_limit) =
        grep_window(&bytes, 0, &keywords, GrepEncodings::Ascii, true, 2, 1, true)
            .expect("the tied grep should run");
    // both tied hits deliver (overshooting the limit) and the scan advances
    assert!(hit_limit);
    assert_eq!(matches.len(), 2);
    assert!(matches.iter().all(|hit| hit.offset == "0x0"));
    assert_eq!(scanned_to, 1);
    // resuming past the boundary finds only the undelivered hit
    let (matches, _, _) = grep_window(
        &bytes[1..],
        1,
        &keywords,
        GrepEncodings::Ascii,
        true,
        2,
        DEFAULT_MATCHES,
        true,
    )
    .expect("the resumed grep should run");
    let hits: Vec<(&str, &str)> = matches
        .iter()
        .map(|hit| (hit.offset.as_str(), hit.keyword.as_str()))
        .collect();
    assert_eq!(hits, [("0x4", "a")]);
    // a clean (untied) cut still resumes without re-delivering anything
    let bytes = b"ab ab".to_vec();
    let (matches, scanned_to, _) = grep_window(
        &bytes,
        0,
        &["ab".to_owned()],
        GrepEncodings::Ascii,
        true,
        2,
        1,
        true,
    )
    .expect("the clean-cut grep should run");
    assert_eq!(matches.len(), 1);
    assert_eq!(scanned_to, 3);
    let (matches, _, _) = grep_window(
        &bytes[3..],
        3,
        &["ab".to_owned()],
        GrepEncodings::Ascii,
        true,
        2,
        DEFAULT_MATCHES,
        true,
    )
    .expect("the clean resume should run");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].offset, "0x3");
}

/// The grep limit stops at the first undelivered match, resume-safe.
#[test]
fn grep_window_limit_and_resume() {
    // three ascii hits in a row
    let bytes = b"xx KEY yy KEY zz KEY ww".to_vec();
    let (matches, scanned_to, hit_limit) = grep_window(
        &bytes,
        0,
        &["key".to_owned()],
        GrepEncodings::Ascii,
        true,
        2,
        1,
        true,
    )
    .expect("the capped grep should run");
    assert!(hit_limit);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].offset, "0x3");
    // the stop is the second hit's offset, so a resume re-finds it first
    assert_eq!(scanned_to, 10);
    // resuming from the stop finds the remaining hits without duplicates
    let (matches, _, _) = grep_window(
        &bytes[10..],
        10,
        &["key".to_owned()],
        GrepEncodings::Ascii,
        true,
        2,
        DEFAULT_MATCHES,
        true,
    )
    .expect("the resumed grep should run");
    let offsets: Vec<&str> = matches.iter().map(|hit| hit.offset.as_str()).collect();
    assert_eq!(offsets, ["0xa", "0x11"]);
}

/// A keyword straddling the window edge is withheld unless the file ends.
#[test]
fn grep_window_withholds_the_cut_edge() {
    // the window ends mid-"CONFIG"; a next page must re-read it whole
    let bytes = b"xx KEY yy CONF".to_vec();
    let (matches, scanned_to, hit_limit) = grep_window(
        &bytes,
        0,
        &["key".to_owned(), "config".to_owned()],
        GrepEncodings::Ascii,
        true,
        2,
        DEFAULT_MATCHES,
        false,
    )
    .expect("the edged grep should run");
    assert!(!hit_limit);
    // the delivered match lands; the scan stops max-needle-len - 1 short
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].keyword, "key");
    assert_eq!(scanned_to, bytes.len() as u64 - 5);
    // the same bytes at EOF scan to the very end
    let (_, scanned_to, _) = grep_window(
        &bytes,
        0,
        &["key".to_owned(), "config".to_owned()],
        GrepEncodings::Ascii,
        true,
        2,
        DEFAULT_MATCHES,
        true,
    )
    .expect("the eof grep should run");
    assert_eq!(scanned_to, bytes.len() as u64);
}

/// The grep tool scans a real file window and notes how to continue.
#[tokio::test]
async fn grep_bytes_tool_scans_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_sample(dir.path(), &grep_fixture());
    let mut context = context(dir.path(), &path).await;
    let result = grep_bytes(
        &mut context,
        vec!["config".to_owned()],
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("the grep should run");
    assert_eq!(result.matches.len(), 2);
    assert_eq!(result.scanned_from, "0x0");
    assert!(result.truncated.is_none());
    // a limit of one notes the resume offset
    let capped = grep_bytes(
        &mut context,
        vec!["config".to_owned()],
        None,
        None,
        None,
        Some(1),
        None,
        None,
        None,
    )
    .await
    .expect("the capped grep should run");
    assert_eq!(capped.matches.len(), 1);
    let note = capped.truncated.expect("the cap should be noted");
    assert!(note.contains("offset ="), "unexpected note: {note}");
    // an empty keyword list is refused
    let error = grep_bytes(
        &mut context,
        Vec::new(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect_err("no keywords should be refused");
    assert!(error.to_string().contains("keywords"));
}

/// strings_range extracts from an explicit bound and filters encodings.
#[tokio::test]
async fn strings_range_bounds_and_filters() {
    let dir = tempfile::tempdir().expect("tempdir");
    // an early string, then the target region: an ascii and a wide string
    let mut bytes = b"EARLY\x00\x00\x00".to_vec();
    let start = bytes.len() as u64;
    bytes.extend_from_slice(b"corncob\x00");
    for byte in b"wide" {
        bytes.push(*byte);
        bytes.push(0);
    }
    bytes.extend_from_slice(b"\x00LATESTRING");
    let path = write_sample(dir.path(), &bytes);
    let mut context = context(dir.path(), &path).await;
    // the explicit bound sees only the region's strings
    let found = strings_range(&mut context, start, Some(18), None, None, None, None, None)
        .await
        .expect("the ranged scan should run");
    let texts: Vec<&str> = found.strings.iter().map(|s| s.text.as_str()).collect();
    assert!(texts.contains(&"corncob"), "missing corncob: {texts:?}");
    assert!(!texts.iter().any(|text| text.contains("EARLY")));
    assert!(!texts.iter().any(|text| text.contains("LATESTRING")));
    assert_eq!(found.scanned_from, format!("{start:#x}"));
    // the encodings post-filter keeps one encoding
    let wide_only = strings_range(
        &mut context,
        start,
        Some(18),
        None,
        None,
        Some("utf16le".to_owned()),
        None,
        None,
    )
    .await
    .expect("the filtered scan should run");
    assert!(wide_only.strings.iter().all(|s| s.encoding == "utf16le"));
    assert!(wide_only.strings.iter().any(|s| s.text.contains("wide")));
    // an unknown encodings filter is refused
    let error = strings_range(
        &mut context,
        0,
        None,
        None,
        None,
        Some("utf32".to_owned()),
        None,
        None,
    )
    .await
    .expect_err("utf32 should be refused");
    assert!(error.to_string().contains("utf16le"));
}

/// decode_bytes reads file windows and saves full output under a name.
#[tokio::test]
async fn decode_bytes_reads_files_and_saves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plain = b"saved payload: http://files.example/next-stage.bin";
    // the sample is the xor-0x5a encoding of the plaintext
    let enc = xor::single_byte(0x5a, plain);
    let path = write_sample(dir.path(), &enc);
    let mut context = context(dir.path(), &path).await;
    let result = decode_bytes(
        &mut context,
        vec![serde_json::json!({ "op": "xor", "key_hex": "5a" })],
        None,
        None,
        None,
        None,
        None,
        Some("decoded.bin".to_owned()),
    )
    .await
    .expect("the decode should run");
    assert_eq!(result.input_len, enc.len());
    assert!(result.text.contains("files.example"));
    // the full output landed in the workspace, addressable by file tools
    let saved = result.saved.expect("the output should save");
    assert_eq!(saved.name, "decoded.bin");
    assert_eq!(saved.info.size, plain.len() as u64);
    let store = SessionStore::from_context(&context).expect("store");
    let saved_path = store.extracted_path("decoded.bin").await.expect("path");
    assert_eq!(tokio::fs::read(&saved_path).await.expect("read"), plain);
}

/// Pipeline parsing rejects bad shapes with instructive errors.
#[test]
fn parse_steps_rejects_bad_pipelines() {
    // an empty pipeline is a no-op worth refusing
    assert!(
        parse_steps(&[])
            .expect_err("empty")
            .contains("at least one")
    );
    // an unknown op lists the supported set
    let error = parse_steps(&[serde_json::json!({ "op": "rot13" })]).expect_err("rot13");
    assert!(error.contains("sub_from_key"), "unexpected error: {error}");
    // both key spellings at once are ambiguous
    let error = parse_steps(&[serde_json::json!({
        "op": "rc4", "key": "a", "key_hex": "61"
    })])
    .expect_err("both keys");
    assert!(error.contains("not both"));
    // too many steps are refused
    let steps: Vec<serde_json::Value> = (0..9)
        .map(|_| serde_json::json!({ "op": "base64" }))
        .collect();
    assert!(
        parse_steps(&steps)
            .expect_err("too many")
            .contains("at most")
    );
    // a bad custom alphabet is refused before any decoding
    let error = parse_steps(&[serde_json::json!({
        "op": "base64", "alphabet": "short"
    })])
    .expect_err("short alphabet");
    assert!(error.contains("64"));
    // an alphabet containing '=' would silently mis-decode (the decoder skips
    // padding before the symbol lookup), so it is refused outright
    let standard = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let padded = standard.replace('+', "=");
    let error = parse_steps(&[serde_json::json!({
        "op": "base64", "alphabet": padded
    })])
    .expect_err("'=' in the alphabet");
    assert!(error.contains("mis-decode"), "unexpected error: {error}");
    // ...and so is one containing whitespace, which the decoder also skips
    let spaced = standard.replace('/', " ");
    let error = parse_steps(&[serde_json::json!({
        "op": "base64", "alphabet": spaced
    })])
    .expect_err("whitespace in the alphabet");
    assert!(error.contains("mis-decode"), "unexpected error: {error}");
}

/// The shared salt-position parser is case-insensitive with a suffix default.
#[test]
fn salt_positions_parse_case_insensitively() {
    // the default is a suffix salt
    assert!(parse_salt_position(None).expect("default"));
    // both spellings parse in any case (shared by the tool and the binding)
    assert!(parse_salt_position(Some("SUFFIX")).expect("suffix"));
    assert!(!parse_salt_position(Some("Prefix")).expect("prefix"));
    assert!(!parse_salt_position(Some("prefix")).expect("prefix"));
    // a typo errors naming the supported positions
    let error = parse_salt_position(Some("middle")).expect_err("middle");
    assert!(error.contains("suffix"), "unexpected error: {error}");
}

/// Inline data decodes under each encoding, and conflicts are refused.
#[tokio::test]
async fn inline_data_encodings_and_conflicts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_sample(dir.path(), b"unused");
    let mut context = context(dir.path(), &path).await;
    // the three encodings all decode (hex tolerates whitespace and 0x)
    assert_eq!(decode_inline("0x41 42", "hex").expect("hex"), b"AB");
    assert_eq!(
        decode_inline("aGVsbG8=", "base64").expect("base64"),
        b"hello"
    );
    assert_eq!(decode_inline("hi", "text").expect("text"), b"hi");
    assert!(decode_inline("zz", "hex").is_err());
    assert!(
        decode_inline("x", "utf7")
            .expect_err("utf7")
            .contains("base64")
    );
    // inline data alongside a target is ambiguous and refused
    let error = decode_bytes(
        &mut context,
        vec![serde_json::json!({ "op": "base64" })],
        Some("child.bin".to_owned()),
        None,
        None,
        Some("aGVsbG8=".to_owned()),
        Some("base64".to_owned()),
        None,
    )
    .await
    .expect_err("data + target should be refused");
    assert!(error.to_string().contains("not both"));
    // the window knobs alongside inline data would be silently ignored, so
    // they are refused the same way
    for (offset, length) in [(Some(1u64), None), (None, Some(16usize))] {
        let error = decode_bytes(
            &mut context,
            vec![serde_json::json!({ "op": "base64" })],
            None,
            offset,
            length,
            Some("aGVsbG8=".to_owned()),
            Some("base64".to_owned()),
            None,
        )
        .await
        .expect_err("data + a window knob should be refused");
        assert!(error.to_string().contains("not both"));
    }
}

/// byte_transform applies keyed ops and brute-forces through the tool.
#[tokio::test]
async fn byte_transform_tool_keys_and_brutes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plain = b"transformed config: https://spine.example/panel entry point";
    let enc = xor::keystream(b"\x5a\x13", plain);
    let path = write_sample(dir.path(), b"unused");
    let mut context = context(dir.path(), &path).await;
    // a hex-string key drives a repeating-key xor
    let result = byte_transform(
        &mut context,
        "xor".to_owned(),
        Some(serde_json::json!("5a13")),
        None,
        None,
        None,
        None,
        None,
        None,
        Some(to_hex(&enc)),
        None,
        None,
    )
    .await
    .expect("the keyed transform should run");
    assert!(
        result["text"]
            .as_str()
            .unwrap_or("")
            .contains("spine.example")
    );
    assert_eq!(result["output_len"], serde_json::json!(plain.len()));
    // the brute path sweeps a single-byte op and reports candidates
    let enc = arithmetic::add_byte(9, plain);
    let result = byte_transform(
        &mut context,
        "sub".to_owned(),
        None,
        None,
        Some(3),
        None,
        None,
        None,
        None,
        Some(to_hex(&enc)),
        None,
        None,
    )
    .await
    .expect("the brute should run");
    assert_eq!(result["tried"], serde_json::json!(256));
    assert_eq!(result["candidates"][0]["key"], serde_json::json!(9));
    assert_eq!(result["candidates"].as_array().map(Vec::len), Some(3));
    // rol round-trips a ror loop (rotate left undoes rotate right)
    let ror: Vec<u8> = plain.iter().map(|byte| byte.rotate_right(3)).collect();
    let result = byte_transform(
        &mut context,
        "rol".to_owned(),
        Some(serde_json::json!(3)),
        None,
        None,
        None,
        None,
        None,
        None,
        Some(to_hex(&ror)),
        None,
        None,
    )
    .await
    .expect("the rol should run");
    assert!(
        result["text"]
            .as_str()
            .unwrap_or("")
            .contains("entry point")
    );
    // a key alongside brute is ambiguous and refused
    let error = byte_transform(
        &mut context,
        "xor".to_owned(),
        Some(serde_json::json!(4)),
        Some(true),
        None,
        None,
        None,
        None,
        None,
        Some("00".to_owned()),
        None,
        None,
    )
    .await
    .expect_err("key + brute should be refused");
    assert!(error.to_string().contains("brute"));
    // a `name` in brute mode would be silently dropped, so it is refused
    // with the path forward named
    let error = byte_transform(
        &mut context,
        "sub".to_owned(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some("00".to_owned()),
        None,
        Some("winner.bin".to_owned()),
    )
    .await
    .expect_err("brute + name should be refused");
    assert!(error.to_string().contains("does not save"));
}

/// The transform core validates ops and key shapes.
#[test]
fn transform_validates_ops_and_keys() {
    // single-byte ops reject multi-byte keys
    assert!(transform("add", &[1, 2], b"abc").is_err());
    // rol demands a real rotation amount
    assert!(transform("rol", &[0], b"abc").is_err());
    assert!(transform("rol", &[8], b"abc").is_err());
    // an unknown op names the supported set
    let error = transform("rot13", &[1], b"abc").expect_err("rot13");
    assert!(error.contains("sub_from_key"));
    // xor accepts any non-empty key length
    assert_eq!(transform("xor", &[0xff], &[0x00]).expect("xor"), [0xff]);
    assert!(transform("xor", &[], b"abc").is_err());
    // sub_from_key replays the Lumma loader loop
    let enc: Vec<u8> = b"abc".iter().map(|b| 0x21u8.wrapping_sub(*b)).collect();
    assert_eq!(
        transform("sub_from_key", &[0x21], &enc).expect("sfk"),
        b"abc"
    );
}

/// Hash-target coercion accepts ints and hex strings, and rejects junk.
#[test]
fn hash_targets_coerce() {
    assert_eq!(
        parse_hash_target(&serde_json::json!(0x1234)).expect("int"),
        0x1234
    );
    assert_eq!(
        parse_hash_target(&serde_json::json!("0xDEADBEEF")).expect("hex"),
        0xDEAD_BEEF
    );
    assert_eq!(
        parse_hash_target(&serde_json::json!("cafef00d")).expect("bare hex"),
        0xCAFE_F00D
    );
    assert!(parse_hash_target(&serde_json::json!(-1)).is_err());
    assert!(parse_hash_target(&serde_json::json!(0x1_0000_0000u64)).is_err());
    assert!(parse_hash_target(&serde_json::json!("corn")).is_err());
    assert!(parse_hash_target(&serde_json::json!([1])).is_err());
}

/// The variants parser applies known flags and rejects typos.
#[test]
fn variants_parse() {
    // absent variants mean everything on
    let variants = parse_variants(None).expect("default");
    assert!(variants.wide && variants.nt_zw_twins);
    // a flag turns its variant off
    let narrowed = parse_variants(Some(&serde_json::json!({ "wide": false }))).expect("narrowed");
    assert!(!narrowed.wide && narrowed.lowercase);
    // typos are rejected with the valid keys named
    let error = parse_variants(Some(&serde_json::json!({ "wied": true }))).expect_err("typo");
    assert!(error.contains("nt_zw_twins"));
}
