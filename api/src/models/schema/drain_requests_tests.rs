//! Tests of the drain request records: their status text and rank, and the request JSON

use std::collections::BTreeSet;

use super::drain_requests::{
    ARGS_TTL_SECS, CreatorMembership, DrainRequest, DrainStatus, IDLE_DAYS, MAX_ARGS_BYTES,
    MAX_REQUESTS, PipelineIdentity, args_sha256,
};
use super::queries::migrations::{insert_if_absent, update_status_if};
use crate::models::ReactionStatus;

/// Every drain status has its own `snake_case` stored text, which parses back to it, and text no
/// status owns is refused
#[test]
fn every_drain_status_has_its_stored_text() {
    // the statuses a reader knows, and the text each is stored as
    let texts = DrainStatus::ALL
        .iter()
        .map(DrainStatus::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        [
            "recorded",
            "resubmitting",
            "resubmitted",
            "refused",
            "stale"
        ]
    );
    assert_eq!(texts.iter().collect::<BTreeSet<_>>().len(), texts.len());
    // each parses back to itself and displays as its stored text
    for status in DrainStatus::ALL {
        assert_eq!(status.as_str().parse::<DrainStatus>().ok(), Some(status));
        assert_eq!(status.to_string(), status.as_str());
    }
    // a run status or an unknown word is not a drain status
    assert!("running".parse::<DrainStatus>().is_err());
    assert!("Recorded".parse::<DrainStatus>().is_err());
    assert!("".parse::<DrainStatus>().is_err());
}

/// The final statuses outrank a reservation, which outranks a recorded or stale request, and a
/// recorded and a stale request rank alike
#[test]
fn drain_statuses_rank_by_how_far_the_request_moved() {
    // the rank of each status
    assert_eq!(DrainStatus::Resubmitted.rank(), 3);
    assert_eq!(DrainStatus::Refused.rank(), 3);
    assert_eq!(DrainStatus::Resubmitting.rank(), 2);
    assert_eq!(DrainStatus::Recorded.rank(), 1);
    assert_eq!(DrainStatus::Stale.rank(), 1);
    // the ordering a monotonic merge relies on
    assert!(DrainStatus::Resubmitted.rank() > DrainStatus::Resubmitting.rank());
    assert!(DrainStatus::Resubmitting.rank() > DrainStatus::Recorded.rank());
    assert_eq!(DrainStatus::Recorded.rank(), DrainStatus::Stale.rank());
}

/// The JSON form of a drain status matches its stored text
#[test]
fn drain_status_json_is_its_stored_text() {
    // every status serializes to its stored text and back
    for status in DrainStatus::ALL {
        let json = serde_json::to_string(&status).unwrap();
        assert_eq!(json, format!("\"{}\"", status.as_str()));
        assert_eq!(serde_json::from_str::<DrainStatus>(&json).unwrap(), status);
    }
}

/// The caps and the args time-to-live hold the documented values
#[test]
fn drain_request_caps_hold_their_values() {
    // the request and args caps
    assert_eq!(MAX_REQUESTS, 10_000);
    assert_eq!(MAX_ARGS_BYTES, 64 * 1024);
    // the idle cutoff and the args row's time-to-live
    assert_eq!(IDLE_DAYS, 30);
    assert_eq!(ARGS_TTL_SECS, 14 * 24 * 60 * 60);
}

/// Build a drain request with every field set
fn full_request() -> DrainRequest {
    // a request a cancel records for a started reaction with arguments
    DrainRequest {
        group: "corn".to_owned(),
        pipeline: "harvest".to_owned(),
        samples: vec!["a".repeat(64)],
        repos: Vec::new(),
        tags: vec!["field:north".to_owned()],
        parent: None,
        trigger_depth: Some(1),
        creator: "farmer".to_owned(),
        old_status: ReactionStatus::Started,
        last_activity: Some(chrono::Utc::now()),
        orphan: false,
        args_sha256: Some(args_sha256("{}")),
        args_bytes: 2,
        buffers: false,
        cache: false,
        identity: PipelineIdentity {
            group: "corn".to_owned(),
            name: "harvest".to_owned(),
            creator: "farmer".to_owned(),
            order_sha256: "0".repeat(64),
        },
        membership: CreatorMembership {
            held_role: true,
            role: "User".to_owned(),
        },
        binding: Some(vec![vec!["tool-a".to_owned()]]),
    }
}

/// A drain request round trips through its JSON and the JSON holds no arguments
#[test]
fn a_drain_request_round_trips_without_args() {
    // serialize and parse a request
    let request = full_request();
    let json = serde_json::to_string(&request).unwrap();
    let parsed = DrainRequest::from_json(&json).unwrap();
    assert_eq!(parsed, request);
    assert_eq!(request.to_json().unwrap(), json);
    // the JSON names no args field, only their hash and size
    let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();
    let object = value.as_object().unwrap();
    assert!(!object.contains_key("args"));
    assert!(object.contains_key("args_sha256"));
    assert!(object.contains_key("args_bytes"));
}

/// A request recorded before the binding pass parses with no binding
#[test]
fn a_drain_request_without_a_binding_parses() {
    // drop the binding from the JSON
    let mut value = serde_json::to_value(full_request()).unwrap();
    value.as_object_mut().unwrap().remove("binding");
    let parsed = serde_json::from_value::<DrainRequest>(value).unwrap();
    assert_eq!(parsed.binding, None);
}

/// The args hash is the lowercase hex sha256 of the args JSON
#[test]
fn the_args_hash_is_hex_sha256() {
    // the well-known digest of `{}`
    assert_eq!(
        args_sha256("{}"),
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
}

/// The conditional writes are the statements the goldens hold, with their conditions last
#[test]
fn the_conditional_writes_end_in_their_conditions() {
    // the insert keeps an existing row and the update checks the status it read
    assert!(insert_if_absent("ks").ends_with(" IF NOT EXISTS"));
    assert!(insert_if_absent("ks").starts_with("INSERT INTO ks.schema_migrations "));
    assert!(update_status_if("ks").ends_with(" WHERE kind = ? AND id = ? IF status = ?"));
    assert!(update_status_if("ks").starts_with("UPDATE ks.schema_migrations SET status = ?"));
}
