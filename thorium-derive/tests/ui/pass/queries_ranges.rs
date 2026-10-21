//! Census statements, token-range selects (plain, distinct, and paged), and paged selects with
//! `IN` lists compile against a table and give typed methods
#![deny(warnings)]

use chrono::{DateTime, Utc};
use thorium::models::schema::{ScyllaProjection, ScyllaTable};
use uuid::Uuid;

/// A table of harvests
#[derive(ScyllaTable)]
#[thorium_table(name = "harvests")]
pub struct HarvestsTable {
    /// The first partition column
    #[thorium(partition)]
    pub group: String,
    /// The second partition column
    #[thorium(partition)]
    pub year: i32,
    /// The first clustering column
    #[thorium(cluster = "desc")]
    pub at: DateTime<Utc>,
    /// The second clustering column
    #[thorium(cluster)]
    pub id: Uuid,
    /// A regular column
    pub crop: String,
}

/// The number of harvests in a partition
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct CountRow {
    /// The group
    pub group: String,
    /// The year
    pub year: i32,
    /// How many harvests the partition holds
    #[thorium(expr = "count(*)")]
    pub count: i64,
}

/// The number of harvests in a group's partitions
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct GroupCountRow {
    /// The group
    pub group: String,
    /// How many harvests the group holds
    #[thorium(expr = "count(*)")]
    pub count: i64,
}

/// A partition's key
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct KeyRow {
    /// The group
    pub group: String,
    /// The year
    pub year: i32,
}

/// A harvest's crop
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct CropRow {
    /// The id
    pub id: Uuid,
    /// The crop
    pub crop: String,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// The harvests in each partition of a token range
    #[census(project = CountRow)]
    census: Census,
    /// The harvests in each group of a token range
    #[census(project = GroupCountRow, group_by(group))]
    census_groups: Census,
    /// Every harvest's crop in a token range
    #[select(project = CropRow, token_range)]
    scan: Select,
    /// Every partition in a token range, each once, a page at a time
    #[select(project = KeyRow, distinct, token_range, paged)]
    partitions: Select,
    /// The crops of a group in some years, a page at a time
    #[select(project = CropRow, eq(group), r#in(year), paged)]
    crops: Select,
}

/// Check the text of the census, scan, and listing statements the queries struct generates
fn main() {
    // the census groups by the partition key unless told otherwise
    let census = HarvestsQueries::census_cql("ks");
    assert_eq!(
        census,
        "SELECT group, year, count(*) AS count FROM ks.harvests WHERE token(group, year) >= ? \
         AND token(group, year) <= ? GROUP BY group, year"
    );
    assert!(HarvestsQueries::census_groups_cql("ks").ends_with("GROUP BY group"));
    assert!(HarvestsQueries::partitions_cql("ks").starts_with("SELECT DISTINCT group, year "));
    assert!(HarvestsQueries::scan_cql("ks").starts_with("SELECT id, crop FROM ks.harvests WHERE"));
    assert!(HarvestsQueries::crops_cql("ks").ends_with("WHERE group = ? AND year IN ?"));
    assert_eq!(HarvestsQueries::statements("ks").len(), 5);
}
