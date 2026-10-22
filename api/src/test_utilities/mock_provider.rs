//! A mock model provider that refresh tests list models from
//!
//! It serves canned replies by method and path on a local port, records every request it gets,
//! and counts how many requests it is answering at once. Replies can be delayed or never sent,
//! so tests can check timeouts and concurrency limits without reaching a real provider.

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::Error;

/// How long a reply that never comes holds its request open
const HANG: Duration = Duration::from_secs(600);

/// One canned reply
#[derive(Debug, Clone)]
pub struct MockReply {
    /// The status to answer with
    pub status: u16,
    /// The body to answer with
    pub body: String,
    /// How long to wait before answering
    pub delay: Duration,
    /// Whether to never answer
    pub hang: bool,
}

impl MockReply {
    /// Answer `200` with a JSON body
    ///
    /// # Arguments
    ///
    /// * `body` - The JSON text to answer with
    #[must_use]
    pub fn json(body: impl Into<String>) -> Self {
        MockReply {
            status: 200,
            body: body.into(),
            delay: Duration::ZERO,
            hang: false,
        }
    }

    /// Answer with a status and a raw body
    ///
    /// # Arguments
    ///
    /// * `status` - The status to answer with
    /// * `body` - The body to answer with
    #[must_use]
    pub fn status(status: u16, body: impl Into<String>) -> Self {
        MockReply {
            status,
            body: body.into(),
            delay: Duration::ZERO,
            hang: false,
        }
    }

    /// Never answer
    #[must_use]
    pub fn hang() -> Self {
        MockReply {
            status: 200,
            body: String::new(),
            delay: Duration::ZERO,
            hang: true,
        }
    }

    /// Wait before answering
    ///
    /// # Arguments
    ///
    /// * `delay` - How long to wait
    #[must_use]
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// One request the mock provider got
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// The request's method
    pub method: String,
    /// The request's path
    pub path: String,
    /// The request's query string, if it had one
    pub query: Option<String>,
    /// The request's headers by lowercase name
    pub headers: BTreeMap<String, String>,
    /// The request's body
    pub body: String,
}

/// What the mock provider's handler shares
#[derive(Debug, Default)]
struct MockState {
    /// The canned replies by method and path, or method, path, and query
    replies: Mutex<HashMap<String, MockReply>>,
    /// Every request so far, in the order they arrived
    requests: Mutex<Vec<RecordedRequest>>,
    /// How many requests are being answered right now
    in_flight: AtomicUsize,
    /// The most requests that were being answered at once
    max_in_flight: AtomicUsize,
}

/// Build the key a reply is stored under
///
/// # Arguments
///
/// * `method` - The request's method
/// * `path` - The request's path, with a query to match only requests with that query
fn reply_key(method: &str, path: &str) -> String {
    // methods are matched without case
    format!("{} {path}", method.to_uppercase())
}

/// Counts a request as in flight until it's dropped
struct InFlight<'a> {
    /// The counter to decrement when dropped
    counter: &'a AtomicUsize,
}

