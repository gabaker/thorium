//! Tools for creating, editing, searching, and running Rhai analysis scripts.
//!
//! Scripts let an agent run ad-hoc computations that would be unreliable by
//! hand -- decoding obfuscated data, scanning functions for complex patterns,
//! or crunching bytes pulled out of the binary. Every script lives in the
//! session's `scripts/` folder (see [`SessionStore`]), is compile-checked
//! before it is saved, and runs inside an embedded [`rhai`] engine with an
//! operation cap and a wall-clock deadline so a runaway script can never hang
//! a run.
//!
//! Scripts that prove useful can be promoted (via `promote_script`) into the
//! persistent cross-session library (see [`ScriptLibrary`]), where they carry
//! an embedded doc header future runs search to find applicable scripts. The
//! read-side tools resolve names session-first: a session script always
//! shadows a same-named library script, and library scripts are edited
//! copy-on-write (create a shadowing session script, then promote with
//! overwrite).
//!
//! The functions a script can call are not fixed: every registry tool may
//! carry a Rhai binding (see `rhai_bind` on
//! [`ToolEntry`](crate::thorpal::tools::registry::ToolEntry)), and `run_script`
//! registers exactly the bindings of the running agent's resolved toolset.
//! Disabling a tool in the workflow therefore also disables its script
//! binding.
//!
//! A running script's `print`/`debug` output is captured for the tool result
//! and simultaneously emitted as `tracing` lines under the `rhai_script`
//! target, which `run_script` declares via `logs = "rhai_script"`, so the
//! output streams live into the chat's log window exactly like Ghidra output.

pub mod bindings;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rig::tool::{ToolContext, ToolExecutionError};
use serde::Serialize;

use crate::thorpal::store::SessionStore;
use crate::thorpal::store::library::{LibraryEntry, ScriptLibrary, ScriptMeta, strip_header};
use crate::thorpal::store::redact::{LibraryRedactor, is_neutral_script_id, neutral_script_id};
use crate::thorpal::tools::ghidra::GhidraSession;
use crate::thorpal::tools::registry::EnabledTools;

/// The default wall-clock deadline for a script run, in seconds
pub(crate) const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// The longest wall-clock deadline a script run may ask for, in seconds
const MAX_TIMEOUT_SECS: u64 = 600;

/// The max number of engine operations a script may execute
///
/// The wall-clock deadline is the real guard against runaway scripts: rhai
/// executes on the order of 10M ops/sec, so this cap exceeds what 600s of
/// pure compute can reach and only backstops a stalled clock. It is a flat
/// cap on purpose -- scaling it with `timeout_secs` would add a second knob
/// without adding any safety.
pub(crate) const MAX_OPERATIONS: u64 = 5_000_000_000;

/// Check the wall-clock deadline only once per this many engine ops
///
/// A power-of-two mask so the check is a single AND; 64K ops of overshoot
/// is milliseconds, while an `Instant::now()` per op would dominate the cost
/// of cheap operations.
const DEADLINE_CHECK_MASK: u64 = (1 << 16) - 1;

/// The max nesting depth of a global-level expression a script may parse
///
/// Rhai's debug-build defaults (32 globally, 16 inside a `fn`) reject the
/// nested literals LLM-written scripts routinely use. 128 gives generous
/// headroom while staying finite: parsing is recursive, so an unlimited
/// depth would let a degenerate source overflow the parser's stack.
const MAX_EXPR_DEPTH: usize = 128;

/// The max nesting depth of an expression inside a script `fn`
const MAX_FUNCTION_EXPR_DEPTH: usize = 128;

/// The max bytes of rendered value/output a script run returns to the model
const MAX_RESULT_BYTES: usize = 32 * 1024;

/// The full Rhai language reference served by the `rhai_docs` tool
const RHAI_REFERENCE: &str = include_str!("../../../rhai_reference.md");

/// The documentation for the always-available builtin helpers
///
/// Appended to the language reference by `rhai_docs`, between the reference
/// and the toolset-dependent host-function list, because the builtins are
/// registered on every engine regardless of the enabled toolset.
const BUILTIN_DOCS: &str = "
# Builtin helpers (always available)

These helpers are registered on every script engine, independent of your
enabled tools:

- `to_char(code: int) -> string` -- the character for a Unicode code point
  (e.g. `to_char(65)` is `\"A\"`); an invalid code point is a catchable error.
- `hex_to_blob(hex: string) -> blob` -- decode a hex string (whitespace is
  ignored) into a blob; invalid hex is a catchable error.
- `blob_to_hex(data: blob) -> string` -- render a blob as lowercase hex.
- `hexdump(data: blob) -> string` and `hexdump(data, base: int) -> string` --
  the classic 16-byte-per-row hexdump with an ASCII gutter, offsets starting
  at `base` (default 0).
";

