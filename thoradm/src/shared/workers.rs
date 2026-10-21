//! Joining the workers of one controller step
//!
//! Every controller sends its orders over a channel to a fixed set of workers. Once the last
//! order is sent, the controller drops its sender and joins the workers directly; a worker that
//! returns an error or panics ends its share of the work, and when every worker of a step has
//! ended early the step fails at once instead of waiting for orders nobody will take. Each worker
//! takes its orders through [`Orders`], which remembers the order it is working on, so a worker
//! that ends early is reported with the order it lost.

use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use kanal::{AsyncReceiver, AsyncSender};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use tokio::task::JoinHandle;

use crate::Error;

/// The orders one worker takes, remembering the order it is working on so a worker that ends
/// early can be reported with what it held
#[derive(Debug)]
pub struct Orders<O> {
    /// The channel every worker of the step takes orders from
    rx: AsyncReceiver<O>,
    /// The order this worker took and hasn't finished, shared with the task that joins it
    held: Arc<Mutex<Option<O>>>,
}

impl<O: Clone> Orders<O> {
    /// Takes the next order, marking the one taken before it finished
    ///
    /// Returns the channel's error once it has ended and every queued order is taken.
    pub async fn recv(&self) -> Result<O, kanal::ReceiveError> {
        // asking for another order means the last one is finished
        self.hold(None);
        let order = self.rx.recv().await?;
        // the new order is held until the worker asks for the next one
        self.hold(Some(order.clone()));
        Ok(order)
    }

    /// Sets the order this worker holds
    ///
    /// # Arguments
    ///
    /// * `order` - The order taken, or `None` when the worker holds none
    fn hold(&self, order: Option<O>) {
        // a panic elsewhere can't leave a half-written order behind, so a poisoned lock is fine
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = order;
    }
}

/// A worker that ended early: its error and the order it held when it did
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerFailure<O> {
    /// The error the worker returned, or the message it panicked with
    pub error: String,
    /// The order the worker took and didn't finish, if it held one
    pub order: Option<O>,
}

/// The join handle of one worker spawned with [`spawn_worker`]
pub type WorkerHandle<W, O> = JoinHandle<Result<W, WorkerFailure<O>>>;

/// Returns the message a panic carried
///
/// # Arguments
///
/// * `panic` - The panic's payload
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    // panics carry a formatted String or a literal str
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

/// Spawns one worker of a step, giving it its own [`Orders`] on the step's channel
///
/// The worker's error, or its panic, comes back with the order it held when it ended.
///
/// # Arguments
///
/// * `orders` - A receiver of the step's order channel
/// * `start` - Starts the worker on its orders
pub fn spawn_worker<W, O, F, Fut>(orders: &AsyncReceiver<O>, start: F) -> WorkerHandle<W, O>
where
    W: Send + 'static,
    O: Clone + Send + 'static,
    F: FnOnce(Orders<O>) -> Fut,
    Fut: Future<Output = Result<W, Error>> + Send + 'static,
{
    // share the held order between the worker and its task
    let held = Arc::new(Mutex::new(None));
    let worker = start(Orders {
        rx: orders.clone(),
        held: held.clone(),
    });
    tokio::spawn(async move {
        // a panic is caught so the order the worker held can be reported with it
        let ended = AssertUnwindSafe(worker).catch_unwind().await;
        let order = || held.lock().unwrap_or_else(PoisonError::into_inner).take();
        match ended {
            Ok(Ok(worker)) => Ok(worker),
            Ok(Err(error)) => Err(WorkerFailure {
                error: error.to_string(),
                order: order(),
            }),
            Err(panic) => Err(WorkerFailure {
                error: format!("worker panicked: {}", panic_message(panic.as_ref())),
                order: order(),
            }),
        }
    })
}

/// How the workers of one step ended
#[derive(Debug)]
pub struct Joined<W, O> {
    /// The workers that returned normally
    pub finished: Vec<W>,
    /// The workers that returned an error or panicked, with the orders they held
    pub errors: Vec<WorkerFailure<O>>,
    /// How many orders were still queued when the last worker ended
    pub orders_left: usize,
}

impl<W, O> Joined<W, O> {
    /// Returns true when the step can't be trusted to have handled its orders: every worker ended
    /// in an error, or some did and orders were left behind
    #[must_use]
    pub fn step_failed(&self) -> bool {
        // without errors the step finished, however many orders a stop signal left queued
        !self.errors.is_empty() && (self.finished.is_empty() || self.orders_left > 0)
    }

    /// Builds the error a failed step ends with, naming every worker's error
    ///
    /// The error is [`Error::WorkersEnded`], since each worker that ended early is reported on
    /// its own.
    ///
    /// # Arguments
    ///
    /// * `step` - What the step was doing (`samples-list backup`, `tags census`)
    #[must_use]
    pub fn step_error(&self, step: &str) -> Error {
        // count the orders the dead workers held beside the ones nobody took
        let held = self
            .errors
            .iter()
            .filter(|failure| failure.order.is_some())
            .count();
        let errors: Vec<&str> = self
            .errors
            .iter()
            .map(|failure| failure.error.as_str())
            .collect();
        // list each worker's error once, in the order they were joined
        Error::WorkersEnded(format!(
            "{step} stopped: {} of its workers ended early holding {held} orders, with {} orders \
             left: {}",
            self.errors.len(),
            self.orders_left,
            errors.join("; ")
        ))
    }
}

