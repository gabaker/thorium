//! The persistent cross-run memory library of generic analysis knowledge.
//!
//! Memories are durable lessons a run proved out and future runs should reuse:
//! techniques that worked, workflow rules, verified family intel, and pitfalls
//! to avoid. They are deliberately NOT a place for sample- or session-specific
//! facts (hashes, paths, decoded strings) -- those belong on the per-session
//! forum. Every memory is a single self-contained markdown file under the
//! library root (by default `~/.thorium/memories/`) whose leading `---`-fenced
//! front-matter header carries its metadata (see [`MemoryMeta`]): the kind and
//! tags future runs filter on, a description they search to decide whether the
//! memory applies, and promotion provenance.
//!
//! A small set of builtin memories ships compiled into the binary (see
//! [`BUILTIN_MEMORIES`]) and merges read-only into every listing; a user file
//! with the same name shadows its builtin, mirroring how session scripts
//! shadow library scripts.
//!
//! Writes go through the same atomic `.tmp`-sibling-plus-rename helper the
//! session store uses, so two concurrent sessions saving at once race only at
//! last-writer-wins granularity per memory and can never tear a file.

use std::path::PathBuf;
use std::sync::Arc;

use rig::tool::{ToolContext, ToolExecutionError};
use serde::Serialize;

use super::library::neutral_session_id;
use super::{validate_script_name, write_bytes_in};

/// The longest a memory's description may be, in bytes
///
/// Sized to fit the required "what it teaches and when it applies" paragraph:
/// a failure census found every natural over-cap description landed
/// between 1,025 and 1,464 bytes against the old 1,024 cap (burning turns),
/// so the cap now leaves headroom above observed natural usage while still
/// keeping the header a header rather than a novel.
const MAX_DESCRIPTION_LEN: usize = 2048;

/// The longest a memory's body may be, in bytes.
const MAX_BODY_LEN: usize = 16 * 1024;

/// The most tags a memory may carry
///
/// Models naturally file 9-13 tags on rich lessons (most observed
/// save_memory failures were tag-count rejections, all at 13 or fewer), so
/// the cap sits above natural usage; tags stay search hints, not prose.
const MAX_TAGS: usize = 16;

/// The longest a single tag may be, in bytes.
const MAX_TAG_LEN: usize = 48;

/// The header keys a memory's front-matter header may carry.
const HEADER_KEYS: [&str; 5] = ["name", "kind", "tags", "promoted-from", "description"];

/// The file extension every stored memory is saved with.
pub(crate) const MEMORY_EXT: &str = "md";

/// The line that opens and closes a memory's front-matter header.
const FENCE: &str = "---";

/// The provenance value builtin memories ship with, exempt from redaction.
const BUILTIN_PROVENANCE: &str = "builtin";

/// The shortest hex run rejected as a sample hash or raw IOC.
const MIN_HEX_RUN: usize = 32;

/// The shortest forbidden term worth matching, so one-letter sample stems
/// cannot reject every memory containing that letter.
const MIN_FORBIDDEN_TERM_LEN: usize = 4;

/// The `(name, source)` of every builtin memory compiled into the binary.
///
/// Builtins merge read-only into every listing and load like saved memories; a
/// user file with the same name shadows its builtin, so an operator can revise
/// one without rebuilding.
pub(crate) const BUILTIN_MEMORIES: &[(&str, &str)] = &[(
    "resolve_aarch64_adrp_add_pairs",
    include_str!("../../../memories/resolve_aarch64_adrp_add_pairs.md"),
)];

/// What class of reusable knowledge a memory records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryKind {
    /// A concrete analysis technique (how to decode, resolve, or extract something)
    Technique,
    /// A rule about how to run an analysis well (process, delegation, verification)
    Workflow,
    /// Verified knowledge about a malware family (traits, protocols, indicators)
    FamilyIntel,
    /// A mistake or dead end future runs should not repeat
    Pitfall,
}

impl MemoryKind {
    /// The kebab-case label this kind renders and parses as
    pub fn label(self) -> &'static str {
        match self {
            MemoryKind::Technique => "technique",
            MemoryKind::Workflow => "workflow",
            MemoryKind::FamilyIntel => "family-intel",
            MemoryKind::Pitfall => "pitfall",
        }
    }

    /// Parses a kind from its kebab-case label, case-insensitively
    ///
    /// # Arguments
    ///
    /// * `text` - The label text to parse
    pub fn parse(text: &str) -> Option<Self> {
        // normalize so hand-edited headers and model args both parse
        match text.trim().to_lowercase().as_str() {
            "technique" => Some(MemoryKind::Technique),
            "workflow" => Some(MemoryKind::Workflow),
            "family-intel" => Some(MemoryKind::FamilyIntel),
            "pitfall" => Some(MemoryKind::Pitfall),
            _ => None,
        }
    }
}

/// The metadata embedded in a memory's leading `---`-fenced front-matter header.
///
/// Rendered by [`MemoryMeta::render`] when a memory is saved and parsed back by
/// [`MemoryMeta::parse`] when the library is listed or searched. Parsing is
/// lenient -- a hand-dropped memory with no header (or a partial one) still
/// loads with defaults -- so the library never refuses to list a memory it can
/// read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemoryMeta {
    /// What class of reusable knowledge this memory records
    pub kind: MemoryKind,
    /// The lowercase search tags this memory is filed under
    pub tags: Vec<String>,
    /// What this memory teaches and when a future run should read it
    pub description: String,
    /// The name of the session this memory was promoted from
    pub promoted_from: String,
    /// The date (YYYY-MM-DD) this memory was last saved
    pub promoted_at: String,
}

