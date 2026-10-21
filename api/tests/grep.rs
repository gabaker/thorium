//! Tests the grep based search routes in Thorium

use std::collections::HashMap;

use chrono::{Duration, Utc};
use thorium::client::{GrepClient, ResultsClient};
use thorium::models::{
    Buffer, CommentRequest, EntityMetadataRequest, EntityRequest, FileDeleteOpts, Flag,
    GrepDocumentMatches, GrepMatches, GrepOpts, GrepSource, GrepStats, GroupUpdate,
    GroupUsersUpdate, ImageVersion, OriginRequest, OutputDisplayType, OutputRequest, RepoCheckout,
    RepoRequest, Sample, SampleRequest, TagRequest, TruncationReason,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, contains_key, fail, is, is_empty, is_not};

/// A pattern that is unlikely to collide with anything else in a shared test cluster
const PATTERN: &str = "CornGrepNeedle[0-9]+";

/// The needle our pattern matches
const NEEDLE: &str = "CornGrepNeedle42";

/// The tool we attach greppable results to
const TOOL: &str = "GrepTestTool";

/// A second tool used to prove the `tools` filter actually excludes results
const OTHER_TOOL: &str = "GrepOtherTool";

/// Collect every snippet out of a set of document matches
///
/// # Arguments
///
/// * `docs` - The documents to collect snippets from
fn snippets<'a, I: IntoIterator<Item = &'a GrepDocumentMatches>>(docs: I) -> Vec<&'a str> {
    docs.into_iter()
        .flat_map(|doc| doc.matches.iter().map(|found| found.snippet.as_str()))
        .collect()
}

/// Check that at least one snippet in a set of documents contains our needle
///
/// # Arguments
///
/// * `docs` - The documents to check for our needle
fn has_needle<'a, I: IntoIterator<Item = &'a GrepDocumentMatches>>(docs: I) -> bool {
    snippets(docs).iter().any(|snip| snip.contains(NEEDLE))
}

/// Count every match across an entire set of item matches
///
/// # Arguments
///
/// * `matches` - The item matches to count matches in
fn total_matches(matches: &GrepMatches) -> usize {
    // count the matches in our inline results and result files
    let results = matches
        .results
        .values()
        .chain(matches.result_files.values())
        .flat_map(|by_id| by_id.values())
        .flat_map(HashMap::values);
    // count the matches in our submission info
    let submissions = matches.submissions.values().flat_map(HashMap::values);
    // count the matches in our tags
    let tags = matches.tags.values().flat_map(HashMap::values);
    // sum up the matches in every source
    results
        .chain(submissions)
        .chain(matches.comments.values())
        .chain(tags)
        .chain(matches.metadata.values())
        .map(|doc| doc.matches.len())
        .sum()
}

/// Chain every source's documents in a set of item matches together
///
/// # Arguments
///
/// * `matches` - The item matches to chain documents from
fn all_docs(matches: &GrepMatches) -> impl Iterator<Item = &GrepDocumentMatches> {
    // chain every source's documents together
    matches
        .results
        .values()
        .chain(matches.result_files.values())
        .flat_map(|by_id| by_id.values())
        .flat_map(HashMap::values)
        .chain(matches.submissions.values().flat_map(HashMap::values))
        .chain(matches.comments.values())
        .chain(matches.tags.values().flat_map(HashMap::values))
        .chain(matches.metadata.values())
}

/// Check if any document in a set of item matches was truncated for a specific reason
///
/// # Arguments
///
/// * `matches` - The item matches to check for truncated documents
/// * `reason` - The truncation reason to look for
fn any_truncated_for(matches: &GrepMatches, reason: TruncationReason) -> bool {
    // look for any document that was truncated for this specific reason
    all_docs(matches).any(|doc| doc.truncated == Some(reason))
}

/// Collect a stable identity for every match in a set of item matches
///
/// Identities are (document path, offset, length) triples so tests can assert
/// that paging a budget starved cursor never duplicates or drops a match.
///
/// # Arguments
///
/// * `matches` - The item matches to collect identities from
fn match_identities(matches: &GrepMatches) -> Vec<(String, usize, usize)> {
    // the identities of every match across every source
    let mut identities = Vec::new();
    // collect the matches in our inline results
    for (tool, by_id) in &matches.results {
        for (id, by_field) in by_id {
            for (field, doc) in by_field {
                for found in &doc.matches {
                    let path = format!("{}:results:{tool}:{id}:{field}", matches.key);
                    identities.push((path, found.offset, found.len));
                }
            }
        }
    }
    // collect the matches in our result files
    for (tool, by_id) in &matches.result_files {
        for (id, by_name) in by_id {
            for (name, doc) in by_name {
                for found in &doc.matches {
                    let path = format!("{}:result_files:{tool}:{id}:{name}", matches.key);
                    identities.push((path, found.offset, found.len));
                }
            }
        }
    }
    // collect the matches in our submission info
    for (id, by_field) in &matches.submissions {
        for (field, doc) in by_field {
            for found in &doc.matches {
                let path = format!("{}:submissions:{id}:{field}", matches.key);
                identities.push((path, found.offset, found.len));
            }
        }
    }
    // collect the matches in our comments
    for (id, doc) in &matches.comments {
        for found in &doc.matches {
            let path = format!("{}:comments:{id}", matches.key);
            identities.push((path, found.offset, found.len));
        }
    }
    // collect the matches in our tags
    for (key, by_value) in &matches.tags {
        for (value, doc) in by_value {
            for found in &doc.matches {
                let path = format!("{}:tags:{key}:{value}", matches.key);
                identities.push((path, found.offset, found.len));
            }
        }
    }
    // collect the matches in any type specific metadata
    for (field, doc) in &matches.metadata {
        for found in &doc.matches {
            let path = format!("{}:metadata:{field}", matches.key);
            identities.push((path, found.offset, found.len));
        }
    }
    identities
}

/// Upload a sample that has our needle in every greppable source
///
/// Each call uploads a unique sample since the buffer is salted with a random
/// string.
///
/// # Arguments
///
/// * `client` - The client to upload this sample with
/// * `group` - The group to upload this sample too
async fn seed_sample(client: &Thorium, group: &str) -> Result<String, Error> {
    // upload a sample whose name, description, and origin all contain our needle;
    // salt the buffer so every call uploads a distinct sample (identical content
    // in the same group would 409 as a duplicate submission)
    let file_req = SampleRequest::new_buffer(
        Buffer::new(format!(
            "sample body {NEEDLE} {}",
            generators::gen_string(16)
        ))
        .name(format!("{NEEDLE}.txt")),
        vec![group.to_owned()],
    )
    .description(format!("a description with {NEEDLE} in it"))
    .tag("GrepTag", NEEDLE)
    .origin(OriginRequest::downloaded(
        format!("https://{NEEDLE}.example.com"),
        Some("google".to_owned()),
    ));
    let hashes = client.files.create(file_req).await?;
    // comment on this sample with our needle
    let comment_req = CommentRequest::new(&hashes.sha256, format!("a comment with {NEEDLE} in it"));
    client.files.comment(comment_req).await?;
    // attach a result with our needle in both its inline JSON and a result file
    let output_req = OutputRequest::new(
        hashes.sha256.clone(),
        TOOL,
        format!("an inline result with {NEEDLE} in it"),
        OutputDisplayType::String,
    )
    .buffer(Buffer::new(format!("line one\nline two {NEEDLE}\n")).name("grep.txt"))
    .tool_version(ImageVersion::SemVer(
        semver::Version::parse("1.0.0").unwrap(),
    ));
    client.files.create_result(output_req).await?;
    Ok(hashes.sha256)
}

/// Upload a sample with no needle in any greppable source
///
/// Each call uploads a unique sample since the buffer is salted with a random
/// string.
///
/// # Arguments
///
/// * `client` - The client to upload this sample with
/// * `group` - The group to upload this sample too
async fn seed_blank_sample(client: &Thorium, group: &str) -> Result<String, Error> {
    // upload a sample whose greppable sources never contain our needle; salt the
    // buffer so every call uploads a distinct sample
    let file_req = SampleRequest::new_buffer(
        Buffer::new(format!("plain body {}", generators::gen_string(16))).name("plain.txt"),
        vec![group.to_owned()],
    )
    .description("a plain description");
    let hashes = client.files.create(file_req).await?;
    Ok(hashes.sha256)
}

