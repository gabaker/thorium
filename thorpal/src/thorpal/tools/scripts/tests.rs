//! Tests for the Rhai script tools.

use std::path::{Path, PathBuf};

use super::*;
use crate::thorpal::tools::ghidra::GhidraSettings;
use crate::thorpal::tools::registry;

/// Builds a tool context backed by a scratch session store
///
/// The context mirrors a real run: a session store, a Ghidra session with no
/// install configured, and an empty enabled toolset (tests opt into bindings
/// by overriding the [`EnabledTools`] entry).
///
/// # Arguments
///
/// * `dir` - The scratch sessions root to create the store under
async fn context(dir: &Path) -> ToolContext {
    // create a store the script tools can save into
    let store = SessionStore::create(
        dir.to_path_buf(),
        None,
        PathBuf::from("bin/target"),
        "target".to_owned(),
        false,
        Vec::new(),
    )
    .await
    .expect("store should create");
    // a session whose Ghidra install is unset fails bindings with a setup error
    let ghidra = GhidraSession::new(GhidraSettings::from_binary("bin/target", "projects", None));
    // seed the context exactly like ThorPal::tool_context does
    let mut context = ToolContext::new();
    context.insert(store);
    context.insert(ghidra);
    context.insert(EnabledTools::default());
    context
}

/// Builds a tool context that also carries a persistent script library
///
/// The library roots under the same scratch dir (at `library/`), so promote
/// and fallback tests exercise the exact wiring `ThorPal::tool_context` sets
/// up when a library is configured.
///
/// # Arguments
///
/// * `dir` - The scratch root for both the sessions and the library
async fn context_with_library(dir: &Path) -> (ToolContext, Arc<ScriptLibrary>) {
    // start from the plain session-backed context
    let mut context = context(dir).await;
    // open a library beside the sessions and seed it into the context
    let library = ScriptLibrary::open(dir.join("library"))
        .await
        .expect("library should open");
    context.insert(Arc::clone(&library));
    (context, library)
}

/// Enables the named registry tools on a context's enabled toolset
///
/// # Arguments
///
/// * `context` - The context to override the enabled toolset of
/// * `names` - The registry tool names to enable
fn enable(context: &mut ToolContext, names: &[&str]) {
    // resolve each name against the registry, failing loudly on typos
    let entries: Vec<_> = names
        .iter()
        .map(|name| registry::find(name).expect("tool should be registered"))
        .collect();
    // replace the enabled set, as run_sub_agent does per sub-agent invocation
    context.insert(EnabledTools::from_entries(&entries));
}

/// Builds a store-backed tool context whose sample holds the given bytes
///
/// Like [`context`], but the (install-less) Ghidra session points at a real
/// file on disk so the file/container bindings have something to read.
///
/// # Arguments
///
/// * `dir` - The scratch dir for the sample and the sessions root
/// * `bytes` - The sample file's contents
async fn context_with_sample(dir: &Path, bytes: &[u8]) -> ToolContext {
    // materialize the sample the bindings read
    let binary = dir.join("sample.bin");
    std::fs::write(&binary, bytes).expect("sample should write");
    // create a store rooted beside it so extractions have a workspace
    let store = SessionStore::create(
        dir.join("sessions"),
        None,
        binary.clone(),
        "sample".to_owned(),
        false,
        Vec::new(),
    )
    .await
    .expect("store should create");
    let ghidra = GhidraSession::new(GhidraSettings::from_binary(&binary, "projects", None));
    // seed the context exactly like ThorPal::tool_context does
    let mut context = ToolContext::new();
    context.insert(store);
    context.insert(ghidra);
    context.insert(EnabledTools::default());
    context
}

/// Scripts create, read, list, search, edit, and refuse overwrites.
#[tokio::test]
async fn script_crud_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // create a script and read it back verbatim
    create_script(&mut context, "answer".to_owned(), "40 + 2".to_owned())
        .await
        .expect("create should save");
    let source = read_script(&mut context, "answer".to_owned())
        .await
        .expect("read should load");
    assert_eq!(source, "40 + 2");
    // creating the same name again is refused with an edit hint
    let error = create_script(&mut context, "answer".to_owned(), "1".to_owned())
        .await
        .expect_err("overwrite should be refused");
    assert!(error.to_string().contains("edit_script"));
    // the listing and search both find the script
    let scripts = list_scripts(&mut context).await.expect("list");
    assert_eq!(scripts.session, ["answer"]);
    assert!(scripts.library.is_empty());
    let matches = search_scripts(&mut context, "40 +".to_owned())
        .await
        .expect("search");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].script, "answer");
    assert_eq!(matches[0].origin, "session");
    assert_eq!(matches[0].line, 1);
    // edit the script in place and read the change back
    edit_script(
        &mut context,
        "answer".to_owned(),
        "40".to_owned(),
        "41".to_owned(),
        None,
    )
    .await
    .expect("edit should apply");
    let source = read_script(&mut context, "answer".to_owned())
        .await
        .expect("read should load");
    assert_eq!(source, "41 + 2");
    // an edit whose needle is missing is rejected with a read hint
    let error = edit_script(
        &mut context,
        "answer".to_owned(),
        "missing".to_owned(),
        "x".to_owned(),
        None,
    )
    .await
    .expect_err("missing needle should be rejected");
    assert!(error.to_string().contains("not found"));
}

/// Sources that do not compile are rejected and never touch the disk.
#[tokio::test]
async fn broken_sources_are_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // a syntax error is rejected at create time with nothing saved
    let error = create_script(&mut context, "broken".to_owned(), "let = ;".to_owned())
        .await
        .expect_err("a broken source should be rejected");
    assert!(error.to_string().contains("compile"));
    assert!(
        list_scripts(&mut context)
            .await
            .expect("list")
            .session
            .is_empty()
    );
    // an edit that breaks a good script is rejected and the old source kept
    create_script(&mut context, "good".to_owned(), "1 + 1".to_owned())
        .await
        .expect("create should save");
    let error = edit_script(
        &mut context,
        "good".to_owned(),
        "1 + 1".to_owned(),
        "let = ;".to_owned(),
        None,
    )
    .await
    .expect_err("a breaking edit should be rejected");
    assert!(error.to_string().contains("not applied"));
    let source = read_script(&mut context, "good".to_owned())
        .await
        .expect("read should load");
    assert_eq!(source, "1 + 1");
}

/// A run returns the final expression, the args value, and printed output.
#[tokio::test]
async fn runs_return_value_args_and_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // a script that reads args, prints progress, and returns a map
    let source = "print(`doubling ${args.x}`);\n#{ doubled: args.x * 2 }";
    create_script(&mut context, "double".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    let run = run_script(
        &mut context,
        "double".to_owned(),
        Some(serde_json::json!({ "x": 21 })),
        None,
    )
    .await
    .expect("run should succeed");
    // the final expression comes back as JSON and the print line is captured
    assert_eq!(run.value, serde_json::json!({ "doubled": 42 }));
    assert_eq!(run.output, ["doubling 21"]);
    assert!(run.truncated.is_none());
}

