//! Integration tests for `YaraRuleHit` entities and their deterministic identity

mod common;

use chrono::Utc;
use rand::RngCore;
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

use thorium::models::entities::rules::{YaraActionToTake, YaraAutoFlag};
use thorium::models::{
    AssociationKind, AssociationListOpts, AssociationTarget, Buffer, Confidence, Entity,
    EntityMetadata, EntityMetadataRequest, EntityRequest, Event, EventData, EventIds, EventPopOpts,
    EventType, GroupAllowedUpdate, GroupUpdate, Hunt, HuntCreateRequest, HuntEngine, HuntSelector,
    MatchedRule, MatchedString, NotificationLevel, NotificationParams, NotificationRequest,
    SampleRequest, YaraHitWriteRequest, YaraRule, YaraRuleAppliesTo, YaraRuleHit,
};
use thorium::test_utilities::{self, generators};
use thorium::{Thorium, fail, is};

/// A small always compiling yara rule for hit route tests
const ROUTE_TEST_RULE: &str = r#"rule M3dRouteTest
{
    strings:
        $marker = "m3d-route-test"
    condition:
        $marker
}"#;

/// Build a random sha256 like string for a fake sample
fn fake_sha256() -> String {
    // uuids give us enough randomness for a fake unique sample hash
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Generate unique random bytes for a brand new sample
fn unique_bytes() -> Vec<u8> {
    // generate unique random bytes so each test uploads a brand new sample
    let mut random_data = [0u8; 32];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut random_data);
    random_data.to_vec()
}

/// Upload some sample bytes into some groups
///
/// # Arguments
///
/// * `client` - The client to upload this sample with
/// * `bytes` - The bytes to upload
/// * `groups` - The groups to upload this sample too
async fn upload_bytes(
    client: &Thorium,
    bytes: Vec<u8>,
    groups: Vec<String>,
) -> Result<String, thorium::Error> {
    // build and upload a sample around these bytes
    let req = SampleRequest::new_buffer(Buffer::new(bytes), groups);
    let resp = client.files.create(req).await?;
    Ok(resp.sha256)
}

/// Create a yara rule entity and return its id and name
///
/// # Arguments
///
/// * `client` - The client to create this rule with
/// * `groups` - The groups this rule should be in
/// * `score` - The score this rule applies on a hit
/// * `actions` - The auto actions this rule carries
async fn create_rule(
    client: &Thorium,
    groups: Vec<String>,
    score: i64,
    actions: Vec<YaraActionToTake>,
) -> Result<(Uuid, String), thorium::Error> {
    // build the yara rule metadata for this rule
    let mut rule = YaraRule::new(ROUTE_TEST_RULE, YaraRuleAppliesTo::Samples)
        .expect("failed to build route test yara rule")
        .score(score);
    // add any auto actions this rule should carry
    for action in actions {
        rule = rule.action(action);
    }
    // build the entity request for this rule with a random name
    let name = generators::gen_string(16);
    let req = EntityRequest::new(&name, EntityMetadataRequest::YaraRule(rule), groups);
    // create this rule entity
    let resp = client.entities.create(req).await?;
    Ok((resp.id, name))
}

/// Build a hit write request for the server side hit route
///
/// # Arguments
///
/// * `rule_id` - The id of the rule that hit
/// * `sha256` - The sha256 of the sample that was hit
/// * `event_groups` - The groups the scan event made the sample visible too
/// * `event_depth` - The depth of the scan event
fn write_req(
    rule_id: Uuid,
    sha256: &str,
    event_groups: Vec<String>,
    event_depth: u8,
) -> YaraHitWriteRequest {
    // build a bounded matched string and rule for this hit
    let matched_string = MatchedString {
        id: "$marker".to_owned(),
        count: 1,
        first_offsets: vec![0],
    };
    let matched = MatchedRule {
        rule_identifier: "M3dRouteTest".to_owned(),
        strings: vec![matched_string],
    };
    // build the hit write request around this scans facts
    YaraHitWriteRequest {
        rule_id,
        sha256: sha256.to_owned(),
        rule_hash: "feed".repeat(16),
        ruleset_hash: "beef".repeat(16),
        engine_version: "yara-x-test".to_owned(),
        scanned_at: Utc::now(),
        event_groups,
        event_depth,
        matched: vec![matched],
        hunt: None,
    }
}

/// Pull the yara hit metadata off an entity or panic
///
/// # Arguments
///
/// * `entity` - The entity to pull hit metadata from
fn hit_meta(entity: &Entity) -> &YaraRuleHit {
    // make sure this entity carries yara hit metadata
    match &entity.metadata {
        EntityMetadata::YaraRuleHit(hit) => hit,
        meta => panic!("entity carries the wrong metadata kind: {meta:?}"),
    }
}

/// Create a hunt frozen over one rule and return its id
///
/// Hunt creating tests must be `#[serial]` since hunts share the cluster
/// global quota census and chunk queue; callers run `cleanup_hunt` when they
/// finish so nothing leaks into the other suites.
///
/// # Arguments
///
/// * `client` - The client to create this hunt with
/// * `groups` - The groups this hunt should be bounded to
/// * `rule_id` - The rule this hunt freezes
async fn create_hunt(
    client: &Thorium,
    groups: Vec<String>,
    rule_id: Uuid,
) -> Result<Uuid, thorium::Error> {
    // build the hunt create request frozen over just this rule
    let req = HuntCreateRequest {
        name: generators::gen_string(16),
        groups,
        engine: HuntEngine::Yara,
        selector: HuntSelector::Rules(vec![rule_id]),
        corpus: None,
        description: None,
    };
    // create this hunt
    let entity = client.entities.create_hunt(&req).await?;
    Ok(entity.id)
}