/// Build a result file body that binary detection truncates after real progress
///
/// The body is a few hundred KiB of needle-free text followed by a NUL byte. The
/// text spans several searcher buffer fills, so earlier fills are searched and
/// counted before the fill with the NUL abandons the document; a NUL in the very
/// first fill would instead be pruned as pure binary noise.
///
/// # Arguments
///
/// * `prefix` - The text to plant at the very start of the body
fn truncated_body(prefix: &str) -> Vec<u8> {
    // start with our prefix so tests can plant a needle before the truncation
    let mut body = prefix.as_bytes().to_vec();
    // pad well past the searcher's buffer capacity so real bytes are searched first
    body.extend_from_slice("grep filler line\n".repeat(16_384).as_bytes());
    // end in a NUL byte so binary detection abandons this document
    body.push(0);
    body
}

/// Upload a sample whose only reportable document is truncated with no matches
///
/// The attached result file is truncated by binary detection after real bytes were
/// searched, and nothing in any source contains our needle, so this sample can only
/// ever come back as a hit-less truncated document.
///
/// Each call uploads a unique sample since the buffer is salted with a random
/// string.
///
/// # Arguments
///
/// * `client` - The client to upload this sample with
/// * `group` - The group to upload this sample too
async fn seed_truncated_sample(client: &Thorium, group: &str) -> Result<String, Error> {
    // upload a sample with no needle in any greppable source; salt the buffer so
    // every call uploads a distinct sample
    let file_req = SampleRequest::new_buffer(
        Buffer::new(format!("truncated body {}", generators::gen_string(16))).name("truncated.txt"),
        vec![group.to_owned()],
    )
    .description("a plain description");
    let hashes = client.files.create(file_req).await?;
    // attach a needle-free result file that binary detection truncates part way
    let output_req = OutputRequest::new(
        hashes.sha256.clone(),
        TOOL,
        "an inline result with no needle",
        OutputDisplayType::String,
    )
    .buffer(Buffer::new(truncated_body("no needle here\n")).name("truncated.bin"))
    .tool_version(ImageVersion::SemVer(
        semver::Version::parse("1.0.0").unwrap(),
    ));
    client.files.create_result(output_req).await?;
    Ok(hashes.sha256)
}

/// Greping a single sample finds our needle in every source it lives in
#[tokio::test]
async fn grep_sample_all_sources() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // grep this sample for our needle across every source
    let matches = client
        .files
        .grep_key(&sha256, GrepOpts::new(PATTERN))
        .await?;
    // make sure these matches are for the sample we greped
    is!(matches.key, sha256);
    // make sure we found our needle in this samples inline results
    contains_key!(matches.results, TOOL);
    is!(
        has_needle(matches.results[TOOL].values().flat_map(HashMap::values)),
        true
    );
    // make sure we found our needle in this samples result files
    contains_key!(matches.result_files, TOOL);
    is!(
        has_needle(
            matches.result_files[TOOL]
                .values()
                .flat_map(HashMap::values)
        ),
        true
    );
    // make sure we found our needle in this samples submission info
    is!(
        has_needle(matches.submissions.values().flat_map(HashMap::values)),
        true
    );
    // make sure we found our needle in this samples comments
    is!(has_needle(matches.comments.values()), true);
    // make sure we found our needle in this samples tags
    contains_key!(matches.tags, "GrepTag");
    is!(has_needle(matches.tags["GrepTag"].values()), true);
    Ok(())
}

/// Restricting the sources to submissions excludes every other source
#[tokio::test]
async fn grep_sample_only_submissions() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // grep this sample for our needle in only its submission info
    let opts = GrepOpts::new(PATTERN).source(GrepSource::Submissions);
    let matches = client.files.grep_key(&sha256, opts).await?;
    // make sure we still found our needle in this samples submission info
    is!(
        has_needle(matches.submissions.values().flat_map(HashMap::values)),
        true
    );
    // make sure every other source was skipped entirely
    is_empty!(matches.results);
    is_empty!(matches.result_files);
    is_empty!(matches.comments);
    is_empty!(matches.tags);
    is_empty!(matches.metadata);
    Ok(())
}

/// Dropping the result file source skips the result files in s3
#[tokio::test]
async fn grep_sample_without_result_files() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // grep this sample for our needle in only its inline results
    let opts = GrepOpts::new(PATTERN).source(GrepSource::Results);
    let matches = client.files.grep_key(&sha256, opts).await?;
    // make sure we still found our needle in this samples inline results
    contains_key!(matches.results, TOOL);
    // make sure this samples result files were never downloaded
    is_empty!(matches.result_files);
    Ok(())
}

/// Restricting the tools excludes results from every other tool
#[tokio::test]
async fn grep_sample_restricts_tools() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // attach a second tools result that also contains our needle
    let output_req = OutputRequest::new(
        sha256.clone(),
        OTHER_TOOL,
        format!("another tools result with {NEEDLE} in it"),
        OutputDisplayType::String,
    )
    .tool_version(ImageVersion::SemVer(
        semver::Version::parse("1.0.0").unwrap(),
    ));
    client.files.create_result(output_req).await?;
    // grep this sample for our needle in only the first tools results
    let opts = GrepOpts::new(PATTERN)
        .source(GrepSource::Results)
        .tool(TOOL);
    let matches = client.files.grep_key(&sha256, opts).await?;
    // make sure we found our needle in the tool we asked for
    contains_key!(matches.results, TOOL);
    // make sure the other tools results were never searched
    is!(matches.results.contains_key(OTHER_TOOL), false);
    Ok(())
}

/// A case insensitive grep matches a needle in a different case
#[tokio::test]
async fn grep_sample_case_insensitive() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // a lowercased pattern shouldn't match our mixed case needle on its own
    let opts = GrepOpts::new(PATTERN.to_lowercase()).source(GrepSource::Submissions);
    let matches = client.files.grep_key(&sha256, opts).await?;
    is_empty!(matches.submissions);
    // but it should match once we ignore case
    let opts = GrepOpts::new(PATTERN.to_lowercase())
        .source(GrepSource::Submissions)
        .case_insensitive();
    let matches = client.files.grep_key(&sha256, opts).await?;
    is!(
        has_needle(matches.submissions.values().flat_map(HashMap::values)),
        true
    );
    Ok(())
}

/// A pattern spanning a newline is rejected without multi line and matches with it
#[tokio::test]
async fn grep_sample_multi_line() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a sample to attach a multi line result file too
    let file_req =
        SampleRequest::new_buffer(Buffer::new("MultiLineGrepSample"), vec![group.clone()])
            .description("test file");
    let hashes = client.files.create(file_req).await?;
    // attach a result file whose needle can only be matched across a newline
    let output_req = OutputRequest::new(
        hashes.sha256.clone(),
        TOOL,
        "an inline result with no needle",
        OutputDisplayType::String,
    )
    .buffer(Buffer::new(format!("{NEEDLE} alpha\n{NEEDLE} omega\n")).name("multiline.txt"))
    .tool_version(ImageVersion::SemVer(
        semver::Version::parse("1.0.0").unwrap(),
    ));
    client.files.create_result(output_req).await?;
    // build a pattern that can only match across a newline
    let pattern = format!("{NEEDLE} alpha\n{NEEDLE} omega");
    // a single line search can never match across a newline so the pattern is rejected
    //
    // this is the same behavior as ripgrep without '-U'
    let check = client
        .files
        .grep_key(&hashes.sha256, GrepOpts::new(&pattern))
        .await;
    fail!(check, 400);
    // but a multi line search should find our needle across the newline
    let opts = GrepOpts::new(&pattern)
        .source(GrepSource::ResultFiles)
        .multi_line();
    let matches = client.files.grep_key(&hashes.sha256, opts).await?;
    // make sure we found our needle across the newline in our result file
    contains_key!(matches.result_files, TOOL);
    is!(
        has_needle(
            matches.result_files[TOOL]
                .values()
                .flat_map(HashMap::values)
        ),
        true
    );
    Ok(())
}

/// A low max matches truncates the search instead of erroring
#[tokio::test]
async fn grep_sample_max_matches_truncates() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // grep this sample with room for a single match, keeping the hit-less
    // documents our budget cut off so the truncation is observable
    let opts = GrepOpts::new(PATTERN).max_matches(1).with_truncated();
    let matches = client.files.grep_key(&sha256, opts).await?;
    // make sure we got exactly the one match we left room for
    is!(total_matches(&matches), 1);
    // make sure at least one document was flagged as truncated by our match budget
    is!(
        any_truncated_for(&matches, TruncationReason::MatchBudget),
        true
    );
    Ok(())
}

