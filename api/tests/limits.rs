//! Tests that every listing route bounds the number of rows a request may ask for

use base64::Engine;
use thorium::models::{EntityListOpts, RepoRequest};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};

/// The most rows a listing route returns in one page
const LIMIT_CAP: usize = 10_000;

/// The most lines a stage-log request returns at once
const STAGE_LOG_CAP: usize = 250_000;

/// Send a GET to the API with this client's credentials and return the status code
///
/// # Arguments
///
/// * `client` - The client whose credentials to send
/// * `path` - The path and query to request, starting with `/api/`
async fn status_of(client: &Thorium, path: &str) -> Result<u16, Error> {
    // get the token for this client
    let token = client.users.info().await?.token;
    // build the auth header the client would send
    let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
    // send our raw request so the limit reaches the API unchanged
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", client.host))
        .header("authorization", format!("token {encoded}"))
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Check that a listing route rejects a limit of 0 or above its cap and accepts 1 and its cap
///
/// # Arguments
///
/// * `client` - The client to send requests with
/// * `path` - The route's path and query, ending where the limit parameter goes
/// * `param` - The name of the limit parameter
/// * `cap` - The largest limit the route allows
async fn check_bounds(client: &Thorium, path: &str, param: &str, cap: usize) -> Result<(), Error> {
    // pick the separator between the existing query and our limit
    let sep = if path.contains('?') { '&' } else { '?' };
    // each limit we send with the status code we expect back
    let cases = [(0, 400), (cap + 1, 400), (1, 200), (cap, 200)];
    // send each limit and check its status
    for (limit, expected) in cases {
        let status = status_of(client, &format!("{path}{sep}{param}={limit}")).await?;
        assert_eq!(
            status, expected,
            "{path} with {param}={limit} returned {status}, expected {expected}"
        );
    }
    Ok(())
}

/// Entity listings refuse a limit of 0, above the cap, or past 32 bits with a 400 and accept 1
/// and the cap
#[tokio::test]
async fn entities_limit_bounds() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a group to list in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // untagged entity listings are bounded
    check_bounds(
        &client,
        &format!("/api/entities/?groups[]={group}"),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    // tagged entity listings are bounded
    check_bounds(
        &client,
        &format!("/api/entities/?groups[]={group}&tags[k][]=v"),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    // entity details listings are bounded
    check_bounds(
        &client,
        &format!("/api/entities/details/?groups[]={group}"),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    // a limit that doesn't fit in a 32 bit signed integer is rejected
    let status = status_of(
        &client,
        &format!("/api/entities/?groups[]={group}&limit=2147483647"),
    )
    .await?;
    assert_eq!(status, 400, "an i32::MAX entity limit returned {status}");
    // a limit past 32 bits is rejected rather than truncated
    let status = status_of(
        &client,
        &format!("/api/entities/?groups[]={group}&limit=4294967297"),
    )
    .await?;
    assert_eq!(status, 400, "a 2^32+1 entity limit returned {status}");
    Ok(())
}

/// File listings refuse a limit of 0 or above the cap with a 400 and accept 1 and the cap
#[tokio::test]
async fn files_limit_bounds() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a group to list in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // file listings are bounded
    check_bounds(
        &client,
        &format!("/api/files/?groups[]={group}"),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    // file details listings are bounded
    check_bounds(
        &client,
        &format!("/api/files/details/?groups[]={group}"),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    // tagged file listings are bounded
    check_bounds(
        &client,
        &format!("/api/files/?groups[]={group}&tags[k][]=v"),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    Ok(())
}

/// Association listings refuse a limit of 0 or above the cap with a 400 and accept 1 and the cap
#[tokio::test]
async fn associations_limit_bounds() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a group and a file to list associations for
    let group = generators::groups(1, &client).await?.remove(0).name;
    let sample = generators::gen_sample(&group);
    let sha256 = client.files.create(sample).await?.sha256;
    // association listings are bounded
    check_bounds(
        &client,
        &format!("/api/files/associations/{sha256}"),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    Ok(())
}

/// Repo and commitish listings refuse a limit of 0 or above the cap with a 400 and accept 1 and
/// the cap
#[tokio::test]
async fn repos_limit_bounds() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a group with a repo in it
    let group = generators::groups(1, &client).await?.remove(0).name;
    let repo: RepoRequest = generators::repos(&group, 1, &client).await?.remove(0);
    // repo listings are bounded
    check_bounds(
        &client,
        &format!("/api/repos/?groups[]={group}"),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    // repo details listings are bounded
    check_bounds(
        &client,
        &format!("/api/repos/details/?groups[]={group}"),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    // commitish listings are bounded
    check_bounds(
        &client,
        &format!("/api/repos/commitishes/{}", repo.url),
        "limit",
        LIMIT_CAP,
    )
    .await?;
    Ok(())
}

/// Network policy listings refuse a limit of 0, above the cap, or one whose next value overflows
/// with a 400 and accept 1 and the cap
#[tokio::test]
async fn network_policies_limit_bounds() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // network policy listings are bounded
    check_bounds(&client, "/api/network-policies/", "limit", LIMIT_CAP).await?;
    // network policy details listings are bounded
    check_bounds(
        &client,
        "/api/network-policies/details/",
        "limit",
        LIMIT_CAP,
    )
    .await?;
    // a limit whose next value overflows a 32 bit signed integer is rejected
    let status = status_of(&client, "/api/network-policies/?limit=2147483647").await?;
    assert_eq!(
        status, 400,
        "an i32::MAX network policy limit returned {status}"
    );
    Ok(())
}

/// Node listings refuse a page size of 0 or above the cap with a 400 and accept 1 and the cap
#[tokio::test]
async fn nodes_page_size_bounds() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // node listings bound their page size
    check_bounds(
        &client,
        "/api/system/nodes/?clusters[]=m0-cluster",
        "page_size",
        LIMIT_CAP,
    )
    .await?;
    // node details listings bound their page size
    check_bounds(
        &client,
        "/api/system/nodes/details/?clusters[]=m0-cluster",
        "page_size",
        LIMIT_CAP,
    )
    .await?;
    Ok(())
}

/// Stage-log reads refuse a limit of 0 or above their own cap with a 400 and accept 1 and the cap
#[tokio::test]
async fn stage_logs_limit_bounds() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a reaction to read stage logs from
    let group = generators::groups(1, &client).await?.remove(0).name;
    let (reqs, resp) = generators::reactions(&group, 1, None, &client).await?;
    let pipeline = client.pipelines.get(&group, &reqs[0].pipeline).await?;
    let stage = &pipeline.order[0][0];
    let reaction = resp.created[0];
    // stage-log reads are bounded by their own cap
    check_bounds(
        &client,
        &format!("/api/reactions/logs/{group}/{reaction}/{stage}"),
        "limit",
        STAGE_LOG_CAP,
    )
    .await?;
    Ok(())
}

/// The Rust client pages a listing to its end, and reports a page size above the cap as a 400
/// that says what was wrong
#[tokio::test]
async fn client_pages_to_the_end_and_reports_oversized_limits() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // build a group with 7 entities in it
    let group = generators::groups(1, &client).await?.remove(0).name;
    let mut expected = std::collections::HashSet::with_capacity(7);
    for _ in 0..7 {
        let req = generators::gen_entity(&group, generators::gen_other_meta());
        expected.insert(client.entities.create(req).await?.id);
    }
    // the client pages the listing 3 at a time until its cursor is exhausted
    let opts = EntityListOpts::default()
        .groups(vec![group.clone()])
        .page_size(3);
    let mut cursor = client.entities.list_details(&opts).await?;
    let mut listed: Vec<_> = cursor.data.drain(..).map(|entity| entity.id).collect();
    while !cursor.exhausted() {
        cursor.refill().await?;
        listed.extend(cursor.data.drain(..).map(|entity| entity.id));
    }
    assert_eq!(listed.len(), 7, "the client listed {listed:?}");
    assert_eq!(
        listed.into_iter().collect::<std::collections::HashSet<_>>(),
        expected
    );
    // a page size above the cap reaches the caller as a 400 that says what was wrong
    let opts = EntityListOpts::default()
        .groups(vec![group])
        .page_size(20_000);
    let Err(error) = client.entities.list_details(&opts).await else {
        panic!("a page size of 20,000 was accepted");
    };
    let message = error.msg().unwrap_or_default();
    assert_eq!(error.status().map(|status| status.as_u16()), Some(400));
    assert!(
        message.contains("limit must be between 1 and 10000, got 20000"),
        "unclear error message: {message}"
    );
    Ok(())
}
