//! Tools for listing, searching, reading, and saving cross-run memories.
//!
//! Memories are durable GENERIC lessons kept across analysis runs in the
//! persistent [`MemoryLibrary`] (see
//! [`memories`](crate::thorpal::store::memories)): techniques that worked,
//! workflow rules, verified family intel, and pitfalls. The read-side tools
//! (`list_memories`, `search_memories`, `read_memory`) are how any
//! memory-enabled agent recalls prior-run knowledge; the write side
//! (`save_memory`) is the curation step, enabled only for the researcher in
//! the builtin workflow so the library stays deliberate rather than a dumping
//! ground.
//!
//! Every model-visible memory surface renders through the run's
//! [`LibraryRedactor`] when one is configured, exactly like the script
//! library: provenance always neutralizes, blinded runs render memory names as
//! `mem-<8 hex>` ids, deny-listed tokens scrub out of free text, and
//! `family-intel` memories matching the blind-family list are hidden outright
//! (treated as nonexistent on every surface), because family intel's content
//! can reveal a blinded eval's ground truth even after token scrubbing.

use std::collections::HashMap;
use std::sync::Arc;

use rig::tool::{ToolContext, ToolExecutionError};
use serde::Serialize;

use crate::thorpal::store::SessionStore;
use crate::thorpal::store::library::ScriptLibrary;
use crate::thorpal::store::memories::{
    MemoryEntry, MemoryKind, MemoryLibrary, MemoryMeta, redact_provenance, rewrite_header_name,
    strip_header, validate_body, validate_generic_terms, validate_healed_caps,
    validate_name_generic, validate_no_placeholders,
};
use crate::thorpal::store::redact::{
    LibraryRedactor, find_placeholder_shape, is_neutral_memory_id, neutral_memory_id,
    neutral_script_id,
};
use crate::thorpal::tools::forum::search_terms;
use crate::thorpal::tools::ghidra::GhidraSession;

/// Deterministic lexical ranking shared with the spawn-prompt memory index
pub(crate) mod search;

use search::{MatchedLine, MemoryDoc, matched_lines, rank_memory_docs};

/// The provenance recorded when a run has no session store to name.
const UNKNOWN_PROVENANCE: &str = "unknown-session";

/// The default number of entries one listing page returns
const LIST_DEFAULT_LIMIT: usize = 50;

/// The most entries one listing page may return
const LIST_MAX_LIMIT: usize = 200;

/// The length (in chars) a listed description is clipped to
const LIST_DESCRIPTION_CHARS: usize = 240;

/// One ranked hit from a search over the memory library
#[derive(Debug, Clone, Serialize)]
pub struct MemorySearchHit {
    /// The matched memory's name as rendered to the model (read it with
    /// `read_memory`)
    pub memory: String,
    /// What class of reusable knowledge the memory records
    pub kind: MemoryKind,
    /// The relevance score; exact-substring hits rank above score-only ones
    pub score: f32,
    /// Up to a few matched lines of the rendered source (1-indexed)
    pub lines: Vec<MatchedLine>,
}