/// A result file larger than the max document size is searched up to that limit
///
/// This is the memory safety check. A naive implementation would buffer the entire result
/// file in the API before searching it, so the API staying up at all is part of what this
/// asserts. Our needle is planted in the first few bytes so it lands inside the searched
/// prefix, and the filler after it pushes the file past the default 64 MiB document limit
/// so the truncation path is actually exercised.
#[tokio::test]
async fn grep_sample_oversized_result_file() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // the max number of bytes the API will search in a single document
    let max = test_utilities::CONF.thorium.grep.max_document_size.as_u64();
    // plant our needle at the very start of this result file
    let mut body = format!("{NEEDLE}\n");
    // pad this result file past our document limit with printable filler so binary
    // detection doesn't quit before we reach the limit we are trying to trip
    let filler = "grep filler line\n";
    let lines = usize::try_from(max).unwrap_or(usize::MAX) / filler.len() + 1024;
    body.push_str(&filler.repeat(lines));
    // upload a sample to attach this large result file too
    let file_req = SampleRequest::new_buffer(
        Buffer::new("OversizedGrepResultFileSample"),
        vec![group.clone()],
    )
    .description("test file");
    let hashes = client.files.create(file_req).await?;
    // attach our large result file to this sample
    let output_req = OutputRequest::new(
        hashes.sha256.clone(),
        TOOL,
        "an inline result with no needle",
        OutputDisplayType::String,
    )
    .buffer(Buffer::new(body).name("oversized.txt"))
    .tool_version(ImageVersion::SemVer(
        semver::Version::parse("1.0.0").unwrap(),
    ));
    client.files.create_result(output_req).await?;
    // grep this samples result files for our needle
    let opts = GrepOpts::new(PATTERN).source(GrepSource::ResultFiles);
    let matches = client.files.grep_key(&hashes.sha256, opts).await?;
    // make sure we found the needle that was in the prefix we searched
    contains_key!(matches.result_files, TOOL);
    let docs: Vec<&GrepDocumentMatches> = matches.result_files[TOOL]
        .values()
        .flat_map(HashMap::values)
        .collect();
    is!(has_needle(docs.iter().copied()), true);
    // make sure we searched right up to our document limit but no further
    let searched: u64 = docs.iter().map(|doc| doc.searched).sum();
    is!((searched >= max), true);
    is!((searched <= max + 1), true);
    // make sure this document was reported back as truncated by our document limit
    is!(
        docs.iter()
            .any(|doc| doc.truncated == Some(TruncationReason::MaxDocumentSize)),
        true
    );
    Ok(())
}

/// A binary result file that was never searched is pruned from the response
///
/// Binary detection abandons a document at its first NUL byte. When that happens
/// before anything was searched the document can never match any text query, so it
/// is dropped from the response instead of coming back as an empty truncated entry.
#[tokio::test]
async fn grep_sample_binary_result_file_is_pruned() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a sample to attach our binary result file too
    let file_req = SampleRequest::new_buffer(
        Buffer::new(format!(
            "BinaryGrepResultFileSample {}",
            generators::gen_string(16)
        )),
        vec![group.clone()],
    )
    .description("test file");
    let hashes = client.files.create(file_req).await?;
    // build a fake png whose header contains NULs so binary detection quits before
    // anything is searched, with our needle after them to prove it is never reported
    let mut body = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0DIHDR".to_vec();
    body.extend_from_slice(NEEDLE.as_bytes());
    // attach our binary result file with an inline result that holds our needle
    let output_req = OutputRequest::new(
        hashes.sha256.clone(),
        TOOL,
        format!("an inline result with {NEEDLE} in it"),
        OutputDisplayType::String,
    )
    .buffer(Buffer::new(body).name("binary.png"))
    .tool_version(ImageVersion::SemVer(
        semver::Version::parse("1.0.0").unwrap(),
    ));
    client.files.create_result(output_req).await?;
    // grep this samples results and result files for our needle
    let opts = GrepOpts::new(PATTERN)
        .source(GrepSource::Results)
        .source(GrepSource::ResultFiles);
    let matches = client.files.grep_key(&hashes.sha256, opts).await?;
    // make sure we still found the needle in our inline result
    contains_key!(matches.results, TOOL);
    is!(
        has_needle(matches.results[TOOL].values().flat_map(HashMap::values)),
        true
    );
    // make sure our binary result file was pruned from the response entirely
    is_empty!(matches.result_files);
    Ok(())
}

/// An invalid regex is rejected as a bad request instead of a 500
#[tokio::test]
async fn grep_invalid_pattern() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // an unclosed group is not a valid regex
    let check = client
        .files
        .grep_key(&sha256, GrepOpts::new("CornGrepNeedle(["))
        .await;
    fail!(check, 400);
    // the corpus route should reject it the same way
    let check = client.files.grep(GrepOpts::new("CornGrepNeedle([")).await;
    fail!(check, 400);
    Ok(())
}

/// A user without access to a samples group gets no hits for it
#[tokio::test]
async fn grep_sample_respects_groups() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group for our hidden sample and one for our other user
    let mut groups = generators::groups(2, &client).await?;
    let hidden_group = groups.remove(0).name;
    let other_group = groups.remove(0).name;
    // seed a hidden sample with our needle in every source
    let hidden = seed_sample(&client, &hidden_group).await?;
    // seed a visible sample in the other group as a positive control
    let visible = seed_sample(&client, &other_group).await?;
    // build a second user that is only in the other group so their corpus grep
    // exercises real listing and scoping instead of the no groups short circuit
    let other = generators::client_in_groups(&client, &[other_group.clone()]).await?;
    // make sure this user cannot grep a sample they cannot see
    let check = other.files.grep_key(&hidden, GrepOpts::new(PATTERN)).await;
    fail!(check, 404);
    // a corpus grep should find their own groups sample but never the hidden one
    let page = other.files.grep(GrepOpts::new(PATTERN)).await?;
    is!(page.data.iter().any(|matches| matches.key == visible), true);
    is!(page.data.iter().any(|matches| matches.key == hidden), false);
    Ok(())
}

/// Group scoping hides sample tags and comments outside the requested groups
#[tokio::test]
async fn grep_sample_groups_scope_sources() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create two groups to scope data across
    let mut groups = generators::groups(2, &client).await?;
    let scoped = groups.remove(0).name;
    let other_group = groups.remove(0).name;
    // upload a sample into both groups with our needle in its submission info
    let file_req = SampleRequest::new_buffer(
        Buffer::new(format!("sample body {}", generators::gen_string(16)))
            .name(format!("{NEEDLE}.txt")),
        vec![scoped.clone(), other_group.clone()],
    );
    let sha256 = client.files.create(file_req).await?.sha256;
    // tag this sample with our needle in only the scoped group
    let mut tag_req = TagRequest::<Sample>::default().group(&scoped);
    tag_req.add_ref("GrepTag", NEEDLE);
    client.files.tag(&sha256, &tag_req).await?;
    // comment on this sample with our needle in only the scoped group
    let comment_req =
        CommentRequest::new(&sha256, format!("a comment with {NEEDLE} in it")).group(&scoped);
    client.files.comment(comment_req).await?;
    // a grep scoped to the other group must not see the scoped tag or comment
    let opts = GrepOpts::new(PATTERN).group(&other_group);
    let matches = client.files.grep_key(&sha256, opts).await?;
    is_empty!(matches.tags);
    is_empty!(matches.comments);
    // but the submission info shared with both groups should still match
    is!(
        has_needle(matches.submissions.values().flat_map(HashMap::values)),
        true
    );
    // a grep scoped to the right group should see the tag and comment
    let opts = GrepOpts::new(PATTERN).group(&scoped);
    let matches = client.files.grep_key(&sha256, opts).await?;
    contains_key!(matches.tags, "GrepTag");
    is!(has_needle(matches.comments.values()), true);
    // the corpus route should scope its sources the same way
    let opts = GrepOpts::new(PATTERN).group(&other_group);
    let page = client.files.grep(opts).await?;
    let corpus = page
        .data
        .iter()
        .find(|matches| matches.key == sha256)
        .ok_or_else(|| Error::new("the corpus grep did not return our seeded sample"))?;
    is_empty!(corpus.tags);
    is_empty!(corpus.comments);
    is!(
        has_needle(corpus.submissions.values().flat_map(HashMap::values)),
        true
    );
    Ok(())
}

/// The corpus route returns the same matches as the per sample route
#[tokio::test]
async fn grep_files_corpus_matches_single() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // grep every sample in our new group, which only has the one sample in it
    let opts = GrepOpts::new(PATTERN).group(&group);
    let page = client.files.grep(opts).await?;
    // find the matches for the sample we seeded
    let corpus = page
        .data
        .iter()
        .find(|matches| matches.key == sha256)
        .ok_or_else(|| Error::new("the corpus grep did not return our seeded sample"))?;
    // grep this single sample directly
    let single = client
        .files
        .grep_key(&sha256, GrepOpts::new(PATTERN))
        .await?;
    // make sure both routes found the same number of matches
    is!(total_matches(corpus), total_matches(&single));
    Ok(())
}

