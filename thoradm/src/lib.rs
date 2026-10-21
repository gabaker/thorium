//! The library behind thoradm, Thorium's admin CLI
//!
//! The binary is a thin wrapper around these modules. They are exposed as a library so the
//! integration tests in `tests/` can drive the backup, restore, and scrub controllers in-process.

// thoradm's `Error` wraps the large error types of its Scylla, S3, and API clients by value, so
// every fallible function would trip this lint
#![allow(clippy::result_large_err)]

pub mod args;
pub mod backup;
pub mod census;
pub mod error;
pub mod outcome;
pub mod provision;
pub mod settings;
pub mod shared;
pub mod users;

pub use error::Error;
