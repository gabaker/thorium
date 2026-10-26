use elasticsearch::{Elasticsearch, OpenPointInTimeParts};
use serde::{Deserialize, Serialize};
use tracing::{Level, event};

use crate::models::ElasticDoc;
use crate::utils::{ApiError, Shared};
use crate::{internal_err, unavailable};

/// A point in time id for an elastic query
#[derive(Deserialize, Serialize, Debug)]
pub struct PointInTime {
    pub id: String,
}

impl PointInTime {
    /// get a new point in time value
    ///
    /// # Arguments
    ///
    /// * `indexes` - The indexes we are creating a point in time for
    /// * `shared` - Shared Thorium objects
    pub async fn new(indexes: &[&str], shared: &Shared) -> Result<Self, ApiError> {
        // open a point in time with our elastic client
        Self::open(&shared.elastic, indexes).await
    }

    /// Open a point in time over some indexes with an Elastic client
    ///
    /// An index that does not exist yet is a search that is not set up, not a failure of this request,
    /// so it answers `503` with a message saying so instead of an opaque `500`.
    ///
    /// # Arguments
    ///
    /// * `elastic` - The client to open the point in time with
    /// * `indexes` - The indexes we are creating a point in time for
    async fn open(elastic: &Elasticsearch, indexes: &[&str]) -> Result<Self, ApiError> {
        // create a point in time api for our query
        let resp = elastic
            .open_point_in_time(OpenPointInTimeParts::Index(indexes))
            .keep_alive("1d")
            .send()
            .await?;
        // elastic answers 404 when an index we asked about does not exist
        if resp.status_code().as_u16() == 404 {
            // read why elastic answered 404 so only a missing index is reported as unavailable
            let body = resp.text().await?;
            if body.contains("index_not_found_exception") {
                return unavailable!(
                    "The search index does not exist yet, so nothing can be searched; the search \
                     streamer creates it"
                        .to_owned()
                );
            }
            // any other 404 is a failure we can't explain to the caller, so log it and hide it
            event!(Level::ERROR, msg = "Elastic answered 404", body = body);
            return internal_err!();
        }
        // cast our response to a pit value
        let pit = resp.json::<Self>().await?;
        Ok(pit)
    }
}

/// The documents that hit on a particular Elastic Query
#[derive(Deserialize, Serialize, Default, Debug)]
pub struct Hits {
    /// The max score of a single document that was returned
    pub max_score: Option<f64>,
    /// The documents that hit
    pub hits: Vec<ElasticDoc>,
}

// A response from Elastic
#[derive(Deserialize, Serialize, Default, Debug)]
pub struct ElasticResponse {
    /// The point in time value used for this search
    pub pit_id: String,
    /// How long this query took in ms
    pub took: u64,
    /// Whether this request timed out or not
    pub timed_out: bool,
    /// The results for this search query
    pub hits: Hits,
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use elasticsearch::Elasticsearch;
    use elasticsearch::http::transport::Transport;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::PointInTime;

    /// Serve one canned HTTP response and build an Elastic client that talks to it
    ///
    /// # Arguments
    ///
    /// * `status` - The status line the fake Elastic answers with
    /// * `body` - The JSON body the fake Elastic answers with
    async fn fake_elastic(status: &'static str, body: &'static str) -> Elasticsearch {
        // listen on a free local port
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a listener");
        let port = listener.local_addr().expect("an address").port();
        // answer the first request with the canned response and close the connection
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("a connection");
            let mut request = vec![0_u8; 4096];
            let _ = socket.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("a write");
        });
        // point an Elastic client at it
        let transport =
            Transport::single_node(&format!("http://127.0.0.1:{port}")).expect("a transport");
        Elasticsearch::new(transport)
    }

    /// A missing index answers `503` with a message that names the cause, not an opaque `500`
    #[tokio::test]
    async fn a_missing_index_is_unavailable() {
        // elastic answers a point in time over a missing index with a 404
        let elastic = fake_elastic(
            "404 Not Found",
            r#"{"error":{"type":"index_not_found_exception","reason":"no such index [thorium_files]"},"status":404}"#,
        )
        .await;
        // opening the point in time is refused as unavailable
        let error = PointInTime::open(&elastic, &["thorium_files"])
            .await
            .expect_err("a missing index must fail");
        assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            error
                .public_msg()
                .is_some_and(|msg| msg.contains("search index does not exist")),
            "the message names the missing index"
        );
    }

    /// A 404 that is not a missing index stays an internal error with no message
    #[tokio::test]
    async fn another_not_found_is_internal() {
        // elastic answers 404 for something other than a missing index
        let elastic = fake_elastic(
            "404 Not Found",
            r#"{"error":{"type":"resource_not_found_exception","reason":"gone"},"status":404}"#,
        )
        .await;
        // the caller learns nothing about the cause
        let error = PointInTime::open(&elastic, &["thorium_files"])
            .await
            .expect_err("a 404 must fail");
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(error.public_msg().is_none());
    }

    /// A point in time that elastic opens is returned
    #[tokio::test]
    async fn an_open_point_in_time_is_returned() {
        // elastic opens the point in time
        let elastic = fake_elastic("200 OK", r#"{"id":"pit-id"}"#).await;
        // we get its id back
        let pit = PointInTime::open(&elastic, &["thorium_files"])
            .await
            .expect("an open point in time");
        assert_eq!(pit.id, "pit-id");
    }
}