/// Registers the always-available builtin helpers on a script engine
///
/// These are pure helpers every script needs sooner or later (character and
/// hex conversions, hexdumps), so they are registered on every engine rather
/// than being gated behind a toolset like the host-function bindings.
///
/// # Arguments
///
/// * `engine` - The engine to register the builtins into
pub(crate) fn register_builtins(engine: &mut rhai::Engine) {
    // to_char: the character for a code point, catchable on invalid input
    engine.register_fn(
        "to_char",
        |code: i64| -> Result<String, Box<rhai::EvalAltResult>> {
            // reject anything outside the valid scalar-value range
            u32::try_from(code)
                .ok()
                .and_then(char::from_u32)
                .map(String::from)
                .ok_or_else(|| script_err(format!("{code} is not a valid Unicode code point")))
        },
    );
    // hex_to_blob: decode hex (whitespace tolerated) into a blob
    engine.register_fn(
        "hex_to_blob",
        |hex: &str| -> Result<rhai::Blob, Box<rhai::EvalAltResult>> {
            // strip whitespace so pasted spaced dumps decode too
            let cleaned: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
            bindings::decode_hex(&cleaned)
        },
    );
    // blob_to_hex: render a blob as lowercase hex
    engine.register_fn("blob_to_hex", |data: rhai::Blob| -> String {
        crate::thorpal::detect::fileinfo::hex(&data)
    });
    // hexdump: the classic dump, based at 0 or at a caller-given offset
    engine.register_fn("hexdump", |data: rhai::Blob| -> String {
        crate::thorpal::tools::file::hexdump(&data, 0)
    });
    engine.register_fn(
        "hexdump",
        |data: rhai::Blob, base: i64| -> Result<String, Box<rhai::EvalAltResult>> {
            // a negative base would render nonsense offsets
            let base = u64::try_from(base).map_err(|_| script_err("base must be >= 0"))?;
            Ok(crate::thorpal::tools::file::hexdump(&data, base))
        },
    );
}

/// The host handles a script's registered bindings call back into
///
/// Built once per `run_script` call on the async side and moved into the
/// blocking script thread, where each binding uses the captured runtime
/// handle to `block_on` the async Ghidra calls.
#[derive(Clone)]
pub struct ScriptHost {
    /// The shared Ghidra session read-only bindings query
    pub ghidra: Arc<GhidraSession>,
    /// The session store whose extracted workspace file bindings may target,
    /// or `None` when the run has session persistence disabled
    pub store: Option<Arc<SessionStore>>,
    /// The runtime handle sync bindings use to block_on async Ghidra calls
    pub handle: tokio::runtime::Handle,
}

/// Converts a host-side failure into a Rhai runtime error a script can catch
///
/// # Arguments
///
/// * `error` - The host-side failure to surface into the script
pub(crate) fn script_err(error: impl std::fmt::Display) -> Box<rhai::EvalAltResult> {
    // render the failure (with any anyhow chain) as a catchable runtime error
    Box::new(rhai::EvalAltResult::ErrorRuntime(
        format!("{error:#}").into(),
        rhai::Position::NONE,
    ))
}

/// A finished script run's value and captured print/debug output
#[derive(Debug, Clone, Serialize)]
pub struct ScriptRun {
    /// Where the executed script lives: "session" or "library"
    pub origin: &'static str,
    /// The script's final expression, serialized to JSON
    pub value: serde_json::Value,
    /// The print/debug lines the script emitted while running
    pub output: Vec<String>,
    /// A note describing any truncation applied to the value or output
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<String>,
}

/// One matched line from a search over the saved and library scripts
#[derive(Debug, Clone, Serialize)]
pub struct ScriptMatch {
    /// The name of the script this match is in
    pub script: String,
    /// Where the matched script lives: "session" or "library"
    pub origin: &'static str,
    /// The 1-indexed line number of the match
    pub line: usize,
    /// The matched line's text
    pub text: String,
}

/// The combined session and library script listing
#[derive(Debug, Clone, Serialize)]
pub struct ScriptListing {
    /// The bare names of this session's own scripts
    pub session: Vec<String>,
    /// The persistent library's scripts with their descriptions and flags
    pub library: Vec<LibraryEntry>,
}

/// Where a resolved script's source came from
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptOrigin {
    /// The script lives in this session's own scripts folder
    Session,
    /// The script lives in the persistent cross-session library
    Library,
}

impl ScriptOrigin {
    /// The lowercase label used in tool results and search matches
    fn label(self) -> &'static str {
        match self {
            ScriptOrigin::Session => "session",
            ScriptOrigin::Library => "library",
        }
    }
}

/// Builds a Rhai engine carrying the shared parse-time limits
///
/// [`compile_check`] and [`execute_script`] must both construct their engines
/// here: a script that compile-checked at save time must never fail to
/// re-parse at run time under a stricter limit.
fn base_engine() -> rhai::Engine {
    // start from a stock engine; run-time-only settings stay in execute_script
    let mut engine = rhai::Engine::new();
    // raise the nesting limits past rhai's debug defaults so reasonably
    // nested literals compile, while a finite cap still protects the
    // recursive parser's stack from degenerate sources
    engine.set_max_expr_depths(MAX_EXPR_DEPTH, MAX_FUNCTION_EXPR_DEPTH);
    engine
}

/// Compile-check a script source without running it
///
/// Function names resolve at call time in Rhai, so this only catches syntax
/// errors; a call to an unknown function surfaces when the script runs.
///
/// # Arguments
///
/// * `source` - The script source to compile-check
pub(crate) fn compile_check(source: &str) -> Result<(), String> {
    // compile on the shared base engine so parse limits match run time;
    // bindings are irrelevant to parsing
    match base_engine().compile(source) {
        Ok(_) => Ok(()),
        Err(error) => {
            // append a fix-it hint when the parser hit the nesting limit
            let hint = if matches!(*error.0, rhai::ParseErrorType::ExprTooDeep) {
                "; hint: flatten the script -- hoist nested map/array literals \
                 into intermediate `let` bindings, split long index/method \
                 chains into steps, and move complex closure bodies into \
                 named fns"
            } else {
                ""
            };
            // the parse error's rendering includes the line/position
            Err(format!("{error}{hint}"))
        }
    }
}

/// Pulls the shared session store out of a tool context as a tool error
///
/// # Arguments
///
/// * `context` - The tool call context the run was prompted with
fn store_from_context(context: &ToolContext) -> Result<Arc<SessionStore>, ToolExecutionError> {
    // a store-less run (a bare library consumer) has nowhere to keep scripts
    SessionStore::from_context(context)
}

