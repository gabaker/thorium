//! Tests of the Elastic rebuild's types: the sources' init keys and indexes, what mismatches the
//! declared mappings, the target a rebuild drops, and the progress its check observes

use serde_json::json;

use super::{
    ElasticIndexDef, REBUILD_MIGRATION, RebuildProgress, RebuildTarget, SnapshotObject,
    mapping_object, reingests,
};
use crate::models::elastic::{ElasticIndex, ElasticSource};

/// Observe every declared index as the probe would find it with the mappings Thorium declares
fn observed_as_declared() -> Vec<SnapshotObject> {
    // the probe's form of each index's `_mapping` response
    ElasticIndex::ALL
        .iter()
        .map(|index| {
            let declared = ElasticIndexDef::of(index);
            let response = json!({ "concrete": declared.body });
            mapping_object(index, &response).expect("an observed index")
        })
        .collect()
}

/// Observe one index with the dynamic mappings Elastic gives an index created by a write
///
/// # Arguments
///
/// * `index` - The index to observe
fn observed_dynamic(index: &ElasticIndex) -> SnapshotObject {
    // a `group` mapped as text with a keyword subfield is what dynamic mapping builds
    let response = json!({"concrete": {"mappings": {"properties": {
        "group": {"type": "text", "fields": {"keyword": {"type": "keyword"}}}
    }}}});
    mapping_object(index, &response).expect("an observed index")
}

/// The init session keys are the search-streamer's `{ns}:search-streamer:init:{info,log}:Elastic-*`
#[test]
fn a_source_names_the_streamers_init_keys() {
    // each source keys its session by the store and its own name
    assert_eq!(
        ElasticSource::Results.init_keys("ns"),
        [
            "ns:search-streamer:init:info:Elastic-Results".to_owned(),
            "ns:search-streamer:init:log:Elastic-Results".to_owned(),
        ]
    );
    assert_eq!(
        ElasticSource::Tags.init_keys("thorium"),
        [
            "thorium:search-streamer:init:info:Elastic-Tags".to_owned(),
            "thorium:search-streamer:init:log:Elastic-Tags".to_owned(),
        ]
    );
}

/// A source owns the two indexes the streamer creates for it
#[test]
fn a_source_owns_its_two_indexes() {
    // results and tags each cover samples and repos
    let names = |source: ElasticSource| {
        source
            .indexes()
            .iter()
            .map(|index| ElasticIndexDef::of(index).name)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(ElasticSource::Results),
        ["SampleResults", "RepoResults"]
    );
    assert_eq!(names(ElasticSource::Tags), ["SampleTags", "RepoTags"]);
}

/// Indexes observed with the declared mappings match; a missing index and an index with other
/// mappings don't
#[test]
fn missing_and_differently_mapped_indexes_mismatch() {
    // every index as declared: nothing mismatches
    let mut observed = observed_as_declared();
    assert_eq!(ElasticIndexDef::mismatched(&observed), Vec::<&str>::new());
    // one index with dynamic mappings and one gone
    observed.retain(|object| object.name != "RepoResults");
    for object in &mut observed {
        if object.name == "SampleResults" {
            *object = observed_dynamic(&ElasticIndex::SampleResults);
        }
    }
    assert_eq!(
        ElasticIndexDef::mismatched(&observed),
        ["SampleResults", "RepoResults"]
    );
    // nothing observed at all: every index is missing
    assert_eq!(ElasticIndexDef::mismatched(&[]).len(), 4);
}

/// A target drops the named indexes and resets the source of each, and leaves out names that
/// aren't Thorium's indexes
#[test]
fn a_target_resets_the_source_of_each_index() {
    // one index of each source
    let both = RebuildTarget::of(["SampleResults", "RepoTags"]);
    assert_eq!(
        both.indexes.iter().map(String::as_str).collect::<Vec<_>>(),
        ["RepoTags", "SampleResults"]
    );
    assert_eq!(
        both.sources.iter().copied().collect::<Vec<_>>(),
        [ElasticSource::Results, ElasticSource::Tags]
    );
    // one source only, and a stray name
    let tags = RebuildTarget::of(["SampleTags", "not-an-index"]);
    assert_eq!(tags.indexes.len(), 1);
    assert_eq!(
        tags.sources.iter().copied().collect::<Vec<_>>(),
        [ElasticSource::Tags]
    );
    assert!(RebuildTarget::of(["not-an-index"]).is_empty());
}

/// Merging two rebuilds drops what both would, once
#[test]
fn merged_targets_drop_the_union() {
    // the results indexes, then one tags index and one results index again
    let first = RebuildTarget::of(["SampleResults", "RepoResults"]);
    let second = RebuildTarget::of(["SampleResults", "SampleTags"]);
    let merged = first.merge(&second);
    assert_eq!(
        merged
            .indexes
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["RepoResults", "SampleResults", "SampleTags"]
    );
    assert_eq!(merged.sources.len(), 2);
}

/// A source is finished only with no init session left and every index as declared; a source with
/// no session and a mismatch lost its reingest
#[test]
fn progress_tells_a_running_reingest_from_a_lost_one() {
    // everything as declared
    let observed = observed_as_declared();
    // a session still in Redis: running, not finished
    let running = RebuildProgress::observe(ElasticSource::Results, true, &observed);
    assert!(!running.finished());
    assert!(!running.lost());
    assert!(running.describe().contains("init session"));
    // no session and nothing mismatched: finished
    let done = RebuildProgress::observe(ElasticSource::Results, false, &observed);
    assert!(done.finished());
    assert!(!done.lost());
    // no session and an index of the source missing: lost, naming the index
    let mut broken = observed;
    broken.retain(|object| object.name != "SampleTags");
    let lost = RebuildProgress::observe(ElasticSource::Tags, false, &broken);
    assert!(lost.lost());
    assert_eq!(lost.mismatched, ["SampleTags"]);
    assert!(lost.describe().contains("SampleTags"));
    // the other source doesn't carry the index's mismatch
    let other = RebuildProgress::observe(ElasticSource::Results, false, &broken);
    assert!(other.finished());
}

/// Only the Elastic rebuild finishes its step `Reingesting`
#[test]
fn only_the_elastic_rebuild_reingests() {
    // the migration's id is the one the registry uses
    assert_eq!(REBUILD_MIGRATION, "elastic-rebuild");
    assert!(reingests("elastic-rebuild"));
    assert!(!reingests("rebucket-files"));
    assert!(!reingests("elastic-keyword-mappings"));
}