/// Cancel a hunt and wipe the shared chunk queue after a decoration test
///
/// # Arguments
///
/// * `client` - The client to clean up with
/// * `hunt_id` - The hunt to cancel
async fn cleanup_hunt(client: &Thorium, hunt_id: Uuid) -> Result<(), thorium::Error> {
    // cancel this hunt so the quota census frees its slot
    client.entities.cancel_hunt(hunt_id).await?;
    // wipe the shared chunk queue so our seeded chunk events do not leak
    test_utilities::wipe_hunt_chunk_queue().await?;
    Ok(())
}

/// Pop reaction trigger events until the hit tag event for a sample appears
///
/// Foreign events popped while searching are cleared so they do not leak in
/// the in flight queue; other suites share these queues in CI so we only ever
/// assert on events for our own sha256.
///
/// # Arguments
///
/// * `client` - A Thorium client
/// * `sha256` - The sha256 our hit tag event must be for
async fn pop_hit_tag_event(
    client: &Thorium,
    sha256: &str,
) -> Result<Option<Event>, thorium::Error> {
    // pop in large pages to cut round trips
    let opts = EventPopOpts::default().limit(200);
    // track any foreign events we popped so we can clear them
    let mut foreign = Vec::default();
    // the event we found if any
    let mut found = None;
    // search a bounded number of pages so a missing event fails instead of hanging
    'outer: for _ in 0..10 {
        // pop a page of reaction trigger events
        let events = client.events.pop(EventType::ReactionTrigger, &opts).await?;
        // an empty page may just mean our event is younger than the pop
        // maturity window so wait for it to mature instead of bailing early
        if events.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        // search this page for our hit tag event
        for event in events {
            // check if this event is our samples hit tag event
            match &event.data {
                EventData::NewTags { item, tags, .. }
                    if item == sha256 && tags.contains_key(YaraRuleHit::HIT_TAG) =>
                {
                    // we found our event so stop searching
                    found = Some(event);
                    break 'outer;
                }
                // this is not our event so track it for clearing
                _ => foreign.push(event.id),
            }
        }
    }
    // clear our event so it does not get replayed
    if let Some(event) = &found {
        // build the list of events to clear
        let ids = EventIds::from(vec![event.id]);
        // clear our event
        client
            .events
            .clear(EventType::ReactionTrigger, &ids, None)
            .await?;
    }
    // clear any foreign events we popped as well
    if !foreign.is_empty() {
        // build the list of foreign events to clear
        let ids = EventIds::from(foreign);
        // clear the foreign events we popped
        client
            .events
            .clear(EventType::ReactionTrigger, &ids, None)
            .await?;
    }
    Ok(found)
}

/// Verify hit creation derives the deterministic v5 id server side
#[tokio::test]
async fn create_derives_v5_id() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a fixed hit identity
    let rule_id = Uuid::new_v4();
    let sha256 = fake_sha256();
    // build a yara rule hit entity request
    let req = generators::gen_entity(&group, generators::gen_yara_hit_meta(rule_id, &sha256));
    // create our hit entity
    let resp = client.entities.create(req).await?;
    // the server must have derived the deterministic id for this hit
    is!(resp.id, YaraRuleHit::deterministic_id(&rule_id, &sha256));
    Ok(())
}

/// Verify creating the same hit identity in a new group merges groups
#[tokio::test]
async fn create_same_identity_merges_groups() -> Result<(), thorium::Error> {
    // set up an admin client and two groups
    let (client, group_a) = common::setup().await?;
    let group_b = generators::groups(1, &client).await?.remove(0).name;
    // build a fixed hit identity
    let rule_id = Uuid::new_v4();
    let sha256 = fake_sha256();
    // create this hit in group a
    let req_a = generators::gen_entity(&group_a, generators::gen_yara_hit_meta(rule_id, &sha256));
    let resp_a = client.entities.create(req_a).await?;
    // create the same hit identity in group b with a different name
    let req_b = generators::gen_entity(&group_b, generators::gen_yara_hit_meta(rule_id, &sha256));
    let resp_b = client.entities.create(req_b).await?;
    // both creates must converge on one hit entity
    is!(resp_a.id, resp_b.id);
    is!(resp_a.name.clone(), resp_b.name);
    // the merged hit must now be visible in both groups
    let entity = client.entities.get(resp_a.id).await?;
    assert!(entity.groups.contains(&group_a), "hit lost group a");
    assert!(entity.groups.contains(&group_b), "hit did not gain group b");
    Ok(())
}

/// Verify re-creating an identical hit is an idempotent no-op
#[tokio::test]
async fn merge_is_idempotent() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a fixed hit identity
    let rule_id = Uuid::new_v4();
    let sha256 = fake_sha256();
    // create this hit twice in the same group
    let req1 = generators::gen_entity(
        &group.clone(),
        generators::gen_yara_hit_meta(rule_id, &sha256),
    );
    let resp1 = client.entities.create(req1).await?;
    let req2 = generators::gen_entity(&group, generators::gen_yara_hit_meta(rule_id, &sha256));
    let resp2 = client.entities.create(req2).await?;
    // both creates must converge on one hit entity
    is!(resp1.id, resp2.id);
    // the hit must still exist in exactly one group
    let entity = client.entities.get(resp1.id).await?;
    is!(entity.groups.clone(), vec![group.clone()]);
    Ok(())
}

