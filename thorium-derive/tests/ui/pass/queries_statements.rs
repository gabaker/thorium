//! Selects, deletes, updates, subset inserts, and hand-written statements with `eq(...)`, `r#in(...)`, grouping,
//! existence checks, limits, and chunk sizes compile against a table and a view named through a
//! `use`, and give typed methods
#![deny(warnings)]

/// The module declaring the objects
pub mod declared {
    use chrono::{DateTime, Utc};
    use thorium::models::schema::{ScyllaTable, ScyllaView};
    use uuid::Uuid;

    /// A table of harvests
    #[derive(ScyllaTable)]
    #[thorium_table(name = "harvests")]
    pub struct HarvestsTable {
        /// The partition column
        #[thorium(partition)]
        pub group: String,
        /// The first clustering column
        #[thorium(cluster = "desc")]
        pub at: DateTime<Utc>,
        /// The second clustering column
        #[thorium(cluster)]
        pub id: Uuid,
        /// A regular column
        pub crop: String,
    }

    /// Harvests by crop
    #[derive(ScyllaView)]
    #[thorium_view(name = "harvests_by_crop", base = HarvestsTable)]
    pub struct HarvestsByCropView {
        /// The crop
        #[thorium(partition)]
        pub crop: String,
        /// The group
        #[thorium(cluster)]
        pub group: String,
        /// The time
        #[thorium(cluster = "desc")]
        pub at: DateTime<Utc>,
        /// The id
        #[thorium(cluster)]
        pub id: Uuid,
    }
}

/// The rows and statements, naming the objects through a `use`
pub mod statements {
    use crate::declared::{HarvestsByCropView, HarvestsTable};
    use thorium::models::schema::ScyllaProjection;
    use uuid::Uuid;

    /// The id of a harvest
    #[derive(ScyllaProjection)]
    #[thorium_projection(of = HarvestsByCropView)]
    pub struct IdRow {
        /// The id
        pub id: Uuid,
    }

    /// When a harvest happened
    #[derive(ScyllaProjection)]
    #[thorium_projection(of = HarvestsTable)]
    pub struct AtRow {
        /// The time
        pub at: chrono::DateTime<chrono::Utc>,
    }

    /// Statements on the table
    #[thorium::table_queries(table = HarvestsTable)]
    pub struct HarvestsQueries {
        /// Whether a harvest exists
        #[select(eq(group, at, id), exists)]
        exists: Select,
        /// Delete one harvest from several groups
        #[delete(by_key, r#in(group), eq(at, id))]
        delete: Delete,
        /// Delete every harvest at one time
        #[delete(prefix, eq(group, at))]
        delete_at: Delete,
        /// Delete a group's harvests
        #[delete(partition, eq(group))]
        delete_group: Delete,
        /// Insert a harvest's key and crop
        #[insert(columns = [group, at, id, crop], ttl = none)]
        insert: Insert,
        /// Insert a harvest's key alone
        #[insert(columns = [group, at, id], ttl = none)]
        insert_key: Insert,
        /// Insert a harvest that never expires
        #[insert(columns = all, ttl = zero)]
        insert_forever: Insert,
        /// Insert a harvest with its own TTL
        #[insert(columns = all, ttl = bind)]
        insert_expiring: Insert,
        /// Insert a harvest's key with its own TTL
        #[insert(columns = [group, at, id], ttl = bind)]
        insert_key_expiring: Insert,
        /// The harvests of a group at or after a time, a bound number per partition
        #[select(project = super::statements::AtRow, eq(group), range(at, le), per_partition_limit)]
        since: Select,
        /// Change a harvest's crop
        #[update(set(crop), by_key, eq(group, at, id))]
        set_crop: Update,
        /// Change a harvest's crop by hand
        #[query(
            "UPDATE {ns}.harvests SET crop = ? WHERE group = ? AND at = ? AND id = ?",
            binds(crop: &str, group: &str, at: chrono::DateTime<chrono::Utc>, id: Uuid)
        )]
        set_crop_by_hand: Raw,
    }

    /// Statements on the view
    #[thorium::view_queries(view = HarvestsByCropView)]
    pub struct HarvestsByCropQueries {
        /// The ids of some crops in some groups
        #[select(project = IdRow, r#in(crop, group), group_by(crop, group), chunk = 50)]
        ids: Select,
        /// One id of a crop
        #[select(project = IdRow, eq(crop), limit = 1)]
        first: Select,
        /// Whether a crop is harvested in some groups
        #[select(eq(crop), r#in(group), exists, per_partition_limit = 1)]
        any: Select,
    }
}

/// Check the generated plans, statement texts, and subset insert bind structs
fn main() {
    // the generated plans and texts are reachable from a downstream crate
    let lists = statements::HarvestsByCropQueries::ids_in_lists([3, 4]);
    assert_eq!(lists[0].chunk, 50);
    let texts = statements::HarvestsQueries::statements("ks");
    assert_eq!(texts.len(), 12);
    // each subset insert has its own bind struct, named after its table and statement
    let row = statements::HarvestsInsert {
        group: "g",
        at: chrono::Utc::now(),
        id: uuid::Uuid::nil(),
        crop: "corn",
    };
    let key = statements::HarvestsInsertKeyInsert {
        group: row.group,
        at: row.at,
        id: row.id,
    };
    assert!(format!("{row:?} {key:?}").contains("corn"));
}
