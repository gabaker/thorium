//! A subset insert writes every key column
use thorium::models::schema::ScyllaTable;
use uuid::Uuid;

/// The table
#[derive(ScyllaTable)]
#[thorium_table(name = "base")]
pub struct BaseTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// The clustering column
    #[thorium(cluster)]
    pub id: Uuid,
    /// A regular column
    pub crop: String,
}

/// Statements on the table
#[thorium::table_queries(table = BaseTable)]
pub struct BaseQueries {
    /// An insert without the clustering column
    #[insert(columns = [group, crop], ttl = none)]
    insert: Insert,
}

fn main() {}