/// One ranked, paged search result over the memory library
#[derive(Debug, Clone, Serialize)]
pub struct MemorySearchResults {
    /// The ranked page of hits
    pub hits: Vec<MemorySearchHit>,
    /// How many memories matched in total (page past them with `offset`)
    pub total_matches: usize,
    /// The offset this page started at
    pub offset: usize,
    /// Paging or byte-budget guidance, present whenever the page was cut
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One filtered, paged listing of the memory library
#[derive(Debug, Clone, Serialize)]
pub struct MemoryListing {
    /// One page of entries, descriptions clipped (read full text with
    /// `read_memory`)
    pub memories: Vec<MemoryEntry>,
    /// How many visible memories matched the filters in total
    pub total: usize,
    /// The offset this page started at
    pub offset: usize,
    /// Paging guidance, present whenever entries lie beyond this page
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Renders a loaded memory source through every model-visible redaction
///
/// # Arguments
///
/// * `source` - The raw memory source that was loaded
/// * `redactor` - The blinding redactor, when one is configured for this run
fn render_memory_source(source: &str, redactor: Option<&Arc<LibraryRedactor>>) -> String {
    // redact session-name provenance on every model-visible memory read
    let source = redact_provenance(source);
    // under blinding the header's `name:` line must render the neutral id
    // (the raw name is exactly what the listing withheld) and deny-listed
    // tokens scrub out of the whole source
    let mut rendered = match redactor {
        Some(redactor) => redactor.redact_text(&rewrite_header_name(&source, |name| {
            redactor.display_memory_name(name)
        })),
        None => source,
    };
    // a pitfall is a prior run's dead end, not a verdict about this binary:
    // scope it explicitly so a recalled wall gets re-tested, never inherited
    // (evaluation regressions anchored unreachable/inert verdicts on cross-run
    // pitfall notes while working decoders for the same bytes sat unsearched
    // in the script library)
    if MemoryMeta::parse(&rendered).kind == MemoryKind::Pitfall {
        rendered.push_str(
            "\n\n> Scope note: this pitfall records a PRIOR run's dead end, possibly on a \
             different binary. Treat it as a hypothesis to re-test here — search the script \
             library and attempt the approach on this sample before treating anything as \
             unreachable.",
        );
    }
    rendered
}

/// Whether a memory is hidden outright by the run's blind-family list
///
/// Loads the memory's source only when a hide can actually apply (a redactor
/// with blind families and a `family-intel` entry), so unblinded runs never
/// pay the extra read. A source that fails to load is treated as hidden,
/// erring on the side of withholding over leaking. The run's own saves are
/// exempt: hiding content the model itself just wrote would only build a
/// vanished-after-save oracle on the blind-family list. Shared with the
/// spawn-prompt memory index so every surface hides identically.
///
/// # Arguments
///
/// * `memories` - The memory library to load the candidate from
/// * `redactor` - The blinding redactor, when one is configured for this run
/// * `entry` - The listing entry to test
pub(crate) async fn hidden_family_intel(
    memories: &MemoryLibrary,
    redactor: Option<&Arc<LibraryRedactor>>,
    entry: &MemoryEntry,
) -> bool {
    // only family intel under a blind-family list can ever hide
    let Some(redactor) = redactor else {
        return false;
    };
    if !redactor.has_blind_families() || entry.kind != MemoryKind::FamilyIntel {
        return false;
    }
    // the run's own saves are the model's own knowledge and never hide
    if memories.saved_this_run(&entry.name) {
        return false;
    }
    // match the blind tokens against the memory's name and full source
    match memories.load(&entry.name).await {
        Ok(source) => redactor.hides_family_intel(&entry.name, &source),
        // withholding an unreadable candidate can never leak anything
        Err(_) => true,
    }
}

/// Whether an already-loaded memory is hidden by the blind-family list
///
/// The synchronous twin of [`hidden_family_intel`] for callers that already
/// hold the source, with the same own-save exemption.
///
/// # Arguments
///
/// * `memories` - The memory library the source came from
/// * `redactor` - The run's blinding redactor
/// * `name` - The bare memory name
/// * `source` - The memory's full source, header included
fn blind_hidden(
    memories: &MemoryLibrary,
    redactor: &LibraryRedactor,
    name: &str,
    source: &str,
) -> bool {
    // the run's own saves are the model's own knowledge and never hide
    !memories.saved_this_run(name) && redactor.hides_family_intel(name, source)
}

/// The miss error every unresolvable memory name gets
///
/// Shared by true misses and blind-hidden memories, so a hidden memory is
/// indistinguishable from a nonexistent one (no existence oracle).
///
/// # Arguments
///
/// * `name` - The memory name that failed to resolve
fn memory_miss(name: &str) -> ToolExecutionError {
    ToolExecutionError::invalid_args(format!(
        "no memory named '{name}' exists in the library; list_memories shows \
         every memory"
    ))
}

/// Resolves a `mem-<8 hex>` id to the unique VISIBLE memory name it aliases
///
/// Works with or without a redactor: ids baked into prior blinded runs' saves
/// must stay followable in production reads too. Blind-hidden family intel is
/// filtered out BEFORE matching, so probing a guessed hash can never
/// distinguish a hidden memory from a missing one; the run's own saves stay
/// resolvable through the shared [`hidden_family_intel`] exemption. Two
/// visible names hashing to one id is an explicit error, never a silent pick
/// (and ambiguity over the visible set only, so a hidden/visible pair
/// resolves to the visible one without signaling the hidden one exists).
///
/// # Arguments
///
/// * `memories` - The memory library whose names to scan
/// * `redactor` - The blinding redactor, when one is configured for this run
/// * `id` - The neutral id to resolve
async fn resolve_neutral_memory_name(
    memories: &Arc<MemoryLibrary>,
    redactor: Option<&Arc<LibraryRedactor>>,
    id: &str,
) -> Result<Option<String>, ToolExecutionError> {
    // only neutral-id-shaped names can be aliases at all
    if !is_neutral_memory_id(id) {
        return Ok(None);
    }
    // scan the visible names for the (unique) one hashing to this id
    let entries = memories
        .list()
        .await
        .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
    let mut resolved = None;
    for entry in entries {
        // a blind-hidden memory must be unresolvable exactly like a missing
        // one, or a guessed hash becomes an existence oracle on hidden intel
        if hidden_family_intel(memories, redactor, &entry).await {
            continue;
        }
        if neutral_memory_id(&entry.name) == id {
            // two names hashing identically must surface, never pick one
            if resolved.is_some() {
                return Err(ToolExecutionError::invalid_args(format!(
                    "the id '{id}' matches more than one memory; use the \
                     names list_memories shows"
                )));
            }
            resolved = Some(entry.name);
        }
    }
    Ok(resolved)
}

/// Builds the healable `mem-`/`lib-` id-to-real-name maps for save healing
///
/// A `None` value marks an id two names collide on, left in place for
/// validation to reject rather than silently picking one. Only names the
/// run's redactor renders PLAINLY are mapped: blind-hidden intel must stay
/// unresolvable, and a neutralized name healed into body text would read
/// straight back on the next render (bodies are not name-scanned), handing a
/// blinded run an id-to-name decoder. Under full anonymization the maps are
/// therefore empty and quoted ids reject in validation instead -- cite
/// memories by topic in blinded saves. A run without a script library simply
/// resolves no `lib-` ids, with the same rejection fallback.
///
/// # Arguments
///
/// * `memories` - The memory library whose names to map
/// * `scripts` - The script library whose stems to map, when the run has one
/// * `redactor` - The blinding redactor, when one is configured for this run
async fn neutral_id_maps(
    memories: &Arc<MemoryLibrary>,
    scripts: Option<&Arc<ScriptLibrary>>,
    redactor: Option<&Arc<LibraryRedactor>>,
) -> Result<
    (
        HashMap<String, Option<String>>,
        HashMap<String, Option<String>>,
    ),
    ToolExecutionError,
> {
    // map every healable memory name under its neutral id
    let mut mem_ids: HashMap<String, Option<String>> = HashMap::new();
    let entries = memories
        .list()
        .await
        .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
    for entry in entries {
        // hidden intel must stay unresolvable on the save path too
        if hidden_family_intel(memories, redactor, &entry).await {
            continue;
        }
        // only names the redactor renders PLAINLY may heal into text: bodies
        // are not name-scanned on later renders, so a healed hidden name
        // would read straight back -- an id-to-name decoder built out of
        // save-then-read. Deny tokens heal because renders re-scrub them;
        // names do not, so only already-visible ones qualify
        if redactor.is_some_and(|redactor| redactor.hides_memory(&entry.name)) {
            continue;
        }
        // an id two names collide on resolves to neither
        mem_ids
            .entry(neutral_memory_id(&entry.name))
            .and_modify(|existing| *existing = None)
            .or_insert(Some(entry.name));
    }
    // map every plainly-rendered script stem the same way (scripts have no
    // hidden class, but neutralized stems must not heal in either)
    let mut lib_ids: HashMap<String, Option<String>> = HashMap::new();
    if let Some(scripts) = scripts {
        let entries = scripts
            .list()
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
        for entry in entries {
            // the same read-back rule as memory names, over the script scheme
            if redactor.is_some_and(|redactor| redactor.hides(&entry.name)) {
                continue;
            }
            lib_ids
                .entry(neutral_script_id(&entry.name))
                .and_modify(|existing| *existing = None)
                .or_insert(Some(entry.name));
        }
    }
    Ok((mem_ids, lib_ids))
}

/// Rewrites resolvable neutral ids in model text to the real names they alias
///
/// Conservative by construction: only a standalone exact 12-char id rewrites
/// -- the adjacent characters must not be name characters (`[A-Za-z0-9-]`) --
/// so a legacy filename that merely embeds an id (`mem-<8 hex>-addendum`) is
/// never spliced mid-token. Unknown and ambiguous ids are left in place for
/// validation to reject with rewrite guidance instead.
///
/// # Arguments
///
/// * `text` - The model-supplied text to heal
/// * `mem_ids` - The visible memory id-to-name map
/// * `lib_ids` - The script id-to-name map
fn heal_neutral_ids(
    text: &str,
    mem_ids: &HashMap<String, Option<String>>,
    lib_ids: &HashMap<String, Option<String>>,
) -> String {
    // nothing to heal when neither id prefix appears at all
    if !text.contains("mem-") && !text.contains("lib-") {
        return text.to_owned();
    }
    // walk the bytes, splicing resolvable standalone ids as they appear
    let bytes = text.as_bytes();
    let is_name_char = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-';
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        // a candidate id is exactly 12 bytes of lowercase prefix + hex
        let candidate = text.get(index..index + 12);
        let resolved = candidate.and_then(|id| {
            // pick the map matching the id's prefix, byte-wise so a candidate
            // window landing inside multi-byte text can never split a char
            let map = match id.as_bytes() {
                [b'm', b'e', b'm', b'-', ..] => mem_ids,
                [b'l', b'i', b'b', b'-', ..] => lib_ids,
                _ => return None,
            };
            // the hex run must fill the candidate exactly
            if !id.as_bytes()[4..]
                .iter()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return None;
            }
            // the id must stand alone: no name character on either side
            if index > 0 && is_name_char(bytes[index - 1]) {
                return None;
            }
            if let Some(&next) = bytes.get(index + 12)
                && is_name_char(next)
            {
                return None;
            }
            // unknown and ambiguous ids stay put for validation to reject
            map.get(id).cloned().flatten()
        });
        match resolved {
            Some(name) => {
                // splice the real name over the whole id
                out.push_str(&name);
                index += 12;
            }
            None => {
                // copy the next full character through untouched
                let ch = text[index..]
                    .chars()
                    .next()
                    .expect("index is on a char boundary");
                out.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    out
}

#[thorium_derive::thorpal_tool(group = "memory")]
#[rig::tool_macro(
    description = "Page through the persistent cross-run memory library: durable GENERIC lessons saved by past analysis runs, each listed with its name, kind, clipped description, tags, and save date. Optional filters: `kind` (technique, workflow, family-intel, or pitfall) and `tag` (exact tag match); page with `offset` and `limit` (default 50 per page). The result reports the filtered total, so a page never reads as the whole library. Read a memory's full body with read_memory, or rank memories by topic with search_memories."
)]
pub async fn list_memories(
    #[rig(context)] context: &mut ToolContext,
    kind: Option<String>,
    tag: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<MemoryListing, rig::tool::ToolExecutionError> {
    // recover this run's memory library and list it, builtins included
    let memories = MemoryLibrary::from_context(context)?;
    // parse the kind filter with the same four-kind guidance saves give
    let kind = match kind {
        Some(text) => Some(MemoryKind::parse(&text).ok_or_else(|| {
            ToolExecutionError::invalid_args(format!(
                "'{text}' is not a memory kind; pass one of \"technique\", \
                 \"workflow\", \"family-intel\", or \"pitfall\""
            ))
        })?),
        None => None,
    };
    // normalize the tag filter like saved tags are normalized
    let tag = tag.map(|tag| tag.trim().to_lowercase());
    let entries = memories
        .list()
        .await
        .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
    // clamp the paging knobs to the supported window
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(LIST_DEFAULT_LIMIT).clamp(1, LIST_MAX_LIMIT);
    // render the listing through the blinding redactor when one is configured:
    // blind-hidden family intel drops out entirely (totals included), names
    // neutralize, and deny-listed tokens scrub out of descriptions and tags
    let redactor = LibraryRedactor::try_from_context(context);
    let mut listed = Vec::new();
    let mut total = 0usize;
    for entry in entries {
        // a blind-hidden memory must be indistinguishable from a missing one,
        // so it never shows up in a page OR a total
        if hidden_family_intel(&memories, redactor.as_ref(), &entry).await {
            continue;
        }
        // apply the filters before counting, so the total describes them
        if kind.is_some_and(|kind| kind != entry.kind) {
            continue;
        }
        if let Some(tag) = &tag {
            // match against the RENDERED tag only: matching raw tag text
            // under a redactor would let a model probe family tags it never
            // saw (a non-empty page for a guessed raw tag is a deny-list
            // membership oracle); unredacted runs match the raw text, which
            // IS the rendered form there
            let matches = entry.tags.iter().any(|candidate| match &redactor {
                Some(redactor) => redactor.redact_text(candidate) == *tag,
                None => candidate == tag,
            });
            if !matches {
                continue;
            }
        }
        // page by visible position; the name-sorted listing keeps it stable
        total += 1;
        if total <= offset || listed.len() >= limit {
            continue;
        }
        // render the page entry, clipping the description to keep pages light
        let (name, description, tags) = match &redactor {
            Some(redactor) => (
                redactor.display_memory_name(&entry.name),
                redactor.redact_text(&entry.description),
                entry
                    .tags
                    .iter()
                    .map(|tag| redactor.redact_text(tag))
                    .collect(),
            ),
            None => (entry.name, entry.description, entry.tags),
        };
        listed.push(MemoryEntry {
            name,
            kind: entry.kind,
            description: search::clip_chars(&description, LIST_DESCRIPTION_CHARS),
            tags,
            promoted_at: entry.promoted_at,
        });
    }
    // point past the page whenever more filtered entries remain
    let end = offset + listed.len();
    let note = (end < total).then(|| {
        format!(
            "showing {offset}..{end} of {total}; call again with offset={end}, \
             or rank by topic with search_memories"
        )
    });
    Ok(MemoryListing {
        memories: listed,
        total,
        offset,
        note,
    })
}

#[thorium_derive::thorpal_tool(group = "memory")]
#[rig::tool_macro(
    description = "Search the cross-run memory library and get back the top-ranked matching memories. Ranking is deterministic lexical: BM25 over names, tags, descriptions, and bodies, unioned with exact substring matching, so hex constants and literal strings (0x4BD, MC1=V=3) match verbatim while prose queries rank by relevance. Returns up to `limit` hits (default 8, max 25), each with up to 3 matched lines, plus the total match count; page further matches with `offset`. Search for the literal terms a relevant lesson would contain. Read a hit's full body with read_memory."
)]
pub async fn search_memories(
    #[rig(context)] context: &mut ToolContext,
    pattern: String,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<MemorySearchResults, rig::tool::ToolExecutionError> {
    // an empty pattern would match every memory meaninglessly
    if pattern.trim().is_empty() {
        return Err(ToolExecutionError::invalid_args(
            "`pattern` must not be empty".to_owned(),
        ));
    }
    // recover this run's memory library and the entries to score
    let memories = MemoryLibrary::from_context(context)?;
    let entries = memories
        .list()
        .await
        .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
    let redactor = LibraryRedactor::try_from_context(context);
    // clamp the paging knobs to the supported window
    let limit = limit
        .unwrap_or(search::DEFAULT_SEARCH_LIMIT)
        .clamp(1, search::MAX_SEARCH_LIMIT);
    let offset = offset.unwrap_or(0);
    // build one scorable document per VISIBLE memory from its rendered
    // source, so blinded runs search exactly the text they can read and a
    // hit line can never carry what redaction withheld
    let mut docs = Vec::with_capacity(entries.len());
    let mut sources = Vec::with_capacity(entries.len());
    for entry in entries {
        // a blind-hidden memory must never surface a hit or join a count
        if hidden_family_intel(&memories, redactor.as_ref(), &entry).await {
            continue;
        }
        // one unreadable memory (a hand-dropped stray the listing tolerates)
        // must not sink every search, mirroring the listing's own policy
        let source = match memories.load(&entry.name).await {
            Ok(source) => source,
            Err(error) => {
                tracing::warn!(name = entry.name, ?error, "skipping an unreadable memory");
                continue;
            }
        };
        let rendered = render_memory_source(&source, redactor.as_ref());
        // the hit's memory name renders exactly like list_memories shows it
        let display = match &redactor {
            Some(redactor) => redactor.display_memory_name(&entry.name),
            None => entry.name.clone(),
        };
        // parse the rendered header so the boosted fields are the redacted
        // ones, and double name/tags/description as a cheap field boost
        let meta = MemoryMeta::parse(&rendered);
        let tags = meta.tags.join(" ");
        let text = format!(
            "{display} {tags} {description} {display} {tags} {description} {body}",
            description = meta.description,
            body = strip_header(&rendered),
        );
        docs.push(MemoryDoc {
            name: entry.name,
            display,
            kind: entry.kind,
            promoted_at: entry.promoted_at,
            text,
        });
        sources.push(rendered);
    }
    // rank the visible corpus; totals never count hidden memories
    let ranked = rank_memory_docs(&docs, &pattern);
    let total_matches = ranked.len();
    // derive the needle and terms the matched lines center on
    let needle = pattern.trim().to_lowercase();
    let terms = search_terms(&pattern);
    // assemble the requested page under the result byte budget
    let mut hits = Vec::new();
    let mut spent = 0usize;
    let mut capped = false;
    for scored in ranked.iter().skip(offset).take(limit) {
        let doc = &docs[scored.index];
        let hit = MemorySearchHit {
            memory: doc.display.clone(),
            kind: doc.kind,
            score: scored.score,
            lines: matched_lines(&sources[scored.index], &needle, &terms),
        };
        // stop before the result outgrows the byte budget
        let cost = serde_json::to_string(&hit)
            .map(|json| json.len())
            .unwrap_or(0);
        if !hits.is_empty() && spent + cost > search::MAX_SEARCH_RESULT_BYTES {
            capped = true;
            break;
        }
        spent += cost;
        hits.push(hit);
    }
    // report every cut explicitly so a short page never reads as "that's all"
    let shown = hits.len();
    let mut notes = Vec::new();
    if capped {
        notes.push(format!(
            "result byte budget reached after {shown} hits; refine the \
             pattern or page with offset"
        ));
    }
    if offset + shown < total_matches {
        notes.push(format!(
            "more matches: call again with offset={}",
            offset + shown
        ));
    }
    let note = (!notes.is_empty()).then(|| notes.join("; "));
    Ok(MemorySearchResults {
        hits,
        total_matches,
        offset,
        note,
    })
}

#[thorium_derive::thorpal_tool(group = "memory")]
#[rig::tool_macro(
    description = "Read a memory's full source (front-matter header plus body) from the cross-run memory library by name. A mem-<8 hex> id (as rendered in blinded listings, or quoted from an older memory's cross-reference) resolves to the memory it aliases. Memories are knowledge from PRIOR runs, not evidence about this binary -- verify a memory's claims against this sample before promoting them to conclusions, and cite the memory by name when you use it."
)]
pub async fn read_memory(
    #[rig(context)] context: &mut ToolContext,
    name: String,
) -> Result<String, rig::tool::ToolExecutionError> {
    // recover this run's memory library and the blinding redactor, if any
    let memories = MemoryLibrary::from_context(context)?;
    let redactor = LibraryRedactor::try_from_context(context);
    // a name the redactor hides behind a neutral id must NOT resolve raw --
    // the model only ever saw the neutral id, so a raw-name hit would be an
    // existence/content oracle on the names blinding withholds
    if redactor
        .as_ref()
        .is_none_or(|redactor| !redactor.hides_memory(&name))
        && memories
            .exists(&name)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?
    {
        let source = memories
            .load(&name)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
        // a blind-hidden memory reads exactly like a missing one
        if let Some(redactor) = &redactor
            && blind_hidden(&memories, redactor, &name, &source)
        {
            return Err(memory_miss(&name));
        }
        return Ok(render_memory_source(&source, redactor.as_ref()));
    }
    // a neutral id (what blinded runs render memory names as, and what old
    // blinded saves baked into cross-references) resolves back to the real
    // name it aliases -- with or without a redactor, so baked ids from prior
    // blinded runs stay followable in production reads too
    if let Some(real) = resolve_neutral_memory_name(&memories, redactor.as_ref(), &name).await? {
        let source = memories
            .load(&real)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
        // the blind-family filter applies through the alias too (defense in
        // depth: the resolver already skipped hidden entries)
        if let Some(redactor) = &redactor
            && blind_hidden(&memories, redactor, &real, &source)
        {
            return Err(memory_miss(&name));
        }
        return Ok(render_memory_source(&source, redactor.as_ref()));
    }
    // the name misses everywhere (or is hidden, indistinguishably)
    Err(memory_miss(&name))
}

