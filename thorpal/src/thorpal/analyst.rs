//! The live analyst link: how a running agent asks the human for a decision.
//!
//! The `ask_analyst` tool blocks its tool future on a human answer, but only
//! when a live analyst UI has registered itself as the responder. The
//! [`AnalystLink`] is that registration point: the builder creates one per
//! ThorPal instance, seeds it into every tool call's [`ToolContext`], and a
//! consumer such as thorctl's chat TUI calls [`AnalystLink::attach`] before the
//! run starts. A pending ask is announced on the run's event stream
//! ([`ThorPalEvent::AnalystAskPending`]) and answered back through
//! [`AnalystLink::answer`]; with no responder attached the tool never waits and
//! takes its default-and-record path instead, so headless, batch, and blinded
//! evaluation runs stay fully unattended.
//!
//! Modeled on the [`ProviderGate`]/`PauseController` split: thorpal owns the
//! shared state, the consumer holds a cloneable handle, and dropping the
//! consumer can never wedge the run -- [`AnalystLink::detach`] resolves every
//! pending ask as declined so an in-flight turn finishes on the recommended
//! default.
//!
//! [`ProviderGate`]: super::gate::ProviderGate

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rig::tool::{ToolContext, ToolExecutionError};
use tokio::sync::oneshot;

use super::events::{ThorPalEvent, ThorPalEventSender};
use super::tools::forum::AskOption;

/// A pending interactive ask, announced to a live consumer on the event stream
#[derive(Debug, Clone)]
pub struct PendingAsk {
    /// The link-local ticket the answer is correlated back through
    pub id: u64,
    /// The display name of the agent asking (always `"main"` in v1)
    pub agent: String,
    /// The question posed to the analyst
    pub question: String,
    /// The 2-4 options offered, in display order
    pub options: Vec<AskOption>,
    /// The label of the designated default/recommended option
    pub recommended: String,
}

/// The analyst's reply to a pending ask
#[derive(Debug, Clone)]
pub enum AnalystAnswer {
    /// An offered option was picked, by its label
    Picked {
        /// The exact label of the picked option
        label: String,
    },
    /// A free-text answer was typed instead of picking an option
    Other {
        /// The analyst's free-text answer
        text: String,
    },
    /// The analyst declined to answer (Esc or TUI teardown): take the default
    Declined,
}

/// The live analyst channel: attached by an interactive consumer, absent headless.
///
/// One link is shared across a whole ThorPal instance through the run's
/// [`ToolContext`]. The pending map is bounded in practice by the forum's
/// per-run ask cap, and every entry is resolved on [`AnalystLink::detach`], so
/// an abandoned ask can never outlive the consumer that was meant to answer it.
pub struct AnalystLink {
    /// The pending asks awaiting answers, keyed by ticket id.
    ///
    /// A std mutex rather than a tokio one: both sides' critical sections are
    /// tiny O(1) map operations with no await inside, and the TUI answers from
    /// a synchronous key handler.
    pending: Mutex<HashMap<u64, oneshot::Sender<AnalystAnswer>>>,
    /// The next ticket id to hand out (ticket 0 is reserved for "no ticket").
    next: AtomicU64,
    /// Whether a live analyst UI is attached as the responder.
    attached: AtomicBool,
    /// The sink pending asks are announced on, installed once at build time.
    events: OnceLock<ThorPalEventSender>,
}

impl AnalystLink {
    /// Creates a fresh, unattached analyst link
    pub fn new() -> Self {
        AnalystLink {
            pending: Mutex::new(HashMap::new()),
            // ticket ids start at 1 so 0 can mean "no analyst UI was attached"
            next: AtomicU64::new(1),
            attached: AtomicBool::new(false),
            events: OnceLock::new(),
        }
    }

    /// Installs the event sink pending asks are announced on
    ///
    /// Called once by the builder when the run has an event sink; a second call
    /// is ignored since the sink is fixed for the link's lifetime.
    ///
    /// # Arguments
    ///
    /// * `sender` - The channel to emit [`ThorPalEvent::AnalystAskPending`] onto
    pub(crate) fn set_events(&self, sender: ThorPalEventSender) {
        // the sink is set at most once; a repeat install keeps the first
        let _ = self.events.set(sender);
    }