impl Drop for InFlight<'_> {
    /// Stop counting this request as in flight
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Record a request and answer it with its canned reply
///
/// A reply stored with the request's query is used before one stored with only its path, and a
/// request with no reply gets a `404`.
///
/// # Arguments
///
/// * `state` - The mock provider's shared state
/// * `method` - The request's method
/// * `uri` - The request's uri
/// * `headers` - The request's headers
/// * `body` - The request's body
async fn answer(
    State(state): State<Arc<MockState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // count this request as in flight while it's answered
    let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_in_flight.fetch_max(now, Ordering::SeqCst);
    let _guard = InFlight {
        counter: &state.in_flight,
    };
    // record the request
    let recorded = RecordedRequest {
        method: method.to_string(),
        path: uri.path().to_owned(),
        query: uri.query().map(ToOwned::to_owned),
        headers: headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_lowercase(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect(),
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    state
        .requests
        .lock()
        .expect("mock requests poisoned")
        .push(recorded);
    // find the reply for this path and query, or for this path alone
    let reply = {
        let replies = state.replies.lock().expect("mock replies poisoned");
        let with_query = uri
            .query()
            .map(|query| reply_key(method.as_str(), &format!("{}?{query}", uri.path())));
        with_query
            .and_then(|key| replies.get(&key).cloned())
            .or_else(|| {
                replies
                    .get(&reply_key(method.as_str(), uri.path()))
                    .cloned()
            })
    };
    let Some(reply) = reply else {
        return (StatusCode::NOT_FOUND, "no mock reply").into_response();
    };
    // wait as long as the reply asks, or forever
    if reply.hang {
        tokio::time::sleep(HANG).await;
    }
    if !reply.delay.is_zero() {
        tokio::time::sleep(reply.delay).await;
    }
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        reply.body,
    )
        .into_response()
}

/// A mock model provider listening on a local port
pub struct MockProvider {
    /// The address the mock provider listens on
    addr: std::net::SocketAddr,
    /// The state its handler shares
    state: Arc<MockState>,
    /// The server task
    handle: JoinHandle<()>,
}

impl MockProvider {
    /// Start a mock provider on a free local port
    ///
    /// # Errors
    ///
    /// Returns an error if no local port can be bound
    pub async fn start() -> Result<Self, Error> {
        // bind a free local port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|err| Error::new(format!("Failed to bind the mock provider: {err}")))?;
        let addr = listener
            .local_addr()
            .map_err(|err| Error::new(format!("Failed to read the mock provider's port: {err}")))?;
        // answer every request with the shared handler
        let state = Arc::new(MockState::default());
        let router = Router::new().fallback(answer).with_state(state.clone());
        let handle = tokio::spawn(async move {
            // the server only stops when the test drops the mock
            let _ = axum::serve(listener, router).await;
        });
        Ok(MockProvider {
            addr,
            state,
            handle,
        })
    }

    /// Get the base url of the mock provider
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Set the reply to a method and path, or to a method, path, and query (`/v1/models?x=1`)
    ///
    /// # Arguments
    ///
    /// * `method` - The method to answer
    /// * `path` - The path to answer, with a query to answer only requests with that query
    /// * `reply` - The reply to give
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while holding the replies
    pub fn reply(&self, method: &str, path: &str, reply: MockReply) {
        // replace any reply already set for this path
        self.state
            .replies
            .lock()
            .expect("mock replies poisoned")
            .insert(reply_key(method, path), reply);
    }

    /// Get every request the mock provider got, in the order they arrived
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while holding the requests
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state
            .requests
            .lock()
            .expect("mock requests poisoned")
            .clone()
    }

    /// Get the requests the mock provider got for one path
    ///
    /// # Arguments
    ///
    /// * `path` - The path the requests were for
    #[must_use]
    pub fn requests_to(&self, path: &str) -> Vec<RecordedRequest> {
        // keep the requests for this path
        self.requests()
            .into_iter()
            .filter(|request| request.path == path)
            .collect()
    }

    /// Forget every request recorded so far and reset the concurrency count
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while holding the requests
    pub fn clear_requests(&self) {
        // drop the recorded requests
        self.state
            .requests
            .lock()
            .expect("mock requests poisoned")
            .clear();
        // start counting concurrency from what's in flight now
        let now = self.state.in_flight.load(Ordering::SeqCst);
        self.state.max_in_flight.store(now, Ordering::SeqCst);
    }

    /// Get the most requests the mock provider was answering at once
    #[must_use]
    pub fn max_in_flight(&self) -> usize {
        self.state.max_in_flight.load(Ordering::SeqCst)
    }
}

impl Drop for MockProvider {
    /// Stop the mock provider's server
    fn drop(&mut self) {
        self.handle.abort();
    }
}
