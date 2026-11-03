//! Tests for the request body limits

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Multipart;
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytesize::ByteSize;
use futures::StreamExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

use super::{BodyLimit, DRAIN_ALLOWANCE, enforce, fixed, limit, unlimited};
use crate::utils::ApiError;

/// The default limit the test app holds bodies to
const DEFAULT: u64 = 1024;

/// The setting named for the default limit
const DEFAULT_SETTING: &str = "thorium.limits.body.default";

/// Answer with the length of a JSON body
///
/// # Arguments
///
/// * `body` - The JSON body
async fn json_len(axum::Json(body): axum::Json<serde_json::Value>) -> String {
    body.to_string().len().to_string()
}

/// Answer with the length of a raw body read the way a streaming service reads it
///
/// # Arguments
///
/// * `body` - The raw body
async fn raw_len(body: Body) -> Result<String, StatusCode> {
    // a streaming service fails its own way when its body fails
    match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => Ok(bytes.len().to_string()),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Answer with the total length of every field of a multipart form
///
/// # Arguments
///
/// * `form` - The multipart form
async fn form_len(mut form: Multipart) -> Result<String, ApiError> {
    // read every field the way the entity and upload routes do
    let mut total = 0;
    while let Some(field) = form.next_field().await? {
        total += field.bytes().await?.len();
    }
    Ok(total.to_string())
}

/// Build the test app with the default limit applied the way the API applies it
fn app() -> Router {
    Router::new()
        .route("/json", post(json_len))
        .route("/raw", post(raw_len))
        .route("/form", post(form_len))
        .route(
            "/entities",
            limit(
                post(json_len),
                ByteSize::b(4 * DEFAULT),
                "thorium.limits.body.entities",
            ),
        )
        .route(
            "/small",
            limit(post(json_len), ByteSize::b(DEFAULT / 2), "small"),
        )
        .route("/icon", fixed(post(raw_len), 2 * DEFAULT))
        .route("/upload", unlimited(post(raw_len)))
        .layer(axum::middleware::from_fn_with_state(
            BodyLimit::configured(ByteSize::b(DEFAULT), DEFAULT_SETTING),
            enforce,
        ))
}

/// A JSON body of exactly this many bytes
///
/// # Arguments
///
/// * `len` - The length of the body
fn json_of(len: u64) -> Vec<u8> {
    // pad a string to fill the body, leaving room for the quotes
    let len = usize::try_from(len).unwrap();
    format!("\"{}\"", "x".repeat(len - 2)).into_bytes()
}

/// Send a body to a path with its length declared
///
/// # Arguments
///
/// * `path` - The path to send to
/// * `body` - The body to send
async fn send(path: &str, body: Vec<u8>) -> Response {
    // declare the length the way clients do
    let req = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap();
    app().oneshot(req).await.unwrap()
}

/// A body sent in chunks with no declared length that counts how much of it was read
///
/// # Arguments
///
/// * `chunks` - How many chunks to send
/// * `chunk` - The size of each chunk
/// * `read` - The count of bytes read so far
fn counted_stream(chunks: u64, chunk: usize, read: &Arc<AtomicU64>) -> Body {
    // each chunk adds its length to the count as it is read
    let read = read.clone();
    let stream = futures::stream::iter(0..chunks).map(move |_| {
        read.fetch_add(chunk as u64, Ordering::SeqCst);
        Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'x'; chunk]))
    });
    Body::from_stream(stream)
}

