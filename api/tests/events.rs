//! Tests the admin event queue peek route, the event queue inspection helper, how workflow
//! graph events are queued and wait to mature, and how model refresh events are queued

use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::time::Duration;
use thorium::models::{
    Event, EventData, EventIds, EventPeekOpts, EventPopOpts, EventType, ModelEndpointEntityRequest,
    ModelProvider, Sample, TagRequest, WorkflowCatalogEntityRequest, WorkflowGraphChange,
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
        admin.events.clear(kind, &ids).await?;
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
        .clear(EventType::ReactionTrigger, &ids)
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
