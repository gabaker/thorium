//! Tests the admin event queue peek route, the event queue inspection helper, how workflow
//! graph events are queued and wait to mature, how model refresh events are queued, yara
//! scan events, and the per component cache flags

use chrono::{DateTime, TimeZone, Utc};
use rand::RngCore;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;
use thorium::models::{
    Buffer, Event, EventData, EventIds, EventPeekOpts, EventPopOpts, EventType, FileDeleteOpts,
    ModelEndpointEntityRequest, ModelProvider, Sample, SampleRequest, ScanStatsSnapshot,
    SubmissionUpdate, TagRequest, UserRole, WorkerStatsReport, WorkflowCatalogEntityRequest,
    WorkflowGraphChange, YaraScanTrigger,
};
use thorium::test_utilities::generators::{GroupRoles, god_group};
use thorium::test_utilities::{self, events, generators};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

/// Create a workflow catalog, which queues one workflow graph event, and get its id
///
/// # Arguments
///
/// * `admin` - The admin client to create it with
/// * `group` - The group to create it in
async fn queue_graph_event(admin: &Thorium, group: &str) -> Result<Uuid, Error> {
    // a workflow without agents is enough to queue a change
    let req = generators::gen_workflow("events", &[group], WorkflowCatalogEntityRequest::default());
    Ok(admin.entities.create(req).await?.id)
}

/// Find the score a queued event of one kind was given, from the queue's Redis state
///
/// # Arguments
///
/// * `kind` - The kind of event queue to read
/// * `matches` - Picks out the event to find
async fn queued_score<F>(kind: EventType, matches: F) -> Result<Option<f64>, Error>
where
    F: Fn(&Event) -> bool,
{
    // read the queue with its scores
    let state = events::queue_state(kind).await?;
    // find the matching event
    for (serialized, score) in &state.queued {
        let event: Event = serde_json::from_str(serialized)?;
        if matches(&event) {
            return Ok(Some(
                score
                    .parse::<f64>()
                    .map_err(|err| Error::new(err.to_string()))?,
            ));
        }
    }
    Ok(None)
}

/// Sleep until some time after the second an event was queued in
///
/// # Arguments
///
/// * `score` - The score the event was queued with, in whole seconds
/// * `after` - How long after the start of that second to wake
async fn sleep_until(score: f64, after: Duration) {
    // find the time to wake at
    #[allow(clippy::cast_possible_truncation)]
    let queued = DateTime::<Utc>::from_timestamp(score as i64, 0).expect("valid score");
    let wake = queued + after;
    // sleep until then if it hasn't passed yet
    if let Ok(left) = (wake - Utc::now()).to_std() {
        tokio::time::sleep(left).await;
    }
}

/// Pop up to a thousand events of one kind and clear them so none stay in flight
///
/// # Arguments
///
/// * `admin` - The admin client to pop with
/// * `kind` - The kind of events to pop
async fn pop_and_clear(admin: &Thorium, kind: EventType) -> Result<Vec<Event>, Error> {
    // pop whatever is mature
    let popped = admin
        .events
        .pop(kind, &EventPopOpts::default().limit(1000))
        .await?;
    // clear the popped events from the in flight set
    if !popped.is_empty() {
        let ids = popped.iter().map(|event| event.id).collect::<EventIds>();
        admin.events.clear(kind, &ids, None).await?;
    }
    Ok(popped)
}

/// Model refresh events go only to their own queue and the first pop after they're queued
/// returns them, with no maturity delay
#[tokio::test]
#[serial_test::serial]
async fn model_refresh_events_pop_at_once() -> Result<(), Error> {
    // capture the trigger queue before an endpoint is created
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let minerva = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let triggers_before = events::queue_state(EventType::ReactionTrigger).await?;
    // creating an endpoint queues a refresh of it
    let provider = ModelProvider::LiteLlm {
        base_url: format!("https://{suffix}.gateway.local/v1"),
        key_header: None,
    };
    let endpoint = generators::model_endpoint(
        &[minerva.as_str()],
        ModelEndpointEntityRequest::new(provider),
        &admin,
    )
    .await?;
    let queued = events::refresh_events(&admin, endpoint.id).await?;
    is!(queued.len(), 1, "one model refresh event");
    // the trigger queue is untouched
    let triggers_after = events::queue_state(EventType::ReactionTrigger).await?;
    is!(&triggers_before, &triggers_after, "trigger queue unchanged");
    // the very next pop returns it without waiting
    let popped = pop_and_clear(&admin, EventType::ModelRefresh).await?;
    let ours = popped
        .iter()
        .filter(|event| events::refresh_targets(event, endpoint.id))
        .count();
    is!(ours, 1, "refresh popped at once");
    // a trigger pop never returns a refresh event
    tokio::time::sleep(Duration::from_secs(4)).await;
    let popped = pop_and_clear(&admin, EventType::ReactionTrigger).await?;
    let refreshes = popped
        .iter()
        .filter(|event| events::refresh_event(event).is_some())
        .count();
    is!(refreshes, 0, "trigger pop returns no refresh event");
    Ok(())
}

/// Workflow graph events wait for their delay before a pop returns them, while trigger events
/// still wait only 3 seconds, and immature events are put back with their original scores
#[tokio::test]
#[serial_test::serial]
async fn workflow_graph_events_mature_after_delay() -> Result<(), Error> {
    // a workflow and a tagged sample, made at about the same time
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let minerva = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let workflow = queue_graph_event(&admin, &minerva).await?;
    let sha256 = admin
        .files
        .create(generators::gen_sample(&minerva))
        .await?
        .sha256;
    let req = TagRequest::<Sample>::default()
        .group(&minerva)
        .add("maturity", generators::gen_string(8));
    admin.files.tag(&sha256, &req).await?;
    // find the second the workflow's event was queued in
    let ours = |event: &Event| {
        events::graph_event(event)
            .is_some_and(|graph| graph.change == WorkflowGraphChange::WorkflowUpdated { workflow })
    };
    let score = queued_score(EventType::WorkflowGraph, ours)
        .await?
        .expect("the workflow's event is queued");
    // find the second the tag event was queued in
    let is_tag = |event: &Event| tag_event(event).is_some_and(|(item, _)| *item == sha256);
    let tag_score = queued_score(EventType::ReactionTrigger, is_tag)
        .await?
        .expect("the tag event is queued");
    // 4 seconds after the tag event, it pops but the workflow graph event is put back
    sleep_until(tag_score, Duration::from_millis(4100)).await;
    let popped = pop_and_clear(&admin, EventType::ReactionTrigger).await?;
    is!(
        popped.iter().any(is_tag),
        true,
        "trigger event popped after 3s"
    );
    let popped = pop_and_clear(&admin, EventType::WorkflowGraph).await?;
    is!(
        popped.iter().any(ours),
        false,
        "graph event not popped at 4s"
    );
    // 9 seconds in, it's still put back, keeping the score it was queued with
    sleep_until(score, Duration::from_millis(9100)).await;
    let popped = pop_and_clear(&admin, EventType::WorkflowGraph).await?;
    is!(
        popped.iter().any(ours),
        false,
        "graph event not popped at 9s"
    );
    let kept = queued_score(EventType::WorkflowGraph, ours).await?;
    is!(kept, Some(score), "immature event keeps its score");
    // 11 seconds in, the pop returns it
    sleep_until(score, Duration::from_millis(11_100)).await;
    let popped = pop_and_clear(&admin, EventType::WorkflowGraph).await?;
    is!(
        popped.iter().filter(|event| ours(event)).count(),
        1,
        "graph event popped at 11s"
    );
    Ok(())
}

/// Workflow graph events only go to their own queue, never to the reaction trigger queue
#[tokio::test]
#[serial_test::serial]
async fn workflow_graph_events_skip_trigger_queue() -> Result<(), Error> {
    // capture the trigger queue before a workflow is created
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let minerva = god_group(&admin, "minerva", &suffix, &GroupRoles::default()).await?;
    let triggers_before = events::queue_state(EventType::ReactionTrigger).await?;
    // creating a workflow queues its change in the workflow graph queue
    let workflow = queue_graph_event(&admin, &minerva).await?;
    let queued = events::graph_changes(&admin, workflow).await?;
    is!(queued.len(), 1, "one workflow graph event");
    // the trigger queue is untouched
    let triggers_after = events::queue_state(EventType::ReactionTrigger).await?;
    is!(&triggers_before, &triggers_after, "trigger queue unchanged");
    // once the trigger delay passes, a trigger pop returns no workflow graph event
    tokio::time::sleep(Duration::from_secs(4)).await;
    let popped = pop_and_clear(&admin, EventType::ReactionTrigger).await?;
    let graph = popped
        .iter()
        .filter(|event| events::graph_event(event).is_some())
        .count();
    is!(graph, 0, "trigger pop returns no workflow graph event");
    Ok(())
}

