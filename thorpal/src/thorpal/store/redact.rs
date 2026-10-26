//! Render-time redaction of the script library's model-visible content.
//!
//! The 0025 provenance redaction neutralized `promoted-from:` headers, but the
//! blinded audit found the library's *content* still leaks: some
//! sessions saw their own malware family named in prior-session library
//! entries (family-derived script names and descriptions rendered at message
//! 2 via `list_scripts`), plus content-level leaks inside script bodies. A
//! [`LibraryRedactor`] closes that surface at render time: under a blinded
//! (`--anonymize`) session every library script name renders as a neutral
//! content-derived id (`lib-<8 hex of sha256(name)>`, resolvable back through
//! [`LibraryRedactor::resolve_alias`]), and an operator-supplied deny-token
//! list additionally scrubs free text (descriptions, usage docs, bodies,
//! search hits, run output) wherever a listed token appears. Tokens replace
//! with stable content-derived placeholders (`fam-<4 hex of sha256(token)>`)
//! so distinct families stay distinguishable to the model and the evaluator
//! (who holds the token list) can recompute the mapping mechanically -- the
//! 0025 philosophy. The deny list is global across the whole corpus by
//! design: the session must never learn its own family from *which* tokens
//! redact. On-disk library files stay operator-side truth; nothing is
//! migrated or rewritten at rest, and the token list itself is never
//! model-visible or persisted into the session store.

use std::sync::Arc;

use rig::tool::ToolContext;

use super::library::ScriptLibrary;
use super::memories::{MemoryKind, MemoryLibrary, MemoryMeta};

/// Redacts library content at render time for blinded or deny-listed runs.
///
/// Attached to every run's tool context (like the [`ScriptLibrary`] itself)
/// when the session is anonymized, a deny-token list was supplied, or a
/// blind-family list was supplied; absent otherwise, so unblinded runs render
/// byte-identical library content. The same redactor covers both cross-run
/// stores: script library names render as `lib-<8 hex>` and memory names as
/// `mem-<8 hex>`, and the blind-family list additionally hides matching
/// `family-intel` memories entirely (see
/// [`LibraryRedactor::hides_family_intel`]).
pub struct LibraryRedactor {
    /// Whether this session is blinded: every library script name renders as
    /// a neutral id, not only deny-listed ones
    anonymized: bool,
    /// The normalized deny tokens paired with their stable placeholders,
    /// longest token first so an overlapping shorter token never splits a
    /// longer one's hit
    tokens: Vec<(String, String)>,
    /// The normalized blind-family tokens: a `family-intel` memory matching
    /// any of these (by name or content) is hidden entirely from every
    /// model-visible memory surface, because its content can reveal the
    /// family under analysis even after name/token redaction
    blind_families: Vec<String>,
}

impl LibraryRedactor {
    /// Builds a redactor when there is anything for one to do
    ///
    /// Returns `None` for a run that is neither anonymized, deny-listed, nor
    /// family-blinded, so every library surface stays byte-identical to a
    /// redactor-less run. The blind-family tokens also join the deny-token
    /// scrub set, so a blinded family's name redacts out of free text
    /// everywhere its intel is not already hidden outright.
    ///
    /// # Arguments
    ///
    /// * `anonymized` - Whether this session analyzes an anonymized staged copy
    /// * `tokens` - The operator-supplied deny tokens (one family name/alias each)
    /// * `blind_families` - The family under analysis's name and aliases, whose
    ///   `family-intel` memories are hidden entirely
    pub fn new(
        anonymized: bool,
        tokens: Vec<String>,
        blind_families: Vec<String>,
    ) -> Option<Arc<Self>> {
        // normalize the blind families: trim, lowercase, drop empties, dedup
        let mut blind_families: Vec<String> = blind_families
            .iter()
            .map(|token| token.trim().to_lowercase())
            .filter(|token| !token.is_empty())
            .flat_map(|token| token_variants(&token))
            .filter(|token| token.len() >= 3)
            .collect();
        blind_families.sort();
        blind_families.dedup();
        // the blind families scrub out of free text too, so fold them into
        // the deny tokens before normalizing the combined set
        let mut cleaned: Vec<String> = tokens
            .iter()
            .map(|token| token.trim().to_lowercase())
            .filter(|token| !token.is_empty())
            .chain(blind_families.iter().cloned())
            .collect();
        cleaned.sort();
        cleaned.dedup();
        // expand each canonical token into its spelling variants (slug vs
        // prose vs squashed vs dotted) so a deny list of slugs still scrubs
        // display forms -- a deny list may carry `lockbit_black` while model
        // prose writes "LockBit Black" and dataset leaves write "Lummac.v2";
        // every variant shares the canonical token's placeholder so one
        // family renders one stable id
        let mut tokens: Vec<(String, String)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for canonical in cleaned {
            let placeholder = token_placeholder(&canonical);
            for variant in token_variants(&canonical) {
                if variant.len() >= 3 && seen.insert(variant.clone()) {
                    tokens.push((variant, placeholder.clone()));
                }
            }
        }
        // longest first so "darkgatev2" wins over "darkgate" at the same spot
        tokens.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        // a run with nothing to redact runs without a redactor at all
        if !anonymized && tokens.is_empty() {
            return None;
        }
        Some(Arc::new(LibraryRedactor {
            anonymized,
            tokens,
            blind_families,
        }))
    }