impl MemoryMeta {
    /// Builds the metadata for a memory saved right now
    ///
    /// # Arguments
    ///
    /// * `kind` - What class of reusable knowledge the memory records
    /// * `tags` - The search tags to file the memory under
    /// * `description` - What the memory teaches and when it applies
    /// * `promoted_from` - The name of the saving session
    pub fn promoted_now(
        kind: MemoryKind,
        tags: Vec<String>,
        description: impl Into<String>,
        promoted_from: impl Into<String>,
    ) -> Self {
        // normalize the tags so filing and searching stay case-insensitive
        let tags = tags
            .into_iter()
            .map(|tag| tag.trim().to_lowercase())
            .filter(|tag| !tag.is_empty())
            .collect();
        // normalize the description line-wise: the rendered header trims each
        // continuation line on re-parse, so trimming up front keeps a rendered
        // header's parse byte-identical to the metadata it was rendered from
        let description = description
            .into()
            .lines()
            .map(str::trim)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        MemoryMeta {
            kind,
            tags,
            description,
            promoted_from: promoted_from.into(),
            // stamp just the date; save provenance needs no finer grain
            promoted_at: chrono::Utc::now().format("%Y-%m-%d").to_string(),
        }
    }

    /// Validates this metadata before a save writes it
    ///
    /// The description and tags are what future runs search to decide whether
    /// the memory applies, so an empty description defeats the library's whole
    /// purpose and is refused with an instructive error.
    pub fn validate(&self) -> Result<(), String> {
        // an undocumented memory can never be found by a future run
        if self.description.trim().is_empty() {
            return Err(
                "a memory needs a non-empty description saying what it teaches \
                 and when a future run should read it"
                    .to_owned(),
            );
        }
        // cap the description so the header stays a header rather than a novel
        if self.description.len() > MAX_DESCRIPTION_LEN {
            return Err(format!(
                "the description is {} bytes; keep it under {MAX_DESCRIPTION_LEN} \
                 (a paragraph saying what the memory teaches and when it applies)",
                self.description.len()
            ));
        }
        // tags are search hints, not prose; cap their count and size
        if self.tags.len() > MAX_TAGS {
            return Err(format!(
                "{} tags were given; keep at most {MAX_TAGS} short search tags",
                self.tags.len()
            ));
        }
        if let Some(tag) = self.tags.iter().find(|tag| tag.len() > MAX_TAG_LEN) {
            return Err(format!(
                "the tag '{tag}' is {} bytes; keep each tag under {MAX_TAG_LEN}",
                tag.len()
            ));
        }
        // a control character in a tag would splice extra lines into the
        // rendered single-line `tags:` field -- a newline there can close the
        // fence early or inject a fake header key, so it is refused outright
        if let Some(tag) = self
            .tags
            .iter()
            .find(|tag| tag.chars().any(char::is_control))
        {
            return Err(format!(
                "the tag '{}' contains a control character; tags must be short \
                 plain words (letters, digits, spaces, hyphens)",
                tag.escape_debug()
            ));
        }
        Ok(())
    }

    /// Renders this metadata as the front-matter header for a named memory
    ///
    /// The description renders last with two-space-indented continuation
    /// lines, so a value line can never be mistaken for a new header key on
    /// re-parse.
    ///
    /// # Arguments
    ///
    /// * `name` - The memory name the header is rendered for
    pub fn render(&self, name: &str) -> String {
        // open the fence and render the single-line fields in a fixed order
        let mut header = format!(
            "{FENCE}\nname: {name}\nkind: {}\ntags: {}\npromoted-from: {} ({})\n",
            self.kind.label(),
            self.tags.join(", "),
            self.promoted_from,
            self.promoted_at
        );
        // append the multi-line description with continuation indenting
        let mut lines = self.description.lines();
        header.push_str(&format!(
            "description: {}\n",
            lines.next().unwrap_or_default()
        ));
        for line in lines {
            header.push_str(&format!("  {line}\n"));
        }
        // close the fence; the body follows after a separating blank line
        header.push_str(FENCE);
        header.push('\n');
        header
    }

    /// Parses the metadata out of a memory source's front-matter header
    ///
    /// Parsing is lenient: missing keys fall back to their defaults (an empty
    /// string, `technique`, no tags), unknown keys are ignored, and a source
    /// with no header -- or an opening fence that never closes -- yields an
    /// all-default metadata. Only the leading `---`-fenced run is a header;
    /// later `---` lines are ordinary markdown.
    ///
    /// # Arguments
    ///
    /// * `source` - The full memory source to parse the header of
    pub fn parse(source: &str) -> Self {
        // start from the all-default metadata lenient parsing falls back to
        let defaults = MemoryMeta {
            kind: MemoryKind::Technique,
            tags: Vec::new(),
            description: String::new(),
            promoted_from: String::new(),
            promoted_at: String::new(),
        };
        // a source without a leading fence has no header at all
        let mut lines = source.lines();
        if lines.next().map(str::trim_end) != Some(FENCE) {
            return defaults;
        }
        let mut meta = defaults.clone();
        // track whether description continuation lines currently append
        let mut in_description = false;
        for line in lines {
            // the first closing fence ends the header; the rest is body
            if line.trim_end() == FENCE {
                return meta;
            }
            // a known `key: value` line starts a new field; an indented line
            // continues the open multi-line description
            if let Some((key, value)) = split_header_key(line) {
                match key {
                    "kind" => meta.kind = MemoryKind::parse(value).unwrap_or(MemoryKind::Technique),
                    "tags" => {
                        // split the rendered comma-separated form back apart
                        meta.tags = value
                            .split(',')
                            .map(|tag| tag.trim().to_lowercase())
                            .filter(|tag| !tag.is_empty())
                            .collect();
                    }
                    "promoted-from" => {
                        // split the rendered `session (date)` form back apart
                        let value = value.trim();
                        if let Some((session, date)) = value.rsplit_once(" (") {
                            meta.promoted_from = session.to_owned();
                            meta.promoted_at = date.trim_end_matches(')').to_owned();
                        } else {
                            meta.promoted_from = value.to_owned();
                        }
                    }
                    "description" => {
                        meta.description = value.trim().to_owned();
                        in_description = true;
                        continue;
                    }
                    // the name key restates the file name; nothing to record
                    _ => (),
                }
                // single-line fields never accept continuations
                in_description = false;
            } else if in_description && line.starts_with(' ') {
                // append the continuation line to the open description
                if !meta.description.is_empty() {
                    meta.description.push('\n');
                }
                meta.description.push_str(line.trim());
            } else {
                // anything else closes the open field but stays ignored
                in_description = false;
            }
        }
        // the opening fence never closed, so nothing here was a real header
        defaults
    }
}