/// Get the item and tag keys of a tag event if it is one
///
/// # Arguments
///
/// * `event` - The event to inspect
fn tag_event(event: &Event) -> Option<(&String, HashSet<&String>)> {
    // only new tag events carry an item and tag keys
    match &event.data {
        EventData::NewTags { item, tags, .. } => Some((item, tags.keys().collect())),
        _ => None,
    }
}

/// Peeking returns queued events oldest first without moving them, and a pop
/// still gets them all
#[tokio::test]
#[serial_test::serial]
async fn peek_is_read_only_and_ordered() -> Result<(), Error> {
    // get an admin client and a group
    let client = test_utilities::admin_client().await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a sample to tag
    let sha256 = client
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    // add three tag requests one after another, each queuing one tag event
    let keys = ["first", "second", "third"];
    for key in keys {
        // build a tag request with a single unique key
        let req = TagRequest::<Sample>::default()
            .group(&group)
            .add(key, generators::gen_string(8));
        // add the tags, queuing an event
        client.files.tag(&sha256, &req).await?;
        // wait briefly so each event gets a later timestamp
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // capture the queue's state in Redis before peeking
    let before = events::queue_state(EventType::ReactionTrigger).await?;
    // peek at every queued trigger event
    let peeked = client
        .events
        .peek(EventType::ReactionTrigger, &EventPeekOpts::default())
        .await?;
    // capture the queue's state in Redis after peeking
    let after = events::queue_state(EventType::ReactionTrigger).await?;
    // peeking must not change the queue or the in flight set
    is!(&before, &after, "queue state unchanged by peek");
    // the peek returns every queued event
    is!(
        peeked.len(),
        before.queued.len(),
        "peek returns every queued event"
    );
    // the peeked events are oldest first
    let mut sorted = peeked.clone();
    sorted.sort_by_key(|event| event.timestamp);
    let peeked_ids = peeked.iter().map(|event| event.id).collect::<Vec<Uuid>>();
    let sorted_ids = sorted.iter().map(|event| event.id).collect::<Vec<Uuid>>();
    is!(&peeked_ids, &sorted_ids, "peeked events are oldest first");
    // our three tag events appear in the order they were made
    let ours = peeked
        .iter()
        .filter_map(tag_event)
        .filter(|(item, tags)| {
            **item == sha256 && keys.iter().any(|key| tags.contains(&(*key).to_owned()))
        })
        .map(|(_, tags)| tags.into_iter().cloned().collect::<Vec<String>>())
        .collect::<Vec<Vec<String>>>();
    let expected = keys
        .iter()
        .map(|key| vec![(*key).to_owned()])
        .collect::<Vec<Vec<String>>>();
    is!(&ours, &expected, "tag events are in creation order");
    // the inspection helper sees the same events
    let helper = events::queued(&client, EventType::ReactionTrigger).await?;
    let helper_ids = helper.iter().map(|event| event.id).collect::<Vec<Uuid>>();
    is!(&helper_ids, &peeked_ids, "helper sees the peeked events");
    // the helper's filter keeps only matching events
    let matching = events::queued_matching(&client, EventType::ReactionTrigger, |event| {
        tag_event(event).is_some_and(|(item, _)| *item == sha256)
    })
    .await?;
    is!(matching.is_empty(), false, "helper filter finds our events");
    // wait until the queued events are old enough to pop
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    // pop every queued event
    let opts = EventPopOpts::default().limit(peeked.len() + 100);
    let popped = client.events.pop(EventType::ReactionTrigger, &opts).await?;
    // every peeked event is still returned by the pop
    let popped_ids = popped
        .iter()
        .map(|event| event.id)
        .collect::<HashSet<Uuid>>();
    for id in &peeked_ids {
        is!(
            popped_ids.contains(id),
            true,
            "pop returns every peeked event"
        );
    }
    // clear the popped events so they leave the in flight set
    let ids = EventIds {
        ids: popped_ids.into_iter().collect(),
    };
    client
        .events
        .clear(EventType::ReactionTrigger, &ids, None)
        .await?;
    Ok(())
}

/// Peeking an empty queue returns an empty list and changes nothing
#[tokio::test]
async fn peek_empty_queue() -> Result<(), Error> {
    // get an admin client
    let client = test_utilities::admin_client().await?;
    // capture the empty queue's state before peeking
    let before = events::queue_state(EventType::SigmaScannableResults).await?;
    // peek at a queue nothing in this test binary writes to
    let peeked = client
        .events
        .peek(EventType::SigmaScannableResults, &EventPeekOpts::default())
        .await?;
    // the queue is empty
    is!(peeked.len(), 0, "empty queue peeks empty");
    // the queue state is unchanged
    let after = events::queue_state(EventType::SigmaScannableResults).await?;
    is!(&before, &after, "empty queue state unchanged");
    Ok(())
}

/// Peeking honors the requested limit
#[tokio::test]
async fn peek_limit() -> Result<(), Error> {
    // get an admin client
    let client = test_utilities::admin_client().await?;
    // peek at no more than zero events
    let peeked = client
        .events
        .peek(
            EventType::ReactionTrigger,
            &EventPeekOpts::default().limit(0),
        )
        .await?;
    // nothing is returned for a zero limit
    is!(peeked.len(), 0, "zero limit returns nothing");
    Ok(())
}

/// Peeking an unknown event kind returns a 400
#[tokio::test]
async fn peek_unknown_kind() -> Result<(), Error> {
    // get an admin client and its token
    let client = test_utilities::admin_client().await?;
    let token = client.users.info().await?.token;
    // request a kind that does not exist
    let url = format!("{}/api/events/peek/NotAKind/", client.host);
    let resp = reqwest::Client::new()
        .get(&url)
        .header("authorization", test_utilities::auth_header(&token))
        .send()
        .await?;
    // the API rejects the unknown kind
    is!(resp.status().as_u16(), 400, "unknown kind is a bad request");
    Ok(())
}

/// Only admins with a full token can peek
#[tokio::test]
async fn peek_requires_admin() -> Result<(), Error> {
    // get an admin client
    let admin = test_utilities::admin_client().await?;
    // a non-admin full token is refused
    let user = generators::client(&admin).await?;
    let opts = EventPeekOpts::default();
    fail!(
        user.events.peek(EventType::ReactionTrigger, &opts).await,
        401
    );
    // a scoped token of an admin is refused
    let scoped = generators::scoped_client(&admin).await?;
    fail!(
        scoped.events.peek(EventType::ReactionTrigger, &opts).await,
        401
    );
    // a request without a token is refused
    let url = format!("{}/api/events/peek/ReactionTrigger/", admin.host);
    let resp = reqwest::Client::new().get(&url).send().await?;
    is!(resp.status().as_u16(), 401, "no token is unauthorized");
    Ok(())
}

/// Build a sample request around unique random bytes
///
/// # Arguments
///
/// * `groups` - The groups to upload this unique sample too
fn unique_sample(groups: Vec<String>) -> SampleRequest {
    // generate unique random bytes so each test uploads a brand new sample
    let mut random_data = [0u8; 32];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut random_data);
    // build a sample request around our unique bytes
    SampleRequest::new_buffer(Buffer::new(random_data.to_vec()), groups)
}

/// Pop yara scan events until one matches a sha256 and trigger kind
///
/// Foreign events popped while searching are cleared so they do not leak in
/// the in flight queue; other suites share these queues in CI so we only ever
/// assert on events for our own sha256.
///
/// # Arguments
///
/// * `client` - A Thorium client
/// * `sha256` - The sha256 our event must be for
/// * `trigger` - The trigger kind our event must carry
async fn pop_yara_event(
    client: &Thorium,
    sha256: &str,
    trigger: &YaraScanTrigger,
) -> Result<Option<Event>, thorium::Error> {
    // pop in large pages to cut round trips
    let opts = EventPopOpts::default().limit(200);
    // track any foreign events we popped so we can clear them
    let mut foreign = Vec::default();
    // the event we found if any
    let mut found = None;
    // search a bounded number of pages so a missing event fails instead of hanging
    'outer: for _ in 0..10 {
        // pop a page of yara scan events
        let events = client
            .events
            .pop(EventType::YaraScannableSample, &opts)
            .await?;
        // an empty page may just mean our event is younger than the pop
        // maturity window so wait for it to mature instead of bailing early
        if events.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        // search this page for our event
        for event in events {
            // check if this event is for our sample and trigger kind
            match &event.data {
                EventData::YaraScannableSample(inner)
                    if inner.sha256 == sha256 && &inner.trigger == trigger =>
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
            .clear(EventType::YaraScannableSample, &ids, None)
            .await?;
    }
    // clear any foreign events we popped as well
    if !foreign.is_empty() {
        // clear the foreign events we popped
        client
            .events
            .clear(
                EventType::YaraScannableSample,
                &EventIds::from(foreign),
                None,
            )
            .await?;
    }
    Ok(found)
}

/// Verify a new submission emits a yara scan event carrying its trigger depth
#[tokio::test]
#[serial_test::serial]
async fn sample_create_emits_yara_scannable() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // build a unique sample request at a non zero trigger depth
    let file_req = unique_sample(vec![group.clone()]).trigger_depth(2);
    // upload this file
    let resp = client.files.create(file_req).await?;
    // find the yara scan event for this upload
    let event = pop_yara_event(&client, &resp.sha256, &YaraScanTrigger::NewSubmission)
        .await?
        .expect("no yara scan event was emitted for a new submission");
    // make sure the originating trigger depth rode along on the event
    is!(event.depth, 2);
    // make sure the event carries exactly the submissions groups
    if let EventData::YaraScannableSample(inner) = &event.data {
        is!(inner.groups.clone(), vec![group.clone()]);
    }
    Ok(())
}