/// A corpus grep can be continued with the cursor it returns
#[tokio::test]
async fn grep_files_cursor_continues() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed two samples that both contain our needle
    let first = seed_sample(&client, &group).await?;
    let second = seed_sample(&client, &group).await?;
    // both samples should be distinct so the cursor has two items to walk
    is_not!(first, second);
    // walk our cursor a single sample at a time, collecting every key we see
    let mut keys = Vec::new();
    // start with only the first page of a single sample
    let mut opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .limit(1);
    // keep paging until our cursor is exhausted or we have made too many requests
    for _ in 0..10 {
        // grep the next page of samples
        let page = client.files.grep(opts).await?;
        // every sample matches, so no page should ever scan past its data limit;
        // only the final probe that discovers the corpus is exhausted may be empty
        is!((page.data.len() <= 1), true);
        // save the keys we found matches for in this page
        keys.extend(page.data.iter().map(|matches| matches.key.clone()));
        // stop once our cursor has been exhausted
        let Some(cursor) = page.cursor else {
            break;
        };
        // continue this cursor for the next page
        opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Submissions)
            .limit(1)
            .cursor(cursor);
    }
    // make sure walking the cursor found both of our samples
    is!(keys.contains(&first), true);
    is!(keys.contains(&second), true);
    Ok(())
}

/// Continuing a cursor id that does not exist is a not found rather than a 500
#[tokio::test]
async fn grep_files_unknown_cursor() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // try to continue a cursor that was never created
    let opts = GrepOpts::new(PATTERN).cursor(uuid::Uuid::new_v4());
    let check = client.files.grep(opts).await;
    fail!(check, 404);
    Ok(())
}

/// A grep cursor can only be continued by the user that created it
#[tokio::test]
async fn grep_files_cursor_rejects_other_user() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed two samples so our corpus grep has items left to search after one page
    let first = seed_sample(&client, &group).await?;
    let second = seed_sample(&client, &group).await?;
    // both samples should be distinct so the cursor has two items to walk
    is_not!(first, second);
    // start a corpus grep that leaves an item unsearched so a cursor is returned
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .limit(1);
    let page = client.files.grep(opts).await?;
    // make sure we got a cursor another user can try to continue
    let Some(cursor) = page.cursor else {
        return Err(Error::new("the corpus grep did not return a cursor"));
    };
    // build a second user and add them to our group so only cursor ownership differs
    let other = generators::client(&client).await?;
    let username = other.users.info().await?.username;
    let update = GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(&username));
    client.groups.update(&group, &update).await?;
    // make sure this user cannot continue a cursor they did not create
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .limit(1)
        .cursor(cursor);
    let check = other.files.grep(opts).await;
    fail!(check, 404);
    // a fresh grep with this users default groups should still find our samples
    let opts = GrepOpts::new(PATTERN).source(GrepSource::Submissions);
    let page = other.files.grep(opts).await?;
    is!(page.data.iter().any(|matches| matches.key == first), true);
    is!(page.data.iter().any(|matches| matches.key == second), true);
    // make sure the cursor is still continuable by the user that created it
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .limit(1)
        .cursor(cursor);
    client.files.grep(opts).await?;
    Ok(())
}

/// The start param bounds which samples a corpus grep searches
#[tokio::test]
async fn grep_files_time_window() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // a window starting now should include our fresh sample
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions);
    let page = client.files.grep(opts).await?;
    is!(page.data.iter().any(|matches| matches.key == sha256), true);
    // a window starting before our sample was uploaded should exclude it
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .start(Utc::now() - Duration::days(1));
    let page = client.files.grep(opts).await?;
    is!(page.data.iter().any(|matches| matches.key == sha256), false);
    Ok(())
}

/// The max matches budget is shared across every item in a corpus grep
#[tokio::test]
async fn grep_files_max_matches_spans_corpus() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed two samples that both contain many copies of our needle
    seed_sample(&client, &group).await?;
    seed_sample(&client, &group).await?;
    // grep our whole corpus with room for a single match across every item,
    // keeping the hit-less documents our budget cut off
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .max_matches(1)
        .with_truncated();
    let page = client.files.grep(opts).await?;
    // make sure we got exactly the one match we left room for across all items
    let total: usize = page.data.iter().map(total_matches).sum();
    is!(total, 1);
    // make sure at least one document was flagged as truncated by our match budget
    is!(
        page.data
            .iter()
            .any(|matches| any_truncated_for(matches, TruncationReason::MatchBudget)),
        true
    );
    // the page metadata should count our one found document and the documents
    // the budget cut off before they found anything
    let stats = page
        .metadata
        .ok_or_else(|| Error::new("the corpus grep did not return metadata"))?;
    is!(stats.found, 1);
    is!((stats.truncated >= 1), true);
    is!(stats.errors, 0);
    // exhausting our budget mid page should hand back a cursor for the unsearched items
    is!(page.cursor.is_some(), true);
    Ok(())
}

/// Exhausting the match budget mid item resumes without duplicates or losses
#[tokio::test]
async fn grep_files_budget_resume_returns_every_match_once() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // collect the ground truth from the single sample route with a roomy budget
    let single = client
        .files
        .grep_key(&sha256, GrepOpts::new(PATTERN))
        .await?;
    let mut expected = match_identities(&single);
    // our sample must have more matches then one page's budget so it gets cut off
    is!((expected.len() >= 4), true);
    // walk a corpus grep whose budget dies inside our sample on every page
    let mut found = Vec::new();
    let mut opts = GrepOpts::new(PATTERN).group(&group).max_matches(3);
    for _ in 0..10 {
        // grep the next page of samples
        let page = client.files.grep(opts).await?;
        // every page should respect our match budget
        let total: usize = page.data.iter().map(total_matches).sum();
        is!((total <= 3), true);
        // save the identity of every match in this page
        found.extend(page.data.iter().flat_map(match_identities));
        // stop once our cursor has been exhausted
        let Some(cursor) = page.cursor else {
            break;
        };
        // continue this cursor for the next page
        opts = GrepOpts::new(PATTERN)
            .group(&group)
            .max_matches(3)
            .cursor(cursor);
    }
    // no match should ever be returned twice
    let mut deduped = found.clone();
    deduped.sort();
    deduped.dedup();
    is!(deduped.len(), found.len());
    // and no match should ever be lost
    expected.sort();
    is!(deduped, expected);
    Ok(())
}

/// A single item with more matches than the whole budget spans multiple pages
#[tokio::test]
async fn grep_files_single_item_exceeds_whole_budget() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let sha256 = seed_sample(&client, &group).await?;
    // collect the ground truth from the single sample route with a roomy budget
    let single = client
        .files
        .grep_key(&sha256, GrepOpts::new(PATTERN))
        .await?;
    let mut expected = match_identities(&single);
    // walk a corpus grep that can only ever return one match per page
    let mut found = Vec::new();
    let mut pages_with_key = 0;
    let mut opts = GrepOpts::new(PATTERN).group(&group).max_matches(1);
    for _ in 0..30 {
        // grep the next page of samples
        let page = client.files.grep(opts).await?;
        // count the pages that returned matches for our sample
        pages_with_key += usize::from(page.data.iter().any(|matches| matches.key == sha256));
        // save the identity of every match in this page
        found.extend(page.data.iter().flat_map(match_identities));
        // stop once our cursor has been exhausted
        let Some(cursor) = page.cursor else {
            break;
        };
        // continue this cursor for the next page
        opts = GrepOpts::new(PATTERN)
            .group(&group)
            .max_matches(1)
            .cursor(cursor);
    }
    // our sample should span several pages since each page holds one match
    is!((pages_with_key >= 3), true);
    // no match should ever be returned twice
    let mut deduped = found.clone();
    deduped.sort();
    deduped.dedup();
    is!(deduped.len(), found.len());
    // and no match should ever be lost
    expected.sort();
    is!(deduped, expected);
    Ok(())
}

/// Budget exhaustion across several items never duplicates or drops a match
#[tokio::test]
async fn grep_files_budget_spans_items_without_dup_or_loss() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed three samples with our needle in every source
    let mut expected = Vec::new();
    for _ in 0..3 {
        let sha256 = seed_sample(&client, &group).await?;
        // collect this samples ground truth from the single sample route
        let single = client
            .files
            .grep_key(&sha256, GrepOpts::new(PATTERN))
            .await?;
        expected.extend(match_identities(&single));
    }
    // walk a corpus grep whose budget dies both inside and between items
    let mut found = Vec::new();
    let mut opts = GrepOpts::new(PATTERN).group(&group).max_matches(2);
    for _ in 0..30 {
        // grep the next page of samples
        let page = client.files.grep(opts).await?;
        // save the identity of every match in this page
        found.extend(page.data.iter().flat_map(match_identities));
        // stop once our cursor has been exhausted
        let Some(cursor) = page.cursor else {
            break;
        };
        // continue this cursor for the next page
        opts = GrepOpts::new(PATTERN)
            .group(&group)
            .max_matches(2)
            .cursor(cursor);
    }
    // no match should ever be returned twice
    let mut deduped = found.clone();
    deduped.sort();
    deduped.dedup();
    is!(deduped.len(), found.len());
    // and no match should ever be lost
    expected.sort();
    is!(deduped, expected);
    Ok(())
}

