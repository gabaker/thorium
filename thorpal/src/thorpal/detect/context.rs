//! Shared analysis context passed to every detection.
//!
//! [`AnalysisContext`] is built once, up front, from a small number of cheap
//! program-wide `ghidra-cli` queries (functions, imported symbols, defined
//! strings, program info). Detections then run against this cache instead of
//! re-querying Ghidra, and the two heavy per-function operations —
//! disassembly and decompilation — are performed lazily and memoized so a
//! function is only ever disassembled/decompiled once (and only if a detection
//! actually needs it). This keeps the tool's cost proportional to the number of
//! *flagged* functions rather than the whole binary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use ghidra_cli::Ghidra;
use ghidra_cli::models::{
    Function, Instruction, ListOptions, ProgramInfo, StringData, Symbol, XRef,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::instrument;

/// Parse a Ghidra hex address string (with or without a `0x` prefix) to `u64`.
///
/// # Arguments
///
/// * `raw` - The address text as reported by the bridge (e.g. `"10001000"`)
///
/// # Returns
///
/// The parsed address, or `None` if the text is not valid hexadecimal.
pub fn parse_addr(raw: &str) -> Option<u64> {
    // tolerate an optional 0x/0X prefix, then parse the remainder as base-16
    let trimmed = raw.trim_start_matches("0x").trim_start_matches("0X");
    u64::from_str_radix(trimmed, 16).ok()
}

/// Format a `u64` address the way the rest of the report renders addresses.
///
/// # Arguments
///
/// * `addr` - The address to format
pub fn fmt_addr(addr: u64) -> String {
    // lowercase, 0x-prefixed hex (matches the Java tool's rendering)
    format!("0x{addr:x}")
}

/// Return the section containing `addr`, if any.
///
/// # Arguments
///
/// * `sections` - The block table from [`AnalysisContext::sections`]
/// * `addr` - The address to locate
pub fn section_at(sections: &[Section], addr: u64) -> Option<&Section> {
    // blocks do not overlap, so the first containing block is the answer
    sections.iter().find(|s| s.contains(addr))
}

/// Whether `addr` lives in executable memory.
///
/// An address the block table does not cover counts as executable: the conservative
/// answer for callers using this to *discard* something is "keep it".
///
/// # Arguments
///
/// * `sections` - The block table from [`AnalysisContext::sections`]
/// * `addr` - The address to test
pub fn is_executable_addr(sections: &[Section], addr: u64) -> bool {
    // an unmapped address is not evidence of anything, so do not act on it
    section_at(sections, addr).is_none_or(Section::is_executable)
}

/// Whether an xref's reference type is a call-like class.
///
/// Matches the CALL classes (`UNCONDITIONAL_CALL`, `CONDITIONAL_CALL`,
/// `COMPUTED_CALL`, ...), the JUMP classes (tail calls and jump stubs), and
/// `THUNK` references; the data classes (`DATA`, `READ`, `WRITE`, `PARAM`,
/// `EXTERNAL_REF`, ...) are not call-like — Ghidra's constant propagation
/// records those at every instruction a target's address flows through, so on
/// their own they prove nothing. Whether a non-call-like reference still counts
/// as a call site is decided by [`is_call_site_evidence`], which weighs the
/// containing function in. A missing/empty type is call-like (fail-open): a
/// bridge version that omits the field must never silently lose a genuine
/// call. The strings are Ghidra `RefType` display names, rendered by the
/// bridge via `getReferenceType().toString()`.
///
/// # Arguments
///
/// * `ref_type` - The reference type string the bridge reported
fn is_call_like_ref(ref_type: &str) -> bool {
    // normalize once; Ghidra renders RefType display names in upper snake case
    let t = ref_type.trim().to_ascii_uppercase();
    // fail-open on a missing type, otherwise demand a call/jump/thunk class
    t.is_empty() || t.contains("CALL") || t.contains("JUMP") || t.contains("THUNK")
}

/// Whether a resolved reference survives as call-site evidence.
///
/// A call/jump/thunk-typed (or missing-typed, fail-open) reference is a call site
/// wherever it sits. A data-typed (`READ`/`DATA`/…) reference is kept only when a
/// function contains it: the x64 indirect-call idiom (`mov rsi, [import]; … call rsi`)
/// records a READ-typed reference at the `mov` — a genuine call-site evidence source
/// (the `WinHttp*`/`InternetOpen*`/`HttpOpenRequest` call-site leads come from exactly
/// this shape) — while the `VMProtect` import slots the type filter exists for are
/// data references in an executable `.vmp` section that no function contains.
///
/// # Arguments
///
/// * `ref_type` - The reference type string the bridge reported
/// * `function` - The entry of the function containing the referencing site, if any
fn is_call_site_evidence(ref_type: &str, function: Option<u64>) -> bool {
    // a call-like (or missing) type stands on its own; a data-typed reference is
    // evidence only when a real function contains the referencing instruction
    is_call_like_ref(ref_type) || function.is_some()
}

/// Decode a hex string (no `0x` prefix) into raw bytes.
///
/// Non-hex characters are skipped and a dangling final nibble is dropped. Used to
/// turn the bridge's `read_memory` hex payload back into bytes for the
/// byte-signature scans (crypto constants, encoded-string blobs).
///
/// # Arguments
///
/// * `hex` - The hex string to decode (e.g. `"90ab..."`)
pub fn parse_hex(hex: &str) -> Vec<u8> {
    // keep only hex digits, mapping each to its 0-15 nibble value
    let nibbles: Vec<u8> = hex
        .bytes()
        .filter_map(|b| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        })
        .collect();
    // fold each high/low nibble pair into a byte (a trailing nibble is ignored)
    nibbles
        .chunks_exact(2)
        .map(|p| (p[0] << 4) | p[1])
        .collect()
}