/// A failing run surfaces the error with the partial output attached.
#[tokio::test]
async fn failed_runs_keep_partial_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // a script that prints progress and then throws
    let source = "print(\"started\");\nthrow \"boom\";";
    create_script(&mut context, "boom".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    let error = run_script(&mut context, "boom".to_owned(), None, None)
        .await
        .expect_err("the throw should fail the run");
    // the error names the script, the throw, and keeps the printed line
    let rendered = error.to_string();
    assert!(rendered.contains("boom"), "unexpected error: {rendered}");
    assert!(rendered.contains("started"), "lost output: {rendered}");
}

/// The bindings a script sees follow the enabled toolset exactly.
#[tokio::test]
async fn bindings_follow_the_enabled_toolset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // a script that calls the bytewise_xor host function
    let source = "bytewise_xor(\"ff00\", \"0f0f\")";
    create_script(&mut context, "xor".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    // with no tools enabled the function does not exist at call time
    let error = run_script(&mut context, "xor".to_owned(), None, None)
        .await
        .expect_err("the binding should be missing");
    assert!(error.to_string().contains("bytewise_xor"));
    // enabling the tool in the toolset makes the same script succeed
    enable(&mut context, &["bytewise_xor"]);
    let run = run_script(&mut context, "xor".to_owned(), None, None)
        .await
        .expect("the binding should now exist");
    assert_eq!(run.value, serde_json::json!("f00f"));
}

/// The raw-file bindings read the real sample without any Ghidra install.
#[tokio::test]
async fn file_bindings_read_the_raw_sample() {
    let dir = tempfile::tempdir().expect("tempdir");
    // materialize the fake sample the raw-file binding reads
    let binary = dir.path().join("sample.bin");
    std::fs::write(&binary, b"CORNFIELD").expect("sample should write");
    // build a context whose (install-less) Ghidra session points at the sample
    let store = SessionStore::create(
        dir.path().to_path_buf(),
        None,
        binary.clone(),
        "sample".to_owned(),
        false,
        Vec::new(),
    )
    .await
    .expect("store should create");
    let ghidra = GhidraSession::new(GhidraSettings::from_binary(&binary, "projects", None));
    let mut context = ToolContext::new();
    context.insert(store);
    context.insert(ghidra);
    context.insert(EnabledTools::default());
    // a script that reads raw bytes back out of the sample as a blob
    let source = "let b = read_file(0, 8);\n\
                  #{ text: b.as_string(), byte: b[0], len: b.len() }";
    create_script(&mut context, "peek_raw".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    // the binding only exists once the tool is enabled
    enable(&mut context, &["read_file"]);
    let run = run_script(&mut context, "peek_raw".to_owned(), None, None)
        .await
        .expect("the raw read should succeed");
    // the blob round-trips the sample's leading bytes exactly
    assert_eq!(run.value["text"], serde_json::json!("CORNFIEL"));
    assert_eq!(run.value["byte"], serde_json::json!(b'C'));
    assert_eq!(run.value["len"], serde_json::json!(8));
}

/// A Ghidra binding without an install fails with the setup error.
#[tokio::test(flavor = "multi_thread")]
async fn ghidra_bindings_surface_setup_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // a script that reads memory through the ghidra-backed binding
    let source = "read(\"00400000\", 4)";
    create_script(&mut context, "peek".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    enable(&mut context, &["read"]);
    // with no Ghidra install the binding fails with the bootstrap error
    let error = run_script(&mut context, "peek".to_owned(), None, None)
        .await
        .expect_err("the binding should fail without an install");
    assert!(error.to_string().contains("Ghidra"));
}

/// A runaway script is terminated by the wall-clock deadline.
#[tokio::test]
async fn runaway_scripts_hit_the_deadline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // an infinite loop that only the deadline (or op cap) can stop
    create_script(&mut context, "spin".to_owned(), "loop { }".to_owned())
        .await
        .expect("create should save");
    // a zero-second deadline terminates on the first throttled progress check
    let error = run_script(&mut context, "spin".to_owned(), None, Some(0))
        .await
        .expect_err("the deadline should kill the loop");
    let rendered = error.to_string();
    assert!(
        rendered.contains("deadline"),
        "unexpected error: {rendered}"
    );
    // the termination message carries the batching hint
    assert!(rendered.contains("batches"), "missing hint: {rendered}");
}

/// Nesting that rhai's debug defaults (32/16) reject compiles and runs.
///
/// This pins both the raised `set_max_expr_depths` limits and the invariant
/// that the compile-check and execution engines share them: a script that
/// saves must never fail to re-parse at run time.
#[tokio::test]
async fn deeply_nested_scripts_compile_and_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // each syntactic level costs ~3 depth units, so the stock debug engine
    // (32 global / 16 in-fn) rejects arrays just 10 deep and in-fn maps 4
    // deep; these nestings fail stock but fit the raised 128 limits
    let global = format!("let v = {}1{};", "[".repeat(40), "]".repeat(40));
    let func = format!(
        "fn deep() {{ let m = {}1{}; 7 }}",
        "#{ a: ".repeat(30),
        " }".repeat(30)
    );
    let source = format!("{global}\n{func}\ndeep()");
    // the save-time compile check and the run-time parse must both accept it
    create_script(&mut context, "deep".to_owned(), source)
        .await
        .expect("the nested script should compile");
    let run = run_script(&mut context, "deep".to_owned(), None, None)
        .await
        .expect("the nested script should run");
    assert_eq!(run.value, serde_json::json!(7));
}

/// Nesting past the raised limit fails with the flattening hint attached.
#[tokio::test]
async fn too_deep_compile_errors_carry_the_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // a 200-deep nested array literal exceeds the raised 128 limit
    let source = format!("let v = {}1{};", "[".repeat(200), "]".repeat(200));
    let error = create_script(&mut context, "too_deep".to_owned(), source)
        .await
        .expect_err("the nesting should exceed the limit");
    // the raw rhai error and the fix-it hint both reach the model
    let rendered = error.to_string();
    assert!(
        rendered.contains("maximum complexity"),
        "unexpected error: {rendered}"
    );
    assert!(rendered.contains("hoist"), "missing hint: {rendered}");
}

