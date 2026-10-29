//! Integration tests for `Hunt` entities: create, freeze, quotas, and cancel
//!
//! Every test is `#[serial]` because hunts share two pieces of cluster global
//! state: the `HuntActive` quota census and the `YaraHuntChunk` event queue.

mod common;

use chrono::{TimeZone, Utc};
use serial_test::serial;
use std::time::Duration;
use uuid::Uuid;

use thorium::models::{
    Entity, EntityKinds, EntityListOpts, EntityMetadata, EntityRequest, EntityUpdate, EventData,
    EventIds, EventPopOpts, EventType, Hunt, HuntCheckpoint, HuntCorpusBoundsRequest,
    HuntCorpusRequest, HuntCounters, HuntCreateRequest, HuntEngine, HuntRuleListOpts, HuntSelector,
    HuntStatus, HuntStatusOpts, TagRequest, UserRole, YaraRule,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, fail, is};

/// Pull the hunt metadata off an entity or panic
///
/// # Arguments
///
/// * `entity` - The entity to pull hunt metadata from
fn hunt_meta(entity: &Entity) -> &thorium::models::Hunt {
    // make sure this entity carries hunt metadata
    match &entity.metadata {
        EntityMetadata::Hunt(hunt) => hunt,
        meta => panic!("entity carries the wrong metadata kind: {meta:?}"),
    }
}

/// Create a yara rule entity in a group and return its id
///
/// # Arguments
///
/// * `client` - The client to create this rule with
/// * `group` - The group this rule should be in
async fn create_yara_rule(client: &Thorium, group: &str) -> Result<Uuid, Error> {
    // build the entity request for this rule with a random name
    let req = EntityRequest::new(
        generators::gen_string(16),
        generators::gen_yara_meta(),
        vec![group.to_owned()],
    );
    // create this rule entity
    let resp = client.entities.create(req).await?;
    Ok(resp.id)
}

/// Build a hunt create request over some rule ids
///
/// # Arguments
///
/// * `groups` - The groups this hunt should be bounded to
/// * `selector` - The selector choosing this hunts rules
fn hunt_req(groups: Vec<String>, selector: HuntSelector) -> HuntCreateRequest {
    HuntCreateRequest {
        name: generators::gen_string(16),
        groups,
        engine: HuntEngine::Yara,
        selector,
        corpus: None,
        description: Some(generators::gen_string(32)),
    }
}

