//! Provider-agnostic retry with backoff for every completion request.
//!
//! Occasional throttles (429) and server faults (5xx) from a provider used to
//! kill whichever agent hit them: a sub-agent mid-run lost all of its
//! accumulated work to a single bad response. [`RetryingModel`] wraps any
//! provider's completion model behind rig's type-erased
//! [`ModelHandle`], so each individual request -- not the whole multi-turn
//! prompt -- is retried with jittered exponential backoff. The classification
//! is status-based ([`is_retryable`]) and therefore identical for every
//! provider (LiteLLM, Bedrock Converse and Mantle, OpenAI, Anthropic, ...);
//! providers whose transport loses the HTTP status normalize it back at their
//! error-mapping boundary instead of being special-cased here.
//!
//! Retries never spam the chat: each failed attempt emits one
//! [`ThorPalEvent::LlmRetrying`] carrying the attempt counter and the next
//! attempt's fire time, so a consumer renders a single in-place status line,
//! plus one `tracing` warning for the file log.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use rand::Rng;
use rig::agent::ModelHandle;
use rig::completion::{
    CompletionError, CompletionModel, CompletionRequest, CompletionResponse, ProviderCapabilities,
};
use rig::streaming::StreamingCompletionResponse;
use tracing::instrument;

use super::events::{ThorPalEvent, ThorPalEventSender};
use super::gate::{GatePass, PauseKind, ProbeToken, ProviderGate};
use super::scrub::{self, ScrubLevel, ScrubPolicy};

/// The attempt budget before a transient error is surfaced as fatal.
///
/// With the backoff parameters below this rides out roughly five minutes of
/// sustained throttling in the worst case before the original error is
/// returned to the caller.
const MAX_ATTEMPTS: u32 = 12;

/// The backoff delay after the first failed attempt.
const BASE_DELAY: Duration = Duration::from_secs(1);

/// The multiplier applied to the backoff delay after each failed attempt.
const BACKOFF_FACTOR: f64 = 2.0;

/// The cap on the exponential backoff delay between attempts.
const MAX_DELAY: Duration = Duration::from_secs(30);

