//! The session-wide pause gate every provider request passes through.
//!
//! Two faults used to end a run outright: an expired credential (401/403 --
//! fatal on the first attempt) and a transient fault that outlived the whole
//! retry budget (a gateway down for longer than ~5 minutes). Both are
//! recoverable with operator help, so instead of surfacing them the
//! [`ProviderGate`] parks every agent's next request on one shared gate,
//! announces the pause ([`ThorPalEvent::SessionPaused`]), and probes its way
//! back: the parked request that first hit the fault is re-sent on a backoff
//! schedule (refreshing credentials first for auth pauses), a consumer can
//! force an immediate probe or abort the pause through [`PauseController`],
//! and the moment one probe gets a decisive answer every parked request
//! resumes. Sub-agents and swarm members need no special handling -- their
//! requests park on the same gate, so an in-flight swarm freezes and resumes
//! coherently.
//!
//! The gate also owns the live provider client: a credential refresh connects
//! a replacement client and bumps an epoch, and every [`RetryingModel`]
//! rebuilds its erased model handle lazily when it notices the epoch moved.
//!
//! [`RetryingModel`]: super::retry::RetryingModel

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use rig::agent::AgentBuilder;
use rig::client::AgentModelExt;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::instrument;

use super::events::{ThorPalEvent, ThorPalEventSender};
use super::retry::RetryingModel;
use super::scrub::ScrubPolicy;
use super::{AiClient, AiProviderConfig};

/// The backoff delay before the first automatic probe of a pause.
const PROBE_BASE_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// The cap on the backoff delay between automatic probes.
const PROBE_MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(600);

/// The paired monotonic/wall-clock fire time for a probe `delay` from now
///
/// The tokio instant drives the actual timer (and honors a paused test
/// clock); the wall-clock twin is what events and status surfaces display.
///
/// # Arguments
///
/// * `delay` - How long from now the probe fires
fn probe_schedule(delay: std::time::Duration) -> (tokio::time::Instant, DateTime<Utc>) {
    // an unrepresentable chrono conversion falls back to the base delay
    let wall = Utc::now()
        + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::seconds(60));
    (tokio::time::Instant::now() + delay, wall)
}

/// A caller-supplied credential refresher run before each auth-pause probe.
///
/// Returns a freshly-credentialed provider config (e.g. thorctl re-reads its
/// config file and re-injects the live secrets). When none is installed the
/// gate reconnects with its current config, which still helps providers whose
/// credentials refresh out of band (the AWS credential chain).
pub type CredentialRefresher =
    Arc<dyn Fn() -> BoxFuture<'static, Result<AiProviderConfig, anyhow::Error>> + Send + Sync>;

/// Why a session paused instead of failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PauseKind {
    /// The provider rejected the request's credentials (401/403, an expired
    /// AWS security token, a missing credential chain).
    Auth,
    /// A transient fault (throttle, server fault, transport error) survived
    /// the whole per-request retry budget.
    RetryBudgetExhausted,
}

impl std::fmt::Display for PauseKind {
    /// Render the kind as the short label consumers show in banners
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // keep these short: they lead a one-line status banner
        match self {
            PauseKind::Auth => write!(f, "provider auth failed"),
            PauseKind::RetryBudgetExhausted => write!(f, "provider unreachable"),
        }
    }
}

/// The gate's consumer-facing state, published on a watch channel.
#[derive(Debug, Clone)]
pub enum PauseStatus {
    /// Requests are flowing normally.
    Running,
    /// Every provider request is parked, waiting for recovery or the operator.
    Paused {
        /// Why the session paused.
        kind: PauseKind,
        /// The display name of the agent whose request first hit the fault.
        agent: String,
        /// A short human-readable summary of the fault.
        error: String,
        /// When the pause began.
        since: DateTime<Utc>,
        /// How many automatic probes have failed since the pause began.
        probe_attempts: u32,
        /// When the next automatic probe fires.
        next_probe_at: DateTime<Utc>,
    },
}

/// The outcome of waiting at the gate.
pub(crate) enum GatePass {
    /// Proceed with the attempt; `Some` marks this waiter as the elected
    /// prober for the current probe tick.
    Proceed(Option<ProbeToken>),
    /// The operator aborted the pause: surface the request's own error.
    Abort,
}

/// The elected prober's claim on the current probe tick.
///
/// Exactly one parked request is elected per tick; while this token lives the
/// other waiters stay parked. Dropping it without a decisive outcome (the
/// probing future was cancelled) releases the claim and wakes the waiters so
/// another request can probe instead of the pause stalling forever.
pub(crate) struct ProbeToken {
    /// The gate this token claims a probe tick on.
    gate: Arc<ProviderGate>,
    /// The pause episode the claimed tick belongs to.
    episode: u64,
    /// The claimed tick, released on drop unless the token was disarmed.
    tick: u64,
    /// Whether the claim is still live (disarmed on a decisive outcome).
    armed: bool,
}