/// Splits a header line into a known `(key, value)` pair
///
/// Only an unindented, known key counts: description continuation lines are
/// rendered with two-space indentation, so a value line containing a colon can
/// never shadow a real key.
///
/// # Arguments
///
/// * `line` - The header line to split
fn split_header_key(line: &str) -> Option<(&str, &str)> {
    // continuation lines are indented and never carry a key
    if line.starts_with(' ') {
        return None;
    }
    // match the leading `key:` against the known header keys only
    let (key, value) = line.split_once(':')?;
    HEADER_KEYS.contains(&key).then_some((key, value))
}

/// The byte offset the body starts at, when a complete fenced header leads
///
/// `None` means the source has no (closed) header: either the first line is
/// not a fence, or the opening fence never closes. Later `---` lines are
/// ordinary markdown and never re-enter header handling.
///
/// # Arguments
///
/// * `source` - The memory source to locate the header in
fn header_end(source: &str) -> Option<usize> {
    // walk the lines byte-accurately so the body offset is exact
    let mut offset = 0;
    let mut first = true;
    for line in source.split_inclusive('\n') {
        // trim_end matches MemoryMeta::parse's fence recognition exactly: a
        // hand-edited fence with trailing whitespace must either close the
        // header for BOTH (so redaction covers what parse read as a header)
        // or for neither -- a disagreement would let a parsed header's
        // provenance line escape the header-scoped rewriters
        let text = line.trim_end();
        if first {
            // a source without a leading fence has no header at all
            if text != FENCE {
                return None;
            }
            first = false;
        } else if text == FENCE {
            // the first closing fence ends the header run
            return Some(offset + line.len());
        }
        offset += line.len();
    }
    // the opening fence never closed, so nothing here was a real header
    None
}

/// Strips a leading front-matter header (and the blank lines after it) off a source
///
/// Used when re-saving a memory whose body already carries a header, so the
/// fresh header never stacks on top of a stale one.
///
/// # Arguments
///
/// * `source` - The memory source to strip the header from
pub fn strip_header(source: &str) -> &str {
    // only a complete fenced header strips; anything else is all body
    match header_end(source) {
        // drop the blank separator lines the header was rendered with
        Some(end) => source[end..].trim_start_matches(['\n', '\r']),
        None => source,
    }
}

/// Redacts session-name provenance out of a memory's front-matter header
///
/// Rewrites the value of every `promoted-from:` line in the leading fenced
/// header to a neutral content-derived id (`session-<first 8 hex of
/// sha256(session name)>`), preserving the ` (date)` suffix. The on-disk file
/// is never touched: this runs at render time on every model-visible memory
/// read, exactly like the script library's provenance redaction and for the
/// same reason -- session names are routinely family-derived, so a raw header
/// would leak the saving sample's identity into every later blinded run. The
/// `builtin` provenance ships with the binary, names no session, and passes
/// through unchanged; so does a value already in neutral form, keeping
/// redaction idempotent.
///
/// # Arguments
///
/// * `source` - The memory source to redact
pub fn redact_provenance(source: &str) -> String {
    // rewrite only the provenance key's value inside the fenced header
    rewrite_header_value(source, "promoted-from", |value| {
        // split the rendered `session (date)` form and neutralize the name
        let (session, date) = match value.rsplit_once(" (") {
            Some((session, date)) => (session, Some(date)),
            None => (value, None),
        };
        // builtins ship with the binary and name no session to leak
        let neutral = if session == BUILTIN_PROVENANCE {
            session.to_owned()
        } else {
            neutral_session_id(session)
        };
        match date {
            Some(date) => format!("{neutral} ({date}"),
            None => neutral,
        }
    })
}

/// Rewrites the `name:` header line of a memory's source
///
/// Used by blinded renders to swap the raw (possibly family-derived) name for
/// its neutral id, mirroring the script library's header-name redaction. The
/// body is copied byte-exact.
///
/// # Arguments
///
/// * `source` - The memory source to rewrite
/// * `rename` - The rewrite applied to the header's name value
pub(crate) fn rewrite_header_name(source: &str, rename: impl Fn(&str) -> String) -> String {
    // rewrite only the name key's value inside the fenced header
    rewrite_header_value(source, "name", rename)
}