/// Read a response's JSON body
///
/// # Arguments
///
/// * `response` - The response to read
async fn json_body(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Check that a response is a 413 naming a limit and its setting
///
/// # Arguments
///
/// * `response` - The response to check
/// * `max` - The limit it should name
/// * `setting` - The setting it should name
async fn assert_too_large(response: Response, max: u64, setting: Option<&str>) {
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    let body = json_body(response).await;
    assert_eq!(body["limit"], max, "{body}");
    let error = body["error"].as_str().unwrap();
    assert!(error.contains(&format!("({max} bytes)")), "{error}");
    match setting {
        Some(setting) => {
            assert_eq!(body["setting"], setting, "{body}");
            assert!(error.contains(setting), "{error}");
        }
        None => assert!(body.get("setting").is_none(), "{body}"),
    }
}

/// A body at the default limit is read and one byte over it is a 413 naming the default setting
#[tokio::test]
async fn default_limit_edges() {
    // a body of exactly the limit reaches the handler
    let response = send("/json", json_of(DEFAULT)).await;
    assert_eq!(response.status(), StatusCode::OK);
    // one byte more is refused with a JSON error naming the setting
    let response = send("/json", json_of(DEFAULT + 1)).await;
    assert_too_large(response, DEFAULT, Some(DEFAULT_SETTING)).await;
}

/// A route's own limit replaces the default whether it is larger or smaller
#[tokio::test]
async fn route_limits_replace_default() {
    // a larger route limit accepts a body over the default
    let response = send("/entities", json_of(3 * DEFAULT)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = send("/entities", json_of(4 * DEFAULT + 1)).await;
    assert_too_large(response, 4 * DEFAULT, Some("thorium.limits.body.entities")).await;
    // a smaller route limit refuses a body under the default
    let response = send("/small", json_of(DEFAULT / 2 + 1)).await;
    assert_too_large(response, DEFAULT / 2, Some("small")).await;
    // a fixed limit names no setting
    let response = send(
        "/icon",
        vec![b'x'; usize::try_from(2 * DEFAULT + 1).unwrap()],
    )
    .await;
    assert_too_large(response, 2 * DEFAULT, None).await;
}

/// An unlimited route reads a body far over the default
#[tokio::test]
async fn unlimited_route_reads_everything() {
    // a body a hundred times the default is read whole
    let len = 100 * DEFAULT;
    let response = send("/upload", vec![b'x'; usize::try_from(len).unwrap()]).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes, len.to_string());
}

/// Raw and multipart bodies over the limit are 413s, not their handlers' own errors
#[tokio::test]
async fn raw_and_multipart_bodies_are_413() {
    // a raw body read by a streaming handler
    let response = send("/raw", vec![b'x'; usize::try_from(DEFAULT + 1).unwrap()]).await;
    assert_too_large(response, DEFAULT, Some(DEFAULT_SETTING)).await;
    // a multipart form whose field goes over the limit
    let boundary = "jupiter";
    let mut form = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"data\"\r\n\r\n")
        .into_bytes();
    form.extend(vec![b'x'; usize::try_from(2 * DEFAULT).unwrap()]);
    form.extend(format!("\r\n--{boundary}--\r\n").into_bytes());
    let req = Request::post("/form")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header(header::CONTENT_LENGTH, form.len())
        .body(Body::from(form))
        .unwrap();
    let response = app().oneshot(req).await.unwrap();
    assert_too_large(response, DEFAULT, Some(DEFAULT_SETTING)).await;
}

/// A body with no declared length is read to its end before the 413, so the client has finished
/// sending when the 413 is written
#[tokio::test]
async fn oversized_chunked_body_is_drained() {
    // send ten times the limit in chunks with no declared length
    let read = Arc::new(AtomicU64::new(0));
    let req = Request::post("/raw")
        .body(counted_stream(10, usize::try_from(DEFAULT).unwrap(), &read))
        .unwrap();
    let response = app().oneshot(req).await.unwrap();
    assert_too_large(response, DEFAULT, Some(DEFAULT_SETTING)).await;
    // every chunk was read and thrown away
    assert_eq!(read.load(Ordering::SeqCst), 10 * DEFAULT);
}

/// A body that declares more than the drain allowance past its limit isn't read at all
#[tokio::test]
async fn far_oversized_body_is_not_read() {
    // declare a length past the allowance but send only a little
    let read = Arc::new(AtomicU64::new(0));
    let req = Request::post("/raw")
        .header(header::CONTENT_LENGTH, DEFAULT + DRAIN_ALLOWANCE + 1)
        .body(counted_stream(4, 16, &read))
        .unwrap();
    let response = app().oneshot(req).await.unwrap();
    assert_too_large(response, DEFAULT, Some(DEFAULT_SETTING)).await;
    // nothing was read
    assert_eq!(read.load(Ordering::SeqCst), 0);
    // a declared length within the allowance is read to its end and thrown away
    let read = Arc::new(AtomicU64::new(0));
    let req = Request::post("/raw")
        .header(header::CONTENT_LENGTH, 64)
        .body(counted_stream(4, 16, &read))
        .unwrap();
    let small =
        Router::new()
            .route("/raw", post(raw_len))
            .layer(axum::middleware::from_fn_with_state(
                BodyLimit::configured(ByteSize::b(16), "tiny"),
                enforce,
            ));
    let response = small.oneshot(req).await.unwrap();
    assert_too_large(response, 16, Some("tiny")).await;
    assert_eq!(read.load(Ordering::SeqCst), 64);
}

/// A multipart form refused by axum's own size limit is a 413 rather than a 400
#[tokio::test]
async fn multipart_size_error_is_413() {
    // a route held only to axum's own limit
    let app = Router::new()
        .route("/form", post(form_len))
        .layer(axum::extract::DefaultBodyLimit::max(64));
    let boundary = "io";
    let mut form = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"data\"\r\n\r\n")
        .into_bytes();
    form.extend(vec![b'x'; 256]);
    form.extend(format!("\r\n--{boundary}--\r\n").into_bytes());
    let req = Request::post("/form")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(form))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}
