//! The test-only fixture catalog of the upgrade harness: revisions, steps, and transforms for the
//! pattern matrix, and the fixtures that build and read the objects they rewrite
//!
//! The catalog is passed to the direct driver as a parameter and never reaches a production
//! build. A test binary includes it with `mod fixture_catalog;` (or `#[path]` from another
//! directory) and uses [`scenario`] to start a scenario on [`MATRIX`] with every family's fixture
//! registered. The users example's `R1 -> R2` edge is [`USERS_THEME`]: it holds the production
//! transform of the `users-settings-theme` step between [`R1`], a pre-release that only this
//! catalog holds, and [`R2`], a release of the production catalog ([`USERS_BASE`] is the release
//! R1 builds on), and an edge from a pre-release to a later release passes `Catalog::check`.
#![allow(dead_code, unused_imports)]

pub mod catalog;
pub mod records;
pub mod transforms;

pub use catalog::{BASE, M1, M2, M3, M4, M5, M5B, MATRIX, R1, R2, USERS_BASE, USERS_THEME};
pub use records::{all_families, in_layout, scenario, seed};
pub use transforms::{ACL, FILES, ITEMS, OWNERS, PEOPLE, RANKS, SENTINEL, TAGS, TASKS, TOKENS};