impl Drop for ProbeToken {
    /// Release a still-armed claim so another waiter can probe
    fn drop(&mut self) {
        // a disarmed token already reported its outcome
        if self.armed {
            self.gate.release_probe(self.episode, self.tick);
        }
    }
}

/// How a pause episode's probe pacing stands, kept inside the gate's state.
#[derive(Debug, Clone)]
struct PauseState {
    /// Why the session paused.
    kind: PauseKind,
    /// The display name of the agent whose request first hit the fault.
    agent: String,
    /// A short human-readable summary of the most recent fault.
    error: String,
    /// When the pause began.
    since: DateTime<Utc>,
    /// How many automatic probes have failed since the pause began.
    probe_attempts: u32,
    /// The current probe tick (bumped after each failed probe).
    tick: u64,
    /// Whether the current tick's probe slot has been claimed.
    tick_claimed: bool,
    /// When the current tick's probe fires, on the tokio clock.
    next_probe_at: tokio::time::Instant,
    /// The same instant on the wall clock, for events and status.
    next_probe_wall: DateTime<Utc>,
}

/// The gate's mode: running or paused on a fault.
enum Mode {
    /// Requests are flowing normally.
    Running,
    /// Every request parks until a probe succeeds or the operator acts.
    Paused(PauseState),
}

/// What one [`ProviderGate::admit`] poll needs, snapshotted under the lock.
enum Snapshot {
    /// Requests are flowing.
    Running {
        /// The episode the operator aborted, for parked-waiter attribution.
        aborted: Option<u64>,
    },
    /// Requests park until a probe succeeds or the operator acts.
    Paused {
        /// The live pause episode.
        episode: u64,
        /// Why the session paused.
        kind: PauseKind,
        /// The current probe tick.
        tick: u64,
        /// Whether the current tick's probe slot is already claimed.
        claimed: bool,
        /// When the current tick's probe fires, on the tokio clock.
        next_probe_at: tokio::time::Instant,
        /// The episode the operator aborted, for parked-waiter attribution.
        aborted: Option<u64>,
    },
}

/// The lock-guarded core of the gate.
struct GateState {
    /// Whether requests flow or park.
    mode: Mode,
    /// The pause episode counter; bumped on every Running -> Paused flip.
    episode: u64,
    /// The episode the operator aborted, so its parked waiters surface their
    /// own errors instead of retrying.
    aborted_episode: Option<u64>,
    /// The live provider client every request is sent through.
    client: Arc<AiClient>,
    /// The epoch of `client`, bumped on every swap so cached model handles
    /// know to rebuild.
    epoch: u64,
}

/// The session-wide pause gate and live-client holder.
///
/// One gate is created per ThorPal instance and shared (via `Arc`) by every
/// [`RetryingModel`] -- the main agent, every sub-agent and swarm member, the
/// post-mortem salvager, the report generator, and the side summarizer -- so
/// a pause freezes the whole session coherently.
pub struct ProviderGate {
    /// The lock-guarded mode, episode bookkeeping, and live client.
    state: std::sync::Mutex<GateState>,
    /// Wakes parked waiters on any state change (pause, resume, abort,
    /// probe-slot release, forced retry).
    wake: watch::Sender<u64>,
    /// The consumer-facing status mirror [`PauseController::status`] reads.
    status: watch::Sender<PauseStatus>,
    /// The full provider config matching the live client, re-connected from
    /// on credential refreshes.
    provider: tokio::sync::Mutex<AiProviderConfig>,
    /// The caller's credential refresher, or `None` to reconnect as-is.
    refresher: Option<CredentialRefresher>,
    /// The sink pause/resume progress is reported on.
    events: Option<ThorPalEventSender>,
    /// The shutdown latch: once set, every request fails fast as interrupted
    /// instead of parking, so a dying process unwinds without writing the
    /// failure debris a genuine death would.
    shutdown: AtomicBool,
}

impl ProviderGate {
    /// Creates a new running gate around a connected client
    ///
    /// # Arguments
    ///
    /// * `client` - The connected provider client requests go through
    /// * `provider` - The fully-credentialed config `client` was built from
    /// * `refresher` - The caller's credential refresher, if one is installed
    /// * `events` - The sink pause/resume progress is reported on
    pub(crate) fn new(
        client: Arc<AiClient>,
        provider: AiProviderConfig,
        refresher: Option<CredentialRefresher>,
        events: Option<ThorPalEventSender>,
    ) -> Arc<Self> {
        // both watch channels start in the running state
        let (wake, _) = watch::channel(0);
        let (status, _) = watch::channel(PauseStatus::Running);
        Arc::new(ProviderGate {
            state: std::sync::Mutex::new(GateState {
                mode: Mode::Running,
                episode: 0,
                aborted_episode: None,
                client,
                epoch: 0,
            }),
            wake,
            status,
            provider: tokio::sync::Mutex::new(provider),
            refresher,
            events,
            shutdown: AtomicBool::new(false),
        })
    }