/// Verify hits missing their identity fields are rejected with a 400
#[tokio::test]
async fn missing_identity_fields_400() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a hit metadata request missing its identity by sending a bare kind;
    // we fake this through a yara hit whose sha256 is empty by building the
    // multipart form by hand via a rule hit with empty identity fields
    let mut meta = match generators::gen_yara_hit_meta(Uuid::new_v4(), fake_sha256()) {
        thorium::models::EntityMetadataRequest::YaraRuleHit(hit) => hit,
        _ => unreachable!("gen_yara_hit_meta must build a yara rule hit"),
    };
    // blank out the sha256 half of the identity
    meta.sha256 = String::default();
    // build a yara rule hit entity request with a missing identity half
    let req = generators::gen_entity(
        &group,
        thorium::models::EntityMetadataRequest::YaraRuleHit(meta),
    );
    // the server must reject a hit whose identity is incomplete
    let resp = client.entities.create(req).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify entity notifications can be created, listed, and deleted
#[tokio::test]
async fn entity_notifications_crud() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a yara rule entity to notify on
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    let resp = client.entities.create(req).await?;
    // build a notification for this rule
    let notification_req =
        NotificationRequest::new("this rule failed to compile", NotificationLevel::Error);
    // create a notification for this rule
    client
        .entities
        .create_notification(resp.id, &notification_req, &NotificationParams::default())
        .await?;
    // list this rules notifications
    let notifications = client.entities.get_notifications(resp.id).await?;
    is!(notifications.len(), 1);
    is!(
        notifications[0].msg.clone(),
        "this rule failed to compile".to_owned()
    );
    // delete this rules notification
    client
        .entities
        .delete_notification(resp.id, &notifications[0].id)
        .await?;
    // the notification must be gone
    let notifications = client.entities.get_notifications(resp.id).await?;
    is!(notifications.len(), 0);
    Ok(())
}

/// Verify the hit route writes the entity, associations, and depth stamped tag
#[tokio::test]
#[serial]
async fn route_writes_full_hit() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, and a sample
    let (client, group) = common::setup().await?;
    let (rule_id, rule_name) = create_rule(&client, vec![group.clone()], 7, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    // write a hit through the server side route at depth 3
    let resp = client
        .entities
        .create_yara_hit(&write_req(rule_id, &sha256, vec![group.clone()], 3))
        .await?;
    // the hit must land in exactly our group under its deterministic id
    is!(resp.id, YaraRuleHit::deterministic_id(&rule_id, &sha256));
    is!(resp.name.clone(), Some(rule_name.clone()));
    is!(resp.written.clone(), vec![group.clone()]);
    assert!(!resp.sample_missing, "the sample must not be missing");
    // the hit entity must exist with the rules authoritative name
    let entity = client.entities.get(resp.id).await?;
    is!(entity.name.clone(), rule_name.clone());
    is!(entity.groups.clone(), vec![group.clone()]);
    // the hits attribution chain must anchor both associations
    let opts = AssociationListOpts::default();
    let cursor = client.associations.list_entity(resp.id, &opts).await?;
    // find the rule and sample associations on this hit
    let mut saw_created_by = false;
    let mut saw_hit_on = false;
    for assoc in &cursor.data {
        match (&assoc.kind, &assoc.other) {
            // the rule created this hit
            (AssociationKind::CreatedBy, AssociationTarget::Entity { id, .. }) => {
                saw_created_by |= *id == rule_id;
            }
            // this hit fired on our sample
            (AssociationKind::YaraRuleHit, AssociationTarget::File(target)) => {
                saw_hit_on |= target == &sha256;
            }
            _ => (),
        }
    }
    assert!(saw_created_by, "the hit must have a CreatedBy association");
    assert!(saw_hit_on, "the hit must have a YaraRuleHit association");
    // the sample must carry the hit tag named after the rule
    let sample = client.files.get(&sha256).await?;
    let hit_tags = sample
        .tags
        .get(YaraRuleHit::HIT_TAG)
        .expect("the sample must carry the hit tag");
    assert!(
        hit_tags.contains_key(&rule_name),
        "the hit tag must name the rule"
    );
    // the hit tags event must carry the stamped loop breaking depth
    let event = pop_hit_tag_event(&client, &sha256)
        .await?
        .expect("the hit tag must emit a NewTags event");
    is!(event.depth, 4);
    Ok(())
}

/// Verify repeated hit writes across groups merge onto one entity
#[tokio::test]
async fn route_merges_across_groups() -> Result<(), thorium::Error> {
    // set up an admin client and two groups
    let (client, group_a) = common::setup().await?;
    let group_b = generators::groups(1, &client).await?.remove(0).name;
    // create a rule in both groups and one sample visible to both
    let (rule_id, _) = create_rule(
        &client,
        vec![group_a.clone(), group_b.clone()],
        0,
        Vec::new(),
    )
    .await?;
    let bytes = unique_bytes();
    let sha256 = upload_bytes(&client, bytes.clone(), vec![group_a.clone()]).await?;
    upload_bytes(&client, bytes, vec![group_b.clone()]).await?;
    // write this hit for the group a scan event
    let resp_a = client
        .entities
        .create_yara_hit(&write_req(rule_id, &sha256, vec![group_a.clone()], 0))
        .await?;
    is!(resp_a.written.clone(), vec![group_a.clone()]);
    // write the same hit identity for the group b scan event
    let resp_b = client
        .entities
        .create_yara_hit(&write_req(rule_id, &sha256, vec![group_b.clone()], 0))
        .await?;
    is!(resp_b.written.clone(), vec![group_b.clone()]);
    // both writes must converge on one hit entity spanning both groups
    is!(resp_a.id, resp_b.id);
    let entity = client.entities.get(resp_a.id).await?;
    assert!(entity.groups.contains(&group_a), "hit lost group a");
    assert!(entity.groups.contains(&group_b), "hit did not gain group b");
    Ok(())
}