#[thorium_derive::thorpal_tool(group = "memory")]
#[rig::tool_macro(
    description = "Save a durable GENERIC lesson into the persistent cross-run memory library (~/.thorium/memories), so future analysis runs can find and reuse it. Only save lessons that are verified and will apply beyond this run; sample-specific facts (hashes, paths, decoded strings, session details) belong on the forum and are refused here. `kind` is one of: \"technique\" (how to decode/resolve/extract something), \"workflow\" (how to run an analysis well), \"family-intel\" (verified knowledge about a malware family), or \"pitfall\" (a mistake future runs should not repeat). Name and describe technique/workflow/pitfall memories by TECHNIQUE, never by malware family or sample name -- family-named memories render as opaque neutral ids in blinded runs and become hard to find. A family-intel memory is the one exception: it SHOULD carry the family's name and aliases in its name and tags, so family-blinded evaluations can withhold exactly that family's intel. Never paste fam-/mem-/lib-/session-<hex> placeholder ids into a memory -- they are blinded-run render artifacts; reference other memories and scripts by the names your tools show you, or by topic when a tool shows only ids (resolvable references are rewritten to real names on save; a save carrying unresolvable ones is refused). `description` says what the lesson teaches and when a future run should read it -- future runs search it to pick memories; keep it under 2 KB, with at most 16 short search `tags`. State NEGATIVE results as scoped observations (\"in the verified sample, X did not apply\"), never as invariants (\"X never applies\", \"not recoverable\", \"yields nothing\") -- protection profiles and build options vary per sample, and a one-sample negative written as a law has anchored later runs into skipping winning approaches. Saving over an existing memory name is refused unless `overwrite: true`."
)]
pub async fn save_memory(
    #[rig(context)] context: &mut ToolContext,
    name: String,
    kind: String,
    description: String,
    body: String,
    tags: Option<Vec<String>>,
    overwrite: Option<bool>,
) -> Result<String, rig::tool::ToolExecutionError> {
    // recover this run's memory library; saving needs nothing else to exist
    let memories = MemoryLibrary::from_context(context)?;
    let redactor = LibraryRedactor::try_from_context(context);
    // parse the kind first so every later error is about real content
    let kind = MemoryKind::parse(&kind).ok_or_else(|| {
        ToolExecutionError::invalid_args(format!(
            "'{kind}' is not a memory kind; pass one of \"technique\", \
             \"workflow\", \"family-intel\", or \"pitfall\""
        ))
    })?;
    // strip any stale header off a re-saved copy FIRST: a quoted header is
    // the dominant carrier of name:/promoted-from: artifacts, and healing and
    // validation must run on exactly the body that persists (a header-only
    // body must read as empty)
    let mut body = strip_header(&body).to_owned();
    let mut description = description;
    let mut tags = tags.unwrap_or_default();
    // every validation whose error can echo content or sizes runs BEFORE
    // healing, on the text exactly as the model wrote it: an error echoing
    // post-heal text (or its byte counts) would hand a blinded model the
    // restored tokens and resolved names every other surface withholds
    let probe = MemoryMeta::promoted_now(kind, tags.clone(), description.clone(), "pre-heal");
    probe.validate().map_err(ToolExecutionError::invalid_args)?;
    validate_body(&body).map_err(ToolExecutionError::invalid_args)?;
    // reject sample-specific content on the model's own text: long hex runs
    // plus this run's sample stems and session name, from the shared context
    let mut forbidden = Vec::new();
    if let Ok(ghidra) = GhidraSession::from_context(context) {
        // the analyzed samples' file stems tie a lesson to this run
        if let Some(stem) = ghidra.binary().file_stem().and_then(|stem| stem.to_str()) {
            forbidden.push(stem.to_owned());
        }
        for extra in ghidra.extra_binaries() {
            if let Some(stem) = extra.file_stem().and_then(|stem| stem.to_str()) {
                forbidden.push(stem.to_owned());
            }
        }
    }
    // assemble the provenance now so its name joins the forbidden terms
    let promoted_from = match SessionStore::try_from_context(context) {
        Some(store) => store.name().await,
        None => UNKNOWN_PROVENANCE.to_owned(),
    };
    forbidden.push(promoted_from.clone());
    let combined = format!("{}\n{description}\n{body}", tags.join(" "));
    validate_generic_terms(&combined, kind, &forbidden)
        .map_err(ToolExecutionError::invalid_args)?;
    // restore any quoted fam- placeholders back to the real tokens: the model
    // only ever saw redacted renders, so without this the placeholders would
    // persist into cross-run storage, where they are dead weight to every
    // other run and defeat blind-family hiding; reads re-redact, so this
    // run's own view never changes
    if let Some(redactor) = &redactor {
        description = redactor.unredact_text(&description);
        body = redactor.unredact_text(&body);
        tags = tags.iter().map(|tag| redactor.unredact_text(tag)).collect();
    }
    // rewrite resolvable mem-/lib- cross-reference ids (quoted out of blinded
    // renders or older corrupted memories) to the real names they alias, so
    // the stored cross-reference web works in every run; the maps only build
    // when an id prefix appears at all
    if [&description, &body]
        .iter()
        .any(|text| text.contains("mem-") || text.contains("lib-"))
        || tags
            .iter()
            .any(|tag| tag.contains("mem-") || tag.contains("lib-"))
    {
        let scripts = ScriptLibrary::try_from_context(context);
        let (mem_ids, lib_ids) =
            neutral_id_maps(&memories, scripts.as_ref(), redactor.as_ref()).await?;
        description = heal_neutral_ids(&description, &mem_ids, &lib_ids);
        body = heal_neutral_ids(&body, &mem_ids, &lib_ids);
        tags = tags
            .iter()
            .map(|tag| heal_neutral_ids(tag, &mem_ids, &lib_ids))
            .collect();
    }
    // a neutral-id name (what blinded runs render memory names as) publishes
    // over the real name it aliases, so an overwrite round trip lands on the
    // true file instead of minting a stray hash-named one; an unresolvable id
    // rejects with wording identical for hidden and missing targets, so a
    // guessed hash can never probe the blind-family list
    let publish_as = if is_neutral_memory_id(&name) {
        match resolve_neutral_memory_name(&memories, redactor.as_ref(), &name).await? {
            Some(real) => real,
            None => {
                return Err(ToolExecutionError::invalid_args(format!(
                    "'{name}' is a blinded alias id, not a memory name; save \
                     under a short descriptive real name (if you meant to \
                     update the memory listed under this id, it no longer \
                     resolves -- save your version under a new name)"
                )));
            }
        }
    } else if find_placeholder_shape(&name).is_some() {
        // a model-chosen name carrying ANY placeholder shape is refused: a
        // bare one can never be found again by topic, and an embedded fam-
        // shape would evade blind-family hiding forever (hiding matches the
        // real token, which the name never carries). Legacy shape-embedding
        // files stay resave-healable through the id-resolution branch above,
        // which never reaches this check
        return Err(ToolExecutionError::invalid_args(format!(
            "'{name}' carries a blinded redaction placeholder; name the \
             memory by its topic or distinguishing trait instead (reference \
             families, memories, and scripts in the tags and body -- quoted \
             placeholders there are restored or resolved on save)"
        )));
    } else {
        name.clone()
    };
    // family names belong only in family-intel: when this run can recognize
    // them (the redactor holds the deny tokens), refuse family-derived names
    // and tags on the generic kinds so family fingerprints cannot dodge
    // blind-family hiding by mis-filing; only a model-chosen raw name is
    // guarded, since an id-resolved resave of a legacy family-named file must
    // stay healable
    if let Some(redactor) = &redactor
        && kind != MemoryKind::FamilyIntel
        && ((publish_as == name && redactor.is_denied(&name))
            || tags.iter().any(|tag| redactor.is_denied(tag)))
    {
        return Err(ToolExecutionError::invalid_args(
            "this memory's name or tags reference a specific malware family \
             (possibly restored from a fam- placeholder you quoted); name and \
             tag technique/workflow/pitfall memories by TECHNIQUE -- family \
             facts belong in a kind: family-intel memory"
                .to_owned(),
        ));
    }
    // refuse to silently clobber an existing memory without the explicit
    // opt-in; a builtin's name counts (saving over it shadows the builtin)
    if !overwrite.unwrap_or(false)
        && memories
            .exists(&publish_as)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?
    {
        // a blind-hidden memory is nonexistent to this run on EVERY surface,
        // collision included: refusing here would be an existence oracle on
        // the very names reads withhold, so the save proceeds and overwrites
        // it (the library's last-writer-wins contract); visible collisions
        // keep refusing as usual
        let hidden = match (&redactor, memories.load(&publish_as).await) {
            (Some(redactor), Ok(existing)) => {
                blind_hidden(&memories, redactor, &publish_as, &existing)
            }
            // an unreadable existing file still counts as a real collision
            _ => false,
        };
        if !hidden {
            return Err(ToolExecutionError::invalid_args(format!(
                "a memory named '{name}' already exists; pass overwrite: true to \
                 save this run's version over it, or save under a different name"
            )));
        }
    }
    // assemble the header metadata, stamping this session as provenance
    let meta = MemoryMeta::promoted_now(kind, tags, description, promoted_from);
    // post-heal checks only ever report echo-free errors: healing can insert
    // restored tokens and resolved names a blinded run must never see
    // verbatim, so neither content nor byte counts may flow back out
    let combined = format!("{}\n{}\n{body}", meta.tags.join(" "), meta.description);
    validate_no_placeholders(&combined).map_err(ToolExecutionError::invalid_args)?;
    validate_healed_caps(&meta.description, &meta.tags, &body)
        .map_err(ToolExecutionError::invalid_args)?;
    // a model-chosen raw name is vetted too; an id-resolved publish_as names
    // a file that already exists, so re-vetting it would only block healing
    // (validate_name_generic skips just the placeholder rule, which the raw
    // name path already enforced in full above)
    if publish_as == name {
        validate_name_generic(&publish_as, kind, &forbidden)
            .map_err(ToolExecutionError::invalid_args)?;
    }
    // render the fresh header in and prove the result round-trips: the parsed
    // header must say exactly what was validated, so no field value can smuggle
    // a line that re-shapes the header on a future read
    let saved = format!("{}\n{body}", meta.render(&publish_as));
    let reparsed = MemoryMeta::parse(&saved);
    if reparsed != meta {
        return Err(ToolExecutionError::invalid_args(
            "the rendered memory failed to round-trip its own header; nothing \
             was saved -- remove unusual characters from the description and \
             tags and try again"
                .to_owned(),
        ));
    }
    // publish the documented memory into the library
    memories
        .save(&publish_as, &saved)
        .await
        .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
    // record the save as this run's own, so blind-family hiding never makes
    // the model's own write vanish from its reads (an oracle on the blind list)
    memories.note_saved(&publish_as);
    // the response names no byte count: healing can change the length, and
    // under blinding a size delta would leak restored-token lengths
    Ok(format!(
        "saved memory '{name}' (kind {}); future runs will find it via \
         search_memories and the memory index",
        kind.label()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pitfall memories render with the re-test scope note; others do not
    #[test]
    fn pitfall_renders_render_the_scope_note() {
        // a pitfall memory gains the hypothesis-not-verdict scope note
        let pitfall = MemoryMeta::promoted_now(
            MemoryKind::Pitfall,
            vec!["packer".to_owned()],
            "A dead end to avoid.",
            "batch-session",
        );
        let source = format!(
            "{}\nThe decoder path was a dead end.\n",
            pitfall.render("dead-end")
        );
        let rendered = render_memory_source(&source, None);
        assert!(rendered.contains("Scope note: this pitfall records a PRIOR run's dead end"));
        // a technique memory renders without it
        let technique = MemoryMeta::promoted_now(
            MemoryKind::Technique,
            vec!["decode".to_owned()],
            "How to decode a table.",
            "batch-session",
        );
        let source = format!(
            "{}\nWalk the table and decode.\n",
            technique.render("decode-how")
        );
        let rendered = render_memory_source(&source, None);
        assert!(!rendered.contains("Scope note:"));
    }

    /// Builds a tool context carrying a scratch memory library
    ///
    /// # Arguments
    ///
    /// * `dir` - The scratch root for the memory library
    async fn context_with_memories(dir: &std::path::Path) -> (ToolContext, Arc<MemoryLibrary>) {
        // open a scratch library and seed it into a fresh context
        let memories = MemoryLibrary::open(dir.join("memories"))
            .await
            .expect("library should open");
        let mut context = ToolContext::new();
        context.insert(Arc::clone(&memories));
        (context, memories)
    }

    /// Seeds one family-intel memory and one technique memory
    ///
    /// # Arguments
    ///
    /// * `memories` - The library to seed into
    async fn seed_memories(memories: &MemoryLibrary) {
        // a family-intel memory naming (and describing) a family
        let intel = MemoryMeta::promoted_now(
            MemoryKind::FamilyIntel,
            vec!["darkgate".to_owned()],
            "Verified DarkGate traits.",
            "batch-session",
        );
        memories
            .save(
                "darkgate-intel",
                &format!(
                    "{}\nDarkGate uses act=* params.\n",
                    intel.render("darkgate-intel")
                ),
            )
            .await
            .expect("seed intel");
        // a generic technique memory that merely mentions the family
        let technique = MemoryMeta::promoted_now(
            MemoryKind::Technique,
            vec!["stackstrings".to_owned()],
            "Recover stackstrings generically.",
            "batch-session",
        );
        memories
            .save(
                "stackstring-recovery",
                &format!(
                    "{}\nWorks on darkgate and others.\n",
                    technique.render("stackstring-recovery")
                ),
            )
            .await
            .expect("seed technique");
    }

    /// Reads render provenance-neutral and name-neutral under blinding.
    #[tokio::test]
    async fn reads_render_redacted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        seed_memories(&memories).await;
        // an unblinded read keeps the name but neutralizes provenance
        let plain = read_memory(&mut context, "stackstring-recovery".to_owned())
            .await
            .expect("plain read");
        assert!(plain.contains("name: stackstring-recovery"));
        assert!(plain.contains("promoted-from: session-"));
        assert!(!plain.contains("batch-session"));
        // a blinded read neutralizes the header name and scrubs tokens
        let redactor =
            LibraryRedactor::new(true, vec!["darkgate".to_owned()], Vec::new()).expect("redactor");
        context.insert(redactor);
        let listing = list_memories(&mut context, None, None, None, None)
            .await
            .expect("list");
        let neutral = listing
            .memories
            .iter()
            .find(|entry| entry.kind == MemoryKind::Technique && entry.name.starts_with("mem-"))
            .expect("neutralized technique entry")
            .name
            .clone();
        let blinded = read_memory(&mut context, neutral.clone())
            .await
            .expect("alias read");
        assert!(blinded.contains(&format!("name: {neutral}")));
        assert!(
            !blinded.to_lowercase().contains("darkgate"),
            "leak: {blinded}"
        );
        // the raw name is withheld, so reading it raw must miss
        let raw = read_memory(&mut context, "stackstring-recovery".to_owned()).await;
        assert!(raw.is_err(), "a hidden raw name must not resolve");
    }

    /// Blind families hide matching family intel from every surface.
    #[tokio::test]
    async fn blind_families_hide_intel_everywhere() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        seed_memories(&memories).await;
        // blind the darkgate family (an unanonymized, family-blinded run)
        let redactor =
            LibraryRedactor::new(false, Vec::new(), vec!["darkgate".to_owned()]).expect("redactor");
        context.insert(redactor);
        // the intel memory drops out of the listing, totals included
        let listing = list_memories(&mut context, None, None, None, None)
            .await
            .expect("list");
        assert!(
            listing
                .memories
                .iter()
                .all(|entry| entry.kind != MemoryKind::FamilyIntel),
            "family intel leaked into the listing: {listing:?}"
        );
        assert_eq!(
            listing.total,
            listing.memories.len(),
            "the total must count only visible entries"
        );
        assert!(
            listing
                .memories
                .iter()
                .any(|entry| entry.name.contains("stackstring") || entry.name.starts_with("mem-")),
            "the generic technique must stay visible"
        );
        // searching never surfaces a hidden memory's lines or counts it
        let results = search_memories(&mut context, "act=*".to_owned(), None, None)
            .await
            .expect("search");
        assert!(
            results.hits.is_empty() && results.total_matches == 0,
            "hidden intel surfaced in search: {results:?}"
        );
        // reading the hidden memory misses exactly like a nonexistent one
        let direct = read_memory(&mut context, "darkgate-intel".to_owned()).await;
        let missing = read_memory(&mut context, "no-such-memory".to_owned()).await;
        assert!(direct.is_err() && missing.is_err());
        assert_eq!(
            direct
                .unwrap_err()
                .to_string()
                .replace("darkgate-intel", "no-such-memory"),
            missing.unwrap_err().to_string(),
            "a hidden memory must be indistinguishable from a missing one"
        );
    }

    /// Saving validates kinds, collisions, and the generic-content rules.
    #[tokio::test]
    async fn saves_validate_and_collide() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        // an unknown kind is refused with the four real kinds listed
        let err = save_memory(
            &mut context,
            "lesson".to_owned(),
            "nonsense".to_owned(),
            "d".to_owned(),
            "b".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("bad kind");
        assert!(err.to_string().contains("family-intel"));
        // a hash-bearing body is refused as sample-specific
        let err = save_memory(
            &mut context,
            "lesson".to_owned(),
            "technique".to_owned(),
            "a lesson".to_owned(),
            format!("the sample {} did it", "ab".repeat(16)),
            None,
            None,
        )
        .await
        .expect_err("hash body");
        assert!(err.to_string().contains("GENERIC"));
        // a body that is nothing but a stale header strips to empty and is
        // refused, so a header-only save can never slip through
        let err = save_memory(
            &mut context,
            "lesson".to_owned(),
            "technique".to_owned(),
            "a lesson".to_owned(),
            "---\nname: lesson\n---\n".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("header-only body");
        assert!(err.to_string().contains("non-empty body"));
        // a clean save lands with a fresh header
        let message = save_memory(
            &mut context,
            "lesson".to_owned(),
            "pitfall".to_owned(),
            "Never trust recall.".to_owned(),
            "Verify against the binary.".to_owned(),
            Some(vec!["Attribution".to_owned()]),
            None,
        )
        .await
        .expect("clean save");
        assert!(message.contains("kind pitfall"));
        let saved = memories.load("lesson").await.expect("load saved");
        assert!(saved.contains("kind: pitfall"));
        assert!(saved.contains("tags: attribution"));
        assert!(saved.contains("promoted-from: unknown-session"));
        // a second save of the same name is refused without overwrite...
        let err = save_memory(
            &mut context,
            "lesson".to_owned(),
            "pitfall".to_owned(),
            "d".to_owned(),
            "b".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("collision");
        assert!(err.to_string().contains("overwrite"));
        // ...and lands with the explicit opt-in
        save_memory(
            &mut context,
            "lesson".to_owned(),
            "pitfall".to_owned(),
            "Replaced lesson.".to_owned(),
            "New body.".to_owned(),
            None,
            Some(true),
        )
        .await
        .expect("overwrite save");
        assert!(
            memories
                .load("lesson")
                .await
                .expect("reload")
                .contains("New body.")
        );
    }

    /// A run's own saves never vanish behind the blind-family filter, and a
    /// collision with blind-hidden intel overwrites instead of oracle-ing.
    #[tokio::test]
    async fn own_saves_stay_visible_and_hidden_collisions_overwrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        seed_memories(&memories).await;
        // blind the darkgate family; the seeded prior-run intel is hidden
        let redactor =
            LibraryRedactor::new(false, Vec::new(), vec!["darkgate".to_owned()]).expect("redactor");
        context.insert(redactor);
        assert!(
            read_memory(&mut context, "darkgate-intel".to_owned())
                .await
                .is_err(),
            "prior-run intel must be hidden"
        );
        // saving over the hidden name succeeds WITHOUT overwrite: refusing
        // would reveal the hidden entry exists (an existence oracle)
        let message = save_memory(
            &mut context,
            "darkgate-intel".to_owned(),
            "family-intel".to_owned(),
            "Fresh verified DarkGate traits.".to_owned(),
            "DarkGate pins its mutex name.".to_owned(),
            Some(vec!["darkgate".to_owned()]),
            None,
        )
        .await
        .expect("hidden collision must save, not oracle");
        assert!(message.contains("saved memory"));
        // the run's own save stays visible (no save-then-vanish oracle): it
        // lists (name-neutralized, since the blind token denies the name)...
        let listing = list_memories(&mut context, None, None, None, None)
            .await
            .expect("list");
        let own = listing
            .memories
            .iter()
            .find(|entry| entry.kind == MemoryKind::FamilyIntel)
            .expect("the run's own intel must stay listed");
        assert!(
            own.name.starts_with("mem-"),
            "denied names neutralize: {}",
            own.name
        );
        // ...and reads back through its neutral id with the NEW body (the
        // hidden prior content was overwritten), tokens scrubbed
        let read = read_memory(&mut context, own.name.clone())
            .await
            .expect("own save must read back");
        assert!(read.contains("mutex"), "the new body must have won: {read}");
        assert!(
            !read.contains("act=*"),
            "the prior hidden body must be gone"
        );
        assert!(
            !read.to_lowercase().contains("darkgate"),
            "tokens scrub: {read}"
        );
        // search also surfaces the run's own intel
        let results = search_memories(&mut context, "mutex".to_owned(), None, None)
            .await
            .expect("search");
        assert!(!results.hits.is_empty(), "own intel must be searchable");
    }

    /// Seeds one memory file with a rendered header
    ///
    /// # Arguments
    ///
    /// * `memories` - The library to seed into
    /// * `name` - The bare memory name
    /// * `kind` - The kind to file it under
    /// * `tags` - The search tags to file it under
    /// * `description` - The header description
    /// * `body` - The memory body
    async fn seed_one(
        memories: &MemoryLibrary,
        name: &str,
        kind: MemoryKind,
        tags: Vec<String>,
        description: &str,
        body: &str,
    ) {
        // render a full header so listings carry kind, tags, and date
        let meta = MemoryMeta::promoted_now(kind, tags, description, "seed-session");
        memories
            .save(name, &format!("{}\n{body}\n", meta.render(name)))
            .await
            .expect("seed memory");
    }

    /// Search ranks deterministically, pages, matches literals, and caps bytes
    #[tokio::test]
    async fn search_ranks_pages_and_caps_deterministically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        // a name/description match must outrank a body-only mention
        seed_one(
            &memories,
            "xor-key-recovery",
            MemoryKind::Technique,
            vec!["xor".to_owned()],
            "Recovering the xor key from decode loops.",
            "Walk the loop and difference the bytes.",
        )
        .await;
        seed_one(
            &memories,
            "buried-mention",
            MemoryKind::Pitfall,
            vec!["decoding".to_owned()],
            "About decode verification.",
            "One route uses an xor key deep in the body.",
        )
        .await;
        seed_one(
            &memories,
            "literal-carrier",
            MemoryKind::Technique,
            vec!["seeds".to_owned()],
            "Where the seed constant hides.",
            "The 0x4BD seed sits at the stream head.",
        )
        .await;
        let results = search_memories(&mut context, "xor key".to_owned(), None, None)
            .await
            .expect("search");
        assert_eq!(results.total_matches, 2, "{results:?}");
        assert_eq!(
            results.hits[0].memory, "xor-key-recovery",
            "field boost must lead"
        );
        assert_eq!(results.hits[1].memory, "buried-mention");
        // limit=1 returns exactly the head of the full ranking
        let head = search_memories(&mut context, "xor key".to_owned(), Some(1), None)
            .await
            .expect("search");
        assert_eq!(head.hits.len(), 1);
        assert_eq!(head.hits[0].memory, "xor-key-recovery");
        assert_eq!(head.total_matches, 2, "totals must be limit-invariant");
        assert!(
            head.note
                .as_deref()
                .is_some_and(|note| note.contains("offset=1")),
            "paging guidance missing: {head:?}"
        );
        // offset pages onto the next ranked hit
        let page = search_memories(&mut context, "xor key".to_owned(), Some(1), Some(1))
            .await
            .expect("search");
        assert_eq!(page.hits[0].memory, "buried-mention");
        // hex literals match verbatim and as bare sub-tokens
        for pattern in ["0x4BD", "4bd"] {
            let results = search_memories(&mut context, pattern.to_owned(), None, None)
                .await
                .expect("search");
            assert!(
                results
                    .hits
                    .iter()
                    .any(|hit| hit.memory == "literal-carrier"),
                "{pattern} missed: {results:?}"
            );
        }
        let literal = search_memories(&mut context, "0x4BD".to_owned(), None, None)
            .await
            .expect("search");
        assert!(
            literal.hits[0]
                .lines
                .iter()
                .any(|line| line.text.contains("0x4BD")),
            "the matched line must carry the literal: {literal:?}"
        );
        // a flood of heavy multi-byte hits stops at the result byte budget
        let heavy_line = format!("capneedle {}", "€".repeat(200));
        let heavy_body = format!("{heavy_line}\n{heavy_line}\n{heavy_line}");
        for index in 0..30 {
            seed_one(
                &memories,
                &format!("heavy-{index:02}"),
                MemoryKind::Technique,
                Vec::new(),
                "A heavy seeded memory.",
                &heavy_body,
            )
            .await;
        }
        let capped = search_memories(&mut context, "capneedle".to_owned(), Some(25), None)
            .await
            .expect("search");
        assert_eq!(capped.total_matches, 30);
        assert!(
            capped.hits.len() < 25,
            "the byte budget must cut the page: {}",
            capped.hits.len()
        );
        assert!(
            capped
                .note
                .as_deref()
                .is_some_and(|note| note.contains("byte budget")),
            "the cut must be reported: {:?}",
            capped.note
        );
    }

    /// Search hits render redacted and placeholders are searchable
    #[tokio::test]
    async fn search_renders_hits_redacted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        seed_memories(&memories).await;
        // a blinded, deny-listed run searches only rendered text
        let redactor =
            LibraryRedactor::new(true, vec!["darkgate".to_owned()], Vec::new()).expect("redactor");
        let placeholder = redactor.redact_text("darkgate");
        context.insert(redactor);
        let results = search_memories(&mut context, "act=*".to_owned(), None, None)
            .await
            .expect("search");
        assert!(!results.hits.is_empty(), "the intel body line must match");
        for hit in &results.hits {
            // names neutralize and no hit line carries a raw token or session
            assert!(hit.memory.starts_with("mem-"), "raw name leaked: {hit:?}");
            for line in &hit.lines {
                let lowered = line.text.to_lowercase();
                assert!(!lowered.contains("darkgate"), "leak: {line:?}");
                assert!(!lowered.contains("batch-session"), "leak: {line:?}");
            }
        }
        // the placeholder itself is searchable, since docs are rendered text
        let results = search_memories(&mut context, placeholder.clone(), None, None)
            .await
            .expect("search");
        assert!(
            !results.hits.is_empty(),
            "the scrubbed line must match its own placeholder"
        );
    }

    /// Listing filters by kind and tag, pages stably, and clips char-safely
    #[tokio::test]
    async fn list_memories_filters_pages_and_clips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        seed_one(
            &memories,
            "a-first-pitfall",
            MemoryKind::Pitfall,
            vec!["attribution".to_owned()],
            "A pitfall about attribution.",
            "Do not trust recall.",
        )
        .await;
        seed_one(
            &memories,
            "b-second-pitfall",
            MemoryKind::Pitfall,
            vec!["decoding".to_owned()],
            &"é".repeat(300),
            "A long-described pitfall.",
        )
        .await;
        seed_one(
            &memories,
            "c-technique",
            MemoryKind::Technique,
            vec!["darkgate".to_owned()],
            "A technique mentioning a family tag.",
            "Walk the loop.",
        )
        .await;
        // the kind filter narrows entries AND the reported total
        let pitfalls = list_memories(&mut context, Some("pitfall".to_owned()), None, None, None)
            .await
            .expect("list");
        assert_eq!(pitfalls.total, 2, "{pitfalls:?}");
        assert!(
            pitfalls
                .memories
                .iter()
                .all(|entry| entry.kind == MemoryKind::Pitfall)
        );
        // an unknown kind errors with the four real kinds listed
        let err = list_memories(&mut context, Some("nonsense".to_owned()), None, None, None)
            .await
            .expect_err("bad kind");
        assert!(
            err.to_string().contains("family-intel"),
            "unexpected: {err}"
        );
        // the tag filter matches saved tags exactly
        let tagged = list_memories(
            &mut context,
            None,
            Some("attribution".to_owned()),
            None,
            None,
        )
        .await
        .expect("list");
        assert_eq!(tagged.total, 1);
        assert_eq!(tagged.memories[0].name, "a-first-pitfall");
        // paging walks the name-sorted listing with explicit guidance
        let page = list_memories(
            &mut context,
            Some("pitfall".to_owned()),
            None,
            None,
            Some(1),
        )
        .await
        .expect("list");
        assert_eq!(page.memories[0].name, "a-first-pitfall");
        assert!(
            page.note
                .as_deref()
                .is_some_and(|note| note.contains("offset=1")),
            "paging guidance missing: {page:?}"
        );
        let page = list_memories(
            &mut context,
            Some("pitfall".to_owned()),
            None,
            Some(1),
            Some(1),
        )
        .await
        .expect("list");
        assert_eq!(page.memories[0].name, "b-second-pitfall");
        // a long description clips char-safely with a trailing ellipsis
        let clipped = &page.memories[0].description;
        assert!(clipped.ends_with('…'), "no ellipsis: {clipped}");
        assert!(
            clipped.chars().count() <= 241,
            "clip overflow: {}",
            clipped.chars().count()
        );
        // under a deny list, listed tags redact and the redacted form filters
        let redactor =
            LibraryRedactor::new(false, vec!["darkgate".to_owned()], Vec::new()).expect("redactor");
        let placeholder = redactor.redact_text("darkgate");
        context.insert(redactor);
        let filtered = list_memories(&mut context, None, Some(placeholder.clone()), None, None)
            .await
            .expect("list");
        assert_eq!(filtered.total, 1, "{filtered:?}");
        assert!(
            filtered.memories[0]
                .tags
                .iter()
                .any(|tag| tag == &placeholder),
            "listed tags must render redacted: {filtered:?}"
        );
        // probing the RAW family tag finds nothing under a redactor: a raw
        // match would be a deny-list membership oracle
        let probed = list_memories(&mut context, None, Some("darkgate".to_owned()), None, None)
            .await
            .expect("list");
        assert_eq!(probed.total, 0, "raw tag probe must miss: {probed:?}");
    }

    /// Saves restore quoted placeholders to real tokens before persisting
    #[tokio::test]
    async fn save_unredacts_placeholders_before_persisting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        // a deny-token redactor, exactly like a blinded eval run carries
        let redactor =
            LibraryRedactor::new(false, vec!["darkgate".to_owned()], Vec::new()).expect("redactor");
        // the model only ever sees the placeholder form of the family name
        let placeholder = redactor.redact_text("darkgate");
        assert!(placeholder.starts_with("fam-"), "sanity: {placeholder}");
        context.insert(redactor);
        let message = save_memory(
            &mut context,
            "loader-import-walk".to_owned(),
            "technique".to_owned(),
            format!("How the {placeholder} loader resolves imports."),
            format!("The {placeholder} loader walks the PEB.\n"),
            Some(vec!["loaders".to_owned()]),
            None,
        )
        .await
        .expect("save should heal and land");
        // the response reports no byte count: a size delta would leak the
        // restored token's length under blinding
        assert!(!message.contains("bytes"), "byte-count oracle: {message}");
        // the persisted file carries the real token, not the placeholder
        let saved = memories.load("loader-import-walk").await.expect("load");
        assert!(saved.contains("darkgate"), "token not restored: {saved}");
        assert!(!saved.contains("fam-"), "placeholder persisted: {saved}");
        // a read in the same blinded run re-renders the placeholder
        let rendered = read_memory(&mut context, "loader-import-walk".to_owned())
            .await
            .expect("own save reads back");
        assert!(
            !rendered.to_lowercase().contains("darkgate"),
            "leak: {rendered}"
        );
    }

    /// Unresolvable placeholder residue rejects without writing anything
    #[tokio::test]
    async fn save_rejects_unresolvable_placeholders_instructively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        // no redactor and a near-empty library: none of these can resolve
        for body in [
            "the fam-9c2e loader",
            "see mem-0123abcd for the route",
            "run lib-0123abcd first",
            "found in session-0123abcd",
        ] {
            let err = save_memory(
                &mut context,
                "residue".to_owned(),
                "technique".to_owned(),
                "a lesson".to_owned(),
                body.to_owned(),
                None,
                None,
            )
            .await
            .expect_err("placeholder residue must reject");
            assert!(err.to_string().contains("placeholder"), "unexpected: {err}");
        }
        // nothing was written on any attempt
        assert!(memories.load("residue").await.is_err());
    }

    /// Resolvable neutral ids heal into real names only where names render
    #[tokio::test]
    async fn save_heals_neutral_ids_into_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        seed_memories(&memories).await;
        // a script library with one stem backs the lib- id route
        let scripts = ScriptLibrary::open(dir.path().join("scripts"))
            .await
            .expect("script library should open");
        scripts
            .save("decode_helper", "// a helper\n")
            .await
            .expect("seed script");
        context.insert(Arc::clone(&scripts));
        // a production run quotes ids baked into legacy memories; both heal
        let mem_id = neutral_memory_id("stackstring-recovery");
        let lib_id = neutral_script_id("decode_helper");
        save_memory(
            &mut context,
            "recovery-route".to_owned(),
            "technique".to_owned(),
            "How to chain the recovery route.".to_owned(),
            format!("First see {mem_id}, then run {lib_id}.\n"),
            None,
            None,
        )
        .await
        .expect("resolvable ids must heal and save");
        // the persisted body cites both stores by their real names
        let saved = memories.load("recovery-route").await.expect("load");
        assert!(saved.contains("see stackstring-recovery"), "{saved}");
        assert!(saved.contains("run decode_helper"), "{saved}");
        assert!(!saved.contains(&mem_id), "memory id persisted: {saved}");
        assert!(!saved.contains(&lib_id), "script id persisted: {saved}");
        // under full anonymization names never render, so healing an id into
        // a body the model can read back would be an id-to-name decoder: the
        // save is refused instead, and the model cites memories by topic
        let redactor = LibraryRedactor::new(true, Vec::new(), Vec::new()).expect("redactor");
        context.insert(redactor);
        let err = save_memory(
            &mut context,
            "decoder-probe".to_owned(),
            "technique".to_owned(),
            "A probe.".to_owned(),
            format!("read back {mem_id} please\n"),
            None,
            None,
        )
        .await
        .expect_err("ids must not heal where names never render");
        assert!(err.to_string().contains("placeholder"), "unexpected: {err}");
        assert!(
            memories.load("decoder-probe").await.is_err(),
            "nothing may persist from the refused probe"
        );
    }

    /// Placeholder-shaped names reject; ids of visible memories resolve
    #[tokio::test]
    async fn save_rejects_placeholder_shaped_names_without_oracle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        seed_memories(&memories).await;
        // blind the darkgate family under full anonymization
        let redactor =
            LibraryRedactor::new(true, Vec::new(), vec!["darkgate".to_owned()]).expect("redactor");
        context.insert(redactor);
        // saving under the HIDDEN intel's id errs exactly like a missing one
        let hidden = save_memory(
            &mut context,
            neutral_memory_id("darkgate-intel"),
            "technique".to_owned(),
            "d".to_owned(),
            "b".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("a hidden target must not resolve");
        let missing = save_memory(
            &mut context,
            neutral_memory_id("no-such-memory"),
            "technique".to_owned(),
            "d".to_owned(),
            "b".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("a missing target must not resolve");
        assert_eq!(
            hidden.to_string().replace(
                &neutral_memory_id("darkgate-intel"),
                &neutral_memory_id("no-such-memory")
            ),
            missing.to_string(),
            "hidden and missing ids must be indistinguishable"
        );
        // a visible memory's id resolves to the real file: collision first...
        let err = save_memory(
            &mut context,
            neutral_memory_id("stackstring-recovery"),
            "technique".to_owned(),
            "d".to_owned(),
            "b".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("the resolved name collides");
        assert!(err.to_string().contains("overwrite"), "unexpected: {err}");
        // ...and the opt-in lands on the true file, not a stray hash name
        save_memory(
            &mut context,
            neutral_memory_id("stackstring-recovery"),
            "technique".to_owned(),
            "Replaced lesson.".to_owned(),
            "New body.".to_owned(),
            None,
            Some(true),
        )
        .await
        .expect("overwrite via id");
        assert!(
            memories
                .load("stackstring-recovery")
                .await
                .expect("reload")
                .contains("New body."),
            "the id save must land on the real file"
        );
        // bare lib-/fam-/session- shaped names reject outright
        for name in ["lib-0123abcd", "fam-0a08", "session-0123abcd"] {
            let err = save_memory(
                &mut context,
                name.to_owned(),
                "technique".to_owned(),
                "d".to_owned(),
                "b".to_owned(),
                None,
                None,
            )
            .await
            .expect_err("placeholder-shaped names must reject");
            assert!(err.to_string().contains("placeholder"), "unexpected: {err}");
        }
    }

    /// Family-derived names and tags reject on generic kinds only
    #[tokio::test]
    async fn save_rejects_denied_names_and_tags_for_generic_kinds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        // the run knows the family tokens through a deny-list redactor
        let redactor =
            LibraryRedactor::new(false, vec!["darkgate".to_owned()], Vec::new()).expect("redactor");
        let placeholder = redactor.redact_text("darkgate");
        context.insert(redactor);
        // a family-derived technique name rejects with the kind guidance
        let err = save_memory(
            &mut context,
            "darkgate-loader-trick".to_owned(),
            "technique".to_owned(),
            "d".to_owned(),
            "b".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("denied names must reject on generic kinds");
        assert!(
            err.to_string().contains("family-intel"),
            "unexpected: {err}"
        );
        // a quoted placeholder tag heals to the family token and rejects too
        let err = save_memory(
            &mut context,
            "loader-trick".to_owned(),
            "pitfall".to_owned(),
            "d".to_owned(),
            "b".to_owned(),
            Some(vec![placeholder.clone()]),
            None,
        )
        .await
        .expect_err("denied tags must reject on generic kinds");
        assert!(
            err.to_string().contains("family-intel"),
            "unexpected: {err}"
        );
        // the same name and tag are family-intel's whole point
        save_memory(
            &mut context,
            "darkgate-loader-trick".to_owned(),
            "family-intel".to_owned(),
            "Verified DarkGate loader traits.".to_owned(),
            "DarkGate walks the PEB.".to_owned(),
            Some(vec!["darkgate".to_owned()]),
            None,
        )
        .await
        .expect("family intel may name its family");
        // an id-resolved resave of a legacy family-named technique must stay
        // healable, so blinded runs can clean up old corruption in place
        let legacy = MemoryMeta::promoted_now(
            MemoryKind::Technique,
            vec!["decoding".to_owned()],
            "A legacy technique under a family-derived name.",
            "old-session",
        );
        memories
            .save(
                "darkgate-string-decode",
                &format!(
                    "{}\nWalk the table.\n",
                    legacy.render("darkgate-string-decode")
                ),
            )
            .await
            .expect("seed legacy");
        save_memory(
            &mut context,
            neutral_memory_id("darkgate-string-decode"),
            "technique".to_owned(),
            "A healed legacy technique.".to_owned(),
            "Walk the table generically.".to_owned(),
            Some(vec!["decoding".to_owned()]),
            Some(true),
        )
        .await
        .expect("an id-resolved resave of a legacy name must stay healable");
        assert!(
            memories
                .load("darkgate-string-decode")
                .await
                .expect("reload")
                .contains("generically")
        );
    }

    /// Post-heal save errors never echo restored tokens or byte counts
    #[tokio::test]
    async fn post_heal_errors_echo_no_restored_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        // a long family token: restoring its 8-char placeholder lengthens text
        let token = "superlongfamilyname";
        let redactor =
            LibraryRedactor::new(false, vec![token.to_owned()], Vec::new()).expect("redactor");
        let placeholder = redactor.redact_text(token);
        context.insert(redactor);
        // a tag exactly at the cap pre-heal crosses it once healed
        let tag = format!("{placeholder}{}", "x".repeat(48 - placeholder.len()));
        let err = save_memory(
            &mut context,
            "family-token-cap-probe".to_owned(),
            "family-intel".to_owned(),
            "d".to_owned(),
            "b".to_owned(),
            Some(vec![tag]),
            None,
        )
        .await
        .expect_err("the healed tag crosses the cap");
        let message = err.to_string();
        // the error names neither the restored token nor any byte count
        assert!(!message.contains(token), "token echoed: {message}");
        assert!(!message.contains("59"), "length echoed: {message}");
        assert!(message.contains("size cap"), "unexpected error: {message}");
        assert!(memories.load("family-token-cap-probe").await.is_err());
    }

    /// This run's sample stems from the Ghidra session stay forbidden
    #[tokio::test]
    async fn save_rejects_this_runs_sample_stems() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        // a Ghidra session names the analyzed binary, like a real run's does
        let ghidra =
            GhidraSession::new(crate::thorpal::tools::ghidra::GhidraSettings::from_binary(
                "bin/keentroll_dropper",
                "projects",
                None,
            ));
        context.insert(ghidra);
        let err = save_memory(
            &mut context,
            "loader-note".to_owned(),
            "technique".to_owned(),
            "a lesson".to_owned(),
            "seen in KeenTroll_dropper first".to_owned(),
            None,
            None,
        )
        .await
        .expect_err("the sample stem must stay forbidden");
        assert!(err.to_string().contains("GENERIC"), "unexpected: {err}");
        assert!(memories.load("loader-note").await.is_err());
    }

    /// The text healer only rewrites standalone, uniquely-resolvable ids
    #[test]
    fn heal_neutral_ids_is_standalone_and_conservative() {
        // one resolvable id, one ambiguous id, and the script route
        let mem_id = neutral_memory_id("stackstring-recovery");
        let dup_id = neutral_memory_id("colliding-memory-71748");
        let lib_id = neutral_script_id("decode_helper");
        let mem_ids = HashMap::from([
            (mem_id.clone(), Some("stackstring-recovery".to_owned())),
            (dup_id.clone(), None),
        ]);
        let lib_ids = HashMap::from([(lib_id.clone(), Some("decode_helper".to_owned()))]);
        // standalone ids heal, punctuation-adjacent included
        let healed = heal_neutral_ids(&format!("see {mem_id}, run {lib_id}."), &mem_ids, &lib_ids);
        assert_eq!(healed, "see stackstring-recovery, run decode_helper.");
        // a name-embedded id never splices (legacy stray filenames)
        let embedded = format!("the {mem_id}-wave2-addendum file");
        assert_eq!(heal_neutral_ids(&embedded, &mem_ids, &lib_ids), embedded);
        // unknown and ambiguous ids stay put for validation to reject
        let unknown = "see mem-0123abcd here".to_owned();
        assert_eq!(heal_neutral_ids(&unknown, &mem_ids, &lib_ids), unknown);
        let ambiguous = format!("see {dup_id} here");
        assert_eq!(heal_neutral_ids(&ambiguous, &mem_ids, &lib_ids), ambiguous);
        // multi-byte neighbors never split and still allow the heal
        let healed = heal_neutral_ids(&format!("é {mem_id} é"), &mem_ids, &lib_ids);
        assert_eq!(healed, "é stackstring-recovery é");
    }

    /// Baked neutral ids resolve on reads even without a redactor
    #[tokio::test]
    async fn read_memory_resolves_neutral_ids_without_a_redactor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        seed_memories(&memories).await;
        // a production (redactor-less) read follows an old baked id
        let source = read_memory(&mut context, neutral_memory_id("stackstring-recovery"))
            .await
            .expect("baked ids must resolve in production");
        assert!(source.contains("name: stackstring-recovery"), "{source}");
        // an unknown id misses exactly like a missing name
        let id = neutral_memory_id("never-saved");
        let err = read_memory(&mut context, id.clone())
            .await
            .expect_err("miss");
        assert_eq!(err.to_string(), memory_miss(&id).to_string());
    }

    /// Two names hashing to one id surface an explicit ambiguity error
    #[tokio::test]
    async fn neutral_id_ambiguity_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut context, memories) = context_with_memories(dir.path()).await;
        // a precomputed sha256-prefix collision pair (both hash to d59031b9)
        let (first, second) = ("colliding-memory-71748", "colliding-memory-83895");
        assert_eq!(
            neutral_memory_id(first),
            neutral_memory_id(second),
            "the pair must still collide"
        );
        memories.save(first, "one\n").await.expect("seed first");
        memories.save(second, "two\n").await.expect("seed second");
        let err = read_memory(&mut context, neutral_memory_id(first))
            .await
            .expect_err("an ambiguous id must error");
        assert!(
            err.to_string().contains("more than one"),
            "unexpected: {err}"
        );
    }
}
