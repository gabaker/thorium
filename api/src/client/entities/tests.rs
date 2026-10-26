//! Tests for the entity tag client methods against a one-request mock API

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use crate::Thorium;
use crate::models::{Entity, TagDeleteRequest, TagRequest};

/// Start a mock API that records its first request and answers `204`
///
/// Returns the url the mock listens at and a handle that resolves to the request it received.
async fn one_request_mock() -> (String, tokio::task::JoinHandle<String>) {
    // bind to a random local port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    // read one whole request in the background and answer it
    let handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut text = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            // read until the head and the declared body have arrived
            let read = stream.read(&mut chunk).await.unwrap();
            text.extend_from_slice(&chunk[..read]);
            let seen = String::from_utf8_lossy(&text).to_string();
            if let Some((head, body)) = seen.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if body.len() >= length || read == 0 {
                    break;
                }
            }
        }
        // answer with no content and close
        stream
            .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")
            .await
            .unwrap();
        stream.shutdown().await.unwrap();
        String::from_utf8_lossy(&text).to_string()
    });
    (host, handle)
}

/// Adding tags posts the request to the entity's tag route with the groups and tags in its body
#[tokio::test]
async fn tag_posts_to_the_entity_tag_route() {
    let (host, received) = one_request_mock().await;
    let thorium = Thorium::build(&host).token("t").build().await.unwrap();
    let id = Uuid::new_v4();
    let tags = TagRequest::<Entity>::default()
        .add("plant", "corn")
        .group("corn");
    let resp = thorium.entities.tag(id, &tags).await.unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    // the request line and body hold the entity, the group, and the tag
    let text = received.await.unwrap();
    assert!(
        text.starts_with(&format!("POST /api/entities/tags/{id} HTTP/1.1")),
        "{text}"
    );
    let body: serde_json::Value =
        serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["groups"], serde_json::json!(["corn"]));
    assert_eq!(body["tags"], serde_json::json!({"plant": ["corn"]}));
}

/// Removing tags sends a delete to the entity's tag route with the groups and tags in its body
#[tokio::test]
async fn untag_deletes_from_the_entity_tag_route() {
    let (host, received) = one_request_mock().await;
    let thorium = Thorium::build(&host).token("t").build().await.unwrap();
    let id = Uuid::new_v4();
    let tags_del = TagDeleteRequest::<Entity>::default()
        .add("plant", "corn")
        .group("corn");
    let resp = thorium.entities.untag(id, &tags_del).await.unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    // the request line and body hold the entity, the group, and the tag
    let text = received.await.unwrap();
    assert!(
        text.starts_with(&format!("DELETE /api/entities/tags/{id} HTTP/1.1")),
        "{text}"
    );
    let body: serde_json::Value =
        serde_json::from_str(text.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["groups"], serde_json::json!(["corn"]));
    assert_eq!(body["tags"], serde_json::json!({"plant": ["corn"]}));
}
