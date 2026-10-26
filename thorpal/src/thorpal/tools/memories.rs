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

use std::sync::Arc;

use rig::tool::{ToolContext, ToolExecutionError};
use serde::Serialize;

use crate::thorpal::store::SessionStore;
use crate::thorpal::store::memories::{
    MemoryEntry, MemoryKind, MemoryLibrary, MemoryMeta, redact_provenance, rewrite_header_name,
    strip_header, validate_body, validate_generic,
};
use crate::thorpal::store::redact::LibraryRedactor;
use crate::thorpal::tools::ghidra::GhidraSession;

/// The provenance recorded when a run has no session store to name.
const UNKNOWN_PROVENANCE: &str = "unknown-session";

/// One matched line from a search over the memory library
#[derive(Debug, Clone, Serialize)]
pub struct MemoryMatch {
    /// The name of the memory this match is in (as rendered to the model)
    pub memory: String,
    /// The 1-indexed line number of the match
    pub line: usize,
    /// The matched line's text
    pub text: String,
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

#[thorium_derive::thorpal_tool(group = "memory")]
#[rig::tool_macro(
    description = "List every memory in the persistent cross-run memory library: durable GENERIC lessons saved by past analysis runs, each with its name, kind (technique, workflow, family-intel, or pitfall), and description. Check this (or your task prompt's memory index) before re-deriving a technique or answering from recall -- a proven lesson may already exist. Read a memory's full body with read_memory."
)]
pub async fn list_memories(
    #[rig(context)] context: &mut ToolContext,
) -> Result<Vec<MemoryEntry>, rig::tool::ToolExecutionError> {
    // recover this run's memory library and list it, builtins included
    let memories = MemoryLibrary::from_context(context)?;
    let entries = memories
        .list()
        .await
        .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
    // render the listing through the blinding redactor when one is configured:
    // blind-hidden family intel drops out entirely, names neutralize, and
    // deny-listed tokens scrub out of the descriptions
    let redactor = LibraryRedactor::try_from_context(context);
    let mut listed = Vec::with_capacity(entries.len());
    for entry in entries {
        // a blind-hidden memory must be indistinguishable from a missing one
        if hidden_family_intel(&memories, redactor.as_ref(), &entry).await {
            continue;
        }
        match &redactor {
            Some(redactor) => listed.push(MemoryEntry {
                name: redactor.display_memory_name(&entry.name),
                kind: entry.kind,
                description: redactor.redact_text(&entry.description),
            }),
            None => listed.push(entry),
        }
    }
    Ok(listed)
}

#[thorium_derive::thorpal_tool(group = "memory")]
#[rig::tool_macro(
    description = "Search every memory in the cross-run memory library for a case-insensitive substring, returning each matching line with its memory name and line number. Headers (kind, tags, description) are searched too, so this finds applicable lessons by topic. Read a hit's full body with read_memory."
)]
pub async fn search_memories(
    #[rig(context)] context: &mut ToolContext,
    pattern: String,
) -> Result<Vec<MemoryMatch>, rig::tool::ToolExecutionError> {
    // an empty pattern would match every line of every memory
    if pattern.is_empty() {
        return Err(ToolExecutionError::invalid_args(
            "`pattern` must not be empty".to_owned(),
        ));
    }
    // recover this run's memory library and the entries to search
    let memories = MemoryLibrary::from_context(context)?;
    let entries = memories
        .list()
        .await
        .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
    let redactor = LibraryRedactor::try_from_context(context);
    // match case-insensitively so searches are forgiving
    let needle = pattern.to_lowercase();
    let mut found = Vec::new();
    for entry in entries {
        // a blind-hidden memory must never surface a hit line
        if hidden_family_intel(&memories, redactor.as_ref(), &entry).await {
            continue;
        }
        let source = memories
            .load(&entry.name)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
        // redact before matching so a hit line can never carry a raw session
        // name or family token into model context
        let source = render_memory_source(&source, redactor.as_ref());
        // the hit's memory name renders exactly like list_memories shows it
        let display = match &redactor {
            Some(redactor) => redactor.display_memory_name(&entry.name),
            None => entry.name.clone(),
        };
        // scan the redacted source line by line for the needle
        for (index, text) in source.lines().enumerate() {
            if text.to_lowercase().contains(&needle) {
                found.push(MemoryMatch {
                    memory: display.clone(),
                    line: index + 1,
                    text: text.to_owned(),
                });
            }
        }
    }
    Ok(found)
}