/// Verify adding groups to a submission emits a groups added scan event
#[tokio::test]
#[serial_test::serial]
async fn add_groups_emits_groups_added() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create three groups
    let mut groups: Vec<String> = generators::groups(3, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    // keep the two groups we will add later
    let added = vec![groups.remove(0), groups.remove(0)];
    // upload a unique sample to the remaining group
    let resp = client.files.create(unique_sample(groups)).await?;
    // drain the new submission event so only the update event remains ours
    pop_yara_event(&client, &resp.sha256, &YaraScanTrigger::NewSubmission).await?;
    // build an update that adds our two new groups
    let mut update = SubmissionUpdate::new(resp.id);
    update.add_groups.extend(added.iter().cloned());
    // apply our update
    client.files.update(&resp.sha256, &update).await?;
    // find the groups added scan event for this update
    let event = pop_yara_event(&client, &resp.sha256, &YaraScanTrigger::GroupsAdded)
        .await?
        .expect("no yara scan event was emitted for added groups");
    // make sure the event carries exactly the added groups
    if let EventData::YaraScannableSample(inner) = &event.data {
        is!(inner.groups.clone(), added);
    }
    Ok(())
}

/// Verify removing groups from a submission emits a groups removed scan event
#[tokio::test]
#[serial_test::serial]
async fn remove_groups_emits_groups_removed() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create two groups
    let groups: Vec<String> = generators::groups(2, &client)
        .await?
        .into_iter()
        .map(|group| group.name)
        .collect();
    // keep the group we will remove later
    let removed = groups[1].clone();
    // upload a unique sample to both groups
    let resp = client.files.create(unique_sample(groups)).await?;
    // build an update that removes one group
    let mut update = SubmissionUpdate::new(resp.id);
    update.remove_groups.push(removed.clone());
    // apply our update
    client.files.update(&resp.sha256, &update).await?;
    // find the groups removed scan event for this update
    let event = pop_yara_event(&client, &resp.sha256, &YaraScanTrigger::GroupsRemoved)
        .await?
        .expect("no yara scan event was emitted for removed groups");
    // make sure the event carries exactly the removed group
    if let EventData::YaraScannableSample(inner) = &event.data {
        is!(inner.groups.clone(), vec![removed.clone()]);
    }
    Ok(())
}

/// Verify deleting a submission emits a groups removed scan event
#[tokio::test]
#[serial_test::serial]
async fn submission_delete_emits_groups_removed() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a unique sample
    let resp = client
        .files
        .create(unique_sample(vec![group.clone()]))
        .await?;
    // delete this submission from all of its groups
    client
        .files
        .delete(&resp.sha256, &resp.id, &FileDeleteOpts::default())
        .await?;
    // find the groups removed scan event for this delete
    let event = pop_yara_event(&client, &resp.sha256, &YaraScanTrigger::GroupsRemoved)
        .await?
        .expect("no yara scan event was emitted for a deleted submission");
    // make sure the event carries the deleted submissions group
    if let EventData::YaraScannableSample(inner) = &event.data {
        is!(inner.groups.clone(), vec![group.clone()]);
    }
    Ok(())
}

/// Verify a duplicate submission emits no second scan event
#[tokio::test]
#[serial_test::serial]
async fn duplicate_submission_emits_nothing() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // build one unique sample request we will upload twice
    let file_req = unique_sample(vec![group]);
    // upload this file
    let resp = client.files.create(file_req.clone()).await?;
    // upload the exact same submission again and expect a conflict
    let dup = client.files.create(file_req).await;
    fail!(dup, 409);
    // the first upload emitted exactly one new submission event
    let first = pop_yara_event(&client, &resp.sha256, &YaraScanTrigger::NewSubmission).await?;
    assert!(first.is_some(), "the first upload must emit a scan event");
    // the duplicate upload must not have emitted a second event
    let second = pop_yara_event(&client, &resp.sha256, &YaraScanTrigger::NewSubmission).await?;
    assert!(
        second.is_none(),
        "a duplicate submission must not emit a scan event"
    );
    Ok(())
}

/// Reset all cache flags and return a fresh status read
///
/// # Arguments
///
/// * `client` - A Thorium client
async fn reset_flags(client: &Thorium) -> Result<(), thorium::Error> {
    // build the opts to reset all cache flags
    let opts = thorium::models::EventCacheStatusOpts::default().reset();
    // read and reset the cache status
    client.events.get_cache_status(&opts).await?;
    Ok(())
}

/// Verify yara rule entity CRUD flips the yara rules cache flag
#[tokio::test]
#[serial_test::serial]
async fn yara_rule_crud_flips_yara_flag() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // reset all cache flags so we observe only our own writes
    reset_flags(&client).await?;
    // create a yara rule entity
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    let resp = client.entities.create(req).await?;
    // read the cache status without resetting it
    let opts = thorium::models::EventCacheStatusOpts::default();
    let status = client.events.get_cache_status(&opts).await?;
    // creating a yara rule must stale the yara rules cache
    assert!(
        status.yara_rules,
        "creating a yara rule must set the yara rules flag"
    );
    // reset the flags and delete our rule
    reset_flags(&client).await?;
    client.entities.delete(resp.id).await?;
    // deleting a yara rule must stale the yara rules cache too
    let status = client.events.get_cache_status(&opts).await?;
    assert!(
        status.yara_rules,
        "deleting a yara rule must set the yara rules flag"
    );
    Ok(())
}

/// Verify sigma rule entity CRUD flips the sigma rules cache flag
#[tokio::test]
#[serial_test::serial]
async fn sigma_rule_crud_flips_sigma_flag() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // reset all cache flags so we observe only our own writes
    reset_flags(&client).await?;
    // create a sigma rule entity
    let req = generators::gen_entity(&group, generators::gen_sigma_meta());
    client.entities.create(req).await?;
    // read the cache status without resetting it
    let opts = thorium::models::EventCacheStatusOpts::default();
    let status = client.events.get_cache_status(&opts).await?;
    // creating a sigma rule must stale the sigma rules cache
    assert!(
        status.sigma_rules,
        "creating a sigma rule must set the sigma rules flag"
    );
    Ok(())
}

/// Verify user and group writes flip the users and triggers cache flags
#[tokio::test]
#[serial_test::serial]
async fn user_and_group_writes_flip_flags() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // reset all cache flags so we observe only our own writes
    reset_flags(&client).await?;
    // create a user which must stale the users and triggers caches
    generators::users(1, &client).await?;
    // read the cache status without resetting it
    let opts = thorium::models::EventCacheStatusOpts::default();
    let status = client.events.get_cache_status(&opts).await?;
    // user writes must stale the users cache
    assert!(status.users, "creating a user must set the users flag");
    // user writes must also stale the trigger cache since it embeds users
    assert!(
        status.triggers,
        "creating a user must also set the triggers flag"
    );
    Ok(())
}

/// Verify resetting the cache status clears the per component flags
#[tokio::test]
#[serial_test::serial]
async fn cache_status_reset_clears_all() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a yara rule entity to set the yara rules flag
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    client.entities.create(req).await?;
    // read the cache status with a reset; the pre reset value must be stale
    let opts = thorium::models::EventCacheStatusOpts::default().reset();
    let status = client.events.get_cache_status(&opts).await?;
    assert!(
        status.yara_rules,
        "the pre reset read must still see the yara rules flag"
    );
    // a followup read must see the reset flag; only the yara flag is asserted
    // since concurrent suites can legitimately re stale users/triggers
    let opts = thorium::models::EventCacheStatusOpts::default();
    let status = client.events.get_cache_status(&opts).await?;
    assert!(
        !status.yara_rules,
        "a reset must clear the yara rules flag until the next rule write"
    );
    Ok(())
}