/// Renders a loaded library source through every model-visible redaction
///
/// # Arguments
///
/// * `source` - The raw library source that was loaded
/// * `redactor` - The blinding redactor, when one is configured for this run
fn render_library_source(source: &str, redactor: Option<&Arc<LibraryRedactor>>) -> String {
    // redact session-name provenance on every model-visible library read
    // (headers are comments, so execution through run_script is unchanged
    // and copy-on-write session copies are born redacted)
    let source = crate::thorpal::store::library::redact_provenance(&source);
    // under blinding the `//! name:` header line must render the neutral id
    // (the raw stem is exactly what the listing withheld) and deny-listed
    // tokens scrub out of the whole source
    match redactor {
        Some(redactor) => redactor.redact_text(&redactor.redact_header_name(&source)),
        None => source,
    }
}

/// Resolves a script's source session-first, falling back to the library
///
/// A session script always shadows a same-named library script, so an agent
/// iterating on a copied library script runs its own version (blinded runs
/// render library names as neutral ids, and a session copy saved under the
/// neutral id shadows the same way). The miss error names both places that
/// were checked.
///
/// # Arguments
///
/// * `store` - The session store whose scripts shadow the library
/// * `library` - The persistent library, when one is configured for this run
/// * `redactor` - The blinding redactor, when one is configured for this run
/// * `name` - The bare script name to resolve
async fn load_any(
    store: &SessionStore,
    library: Option<&Arc<ScriptLibrary>>,
    redactor: Option<&Arc<LibraryRedactor>>,
    name: &str,
) -> Result<(String, ScriptOrigin), ToolExecutionError> {
    // the session's own script always wins so in-progress edits shadow the library
    match store.script_exists(name).await {
        Ok(true) => {
            let source = store
                .load_script(name)
                .await
                .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
            return Ok((source, ScriptOrigin::Session));
        }
        Ok(false) => (),
        Err(error) => return Err(ToolExecutionError::invalid_args(error.to_string())),
    }
    // fall back to the persistent library when one is configured; a name the
    // redactor hides behind a neutral id must NOT resolve raw -- the model
    // only ever saw the neutral id, so a raw-stem hit would be an
    // existence/content oracle on the names blinding withholds
    if let Some(library) = library
        && redactor.is_none_or(|redactor| !redactor.hides(name))
        && library
            .exists(name)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?
    {
        let source = library
            .load(name)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
        return Ok((
            render_library_source(&source, redactor),
            ScriptOrigin::Library,
        ));
    }
    // a neutral id (what blinded runs render library names as, and what prior
    // blinded runs baked into cross-run memory text) resolves back to the
    // real stem it aliases -- with or without a redactor, and over EVERY
    // stem, so baked lib-<8 hex> pointers stay followable in production,
    // deny-only, and blinded runs alike. Scripts have no hidden class, so
    // scanning every stem leaks nothing (a plainly-rendered stem's id adds
    // no information the listing does not already show), and the resolved
    // source still renders through the redactor
    if let Some(library) = library
        && is_neutral_script_id(name)
    {
        // scan the stems for the (unique) one hashing to this id
        let entries = library
            .list()
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
        let mut resolved = None;
        for entry in entries {
            if neutral_script_id(&entry.name) == name {
                // two stems hashing identically must surface, never pick one
                if resolved.is_some() {
                    return Err(ToolExecutionError::invalid_args(format!(
                        "the id '{name}' matches more than one library script; \
                         use the names list_scripts shows"
                    )));
                }
                resolved = Some(entry.name);
            }
        }
        if let Some(real) = resolved {
            let source = library
                .load(&real)
                .await
                .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
            return Ok((
                render_library_source(&source, redactor),
                ScriptOrigin::Library,
            ));
        }
    }
    // the name misses everywhere; say exactly where was checked
    Err(ToolExecutionError::invalid_args(format!(
        "no script named '{name}' exists in this session or the library; \
         list_scripts shows both"
    )))
}

/// Loads a script for editing, which only session scripts allow
///
/// Library scripts are immutable from a run's point of view and edited
/// copy-on-write, so a library-only name errors with the exact flow to follow
/// instead of a bare miss.
///
/// # Arguments
///
/// * `store` - The session store whose scripts are editable
/// * `library` - The persistent library, when one is configured for this run
/// * `redactor` - The blinding redactor, when one is configured for this run
/// * `name` - The bare script name to load for editing
async fn load_for_edit(
    store: &SessionStore,
    library: Option<&Arc<ScriptLibrary>>,
    redactor: Option<&Arc<LibraryRedactor>>,
    name: &str,
) -> Result<String, ToolExecutionError> {
    // an existing session script loads for editing like before
    match store.script_exists(name).await {
        Ok(true) => store
            .load_script(name)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string())),
        Ok(false) => {
            // a library-only name teaches the copy-on-write flow; a neutral
            // alias (what blinded runs render library names as) counts too,
            // while a redactor-hidden raw stem must stay a plain miss (the
            // teach message would otherwise confirm the name exists)
            if let Some(library) = library {
                let mut in_library = redactor.is_none_or(|redactor| !redactor.hides(name))
                    && library.exists(name).await.unwrap_or(false);
                if !in_library && let Some(redactor) = redactor {
                    in_library = redactor
                        .resolve_alias(library, name)
                        .await
                        .ok()
                        .flatten()
                        .is_some();
                }
                if in_library {
                    return Err(ToolExecutionError::invalid_args(format!(
                        "'{name}' is a library script; library scripts are edited \
                         copy-on-write: read_script it, create_script a session script \
                         named '{name}' with your changes (it shadows the library \
                         version), and once it works promote_script it with overwrite \
                         to publish the new version"
                    )));
                }
            }
            // the name misses everywhere; keep the plain miss message
            Err(ToolExecutionError::invalid_args(format!(
                "no script named '{name}' exists in this session; list_scripts \
                 shows what is saved"
            )))
        }
        Err(error) => Err(ToolExecutionError::invalid_args(error.to_string())),
    }
}

