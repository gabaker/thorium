//! BM25 lexical search over the forum's posts and comments
//!
//! Backs both the `search_forum` agent tool and the TUI message board's search
//! box through one [`super::ForumState::search`] entry point. One engine is
//! fitted per forum revision and cached inside [`super::ForumState`] (see
//! [`engine_at`], mirroring the constant tracker's revision-keyed presence
//! cache), then refitted lazily on the first search after a mutation -- so a
//! burst of queries over an unchanged forum re-tokenizes nothing. Ranking
//! orders the full candidate set deterministically, but only the top `limit`
//! hits pay for snippet extraction.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use bm25::{DefaultTokenizer, Document, Language, SearchEngine, SearchEngineBuilder, Tokenizer};
use serde::{Deserialize, Serialize};

use super::{Comment, Kind, ParentRef, Post, Stance, root_post_of};

/// The BM25 term-frequency saturation parameter
const BM25_K1: f32 = 1.2;

/// The BM25 document-length normalization parameter
const BM25_B: f32 = 0.75;

/// The length (in characters) a hit's snippet is clipped to
const SNIPPET_CHARS: usize = 160;

/// The shortest identifier sub-word worth indexing on its own
///
/// Splitting `aX` into `a` + `X` would only add noise, so expansions shorter
/// than this are dropped (the whole identifier always remains searchable).
const MIN_SUBWORD_CHARS: usize = 2;

/// A single forum search hit: enough to decide whether to pull the thread
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// The matched post or comment, typed for programmatic consumers (the TUI)
    pub target: ParentRef,
    /// The reference to pull the full thread with (`#post-name` or `!123`)
    pub reference: String,
    /// The matched post's title; a comment hit carries its root post's title
    pub title: String,
    /// A short one-line excerpt of the matched body around the query terms
    pub snippet: String,
    /// Whether the hit states something or asks a (tracked) question
    pub kind: Kind,
    /// How a comment hit's evidence relates to its parent (`None` for posts)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stance: Option<Stance>,
    /// The author of the matched post or comment
    pub author: String,
    /// The BM25 relevance score; higher is more relevant
    pub score: f32,
}

/// The tokenizer the forum's BM25 engine applies to documents and queries
///
/// Wraps the stock English tokenizer (unicode normalization, stop words,
/// stemming) but first expands camelCase and snake_case identifiers beside the
/// originals, so `CreateRemoteThread` also matches a `remote thread` query
/// while literal tokens like hex addresses and hashes keep matching whole.
pub(crate) struct ForumTokenizer {
    /// The stock tokenizer the expanded text is handed to
    inner: DefaultTokenizer,
}

impl ForumTokenizer {
    /// Creates the forum tokenizer
    pub(crate) fn new() -> Self {
        // the forum is technical English, so fix the language up front rather
        // than paying for per-text language detection
        let inner = DefaultTokenizer::builder()
            .language_mode(Language::English)
            .normalization(true)
            .stopwords(true)
            .stemming(true)
            .build();
        ForumTokenizer { inner }
    }
}

impl Tokenizer for ForumTokenizer {
    /// Tokenizes forum text or a query for the BM25 engine
    ///
    /// # Arguments
    ///
    /// * `input_text` - The text to tokenize
    fn tokenize(&self, input_text: &str) -> Vec<String> {
        // append identifier sub-words beside the original text, then hand the
        // whole thing to the stock tokenizer so both get the same treatment
        self.inner.tokenize(&expand_subwords(input_text))
    }
}