/// Pop every chunk event for some hunts, clearing everything popped
///
/// Test hunts have no worker so their chunk events sit queued; this helper
/// drains the queue, keeps the events for our hunts, and clears every popped
/// event (ours and other test hunts') so nothing leaks into the in flight
/// queue. Tests with several hunts must drain them in one call: the first
/// drain clears every popped event, so a second call would just burn its
/// whole retry budget on an already empty queue.
///
/// # Arguments
///
/// * `client` - A Thorium client
/// * `hunts` - The hunts whose chunk events we want
/// * `expected` - How many chunk events these hunts should have emitted in total
async fn pop_chunk_events(
    client: &Thorium,
    hunts: &[Uuid],
    expected: usize,
) -> Result<Vec<thorium::models::Event>, Error> {
    // pop in large pages to cut round trips
    let opts = EventPopOpts::default().limit(200);
    // the chunk events we found for our hunt
    let mut found = Vec::default();
    // every event id we popped so we can clear them all
    let mut popped = Vec::default();
    // search a bounded number of pages so a missing event fails instead of hanging
    for _ in 0..20 {
        // pop a page of yara hunt chunk events
        let events = client.events.pop(EventType::YaraHuntChunk, &opts).await?;
        // an empty page may just mean our events are younger than the pop
        // maturity window so wait for them to mature instead of bailing early
        if events.is_empty() {
            // stop early once we have every expected event
            if found.len() >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        // search this page for our hunts chunk events
        for event in events {
            // track this event so we can clear it
            popped.push(event.id);
            // keep this event if it belongs to one of our hunts
            if let EventData::YaraHuntChunk(chunk) = &event.data
                && hunts.contains(&chunk.hunt)
            {
                found.push(event);
            }
        }
    }
    // clear every event we popped so nothing leaks in flight
    if !popped.is_empty() {
        let ids = EventIds::from(popped);
        client
            .events
            .clear(EventType::YaraHuntChunk, &ids, None)
            .await?;
    }
    Ok(found)
}

/// Verify an explicit rules selector freezes, tags, seeds, and emits
#[tokio::test]
#[serial]
async fn create_rules_selector_freezes_and_emits() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create two rules for this hunt to freeze
    let rule_one = create_yara_rule(&client, &group).await?;
    let rule_two = create_yara_rule(&client, &group).await?;
    // create a hunt over those rules
    let req = hunt_req(
        vec![group.clone()],
        HuntSelector::Rules(vec![rule_one, rule_two]),
    );
    let before = Utc::now();
    let entity = client.entities.create_hunt(&req).await?;
    // the frozen snapshot must hold exactly our rules with nothing excluded
    let hunt = hunt_meta(&entity);
    assert_eq!(hunt.rules.len(), 2);
    assert!(hunt.rules.contains(&rule_one));
    assert!(hunt.rules.contains(&rule_two));
    assert_eq!(hunt.excluded_quarantined, 0);
    assert_eq!(hunt.excluded_foreign, 0);
    // the hunt must start its lifecycle in created with default counters
    assert_eq!(hunt.status, HuntStatus::Created);
    assert_eq!(hunt.counters.listed, 0);
    // the corpus must clamp to the creation instant and default to the
    // configured earliest file timestamp with the hunts own groups
    let bounds = hunt.corpus.bounds();
    assert!(bounds.start >= before && bounds.start <= Utc::now());
    let earliest = Utc
        .timestamp_opt(test_utilities::CONF.thorium.files.earliest, 0)
        .unwrap();
    assert_eq!(bounds.end, earliest);
    assert_eq!(bounds.groups, vec![group.clone()]);
    // the window must split into the configured number of lanes
    let lanes = test_utilities::CONF.thorium.hunts.lanes;
    assert_eq!(hunt.lanes.len(), lanes as usize);
    // the response must carry the intrinsic status/engine/active tags
    assert!(
        entity
            .tags
            .get("HuntStatus")
            .is_some_and(|values| values.contains_key("Created"))
    );
    assert!(
        entity
            .tags
            .get("HuntActive")
            .is_some_and(|values| values.contains_key("true"))
    );
    assert!(
        entity
            .tags
            .get("HuntEngine")
            .is_some_and(|values| values.contains_key("yara"))
    );
    // every lanes checkpoint sequence key must be seeded at 0
    for lane in 0..lanes {
        let seq = test_utilities::hunt_lane_seq(&entity.id, lane).await?;
        assert_eq!(seq, Some(0), "lane {lane} sequence was not seeded");
    }
    // one depth 0 first chunk event per lane must land on the yara hunt queue
    let events = pop_chunk_events(&client, &[entity.id], lanes as usize).await?;
    assert_eq!(events.len(), lanes as usize);
    // collect the lanes the chunk events cover
    let mut seen_lanes = Vec::default();
    for event in &events {
        // every chunk event starts trigger chains at depth 0 with seq 0
        assert_eq!(event.depth, 0);
        let EventData::YaraHuntChunk(chunk) = &event.data else {
            panic!("popped a non chunk event from the chunk queue");
        };
        assert_eq!(chunk.seq, 0);
        seen_lanes.push(chunk.lane);
    }
    // every lane must have exactly one first chunk event
    seen_lanes.sort_unstable();
    let expected: Vec<u32> = (0..lanes).collect();
    assert_eq!(seen_lanes, expected);
    // clean up our active hunt so later quota tests see a quiet cluster
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify a tag selector freezes only rules carrying the tag
#[tokio::test]
#[serial]
async fn create_tag_selector() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a distinctive tag for this test
    let (key, value) = (generators::gen_string(12), generators::gen_string(12));
    // create a tagged rule and an untagged rule
    let tagged_req = EntityRequest::new(
        generators::gen_string(16),
        generators::gen_yara_meta(),
        vec![group.clone()],
    )
    .tag(&key, &value);
    let tagged = client.entities.create(tagged_req).await?.id;
    let untagged = create_yara_rule(&client, &group).await?;
    // create a hunt selecting rules by that tag
    let req = hunt_req(vec![group.clone()], HuntSelector::Tag { key, value });
    let entity = client.entities.create_hunt(&req).await?;
    // only the tagged rule may freeze
    let hunt = hunt_meta(&entity);
    assert_eq!(hunt.rules, vec![tagged]);
    assert!(!hunt.rules.contains(&untagged));
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify an all selector freezes only the engines rule kind under the creator
#[tokio::test]
#[serial]
async fn create_all_selector_scoped_to_engine_kind() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a non admin creator confined to this fresh group; an admin
    // creator would freeze every yara rule other suites created
    let creator = common::member(&client, &[&group]).await?;
    // create two yara rules and a sigma rule in this group
    let yara_one = create_yara_rule(&client, &group).await?;
    let yara_two = create_yara_rule(&client, &group).await?;
    let sigma_req = EntityRequest::new(
        generators::gen_string(16),
        generators::gen_sigma_meta(),
        vec![group.clone()],
    );
    client.entities.create(sigma_req).await?;
    // create an all selector hunt as the confined creator
    let req = hunt_req(vec![group.clone()], HuntSelector::All);
    let entity = creator.entities.create_hunt(&req).await?;
    // only the yara rules may freeze and the sigma rule must not count as
    // foreign since all selectors only ever resolve the engines kind
    let hunt = hunt_meta(&entity);
    assert_eq!(hunt.rules.len(), 2);
    assert!(hunt.rules.contains(&yara_one));
    assert!(hunt.rules.contains(&yara_two));
    assert_eq!(hunt.excluded_foreign, 0);
    // clean up our active hunt
    creator.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify a rule set selector counts foreign engine members
#[tokio::test]
#[serial]
async fn create_ruleset_selector_counts_foreign() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a yara rule and a sigma rule to share a rule set
    let yara = create_yara_rule(&client, &group).await?;
    let sigma_req = EntityRequest::new(
        generators::gen_string(16),
        generators::gen_sigma_meta(),
        vec![group.clone()],
    );
    let sigma = client.entities.create(sigma_req).await?.id;
    // create a mixed engine rule set holding both rules
    let ruleset_req =
        generators::gen_entity(&group, generators::gen_ruleset_meta(vec![yara, sigma]));
    let ruleset = client.entities.create(ruleset_req).await?.id;
    // create a yara hunt from that rule set
    let req = hunt_req(vec![group.clone()], HuntSelector::RuleSet(ruleset));
    let entity = client.entities.create_hunt(&req).await?;
    // only the yara member may freeze with the sigma member counted foreign
    let hunt = hunt_meta(&entity);
    assert_eq!(hunt.rules, vec![yara]);
    assert_eq!(hunt.excluded_foreign, 1);
    assert_eq!(hunt.excluded_quarantined, 0);
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify quarantined rules are excluded and counted but broken rules freeze
#[tokio::test]
#[serial]
async fn create_excludes_quarantined_counts() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a noisy quarantined rule, a broken tagged rule, and a clean rule
    let noisy = create_yara_rule(&client, &group).await?;
    let broken = create_yara_rule(&client, &group).await?;
    let clean = create_yara_rule(&client, &group).await?;
    // quarantine the noisy rule exactly like the scanner would
    let noisy_tag = TagRequest::<Entity>::default()
        .groups(vec![group.clone()])
        .add(YaraRule::STATUS_TAG, YaraRule::STATUS_NOISY);
    client.entities.tag(noisy, &noisy_tag).await?;
    // tag the broken rule exactly like the scanner would
    let broken_tag = TagRequest::<Entity>::default()
        .groups(vec![group.clone()])
        .add(YaraRule::STATUS_TAG, "Broken");
    client.entities.tag(broken, &broken_tag).await?;
    // create a hunt explicitly naming all three rules
    let req = hunt_req(
        vec![group.clone()],
        HuntSelector::Rules(vec![noisy, broken, clean]),
    );
    let entity = client.entities.create_hunt(&req).await?;
    // the noisy rule must be excluded and counted while the broken tagged
    // rule still freezes exactly like it scans
    let hunt = hunt_meta(&entity);
    assert_eq!(hunt.rules.len(), 2);
    assert!(hunt.rules.contains(&broken));
    assert!(hunt.rules.contains(&clean));
    assert!(!hunt.rules.contains(&noisy));
    assert_eq!(hunt.excluded_quarantined, 1);
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify an exempted rule freezes after exemption cleared its quarantine
#[tokio::test]
#[serial]
async fn create_includes_exempted_rule() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule and quarantine it exactly like the scanner would
    let rule = create_yara_rule(&client, &group).await?;
    let noisy_tag = TagRequest::<Entity>::default()
        .groups(vec![group.clone()])
        .add(YaraRule::STATUS_TAG, YaraRule::STATUS_NOISY);
    client.entities.tag(rule, &noisy_tag).await?;
    // exempt this rule which also clears its noisy quarantine status
    client.entities.set_quarantine_exemption(rule).await?;
    // create a hunt explicitly naming the exempted rule
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let entity = client.entities.create_hunt(&req).await?;
    // the exempted rule must freeze with nothing excluded as quarantined
    let hunt = hunt_meta(&entity);
    assert_eq!(hunt.rules, vec![rule]);
    assert_eq!(hunt.excluded_quarantined, 0);
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify exemption beats a stale quarantine tag at the snapshot filter
///
/// A scanner worker racing the admin's exemption can leave a status tag on
/// an already exempt rule; the snapshot filter must honor the exemption so
/// the rule still freezes into new hunts instead of being silently benched.
#[tokio::test]
#[serial]
async fn create_includes_exempt_rule_with_stale_tag() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule and exempt it before any quarantine exists
    let rule = create_yara_rule(&client, &group).await?;
    client.entities.set_quarantine_exemption(rule).await?;
    // tag the exempt rule like a racing scanner worker would
    let noisy_tag = TagRequest::<Entity>::default()
        .groups(vec![group.clone()])
        .add(YaraRule::STATUS_TAG, YaraRule::STATUS_NOISY);
    client.entities.tag(rule, &noisy_tag).await?;
    // create a hunt explicitly naming the exempt but stale tagged rule
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let entity = client.entities.create_hunt(&req).await?;
    // the exemption must win: the rule freezes with nothing excluded
    let hunt = hunt_meta(&entity);
    assert_eq!(hunt.rules, vec![rule]);
    assert_eq!(hunt.excluded_quarantined, 0);
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify a hunt naming a nonexistent rule is a 404
#[tokio::test]
#[serial]
async fn create_missing_rule_404() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a hunt naming a rule that does not exist
    let req = hunt_req(vec![group], HuntSelector::Rules(vec![Uuid::new_v4()]));
    let resp = client.entities.create_hunt(&req).await;
    fail!(resp, 404);
    Ok(())
}