/// A pending item that is deleted mid cursor is skipped instead of wedging the cursor
#[tokio::test]
async fn grep_files_cursor_skips_deleted_item() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed two samples that both contain our needle
    let first = seed_sample(&client, &group).await?;
    let second = seed_sample(&client, &group).await?;
    // search a single sample so the other is left pending on the cursor
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .limit(1);
    let page = client.files.grep(opts).await?;
    // make sure we got a cursor with the unsearched sample pending
    let Some(cursor) = page.cursor else {
        return Err(Error::new("the corpus grep did not return a cursor"));
    };
    // figure out which sample is still pending on this cursor
    let searched = page
        .data
        .first()
        .map(|matches| matches.key.clone())
        .ok_or_else(|| Error::new("the first page returned no matches"))?;
    let pending = if searched == first { &second } else { &first };
    // delete every submission for our pending sample so it no longer exists
    let sample = client.files.get(pending).await?;
    for sub in &sample.submissions {
        client
            .files
            .delete(pending, &sub.id, &FileDeleteOpts::default())
            .await?;
    }
    // continuing this cursor should skip the deleted sample instead of erroring
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .limit(1)
        .cursor(cursor);
    let page = client.files.grep(opts).await?;
    // the deleted sample should never come back as a match
    is!(
        page.data.iter().any(|matches| matches.key == *pending),
        false
    );
    Ok(())
}

/// A single page scans past items without matches instead of returning empty pages
#[tokio::test]
async fn grep_files_scans_past_empty_candidates() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed two samples that both contain our needle first so the newest first
    // corpus walk has to scan past every blank sample to reach them
    let first = seed_sample(&client, &group).await?;
    let second = seed_sample(&client, &group).await?;
    // bury our matching samples behind a corpus of samples with no matches at all
    let mut blanks = Vec::new();
    for _ in 0..15 {
        blanks.push(seed_blank_sample(&client, &group).await?);
    }
    // a single default page should scan past every blank sample and return matching
    // samples instead of the empty page an item scanned bound would have returned
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions);
    let page = client.files.grep(opts).await?;
    is!(page.data.is_empty(), false);
    // collect every key this walk returns, starting with the first page
    let mut keys = page
        .data
        .iter()
        .map(|matches| matches.key.clone())
        .collect::<Vec<String>>();
    // walk any leftover pages to exhaustion
    let mut cursor = page.cursor;
    for _ in 0..10 {
        // stop once our cursor has been exhausted
        let Some(id) = cursor else {
            break;
        };
        // grep the next page of samples
        let opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Submissions)
            .cursor(id);
        let page = client.files.grep(opts).await?;
        // save the keys we found matches for in this page
        keys.extend(page.data.iter().map(|matches| matches.key.clone()));
        cursor = page.cursor;
    }
    // make sure this walk found both of our matching samples
    is!(keys.contains(&first), true);
    is!(keys.contains(&second), true);
    // no blank sample should ever come back as a match
    is!(blanks.iter().any(|blank| keys.contains(blank)), false);
    Ok(())
}

/// The scan time budget caps a page while the cursor stays lossless
#[tokio::test]
async fn grep_files_scan_time_caps_request() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed three samples that all contain our needle
    let seeded = vec![
        seed_sample(&client, &group).await?,
        seed_sample(&client, &group).await?,
        seed_sample(&client, &group).await?,
    ];
    // walk our cursor with a tiny time budget so every page stops after the one
    // item its progress floor guarantees
    let mut pages = 0;
    let mut identities = Vec::new();
    let mut opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .scan_time(1);
    // keep paging until our cursor is exhausted or we have made too many requests
    for _ in 0..30 {
        // grep the next page of samples
        let page = client.files.grep(opts).await?;
        pages += 1;
        // collect the identity of every match in this page
        for matches in &page.data {
            identities.extend(match_identities(matches));
        }
        // stop once our cursor has been exhausted
        let Some(cursor) = page.cursor else {
            break;
        };
        // continue this cursor for the next page
        opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Submissions)
            .scan_time(1)
            .cursor(cursor);
    }
    // the time budget should force this walk across at least one page per sample
    is!((pages >= 3), true);
    // every match should have been returned exactly once
    let mut deduped = identities.clone();
    deduped.sort();
    deduped.dedup();
    is!(identities.len(), deduped.len());
    // and every seeded sample should have been found
    for sha256 in &seeded {
        is!(
            identities
                .iter()
                .any(|(path, _, _)| path.starts_with(sha256)),
            true
        );
    }
    Ok(())
}

/// The idle cutoff returns a partially full page once matches stop flowing
#[tokio::test]
async fn grep_files_scan_idle_returns_early() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed blank samples first so they are older then our matching sample
    for _ in 0..5 {
        seed_blank_sample(&client, &group).await?;
    }
    // seed our matching sample last so the newest first corpus walk scans it first
    let sha256 = seed_sample(&client, &group).await?;
    // grep with a tiny idle cutoff so the page stops shortly after our match
    // instead of walking the rest of the corpus
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .scan_idle(1);
    let page = client.files.grep(opts).await?;
    // our matching sample should be the only item in this page
    is!(page.data.len(), 1);
    is!(page.data[0].key.clone(), sha256);
    // the idle cutoff should leave the rest of the corpus for a later page
    is!(page.cursor.is_some(), true);
    // walking the leftover pages to exhaustion should find nothing else
    let mut cursor = page.cursor;
    for _ in 0..10 {
        // stop once our cursor has been exhausted
        let Some(id) = cursor else {
            break;
        };
        // grep the next page of samples
        let opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Submissions)
            .cursor(id);
        let page = client.files.grep(opts).await?;
        // the blank samples should never come back as matches
        is_empty!(page.data);
        cursor = page.cursor;
    }
    Ok(())
}

/// The idle cutoff only arms once a page has found at least one match
#[tokio::test]
async fn grep_files_idle_unarmed_scans_to_exhaustion() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a corpus with no matches at all
    for _ in 0..5 {
        seed_blank_sample(&client, &group).await?;
    }
    // a page with a tiny idle cutoff should still scan the whole corpus since the
    // cutoff never arms without a match
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .scan_idle(1);
    let page = client.files.grep(opts).await?;
    // the whole corpus should be exhausted in one empty page
    is_empty!(page.data);
    is!(page.exhausted(), true);
    Ok(())
}

/// One big page returns the same matches as many time capped pages
#[tokio::test]
async fn grep_files_time_budget_equals_small_pages() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a mixed corpus of blank and matching samples
    for _ in 0..3 {
        seed_blank_sample(&client, &group).await?;
    }
    for _ in 0..3 {
        seed_sample(&client, &group).await?;
    }
    // collect every match identity from a default page walk
    let mut big = Vec::new();
    let mut opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions);
    for _ in 0..30 {
        // grep the next page of samples
        let page = client.files.grep(opts).await?;
        // collect the identity of every match in this page
        for matches in &page.data {
            big.extend(match_identities(matches));
        }
        // stop once our cursor has been exhausted
        let Some(cursor) = page.cursor else {
            break;
        };
        // continue this cursor for the next page
        opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Submissions)
            .cursor(cursor);
    }
    // collect every match identity from a time starved walk of the same corpus
    let mut small = Vec::new();
    let mut opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Submissions)
        .scan_time(1);
    for _ in 0..30 {
        // grep the next page of samples
        let page = client.files.grep(opts).await?;
        // collect the identity of every match in this page
        for matches in &page.data {
            small.extend(match_identities(matches));
        }
        // stop once our cursor has been exhausted
        let Some(cursor) = page.cursor else {
            break;
        };
        // continue this cursor for the next page
        opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Submissions)
            .scan_time(1)
            .cursor(cursor);
    }
    // both walks should have returned exactly the same matches
    big.sort();
    small.sort();
    is!(big, small);
    Ok(())
}

