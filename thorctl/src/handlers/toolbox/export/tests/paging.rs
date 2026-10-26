//! Tests that exports page their lists and bound their concurrent reads

use chrono::Utc;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thorium::Thorium;
use thorium::models::{EntityKinds, ToolInstanceSummary, ToolKind, ToolVersion};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use uuid::Uuid;

use super::tool_entity;
use crate::handlers::toolbox::export::live::{self, PAGE_SIZE};

/// How many requests a mock API got
#[derive(Default)]
struct Counters {
    /// Requests for a page of entities
    lists: AtomicUsize,
    /// Requests for a tool's instances
    instance_reads: AtomicUsize,
    /// Instance requests being answered right now
    in_flight: AtomicUsize,
    /// The most instance requests that were being answered at once
    peak: AtomicUsize,
    /// The request lines received
    lines: Mutex<Vec<String>>,
}

/// Start a mock API that serves `count` tools a page at a time, each with one instance
///
/// Returns the mock's url and what it counted.
///
/// # Arguments
///
/// * `count` - The number of tools to serve
async fn mock_tools(count: usize) -> (String, Arc<Counters>) {
    // the tools served, in order
    let tools: Arc<Vec<Value>> = Arc::new(
        (0..count)
            .map(|index| {
                let tool = tool_entity(
                    Uuid::from_u128(index as u128 + 1_000),
                    &format!("tool{index}"),
                    &["g"],
                    ToolKind::Image,
                    None,
                );
                serde_json::to_value(tool).unwrap()
            })
            .collect(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let counters = Arc::new(Counters::default());
    let shared = counters.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let tools = tools.clone();
            let counters = shared.clone();
            // answer each connection on its own task so requests overlap
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 16_384];
                let read = stream.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..read]).to_string();
                let line = text.lines().next().unwrap_or_default().to_owned();
                counters.lines.lock().unwrap().push(line.clone());
                let body = if line.starts_with("GET /api/entities/details/") {
                    counters.lists.fetch_add(1, Ordering::SeqCst);
                    // the cursor id carries the offset of the page
                    let offset = line
                        .split("cursor=")
                        .nth(1)
                        .and_then(|rest| rest.split(['&', ' ']).next())
                        .and_then(|id| Uuid::parse_str(id).ok())
                        .map_or(0, |id| id.as_u128() as usize);
                    let end = (offset + PAGE_SIZE).min(tools.len());
                    let mut page = json!({"data": tools[offset..end]});
                    if end < tools.len() {
                        page["cursor"] = json!(Uuid::from_u128(end as u128));
                    }
                    page.to_string()
                } else if line.contains("/instances") {
                    counters.instance_reads.fetch_add(1, Ordering::SeqCst);
                    let now = counters.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    counters.peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    counters.in_flight.fetch_sub(1, Ordering::SeqCst);
                    let summary = ToolInstanceSummary {
                        id: Uuid::new_v4(),
                        version: ToolVersion::from("1.0.0"),
                        spec_hash: "hash".to_owned(),
                        deprecated: false,
                        created: Utc::now(),
                    };
                    json!([summary]).to_string()
                } else {
                    "{}".to_owned()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    (host, counters)
}

/// Listing 120 tools takes one request per page of 100, and their instances are read with a
/// bounded number of requests in flight
#[tokio::test]
async fn export_pages_lists_and_bounds_instance_reads() {
    // a mock API with 120 tools
    let (host, counters) = mock_tools(120).await;
    let thorium = Thorium::build(&host).token("token").build().await.unwrap();
    // two pages hold every tool
    let entities = live::list_entities(&thorium, &["g".to_owned()], EntityKinds::Tool)
        .await
        .unwrap();
    assert_eq!(entities.len(), 120);
    assert_eq!(
        counters.lists.load(Ordering::SeqCst),
        120_usize.div_ceil(PAGE_SIZE)
    );
    // each tool's instances are read once, never more than the workers at a time
    let tools = live::read_tools(&thorium, &entities, 4).await.unwrap();
    assert_eq!(tools.len(), 120);
    assert!(tools.iter().all(|tool| tool.instances.len() == 1));
    assert_eq!(counters.instance_reads.load(Ordering::SeqCst), 120);
    let peak = counters.peak.load(Ordering::SeqCst);
    assert!((2..=4).contains(&peak), "peak concurrency was {peak}");
}