    /// Whether this redactor hides any family's intel memories at all
    ///
    /// Callers use this to skip loading memory sources when no blind-family
    /// filtering can apply.
    pub fn has_blind_families(&self) -> bool {
        !self.blind_families.is_empty()
    }

    /// Whether a memory is hidden outright as the blinded family's intel
    ///
    /// Only `family-intel` memories hide -- technique/workflow/pitfall
    /// memories are generic by construction and stay visible (render-blinded
    /// like everything else). A family-intel memory hides when any
    /// blind-family token matches its name or source (tags, description, and
    /// body included) case-insensitively, because its content can reveal the
    /// family under analysis even after name neutralization and token
    /// scrubbing. A hidden memory must behave as nonexistent on every surface
    /// so no miss/hit difference becomes an existence oracle.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare memory name to test
    /// * `source` - The memory's full source, header included
    pub fn hides_family_intel(&self, name: &str, source: &str) -> bool {
        // only family intel ever hides; generic kinds stay visible
        if self.blind_families.is_empty()
            || MemoryMeta::parse(source).kind != MemoryKind::FamilyIntel
        {
            return false;
        }
        // match the tokens against the name and the whole source, lowercased
        let name = name.to_lowercase();
        let source = source.to_lowercase();
        self.blind_families
            .iter()
            .any(|token| name.contains(token) || source.contains(token))
    }

    /// Replaces every deny-token occurrence in a text with its placeholder
    ///
    /// Matching is ASCII-case-insensitive and byte-aligned, so multi-byte
    /// text around a hit survives untouched. Text with no hits passes
    /// through byte-exact.
    ///
    /// # Arguments
    ///
    /// * `text` - The model-visible library text to scrub
    pub fn redact_text(&self, text: &str) -> String {
        // apply each token longest-first over the accumulating result
        let mut out = text.to_owned();
        for (token, placeholder) in &self.tokens {
            out = replace_token(&out, token, placeholder);
        }
        out
    }

