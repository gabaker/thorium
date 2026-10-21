//! Retrying transient failures with exponential, jittered backoff
//!
//! Each backend's errors are classified as transient (worth another attempt) or permanent:
//!
//! | Backend | Transient | Permanent |
//! |---|---|---|
//! | Scylla | timeouts, unavailable, overloaded, broken connections, a failed next page or metadata fetch | syntax, invalid requests, authorization, undecodable rows, bad cluster metadata |
//! | S3 | 5xx, throttling, timeouts and failed connections, a body cut short | 403 (including clock skew), 404, 400, requests that couldn't be built |
//! | The API | 5xx, connection failures, timeouts, a truncated or unparsable body | 4xx |
//! | Local disk | none | every error |

use rand::Rng;
use scylla::errors::{
    DbError, ExecutionError, MetadataError, MetadataFetchErrorKind, NextPageError, NextRowError,
    PagerExecutionError, RequestAttemptError, RequestError,
};
use std::future::Future;
use std::time::Duration;

use crate::Error;

/// S3 error codes that mean "try again later" whatever the status
const TRANSIENT_S3_CODES: &[&str] = &[
    "SlowDown",
    "Throttling",
    "ThrottlingException",
    "RequestTimeout",
    "InternalError",
    "ServiceUnavailable",
];

/// How many times, and how far apart, a transient failure is retried
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts per object, including the first
    pub attempts: u32,
    /// The ceiling of the delay before the second attempt; each later ceiling doubles
    pub base_delay: Duration,
    /// The longest delay between attempts
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    /// Five attempts, with delays from 200 ms up to 10 s
    fn default() -> Self {
        RetryPolicy {
            attempts: 5,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(10),
        }
    }
}

/// The result of an operation run under a retry policy, and how many attempts it took
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retried<T, E> {
    /// The last attempt's result
    pub result: Result<T, E>,
    /// How many attempts were made, including the first
    pub attempts: u32,
}

/// Whether an error is worth retrying
pub trait Transient {
    /// Returns true for errors the retry table lists as transient
    fn is_transient(&self) -> bool;
}

impl RetryPolicy {
    /// Builds the default policy with a different number of attempts
    ///
    /// # Arguments
    ///
    /// * `attempts` - Attempts per object, including the first; at least one is always made
    #[must_use]
    pub fn new(attempts: u32) -> Self {
        // keep the default delays
        RetryPolicy {
            attempts: attempts.max(1),
            ..RetryPolicy::default()
        }
    }

    /// Returns the ceiling of the delay before a retry
    ///
    /// # Arguments
    ///
    /// * `retry` - Which retry this is (1 for the delay before the second attempt)
    #[must_use]
    pub fn delay_ceiling(&self, retry: u32) -> Duration {
        // double the base delay for each earlier retry, never past the longest delay
        let doublings = retry.saturating_sub(1).min(31);
        self.base_delay
            .saturating_mul(1_u32 << doublings)
            .min(self.max_delay)
    }

    /// Returns a jittered delay before a retry, anywhere from zero up to its ceiling
    ///
    /// # Arguments
    ///
    /// * `retry` - Which retry this is (1 for the delay before the second attempt)
    #[must_use]
    pub fn delay(&self, retry: u32) -> Duration {
        // full jitter spreads retries from many workers apart
        let ceiling = u64::try_from(self.delay_ceiling(retry).as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(rand::rng().random_range(0..=ceiling))
    }

    /// Runs an operation, retrying transient failures after jittered delays
    ///
    /// # Arguments
    ///
    /// * `op` - Starts one attempt of the operation
    pub async fn run<T, E, F, Fut>(&self, op: F) -> Retried<T, E>
    where
        E: Transient,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        // sleep on the runtime's timer between attempts
        self.run_with(op, tokio::time::sleep).await
    }

    /// Runs an operation, retrying transient failures, with a chosen way of waiting
    ///
    /// # Arguments
    ///
    /// * `op` - Starts one attempt of the operation
    /// * `sleep` - Waits for a delay between attempts
    pub async fn run_with<T, E, F, Fut, S, SFut>(&self, mut op: F, mut sleep: S) -> Retried<T, E>
    where
        E: Transient,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        S: FnMut(Duration) -> SFut,
        SFut: Future<Output = ()>,
    {
        // attempt until success, a permanent error, or the last attempt
        let mut attempts = 1;
        loop {
            let result = op().await;
            match result {
                Err(error) if error.is_transient() && attempts < self.attempts => {
                    // wait a jittered, growing delay before the next attempt
                    sleep(self.delay(attempts)).await;
                    attempts += 1;
                }
                result => return Retried { result, attempts },
            }
        }
    }
}

/// Returns whether a database error from Scylla is worth retrying
///
/// # Arguments
///
/// * `error` - The error the server returned
fn db_error_is_transient(error: &DbError) -> bool {
    // the server was busy, short of replicas, or timed out; anything else won't change
    matches!(
        error,
        DbError::Unavailable { .. }
            | DbError::Overloaded
            | DbError::IsBootstrapping
            | DbError::ReadTimeout { .. }
            | DbError::WriteTimeout { .. }
            | DbError::ReadFailure { .. }
            | DbError::WriteFailure { .. }
            | DbError::ServerError
            | DbError::RateLimitReached { .. }
            | DbError::TruncateError
    )
}