/// Verify a hunt explicitly naming a non yara rule is a 400
#[tokio::test]
#[serial]
async fn create_non_rule_in_rules_400() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a sigma rule which is not the yara engines kind
    let sigma_req = EntityRequest::new(
        generators::gen_string(16),
        generators::gen_sigma_meta(),
        vec![group.clone()],
    );
    let sigma = client.entities.create(sigma_req).await?.id;
    // explicitly naming a foreign engines rule is a user error not a count
    let req = hunt_req(vec![group], HuntSelector::Rules(vec![sigma]));
    let resp = client.entities.create_hunt(&req).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify a selector resolving to zero usable rules is a 400
#[tokio::test]
#[serial]
async fn create_empty_snapshot_400() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule and quarantine it as slow
    let slow = create_yara_rule(&client, &group).await?;
    let slow_tag = TagRequest::<Entity>::default()
        .groups(vec![group.clone()])
        .add(YaraRule::STATUS_TAG, YaraRule::STATUS_SLOW);
    client.entities.tag(slow, &slow_tag).await?;
    // a hunt whose only rule is quarantined can never produce work
    let req = hunt_req(vec![group], HuntSelector::Rules(vec![slow]));
    let resp = client.entities.create_hunt(&req).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify a repos corpus is rejected until an engine scans repos
#[tokio::test]
#[serial]
async fn create_repos_corpus_400() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule so the selector itself is valid
    let rule = create_yara_rule(&client, &group).await?;
    // build a hunt walking the repo listing
    let mut req = hunt_req(vec![group], HuntSelector::Rules(vec![rule]));
    req.corpus = Some(HuntCorpusRequest::Repos(HuntCorpusBoundsRequest::default()));
    // no engine scans repos yet so this must be a 400
    let resp = client.entities.create_hunt(&req).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify corpus groups outside the hunts groups are a 400
#[tokio::test]
#[serial]
async fn create_corpus_groups_outside_hunt_400() -> Result<(), Error> {
    // set up an admin client and two groups
    let (client, group) = common::setup().await?;
    let other_group = generators::groups(1, &client).await?.remove(0).name;
    // create a rule so the selector itself is valid
    let rule = create_yara_rule(&client, &group).await?;
    // build a hunt whose corpus names a group outside the hunts groups
    let mut req = hunt_req(vec![group], HuntSelector::Rules(vec![rule]));
    req.corpus = Some(HuntCorpusRequest::Files(HuntCorpusBoundsRequest {
        groups: vec![other_group],
        ..HuntCorpusBoundsRequest::default()
    }));
    // corpus groups must stay a subset of the hunts groups
    let resp = client.entities.create_hunt(&req).await;
    fail!(resp, 400);
    Ok(())
}

/// Verify a future corpus start clamps to the creation instant
#[tokio::test]
#[serial]
async fn create_clamps_future_start() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule so the selector itself is valid
    let rule = create_yara_rule(&client, &group).await?;
    // build a hunt whose corpus claims to start an hour in the future
    let mut req = hunt_req(vec![group], HuntSelector::Rules(vec![rule]));
    req.corpus = Some(HuntCorpusRequest::Files(HuntCorpusBoundsRequest {
        start: Some(Utc::now() + chrono::Duration::hours(1)),
        ..HuntCorpusBoundsRequest::default()
    }));
    let entity = client.entities.create_hunt(&req).await?;
    // the stored start must be clamped to the creation instant since the scan
    // path owns everything after it
    let hunt = hunt_meta(&entity);
    assert!(hunt.corpus.bounds().start <= Utc::now());
    // the newest lane must start exactly at the clamped corpus start
    assert_eq!(hunt.lanes[0].start, hunt.corpus.bounds().start);
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify the per user quota rejects creates and cancel frees the slot
#[tokio::test]
#[serial]
async fn quota_per_user_enforced_and_freed_by_cancel() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a dedicated user so other suites hunts can never interfere
    let creator = common::member(&client, &[&group]).await?;
    // create a rule for every hunt to freeze
    let rule = create_yara_rule(&client, &group).await?;
    // fill this users quota with active hunts
    let cap = test_utilities::CONF.thorium.hunts.max_active_per_user;
    let mut active = Vec::default();
    for _ in 0..cap {
        let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
        active.push(creator.entities.create_hunt(&req).await?.id);
    }
    // one more create must trip the per user quota
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let resp = creator.entities.create_hunt(&req).await;
    fail!(resp, 400);
    // cancelling one hunt must free its quota slot
    let freed = active.pop().expect("no active hunts to cancel");
    creator.entities.cancel_hunt(freed).await?;
    // the same create must now succeed; this also proves the cancel tag swap
    // feeds the quota census
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let replacement = creator.entities.create_hunt(&req).await?;
    active.push(replacement.id);
    // clean up every active hunt we created
    for hunt in active {
        creator.entities.cancel_hunt(hunt).await?;
    }
    Ok(())
}

/// Verify the global quota rejects creates across users
#[tokio::test]
#[serial]
async fn quota_global_enforced() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule for every hunt to freeze
    let rule = create_yara_rule(&client, &group).await?;
    // get the quota caps from the test config
    let per_user = test_utilities::CONF.thorium.hunts.max_active_per_user;
    let global = test_utilities::CONF.thorium.hunts.max_active_global;
    // create enough users to fill the global quota under the per user cap
    let users_needed = global.div_ceil(per_user);
    let mut active = Vec::default();
    'fill: for _ in 0..users_needed {
        // create a dedicated user for this quota slice
        let creator = common::member(&client, &[&group]).await?;
        // fill this users per user quota
        for _ in 0..per_user {
            // stop once the global quota is exactly full
            if active.len() >= global {
                break 'fill;
            }
            let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
            active.push((
                creator.clone(),
                creator.entities.create_hunt(&req).await?.id,
            ));
        }
    }
    // a fresh users create must now trip the global quota
    let extra = common::member(&client, &[&group]).await?;
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let resp = extra.entities.create_hunt(&req).await;
    fail!(resp, 400);
    // clean up every active hunt we created
    for (creator, hunt) in active {
        creator.entities.cancel_hunt(hunt).await?;
    }
    Ok(())
}

/// Verify cancel flips the status and swaps the active tags
#[tokio::test]
#[serial]
async fn cancel_flips_status_and_tags() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule and an active hunt over it
    let rule = create_yara_rule(&client, &group).await?;
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let entity = client.entities.create_hunt(&req).await?;
    // cancel this hunt
    let resp = client.entities.cancel_hunt(entity.id).await?;
    assert_eq!(resp.status().as_u16(), 204);
    // get the hunt back and make sure its lifecycle flipped
    let cancelled = client.entities.get(entity.id).await?;
    assert_eq!(hunt_meta(&cancelled).status, HuntStatus::Cancelled);
    // the active tag must be gone so the quota census frees the slot
    assert!(cancelled.tags.get("HuntActive").is_none());
    // the status tag must carry exactly the cancelled value
    let status_values = cancelled
        .tags
        .get("HuntStatus")
        .expect("cancelled hunt lost its status tag");
    assert!(status_values.contains_key("Cancelled"));
    assert!(!status_values.contains_key("Created"));
    assert!(!status_values.contains_key("Running"));
    // the engine tag must survive the cancel
    assert!(
        cancelled
            .tags
            .get("HuntEngine")
            .is_some_and(|values| values.contains_key("yara"))
    );
    Ok(())
}

/// Verify re-cancelling an already cancelled hunt is an idempotent 204
#[tokio::test]
#[serial]
async fn cancel_idempotent_204() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule and an active hunt over it
    let rule = create_yara_rule(&client, &group).await?;
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let entity = client.entities.create_hunt(&req).await?;
    // cancel this hunt twice
    client.entities.cancel_hunt(entity.id).await?;
    let resp = client.entities.cancel_hunt(entity.id).await?;
    // the second cancel must be an idempotent 204
    assert_eq!(resp.status().as_u16(), 204);
    Ok(())
}