/// Appends the camelCase/snake_case sub-words of a text's identifiers to it
///
/// The original text is kept intact (whole identifiers still match verbatim);
/// only identifiers that actually split contribute extra words. Alphanumeric
/// runs are never split between letters and digits, so hex addresses like
/// `0x401000`, hashes, and names like `c2` survive whole.
///
/// # Arguments
///
/// * `text` - The text whose identifiers to expand
fn expand_subwords(text: &str) -> String {
    // collect the sub-words of every whitespace-separated identifier
    let mut expansions: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        // walk each alphanumeric/underscore run inside the token separately so
        // punctuation like `#`, `(`, or `=` never glues identifiers together
        for run in token.split(|c: char| !c.is_alphanumeric() && c != '_') {
            // split the run on underscores and camelCase boundaries
            let words = split_identifier(run);
            // a run that survives whole has nothing new to contribute; a single
            // word that differs from the run (`__init__` -> `init`) still does,
            // since the stock tokenizer keeps underscores glued to the literal
            if words.len() < 2 && words.first().map(String::as_str) == Some(run) {
                continue;
            }
            // keep only sub-words long enough to be meaningful search terms
            for word in words {
                if word.chars().count() >= MIN_SUBWORD_CHARS {
                    expansions.push(word);
                }
            }
        }
    }
    // nothing split: hand back the original text without reallocating
    if expansions.is_empty() {
        return text.to_owned();
    }
    // append the expansions after the original text
    format!("{text} {}", expansions.join(" "))
}

/// Splits one identifier on underscores and camelCase boundaries
///
/// Boundaries are a lowercase letter or digit followed by an uppercase letter
/// (`SHA256Hash` -> `SHA256`, `Hash`) and the last uppercase letter of a run
/// that precedes a lowercase one (`XORDecode` -> `XOR`, `Decode`). Letters and
/// digits never split apart.
///
/// # Arguments
///
/// * `identifier` - The alphanumeric/underscore run to split
fn split_identifier(identifier: &str) -> Vec<String> {
    // collect the split words across every underscore segment
    let mut words = Vec::new();
    for segment in identifier.split('_') {
        // index by char so multibyte text can never split mid-character
        let chars: Vec<char> = segment.chars().collect();
        if chars.is_empty() {
            continue;
        }
        // walk the chars, cutting a word at each camelCase boundary
        let mut start = 0;
        for i in 1..chars.len() {
            let prev = chars[i - 1];
            let current = chars[i];
            // an upper-run followed by a lowercase letter ends before its last upper
            let acronym_end = prev.is_uppercase()
                && current.is_uppercase()
                && chars.get(i + 1).is_some_and(|next| next.is_lowercase());
            // a lowercase letter or digit followed by an uppercase letter splits
            let case_flip = (prev.is_lowercase() || prev.is_numeric()) && current.is_uppercase();
            if case_flip || acronym_end {
                // close the word that ends at this boundary
                words.push(chars[start..i].iter().collect());
                start = i;
            }
        }
        // close the final word of this segment
        words.push(chars[start..].iter().collect());
    }
    words
}

/// Derives the query terms shared by ranking, snippets, and highlighting
///
/// The list is the tokenizer's output (lowercased stems, with camelCase and
/// snake_case identifiers expanded beside their whole forms) plus the raw
/// lowercased query words, since stemming can rewrite a term (e.g. "memory"
/// -> "memori") into something the raw text never contains. Raw words the
/// tokenizer drops outright (stop words, bare punctuation) are skipped so a
/// highlighter never marks every "the", and duplicates are dropped. Stems are
/// prefixes of the words they came from, so every term substring-matches raw
/// text when paired with [`match_ranges`].
///
/// # Arguments
///
/// * `query` - The query to derive terms from
pub fn search_terms(query: &str) -> Vec<String> {
    // tokenize the query the same way documents are indexed
    let tokenizer = ForumTokenizer::new();
    let terms = tokenizer.tokenize(query);
    // keep the raw lowercased words too, since stemming can rewrite a term
    // into something the raw text never contains
    let raw = query
        .split_whitespace()
        // a word the tokenizer drops entirely is a stop word or punctuation,
        // which would mark text everywhere without helping the search
        .filter(|word| !tokenizer.tokenize(word).is_empty())
        .map(str::to_lowercase);
    // drop empty terms and duplicates, keeping first occurrences in order
    let mut deduped: Vec<String> = Vec::with_capacity(terms.len());
    for term in terms.into_iter().chain(raw) {
        if !term.is_empty() && !deduped.contains(&term) {
            deduped.push(term);
        }
    }
    deduped
}

