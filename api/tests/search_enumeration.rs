//! The search streamer's partition enumerations return every key once across the token ranges
//! it splits the ring into
#![cfg(feature = "search-streamer")]

use chrono::Utc;
use futures::StreamExt;
use std::collections::BTreeMap;
use uuid::Uuid;

use scylla::client::pager::TypedRowStream;
use thorium::Error;
use thorium::models::schema::queries::search;
use thorium::test_utilities::{self, generators};

/// How many results and tagged items the tests write
const KEYS: usize = 60;

/// Split the token ring into ranges the way the search streamer's init does: equal chunks whose
/// inclusive bounds don't overlap, the last ending at the ring's end
///
/// # Arguments
///
/// * `chunk_count` - The number of chunks
fn token_ranges(chunk_count: u64) -> Vec<(i64, i64)> {
    // a chunk count of 1 doesn't fit in an i64, so the size is clamped as the streamer does
    let chunk_size = i64::try_from(u64::MAX / chunk_count).unwrap_or(i64::MAX);
    let mut ranges = Vec::new();
    let mut start = i64::MIN;
    for _ in 1..chunk_count {
        let end = start + chunk_size;
        ranges.push((start, end));
        start = end + 1;
    }
    ranges.push((start, i64::MAX));
    ranges
}

/// Read every row a generated enumeration returns across the ranges of a ring split, counting
/// each enumerated key whose text starts with a prefix
///
/// # Arguments
///
/// * `chunk_count` - How many ranges to split the ring into
/// * `prefix` - The prefix of the keys to count
/// * `key` - Picks the enumerated key out of a row
/// * `read` - Streams the rows of one token range through the generated enumeration
async fn enumerate<R>(
    chunk_count: u64,
    prefix: &str,
    key: fn(R) -> String,
    read: impl AsyncFn(i64, i64) -> TypedRowStream<R>,
) -> BTreeMap<String, usize>
where
    R: for<'frame, 'metadata> scylla::deserialize::row::DeserializeRow<'frame, 'metadata>,
{
    // count how often each key is seen across all the ranges
    let mut counts = BTreeMap::new();
    // page through each range in turn, as each streamer worker reads its range
    for (start, end) in token_ranges(chunk_count) {
        let mut rows = read(start, end).await;
        // count each enumerated key under the prefix
        while let Some(row) = rows.next().await {
            let key = key(row.unwrap());
            if key.starts_with(prefix) {
                *counts.entry(key).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// Wait until a materialized view holds every row written to its base table
///
/// # Arguments
///
/// * `count` - The query counting the view's rows for the written keys
/// * `expected` - How many rows the view should hold
async fn settle(count: &str, expected: i64) {
    let session = test_utilities::scylla_session().await.unwrap();
    // views are written after their base table, so poll for a while
    for _ in 0..100 {
        let rows = session
            .query_unpaged(count, ())
            .await
            .unwrap()
            .into_rows_result()
            .unwrap();
        let (found,): (i64,) = rows.first_row().unwrap();
        if found >= expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the view never held {expected} rows: {count}");
}

/// The results enumeration over `results_ids` returns every result key exactly once across the
/// streamer's token ranges, however many ranges the ring is split into
#[tokio::test]
async fn result_keys_are_enumerated_once() -> Result<(), Error> {
    // the test API creates the keyspace when it starts
    test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let prefix = format!("enum-{}-", generators::gen_string(12));
    // write two results for each key, so the enumeration must collapse a key's rows
    let insert = session
        .prepare(format!(
            "INSERT INTO {ns}.results_stream (kind, group, year, bucket, uploaded, id, key, tool) \
             VALUES ('Files', 'enum', 2026, 1, ?, ?, ?, 'tool')"
        ))
        .await
        .unwrap();
    for index in 0..KEYS {
        for _ in 0..2 {
            let key = format!("{prefix}{index}");
            session
                .execute_unpaged(&insert, (Utc::now(), Uuid::new_v4(), key))
                .await
                .unwrap();
        }
    }
    let count = format!(
        "SELECT count(*) FROM {ns}.results_ids WHERE key IN ({}) ALLOW FILTERING",
        (0..KEYS)
            .map(|index| format!("'{prefix}{index}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    settle(&count, i64::try_from(KEYS * 2).unwrap()).await;
    // enumerate through the statement the streamer prepares
    let queries = search::SearchResultsIdsQueries::prepare(&session, ns)
        .await
        .unwrap();
    // every key comes back once whatever the split
    for chunk_count in [1, 7, 64] {
        let counts = enumerate(
            chunk_count,
            &prefix,
            |row: search::ResultEnumerateRow| row.key,
            async |start, end| queries.enumerate(&session, start, end).await.unwrap(),
        )
        .await;
        assert_eq!(counts.len(), KEYS, "{chunk_count} ranges");
        assert!(
            counts.values().all(|seen| *seen == 1),
            "{chunk_count} ranges: {counts:?}"
        );
    }
    Ok(())
}

/// The tags enumeration over `tags_by_item` returns every tagged item exactly once across the
/// streamer's token ranges, however many ranges the ring is split into
#[tokio::test]
async fn tagged_items_are_enumerated_once() -> Result<(), Error> {
    // the test API creates the keyspace when it starts
    test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let prefix = format!("enum-{}-", generators::gen_string(12));
    // write two tags for each item, so the enumeration must collapse an item's rows
    let insert = session
        .prepare(format!(
            "INSERT INTO {ns}.tags (type, group, year, bucket, key, value, uploaded, item, \
             key_lower, value_lower) VALUES ('Files', 'enum', 2026, 1, ?, 'v', ?, ?, ?, 'v')"
        ))
        .await
        .unwrap();
    for index in 0..KEYS {
        for tag in ["a", "b"] {
            let item = format!("{prefix}{index}");
            session
                .execute_unpaged(&insert, (tag, Utc::now(), item, tag))
                .await
                .unwrap();
        }
    }
    let count = format!(
        "SELECT count(*) FROM {ns}.tags_by_item WHERE type = 'Files' AND item IN ({}) \
         ALLOW FILTERING",
        (0..KEYS)
            .map(|index| format!("'{prefix}{index}'"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    settle(&count, i64::try_from(KEYS * 2).unwrap()).await;
    // enumerate through the statement the streamer prepares
    let queries = search::SearchTagsByItemQueries::prepare(&session, ns)
        .await
        .unwrap();
    // every item comes back once whatever the split
    for chunk_count in [1, 7, 64] {
        let counts = enumerate(
            chunk_count,
            &prefix,
            |row: search::TagEnumerateRow| row.item,
            async |start, end| queries.enumerate(&session, start, end).await.unwrap(),
        )
        .await;
        assert_eq!(counts.len(), KEYS, "{chunk_count} ranges");
        assert!(
            counts.values().all(|seen| *seen == 1),
            "{chunk_count} ranges: {counts:?}"
        );
    }
    Ok(())
}

/// The search streamer's reads of a chunk of keys return the rows written for them, through the
/// typed methods the streamer calls
#[tokio::test]
// writing the rows and checking each of the five reads keeps one story in one test
#[allow(clippy::too_many_lines)]
async fn streamer_reads_return_the_written_rows() -> Result<(), Error> {
    // the test API creates the keyspace when it starts
    test_utilities::admin_client().await?;
    let session = test_utilities::scylla_session().await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    let key = format!("read-{}", generators::gen_string(12));
    let id = Uuid::new_v4();
    // write a result, its stream row, and a tag for one key
    session
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.results (id, uploaded, result, files, children) \
                 VALUES (?, ?, 'body', {{'a.txt'}}, {{'child': ?}})"
            ),
            (id, Utc::now(), id),
        )
        .await
        .unwrap();
    session
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.results_stream (kind, group, year, bucket, uploaded, id, key, \
                 tool) VALUES ('Files', 'reads', 2026, 1, ?, ?, ?, 'tool')"
            ),
            (Utc::now(), id, key.as_str()),
        )
        .await
        .unwrap();
    session
        .query_unpaged(
            format!(
                "INSERT INTO {ns}.tags (type, group, year, bucket, key, value, uploaded, item, \
                 key_lower, value_lower) VALUES ('Files', 'reads', 2026, 1, 'Hue', 'Red', ?, ?, \
                 'hue', 'red')"
            ),
            (Utc::now(), key.as_str()),
        )
        .await
        .unwrap();
    settle(
        &format!("SELECT count(*) FROM {ns}.results_auth WHERE key = '{key}' ALLOW FILTERING"),
        1,
    )
    .await;
    settle(
        &format!(
            "SELECT count(*) FROM {ns}.tags_by_item WHERE type = 'Files' AND item = '{key}' \
             ALLOW FILTERING"
        ),
        1,
    )
    .await;
    // prepare the streamer's statements
    let ids = search::SearchResultsIdsQueries::prepare(&session, ns)
        .await
        .unwrap();
    let results = search::SearchResultsQueries::prepare(&session, ns)
        .await
        .unwrap();
    let auth = search::SearchResultsAuthQueries::prepare(&session, ns)
        .await
        .unwrap();
    let tags = search::SearchTagsByItemQueries::prepare(&session, ns)
        .await
        .unwrap();
    let groups = vec!["reads".to_owned()];
    // each read returns the rows written for the key
    let init = ids
        .init_data_chunk(&session, std::slice::from_ref(&key))
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;
    assert_eq!(init.len(), 1);
    assert_eq!((init[0].key.as_str(), init[0].id), (key.as_str(), id));
    assert_eq!(init[0].group, "reads");
    let bodies = results
        .results_chunk(&session, &[id])
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0].result, "body");
    assert_eq!(bodies[0].files, vec!["a.txt".to_owned()]);
    assert_eq!(bodies[0].children.get("child"), Some(&id));
    let event = auth
        .event_data_chunk(&session, &key, &thorium::models::OutputKind::Files, &groups)
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;
    assert_eq!(event.len(), 1);
    assert_eq!((event[0].group.as_str(), event[0].id), ("reads", id));
    let tagged = tags
        .init_chunk(
            &session,
            &thorium::models::TagType::Files,
            std::slice::from_ref(&key),
        )
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;
    assert_eq!(tagged.len(), 1);
    assert_eq!(
        (tagged[0].key.as_str(), tagged[0].value.as_str()),
        ("Hue", "Red")
    );
    let tag_event = tags
        .event_chunk(&session, &thorium::models::TagType::Files, &key, &groups)
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .await;
    assert_eq!(tag_event.len(), 1);
    assert_eq!(tag_event[0].group, "reads");
    Ok(())
}