/// Rewrites one known key's value inside a memory's fenced header
///
/// Only the leading `---`-fenced run is a header: the body (later `---` lines
/// included) is copied byte-exact, and a source with no complete header passes
/// through untouched. Line endings are preserved so output re-joins byte-exact.
///
/// # Arguments
///
/// * `source` - The memory source to rewrite
/// * `key` - The known header key whose value is rewritten
/// * `rewrite` - The rewrite applied to the trimmed value
fn rewrite_header_value(source: &str, key: &str, rewrite: impl Fn(&str) -> String) -> String {
    // a source without a complete header has nothing to rewrite
    let Some(end) = header_end(source) else {
        return source.to_owned();
    };
    let mut out = String::with_capacity(source.len());
    let mut offset = 0;
    // walk only the header run; the body is appended verbatim below
    for line in source[..end].split_inclusive('\n') {
        offset += line.len();
        // split_inclusive keeps the line ending, so output re-joins byte-exact
        let text = line.trim_end_matches(['\n', '\r']);
        // rewrite only the targeted real key line inside the header run
        if let Some((found, value)) = split_header_key(text)
            && found == key
        {
            out.push_str(key);
            out.push_str(": ");
            out.push_str(&rewrite(value.trim()));
            // keep whatever line ending the original line carried
            out.push_str(&line[text.len()..]);
            continue;
        }
        out.push_str(line);
    }
    // the body (later fences included) is never header material
    out.push_str(&source[offset..]);
    out
}

/// Validates a memory body against the size and generic-content rules
///
/// # Arguments
///
/// * `body` - The memory body to validate
pub(crate) fn validate_body(body: &str) -> Result<(), String> {
    // an empty memory teaches nothing and can never be worth recalling
    if body.trim().is_empty() {
        return Err("a memory needs a non-empty body holding the lesson itself".to_owned());
    }
    // cap the body so recalling a memory stays cheap in future runs
    if body.len() > MAX_BODY_LEN {
        return Err(format!(
            "the body is {} bytes; keep it under {MAX_BODY_LEN} -- distill the \
             lesson rather than archiving raw output",
            body.len()
        ));
    }
    Ok(())
}

/// Rejects sample-specific content in a memory's model-supplied text
///
/// Memories must stay GENERIC: a long hex run is a sample hash or raw IOC, and
/// the analyzed sample's name or the session's name ties the lesson to this
/// run. Both are refused with instructive errors pointing at the forum, which
/// is where sample-specific facts belong. The one exception is the
/// name-matching rule for `family-intel` memories: naming the family is their
/// whole point, and a sample or session is routinely named after the family
/// (the finish ladder's rename nudge asks for exactly that), so family intel
/// skips the forbidden-term check while the hex-run rule still applies.
///
/// # Arguments
///
/// * `text` - The combined model-supplied text (name, tags, description, body)
/// * `kind` - The kind being saved, exempting family intel from name matching
/// * `forbidden` - This run's sample stems and session name
pub(crate) fn validate_generic(
    text: &str,
    kind: MemoryKind,
    forbidden: &[String],
) -> Result<(), String> {
    // a 32+ char hex run is a hash or raw IOC, never generic knowledge
    if let Some(run) = find_hex_run(text, MIN_HEX_RUN) {
        return Err(format!(
            "memories must be GENERIC: '{run}' looks like a sample hash or raw \
             IOC -- sample-specific facts belong on the forum, not in cross-run \
             memory (describe the technique and use placeholders instead)"
        ));
    }
    // family intel must name its family even when the sample/session does too
    if kind == MemoryKind::FamilyIntel {
        return Ok(());
    }
    // the sample or session name ties the lesson to this run
    let lowered = text.to_lowercase();
    for term in forbidden {
        // skip terms too short to match meaningfully
        if term.len() >= MIN_FORBIDDEN_TERM_LEN && lowered.contains(&term.to_lowercase()) {
            return Err(format!(
                "memories must be GENERIC: '{term}' names this run's sample or \
                 session -- keep run-specific facts on the forum and describe \
                 the lesson so any future run can apply it (family facts belong \
                 in a kind: family-intel memory)"
            ));
        }
    }
    Ok(())
}

/// Finds the first hash-like run of hex characters at least `min` long
///
/// A qualifying run must contain at least one `a-f` letter: a pure-digit run
/// is a (rare but legitimate) long decimal constant, while a real 32+ char
/// hash without a single letter is vanishingly unlikely.
///
/// # Arguments
///
/// * `text` - The text to scan
/// * `min` - The shortest run that counts as a hit
fn find_hex_run(text: &str, min: usize) -> Option<String> {
    // walk the chars counting consecutive hex digits
    let mut start = None;
    let mut count = 0;
    let mut has_letter = false;
    for (index, ch) in text.char_indices() {
        if ch.is_ascii_hexdigit() {
            // extend (or open) the current run
            if start.is_none() {
                start = Some(index);
            }
            count += 1;
            has_letter |= ch.is_ascii_alphabetic();
        } else {
            // the run ended; report it when it was long and hash-like enough
            if count >= min
                && has_letter
                && let Some(begin) = start
            {
                return Some(text[begin..index].to_owned());
            }
            start = None;
            count = 0;
            has_letter = false;
        }
    }
    // a run can also end at the end of the text
    if count >= min
        && has_letter
        && let Some(begin) = start
    {
        return Some(text[begin..].to_owned());
    }
    None
}

/// One memory's listing entry: its name plus the searchable summary.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryEntry {
    /// The bare memory name (read it with `read_memory`)
    pub name: String,
    /// What class of reusable knowledge this memory records
    pub kind: MemoryKind,
    /// What the memory teaches and when it applies, from its header
    pub description: String,
}

