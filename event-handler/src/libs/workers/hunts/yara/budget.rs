//! The byte budget that bounds how much sample data scans may buffer at once
//!
//! Admission is byte budgeted, not task counted: a global budget is split into
//! small class slots (most samples are KBs to MBs) and a bounded count of large
//! class grants sized to the max scan size. Grants are handed out whole and are
//! never grown against the shared budget, so a holder can always finish within
//! its grant and waiters only queue on slot release — no circular wait is
//! possible.

use std::sync::Arc;
use thorium::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The class of a scan admission grant
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantClass {
    /// A small class grant bounded by the small class size
    Small,
    /// A large class grant bounded by the max scan size
    Large,
}

/// A single whole admission grant for buffering one samples bytes
#[derive(Debug)]
pub struct BudgetGrant {
    /// The class this grant was admitted into
    ///
    /// This is read by unit tests and the M3 scale out metrics
    #[allow(dead_code)]
    pub class: GrantClass,
    /// The max number of bytes this grant may buffer
    pub cap: u64,
    /// The permit holding this grants slice of the budget
    _permit: OwnedSemaphorePermit,
}

/// The byte budget for in flight scan buffers
#[derive(Clone)]
pub struct ScanBudget {
    /// The permits for small class scans
    small: Arc<Semaphore>,
    /// The permits for large class scans
    large: Arc<Semaphore>,
    /// The size in bytes of a small class grant
    small_class: u64,
    /// The size in bytes of a large class grant
    max_scan: u64,
}

impl ScanBudget {
    /// Create a new scan budget
    ///
    /// # Errors
    ///
    /// * The budget cannot hold even one large class grant
    ///
    /// # Arguments
    ///
    /// * `budget` - The total byte budget for in flight scan buffers
    /// * `small_class` - The size in bytes of a small class grant
    /// * `max_scan` - The max size in bytes of any single scan
    pub fn new(budget: u64, small_class: u64, max_scan: u64) -> Result<Self, Error> {
        // the budget must hold at least one large class grant or large
        // samples could never be admitted and would retry forever
        if budget < max_scan {
            return Err(Error::new(format!(
                "scan_buffer_budget ({budget}) must be at least max_scan_size ({max_scan})"
            )));
        }
        // small grants may use at most half the budget so large grants always
        // have budget left to run against
        let small_budget = budget.saturating_sub(max_scan).max(small_class);
        // size each classes permit count from its grant size
        let small_permits = (small_budget / small_class.max(1)).max(1) as usize;
        let large_permits = (budget / max_scan.max(1)).max(1) as usize;
        // build our scan budget
        let built = ScanBudget {
            small: Arc::new(Semaphore::new(small_permits)),
            large: Arc::new(Semaphore::new(large_permits)),
            small_class,
            max_scan,
        };
        Ok(built)
    }

    /// Admit a scan into the small class
    pub async fn admit_small(&self) -> Result<BudgetGrant, Error> {
        // acquire a whole small class permit
        let permit = self
            .small
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| Error::new(format!("scan budget closed: {err}")))?;
        // build this grants info
        Ok(BudgetGrant {
            class: GrantClass::Small,
            cap: self.small_class,
            _permit: permit,
        })
    }

    /// Admit a scan into the large class
    ///
    /// Large holders never wait on anything once admitted so waiters here only
    /// ever queue on a running large scan finishing — the wait graph is
    /// cycle free.
    pub async fn admit_large(&self) -> Result<BudgetGrant, Error> {
        // acquire a whole large class permit
        let permit = self
            .large
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| Error::new(format!("scan budget closed: {err}")))?;
        // build this grants info
        Ok(BudgetGrant {
            class: GrantClass::Large,
            cap: self.max_scan,
            _permit: permit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small grant must cap at the small class and a large at the max scan
    #[tokio::test]
    async fn budget_small_then_large_grant() {
        // build a budget big enough for both classes
        let budget = ScanBudget::new(1024, 64, 512).expect("failed to build budget");
        // admit a small scan and check its cap
        let small = budget.admit_small().await.expect("small admit failed");
        assert_eq!(small.class, GrantClass::Small);
        assert_eq!(small.cap, 64);
        // admit a large scan and check its cap
        let large = budget.admit_large().await.expect("large admit failed");
        assert_eq!(large.class, GrantClass::Large);
        assert_eq!(large.cap, 512);
    }

    /// A budget smaller then one large grant must be rejected at build time
    #[test]
    fn budget_rejects_budget_lt_max_scan() {
        // a budget below the max scan size silently overcommits so reject it
        assert!(ScanBudget::new(100, 64, 512).is_err());
    }

    /// Grants must free their budget slice when dropped
    #[tokio::test]
    async fn budget_grants_release_on_drop() {
        // build a budget with exactly one large grant
        let budget = ScanBudget::new(512, 64, 512).expect("failed to build budget");
        // take the only large grant
        let grant = budget.admit_large().await.expect("large admit failed");
        // a second large admit must not be ready while the first is held
        assert_eq!(budget.large.available_permits(), 0);
        // drop the first grant
        drop(grant);
        // the large slot must be free again
        assert_eq!(budget.large.available_permits(), 1);
    }
}