/// Finds every place in a text where any query term occurs
///
/// Matching is case-insensitive and char-accurate: the text is lowercased
/// char by char with every lowered char mapped back to the char it came from,
/// so a multi-char lowering (e.g. 'İ' -> "i\u{307}") can never drift a range
/// off the text it matched. Overlapping and touching ranges merge, so a stem
/// and the raw word it prefixes mark one continuous run of text.
///
/// The returned ranges index **chars** of `text`, sorted ascending.
///
/// # Arguments
///
/// * `text` - The text to find the terms in
/// * `terms` - The lowercased terms to look for
pub fn match_ranges(text: &str, terms: &[String]) -> Vec<Range<usize>> {
    // nothing can match an empty text or an empty term list
    if text.is_empty() || terms.is_empty() {
        return Vec::new();
    }
    // lowercase char by char, mapping every lowered char back to the text
    // char it came from, so a multi-char lowering can never drift a range
    let mut lower = String::with_capacity(text.len());
    let mut source = Vec::with_capacity(text.len());
    for (index, c) in text.chars().enumerate() {
        for lowered in c.to_lowercase() {
            lower.push(lowered);
            source.push(index);
        }
    }
    // collect the char range of every occurrence of every term
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for term in terms.iter().filter(|term| !term.is_empty()) {
        // the term's length in lowered chars, for mapping each match's end
        let term_chars = term.chars().count();
        for (byte_idx, _) in lower.match_indices(term.as_str()) {
            // map the match's lowered byte offset to a lowered char offset
            let start = lower[..byte_idx].chars().count();
            // map both lowered ends back onto the chars they came from
            let end = source[start + term_chars - 1] + 1;
            ranges.push(source[start]..end);
        }
    }
    // sort the ranges so overlaps are adjacent and the output is ordered
    ranges.sort_by_key(|range| (range.start, range.end));
    // merge overlapping and touching ranges into continuous runs
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            // extend the previous run when this range touches or overlaps it
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            // otherwise this range starts a new run
            _ => merged.push(range),
        }
    }
    merged
}

/// The concrete BM25 engine type fitted over the forum's corpus
pub(crate) type ForumEngine = SearchEngine<ParentRef, u32, ForumTokenizer>;

/// A BM25 engine fitted to the forum's corpus, keyed by forum revision
///
/// Stored by [`super::ForumState`] behind a mutex so one fit at a time serves
/// every searcher; the engine itself is handed out as an [`Arc`] so scoring
/// can run after the cache lock is released.
pub(crate) struct CachedEngine {
    /// The forum revision read just before the corpus the engine was fitted to
    revision: u64,
    /// The fitted engine, shared so scoring can outlive the cache lock
    engine: Arc<ForumEngine>,
}

/// Reuses the cached engine when its revision matches, refitting it otherwise
///
/// The caller must read `revision` *before* taking the corpus read locks, so
/// the cached engine can only ever be fresher than the revision it is keyed
/// by, never staler: a mutation that lands between the revision read and the
/// corpus locks bumps the counter, forcing a refit on the next search.
///
/// # Arguments
///
/// * `cache` - The cache slot guarding the fitted engine
/// * `revision` - The forum revision read before the corpus was locked
/// * `posts` - The live posts to fit over on a cache miss
/// * `comments` - The live comments to fit over on a cache miss
pub(crate) fn engine_at(
    cache: &mut Option<CachedEngine>,
    revision: u64,
    posts: &HashMap<String, Post>,
    comments: &HashMap<usize, Comment>,
) -> Arc<ForumEngine> {
    // an engine fitted at this revision -- or a newer one, left by a search
    // that read the counter later but reached the cache first -- is reused;
    // taking the fresher engine is always sound (its corpus covers ours) and
    // never downgrades the cache key, so an unchanged forum never refits
    if let Some(cached) = cache
        && cached.revision >= revision
    {
        return cached.engine.clone();
    }
    // the forum changed since the last fit: fit a fresh engine over it
    let engine = Arc::new(fit(documents(posts, comments)));
    // remember the revision this fit covers for the next search
    *cache = Some(CachedEngine {
        revision,
        engine: engine.clone(),
    });
    engine
}

