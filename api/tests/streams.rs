//! Tests the stream routes in Thorium

use serde_json::Value;

use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};

/// Send a GET to the API with this client's credentials and return its status and JSON body
///
/// # Arguments
///
/// * `client` - The client whose credentials to send
/// * `path` - The path and query to request, starting with `/api/`
async fn get_json(client: &Thorium, path: &str) -> Result<(u16, Value), Error> {
    // get the token for this client
    let token = client.users.info().await?.token;
    // send our raw request so the path values reach the API unchanged
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", client.host))
        .header("authorization", test_utilities::auth_header(&token))
        .send()
        .await?;
    // get the status and body of the response
    let status = resp.status().as_u16();
    let body = resp.text().await?;
    let json = serde_json::from_str(&body).unwrap_or(Value::String(body));
    Ok((status, json))
}

/// Add members to a stream at the given timestamps
///
/// # Arguments
///
/// * `group` - The group the stream is in
/// * `scores` - The timestamps to add members at
async fn seed_stream(group: &str, scores: impl Iterator<Item = i64>) -> Result<(), Error> {
    // build the key the API stores this stream at
    let ns = &test_utilities::CONF.thorium.namespace;
    let key = format!("{ns}:streams:{group}:corn:harvest");
    // add one member per timestamp
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    for score in scores {
        let (): () = bb8_redis::redis::cmd("ZADD")
            .arg(&key)
            .arg(score)
            .arg(format!("member{score}"))
            .query_async(&mut *conn)
            .await
            .map_err(|err| Error::new(err.to_string()))?;
    }
    Ok(())
}

/// Depth requests whose span or split don't fit are refused instead of aborting the API, and
/// normal depth requests keep working
#[tokio::test]
async fn depth_refuses_spans_that_overflow() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group with a stream of 50 members
    let group = generators::groups(1, &client).await?.remove(0).name;
    seed_stream(&group, 1..=50).await?;
    let base = format!("/api/streams/depth/{group}/corn/harvest");
    // a span wider than an i64, a split larger than an i64, and too many probes are refused
    for path in [
        format!("{base}/-4611686018427387904/4611686018427387905/1000"),
        format!("{base}/-9223372036854775808/9223372036854775807/1"),
        format!("{base}/0/100/18446744073709551615"),
        format!("{base}/0/1000000000/1"),
    ] {
        let (status, body) = get_json(&client, &path).await?;
        assert_eq!(status, 400, "{path} returned {status}: {body}");
    }
    // the API keeps serving and still counts the stream correctly
    let (status, body) = get_json(&client, &format!("{base}/0/100/1000")).await?;
    assert_eq!(
        status, 200,
        "a normal depth request returned {status}: {body}"
    );
    let depths = body.as_array().cloned().unwrap_or_default();
    assert_eq!(
        depths.len(),
        1,
        "a split wider than the span probes once: {body}"
    );
    assert_eq!(
        depths[0]["depth"], 50,
        "the stream holds 50 members: {body}"
    );
    // smaller splits cover the same members in order without gaps or overlaps
    let (status, body) = get_json(&client, &format!("{base}/1/50/20")).await?;
    assert_eq!(
        status, 200,
        "a split depth request returned {status}: {body}"
    );
    let counts = body.as_array().map_or_else(Vec::new, |depths| {
        depths.iter().map(|depth| depth["depth"].clone()).collect()
    });
    assert_eq!(
        counts,
        vec![20, 20, 10],
        "splits of 20 over 50 members: {body}"
    );
    // mapping the stream still works
    let (status, body) =
        get_json(&client, &format!("/api/streams/map/{group}/corn/harvest")).await?;
    assert_eq!(status, 200, "mapping the stream returned {status}: {body}");
    Ok(())
}

/// Depth requests with timestamps no date can hold are refused on both depth routes
#[tokio::test]
async fn depth_refuses_timestamps_out_of_range() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group with a stream of 50 members
    let group = generators::groups(1, &client).await?.remove(0).name;
    seed_stream(&group, 1..=50).await?;
    let base = format!("/api/streams/depth/{group}/corn/harvest");
    // starts and ends beyond the range of dates are refused with or without a split
    for path in [
        format!("{base}/9000000000000/9000000000010"),
        format!("{base}/9000000000000/9000000000010/1"),
        format!("{base}/-9000000000010/-9000000000000"),
        format!("{base}/0/9000000000000"),
        format!("{base}/0/9000000000000/1000000000000"),
    ] {
        let (status, body) = get_json(&client, &path).await?;
        assert_eq!(status, 400, "{path} returned {status}: {body}");
    }
    Ok(())
}