/// The maximum number of bytes read from any single memory block.
///
/// The byte-scanning detections only need code/data sections; this cap keeps a
/// pathologically large section from ballooning the `read_memory` request.
const MAX_BLOCK_READ: u64 = 16 * 1024 * 1024;

/// The bytes of one initialized memory block: `(block name, base address, bytes)`.
///
/// Returned (in bulk) by [`AnalysisContext::initialized_bytes`] and consumed by the
/// byte-scanning detections (crypto-constant signatures, encoded-string blobs).
pub type SectionBytes = (String, u64, Vec<u8>);

/// The maximum number of bytes read from the sample file on disk.
///
/// The raw-byte consumers (whole-file entropy, overlay detection, format-header
/// parsing) only need the file itself; this cap keeps a pathologically large
/// submission from being slurped whole into memory.
const MAX_RAW_FILE: u64 = 256 * 1024 * 1024;

/// One memory block (section) of the program, with the metadata the byte-only
/// [`SectionBytes`] view discards.
///
/// Unlike [`AnalysisContext::initialized_bytes`], this keeps **uninitialized**
/// blocks: a block that reserves a large virtual range but carries no bytes on
/// disk is itself a signal (it is what a runtime packer leaves for its unpacked
/// image), so dropping it would hide the very shape the packing detection looks
/// for.
#[derive(Debug, Clone)]
pub struct Section {
    /// The block name (e.g. `.text`).
    pub name: String,
    /// The block's base address.
    pub base: u64,
    /// The block's (exclusive) end address.
    pub end: u64,
    /// The block's virtual size in bytes.
    pub size: u64,
    /// The permission string, some subset of `rwx`.
    pub permissions: String,
    /// Whether the block has initialized bytes on disk.
    pub is_initialized: bool,
    /// Whether the block is loaded into the image.
    pub is_loaded: bool,
    /// The block's bytes. Empty for an uninitialized block (there is nothing to read).
    pub bytes: Vec<u8>,
    /// Whether the read was cut short by `MAX_BLOCK_READ`.
    pub truncated: bool,
}

impl Section {
    /// Whether the block is writable.
    pub fn is_writable(&self) -> bool {
        // the bridge renders permissions as a subset of "rwx"
        self.permissions.contains('w')
    }

    /// Whether the block is executable.
    pub fn is_executable(&self) -> bool {
        // the bridge renders permissions as a subset of "rwx"
        self.permissions.contains('x')
    }

    /// The Shannon entropy of the block's bytes, or `None` when it has none.
    ///
    /// A high value over a sizeable block means compressed or encrypted content;
    /// this is the per-section rollup the tool previously only computed per string.
    pub fn entropy(&self) -> Option<f32> {
        // an uninitialized (or unread) block has no content to measure
        if self.bytes.is_empty() {
            return None;
        }
        Some(crate::thorpal::detect::decode::shannon_entropy(&self.bytes))
    }

    /// Whether `addr` falls inside this block.
    ///
    /// # Arguments
    ///
    /// * `addr` - The address to test
    pub fn contains(&self, addr: u64) -> bool {
        // the range is half-open: [base, end)
        addr >= self.base && addr < self.end
    }
}

