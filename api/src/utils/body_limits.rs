//! Request body limits that answer an oversized body with a JSON `413` the client can read
//!
//! One app-wide layer ([`enforce`]) wraps every request body and holds it to the default limit.
//! Routes with their own limit ([`limit`], [`fixed`]) or none ([`unlimited`]) change the limit the
//! body is held to before their handler reads it. A body over its limit is read and thrown away
//! up to [`DRAIN_ALLOWANCE`] bytes past the limit before the handler sees an error, so the client
//! has finished sending when the `413` is written and reads it instead of a connection reset.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_LENGTH;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use bytesize::ByteSize;
use http_body::{Frame, SizeHint};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use super::ApiError;
use crate::conf::BodyLimits;

/// How many bytes past its limit an oversized body is read and thrown away
///
/// A client that is still sending when the API closes the connection may see a reset instead of
/// the `413`, so the rest of an oversized body is read first. A body that declares or sends more
/// than this past its limit isn't read any further, so a client can't make the API read an
/// unbounded body.
pub const DRAIN_ALLOWANCE: u64 = 64 << 20;

/// The limit a request body is held to
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLimit {
    /// The most bytes the body may have, or `None` for no limit
    pub max: Option<u64>,
    /// The setting that controls this limit, if an admin can change it
    pub setting: Option<&'static str>,
}

impl BodyLimit {
    /// Build a limit an admin controls with a setting
    ///
    /// # Arguments
    ///
    /// * `max` - The most bytes the body may have
    /// * `setting` - The setting that controls this limit
    #[must_use]
    pub fn configured(max: ByteSize, setting: &'static str) -> Self {
        BodyLimit {
            max: Some(max.as_u64()),
            setting: Some(setting),
        }
    }

    /// Build the response for a body over this limit
    ///
    /// The JSON body has the error message, the limit in bytes, and the setting that controls
    /// it when there is one.
    #[must_use]
    pub fn too_large(&self) -> Response {
        // describe the limit in both a readable size and exact bytes
        let max = self.max.unwrap_or(u64::MAX);
        let size = ByteSize::b(max).display().iec();
        let msg = match self.setting {
            Some(setting) => format!(
                "The request body is larger than the {size} ({max} bytes) this route accepts; an admin can raise {setting}"
            ),
            None => format!(
                "The request body is larger than the {size} ({max} bytes) this route accepts"
            ),
        };
        // write the limit and its setting beside the message
        let mut details = serde_json::Map::new();
        details.insert("limit".to_owned(), serde_json::Value::from(max));
        if let Some(setting) = self.setting {
            details.insert("setting".to_owned(), serde_json::Value::from(setting));
        }
        ApiError::Detailed {
            code: StatusCode::PAYLOAD_TOO_LARGE,
            msg,
            details,
        }
        .into_response()
    }
}

/// What a request's body limit and its body share
#[derive(Debug)]
struct LimitState {
    /// The limit the body is held to
    limit: BodyLimit,
    /// Whether the body was found to be over its limit
    exceeded: bool,
}

/// A handle on a request's body limit, kept in the request's extensions
#[derive(Debug, Clone)]
struct LimitHandle(Arc<Mutex<LimitState>>);

impl LimitHandle {
    /// Run a closure on the shared state
    ///
    /// # Arguments
    ///
    /// * `f` - The closure to run
    fn with<T>(&self, f: impl FnOnce(&mut LimitState) -> T) -> T {
        // a panic while holding the lock can't leave the state half written, so use it anyway
        let mut state = match self.0.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut state)
    }
}

/// The error a body over its limit ends with
#[derive(Debug)]
struct BodyTooLarge;

impl std::fmt::Display for BodyTooLarge {
    /// Describe this error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the request body is over its limit")
    }
}

impl std::error::Error for BodyTooLarge {}

/// A request body held to the limit in its shared state
struct LimitedBody {
    /// The body being read
    inner: Body,
    /// The limit this body is held to
    handle: LimitHandle,
    /// The length the request declared, if it declared one
    declared: Option<u64>,
    /// How many bytes have been read so far
    read: u64,
    /// Whether this body is over its limit and being thrown away
    draining: bool,
}

impl LimitedBody {
    /// Mark this body as over its limit and decide whether the rest of it should be read
    ///
    /// Returns true when the body should be read and thrown away, or false when it is so far
    /// over its limit that reading it should stop now.
    ///
    /// # Arguments
    ///
    /// * `max` - The limit this body is over
    fn exceed(&mut self, max: u64) -> bool {
        // remember the body was too large so the response becomes a 413
        self.handle.with(|state| state.exceeded = true);
        self.draining = true;
        // read the rest only if it ends within the allowance past the limit
        let cap = max.saturating_add(DRAIN_ALLOWANCE);
        self.read <= cap && self.declared.is_none_or(|declared| declared <= cap)
    }
}

impl http_body::Body for LimitedBody {
    /// The data the body yields
    type Data = Bytes;
    /// The error the body yields
    type Error = axum::Error;

