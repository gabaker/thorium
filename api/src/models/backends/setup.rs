//! Sets up the connection pool for the configured backend

mod elastic_setup;
pub mod redis_setup;
mod scylla_setup;

pub use elastic_setup::elastic;
pub use redis_setup::redis;
pub use scylla_setup::{Scylla, SnapshotLoader};
