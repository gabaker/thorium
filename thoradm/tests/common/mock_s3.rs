//! An in-process HTTP proxy in front of the real S3 service that injects faults by rule
//!
//! thoradm is configured with the proxy's endpoint. Requests matching no rule are forwarded to the
//! real service unchanged; a matching request can get an S3 error status, a truncated or reset
//! body, or a delay. Every request is recorded with its `Host` header, so tests can tell
//! path-style from virtual-hosted addressing.
//!
//! The proxy forwards requests as they were signed for its own address, so the upstream must not
//! verify `SigV4` signatures (the harness's `SeaweedFS` runs without identities).

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
use futures::StreamExt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};

/// Which requests a rule applies to and what it does to them
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Rule {
    /// The HTTP method to match (`GET`, `PUT`, `POST`, `HEAD`, `DELETE`)
    pub method: Method,
    /// A glob over the object key (`*/4f2a*`); `*` matches every key
    pub key: String,
    /// The S3 operation to match by query parameter (`uploadId`, `partNumber`, `uploads`)
    pub query: Option<String>,
    /// Apply to the nth matching request only (1-based); `None` applies to every match
    pub nth: Option<u64>,
    /// What happens to a matching request
    pub behavior: S3Behavior,
}

/// What `MockS3` does to a matching request
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S3Behavior {
    /// Forward unchanged
    Forward,
    /// Answer with this status and an S3 XML error body naming the code (`SlowDown`, `NoSuchKey`)
    Status {
        /// The HTTP status
        status: u16,
        /// The S3 error code in the body
        code: String,
    },
    /// Forward, then cut the response body after this many bytes, keeping the full `Content-Length`
    TruncateBody(u64),
    /// Forward, then abort the connection after this many body bytes
    ResetAfter(u64),
    /// Forward, then send nothing more after this many body bytes, holding the connection open
    StallAfter(u64),
    /// Wait this long before forwarding
    Delay(Duration),
}

/// One request `MockS3` saw
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenRequest {
    /// The HTTP method
    pub method: Method,
    /// The `Host` header, which shows path-style versus virtual-hosted addressing
    pub host: String,
    /// The request path and query
    pub path: String,
    /// The bucket the request addressed, from the host or the path
    pub bucket: String,
    /// The object key the request addressed, decoded
    pub key: String,
    /// The status `MockS3` returned
    pub status: u16,
    /// The request's `Content-Type` header, if it sent one
    pub content_type: Option<String>,
}

/// One active rule and how many requests it has matched
#[derive(Debug)]
struct ActiveRule {
    /// The rule as configured
    rule: S3Rule,
    /// How many requests matched it since it was set
    matched: AtomicU64,
}

/// The state shared by the proxy's request handlers
#[derive(Debug)]
struct Shared {
    /// The real S3 endpoint requests are forwarded to
    upstream: String,
    /// The host name (without port) the proxy is addressed by
    own_host: String,
    /// The active rules, checked in order for each request
    rules: RwLock<Vec<ActiveRule>>,
    /// Every request seen
    seen: Mutex<Vec<SeenRequest>>,
    /// The client requests are forwarded with
    client: reqwest::Client,
}

/// An in-process HTTP proxy in front of the real S3 service that injects faults by rule
#[derive(Debug, Clone)]
pub struct MockS3 {
    /// The endpoint thoradm is configured with
    pub endpoint: String,
    /// The real S3 endpoint requests are forwarded to
    pub upstream: String,
    /// The state shared with the request handlers
    shared: Arc<Shared>,
}