/// Executes a compiled-in script source on a fresh engine (blocking)
///
/// This runs on a `spawn_blocking` thread: the engine (and every `!Send` rhai
/// value) is constructed and dropped entirely inside this call, and the
/// result crosses back to the async side only after conversion to a
/// `serde_json::Value`.
///
/// # Arguments
///
/// * `source` - The script source to run
/// * `args` - The JSON value exposed to the script as the `args` constant
/// * `bindings` - The binding registrars of the running agent's enabled tools
/// * `host` - The host handle the bindings call back into
/// * `deadline` - The wall-clock instant the script is terminated at
/// * `max_operations` - The engine operation cap backstopping the deadline
/// * `output` - The shared buffer print/debug lines are captured into
pub(crate) fn execute_script(
    source: &str,
    args: &serde_json::Value,
    bindings: &[fn(&mut rhai::Engine, &ScriptHost)],
    host: &ScriptHost,
    deadline: Instant,
    max_operations: u64,
    output: &Arc<Mutex<Vec<String>>>,
) -> Result<serde_json::Value, String> {
    // build a fresh engine (with the shared parse limits) so no state leaks
    // between script runs and a saved script always re-parses here
    let mut engine = base_engine();
    // the builtin helpers are always available, before any toolset bindings
    register_builtins(&mut engine);
    // cap raw operation count as a backstop behind the wall-clock deadline
    engine.set_max_operations(max_operations);
    // terminate the script once the wall-clock deadline passes; this is the
    // reliable kill switch since an async timeout cannot stop a blocking thread
    engine.on_progress(move |ops| {
        // read the clock only every 64K ops; the counter makes this nearly free
        if ops & DEADLINE_CHECK_MASK == 0 && Instant::now() > deadline {
            Some(
                "script wall-clock deadline exceeded; split the work into \
                 smaller batches driven by `args` (e.g. skip/limit or an \
                 address range) across several run_script calls, or raise \
                 timeout_secs (max 600)"
                    .into(),
            )
        } else {
            None
        }
    });
    // capture print lines and stream them live under the rhai_script target
    let print_buffer = Arc::clone(output);
    engine.on_print(move |line| {
        tracing::info!(target: "rhai_script", "{line}");
        if let Ok(mut buffer) = print_buffer.lock() {
            buffer.push(line.to_owned());
        }
    });
    // capture debug lines too, prefixed so they read distinctly in the log
    let debug_buffer = Arc::clone(output);
    engine.on_debug(move |line, _source, _pos| {
        let line = format!("[debug] {line}");
        tracing::info!(target: "rhai_script", "{line}");
        if let Ok(mut buffer) = debug_buffer.lock() {
            buffer.push(line);
        }
    });
    // register exactly the bindings the running agent's toolset enables
    for bind in bindings {
        bind(&mut engine, host);
    }
    // expose the caller-provided args to the script as a constant
    let mut scope = rhai::Scope::new();
    let args = rhai::serde::to_dynamic(args)
        .map_err(|error| format!("failed to convert args for the script: {error}"))?;
    scope.push_constant("args", args);
    // run the script, converting its final expression back to JSON while we
    // are still on this thread (Dynamic values must not cross threads)
    match engine.eval_with_scope::<rhai::Dynamic>(&mut scope, source) {
        // a script with no final expression yields unit, which maps to null
        Ok(value) if value.is_unit() => Ok(serde_json::Value::Null),
        Ok(value) => match rhai::serde::from_dynamic(&value) {
            Ok(json) => Ok(json),
            // a non-serializable value still comes back as its rendering
            Err(_) => Ok(serde_json::Value::String(value.to_string())),
        },
        // a termination renders its token (rhai's own display drops it), so
        // the deadline reason survives to the model
        Err(error) => match *error {
            rhai::EvalAltResult::ErrorTerminated(token, _) => {
                Err(format!("script terminated: {token}"))
            }
            // exhausting the op cap gets a batching hint, since the raw
            // rendering ("Too many operations") gives the model nothing to act on
            rhai::EvalAltResult::ErrorTooManyOperations(pos) => Err(format!(
                "script hit the engine operation cap at {pos}; the work is too \
                 big for one run. Batch it: accept skip/limit (or an address \
                 range) through `args` and call run_script repeatedly over \
                 smaller slices, or raise timeout_secs (max 600) if the script \
                 is simply slow"
            )),
            // any other eval error's rendering includes the failing position
            other => Err(other.to_string()),
        },
    }
}