/// A page scans past hit-less truncated documents instead of returning early
///
/// This is the regression test for truncated documents ending scans early: a page
/// used to treat a hit-less truncated document like a found match, arming its idle
/// cutoff and returning with zero actual hits before ever reaching the real match.
#[tokio::test]
async fn grep_files_scans_past_truncated_docs() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed our matching sample first so the newest first corpus walk has to scan
    // past every truncated sample to reach it
    let sha256 = seed_sample(&client, &group).await?;
    // bury our matching sample behind samples that only have truncated documents
    let mut truncated = Vec::new();
    for _ in 0..5 {
        truncated.push(seed_truncated_sample(&client, &group).await?);
    }
    // grep with a tiny idle cutoff; only an actual match may arm it, so this page
    // must scan past every truncated sample and still find our match
    let opts = GrepOpts::new(PATTERN).group(&group).scan_idle(1);
    let page = client.files.grep(opts).await?;
    // our matching sample should be the only item in this page since hit-less
    // truncated documents are dropped by default
    is!(page.data.len(), 1);
    is!(page.data[0].key.clone(), sha256);
    // the page metadata should still count every truncated document we scanned past
    let stats = page
        .metadata
        .ok_or_else(|| Error::new("the corpus grep did not return metadata"))?;
    is!((stats.found >= 1), true);
    is!(stats.truncated, 5);
    is!(stats.errors, 0);
    // walking any leftover pages to exhaustion should find nothing else
    let mut cursor = page.cursor;
    for _ in 0..10 {
        // stop once our cursor has been exhausted
        let Some(id) = cursor else {
            break;
        };
        // grep the next page of samples
        let opts = GrepOpts::new(PATTERN).group(&group).cursor(id);
        let page = client.files.grep(opts).await?;
        // the truncated samples should never come back as matches
        is_empty!(page.data);
        cursor = page.cursor;
    }
    Ok(())
}

/// The with truncated param returns the hit-less documents dropped by default
#[tokio::test]
async fn grep_files_with_truncated_includes_hitless_docs() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample whose only reportable document is truncated with no matches
    let sha256 = seed_truncated_sample(&client, &group).await?;
    // a default corpus grep should drop the hit-less truncated document entirely
    let opts = GrepOpts::new(PATTERN).group(&group);
    let page = client.files.grep(opts).await?;
    is_empty!(page.data);
    is!(page.exhausted(), true);
    // but its metadata should still count the truncated document we scanned
    is!(
        page.metadata,
        Some(GrepStats {
            found: 0,
            truncated: 1,
            errors: 0,
        })
    );
    // asking for truncated documents should return the same item with its document
    let opts = GrepOpts::new(PATTERN).group(&group).with_truncated();
    let page = client.files.grep(opts).await?;
    is!(page.data.len(), 1);
    is!(page.data[0].key.clone(), sha256);
    // make sure the document came back truncated by binary detection with no matches
    contains_key!(page.data[0].result_files, TOOL);
    let docs: Vec<&GrepDocumentMatches> = page.data[0].result_files[TOOL]
        .values()
        .flat_map(HashMap::values)
        .collect();
    is!(
        docs.iter()
            .all(|doc| doc.truncated == Some(TruncationReason::Binary)),
        true
    );
    is!(docs.iter().all(|doc| doc.matches.is_empty()), true);
    is!(docs.iter().all(|doc| doc.searched > 0), true);
    // the single item route should drop and return the same document by the same flag
    let matches = client
        .files
        .grep_key(&sha256, GrepOpts::new(PATTERN))
        .await?;
    is!(matches.is_empty(), true);
    let matches = client
        .files
        .grep_key(&sha256, GrepOpts::new(PATTERN).with_truncated())
        .await?;
    contains_key!(matches.result_files, TOOL);
    Ok(())
}

/// Truncated documents never arm the idle cutoff even when they are returned
#[tokio::test]
async fn grep_files_idle_not_armed_by_truncated_items() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a corpus with only hit-less truncated documents
    let mut truncated = Vec::new();
    for _ in 0..5 {
        truncated.push(seed_truncated_sample(&client, &group).await?);
    }
    // a page with a tiny idle cutoff should still scan the whole corpus since
    // truncated documents never arm the cutoff, even when they are returned
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .scan_idle(1)
        .with_truncated();
    let page = client.files.grep(opts).await?;
    // the whole corpus should come back in one exhausted page
    is!(page.data.len(), 5);
    is!(page.exhausted(), true);
    // every seeded sample should be in this page
    for sha256 in &truncated {
        is!(page.data.iter().any(|matches| matches.key == *sha256), true);
    }
    // the page metadata should count every truncated document and no found ones
    is!(
        page.metadata,
        Some(GrepStats {
            found: 0,
            truncated: 5,
            errors: 0,
        })
    );
    Ok(())
}

/// The page metadata counts found and truncated documents, overlapping on both
#[tokio::test]
async fn grep_files_metadata_counts() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // seed a sample with our needle in every source
    let found = seed_sample(&client, &group).await?;
    // seed two samples whose only reportable documents are hit-less and truncated
    seed_truncated_sample(&client, &group).await?;
    seed_truncated_sample(&client, &group).await?;
    // seed a sample whose result file finds our needle before being truncated, so
    // one document lands in both the found and truncated buckets
    let file_req = SampleRequest::new_buffer(
        Buffer::new(format!("overlap body {}", generators::gen_string(16))).name("overlap.txt"),
        vec![group.clone()],
    )
    .description("a plain description");
    let overlap = client.files.create(file_req).await?.sha256;
    let output_req = OutputRequest::new(
        overlap.clone(),
        TOOL,
        "an inline result with no needle",
        OutputDisplayType::String,
    )
    .buffer(Buffer::new(truncated_body(&format!("{NEEDLE} early\n"))).name("overlap.bin"))
    .tool_version(ImageVersion::SemVer(
        semver::Version::parse("1.0.0").unwrap(),
    ));
    client.files.create_result(output_req).await?;
    // grep this whole corpus in one default page
    let opts = GrepOpts::new(PATTERN).group(&group);
    let page = client.files.grep(opts).await?;
    // only the items with actual matches should come back by default
    is!(page.data.len(), 2);
    is!(page.data.iter().any(|matches| matches.key == found), true);
    is!(page.data.iter().any(|matches| matches.key == overlap), true);
    // the overlap document should come back with its matches and its truncation
    let matches = page
        .data
        .iter()
        .find(|matches| matches.key == overlap)
        .ok_or_else(|| Error::new("the corpus grep did not return our overlap sample"))?;
    contains_key!(matches.result_files, TOOL);
    let docs: Vec<&GrepDocumentMatches> = matches.result_files[TOOL]
        .values()
        .flat_map(HashMap::values)
        .collect();
    is!(has_needle(docs.iter().copied()), true);
    is!(
        docs.iter()
            .any(|doc| doc.truncated == Some(TruncationReason::Binary)),
        true
    );
    // the page metadata should count the overlap document in both buckets: our
    // fully searched sample only adds found documents, the two truncated samples
    // only add truncated ones, and the overlap document adds one of each
    let stats = page
        .metadata
        .ok_or_else(|| Error::new("the corpus grep did not return metadata"))?;
    is!((stats.found >= 2), true);
    is!(stats.truncated, 3);
    is!(stats.errors, 0);
    Ok(())
}

/// Greping a single repo finds our needle in its submission info and tags
#[tokio::test]
async fn grep_repo_all_sources() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a repo tagged with our needle
    let url = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(
        &url,
        vec![group.clone()],
        Some(RepoCheckout::branch("main")),
    )
    .tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    // attach a result to this repo with our needle in it
    let output_req = OutputRequest::new(
        url.clone(),
        TOOL,
        format!("a repo result with {NEEDLE} in it"),
        OutputDisplayType::String,
    )
    .buffer(Buffer::new(format!("repo result file {NEEDLE}\n")).name("grep.txt"))
    .tool_version(ImageVersion::SemVer(
        semver::Version::parse("1.0.0").unwrap(),
    ));
    client.repos.create_result(output_req).await?;
    // grep this repo for our needle across every source
    let matches = client.repos.grep_key(&url, GrepOpts::new(PATTERN)).await?;
    // make sure these matches are for the repo we greped
    is!(matches.key, url);
    // make sure we found our needle in this repos tags
    contains_key!(matches.tags, "GrepTag");
    is!(has_needle(matches.tags["GrepTag"].values()), true);
    // make sure we found our needle in this repos inline results
    contains_key!(matches.results, TOOL);
    is!(
        has_needle(matches.results[TOOL].values().flat_map(HashMap::values)),
        true
    );
    // make sure we found our needle in this repos result files
    contains_key!(matches.result_files, TOOL);
    is!(
        has_needle(
            matches.result_files[TOOL]
                .values()
                .flat_map(HashMap::values)
        ),
        true
    );
    Ok(())
}