/// Exhausting the op cap fails with the batching hint attached.
#[tokio::test]
async fn op_cap_exhaustion_suggests_batching() {
    // build a host directly since the cap is a parameter of execute_script
    let host = ScriptHost {
        ghidra: GhidraSession::new(GhidraSettings::from_binary("bin/target", "projects", None)),
        store: None,
        handle: tokio::runtime::Handle::current(),
    };
    // a spinning script with a far-off deadline only the op cap can stop
    let output = Arc::new(Mutex::new(Vec::new()));
    let deadline = Instant::now() + Duration::from_secs(60);
    let error = execute_script(
        "loop { }",
        &serde_json::Value::Null,
        &[],
        &host,
        deadline,
        1_000,
        &output,
    )
    .expect_err("the op cap should kill the loop");
    // the error names the cap and tells the model how to batch
    assert!(error.contains("operation cap"), "unexpected error: {error}");
    assert!(error.contains("skip/limit"), "missing hint: {error}");
}

/// Long scripts build in chunks and broken appends never touch the disk.
#[tokio::test]
async fn append_script_builds_scripts_in_chunks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // save a small working core first
    create_script(
        &mut context,
        "chunks".to_owned(),
        "fn add(a, b) { a + b }".to_owned(),
    )
    .await
    .expect("the core should save");
    // append a complete fn plus the driver expression
    append_script(
        &mut context,
        "chunks".to_owned(),
        "fn mul(a, b) { a * b }\nmul(add(1, 2), 14)".to_owned(),
    )
    .await
    .expect("the append should apply");
    let run = run_script(&mut context, "chunks".to_owned(), None, None)
        .await
        .expect("the combined script should run");
    assert_eq!(run.value, serde_json::json!(42));
    // a breaking append is rejected and the previous source kept
    let error = append_script(&mut context, "chunks".to_owned(), "let = ;".to_owned())
        .await
        .expect_err("a broken append should be rejected");
    assert!(error.to_string().contains("not applied"));
    let source = read_script(&mut context, "chunks".to_owned())
        .await
        .expect("read should load");
    assert_eq!(
        source,
        "fn add(a, b) { a + b }\nfn mul(a, b) { a * b }\nmul(add(1, 2), 14)"
    );
}

/// The docs tool reports the enabled host functions dynamically.
#[tokio::test]
async fn rhai_docs_follow_the_enabled_toolset() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // with nothing enabled the docs say scripts are pure Rhai
    let docs = rhai_docs(&mut context).await.expect("docs should render");
    assert!(docs.contains("No host functions are enabled"));
    // enabling a bound tool surfaces its signature and full reference
    enable(&mut context, &["bytewise_xor"]);
    let docs = rhai_docs(&mut context).await.expect("docs should render");
    assert!(docs.contains("bytewise_xor(a: string, b: string)"));
    assert!(docs.contains("equal-length hex"));
}

/// Every construct the Rhai reference documents actually evaluates.
///
/// This pins the docs (`rhai_reference.md` and the Developer preamble's
/// cheat-sheet) to what the compiled rhai version really supports, so a rhai
/// upgrade that drops or renames one of them fails here instead of lying to
/// the model.
#[tokio::test]
async fn documented_rhai_constructs_evaluate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // one composite script exercising the documented language surface
    let source = r#"
        // variables, consts, hex literals, and bit ops
        let x = 0x5a ^ 0x0f;
        const K = 2;
        // strings: methods, interpolation, parsing with a radix
        let s = "ab,cd".split(",");
        let joined = `${s[0]}-${s[1]}`;
        let parsed = "ff".parse_int(16);
        let part = "hello".sub_string(1, 3);
        let shout = "hey".to_upper();
        // trim/replace mutate IN PLACE and return (), exactly as documented
        let padded = "  x  ";
        let trim_ret = padded.trim();
        let swapped = "aaa";
        swapped.replace("a", "b");
        let hex = 255.to_hex();
        // arrays: push, map, filter, reduce, membership
        let a = [1, 2, 3];
        a.push(4);
        let doubled = a.map(|v| v * K);
        let evens = a.filter(|v| v % 2 == 0);
        let sum = a.reduce(|total, v| total + v, 0);
        let has = 3 in a;
        a.sort();
        // blobs: construction, indexing, parsing, and reinterpretation
        let b = blob(8);
        b[0] = 0x41;
        let le = b.parse_le_int(0..4);
        let be = b.parse_be_int(4..8);
        let text = b.as_string();
        let ints = b.to_array();
        // maps: literals, field access, membership, keys
        let m = #{ name: "entry", size: 64 };
        m.extra = true;
        let known = "name" in m;
        let keys = m.keys();
        // control flow: for over ranges and arrays, while, loop, switch
        let count = 0;
        for i in 0..4 { count += i; }
        for v in a { count += v; }
        while count > 100 { count -= 1; }
        loop { count += 1; break; }
        let word = switch count { 0 => "zero", _ => "many" };
        // functions, closures (called with .call), throw, and try/catch
        fn add(l, r) { l + r }
        let inc = |v| v + 1;
        let caught = "";
        try { throw "boom"; } catch (e) { caught = `${e}!`; }
        // fn args are BY VALUE: pushing inside leaves the caller's array alone
        fn try_mutate(arr) { arr.push(9); arr.len() }
        let untouched = [1, 2];
        let inner_len = try_mutate(untouched);
        // print/debug stream and the final expression is the result
        print("done");
        debug(sum);
        #{ x: x, parsed: parsed, sum: sum, le: le, word: word, caught: caught,
           added: add(20, inc.call(21)),
           trim_ret_is_unit: trim_ret == (), trimmed: padded,
           swapped: swapped, hex: hex,
           inner_len: inner_len, untouched_len: untouched.len() }
    "#;
    create_script(&mut context, "reference".to_owned(), source.to_owned())
        .await
        .expect("the reference script should compile");
    let run = run_script(&mut context, "reference".to_owned(), None, None)
        .await
        .expect("the reference script should run");
    // spot-check the computed values so the constructs really executed
    assert_eq!(run.value["x"], serde_json::json!(0x55));
    assert_eq!(run.value["parsed"], serde_json::json!(255));
    assert_eq!(run.value["sum"], serde_json::json!(10));
    assert_eq!(run.value["word"], serde_json::json!("many"));
    assert_eq!(run.value["caught"], serde_json::json!("boom!"));
    assert_eq!(run.value["added"], serde_json::json!(42));
    // the in-place semantics the Gotchas section teaches really hold
    assert_eq!(run.value["trim_ret_is_unit"], serde_json::json!(true));
    assert_eq!(run.value["trimmed"], serde_json::json!("x"));
    assert_eq!(run.value["swapped"], serde_json::json!("bbb"));
    assert_eq!(run.value["hex"], serde_json::json!("ff"));
    // by-value fn args: the inner push never reaches the caller's array
    assert_eq!(run.value["inner_len"], serde_json::json!(3));
    assert_eq!(run.value["untouched_len"], serde_json::json!(2));
    assert_eq!(run.output, ["done", "[debug] 10"]);
}