/// Verify popped yara events round trip their payload through redis
#[tokio::test]
#[serial_test::serial]
async fn yara_event_payload_round_trips() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a unique sample
    let resp = client
        .files
        .create(unique_sample(vec![group.clone()]).trigger_depth(1))
        .await?;
    // find the yara scan event for this upload
    let event = pop_yara_event(&client, &resp.sha256, &YaraScanTrigger::NewSubmission)
        .await?
        .expect("no yara scan event was emitted for a new submission");
    // make sure every field round tripped through redis intact
    is!(event.depth, 1);
    assert!(event.parent.is_none());
    assert!(Uuid::try_parse(&event.id.to_string()).is_ok());
    if let EventData::YaraScannableSample(inner) = &event.data {
        is!(inner.sha256.clone(), resp.sha256);
        is!(inner.groups.clone(), vec![group.clone()]);
    } else {
        panic!("popped event does not carry yara scan data");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Worker scoped queue ownership tests (M3)
//
// These simulate multiple event handler replicas as distinct worker id strings
// against the one test redis. The testing config pins tiny reclaim knobs
// (heartbeat 1s / expiry 3s / backstop 4s) so death and backstop reclaim run
// in seconds.
// ---------------------------------------------------------------------------

/// Build a unique event handler worker id for a test
///
/// # Arguments
///
/// * `prefix` - A human readable prefix naming the test this worker is for
fn test_worker(prefix: &str) -> String {
    // suffix with a random uuid so reruns never collide in the shared registry
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

/// Pop a page of yara scan events for an optional worker
///
/// # Arguments
///
/// * `client` - A Thorium client
/// * `worker` - The worker to claim the popped events for if one was set
/// * `limit` - The max number of events to pop
async fn pop_page(
    client: &Thorium,
    worker: Option<&str>,
    limit: usize,
) -> Result<Vec<Event>, thorium::Error> {
    // build the pop opts for this page
    let mut opts = EventPopOpts::default().limit(limit);
    // claim this page for a worker if one was set
    if let Some(worker) = worker {
        opts = opts.worker(worker);
    }
    // pop a page of yara scan events
    client
        .events
        .pop(EventType::YaraScannableSample, &opts)
        .await
}

/// Clear a set of yara scan events from an optional workers in flight keys
///
/// # Arguments
///
/// * `client` - A Thorium client
/// * `worker` - The worker whose in flight keys to clear from if one was set
/// * `ids` - The event ids to clear
async fn clear_ids(
    client: &Thorium,
    worker: Option<&str>,
    ids: Vec<Uuid>,
) -> Result<(), thorium::Error> {
    // skip the round trip if there is nothing to clear
    if ids.is_empty() {
        return Ok(());
    }
    // clear these events from the right in flight keys
    client
        .events
        .clear(EventType::YaraScannableSample, &EventIds::from(ids), worker)
        .await?;
    Ok(())
}

/// Claim pending yara scan events for a worker until every target sha256 is seen
///
/// Returns every event this worker claimed in pop order plus the matched
/// target events aligned with the `shas` argument. Popping retries with a
/// short sleep so freshly created events have time to mature.
///
/// # Arguments
///
/// * `client` - A Thorium client
/// * `worker` - The worker to claim the popped events for
/// * `shas` - The sha256s whose scan events must all be claimed
async fn claim_shas(
    client: &Thorium,
    worker: &str,
    shas: &[&str],
) -> Result<(Vec<Event>, Vec<Event>), thorium::Error> {
    // track every event this worker claims in pop order
    let mut popped = Vec::default();
    // search a bounded number of pages so a missing event fails instead of hanging
    for _ in 0..30 {
        // pop a page of events claimed by this worker
        let page = pop_page(client, Some(worker), 500).await?;
        // an empty page means our events may not be mature yet so wait a beat
        if page.is_empty() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        } else {
            // keep this page in pop order
            popped.extend(page);
        }
        // check whether every target sha has been claimed yet
        let found_all = shas.iter().all(|sha| {
            popped.iter().any(|event| {
                matches!(&event.data, EventData::YaraScannableSample(inner) if inner.sha256 == *sha)
            })
        });
        // stop popping once every target sha has been claimed
        if found_all {
            // align the matched events with the target shas
            let matched = shas
                .iter()
                .map(|sha| {
                    popped
                        .iter()
                        .find(|event| {
                            matches!(&event.data, EventData::YaraScannableSample(inner) if inner.sha256 == *sha)
                        })
                        .cloned()
                        .expect("matched event vanished from the claimed set")
                })
                .collect();
            return Ok((popped, matched));
        }
    }
    // we never claimed every target event so fail loudly
    Err(thorium::Error::new(format!(
        "failed to claim scan events for {shas:?} after 30 pages"
    )))
}

/// Drain the pending yara scan queue by popping and clearing everything
///
/// This consumes any backlog left by this or other suites so depth assertions
/// are stable.
///
/// # Arguments
///
/// * `client` - A Thorium client
/// * `worker` - The worker to claim and clear the drained events with
async fn drain_yara_queue(client: &Thorium, worker: &str) -> Result<(), thorium::Error> {
    // drain a bounded number of pages so a flooded queue fails instead of hanging
    for _ in 0..30 {
        // pop a page of events claimed by this worker
        let page = pop_page(client, Some(worker), 500).await?;
        // an empty page means the pending queue is drained
        if page.is_empty() {
            return Ok(());
        }
        // clear this page from our workers in flight keys
        clear_ids(
            client,
            Some(worker),
            page.into_iter().map(|event| event.id).collect(),
        )
        .await?;
    }
    Ok(())
}

/// Get the current depths for the yara scan event kind
///
/// # Arguments
///
/// * `client` - A Thorium client
async fn yara_depth(client: &Thorium) -> Result<thorium::models::KindDepth, thorium::Error> {
    // get the depths for every kind of event
    let depths = client.events.depths().await?;
    // pull out the yara scan kinds depths
    depths
        .kinds
        .get(&EventType::YaraScannableSample)
        .copied()
        .ok_or_else(|| thorium::Error::new("depths is missing the yara scan kind".to_owned()))
}

/// Verify a worker scoped pop claims events that clear back down to empty depths
#[tokio::test]
#[serial_test::serial]
async fn pop_with_worker_then_clear() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a unique worker for this test
    let worker = test_worker("w-popclear");
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload two unique samples to emit two scan events
    let first = client
        .files
        .create(unique_sample(vec![group.clone()]))
        .await?;
    let second = client.files.create(unique_sample(vec![group])).await?;
    // claim everything pending with our worker until both our events are claimed
    let (popped, matched) = claim_shas(&client, &worker, &[&first.sha256, &second.sha256]).await?;
    // both of our scan events must have been claimed
    is!(matched.len(), 2);
    // our claimed events must now show up as in flight with a live worker
    let depth = yara_depth(&client).await?;
    assert!(
        depth.in_flight >= 2,
        "claimed events must count as in flight"
    );
    let depths = client.events.depths().await?;
    assert!(depths.workers >= 1, "a popping worker must count as live");
    // clear everything we claimed from our workers in flight keys
    clear_ids(
        &client,
        Some(&worker),
        popped.iter().map(|event| event.id).collect(),
    )
    .await?;
    // the depths must return to zero once the queues are drained; retry a few
    // times since concurrent suites can race new events into the shared queue
    let mut settled = false;
    for _ in 0..10 {
        // consume any backlog so the pending queue can actually reach zero
        drain_yara_queue(&client, &worker).await?;
        // check if the yara queues have settled to zero
        let depth = yara_depth(&client).await?;
        if depth.queued == 0 && depth.in_flight == 0 {
            settled = true;
            break;
        }
        // give racing suites a beat to go quiet
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(settled, "yara queue depths never returned to zero");
    Ok(())
}

/// Verify resetting one worker never replays a peer workers in flight events
#[tokio::test]
#[serial_test::serial]
async fn scoped_reset_isolation() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build two simulated replica workers plus a prober
    let worker_a = test_worker("w-iso-a");
    let worker_b = test_worker("w-iso-b");
    let prober = test_worker("w-iso-probe");
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a sample and claim its event with worker a
    let sample_a = client
        .files
        .create(unique_sample(vec![group.clone()]))
        .await?;
    let (popped_a, _) = claim_shas(&client, &worker_a, &[&sample_a.sha256]).await?;
    // upload a second sample and claim its event with worker b
    let sample_b = client.files.create(unique_sample(vec![group])).await?;
    let (popped_b, _) = claim_shas(&client, &worker_b, &[&sample_b.sha256]).await?;
    // reset only worker a so its claims restore to the pending queue
    client
        .events
        .reset_worker(EventType::YaraScannableSample, &worker_a)
        .await?;
    // worker a's event must be poppable again
    let (popped_probe, _) = claim_shas(&client, &prober, &[&sample_a.sha256]).await?;
    // worker b's event must still be held in its own in flight keys
    let stole_b = popped_probe.iter().any(|event| {
        matches!(&event.data, EventData::YaraScannableSample(inner) if inner.sha256 == sample_b.sha256)
    });
    assert!(
        !stole_b,
        "a scoped reset must not replay a peer workers events"
    );
    // clean up every claim so later depth assertions stay stable
    clear_ids(
        &client,
        Some(&prober),
        popped_probe.iter().map(|event| event.id).collect(),
    )
    .await?;
    clear_ids(
        &client,
        Some(&worker_b),
        popped_b.iter().map(|event| event.id).collect(),
    )
    .await?;
    // worker a's keys are already empty after the reset but clear defensively
    clear_ids(
        &client,
        Some(&worker_a),
        popped_a.iter().map(|event| event.id).collect(),
    )
    .await?;
    // drain anything the reset restored
    drain_yara_queue(&client, &prober).await?;
    Ok(())
}

