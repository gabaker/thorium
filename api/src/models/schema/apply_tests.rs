//! Tests of the applier's pure helpers

use super::apply::left_building;
use super::redact_trigger_key;

/// The files password's hash is dropped from a trigger key, whatever the case of its name, and
/// every other segment keeps its place
#[test]
fn the_password_segment_is_dropped_from_a_trigger_key() {
    // a key with the hash in the middle keeps the segments around it
    assert_eq!(
        redact_trigger_key("image=thorium:1;password=cc;revision=2026-10-v02"),
        "image=thorium:1;revision=2026-10-v02"
    );
    // the name matches in any case, first or last
    assert_eq!(
        redact_trigger_key("PASSWORD=aa;image=thorium:1;Password=bb"),
        "image=thorium:1"
    );
    // a key of only the hash, and an empty key, are empty
    assert_eq!(redact_trigger_key("password=cc"), "");
    assert_eq!(redact_trigger_key(""), "");
    // a key without the hash, or a value that merely mentions it, is unchanged
    assert_eq!(
        redact_trigger_key("image=password:1;revision=r"),
        "image=password:1;revision=r"
    );
    assert_eq!(redact_trigger_key("key-record"), "key-record");
}

/// A view that exists and is still building is waited for, one this run created is left to its
/// own wait, and one that doesn't exist, because a table holds its name, is never waited for
#[test]
fn only_existing_unbuilt_views_are_left_building() {
    // three unbuilt views: one existing, one just created, one that the keyspace lacks
    let unbuilt = ["samples", "comments_by_id", "results_auth"];
    let existing = [["samples".to_owned()], ["comments_by_id".to_owned()]];
    // the existing view this run didn't create is left building
    assert_eq!(
        left_building(&unbuilt, &["comments_by_id"], &existing),
        ["samples"]
    );
    // with nothing created, both existing views are, and the missing one never is
    assert_eq!(
        left_building(&unbuilt, &[], &existing),
        ["samples", "comments_by_id"]
    );
    // a keyspace holding no views leaves nothing to wait for
    assert!(left_building(&unbuilt, &[], &[]).is_empty());
}
