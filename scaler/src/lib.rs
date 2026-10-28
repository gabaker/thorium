//! The scaler responsible for scaling workers for Thorium

mod args;
mod libs;

pub use libs::{Scaler, Spawned};

// these are only for tests
#[cfg(feature = "test-utilities")]
pub use libs::schedulers::dry_run::{DryRun, DryRunNode};

// expose test utilities if that feature is enabled
#[cfg(feature = "test-utilities")]
pub mod test_utilities;