/// Verify only the submitter or an admin may cancel a hunt
#[tokio::test]
#[serial]
async fn cancel_requires_submitter_or_admin() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a non admin submitter and a non admin stranger in the group
    let submitter = common::member(&client, &[&group]).await?;
    let stranger = common::member(&client, &[&group]).await?;
    // create a rule and an active hunt as the submitter
    let rule = create_yara_rule(&client, &group).await?;
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let entity = submitter.entities.create_hunt(&req).await?;
    // a group member who is not the submitter must not be able to cancel
    let resp = stranger.entities.cancel_hunt(entity.id).await;
    fail!(resp, 401);
    // an admin must be able to cancel any hunt
    let resp = client.entities.cancel_hunt(entity.id).await?;
    assert_eq!(resp.status().as_u16(), 204);
    Ok(())
}

/// Verify a hunts frozen snapshot survives its rule sets deletion
#[tokio::test]
#[serial]
async fn hunt_survives_ruleset_delete() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule and a rule set holding it
    let rule = create_yara_rule(&client, &group).await?;
    let ruleset_req = generators::gen_entity(&group, generators::gen_ruleset_meta(vec![rule]));
    let ruleset = client.entities.create(ruleset_req).await?.id;
    // create a hunt frozen from that rule set
    let req = hunt_req(vec![group.clone()], HuntSelector::RuleSet(ruleset));
    let entity = client.entities.create_hunt(&req).await?;
    assert_eq!(hunt_meta(&entity).rules, vec![rule]);
    // delete the rule set out from under the hunt
    client.entities.delete(ruleset).await?;
    // the hunts frozen snapshot must be unaffected since it froze ids
    let after = client.entities.get(entity.id).await?;
    assert_eq!(hunt_meta(&after).rules, vec![rule]);
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify the generic update path cannot touch hunt metadata
#[tokio::test]
#[serial]
async fn generic_update_cannot_touch_hunt_metadata() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule and an active hunt over it
    let rule = create_yara_rule(&client, &group).await?;
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let entity = client.entities.create_hunt(&req).await?;
    // apply a generic update touching the name and description
    let update = EntityUpdate::default()
        .name(generators::gen_string(16))
        .description(generators::gen_string(32));
    client.entities.update(entity.id, update.clone()).await?;
    // the generic fields must update while the hunt metadata is untouched
    let updated = client.entities.get(entity.id).await?;
    assert_eq!(updated.name, update.name.clone().unwrap());
    assert_eq!(hunt_meta(&updated).rules, hunt_meta(&entity).rules);
    assert_eq!(hunt_meta(&updated).status, HuntStatus::Created);
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify listing a hunts frozen rules pages in frozen order
#[tokio::test]
#[serial]
async fn list_hunt_rules_pages_in_frozen_order() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create three rules for this hunt to freeze
    let rule_one = create_yara_rule(&client, &group).await?;
    let rule_two = create_yara_rule(&client, &group).await?;
    let rule_three = create_yara_rule(&client, &group).await?;
    // create a hunt explicitly naming all three rules
    let req = hunt_req(
        vec![group.clone()],
        HuntSelector::Rules(vec![rule_one, rule_two, rule_three]),
    );
    let entity = client.entities.create_hunt(&req).await?;
    // the frozen snapshot is the authoritative order the listing must follow
    let frozen = hunt_meta(&entity).rules.clone();
    assert_eq!(frozen.len(), 3);
    // list this hunts frozen rules two lines at a time
    let opts = HuntRuleListOpts::default().page_size(2).limit(10);
    let mut cursor = client.entities.list_hunt_rules(entity.id, &opts).await?;
    // the first page must hold the first two frozen lines in frozen order
    assert_eq!(cursor.data.len(), 2);
    assert_eq!(cursor.data[0].id, frozen[0]);
    assert_eq!(cursor.data[1].id, frozen[1]);
    // every line on this page is visible to the admin so it must hydrate
    assert!(cursor.data.iter().all(|line| line.name.is_some()));
    // the wire cursor must be the next frozen index encoded as a uuid
    assert_eq!(cursor.id, Some(Uuid::from_u128(2)));
    assert!(!cursor.exhausted());
    // refilling must deliver the final frozen line and exhaust the cursor
    cursor.refill().await?;
    assert_eq!(cursor.data.len(), 1);
    assert_eq!(cursor.data[0].id, frozen[2]);
    assert!(cursor.exhausted());
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify rules deleted after the freeze list as id only lines
#[tokio::test]
#[serial]
async fn list_hunt_rules_marks_deleted_id_only() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create two rules for this hunt to freeze
    let doomed = create_yara_rule(&client, &group).await?;
    let survivor = create_yara_rule(&client, &group).await?;
    // create a hunt over those rules
    let req = hunt_req(
        vec![group.clone()],
        HuntSelector::Rules(vec![doomed, survivor]),
    );
    let entity = client.entities.create_hunt(&req).await?;
    let frozen = hunt_meta(&entity).rules.clone();
    assert_eq!(frozen.len(), 2);
    // delete one frozen rule out from under the hunt
    client.entities.delete(doomed).await?;
    // list this hunts frozen rules
    let opts = HuntRuleListOpts::default();
    let cursor = client.entities.list_hunt_rules(entity.id, &opts).await?;
    // both lines must come back in frozen order
    assert_eq!(cursor.data.len(), 2);
    assert_eq!(cursor.data[0].id, frozen[0]);
    assert_eq!(cursor.data[1].id, frozen[1]);
    // check each line against its rules fate
    for line in &cursor.data {
        if line.id == doomed {
            // the deleted rule must render as an id only line
            assert_eq!(line.name, None);
            assert_eq!(line.kind, None);
            assert_eq!(line.created, None);
            assert!(line.status.is_empty());
            assert!(!line.quarantine_exempt);
        } else {
            // the surviving rule must hydrate
            assert!(line.name.is_some());
            assert_eq!(line.kind, Some(EntityKinds::YaraRule));
            assert!(line.created.is_some());
        }
    }
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify frozen members a caller cannot see list as id only lines
#[tokio::test]
#[serial]
async fn list_hunt_rules_hides_invisible_members() -> Result<(), Error> {
    // set up an admin client and a shared group
    let (client, shared_group) = common::setup().await?;
    // create a second group the member will never join
    let hidden_group = generators::groups(1, &client).await?.remove(0).name;
    // create a non admin member of only the shared group
    let user = common::member(&client, &[&shared_group]).await?;
    // create a visible rule in the shared group and a hidden rule outside it
    let visible = create_yara_rule(&client, &shared_group).await?;
    let hidden = create_yara_rule(&client, &hidden_group).await?;
    // create a hunt in the shared group freezing both rules as the admin so
    // the member can see the hunt but not every frozen member
    let req = hunt_req(
        vec![shared_group.clone()],
        HuntSelector::Rules(vec![visible, hidden]),
    );
    let entity = client.entities.create_hunt(&req).await?;
    let frozen = hunt_meta(&entity).rules.clone();
    assert_eq!(frozen.len(), 2);
    // list this hunts frozen rules as the member
    let opts = HuntRuleListOpts::default();
    let cursor = user.entities.list_hunt_rules(entity.id, &opts).await?;
    // both lines must come back in frozen order
    assert_eq!(cursor.data.len(), 2);
    assert_eq!(cursor.data[0].id, frozen[0]);
    assert_eq!(cursor.data[1].id, frozen[1]);
    // check each line against its rules visibility
    for line in &cursor.data {
        if line.id == hidden {
            // the invisible rule must render as an id only line
            assert_eq!(line.name, None);
            assert_eq!(line.kind, None);
            assert_eq!(line.created, None);
            assert!(line.status.is_empty());
            assert!(!line.quarantine_exempt);
        } else {
            // the visible rule must hydrate
            assert!(line.name.is_some());
            assert_eq!(line.kind, Some(EntityKinds::YaraRule));
            assert!(line.created.is_some());
        }
    }
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify lines carry current quarantine status and exemption flags
#[tokio::test]
#[serial]
async fn list_hunt_rules_carries_status_and_exemption() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create two clean rules for this hunt to freeze
    let noisy = create_yara_rule(&client, &group).await?;
    let exempt = create_yara_rule(&client, &group).await?;
    // create a hunt over those rules while both are still clean
    let req = hunt_req(
        vec![group.clone()],
        HuntSelector::Rules(vec![noisy, exempt]),
    );
    let entity = client.entities.create_hunt(&req).await?;
    assert_eq!(hunt_meta(&entity).rules.len(), 2);
    // quarantine the first rule exactly like the scanner would after the freeze
    let noisy_tag = TagRequest::<Entity>::default()
        .groups(vec![group.clone()])
        .add(YaraRule::STATUS_TAG, YaraRule::STATUS_NOISY);
    client.entities.tag(noisy, &noisy_tag).await?;
    // exempt the second rule from quarantine
    client.entities.set_quarantine_exemption(exempt).await?;
    // list this hunts frozen rules
    let opts = HuntRuleListOpts::default();
    let cursor = client.entities.list_hunt_rules(entity.id, &opts).await?;
    // both frozen rules must have a line
    assert_eq!(cursor.data.len(), 2);
    assert!(cursor.data.iter().any(|line| line.id == noisy));
    assert!(cursor.data.iter().any(|line| line.id == exempt));
    // check each line against its rules current state
    for line in &cursor.data {
        if line.id == noisy {
            // the quarantined line must carry its current status values
            assert_eq!(line.status, vec![YaraRule::STATUS_NOISY]);
            assert!(!line.quarantine_exempt);
        } else {
            // the exempted line must carry the metadata flag with no status
            assert!(line.status.is_empty());
            assert!(line.quarantine_exempt);
        }
    }
    // clean up our active hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify listing frozen rules for a missing hunt is a 404
#[tokio::test]
#[serial]
async fn list_hunt_rules_404_unknown() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, _group) = common::setup().await?;
    // list frozen rules for a hunt that does not exist
    let opts = HuntRuleListOpts::default();
    // drop the cursor on success since Cursor does not implement Debug
    let resp = client
        .entities
        .list_hunt_rules(Uuid::new_v4(), &opts)
        .await
        .map(|_| ());
    fail!(resp, 404);
    Ok(())
}

