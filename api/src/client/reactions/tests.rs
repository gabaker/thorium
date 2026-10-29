//! Tests for the urls the reaction listing client methods request, against a one-request mock API

use std::future::Future;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::Thorium;
use crate::models::{ReactionCursorOpts, ReactionStatus};

/// The name of a run pseudo-pipeline, whose `:` and `#` must not reach the url unencoded
const RUN_PIPELINE: &str = "workflow:wf#r1";

/// The encoded form of [`RUN_PIPELINE`] as one url path segment
const RUN_PIPELINE_ENCODED: &str = "workflow%3Awf%23r1";

/// Start a mock API that records the request line of its first request and answers an empty cursor
///
/// Returns the url the mock listens at and a handle that resolves to the request line it received.
async fn one_request_mock() -> (String, tokio::task::JoinHandle<String>) {
    // bind to a random local port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    // read the head of one request in the background and answer it
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut text = Vec::new();
        let mut chunk = [0_u8; 4096];
        // read until the whole head of the request has arrived
        while !String::from_utf8_lossy(&text).contains("\r\n\r\n") {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "the client closed before it sent a request");
            text.extend_from_slice(&chunk[..read]);
        }
        // answer with a cursor that holds no data and close
        let body = r#"{"cursor":null,"names":[],"data":[]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(resp.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
        // keep only the request line
        String::from_utf8_lossy(&text)
            .lines()
            .next()
            .unwrap()
            .to_owned()
    });
    (host, handle)
}

/// Run a client call against the mock API and return the path it requested
///
/// # Arguments
///
/// * `call` - The client call to run with a client that targets the mock API
async fn requested_path<F, Fut>(call: F) -> String
where
    F: FnOnce(Thorium) -> Fut,
    Fut: Future<Output = ()>,
{
    // start the mock and build a client that targets it
    let (host, received) = one_request_mock().await;
    let thorium = Thorium::build(&host).token("t").build().await.unwrap();
    // run the call and read the request line the mock saw
    call(thorium).await;
    let line = received.await.unwrap();
    // the request line is `GET <path>[?<query>] HTTP/1.1`; return the path alone
    let target = line.split(' ').nth(1).unwrap().to_owned();
    target.split('?').next().unwrap().to_owned()
}

/// Listing a pipeline's reactions encodes the pipeline name in the path
#[tokio::test]
async fn list_encodes_the_pipeline() {
    // list the run pseudo-pipeline's reactions against the mock API
    let path = requested_path(|thorium| async move {
        thorium
            .reactions
            .list("corn", RUN_PIPELINE)
            .exec()
            .await
            .unwrap();
    })
    .await;
    // the pipeline name should reach the path as one encoded segment
    assert_eq!(
        path,
        format!("/api/reactions/list/corn/{RUN_PIPELINE_ENCODED}/")
    );
}

/// Listing a pipeline's reactions by status encodes the pipeline name and keeps the status
#[tokio::test]
async fn list_status_encodes_the_pipeline() {
    // list the run pseudo-pipeline's created reactions against the mock API
    let path = requested_path(|thorium| async move {
        thorium
            .reactions
            .list_status("corn", RUN_PIPELINE, &ReactionStatus::Created)
            .exec()
            .await
            .unwrap();
    })
    .await;
    // the pipeline name should reach the path as one encoded segment
    assert_eq!(
        path,
        format!("/api/reactions/status/corn/{RUN_PIPELINE_ENCODED}/Created/")
    );
}

/// Listing a pipeline's reactions across groups encodes the pipeline name in the path
#[tokio::test]
async fn list_new_encodes_the_pipeline() {
    // list the run pseudo-pipeline's reactions across groups against the mock API
    let path = requested_path(|thorium| async move {
        thorium
            .reactions
            .list_new(RUN_PIPELINE, &ReactionCursorOpts::default())
            .await
            .unwrap();
    })
    .await;
    // the pipeline name should reach the path as one encoded segment
    assert_eq!(path, format!("/api/reactions/list/{RUN_PIPELINE_ENCODED}/"));
}

/// Listing a pipeline's reactions across groups by status encodes the pipeline and keeps the status
#[tokio::test]
async fn list_status_new_encodes_the_pipeline() {
    // list the run pseudo-pipeline's created reactions across groups against the mock API
    let path = requested_path(|thorium| async move {
        thorium
            .reactions
            .list_status_new(
                RUN_PIPELINE,
                &ReactionStatus::Created,
                &ReactionCursorOpts::default(),
            )
            .await
            .unwrap();
    })
    .await;
    // the pipeline name should reach the path as one encoded segment
    assert_eq!(
        path,
        format!("/api/reactions/status/{RUN_PIPELINE_ENCODED}/Created/")
    );
}

/// An ordinary pipeline name reaches the path unchanged
#[tokio::test]
async fn ordinary_pipeline_names_are_unchanged() {
    // list an ordinary pipeline's reactions against the mock API
    let path = requested_path(|thorium| async move {
        thorium
            .reactions
            .list("corn", "CornHarvest")
            .exec()
            .await
            .unwrap();
    })
    .await;
    // a name with no reserved characters should reach the path unchanged
    assert_eq!(path, "/api/reactions/list/corn/CornHarvest/");
}
