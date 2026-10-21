//! Views and projections over a table declared in another module compile, whether the table is
//! named through its full path or through a `use`
#![deny(warnings)]

/// The module declaring the table, re-exporting it from a private submodule
pub mod declared {
    /// The private module holding the table
    mod inner {
        use thorium::models::schema::ScyllaTable;
        use uuid::Uuid;

        /// A table declared away from the rows that read it
        #[derive(ScyllaTable)]
        #[thorium_table(name = "harvests")]
        pub struct HarvestsTable {
            /// The partition column
            #[thorium(partition)]
            pub group: String,
            /// The clustering column
            #[thorium(cluster)]
            pub id: Uuid,
            /// A regular column
            pub crop: String,
        }
    }

    pub use inner::HarvestsTable;
}

/// Rows and views that name the table through its full path
pub mod by_path {
    use thorium::models::schema::{ScyllaProjection, ScyllaView};
    use uuid::Uuid;

    /// The crop of each harvest, through the table's full path
    #[derive(ScyllaProjection)]
    #[thorium_projection(of = crate::declared::HarvestsTable)]
    pub struct CropRow {
        /// The crop
        pub crop: String,
    }

    /// Harvests by crop, over the table's full path
    #[derive(ScyllaView)]
    #[thorium_view(name = "harvests_by_crop", base = crate::declared::HarvestsTable)]
    pub struct HarvestsByCropView {
        /// The crop
        #[thorium(partition)]
        pub crop: String,
        /// The group
        #[thorium(cluster)]
        pub group: String,
        /// The id
        #[thorium(cluster)]
        pub id: Uuid,
    }
}

/// Rows that name the table through a `use`
pub mod by_use {
    use crate::declared::HarvestsTable;
    use thorium::models::schema::ScyllaProjection;
    use uuid::Uuid;

    /// The id of each harvest, through an imported name
    #[derive(ScyllaProjection)]
    #[thorium_projection(of = HarvestsTable)]
    pub struct IdRow {
        /// The id
        pub id: Uuid,
    }

    /// The crop and id, reading the table and the view in another module
    #[derive(ScyllaProjection)]
    #[thorium_projection(of(HarvestsTable, crate::by_path::HarvestsByCropView))]
    pub struct CropIdRow {
        /// The crop
        pub crop: String,
        /// The id
        pub id: Uuid,
    }
}

fn main() {
    // the rows read their columns by name
    assert_eq!(
        <by_path::CropRow as thorium::models::schema::Projection>::COLUMNS,
        &["crop"]
    );
    assert_eq!(
        <by_use::IdRow as thorium::models::schema::Projection>::COLUMNS,
        &["id"]
    );
    assert_eq!(
        <by_use::CropIdRow as thorium::models::schema::Projection>::COLUMNS,
        &["crop", "id"]
    );
}