    /// Whether the session's shutdown latch has been set
    pub(crate) fn is_shutdown(&self) -> bool {
        // a plain load is enough: the latch only ever flips once, to true
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Lock the gate's state, tolerating a poisoned lock
    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        // the state is plain data, so a panicking peer never invalidates it
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Wake every parked waiter to re-read the gate's state
    fn notify(&self) {
        // bump the generation; the payload only matters as a change signal
        self.wake.send_modify(|generation| *generation += 1);
    }

    /// The current client and its epoch, for lazily rebuilding model handles
    pub(crate) fn client(&self) -> (u64, Arc<AiClient>) {
        // snapshot both under the lock so they always match
        let state = self.lock();
        (state.epoch, Arc::clone(&state.client))
    }

    /// Builds an agent whose completion requests ride this gate
    ///
    /// The retrying/pausing wrapper is seeded with the gate so the agent's
    /// requests retry transient faults, park on pauses, and rebuild their
    /// model handle after a credential refresh.
    ///
    /// # Arguments
    ///
    /// * `model` - The model to talk to
    /// * `agent` - The display name of the agent this builder serves
    pub(crate) fn retrying_agent(
        self: &Arc<Self>,
        model: impl Into<String>,
        agent: &str,
    ) -> AgentBuilder {
        // almost every agent wants the scrub ladder: a scrubbed retry beats
        // dying with partial work on a content-filter kill
        self.retrying_agent_with_policy(model, agent, ScrubPolicy::Ladder)
    }

    /// Builds a gated agent with an explicit content-filter scrub policy
    ///
    /// The report path opts out of the retry-level scrub ladder
    /// ([`ScrubPolicy::Surface`]) because it owns a smarter recovery of its
    /// own: defang-at-render with a client-side refang, then per-section
    /// generation (see [`crate::thorpal::report`]).
    ///
    /// # Arguments
    ///
    /// * `model` - The model to talk to
    /// * `agent` - The display name of the agent this builder serves
    /// * `scrub` - The policy the agent's requests answer content-filter 403s with
    pub(crate) fn retrying_agent_with_policy(
        self: &Arc<Self>,
        model: impl Into<String>,
        agent: &str,
        scrub: ScrubPolicy,
    ) -> AgentBuilder {
        // seed the wrapper from the gate's current client; it re-reads the
        // gate on every attempt, so a later swap is picked up lazily
        let model = model.into();
        let handle = self.lock().client.erased_model(&model);
        RetryingModel::new(
            handle,
            agent,
            self.events.clone(),
            Some(Arc::clone(self)),
            model,
        )
        .with_scrub_policy(scrub)
        .into_agent_builder()
    }

    /// A controller handle for the consumer's pause status and actions
    pub fn controller(self: &Arc<Self>) -> PauseController {
        PauseController {
            gate: Arc::clone(self),
        }
    }

    /// Wait until requests may flow, parking while the session is paused
    ///
    /// A waiter holding a live probe token passes straight through -- it IS
    /// the probe. Returns [`GatePass::Abort`] when the operator aborted the
    /// episode this waiter parked under.
    ///
    /// # Arguments
    ///
    /// * `token` - The waiter's probe token, when it is the elected prober
    pub(crate) async fn admit(self: &Arc<Self>, token: &Option<ProbeToken>) -> GatePass {
        // a shutting-down session fails every request fast -- parking (or
        // probing) during teardown would only stall the exit
        if self.is_shutdown() {
            return GatePass::Abort;
        }
        // the elected prober bypasses the gate: its attempt is the probe, and
        // parking it would stall the whole pause until the token dropped
        if token.as_ref().is_some_and(|token| token.armed) {
            return GatePass::Proceed(None);
        }
        // subscribe before the first snapshot so no wake-up can be missed
        let mut wake = self.wake.subscribe();
        // the episode this waiter parked under, for abort attribution
        let mut parked_episode: Option<u64> = None;
        loop {
            // snapshot what this waiter needs under the lock, then drop it
            let snapshot = {
                let state = self.lock();
                match &state.mode {
                    Mode::Running => Snapshot::Running {
                        aborted: state.aborted_episode,
                    },
                    Mode::Paused(pause) => Snapshot::Paused {
                        episode: state.episode,
                        kind: pause.kind,
                        tick: pause.tick,
                        claimed: pause.tick_claimed,
                        next_probe_at: pause.next_probe_at,
                        aborted: state.aborted_episode,
                    },
                }
            };
            match snapshot {
                Snapshot::Running { aborted } => {
                    // a shutdown latched while this waiter was parked fails it
                    // fast instead of letting it proceed into teardown
                    if self.is_shutdown() {
                        return GatePass::Abort;
                    }
                    // a waiter parked under an episode the operator aborted
                    // surfaces its own error instead of silently retrying
                    if parked_episode.is_some() && parked_episode == aborted {
                        return GatePass::Abort;
                    }
                    return GatePass::Proceed(None);
                }
                Snapshot::Paused {
                    episode,
                    kind,
                    tick,
                    claimed,
                    next_probe_at,
                    aborted,
                } => {
                    // an aborted parked waiter surfaces its own error even
                    // when an independent new pause landed before this waiter
                    // re-polled; check before the parked episode is replaced
                    if parked_episode.is_some() && parked_episode == aborted {
                        return GatePass::Abort;
                    }
                    // remember the episode this waiter is parked under
                    parked_episode = Some(episode);
                    // an unclaimed tick elects a prober once its fire time
                    // arrives; a claimed tick leaves this waiter parked
                    if claimed {
                        // wait for any state change, then re-read
                        let _ = wake.changed().await;
                        continue;
                    }
                    tokio::select! {
                        // any state change (resume, abort, re-pause, forced retry)
                        _ = wake.changed() => continue,
                        // the probe timer fired: try to claim this tick
                        () = tokio::time::sleep_until(next_probe_at) => {
                            if self.claim_probe(episode, tick) {
                                // arm the token before the refresh await so a
                                // cancellation there releases the claim via
                                // the token's drop instead of leaking it
                                let probe = ProbeToken {
                                    gate: Arc::clone(self),
                                    episode,
                                    tick,
                                    armed: true,
                                };
                                // an auth pause refreshes credentials first
                                if kind == PauseKind::Auth {
                                    self.refresh_credentials().await;
                                }
                                return GatePass::Proceed(Some(probe));
                            }
                            // another waiter won the election; park for its outcome
                            let _ = wake.changed().await;
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// Atomically claim the current probe tick, if it is still unclaimed
    ///
    /// # Arguments
    ///
    /// * `episode` - The pause episode the claim targets
    /// * `tick` - The probe tick the claim targets
    fn claim_probe(&self, episode: u64, tick: u64) -> bool {
        let mut state = self.lock();
        // the claim only holds while the same episode and tick are current
        if state.episode != episode {
            return false;
        }
        match &mut state.mode {
            Mode::Paused(pause) if pause.tick == tick && !pause.tick_claimed => {
                // claim the slot; it stays claimed until an outcome or a drop
                pause.tick_claimed = true;
                true
            }
            _ => false,
        }
    }

    /// Release a cancelled prober's claim so another waiter can probe
    ///
    /// # Arguments
    ///
    /// * `episode` - The pause episode the dropped token claimed under
    /// * `tick` - The probe tick the dropped token claimed
    fn release_probe(&self, episode: u64, tick: u64) {
        {
            let mut state = self.lock();
            // only release the exact claim this token held
            if state.episode != episode {
                return;
            }
            match &mut state.mode {
                Mode::Paused(pause) if pause.tick == tick && pause.tick_claimed => {
                    pause.tick_claimed = false;
                }
                _ => return,
            }
        }
        // wake the parked waiters so one of them claims the open slot
        self.notify();
    }

    /// Park on a pause-worthy fault, flipping the gate to paused if needed
    ///
    /// The first pauser of an episode flips the mode and announces the pause;
    /// the live prober's failure re-arms the next probe tick with backoff; any
    /// other request -- including one reporting through a stale token from a
    /// dead episode -- just joins the current episode as a waiter. A prober
    /// whose episode the operator aborted surfaces its own error instead of
    /// re-pausing. Everyone else then parks via [`ProviderGate::admit`].
    ///
    /// # Arguments
    ///
    /// * `kind` - Why the request wants the session paused
    /// * `agent` - The display name of the agent whose request failed
    /// * `error` - A short human-readable summary of the fault
    /// * `token` - The failed probe's token, when this request was the prober
    #[instrument(name = "ProviderGate::pause_and_park", skip_all, fields(agent = %agent, ?kind))]
    pub(crate) async fn pause_and_park(
        self: &Arc<Self>,
        kind: PauseKind,
        agent: &str,
        error: String,
        token: Option<ProbeToken>,
    ) -> GatePass {
        // a shutting-down session never pauses: the request fails fast as
        // interrupted so the dying process unwinds without parking anything
        if self.is_shutdown() {
            // the token (if any) drops here; release_probe tolerates it
            return GatePass::Abort;
        }
        // update the gate under the lock, capturing what the event needs
        let announce = {
            let mut state = self.lock();
            // a prober whose episode the operator aborted surfaces its own
            // error: no re-pause, no episode bump (the Some/Some shape keeps
            // a tokenless first-ever pause from matching an empty abort
            // record, and a live episode is always newer than an aborted one)
            if matches!(
                (state.aborted_episode, &token),
                (Some(aborted), Some(probe)) if aborted == probe.episode
            ) {
                return GatePass::Abort;
            }
            // snapshot the live episode before borrowing the mode mutably
            let episode = state.episode;
            match &mut state.mode {
                // the first pauser of a new episode flips the mode
                Mode::Running => {
                    state.episode += 1;
                    // the first probe fires after the base delay
                    let (next_probe_at, next_probe_wall) = probe_schedule(PROBE_BASE_DELAY);
                    let pause = PauseState {
                        kind,
                        agent: agent.to_owned(),
                        error,
                        since: Utc::now(),
                        probe_attempts: 0,
                        tick: 0,
                        tick_claimed: false,
                        next_probe_at,
                        next_probe_wall,
                    };
                    let announce = pause.clone();
                    state.mode = Mode::Paused(pause);
                    Some(announce)
                }
                // the live prober's failed probe schedules the next tick with
                // backoff; a stale token (wrong episode or tick) joins below
                Mode::Paused(pause)
                    if token.as_ref().is_some_and(|probe| {
                        probe.episode == episode && probe.tick == pause.tick
                    }) =>
                {
                    pause.probe_attempts += 1;
                    pause.tick += 1;
                    pause.tick_claimed = false;
                    pause.error = error;
                    // back the probes off exponentially, capped
                    let exponent = pause.probe_attempts.min(8);
                    let delay = PROBE_BASE_DELAY
                        .saturating_mul(2u32.saturating_pow(exponent))
                        .min(PROBE_MAX_DELAY);
                    let (next_probe_at, next_probe_wall) = probe_schedule(delay);
                    pause.next_probe_at = next_probe_at;
                    pause.next_probe_wall = next_probe_wall;
                    Some(pause.clone())
                }
                // any other request (stale token included) joins the episode
                Mode::Paused(_) => None,
            }
        };
        // the token has reported; drop it armed -- release_probe validates
        // the episode and tick, so a superseded or stale claim is a safe
        // no-op and never perturbs the slot the match above re-armed
        drop(token);
        // announce the (re-)pause outside the lock
        if let Some(pause) = announce {
            // one warning per transition is the file log's whole record
            tracing::warn!(
                ?kind,
                agent,
                error = %pause.error,
                probe_attempts = pause.probe_attempts,
                "provider requests paused; waiting for recovery or the operator"
            );
            self.publish_paused(&pause);
            self.notify();
        }
        // park like any other waiter
        self.admit(&None).await
    }

    /// Report a decisive success, resuming every parked waiter
    ///
    /// Any 2xx proves both the provider and the credentials work, so a pause
    /// of either kind ends. A no-op while the gate is already running.
    ///
    /// # Arguments
    ///
    /// * `token` - The reporting request's probe token, if it was the prober
    pub(crate) fn report_success(&self, token: Option<ProbeToken>) {
        // the outcome is the report; the claim must not be released as idle
        if let Some(mut token) = token {
            token.armed = false;
        }
        // resume everyone when the gate was paused
        let resumed = {
            let mut state = self.lock();
            match &state.mode {
                Mode::Paused(pause) => {
                    let kind = pause.kind;
                    state.mode = Mode::Running;
                    Some(kind)
                }
                Mode::Running => None,
            }
        };
        if let Some(kind) = resumed {
            tracing::info!(?kind, "provider requests resumed");
            self.emit(ThorPalEvent::SessionResumed {
                kind,
                recovered: true,
            });
            let _ = self.status.send(PauseStatus::Running);
            self.notify();
        }
    }

    /// Report a decisive non-pause failure, resuming only an outage pause
    ///
    /// The provider answered, so a [`PauseKind::RetryBudgetExhausted`] pause
    /// is over; an auth pause stays, because a decisive non-auth failure
    /// (e.g. a request-build fault) proves nothing about the credentials.
    /// When the reporter was an auth pause's prober its token drops still
    /// armed here, so [`ProbeToken`]'s drop releases the claim and the next
    /// parked waiter re-elects at the already-elapsed tick. Benign residual:
    /// if the last parked request drains through this path the gate idles
    /// paused with a past `next_probe_at` until the next request parks and
    /// self-elects, probing immediately.
    ///
    /// # Arguments
    ///
    /// * `token` - The reporting request's probe token, if it was the prober
    pub(crate) fn report_decisive_failure(&self, token: Option<ProbeToken>) {
        // resume only an outage pause: the provider answering ends it
        let resumed = {
            let mut state = self.lock();
            if matches!(
                &state.mode,
                Mode::Paused(pause) if pause.kind == PauseKind::RetryBudgetExhausted
            ) {
                state.mode = Mode::Running;
                Some(PauseKind::RetryBudgetExhausted)
            } else {
                // an auth pause (or a running gate) is left untouched
                None
            }
        };
        if let Some(kind) = resumed {
            // the outcome was the report; keep the (now moot) claim from
            // being released as idle
            if let Some(mut token) = token {
                token.armed = false;
            }
            tracing::info!(?kind, "provider requests resumed");
            self.emit(ThorPalEvent::SessionResumed {
                kind,
                recovered: true,
            });
            let _ = self.status.send(PauseStatus::Running);
            self.notify();
        }
        // otherwise the token (if any) drops here still armed: its drop
        // releases the probe claim so the next waiter re-elects immediately
    }

    /// Refresh the gate's credentials and swap in a freshly connected client
    ///
    /// Runs the caller's refresher when one is installed and connects a
    /// replacement client from the config it returns, bumping the epoch so
    /// every cached model handle rebuilds. Without a refresher (or when it
    /// fails) the current client stays and the probe simply fires -- which is
    /// all a self-refreshing credential chain (AWS profiles/SSO) needs anyway,
    /// since the SDK re-resolves its credentials internally.
    #[instrument(name = "ProviderGate::refresh_credentials", skip_all)]
    async fn refresh_credentials(&self) {
        // without a refresher there is nothing fresher to connect with
        let Some(refresher) = &self.refresher else {
            return;
        };
        // ask the caller for a freshly-credentialed config
        let config = match refresher().await {
            Ok(config) => config,
            Err(error) => {
                // a failed refresh is not fatal: probe with what we have
                tracing::warn!(
                    ?error,
                    "credential refresh failed; probing with the current client"
                );
                return;
            }
        };
        // connect a replacement client from the refreshed config
        match config.connect().await {
            Ok(client) => {
                // publish the new client and bump the epoch under the lock
                {
                    let mut state = self.lock();
                    state.client = Arc::new(client);
                    state.epoch += 1;
                }
                // remember the config the live client was built from
                *self.provider.lock().await = config;
            }
            Err(error) => {
                // keep the old client; the probe still fires with it
                tracing::warn!(
                    ?error,
                    "reconnecting the provider client failed; probing with the current client"
                );
            }
        }
    }

    /// Publish a pause state onto the event sink and the status mirror
    ///
    /// # Arguments
    ///
    /// * `pause` - The pause state to publish
    fn publish_paused(&self, pause: &PauseState) {
        // bind the published fields once so both payloads agree by construction
        let PauseState {
            kind,
            agent,
            error,
            since,
            probe_attempts,
            next_probe_wall,
            ..
        } = pause;
        self.emit(ThorPalEvent::SessionPaused {
            kind: *kind,
            agent: agent.clone(),
            error: error.clone(),
            since: *since,
            probe_attempts: *probe_attempts,
            next_probe_at: *next_probe_wall,
        });
        let _ = self.status.send(PauseStatus::Paused {
            kind: *kind,
            agent: agent.clone(),
            error: error.clone(),
            since: *since,
            probe_attempts: *probe_attempts,
            next_probe_at: *next_probe_wall,
        });
    }
}

/// The consumer's handle for observing and steering a session's pause state.
///
/// Cloneable and cheap: the TUI keeps one for its banner and slash commands,
/// headless mode keeps one for its pause supervisor.
#[derive(Clone)]
pub struct PauseController {
    /// The gate this controller observes and steers.
    gate: Arc<ProviderGate>,
}

impl PauseController {
    /// A fresh receiver of the gate's live pause status
    pub fn status(&self) -> watch::Receiver<PauseStatus> {
        // every caller gets its own cursor over the shared mirror
        self.gate.status.subscribe()
    }

    /// Fire the next automatic probe now instead of waiting out the backoff
    ///
    /// The elected prober still refreshes credentials first for auth pauses,
    /// so this is the "I fixed the config, try again" action.
    pub fn retry_now(&self) {
        {
            let mut state = self.gate.lock();
            // pull the pending probe tick forward to now
            if let Mode::Paused(pause) = &mut state.mode {
                let (next_probe_at, next_probe_wall) = probe_schedule(std::time::Duration::ZERO);
                pause.next_probe_at = next_probe_at;
                pause.next_probe_wall = next_probe_wall;
            }
        }
        // wake the parked waiters so one claims the (now due) probe
        self.gate.notify();
    }

    /// Abort any pause AND latch the session as shutting down
    ///
    /// The teardown flavor of [`PauseController::abort`]: beyond surfacing the
    /// parked requests' errors, the latch makes every later request fail fast
    /// as a shutdown interruption -- the error carries a marker the death
    /// paths recognize to freeze state for resume (no salvage posts, no
    /// failure events, the pending turn stays pending) instead of recording a
    /// genuine failure. Used when the process is going down: the headless
    /// pause supervisor giving up on a pause, and the TUI tearing down with a
    /// turn still in flight.
    pub fn abort_for_shutdown(&self) {
        // latch first so requests surfacing from the abort classify as
        // interrupted rather than failed
        self.gate.shutdown.store(true, Ordering::SeqCst);
        // then abort any live pause so parked requests wake and surface
        self.abort();
        // wake unparked waiters too (abort only notifies when a pause ended),
        // so an idle gate's next request still observes the latch promptly
        self.gate.notify();
    }

    /// Abort the current pause: parked requests surface their own errors
    ///
    /// The session itself stays alive -- the turn or run that was in flight
    /// fails exactly as a fatal provider error always did, and later requests
    /// can pause again.
    pub fn abort(&self) {
        let aborted = {
            let mut state = self.gate.lock();
            match &state.mode {
                Mode::Paused(pause) => {
                    let kind = pause.kind;
                    // mark the episode aborted so its waiters surface errors
                    state.aborted_episode = Some(state.episode);
                    state.mode = Mode::Running;
                    Some(kind)
                }
                Mode::Running => None,
            }
        };
        if let Some(kind) = aborted {
            tracing::warn!(?kind, "provider pause aborted by the operator");
            self.gate.emit(ThorPalEvent::SessionResumed {
                kind,
                recovered: false,
            });
            let _ = self.gate.status.send(PauseStatus::Running);
            self.gate.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::Notify;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::time::timeout;

    /// A generous paused-clock bound turning a protocol hang into a failure.
    const SETTLE: Duration = Duration::from_secs(3600);

    /// Builds a running gate around a placeholder client
    ///
    /// # Arguments
    ///
    /// * `refresher` - The credential refresher to install, if any
    async fn test_gate(
        refresher: Option<CredentialRefresher>,
    ) -> (Arc<ProviderGate>, UnboundedReceiver<ThorPalEvent>) {
        // a placeholder provider config that builds without any network
        let provider = AiProviderConfig::OpenAi {
            endpoint: None,
            api_key: "test-key".to_owned(),
            model: "test-model".to_owned(),
        };
        let client = provider
            .connect()
            .await
            .expect("the placeholder client should build");
        // tee the gate's events into an inspectable channel
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            ProviderGate::new(Arc::new(client), provider, refresher, Some(tx)),
            rx,
        )
    }

    /// A refresher that announces each entry and parks until released
    ///
    /// Parking the refresher holds the probe-claim window open: the elected
    /// prober sits inside [`ProviderGate::refresh_credentials`] with the
    /// claim held until the test notifies `hold`.
    ///
    /// # Arguments
    ///
    /// * `entered` - Notified once per refresher entry
    /// * `hold` - The refresher parks until this is notified
    fn parked_refresher(entered: Arc<Notify>, hold: Arc<Notify>) -> CredentialRefresher {
        Arc::new(move || {
            // clone the handles into the returned future
            let entered = Arc::clone(&entered);
            let hold = Arc::clone(&hold);
            Box::pin(async move {
                // tell the test the probe-claim window is open
                entered.notify_one();
                // park until the test releases this probe
                hold.notified().await;
                // fail the refresh so the gate probes with its current client
                Err(anyhow::anyhow!("refresher released"))
            })
        })
    }

    /// Spawn a first failing request that flips the gate to paused and parks
    ///
    /// # Arguments
    ///
    /// * `gate` - The gate to pause
    fn spawn_pauser(gate: &Arc<ProviderGate>) -> tokio::task::JoinHandle<GatePass> {
        // the pauser is tokenless: it is an ordinary failing request
        let gate = Arc::clone(gate);
        tokio::spawn(async move {
            gate.pause_and_park(PauseKind::Auth, "main", "401".to_owned(), None)
                .await
        })
    }

    /// An abort landing while the prober holds the claim aborts the parked
    /// waiters, and the prober's own failure report does not re-pause
    #[tokio::test(start_paused = true)]
    async fn abort_while_probe_claim_held_sticks() {
        // park the credential refresher so the probe-claim window stays open
        let entered = Arc::new(Notify::new());
        let hold = Arc::new(Notify::new());
        let (gate, mut rx) = test_gate(Some(parked_refresher(entered.clone(), hold.clone()))).await;
        let controller = gate.controller();
        let mut status = controller.status();
        // the first failing request flips the gate to paused and parks
        let pauser = spawn_pauser(&gate);
        status.changed().await.expect("a pause status change");
        // a second request joins the pause as a plain parked waiter
        let waiter = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.admit(&None).await })
        };
        // the paused clock advances to the probe; the elected prober parks
        // inside the refresher with the claim held
        timeout(SETTLE, entered.notified())
            .await
            .expect("a prober should enter the refresher");
        // the operator aborts while the claim is held
        controller.abort();
        // release the prober; its admit hands back the armed probe token
        hold.notify_one();
        let first = timeout(SETTLE, pauser)
            .await
            .expect("the pauser should settle")
            .expect("the pauser should join");
        let second = timeout(SETTLE, waiter)
            .await
            .expect("the waiter should settle")
            .expect("the waiter should join");
        // exactly one of the two was elected prober; the other saw the abort
        let token = match (first, second) {
            (GatePass::Proceed(Some(token)), GatePass::Abort)
            | (GatePass::Abort, GatePass::Proceed(Some(token))) => token,
            _ => panic!("expected one elected prober and one aborted waiter"),
        };
        // the prober's own failure joins the abort instead of re-pausing
        let pass = gate
            .pause_and_park(PauseKind::Auth, "main", "still 401".to_owned(), Some(token))
            .await;
        assert!(matches!(pass, GatePass::Abort));
        // the gate stayed running: one pause, one non-recovered resume, and
        // no second pause announcement
        assert!(matches!(*status.borrow(), PauseStatus::Running));
        let mut paused = 0;
        let mut resumed = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                ThorPalEvent::SessionPaused { .. } => paused += 1,
                ThorPalEvent::SessionResumed { recovered, .. } => {
                    assert!(!recovered, "an abort is never a recovery");
                    resumed += 1;
                }
                _ => (),
            }
        }
        assert_eq!(paused, 1, "the abort must not be followed by a re-pause");
        assert_eq!(resumed, 1, "the abort must be announced exactly once");
    }

    /// Cancelling an elected prober mid-refresh releases its claim so another
    /// waiter can be elected instead of the pause stalling forever
    #[tokio::test(start_paused = true)]
    async fn cancelled_probe_mid_refresh_releases_the_claim() {
        // park the credential refresher so the cancellation window is open
        let entered = Arc::new(Notify::new());
        let hold = Arc::new(Notify::new());
        let (gate, _rx) = test_gate(Some(parked_refresher(entered.clone(), hold.clone()))).await;
        let mut status = gate.controller().status();
        // flip the gate to paused, then cancel the pauser so the elections
        // below have exactly one competitor at a time
        let pauser = spawn_pauser(&gate);
        status.changed().await.expect("a pause status change");
        pauser.abort();
        let _ = pauser.await;
        // the lone waiter is elected and parks inside the refresher
        let doomed = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.admit(&None).await })
        };
        timeout(SETTLE, entered.notified())
            .await
            .expect("the doomed waiter should enter the refresher");
        // cancel the prober while it holds the claim; destroying the task
        // drops its armed token, which must release the claim
        doomed.abort();
        let join = doomed.await;
        assert!(join.is_err_and(|err| err.is_cancelled()));
        // a replacement waiter can now claim the already-elapsed tick
        let replacement = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.admit(&None).await })
        };
        timeout(SETTLE, entered.notified())
            .await
            .expect("the replacement waiter should re-claim the probe");
        // release its refresher pass so admit hands back the armed token
        hold.notify_one();
        let pass = timeout(SETTLE, replacement)
            .await
            .expect("the replacement waiter should settle")
            .expect("the replacement waiter should join");
        assert!(matches!(pass, GatePass::Proceed(Some(_))));
    }

    /// A stale probe token (wrong episode) joins the live pause as a plain
    /// waiter instead of perturbing its tick, backoff, or recorded error
    #[tokio::test(start_paused = true)]
    async fn stale_token_joins_without_perturbing_the_pause() {
        let (gate, _rx) = test_gate(None).await;
        let controller = gate.controller();
        let mut status = controller.status();
        // flip the gate to paused under episode 1
        let pauser = spawn_pauser(&gate);
        status.changed().await.expect("a pause status change");
        // forge a token from a dead episode, as held by a prober whose pause
        // resumed and re-paused while its request was in flight
        let stale = ProbeToken {
            gate: Arc::clone(&gate),
            episode: 0,
            tick: 0,
            armed: true,
        };
        // the stale report must join as a waiter, not re-arm the live pause
        let joiner = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move {
                gate.pause_and_park(PauseKind::Auth, "stale", "late 401".to_owned(), Some(stale))
                    .await
            })
        };
        // let the joiner run its report and park (no timers elapse here)
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        // the live pause is untouched: same tick, no probe counted, no claim
        // held, and the first pauser's error still stands
        {
            let state = gate.lock();
            match &state.mode {
                Mode::Paused(pause) => {
                    assert_eq!(pause.tick, 0, "the stale token must not bump the tick");
                    assert_eq!(pause.probe_attempts, 0, "no probe ran, none may be counted");
                    assert!(!pause.tick_claimed, "the stale token must not hold a claim");
                    assert_eq!(pause.error, "401", "the live error must not be replaced");
                }
                Mode::Running => panic!("the gate must stay paused"),
            }
            assert_eq!(state.episode, 1);
        }
        // aborting the pause releases both parked requests with the abort
        controller.abort();
        let first = timeout(SETTLE, pauser)
            .await
            .expect("the pauser should settle")
            .expect("the pauser should join");
        let second = timeout(SETTLE, joiner)
            .await
            .expect("the joiner should settle")
            .expect("the joiner should join");
        assert!(matches!(first, GatePass::Abort));
        assert!(matches!(second, GatePass::Abort));
    }

    /// The shutdown latch fails every request fast instead of parking it
    #[tokio::test]
    async fn shutdown_latch_aborts_everything() {
        let (gate, _events) = test_gate(None).await;
        // a running gate admits requests normally
        assert!(matches!(gate.admit(&None).await, GatePass::Proceed(None)));
        // latch the shutdown through the controller's teardown abort
        gate.controller().abort_for_shutdown();
        assert!(gate.is_shutdown());
        // a new request fails fast instead of proceeding into teardown
        assert!(matches!(gate.admit(&None).await, GatePass::Abort));
        // a pause-worthy fault aborts instead of parking the dying process
        let pass = gate
            .pause_and_park(PauseKind::Auth, "main", "401".to_owned(), None)
            .await;
        assert!(matches!(pass, GatePass::Abort));
    }
}