/// Verify stale event groups outside the rules groups are trimmed server side
#[tokio::test]
async fn route_trims_stale_rule_groups() -> Result<(), thorium::Error> {
    // set up an admin client and two groups
    let (client, group_a) = common::setup().await?;
    let group_b = generators::groups(1, &client).await?.remove(0).name;
    // the rule only lives in group a but the sample is in both
    let (rule_id, _) = create_rule(&client, vec![group_a.clone()], 0, Vec::new()).await?;
    let sha256 = upload_bytes(
        &client,
        unique_bytes(),
        vec![group_a.clone(), group_b.clone()],
    )
    .await?;
    // write a hit whose event claims both groups like a stale cache would
    let resp = client
        .entities
        .create_yara_hit(&write_req(
            rule_id,
            &sha256,
            vec![group_a.clone(), group_b.clone()],
            0,
        ))
        .await?;
    // the write must trim to the rules authoritative group
    is!(resp.written.clone(), vec![group_a.clone()]);
    is!(resp.suppressed_rule.clone(), vec![group_b.clone()]);
    // the hit entity must only be visible in group a
    let entity = client.entities.get(resp.id).await?;
    is!(entity.groups.clone(), vec![group_a.clone()]);
    Ok(())
}

/// Verify event groups the sample is no longer in are trimmed server side
#[tokio::test]
async fn route_trims_missing_sample_groups() -> Result<(), thorium::Error> {
    // set up an admin client and two groups
    let (client, group_a) = common::setup().await?;
    let group_b = generators::groups(1, &client).await?.remove(0).name;
    // the rule lives in both groups but the sample only in group a
    let (rule_id, _) = create_rule(
        &client,
        vec![group_a.clone(), group_b.clone()],
        0,
        Vec::new(),
    )
    .await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group_a.clone()]).await?;
    // write a hit whose event claims a group the sample has since lost
    let resp = client
        .entities
        .create_yara_hit(&write_req(
            rule_id,
            &sha256,
            vec![group_a.clone(), group_b.clone()],
            0,
        ))
        .await?;
    // the write must trim to the samples current groups
    is!(resp.written.clone(), vec![group_a.clone()]);
    is!(resp.suppressed_sample.clone(), vec![group_b.clone()]);
    Ok(())
}

/// Verify kill switched groups are suppressed by policy per group
#[tokio::test]
async fn route_suppresses_kill_switched_groups() -> Result<(), thorium::Error> {
    // set up an admin client and two groups
    let (client, group_a) = common::setup().await?;
    let group_b = generators::groups(1, &client).await?.remove(0).name;
    // the rule and sample live in both groups
    let (rule_id, _) = create_rule(
        &client,
        vec![group_a.clone(), group_b.clone()],
        0,
        Vec::new(),
    )
    .await?;
    let sha256 = upload_bytes(
        &client,
        unique_bytes(),
        vec![group_a.clone(), group_b.clone()],
    )
    .await?;
    // flip group bs entity kill switch off
    let update = GroupUpdate {
        allowed: GroupAllowedUpdate {
            entities: Some(false),
            ..GroupAllowedUpdate::default()
        },
        ..GroupUpdate::default()
    };
    client.groups.update(&group_b, &update).await?;
    // write a hit for an event spanning both groups
    let resp = client
        .entities
        .create_yara_hit(&write_req(
            rule_id,
            &sha256,
            vec![group_a.clone(), group_b.clone()],
            0,
        ))
        .await?;
    // the kill switched group must be suppressed by policy, not fail the write
    is!(resp.written.clone(), vec![group_a.clone()]);
    is!(resp.suppressed_policy.clone(), vec![group_b.clone()]);
    // the hit entity must only be visible in group a
    let entity = client.entities.get(resp.id).await?;
    is!(entity.groups.clone(), vec![group_a.clone()]);
    Ok(())
}

/// Verify an empty intersection writes nothing at all
#[tokio::test]
async fn route_empty_intersection_writes_nothing() -> Result<(), thorium::Error> {
    // set up an admin client and two groups
    let (client, group_a) = common::setup().await?;
    let group_b = generators::groups(1, &client).await?.remove(0).name;
    // the rule lives only in group a and the sample only in group b
    let (rule_id, _) = create_rule(&client, vec![group_a], 0, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group_b.clone()]).await?;
    // write a hit for the group b event; the rule term suppresses it
    let resp = client
        .entities
        .create_yara_hit(&write_req(rule_id, &sha256, vec![group_b.clone()], 0))
        .await?;
    // nothing may be written at all - not even for the rule owner
    assert!(
        resp.written.is_empty(),
        "an empty intersection must not write"
    );
    is!(resp.suppressed_rule.clone(), vec![group_b.clone()]);
    is!(resp.name.clone(), None::<String>);
    // no hit entity may exist under the deterministic id
    let missing = client.entities.get(resp.id).await;
    fail!(missing, 404);
    // the sample must not carry any hit tag
    let sample = client.files.get(&sha256).await?;
    assert!(
        !sample.tags.contains_key(YaraRuleHit::HIT_TAG),
        "a suppressed hit must not tag the sample"
    );
    Ok(())
}