/// Truncate the value and output of a finished run to a sane total size
///
/// # Arguments
///
/// * `origin` - Where the executed script lives (session or library)
/// * `value` - The script's serialized final expression
/// * `output` - The captured print/debug lines
fn truncate_run(
    origin: ScriptOrigin,
    value: serde_json::Value,
    mut output: Vec<String>,
) -> ScriptRun {
    // track what was cut so the model knows the run itself succeeded in full
    let mut notes = Vec::new();
    // cap the rendered value, replacing it with a truncated rendering
    let rendered = value.to_string();
    let value = if rendered.len() > MAX_RESULT_BYTES {
        notes.push(format!("value truncated to {MAX_RESULT_BYTES} bytes"));
        // cut on a char boundary so the truncated rendering stays valid utf-8
        let mut cut = MAX_RESULT_BYTES;
        while !rendered.is_char_boundary(cut) {
            cut -= 1;
        }
        serde_json::Value::String(rendered[..cut].to_owned())
    } else {
        value
    };
    // cap the captured output lines at the same total budget
    let mut total = 0;
    let mut kept = 0;
    for line in &output {
        // stop keeping lines once the running total passes the budget
        if total + line.len() > MAX_RESULT_BYTES {
            break;
        }
        total += line.len();
        kept += 1;
    }
    if kept < output.len() {
        notes.push(format!(
            "output truncated to the first {kept} of {} lines",
            output.len()
        ));
        output.truncate(kept);
    }
    ScriptRun {
        origin: origin.label(),
        value,
        output,
        truncated: (!notes.is_empty()).then(|| notes.join("; ")),
    }
}

#[thorium_derive::thorpal_tool(group = "scripts")]
#[rig::tool_macro(
    description = "Create a new Rhai analysis script in the session's scripts folder. The source is compile-checked first and rejected (nothing saved) on a syntax error. Fails if the name is already taken in this session; use edit_script to change an existing session script. Reusing a library script's name is allowed and shadows it (the copy-on-write way to edit a library script). Keep sources small (a few KB) -- very large tool calls can be truncated; build long scripts by saving a working core and extending it with append_script/edit_script. Call rhai_docs for the language reference and the host functions your scripts can call."
)]
pub async fn create_script(
    #[rig(context)] context: &mut ToolContext,
    name: String,
    source: String,
) -> Result<String, rig::tool::ToolExecutionError> {
    // recover this run's session store so the script has a durable home
    let store = store_from_context(context)?;
    // refuse to silently overwrite an existing script
    match store.script_exists(&name).await {
        Ok(true) => {
            return Err(ToolExecutionError::invalid_args(format!(
                "a script named '{name}' already exists; use edit_script to change it"
            )));
        }
        Ok(false) => (),
        Err(error) => return Err(ToolExecutionError::invalid_args(error.to_string())),
    }
    // reject sources that do not even parse, so saved scripts always compile
    if let Err(error) = compile_check(&source) {
        return Err(ToolExecutionError::invalid_args(format!(
            "script '{name}' failed to compile and was not saved: {error}"
        )));
    }
    // persist the script into the session's scripts folder
    match store.save_script(&name, &source).await {
        Ok(()) => Ok(format!(
            "saved script '{name}' ({} bytes); execute it with run_script",
            source.len()
        )),
        Err(error) => Err(ToolExecutionError::invalid_args(error.to_string())),
    }
}

#[thorium_derive::thorpal_tool(group = "scripts")]
#[rig::tool_macro(
    description = "Edit an existing Rhai script by exact find/replace. `find` must match the current source (set replace_all to true to change every occurrence; otherwise it must match exactly once). The edited source is compile-checked and the edit is rejected (old version kept) on a syntax error."
)]
pub async fn edit_script(
    #[rig(context)] context: &mut ToolContext,
    name: String,
    find: String,
    replace: String,
    replace_all: Option<bool>,
) -> Result<String, rig::tool::ToolExecutionError> {
    // an empty needle would match everywhere and edit nothing meaningful
    if find.is_empty() {
        return Err(ToolExecutionError::invalid_args(
            "`find` must not be empty".to_owned(),
        ));
    }
    // recover this run's session store and the script's current source; a
    // library-only name errors with the copy-on-write flow instead
    let store = store_from_context(context)?;
    let library = ScriptLibrary::try_from_context(context);
    let redactor = LibraryRedactor::try_from_context(context);
    let source = load_for_edit(&store, library.as_ref(), redactor.as_ref(), &name).await?;
    // count the matches so ambiguous single replacements are rejected
    let matches = source.matches(&find).count();
    if matches == 0 {
        return Err(ToolExecutionError::invalid_args(format!(
            "`find` text not found in script '{name}'; read_script to see its current source"
        )));
    }
    let replace_all = replace_all.unwrap_or(false);
    if matches > 1 && !replace_all {
        return Err(ToolExecutionError::invalid_args(format!(
            "`find` text matches {matches} times in script '{name}'; pass replace_all \
             or a more specific `find`"
        )));
    }
    // apply the replacement(s) to a candidate source
    let edited = if replace_all {
        source.replace(&find, &replace)
    } else {
        source.replacen(&find, &replace, 1)
    };
    // reject edits that break the script, keeping the old version on disk
    if let Err(error) = compile_check(&edited) {
        return Err(ToolExecutionError::invalid_args(format!(
            "the edit breaks script '{name}' and was not applied: {error}"
        )));
    }
    // persist the edited script over the old version
    match store.save_script(&name, &edited).await {
        Ok(()) => Ok(format!(
            "edited script '{name}' ({matches} replacement{})",
            if matches == 1 { "" } else { "s" }
        )),
        Err(error) => Err(ToolExecutionError::invalid_args(error.to_string())),
    }
}