/// The cap honored for a provider-mandated `Retry-After` wait.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Whether an HTTP status names a fault worth retrying.
///
/// # Arguments
///
/// * `status` - The status the provider answered with
fn retryable_status(status: http::StatusCode) -> bool {
    // 408/429 are pacing faults; 5xx are server faults; 529 is Anthropic's
    // non-standard "overloaded" answer
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Whether a completion error is a temporary fault worth retrying.
///
/// Classification is provider-agnostic: a preserved HTTP status decides when
/// one exists (every HTTP provider, plus SDK transports that normalize their
/// status back on, like Bedrock Converse), and otherwise only the error shapes
/// that are transient by construction are retried.
///
/// # Arguments
///
/// * `err` - The completion error to classify
pub(crate) fn is_retryable(err: &CompletionError) -> bool {
    // a preserved status is the authoritative signal when one exists
    if let Some(status) = err.provider_response_status() {
        return retryable_status(status);
    }
    match err {
        // a transport-level failure (connect, timeout, reset) never reached
        // the model and is always worth retrying
        CompletionError::HttpError(rig::http_client::Error::Instance(_)) => true,
        // malformed provider output has long been observed as transient (e.g.
        // LiteLLM responses missing the `type` tag rig needs to deserialize),
        // so keep retrying what the prompt-level loop used to
        CompletionError::JsonError(_) | CompletionError::ResponseError(_) => true,
        // everything else (provider prose, request-building faults) is fatal
        _ => false,
    }
}

/// Whether a completion error names an auth/credential fault.
///
/// A preserved 401/403 is decisive; any other preserved status is decisively
/// NOT an auth fault (AWS's expired/unrecognized-credential errors all arrive
/// as 403s when the status survives). A status-less error falls back to the
/// credential-shaped markers providers and SDK transports are known to emit
/// (e.g. Bedrock Mantle resolves its credential chain before the request and
/// surfaces a failure as transport-level prose).
///
/// # Arguments
///
/// * `err` - The completion error to classify
pub(crate) fn is_auth_error(err: &CompletionError) -> bool {
    // a preserved status is the authoritative signal when one exists
    if let Some(status) = err.provider_response_status() {
        return matches!(status.as_u16(), 401 | 403);
    }
    // otherwise look for credential-shaped markers in the whole error text;
    // the chain markers are deliberately narrow so transient chain prose
    // ("credentials provider timed out ...") stays retryable, not auth
    let text = err.to_string().to_lowercase();
    [
        "expiredtoken",
        "unrecognizedclient",
        "invalidsignature",
        "security token",
        "authentication_error",
        "invalid api key",
        "incorrect api key",
        "yielded no credentials",
        "failed to load credentials",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// Whether a completion error is a content-filtering proxy kill.
///
/// A middlebox that blocks a request on its payload answers with its own
/// block page: observed Zscaler kills were 403s whose bodies open with an
/// HTML comment nonce and a DOCTYPE, while every genuine provider auth fault
/// (AWS expired/unrecognized tokens included -- those also arrive as 403s)
/// carries a JSON error envelope. Classification therefore demands positive
/// evidence: a preserved 403 AND a non-empty preserved body that fails to
/// parse as JSON and starts with an HTML/SGML bracket. An absent or
/// ambiguous body stays auth-classified so the pause gate keeps covering
/// credential expiry (0029). The retry loop answers this class with
/// scrubbed-context retries instead of a pause: re-probing the identical
/// payload can never clear a content block, and the block is
/// payload-specific, so parking the whole session would stall unaffected
/// agents.
///
/// # Arguments
///
/// * `err` - The completion error to classify
pub(crate) fn is_content_filter_error(err: &CompletionError) -> bool {
    // only a preserved 403 can be a proxy content block
    if err.provider_response_status().map(|status| status.as_u16()) != Some(403) {
        return false;
    }
    // demand positive block-page evidence in the preserved body
    match err.provider_response_body() {
        Some(body) => {
            // a provider auth envelope is JSON; a block page is HTML/SGML
            let trimmed = body.trim_start();
            !trimmed.is_empty()
                && trimmed.starts_with('<')
                && serde_json::from_str::<serde_json::Value>(trimmed).is_err()
        }
        // an absent body is ambiguous and stays auth-classified
        None => false,
    }
}

/// Whether a completion error names an oversized-request fault.
///
/// Replaying a salvaged turn whose history already overflowed the model's
/// context would deterministically re-fail, so the salvage path drops those
/// tails instead. Classification is conservative: a 4xx (or status-less)
/// error whose text carries a context/length marker.
///
/// # Arguments
///
/// * `err` - The completion error to classify
pub(crate) fn is_context_length_error(err: &CompletionError) -> bool {
    // a server fault or throttle is never a request-size problem
    if let Some(status) = err.provider_response_status()
        && (status.is_server_error() || status.as_u16() == 429)
    {
        return false;
    }
    // look for the shapes providers use for an oversized request
    let text = err.to_string().to_lowercase();
    [
        "context length",
        "context_length",
        "maximum context",
        "prompt is too long",
        "input is too large",
        "exceeds the maximum",
        "too many tokens",
        "request too large",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// The marker prefix identifying a shutdown-time interruption error.
///
/// Stamped onto the error a request surfaces when the gate's shutdown latch
/// is set (see [`super::gate::PauseController::abort_for_shutdown`]), so every
/// death path downstream can tell "the process is going down" apart from a
/// genuine failure: interrupted state is frozen for resume (no salvage posts,
/// no failure events, the pending turn stays pending) instead of settled as a
/// death.
pub(crate) const SHUTDOWN_INTERRUPT_MARKER: &str = "[thorpal:shutdown-interrupt]";

/// Whether a completion error is a shutdown-time interruption.
///
/// Marker-based so a genuine failure that races the shutdown (a budget death,
/// a fatal 400) is never misclassified as an interruption.
///
/// # Arguments
///
/// * `err` - The completion error to classify
pub(crate) fn is_shutdown_interrupt_completion(err: &CompletionError) -> bool {
    // only the gate's own abort arms mint marker-prefixed provider errors
    matches!(err, CompletionError::ProviderError(text) if text.starts_with(SHUTDOWN_INTERRUPT_MARKER))
}

/// Whether a prompt error carries a shutdown-time interruption.
///
/// Only the completion arm can carry one: the marker is minted by the gate's
/// abort arms, and budget/cancellation/unknown-tool errors never park there.
///
/// # Arguments
///
/// * `err` - The prompt error to classify
pub(crate) fn is_shutdown_interrupt(err: &rig::completion::PromptError) -> bool {
    // unwrap the completion error and check it for the marker
    matches!(err, rig::completion::PromptError::CompletionError(inner) if is_shutdown_interrupt_completion(inner))
}

/// The provider-mandated `Retry-After` wait, when the error captured one.
///
/// # Arguments
///
/// * `err` - The completion error to read rate-limit metadata from
fn retry_after(err: &CompletionError) -> Option<Duration> {
    // read the integer-seconds form of the header; the HTTP-date form is rare
    // enough on rate limits that it just falls back to pure backoff
    let seconds = err
        .provider_response_headers()?
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    // clamp so a hostile or buggy header can never stall a run for minutes
    Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER))
}

/// A short single-line summary of a completion error for a status line.
///
/// # Arguments
///
/// * `err` - The completion error to summarize
pub(crate) fn error_summary(err: &CompletionError) -> String {
    // the status line names the fault precisely (e.g. "429 Too Many Requests")
    if let Some(status) = err.provider_response_status() {
        return status.to_string();
    }
    // otherwise flatten the error text onto one line and keep it short
    let text = err.to_string().replace('\n', " ");
    if text.chars().count() > 80 {
        let truncated: String = text.chars().take(77).collect();
        format!("{truncated}...")
    } else {
        text
    }
}

/// The provider request id a completion error preserved, when one exists.
///
/// Rig captures the id off the provider's response headers onto
/// `ProviderResponse` errors; everything else (transport faults, parse
/// errors) has none. Cloned so events can own it beyond the error's lifetime.
///
/// # Arguments
///
/// * `err` - The completion error to read the request id from
pub(crate) fn request_id(err: &CompletionError) -> Option<String> {
    // only provider-response errors carry a transport request id
    err.provider_request_id().map(str::to_owned)
}

/// The delay before the next attempt.
///
/// Kept pure (the jitter factor is an argument) so the pacing policy is unit
/// testable without sleeping or seeding a generator.
///
/// # Arguments
///
/// * `attempt` - The 1-based attempt that just failed
/// * `retry_after` - The provider-mandated wait, when one was captured
/// * `jitter` - The jitter factor drawn from `0.5..=1.0`
fn backoff_delay(attempt: u32, retry_after: Option<Duration>, jitter: f64) -> Duration {
    // grow the delay exponentially with the attempt, capped so late attempts
    // stay bounded
    let exponential =
        BASE_DELAY.as_secs_f64() * BACKOFF_FACTOR.powi(attempt.saturating_sub(1) as i32);
    let capped = exponential.min(MAX_DELAY.as_secs_f64());
    // jitter spreads concurrent agents out so a swarm that was throttled
    // together does not re-throttle itself in lockstep
    let jittered = Duration::from_secs_f64(capped * jitter);
    // a provider-mandated wait wins when it asks for more than the backoff
    match retry_after {
        Some(wait) => jittered.max(wait),
        None => jittered,
    }
}

/// A model handle cached against the gate epoch it was built from.
struct CachedHandle {
    /// The gate client epoch this handle was built from.
    epoch: u64,
    /// The erased provider model built from that client.
    handle: ModelHandle,
}

/// A provider-agnostic completion model that retries transient faults.
///
/// Wraps any provider's model behind a type-erased [`ModelHandle`] and retries
/// each completion request with jittered exponential backoff, emitting
/// [`ThorPalEvent::LlmRetrying`]/[`ThorPalEvent::LlmRetryResolved`] so a
/// consumer can render one live status line per retry episode. A fatal error,
/// or a transient one that spends the whole budget, is returned unmodified so
/// every downstream failure path behaves exactly as before -- unless a pause
/// gate is installed, in which case auth faults and spent budgets park the
/// request on the session-wide [`ProviderGate`] instead of surfacing, and the
/// model handle is rebuilt lazily whenever the gate swaps clients after a
/// credential refresh.
#[derive(Clone)]
pub(crate) struct RetryingModel {
    /// The display name of the agent this model serves (e.g. `"main"`)
    agent: String,
    /// The optional sink retry progress is reported on
    events: Option<ThorPalEventSender>,
    /// A counter minting a unique id per retry episode, shared across clones
    /// so concurrent swarm members of the same role stay distinguishable
    episodes: Arc<AtomicU64>,
    /// The session-wide pause gate, or `None` to keep the fail-fast behavior
    gate: Option<Arc<ProviderGate>>,
    /// The model id, for rebuilding the handle after a client swap
    model: String,
    /// How a content-filter 403 is answered: climb the scrub ladder (the
    /// default) or surface the classified error to a caller with its own plan
    scrub: ScrubPolicy,
    /// The cached (epoch, handle) pair, seeded at epoch 0 with the handle the
    /// model was built from and shared so one rebuild serves every clone
    handle: Arc<std::sync::RwLock<CachedHandle>>,
}

impl RetryingModel {
    /// Creates a new retrying model around an erased provider model
    ///
    /// # Arguments
    ///
    /// * `inner` - The provider model requests are forwarded to, seeding the
    ///   epoch-0 handle cache
    /// * `agent` - The display name of the agent this model serves
    /// * `events` - The optional sink to report retry progress on
    /// * `gate` - The session-wide pause gate, or `None` to fail fast
    /// * `model` - The model id, for rebuilds after a client swap
    pub fn new(
        inner: ModelHandle,
        agent: impl Into<String>,
        events: Option<ThorPalEventSender>,
        gate: Option<Arc<ProviderGate>>,
        model: impl Into<String>,
    ) -> Self {
        RetryingModel {
            // seed the epoch-0 cache with the built handle; gateless models
            // serve it forever, gated ones swap it when the epoch moves
            handle: Arc::new(std::sync::RwLock::new(CachedHandle {
                epoch: 0,
                handle: inner,
            })),
            agent: agent.into(),
            events,
            episodes: Arc::new(AtomicU64::new(0)),
            gate,
            model: model.into(),
            scrub: ScrubPolicy::default(),
        }
    }

    /// Sets the content-filter scrub policy for every request this model sends
    ///
    /// # Arguments
    ///
    /// * `scrub` - The policy to answer content-filter 403s with
    pub fn with_scrub_policy(mut self, scrub: ScrubPolicy) -> Self {
        self.scrub = scrub;
        self
    }

    /// The currently cached model handle, regardless of epoch freshness
    fn cached_handle(&self) -> ModelHandle {
        // the cache always holds a servable handle (seeded at build time)
        self.handle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .handle
            .clone()
    }

    /// The freshest model handle, rebuilt when the gate swapped clients
    ///
    /// Gateless models always use the epoch-0 handle they were seeded with.
    /// Gated ones compare their cached epoch against the gate's: a mismatch
    /// (a credential refresh connected a replacement client) rebuilds the
    /// erased model from the new client and publishes it for every clone.
    fn refreshed_handle(&self) -> ModelHandle {
        // without a gate there is nothing to swap
        let Some(gate) = &self.gate else {
            return self.cached_handle();
        };
        let (epoch, client) = gate.client();
        {
            // fast path: the cached handle still matches the gate's epoch
            let cached = self
                .handle
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if cached.epoch == epoch {
                return cached.handle.clone();
            }
        }
        // rebuild from the swapped client and cache it for every clone
        let handle = client.erased_model(&self.model);
        let mut cached = self
            .handle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cached = CachedHandle {
            epoch,
            handle: handle.clone(),
        };
        handle
    }

    /// Send an event to the consumer's sink if one is installed
    ///
    /// # Arguments
    ///
    /// * `event` - The event to send
    fn emit(&self, event: ThorPalEvent) {
        // a dropped or absent consumer is a no-op, never an error
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    /// Drive a request through the retry loop until it succeeds, turns out to
    /// be fatal, spends the attempt budget, or (with a gate installed) parks
    /// on a session-wide pause and later resumes
    ///
    /// # Arguments
    ///
    /// * `request` - The completion request to send (cloned per attempt)
    /// * `send` - The underlying model call to retry, taking the fresh handle
    #[instrument(name = "RetryingModel::with_retries", skip_all, fields(agent = %self.agent), err(Debug))]
    async fn with_retries<T, F, Fut>(
        &self,
        request: CompletionRequest,
        send: F,
    ) -> Result<T, CompletionError>
    where
        F: Fn(ModelHandle, CompletionRequest) -> Fut,
        Fut: Future<Output = Result<T, CompletionError>>,
    {
        // the episode id is minted lazily on the first failure, so a request
        // that succeeds immediately costs nothing and emits nothing
        let mut retry_id: Option<String> = None;
        // try the request until it succeeds or the budget is spent
        let mut attempt = 1u32;
        // the probe token this request holds while it is a pause's elected
        // prober; a decisive outcome reports through it
        let mut token: Option<ProbeToken> = None;
        // the scrub level a content-filter kill escalated this request to;
        // None sends the pristine request
        let mut scrub_level: Option<ScrubLevel> = None;
        loop {
            // park while the session is paused; the elected prober (and every
            // gateless request) passes straight through
            if let Some(gate) = &self.gate {
                match gate.admit(&token).await {
                    // this request was elected to probe the provider
                    GatePass::Proceed(Some(elected)) => token = Some(elected),
                    // requests are flowing (or this request holds the token)
                    GatePass::Proceed(None) => (),
                    // the operator aborted a pause this request parked under
                    // before its first attempt ever ran
                    GatePass::Abort => {
                        // a shutdown-time abort is an interruption, not a
                        // failure: mark it so death paths freeze for resume
                        if gate.is_shutdown() {
                            return Err(CompletionError::ProviderError(format!(
                                "{SHUTDOWN_INTERRUPT_MARKER} the session is shutting down \
                                 before this request could run"
                            )));
                        }
                        return Err(CompletionError::ProviderError(
                            "the provider pause was aborted by the operator before \
                             this request could run"
                                .to_owned(),
                        ));
                    }
                }
            }
            // pick up the freshest handle (a credential refresh swaps clients)
            let handle = self.refreshed_handle();
            // a content-filter kill escalates this request to a scrubbed copy;
            // the pristine request survives untouched for unscrubbed attempts
            let attempt_request = match scrub_level {
                None => request.clone(),
                Some(level) => scrub::scrub_request(&request, level),
            };
            match send(handle, attempt_request).await {
                Ok(response) => {
                    // a success resumes any pause this request probed: a 2xx
                    // proves both the provider and the credentials work
                    if let Some(gate) = &self.gate {
                        gate.report_success(token.take());
                    }
                    // report a recovered episode so the consumer clears its line
                    if let Some(retry_id) = retry_id {
                        self.emit(ThorPalEvent::LlmRetryResolved {
                            agent: self.agent.clone(),
                            retry_id,
                            attempts: attempt,
                            succeeded: true,
                        });
                    }
                    return Ok(response);
                }
                Err(err) => {
                    // a content-filter kill is answered on the spot: a pause
                    // probe re-sends the same payload and can never clear it
                    if is_content_filter_error(&err) {
                        match self.scrub {
                            // the caller owns its own recovery ladder; close
                            // the episode and surface the classified error
                            ScrubPolicy::Surface => {
                                // a decisive answer resumes an outage pause
                                // this request probed, but never an auth pause
                                if let Some(gate) = &self.gate {
                                    gate.report_decisive_failure(token.take());
                                }
                                if let Some(retry_id) = retry_id {
                                    self.emit(ThorPalEvent::LlmRetryResolved {
                                        agent: self.agent.clone(),
                                        retry_id,
                                        attempts: attempt,
                                        succeeded: false,
                                    });
                                }
                                return Err(err);
                            }
                            // climb the scrub ladder one level per kill
                            ScrubPolicy::Ladder => {
                                let next = match scrub_level {
                                    None => Some(ScrubLevel::Defang),
                                    Some(ScrubLevel::Defang) => Some(ScrubLevel::Deep),
                                    // the deep level already failed: fall
                                    // through to the fatal path below
                                    Some(ScrubLevel::Deep) => None,
                                };
                                if let Some(level) = next {
                                    scrub_level = Some(level);
                                    // mint/reuse the episode id so the scrub
                                    // joins this request's retry record
                                    let episode = retry_id
                                        .get_or_insert_with(|| {
                                            let n = self.episodes.fetch_add(1, Ordering::Relaxed);
                                            format!("{}#{n}", self.agent)
                                        })
                                        .clone();
                                    // one warning per scrub for the file log
                                    tracing::warn!(
                                        agent = %self.agent,
                                        level = level.as_u8(),
                                        error = %err,
                                        "LLM request was killed by a content filter; \
                                         retrying with scrubbed context"
                                    );
                                    // report the scrub so consumers and the
                                    // durable transcript can count this class
                                    self.emit(ThorPalEvent::LlmScrubbed {
                                        agent: self.agent.clone(),
                                        retry_id: episode,
                                        level: level.as_u8(),
                                        error: error_summary(&err),
                                        request_id: request_id(&err),
                                    });
                                    // no backoff: the block is deterministic
                                    // per payload, not a pacing fault
                                    continue;
                                }
                            }
                        }
                    }
                    // classify a pause-worthy fault when a gate is installed:
                    // an auth fault pauses on any attempt (retrying cannot fix
                    // a credential), a transient fault only once the whole
                    // backoff budget is spent; a content-filter 403 is carved
                    // out of the auth arm -- a credential refresh cannot clear
                    // it, so an exhausted scrub ladder must surface instead of
                    // probing the blocked payload forever
                    let pause_kind = match &self.gate {
                        Some(_) if is_auth_error(&err) && !is_content_filter_error(&err) => {
                            Some(PauseKind::Auth)
                        }
                        Some(_) if is_retryable(&err) && attempt >= MAX_ATTEMPTS => {
                            Some(PauseKind::RetryBudgetExhausted)
                        }
                        _ => None,
                    };
                    if let (Some(gate), Some(kind)) = (&self.gate, pause_kind) {
                        // close any live retry episode so its status line
                        // clears before the pause banner takes over
                        if let Some(retry_id) = retry_id.take() {
                            self.emit(ThorPalEvent::LlmRetryResolved {
                                agent: self.agent.clone(),
                                retry_id,
                                attempts: attempt,
                                succeeded: false,
                            });
                        }
                        // park on the session-wide gate until a probe (maybe
                        // this request's own) recovers or the operator acts
                        match gate
                            .pause_and_park(
                                kind,
                                &self.agent,
                                error_summary(&err),
                                request_id(&err),
                                token.take(),
                            )
                            .await
                        {
                            // resumed: start over with a fresh attempt budget
                            GatePass::Proceed(elected) => {
                                token = elected;
                                attempt = 1;
                                continue;
                            }
                            // aborted: surface this request's own error so
                            // every downstream failure path behaves as before
                            // -- unless the session is shutting down, which
                            // marks the error as an interruption to freeze
                            // state for resume instead of recording a failure
                            GatePass::Abort => {
                                if gate.is_shutdown() {
                                    return Err(CompletionError::ProviderError(format!(
                                        "{SHUTDOWN_INTERRUPT_MARKER} the session is \
                                         shutting down ({})",
                                        error_summary(&err)
                                    )));
                                }
                                return Err(err);
                            }
                        }
                    }
                    // a fatal fault or a spent budget ends the episode with the
                    // original error, so downstream handling is unchanged
                    if !is_retryable(&err) || attempt >= MAX_ATTEMPTS {
                        // a decisive (non-pause) failure resumes an outage
                        // pause this request probed -- the provider answered
                        // again -- but never an auth pause, which only a
                        // success or the operator can end
                        if let Some(gate) = &self.gate {
                            gate.report_decisive_failure(token.take());
                        }
                        if let Some(retry_id) = retry_id {
                            self.emit(ThorPalEvent::LlmRetryResolved {
                                agent: self.agent.clone(),
                                retry_id,
                                attempts: attempt,
                                succeeded: false,
                            });
                        }
                        return Err(err);
                    }
                    // mint this episode's id on its first failure
                    let episode = retry_id
                        .get_or_insert_with(|| {
                            // the counter is shared across clones, so ids stay
                            // unique even for same-role swarm members
                            let n = self.episodes.fetch_add(1, Ordering::Relaxed);
                            format!("{}#{n}", self.agent)
                        })
                        .clone();
                    // pace the next attempt with jittered exponential backoff
                    let jitter = rand::rng().random_range(0.5..=1.0);
                    let delay = backoff_delay(attempt, retry_after(&err), jitter);
                    // one warning per attempt is the file log's whole record
                    tracing::warn!(
                        agent = %self.agent,
                        attempt,
                        max_attempts = MAX_ATTEMPTS,
                        delay_ms = delay.as_millis() as u64,
                        error = %err,
                        "LLM request failed with a transient fault; backing off"
                    );
                    // report the failed attempt so the consumer's status line
                    // can show the counter, elapsed time, and countdown
                    let now = Utc::now();
                    self.emit(ThorPalEvent::LlmRetrying {
                        agent: self.agent.clone(),
                        retry_id: episode,
                        attempt,
                        max_attempts: MAX_ATTEMPTS,
                        error: error_summary(&err),
                        request_id: request_id(&err),
                        last_attempt_at: now,
                        next_attempt_at: now
                            + chrono::Duration::from_std(delay)
                                .unwrap_or_else(|_| chrono::Duration::seconds(0)),
                    });
                    // wait out the backoff before the next attempt
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }
}

impl CompletionModel for RetryingModel {
    /// Generate a completion response, retrying transient faults with backoff
    ///
    /// # Arguments
    ///
    /// * `request` - The completion request to send
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, CompletionError> {
        // forward each attempt to the freshest provider model
        self.with_retries(request, |handle, request| async move {
            handle.completion(request).await
        })
        .await
    }

    /// Stream a completion response, retrying transient faults with backoff
    ///
    /// Only stream *establishment* is retried: an error after the stream has
    /// started flowing surfaces to the caller unretried. ThorPal only uses
    /// the blocking prompt path today, so this exists for completeness.
    ///
    /// # Arguments
    ///
    /// * `request` - The completion request to stream
    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<StreamingCompletionResponse, CompletionError> {
        // forward each attempt to the freshest provider model
        self.with_retries(request, |handle, request| async move {
            handle.stream(request).await
        })
        .await
    }

    /// The wrapped provider model's capabilities, unchanged
    fn capabilities(&self) -> ProviderCapabilities {
        // retrying changes pacing, never what the provider supports; the
        // cached handle (seeded at build, swapped on refresh) answers
        self.cached_handle().capabilities()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::ProviderResponseError;
    use rig::completion::{AssistantContent, CompletionRequestBuilder, Usage};
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// A scripted completion model that pops one pre-staged result per request
    #[derive(Clone)]
    struct ScriptedModel {
        /// The results handed out in order, one per attempt
        script: Arc<Mutex<VecDeque<Result<CompletionResponse, CompletionError>>>>,
        /// Every request the model observed, in order, for scrub assertions
        seen: Arc<Mutex<Vec<CompletionRequest>>>,
    }

    impl ScriptedModel {
        /// Creates a scripted model from a list of per-attempt results
        ///
        /// # Arguments
        ///
        /// * `results` - The result each successive attempt observes
        fn new(results: Vec<Result<CompletionResponse, CompletionError>>) -> Self {
            ScriptedModel {
                script: Arc::new(Mutex::new(results.into_iter().collect())),
                seen: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Render the nth observed request's history for content assertions
        ///
        /// # Arguments
        ///
        /// * `index` - The 0-based attempt whose request to render
        fn seen_history(&self, index: usize) -> String {
            // a debug render is enough to assert text presence/absence
            format!("{:?}", self.seen.lock().unwrap()[index].chat_history)
        }
    }

    impl CompletionModel for ScriptedModel {
        /// Pop the next scripted result; running out of script is a test bug
        async fn completion(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, CompletionError> {
            // record what was actually sent so scrubs are observable
            self.seen.lock().unwrap().push(request);
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .expect("the scripted model ran out of staged results")
        }

        /// The tests only exercise the blocking path
        async fn stream(
            &self,
            _request: CompletionRequest,
        ) -> Result<StreamingCompletionResponse, CompletionError> {
            unimplemented!("the retry tests only exercise the blocking path")
        }
    }

    /// A minimal successful completion response
    fn ok_response() -> CompletionResponse {
        CompletionResponse::new(vec![AssistantContent::text("ok")], Usage::default(), "test")
    }

    /// A completion error preserving the given HTTP status
    ///
    /// # Arguments
    ///
    /// * `status` - The status the fake provider answered with
    fn status_error(status: http::StatusCode) -> CompletionError {
        CompletionError::ProviderResponse(ProviderResponseError::new(status, "nope"))
    }

    /// A content-filter 403 with the observed HTML block-page shape
    fn content_filter_error() -> CompletionError {
        CompletionError::ProviderResponse(ProviderResponseError::new(
            http::StatusCode::FORBIDDEN,
            "<!--# 7Pt2nHS3FfW3NNVFPvHRDjrj4k-->\n<!DOCTYPE HTML PUBLIC \
             \"-//W3C//DTD HTML 4.01 Transitional//EN\"><html><head>\
             <title>Zscaler</title></head><body>blocked</body></html>",
        ))
    }

    /// A genuine credential 403 with the provider JSON envelope shape
    fn auth_json_error() -> CompletionError {
        CompletionError::ProviderResponse(ProviderResponseError::new(
            http::StatusCode::FORBIDDEN,
            "{\"message\":\"The security token included in the request is expired\"}",
        ))
    }

    /// Build a retrying model, its event receiver, and a request to send
    ///
    /// # Arguments
    ///
    /// * `results` - The result each successive attempt observes
    fn harness(
        results: Vec<Result<CompletionResponse, CompletionError>>,
    ) -> (
        RetryingModel,
        UnboundedReceiver<ThorPalEvent>,
        CompletionRequest,
    ) {
        // stage the fake provider and derive a request from it
        let fake = ScriptedModel::new(results);
        let request = CompletionRequestBuilder::new(fake.clone(), "hi").build();
        // tee the retry events into an inspectable channel
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let model = RetryingModel::new(ModelHandle::new(fake), "main", Some(tx), None, "test");
        (model, rx, request)
    }

    /// Transient faults recover: two throttles then a success returns the
    /// response and reports each attempt plus a succeeded resolution
    #[tokio::test(start_paused = true)]
    async fn transient_faults_recover_and_report_each_attempt() {
        let (model, mut rx, request) = harness(vec![
            Err(status_error(http::StatusCode::TOO_MANY_REQUESTS)),
            Err(status_error(http::StatusCode::INTERNAL_SERVER_ERROR)),
            Ok(ok_response()),
        ]);
        // the paused clock fast-forwards through both backoff sleeps
        let response = model.completion(request).await.expect("should recover");
        assert_eq!(response.choice, vec![AssistantContent::text("ok")]);
        // both failed attempts reported under one episode id, then a success
        let mut episode = None;
        for expected_attempt in 1..=2 {
            match rx.try_recv().expect("a retrying event per failed attempt") {
                ThorPalEvent::LlmRetrying {
                    agent,
                    retry_id,
                    attempt,
                    max_attempts,
                    ..
                } => {
                    assert_eq!(agent, "main");
                    assert_eq!(attempt, expected_attempt);
                    assert_eq!(max_attempts, MAX_ATTEMPTS);
                    // every attempt of one episode shares the same id
                    assert_eq!(*episode.get_or_insert(retry_id.clone()), retry_id);
                }
                other => panic!("expected LlmRetrying, got {other:?}"),
            }
        }
        match rx.try_recv().expect("a resolution event") {
            ThorPalEvent::LlmRetryResolved {
                retry_id,
                attempts,
                succeeded,
                ..
            } => {
                assert_eq!(Some(retry_id), episode);
                assert_eq!(attempts, 3);
                assert!(succeeded);
            }
            other => panic!("expected LlmRetryResolved, got {other:?}"),
        }
        // nothing else rides the channel
        assert!(rx.try_recv().is_err());
    }

    /// A fault that preserved a provider request id reports it on its event
    #[tokio::test(start_paused = true)]
    async fn retrying_events_carry_the_provider_request_id() {
        // one throttle carrying an id, one bare, then a success
        let with_id = CompletionError::ProviderResponse(
            ProviderResponseError::new(http::StatusCode::TOO_MANY_REQUESTS, "nope")
                .with_provider_request_id(Some("req-test".to_owned())),
        );
        let (model, mut rx, request) = harness(vec![
            Err(with_id),
            Err(status_error(http::StatusCode::TOO_MANY_REQUESTS)),
            Ok(ok_response()),
        ]);
        // the paused clock fast-forwards through both backoff sleeps
        model.completion(request).await.expect("should recover");
        // the id-bearing attempt reports its id; the bare one reports none
        for expected in [Some("req-test".to_owned()), None] {
            match rx.try_recv().expect("a retrying event per failed attempt") {
                ThorPalEvent::LlmRetrying {
                    error, request_id, ..
                } => {
                    // the summary keeps its stable status-line shape
                    assert_eq!(error, "429 Too Many Requests");
                    assert_eq!(request_id, expected);
                }
                other => panic!("expected LlmRetrying, got {other:?}"),
            }
        }
    }

    /// A fatal fault fails immediately, unretried and with no events
    #[tokio::test(start_paused = true)]
    async fn fatal_faults_fail_immediately_with_no_events() {
        let (model, mut rx, request) =
            harness(vec![Err(status_error(http::StatusCode::BAD_REQUEST))]);
        let err = model.completion(request).await.expect_err("should fail");
        // the original error surfaces unmodified
        assert_eq!(
            err.provider_response_status(),
            Some(http::StatusCode::BAD_REQUEST)
        );
        // an immediate failure emits nothing to clear
        assert!(rx.try_recv().is_err());
    }

    /// A spent budget surfaces the original error and a failed resolution
    #[tokio::test(start_paused = true)]
    async fn exhaustion_returns_the_original_error() {
        // every attempt in the budget observes a throttle
        let script = (0..MAX_ATTEMPTS)
            .map(|_| Err(status_error(http::StatusCode::TOO_MANY_REQUESTS)))
            .collect();
        let (model, mut rx, request) = harness(script);
        let err = model.completion(request).await.expect_err("should exhaust");
        assert_eq!(
            err.provider_response_status(),
            Some(http::StatusCode::TOO_MANY_REQUESTS)
        );
        // one retrying event per failed attempt except the last, then a failed
        // resolution for the whole episode
        let mut retrying = 0;
        let mut resolved = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                ThorPalEvent::LlmRetrying { .. } => retrying += 1,
                ThorPalEvent::LlmRetryResolved {
                    attempts,
                    succeeded,
                    ..
                } => resolved = Some((attempts, succeeded)),
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(retrying, MAX_ATTEMPTS - 1);
        assert_eq!(resolved, Some((MAX_ATTEMPTS, false)));
    }

    /// The classifier retries pacing and server faults and nothing else
    #[test]
    fn classification_is_status_based() {
        // pacing and server faults are retryable
        for status in [408u16, 429, 500, 502, 503, 504, 529] {
            let status = http::StatusCode::from_u16(status).unwrap();
            assert!(is_retryable(&status_error(status)), "{status} must retry");
        }
        // caller faults are fatal
        for status in [400u16, 401, 403, 404, 422] {
            let status = http::StatusCode::from_u16(status).unwrap();
            assert!(!is_retryable(&status_error(status)), "{status} is fatal");
        }
        // a status-less provider body (e.g. an unnormalized SDK transport) and
        // provider prose are fatal: there is no signal they are temporary
        assert!(!is_retryable(&CompletionError::ProviderResponse(
            ProviderResponseError::without_status("opaque"),
        )));
        assert!(!is_retryable(&CompletionError::ProviderError(
            "rig prose".to_string(),
        )));
        // malformed provider output and transport failures stay retryable
        let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        assert!(is_retryable(&CompletionError::JsonError(json_err)));
        assert!(is_retryable(&CompletionError::ResponseError(
            "no choices".to_string(),
        )));
        assert!(is_retryable(&CompletionError::HttpError(
            rig::http_client::Error::Instance("connection reset".into()),
        )));
    }

    /// The pacing policy honors Retry-After, the delay cap, and the jitter
    #[test]
    fn backoff_delay_honors_retry_after_cap_and_jitter() {
        // the exponential doubles from the base without jitter
        assert_eq!(backoff_delay(1, None, 1.0), Duration::from_secs(1));
        assert_eq!(backoff_delay(3, None, 1.0), Duration::from_secs(4));
        // late attempts cap at the max delay
        assert_eq!(backoff_delay(12, None, 1.0), MAX_DELAY);
        // jitter scales the exponential down, never the provider's wait
        assert_eq!(backoff_delay(1, None, 0.5), Duration::from_millis(500));
        // a provider-mandated wait wins when it asks for more than the backoff
        assert_eq!(
            backoff_delay(1, Some(Duration::from_secs(20)), 1.0),
            Duration::from_secs(20)
        );
        // ...but a shorter mandate never shrinks the computed backoff
        assert_eq!(
            backoff_delay(6, Some(Duration::from_secs(2)), 1.0),
            Duration::from_secs(30)
        );
    }

    /// The Retry-After header is read when captured and clamped when hostile
    #[test]
    fn retry_after_reads_and_clamps_the_header() {
        /// Build a 429 carrying the given Retry-After header value
        fn throttled(value: &str) -> CompletionError {
            let mut headers = http::HeaderMap::new();
            headers.insert(http::header::RETRY_AFTER, value.parse().unwrap());
            CompletionError::ProviderResponse(
                ProviderResponseError::new(http::StatusCode::TOO_MANY_REQUESTS, "nope")
                    .with_headers(Some(Box::new(headers))),
            )
        }
        // a sane header is honored verbatim
        assert_eq!(retry_after(&throttled("20")), Some(Duration::from_secs(20)));
        // a hostile header clamps to the cap instead of stalling the run
        assert_eq!(retry_after(&throttled("9999")), Some(MAX_RETRY_AFTER));
        // an unparseable header falls back to pure backoff
        assert_eq!(retry_after(&throttled("soon")), None);
        // no captured headers also falls back to pure backoff
        assert_eq!(
            retry_after(&status_error(http::StatusCode::TOO_MANY_REQUESTS)),
            None
        );
    }

    /// Context-length classification: oversized-request shapes and nothing else
    #[test]
    fn context_length_classification() {
        // provider prose naming an oversized request is recognized
        assert!(is_context_length_error(&CompletionError::ProviderError(
            "prompt is too long: 250000 tokens > 200000 maximum".to_owned()
        )));
        assert!(is_context_length_error(&CompletionError::ProviderResponse(
            ProviderResponseError::new(
                http::StatusCode::BAD_REQUEST,
                "This model's maximum context length is 200000 tokens"
            ),
        )));
        // server faults and throttles are never request-size problems
        assert!(!is_context_length_error(&status_error(
            http::StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!is_context_length_error(
            &CompletionError::ProviderResponse(ProviderResponseError::new(
                http::StatusCode::TOO_MANY_REQUESTS,
                "too many tokens"
            ),)
        ));
        // ordinary request faults are not misclassified
        assert!(!is_context_length_error(&status_error(
            http::StatusCode::BAD_REQUEST
        )));
    }

    /// Auth classification: 401/403 and credential prose pause, the rest do not
    #[test]
    fn auth_classification_is_conservative() {
        // a preserved 401/403 is decisive
        assert!(is_auth_error(&status_error(http::StatusCode::UNAUTHORIZED)));
        assert!(is_auth_error(&status_error(http::StatusCode::FORBIDDEN)));
        // any other preserved status is decisively not auth
        assert!(!is_auth_error(&status_error(http::StatusCode::BAD_REQUEST)));
        assert!(!is_auth_error(&status_error(
            http::StatusCode::TOO_MANY_REQUESTS
        )));
        // status-less credential prose is recognized (SDK transports)
        assert!(is_auth_error(&CompletionError::ProviderError(
            "ExpiredTokenException: the security token included in the request is expired"
                .to_owned(),
        )));
        assert!(is_auth_error(&CompletionError::HttpError(
            rig::http_client::Error::Instance("failed to load credentials from the chain".into()),
        )));
        assert!(is_auth_error(&CompletionError::HttpError(
            rig::http_client::Error::Instance("the credential chain yielded no credentials".into()),
        )));
        // transient credential-chain prose is NOT auth: it stays retryable so
        // a brief chain hiccup backs off instead of pausing the session
        let chain_timeout = CompletionError::HttpError(rig::http_client::Error::Instance(
            "credentials provider timed out after 5s".into(),
        ));
        assert!(!is_auth_error(&chain_timeout));
        assert!(is_retryable(&chain_timeout));
        // ordinary prose and plain transport faults are not auth
        assert!(!is_auth_error(&CompletionError::ProviderError(
            "rig prose".to_owned()
        )));
        assert!(!is_auth_error(&CompletionError::HttpError(
            rig::http_client::Error::Instance("connection reset".into()),
        )));
        // a content-filter 403 still classifies as auth here; the retry loop's
        // ordering (content-filter checked first, and the auth arm carved out
        // with !is_content_filter_error) is what diverts it to the scrub path
        assert!(is_auth_error(&content_filter_error()));
        assert!(is_content_filter_error(&content_filter_error()));
    }

    /// Content-filter classification demands an HTML-bodied preserved 403
    #[test]
    fn content_filter_classification_is_html_bodied_403_only() {
        // the observed Zscaler shape (HTML block page behind a 403) classifies
        assert!(is_content_filter_error(&content_filter_error()));
        // a provider JSON envelope is a genuine auth fault, not a filter
        assert!(!is_content_filter_error(&auth_json_error()));
        assert!(!is_content_filter_error(
            &CompletionError::ProviderResponse(ProviderResponseError::new(
                http::StatusCode::FORBIDDEN,
                "{\"error\":{\"type\":\"permission_error\",\"message\":\"no\"}}",
            ))
        ));
        // an empty or plain-text body is ambiguous and stays auth-classified
        assert!(!is_content_filter_error(
            &CompletionError::ProviderResponse(ProviderResponseError::new(
                http::StatusCode::FORBIDDEN,
                ""
            ))
        ));
        assert!(!is_content_filter_error(&status_error(
            http::StatusCode::FORBIDDEN
        )));
        // other statuses never classify, HTML-bodied or not
        assert!(!is_content_filter_error(
            &CompletionError::ProviderResponse(ProviderResponseError::new(
                http::StatusCode::UNAUTHORIZED,
                "<html>no</html>"
            ))
        ));
        assert!(!is_content_filter_error(&status_error(
            http::StatusCode::TOO_MANY_REQUESTS
        )));
        assert!(!is_content_filter_error(&status_error(
            http::StatusCode::INTERNAL_SERVER_ERROR
        )));
        // status-less errors carry no proxy evidence
        assert!(!is_content_filter_error(&CompletionError::ProviderError(
            "<html>prose</html>".to_owned()
        )));
    }

    /// Builds a running gate around a placeholder client
    ///
    /// With no refresher installed the gate never swaps clients, so the
    /// scripted model stays the model actually called throughout a test.
    async fn test_gate(tx: Option<ThorPalEventSender>) -> Arc<crate::thorpal::gate::ProviderGate> {
        // a placeholder provider config that builds without any network
        let provider = crate::thorpal::AiProviderConfig::OpenAi {
            endpoint: None,
            api_key: "test-key".to_owned(),
            model: "test-model".to_owned(),
        };
        let client = provider
            .connect()
            .await
            .expect("the placeholder client should build");
        ProviderGate::new(Arc::new(client), provider, None, tx)
    }

    /// Build a gated retrying model, its gate, an event receiver, and a request
    async fn gated_harness(
        results: Vec<Result<CompletionResponse, CompletionError>>,
    ) -> (
        RetryingModel,
        Arc<crate::thorpal::gate::ProviderGate>,
        UnboundedReceiver<ThorPalEvent>,
        CompletionRequest,
    ) {
        // stage the fake provider and derive a request from it
        let fake = ScriptedModel::new(results);
        let request = CompletionRequestBuilder::new(fake.clone(), "hi").build();
        // tee the events into an inspectable channel shared with the gate
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = test_gate(Some(tx.clone())).await;
        let model = RetryingModel::new(
            ModelHandle::new(fake),
            "main",
            Some(tx),
            Some(Arc::clone(&gate)),
            "test-model",
        );
        (model, gate, rx, request)
    }

    /// An auth fault pauses the session; the automatic probe then recovers it
    #[tokio::test(start_paused = true)]
    async fn auth_fault_pauses_then_probe_recovers() {
        let (model, gate, mut rx, request) = gated_harness(vec![
            Err(status_error(http::StatusCode::UNAUTHORIZED)),
            Ok(ok_response()),
        ])
        .await;
        // watch the pause status like a consumer would
        let mut status = gate.controller().status();
        // drive the request on a task so the pause can be observed live
        let worker = tokio::spawn(async move { model.completion(request).await });
        // the status flips to paused before the request parks
        status.changed().await.expect("a pause status change");
        assert!(matches!(
            *status.borrow(),
            crate::thorpal::gate::PauseStatus::Paused {
                kind: PauseKind::Auth,
                ..
            }
        ));
        // the paused clock fast-forwards to the first probe, which succeeds
        let response = worker
            .await
            .expect("the request task should join")
            .expect("the probe should recover the request");
        assert_eq!(response.choice, vec![AssistantContent::text("ok")]);
        // the pause round trip is announced on the event stream
        let mut saw_paused = false;
        let mut saw_resumed = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                ThorPalEvent::SessionPaused {
                    kind: PauseKind::Auth,
                    ..
                } => saw_paused = true,
                ThorPalEvent::SessionResumed {
                    recovered: true,
                    probe_attempts,
                    ..
                } => {
                    // the first probe succeeded, so none failed on the way out
                    assert_eq!(probe_attempts, 0, "no probe failed before recovery");
                    saw_resumed = true;
                }
                _ => (),
            }
        }
        assert!(saw_paused, "the pause must be announced");
        assert!(saw_resumed, "the recovery must be announced");
    }

    /// A failed probe's tally rides the re-pause announcements and lands on
    /// the final resume event
    #[tokio::test(start_paused = true)]
    async fn failed_probe_tally_lands_on_the_resume() {
        // the request 401s (pause), the first probe 401s again (one failed
        // probe), and the second probe recovers
        let (model, _gate, mut rx, request) = gated_harness(vec![
            Err(status_error(http::StatusCode::UNAUTHORIZED)),
            Err(status_error(http::StatusCode::UNAUTHORIZED)),
            Ok(ok_response()),
        ])
        .await;
        // the paused clock rides out the pause and both probes
        let response = model.completion(request).await.expect("should recover");
        assert_eq!(response.choice, vec![AssistantContent::text("ok")]);
        // the initial publish counts zero failed probes, the refresh counts
        // one, and the resume carries the final tally
        let mut publishes = Vec::new();
        let mut resume_tally = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                ThorPalEvent::SessionPaused { probe_attempts, .. } => {
                    publishes.push(probe_attempts);
                }
                ThorPalEvent::SessionResumed {
                    recovered: true,
                    probe_attempts,
                    ..
                } => resume_tally = Some(probe_attempts),
                _ => (),
            }
        }
        assert_eq!(publishes, vec![0, 1], "one initial publish, one refresh");
        assert_eq!(
            resume_tally,
            Some(1),
            "the failed probe lands on the resume"
        );
    }

    /// A spent transient budget pauses instead of surfacing the error
    #[tokio::test(start_paused = true)]
    async fn exhausted_budget_pauses_then_probe_recovers() {
        // every budgeted attempt throttles; the probe after the pause succeeds
        let mut script: Vec<Result<CompletionResponse, CompletionError>> = (0..MAX_ATTEMPTS)
            .map(|_| Err(status_error(http::StatusCode::TOO_MANY_REQUESTS)))
            .collect();
        script.push(Ok(ok_response()));
        let (model, _gate, mut rx, request) = gated_harness(script).await;
        // the paused clock rides out the backoffs, the pause, and the probe
        let response = model.completion(request).await.expect("should recover");
        assert_eq!(response.choice, vec![AssistantContent::text("ok")]);
        // the exhaustion pause (not an error) is what the events announce
        let mut saw_paused = false;
        while let Ok(event) = rx.try_recv() {
            if let ThorPalEvent::SessionPaused {
                kind: PauseKind::RetryBudgetExhausted,
                ..
            } = event
            {
                saw_paused = true;
            }
        }
        assert!(saw_paused, "the exhaustion pause must be announced");
    }

    /// Aborting a pause surfaces the parked request's own error
    #[tokio::test(start_paused = true)]
    async fn abort_surfaces_the_original_error() {
        let (model, gate, mut rx, request) =
            gated_harness(vec![Err(status_error(http::StatusCode::UNAUTHORIZED))]).await;
        let controller = gate.controller();
        let mut status = controller.status();
        // drive the request on a task so the pause can be aborted mid-park
        let worker = tokio::spawn(async move { model.completion(request).await });
        // wait for the pause to land, then abort it
        status.changed().await.expect("a pause status change");
        controller.abort();
        // the parked request surfaces its own original error
        let err = worker
            .await
            .expect("the request task should join")
            .expect_err("the aborted request should fail");
        assert_eq!(
            err.provider_response_status(),
            Some(http::StatusCode::UNAUTHORIZED)
        );
        // the abort is announced as a non-recovery resume with the (empty)
        // probe tally of a pause ended before any probe fired
        let mut saw_abort = false;
        while let Ok(event) = rx.try_recv() {
            if let ThorPalEvent::SessionResumed {
                recovered: false,
                probe_attempts,
                ..
            } = event
            {
                assert_eq!(probe_attempts, 0, "no probe ran before the abort");
                saw_abort = true;
            }
        }
        assert!(saw_abort, "the abort must be announced");
    }

    /// A decisive non-auth failure during an auth pause surfaces its own
    /// error but leaves the session paused; a later probe success resumes it
    #[tokio::test(start_paused = true)]
    async fn decisive_failure_during_auth_pause_keeps_the_pause() {
        // the first request hits a 401 (pause) and its probe then hits a 400
        // (decisive); a second request's probe later gets the recovery
        let (model, gate, mut rx, request) = gated_harness(vec![
            Err(status_error(http::StatusCode::UNAUTHORIZED)),
            Err(status_error(http::StatusCode::BAD_REQUEST)),
            Ok(ok_response()),
        ])
        .await;
        let mut status = gate.controller().status();
        // keep a clone and a request for the second, recovering request
        let second_model = model.clone();
        let second_request = request.clone();
        // drive the first request to the pause, through its probe, and out
        let worker = tokio::spawn(async move { model.completion(request).await });
        status.changed().await.expect("a pause status change");
        // the paused clock advances to the probe; the 400 is decisive for
        // the request itself, so its own error surfaces
        let err = worker
            .await
            .expect("the request task should join")
            .expect_err("the decisive failure should surface");
        assert_eq!(
            err.provider_response_status(),
            Some(http::StatusCode::BAD_REQUEST)
        );
        // the auth pause survives: a non-auth failure proves nothing about
        // the credentials, so the gate must still be paused
        assert!(matches!(
            *status.borrow(),
            crate::thorpal::gate::PauseStatus::Paused {
                kind: PauseKind::Auth,
                ..
            }
        ));
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, ThorPalEvent::SessionResumed { .. }),
                "a decisive non-auth failure must not resume an auth pause"
            );
        }
        // a second request parks, is elected at the already-elapsed tick
        // (the failed probe's claim was released), and its success resumes
        let response = second_model
            .completion(second_request)
            .await
            .expect("the probe should recover the second request");
        assert_eq!(response.choice, vec![AssistantContent::text("ok")]);
        assert!(matches!(
            *status.borrow(),
            crate::thorpal::gate::PauseStatus::Running
        ));
        // the recovery is announced on the event stream
        let mut saw_resumed = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                ThorPalEvent::SessionResumed {
                    recovered: true,
                    ..
                }
            ) {
                saw_resumed = true;
            }
        }
        assert!(saw_resumed, "the probe recovery must be announced");
    }

    /// Builds a gated model plus the scripted fake itself for scrub assertions
    ///
    /// # Arguments
    ///
    /// * `results` - The result each successive attempt observes
    async fn scrub_harness(
        results: Vec<Result<CompletionResponse, CompletionError>>,
    ) -> (
        RetryingModel,
        ScriptedModel,
        Arc<crate::thorpal::gate::ProviderGate>,
        UnboundedReceiver<ThorPalEvent>,
        CompletionRequest,
    ) {
        // stage the fake provider and derive an IOC-bearing request from it
        let fake = ScriptedModel::new(results);
        let request =
            CompletionRequestBuilder::new(fake.clone(), "fetch http://evil.com now").build();
        // tee the events into an inspectable channel shared with the gate
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = test_gate(Some(tx.clone())).await;
        let model = RetryingModel::new(
            ModelHandle::new(fake.clone()),
            "main",
            Some(tx),
            Some(Arc::clone(&gate)),
            "test-model",
        );
        (model, fake, gate, rx, request)
    }

    /// A content-filter 403 retries with scrubbed context instead of pausing
    #[tokio::test(start_paused = true)]
    async fn content_filter_403_retries_with_scrubbed_context() {
        let (model, fake, gate, mut rx, request) =
            scrub_harness(vec![Err(content_filter_error()), Ok(ok_response())]).await;
        // the scrubbed retry recovers without any pause
        let response = model.completion(request).await.expect("should recover");
        assert_eq!(response.choice, vec![AssistantContent::text("ok")]);
        // the first attempt carried the pristine text, the retry the defanged form
        assert!(fake.seen_history(0).contains("http://evil.com"));
        assert!(!fake.seen_history(0).contains("hxxp"));
        assert!(fake.seen_history(1).contains("hxxp[://]evil[.]com"));
        // one level-1 scrub was announced and the episode resolved as succeeded
        let mut saw_scrub = false;
        let mut saw_resolved = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                ThorPalEvent::LlmScrubbed { level, .. } => {
                    assert_eq!(level, 1, "the first escalation is the defang level");
                    saw_scrub = true;
                }
                ThorPalEvent::LlmRetryResolved { succeeded, .. } => saw_resolved = succeeded,
                ThorPalEvent::SessionPaused { .. } => {
                    panic!("a content-filter kill must never pause the session")
                }
                _ => (),
            }
        }
        assert!(saw_scrub, "the scrub must be announced");
        assert!(saw_resolved, "the episode must resolve as succeeded");
        // the gate stayed running throughout
        assert!(matches!(
            *gate.controller().status().borrow(),
            crate::thorpal::gate::PauseStatus::Running
        ));
    }

    /// An exhausted scrub ladder surfaces a fatal error, never a pause
    #[tokio::test(start_paused = true)]
    async fn content_filter_ladder_exhausts_to_a_fatal_error() {
        let (model, fake, gate, mut rx, request) = scrub_harness(vec![
            Err(content_filter_error()),
            Err(content_filter_error()),
            Err(content_filter_error()),
        ])
        .await;
        // the exhausted ladder surfaces the classified error itself
        let err = model.completion(request).await.expect_err("should surface");
        assert_eq!(
            err.provider_response_status(),
            Some(http::StatusCode::FORBIDDEN)
        );
        // both escalations re-sent defanged payloads (level 2 includes level 1)
        assert!(fake.seen_history(1).contains("hxxp[://]evil[.]com"));
        assert!(fake.seen_history(2).contains("hxxp[://]evil[.]com"));
        // the escalations were announced in order, and nothing ever paused
        let mut levels = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                ThorPalEvent::LlmScrubbed { level, .. } => levels.push(level),
                ThorPalEvent::SessionPaused { .. } => {
                    panic!("an exhausted scrub ladder must not pause the session")
                }
                _ => (),
            }
        }
        assert_eq!(levels, vec![1, 2], "one escalation per kill");
        assert!(matches!(
            *gate.controller().status().borrow(),
            crate::thorpal::gate::PauseStatus::Running
        ));
    }

    /// The surface policy returns the classified error after one attempt
    #[tokio::test(start_paused = true)]
    async fn surface_policy_returns_the_content_filter_error_unscrubbed() {
        let (model, fake, gate, mut rx, request) =
            scrub_harness(vec![Err(content_filter_error())]).await;
        // the report path opts out: the classified error surfaces untouched
        let model = model.with_scrub_policy(ScrubPolicy::Surface);
        let err = model.completion(request).await.expect_err("should surface");
        assert_eq!(
            err.provider_response_status(),
            Some(http::StatusCode::FORBIDDEN)
        );
        // exactly one pristine attempt was sent, and no scrub or pause fired
        assert_eq!(fake.seen.lock().unwrap().len(), 1);
        assert!(fake.seen_history(0).contains("http://evil.com"));
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event,
                    ThorPalEvent::LlmScrubbed { .. } | ThorPalEvent::SessionPaused { .. }
                ),
                "surface policy must neither scrub nor pause"
            );
        }
        assert!(matches!(
            *gate.controller().status().borrow(),
            crate::thorpal::gate::PauseStatus::Running
        ));
    }

    /// A JSON-enveloped 403 is a credential fault: the pause gate still covers it
    #[tokio::test(start_paused = true)]
    async fn genuine_auth_403_still_pauses() {
        let (model, fake, gate, mut rx, request) =
            scrub_harness(vec![Err(auth_json_error()), Ok(ok_response())]).await;
        let mut status = gate.controller().status();
        // drive the request on a task so the pause can be observed live
        let worker = tokio::spawn(async move { model.completion(request).await });
        // the status flips to an auth pause before the request parks
        status.changed().await.expect("a pause status change");
        assert!(matches!(
            *status.borrow(),
            crate::thorpal::gate::PauseStatus::Paused {
                kind: PauseKind::Auth,
                ..
            }
        ));
        // the paused clock advances to the probe, whose success recovers the run
        let response = worker
            .await
            .expect("the request task should join")
            .expect("the probe should recover");
        assert_eq!(response.choice, vec![AssistantContent::text("ok")]);
        // the probe re-sent the pristine payload: no scrub events anywhere
        assert!(fake.seen_history(1).contains("http://evil.com"));
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, ThorPalEvent::LlmScrubbed { .. }),
                "a credential 403 must never trigger a scrub"
            );
        }
    }

    /// Error summaries prefer the precise status line and stay single-line
    #[test]
    fn error_summaries_are_short_and_precise() {
        // a preserved status renders as the canonical status line
        assert_eq!(
            error_summary(&status_error(http::StatusCode::TOO_MANY_REQUESTS)),
            "429 Too Many Requests"
        );
        // a status-less error flattens onto one truncated line
        let long = CompletionError::ProviderError(format!("a\nb{}", "x".repeat(200)));
        let summary = error_summary(&long);
        assert!(!summary.contains('\n'));
        assert!(summary.chars().count() <= 80);
    }

    /// Only marker-prefixed provider errors classify as shutdown interrupts
    #[test]
    fn shutdown_marker_classification_is_exact() {
        // the marker-prefixed error the gate's abort arms mint
        let marked = CompletionError::ProviderError(format!(
            "{SHUTDOWN_INTERRUPT_MARKER} the session is shutting down (401 Unauthorized)"
        ));
        assert!(is_shutdown_interrupt_completion(&marked));
        assert!(is_shutdown_interrupt(
            &rig::completion::PromptError::CompletionError(marked)
        ));
        // a genuine provider error that merely mentions shutdown never matches
        let plain = CompletionError::ProviderError(
            "the server is shutting down for maintenance".to_owned(),
        );
        assert!(!is_shutdown_interrupt_completion(&plain));
        // the marker must lead the text, not merely appear inside it
        let embedded = CompletionError::ProviderError(format!(
            "wrapped: {SHUTDOWN_INTERRUPT_MARKER} not a real interrupt"
        ));
        assert!(!is_shutdown_interrupt_completion(&embedded));
        // non-completion prompt errors never classify as interrupts
        let cancelled = rig::completion::PromptError::PromptCancelled {
            chat_history: Vec::new(),
            reason: SHUTDOWN_INTERRUPT_MARKER.to_owned(),
        };
        assert!(!is_shutdown_interrupt(&cancelled));
    }
}