/// Restricting a repo grep to tags excludes its results
#[tokio::test]
async fn grep_repo_only_tags() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a repo tagged with our needle
    let url = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(&url, vec![group.clone()], None).tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    // attach a result to this repo with our needle in it
    let output_req = OutputRequest::new(
        url.clone(),
        TOOL,
        format!("a repo result with {NEEDLE} in it"),
        OutputDisplayType::String,
    )
    .tool_version(ImageVersion::SemVer(
        semver::Version::parse("1.0.0").unwrap(),
    ));
    client.repos.create_result(output_req).await?;
    // grep this repo for our needle in only its tags
    let opts = GrepOpts::new(PATTERN).source(GrepSource::Tags);
    let matches = client.repos.grep_key(&url, opts).await?;
    // make sure we still found our needle in this repos tags
    contains_key!(matches.tags, "GrepTag");
    // make sure this repos results were never searched
    is_empty!(matches.results);
    is_empty!(matches.result_files);
    Ok(())
}

/// The repo corpus route returns the same matches as the per repo route
#[tokio::test]
async fn grep_repos_corpus_matches_single() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a repo tagged with our needle
    let url = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(&url, vec![group.clone()], None).tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    // grep every repo in our new group, which only has the one repo in it
    let opts = GrepOpts::new(PATTERN).group(&group);
    let page = client.repos.grep(opts).await?;
    // find the matches for the repo we created
    let corpus = page
        .data
        .iter()
        .find(|matches| matches.key == url)
        .ok_or_else(|| Error::new("the corpus grep did not return our new repo"))?;
    // grep this single repo directly
    let single = client.repos.grep_key(&url, GrepOpts::new(PATTERN)).await?;
    // make sure both routes found the same number of matches
    is!(total_matches(corpus), total_matches(&single));
    Ok(())
}

/// A repo corpus grep can be continued with the cursor it returns
#[tokio::test]
async fn grep_repos_cursor_continues() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create two repos that are both tagged with our needle
    let first = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(&first, vec![group.clone()], None).tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    let second = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(&second, vec![group.clone()], None).tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    // both repos should be distinct so the cursor has two items to walk
    is_not!(first, second);
    // walk our cursor a single repo at a time, collecting every key we see
    let mut keys = Vec::new();
    // start with only the first page of a single repo
    let mut opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Tags)
        .limit(1);
    // keep paging until our cursor is exhausted or we have made too many requests
    for _ in 0..10 {
        // grep the next page of repos
        let page = client.repos.grep(opts).await?;
        // save the keys we found matches for in this page
        keys.extend(page.data.iter().map(|matches| matches.key.clone()));
        // stop once our cursor has been exhausted
        let Some(cursor) = page.cursor else {
            break;
        };
        // continue this cursor for the next page
        opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Tags)
            .limit(1)
            .cursor(cursor);
    }
    // make sure walking the cursor found both of our repos
    is!(keys.contains(&first), true);
    is!(keys.contains(&second), true);
    Ok(())
}

/// A single repo page scans past repos without matches instead of returning empty pages
#[tokio::test]
async fn grep_repos_scans_past_empty_candidates() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create two repos tagged with our needle first so the newest first corpus
    // walk has to scan past every untagged repo to reach them
    let first = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(&first, vec![group.clone()], None).tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    let second = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(&second, vec![group.clone()], None).tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    // bury our tagged repos behind a corpus of repos with no matches at all
    let mut blanks = Vec::new();
    for _ in 0..15 {
        let url = format!("github.com/{}/grep-blank", generators::gen_string(16));
        let req = RepoRequest::new(&url, vec![group.clone()], None);
        client.repos.create(&req).await?;
        blanks.push(url);
    }
    // a single default page should scan past every untagged repo and return matching
    // repos instead of the empty page an item scanned bound would have returned
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Tags);
    let page = client.repos.grep(opts).await?;
    is!(page.data.is_empty(), false);
    // collect every key this walk returns, starting with the first page
    let mut keys = page
        .data
        .iter()
        .map(|matches| matches.key.clone())
        .collect::<Vec<String>>();
    // walk any leftover pages to exhaustion
    let mut cursor = page.cursor;
    for _ in 0..10 {
        // stop once our cursor has been exhausted
        let Some(id) = cursor else {
            break;
        };
        // grep the next page of repos
        let opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Tags)
            .cursor(id);
        let page = client.repos.grep(opts).await?;
        // save the keys we found matches for in this page
        keys.extend(page.data.iter().map(|matches| matches.key.clone()));
        cursor = page.cursor;
    }
    // make sure this walk found both of our tagged repos
    is!(keys.contains(&first), true);
    is!(keys.contains(&second), true);
    // no untagged repo should ever come back as a match
    is!(blanks.iter().any(|blank| keys.contains(blank)), false);
    Ok(())
}

/// A user without access to a repos group gets no hits for it
#[tokio::test]
async fn grep_repo_respects_groups() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group for our hidden repo and one for our other user
    let mut groups = generators::groups(2, &client).await?;
    let hidden_group = groups.remove(0).name;
    let other_group = groups.remove(0).name;
    // create a hidden repo tagged with our needle
    let hidden = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(&hidden, vec![hidden_group.clone()], None).tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    // create a visible repo in the other group as a positive control
    let visible = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(&visible, vec![other_group.clone()], None).tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    // build a second user that is only in the other group so their corpus grep
    // exercises real listing and scoping instead of the no groups short circuit
    let other = generators::client_in_groups(&client, &[other_group.clone()]).await?;
    // make sure this user cannot grep a repo they cannot see
    let check = other.repos.grep_key(&hidden, GrepOpts::new(PATTERN)).await;
    fail!(check, 404);
    // a corpus grep should find their own groups repo but never the hidden one
    let page = other.repos.grep(GrepOpts::new(PATTERN)).await?;
    is!(page.data.iter().any(|matches| matches.key == visible), true);
    is!(page.data.iter().any(|matches| matches.key == hidden), false);
    Ok(())
}

/// Group scoping hides repo tags and submissions outside the requested groups
#[tokio::test]
async fn grep_repo_groups_scope_sources() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create two groups to scope data across
    let mut groups = generators::groups(2, &client).await?;
    let scoped = groups.remove(0).name;
    let other_group = groups.remove(0).name;
    // create a repo in the scoped group as the admin, tagged with our needle
    let url = format!("github.com/{}/grep-test", generators::gen_string(16));
    let req = RepoRequest::new(&url, vec![scoped.clone()], None).tag("GrepTag", NEEDLE);
    client.repos.create(&req).await?;
    // submit this repo to the other group as a randomly named user, so the repo
    // has one submission chunk per group with distinct creators
    let other = generators::client_in_groups(&client, &[other_group.clone()]).await?;
    let req = RepoRequest::new(&url, vec![other_group.clone()], None);
    other.repos.create(&req).await?;
    // a grep scoped to the other group must not see the admins creator submission
    let opts = GrepOpts::new("thorium").group(&other_group);
    let matches = client.repos.grep_key(&url, opts).await?;
    is_empty!(matches.submissions);
    // or the scoped tag
    let opts = GrepOpts::new(PATTERN).group(&other_group);
    let matches = client.repos.grep_key(&url, opts).await?;
    is_empty!(matches.tags);
    // a grep scoped to the right group should see the admins creator submission
    let opts = GrepOpts::new("thorium").group(&scoped);
    let matches = client.repos.grep_key(&url, opts).await?;
    is!(
        matches
            .submissions
            .values()
            .flat_map(HashMap::values)
            .any(|doc| !doc.matches.is_empty()),
        true
    );
    // and the scoped tag
    let opts = GrepOpts::new(PATTERN).group(&scoped);
    let matches = client.repos.grep_key(&url, opts).await?;
    contains_key!(matches.tags, "GrepTag");
    Ok(())
}

/// Greping a single entity finds our needle in its metadata and tags
#[tokio::test]
async fn grep_entity_all_sources() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // build a flag entity whose reasoning and content both contain our needle
    let metadata = EntityMetadataRequest::Flag(Flag {
        suspicion: 50,
        confidence: thorium::models::Confidence::Likely,
        content: Some(format!("flag content {NEEDLE}")),
        reasoning: format!("flag reasoning {NEEDLE}"),
    });
    // build our entity request with our needle in its name and tags too
    let mut req = EntityRequest::new(format!("entity {NEEDLE}"), metadata, vec![group.clone()])
        .tag("GrepTag", NEEDLE);
    req.description = Some(format!("an entity description with {NEEDLE} in it"));
    // create this entity
    let resp = client.entities.create(req).await?;
    // grep this entity for our needle across every source
    let id = resp.id.to_string();
    let matches = client
        .entities
        .grep_key(&id, GrepOpts::new(PATTERN))
        .await?;
    // make sure these matches are for the entity we greped
    is!(matches.key, id);
    // make sure we found our needle in every greppable metadata field
    contains_key!(matches.metadata, "name");
    contains_key!(matches.metadata, "description");
    contains_key!(matches.metadata, "reasoning");
    contains_key!(matches.metadata, "content");
    is!(has_needle(matches.metadata.values()), true);
    // make sure we found our needle in this entities tags
    contains_key!(matches.tags, "GrepTag");
    is!(has_needle(matches.tags["GrepTag"].values()), true);
    // entities have no submissions, comments, or results in Thorium today
    is_empty!(matches.submissions);
    is_empty!(matches.comments);
    is_empty!(matches.results);
    is_empty!(matches.result_files);
    Ok(())
}