/// Builds one BM25 document per post and per comment on the forum
///
/// # Arguments
///
/// * `posts` - Every post on the forum, keyed by name
/// * `comments` - Every comment on the forum, keyed by id
fn documents(
    posts: &HashMap<String, Post>,
    comments: &HashMap<usize, Comment>,
) -> Vec<Document<ParentRef>> {
    // build one BM25 document per post and per comment
    let mut documents = Vec::with_capacity(posts.len() + comments.len());
    for post in posts.values() {
        // double the name and title so matches there outrank body matches
        let contents = format!(
            "{} {} {} {} {}",
            post.name, post.title, post.name, post.title, post.body
        );
        documents.push(Document::new(ParentRef::Post(post.name.clone()), contents));
    }
    for comment in comments.values() {
        // a comment is just its body; its root title is display-only so every
        // comment under a well-titled post does not match title queries
        documents.push(Document::new(
            ParentRef::Comment(comment.id),
            comment.body.clone(),
        ));
    }
    documents
}

/// Fits a BM25 engine over the forum's documents
///
/// This is the expensive step the revision-keyed cache amortizes: fitting
/// re-tokenizes every document (identifier expansion, stemming, stop words).
///
/// # Arguments
///
/// * `documents` - The corpus to fit the engine to
fn fit(documents: Vec<Document<ParentRef>>) -> ForumEngine {
    // the tokenizer the engine applies to documents now and queries later
    let tokenizer = ForumTokenizer::new();
    // fit the engine over the corpus with the standard BM25 parameters
    SearchEngineBuilder::<ParentRef, u32, ForumTokenizer>::with_tokenizer_and_documents(
        tokenizer, documents,
    )
    .k1(BM25_K1)
    .b(BM25_B)
    .build()
}

/// A scored match before its hit is assembled: just enough to order and cut
///
/// Keeping the pre-cut candidates this light is the point of the restructured
/// ranking: snippet extraction (which lowercases whole bodies char by char)
/// happens only for the hits that survive the cut at `limit`.
struct Scored {
    /// The matched post or comment
    target: ParentRef,
    /// The reference the deterministic tie-break compares (`#name` / `!123`)
    reference: String,
    /// The BM25 relevance score
    score: f32,
    /// The recency stamp score ties break on
    updated: u64,
}

/// Scores every post and comment against a query, best hits first
///
/// Posts index their name, title (both doubled as a cheap field boost), and
/// body; comments index only their body, with the root post's title attached
/// to the hit for display. Results are deterministic: BM25 score descending,
/// then most recently updated, then reference. Only the top `limit` hits pay
/// for snippet extraction; candidates below the cut are ordered and dropped
/// without their body text ever being touched.
///
/// # Arguments
///
/// * `engine` - The BM25 engine fitted over the forum (see [`engine_at`])
/// * `posts` - Every post on the forum, keyed by name
/// * `comments` - Every comment on the forum, keyed by id
/// * `query` - The query to rank against
/// * `limit` - The most hits to return
pub(crate) fn rank(
    engine: &ForumEngine,
    posts: &HashMap<String, Post>,
    comments: &HashMap<usize, Comment>,
    query: &str,
    limit: usize,
) -> Vec<SearchHit> {
    // an empty query, an empty forum, or a zero limit can never match anything
    if query.trim().is_empty() || limit == 0 || (posts.is_empty() && comments.is_empty()) {
        return Vec::new();
    }
    // pull every match so the deterministic tie-break below orders the full
    // candidate set before the cut at `limit`
    let results = engine.search(query, None);
    // resolve each scored document into the lightweight fields the ordering
    // needs, leaving every body untouched below the cut
    let mut scored: Vec<Scored> = results
        .into_iter()
        .filter_map(|result| {
            // look up the reference and recency stamp the tie-break orders on
            let (reference, updated) = match &result.document.id {
                ParentRef::Post(name) => (format!("#{name}"), posts.get(name)?.updated),
                ParentRef::Comment(id) => (format!("!{id}"), comments.get(id)?.updated),
            };
            Some(Scored {
                target: result.document.id,
                reference,
                score: result.score,
                updated,
            })
        })
        .collect();
    // order deterministically: score, then recency, then reference
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.updated.cmp(&a.updated))
            .then(a.reference.cmp(&b.reference))
    });
    // cut to the requested number of hits before any snippet work
    scored.truncate(limit);
    // derive the terms snippets center on; stems are prefixes, so they still
    // substring-match raw bodies
    let terms = search_terms(query);
    // assemble the full hits, paying for snippets only above the cut
    scored
        .into_iter()
        .filter_map(|hit| {
            let assembled = match &hit.target {
                // a post hit carries its own title and no stance
                ParentRef::Post(name) => {
                    let post = posts.get(name)?;
                    SearchHit {
                        target: hit.target.clone(),
                        reference: hit.reference,
                        title: post.title.clone(),
                        snippet: snippet(&post.body, &post.title, &terms),
                        kind: post.kind,
                        stance: None,
                        author: post.author.clone(),
                        score: hit.score,
                    }
                }
                // a comment hit borrows its root post's title for context
                ParentRef::Comment(id) => {
                    let comment = comments.get(id)?;
                    let title = root_post_of(comments, *id)
                        .and_then(|name| posts.get(&name))
                        .map(|post| post.title.clone())
                        .unwrap_or_default();
                    SearchHit {
                        target: hit.target.clone(),
                        reference: hit.reference,
                        title,
                        snippet: snippet(&comment.body, "", &terms),
                        kind: comment.kind,
                        stance: Some(comment.stance),
                        author: comment.author.clone(),
                        score: hit.score,
                    }
                }
            };
            Some(assembled)
        })
        .collect()
}