/// Verify a deleted sample reports a convergent skip instead of an error
#[tokio::test]
async fn route_skips_missing_sample() -> Result<(), thorium::Error> {
    // set up an admin client, a group, and a rule
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    // write a hit for a sample that does not exist
    let resp = client
        .entities
        .create_yara_hit(&write_req(rule_id, &fake_sha256(), vec![group.clone()], 0))
        .await?;
    // the write must report the missing sample as a skip, not an error
    assert!(resp.sample_missing, "a missing sample must be reported");
    assert!(
        resp.written.is_empty(),
        "a missing sample must write nothing"
    );
    is!(resp.suppressed_sample.clone(), vec![group.clone()]);
    // no hit entity may exist under the deterministic id
    let missing = client.entities.get(resp.id).await;
    fail!(missing, 404);
    Ok(())
}

/// Verify only admins may use the hit write route
#[tokio::test]
async fn route_requires_admin() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, and a non admin user
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let user_client = generators::client(&client).await?;
    // a non admin hit write must be rejected
    let resp = user_client
        .entities
        .create_yara_hit(&write_req(rule_id, &fake_sha256(), vec![group], 0))
        .await;
    fail!(resp, 401);
    Ok(())
}

/// Verify unknown rules 404 and non rule entities 400
#[tokio::test]
async fn route_rejects_bad_rules() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // a rule id that does not exist must 404 so the scanner retries
    let resp = client
        .entities
        .create_yara_hit(&write_req(
            Uuid::new_v4(),
            &fake_sha256(),
            vec![group.clone()],
            0,
        ))
        .await;
    fail!(resp, 404);
    // an entity that is not a yara rule must be rejected as malformed
    let hit_req = generators::gen_entity(
        &group,
        generators::gen_yara_hit_meta(Uuid::new_v4(), fake_sha256()),
    );
    let hit = client.entities.create(hit_req).await?;
    let resp = client
        .entities
        .create_yara_hit(&write_req(hit.id, &fake_sha256(), vec![group], 0))
        .await;
    fail!(resp, 400);
    Ok(())
}

/// Verify repeating the same hit write is idempotent
#[tokio::test]
async fn route_repeat_write_is_idempotent() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, and a sample
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    // write the same hit twice like a retry would
    let req = write_req(rule_id, &sha256, vec![group.clone()], 0);
    let first = client.entities.create_yara_hit(&req).await?;
    let second = client.entities.create_yara_hit(&req).await?;
    // both writes must converge on the same hit in the same single group
    is!(first.id, second.id);
    is!(second.written.clone(), vec![group.clone()]);
    let entity = client.entities.get(first.id).await?;
    is!(entity.groups.clone(), vec![group.clone()]);
    Ok(())
}

/// Verify auto flag actions run from the rules authoritative state
#[tokio::test]
async fn route_runs_auto_flag_actions() -> Result<(), thorium::Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule carrying an auto flag action and a known score
    let action = YaraActionToTake::Flag(YaraAutoFlag {
        confidence: Confidence::Likely,
        content: Some("m3d route test".to_owned()),
        reasoning: "route test auto flag".to_owned(),
    });
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 42, vec![action]).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    // write a hit through the route
    let resp = client
        .entities
        .create_yara_hit(&write_req(rule_id, &sha256, vec![group.clone()], 0))
        .await?;
    is!(resp.written.clone(), vec![group.clone()]);
    // find the flag promoted onto this hit through its association
    let opts = AssociationListOpts::default();
    let cursor = client.associations.list_entity(resp.id, &opts).await?;
    let flag_id = cursor
        .data
        .iter()
        .find_map(|assoc| match (&assoc.kind, &assoc.other) {
            // flags point at the hit they were promoted from
            (AssociationKind::FlagFor, AssociationTarget::Entity { id, .. }) => Some(*id),
            _ => None,
        })
        .expect("the hit must have a FlagFor association");
    // the flag entity must carry the rules authoritative score
    let flag = client.entities.get(flag_id).await?;
    let EntityMetadata::Flag(flag_meta) = &flag.metadata else {
        panic!("the flag association must point at a flag entity");
    };
    is!(flag_meta.suspicion, 42);
    is!(
        flag_meta.reasoning.clone(),
        "route test auto flag".to_owned()
    );
    Ok(())
}

/// Verify the generic create path trims hits to their rules groups
#[tokio::test]
async fn generic_create_trims_to_rule_groups() -> Result<(), thorium::Error> {
    // set up an admin client and two groups
    let (client, group_a) = common::setup().await?;
    let group_b = generators::groups(1, &client).await?.remove(0).name;
    // create a real rule that only lives in group a
    let (rule_id, _) = create_rule(&client, vec![group_a.clone()], 0, Vec::new()).await?;
    let sha256 = fake_sha256();
    // create a hit for this rule in both groups through the generic path
    let req = EntityRequest::new(
        generators::gen_string(16),
        generators::gen_yara_hit_meta(rule_id, &sha256),
        vec![group_a.clone(), group_b.clone()],
    );
    let resp = client.entities.create(req).await?;
    // the generic path must trim the hit to the rules authoritative group
    let entity = client.entities.get(resp.id).await?;
    is!(entity.groups.clone(), vec![group_a.clone()]);
    // a hit fully outside its rules groups must be rejected outright
    let disjoint = EntityRequest::new(
        generators::gen_string(16),
        generators::gen_yara_hit_meta(rule_id, fake_sha256()),
        vec![group_b],
    );
    let resp = client.entities.create(disjoint).await;
    fail!(resp, 401);
    Ok(())
}