/// Verify the reaper reclaims a dead owners in flight events via heartbeats
#[tokio::test]
#[serial_test::serial]
async fn heartbeat_reap_dead_owner() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a worker that will die and one that keeps beating
    let dead = test_worker("w-reap-dead");
    let live = test_worker("w-reap-live");
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a sample and claim its event with the soon to be dead worker
    let sample = client.files.create(unique_sample(vec![group])).await?;
    let (popped, _) = claim_shas(&client, &dead, &[&sample.sha256]).await?;
    assert!(!popped.is_empty());
    // register the live worker in the registry
    client.events.heartbeat(&live).await?;
    // wait past the 3s worker expiry without any beats from the dead worker
    tokio::time::sleep(Duration::from_secs(4)).await;
    // beat the live worker so it survives the reap
    client.events.heartbeat(&live).await?;
    // reap dead workers
    let summary = client.events.reap().await?;
    // our dead worker must have been reaped with its events reclaimed
    assert!(summary.dead_workers >= 1, "the dead worker must be reaped");
    assert!(
        summary.reclaimed >= 1,
        "the dead workers events must reclaim"
    );
    // the live worker must still count as alive after the reap
    let depths = client.events.depths().await?;
    assert!(
        depths.workers >= 1,
        "a beating worker must survive the reap"
    );
    // the reclaimed event must be poppable again
    let prober = test_worker("w-reap-probe");
    let (popped_probe, _) = claim_shas(&client, &prober, &[&sample.sha256]).await?;
    // clean up every claim
    clear_ids(
        &client,
        Some(&prober),
        popped_probe.iter().map(|event| event.id).collect(),
    )
    .await?;
    drain_yara_queue(&client, &prober).await?;
    Ok(())
}

/// Verify the age backstop reclaims leases from a live but wedged worker
#[tokio::test]
#[serial_test::serial]
async fn age_backstop_live_owner() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a worker that stays alive but never clears its claims
    let wedged = test_worker("w-backstop");
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a sample and claim its event with our wedged worker
    let sample = client.files.create(unique_sample(vec![group])).await?;
    let (popped, _) = claim_shas(&client, &wedged, &[&sample.sha256]).await?;
    assert!(!popped.is_empty());
    // keep beating while the lease ages past the 4s backstop
    for _ in 0..5 {
        // wait one heartbeat interval
        tokio::time::sleep(Duration::from_secs(1)).await;
        // beat so this worker is never presumed dead
        client.events.heartbeat(&wedged).await?;
    }
    // reap over-aged leases
    let summary = client.events.reap().await?;
    // our live workers aged lease must have been backstop reclaimed
    assert!(
        summary.backstopped >= 1,
        "aged leases must backstop reclaim"
    );
    // the reclaimed event must be poppable again
    let prober = test_worker("w-backstop-probe");
    let (popped_probe, _) = claim_shas(&client, &prober, &[&sample.sha256]).await?;
    // clean up every claim
    clear_ids(
        &client,
        Some(&prober),
        popped_probe.iter().map(|event| event.id).collect(),
    )
    .await?;
    drain_yara_queue(&client, &prober).await?;
    Ok(())
}

/// Verify reclaimed events restore scored by their own body timestamp in order
#[tokio::test]
#[serial_test::serial]
async fn lease_restore_score() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a worker to claim and reset events with
    let worker = test_worker("w-lease");
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload two samples far enough apart that their timestamps must differ
    let first = client
        .files
        .create(unique_sample(vec![group.clone()]))
        .await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let second = client.files.create(unique_sample(vec![group])).await?;
    // claim both scan events with our worker
    let (_, matched) = claim_shas(&client, &worker, &[&first.sha256, &second.sha256]).await?;
    let (event_one, event_two) = (&matched[0], &matched[1]);
    // reset our worker so its claims restore to the pending queue
    client
        .events
        .reset_worker(EventType::YaraScannableSample, &worker)
        .await?;
    // each restored events pending score must equal its own body timestamp,
    // never the much newer lease time it was popped at
    let score_one =
        test_utilities::pending_event_score(&EventType::YaraScannableSample, &event_one.id)
            .await?
            .expect("first restored event is missing from the pending queue");
    is!(score_one, event_one.timestamp.timestamp());
    let score_two =
        test_utilities::pending_event_score(&EventType::YaraScannableSample, &event_two.id)
            .await?
            .expect("second restored event is missing from the pending queue");
    is!(score_two, event_two.timestamp.timestamp());
    // re-popping must return the restored events oldest first
    let (replayed, _) = claim_shas(&client, &worker, &[&first.sha256, &second.sha256]).await?;
    let index_one = replayed
        .iter()
        .position(|event| event.id == event_one.id)
        .expect("the first restored event never replayed");
    let index_two = replayed
        .iter()
        .position(|event| event.id == event_two.id)
        .expect("the second restored event never replayed");
    assert!(
        index_one < index_two,
        "restored events must replay oldest first"
    );
    // clean up every claim
    clear_ids(
        &client,
        Some(&worker),
        replayed.iter().map(|event| event.id).collect(),
    )
    .await?;
    drain_yara_queue(&client, &worker).await?;
    Ok(())
}

/// Verify legacy un-owned in flight events reclaim via the reap age gate
#[tokio::test]
#[serial_test::serial]
async fn legacy_inflight_reclaim() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a sample to emit a scan event
    let sample = client.files.create(unique_sample(vec![group])).await?;
    // wait until the events creation score is older than the 4s legacy age gate
    tokio::time::sleep(Duration::from_secs(5)).await;
    // pop with the legacy owner-less path so the event lands in the shared keys
    let mut popped = Vec::default();
    for _ in 0..30 {
        // pop a legacy page
        let page = pop_page(&client, None, 500).await?;
        // an empty page means the pending queue is drained
        if page.is_empty() {
            break;
        }
        // keep this page so we can clean the legacy keys up afterwards
        popped.extend(page);
        // stop once our event has been popped
        if popped.iter().any(|event| {
            matches!(&event.data, EventData::YaraScannableSample(inner) if inner.sha256 == sample.sha256)
        }) {
            break;
        }
    }
    // our event must have been popped onto the legacy in flight keys
    assert!(
        popped.iter().any(|event| {
            matches!(&event.data, EventData::YaraScannableSample(inner) if inner.sha256 == sample.sha256)
        }),
        "the scan event never popped on the legacy path"
    );
    // reap so the age gated legacy entries restore to the pending queue
    let summary = client.events.reap().await?;
    assert!(summary.legacy_reclaimed >= 1, "legacy entries must reclaim");
    // clear every popped id from the legacy keys so young foreign events we
    // popped alongside ours never leak as stuck legacy in flight entries
    clear_ids(&client, None, popped.iter().map(|event| event.id).collect()).await?;
    // the reclaimed event must be poppable again
    let prober = test_worker("w-legacy-probe");
    let (popped_probe, _) = claim_shas(&client, &prober, &[&sample.sha256]).await?;
    // clean up every claim
    clear_ids(
        &client,
        Some(&prober),
        popped_probe.iter().map(|event| event.id).collect(),
    )
    .await?;
    drain_yara_queue(&client, &prober).await?;
    Ok(())
}

/// Verify draining a worker restores its claims and deregisters it
#[tokio::test]
#[serial_test::serial]
async fn drain_worker_test() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a worker to drain
    let worker = test_worker("w-drain");
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a sample and claim its event with our worker
    let sample = client.files.create(unique_sample(vec![group])).await?;
    let (popped, _) = claim_shas(&client, &worker, &[&sample.sha256]).await?;
    // drain our worker back to the pending queues
    let drained = client.events.drain_worker(&worker).await?;
    // every claim this worker held must have been drained
    is!(drained, popped.len() as u64);
    // a second drain must find nothing left
    let drained_again = client.events.drain_worker(&worker).await?;
    is!(drained_again, 0);
    // the drained event must be poppable again
    let prober = test_worker("w-drain-probe");
    let (popped_probe, _) = claim_shas(&client, &prober, &[&sample.sha256]).await?;
    // clean up every claim
    clear_ids(
        &client,
        Some(&prober),
        popped_probe.iter().map(|event| event.id).collect(),
    )
    .await?;
    drain_yara_queue(&client, &prober).await?;
    Ok(())
}

