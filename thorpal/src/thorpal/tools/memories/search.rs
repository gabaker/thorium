//! Deterministic lexical ranking shared by `search_memories` and the
//! spawn-prompt memory index.
//!
//! BM25 (through the forum's identifier-expanding [`ForumTokenizer`]) unioned
//! with exact-substring matching: BM25 alone loses the grep use-case -- it
//! cannot match inside a token (`4bd` against `0x4BD`) and punctuation-laden
//! literals (`MC1=V=3`) tokenize lossily -- while substring alone has no
//! notion of relevance. Documents are built from RENDERED (redacted) sources,
//! so blinded models can search the very placeholders they see and a hit line
//! can never carry what redaction withheld. Ordering is fully deterministic:
//! exact matches first, then score, then recency, then name -- the corpus
//! arrives in the library's sorted listing order, so repeated calls over an
//! unchanged library rank identically.

use bm25::{Document, SearchEngineBuilder};
use serde::Serialize;

use crate::thorpal::store::memories::MemoryKind;
use crate::thorpal::tools::forum::ForumTokenizer;

/// The BM25 term-frequency saturation parameter (mirrors the forum's)
const BM25_K1: f32 = 1.2;

/// The BM25 document-length normalization parameter (mirrors the forum's)
const BM25_B: f32 = 0.75;

/// The default number of ranked memories a search returns
pub(crate) const DEFAULT_SEARCH_LIMIT: usize = 8;

/// The most ranked memories one search call may return
pub(crate) const MAX_SEARCH_LIMIT: usize = 25;

/// The most matched lines returned per ranked memory
pub(crate) const SNIPPET_LINES: usize = 3;

/// The length (in chars) each matched line is clipped to
pub(crate) const SNIPPET_LINE_CHARS: usize = 160;

/// The byte budget one search result may spend across its hits
pub(crate) const MAX_SEARCH_RESULT_BYTES: usize = 32 * 1024;

/// One scorable memory document, built from model-VISIBLE (rendered) text
pub(crate) struct MemoryDoc {
    /// The real memory name, kept only as the deterministic final tie-break
    /// and resolution key; never emitted to the model
    pub name: String,
    /// The name the model sees and queries by
    pub display: String,
    /// What class of reusable knowledge the memory records
    pub kind: MemoryKind,
    /// The date the memory was last saved, for the recency tie-break
    pub promoted_at: String,
    /// The scorable text: display name, tags, and description (doubled as a
    /// cheap field boost), then the body when the surface indexes bodies
    pub text: String,
}

/// A ranked document index with its deterministic ordering keys
pub(crate) struct RankedDoc {
    /// The index of the matched document in the scored slice
    pub index: usize,
    /// The BM25 relevance score (0.0 for substring-only matches)
    pub score: f32,
    /// Whether the whole query occurs verbatim in the document
    pub exact: bool,
}

/// Scores documents against a query: BM25 unioned with exact substring
///
/// Candidates are every document with a positive BM25 score plus every
/// document containing the whole lowercased query verbatim (the grep
/// use-case). Ordering is deterministic: exact matches first, then score,
/// then save date (newest first), then name.
///
/// # Arguments
///
/// * `docs` - The documents to score, in the library's sorted listing order
/// * `query` - The query to rank against
pub(crate) fn rank_memory_docs(docs: &[MemoryDoc], query: &str) -> Vec<RankedDoc> {
    // an empty query or an empty corpus can never match anything
    if query.trim().is_empty() || docs.is_empty() {
        return Vec::new();
    }
    // fit a BM25 engine over the documents with the forum's parameters; the
    // corpus order is the sorted listing, so scoring is deterministic
    let documents: Vec<Document<usize>> = docs
        .iter()
        .enumerate()
        .map(|(index, doc)| Document::new(index, doc.text.clone()))
        .collect();
    let engine = SearchEngineBuilder::<usize, u32, ForumTokenizer>::with_tokenizer_and_documents(
        ForumTokenizer::new(),
        documents,
    )
    .k1(BM25_K1)
    .b(BM25_B)
    .build();
    // collect every positive BM25 score keyed by document index
    let mut scores: std::collections::HashMap<usize, f32> = engine
        .search(query, None)
        .into_iter()
        .map(|result| (result.document.id, result.score))
        .collect();
    // union in exact-substring matches, which BM25 tokenization can miss
    let needle = query.trim().to_lowercase();
    let mut ranked: Vec<RankedDoc> = Vec::new();
    for (index, doc) in docs.iter().enumerate() {
        let exact = doc.text.to_lowercase().contains(&needle);
        let score = scores.remove(&index).unwrap_or(0.0);
        // a document matches when either route found it
        if exact || score > 0.0 {
            ranked.push(RankedDoc {
                index,
                score,
                exact,
            });
        }
    }
    // order deterministically: exact first, then score, recency, name
    ranked.sort_by(|a, b| {
        b.exact
            .cmp(&a.exact)
            .then(b.score.total_cmp(&a.score))
            .then(docs[b.index].promoted_at.cmp(&docs[a.index].promoted_at))
            .then(docs[a.index].name.cmp(&docs[b.index].name))
    });
    ranked
}

/// A matched line of a rendered memory source
#[derive(Debug, Clone, Serialize)]
pub struct MatchedLine {
    /// The 1-indexed line number of the match in the rendered source
    pub line: usize,
    /// The matched line's text, clipped to a readable length
    pub text: String,
}