/// Verify a hit write carrying a live hunt gains both decorations
#[tokio::test]
#[serial]
async fn route_decorates_hunt_hit() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, a sample, and a hunt
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    let hunt_id = create_hunt(&client, vec![group.clone()], rule_id).await?;
    // write a hit through the route decorated with this hunt
    let mut req = write_req(rule_id, &sha256, vec![group.clone()], 0);
    req.hunt = Some(hunt_id);
    let resp = client.entities.create_yara_hit(&req).await?;
    // the hit must land decorated with nothing reported missing
    is!(resp.written.clone(), vec![group.clone()]);
    assert!(!resp.hunt_missing, "a live hunt must not report missing");
    // the hits association walk must anchor all three association kinds
    let opts = AssociationListOpts::default();
    let cursor = client.associations.list_entity(resp.id, &opts).await?;
    let mut saw_created_by = false;
    let mut saw_hit_on = false;
    let mut saw_hunt_hit = false;
    for assoc in &cursor.data {
        match (&assoc.kind, &assoc.other) {
            // the rule created this hit
            (AssociationKind::CreatedBy, AssociationTarget::Entity { id, .. }) => {
                saw_created_by |= *id == rule_id;
            }
            // this hit fired on our sample
            (AssociationKind::YaraRuleHit, AssociationTarget::File(target)) => {
                saw_hit_on |= target == &sha256;
            }
            // the hunt found this hit
            (AssociationKind::HuntHit, AssociationTarget::Entity { id, .. }) => {
                saw_hunt_hit |= *id == hunt_id;
            }
            _ => (),
        }
    }
    assert!(
        saw_created_by,
        "the hit must keep its CreatedBy association"
    );
    assert!(saw_hit_on, "the hit must keep its YaraRuleHit association");
    assert!(saw_hunt_hit, "the hit must gain a HuntHit association");
    // the hit must carry the hunts id tag so hunt browse is one tag filter
    let entity = client.entities.get(resp.id).await?;
    let hunt_tags = entity
        .tags
        .get(Hunt::ID_TAG)
        .expect("the hit must carry the hunt id tag");
    assert!(
        hunt_tags.contains_key(&hunt_id.to_string()),
        "the hunt id tag must carry the hunts uuid"
    );
    // clean up this tests hunt and its queued chunk events
    cleanup_hunt(&client, hunt_id).await?;
    Ok(())
}

/// Verify repeating a decorated write converges its decorations
///
/// The hit entity and the hunt id tag are strictly idempotent; association
/// rows are additive per apply (the platform wide duplicate row gap the
/// deferred reaper covers) so the graph converges as a set of edges.
#[tokio::test]
#[serial]
async fn route_decoration_is_idempotent() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, a sample, and a hunt
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    let hunt_id = create_hunt(&client, vec![group.clone()], rule_id).await?;
    // write the same decorated hit twice like a redelivered chunk would
    let mut req = write_req(rule_id, &sha256, vec![group.clone()], 0);
    req.hunt = Some(hunt_id);
    let resp1 = client.entities.create_yara_hit(&req).await?;
    let resp2 = client.entities.create_yara_hit(&req).await?;
    // both writes must converge on one decorated hit
    is!(resp1.id, resp2.id);
    is!(resp2.written.clone(), vec![group.clone()]);
    assert!(!resp2.hunt_missing, "a live hunt must not report missing");
    // every hunt edge must point at our hunt; association rows are additive
    // per apply (keyed by their insert time platform wide, reaper deferred)
    // so the converged graph is asserted as a set, not a row count
    let opts = AssociationListOpts::default();
    let cursor = client.associations.list_entity(resp1.id, &opts).await?;
    let mut hunt_edges = 0;
    for assoc in &cursor.data {
        // only hunt edges matter to this converge assertion
        if assoc.kind == AssociationKind::HuntHit {
            hunt_edges += 1;
            // a repeated write must never fan out to another hunt
            match &assoc.other {
                AssociationTarget::Entity { id, .. } => is!(*id, hunt_id),
                other => panic!("hunt edge must point at an entity: {other:?}"),
            }
        }
    }
    assert!(hunt_edges >= 1, "the hit must keep its hunt association");
    // the hunt id tag upserts the same row so it must hold exactly one value
    let entity = client.entities.get(resp1.id).await?;
    let hunt_tags = entity
        .tags
        .get(Hunt::ID_TAG)
        .expect("the hit must carry the hunt id tag");
    is!(hunt_tags.len(), 1);
    assert!(
        hunt_tags.contains_key(&hunt_id.to_string()),
        "the hunt id tag must carry the hunts uuid"
    );
    // clean up this tests hunt and its queued chunk events
    cleanup_hunt(&client, hunt_id).await?;
    Ok(())
}