/// A script decompresses a gzip blob in memory through the binding.
#[tokio::test]
async fn scripts_decompress_blobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // a script that builds no blob of its own: args carries the hex payload
    let source = r#"
        // decode the hex payload from args into a blob
        let packed = blob();
        let hex = args.hex;
        for i in 0..hex.len() / 2 {
            packed.push(hex.sub_string(i * 2, 2).parse_int(16));
        }
        // inflate it natively instead of hand-writing a decompressor
        let plain = decompress(packed, "gzip");
        plain.as_string()
    "#;
    create_script(&mut context, "gunzip".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    // gzip a known payload and hand it to the script as hex
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut encoder, b"INFLATED FINE").expect("gzip write");
    let packed = encoder.finish().expect("gzip finish");
    let hex: String = packed.iter().map(|byte| format!("{byte:02x}")).collect();
    // the binding only exists once the tool is enabled
    enable(&mut context, &["decompress"]);
    let run = run_script(
        &mut context,
        "gunzip".to_owned(),
        Some(serde_json::json!({ "hex": hex })),
        None,
    )
    .await
    .expect("the decompression should succeed");
    assert_eq!(run.value, serde_json::json!("INFLATED FINE"));
    // a cap or format failure is catchable inside the script
    let catching = r#"
        let bad = blob(4, 0x41);
        let result = "missed";
        try { decompress(bad, "gzip"); } catch (e) { result = `caught: ${e}`; }
        result
    "#;
    create_script(&mut context, "catcher".to_owned(), catching.to_owned())
        .await
        .expect("create should save");
    let run = run_script(&mut context, "catcher".to_owned(), None, None)
        .await
        .expect("the catch should succeed");
    assert!(run.value.as_str().unwrap_or("").starts_with("caught:"));
}

/// A script lists a zip, extracts an entry, and reads the child by target.
#[tokio::test]
async fn scripts_extract_and_target_children() {
    let dir = tempfile::tempdir().expect("tempdir");
    // the sample is a zip holding one stored payload
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    writer
        .start_file("payload.bin", zip::write::SimpleFileOptions::default())
        .expect("entry should start");
    std::io::Write::write_all(&mut writer, b"CHILD PAYLOAD").expect("entry should write");
    let bytes = writer.finish().expect("zip should finish").into_inner();
    let mut context = context_with_sample(dir.path(), &bytes).await;
    // a script that lists the container, extracts the entry, and reads it back
    let source = r#"
        let listing = list_container();
        let entry = listing.entries[0].name;
        let extracted = extract_file(entry);
        let child = read_file(extracted.name, 0, 5);
        #{ kind: listing.kind, entry: entry, name: extracted.name,
           size: extracted.info.size, head: child.as_string(),
           ident: file_info(extracted.name).size }
    "#;
    create_script(&mut context, "unpack".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    // the bindings only exist once their tools are enabled
    enable(
        &mut context,
        &["list_container", "extract_file", "read_file", "file_info"],
    );
    let run = run_script(&mut context, "unpack".to_owned(), None, None)
        .await
        .expect("the unpack should succeed");
    assert_eq!(run.value["kind"], serde_json::json!("zip"));
    assert_eq!(run.value["entry"], serde_json::json!("payload.bin"));
    assert_eq!(run.value["name"], serde_json::json!("payload.bin"));
    assert_eq!(run.value["size"], serde_json::json!(13));
    assert_eq!(run.value["head"], serde_json::json!("CHILD"));
    assert_eq!(run.value["ident"], serde_json::json!(13));
}

/// A store-less run surfaces extraction as a catchable script error.
#[tokio::test]
async fn storeless_extraction_is_catchable() {
    // build a host directly with no store, as a bare library consumer runs
    let host = ScriptHost {
        ghidra: GhidraSession::new(GhidraSettings::from_binary("bin/target", "projects", None)),
        store: None,
        handle: tokio::runtime::Handle::current(),
    };
    // a script that catches the extraction failure
    let source = r#"
        let result = "missed";
        try { extract_file("anything"); } catch (e) { result = `caught: ${e}`; }
        result
    "#;
    let output = Arc::new(Mutex::new(Vec::new()));
    let bindings: Vec<fn(&mut rhai::Engine, &ScriptHost)> =
        vec![crate::thorpal::tools::scripts::bindings::bind_extract_file];
    let value = execute_script(
        source,
        &serde_json::Value::Null,
        &bindings,
        &host,
        Instant::now() + Duration::from_secs(30),
        MAX_OPERATIONS,
        &output,
    )
    .expect("the catch should succeed");
    let rendered = value.as_str().unwrap_or("").to_owned();
    assert!(rendered.contains("caught:"), "not caught: {rendered}");
    assert!(rendered.contains("persistence"), "wrong error: {rendered}");
}

/// The builtin helpers are available to every script, no toolset needed.
#[tokio::test]
async fn builtins_are_always_available() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // a script exercising every builtin with no tools enabled at all
    let source = r#"
        let b = hex_to_blob("4d 5a 90 00");
        let hex = blob_to_hex(b);
        let dump = hexdump(b);
        let based = hexdump(b, 0x10);
        let c = to_char(65);
        // an invalid code point is a catchable error
        let caught = "missed";
        try { to_char(0xD800); } catch (e) { caught = "caught"; }
        #{ hex: hex, dump: dump, based: based, c: c, caught: caught, len: b.len() }
    "#;
    create_script(&mut context, "builtins".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    let run = run_script(&mut context, "builtins".to_owned(), None, None)
        .await
        .expect("the builtins should run");
    // hex_to_blob tolerates whitespace and round-trips through blob_to_hex
    assert_eq!(run.value["hex"], serde_json::json!("4d5a9000"));
    assert_eq!(run.value["len"], serde_json::json!(4));
    // the hexdump carries the classic layout, based at 0 or the given base
    assert!(
        run.value["dump"]
            .as_str()
            .unwrap_or("")
            .starts_with("00000000  4d 5a 90 00")
    );
    assert!(
        run.value["based"]
            .as_str()
            .unwrap_or("")
            .starts_with("00000010  4d 5a 90 00")
    );
    assert_eq!(run.value["c"], serde_json::json!("A"));
    // the surrogate code point was caught, not a crash
    assert_eq!(run.value["caught"], serde_json::json!("caught"));
    // bad hex is catchable too
    let catching = r#"
        let result = "missed";
        try { hex_to_blob("zz"); } catch (e) { result = `caught: ${e}`; }
        result
    "#;
    create_script(&mut context, "badhex".to_owned(), catching.to_owned())
        .await
        .expect("create should save");
    let run = run_script(&mut context, "badhex".to_owned(), None, None)
        .await
        .expect("the catch should run");
    assert!(run.value.as_str().unwrap_or("").starts_with("caught:"));
}