/// Verify rule CRUD bumps the yara generation and bool resets never clear it
#[tokio::test]
#[serial_test::serial]
async fn cache_generations_bump() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // snapshot the yara rules generation before any of our writes
    let opts = thorium::models::EventCacheStatusOpts::default();
    let gen_start = client.events.get_cache_status(&opts).await?.yara_rules_gen;
    // create a yara rule entity
    let req = generators::gen_entity(&group, generators::gen_yara_meta());
    let resp = client.entities.create(req).await?;
    // creating a rule must bump the yara rules generation
    let gen_created = client.events.get_cache_status(&opts).await?.yara_rules_gen;
    assert!(
        gen_created > gen_start,
        "rule create must bump the yara gen"
    );
    // reset the boolean staleness flags
    let reset_opts = thorium::models::EventCacheStatusOpts::default().reset();
    client.events.get_cache_status(&reset_opts).await?;
    // a bool reset must never clear the monotonic generation counters
    let gen_reset = client.events.get_cache_status(&opts).await?.yara_rules_gen;
    assert!(gen_reset >= gen_created, "a reset must never clear gens");
    // update our rule entity without touching its groups or metadata
    let update = thorium::models::EntityUpdate::default().description("gen bump check");
    client.entities.update(resp.id, update).await?;
    // updating a rule must bump the yara rules generation
    let gen_updated = client.events.get_cache_status(&opts).await?.yara_rules_gen;
    assert!(
        gen_updated > gen_reset,
        "rule update must bump the yara gen"
    );
    // delete our rule entity
    client.entities.delete(resp.id).await?;
    // deleting a rule must bump the yara rules generation again
    let gen_deleted = client.events.get_cache_status(&opts).await?.yara_rules_gen;
    assert!(
        gen_deleted > gen_updated,
        "rule delete must bump the yara gen"
    );
    Ok(())
}

/// Verify the depths route reports queued, in flight, and worker counts
#[tokio::test]
#[serial_test::serial]
async fn depths_route() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group
    let group = generators::groups(1, &client).await?.remove(0).name;
    // upload a sample to emit a scan event
    let sample = client.files.create(unique_sample(vec![group])).await?;
    // wait for the event to mature so a queued depth is observable
    tokio::time::sleep(Duration::from_secs(4)).await;
    // every kind of event must report a depth entry
    let depths = client.events.depths().await?;
    is!(
        depths.kinds.len(),
        <thorium::models::EventType as strum::IntoEnumIterator>::iter().count()
    );
    // our pending scan event must count as queued
    let depth = yara_depth(&client).await?;
    assert!(depth.queued >= 1, "a pending event must count as queued");
    // claim our event with a worker
    let worker = test_worker("w-depths");
    let (popped, _) = claim_shas(&client, &worker, &[&sample.sha256]).await?;
    // our claimed event must count as in flight with a live worker
    let depth = yara_depth(&client).await?;
    assert!(depth.in_flight >= 1, "a claimed event must count in flight");
    let depths = client.events.depths().await?;
    assert!(depths.workers >= 1, "a popping worker must count as live");
    // clean up every claim
    clear_ids(
        &client,
        Some(&worker),
        popped.iter().map(|event| event.id).collect(),
    )
    .await?;
    drain_yara_queue(&client, &worker).await?;
    Ok(())
}

/// Verify the serialized yara ruleset cache round trips and misses cleanly
#[tokio::test]
#[serial_test::serial]
async fn yara_ruleset_cache_roundtrip() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a unique engine version namespace so reruns never collide
    let engine = format!("1.21.0-test-{}", Uuid::new_v4().simple());
    // build a fake content hash for our first ruleset
    let hash_one = Uuid::new_v4().simple().to_string();
    // a cache miss must come back as a clean none rather than an error
    let miss = client.events.get_yara_ruleset(&engine, &hash_one).await?;
    assert!(miss.is_none(), "a cache miss must be a clean none");
    // build a random serialized ruleset big enough to exercise streaming
    let mut data = vec![0u8; 1 << 20];
    rand::rng().fill_bytes(&mut data);
    // upload our serialized ruleset
    client
        .events
        .upload_yara_ruleset(&engine, &hash_one, data.clone())
        .await?;
    // the cached ruleset must round trip byte for byte
    let fetched = client
        .events
        .get_yara_ruleset(&engine, &hash_one)
        .await?
        .expect("the cached ruleset is missing after upload");
    is!(fetched.len(), data.len());
    assert!(fetched == data, "the cached ruleset bytes must round trip");
    // uploading a sibling hash must prune the old same engine version entry
    let hash_two = Uuid::new_v4().simple().to_string();
    client
        .events
        .upload_yara_ruleset(&engine, &hash_two, b"second".to_vec())
        .await?;
    // the old entry must now be a clean miss
    let pruned = client.events.get_yara_ruleset(&engine, &hash_one).await?;
    assert!(
        pruned.is_none(),
        "sibling rulesets must be pruned on upload"
    );
    // the fresh entry must still round trip
    let kept = client.events.get_yara_ruleset(&engine, &hash_two).await?;
    is!(kept, Some(b"second".to_vec()));
    // path segments with unsafe characters must be rejected not routed to s3
    let unsafe_get = client.events.get_yara_ruleset("has space", &hash_two).await;
    test_utilities::assert_status(unsafe_get, 400, "unsafe engine version segment")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Worker stats publish tests (M5a)
//
// These cover the admin-only counter publish route: reports land in TTL'd
// redis keys that expire on the worker death clock (the testing config pins
// worker_expiry_secs to 3s so real expiry is cheap to assert), non-admins get
// a 401 before anything is read, and unsafe worker ids or oversized counter
// maps are rejected before any key is built.
// ---------------------------------------------------------------------------

/// Verify a published counter report lands in a TTL'd redis key, upserts in
/// place, and expires on the worker death clock
#[tokio::test]
#[serial_test::serial]
async fn publish_worker_stats_roundtrip() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a unique worker id for this test
    let worker = test_worker("w-stats-pub");
    // build a counter report to publish
    let report = WorkerStatsReport {
        counters: BTreeMap::from([("scanned".to_owned(), 5)]),
        reported_at: Utc::now(),
    };
    // publish this workers counters
    client
        .events
        .publish_worker_stats(EventType::YaraScannableSample, &worker, &report)
        .await?;
    // the report must land in redis under a TTL'd key
    let (stored, ttl) =
        test_utilities::worker_stats_report(EventType::YaraScannableSample, &worker)
            .await?
            .expect("the published report is missing from redis");
    is!(stored, report);
    // the TTL must ride the testing configs tiny worker expiry
    assert!(
        ttl > 0 && ttl <= 3,
        "the counter key must expire on the worker death clock but has ttl {ttl}"
    );
    // publishing again must upsert the existing key
    let updated = WorkerStatsReport {
        counters: BTreeMap::from([("scanned".to_owned(), 9)]),
        reported_at: Utc::now(),
    };
    client
        .events
        .publish_worker_stats(EventType::YaraScannableSample, &worker, &updated)
        .await?;
    // the stored report must now carry the updated counters
    let (stored, _) = test_utilities::worker_stats_report(EventType::YaraScannableSample, &worker)
        .await?
        .expect("the upserted report is missing from redis");
    is!(stored, updated);
    // the key must expire within the worker expiry window of its last publish
    tokio::time::sleep(Duration::from_secs(4)).await;
    let expired =
        test_utilities::worker_stats_report(EventType::YaraScannableSample, &worker).await?;
    assert!(
        expired.is_none(),
        "the counter key must expire with its worker"
    );
    Ok(())
}

/// Verify a non admin publish is rejected with a 401 before anything is written
#[tokio::test]
#[serial_test::serial]
async fn publish_worker_stats_requires_admin() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a regular user client
    let user_client = generators::client(&client).await?;
    // build a unique worker id and a tiny report
    let worker = test_worker("w-stats-auth");
    let report = WorkerStatsReport {
        counters: BTreeMap::from([("scanned".to_owned(), 1)]),
        reported_at: Utc::now(),
    };
    // a non admin publish must be rejected with a 401
    let resp = user_client
        .events
        .publish_worker_stats(EventType::ReactionTrigger, &worker, &report)
        .await;
    fail!(resp, 401);
    // nothing may have been written for this worker
    let stored = test_utilities::worker_stats_report(EventType::ReactionTrigger, &worker).await?;
    assert!(stored.is_none(), "a rejected publish must write nothing");
    Ok(())
}