/// Verify a vanished hunt skips decoration without failing the write
#[tokio::test]
async fn route_skips_missing_hunt() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, and a sample
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    // write a hit decorated with a hunt that does not exist (deleted mid scan)
    let mut req = write_req(rule_id, &sha256, vec![group.clone()], 0);
    req.hunt = Some(Uuid::new_v4());
    let resp = client.entities.create_yara_hit(&req).await?;
    // the hit must land with the skipped decoration reported
    is!(resp.written.clone(), vec![group.clone()]);
    assert!(resp.hunt_missing, "a vanished hunt must report missing");
    // the hit must exist but carry no hunt decoration at all
    let entity = client.entities.get(resp.id).await?;
    assert!(
        entity.tags.get(Hunt::ID_TAG).is_none(),
        "a vanished hunt must not tag the hit"
    );
    let opts = AssociationListOpts::default();
    let cursor = client.associations.list_entity(resp.id, &opts).await?;
    let hunt_edges = cursor
        .data
        .iter()
        .filter(|assoc| assoc.kind == AssociationKind::HuntHit)
        .count();
    is!(hunt_edges, 0);
    Ok(())
}

/// Verify a non hunt decoration id is a reported skip, never a failure
#[tokio::test]
async fn route_skips_wrong_kind_hunt() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, and a sample
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    // write a hit whose hunt id names the rule entity instead of a hunt
    let mut req = write_req(rule_id, &sha256, vec![group.clone()], 0);
    req.hunt = Some(rule_id);
    let resp = client.entities.create_yara_hit(&req).await?;
    // the hit must land since the hunt is only decoration, not identity
    is!(resp.written.clone(), vec![group.clone()]);
    assert!(resp.hunt_missing, "a non hunt id must report missing");
    // the hit must carry no hunt id tag from the bad decoration
    let entity = client.entities.get(resp.id).await?;
    assert!(
        entity.tags.get(Hunt::ID_TAG).is_none(),
        "a non hunt id must not tag the hit"
    );
    Ok(())
}

/// Verify a missing sample skip never reports a hunt decoration skip
#[tokio::test]
#[serial]
async fn route_missing_sample_reports_no_hunt_skip() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, and a hunt
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let hunt_id = create_hunt(&client, vec![group.clone()], rule_id).await?;
    // write a decorated hit for a sample that does not exist
    let mut req = write_req(rule_id, &fake_sha256(), vec![group.clone()], 0);
    req.hunt = Some(hunt_id);
    let resp = client.entities.create_yara_hit(&req).await?;
    // the skip must be attributed to the sample, never the live hunt
    assert!(resp.sample_missing, "the sample must report missing");
    assert!(
        !resp.hunt_missing,
        "an unattempted decoration must not report missing"
    );
    assert!(resp.written.is_empty(), "a missing sample writes nothing");
    // clean up this tests hunt and its queued chunk events
    cleanup_hunt(&client, hunt_id).await?;
    Ok(())
}

/// Verify a strictly newer converge refreshes the stored scan facts
#[tokio::test]
async fn converge_refreshes_metadata_newer_wins() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, and a sample
    let (client, group) = common::setup().await?;
    let (rule_id, rule_name) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    // write the first hit with pinned stale scan facts
    let mut stale = write_req(rule_id, &sha256, vec![group.clone()], 0);
    stale.scanned_at = Utc::now();
    let resp = client.entities.create_yara_hit(&stale).await?;
    // write the same identity again with strictly newer scan facts
    let mut fresh = write_req(rule_id, &sha256, vec![group.clone()], 0);
    fresh.rule_hash = "abcd".repeat(16);
    fresh.ruleset_hash = "ef01".repeat(16);
    fresh.engine_version = "yara-x-test-fresh".to_owned();
    fresh.scanned_at = stale.scanned_at + chrono::Duration::hours(1);
    fresh.matched = Vec::default();
    let refreshed = client.entities.create_yara_hit(&fresh).await?;
    is!(resp.id, refreshed.id);
    // the stored metadata must now carry the five fresh scan facts
    let entity = client.entities.get(resp.id).await?;
    let hit = hit_meta(&entity);
    is!(hit.rule_hash.clone(), fresh.rule_hash.clone());
    is!(hit.ruleset_hash.clone(), fresh.ruleset_hash.clone());
    is!(hit.engine_version.clone(), fresh.engine_version.clone());
    is!(hit.scanned_at, fresh.scanned_at);
    assert!(hit.matched.is_empty(), "the fresh matches must be stored");
    // the identity facts and rule name must never move on a refresh
    is!(hit.rule_id, rule_id);
    is!(hit.sha256.clone(), sha256.clone());
    is!(hit.rule_name.clone(), rule_name.clone());
    // the refresh must not duplicate the hits group rows
    is!(entity.groups.clone(), vec![group.clone()]);
    // the stale hash tag row must have been swept with only the fresh left
    let hash_tags = entity
        .tags
        .get(YaraRuleHit::RULE_HASH_TAG)
        .expect("the hit must carry a rule hash tag");
    is!(hash_tags.len(), 1);
    assert!(
        hash_tags.contains_key(&fresh.rule_hash),
        "the fresh hash tag row must exist"
    );
    Ok(())
}