/// Builds a one-line excerpt of a body windowed around the first query match
///
/// The body's whitespace is flattened to single spaces, the window is centered
/// on the earliest occurrence of any query term, and `…` marks each clipped
/// edge. All indexing is by char so multibyte text can never split mid-character.
///
/// # Arguments
///
/// * `body` - The matched post or comment body to excerpt
/// * `fallback` - The text to excerpt instead when the body is empty
/// * `terms` - The lowercased query terms to center the window on
fn snippet(body: &str, fallback: &str, terms: &[String]) -> String {
    // flatten runs of whitespace (and newlines) into single spaces
    let flat = body.split_whitespace().collect::<Vec<_>>().join(" ");
    // an empty body falls back to the alternate text (e.g. the post title)
    if flat.is_empty() {
        return fallback.chars().take(SNIPPET_CHARS).collect();
    }
    // find the earliest query-term occurrence in flat char space; the shared
    // matcher keeps snippet windows and highlights on the exact same text
    let earliest = match_ranges(&flat, terms).first().map(|range| range.start);
    // window the flattened body around the match, clamped to its ends
    let chars: Vec<char> = flat.chars().collect();
    let total = chars.len();
    let (start, end) = match earliest {
        // center the window on the match when the body overflows the snippet
        Some(position) if total > SNIPPET_CHARS => {
            let start = position.saturating_sub(SNIPPET_CHARS / 2);
            let end = (start + SNIPPET_CHARS).min(total);
            // pull the window back left when it ran past the end of the body
            (end.saturating_sub(SNIPPET_CHARS), end)
        }
        // a short body (or no local match) is excerpted from the start
        _ => (0, total.min(SNIPPET_CHARS)),
    };
    // assemble the window with ellipses marking each clipped edge
    let mut out = String::with_capacity(SNIPPET_CHARS + 2);
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < total {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a statement post for ranking tests
    ///
    /// # Arguments
    ///
    /// * `name` - The unique post name
    /// * `title` - The post title
    /// * `body` - The post body
    /// * `updated` - The forum revision stamp
    fn post(name: &str, title: &str, body: &str, updated: u64) -> Post {
        Post {
            name: name.to_owned(),
            author: "tester".to_owned(),
            title: title.to_owned(),
            body: body.to_owned(),
            kind: Kind::Statement,
            updated,
        }
    }

    /// Builds a neutral statement comment for ranking tests
    ///
    /// # Arguments
    ///
    /// * `id` - The comment id
    /// * `parent` - The post or comment this comment replies to
    /// * `body` - The comment body
    /// * `updated` - The forum revision stamp
    fn comment(id: usize, parent: ParentRef, body: &str, updated: u64) -> Comment {
        Comment {
            id,
            parent,
            author: "tester".to_owned(),
            body: body.to_owned(),
            stance: Stance::Neutral,
            kind: Kind::Statement,
            updated,
        }
    }

    /// Builds the posts/comments maps rank() takes from plain lists
    ///
    /// # Arguments
    ///
    /// * `posts` - The posts to key by name
    /// * `comments` - The comments to key by id
    fn corpus(
        posts: Vec<Post>,
        comments: Vec<Comment>,
    ) -> (HashMap<String, Post>, HashMap<usize, Comment>) {
        let posts = posts.into_iter().map(|p| (p.name.clone(), p)).collect();
        let comments = comments.into_iter().map(|c| (c.id, c)).collect();
        (posts, comments)
    }

    /// Fits an engine over the corpus and ranks a query against it
    ///
    /// # Arguments
    ///
    /// * `posts` - The posts to rank over, keyed by name
    /// * `comments` - The comments to rank over, keyed by id
    /// * `query` - The query to rank against
    /// * `limit` - The most hits to return
    fn rank_all(
        posts: &HashMap<String, Post>,
        comments: &HashMap<usize, Comment>,
        query: &str,
        limit: usize,
    ) -> Vec<SearchHit> {
        // fit a fresh engine the way ForumState's cache does on a miss
        let engine = fit(documents(posts, comments));
        rank(&engine, posts, comments, query, limit)
    }

    /// CamelCase and snake_case identifiers split while literals stay whole
    #[test]
    fn tokenize_splits_identifiers_and_keeps_literals_whole() {
        let tokenizer = ForumTokenizer::new();
        // a camelCase API name matches by its words and verbatim
        let tokens = tokenizer.tokenize("CreateRemoteThread");
        assert!(tokens.contains(&"createremotethread".to_owned()));
        assert!(tokens.contains(&"creat".to_owned()) || tokens.contains(&"create".to_owned()));
        assert!(tokens.contains(&"remot".to_owned()) || tokens.contains(&"remote".to_owned()));
        assert!(tokens.contains(&"thread".to_owned()));
        // a snake_case key splits on the underscore (modulo stemming)
        let tokens = tokenizer.tokenize("act=recive_message");
        assert!(tokens.iter().any(|t| t.starts_with("reciv")));
        assert!(tokens.iter().any(|t| t.starts_with("messag")));
        // hex addresses never split between letters and digits
        let tokens = tokenizer.tokenize("0x401000");
        assert!(tokens.contains(&"0x401000".to_owned()));
        // underscore-wrapped single words still expand to their bare word,
        // since the stock tokenizer keeps the underscores glued to the literal
        let tokens = tokenizer.tokenize("the loader calls __init__ then _start");
        assert!(tokens.contains(&"init".to_owned()));
        assert!(tokens.contains(&"start".to_owned()));
        // an upper-run acronym splits before its trailing word
        assert_eq!(split_identifier("XORDecode"), vec!["XOR", "Decode"]);
        // a digit-to-upper flip splits after the digits
        assert_eq!(split_identifier("SHA256Hash"), vec!["SHA256", "Hash"]);
    }

    /// Search terms keep stems, raw words, and expansions but drop stop words
    #[test]
    fn search_terms_drop_stopwords_and_keep_raw_words() {
        let terms = search_terms("the CreateRemoteThread memory");
        // the whole lowercased identifier and its sub-words are all present
        assert!(terms.contains(&"createremotethread".to_owned()));
        assert!(terms.iter().any(|term| term.starts_with("creat")));
        assert!(terms.iter().any(|term| term.starts_with("remot")));
        assert!(terms.contains(&"thread".to_owned()));
        // the raw word survives even though stemming rewrites it
        assert!(terms.contains(&"memory".to_owned()));
        // the stop word is dropped rather than highlighting every "the"
        assert!(!terms.contains(&"the".to_owned()));
        // literals like hex addresses stay whole
        let terms = search_terms("0x401000");
        assert!(terms.contains(&"0x401000".to_owned()));
        // nothing appears twice
        let terms = search_terms("thread thread Thread");
        assert_eq!(terms, vec!["thread".to_owned()]);
    }

    /// Match ranges find every occurrence, merge overlaps, and stay char-safe
    #[test]
    fn match_ranges_find_merge_and_stay_char_safe() {
        // every occurrence of every term is found case-insensitively, in order
        let terms = vec!["xor".to_owned()];
        assert_eq!(
            match_ranges("XOR then xor again", &terms),
            vec![0..3, 9..12]
        );
        // a stem and the raw word it prefixes merge into one range
        let terms = vec!["remot".to_owned(), "remote".to_owned()];
        assert_eq!(match_ranges("a remote thread", &terms), vec![2..8]);
        // touching ranges merge into one continuous run
        let terms = vec!["ab".to_owned(), "cd".to_owned()];
        assert_eq!(match_ranges("abcd", &terms), vec![0..4]);
        // ranges stay char accurate when lowercasing grows a char ('İ' -> 2)
        let terms = vec!["xor".to_owned()];
        let text = format!("{} xor", "İ".repeat(3));
        assert_eq!(match_ranges(&text, &terms), vec![4..7]);
        // multibyte text matches whole and never splits a char
        let terms = vec!["🔑".to_owned()];
        assert_eq!(match_ranges("ключ 🔑 here", &terms), vec![5..6]);
        // no terms, no matches, and no text all find nothing
        assert!(match_ranges("anything", &[]).is_empty());
        assert!(match_ranges("anything", &["zzz".to_owned()]).is_empty());
        assert!(match_ranges("", &["x".to_owned()]).is_empty());
    }

    /// Posts and comments both match, and comment hits carry their post's title
    #[test]
    fn rank_matches_posts_and_comments() {
        let (posts, comments) = corpus(
            vec![post(
                "c2-config",
                "Decoded C2 configuration",
                "The beacon calls home.",
                1,
            )],
            vec![comment(
                1,
                ParentRef::Post("c2-config".to_owned()),
                "The beacon interval is 60 seconds.",
                2,
            )],
        );
        let hits = rank_all(&posts, &comments, "beacon", 10);
        assert_eq!(hits.len(), 2);
        // both references are well formed
        assert!(hits.iter().any(|hit| hit.reference == "#c2-config"));
        assert!(hits.iter().any(|hit| hit.reference == "!1"));
        // the comment hit borrows the root post's title and keeps its stance
        let comment_hit = hits.iter().find(|hit| hit.reference == "!1").unwrap();
        assert_eq!(comment_hit.title, "Decoded C2 configuration");
        assert_eq!(comment_hit.stance, Some(Stance::Neutral));
        assert!(comment_hit.snippet.contains("beacon interval"));
    }

    /// A title match outranks the same term buried in another post's body
    #[test]
    fn rank_prefers_title_matches_over_body_matches() {
        let (posts, comments) = corpus(
            vec![
                post(
                    "in-body",
                    "Unrelated title",
                    "This body mentions ransomware once.",
                    1,
                ),
                post(
                    "in-title",
                    "Ransomware note analysis",
                    "A body about something else.",
                    2,
                ),
            ],
            Vec::new(),
        );
        let hits = rank_all(&posts, &comments, "ransomware", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].reference, "#in-title");
    }

    /// A rare term outranks a term that appears in every document
    #[test]
    fn rank_prefers_rare_terms() {
        let (posts, comments) = corpus(
            vec![
                post(
                    "common-1",
                    "Entry one",
                    "the malware does malware things",
                    1,
                ),
                post("common-2", "Entry two", "more malware notes on malware", 2),
                post(
                    "rare",
                    "Entry three",
                    "the malware uses salsa20 encryption",
                    3,
                ),
            ],
            Vec::new(),
        );
        // both query terms appear in the rare post, only one in the others
        let hits = rank_all(&posts, &comments, "malware salsa20", 10);
        assert_eq!(hits[0].reference, "#rare");
    }

    /// The limit caps hits and equal scores break ties by recency
    #[test]
    fn rank_respects_limit_and_breaks_ties_by_recency() {
        let (posts, comments) = corpus(
            vec![
                post("older", "Same words here", "identical body text", 1),
                post("newer", "Same words here", "identical body text", 2),
            ],
            Vec::new(),
        );
        // identical documents score identically, so recency decides the order
        let hits = rank_all(&posts, &comments, "identical", 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].reference, "#newer");
        // the limit cuts the ranked list, not the corpus
        let hits = rank_all(&posts, &comments, "identical", 1);
        assert_eq!(hits.len(), 1);
    }

    /// Empty queries, unmatched queries, and empty forums return nothing
    #[test]
    fn rank_returns_nothing_for_empty_or_unmatched_queries() {
        let (posts, comments) = corpus(vec![post("only", "A post", "with a body", 1)], Vec::new());
        assert!(rank_all(&posts, &comments, "", 10).is_empty());
        assert!(rank_all(&posts, &comments, "   ", 10).is_empty());
        assert!(rank_all(&posts, &comments, "zzzmissing", 10).is_empty());
        assert!(rank_all(&posts, &comments, "body", 0).is_empty());
        let empty = (HashMap::new(), HashMap::new());
        assert!(rank_all(&empty.0, &empty.1, "body", 10).is_empty());
    }

    /// Truncation happens after the full deterministic sort, not before
    #[test]
    fn limit_cut_preserves_full_ranking_order() {
        // three posts that score distinctly against the same query
        let (posts, comments) = corpus(
            vec![
                post(
                    "body-hit",
                    "Unrelated title",
                    "the loader mentions beacon once",
                    1,
                ),
                post(
                    "title-hit",
                    "Beacon configuration",
                    "a body about something else",
                    2,
                ),
                post(
                    "double-hit",
                    "Beacon beacon notes",
                    "the beacon calls the beacon home",
                    3,
                ),
            ],
            Vec::new(),
        );
        // the full ordering over every candidate
        let full = rank_all(&posts, &comments, "beacon", 10);
        assert_eq!(full.len(), 3);
        // a limit of one returns exactly the full ordering's best hit
        let top = rank_all(&posts, &comments, "beacon", 1);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].reference, full[0].reference);
        // a limit of two returns exactly the full ordering's first two hits
        let two: Vec<String> = rank_all(&posts, &comments, "beacon", 2)
            .into_iter()
            .map(|hit| hit.reference)
            .collect();
        let expected: Vec<String> = full
            .iter()
            .take(2)
            .map(|hit| hit.reference.clone())
            .collect();
        assert_eq!(two, expected);
    }

    /// Snippets window the match, flatten newlines, and stay char-safe
    #[test]
    fn snippet_windows_the_match_and_flattens_newlines() {
        // a long multiline body with the match far from the start
        let padding = "irrelevant words ".repeat(30);
        let body = format!("{padding}\nthe xor key\nis 0x5A\n{padding}");
        let out = snippet(&body, "", &["xor".to_owned()]);
        assert!(out.contains("xor key is 0x5A"));
        assert!(out.starts_with('…') && out.ends_with('…'));
        assert!(out.chars().count() <= SNIPPET_CHARS + 2);
        // multibyte text never panics and still clips cleanly
        let unicode = "ключ-🔑 ".repeat(100);
        let out = snippet(&unicode, "", &["🔑".to_owned()]);
        assert!(out.chars().count() <= SNIPPET_CHARS + 2);
        // chars whose lowercase form grows ('İ' -> "i\u{307}") before the match
        // must not drift the window off the matched term
        let drifting = format!("{} the xor key {}", "İ".repeat(100), padding);
        let out = snippet(&drifting, "", &["xor".to_owned()]);
        assert!(out.contains("xor"));
        // an empty body falls back to the supplied title text
        assert_eq!(snippet("", "Fallback title", &[]), "Fallback title");
    }
}