/// Verify unsafe worker ids and oversized counter maps are rejected with a 400
/// before any redis key is built
#[tokio::test]
#[serial_test::serial]
async fn publish_worker_stats_validates_input() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a tiny valid report
    let report = WorkerStatsReport {
        counters: BTreeMap::from([("scanned".to_owned(), 1)]),
        reported_at: Utc::now(),
    };
    // a worker id with unsafe characters must be rejected not embedded in a key
    let resp = client
        .events
        .publish_worker_stats(EventType::ReactionTrigger, "has space", &report)
        .await;
    fail!(resp, 400);
    // a counter map over the entry cap must be rejected
    let oversized = WorkerStatsReport {
        counters: (0..=WorkerStatsReport::MAX_COUNTERS)
            .map(|entry| (format!("counter_{entry}"), entry as u64))
            .collect(),
        reported_at: Utc::now(),
    };
    let worker = test_worker("w-stats-bounds");
    let resp = client
        .events
        .publish_worker_stats(EventType::ReactionTrigger, &worker, &oversized)
        .await;
    fail!(resp, 400);
    // a counter name over the length cap must be rejected
    let long_name = WorkerStatsReport {
        counters: BTreeMap::from([("x".repeat(WorkerStatsReport::MAX_COUNTER_NAME_LEN + 1), 1)]),
        reported_at: Utc::now(),
    };
    let resp = client
        .events
        .publish_worker_stats(EventType::ReactionTrigger, &worker, &long_name)
        .await;
    fail!(resp, 400);
    // none of the rejected publishes may have written anything
    let stored = test_utilities::worker_stats_report(EventType::ReactionTrigger, &worker).await?;
    assert!(stored.is_none(), "rejected publishes must write nothing");
    Ok(())
}

// ---------------------------------------------------------------------------
// Live stats aggregate tests (M5b)
//
// These cover the admin-only live aggregate route: per kind counter sums are
// built only from registered live workers reports, the per worker breakdown
// round trips byte-equal, dead or unregistered workers never contribute, and
// non-admins get a 401 before anything is read. Counter names are uuid-unique
// per test so a leftover key inside the 3s TTL can never pollute exact sums.
// ---------------------------------------------------------------------------

/// Build a counter report with one uniquely named counter
///
/// # Arguments
///
/// * `counter` - The unique counter name to report
/// * `count` - The cumulative count to report
fn counter_report(counter: &str, count: u64) -> WorkerStatsReport {
    // build a report carrying just this one counter
    WorkerStatsReport {
        counters: BTreeMap::from([(counter.to_owned(), count)]),
        reported_at: Utc::now(),
    }
}

/// Verify the live aggregate sums counters across live workers and matches an
/// adjacent depths readout
#[tokio::test]
#[serial_test::serial]
async fn stats_aggregate_sums_live_workers() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build two unique workers and a unique counter name for this test
    let worker_one = test_worker("w-stats-sum-one");
    let worker_two = test_worker("w-stats-sum-two");
    let counter = format!("scanned-{}", Uuid::new_v4().simple());
    // register both workers with a live heartbeat
    client.events.heartbeat(&worker_one).await?;
    client.events.heartbeat(&worker_two).await?;
    // publish each workers counters for the same kind
    let report_one = counter_report(&counter, 5);
    let report_two = counter_report(&counter, 7);
    client
        .events
        .publish_worker_stats(EventType::YaraScannableSample, &worker_one, &report_one)
        .await?;
    client
        .events
        .publish_worker_stats(EventType::YaraScannableSample, &worker_two, &report_two)
        .await?;
    // get the live stats aggregate
    let stats = client.events.stats().await?;
    // every kind of event must always be present in the aggregate
    for kind in <thorium::models::EventType as strum::IntoEnumIterator>::iter() {
        assert!(
            stats.kinds.contains_key(&kind),
            "the aggregate must report stats for {kind}"
        );
    }
    // our counter must sum across both live workers
    let kind_stats = &stats.kinds[&EventType::YaraScannableSample];
    is!(kind_stats.counters.get(&counter).copied(), Some(12));
    // both workers reports must ride the per worker breakdown byte equal
    let one_stats = &stats.per_worker[&worker_one][&EventType::YaraScannableSample];
    is!(one_stats.clone(), report_one);
    let two_stats = &stats.per_worker[&worker_two][&EventType::YaraScannableSample];
    is!(two_stats.clone(), report_two);
    // both of our heartbeated workers must count as live
    assert!(
        stats.workers >= 2,
        "both heartbeated workers must be live but got {}",
        stats.workers
    );
    // the aggregates depths must match an adjacent depths readout since no
    // events flow while this serial group holds the queues
    let depths = client.events.depths().await?;
    for (kind, kind_stats) in &stats.kinds {
        // get this kinds depths from the depths route
        let depth = &depths.kinds[kind];
        is!(kind_stats.queued, depth.queued);
        is!(kind_stats.in_flight, depth.in_flight);
    }
    Ok(())
}

/// Verify unregistered and dead workers never contribute to the aggregate
#[tokio::test]
#[serial_test::serial]
async fn stats_excludes_unregistered_and_dead_workers() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a unique worker and counter name for this test
    let worker = test_worker("w-stats-live");
    let counter = format!("scanned-{}", Uuid::new_v4().simple());
    // publish counters without ever heartbeating this worker
    let report = counter_report(&counter, 3);
    client
        .events
        .publish_worker_stats(EventType::YaraScannableSample, &worker, &report)
        .await?;
    // an unregistered workers report must not reach the aggregate
    let stats = client.events.stats().await?;
    assert!(
        !stats.per_worker.contains_key(&worker),
        "an unregistered worker must not ride the per worker breakdown"
    );
    let kind_stats = &stats.kinds[&EventType::YaraScannableSample];
    assert!(
        !kind_stats.counters.contains_key(&counter),
        "an unregistered workers counters must not reach the sums"
    );
    // registering this worker must surface its already published report
    client.events.heartbeat(&worker).await?;
    let stats = client.events.stats().await?;
    let worker_stats = &stats.per_worker[&worker][&EventType::YaraScannableSample];
    is!(worker_stats.clone(), report);
    let kind_stats = &stats.kinds[&EventType::YaraScannableSample];
    is!(kind_stats.counters.get(&counter).copied(), Some(3));
    // once this worker is past the death clock it must vanish from the view
    tokio::time::sleep(Duration::from_secs(4)).await;
    let stats = client.events.stats().await?;
    assert!(
        !stats.per_worker.contains_key(&worker),
        "a dead workers report must age out of the per worker breakdown"
    );
    let kind_stats = &stats.kinds[&EventType::YaraScannableSample];
    assert!(
        !kind_stats.counters.contains_key(&counter),
        "a dead workers counters must age out of the sums"
    );
    Ok(())
}

/// Verify a non admin stats read is rejected with a 401
#[tokio::test]
#[serial_test::serial]
async fn stats_requires_admin() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a regular user client
    let user_client = generators::client(&client).await?;
    // a non admin stats read must be rejected with a 401
    let resp = user_client.events.stats().await;
    fail!(resp, 401);
    Ok(())
}

// ---------------------------------------------------------------------------
// Snapshot path tests (M5d)
//
// These cover the admin-only snapshot trigger: row timestamps quantize to the
// testing configs 2s stats_snapshot_secs, the window-keyed SET NX gate turns
// a second same-window trigger into a 204, persisted rows never carry the per
// worker breakdown, and a rejected non-admin trigger neither writes a row nor
// consumes the gate. Each test first aligns to just past a window boundary so
// its triggers can never straddle one.
// ---------------------------------------------------------------------------

/// Sleep until just past the next snapshot window boundary
///
/// Lands ~100ms into a fresh 2s window so the following requests have ~1.9s
/// of margin before the window rolls over.
async fn align_to_snapshot_window() {
    // figure out how far into the current 2s window we are
    let into_window = Utc::now().timestamp_millis().rem_euclid(2_000);
    // sleep past the next boundary plus a small landing margin
    tokio::time::sleep(Duration::from_millis((2_100 - into_window) as u64)).await;
}