    /// Registers a live analyst UI as the responder for this run's asks
    ///
    /// Until this is called the `ask_analyst` tool never waits: it resolves
    /// every ask with its recommended default immediately.
    pub fn attach(&self) {
        // flip the responder flag so the next ask announces instead of defaulting
        self.attached.store(true, Ordering::SeqCst);
    }

    /// Detaches the live analyst UI, resolving every pending ask as declined
    ///
    /// Called on TUI teardown so an in-flight turn blocked on an ask finishes
    /// on the recommended default instead of wedging the session join. The
    /// flag flips and the map drains under the same lock [`AnalystLink::register`]
    /// inserts under, so a racing register either lands before the drain (and
    /// is resolved as declined here) or observes the detach and never parks.
    pub fn detach(&self) {
        // flip the flag and drain the map under one lock so no register can
        // slip a waiter in after the drain; answers are sent outside the lock
        let drained: Vec<oneshot::Sender<AnalystAnswer>> = {
            let mut pending = self.pending.lock().expect("analyst link lock poisoned");
            self.attached.store(false, Ordering::SeqCst);
            pending.drain().map(|(_, sender)| sender).collect()
        };
        // resolve every waiter as declined; a dropped receiver is a no-op
        for sender in drained {
            let _ = sender.send(AnalystAnswer::Declined);
        }
    }

    /// Whether a live analyst UI is currently attached as the responder
    pub fn is_attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }

    /// Registers a new pending ask and returns its ticket and answer channel
    ///
    /// Returns `None` when no live analyst UI is attached, checked under the
    /// same lock [`AnalystLink::detach`] drains under: a register can never
    /// slip a waiter in behind a detach, so an ask racing a quit takes its
    /// default path instead of parking forever. The caller announces the ask
    /// with [`AnalystLink::announce`] and then awaits the receiver; a receiver
    /// dropped mid-wait (the turn was cancelled) makes a later answer a no-op.
    pub(crate) fn register(&self) -> Option<(u64, oneshot::Receiver<AnalystAnswer>)> {
        // park the answer channel under the lock, re-checking attachment there
        // so this is atomic with detach's flag-flip-and-drain
        let mut pending = self.pending.lock().expect("analyst link lock poisoned");
        if !self.attached.load(Ordering::SeqCst) {
            return None;
        }
        // hand out the next ticket and park its answer channel in the map
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        pending.insert(id, tx);
        Some((id, rx))
    }

    /// Announces a pending ask on the event stream for the attached UI
    ///
    /// # Arguments
    ///
    /// * `ask` - The pending ask to announce
    pub(crate) fn announce(&self, ask: PendingAsk) {
        // report the ask without blocking; a closed channel is a no-op
        if let Some(tx) = self.events.get() {
            let _ = tx.send(ThorPalEvent::AnalystAskPending {
                id: ask.id,
                agent: ask.agent,
                question: ask.question,
                options: ask.options,
                recommended: ask.recommended,
            });
        }
    }

    /// Poses a consumer-level question through the attached analyst UI
    ///
    /// Unlike the agent's `ask_analyst` tool (which also records the ask on
    /// the forum), this is a plain interactive prompt a consumer poses on its
    /// own behalf -- e.g. the TUI confirming whether to resume interrupted
    /// work. Without an attached UI, or when the UI declines or tears down,
    /// the answer is [`AnalystAnswer::Declined`], so a headless caller never
    /// blocks and the consumer decides what declining means.
    ///
    /// # Arguments
    ///
    /// * `agent` - The display name the question is posed under
    /// * `question` - The question to pose
    /// * `options` - The 2-4 options offered, in display order
    /// * `recommended` - The label of the default/recommended option
    pub async fn pose(
        &self,
        agent: impl Into<String>,
        question: impl Into<String>,
        options: Vec<AskOption>,
        recommended: impl Into<String>,
    ) -> AnalystAnswer {
        // without an attached UI the question has no responder
        let Some((id, rx)) = self.register() else {
            return AnalystAnswer::Declined;
        };
        // announce the ask so the attached UI renders its modal
        self.announce(PendingAsk {
            id,
            agent: agent.into(),
            question: question.into(),
            options,
            recommended: recommended.into(),
        });
        // wait for the answer; a dropped channel (teardown) declines
        rx.await.unwrap_or(AnalystAnswer::Declined)
    }

    /// Answers a pending ask, waking the tool future waiting on it
    ///
    /// Returns `false` when the ask was already resolved (a detach raced the
    /// answer, or the awaiting turn was cancelled), so a UI can close its
    /// modal unconditionally on any outcome.
    ///
    /// # Arguments
    ///
    /// * `id` - The ticket from the matching pending-ask announcement
    /// * `answer` - The analyst's reply
    pub fn answer(&self, id: u64, answer: AnalystAnswer) -> bool {
        // remove the waiter under the lock, then send outside it
        let sender = {
            let mut pending = self.pending.lock().expect("analyst link lock poisoned");
            pending.remove(&id)
        };
        // a missing waiter or a dropped receiver both mean "already resolved"
        match sender {
            Some(sender) => sender.send(answer).is_ok(),
            None => false,
        }
    }

    /// Pulls the shared analyst link out of a tool call's [`ToolContext`]
    ///
    /// The `ask_analyst` tool calls this at the top of every invocation to
    /// recover the `Arc<AnalystLink>` that `ThorPal` seeded into the run's
    /// context. A missing value is a wiring bug, so it surfaces as an
    /// invalid-args tool error.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn from_context(
        context: &ToolContext,
    ) -> Result<Arc<AnalystLink>, ToolExecutionError> {
        // recover the shared link, mapping a missing slot to a tool error
        context
            .require::<Arc<AnalystLink>>()
            .map(Arc::clone)
            .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))
    }
}

