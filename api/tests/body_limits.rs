//! Tests that the API holds request bodies to their route's limit and that clients read the 413

use bytes::Bytes;
use reqwest::multipart::Form;
use thorium::models::{
    Buffer, CommentRequest, EntityMetadataRequest, EntityRequest, GroupRequest,
    ReactionCacheFileUpdate, SampleRequest, TarredRepo,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium};

/// The default body limit
const DEFAULT: usize = 16 << 20;

/// The entities body limit
const ENTITIES: usize = 14 << 20;

/// The setting for the default body limit
const DEFAULT_SETTING: &str = "thorium.limits.body.default";

/// The setting for the entities body limit
const ENTITIES_SETTING: &str = "thorium.limits.body.entities";

/// Get the token of a client
///
/// # Arguments
///
/// * `client` - The client whose token to get
async fn token_of(client: &Thorium) -> Result<String, Error> {
    // read the token from the client's user
    Ok(client.users.info().await?.token)
}

/// A JSON body of exactly this many bytes that isn't any request the API takes
///
/// # Arguments
///
/// * `len` - The length of the body
fn padded_json(len: usize) -> String {
    // pad one field to fill the body, leaving room for the braces, key, and quotes
    let pad = len - r#"{"pad":""}"#.len();
    format!(r#"{{"pad":"{}"}}"#, "x".repeat(pad))
}

/// Send a raw JSON body to a route and get the status and body of the response
///
/// # Arguments
///
/// * `client` - The client whose credentials to send
/// * `path` - The path to send to, starting with `/api/`
/// * `body` - The body to send
async fn send_json(client: &Thorium, path: &str, body: String) -> Result<(u16, String), Error> {
    // send the body with its length declared
    let resp = reqwest::Client::new()
        .post(format!("{}{path}", client.host))
        .header(
            "authorization",
            test_utilities::auth_header(&token_of(client).await?),
        )
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;
    let status = resp.status().as_u16();
    Ok((status, resp.text().await?))
}

/// Check that a response is a JSON 413 naming a limit and its setting
///
/// # Arguments
///
/// * `status` - The response's status
/// * `body` - The response's body
/// * `limit` - The limit it should name
/// * `setting` - The setting it should name
fn assert_too_large(status: u16, body: &str, limit: usize, setting: &str) {
    assert_eq!(status, 413, "{body}");
    let body: serde_json::Value = serde_json::from_str(body).expect("a JSON 413 body");
    assert_eq!(body["limit"], limit, "{body}");
    assert_eq!(body["setting"], setting, "{body}");
    let error = body["error"].as_str().expect("an error message");
    assert!(error.contains(setting), "{error}");
}

/// Check that a client error is a 413 whose message names a setting
///
/// # Arguments
///
/// * `result` - The refused request
/// * `setting` - The setting the message should name
fn assert_client_too_large<T: std::fmt::Debug>(result: Result<T, Error>, setting: &str) {
    // the client reports the status and the body's message as thorctl shows them
    let err = result.expect_err("the request should be refused");
    assert_eq!(err.status().map(|code| code.as_u16()), Some(413), "{err:?}");
    let msg = err.msg().unwrap_or_default();
    assert!(msg.contains(setting), "{msg}");
}

/// A JSON route holds bodies to the default limit: just under it is parsed and just over it
/// is a JSON 413 naming the setting, also through the client
#[tokio::test]
async fn default_limit() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // a body just under the limit is read and refused as the wrong shape
    let (status, body) =
        send_json(&client, "/api/groups/", padded_json(DEFAULT - (64 << 10))).await?;
    assert_eq!(status, 422, "{body}");
    // one byte over the limit is a 413 naming the setting
    let (status, body) = send_json(&client, "/api/groups/", padded_json(DEFAULT + 1)).await?;
    assert_too_large(status, &body, DEFAULT, DEFAULT_SETTING);
    // the client reports the 413 and its message
    let name = format!("limits-{}", generators::gen_string(8)).to_lowercase();
    let req = GroupRequest::new(name).description("x".repeat(DEFAULT));
    assert_client_too_large(client.groups.create(&req).await, DEFAULT_SETTING);
    Ok(())
}

/// A client still sending a body far over its limit reads the 413 instead of a reset, whether
/// it declares the length or streams the body in chunks
#[tokio::test]
async fn oversized_bodies_reach_clients() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let token = token_of(&client).await?;
    // a declared body more than three times the limit is answered with a 413
    let (status, body) = send_json(&client, "/api/groups/", padded_json(3 * DEFAULT)).await?;
    assert_too_large(status, &body, DEFAULT, DEFAULT_SETTING);
    // so is a body of twice the limit streamed in chunks with no declared length
    let chunks = (0..32).map(|_| Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'x'; 1 << 20])));
    let resp = reqwest::Client::new()
        .post(format!("{}/api/groups/", client.host))
        .header("authorization", test_utilities::auth_header(&token))
        .header("content-type", "application/json")
        .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
        .send()
        .await?;
    let status = resp.status().as_u16();
    assert_too_large(status, &resp.text().await?, DEFAULT, DEFAULT_SETTING);
    Ok(())
}