/// Verify a snapshot trigger persists one quantized row per window and the
/// window gate turns a same-window peer trigger into a clean skip
#[tokio::test]
#[serial_test::serial]
async fn snapshot_writes_quantized_row_once_per_window() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // land just past a window boundary so this test never straddles one
    align_to_snapshot_window().await;
    // the first trigger in a window must win the gate and persist a row
    let snapshot = client
        .events
        .snapshot_stats()
        .await?
        .expect("the first trigger in a window must persist a row");
    // the rows timestamp must be quantized to the snapshot cadence
    is!(snapshot.snapshot_at.timestamp().rem_euclid(2), 0);
    // every kind of event must ride the persisted row
    for kind in <thorium::models::EventType as strum::IntoEnumIterator>::iter() {
        assert!(
            snapshot.kinds.contains_key(&kind),
            "the snapshot must persist stats for {kind}"
        );
    }
    // the persisted shape must never carry the per worker breakdown; this
    // serializes the exact struct the db layer writes to scylla
    let serialized = serde_json::to_value(&snapshot)
        .map_err(|err| thorium::Error::new(format!("Failed to serialize snapshot: {err}")))?;
    assert!(
        serialized.get("per_worker").is_none(),
        "persisted snapshots must never carry the per worker breakdown"
    );
    // a second trigger in the same window must hit the gate
    let gated = client.events.snapshot_stats().await?;
    assert!(
        gated.is_none(),
        "a second trigger in one window must be gated into a 204"
    );
    // the next window must persist a fresh row with a later timestamp
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    let next = client
        .events
        .snapshot_stats()
        .await?
        .expect("the next window must persist a fresh row");
    assert!(
        next.snapshot_at > snapshot.snapshot_at,
        "the next windows row must carry a later quantized timestamp"
    );
    Ok(())
}

/// Verify a non admin trigger is rejected with a 401 before the gate is taken
#[tokio::test]
#[serial_test::serial]
async fn snapshot_requires_admin_and_takes_no_gate() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a regular user client
    let user_client = generators::client(&client).await?;
    // land just past a window boundary so both triggers share one window
    align_to_snapshot_window().await;
    // a non admin trigger must be rejected with a 401
    let resp = user_client.events.snapshot_stats().await;
    fail!(resp, 401);
    // the rejected trigger must not have consumed this windows gate
    let snapshot = client.events.snapshot_stats().await?;
    assert!(
        snapshot.is_some(),
        "a rejected trigger must neither write a row nor take the gate"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Stats history tests (M5e)
//
// These cover the admin-only ranged history read: the half open [start, end)
// contract, newest-row-per-stride downsampling across partition boundaries
// (the testing config pins 2s windows in 10s partitions), the wide-range
// stride path where sparse windows stay honest gaps, range validation, and
// the non-admin 401. Rows are backfilled at fixed past timestamps so the
// snapshot tests rows (written at now) can never pollute these ranges.
// ---------------------------------------------------------------------------

/// Build and backfill one scan stats row at a fixed timestamp
///
/// The workers count doubles as a row identity so tests can tell which rows a
/// stride kept.
///
/// # Arguments
///
/// * `snapshot_at` - The timestamp to backfill this row at
/// * `workers` - The row identity to carry in the workers gauge
async fn backfill_row(snapshot_at: DateTime<Utc>, workers: u64) -> Result<(), thorium::Error> {
    // build a minimal snapshot row for this timestamp
    let snapshot = ScanStatsSnapshot {
        snapshot_at,
        workers,
        kinds: HashMap::new(),
    };
    // backfill this row straight into the test scylla
    test_utilities::insert_scan_stats_row(&snapshot).await
}

/// Verify history reads honor the half open range and return rows oldest first
#[tokio::test]
#[serial_test::serial]
async fn stats_history_returns_oldest_first_within_range() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // backfill rows every window at a fixed past base no other test touches
    let base = Utc.with_ymd_and_hms(2020, 1, 10, 0, 0, 0).unwrap();
    for index in 0..4 {
        backfill_row(base + chrono::Duration::seconds(index * 2), index as u64).await?;
    }
    // read the two middle rows through the half open range
    let history = client
        .events
        .stats_history(
            base + chrono::Duration::seconds(2),
            base + chrono::Duration::seconds(6),
            None,
        )
        .await?;
    // only the rows inside [start, end) may come back, oldest first
    is!(history.len(), 2);
    is!(history[0].snapshot_at, base + chrono::Duration::seconds(2));
    is!(history[0].workers, 1);
    is!(history[1].snapshot_at, base + chrono::Duration::seconds(4));
    is!(history[1].workers, 2);
    Ok(())
}

/// Verify strides keep the newest row per window across partition boundaries
#[tokio::test]
#[serial_test::serial]
async fn stats_history_downsamples_by_stride() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // backfill rows every 2s window across the 10s partition boundary
    let base = Utc.with_ymd_and_hms(2020, 2, 1, 0, 0, 0).unwrap();
    for index in 0..10 {
        backfill_row(base + chrono::Duration::seconds(index * 2), index as u64).await?;
    }
    // read the whole range downsampled to a 4s stride
    let history = client
        .events
        .stats_history(base, base + chrono::Duration::seconds(20), Some(4))
        .await?;
    // each 4s window must keep exactly its newest row, oldest first
    is!(history.len(), 5);
    for (index, snapshot) in history.iter().enumerate() {
        // window k spans [4k, 4k + 4) so its newest 2s row sits at 4k + 2
        let expected = base + chrono::Duration::seconds((index as i64) * 4 + 2);
        is!(snapshot.snapshot_at, expected);
    }
    Ok(())
}

/// Verify wide ranges take the stride path and sparse windows stay gaps
#[tokio::test]
#[serial_test::serial]
async fn stats_history_strides_large_ranges() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // pick a range whose raw row estimate (range / 2s cadence) tops the 5,000
    // point cap so the read must take the one-row-per-window stride path
    let base = Utc.with_ymd_and_hms(2020, 3, 1, 0, 0, 0).unwrap();
    let end = base + chrono::Duration::seconds(15_000);
    // backfill three sparse rows far apart inside the range
    for (index, offset) in [10, 7_000, 14_000].iter().enumerate() {
        backfill_row(base + chrono::Duration::seconds(*offset), index as u64).await?;
    }
    // read the whole range with the default stride
    let history = client.events.stats_history(base, end, None).await?;
    // only the three populated windows may come back; empty windows stay gaps
    is!(history.len(), 3);
    is!(history[0].snapshot_at, base + chrono::Duration::seconds(10));
    is!(
        history[1].snapshot_at,
        base + chrono::Duration::seconds(7_000)
    );
    is!(
        history[2].snapshot_at,
        base + chrono::Duration::seconds(14_000)
    );
    Ok(())
}

/// Verify inverted and over-wide ranges are rejected with a 400
#[tokio::test]
#[serial_test::serial]
async fn stats_history_rejects_bad_ranges() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // an inverted range must be rejected
    let now = Utc::now();
    let resp = client
        .events
        .stats_history(now, now - chrono::Duration::seconds(60), None)
        .await;
    fail!(resp, 400);
    // a range wider than the retention window must be rejected since nothing
    // older than the retention can exist
    let resp = client
        .events
        .stats_history(now - chrono::Duration::seconds(2_600_000), now, None)
        .await;
    fail!(resp, 400);
    Ok(())
}

/// Verify a non admin history read is rejected with a 401
#[tokio::test]
#[serial_test::serial]
async fn stats_history_requires_admin() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a regular user client
    let user_client = generators::client(&client).await?;
    // a non admin history read must be rejected with a 401
    let resp = user_client
        .events
        .stats_history(Utc::now() - chrono::Duration::seconds(60), Utc::now(), None)
        .await;
    fail!(resp, 401);
    Ok(())
}

// ---------------------------------------------------------------------------
// Hardening tests (M5h)
//
// The 401 matrix hits every admin-only stats surface (plus the depths gauge,
// which never had a non-admin test) with every non-admin role, and then proves
// none of the rejected snapshot triggers consumed a window gate.
// ---------------------------------------------------------------------------

/// Verify every stats surface rejects every non admin role with a 401
#[tokio::test]
#[serial_test::serial]
async fn events_stats_routes_401_matrix() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
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
        // the live aggregate is admin only
        fail!(user_client.events.stats().await, 401);
        // the snapshot trigger is admin only
        fail!(user_client.events.snapshot_stats().await, 401);
        // the history read is admin only
        let history = user_client
            .events
            .stats_history(Utc::now() - chrono::Duration::seconds(60), Utc::now(), None)
            .await;
        fail!(history, 401);
        // the counter publish is admin only
        let worker = test_worker("w-matrix");
        let report = counter_report("matrix", 1);
        let publish = user_client
            .events
            .publish_worker_stats(EventType::ReactionTrigger, &worker, &report)
            .await;
        fail!(publish, 401);
        // the depths gauge is admin only too
        fail!(user_client.events.depths().await, 401);
    }
    // land in a fresh window so no earlier tests gate can confuse the check
    align_to_snapshot_window().await;
    // none of the rejected triggers may have consumed a window gate
    let snapshot = client.events.snapshot_stats().await?;
    assert!(
        snapshot.is_some(),
        "rejected triggers must never take a window gate"
    );
    Ok(())
}