/// Restricting an entity grep to tags excludes its metadata
#[tokio::test]
async fn grep_entity_only_tags() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // build a sigma entity tagged with our needle
    let mut req = generators::gen_entity(&group, generators::gen_sigma_meta());
    req.name = format!("entity {NEEDLE}");
    req = req.tag("GrepTag", NEEDLE);
    // create this entity
    let resp = client.entities.create(req).await?;
    // grep this entity for our needle in only its tags
    let opts = GrepOpts::new(PATTERN).source(GrepSource::Tags);
    let id = resp.id.to_string();
    let matches = client.entities.grep_key(&id, opts).await?;
    // make sure we still found our needle in this entities tags
    contains_key!(matches.tags, "GrepTag");
    // make sure this entities metadata was never searched
    is_empty!(matches.metadata);
    Ok(())
}

/// The entity corpus route returns the same matches as the per entity route
#[tokio::test]
async fn grep_entities_corpus_matches_single() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // build a sigma entity whose name contains our needle
    let mut req = generators::gen_entity(&group, generators::gen_sigma_meta());
    req.name = format!("entity {NEEDLE}");
    // create this entity
    let resp = client.entities.create(req).await?;
    // grep every entity in our new group, which only has the one entity in it
    let opts = GrepOpts::new(PATTERN).group(&group);
    let page = client.entities.grep(opts).await?;
    // find the matches for the entity we created
    let id = resp.id.to_string();
    let corpus = page
        .data
        .iter()
        .find(|matches| matches.key == id)
        .ok_or_else(|| Error::new("the corpus grep did not return our new entity"))?;
    // grep this single entity directly
    let single = client
        .entities
        .grep_key(&id, GrepOpts::new(PATTERN))
        .await?;
    // make sure both routes found the same number of matches
    is!(total_matches(corpus), total_matches(&single));
    Ok(())
}

/// An entity corpus grep can be continued with the cursor it returns
#[tokio::test]
async fn grep_entities_cursor_continues() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // build two sigma entities whose names both contain our needle
    let mut req = generators::gen_entity(&group, generators::gen_sigma_meta());
    req.name = format!("entity {NEEDLE}");
    let first = client.entities.create(req).await?.id.to_string();
    let mut req = generators::gen_entity(&group, generators::gen_sigma_meta());
    req.name = format!("entity {NEEDLE}");
    let second = client.entities.create(req).await?.id.to_string();
    // both entities should be distinct so the cursor has two items to walk
    is_not!(first, second);
    // walk our cursor a single entity at a time, collecting every key we see
    let mut keys = Vec::new();
    // start with only the first page of a single entity
    let mut opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Metadata)
        .limit(1);
    // keep paging until our cursor is exhausted or we have made too many requests
    for _ in 0..10 {
        // grep the next page of entities
        let page = client.entities.grep(opts).await?;
        // save the keys we found matches for in this page
        keys.extend(page.data.iter().map(|matches| matches.key.clone()));
        // stop once our cursor has been exhausted
        let Some(cursor) = page.cursor else {
            break;
        };
        // continue this cursor for the next page
        opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Metadata)
            .limit(1)
            .cursor(cursor);
    }
    // make sure walking the cursor found both of our entities
    is!(keys.contains(&first), true);
    is!(keys.contains(&second), true);
    Ok(())
}

/// A single entity page scans past entities without matches instead of returning empty pages
#[tokio::test]
async fn grep_entities_scans_past_empty_candidates() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // build two entities whose names contain our needle first so the newest first
    // corpus walk has to scan past every blank entity to reach them
    let mut req = generators::gen_entity(&group, generators::gen_sigma_meta());
    req.name = format!("entity {NEEDLE}");
    let first = client.entities.create(req).await?.id.to_string();
    let mut req = generators::gen_entity(&group, generators::gen_sigma_meta());
    req.name = format!("entity {NEEDLE}");
    let second = client.entities.create(req).await?.id.to_string();
    // bury our matching entities behind a corpus of entities with no matches at all
    let mut blanks = Vec::new();
    for _ in 0..15 {
        let req = generators::gen_entity(&group, generators::gen_sigma_meta());
        blanks.push(client.entities.create(req).await?.id.to_string());
    }
    // a single default page should scan past every blank entity and return matching
    // entities instead of the empty page an item scanned bound would have returned
    let opts = GrepOpts::new(PATTERN)
        .group(&group)
        .source(GrepSource::Metadata);
    let page = client.entities.grep(opts).await?;
    is!(page.data.is_empty(), false);
    // collect every key this walk returns, starting with the first page
    let mut keys = page
        .data
        .iter()
        .map(|matches| matches.key.clone())
        .collect::<Vec<String>>();
    // walk any leftover pages to exhaustion
    let mut cursor = page.cursor;
    for _ in 0..10 {
        // stop once our cursor has been exhausted
        let Some(id) = cursor else {
            break;
        };
        // grep the next page of entities
        let opts = GrepOpts::new(PATTERN)
            .group(&group)
            .source(GrepSource::Metadata)
            .cursor(id);
        let page = client.entities.grep(opts).await?;
        // save the keys we found matches for in this page
        keys.extend(page.data.iter().map(|matches| matches.key.clone()));
        cursor = page.cursor;
    }
    // make sure this walk found both of our matching entities
    is!(keys.contains(&first), true);
    is!(keys.contains(&second), true);
    // no blank entity should ever come back as a match
    is!(blanks.iter().any(|blank| keys.contains(blank)), false);
    Ok(())
}

/// A user without access to an entities group gets no hits for it
#[tokio::test]
async fn grep_entity_respects_groups() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group for our hidden entity and one for our other user
    let mut groups = generators::groups(2, &client).await?;
    let hidden_group = groups.remove(0).name;
    let other_group = groups.remove(0).name;
    // build a hidden sigma entity whose name contains our needle
    let mut req = generators::gen_entity(&hidden_group, generators::gen_sigma_meta());
    req.name = format!("entity {NEEDLE}");
    let hidden = client.entities.create(req).await?.id.to_string();
    // build a visible entity in the other group as a positive control
    let mut req = generators::gen_entity(&other_group, generators::gen_sigma_meta());
    req.name = format!("entity {NEEDLE}");
    let visible = client.entities.create(req).await?.id.to_string();
    // build a second user that is only in the other group so their corpus grep
    // exercises real listing and scoping instead of the no groups short circuit
    let other = generators::client_in_groups(&client, &[other_group.clone()]).await?;
    // make sure this user cannot grep an entity they cannot see
    let check = other
        .entities
        .grep_key(&hidden, GrepOpts::new(PATTERN))
        .await;
    fail!(check, 404);
    // a corpus grep should find their own groups entity but never the hidden one
    let page = other.entities.grep(GrepOpts::new(PATTERN)).await?;
    is!(page.data.iter().any(|matches| matches.key == visible), true);
    is!(page.data.iter().any(|matches| matches.key == hidden), false);
    Ok(())
}

/// Group scoping hides entity metadata and tags outside the requested groups
#[tokio::test]
async fn grep_entity_groups_scope_metadata() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create two groups to scope data across
    let mut groups = generators::groups(2, &client).await?;
    let scoped = groups.remove(0).name;
    let other_group = groups.remove(0).name;
    // build a sigma entity in only the scoped group with our needle in its name and tags
    let mut req = generators::gen_entity(&scoped, generators::gen_sigma_meta());
    req.name = format!("entity {NEEDLE}");
    req = req.tag("GrepTag", NEEDLE);
    let id = client.entities.create(req).await?.id.to_string();
    // a grep scoped to the other group must not see this entities metadata or tags
    let opts = GrepOpts::new(PATTERN).group(&other_group);
    let matches = client.entities.grep_key(&id, opts).await?;
    is_empty!(matches.metadata);
    is_empty!(matches.tags);
    // a grep scoped to the right group should see both
    let opts = GrepOpts::new(PATTERN).group(&scoped);
    let matches = client.entities.grep_key(&id, opts).await?;
    contains_key!(matches.metadata, "name");
    contains_key!(matches.tags, "GrepTag");
    Ok(())
}

/// Greping an entity id that is not a uuid is a bad request rather than a 500
#[tokio::test]
async fn grep_entity_invalid_id() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // an id that is not a uuid cannot be greped
    let check = client
        .entities
        .grep_key("not-a-uuid", GrepOpts::new(PATTERN))
        .await;
    fail!(check, 400);
    Ok(())
}
