//! The persistent Rhai script library shared across analysis sessions.
//!
//! Scripts a run proves useful can be promoted out of the session's own
//! `scripts/` folder into this library (by default `~/.thorium/scripts/`), so
//! future runs can find and reuse them instead of rewriting them from scratch.
//! Every promoted script is a single self-contained `.rhai` file whose leading
//! `//!` doc-comment header carries its metadata (see [`ScriptMeta`]): a
//! description and usage docs future runs search to decide whether the script
//! applies, a `detector` flag marking scripts that run automatically at session
//! start like the native Rust detectors, and promotion provenance.
//!
//! Writes go through the same atomic `.tmp`-sibling-plus-rename helper the
//! session store uses, so two concurrent sessions promoting at once race only
//! at last-writer-wins granularity per script and can never tear a file.

use std::path::PathBuf;
use std::sync::Arc;

use rig::tool::{ToolContext, ToolExecutionError};
use serde::Serialize;

use super::{SCRIPT_EXT, validate_script_name, write_bytes_in};

/// The longest a promoted script's description may be, in bytes.
const MAX_DESCRIPTION_LEN: usize = 1024;

/// The longest a promoted script's usage docs may be, in bytes.
const MAX_USAGE_LEN: usize = 8 * 1024;

/// The header keys a library script's `//!` doc header may carry.
const HEADER_KEYS: [&str; 6] = [
    "name",
    "detector",
    "promoted-from",
    "sample",
    "description",
    "usage",
];

/// The metadata embedded in a library script's leading `//!` doc header.
///
/// Rendered by [`ScriptMeta::render`] when a script is promoted and parsed back
/// by [`ScriptMeta::parse`] when the library is listed or searched. Parsing is
/// lenient -- a hand-dropped script with no header (or a partial one) still
/// loads with defaults -- so the library never refuses to list a script it can
/// execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptMeta {
    /// What the script does and when a future run should use it
    pub description: String,
    /// The args contract and return shape (how to call the script)
    pub usage: String,
    /// Whether this script runs automatically as a detector at session start
    pub detector: bool,
    /// The name of the session this script was promoted from
    pub promoted_from: String,
    /// The date (YYYY-MM-DD) this script was last promoted
    pub promoted_at: String,
    /// The SHA-256 of the sample the promoting session analyzed, when known
    ///
    /// Empty for legacy promotions and sessions whose project name is not a
    /// bare digest. The prior-toolkit seeding lead matches on this identity,
    /// so a future run on the same bytes opens with this script surfaced.
    pub sample: String,
}

impl ScriptMeta {
    /// Builds the metadata for a script promoted right now
    ///
    /// # Arguments
    ///
    /// * `description` - What the script does and when it applies
    /// * `usage` - The args contract and return shape
    /// * `detector` - Whether this script runs automatically at session start
    /// * `promoted_from` - The name of the promoting session
    /// * `sample` - The promoting session's primary sample SHA-256, or empty
    pub fn promoted_now(
        description: impl Into<String>,
        usage: impl Into<String>,
        detector: bool,
        promoted_from: impl Into<String>,
        sample: impl Into<String>,
    ) -> Self {
        ScriptMeta {
            description: description.into(),
            usage: usage.into(),
            detector,
            promoted_from: promoted_from.into(),
            // stamp just the date; promotion provenance needs no finer grain
            promoted_at: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            sample: sample.into(),
        }
    }

    /// Validates this metadata before a promotion writes it
    ///
    /// The description and usage are what future runs search to decide whether
    /// the script applies, so an empty description defeats the library's whole
    /// purpose and is refused with an instructive error.
    pub fn validate(&self) -> Result<(), String> {
        // an undocumented script can never be found by a future run
        if self.description.trim().is_empty() {
            return Err(
                "a promoted script needs a non-empty description saying what it \
                 does and when a future run should use it"
                    .to_owned(),
            );
        }
        // cap the docs so the header stays a header rather than a novel
        if self.description.len() > MAX_DESCRIPTION_LEN {
            return Err(format!(
                "the description is {} bytes; keep it under {MAX_DESCRIPTION_LEN} \
                 (a paragraph saying what the script does and when it applies)",
                self.description.len()
            ));
        }
        if self.usage.len() > MAX_USAGE_LEN {
            return Err(format!(
                "the usage docs are {} bytes; keep them under {MAX_USAGE_LEN} \
                 (the args contract, the return shape, and one example)",
                self.usage.len()
            ));
        }
        Ok(())
    }

