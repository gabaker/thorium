//! Tests the admin event queue peek route and the event queue inspection helper

use std::collections::HashSet;
use thorium::models::{
    Event, EventData, EventIds, EventPeekOpts, EventPopOpts, EventType, Sample, TagRequest,
};
use thorium::test_utilities::{self, events, generators};
use thorium::{Error, fail, is};
use uuid::Uuid;

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