    /// Neutralizes the `//! name:` header line of a library script's source
    ///
    /// Every promoted script's doc header opens with `name: <real stem>`, so
    /// a blinded read would hand the raw (routinely family-derived) stem
    /// straight back even though the listing showed a neutral id. Mirrors
    /// `redact_provenance`'s shape: header-scoped, idempotent (an
    /// already-neutral value renders as itself), and byte-exact on bodies.
    ///
    /// # Arguments
    ///
    /// * `source` - The library script source to rewrite
    pub fn redact_header_name(&self, source: &str) -> String {
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
            // rewrite only the real name key line inside the header run; a
            // continuation line is indented past the single space after `//!`
            // and must pass through byte-exact (mirroring split_header_key)
            if in_header
                && let Some(rest) = text.strip_prefix("//!")
                && let Some(unindented) = rest.strip_prefix(' ')
                && !unindented.starts_with(' ')
                && let Some(value) = unindented.strip_prefix("name:")
            {
                out.push_str("//! name: ");
                out.push_str(&self.display_name(value.trim()));
                // keep whatever line ending the original line carried
                out.push_str(&line[text.len()..]);
                continue;
            }
            out.push_str(line);
        }
        out
    }

    /// Scrubs deny-listed tokens out of every string leaf of a JSON value
    ///
    /// # Arguments
    ///
    /// * `value` - The JSON value to scrub in place
    pub fn redact_json(&self, value: &mut serde_json::Value) {
        match value {
            // rewrite string leaves through the token scrubber
            serde_json::Value::String(text) => *text = self.redact_text(text),
            // recurse into containers; keys stay structural and untouched
            serde_json::Value::Array(items) => {
                for item in items {
                    self.redact_json(item);
                }
            }
            serde_json::Value::Object(map) => {
                for item in map.values_mut() {
                    self.redact_json(item);
                }
            }
            // numbers, bools, and nulls carry no text
            _ => (),
        }
    }

    /// Whether this redactor hides a library name behind a neutral id
    ///
    /// A hidden name must never resolve raw: the model only ever saw the
    /// neutral id, so a raw-name hit would be an existence/content oracle on
    /// the very names blinding withholds.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare library script name to test
    pub fn hides(&self, name: &str) -> bool {
        // a name renders differently exactly when it is hidden
        self.display_name(name) != name
    }

    /// Whether a script name contains any deny-listed token
    ///
    /// # Arguments
    ///
    /// * `name` - The bare library script name to test
    pub fn is_denied(&self, name: &str) -> bool {
        // a name is denied when any token matches it case-insensitively
        let lowered = name.to_lowercase();
        self.tokens.iter().any(|(token, _)| lowered.contains(token))
    }

    /// The name a library script renders as on every model-visible surface
    ///
    /// Blinded sessions neutralize every library name (abbreviation-derived
    /// names like `dg_*` leak without ever matching a token list); unblinded
    /// deny-listed runs neutralize only names carrying a listed token. An
    /// already-neutral id passes through, keeping rendering idempotent.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare library script name to render
    pub fn display_name(&self, name: &str) -> String {
        // an already-neutral id stays put so re-rendering is idempotent
        if is_neutral_script_id(name) {
            return name.to_owned();
        }
        // blinding neutralizes everything; a deny list only what it names
        if self.anonymized || self.is_denied(name) {
            neutral_script_id(name)
        } else {
            name.to_owned()
        }
    }

    /// Resolves a neutral id back to the real library script name
    ///
    /// Scans the library's stems for the one whose neutral id matches; only
    /// names this redactor would actually render neutral resolve, so a model
    /// guessing hashes of plainly-rendered names gets a miss. Two stems
    /// hashing to the same id is an error rather than a silent pick.
    ///
    /// # Arguments
    ///
    /// * `library` - The library whose stems to scan
    /// * `name` - The neutral id to resolve
    pub async fn resolve_alias(
        &self,
        library: &ScriptLibrary,
        name: &str,
    ) -> Result<Option<String>, anyhow::Error> {
        // only neutral-id-shaped names can be aliases at all
        if !is_neutral_script_id(name) {
            return Ok(None);
        }
        // scan the stems for the (unique) one rendering as this id
        let mut resolved = None;
        for entry in library.list().await? {
            // skip names this redactor renders plainly
            if !(self.anonymized || self.is_denied(&entry.name)) {
                continue;
            }
            if neutral_script_id(&entry.name) == name {
                // two stems hashing identically must surface, never pick one
                if resolved.is_some() {
                    anyhow::bail!(
                        "the neutral id '{name}' is ambiguous: two library \
                         scripts hash to it"
                    );
                }
                resolved = Some(entry.name);
            }
        }
        Ok(resolved)
    }

    /// Whether this redactor hides a memory name behind a neutral id
    ///
    /// Mirrors [`LibraryRedactor::hides`]: a hidden name must never resolve
    /// raw, since the model only ever saw the neutral id.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare memory name to test
    pub fn hides_memory(&self, name: &str) -> bool {
        // a name renders differently exactly when it is hidden
        self.display_memory_name(name) != name
    }

    /// The name a memory renders as on every model-visible surface
    ///
    /// Mirrors [`LibraryRedactor::display_name`] with the memory id scheme:
    /// blinded sessions neutralize every memory name, unblinded deny-listed
    /// runs neutralize only names carrying a listed token, and an
    /// already-neutral id passes through so rendering is idempotent.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare memory name to render
    pub fn display_memory_name(&self, name: &str) -> String {
        // an already-neutral id stays put so re-rendering is idempotent
        if is_neutral_memory_id(name) {
            return name.to_owned();
        }
        // blinding neutralizes everything; a deny list only what it names
        if self.anonymized || self.is_denied(name) {
            neutral_memory_id(name)
        } else {
            name.to_owned()
        }
    }

    /// Resolves a neutral memory id back to the real memory name
    ///
    /// Mirrors [`LibraryRedactor::resolve_alias`] over the memory library's
    /// listing (builtins included): only names this redactor would actually
    /// render neutral resolve, and two names hashing to the same id is an
    /// error rather than a silent pick.
    ///
    /// # Arguments
    ///
    /// * `memories` - The memory library whose names to scan
    /// * `name` - The neutral id to resolve
    pub async fn resolve_memory_alias(
        &self,
        memories: &MemoryLibrary,
        name: &str,
    ) -> Result<Option<String>, anyhow::Error> {
        // only neutral-id-shaped names can be aliases at all
        if !is_neutral_memory_id(name) {
            return Ok(None);
        }
        // scan the names for the (unique) one rendering as this id
        let mut resolved = None;
        for entry in memories.list().await? {
            // skip names this redactor renders plainly
            if !(self.anonymized || self.is_denied(&entry.name)) {
                continue;
            }
            if neutral_memory_id(&entry.name) == name {
                // two names hashing identically must surface, never pick one
                if resolved.is_some() {
                    anyhow::bail!(
                        "the neutral id '{name}' is ambiguous: two memories \
                         hash to it"
                    );
                }
                resolved = Some(entry.name);
            }
        }
        Ok(resolved)
    }

    /// Pulls the shared redactor out of a [`ToolContext`], if one is set
    ///
    /// A run without a redactor (every unblinded, deny-list-less run) is the
    /// normal case -- callers simply skip redaction.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn try_from_context(context: &ToolContext) -> Option<Arc<LibraryRedactor>> {
        // a missing redactor just means nothing was configured to redact
        context
            .require::<Arc<LibraryRedactor>>()
            .ok()
            .map(Arc::clone)
    }
}