/// Returns whether a failed attempt of a Scylla request is worth retrying
///
/// # Arguments
///
/// * `error` - The attempt's error
fn attempt_is_transient(error: &RequestAttemptError) -> bool {
    // broken connections and busy servers retry; malformed requests don't
    match error {
        RequestAttemptError::DbError(db_error, _) => db_error_is_transient(db_error),
        RequestAttemptError::BrokenConnectionError(_)
        | RequestAttemptError::UnableToAllocStreamId => true,
        _ => false,
    }
}

/// Returns whether a failed Scylla request is worth retrying
///
/// # Arguments
///
/// * `error` - The request's error
fn request_is_transient(error: &RequestError) -> bool {
    // timeouts and unusable pools retry; otherwise judge the last attempt
    match error {
        RequestError::ConnectionPoolError(_)
        | RequestError::RequestTimeout(_)
        | RequestError::EmptyPlan => true,
        RequestError::LastAttemptError(attempt) => attempt_is_transient(attempt),
        _ => false,
    }
}

/// Returns whether a failed page fetch is worth retrying
///
/// # Arguments
///
/// * `error` - The page's error
fn next_page_is_transient(error: &NextPageError) -> bool {
    // only a failed request can succeed later; bad metadata stays bad
    match error {
        NextPageError::RequestFailure(request) => request_is_transient(request),
        _ => false,
    }
}

/// Returns whether refreshing the cluster's metadata failed for a reason that can pass
///
/// # Arguments
///
/// * `error` - The metadata error
fn metadata_is_transient(error: &MetadataError) -> bool {
    // an unreachable control connection or a failed fetch can succeed later; bad metadata stays
    // bad
    match error {
        MetadataError::ConnectionPoolError(_) => true,
        MetadataError::FetchError(fetch) => match &fetch.error {
            MetadataFetchErrorKind::PrepareError(attempt) => attempt_is_transient(attempt),
            MetadataFetchErrorKind::NextRowError(row) => row.is_transient(),
            _ => false,
        },
        _ => false,
    }
}

impl Transient for ExecutionError {
    /// Returns true for timeouts, unusable pools, and busy or unreachable nodes
    fn is_transient(&self) -> bool {
        // classify by what went wrong
        match self {
            ExecutionError::ConnectionPoolError(_)
            | ExecutionError::RequestTimeout(_)
            | ExecutionError::EmptyPlan
            | ExecutionError::SchemaAgreementError(_) => true,
            ExecutionError::LastAttemptError(attempt) => attempt_is_transient(attempt),
            _ => false,
        }
    }
}

impl Transient for PagerExecutionError {
    /// Returns true when the first page failed for a transient reason
    fn is_transient(&self) -> bool {
        // classify by what went wrong
        match self {
            PagerExecutionError::NextPageError(page) => next_page_is_transient(page),
            PagerExecutionError::SchemaAgreementError(_) => true,
            PagerExecutionError::MetadataError(metadata) => metadata_is_transient(metadata),
            _ => false,
        }
    }
}

impl Transient for NextRowError {
    /// Returns true when a later page failed for a transient reason; an undecodable row never is
    fn is_transient(&self) -> bool {
        // classify by what went wrong
        match self {
            NextRowError::NextPageError(page) => next_page_is_transient(page),
            _ => false,
        }
    }
}

impl Transient for thorium::Error {
    /// Returns true for 5xx responses, connection failures, and timeouts from the API
    fn is_transient(&self) -> bool {
        // classify by the response status, or by how the request failed
        match self {
            thorium::Error::Thorium { code, .. } => code.is_server_error(),
            thorium::Error::Reqwest(error) => {
                error.is_connect()
                    || error.is_timeout()
                    || error.is_request()
                    || error.is_body()
                    || error.is_decode()
                    || error
                        .status()
                        .is_some_and(|status| status.is_server_error())
            }
            _ => false,
        }
    }
}

impl Transient for Error {
    /// Returns true for the errors the retry table lists as transient
    fn is_transient(&self) -> bool {
        // delegate to each backend's classification
        match self {
            Error::ScyllaQuery(error) => error.is_transient(),
            Error::ScyllaPagedQuery(error) => error.is_transient(),
            Error::ScyllaNextRow(error) => error.is_transient(),
            Error::Thorium(error) => error.is_transient(),
            Error::S3 {
                status,
                code,
                no_response,
                ..
            } => {
                // a response with a 5xx or a throttling code retries, and so does a request that
                // timed out or couldn't connect; other responses, and requests that couldn't be
                // built, are permanent
                status.is_some_and(|status| status >= 500)
                    || code
                        .as_deref()
                        .is_some_and(|code| TRANSIENT_S3_CODES.contains(&code))
                    || *no_response
            }
            Error::S3ByteStream(_) | Error::ShortBody { .. } => true,
            _ => false,
        }
    }
}