/// Verify listing frozen rules for a non hunt entity is a 400
#[tokio::test]
#[serial]
async fn list_hunt_rules_400_non_hunt() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule which is not a hunt
    let rule = create_yara_rule(&client, &group).await?;
    // list frozen rules for the rule instead of a hunt
    let opts = HuntRuleListOpts::default();
    // drop the cursor on success since Cursor does not implement Debug
    let resp = client
        .entities
        .list_hunt_rules(rule, &opts)
        .await
        .map(|_| ());
    fail!(resp, 400);
    Ok(())
}

/// Build a checkpoint request for one lane with default optional fields
///
/// # Arguments
///
/// * `lane` - The lane this checkpoint advances
/// * `seq` - The sequence this checkpoint commits
/// * `current` - The oldest upload timestamp this chunk processed
fn checkpoint_req(lane: u32, seq: u64, current: chrono::DateTime<Utc>) -> HuntCheckpoint {
    HuntCheckpoint {
        lane,
        seq,
        current,
        cursor: None,
        exhausted: false,
        failed: None,
        counters: HuntCounters::default(),
    }
}

/// Create a hunt over a fresh rule and drain its first chunk events
///
/// # Arguments
///
/// * `client` - The client to create this hunt with
/// * `group` - The group this hunt should be bounded to
async fn checkpoint_hunt_setup(client: &Thorium, group: &str) -> Result<Entity, Error> {
    // create a rule and a hunt over it
    let rule = create_yara_rule(client, group).await?;
    let req = hunt_req(vec![group.to_owned()], HuntSelector::Rules(vec![rule]));
    let entity = client.entities.create_hunt(&req).await?;
    // drain the first chunk events so queue assertions see only our commits
    pop_chunk_events(client, &[entity.id], hunt_meta(&entity).lanes.len()).await?;
    Ok(entity)
}

/// Verify a won checkpoint advances the lane and emits a paced next chunk
#[tokio::test]
#[serial]
async fn checkpoint_advances_lane_and_emits_paced() -> Result<(), Error> {
    // set up an admin client, a group, and a drained hunt
    let (client, group) = common::setup().await?;
    let entity = checkpoint_hunt_setup(&client, &group).await?;
    // build a mid lane checkpoint for lane 0 with distinctive facts
    let lane_start = hunt_meta(&entity).lanes[0].start;
    let current = lane_start - chrono::Duration::minutes(10);
    let cursor_id = Uuid::new_v4();
    let mut checkpoint = checkpoint_req(0, 0, current);
    checkpoint.cursor = Some(cursor_id);
    checkpoint.counters = HuntCounters {
        listed: 500,
        scanned: 450,
        matched: 3,
        written: 2,
        suppressed: 1,
        skipped: 40,
        missing: 5,
        errors: 1,
        error_samples: vec!["scan failed for aaa: boom".to_owned()],
    };
    // commit this checkpoint and remember when
    let before = Utc::now().timestamp();
    let resp = client
        .entities
        .checkpoint_hunt(entity.id, &checkpoint)
        .await?;
    // the first won checkpoint moves the hunt to running and emits
    assert_eq!(resp.status, HuntStatus::Running);
    assert!(resp.emitted);
    // the persisted view must carry this checkpoints facts
    let after = client.entities.get(entity.id).await?;
    let hunt = hunt_meta(&after);
    assert_eq!(hunt.status, HuntStatus::Running);
    assert_eq!(hunt.lanes[0].seq, 1);
    assert_eq!(hunt.lanes[0].current, current);
    assert_eq!(hunt.lanes[0].cursor, Some(cursor_id));
    assert!(!hunt.lanes[0].exhausted);
    assert_eq!(hunt.counters.listed, 500);
    assert_eq!(hunt.counters.scanned, 450);
    assert_eq!(hunt.counters.matched, 3);
    assert_eq!(hunt.counters.written, 2);
    assert_eq!(hunt.counters.errors, 1);
    assert_eq!(hunt.counters.error_samples.len(), 1);
    // the status tag must have moved to running while staying active
    let status_values = after.tags.get("HuntStatus").expect("lost the status tag");
    assert!(status_values.contains_key("Running"));
    assert!(!status_values.contains_key("Created"));
    assert!(after.tags.get("HuntActive").is_some());
    // the authoritative lane sequence must have advanced
    assert_eq!(test_utilities::hunt_lane_seq(&entity.id, 0).await?, Some(1));
    // exactly one next chunk event must be pending for this lane
    let pending = test_utilities::pending_hunt_chunks(&entity.id, 0).await?;
    assert_eq!(pending.len(), 1);
    let (event, score) = &pending[0];
    // the next chunk must carry the advanced sequence at depth 0 and keep
    // carrying the hunts creator
    let EventData::YaraHuntChunk(chunk) = &event.data else {
        panic!("the pending event is not a chunk event");
    };
    assert_eq!(chunk.seq, 1);
    assert_eq!(event.depth, 0);
    assert_eq!(event.user, after.submitter);
    // the emission must be paced into the future by the configured delay
    let pacing = i64::try_from(test_utilities::CONF.thorium.hunts.inter_chunk_pacing_secs).unwrap();
    assert!(*score >= before + pacing, "emission was not paced");
    assert!(*score <= Utc::now().timestamp() + pacing);
    // clean up our hunt and the paced event we left pending
    client.entities.cancel_hunt(entity.id).await?;
    test_utilities::wipe_hunt_chunk_queue().await?;
    Ok(())
}