/// Entity creates and updates are held to the entities limit, also through the client with a
/// form far over it
#[tokio::test]
async fn entities_limit() -> Result<(), Error> {
    // get admin client and a group to create entities in
    let client = test_utilities::admin_client().await?;
    let token = token_of(&client).await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    // send a raw entity form with a padded description
    let send_form = |method: reqwest::Method, path: String, kind: &'static str, pad: usize| {
        let form = Form::new()
            .text("name", "padded")
            .text("kind", kind)
            .text("groups[]", group.clone())
            .text("description", "x".repeat(pad));
        reqwest::Client::new()
            .request(method, format!("{}{path}", client.host))
            .header("authorization", test_utilities::auth_header(&token))
            .multipart(form)
            .send()
    };
    // a create just under the limit is read and refused for its unknown kind
    let resp = send_form(
        reqwest::Method::POST,
        "/api/entities/".into(),
        "NotAKind",
        ENTITIES - (64 << 10),
    )
    .await?;
    assert_eq!(resp.status().as_u16(), 400);
    // just over the limit is a 413 naming the setting
    let resp = send_form(
        reqwest::Method::POST,
        "/api/entities/".into(),
        "Other",
        ENTITIES + 1,
    )
    .await?;
    let status = resp.status().as_u16();
    assert_too_large(status, &resp.text().await?, ENTITIES, ENTITIES_SETTING);
    // an update is held to the same limit
    let small = EntityRequest::new("small", EntityMetadataRequest::Other, vec![group.clone()]);
    let id = client.entities.create(small).await?.id;
    let path = format!("/api/entities/{id}");
    let resp = send_form(reqwest::Method::PATCH, path.clone(), "Other", ENTITIES + 1).await?;
    let status = resp.status().as_u16();
    assert_too_large(status, &resp.text().await?, ENTITIES, ENTITIES_SETTING);
    // the client reports the 413 for a form just over the limit and one five times over it
    for pad in [ENTITIES + 1, 5 * ENTITIES] {
        let mut req =
            EntityRequest::new("padded", EntityMetadataRequest::Other, vec![group.clone()]);
        req.description = Some("x".repeat(pad));
        assert_client_too_large(client.entities.create(req).await, ENTITIES_SETTING);
    }
    // the small entity is unchanged
    assert_eq!(client.entities.get(id).await?.description, None);
    Ok(())
}

/// Uploads, including serialized YARA ruleset caches, and restores have no body limit
#[tokio::test]
async fn uploads_have_no_limit() -> Result<(), Error> {
    // get admin client and a group to upload into
    let client = test_utilities::admin_client().await?;
    let group = generators::groups(1, &client).await?.remove(0).name;
    let large = || {
        let mut data = vec![0u8; DEFAULT + (1 << 20)];
        rand::fill(&mut data[..]);
        data
    };
    // a file larger than the default limit uploads
    let file = SampleRequest::new_buffer(Buffer::new(large()), vec![group.clone()]);
    let sha256 = client.files.create(file).await?.sha256;
    // so does a comment attachment
    let comment = CommentRequest::new(&sha256, "a large attachment")
        .group(&group)
        .buffer(Buffer::new(large()).name("large.bin"));
    client.files.comment(comment).await?;
    // and a repo's data
    let repo = generators::repos(&group, 1, &client).await?.remove(0).url;
    let path =
        std::env::temp_dir().join(format!("thorium-large-repo-{}", generators::gen_string(16)));
    tokio::fs::write(&path, large()).await?;
    let tar = TarredRepo {
        name: "data".to_owned(),
        repo: path.clone(),
    };
    let uploaded = client.repos.upload(&repo, tar, vec![group.clone()]).await;
    tokio::fs::remove_file(&path).await?;
    uploaded?;
    // and a reaction's cache files
    let (_, created) = generators::reactions(&group, 1, None, &client).await?;
    let update = ReactionCacheFileUpdate::default().buffer(Buffer::new(large()).name("cache.bin"));
    client
        .reactions
        .update_cache_files(&group, created.created[0], update)
        .await?;
    // and a serialized YARA ruleset, under an engine version of its own so no other ruleset is
    // pruned as its sibling
    let engine = format!("test-{}", generators::gen_string(16));
    let hash = generators::gen_string(32);
    let ruleset = large();
    client
        .events
        .upload_yara_ruleset(&engine, &hash, ruleset.clone())
        .await?;
    let cached = client.events.get_yara_ruleset(&engine, &hash).await?;
    assert_eq!(cached.as_deref(), Some(&ruleset[..]));
    // a restore larger than the default limit is read and refused as invalid, not as too large
    let (status, body) = send_json(
        &client,
        "/api/system/restore",
        padded_json(DEFAULT + (1 << 20)),
    )
    .await?;
    assert_eq!(status, 422, "{body}");
    Ok(())
}

/// The MCP server holds the bodies it streams to the default limit
#[tokio::test]
async fn mcp_limit() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    let token = token_of(&client).await?;
    // a JSON-RPC request padded over the limit is a 413
    let padding = "x".repeat(DEFAULT);
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {"padding": padding}
    });
    let resp = reqwest::Client::new()
        .post(format!("{}/api/mcp", client.host))
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .body(body.to_string())
        .send()
        .await?;
    let status = resp.status().as_u16();
    assert_too_large(status, &resp.text().await?, DEFAULT, DEFAULT_SETTING);
    Ok(())
}