/// Derives the neutral id a library script name renders as
///
/// `lib-` plus the first 8 hex of the name's sha256, mirroring the 0025
/// `promoted-from` scheme (`session-<8 hex>`): stable, content-derived, and
/// recomputable by an evaluator holding the real names.
///
/// # Arguments
///
/// * `name` - The raw library script name
pub(crate) fn neutral_script_id(name: &str) -> String {
    // hash the name so the id is stable and computable evaluator-side
    let digest = <sha2::Sha256 as sha2::Digest>::digest(name.as_bytes());
    let hex: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("lib-{hex}")
}

/// Whether a name already has the neutral-id shape
///
/// # Arguments
///
/// * `name` - The name to test
fn is_neutral_script_id(name: &str) -> bool {
    // `lib-` plus exactly 8 lowercase hex chars
    name.len() == 12
        && name.starts_with("lib-")
        && name[4..]
            .chars()
            .all(|ch| matches!(ch, '0'..='9' | 'a'..='f'))
}

/// Derives the neutral id a memory name renders as
///
/// `mem-` plus the first 8 hex of the name's sha256, mirroring the script
/// library's `lib-<8 hex>` scheme: stable, content-derived, and recomputable
/// by an evaluator holding the real names.
///
/// # Arguments
///
/// * `name` - The raw memory name
pub(crate) fn neutral_memory_id(name: &str) -> String {
    // hash the name so the id is stable and computable evaluator-side
    let digest = <sha2::Sha256 as sha2::Digest>::digest(name.as_bytes());
    let hex: String = digest[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("mem-{hex}")
}

/// Whether a name already has the neutral memory-id shape
///
/// # Arguments
///
/// * `name` - The name to test
fn is_neutral_memory_id(name: &str) -> bool {
    // `mem-` plus exactly 8 lowercase hex chars
    name.len() == 12
        && name.starts_with("mem-")
        && name[4..]
            .chars()
            .all(|ch| matches!(ch, '0'..='9' | 'a'..='f'))
}

/// The spelling variants one canonical deny token must also match
///
/// A corpus deny list carries slugs (`lockbit_black`), but model prose and
/// dataset leaves spell families as display names ("LockBit Black"),
/// squashed ("lockbitblack"), or dotted ("lummac.v2") -- matching is
/// case-insensitive already, so expanding separators covers the gap.
///
/// # Arguments
///
/// * `canonical` - The normalized (trimmed, lowercased) deny token
fn token_variants(canonical: &str) -> Vec<String> {
    let mut variants = vec![canonical.to_owned()];
    if canonical.contains('_') {
        variants.push(canonical.replace('_', " "));
        variants.push(canonical.replace('_', ""));
        variants.push(canonical.replace('_', "."));
    }
    if canonical.contains(' ') {
        variants.push(canonical.replace(' ', ""));
    }
    variants
}

/// Derives the stable placeholder a deny token redacts to
///
/// # Arguments
///
/// * `token` - The normalized (lowercased) deny token
fn token_placeholder(token: &str) -> String {
    // hash the token so distinct families stay distinguishable and the
    // evaluator can recompute the mapping from the list it already holds
    let digest = <sha2::Sha256 as sha2::Digest>::digest(token.as_bytes());
    let hex: String = digest[..2]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("fam-{hex}")
}

/// Replaces every ASCII-case-insensitive occurrence of one token
///
/// Byte-aligned by construction: a non-ASCII byte can never equal an ASCII
/// token byte under `eq_ignore_ascii_case`, and a non-ASCII token span only
/// matches its exact byte sequence, so every splice lands on char boundaries.
///
/// # Arguments
///
/// * `text` - The text to scan
/// * `token` - The normalized token to replace
/// * `placeholder` - The placeholder each occurrence is replaced with
fn replace_token(text: &str, token: &str, placeholder: &str) -> String {
    // walk the bytes, splicing the placeholder over each hit
    let bytes = text.as_bytes();
    let token_bytes = token.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if index + token_bytes.len() <= bytes.len()
            && bytes[index..index + token_bytes.len()].eq_ignore_ascii_case(token_bytes)
        {
            out.extend_from_slice(placeholder.as_bytes());
            index += token_bytes.len();
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    // the splices only ever replace whole matched spans, so this stays UTF-8
    String::from_utf8(out).expect("token splices preserve utf-8")
}

/// Parses an operator-supplied deny-token file
///
/// One token per line; blank lines and `#` comments are skipped and tokens
/// are trimmed and lowercased. Prefer tokens of at least 4 characters --
/// shorter ones over-redact ordinary prose.
///
/// # Arguments
///
/// * `contents` - The token file's text
pub fn parse_token_file(contents: &str) -> Vec<String> {
    // one trimmed, lowercased token per non-blank, non-comment line
    let mut tokens: Vec<String> = contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_lowercase)
        .collect();
    // drop duplicates so the redactor's longest-first ordering stays clean
    tokens.sort();
    tokens.dedup();
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slug deny token scrubs its prose, squashed, and dotted spellings
    #[test]
    fn deny_tokens_match_spelling_variants() {
        let redactor = LibraryRedactor::new(
            false,
            vec!["lockbit_black".to_owned(), "lummac_v2".to_owned()],
            Vec::new(),
        )
        .expect("tokens should build a redactor");
        // all separator spellings scrub to the same family placeholder
        let scrubbed = redactor.redact_text("LockBit Black and lockbitblack and lockbit_black");
        assert!(!scrubbed.to_lowercase().contains("lockbit"), "{scrubbed}");
        let ids: std::collections::HashSet<&str> = scrubbed
            .split_whitespace()
            .filter(|w| w.starts_with("fam-"))
            .collect();
        assert_eq!(ids.len(), 1, "one family renders one id: {scrubbed}");
        // the dotted dataset-leaf spelling scrubs too
        let scrubbed = redactor.redact_text("the Lummac.v2 loader");
        assert!(!scrubbed.to_lowercase().contains("lummac"), "{scrubbed}");
    }

    /// Tokens redact case-insensitively with stable, distinct placeholders
    #[test]
    fn tokens_redact_case_insensitively_with_stable_placeholders() {
        let redactor = LibraryRedactor::new(
            false,
            vec!["darkgate".to_owned(), "lockbit".to_owned()],
            Vec::new(),
        )
        .expect("a deny list builds a redactor");
        // every case spelling of a token redacts to the same placeholder
        let once = redactor.redact_text("DarkGate config; darkgate v2; DARKGATE");
        let placeholder = token_placeholder("darkgate");
        assert_eq!(
            once,
            format!("{placeholder} config; {placeholder} v2; {placeholder}")
        );
        // distinct tokens get distinct placeholders
        let both = redactor.redact_text("DarkGate drops LockBit");
        assert!(both.contains(&token_placeholder("darkgate")));
        assert!(both.contains(&token_placeholder("lockbit")));
        assert_ne!(token_placeholder("darkgate"), token_placeholder("lockbit"));
        // untouched text passes through byte-exact, multi-byte included
        assert_eq!(redactor.redact_text("Ghidra’s decoder"), "Ghidra’s decoder");
        // redaction is idempotent: placeholders carry no tokens
        assert_eq!(redactor.redact_text(&once), once);
    }

    /// Longer tokens win over their own prefixes at the same position
    #[test]
    fn longest_token_wins_overlaps() {
        let redactor = LibraryRedactor::new(
            false,
            vec!["dark".to_owned(), "darkgate".to_owned()],
            Vec::new(),
        )
        .expect("a deny list builds a redactor");
        // the full family name redacts as one unit, not as its prefix
        let out = redactor.redact_text("darkgate and dark");
        assert_eq!(
            out,
            format!(
                "{} and {}",
                token_placeholder("darkgate"),
                token_placeholder("dark")
            )
        );
    }

    /// Denied names render deterministic neutral ids; rendering is idempotent
    #[test]
    fn denied_names_render_deterministic_neutral_ids() {
        let redactor = LibraryRedactor::new(false, vec!["darkgate".to_owned()], Vec::new())
            .expect("a deny list builds a redactor");
        // a denied name renders as its stable neutral id
        let neutral = redactor.display_name("darkgate_extract_config");
        assert_eq!(neutral, neutral_script_id("darkgate_extract_config"));
        assert!(
            is_neutral_script_id(&neutral),
            "the id has the neutral shape"
        );
        // rendering the neutral id again is a no-op
        assert_eq!(redactor.display_name(&neutral), neutral);
        // a clean name renders plainly when the session is not blinded
        assert_eq!(
            redactor.display_name("extract_xor_config"),
            "extract_xor_config"
        );
    }

    /// A blinded session neutralizes every library name, deny-listed or not
    #[test]
    fn anonymized_sessions_neutralize_all_names() {
        let redactor = LibraryRedactor::new(true, Vec::new(), Vec::new())
            .expect("blinding alone builds a redactor");
        // even an abbreviation-derived name with no listed token neutralizes
        let neutral = redactor.display_name("dg_extract_config");
        assert_eq!(neutral, neutral_script_id("dg_extract_config"));
        // with no tokens, free text passes through byte-exact
        assert_eq!(redactor.redact_text("DarkGate config"), "DarkGate config");
    }

    /// Token files parse line-wise, skipping blanks and comments
    #[test]
    fn token_file_parsing_skips_blanks_and_comments() {
        let parsed = parse_token_file(
            "# families staged in this corpus\nDarkGate\n\n  lockbit  \n# alias\nLockBit\n",
        );
        assert_eq!(parsed, vec!["darkgate".to_owned(), "lockbit".to_owned()]);
    }

    /// JSON redaction scrubs string leaves everywhere, structure untouched
    #[test]
    fn json_redaction_scrubs_string_leaves() {
        let redactor = LibraryRedactor::new(false, vec!["darkgate".to_owned()], Vec::new())
            .expect("a deny list builds a redactor");
        // a detector-script-shaped value with tokens at several depths
        let mut value = serde_json::json!({
            "hits": [{
                "address": 4096,
                "content": "DarkGate key",
                "reasoning": "matches the darkgate loader"
            }],
            "questions": [{ "title": "DarkGate C2?", "body": "see above" }]
        });
        redactor.redact_json(&mut value);
        // every string leaf scrubbed; numbers and keys untouched
        let rendered = value.to_string();
        assert!(
            !rendered.to_lowercase().contains("darkgate"),
            "leak: {rendered}"
        );
        assert!(rendered.contains("4096"), "numbers survive");
        assert!(rendered.contains("reasoning"), "keys survive");
        assert!(rendered.contains("see above"), "clean strings survive");
    }

    /// A run with nothing to redact builds no redactor at all
    #[test]
    fn empty_token_list_builds_no_redactor() {
        // neither blinded nor deny-listed: no redactor
        assert!(LibraryRedactor::new(false, Vec::new(), Vec::new()).is_none());
        // whitespace-only tokens normalize away too
        assert!(LibraryRedactor::new(false, vec!["  ".to_owned()], Vec::new()).is_none());
        // a blind-family list alone is enough to build one
        assert!(LibraryRedactor::new(false, Vec::new(), vec!["darkgate".to_owned()]).is_some());
        // blinding alone is enough to build one
        assert!(LibraryRedactor::new(true, Vec::new(), Vec::new()).is_some());
    }

    /// Memory names render deterministic `mem-` ids; rendering is idempotent
    #[test]
    fn memory_names_render_deterministic_neutral_ids() {
        let redactor = LibraryRedactor::new(false, vec!["darkgate".to_owned()], Vec::new())
            .expect("a deny list builds a redactor");
        // a denied memory name renders as its stable neutral id
        let neutral = redactor.display_memory_name("darkgate-config-schema");
        assert_eq!(neutral, neutral_memory_id("darkgate-config-schema"));
        assert!(neutral.starts_with("mem-"), "the id has the memory shape");
        assert!(redactor.hides_memory("darkgate-config-schema"));
        // rendering the neutral id again is a no-op
        assert_eq!(redactor.display_memory_name(&neutral), neutral);
        // a clean name renders plainly when the session is not blinded
        assert_eq!(
            redactor.display_memory_name("stackstring-recovery"),
            "stackstring-recovery"
        );
        // a blinded session neutralizes every memory name
        let blinded = LibraryRedactor::new(true, Vec::new(), Vec::new()).expect("blinded");
        assert!(blinded.hides_memory("stackstring-recovery"));
    }

    /// Blind families hide matching family intel and nothing else
    #[test]
    fn blind_families_hide_matching_family_intel() {
        let redactor = LibraryRedactor::new(false, Vec::new(), vec!["DarkGate".to_owned()])
            .expect("a blind-family list builds a redactor");
        assert!(redactor.has_blind_families());
        // a family-intel memory naming the blinded family hides, by name...
        let by_name = "---\nkind: family-intel\ndescription: d\n---\nbody";
        assert!(redactor.hides_family_intel("darkgate-intel", by_name));
        // ...or by content (tags, description, or body)
        let by_body = "---\nkind: family-intel\ndescription: d\n---\nDarkGate uses act=*";
        assert!(redactor.hides_family_intel("loader-intel", by_body));
        // family intel about other families stays visible
        let other = "---\nkind: family-intel\ndescription: d\n---\nLockBit traits";
        assert!(!redactor.hides_family_intel("lockbit-intel", other));
        // generic kinds never hide, even when they mention the family
        let technique = "---\nkind: technique\ndescription: d\n---\nseen in darkgate too";
        assert!(!redactor.hides_family_intel("stackstrings", technique));
        // the blind token also joined the deny-token scrub set
        assert!(
            !redactor
                .redact_text("a DarkGate dropper")
                .contains("DarkGate")
        );
        // a redactor with no blind families hides nothing
        let plain = LibraryRedactor::new(true, Vec::new(), Vec::new()).expect("blinded");
        assert!(!plain.has_blind_families());
        assert!(!plain.hides_family_intel("darkgate-intel", by_name));
    }
}