/// Verify duplicate and stale sequences lose the CAS without emitting
#[tokio::test]
#[serial]
async fn checkpoint_duplicate_seq_409_no_emission() -> Result<(), Error> {
    // set up an admin client, a group, and a drained hunt
    let (client, group) = common::setup().await?;
    let entity = checkpoint_hunt_setup(&client, &group).await?;
    let lane_start = hunt_meta(&entity).lanes[0].start;
    // win lane 0 seq 0 then wipe its emission so the queue is clean
    let first = checkpoint_req(0, 0, lane_start - chrono::Duration::minutes(5));
    client.entities.checkpoint_hunt(entity.id, &first).await?;
    test_utilities::wipe_hunt_chunk_queue().await?;
    // a duplicate of the same sequence must lose the CAS
    let duplicate = checkpoint_req(0, 0, lane_start - chrono::Duration::minutes(5));
    fail!(
        client.entities.checkpoint_hunt(entity.id, &duplicate).await,
        409
    );
    // a lost CAS must not have emitted a forked chain link
    let pending = test_utilities::pending_hunt_chunks(&entity.id, 0).await?;
    assert!(pending.is_empty(), "a lost CAS emitted a chunk event");
    // the authoritative sequence must be untouched by the loss
    assert_eq!(test_utilities::hunt_lane_seq(&entity.id, 0).await?, Some(1));
    // clean up our hunt
    client.entities.cancel_hunt(entity.id).await?;
    test_utilities::wipe_hunt_chunk_queue().await?;
    Ok(())
}

/// Verify a lost CAS still merges its sticky facts into the persisted view
///
/// This manufactures the "commit won but its view persist was lost" state by
/// bumping the authoritative sequence past the entity's view, then replays the
/// checkpoint: the replay must lose the CAS with a 409 while still repairing
/// the lanes exhaustion, progress, and sequence floor.
#[tokio::test]
#[serial]
async fn checkpoint_lost_cas_sticky_merge_repairs_view() -> Result<(), Error> {
    // set up an admin client, a group, and a drained hunt
    let (client, group) = common::setup().await?;
    let entity = checkpoint_hunt_setup(&client, &group).await?;
    let lane_start = hunt_meta(&entity).lanes[0].start;
    // simulate a won commit whose view persist was lost: the authoritative
    // sequence is ahead of the entity's persisted lane view
    test_utilities::set_hunt_lane_seq(&entity.id, 0, 1).await?;
    // replay the lane exhausting checkpoint that lost its persist
    let current = lane_start - chrono::Duration::minutes(30);
    let mut replay = checkpoint_req(0, 0, current);
    replay.exhausted = true;
    replay.counters = HuntCounters {
        listed: 100,
        ..HuntCounters::default()
    };
    // the replay must lose the CAS since its sequence already committed
    fail!(
        client.entities.checkpoint_hunt(entity.id, &replay).await,
        409
    );
    // the loss path must still have repaired the persisted lane view
    let after = client.entities.get(entity.id).await?;
    let hunt = hunt_meta(&after);
    assert_eq!(hunt.lanes[0].seq, 1, "the sequence floor was not repaired");
    assert!(hunt.lanes[0].exhausted, "exhaustion was not sticky merged");
    assert_eq!(hunt.lanes[0].current, current, "progress was not merged");
    // a lost CAS must never absorb counter deltas
    assert_eq!(hunt.counters.listed, 0, "a lost CAS absorbed counters");
    // and must never emit a chunk event
    let pending = test_utilities::pending_hunt_chunks(&entity.id, 0).await?;
    assert!(pending.is_empty(), "a lost CAS emitted a chunk event");
    // clean up our hunt
    client.entities.cancel_hunt(entity.id).await?;
    test_utilities::wipe_hunt_chunk_queue().await?;
    Ok(())
}

/// Verify exhausting every lane completes the hunt and sweeps its tags
#[tokio::test]
#[serial]
async fn checkpoint_all_lanes_exhausted_completes() -> Result<(), Error> {
    // set up an admin client, a group, and a drained hunt
    let (client, group) = common::setup().await?;
    let entity = checkpoint_hunt_setup(&client, &group).await?;
    let lanes = hunt_meta(&entity).lanes.clone();
    // exhaust every lane one checkpoint at a time
    for (index, lane) in lanes.iter().enumerate() {
        // build the lane exhausting checkpoint
        let mut checkpoint = checkpoint_req(index as u32, 0, lane.end);
        checkpoint.exhausted = true;
        let resp = client
            .entities
            .checkpoint_hunt(entity.id, &checkpoint)
            .await?;
        // exhausted lanes end their chains without emitting
        assert!(!resp.emitted);
        // the hunt only completes once the final lane exhausts
        if index + 1 < lanes.len() {
            assert_eq!(resp.status, HuntStatus::Running);
        } else {
            assert_eq!(resp.status, HuntStatus::Completed);
        }
        // no chunk event may be pending for an exhausted lane
        let pending = test_utilities::pending_hunt_chunks(&entity.id, index as u32).await?;
        assert!(pending.is_empty(), "an exhausted lane emitted a chunk");
    }
    // the persisted view must be completed with every lane exhausted
    let after = client.entities.get(entity.id).await?;
    let hunt = hunt_meta(&after);
    assert_eq!(hunt.status, HuntStatus::Completed);
    assert!(hunt.lanes.iter().all(|lane| lane.exhausted));
    // the active tag must be swept so the quota census frees the slot
    assert!(after.tags.get("HuntActive").is_none());
    let status_values = after.tags.get("HuntStatus").expect("lost the status tag");
    assert!(status_values.contains_key("Completed"));
    assert!(!status_values.contains_key("Created"));
    assert!(!status_values.contains_key("Running"));
    // a duplicate of the final chunk lands on the terminal entry path and
    // self heals the tag sweep without committing anything
    let last_lane = (lanes.len() - 1) as u32;
    let mut duplicate = checkpoint_req(last_lane, 0, lanes[lanes.len() - 1].end);
    duplicate.exhausted = true;
    let resp = client
        .entities
        .checkpoint_hunt(entity.id, &duplicate)
        .await?;
    assert_eq!(resp.status, HuntStatus::Completed);
    assert!(!resp.emitted);
    // a hunt that finished on its own can no longer be cancelled
    fail!(client.entities.cancel_hunt(entity.id).await, 400);
    Ok(())
}