#[thorium_derive::thorpal_tool(group = "memory")]
#[rig::tool_macro(
    description = "Read a memory's full source (front-matter header plus body) from the cross-run memory library by name. Memories are knowledge from PRIOR runs, not evidence about this binary -- verify a memory's claims against this sample before promoting them to conclusions, and cite the memory by name when you use it."
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
    // a neutral id (what blinded runs render memory names as) resolves back
    // to the real name it aliases, so reads work on what list_memories shows
    if let Some(redactor) = &redactor
        && let Some(real) = redactor
            .resolve_memory_alias(&memories, &name)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?
    {
        let source = memories
            .load(&real)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?;
        // the blind-family filter applies through the alias too
        if blind_hidden(&memories, redactor, &real, &source) {
            return Err(memory_miss(&name));
        }
        return Ok(render_memory_source(&source, Some(redactor)));
    }
    // the name misses everywhere (or is hidden, indistinguishably)
    Err(memory_miss(&name))
}

#[thorium_derive::thorpal_tool(group = "memory")]
#[rig::tool_macro(
    description = "Save a durable GENERIC lesson into the persistent cross-run memory library (~/.thorium/memories), so future analysis runs can find and reuse it. Only save lessons that are verified and will apply beyond this run; sample-specific facts (hashes, paths, decoded strings, session details) belong on the forum and are refused here. `kind` is one of: \"technique\" (how to decode/resolve/extract something), \"workflow\" (how to run an analysis well), \"family-intel\" (verified knowledge about a malware family), or \"pitfall\" (a mistake future runs should not repeat). Name and describe technique/workflow/pitfall memories by TECHNIQUE, never by malware family or sample name -- family-named memories render as opaque neutral ids in blinded runs and become hard to find. A family-intel memory is the one exception: it SHOULD carry the family's name and aliases in its name and tags, so family-blinded evaluations can withhold exactly that family's intel. `description` says what the lesson teaches and when a future run should read it -- future runs search it to pick memories; keep it under 2 KB, with at most 16 short search `tags`. State NEGATIVE results as scoped observations (\"in the verified sample, X did not apply\"), never as invariants (\"X never applies\", \"not recoverable\", \"yields nothing\") -- protection profiles and build options vary per sample, and a one-sample negative written as a law has anchored later runs into skipping winning approaches. Saving over an existing memory name is refused unless `overwrite: true`."
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
    // a neutral-id name (what blinded runs render memory names as) publishes
    // over the real name it aliases, so an overwrite round trip lands on the
    // true file instead of minting a stray hash-named one
    let publish_as = match &redactor {
        Some(redactor) => redactor
            .resolve_memory_alias(&memories, &name)
            .await
            .map_err(|error| ToolExecutionError::invalid_args(error.to_string()))?
            .unwrap_or_else(|| name.clone()),
        None => name.clone(),
    };
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
    let promoted_from = match SessionStore::try_from_context(context) {
        Some(store) => store.name().await,
        None => UNKNOWN_PROVENANCE.to_owned(),
    };
    let meta = MemoryMeta::promoted_now(
        kind,
        tags.unwrap_or_default(),
        description,
        promoted_from.clone(),
    );
    // strip any stale header off a re-saved copy FIRST, so validation runs on
    // exactly the body that persists (a header-only body must read as empty)
    let body = strip_header(&body);
    // validate the docs and the stripped body before anything is written
    meta.validate().map_err(ToolExecutionError::invalid_args)?;
    validate_body(body).map_err(ToolExecutionError::invalid_args)?;
    // reject sample-specific content: long hex runs plus this run's sample
    // stems and session name, gathered from the shared context
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
    forbidden.push(promoted_from);
    let combined = format!(
        "{publish_as}\n{}\n{}\n{body}",
        meta.tags.join(" "),
        meta.description
    );
    validate_generic(&combined, kind, &forbidden).map_err(ToolExecutionError::invalid_args)?;
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
    Ok(format!(
        "saved memory '{name}' ({} bytes, kind {}); future runs will find it \
         via list_memories/search_memories",
        saved.len(),
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
        let listing = list_memories(&mut context).await.expect("list");
        let neutral = listing
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
        // the intel memory drops out of the listing; the technique stays
        let listing = list_memories(&mut context).await.expect("list");
        assert!(
            listing
                .iter()
                .all(|entry| entry.kind != MemoryKind::FamilyIntel),
            "family intel leaked into the listing: {listing:?}"
        );
        assert!(
            listing
                .iter()
                .any(|entry| entry.name.contains("stackstring") || entry.name.starts_with("mem-")),
            "the generic technique must stay visible"
        );
        // searching never surfaces a hidden memory's lines
        let hits = search_memories(&mut context, "act=*".to_owned())
            .await
            .expect("search");
        assert!(hits.is_empty(), "hidden intel surfaced in search: {hits:?}");
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
        let listing = list_memories(&mut context).await.expect("list");
        let own = listing
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
        let hits = search_memories(&mut context, "mutex".to_owned())
            .await
            .expect("search");
        assert!(!hits.is_empty(), "own intel must be searchable");
    }
}