    /// Renders this metadata as the `//!` doc header for a named script
    ///
    /// Multi-line field values become indented `//!   ` continuation lines, so
    /// a value line can never be mistaken for a new header key on re-parse.
    ///
    /// # Arguments
    ///
    /// * `name` - The script name the header is rendered for
    pub fn render(&self, name: &str) -> String {
        // start with the single-line fields in a fixed, readable order
        let mut header = format!(
            "//! name: {name}\n//! detector: {}\n//! promoted-from: {} ({})\n",
            self.detector, self.promoted_from, self.promoted_at
        );
        // record the sample identity so same-bytes runs can find this script
        if !self.sample.is_empty() {
            header.push_str(&format!("//! sample: {}\n", self.sample));
        }
        // append the multi-line docs fields with continuation indenting
        render_field(&mut header, "description", &self.description);
        render_field(&mut header, "usage", &self.usage);
        header
    }

    /// Parses the metadata out of a script source's leading `//!` doc header
    ///
    /// Parsing is lenient: missing keys fall back to their defaults (an empty
    /// string, `detector: false`), unknown keys are ignored, and a source with
    /// no header at all yields an all-default metadata. The header ends at the
    /// first line that does not start with `//!`.
    ///
    /// # Arguments
    ///
    /// * `source` - The full script source to parse the header of
    pub fn parse(source: &str) -> Self {
        // start from the all-default metadata lenient parsing falls back to
        let mut meta = ScriptMeta {
            description: String::new(),
            usage: String::new(),
            detector: false,
            promoted_from: String::new(),
            promoted_at: String::new(),
            sample: String::new(),
        };
        // track which field continuation lines currently append to
        let mut current: Option<&str> = None;
        for line in source.lines() {
            // the header is strictly the leading run of `//!` lines
            let Some(rest) = line.strip_prefix("//!") else {
                break;
            };
            // a known `key: value` line starts a new field; anything else is a
            // continuation of the field above it
            if let Some((key, value)) = split_header_key(rest) {
                match key {
                    "detector" => meta.detector = value.trim() == "true",
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
                    "sample" => meta.sample = value.trim().to_ascii_lowercase(),
                    "description" => {
                        meta.description = value.trim().to_owned();
                        current = Some("description");
                        continue;
                    }
                    "usage" => {
                        meta.usage = value.trim().to_owned();
                        current = Some("usage");
                        continue;
                    }
                    // the name key restates the file name; nothing to record
                    _ => (),
                }
                // single-line fields never accept continuations
                current = None;
            } else if let Some(field) = current {
                // append the continuation line to the open multi-line field
                let target = match field {
                    "description" => &mut meta.description,
                    _ => &mut meta.usage,
                };
                if !target.is_empty() {
                    target.push('\n');
                }
                target.push_str(rest.trim());
            }
        }
        meta
    }
}

/// Appends one multi-line header field with `//!   ` continuation lines
///
/// # Arguments
///
/// * `header` - The header buffer to append to
/// * `key` - The header key to render
/// * `value` - The (possibly multi-line) field value
fn render_field(header: &mut String, key: &str, value: &str) {
    // the first line carries the key; the rest become indented continuations
    let mut lines = value.lines();
    header.push_str(&format!(
        "//! {key}: {}\n",
        lines.next().unwrap_or_default()
    ));
    for line in lines {
        header.push_str(&format!("//!   {line}\n"));
    }
}