/// Verify a terminal failure stops the hunt and records its reason
#[tokio::test]
#[serial]
async fn checkpoint_failed_marks_failed() -> Result<(), Error> {
    // set up an admin client, a group, and a drained hunt
    let (client, group) = common::setup().await?;
    let entity = checkpoint_hunt_setup(&client, &group).await?;
    let lane_start = hunt_meta(&entity).lanes[0].start;
    // report a terminal failure from lane 0
    let mut checkpoint = checkpoint_req(0, 0, lane_start);
    checkpoint.failed = Some("the frozen rule snapshot loaded to 0 usable rules".to_owned());
    let resp = client
        .entities
        .checkpoint_hunt(entity.id, &checkpoint)
        .await?;
    // the hunt must fail without emitting another chunk
    assert_eq!(resp.status, HuntStatus::Failed);
    assert!(!resp.emitted);
    // the persisted view must carry the failure reason
    let after = client.entities.get(entity.id).await?;
    let hunt = hunt_meta(&after);
    assert_eq!(hunt.status, HuntStatus::Failed);
    assert_eq!(
        hunt.failure.as_deref(),
        Some("the frozen rule snapshot loaded to 0 usable rules")
    );
    // the active tag must be swept so the quota census frees the slot
    assert!(after.tags.get("HuntActive").is_none());
    // no chunk event may be pending for the failed lane
    let pending = test_utilities::pending_hunt_chunks(&entity.id, 0).await?;
    assert!(pending.is_empty(), "a failed checkpoint emitted a chunk");
    // a hunt that failed on its own can no longer be cancelled
    fail!(client.entities.cancel_hunt(entity.id).await, 400);
    Ok(())
}

/// Verify a checkpoint racing a cancel can never revive the hunt
#[tokio::test]
#[serial]
async fn checkpoint_after_cancel_stays_cancelled() -> Result<(), Error> {
    // set up an admin client, a group, and a drained hunt
    let (client, group) = common::setup().await?;
    let entity = checkpoint_hunt_setup(&client, &group).await?;
    let lane_start = hunt_meta(&entity).lanes[0].start;
    // cancel this hunt before its chunk checkpoints
    client.entities.cancel_hunt(entity.id).await?;
    // a checkpoint for the cancelled hunt lands on the terminal entry path
    let mut checkpoint = checkpoint_req(0, 0, lane_start - chrono::Duration::minutes(5));
    checkpoint.counters = HuntCounters {
        listed: 100,
        ..HuntCounters::default()
    };
    let resp = client
        .entities
        .checkpoint_hunt(entity.id, &checkpoint)
        .await?;
    // the terminal status must be reported with nothing emitted
    assert_eq!(resp.status, HuntStatus::Cancelled);
    assert!(!resp.emitted);
    // the hunt must stay cancelled with its active tags swept and nothing
    // from the late chunk committed
    let after = client.entities.get(entity.id).await?;
    let hunt = hunt_meta(&after);
    assert_eq!(hunt.status, HuntStatus::Cancelled);
    assert_eq!(hunt.counters.listed, 0);
    assert_eq!(hunt.lanes[0].seq, 0);
    assert!(after.tags.get("HuntActive").is_none());
    // the terminal entry path runs no CAS so the authoritative sequence and
    // the pending queue are untouched
    assert_eq!(test_utilities::hunt_lane_seq(&entity.id, 0).await?, Some(0));
    let pending = test_utilities::pending_hunt_chunks(&entity.id, 0).await?;
    assert!(pending.is_empty(), "a terminal checkpoint emitted a chunk");
    Ok(())
}

/// Verify a lost sequence key reseeds without stranding the live chain
#[tokio::test]
#[serial]
async fn checkpoint_reseeds_lost_seq_key() -> Result<(), Error> {
    // set up an admin client, a group, and a drained hunt
    let (client, group) = common::setup().await?;
    let entity = checkpoint_hunt_setup(&client, &group).await?;
    let lane_start = hunt_meta(&entity).lanes[0].start;
    // win lane 0 seq 0 so the entity floor advances to 1
    let first = checkpoint_req(0, 0, lane_start - chrono::Duration::minutes(5));
    client.entities.checkpoint_hunt(entity.id, &first).await?;
    // lose the authoritative sequence key
    test_utilities::delete_hunt_lane_seq(&entity.id, 0).await?;
    // the live head commits seq 1: the reseed must take the max of the entity
    // floor and the submitted sequence so the head is never rejected
    let second = checkpoint_req(0, 1, lane_start - chrono::Duration::minutes(10));
    let resp = client.entities.checkpoint_hunt(entity.id, &second).await?;
    assert_eq!(resp.status, HuntStatus::Running);
    // lose the key again with the live head now ahead of the entity floor;
    // floor is 2 but the in flight chunk legitimately carries seq 3
    test_utilities::delete_hunt_lane_seq(&entity.id, 0).await?;
    test_utilities::set_hunt_lane_seq(&entity.id, 0, 3).await?;
    test_utilities::delete_hunt_lane_seq(&entity.id, 0).await?;
    let third = checkpoint_req(0, 3, lane_start - chrono::Duration::minutes(15));
    let resp = client.entities.checkpoint_hunt(entity.id, &third).await?;
    assert_eq!(resp.status, HuntStatus::Running);
    // the authoritative sequence must now expect 4
    assert_eq!(test_utilities::hunt_lane_seq(&entity.id, 0).await?, Some(4));
    // a stale chunk behind the reseeded head must still lose
    let stale = checkpoint_req(0, 2, lane_start - chrono::Duration::minutes(20));
    fail!(
        client.entities.checkpoint_hunt(entity.id, &stale).await,
        409
    );
    // clean up our hunt and the paced events our wins emitted
    client.entities.cancel_hunt(entity.id).await?;
    test_utilities::wipe_hunt_chunk_queue().await?;
    Ok(())
}

/// Verify counters accumulate across checkpoints with bounded error samples
#[tokio::test]
#[serial]
async fn checkpoint_counters_accumulate_and_cap_samples() -> Result<(), Error> {
    // set up an admin client, a group, and a drained hunt
    let (client, group) = common::setup().await?;
    let entity = checkpoint_hunt_setup(&client, &group).await?;
    let lane_start = hunt_meta(&entity).lanes[0].start;
    // the configured error sample cap drives both commits sample counts
    let cap = test_utilities::CONF.thorium.hunts.max_error_samples;
    // commit a first chunk with almost a full cap of samples
    let mut first = checkpoint_req(0, 0, lane_start - chrono::Duration::minutes(5));
    first.counters = HuntCounters {
        listed: 500,
        scanned: 400,
        errors: (cap - 5) as u64,
        error_samples: (0..cap - 5).map(|i| format!("error {i}")).collect(),
        ..HuntCounters::default()
    };
    client.entities.checkpoint_hunt(entity.id, &first).await?;
    // commit a second chunk pushing the samples past the cap
    let mut second = checkpoint_req(0, 1, lane_start - chrono::Duration::minutes(10));
    second.counters = HuntCounters {
        listed: 300,
        scanned: 250,
        errors: 10,
        error_samples: (0..10).map(|i| format!("late error {i}")).collect(),
        ..HuntCounters::default()
    };
    client.entities.checkpoint_hunt(entity.id, &second).await?;
    // the totals must accumulate while the samples stay bounded
    let after = client.entities.get(entity.id).await?;
    let hunt = hunt_meta(&after);
    assert_eq!(hunt.counters.listed, 800);
    assert_eq!(hunt.counters.scanned, 650);
    assert_eq!(hunt.counters.errors, (cap - 5 + 10) as u64);
    assert_eq!(hunt.counters.error_samples.len(), cap);
    // clean up our hunt and the paced events our wins emitted
    client.entities.cancel_hunt(entity.id).await?;
    test_utilities::wipe_hunt_chunk_queue().await?;
    Ok(())
}

/// Verify only admins may checkpoint hunts
#[tokio::test]
#[serial]
async fn checkpoint_requires_admin() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a non admin member of the group
    let user = common::member(&client, &[&group]).await?;
    // create a hunt as the admin
    let entity = checkpoint_hunt_setup(&client, &group).await?;
    let lane_start = hunt_meta(&entity).lanes[0].start;
    // a non admin checkpoint must be rejected
    let checkpoint = checkpoint_req(0, 0, lane_start);
    fail!(
        user.entities.checkpoint_hunt(entity.id, &checkpoint).await,
        401
    );
    // clean up our hunt
    client.entities.cancel_hunt(entity.id).await?;
    Ok(())
}