#[thorium_derive::thorpal_tool(group = "scripts")]
#[rig::tool_macro(
    description = "Append source to the end of an existing Rhai script, so long scripts can be built in small chunks (e.g. one complete `fn` per call). The combined source is compile-checked and the append is rejected (old version kept) if it does not parse, so append syntactically complete blocks."
)]
pub async fn append_script(
    #[rig(context)] context: &mut ToolContext,
    name: String,
    source: String,
) -> Result<String, rig::tool::ToolExecutionError> {
    // recover this run's session store and the script's current source; a
    // library-only name errors with the copy-on-write flow instead
    let store = store_from_context(context)?;
    let library = ScriptLibrary::try_from_context(context);
    let redactor = LibraryRedactor::try_from_context(context);
    let existing = load_for_edit(&store, library.as_ref(), redactor.as_ref(), &name).await?;
    // build the combined source the script would become
    let combined = format!("{existing}\n{source}");
    // reject appends that break the script, keeping the old version on disk
    if let Err(error) = compile_check(&combined) {
        return Err(ToolExecutionError::invalid_args(format!(
            "the appended source breaks script '{name}' and was not applied: \
             {error} -- append complete top-level blocks (whole fns/statements)"
        )));
    }
    // persist the extended script over the old version
    match store.save_script(&name, &combined).await {
        Ok(()) => Ok(format!(
            "appended {} bytes to script '{name}' ({} bytes total)",
            source.len(),
            combined.len()
        )),
        Err(error) => Err(ToolExecutionError::invalid_args(error.to_string())),
    }
}

#[thorium_derive::thorpal_tool(group = "scripts")]
#[rig::tool_macro(
    description = "Read the source of a saved Rhai script by name. Session scripts shadow same-named library scripts; a library script's source starts with its //! doc header (description, usage, detector flag)."
)]
pub async fn read_script(
    #[rig(context)] context: &mut ToolContext,
    name: String,
) -> Result<String, rig::tool::ToolExecutionError> {
    // resolve the script session-first, falling back to the library
    let store = store_from_context(context)?;
    let library = ScriptLibrary::try_from_context(context);
    let redactor = LibraryRedactor::try_from_context(context);
    let (source, _) = load_any(&store, library.as_ref(), redactor.as_ref(), &name).await?;
    Ok(source)
}

#[thorium_derive::thorpal_tool(group = "scripts")]
#[rig::tool_macro(
    description = "List every available Rhai script: this session's own scripts (bare names) plus the persistent library's scripts with their descriptions and detector flags. Check the library before writing a new script -- a documented, proven script may already do the job."
)]
pub async fn list_scripts(
    #[rig(context)] context: &mut ToolContext,
) -> Result<ScriptListing, rig::tool::ToolExecutionError> {
    // recover this run's session store and list its saved scripts
    let store = store_from_context(context)?;
    let session = match store.list_scripts().await {
        Ok(scripts) => scripts,
        Err(error) => return Err(ToolExecutionError::invalid_args(error.to_string())),
    };
    // append the persistent library's listing when one is configured
    let library = match ScriptLibrary::try_from_context(context) {
        Some(library) => library
            .list()
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?,
        // a library-less run simply lists an empty library section
        None => Vec::new(),
    };
    // render the library section through the blinding redactor when one is
    // configured: names neutralize and deny-listed tokens scrub out of the
    // descriptions (session scripts are the model's own naming and stay raw)
    let library = match LibraryRedactor::try_from_context(context) {
        Some(redactor) => library
            .into_iter()
            .map(|entry| LibraryEntry {
                name: redactor.display_name(&entry.name),
                description: redactor.redact_text(&entry.description),
                detector: entry.detector,
            })
            .collect(),
        None => library,
    };
    Ok(ScriptListing { session, library })
}

#[thorium_derive::thorpal_tool(group = "scripts")]
#[rig::tool_macro(
    description = "Search every saved and library Rhai script for a case-insensitive substring, returning each matching line with its script name, origin (session or library), and line number. Library scripts' doc headers (description/usage) are searched too, so this finds applicable library scripts by what they do."
)]
pub async fn search_scripts(
    #[rig(context)] context: &mut ToolContext,
    pattern: String,
) -> Result<Vec<ScriptMatch>, rig::tool::ToolExecutionError> {
    // an empty pattern would match every line of every script
    if pattern.is_empty() {
        return Err(ToolExecutionError::invalid_args(
            "`pattern` must not be empty".to_owned(),
        ));
    }
    // recover this run's session store and the scripts to search
    let store = store_from_context(context)?;
    let scripts = match store.list_scripts().await {
        Ok(scripts) => scripts,
        Err(error) => return Err(ToolExecutionError::invalid_args(error.to_string())),
    };
    // match case-insensitively so searches are forgiving
    let needle = pattern.to_lowercase();
    let mut found = Vec::new();
    // scan every session script line by line for the needle
    for script in scripts {
        let source = match store.load_script(&script).await {
            Ok(source) => source,
            Err(error) => return Err(ToolExecutionError::invalid_args(error.to_string())),
        };
        search_source(&mut found, &script, ScriptOrigin::Session, &source, &needle);
    }
    // scan the persistent library too (doc headers included) when configured
    if let Some(library) = ScriptLibrary::try_from_context(context) {
        let redactor = LibraryRedactor::try_from_context(context);
        let entries = library
            .list()
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
        for entry in entries {
            let source = library
                .load(&entry.name)
                .await
                .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
            // redact provenance (and deny-listed tokens, under blinding)
            // before matching so a hit line can never carry a raw session
            // name or family token into model context
            let source = render_library_source(&source, redactor.as_ref());
            // the hit's script name renders exactly like list_scripts shows it
            let display = match &redactor {
                Some(redactor) => redactor.display_name(&entry.name),
                None => entry.name.clone(),
            };
            search_source(
                &mut found,
                &display,
                ScriptOrigin::Library,
                &source,
                &needle,
            );
        }
    }
    Ok(found)
}

