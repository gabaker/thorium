//! Unsigned columns with matching annotations and a signed column narrower than its value type
//! compile
#![deny(warnings)]

use std::collections::HashMap;
use thorium::models::schema::{ScyllaProjection, ScyllaTable};

/// A table with unsigned columns and an int bucket
#[derive(ScyllaTable)]
#[thorium_table(name = "counts")]
pub struct CountsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// A bucket computed with a checked conversion where it's set
    #[thorium(partition)]
    pub bucket: i32,
    /// An unsigned column stored as a bigint
    #[thorium(unsigned = "bigint")]
    pub count: u64,
    /// A small unsigned column stored as an int
    #[thorium(unsigned = "int")]
    pub small: u16,
    /// A map of unsigned values stored as bigints
    #[thorium(unsigned = "bigint")]
    pub per_kind: HashMap<String, usize>,
}

/// A row reading the unsigned columns with the table's annotations
#[derive(ScyllaProjection)]
#[thorium_projection(of = CountsTable)]
pub struct CountRow {
    /// The unsigned column
    #[thorium(unsigned = "bigint")]
    pub count: u64,
    /// The small unsigned column
    #[thorium(unsigned = "int")]
    pub small: u16,
    /// The map of unsigned values
    #[thorium(unsigned = "bigint")]
    pub per_kind: HashMap<String, usize>,
}

fn main() {}