/// The docs carry the builtins section between the reference and the
/// host-function list, independent of the enabled toolset.
#[tokio::test]
async fn rhai_docs_list_the_builtins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    let docs = rhai_docs(&mut context).await.expect("docs should render");
    // the builtins section renders even with no tools enabled
    assert!(docs.contains("# Builtin helpers (always available)"));
    assert!(docs.contains("hex_to_blob"));
    assert!(docs.contains("to_char"));
    // and it sits before the host-function section
    let builtins = docs.find("# Builtin helpers").expect("builtins section");
    let hosts = docs.find("# Host functions").expect("host section");
    assert!(builtins < hosts);
}

/// A script chains read_file -> decode_bytes -> strings_range natively.
#[tokio::test]
async fn scripts_chain_the_decode_bindings() {
    let dir = tempfile::tempdir().expect("tempdir");
    // the sample is the xor-0x5a encoding of a config string
    let plain = b"panel=https://decode-chain.example/gate";
    let enc: Vec<u8> = plain.iter().map(|byte| byte ^ 0x5a).collect();
    let mut context = context_with_sample(dir.path(), &enc).await;
    // read the raw bytes, decode them natively, then string-scan the result
    let source = r#"
        let raw = read_file(0, 4096);
        let out = decode_bytes(raw, [#{ op: "xor", key_hex: "5a" }]);
        let strs = strings_range(out, 4);
        #{ text: out.as_string(), count: strs.len(), first: strs[0].text }
    "#;
    create_script(&mut context, "chain".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    // the bindings only exist once their tools are enabled
    enable(
        &mut context,
        &["read_file", "decode_bytes", "strings_range"],
    );
    let run = run_script(&mut context, "chain".to_owned(), None, None)
        .await
        .expect("the chain should run");
    assert_eq!(
        run.value["text"],
        serde_json::json!("panel=https://decode-chain.example/gate")
    );
    assert_eq!(run.value["count"], serde_json::json!(1));
    assert_eq!(
        run.value["first"],
        serde_json::json!("panel=https://decode-chain.example/gate")
    );
}

/// The grep and brute bindings return maps carrying their limit indicators.
#[tokio::test]
async fn script_grep_and_brute_report_their_limits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context_with_sample(dir.path(), b"unused").await;
    // grep a blob with a limit of one, and brute-force a sub key, in-script
    let source = r#"
        let data = hex_to_blob(args.grep_hex);
        let g = grep_bytes(data, ["key"], #{ limit: 1 });
        let enc = hex_to_blob(args.brute_hex);
        let sweep = byte_transform(enc, "sub");
        #{ hit_limit: g.hit_limit, scanned_to: g.scanned_to,
           found: g.matches.len(), first: g.matches[0].offset,
           tried: sweep.tried, best: sweep.candidates[0].key }
    "#;
    create_script(&mut context, "limits".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    enable(&mut context, &["grep_bytes", "byte_transform"]);
    // two grep hits under a limit of one, and an add-9 payload to sweep
    let grep_hex: String = b"xx KEY yy KEY zz"
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let plain = b"swept config: https://limits.example/gate entry point here";
    let brute_hex: String = plain
        .iter()
        .map(|byte| format!("{:02x}", byte.wrapping_add(9)))
        .collect();
    let run = run_script(
        &mut context,
        "limits".to_owned(),
        Some(serde_json::json!({ "grep_hex": grep_hex, "brute_hex": brute_hex })),
        None,
    )
    .await
    .expect("the script should run");
    // the grep map says the limit cut the scan and where to resume
    assert_eq!(run.value["hit_limit"], serde_json::json!(true));
    assert_eq!(run.value["found"], serde_json::json!(1));
    assert_eq!(run.value["first"], serde_json::json!("0x3"));
    assert_eq!(run.value["scanned_to"], serde_json::json!(10));
    // the brute map carries the sweep size and the winning key
    assert_eq!(run.value["tried"], serde_json::json!(256));
    assert_eq!(run.value["best"], serde_json::json!(9));
}

/// Scripts resolve API hashes and run byte transforms through the bindings.
#[tokio::test]
async fn scripts_match_api_hashes_and_transform() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context_with_sample(dir.path(), b"unused").await;
    // ror13("GetProcAddress"), computed in-script via args to stay honest
    let ror13 = |name: &str| -> u32 {
        name.bytes().fold(0u32, |hash, byte| {
            hash.rotate_right(13).wrapping_add(u32::from(byte))
        })
    };
    let target = ror13("GetProcAddress");
    // targets mix an int and a hex string; a transform round-trips too
    let source = r#"
        let hits = api_hash_match([args.target, args.target_hex], "ror13");
        let enc = byte_transform(hex_to_blob(args.enc_hex), "sub_from_key", 0x21);
        #{ name: hits.matched[0].name,
           unmatched: hits.unmatched.len(),
           decoded: enc.as_string() }
    "#;
    create_script(&mut context, "hashes".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    enable(&mut context, &["api_hash_match", "byte_transform"]);
    // the sub_from_key payload is the Lumma loader shape over "LoadLibraryA"
    let enc_hex: String = b"LoadLibraryA"
        .iter()
        .map(|byte| format!("{:02x}", 0x21u8.wrapping_sub(*byte)))
        .collect();
    let run = run_script(
        &mut context,
        "hashes".to_owned(),
        Some(serde_json::json!({
            "target": target,
            "target_hex": format!("{target:#x}"),
            "enc_hex": enc_hex,
        })),
        None,
    )
    .await
    .expect("the script should run");
    assert_eq!(run.value["name"], serde_json::json!("GetProcAddress"));
    assert_eq!(run.value["unmatched"], serde_json::json!(0));
    assert_eq!(run.value["decoded"], serde_json::json!("LoadLibraryA"));
}

/// Promotion writes the documented header and the listing surfaces it.
#[tokio::test]
async fn promote_writes_the_documented_script() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_library(dir.path()).await;
    // save a working session script and promote it with docs
    create_script(&mut context, "double".to_owned(), "args.x * 2".to_owned())
        .await
        .expect("create should save");
    let message = promote_script(
        &mut context,
        "double".to_owned(),
        "Doubles a number".to_owned(),
        "args: #{ x: int }; returns the doubled int".to_owned(),
        None,
        None,
    )
    .await
    .expect("promotion should succeed");
    assert!(message.contains("promoted script 'double'"));
    // the library copy carries the rendered header above the original body
    let promoted = library.load("double").await.expect("library load");
    assert!(promoted.starts_with("//! name: double\n"));
    assert!(promoted.contains("//! detector: false"));
    assert!(promoted.contains("//! description: Doubles a number"));
    assert!(promoted.ends_with("args.x * 2"));
    // the parsed metadata records the promoting session as provenance
    let meta = library.load_meta("double").await.expect("library meta");
    assert_eq!(meta.description, "Doubles a number");
    assert!(!meta.promoted_from.is_empty());
    // the combined listing shows both the session and library copies
    let listing = list_scripts(&mut context).await.expect("list");
    assert_eq!(listing.session, ["double"]);
    assert_eq!(listing.library.len(), 1);
    assert_eq!(listing.library[0].name, "double");
    assert!(!listing.library[0].detector);
}

/// Promotion refuses missing scripts, empty docs, and silent collisions.
#[tokio::test]
async fn promote_refusals_teach_the_flow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_library(dir.path()).await;
    // a script this session never saved cannot promote
    let error = promote_script(
        &mut context,
        "ghost".to_owned(),
        "docs".to_owned(),
        "docs".to_owned(),
        None,
        None,
    )
    .await
    .expect_err("a missing script should be refused");
    assert!(error.to_string().contains("ghost"));
    // an empty description defeats the library's searchability and is refused
    create_script(&mut context, "real".to_owned(), "1 + 1".to_owned())
        .await
        .expect("create should save");
    let error = promote_script(
        &mut context,
        "real".to_owned(),
        "  ".to_owned(),
        "docs".to_owned(),
        None,
        None,
    )
    .await
    .expect_err("an empty description should be refused");
    assert!(error.to_string().contains("description"));
    // a collision without overwrite is refused with the opt-in named
    library.save("real", "2 + 2").await.expect("seed collision");
    let error = promote_script(
        &mut context,
        "real".to_owned(),
        "adds".to_owned(),
        "no args".to_owned(),
        None,
        None,
    )
    .await
    .expect_err("a collision should be refused");
    assert!(error.to_string().contains("overwrite"));
    // passing overwrite publishes this session's version over it
    promote_script(
        &mut context,
        "real".to_owned(),
        "adds".to_owned(),
        "no args".to_owned(),
        Some(true),
        Some(true),
    )
    .await
    .expect("overwrite should publish");
    assert!(library.load("real").await.expect("load").ends_with("1 + 1"));
    // the detector flag landed in the published header
    assert!(library.load_meta("real").await.expect("meta").detector);
}