/// Verify stale and equal timestamp redeliveries can never clobber a refresh
#[tokio::test]
async fn converge_stale_write_cannot_clobber() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, and a sample
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    // land a stale write then refresh it with strictly newer facts
    let mut stale = write_req(rule_id, &sha256, vec![group.clone()], 0);
    stale.scanned_at = Utc::now();
    client.entities.create_yara_hit(&stale).await?;
    let mut fresh = write_req(rule_id, &sha256, vec![group.clone()], 0);
    fresh.rule_hash = "abcd".repeat(16);
    fresh.scanned_at = stale.scanned_at + chrono::Duration::hours(1);
    let refreshed = client.entities.create_yara_hit(&fresh).await?;
    // redeliver the original stale write like a reclaimed event would
    client.entities.create_yara_hit(&stale).await?;
    // the stale redelivery must not clobber the fresher stored facts
    let entity = client.entities.get(refreshed.id).await?;
    is!(hit_meta(&entity).rule_hash.clone(), fresh.rule_hash.clone());
    is!(hit_meta(&entity).scanned_at, fresh.scanned_at);
    // an equal timestamp redelivery is idempotent and must not rewrite either
    let mut equal = write_req(rule_id, &sha256, vec![group.clone()], 0);
    equal.rule_hash = "9876".repeat(16);
    equal.scanned_at = fresh.scanned_at;
    client.entities.create_yara_hit(&equal).await?;
    let entity = client.entities.get(refreshed.id).await?;
    is!(hit_meta(&entity).rule_hash.clone(), fresh.rule_hash.clone());
    // the group rows must have stayed single through every redelivery
    is!(entity.groups.clone(), vec![group.clone()]);
    Ok(())
}

/// Verify the generic create path refreshes newer scans and ignores stale
#[tokio::test]
async fn generic_create_refreshes_newer() -> Result<(), thorium::Error> {
    // set up an admin client, a group, a rule, and a sample
    let (client, group) = common::setup().await?;
    let (rule_id, _) = create_rule(&client, vec![group.clone()], 0, Vec::new()).await?;
    let sha256 = upload_bytes(&client, unique_bytes(), vec![group.clone()]).await?;
    // seed this hit through the route with pinned stale facts
    let mut stale = write_req(rule_id, &sha256, vec![group.clone()], 0);
    stale.scanned_at = Utc::now();
    let resp = client.entities.create_yara_hit(&stale).await?;
    // build a generic multipart create carrying strictly newer facts
    let mut newer = match generators::gen_yara_hit_meta(rule_id, &sha256) {
        EntityMetadataRequest::YaraRuleHit(hit) => hit,
        _ => unreachable!("gen_yara_hit_meta must build a yara rule hit"),
    };
    newer.rule_hash = "abcd".repeat(16);
    newer.scanned_at = stale.scanned_at + chrono::Duration::hours(1);
    let newer_at = newer.scanned_at;
    let req = EntityRequest::new(
        generators::gen_string(16),
        EntityMetadataRequest::YaraRuleHit(newer),
        vec![group.clone()],
    );
    let merged = client.entities.create(req).await?;
    is!(resp.id, merged.id);
    // the generic converge must have refreshed the stored facts
    let entity = client.entities.get(resp.id).await?;
    is!(hit_meta(&entity).rule_hash.clone(), "abcd".repeat(16));
    is!(hit_meta(&entity).scanned_at, newer_at);
    // a stale generic create must converge without clobbering the facts
    let mut older = match generators::gen_yara_hit_meta(rule_id, &sha256) {
        EntityMetadataRequest::YaraRuleHit(hit) => hit,
        _ => unreachable!("gen_yara_hit_meta must build a yara rule hit"),
    };
    older.rule_hash = "9876".repeat(16);
    older.scanned_at = stale.scanned_at;
    let req = EntityRequest::new(
        generators::gen_string(16),
        EntityMetadataRequest::YaraRuleHit(older),
        vec![group.clone()],
    );
    client.entities.create(req).await?;
    let entity = client.entities.get(resp.id).await?;
    is!(hit_meta(&entity).rule_hash.clone(), "abcd".repeat(16));
    Ok(())
}

/// Verify one write can merge a new group and refresh the facts together
#[tokio::test]
async fn route_merges_groups_and_refreshes_in_one_write() -> Result<(), thorium::Error> {
    // set up an admin client and two groups
    let (client, group_a) = common::setup().await?;
    let group_b = generators::groups(1, &client).await?.remove(0).name;
    // create a rule in both groups and one sample visible to both
    let (rule_id, _) = create_rule(
        &client,
        vec![group_a.clone(), group_b.clone()],
        0,
        Vec::new(),
    )
    .await?;
    let bytes = unique_bytes();
    let sha256 = upload_bytes(&client, bytes.clone(), vec![group_a.clone()]).await?;
    upload_bytes(&client, bytes, vec![group_b.clone()]).await?;
    // land the first hit in group a with pinned stale facts
    let mut stale = write_req(rule_id, &sha256, vec![group_a.clone()], 0);
    stale.scanned_at = Utc::now();
    let resp_a = client.entities.create_yara_hit(&stale).await?;
    // write the group b event with strictly newer scan facts
    let mut fresh = write_req(rule_id, &sha256, vec![group_b.clone()], 0);
    fresh.rule_hash = "abcd".repeat(16);
    fresh.scanned_at = stale.scanned_at + chrono::Duration::hours(1);
    let resp_b = client.entities.create_yara_hit(&fresh).await?;
    is!(resp_a.id, resp_b.id);
    // one write must have merged the group and refreshed the facts
    let entity = client.entities.get(resp_a.id).await?;
    assert!(entity.groups.contains(&group_a), "hit lost group a");
    assert!(entity.groups.contains(&group_b), "hit did not gain group b");
    is!(hit_meta(&entity).rule_hash.clone(), fresh.rule_hash.clone());
    is!(hit_meta(&entity).scanned_at, fresh.scanned_at);
    Ok(())
}