/// Stops sending orders and joins every worker of a step
///
/// The controller's sender is replaced by a disconnected one and dropped, so workers drain the
/// queued orders and then see the channel end; nothing polls the queue.
///
/// # Arguments
///
/// * `orders_tx` - The controller's sender, replaced and dropped here
/// * `orders_rx` - A receiver of the same channel, used to count orders left behind
/// * `active` - The workers' join handles
pub async fn join_workers<W, O>(
    orders_tx: &mut AsyncSender<O>,
    orders_rx: &AsyncReceiver<O>,
    active: &mut FuturesUnordered<WorkerHandle<W, O>>,
) -> Joined<W, O> {
    // end the channel from the send side so workers stop once the queue is empty
    let (disconnected, _) = kanal::unbounded_async();
    drop(std::mem::replace(orders_tx, disconnected));
    // join every worker as it ends, keeping its result or its failure
    let mut finished = Vec::new();
    let mut errors = Vec::new();
    while let Some(worker) = active.next().await {
        match worker {
            Ok(Ok(worker)) => finished.push(worker),
            Ok(Err(failure)) => errors.push(failure),
            Err(error) => errors.push(WorkerFailure {
                error: format!("worker task ended: {error}"),
                order: None,
            }),
        }
    }
    // count the orders no worker took
    Joined {
        finished,
        errors,
        orders_left: orders_rx.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::{WorkerFailure, join_workers, spawn_worker};
    use crate::Error;
    use futures::stream::FuturesUnordered;
    use std::time::Duration;

    /// Workers that take every order finish, and the step succeeds
    #[tokio::test]
    async fn workers_that_drain_the_orders_finish() {
        // two workers summing the orders they take
        let (mut tx, rx) = kanal::unbounded_async::<u32>();
        let mut active = FuturesUnordered::new();
        for _ in 0..2 {
            active.push(spawn_worker(&rx, |orders| async move {
                let mut sum = 0;
                while let Ok(order) = orders.recv().await {
                    sum += order;
                }
                Ok::<u32, Error>(sum)
            }));
        }
        for order in 1..=10 {
            tx.send(order).await.unwrap();
        }
        // joining ends the channel and collects both workers
        let joined = join_workers(&mut tx, &rx, &mut active).await;
        assert_eq!(joined.finished.iter().sum::<u32>(), 55);
        assert!(joined.errors.is_empty() && !joined.step_failed());
        assert_eq!(joined.orders_left, 0);
    }

    /// When every worker dies with orders queued, joining returns at once and the step fails,
    /// naming each worker's error and the order it held
    #[tokio::test]
    async fn a_step_whose_workers_all_die_fails_at_once() {
        // one worker that dies on its first order, and one that panics on its first
        let (mut tx, rx) = kanal::unbounded_async::<u32>();
        let mut active = FuturesUnordered::new();
        active.push(spawn_worker(&rx, |orders| async move {
            let _ = orders.recv().await;
            Err::<u32, Error>(Error::new("disk full"))
        }));
        active.push(spawn_worker(&rx, |orders| async move {
            let _ = orders.recv().await;
            panic!("worker bug");
        }));
        for order in 0..100 {
            tx.send(order).await.unwrap();
        }
        // joining returns promptly, naming both errors and the orders left behind
        let joined = tokio::time::timeout(
            Duration::from_secs(5),
            join_workers(&mut tx, &rx, &mut active),
        )
        .await
        .expect("joining hung");
        assert!(joined.step_failed());
        assert_eq!(joined.orders_left, 98);
        // each dead worker comes back with the order it took
        let mut held: Vec<u32> = joined.errors.iter().filter_map(|f| f.order).collect();
        held.sort_unstable();
        assert_eq!(held, vec![0, 1], "{:?}", joined.errors);
        let error = joined.step_error("tags backup");
        assert!(matches!(error, Error::WorkersEnded(_)), "{error}");
        let error = error.to_string();
        assert!(
            error.contains("disk full")
                && error.contains("worker panicked: worker bug")
                && error.contains("holding 2 orders, with 98 orders left"),
            "{error}"
        );
    }

    /// A worker that dies while others drain the orders is reported without failing the step
    #[tokio::test]
    async fn one_dead_worker_does_not_fail_a_drained_step() {
        // one worker dies before taking an order, the other takes every order
        let (mut tx, rx) = kanal::unbounded_async::<u32>();
        let mut active = FuturesUnordered::new();
        active.push(spawn_worker(&rx, |_orders| async {
            Err::<u32, Error>(Error::new("bad archive"))
        }));
        active.push(spawn_worker(&rx, |orders| async move {
            let mut taken = 0;
            while orders.recv().await.is_ok() {
                taken += 1;
            }
            Ok(taken)
        }));
        for order in 0..10 {
            tx.send(order).await.unwrap();
        }
        // the error is kept with no order, but the step handled every order
        let joined = join_workers(&mut tx, &rx, &mut active).await;
        assert_eq!(
            joined.errors,
            vec![WorkerFailure {
                error: "bad archive".to_owned(),
                order: None,
            }]
        );
        assert_eq!(joined.finished, vec![10]);
        assert!(!joined.step_failed());
    }

    /// A worker that finished its order and dies before taking another holds no order
    #[tokio::test]
    async fn a_worker_dying_between_orders_holds_none() {
        // a worker that finishes one order, asks for the next, and dies once the channel ends
        let (mut tx, rx) = kanal::unbounded_async::<u32>();
        let mut active = FuturesUnordered::new();
        active.push(spawn_worker(&rx, |orders| async move {
            while orders.recv().await.is_ok() {}
            Err::<u32, Error>(Error::new("final flush failed"))
        }));
        tx.send(7).await.unwrap();
        // the order it finished isn't reported as lost
        let joined = join_workers(&mut tx, &rx, &mut active).await;
        assert_eq!(joined.errors.len(), 1);
        assert_eq!(joined.errors[0].order, None);
    }
}