/// Render a JSON value that may be a string *or* an array of strings as a single
/// optional string (array elements are space-joined).
///
/// Ghidra 12's bridge renders some instruction fields (e.g. `operands`) as an
/// array, which the strict typed model rejects; this accepts either shape.
///
/// # Arguments
///
/// * `value` - The field value, when present
fn str_or_joined(value: Option<&Value>) -> Option<String> {
    match value {
        // already a string
        Some(Value::String(s)) => Some(s.clone()),
        // an array of per-operand strings — join as "op0, op1" so comma-based
        // operand parsing (e.g. `eax,0x65`) in the detections keeps working
        Some(Value::Array(items)) => {
            let parts: Vec<String> = items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        // anything else (null/number/absent) has no string form
        _ => None,
    }
}

/// Coerce a bridge JSON field into the per-operand list ThorPal's model holds.
///
/// The `disasm` bridge renders operands as either a single joined string or an
/// array of per-operand strings. ThorPal's `Instruction::operands` is a
/// `Vec<String>`, so a lone string is wrapped in a one-element list and an array
/// is taken as-is; anything else has no operand form and yields an empty list.
///
/// # Arguments
///
/// * `value` - The field value, when present
fn str_or_list(value: Option<&Value>) -> Vec<String> {
    match value {
        // a single joined string becomes a one-element list
        Some(Value::String(s)) => vec![s.clone()],
        // an array of per-operand strings is taken verbatim
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        // anything else (null/number/absent) has no operand form
        _ => Vec::new(),
    }
}

/// Tolerantly parse one disassembled instruction from a bridge JSON object.
///
/// Requires `address` and `mnemonic`; treats `operands`/`bytes`/`flow_type` as
/// string-or-array (see [`str_or_joined`]).
///
/// # Arguments
///
/// * `item` - A single instruction object from the `disasm` response
fn parse_instruction(item: &Value) -> Option<Instruction> {
    // address and mnemonic are required to make sense of an instruction
    let address = item.get("address")?.as_str()?.to_string();
    let mnemonic = item.get("mnemonic")?.as_str()?.to_string();
    Some(Instruction {
        address,
        mnemonic,
        // operands may arrive as a single string or an array of per-operand
        // strings; ThorPal's model holds them as a list either way
        operands: str_or_list(item.get("operands")),
        bytes: str_or_joined(item.get("bytes")),
        // length is a number when present
        length: item.get("length").and_then(Value::as_u64).map(|n| n as u32),
        flow_type: str_or_joined(item.get("flow_type")),
    })
}

/// Return whether a function body is an import thunk: a single `JMP` instruction.
///
/// # Arguments
///
/// * `insns` - The function's disassembled instructions
fn is_thunk_body(insns: &[Instruction]) -> bool {
    // a thunk stub is exactly one unconditional jump through the import table
    matches!(insns, [only] if only.mnemonic.eq_ignore_ascii_case("JMP"))
}

/// A function's half-open address range `[start, end)` plus its entry point.
#[derive(Debug, Clone, Copy)]
struct FnRange {
    /// The inclusive start address of the function body.
    start: u64,
    /// The exclusive end address of the function body.
    end: u64,
    /// The function entry-point address (used as the function's stable id).
    entry: u64,
}

/// Cached, program-wide facts plus the live Ghidra handle shared by detections.
pub struct AnalysisContext {
    /// The connected Ghidra bridge handle.
    pub ghidra: Ghidra,
    /// Metadata for the analyzed program (format, arch, image base, ...).
    pub program: ProgramInfo,
    /// The on-disk path the sample was submitted from.
    ///
    /// Ghidra's loaded image is not the file: it drops the format header, drops
    /// anything appended past the last mapped section, and re-lays sections at
    /// their virtual addresses. Keeping the path lets the raw-byte consumers
    /// (whole-file entropy, overlay detection, header parsing) see the file as it
    /// actually exists.
    pub sample_path: PathBuf,
    /// Every function in the program (entry address + size + name).
    pub functions: Vec<Function>,
    /// Function ranges sorted by start address for fast containment lookups.
    ranges: Vec<FnRange>,
    /// The imported symbols (external APIs) referenced by the program.
    pub imports: Vec<Symbol>,
    /// The defined strings in the program.
    pub strings: Vec<StringData>,
    /// The statically linked toolchain fingerprinted at build time, when one
    /// was recognizable (see [`super::toolchain`]). Lets the detections and
    /// the forum seeder judge runtime-library noise with one rule.
    pub toolchain: Option<super::toolchain::Toolchain>,
    /// Memoized per-function disassembly, keyed by entry-point address.
    disasm_cache: Mutex<HashMap<u64, Vec<Instruction>>>,
    /// Memoized per-function decompiled C, keyed by entry-point address.
    decomp_cache: Mutex<HashMap<u64, String>>,
    /// The functions the decompiler has already refused, and the reason it gave.
    ///
    /// Kept apart from `decomp_cache` because a failure is not a decompilation and
    /// must never be returned as one. Its purpose is the timeout: the decompiler
    /// gives up on a function only after `GHIDRA_CLI_DECOMPILE_TIMEOUT` seconds, and
    /// several detections converge on the same function — a byte scan attributing
    /// dozens of candidates that all land in one place is the usual way — so without
    /// this a single stubborn function is re-requested once per candidate and each
    /// caller waits out the full timeout again. Whatever made the decompiler give up
    /// is a property of the function, so the second answer would match the first.
    decomp_failed: Mutex<HashMap<u64, String>>,
    /// Memoized callers (CALL xrefs) of a function, keyed by entry-point address.
    callers_cache: Mutex<HashMap<u64, Vec<XRef>>>,
    /// Memoized "is this function an import thunk?" result, keyed by entry address.
    thunk_cache: Mutex<HashMap<u64, bool>>,
    /// Memoized bytes of every initialized memory block (fetched at most once).
    ///
    /// Each entry is `(block name, base address, bytes)`; shared by the
    /// byte-scanning detections (crypto-constant signatures, encoded-string blobs).
    section_bytes_cache: Mutex<Option<Vec<SectionBytes>>>,
    /// Memoized full memory-block table with metadata (fetched at most once).
    ///
    /// This is the superset of `section_bytes_cache`: it keeps the block
    /// permissions, initialized/loaded flags, and the uninitialized blocks that
    /// the byte-only view drops.
    sections_cache: Mutex<Option<Vec<Section>>>,
    /// Memoized reverse index `(data address, referencing function entry)`, sorted by
    /// data address. Built at most once; used to attribute a decoded data buffer to
    /// the function that references it.
    ref_index_cache: Mutex<Option<Vec<(u64, u64)>>>,
    /// Memoized bytes of the sample file on disk (read at most once).
    raw_bytes_cache: Mutex<Option<Arc<Vec<u8>>>>,
}

/// A resolved call site of a target (import), after transparently hopping thunks.
#[derive(Debug, Clone, Copy)]
pub struct CallSite {
    /// The address of the referencing instruction (the real `CALL`).
    pub site: u64,
    /// The entry of the function containing the call site, when known.
    pub function: Option<u64>,
    /// The thunk entry this site was reached through, when the target was thunked.
    pub via_thunk: Option<u64>,
}

impl AnalysisContext {
    /// Build the context by issuing the cheap program-wide queries once.
    ///
    /// # Arguments
    ///
    /// * `ghidra` - A connected Ghidra bridge handle with the sample open
    /// * `sample_path` - The on-disk path the sample was submitted from
    ///
    /// # Errors
    ///
    /// Returns an error if any of the initial bridge queries fail.
    #[instrument(name = "AnalysisContext::build", skip_all, err(Debug))]
    pub async fn build(ghidra: Ghidra, sample_path: impl Into<PathBuf>) -> Result<Self> {
        // fetch program metadata (format/arch/image base) for the report header
        let program = ghidra
            .info()
            .await
            .context("failed to fetch program info")?;
        // list every function (unbounded) so we can build the containment index
        let functions = ghidra
            .function
            .list(ListOptions::default())
            .await
            .context("failed to list functions")?;
        // list every symbol, then keep only the imported (external API) ones
        let all_symbols = ghidra
            .symbol
            .list(ListOptions::default())
            .await
            .context("failed to list symbols")?;
        let imports: Vec<Symbol> = all_symbols
            .into_iter()
            // an import is a symbol whose source Ghidra marks as IMPORTED
            .filter(|s| {
                s.source
                    .as_deref()
                    .is_some_and(|src| src.eq_ignore_ascii_case("IMPORTED"))
            })
            .collect();
        // list every defined string for the string-artifact detections
        let strings = ghidra
            .string
            .list(ListOptions::default())
            .await
            .context("failed to list strings")?;

        // build a sorted range index so we can map an address to its function
        let mut ranges: Vec<FnRange> = functions
            .iter()
            .filter_map(|f| {
                // parse the entry address; skip functions we cannot locate
                let entry = parse_addr(&f.entry_point).or_else(|| parse_addr(&f.address))?;
                // the body spans `size` addresses starting at the entry
                Some(FnRange {
                    start: entry,
                    end: entry.saturating_add(f.size.max(1)),
                    entry,
                })
            })
            .collect();
        // sort by start so `containing_function` can binary-search
        ranges.sort_by_key(|r| r.start);

        Ok(Self {
            ghidra,
            program,
            sample_path: sample_path.into(),
            // fingerprint the toolchain once off the listings gathered above,
            // before they move into the context
            toolchain: {
                let toolchain = super::toolchain::detect(functions.as_slice(), &imports, &strings);
                tracing::info!(?toolchain, "toolchain fingerprint");
                toolchain
            },
            functions,
            ranges,
            imports,
            strings,
            disasm_cache: Mutex::new(HashMap::new()),
            decomp_cache: Mutex::new(HashMap::new()),
            decomp_failed: Mutex::new(HashMap::new()),
            callers_cache: Mutex::new(HashMap::new()),
            thunk_cache: Mutex::new(HashMap::new()),
            section_bytes_cache: Mutex::new(None),
            sections_cache: Mutex::new(None),
            ref_index_cache: Mutex::new(None),
            raw_bytes_cache: Mutex::new(None),
        })
    }

    /// Read (and memoize) the sample's bytes as they exist on disk.
    ///
    /// This is deliberately *not* the loaded image: it includes the format
    /// header, any data appended past the last mapped section, and the sections
    /// at their file offsets rather than their virtual addresses. The read is
    /// bounded by `MAX_RAW_FILE`.
    ///
    /// # Errors
    ///
    /// Returns an error if the sample cannot be read.
    #[instrument(name = "AnalysisContext::raw_bytes", skip_all, err(Debug))]
    pub async fn raw_bytes(&self) -> Result<Arc<Vec<u8>>> {
        // fast path: return the cached file contents if we've already read them
        let mut cache = self.raw_bytes_cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            return Ok(Arc::clone(cached));
        }
        // read the whole file, then bound it so an enormous submission cannot
        // balloon memory for the handful of scanners that want raw bytes
        let mut bytes = std::fs::read(&self.sample_path)
            .with_context(|| format!("reading sample '{}'", self.sample_path.display()))?;
        if bytes.len() as u64 > MAX_RAW_FILE {
            tracing::warn!(
                path = %self.sample_path.display(),
                size = bytes.len(),
                cap = MAX_RAW_FILE,
                "sample truncated for raw-byte analysis"
            );
            bytes.truncate(MAX_RAW_FILE as usize);
        }
        // memoize and return
        let shared = Arc::new(bytes);
        *cache = Some(Arc::clone(&shared));
        Ok(shared)
    }

    /// Fetch (and memoize) the program's full memory-block table.
    ///
    /// Returns one [`Section`] per block — **including uninitialized blocks**,
    /// whose `bytes` are empty — with the permissions and initialized/loaded
    /// flags that [`AnalysisContext::initialized_bytes`] discards. Initialized
    /// blocks are read to a bounded maximum (`MAX_BLOCK_READ`) and marked
    /// `truncated` when the cap applied.
    ///
    /// # Errors
    ///
    /// Returns an error if the memory map or a block read fails.
    #[instrument(name = "AnalysisContext::sections", skip_all, err(Debug))]
    pub async fn sections(&self) -> Result<Vec<Section>> {
        // fast path: return the cached block table if we've already built it
        let mut cache = self.sections_cache.lock().await;
        if let Some(cached) = cache.as_ref() {
            return Ok(cached.clone());
        }
        // fetch the program's memory map (blocks/sections) once
        let blocks = self
            .ghidra
            .memory
            .map()
            .await
            .context("failed to fetch memory map")?;
        let mut out = Vec::with_capacity(blocks.len());
        for block in blocks {
            // locate the block base; skip any block we cannot address
            let Some(base) = parse_addr(&block.start) else {
                continue;
            };
            // an uninitialized block has no bytes on disk to read, but the block
            // itself still matters (it is the shape a packer's unpack target has)
            let read_len = if block.is_initialized {
                block.size.min(MAX_BLOCK_READ) as usize
            } else {
                0
            };
            // read the block in one request and decode the hex payload to bytes
            let bytes = if read_len == 0 {
                Vec::new()
            } else {
                let read = self
                    .ghidra
                    .memory
                    .read(&block.start, read_len)
                    .await
                    .with_context(|| format!("reading memory block '{}'", block.name))?;
                parse_hex(&read.hex)
            };
            out.push(Section {
                name: block.name,
                base,
                end: base.saturating_add(block.size),
                size: block.size,
                permissions: block.permissions,
                is_initialized: block.is_initialized,
                is_loaded: block.is_loaded,
                bytes,
                truncated: block.size > MAX_BLOCK_READ,
            });
        }
        // memoize and return
        *cache = Some(out.clone());
        Ok(out)
    }

    /// Return the entry address of the function containing `addr`, if any.
    ///
    /// # Arguments
    ///
    /// * `addr` - The address to locate
    pub fn containing_function(&self, addr: u64) -> Option<u64> {
        // binary search for the last range whose start is <= addr
        let idx = match self.ranges.binary_search_by(|r| r.start.cmp(&addr)) {
            // exact hit on a function entry
            Ok(i) => i,
            // Err(0) means addr precedes every function -> no container
            Err(0) => return None,
            // otherwise the candidate is the range just before the insertion point
            Err(i) => i - 1,
        };
        // confirm the address actually falls within that function's body
        let range = self.ranges[idx];
        (addr >= range.start && addr < range.end).then_some(range.entry)
    }

    /// Return the name of the function with the given entry address, if known.
    ///
    /// # Arguments
    ///
    /// * `entry` - The function entry-point address
    pub fn function_name(&self, entry: u64) -> Option<&str> {
        // scan the function list for a matching entry point (only called for the
        // handful of flagged functions, so a linear scan is fine)
        self.functions
            .iter()
            .find(|f| parse_addr(&f.entry_point).or_else(|| parse_addr(&f.address)) == Some(entry))
            .map(|f| f.name.as_str())
    }

    /// Disassemble **every** function in the program (cached) and return each
    /// function's entry address paired with its instructions.
    ///
    /// This is the accuracy-first path: detections scan real *decoded*
    /// instructions (mnemonics + operands) rather than raw byte patterns, which
    /// avoids the false positives of byte search and gives precise operand
    /// context (CPUID leaf, PEB field offset, syscall number, EFLAGS trap-flag).
    /// It is deliberately slower — one disassembly per function — but every
    /// result is memoized so repeated detection passes (and the report builder)
    /// reuse it. The returned instructions are trimmed to each function's body.
    ///
    /// # Errors
    ///
    /// Returns an error if a bridge disassembly fails.
    #[instrument(
        name = "AnalysisContext::all_function_disassembly",
        skip_all,
        err(Debug)
    )]
    pub async fn all_function_disassembly(&self) -> Result<Vec<(u64, Vec<Instruction>)>> {
        // disassemble every known function body (memoized in disasm_fn)
        let mut out = Vec::with_capacity(self.ranges.len());
        for range in &self.ranges {
            // reuse the cached disassembly, or fetch+cache it now
            let insns = self.disasm_fn(range.entry).await?;
            out.push((range.entry, insns));
        }
        Ok(out)
    }

    /// Read and cache the raw bytes of every initialized memory block.
    ///
    /// Returns one `(block name, base address, bytes)` tuple per initialized,
    /// on-disk block (uninitialized blocks like `.bss` have no bytes to scan).
    /// The whole set is memoized so the byte-scanning detections
    /// (crypto-constant signatures, encoded-string blobs) share a single pass
    /// over the program's sections instead of re-reading them.
    ///
    /// Each block is read to a bounded maximum (`MAX_BLOCK_READ`) so a huge
    /// section cannot balloon the read.
    ///
    /// # Errors
    ///
    /// Returns an error if the memory map or a block read fails.
    #[instrument(name = "AnalysisContext::initialized_bytes", skip_all, err(Debug))]
    pub async fn initialized_bytes(&self) -> Result<Vec<SectionBytes>> {
        // fast path: return the cached section bytes if we've already read them
        if let Some(cached) = self.section_bytes_cache.lock().await.as_ref() {
            return Ok(cached.clone());
        }
        // project the (memoized) full block table down to the byte-only view, so
        // the two accessors never read the same block from the bridge twice
        let out: Vec<SectionBytes> = self
            .sections()
            .await?
            .into_iter()
            // keep only blocks that actually carry bytes to scan (drops .bss)
            .filter(|s| s.is_initialized && !s.bytes.is_empty())
            .map(|s| (s.name, s.base, s.bytes))
            .collect();
        // memoize and return
        *self.section_bytes_cache.lock().await = Some(out.clone());
        Ok(out)
    }

    /// Build (and cache) the reverse index mapping each referenced data address to a
    /// function that references it.
    ///
    /// Disassembles every function and reads the section bytes (both cached), then
    /// delegates to [`crate::thorpal::detect::decode::buffers::reference_index`]. Detections use it via
    /// [`crate::thorpal::detect::decode::buffers::referencing_function`] to attribute a decoded buffer
    /// to the function that decodes it. Built at most once.
    ///
    /// # Errors
    ///
    /// Returns an error if disassembling the program or reading its sections fails.
    #[instrument(name = "AnalysisContext::reference_index", skip_all, err(Debug))]
    pub async fn reference_index(&self) -> Result<Vec<(u64, u64)>> {
        // fast path: return the cached index if we've already built it
        if let Some(cached) = self.ref_index_cache.lock().await.as_ref() {
            return Ok(cached.clone());
        }
        // build it from the (cached) whole-program disassembly + section bytes
        let all = self.all_function_disassembly().await?;
        let sections = self.initialized_bytes().await?;
        let index = crate::thorpal::detect::decode::buffers::reference_index(&all, &sections);
        // memoize and return
        *self.ref_index_cache.lock().await = Some(index.clone());
        Ok(index)
    }

    /// Return the cross-references *to* a target (address or symbol name).
    ///
    /// Used to find the call sites of an imported API by name.
    ///
    /// # Arguments
    ///
    /// * `target` - A symbol name or address to find references to
    ///
    /// # Errors
    ///
    /// Returns an error if the bridge query fails.
    #[instrument(name = "AnalysisContext::xrefs_to", skip_all, err(Debug))]
    pub async fn xrefs_to(&self, target: &str) -> Result<Vec<XRef>> {
        // reuse the function-xrefs handler (it issues `xrefs_to` under the hood)
        self.ghidra.function.xrefs(target).await
    }

    /// Return the CALL cross-references *to* a function (its callers), cached.
    ///
    /// Used by the reachability validator: a function with no callers may be dead
    /// or reached only indirectly.
    ///
    /// # Arguments
    ///
    /// * `entry` - The function entry-point address
    ///
    /// # Errors
    ///
    /// Returns an error if the bridge xref query fails.
    #[instrument(name = "AnalysisContext::callers", skip_all, err(Debug))]
    pub async fn callers(&self, entry: u64) -> Result<Vec<XRef>> {
        // fast path: return the cached caller list if present
        if let Some(cached) = self.callers_cache.lock().await.get(&entry) {
            return Ok(cached.clone());
        }
        // resolve all references to the function entry, keeping only CALLs
        let callers: Vec<XRef> = self
            .xrefs_to(&fmt_addr(entry))
            .await?
            .into_iter()
            .filter(|x| x.ref_type.to_ascii_uppercase().contains("CALL"))
            .collect();
        // memoize and return
        self.callers_cache
            .lock()
            .await
            .insert(entry, callers.clone());
        Ok(callers)
    }

    /// Return whether a function is an import thunk (a one-instruction `JMP` stub).
    ///
    /// Ghidra represents imported APIs with a small stub that jumps through the
    /// import address table; its body is a single `JMP`, which is useless to a
    /// reverse engineer. Detecting this lets callers attribute a finding to the
    /// real caller instead. Result is memoized.
    ///
    /// # Arguments
    ///
    /// * `entry` - The function entry-point address
    ///
    /// # Errors
    ///
    /// Returns an error if the function cannot be disassembled.
    #[instrument(name = "AnalysisContext::is_thunk", skip_all, err(Debug))]
    pub async fn is_thunk(&self, entry: u64) -> Result<bool> {
        // fast path: return the cached verdict if we've classified this function
        if let Some(cached) = self.thunk_cache.lock().await.get(&entry) {
            return Ok(*cached);
        }
        // classify from the (cached) disassembly: a lone JMP is a thunk stub
        let thunk = is_thunk_body(&self.disasm_fn(entry).await?);
        // memoize and return
        self.thunk_cache.lock().await.insert(entry, thunk);
        Ok(thunk)
    }

    /// Resolve the call sites of a target address, hopping through import thunks.
    ///
    /// Each reference to `target_addr` whose containing function is a thunk is
    /// expanded to the thunk's real callers, so the returned sites point at the
    /// code that actually invokes the target. Non-thunk references pass through
    /// unchanged. Bounded to a single hop (chained thunks are vanishingly rare).
    ///
    /// References from **data** are dropped: every import is referenced by its own
    /// import-address-table slot in `.rdata`, and reporting that slot as a call site
    /// produces a finding at an address no code path ever executes — attributable to
    /// no function, and so invisible to every caller-context judgement downstream
    /// (see `crate::provenance`). An import referenced only from data still
    /// surfaces through the "imported, but no direct call site" path.
    ///
    /// References are also judged by type and attribution together (see
    /// [`is_call_site_evidence`]): CALL-, JUMP- (tail calls), and THUNK-typed
    /// references are call sites wherever they sit, and a `DATA`/`READ`-typed
    /// reference is kept only when a function contains it. Ghidra's constant
    /// propagation records data-typed references at every instruction an
    /// import's address flows through, which cuts both ways: the x64 indirect
    /// call idiom (`mov rsi, [import]; … call rsi`) records a READ at the `mov`
    /// inside a real function — genuine call-site evidence — while `VMProtect`'s
    /// `.vmp` import slots record them in executable memory that no function
    /// contains, fabricating call sites no instruction executes. Each site is
    /// therefore attributed to its containing function *first*, and a
    /// data-typed reference without one is dropped. A reference with a
    /// missing/empty type is kept (fail-open), so bridge versions that omit
    /// the field never silently lose a genuine call.
    ///
    /// # Arguments
    ///
    /// * `target_addr` - The address to resolve references to (e.g. an import)
    ///
    /// # Errors
    ///
    /// Returns an error if a bridge xref query, a section query, or a thunk
    /// disassembly fails.
    #[instrument(name = "AnalysisContext::resolve_call_sites", skip_all, err(Debug))]
    pub async fn resolve_call_sites(&self, target_addr: &str) -> Result<Vec<CallSite>> {
        // the block table (cached) tells a call site apart from a data pointer
        let sections = self.sections().await?;
        // start from the direct references to the target
        let mut sites = Vec::new();
        for xref in self.xrefs_to(target_addr).await? {
            // parse the referencing address; skip any we cannot locate
            let Some(from) = parse_addr(&xref.from) else {
                continue;
            };
            // a reference from non-executable memory is a pointer, not a call
            if !is_executable_addr(&sections, from) {
                continue;
            }
            // attribute the site to its containing function *before* judging the
            // reference: the keep decision below needs the attribution
            let container = self.containing_function(from);
            // keep call/jump/thunk-typed references anywhere, and data-typed ones
            // only inside a function (the mov-load idiom); a function-less data
            // reference is a VMProtect-style import slot, not a call site
            if !is_call_site_evidence(&xref.ref_type, container) {
                continue;
            }
            // if that function is a thunk, hop to its real callers
            if let Some(thunk) = container {
                if self.is_thunk(thunk).await? {
                    let callers = self.callers(thunk).await?;
                    if !callers.is_empty() {
                        // one resolved site per real caller of the thunk
                        for caller in callers {
                            if let Some(csite) = parse_addr(&caller.from) {
                                sites.push(CallSite {
                                    site: csite,
                                    function: self.containing_function(csite),
                                    via_thunk: Some(thunk),
                                });
                            }
                        }
                        // handled this reference via the thunk's callers
                        continue;
                    }
                    // a thunk with no discovered callers: fall through and keep it
                }
            }
            // ordinary (non-thunk) reference: keep the site as-is
            sites.push(CallSite {
                site: from,
                function: container,
                via_thunk: None,
            });
        }
        Ok(sites)
    }

    /// Disassemble a function once and cache the result.
    ///
    /// The bridge's `disasm` starts at the entry and returns up to `count`
    /// instructions; we request an upper bound of the function's byte size and
    /// then trim to the function's own address range so we never spill into the
    /// next function.
    ///
    /// # Arguments
    ///
    /// * `entry` - The function entry-point address
    ///
    /// # Errors
    ///
    /// Never returns an error for a per-function disassembly failure: a body Ghidra
    /// could not decode (data, an unanalyzed gap, or a symbol pointing outside
    /// analyzed code — common in Go/packed binaries) is logged at WARN, negatively
    /// cached as an empty list, and returns `Ok(Vec::new())` so a single bad
    /// function never aborts a whole-program pass. Every caller already treats "no
    /// instructions" as "nothing to see here" (mirrors `decompile_fn`'s philosophy).
    // a per-function disassembly failure is handled gracefully here (log + skip), so
    // there is no error to capture at this span.
    #[instrument(name = "AnalysisContext::disasm_fn", skip_all)]
    pub async fn disasm_fn(&self, entry: u64) -> Result<Vec<Instruction>> {
        // fast path: return a clone of the cached disassembly if present (a
        // previously-failed function is cached as an empty list, so we neither
        // re-hit the bridge nor re-log for it)
        if let Some(cached) = self.disasm_cache.lock().await.get(&entry) {
            return Ok(cached.clone());
        }
        // determine the function's byte size to bound the instruction request
        let end = self
            .ranges
            .iter()
            .find(|r| r.entry == entry)
            .map(|r| r.end)
            .unwrap_or(entry.saturating_add(4096));
        let count = (end - entry) as usize;
        // request the disassembly via the raw bridge command and parse it
        // tolerantly: Ghidra 12's bridge renders some fields (e.g. operands) as
        // arrays, which ghidra-cli's strict typed `disasm` rejects.
        let value = match self
            .ghidra
            .command(
                "disasm",
                Some(json!({"address": fmt_addr(entry), "count": count.max(1)})),
            )
            .await
        {
            Ok(value) => value,
            // a body Ghidra never decoded (e.g. "No instruction at address …; may be
            // data or unanalyzed code") is expected for some functions: warn, cache
            // the failure as an empty list so we only attempt it once, and continue
            Err(error) => {
                tracing::warn!(
                    address = %fmt_addr(entry),
                    %error,
                    "disassembly failed; treating function as undecodable (data or unanalyzed code)",
                );
                self.disasm_cache.lock().await.insert(entry, Vec::new());
                return Ok(Vec::new());
            }
        };
        // pull the instruction array from the response envelope
        let raw = value
            .get("instructions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // convert each entry, trimming instructions outside this function's body
        let insns: Vec<Instruction> = raw
            .iter()
            .filter_map(parse_instruction)
            .filter(|i| parse_addr(&i.address).is_some_and(|a| a >= entry && a < end))
            .collect();
        // memoize and return
        self.disasm_cache.lock().await.insert(entry, insns.clone());
        Ok(insns)
    }

    /// Disassemble the function that contains `addr`, returning its entry and body.
    ///
    /// # Arguments
    ///
    /// * `addr` - An address somewhere inside the function of interest
    ///
    /// # Errors
    ///
    /// Returns an error if disassembly fails.
    pub async fn disasm_containing(&self, addr: u64) -> Result<Option<(u64, Vec<Instruction>)>> {
        // locate the containing function, then disassemble it (cached)
        match self.containing_function(addr) {
            Some(entry) => Ok(Some((entry, self.disasm_fn(entry).await?))),
            None => Ok(None),
        }
    }

    // (helper `decompile_fn` follows)

    /// Decompile a function to C once and cache the result.
    ///
    /// Both outcomes are cached. Caching the failure matters as much as caching the
    /// success, because a refusal is the expensive answer: the decompiler reaches it
    /// by exhausting its timeout, so a function several detections all ask about
    /// costs that timeout once per asker unless the first refusal is remembered. See
    /// `decomp_failed`.
    ///
    /// # Arguments
    ///
    /// * `entry` - The function entry-point address
    ///
    /// # Errors
    ///
    /// Returns an error if decompilation fails, or if it has already failed.
    // decompilation failures are expected for some (e.g. obfuscated) functions and
    // are handled gracefully by the caller, so log them at WARN rather than ERROR
    #[instrument(
        name = "AnalysisContext::decompile_fn",
        skip_all,
        err(level = "warn", Debug)
    )]
    pub async fn decompile_fn(&self, entry: u64) -> Result<String> {
        // fast path: return the cached decompilation if present
        if let Some(cached) = self.decomp_cache.lock().await.get(&entry) {
            return Ok(cached.clone());
        }
        // and the other fast path: replay a refusal instead of waiting out the
        // timeout that produced it a second time
        if let Some(reason) = self.decomp_failed.lock().await.get(&entry) {
            return Err(anyhow::anyhow!(reason.clone()));
        }
        // request decompilation of the function at the entry address
        let result = match self
            .ghidra
            .function
            .decompile(&fmt_addr(entry), false, false)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                // remember why, so the next caller pays nothing for the same answer
                self.decomp_failed
                    .lock()
                    .await
                    .insert(entry, err.to_string());
                return Err(err);
            }
        };
        // memoize the C source and return it
        self.decomp_cache
            .lock()
            .await
            .insert(entry, result.code.clone());
        Ok(result.code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `parse_addr` accepts bare and `0x`-prefixed hex, and rejects non-hex.
    #[test]
    fn parse_addr_handles_prefixes() {
        assert_eq!(parse_addr("10001000"), Some(0x1000_1000));
        assert_eq!(parse_addr("0x401000"), Some(0x40_1000));
        assert_eq!(parse_addr("not-hex"), None);
    }

    /// An `operands` array (Ghidra 12's shape) is joined into `"op0, op1"` so the
    /// detections' comma-based operand parsing keeps working.
    #[test]
    fn parse_instruction_joins_operand_array() {
        let value = json!({"address": "401000", "mnemonic": "MOV", "operands": ["EAX", "0x65"]});
        let insn = parse_instruction(&value).expect("instruction parses");
        assert_eq!(insn.mnemonic, "MOV");
        // the stored array is joined back into the detections' comma-separated form
        assert_eq!(crate::thorpal::detect::op_str(&insn), "EAX, 0x65");
    }

    /// A plain-string `operands` field is accepted unchanged.
    #[test]
    fn parse_instruction_accepts_operand_string() {
        let value = json!({"address": "401000", "mnemonic": "CPUID", "operands": "EAX"});
        let insn = parse_instruction(&value).expect("instruction parses");
        // a lone operand string round-trips as a single-element list
        assert_eq!(crate::thorpal::detect::op_str(&insn), "EAX");
    }

    /// Build a minimal instruction with the given mnemonic for thunk-body tests.
    fn insn(mnemonic: &str) -> Instruction {
        Instruction {
            address: "401000".to_string(),
            mnemonic: mnemonic.to_string(),
            operands: Vec::new(),
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// A lone `JMP` body is a thunk; anything else is not.
    #[test]
    fn is_thunk_body_detects_single_jmp() {
        // exactly one JMP → thunk (case-insensitive)
        assert!(is_thunk_body(&[insn("JMP")]));
        assert!(is_thunk_body(&[insn("jmp")]));
        // empty, multi-instruction, or non-jump → not a thunk
        assert!(!is_thunk_body(&[]));
        assert!(!is_thunk_body(&[insn("PUSH")]));
        assert!(!is_thunk_body(&[insn("JMP"), insn("RET")]));
    }

    /// Call/jump/thunk reference types are call-like, data types are not, empty is.
    #[test]
    fn call_like_ref_types_filter_data_references() {
        // the call/jump/thunk classes Ghidra renders for genuine control flow
        for kept in [
            "UNCONDITIONAL_CALL",
            "CONDITIONAL_CALL",
            "COMPUTED_CALL",
            "UNCONDITIONAL_JUMP",
            "THUNK",
        ] {
            assert!(is_call_like_ref(kept), "{kept} must be call-like");
        }
        // the data classes constant propagation records at import slots
        for data in ["DATA", "READ", "WRITE", "PARAM", "EXTERNAL_REF"] {
            assert!(!is_call_like_ref(data), "{data} must not be call-like");
        }
        // a missing/empty type is call-like: never silently lose a genuine call
        assert!(is_call_like_ref(""));
        assert!(is_call_like_ref("  "));
    }

    /// A data-typed reference is call-site evidence only inside a function: the x64
    /// `mov rsi, [import]; … call rsi` idiom records a READ at the `mov` (the shape
    /// behind the confirmed `WinHttp*`/`InternetOpen*`/`HttpOpenRequest` and
    /// `OutputDebugString` call-site leads), while a `VMProtect` import slot's READ
    /// has no containing function and stays dropped.
    #[test]
    fn data_refs_are_call_site_evidence_only_inside_functions() {
        // the mov-load shape: READ/DATA-typed, inside a real function → kept
        assert!(is_call_site_evidence("READ", Some(0x1400_0100)));
        assert!(is_call_site_evidence("DATA", Some(0x1400_0100)));
        // the VMProtect import-slot shape: data-typed, no containing function → dropped
        assert!(!is_call_site_evidence("READ", None));
        assert!(!is_call_site_evidence("DATA", None));
        assert!(!is_call_site_evidence("EXTERNAL_REF", None));
        // call/jump/thunk references are call sites even without a containing function
        assert!(is_call_site_evidence("UNCONDITIONAL_CALL", None));
        assert!(is_call_site_evidence("CONDITIONAL_JUMP", None));
        assert!(is_call_site_evidence("THUNK", None));
        // the fail-open empty type still never silently loses a genuine call
        assert!(is_call_site_evidence("", None));
        assert!(is_call_site_evidence("  ", None));
    }

    /// Only code addresses count as executable, and an unmapped one is kept.
    #[test]
    fn is_executable_addr_separates_code_from_data() {
        // build the two blocks a call-site reference can land in
        let block = |name: &str, base: u64, perms: &str| Section {
            name: name.to_string(),
            base,
            end: base + 0x1000,
            size: 0x1000,
            permissions: perms.to_string(),
            is_initialized: true,
            is_loaded: true,
            bytes: Vec::new(),
            truncated: false,
        };
        let sections = vec![
            block(".text", 0x180001000, "rx"),
            block(".rdata", 0x180019000, "r"),
        ];
        // a call in .text is a call site
        assert!(is_executable_addr(&sections, 0x180001234));
        // the import-address-table slot in .rdata is a pointer, not a call
        assert!(!is_executable_addr(&sections, 0x1800190b8));
        // an address outside the block table is kept (nothing is proven about it)
        assert!(is_executable_addr(&sections, 0x400000));
    }
}