/// Appends one source's matching lines to a search's found list
///
/// # Arguments
///
/// * `found` - The accumulated matches to append to
/// * `script` - The name of the script being searched
/// * `origin` - Where the script lives (session or library)
/// * `source` - The script's full source (library doc headers included)
/// * `needle` - The lowercased pattern to match lines against
fn search_source(
    found: &mut Vec<ScriptMatch>,
    script: &str,
    origin: ScriptOrigin,
    source: &str,
    needle: &str,
) {
    // scan the source line by line for the lowercased needle
    for (index, text) in source.lines().enumerate() {
        if text.to_lowercase().contains(needle) {
            found.push(ScriptMatch {
                script: script.to_owned(),
                origin: origin.label(),
                line: index + 1,
                text: text.to_owned(),
            });
        }
    }
}

#[thorium_derive::thorpal_tool(group = "scripts", logs = "rhai_script")]
#[rig::tool_macro(
    description = "Run a saved Rhai script by name (session scripts shadow same-named library scripts). The optional `args` JSON value is exposed to the script as the `args` constant, print/debug output streams live and is returned, and the script's final expression comes back as the result. Runs are capped by an operation limit and a wall-clock deadline (default 300s, `timeout_secs` caps at 600)."
)]
pub async fn run_script(
    #[rig(context)] context: &mut ToolContext,
    name: String,
    args: Option<serde_json::Value>,
    timeout_secs: Option<u64>,
) -> Result<ScriptRun, rig::tool::ToolExecutionError> {
    // resolve the script session-first, falling back to the library
    let store = store_from_context(context)?;
    let library = ScriptLibrary::try_from_context(context);
    let redactor = LibraryRedactor::try_from_context(context);
    let (source, origin) = load_any(&store, library.as_ref(), redactor.as_ref(), &name).await?;
    // collect the binding registrars of the running agent's enabled tools, so
    // the functions the script can call always mirror the workflow's toolset
    let bindings: Vec<fn(&mut rhai::Engine, &ScriptHost)> = EnabledTools::from_context(context)
        .script_bindings()
        .iter()
        .filter_map(|entry| entry.rhai_bind)
        .collect();
    // build the host the bindings call back into, capturing the runtime
    // handle here on the async side so bindings can block_on Ghidra calls
    let host = ScriptHost {
        ghidra: GhidraSession::from_context(context)?,
        store: Some(Arc::clone(&store)),
        handle: tokio::runtime::Handle::current(),
    };
    // resolve the wall-clock deadline this run is terminated at
    let timeout = timeout_secs
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .min(MAX_TIMEOUT_SECS);
    let deadline = Instant::now() + Duration::from_secs(timeout);
    // the print/debug lines are captured here and streamed live via tracing
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&output);
    // run the script on a blocking thread; the engine and every !Send rhai
    // value live and die entirely inside this closure
    let args = args.unwrap_or(serde_json::Value::Null);
    let evaled = tokio::task::spawn_blocking(move || {
        execute_script(
            &source,
            &args,
            &bindings,
            &host,
            deadline,
            MAX_OPERATIONS,
            &captured,
        )
    })
    .await;
    // take the captured output back out of the shared buffer
    let output = match output.lock() {
        Ok(mut buffer) => std::mem::take(&mut *buffer),
        Err(_) => Vec::new(),
    };
    // a library script's prints and result ride a library surface, so deny
    // tokens scrub out of them under blinding (an observed content leak was a
    // family pass-key script's own strings); the model can still read any
    // sample-derived string through its own file tools, so no analysis signal
    // is lost -- only the harness-handed copy is neutralized. Session scripts
    // are the model's own work and stay raw
    let redactor = redactor.filter(|_| matches!(origin, ScriptOrigin::Library));
    let output = match &redactor {
        Some(redactor) => output
            .iter()
            .map(|line| redactor.redact_text(line))
            .collect(),
        None => output,
    };
    match evaled {
        // a finished run hands back its value and output, size-capped
        Ok(Ok(mut value)) => {
            // scrub deny tokens out of the result's string leaves too
            if let Some(redactor) = &redactor {
                redactor.redact_json(&mut value);
            }
            Ok(truncate_run(origin, value, output))
        }
        // a failed run keeps its partial output so progress is not lost
        Ok(Err(error)) => {
            let message = format!(
                "script '{name}' ({}) failed: {error}\n--- script output ---\n{}",
                origin.label(),
                output.join("\n")
            );
            // the error text can embed library source lines, so it scrubs too
            Err(ToolExecutionError::invalid_args(match &redactor {
                Some(redactor) => redactor.redact_text(&message),
                None => message,
            }))
        }
        // the blocking task itself failed (a panic inside the engine)
        Err(error) => Err(ToolExecutionError::invalid_args(format!(
            "script '{name}' crashed the script thread: {error}"
        ))),
    }
}