/// Splits a header line's remainder into a known `(key, value)` pair
///
/// Only an unindented, known key counts: continuation lines are rendered with
/// extra indentation, so a value line containing a colon can never shadow a
/// real key.
///
/// # Arguments
///
/// * `rest` - The header line's text after the `//!` prefix
fn split_header_key(rest: &str) -> Option<(&str, &str)> {
    // continuation lines are indented past the single space after `//!`
    let unindented = rest.strip_prefix(' ')?;
    if unindented.starts_with(' ') {
        return None;
    }
    // match the leading `key:` against the known header keys only
    let (key, value) = unindented.split_once(':')?;
    HEADER_KEYS.contains(&key).then_some((key, value))
}

/// Redacts session-name provenance out of a library script's doc header
///
/// Rewrites the value of every `promoted-from:` line in the leading `//!`
/// header run to a neutral content-derived id (`session-<first 8 hex of
/// sha256(session name)>`), preserving the ` (date)` suffix. The on-disk file
/// is never touched: this runs at render time on every model-visible library
/// read, so already-deployed headers need no migration and an evaluator can
/// recompute the alias map from the session names it already holds. Session
/// names are routinely family-derived (the finish-time rename nudge asks for
/// a descriptive name), so a raw header would leak the promoting sample's
/// identity into every later blinded run — an audit found exactly
/// that in most carryover sessions. A value already in neutral form
/// passes through unchanged, so redaction is idempotent.
///
/// # Arguments
///
/// * `source` - The library script source to redact
pub fn redact_provenance(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    // only the leading `//!` run is a header; the body is copied verbatim
    let mut in_header = true;
    for line in source.split_inclusive('\n') {
        // split_inclusive keeps the line ending, so output re-joins byte-exact
        let text = line.strip_suffix('\n').unwrap_or(line);
        let text = text.strip_suffix('\r').unwrap_or(text);
        if in_header && !text.starts_with("//!") {
            in_header = false;
        }
        // rewrite only real provenance key lines inside the header run
        if in_header
            && let Some(rest) = text.strip_prefix("//!")
            && let Some(("promoted-from", value)) = split_header_key(rest)
        {
            // split the rendered `session (date)` form and neutralize the name
            let value = value.trim();
            let (session, date) = match value.rsplit_once(" (") {
                Some((session, date)) => (session, Some(date)),
                None => (value, None),
            };
            out.push_str("//! promoted-from: ");
            out.push_str(&neutral_session_id(session));
            if let Some(date) = date {
                out.push_str(" (");
                out.push_str(date);
            }
            // keep whatever line ending the original line carried
            out.push_str(&line[text.len()..]);
            continue;
        }
        out.push_str(line);
    }
    out
}

/// Derives the neutral id a session name renders as under redaction
///
/// Shared with the memory library's provenance redaction, so both cross-run
/// stores render the same session as the same neutral id.
///
/// # Arguments
///
/// * `session` - The raw session name from the provenance header
pub(crate) fn neutral_session_id(session: &str) -> String {
    // a value already in neutral form stays put, keeping redaction idempotent
    if session.len() == 16
        && session.starts_with("session-")
        && session[8..]
            .chars()
            .all(|ch| matches!(ch, '0'..='9' | 'a'..='f'))
    {
        return session.to_owned();
    }
    // hash the name so the id is stable and computable evaluator-side
    let digest = <sha2::Sha256 as sha2::Digest>::digest(session.as_bytes());
    let hex: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("session-{hex}")
}

/// Strips a leading `//!` doc header (and the blank lines after it) off a source
///
/// Used when re-promoting a script that already carries a header, so the fresh
/// header never stacks on top of a stale one.
///
/// # Arguments
///
/// * `source` - The script source to strip the header from
pub fn strip_header(source: &str) -> &str {
    // walk past the leading run of `//!` lines byte-accurately
    let mut rest = source;
    while let Some(line_end) = rest.find('\n') {
        if !rest.starts_with("//!") {
            break;
        }
        rest = &rest[line_end + 1..];
    }
    // a final header line without a trailing newline must also be consumed
    if rest.starts_with("//!") && !rest.contains('\n') {
        rest = "";
    }
    // drop the blank separator lines the header was rendered with
    rest.trim_start_matches(['\n', '\r'])
}

