//! Every cursor kind compiles against tables and a view named through a `use`, and implements
//! its family's statements trait
#![deny(warnings)]

/// The module declaring the objects
pub mod declared {
    use chrono::{DateTime, Utc};
    use thorium::models::schema::{ScyllaTable, ScyllaView};
    use uuid::Uuid;

    /// Harvests bucketed by time, listed by kind
    #[derive(ScyllaTable)]
    #[thorium_table(name = "harvests")]
    pub struct HarvestsTable {
        /// The kind of harvest
        #[thorium(partition)]
        pub kind: String,
        /// The group
        #[thorium(partition)]
        pub group: String,
        /// The year
        #[thorium(partition, year)]
        pub year: i32,
        /// The bucket
        #[thorium(partition, bucket(partition_size = "files.partition_size"))]
        pub bucket: i32,
        /// The farm
        #[thorium(partition)]
        pub farm: String,
        /// The time
        #[thorium(cluster = "asc", timestamp)]
        pub at: DateTime<Utc>,
        /// The id
        #[thorium(cluster = "asc")]
        pub id: Uuid,
        /// The crop
        pub crop: String,
    }

    /// Harvests by crop
    #[derive(ScyllaView)]
    #[thorium_view(name = "harvests_by_crop", base = HarvestsTable)]
    pub struct HarvestsByCropView {
        /// The crop
        #[thorium(partition)]
        pub crop: String,
        /// The year
        #[thorium(partition, year)]
        pub year: i32,
        /// The bucket
        #[thorium(partition, bucket(partition_size = "files.partition_size"))]
        pub bucket: i32,
        /// The kind
        #[thorium(cluster)]
        pub kind: String,
        /// The group
        #[thorium(cluster)]
        pub group: String,
        /// The farm
        #[thorium(cluster)]
        pub farm: String,
        /// The time
        #[thorium(cluster)]
        pub at: DateTime<Utc>,
        /// The id
        #[thorium(cluster)]
        pub id: Uuid,
    }

    /// Fields, listed by name within a farm
    #[derive(ScyllaTable)]
    #[thorium_table(name = "fields")]
    pub struct FieldsTable {
        /// The farm
        #[thorium(partition)]
        pub farm: String,
        /// The field's name
        #[thorium(cluster)]
        pub name: String,
        /// The field's id
        pub id: Uuid,
    }
}

/// The rows and statements, naming the objects through a `use`
pub mod statements {
    use crate::declared::{FieldsTable, HarvestsByCropView, HarvestsTable};
    use chrono::{DateTime, Utc};
    use thorium::models::schema::ScyllaProjection;
    use uuid::Uuid;

    /// A harvest in a listing
    #[derive(ScyllaProjection)]
    #[thorium_projection(of = HarvestsTable)]
    pub struct HarvestRow {
        /// The kind
        pub kind: String,
        /// The group
        pub group: String,
        /// The time
        pub at: DateTime<Utc>,
        /// The harvest's id
        #[thorium(column = "id")]
        pub harvest: Uuid,
    }

    /// A field in a listing
    #[derive(ScyllaProjection)]
    #[thorium_projection(of = FieldsTable)]
    pub struct FieldRow {
        /// The farm
        pub farm: String,
        /// The name
        pub name: String,
        /// The id
        pub id: Uuid,
    }

    /// Statements on the harvests table
    #[thorium::table_queries(table = HarvestsTable)]
    pub struct HarvestsQueries {
        /// Harvests of some kinds on one farm, newest first
        #[cursor(kind = time, group_by = group, r#in(kind), eq(farm), year = year,
                 bucket = bucket, range = at, tie = id, order = "desc", project = HarvestRow,
                 limit = per_partition)]
        list: Cursor,
        /// Harvests of one kind on one farm, newest first
        #[cursor(kind = time, group_by = group, eq(kind, farm), year = year, bucket = bucket,
                 range = at, tie = id, order = "desc", project = HarvestRow,
                 limit = per_partition)]
        one_kind: Cursor,
    }

    /// Statements on the harvests by crop view
    #[thorium::view_queries(view = HarvestsByCropView)]
    pub struct HarvestsByCropQueries {
        /// Whether any harvest of a crop exists in some buckets
        #[cursor(kind = exists, key = crop, year = year, bucket = bucket)]
        any: Cursor,
    }

    /// Statements on the fields table
    #[thorium::table_queries(table = FieldsTable)]
    pub struct FieldsQueries {
        /// The fields of one farm at a time
        #[cursor(kind = simple, partition = farm, tie = name, project = FieldRow, limit = limit)]
        list: Cursor,
        /// The fields of some farms, sorted by name
        #[cursor(kind = grouped, group_by = farm, sort = name, tie_breaker = id, project = FieldRow)]
        sorted: Cursor,
    }
}

use thorium::models::schema::TimeCursorStatements;

/// Require that a cursor implements its family's statements trait
fn time<T: thorium::models::schema::TimeCursorStatements>(_: &T) {}

/// Require an exists statement
fn exists<T: thorium::models::schema::ExistsStatement>(_: &T) {}

/// Require a simple cursor
fn simple<T: thorium::models::schema::SimpleCursorStatements>(_: &T) {}

/// Require a grouped cursor
fn grouped<T: thorium::models::schema::GroupedCursorStatements>(_: &T) {}

/// Check every generated cursor type against its trait
#[allow(dead_code)]
fn check(
    harvests: &statements::HarvestsQueries,
    crops: &statements::HarvestsByCropQueries,
    fields: &statements::FieldsQueries,
) {
    time(&harvests.list);
    time(&harvests.one_kind);
    exists(&crops.any);
    simple(&fields.list);
    grouped(&fields.sorted);
}

fn main() {
    // the statements are named after the cursor's field
    let names = statements::HarvestsQueries::statements("ks")
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["list_ties", "list_pull", "one_kind_ties", "one_kind_pull"]
    );
    // a tie's scope names its group, first among its `IN` filters or as the whole scope
    let scope = ("north".to_owned(), "wheat".to_owned());
    let group = <statements::HarvestsListCursor as TimeCursorStatements>::scope_group(&scope);
    assert_eq!(group, "north");
    let scope = "south".to_owned();
    let group = <statements::HarvestsOneKindCursor as TimeCursorStatements>::scope_group(&scope);
    assert_eq!(group, "south");
}