/// Session scripts shadow library scripts; library-only names fall back.
#[tokio::test]
async fn session_scripts_shadow_the_library() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_library(dir.path()).await;
    // a library-only script runs and reads through the fallback
    library
        .save("lib-only", "40 + 2")
        .await
        .expect("seed library");
    let run = run_script(&mut context, "lib-only".to_owned(), None, None)
        .await
        .expect("the library fallback should run");
    assert_eq!(run.origin, "library");
    assert_eq!(run.value, serde_json::json!(42));
    let source = read_script(&mut context, "lib-only".to_owned())
        .await
        .expect("the library fallback should read");
    assert_eq!(source, "40 + 2");
    // a same-named session script shadows the library version
    create_script(&mut context, "lib-only".to_owned(), "1 + 1".to_owned())
        .await
        .expect("the shadowing copy should save");
    let run = run_script(&mut context, "lib-only".to_owned(), None, None)
        .await
        .expect("the shadowing copy should run");
    assert_eq!(run.origin, "session");
    assert_eq!(run.value, serde_json::json!(2));
    // a name missing everywhere says both places were checked
    let error = run_script(&mut context, "nowhere".to_owned(), None, None)
        .await
        .expect_err("a missing name should be refused");
    assert!(error.to_string().contains("library"));
}

/// Library scripts are edit-protected with the copy-on-write recipe.
#[tokio::test]
async fn library_edits_teach_copy_on_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_library(dir.path()).await;
    library.save("locked", "1 + 1").await.expect("seed library");
    // edit and append on a library-only name both return the recipe
    let error = edit_script(
        &mut context,
        "locked".to_owned(),
        "1".to_owned(),
        "2".to_owned(),
        None,
    )
    .await
    .expect_err("a library edit should be refused");
    assert!(error.to_string().contains("copy-on-write"));
    let error = append_script(&mut context, "locked".to_owned(), "2".to_owned())
        .await
        .expect_err("a library append should be refused");
    assert!(error.to_string().contains("copy-on-write"));
    // the library copy is untouched either way
    assert_eq!(library.load("locked").await.expect("load"), "1 + 1");
}

/// Search spans both origins and matches library doc headers.
#[tokio::test]
async fn search_spans_session_and_library() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_library(dir.path()).await;
    // a session script and a documented library script
    create_script(&mut context, "mine".to_owned(), "needle_here()".to_owned())
        .await
        .expect("create should save");
    let meta =
        ScriptMeta::promoted_now("finds the needle_here marker", "no args", false, "past", "");
    library
        .save("theirs", &format!("{}\n1 + 1", meta.render("theirs")))
        .await
        .expect("seed library");
    // the search finds the session source and the library's doc header
    let matches = search_scripts(&mut context, "needle_here".to_owned())
        .await
        .expect("search");
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].script, "mine");
    assert_eq!(matches[0].origin, "session");
    assert_eq!(matches[1].script, "theirs");
    assert_eq!(matches[1].origin, "library");
    assert!(matches[1].text.contains("description"));
}

/// save_bytes carves a script-built payload into the workspace.
#[tokio::test]
async fn save_bytes_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // an MZ-headed payload earns the import_child nudge on its identity
    let source = r#"
        let saved = save_bytes("carved.bin", hex_to_blob("4d5a900003000000"));
        #{ name: saved.name, size: saved.info.size, note: saved.note,
           ident: file_info(saved.name).size }
    "#;
    create_script(&mut context, "carver".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    // save_bytes rides the decode_bytes toolset; file_info proves addressing
    enable(&mut context, &["decode_bytes", "file_info"]);
    let run = run_script(&mut context, "carver".to_owned(), None, None)
        .await
        .expect("the carve should succeed");
    assert_eq!(run.value["name"], serde_json::json!("carved.bin"));
    assert_eq!(run.value["size"], serde_json::json!(8));
    assert_eq!(run.value["ident"], serde_json::json!(8));
    // the deterministic nudge names the tool and the saved target
    let note = run.value["note"]
        .as_str()
        .expect("an executable gets a note");
    assert!(note.contains("import_child"), "unexpected note: {note}");
    assert!(note.contains("carved.bin"), "unexpected note: {note}");
}