/// The persistent, cross-run memory library on disk.
///
/// Rooted at `~/.thorium/memories` by default (see
/// [`MemoryLibrary::default_root`]) and shared as an `Arc` through every run's
/// tool context, exactly like the [`ScriptLibrary`](super::library::ScriptLibrary).
/// Every method resolves paths against the fixed root, writes are atomic per
/// memory, and the compiled-in builtins merge read-only into reads and
/// listings. The only mutable state is the set of names this run's own
/// `save_memory` calls wrote, kept so blind-family hiding never withholds the
/// run's own saves (a save that immediately "vanished" from reads would be an
/// oracle on the blind-family list).
pub struct MemoryLibrary {
    /// The directory every saved memory lives in (e.g. `~/.thorium/memories`).
    root: PathBuf,
    /// The names this run's own `save_memory` calls wrote, exempt from
    /// blind-family hiding because their content is the model's own knowledge.
    saved_this_run: std::sync::RwLock<std::collections::HashSet<String>>,
}

impl MemoryLibrary {
    /// The default library root: `~/.thorium/memories`
    pub fn default_root() -> Result<PathBuf, anyhow::Error> {
        // resolve the home dir so the library lands in the user's `.thorium`
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("could not resolve a home directory for ~/.thorium"))?;
        Ok(home.join(".thorium").join("memories"))
    }

    /// Opens the library at a root, creating the directory if needed
    ///
    /// # Arguments
    ///
    /// * `root` - The directory the library's memories live in
    #[tracing::instrument(name = "MemoryLibrary::open", skip_all, err(Debug))]
    pub async fn open(root: PathBuf) -> Result<Arc<Self>, anyhow::Error> {
        // make sure the library directory exists before anything resolves into it
        tokio::fs::create_dir_all(&root).await?;
        Ok(Arc::new(MemoryLibrary {
            root,
            saved_this_run: std::sync::RwLock::new(std::collections::HashSet::new()),
        }))
    }

    /// Records that this run's own `save_memory` wrote a memory
    ///
    /// Recorded by the tool layer (not by [`MemoryLibrary::save`], so operator
    /// and test seeding stay neutral), and consulted by the blind-family
    /// filter: the run's own saves are the model's own knowledge, so hiding
    /// them would only build a vanished-after-save oracle on the blind list.
    /// The set is per-session; a resumed run starts it empty.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare memory name that was saved
    pub(crate) fn note_saved(&self, name: &str) {
        // a poisoned lock only ever means a panicking writer; skip the note
        if let Ok(mut saved) = self.saved_this_run.write() {
            saved.insert(name.to_owned());
        }
    }

    /// Whether this run's own `save_memory` wrote a memory with this name
    ///
    /// # Arguments
    ///
    /// * `name` - The bare memory name to test
    pub(crate) fn saved_this_run(&self, name: &str) -> bool {
        // a poisoned lock degrades to "not ours", the withholding-safe answer
        self.saved_this_run
            .read()
            .map(|saved| saved.contains(name))
            .unwrap_or(false)
    }

    /// The builtin source for a name, when one ships compiled in
    ///
    /// # Arguments
    ///
    /// * `name` - The bare memory name to look up
    fn builtin(name: &str) -> Option<&'static str> {
        // scan the compiled-in set; it is small by construction
        BUILTIN_MEMORIES
            .iter()
            .find(|(builtin, _)| *builtin == name)
            .map(|(_, source)| *source)
    }

    /// Saves a memory (its header already rendered in) into the library
    ///
    /// Silently overwrites any previous version; the overwrite policy (refusing
    /// a collision unless the caller opted in) lives with the `save_memory`
    /// tool, since a trusted caller like a resave must stay unconditional. A
    /// save under a builtin's name shadows that builtin.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare memory name (the `.md` extension is appended)
    /// * `source` - The full memory source, front-matter header included
    #[tracing::instrument(name = "MemoryLibrary::save", skip_all, err(Debug))]
    pub async fn save(&self, name: &str, source: &str) -> Result<(), anyhow::Error> {
        // refuse names that could escape the library directory
        validate_script_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // write atomically so a crash can never leave a torn memory behind
        write_bytes_in(
            &self.root,
            &format!("{name}.{MEMORY_EXT}"),
            source.as_bytes(),
        )
        .await
    }

    /// Loads a memory's full source (front-matter header included)
    ///
    /// A saved file wins over a same-named builtin, so operators and runs can
    /// revise shipped memories without rebuilding.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare memory name (the `.md` extension is appended)
    #[tracing::instrument(name = "MemoryLibrary::load", skip_all, err(Debug))]
    pub async fn load(&self, name: &str) -> Result<String, anyhow::Error> {
        // refuse names that could escape the library directory
        validate_script_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // a saved file shadows a same-named builtin
        match tokio::fs::read_to_string(self.root.join(format!("{name}.{MEMORY_EXT}"))).await {
            Ok(source) => Ok(source),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // fall back to the compiled-in builtin when one exists
                match Self::builtin(name) {
                    Some(source) => Ok(source.to_owned()),
                    None => anyhow::bail!("no memory named '{name}' exists in the library"),
                }
            }
            Err(err) => anyhow::bail!("failed to read memory '{name}': {err}"),
        }
    }

    /// Whether a memory with this name exists (saved or builtin)
    ///
    /// # Arguments
    ///
    /// * `name` - The bare memory name (the `.md` extension is appended)
    #[tracing::instrument(name = "MemoryLibrary::exists", skip_all, err(Debug))]
    pub async fn exists(&self, name: &str) -> Result<bool, anyhow::Error> {
        // refuse names that could escape the library directory
        validate_script_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // a saved file or a compiled-in builtin both count
        Ok(
            tokio::fs::try_exists(self.root.join(format!("{name}.{MEMORY_EXT}"))).await?
                || Self::builtin(name).is_some(),
        )
    }

    /// Lists every memory (builtins included) with its kind and description
    ///
    /// Entries are sorted by name so the listing is stable across calls; a
    /// saved file shadows a same-named builtin. A memory whose file fails to
    /// read is skipped with a warning rather than sinking the whole listing.
    #[tracing::instrument(name = "MemoryLibrary::list", skip_all, err(Debug))]
    pub async fn list(&self) -> Result<Vec<MemoryEntry>, anyhow::Error> {
        // start from the compiled-in builtins; saved files shadow them below
        let mut by_name: std::collections::BTreeMap<String, MemoryEntry> = BUILTIN_MEMORIES
            .iter()
            .map(|(name, source)| {
                let meta = MemoryMeta::parse(source);
                (
                    (*name).to_owned(),
                    MemoryEntry {
                        name: (*name).to_owned(),
                        kind: meta.kind,
                        description: meta.description,
                    },
                )
            })
            .collect();
        // walk the library root collecting every saved memory
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            // only `.md` files are memories; skip temp files and strays
            let path = entry.path();
            let Some(name) = path
                .extension()
                .and_then(|ext| ext.to_str())
                .filter(|ext| *ext == MEMORY_EXT)
                .and_then(|_| path.file_stem())
                .and_then(|stem| stem.to_str())
            else {
                continue;
            };
            // parse the header for the searchable summary, skipping bad reads
            match tokio::fs::read_to_string(&path).await {
                Ok(source) => {
                    let meta = MemoryMeta::parse(&source);
                    by_name.insert(
                        name.to_owned(),
                        MemoryEntry {
                            name: name.to_owned(),
                            kind: meta.kind,
                            description: meta.description,
                        },
                    );
                }
                Err(error) => {
                    // one unreadable memory must not sink the whole listing
                    tracing::warn!(name, ?error, "skipping an unreadable memory");
                }
            }
        }
        // the map is keyed by name, so the listing is already sorted
        Ok(by_name.into_values().collect())
    }

    /// Pulls the shared memory library out of a tool call's [`ToolContext`]
    ///
    /// A missing library is a wiring bug for the tools that require one, so it
    /// surfaces as an invalid-args tool error.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn from_context(
        context: &ToolContext,
    ) -> Result<Arc<MemoryLibrary>, ToolExecutionError> {
        // recover the shared library, mapping a missing slot to a tool error
        context
            .require::<Arc<MemoryLibrary>>()
            .map(Arc::clone)
            .map_err(|_| {
                ToolExecutionError::invalid_args(
                    "no persistent memory library is configured for this run".to_owned(),
                )
            })
    }

    /// Pulls the shared memory library out of a [`ToolContext`], if one is set
    ///
    /// Unlike [`MemoryLibrary::from_context`], a run without a memory library
    /// (e.g. a consumer whose `~/.thorium` could not be resolved) is not an
    /// error -- callers simply skip the memory-backed behavior.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn try_from_context(context: &ToolContext) -> Option<Arc<MemoryLibrary>> {
        // a missing library just means no cross-run memory was configured
        context.require::<Arc<MemoryLibrary>>().ok().map(Arc::clone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a representative metadata value for round-trip tests
    fn meta() -> MemoryMeta {
        MemoryMeta {
            kind: MemoryKind::Pitfall,
            tags: vec!["attribution".to_owned(), "citation-hygiene".to_owned()],
            description: "Never promote recalled family lore to a conclusion.\nPost it as a hypothesis instead.".to_owned(),
            promoted_from: "mwater-triage".to_owned(),
            promoted_at: "2026-10-21".to_owned(),
        }
    }

    /// A rendered header parses back to the exact same metadata.
    #[test]
    fn header_round_trips() {
        let meta = meta();
        // render the header over a body and parse the combined source back
        let source = format!(
            "{}\nThe lesson body.\n",
            meta.render("attribution-discipline")
        );
        assert_eq!(MemoryMeta::parse(&source), meta);
        // stripping the header leaves exactly the body
        assert_eq!(strip_header(&source), "The lesson body.\n");
    }

    /// A headerless (hand-dropped) memory parses to lenient defaults.
    #[test]
    fn headerless_memories_parse_to_defaults() {
        let meta = MemoryMeta::parse("# Just a note\nSome text.");
        assert_eq!(meta.kind, MemoryKind::Technique);
        assert!(meta.tags.is_empty());
        assert!(meta.description.is_empty());
        // stripping a headerless source is a no-op
        assert_eq!(strip_header("# Just a note"), "# Just a note");
    }

    /// An opening fence that never closes is body, not a header.
    #[test]
    fn unclosed_fences_parse_to_defaults() {
        let source = "---\nkind: pitfall\nno closing fence here";
        assert_eq!(MemoryMeta::parse(source).kind, MemoryKind::Technique);
        // stripping leaves the whole source untouched too
        assert_eq!(strip_header(source), source);
        // and the rewriters pass it through byte-exact
        assert_eq!(redact_provenance(source), source);
    }

    /// Later `---` lines in the body never re-enter header handling.
    #[test]
    fn body_fences_stay_body() {
        let source = format!(
            "{}\nIntro.\n---\npromoted-from: fake-session\n---\n",
            meta().render("fences")
        );
        // the body's provenance-looking line survives redaction untouched
        let redacted = redact_provenance(&source);
        assert!(redacted.contains("promoted-from: fake-session"));
        // only the real header's provenance was neutralized
        assert!(redacted.contains("promoted-from: session-"));
        // the parse stops at the first closing fence
        assert_eq!(MemoryMeta::parse(&source), meta());
    }

    /// A value line containing a `key:` colon cannot shadow a real header key.
    #[test]
    fn continuation_lines_never_shadow_keys() {
        let mut meta = meta();
        // a description line that looks exactly like the kind key
        meta.description = "first line\nkind: workflow is not a kind here".to_owned();
        let source = format!("{}\nbody", meta.render("tricky"));
        let parsed = MemoryMeta::parse(&source);
        // the tricky line stays inside the description and the kind holds
        assert_eq!(parsed.kind, MemoryKind::Pitfall);
        assert!(
            parsed
                .description
                .contains("kind: workflow is not a kind here")
        );
    }

    /// Kinds parse case-insensitively and unknown kinds fall back lenient.
    #[test]
    fn kind_parsing_is_lenient() {
        assert_eq!(
            MemoryKind::parse(" Family-Intel "),
            Some(MemoryKind::FamilyIntel)
        );
        assert_eq!(MemoryKind::parse("WORKFLOW"), Some(MemoryKind::Workflow));
        assert_eq!(MemoryKind::parse("nonsense"), None);
        // an unknown kind in a header falls back to technique
        let source = "---\nkind: nonsense\n---\nbody";
        assert_eq!(MemoryMeta::parse(source).kind, MemoryKind::Technique);
    }

    /// The validation rules refuse undocumented or oversized metadata.
    #[test]
    fn validation_rules() {
        // a complete metadata validates
        assert!(meta().validate().is_ok());
        // an empty description is refused with an instructive error
        let mut empty = meta();
        empty.description = "  ".to_owned();
        assert!(empty.validate().unwrap_err().contains("description"));
        // oversized descriptions and tag sets are refused with their caps
        let mut big = meta();
        big.description = "x".repeat(MAX_DESCRIPTION_LEN + 1);
        assert!(big.validate().is_err());
        let mut tags = meta();
        tags.tags = (0..MAX_TAGS + 1).map(|i| format!("tag{i}")).collect();
        assert!(tags.validate().is_err());
        let mut long = meta();
        long.tags = vec!["x".repeat(MAX_TAG_LEN + 1)];
        assert!(long.validate().is_err());
        // a control character in a tag could splice header lines and is refused
        let mut sneaky = meta();
        sneaky.tags = vec!["x\n---".to_owned()];
        let err = sneaky.validate().unwrap_err();
        assert!(err.contains("control character"), "unexpected error: {err}");
    }

    /// The body rules refuse empty and oversized bodies.
    #[test]
    fn body_rules() {
        assert!(validate_body("a real lesson").is_ok());
        assert!(validate_body("   ").is_err());
        assert!(validate_body(&"x".repeat(MAX_BODY_LEN + 1)).is_err());
    }

    /// The generic-content guard rejects hashes and run-specific names.
    #[test]
    fn generic_content_guard() {
        // a 32+ char hex run reads as a hash and is refused for every kind
        let hash = format!("the sample 3f2a{} dropped it", "ab".repeat(16));
        let err = validate_generic(&hash, MemoryKind::Technique, &[]).unwrap_err();
        assert!(err.contains("hash"), "unexpected error: {err}");
        assert!(validate_generic(&hash, MemoryKind::FamilyIntel, &[]).is_err());
        // short hex runs (addresses, constants) stay fine
        assert!(
            validate_generic(
                "the marker 0xA00000 at 0x401000",
                MemoryKind::Technique,
                &[]
            )
            .is_ok()
        );
        // a long pure-decimal constant is not a hash and stays fine
        let decimal = format!("the constant {}", "9".repeat(40));
        assert!(validate_generic(&decimal, MemoryKind::Technique, &[]).is_ok());
        // the sample stem and session name are refused case-insensitively
        let forbidden = vec!["KeenTroll_dropper".to_owned(), "mwater-triage".to_owned()];
        assert!(
            validate_generic(
                "seen in keentroll_dropper",
                MemoryKind::Technique,
                &forbidden
            )
            .is_err()
        );
        assert!(
            validate_generic(
                "during mwater-triage we saw",
                MemoryKind::Workflow,
                &forbidden
            )
            .is_err()
        );
        assert!(
            validate_generic("a clean generic lesson", MemoryKind::Technique, &forbidden).is_ok()
        );
        // family intel must name its family even when the sample does too
        assert!(
            validate_generic(
                "keentroll_dropper traits",
                MemoryKind::FamilyIntel,
                &forbidden
            )
            .is_ok()
        );
        // terms too short to mean anything never match
        assert!(
            validate_generic(
                "a body with the letter a",
                MemoryKind::Technique,
                &["a".to_owned()]
            )
            .is_ok()
        );
    }

    /// Fence recognition agrees between parse and the header rewriters.
    #[test]
    fn fence_whitespace_agrees_across_parse_and_rewriters() {
        // a hand-edited closing fence with trailing whitespace still closes
        // the header for parse AND the rewriters, so a parsed header can
        // never escape header-scoped redaction
        let source = "---\nkind: family-intel\npromoted-from: darkgate-triage (2026-01-01)\n---  \nThe body.\n";
        assert_eq!(MemoryMeta::parse(source).kind, MemoryKind::FamilyIntel);
        assert_eq!(strip_header(source), "The body.\n");
        let redacted = redact_provenance(source);
        assert!(
            !redacted.contains("darkgate-triage"),
            "provenance escaped redaction: {redacted}"
        );
    }

    /// Provenance redacts to the pinned neutral id with the date preserved.
    #[test]
    fn provenance_redacts_to_the_neutral_id() {
        let source = format!("{}\nbody\n", meta().render("finder"));
        let redacted = redact_provenance(&source);
        // the neutral id is the pinned first 8 hex of sha256("mwater-triage")
        assert!(
            redacted.contains("promoted-from: session-11a0d38f (2026-10-21)"),
            "unexpected redaction: {redacted}"
        );
        // the raw session name is gone and the body survives verbatim
        assert!(!redacted.contains("mwater-triage"));
        assert!(redacted.ends_with("body\n"));
        // a second pass is byte-identical, so redaction is idempotent
        assert_eq!(redact_provenance(&redacted), redacted);
    }

    /// Builtin provenance passes through redaction unchanged.
    #[test]
    fn builtin_provenance_is_exempt() {
        let source = "---\nname: x\npromoted-from: builtin (2026-10-21)\n---\nbody";
        let redacted = redact_provenance(source);
        assert!(redacted.contains("promoted-from: builtin (2026-10-21)"));
    }

    /// The header name line rewrites while the body stays byte-exact.
    #[test]
    fn header_name_rewrites() {
        let source = format!(
            "{}\nname: not-a-key in the body\n",
            meta().render("real-name")
        );
        let rewritten = rewrite_header_name(&source, |name| format!("mem-{name}"));
        // the header's name was rewritten; the body line survived verbatim
        assert!(rewritten.contains("name: mem-real-name"));
        assert!(rewritten.contains("name: not-a-key in the body"));
    }

    /// Memories save, load, list, and probe through the library root.
    #[tokio::test]
    async fn library_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = MemoryLibrary::open(dir.path().join("memories"))
            .await
            .expect("library should open");
        // a fresh library still lists (and loads) the compiled-in builtins
        let listing = library.list().await.expect("list");
        assert_eq!(listing.len(), BUILTIN_MEMORIES.len());
        assert!(
            library
                .exists("resolve_aarch64_adrp_add_pairs")
                .await
                .expect("probe")
        );
        assert!(
            library
                .load("resolve_aarch64_adrp_add_pairs")
                .await
                .expect("load builtin")
                .contains("ADRP")
        );
        // save a documented memory and a bare one
        let saved = format!(
            "{}\nThe lesson body.\n",
            meta().render("attribution-discipline")
        );
        library
            .save("attribution-discipline", &saved)
            .await
            .expect("save");
        library
            .save("bare-note", "just text")
            .await
            .expect("save bare");
        // the full source (header included) loads back verbatim
        assert_eq!(
            library.load("attribution-discipline").await.expect("load"),
            saved
        );
        // the listing carries each memory's summary, sorted by name
        let listing = library.list().await.expect("list");
        assert_eq!(listing.len(), BUILTIN_MEMORIES.len() + 2);
        let entry = listing
            .iter()
            .find(|entry| entry.name == "attribution-discipline")
            .expect("saved memory listed");
        assert_eq!(entry.kind, MemoryKind::Pitfall);
        assert!(entry.description.contains("family lore"));
        // names come back sorted
        let names: Vec<&str> = listing.iter().map(|entry| entry.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        // a miss errors with the memory's name
        assert!(library.load("no-such-memory").await.is_err());
    }

    /// A saved file shadows a same-named builtin memory.
    #[tokio::test]
    async fn saved_memories_shadow_builtins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = MemoryLibrary::open(dir.path().to_path_buf())
            .await
            .expect("library should open");
        // shadow the builtin with a revised copy
        let revised = format!(
            "{}\nRevised guide body.\n",
            MemoryMeta::promoted_now(
                MemoryKind::Technique,
                vec!["aarch64".to_owned()],
                "A revised ADRP guide.",
                "some-session",
            )
            .render("resolve_aarch64_adrp_add_pairs")
        );
        library
            .save("resolve_aarch64_adrp_add_pairs", &revised)
            .await
            .expect("save shadow");
        // the saved copy wins on load and in the listing (no duplicate entry)
        assert_eq!(
            library
                .load("resolve_aarch64_adrp_add_pairs")
                .await
                .expect("load"),
            revised
        );
        let listing = library.list().await.expect("list");
        assert_eq!(listing.len(), BUILTIN_MEMORIES.len());
        assert!(listing[0].description.contains("revised ADRP"));
    }

    /// Memory names that could escape the library are rejected everywhere.
    #[tokio::test]
    async fn bad_memory_names_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = MemoryLibrary::open(dir.path().to_path_buf())
            .await
            .expect("library should open");
        // separators, traversal, extensions, and empty names are all rejected
        for name in ["../escape", "a/b", "a\\b", "a.md", "", &"x".repeat(65)] {
            assert!(library.save(name, "1").await.is_err(), "{name:?} saved");
            assert!(library.load(name).await.is_err(), "{name:?} loaded");
            assert!(library.exists(name).await.is_err(), "{name:?} probed");
        }
    }

    /// Every compiled-in builtin has a valid name and a parseable header.
    #[test]
    fn builtins_validate_and_parse() {
        for (name, source) in BUILTIN_MEMORIES {
            // builtin names must pass the same rules as saved names
            assert!(
                crate::thorpal::store::validate_script_name(name).is_ok(),
                "builtin name '{name}' is invalid"
            );
            // the header must parse with a real description for the index
            let meta = MemoryMeta::parse(source);
            assert!(
                !meta.description.is_empty(),
                "builtin '{name}' lacks a description"
            );
            assert_eq!(
                meta.promoted_from, BUILTIN_PROVENANCE,
                "builtin '{name}' provenance"
            );
            // the body must survive a strip (the header is complete)
            assert!(
                !strip_header(source).is_empty(),
                "builtin '{name}' has no body"
            );
        }
    }
}