/// One library script's listing entry: its name plus the searchable summary.
#[derive(Debug, Clone, Serialize)]
pub struct LibraryEntry {
    /// The bare script name (run it with `run_script`)
    pub name: String,
    /// What the script does and when it applies, from its doc header
    pub description: String,
    /// Whether this script runs automatically as a detector at session start
    pub detector: bool,
}

/// The persistent, cross-session Rhai script library on disk.
///
/// Rooted at `~/.thorium/scripts` by default (see
/// [`ScriptLibrary::default_root`]) and shared as an `Arc` through every run's
/// tool context, exactly like the [`SessionStore`](super::SessionStore). Unlike
/// the session store it holds no mutable state: every method resolves paths
/// against the fixed root, and writes are atomic per script.
pub struct ScriptLibrary {
    /// The directory every library script lives in (e.g. `~/.thorium/scripts`).
    root: PathBuf,
}

impl ScriptLibrary {
    /// The default library root: `~/.thorium/scripts`
    pub fn default_root() -> Result<PathBuf, anyhow::Error> {
        // resolve the home dir so the library lands in the user's `.thorium`
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("could not resolve a home directory for ~/.thorium"))?;
        Ok(home.join(".thorium").join("scripts"))
    }

    /// Opens the library at a root, creating the directory if needed
    ///
    /// # Arguments
    ///
    /// * `root` - The directory the library's scripts live in
    #[tracing::instrument(name = "ScriptLibrary::open", skip_all, err(Debug))]
    pub async fn open(root: PathBuf) -> Result<Arc<Self>, anyhow::Error> {
        // make sure the library directory exists before anything resolves into it
        tokio::fs::create_dir_all(&root).await?;
        Ok(Arc::new(ScriptLibrary { root }))
    }

    /// Saves a script (its header already rendered in) into the library
    ///
    /// Silently overwrites any previous version; the overwrite policy (refusing
    /// a collision unless the caller opted in) lives with the `promote_script`
    /// tool, since a trusted caller like a resave must stay unconditional.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare script name (the `.rhai` extension is appended)
    /// * `source` - The full script source, doc header included
    #[tracing::instrument(name = "ScriptLibrary::save", skip_all, err(Debug))]
    pub async fn save(&self, name: &str, source: &str) -> Result<(), anyhow::Error> {
        // refuse names that could escape the library directory
        validate_script_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // write atomically so a crash can never leave a torn script behind
        write_bytes_in(
            &self.root,
            &format!("{name}.{SCRIPT_EXT}"),
            source.as_bytes(),
        )
        .await
    }

    /// Loads a library script's full source (doc header included)
    ///
    /// # Arguments
    ///
    /// * `name` - The bare script name (the `.rhai` extension is appended)
    #[tracing::instrument(name = "ScriptLibrary::load", skip_all, err(Debug))]
    pub async fn load(&self, name: &str) -> Result<String, anyhow::Error> {
        // refuse names that could escape the library directory
        validate_script_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // read the script's source from the library root
        tokio::fs::read_to_string(self.root.join(format!("{name}.{SCRIPT_EXT}")))
            .await
            .map_err(|err| anyhow::anyhow!("failed to read library script '{name}': {err}"))
    }

    /// Loads a library script's parsed doc-header metadata
    ///
    /// # Arguments
    ///
    /// * `name` - The bare script name (the `.rhai` extension is appended)
    #[tracing::instrument(name = "ScriptLibrary::load_meta", skip_all, err(Debug))]
    pub async fn load_meta(&self, name: &str) -> Result<ScriptMeta, anyhow::Error> {
        // the metadata is embedded in the source's doc header
        Ok(ScriptMeta::parse(&self.load(name).await?))
    }

    /// Whether a script with this name is already in the library
    ///
    /// # Arguments
    ///
    /// * `name` - The bare script name (the `.rhai` extension is appended)
    #[tracing::instrument(name = "ScriptLibrary::exists", skip_all, err(Debug))]
    pub async fn exists(&self, name: &str) -> Result<bool, anyhow::Error> {
        // refuse names that could escape the library directory
        validate_script_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // probe for the script file under the library root
        Ok(tokio::fs::try_exists(self.root.join(format!("{name}.{SCRIPT_EXT}"))).await?)
    }

    /// Lists every library script with its description and detector flag
    ///
    /// Entries are sorted by name so the listing is stable across calls. A
    /// script whose header fails to read is skipped with a warning rather than
    /// sinking the whole listing.
    #[tracing::instrument(name = "ScriptLibrary::list", skip_all, err(Debug))]
    pub async fn list(&self) -> Result<Vec<LibraryEntry>, anyhow::Error> {
        // walk the library root collecting every saved script
        let mut entries = tokio::fs::read_dir(&self.root).await?;
        let mut scripts = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            // only `.rhai` files are scripts; skip temp files and strays
            let path = entry.path();
            let Some(name) = path
                .extension()
                .and_then(|ext| ext.to_str())
                .filter(|ext| *ext == SCRIPT_EXT)
                .and_then(|_| path.file_stem())
                .and_then(|stem| stem.to_str())
            else {
                continue;
            };
            // parse the header for the searchable summary, skipping bad reads
            match tokio::fs::read_to_string(&path).await {
                Ok(source) => {
                    let meta = ScriptMeta::parse(&source);
                    scripts.push(LibraryEntry {
                        name: name.to_owned(),
                        description: meta.description,
                        detector: meta.detector,
                    });
                }
                Err(error) => {
                    // one unreadable script must not sink the whole listing
                    tracing::warn!(name, ?error, "skipping an unreadable library script");
                }
            }
        }
        // sort so the listing is stable across calls
        scripts.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(scripts)
    }

    /// Gets the `(name, source)` of every detector-flagged library script
    ///
    /// These are the scripts a fresh session runs automatically during the
    /// detector-seeding phase, alongside the native Rust detectors.
    #[tracing::instrument(name = "ScriptLibrary::detector_scripts", skip_all, err(Debug))]
    pub async fn detector_scripts(&self) -> Result<Vec<(String, String)>, anyhow::Error> {
        // walk the listing, loading the source of each detector-flagged script
        let mut detectors = Vec::new();
        for entry in self.list().await? {
            if entry.detector {
                detectors.push((entry.name.clone(), self.load(&entry.name).await?));
            }
        }
        Ok(detectors)
    }

    /// Pulls the shared script library out of a tool call's [`ToolContext`]
    ///
    /// A missing library is a wiring bug for the tools that require one, so it
    /// surfaces as an invalid-args tool error.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn from_context(
        context: &ToolContext,
    ) -> Result<Arc<ScriptLibrary>, ToolExecutionError> {
        // recover the shared library, mapping a missing slot to a tool error
        context
            .require::<Arc<ScriptLibrary>>()
            .map(Arc::clone)
            .map_err(|_| {
                ToolExecutionError::invalid_args(
                    "no persistent script library is configured for this run".to_owned(),
                )
            })
    }

    /// Pulls the shared script library out of a [`ToolContext`], if one is set
    ///
    /// Unlike [`ScriptLibrary::from_context`], a run without a library (e.g. a
    /// consumer whose `~/.thorium` could not be resolved) is not an error --
    /// callers simply skip the library-backed behavior.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn try_from_context(context: &ToolContext) -> Option<Arc<ScriptLibrary>> {
        // a missing library just means no cross-session reuse was configured
        context.require::<Arc<ScriptLibrary>>().ok().map(Arc::clone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a representative metadata value for round-trip tests
    fn meta() -> ScriptMeta {
        ScriptMeta {
            description: "Finds single-byte XOR decoder stubs.\nReturns candidates.".to_owned(),
            usage: "run with args = #{ max_size: 128 };\nreturns [#{ address, key }]".to_owned(),
            detector: true,
            promoted_from: "mwater-triage".to_owned(),
            promoted_at: "2026-10-09".to_owned(),
            sample: "ab".repeat(32),
        }
    }

    /// Diagnostic: scan the real operator library for a sample stem
    ///
    /// Ignored by default; run with
    /// `THORPAL_TOOLKIT_STEM=<16hex> cargo test -p thorpal --lib -- --ignored real_library_prior_toolkit_scan --nocapture`
    #[tokio::test]
    #[ignore]
    async fn real_library_prior_toolkit_scan() {
        // no stem means nothing to probe
        let Ok(stem) = std::env::var("THORPAL_TOOLKIT_STEM") else {
            return;
        };
        // open the operator's real library and run the seeding scan verbatim
        let root = ScriptLibrary::default_root().expect("library root");
        let library = ScriptLibrary::open(root).await.expect("library open");
        let entries = library.list().await.expect("library list");
        println!("entries: {}", entries.len());
        let mut hits = 0;
        for entry in &entries {
            let mut hit = entry.description.contains(&stem);
            if !hit && let Ok(source) = library.load(&entry.name).await {
                hit = source.contains(&stem);
            }
            if hit {
                hits += 1;
                println!("hit: {}", entry.name);
            }
        }
        println!("hits: {hits}");
    }

    /// A rendered header parses back to the exact same metadata.
    #[test]
    fn header_round_trips() {
        let meta = meta();
        // render the header over a body and parse the combined source back
        let source = format!("{}\nlet hits = [];\nhits", meta.render("find-xor-stubs"));
        assert_eq!(ScriptMeta::parse(&source), meta);
        // stripping the header leaves exactly the body
        assert_eq!(strip_header(&source), "let hits = [];\nhits");
    }

    /// A headerless (hand-dropped) script parses to lenient defaults.
    #[test]
    fn headerless_scripts_parse_to_defaults() {
        let meta = ScriptMeta::parse("let x = 1;\nx");
        assert!(meta.description.is_empty());
        assert!(meta.usage.is_empty());
        assert!(!meta.detector);
        // stripping a headerless source is a no-op
        assert_eq!(strip_header("let x = 1;\nx"), "let x = 1;\nx");
    }

    /// A value line containing a `key:` colon cannot shadow a real header key.
    #[test]
    fn continuation_lines_never_shadow_keys() {
        let mut meta = meta();
        // a usage line that looks exactly like the detector key
        meta.usage = "first line\ndetector: true is not a flag here".to_owned();
        meta.detector = false;
        let source = format!("{}\n1", meta.render("tricky"));
        let parsed = ScriptMeta::parse(&source);
        // the tricky line stays inside usage and the flag stays off
        assert!(!parsed.detector);
        assert!(parsed.usage.contains("detector: true is not a flag here"));
    }

    /// The validation rules refuse undocumented or oversized docs.
    #[test]
    fn validation_rules() {
        // a complete metadata validates
        assert!(meta().validate().is_ok());
        // an empty description is refused with an instructive error
        let mut empty = meta();
        empty.description = "  ".to_owned();
        assert!(empty.validate().unwrap_err().contains("description"));
        // oversized docs are refused with their caps
        let mut big = meta();
        big.description = "x".repeat(MAX_DESCRIPTION_LEN + 1);
        assert!(big.validate().is_err());
        let mut big = meta();
        big.usage = "x".repeat(MAX_USAGE_LEN + 1);
        assert!(big.validate().is_err());
    }

    /// Scripts save, load, list, and probe through the library root.
    #[tokio::test]
    async fn library_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = ScriptLibrary::open(dir.path().join("scripts"))
            .await
            .expect("library should open");
        // a fresh library starts empty
        assert!(library.list().await.expect("list").is_empty());
        assert!(!library.exists("decode").await.expect("probe"));
        // save a documented detector script and a plain one
        let detector = format!("{}\n40 + 2", meta().render("decode"));
        library
            .save("decode", &detector)
            .await
            .expect("save decode");
        library.save("scan", "1 + 1").await.expect("save scan");
        // the full source (header included) loads back verbatim
        assert_eq!(library.load("decode").await.expect("load"), detector);
        assert!(library.exists("decode").await.expect("probe"));
        // the metadata parses out of the stored header
        let loaded = library.load_meta("decode").await.expect("meta");
        assert_eq!(loaded, meta());
        // the listing carries each script's summary, sorted by name
        let listing = library.list().await.expect("list");
        assert_eq!(listing.len(), 2);
        assert_eq!(listing[0].name, "decode");
        assert!(listing[0].detector);
        assert!(listing[0].description.contains("XOR decoder"));
        assert_eq!(listing[1].name, "scan");
        assert!(!listing[1].detector);
        assert!(listing[1].description.is_empty());
        // only the flagged script comes back as a detector
        let detectors = library.detector_scripts().await.expect("detectors");
        assert_eq!(detectors.len(), 1);
        assert_eq!(detectors[0].0, "decode");
    }

    /// Script names that could escape the library are rejected everywhere.
    #[tokio::test]
    async fn bad_library_names_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = ScriptLibrary::open(dir.path().to_path_buf())
            .await
            .expect("library should open");
        // separators, traversal, extensions, and empty names are all rejected
        for name in ["../escape", "a/b", "a\\b", "a.rhai", "", &"x".repeat(65)] {
            assert!(library.save(name, "1").await.is_err(), "{name:?} saved");
            assert!(library.load(name).await.is_err(), "{name:?} loaded");
            assert!(library.exists(name).await.is_err(), "{name:?} probed");
        }
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    /// Provenance redacts to the pinned neutral id with the date preserved.
    #[test]
    fn provenance_redacts_to_the_neutral_id() {
        // render a real header and redact it
        let meta = ScriptMeta {
            description: "Finds things.".to_owned(),
            usage: "no args".to_owned(),
            detector: false,
            promoted_from: "mwater-triage".to_owned(),
            promoted_at: "2026-10-09".to_owned(),
            sample: String::new(),
        };
        let source = format!("{}\nlet x = 1;\nx", meta.render("finder"));
        let redacted = redact_provenance(&source);
        // the neutral id is the pinned first 8 hex of sha256("mwater-triage")
        assert!(
            redacted.contains("//! promoted-from: session-11a0d38f (2026-10-09)"),
            "unexpected redaction: {redacted}"
        );
        // the raw session name is gone and the body survives verbatim
        assert!(!redacted.contains("mwater-triage"));
        assert!(redacted.ends_with("let x = 1;\nx"));
        // the other header lines are untouched
        assert!(redacted.contains("//! description: Finds things."));
    }

    /// Redaction is idempotent: a neutral id passes through unchanged.
    #[test]
    fn redaction_is_idempotent() {
        let meta = ScriptMeta {
            description: "d".to_owned(),
            usage: "u".to_owned(),
            detector: false,
            promoted_from: "batch1-family_a".to_owned(),
            promoted_at: "2026-10-12".to_owned(),
            sample: String::new(),
        };
        let source = format!("{}\n1 + 1", meta.render("thing"));
        let once = redact_provenance(&source);
        assert!(once.contains("session-d47e86a7 (2026-10-12)"));
        // a second pass is byte-identical
        assert_eq!(redact_provenance(&once), once);
    }

    /// Headerless sources and body mentions pass through untouched.
    #[test]
    fn redaction_leaves_bodies_alone() {
        // no header at all means no change
        let body_only = "let x = 1;\nx";
        assert_eq!(redact_provenance(body_only), body_only);
        // a body line mentioning the key is not a header line
        let tricky = "//! name: x\nlet s = \"promoted-from: secret-session\";\ns";
        let redacted = redact_provenance(tricky);
        assert!(
            redacted.contains("secret-session"),
            "body text must survive"
        );
        // a continuation-indented header line never parses as the key
        let indented = "//! description: d\n//!   promoted-from: not-a-key\n1";
        assert_eq!(redact_provenance(indented), indented);
    }

    /// A dateless provenance value still redacts to a bare neutral id.
    #[test]
    fn dateless_provenance_redacts() {
        // hand-dropped scripts may carry a bare name with no date suffix
        let source = "//! promoted-from: SomeFamilySession\n1 + 1";
        let redacted = redact_provenance(source);
        assert!(
            redacted.starts_with("//! promoted-from: session-"),
            "unexpected redaction: {redacted}"
        );
        assert!(!redacted.contains("SomeFamilySession"));
    }
}