#[thorium_derive::thorpal_tool(group = "scripts")]
#[rig::tool_macro(
    description = "Promote a proven session script into the persistent cross-session library (~/.thorium/scripts), so future analysis runs can find and reuse it. Only promote scripts that worked and are generic: every binary-specific input (addresses, keys, sizes) must flow through `args`, never be hardcoded. Name and document scripts by TECHNIQUE (what they detect or decode, e.g. `extract_stackstrings`), never by malware family or sample name -- family-named scripts render as opaque neutral ids in blinded runs and become hard to find. `description` says what the script does and when a future run should use it; `usage` gives the args contract, the return shape, and one example args value -- future runs search these docs to pick scripts. Set `detector: true` ONLY for an argument-free, binary-agnostic whole-binary scan worth running automatically at every session start; a detector script's final expression must be an array of hit maps (each needs `address`, `content`, `reasoning`; optional `confidence` of \"low\"/\"medium\"/\"high\", ATT&CK `technique` + `technique_name`, `tactic`, `function`, `sites`) or a map `#{ hits: [...], questions: [...] }` where each question has a space-free `name` plus `title` and `body`. Promoting an existing library name is refused unless `overwrite: true` (the copy-on-write publish step)."
)]
pub async fn promote_script(
    #[rig(context)] context: &mut ToolContext,
    name: String,
    description: String,
    usage: String,
    detector: Option<bool>,
    overwrite: Option<bool>,
) -> Result<String, rig::tool::ToolExecutionError> {
    // promotion needs both this session's store and the persistent library
    let store = store_from_context(context)?;
    let library = ScriptLibrary::from_context(context)?;
    // a neutral-id name (what blinded runs render library names as) publishes
    // over the real stem it aliases, so the copy-on-write round trip lands on
    // the true file instead of minting a stray hash-named one
    let publish_as = match LibraryRedactor::try_from_context(context) {
        Some(redactor) => redactor
            .resolve_alias(&library, &name)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?
            .unwrap_or_else(|| name.clone()),
        None => name.clone(),
    };
    // assemble the doc-header metadata, stamping this session as provenance
    // and the sample identity so same-bytes runs can find the script by hash
    let meta = ScriptMeta::promoted_now(
        description,
        usage,
        detector.unwrap_or(false),
        store.name().await,
        store.sample_sha().unwrap_or_default(),
    );
    // run the shared promotion flow, mapping failures to instructive tool errors
    promote_to_library(
        &store,
        &library,
        &name,
        &publish_as,
        meta,
        overwrite.unwrap_or(false),
    )
    .await
    .map_err(ToolExecutionError::invalid_args)
}

/// The shared promotion flow behind the `promote_script` tool and `/promote`.
///
/// Loads the session's copy of the script, refuses a library collision unless
/// `overwrite` is set, validates the docs, strips any stale header before
/// rendering the fresh one in, compile-checks the result, and publishes it
/// into the library. Returns the success message describing what landed.
///
/// # Arguments
///
/// * `store` - The session store holding the script being promoted
/// * `library` - The persistent library to publish into
/// * `name` - The bare script name to promote (the session copy's name, and
///   what the success message echoes back)
/// * `publish_as` - The library stem to publish under (differs from `name`
///   only when a blinded run promoted via a neutral alias)
/// * `meta` - The validated-on-entry doc-header metadata to render in
/// * `overwrite` - Whether an existing library script may be replaced
pub(crate) async fn promote_to_library(
    store: &SessionStore,
    library: &ScriptLibrary,
    name: &str,
    publish_as: &str,
    meta: ScriptMeta,
    overwrite: bool,
) -> Result<String, String> {
    // only a script this session actually saved (and presumably ran) promotes
    let source = store
        .load_script(name)
        .await
        .map_err(|error| error.to_string())?;
    // refuse to silently clobber a library script without the explicit opt-in
    if !overwrite
        && library
            .exists(publish_as)
            .await
            .map_err(|error| error.to_string())?
    {
        return Err(format!(
            "a library script named '{name}' already exists; pass overwrite: \
             true to publish this session's version over it, or promote under \
             a different name"
        ));
    }
    // validate the docs before anything is written
    meta.validate()?;
    // strip any stale header off a re-promoted copy and render the fresh one in
    let body = strip_header(&source);
    let promoted = format!("{}\n{body}", meta.render(publish_as));
    // a promoted script must still parse; headers are comments so this only
    // fails when the session copy itself was somehow corrupted
    if let Err(error) = compile_check(&promoted) {
        return Err(format!(
            "script '{name}' failed to compile and was not promoted: {error}"
        ));
    }
    // publish the documented script into the library
    library
        .save(publish_as, &promoted)
        .await
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "promoted script '{name}' to the library ({} bytes{}); future \
         sessions will find it via list_scripts/search_scripts",
        promoted.len(),
        if meta.detector {
            ", flagged to run as a detector at session start"
        } else {
            ""
        }
    ))
}

#[thorium_derive::thorpal_tool(group = "scripts")]
#[rig::tool_macro(
    description = "Get the Rhai language reference plus the full documentation (arguments, overloads, and return shapes) of every host function your scripts can currently call. The host function list follows your enabled tools, so it is authoritative for this run."
)]
pub async fn rhai_docs(
    #[rig(context)] context: &mut ToolContext,
) -> Result<String, rig::tool::ToolExecutionError> {
    // start from the compiled-in language reference
    let mut docs = String::from(RHAI_REFERENCE);
    // the always-available builtins sit between the reference and the
    // toolset-dependent host functions, since they never depend on the toolset
    docs.push_str(BUILTIN_DOCS);
    // append the live host-function list for the running agent's toolset
    docs.push_str("\n\n# Host functions enabled for your scripts\n\n");
    let entries = EnabledTools::from_context(context).script_bindings();
    if entries.is_empty() {
        docs.push_str(
            "No host functions are enabled for this agent, so scripts are limited \
             to pure Rhai computation over the `args` value.",
        );
        return Ok(docs);
    }
    docs.push_str(
        "This list is derived from your enabled tools, so it is authoritative for \
         this run.\n",
    );
    // render each enabled binding's signature and full reference
    for entry in entries {
        if let Some(signature) = entry.rhai_signature {
            docs.push_str(&format!("\n## `{signature}`\n\n"));
        }
        if let Some(reference) = entry.rhai_reference {
            docs.push_str(reference);
            docs.push('\n');
        }
    }
    Ok(docs)
}

#[cfg(test)]
mod tests;