/// A non-executable save gets no nudge and overwrite keeps the name stable.
#[tokio::test]
async fn save_bytes_overwrites_and_skips_the_nudge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // save twice under one name: the second (longer) payload wins
    let source = r#"
        save_bytes("blob.bin", hex_to_blob("01"));
        let saved = save_bytes("blob.bin", hex_to_blob("0203040506"));
        #{ name: saved.name, size: file_info(saved.name).size,
           has_note: saved.note != () }
    "#;
    create_script(&mut context, "resave".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    enable(&mut context, &["decode_bytes", "file_info"]);
    let run = run_script(&mut context, "resave".to_owned(), None, None)
        .await
        .expect("the resave should succeed");
    // plain bytes are not an executable, so no import nudge appears
    assert_eq!(run.value["name"], serde_json::json!("blob.bin"));
    assert_eq!(run.value["size"], serde_json::json!(5));
    assert_eq!(run.value["has_note"], serde_json::json!(false));
}

/// Traversal-shaped names are rejected as catchable script errors.
#[tokio::test]
async fn save_bytes_rejects_traversal_names() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut context = context(dir.path()).await;
    // the store's name validation fires before any byte is written
    let source = r#"
        let result = "missed";
        try { save_bytes("../escape.bin", hex_to_blob("00")); }
        catch (e) { result = `caught: ${e}`; }
        result
    "#;
    create_script(&mut context, "escape".to_owned(), source.to_owned())
        .await
        .expect("create should save");
    enable(&mut context, &["decode_bytes"]);
    let run = run_script(&mut context, "escape".to_owned(), None, None)
        .await
        .expect("the script itself should succeed");
    let result = run.value.as_str().expect("a string result");
    assert!(result.starts_with("caught:"), "unexpected result: {result}");
}

/// A store-less run surfaces save_bytes as a catchable script error.
#[tokio::test]
async fn storeless_save_bytes_is_catchable() {
    // build a host directly with no store, as a bare library consumer runs
    let host = ScriptHost {
        ghidra: GhidraSession::new(GhidraSettings::from_binary("bin/target", "projects", None)),
        store: None,
        handle: tokio::runtime::Handle::current(),
    };
    // a script that catches the save failure
    let source = r#"
        let result = "missed";
        try { save_bytes("x.bin", hex_to_blob("00")); }
        catch (e) { result = `caught: ${e}`; }
        result
    "#;
    let output = Arc::new(Mutex::new(Vec::new()));
    let bindings: Vec<fn(&mut rhai::Engine, &ScriptHost)> =
        vec![crate::thorpal::tools::scripts::bindings::bind_decode_bytes];
    let value = execute_script(
        source,
        &serde_json::Value::Null,
        &bindings,
        &host,
        Instant::now() + Duration::from_secs(30),
        MAX_OPERATIONS,
        &output,
    )
    .expect("the catch should succeed");
    let rendered = value.as_str().unwrap_or("").to_owned();
    assert!(rendered.contains("caught:"), "not caught: {rendered}");
    assert!(rendered.contains("persistence"), "wrong error: {rendered}");
}

/// Library reads redact provenance; session scripts stay verbatim.
#[tokio::test]
async fn library_reads_redact_provenance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_library(dir.path()).await;
    // seed a library script promoted from a family-named session
    let meta = ScriptMeta::promoted_now("finds widgets", "no args", false, "batch1-family_a", "");
    library
        .save("widget", &format!("{}\n1 + 1", meta.render("widget")))
        .await
        .expect("seed library");
    // the model-visible read carries the neutral id, never the raw name
    let source = read_script(&mut context, "widget".to_owned())
        .await
        .expect("read should load");
    assert!(
        source.contains("promoted-from: session-d47e86a7"),
        "unexpected header: {source}"
    );
    assert!(
        !source.contains("batch1-family_a"),
        "raw name leaked: {source}"
    );
    // a session script's own text is returned verbatim, even provenance-shaped
    create_script(
        &mut context,
        "mine".to_owned(),
        "//! promoted-from: batch1-family_a\n1 + 1".to_owned(),
    )
    .await
    .expect("create should save");
    let source = read_script(&mut context, "mine".to_owned())
        .await
        .expect("session read should load");
    assert!(
        source.contains("batch1-family_a"),
        "session text must survive"
    );
}

/// Search can never surface a raw library session name.
#[tokio::test]
async fn search_never_leaks_library_provenance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_library(dir.path()).await;
    // seed a library script whose only mention of the session is provenance
    let meta = ScriptMeta::promoted_now("finds gadgets", "no args", false, "batch1-family_a", "");
    library
        .save("gadget", &format!("{}\n2 + 2", meta.render("gadget")))
        .await
        .expect("seed library");
    // the raw session name misses; the neutral id is what search surfaces
    let matches = search_scripts(&mut context, "batch1-family_a".to_owned())
        .await
        .expect("search");
    assert!(matches.is_empty(), "raw name matched: {matches:?}");
    let matches = search_scripts(&mut context, "session-d47e86a7".to_owned())
        .await
        .expect("search");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].script, "gadget");
}

/// Builds a library-backed context that also carries a blinding redactor
///
/// # Arguments
///
/// * `dir` - The scratch root for the sessions and the library
/// * `anonymized` - Whether the simulated run is blinded
/// * `tokens` - The deny tokens the redactor scrubs
async fn context_with_redactor(
    dir: &Path,
    anonymized: bool,
    tokens: &[&str],
) -> (ToolContext, Arc<ScriptLibrary>) {
    // start from the library-backed context
    let (mut context, library) = context_with_library(dir).await;
    // seed the redactor exactly like ThorPal::tool_context does
    let redactor = LibraryRedactor::new(
        anonymized,
        tokens.iter().map(|token| (*token).to_owned()).collect(),
        Vec::new(),
    )
    .expect("a redactor should build");
    context.insert(redactor);
    (context, library)
}

/// Seeds one family-named library script with family-laden docs and body
///
/// # Arguments
///
/// * `library` - The library to seed into
async fn seed_family_script(library: &ScriptLibrary) -> String {
    // the name, description, and body all carry the family token
    let meta = ScriptMeta::promoted_now(
        "decodes DarkGate config strings",
        "no args; returns the DarkGate key",
        false,
        "batch1-family_b",
        "",
    );
    library
        .save(
            "darkgate_decode",
            &format!(
                "{}\nprint(\"DarkGate key found\");\n\"darkgate:done\"",
                meta.render("darkgate_decode")
            ),
        )
        .await
        .expect("seed library");
    // hand back the neutral id this name renders as under blinding
    crate::thorpal::store::redact::neutral_script_id("darkgate_decode")
}