/// Returns whether a glob with `*` wildcards matches a whole string
///
/// # Arguments
///
/// * `glob` - The pattern; `*` matches any run of characters, including none
/// * `text` - The string to match
#[must_use]
pub fn glob_matches(glob: &str, text: &str) -> bool {
    // split the pattern on wildcards; the pieces must appear in order
    let pieces: Vec<&str> = glob.split('*').collect();
    // a pattern without wildcards must match exactly
    if pieces.len() == 1 {
        return glob == text;
    }
    // the first piece anchors the start and the last anchors the end
    let (first, last) = (pieces[0], pieces[pieces.len() - 1]);
    if !text.starts_with(first) || text.len() < first.len() + last.len() || !text.ends_with(last) {
        return false;
    }
    // every middle piece must appear, in order, between the anchors
    let mut rest = &text[first.len()..text.len() - last.len()];
    for piece in &pieces[1..pieces.len() - 1] {
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    true
}

/// Decodes `%XX` escapes in a URL path
///
/// # Arguments
///
/// * `raw` - The escaped path
#[must_use]
pub fn percent_decode(raw: &str) -> String {
    // copy bytes, turning each valid escape into the byte it names
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // decode an escape when two hex digits follow it
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |byte: u8| char::from(byte).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(u8::try_from(high * 16 + low).unwrap_or_default());
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl MockS3 {
    /// Starts a proxy on an ephemeral local port in front of an S3 endpoint
    ///
    /// # Arguments
    ///
    /// * `upstream` - The real S3 endpoint (`http://127.0.0.1:8333`)
    pub async fn start(upstream: &str) -> std::io::Result<Self> {
        // bind an ephemeral port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr: SocketAddr = listener.local_addr()?;
        // build the state the handlers share
        let shared = Arc::new(Shared {
            upstream: upstream.trim_end_matches('/').to_owned(),
            own_host: "localhost".to_owned(),
            rules: RwLock::default(),
            seen: Mutex::default(),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .map_err(std::io::Error::other)?,
        });
        // serve every request through one handler
        let app = Router::new().fallback(handle).with_state(shared.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(MockS3 {
            endpoint: format!("http://localhost:{}", addr.port()),
            upstream: shared.upstream.clone(),
            shared,
        })
    }

    /// Replaces the active rules; an empty list makes the proxy forward unchanged
    ///
    /// # Arguments
    ///
    /// * `rules` - The rules to apply from now on
    pub async fn set_rules(&self, rules: Vec<S3Rule>) {
        // wrap each rule with a fresh match count
        let active = rules
            .into_iter()
            .map(|rule| ActiveRule {
                rule,
                matched: AtomicU64::new(0),
            })
            .collect();
        // swap the rules under the lock
        *self.shared.rules.write().await = active;
    }

    /// Returns every request seen so far
    pub async fn seen(&self) -> Vec<SeenRequest> {
        // copy the record out from under the lock
        self.shared.seen.lock().await.clone()
    }

    /// Forgets the requests seen so far
    pub async fn clear_seen(&self) {
        // empty the record under the lock
        self.shared.seen.lock().await.clear();
    }
}

/// Splits a request into its bucket and decoded key, from the host or the path
///
/// # Arguments
///
/// * `own_host` - The host name the proxy is addressed by
/// * `host` - The request's `Host` header without its port
/// * `path` - The request's path
fn bucket_and_key(own_host: &str, host: &str, path: &str) -> (String, String, bool) {
    // a virtual-hosted request names the bucket in front of the proxy's host name
    if let Some(bucket) = host
        .strip_suffix(own_host)
        .and_then(|rest| rest.strip_suffix('.'))
    {
        return (
            bucket.to_owned(),
            percent_decode(path.trim_start_matches('/')),
            true,
        );
    }
    // a path-style request names the bucket in the first path segment
    let trimmed = path.trim_start_matches('/');
    let (bucket, key) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    (bucket.to_owned(), percent_decode(key), false)
}

/// Finds the first rule matching a request, counting the match
///
/// # Arguments
///
/// * `shared` - The proxy's state
/// * `method` - The request's method
/// * `key` - The decoded object key
/// * `query` - The raw query string
async fn matching_behavior(shared: &Shared, method: &Method, key: &str, query: &str) -> S3Behavior {
    // check the rules in order
    for active in shared.rules.read().await.iter() {
        // the method, key, and query parameter must all match
        let rule = &active.rule;
        let query_matches = rule.query.as_ref().is_none_or(|param| {
            query
                .split('&')
                .any(|pair| pair.split('=').next() == Some(param.as_str()))
        });
        if rule.method != *method || !glob_matches(&rule.key, key) || !query_matches {
            continue;
        }
        // count the match and apply the rule to the chosen match only
        let number = active.matched.fetch_add(1, Ordering::SeqCst) + 1;
        if rule.nth.is_none_or(|nth| nth == number) {
            return rule.behavior.clone();
        }
    }
    S3Behavior::Forward
}

/// Builds an S3 XML error response
///
/// # Arguments
///
/// * `status` - The HTTP status
/// * `code` - The S3 error code
fn error_response(status: u16, code: &str) -> Response {
    // S3 errors are an XML document naming the code
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error><Code>{code}</Code><Message>injected by MockS3</Message></Error>"
    );
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header("content-type", "application/xml")
        .body(Body::from(body))
        .expect("failed to build an error response")
}

/// Handles one request: records it, applies the first matching rule, and forwards it
///
/// # Arguments
///
/// * `shared` - The proxy's state
/// * `request` - The incoming request
async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    // read the parts that decide routing and matching
    let method = request.method().clone();
    let headers = request.headers().clone();
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let host_name = host.split(':').next().unwrap_or_default().to_owned();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let (bucket, key, virtual_hosted) = bucket_and_key(&shared.own_host, &host_name, &path);
    // pick what to do with this request
    let behavior = matching_behavior(&shared, &method, &key, &query).await;
    // read the whole request body so it can be forwarded with a known length
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    // answer injected errors without forwarding
    let response = match &behavior {
        S3Behavior::Status { status, code } => error_response(*status, code),
        _ => {
            // wait first when the rule delays the request
            if let S3Behavior::Delay(delay) = &behavior {
                tokio::time::sleep(*delay).await;
            }
            // forward in path style, rebuilding the path for a virtual-hosted request
            let upstream_path = if virtual_hosted {
                format!("/{bucket}{path}")
            } else {
                path.clone()
            };
            forward(
                &shared,
                &method,
                &headers,
                &upstream_path,
                &query,
                body,
                &behavior,
            )
            .await
        }
    };
    // record what was seen and what was returned
    let full_path = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    // and the content type the request declared, which an upload's object is stored with
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    shared.seen.lock().await.push(SeenRequest {
        method,
        host,
        path: full_path,
        bucket,
        key,
        status: response.status().as_u16(),
        content_type,
    });
    response
}

/// Forwards a request to the real S3 service and relays its response
///
/// # Arguments
///
/// * `shared` - The proxy's state
/// * `method` - The request's method
/// * `headers` - The request's headers
/// * `path` - The path-style path to forward to
/// * `query` - The raw query string
/// * `body` - The request body
/// * `behavior` - What to do to the response body
async fn forward(
    shared: &Shared,
    method: &Method,
    headers: &HeaderMap,
    path: &str,
    query: &str,
    body: Bytes,
    behavior: &S3Behavior,
) -> Response {
    // build the upstream URL
    let url = if query.is_empty() {
        format!("{}{path}", shared.upstream)
    } else {
        format!("{}{path}?{query}", shared.upstream)
    };
    // copy the request headers except the ones tied to this hop
    let mut request = shared.client.request(method.clone(), &url).body(body);
    for (name, value) in headers {
        if !matches!(
            name.as_str(),
            "host" | "content-length" | "transfer-encoding" | "connection" | "keep-alive"
        ) {
            request = request.header(name, value);
        }
    }
    // send it, answering a gateway error when the upstream can't be reached
    let upstream = match request.send().await {
        Ok(upstream) => upstream,
        Err(err) => {
            return Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .body(Body::from(err.to_string()))
                .expect("failed to build a gateway error");
        }
    };
    // relay the status and headers except the ones tied to this hop
    let mut builder = Response::builder().status(upstream.status().as_u16());
    for (name, value) in upstream.headers() {
        if !matches!(
            name.as_str(),
            "transfer-encoding" | "connection" | "keep-alive"
        ) {
            builder = builder.header(
                name.as_str(),
                HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
        }
    }
    // relay the body, cut short when the rule asks for it
    let cut = match behavior {
        S3Behavior::TruncateBody(limit) => Some((*limit, false)),
        S3Behavior::ResetAfter(limit) => Some((*limit, true)),
        _ => None,
    };
    let stream = upstream.bytes_stream();
    let body = match (cut, behavior) {
        (Some((limit, abort)), _) => Body::from_stream(cut_stream(stream, limit, abort)),
        (None, S3Behavior::StallAfter(limit)) => Body::from_stream(stall_stream(stream, *limit)),
        (None, _) => Body::from_stream(stream),
    };
    builder
        .body(body)
        .expect("failed to build a relayed response")
}

/// How long a cut body pauses before it ends, so the server flushes what it already sent
const FLUSH_PAUSE: Duration = Duration::from_millis(100);

/// Passes at most `limit` bytes of a body stream, then ends it or fails it
///
/// Ending early under the original `Content-Length` makes the server close the connection with
/// the body short; failing the stream makes it abort the connection. Either way the stream pauses
/// first, so the response head and the bytes before the cut reach the client.
///
/// # Arguments
///
/// * `stream` - The upstream body
/// * `limit` - How many bytes to pass
/// * `abort` - Fail the stream at the limit instead of ending it
fn cut_stream<S>(
    stream: S,
    limit: u64,
    abort: bool,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    // pass chunks until the limit
    let trimmed = trim_stream(stream, limit);
    // pause so the server flushes, then end the body or fail it
    let tail = futures::stream::once(async move {
        tokio::time::sleep(FLUSH_PAUSE).await;
        abort.then(|| Err(std::io::Error::other("connection aborted by MockS3")))
    })
    .filter_map(|item| async move { item });
    trimmed.chain(tail)
}

/// Passes at most `limit` bytes of a body stream, then never yields again, so the response stays
/// open with its body short until the client gives up
///
/// # Arguments
///
/// * `stream` - The upstream body
/// * `limit` - How many bytes to pass
fn stall_stream<S>(
    stream: S,
    limit: u64,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    // pass chunks until the limit, then hold the body open
    trim_stream(stream, limit).chain(futures::stream::pending())
}

/// Passes at most `limit` bytes of a body stream, trimming the chunk that crosses it, then ends
///
/// # Arguments
///
/// * `stream` - The upstream body
/// * `limit` - How many bytes to pass
fn trim_stream<S>(
    stream: S,
    limit: u64,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    // pass chunks until the limit, trimming the chunk that crosses it
    futures::stream::unfold(
        (Box::pin(stream), 0_u64),
        move |(mut stream, passed)| async move {
            // stop once the limit was reached
            if passed >= limit {
                return None;
            }
            // pass the next chunk, trimmed to the room left
            match stream.next().await? {
                Ok(chunk) => {
                    let room = usize::try_from(limit - passed).unwrap_or(usize::MAX);
                    let chunk = if chunk.len() > room {
                        chunk.slice(..room)
                    } else {
                        chunk
                    };
                    let passed = passed + chunk.len() as u64;
                    Some((Ok(chunk), (stream, passed)))
                }
                Err(err) => Some((Err(std::io::Error::other(err)), (stream, limit))),
            }
        },
    )
}