/// Picks up to [`SNIPPET_LINES`] matched lines out of a rendered source
///
/// Whole-query (needle) hits come first -- they are what a literal grep was
/// after -- then lines matching any derived term fill the remaining slots.
/// A document can match through stemming or identifier expansion without any
/// line containing a term verbatim, so an empty pick falls back to the
/// description line (or the first non-empty line) rather than rendering a
/// bare hit. Lines clip char-safely to [`SNIPPET_LINE_CHARS`].
///
/// # Arguments
///
/// * `source` - The rendered memory source the hit matched
/// * `needle` - The whole lowercased query
/// * `terms` - The lowercased query terms (see `search_terms`)
pub(crate) fn matched_lines(source: &str, needle: &str, terms: &[String]) -> Vec<MatchedLine> {
    // collect needle hits eagerly and term hits as fallback fillers
    let mut lines: Vec<MatchedLine> = Vec::new();
    let mut term_hits: Vec<MatchedLine> = Vec::new();
    for (index, text) in source.lines().enumerate() {
        let lowered = text.to_lowercase();
        if !needle.is_empty() && lowered.contains(needle) {
            lines.push(clip_line(index + 1, text));
            // the needle hits alone can fill the budget
            if lines.len() >= SNIPPET_LINES {
                break;
            }
        } else if term_hits.len() < SNIPPET_LINES && terms.iter().any(|term| lowered.contains(term))
        {
            term_hits.push(clip_line(index + 1, text));
        }
    }
    // fill any remaining slots with term-matched lines
    for hit in term_hits {
        if lines.len() >= SNIPPET_LINES {
            break;
        }
        lines.push(hit);
    }
    // a stem- or subword-only match leaves no literal line: fall back to the
    // description line, else the first non-empty line, so a hit is never bare
    if lines.is_empty() {
        let fallback = source
            .lines()
            .enumerate()
            .find(|(_, text)| text.starts_with("description:"))
            .or_else(|| {
                source
                    .lines()
                    .enumerate()
                    .find(|(_, text)| !text.trim().is_empty())
            });
        if let Some((index, text)) = fallback {
            lines.push(clip_line(index + 1, text));
        }
    }
    lines
}

/// Clips a text to a char budget with a trailing ellipsis
///
/// # Arguments
///
/// * `text` - The text to clip
/// * `max` - The most chars to keep
pub(crate) fn clip_chars(text: &str, max: usize) -> String {
    // walk chars so a multi-byte boundary can never split
    let mut chars = text.chars();
    let clipped: String = chars.by_ref().take(max).collect();
    // mark the cut only when something was actually dropped
    if chars.next().is_some() {
        format!("{clipped}…")
    } else {
        clipped
    }
}

/// Builds one clipped matched line
///
/// # Arguments
///
/// * `line` - The 1-indexed line number of the match
/// * `text` - The matched line's full text
fn clip_line(line: usize, text: &str) -> MatchedLine {
    MatchedLine {
        line,
        text: clip_chars(text, SNIPPET_LINE_CHARS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal document for ranking tests
    ///
    /// # Arguments
    ///
    /// * `name` - The document's (and display) name
    /// * `promoted_at` - The save date the recency tie-break orders on
    /// * `text` - The scorable text
    fn doc(name: &str, promoted_at: &str, text: &str) -> MemoryDoc {
        MemoryDoc {
            name: name.to_owned(),
            display: name.to_owned(),
            kind: MemoryKind::Technique,
            promoted_at: promoted_at.to_owned(),
            text: text.to_owned(),
        }
    }

    /// Exact substring matches rank above score-only matches
    #[test]
    fn exact_matches_outrank_scored_matches() {
        let docs = vec![
            doc(
                "scored",
                "2026-10-27",
                "xor decode loops and xor key recovery",
            ),
            doc("exact", "2026-10-01", "the 0x4BD seed constant"),
        ];
        // the literal query only substring-matches the second document
        let ranked = rank_memory_docs(&docs, "0x4BD");
        assert_eq!(docs[ranked[0].index].name, "exact");
        assert!(ranked[0].exact);
        // a sub-token query still finds it through the substring arm
        let ranked = rank_memory_docs(&docs, "4bd");
        assert!(ranked.iter().any(|hit| docs[hit.index].name == "exact"));
    }

    /// Ranking is deterministic and ties break on recency then name
    #[test]
    fn ties_break_on_recency_then_name() {
        let docs = vec![
            doc("older", "2026-10-01", "identical text"),
            doc("newer", "2026-10-27", "identical text"),
            doc("dated", "2026-10-27", "identical text"),
        ];
        let ranked = rank_memory_docs(&docs, "identical text");
        let names: Vec<&str> = ranked
            .iter()
            .map(|hit| docs[hit.index].name.as_str())
            .collect();
        // same exactness and score: newest first, then name ascending
        assert_eq!(names, vec!["dated", "newer", "older"]);
        // an empty query matches nothing at all
        assert!(rank_memory_docs(&docs, "  ").is_empty());
    }

    /// Matched lines prefer needle hits and clip char-safely
    #[test]
    fn matched_lines_prefer_needle_hits_and_clip() {
        let source = "---\ndescription: a lesson\n---\nthe xor key sits here\nunrelated line\n";
        let lines = matched_lines(source, "xor key", &["lesson".to_owned()]);
        // the needle line leads even though a term line appears earlier
        assert_eq!(lines[0].line, 4);
        assert!(lines[0].text.contains("xor key"));
        // a long line clips to the char budget with an ellipsis
        let long = format!("{} xor", "é".repeat(SNIPPET_LINE_CHARS * 2));
        let clipped = matched_lines(&long, "xor", &[]);
        assert!(clipped[0].text.chars().count() <= SNIPPET_LINE_CHARS + 1);
        assert!(clipped[0].text.ends_with('…'));
        // no literal hit at all falls back to the description line
        let bare = matched_lines(source, "zzz-no-hit", &[]);
        assert_eq!(bare[0].line, 2);
        assert!(bare[0].text.starts_with("description:"));
    }
}