impl Default for AnalystLink {
    /// Creates a fresh, unattached analyst link
    fn default() -> Self {
        AnalystLink::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registered ask wakes its receiver with the analyst's answer
    #[tokio::test]
    async fn answer_wakes_the_registered_receiver() {
        let link = AnalystLink::new();
        link.attach();
        // register a pending ask and answer it from "the TUI"
        let (id, rx) = link.register().expect("an attached link should register");
        assert!(link.answer(
            id,
            AnalystAnswer::Picked {
                label: "static".to_owned()
            }
        ));
        // the waiting future sees exactly the answer that was sent
        match rx.await.expect("the answer channel closed unexpectedly") {
            AnalystAnswer::Picked { label } => assert_eq!(label, "static"),
            other => panic!("expected a picked answer, got {other:?}"),
        }
    }

    /// Detaching resolves every pending ask as declined and flips attachment
    #[tokio::test]
    async fn detach_resolves_all_pending_asks_as_declined() {
        let link = AnalystLink::new();
        link.attach();
        assert!(link.is_attached());
        // park two pending asks, then tear the "TUI" down
        let (_, rx_a) = link.register().expect("an attached link should register");
        let (_, rx_b) = link.register().expect("an attached link should register");
        link.detach();
        assert!(!link.is_attached());
        // both waiters resolve as declined rather than hanging forever
        assert!(matches!(rx_a.await, Ok(AnalystAnswer::Declined)));
        assert!(matches!(rx_b.await, Ok(AnalystAnswer::Declined)));
        // a register landing after the detach is refused instead of parking a
        // waiter nobody is left to answer (the detach/register race guard)
        assert!(link.register().is_none());
    }

    /// Answering an unknown or already-resolved ticket reports failure
    #[tokio::test]
    async fn stale_answers_report_failure() {
        let link = AnalystLink::new();
        link.attach();
        // an unknown ticket was never registered
        assert!(!link.answer(999, AnalystAnswer::Declined));
        // a ticket whose receiver was dropped (cancelled turn) also fails
        let (id, rx) = link.register().expect("an attached link should register");
        drop(rx);
        assert!(!link.answer(id, AnalystAnswer::Declined));
        // and a second answer to the same ticket finds nothing to wake
        let (id, rx) = link.register().expect("an attached link should register");
        assert!(link.answer(id, AnalystAnswer::Declined));
        drop(rx);
        assert!(!link.answer(id, AnalystAnswer::Declined));
    }
}