/// Blinded listings and searches carry zero raw family tokens or names.
#[tokio::test]
async fn blinded_list_and_search_render_neutral_names_and_scrubbed_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_redactor(dir.path(), true, &["darkgate"]).await;
    let neutral = seed_family_script(&library).await;
    // the listing's library section renders the neutral id and scrubbed text
    let listing = list_scripts(&mut context).await.expect("list");
    assert_eq!(listing.library.len(), 1);
    assert_eq!(listing.library[0].name, neutral);
    // the audit criterion: zero raw tokens anywhere in the serialized result
    let serialized = serde_json::to_string(&listing).expect("listing serializes");
    assert!(
        !serialized.to_lowercase().contains("darkgate"),
        "raw family token leaked: {serialized}"
    );
    // search hits render the same neutral name with scrubbed lines
    let matches = search_scripts(&mut context, "decodes".to_owned())
        .await
        .expect("search");
    assert!(!matches.is_empty(), "the description line should match");
    let serialized = serde_json::to_string(&matches).expect("matches serialize");
    assert!(
        !serialized.to_lowercase().contains("darkgate"),
        "raw family token leaked from search: {serialized}"
    );
    assert_eq!(matches[0].script, neutral);
}

/// Neutral ids resolve for reads, runs, and the copy-on-write teach error.
#[tokio::test]
async fn neutral_ids_resolve_for_read_run_and_edit_teach() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_redactor(dir.path(), true, &["darkgate"]).await;
    let neutral = seed_family_script(&library).await;
    // reading by the neutral id loads the (scrubbed) library source
    let source = read_script(&mut context, neutral.clone())
        .await
        .expect("the neutral id should resolve");
    assert!(
        !source.to_lowercase().contains("darkgate"),
        "raw family token leaked from read: {source}"
    );
    // running by the neutral id executes the real script, with scrubbed output
    let run = run_script(&mut context, neutral.clone(), None, None)
        .await
        .expect("the neutral id should run");
    let serialized = serde_json::to_string(&run).expect("run serializes");
    assert!(
        !serialized.to_lowercase().contains("darkgate"),
        "raw family token leaked from run output: {serialized}"
    );
    // editing by the neutral id teaches the copy-on-write flow
    let error = edit_script(
        &mut context,
        neutral.clone(),
        "print".to_owned(),
        "debug".to_owned(),
        None,
    )
    .await
    .expect_err("library scripts are not edited in place");
    assert!(
        error.to_string().contains("copy-on-write"),
        "unexpected teach error: {error}"
    );
}

/// A copy-on-write round trip under a neutral id publishes to the real stem.
#[tokio::test]
async fn copy_on_write_under_a_neutral_id_promotes_to_the_real_stem() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_redactor(dir.path(), true, &["darkgate"]).await;
    let neutral = seed_family_script(&library).await;
    // the session copy saved under the neutral id shadows the library version
    create_script(&mut context, neutral.clone(), "\"v2\"".to_owned())
        .await
        .expect("the shadow copy should save");
    let run = run_script(&mut context, neutral.clone(), None, None)
        .await
        .expect("the shadow should run");
    assert_eq!(
        run.value,
        serde_json::json!("v2"),
        "the session copy shadows"
    );
    // promoting the shadow with overwrite publishes over the real stem
    promote_script(
        &mut context,
        neutral.clone(),
        "decodes vendor config strings".to_owned(),
        "no args".to_owned(),
        None,
        Some(true),
    )
    .await
    .expect("the promotion should publish");
    let published = library
        .load("darkgate_decode")
        .await
        .expect("the real stem should hold the new version");
    assert!(
        published.contains("\"v2\""),
        "the new body should land: {published}"
    );
    // no stray hash-named file was minted in the library
    assert!(
        !library
            .exists(&neutral)
            .await
            .expect("exists should answer"),
        "a stray neutral-id file was minted"
    );
}

/// Without a redactor every library surface stays byte-identical.
#[tokio::test]
async fn no_redactor_means_byte_identical_surfaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut context, library) = context_with_library(dir.path()).await;
    seed_family_script(&library).await;
    // the raw name and description render untouched
    let listing = list_scripts(&mut context).await.expect("list");
    assert_eq!(listing.library[0].name, "darkgate_decode");
    assert!(listing.library[0].description.contains("DarkGate"));
    // the raw body text reads back untouched (provenance aside)
    let source = read_script(&mut context, "darkgate_decode".to_owned())
        .await
        .expect("read");
    assert!(source.contains("DarkGate key found"));
    // run output stays raw too
    let run = run_script(&mut context, "darkgate_decode".to_owned(), None, None)
        .await
        .expect("run");
    assert_eq!(run.output, ["DarkGate key found"]);
}

/// Blinding closes the name-header leak and the raw-stem oracle together.
///
/// The class this pins: abbreviation-derived stems (`dg_*`) that no
/// deny-token list will ever cover must neither render anywhere nor resolve.
#[tokio::test]
async fn blinded_runs_hide_raw_stems_everywhere() {
    let dir = tempfile::tempdir().expect("tempdir");
    // blinded with NO deny tokens: name hygiene must not depend on the list
    let (mut context, library) = context_with_redactor(dir.path(), true, &[]).await;
    // seed an abbreviation-named script whose header carries the raw stem
    let meta =
        ScriptMeta::promoted_now("extracts embedded config", "no args", false, "batch1-x", "");
    library
        .save(
            "dg_extract_config",
            &format!("{}\n\"ok\"", meta.render("dg_extract_config")),
        )
        .await
        .expect("seed library");
    let neutral = crate::thorpal::store::redact::neutral_script_id("dg_extract_config");
    // the listing shows only the neutral id
    let listing = list_scripts(&mut context).await.expect("list");
    assert_eq!(listing.library[0].name, neutral);
    // reading by the neutral id neutralizes the `//! name:` header line too
    let source = read_script(&mut context, neutral.clone())
        .await
        .expect("the neutral id should resolve");
    assert!(
        source.starts_with(&format!("//! name: {neutral}")),
        "the name header must render the neutral id: {source}"
    );
    assert!(
        !source.contains("dg_extract_config"),
        "raw stem leaked: {source}"
    );
    // searching cannot surface the raw stem from any line either
    let matches = search_scripts(&mut context, "dg_extract".to_owned())
        .await
        .expect("search");
    assert!(matches.is_empty(), "the raw stem matched: {matches:?}");
    // the raw stem does not resolve: no existence/content oracle
    let error = read_script(&mut context, "dg_extract_config".to_owned())
        .await
        .expect_err("the raw stem must not resolve under blinding");
    assert!(
        error.to_string().contains("no script named"),
        "oracle: {error}"
    );
    // the edit path answers with the plain miss, never the teach message
    let error = edit_script(
        &mut context,
        "dg_extract_config".to_owned(),
        "a".to_owned(),
        "b".to_owned(),
        None,
    )
    .await
    .expect_err("the raw stem must not teach copy-on-write under blinding");
    assert!(
        !error.to_string().contains("copy-on-write"),
        "the teach message confirms existence: {error}"
    );
}