/// Verify checkpoint validation rejects bad lanes, kinds, and ids
#[tokio::test]
#[serial]
async fn checkpoint_validates_lane_and_kind() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule and a hunt over it
    let rule = create_yara_rule(&client, &group).await?;
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let entity = client.entities.create_hunt(&req).await?;
    let lane_start = hunt_meta(&entity).lanes[0].start;
    // a lane index past the hunts lanes must be rejected
    let bad_lane = checkpoint_req(99, 0, lane_start);
    fail!(
        client.entities.checkpoint_hunt(entity.id, &bad_lane).await,
        400
    );
    // a checkpoint against a non hunt entity must be rejected
    let checkpoint = checkpoint_req(0, 0, lane_start);
    fail!(
        client.entities.checkpoint_hunt(rule, &checkpoint).await,
        400
    );
    // a checkpoint against a missing hunt must 404
    fail!(
        client
            .entities
            .checkpoint_hunt(Uuid::new_v4(), &checkpoint)
            .await,
        404
    );
    // clean up our hunt and its first chunk events
    client.entities.cancel_hunt(entity.id).await?;
    pop_chunk_events(&client, &[entity.id], hunt_meta(&entity).lanes.len()).await?;
    Ok(())
}

/// Verify the census rejects every non admin role with a 401
#[tokio::test]
#[serial]
async fn hunt_system_status_401_matrix() -> Result<(), Error> {
    // set up an admin client
    let (client, _group) = common::setup().await?;
    // cover every non admin role the API knows
    let roles = vec![
        UserRole::User,
        UserRole::Analyst,
        UserRole::Developer {
            k8s: true,
            bare_metal: true,
            windows: true,
            kvm: false,
            external: true,
        },
    ];
    for role in roles {
        // build a client for this role
        let user_client = generators::client_with_role(&client, role).await?;
        // the census must be rejected with a 401 for this role
        let resp = user_client
            .entities
            .hunt_system_status(&HuntStatusOpts::default())
            .await;
        fail!(resp, 401);
    }
    Ok(())
}

/// Verify a non admin census read is rejected with a 401
#[tokio::test]
#[serial]
async fn hunt_system_status_requires_admin() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // build a regular member client
    let member = common::member(&client, &[&group]).await?;
    // a non admin census read must be rejected with a 401
    let resp = member
        .entities
        .hunt_system_status(&HuntStatusOpts::default())
        .await;
    fail!(resp, 401);
    Ok(())
}

/// Verify the census agrees with the active tag listing and tracks lifecycle
#[tokio::test]
#[serial]
async fn hunt_system_status_matches_active_census() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a dedicated user so the census rows under test are ours
    let creator = common::member(&client, &[&group]).await?;
    // create a rule for the hunts to freeze
    let rule = create_yara_rule(&client, &group).await?;
    // create two active hunts as our dedicated user
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let first = creator.entities.create_hunt(&req).await?;
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let second = creator.entities.create_hunt(&req).await?;
    // read the admin census
    let census = client
        .entities
        .hunt_system_status(&HuntStatusOpts::default())
        .await?;
    // both fresh hunts must sit in upcoming as bounded Created lines
    for id in [first.id, second.id] {
        let line = census
            .upcoming
            .iter()
            .find(|line| line.id == id)
            .expect("a fresh hunt is missing from the upcoming census");
        is!(line.status, HuntStatus::Created);
        is!(line.frozen_rules, 1);
        assert!(!line.lanes.is_empty(), "census lines must carry lanes");
    }
    // the census must agree with a HuntActive tag listing across all groups
    let opts = EntityListOpts::default()
        .kinds(vec![EntityKinds::Hunt])
        .tag(Hunt::ACTIVE_TAG, "true")
        .limit(10_000);
    let listing = client.entities.list_details(&opts).await?;
    let active_census = census.ongoing.len() + census.upcoming.len();
    is!(active_census, listing.data.len());
    is!(census.quotas.active_global, active_census as u64);
    // the quota view must mirror the configured quotas
    let conf = &test_utilities::CONF.thorium.hunts;
    is!(
        census.quotas.max_active_per_user,
        conf.max_active_per_user as u64
    );
    is!(
        census.quotas.max_active_global,
        conf.max_active_global as u64
    );
    // the chunk backlog must cover exactly the hunt chunk kinds
    assert!(
        census.chunk_backlog.keys().all(EventType::is_hunt_chunk),
        "only hunt chunk kinds may ride the backlog"
    );
    assert!(
        census.chunk_backlog.contains_key(&EventType::YaraHuntChunk),
        "the yara hunt chunk backlog must always be reported"
    );
    // our creators census row must count both active hunts
    let submitter = &census
        .upcoming
        .iter()
        .find(|line| line.id == first.id)
        .expect("our hunt vanished from upcoming")
        .submitter;
    let row = census
        .users
        .iter()
        .find(|row| &row.username == submitter)
        .expect("our creator is missing from the user census");
    assert!(
        row.active >= 2,
        "both active hunts must count for their creator"
    );
    is!(row.quota, conf.max_active_per_user as u64);
    assert!(
        row.recent_created >= 2,
        "window creations must count for their creator"
    );
    // cancelling a hunt must move it from upcoming to recent
    client.entities.cancel_hunt(first.id).await?;
    let census = client
        .entities
        .hunt_system_status(&HuntStatusOpts::default())
        .await?;
    assert!(
        !census.upcoming.iter().any(|line| line.id == first.id),
        "a cancelled hunt must leave the upcoming census"
    );
    let line = census
        .recent
        .iter()
        .find(|line| line.id == first.id)
        .expect("a cancelled hunt must join the recent census");
    is!(line.status, HuntStatus::Cancelled);
    // clean up our remaining hunt and both hunts chunk events in one drain
    client.entities.cancel_hunt(second.id).await?;
    pop_chunk_events(
        &client,
        &[first.id, second.id],
        hunt_meta(&first).lanes.len() + hunt_meta(&second).lanes.len(),
    )
    .await?;
    Ok(())
}

/// Verify the census display bounds are honored and silly windows clamp
#[tokio::test]
#[serial]
async fn hunt_system_status_bounds_params() -> Result<(), Error> {
    // set up an admin client and a group
    let (client, group) = common::setup().await?;
    // create a rule and two hunts then cancel both so recent holds at least two
    let rule = create_yara_rule(&client, &group).await?;
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let first = client.entities.create_hunt(&req).await?;
    let req = hunt_req(vec![group.clone()], HuntSelector::Rules(vec![rule]));
    let second = client.entities.create_hunt(&req).await?;
    client.entities.cancel_hunt(first.id).await?;
    client.entities.cancel_hunt(second.id).await?;
    // the recent display cap must be honored
    let census = client
        .entities
        .hunt_system_status(&HuntStatusOpts::default().recent(1))
        .await?;
    assert!(
        census.recent.len() <= 1,
        "the recent display cap must bound the response"
    );
    // a silly window must clamp instead of erroring
    let census = client
        .entities
        .hunt_system_status(&HuntStatusOpts::default().window_days(400))
        .await?;
    assert!(
        census
            .recent
            .iter()
            .any(|line| line.id == first.id || line.id == second.id),
        "a clamped window must still cover fresh terminal hunts"
    );
    // clean up both hunts chunk events in one drain
    pop_chunk_events(
        &client,
        &[first.id, second.id],
        hunt_meta(&first).lanes.len() + hunt_meta(&second).lanes.len(),
    )
    .await?;
    Ok(())
}