    /// Read the next frame, ending with an error once the body is over its limit
    ///
    /// # Arguments
    ///
    /// * `cx` - The context of the task reading the body
    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        // the body is Unpin, so it can be read through a plain mutable reference
        let this = self.get_mut();
        loop {
            // get the limit this body is held to now, since a route may have changed it
            let max = this.handle.with(|state| state.limit.max);
            // a declared length over the limit is refused before any of it is passed on
            if let (false, Some(max), Some(declared)) = (this.draining, max, this.declared)
                && declared > max
                && !this.exceed(max)
            {
                return Poll::Ready(Some(Err(axum::Error::new(BodyTooLarge))));
            }
            // read the next frame of the body
            let frame = match Pin::new(&mut this.inner).poll_frame(cx) {
                Poll::Ready(frame) => frame,
                Poll::Pending => return Poll::Pending,
            };
            match frame {
                // a body that was over its limit fails once all of it has been read
                None if this.draining => {
                    return Poll::Ready(Some(Err(axum::Error::new(BodyTooLarge))));
                }
                None => return Poll::Ready(None),
                Some(Err(error)) => return Poll::Ready(Some(Err(error))),
                Some(Ok(frame)) => {
                    // count the bytes this frame holds
                    if let Some(data) = frame.data_ref() {
                        this.read = this.read.saturating_add(data.len() as u64);
                    }
                    // pass the frame on while the body is within its limit
                    let Some(max) = max else {
                        return Poll::Ready(Some(Ok(frame)));
                    };
                    if !this.draining && this.read <= max {
                        return Poll::Ready(Some(Ok(frame)));
                    }
                    // throw away the frame that goes over the limit and the rest after it
                    if !this.exceed(max) {
                        return Poll::Ready(Some(Err(axum::Error::new(BodyTooLarge))));
                    }
                }
            }
        }
    }

    /// Whether the body has no more frames
    fn is_end_stream(&self) -> bool {
        // a body being thrown away still ends with its error
        !self.draining && self.inner.is_end_stream()
    }

    /// The bounds on the body's remaining length
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Hold every request body to a default limit and answer a body over its limit with a `413`
///
/// This is applied once to the whole app. A route's own [`limit`], [`fixed`], or [`unlimited`]
/// layer changes the limit before the route's handler reads the body.
///
/// # Arguments
///
/// * `default` - The limit of routes without their own
/// * `req` - The request to hold to a limit
/// * `next` - The rest of the app
pub async fn enforce(State(default): State<BodyLimit>, req: Request, next: Next) -> Response {
    // share the limit between the body, the route's own limit layer, and this layer
    let handle = LimitHandle(Arc::new(Mutex::new(LimitState {
        limit: default,
        exceeded: false,
    })));
    // get the length the request declares, if it declares one
    let declared = req
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    // hold the body to the shared limit
    let (mut parts, body) = req.into_parts();
    let body = LimitedBody {
        inner: body,
        handle: handle.clone(),
        declared,
        read: 0,
        draining: false,
    };
    parts.extensions.insert(handle.clone());
    let req = Request::from_parts(parts, Body::new(body));
    // run the request and answer with a 413 if its body was over its limit
    let response = next.run(req).await;
    match handle.with(|state| state.exceeded.then_some(state.limit)) {
        Some(limit) => limit.too_large(),
        None => response,
    }
}

/// Hold every request body of an app to the configured default limit, with axum's own limit
/// lifted so the routes' limits are the only ones
///
/// Apply this inside the trace and CORS layers, so a `413` is traced and still carries CORS
/// headers.
///
/// # Arguments
///
/// * `app` - The app whose request bodies to limit
/// * `limits` - The configured body limits
pub fn enforce_default<S>(app: Router<S>, limits: &BodyLimits) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    // limit routes without their own limit to the default, naming its setting in a 413
    let default = BodyLimit::configured(limits.default, "thorium.limits.body.default");
    // lift axum's limit and wrap every body in the shared limit
    app.layer(DefaultBodyLimit::disable())
        .layer(axum::middleware::from_fn_with_state(default, enforce))
}

/// Change the limit a request's body is held to
///
/// # Arguments
///
/// * `limit` - The limit to hold the body to
/// * `req` - The request whose limit to change
/// * `next` - The rest of the route
async fn apply(State(limit): State<BodyLimit>, req: Request, next: Next) -> Response {
    // the app-wide layer shares the body's limit in the request's extensions
    if let Some(handle) = req.extensions().get::<LimitHandle>() {
        handle.with(|state| state.limit = limit);
    }
    next.run(req).await
}

/// Hold the bodies of a route's methods to their own configured limit
///
/// Only the methods already added to the route are limited, so add the limited methods first.
///
/// # Arguments
///
/// * `route` - The route whose methods to limit
/// * `max` - The most bytes their bodies may have
/// * `setting` - The setting that controls the limit
pub fn limit<S>(route: MethodRouter<S>, max: ByteSize, setting: &'static str) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    // hold the bodies to the configured limit
    route.layer(axum::middleware::from_fn_with_state(
        BodyLimit::configured(max, setting),
        apply,
    ))
}

/// Hold the bodies of a route's methods to a fixed limit that no setting controls
///
/// Only the methods already added to the route are limited, so add the limited methods first.
///
/// # Arguments
///
/// * `route` - The route whose methods to limit
/// * `max` - The most bytes their bodies may have
pub fn fixed<S>(route: MethodRouter<S>, max: u64) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    // hold the bodies to the fixed limit
    let limit = BodyLimit {
        max: Some(max),
        setting: None,
    };
    route.layer(axum::middleware::from_fn_with_state(limit, apply))
}

/// Let the bodies of a route's methods be any size
///
/// Only the methods already added to the route are unlimited, so add them first.
///
/// # Arguments
///
/// * `route` - The route whose methods to leave unlimited
pub fn unlimited<S>(route: MethodRouter<S>) -> MethodRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    // lift the limit for these methods
    let limit = BodyLimit {
        max: None,
        setting: None,
    };
    route.layer(axum::middleware::from_fn_with_state(limit, apply))
}

#[cfg(test)]
mod tests;
