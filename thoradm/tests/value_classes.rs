//! The coverage meta-test: every column, `Backup` field, and object layout of every backed-up
//! entry holds every value class its type allows

mod common;

use common::classes::{
    CoverageMatrix, Location, MatrixSize, NEAR_LIMIT_COLUMNS, Need, ValueClass, report_dir,
    write_report,
};
use common::columns::{CqlType, Kind, Planner, live_table, stale_entries};
use common::fixture::{self, ValueClasses};
use common::harness::{Harness, redis_pool};
use common::schema::{BackupSchema, Node, Scalar};
use scylla::value::CqlValue;
use std::collections::BTreeMap;
use thoradm::backup::coverage::{Registry, Store};

/// With the fixture seeded, every required cell has at least one case and no rule is broken, and
/// each kind of gap the meta-test guards against makes it fail
#[tokio::test]
async fn every_required_cell_has_a_fixture_case() {
    // seed the broad and value-class fixtures
    let harness = Harness::get().await;
    fixture::broad(harness).await;
    let seeded = Box::pin(fixture::value_classes(harness)).await;
    // write the coverage report every harness run keeps
    write_report(&seeded.matrix, seeded.size, &report_dir()).unwrap();
    // no rule is broken and no required cell is missing
    assert!(
        seeded.problems.is_empty(),
        "rule violations:\n{}",
        seeded.problems.join("\n")
    );
    let missing = seeded.matrix.missing(seeded.size);
    let listed: Vec<String> = missing
        .iter()
        .map(|(location, class)| format!("{location} needs {class}"))
        .collect();
    assert!(
        missing.is_empty(),
        "{} cells have no case:\n{}",
        missing.len(),
        listed.join("\n")
    );
    // each gap the meta-test guards against is caught, naming what is missing
    a_removed_case_is_missing(&seeded);
    a_new_backup_field_is_missing(&seeded);
    an_opaque_node_is_a_problem();
    near_limit_cells_are_full_only(&seeded);
    a_json_column_without_a_domain_is_a_problem(harness).await;
    a_stale_list_entry_is_a_problem(harness).await;
    an_unmapped_hash_field_is_a_problem(harness).await;
    a_new_column_is_missing(harness).await;
}

/// Removing every case of one covered cell makes that cell missing, and only that cell
///
/// # Arguments
///
/// * `seeded` - The seeded fixture's matrix
fn a_removed_case_is_missing(seeded: &ValueClasses) {
    // drop one column cell's and one field cell's cases
    for prefix in ["Column", "Field"] {
        let (key, _) = seeded
            .matrix
            .needs
            .iter()
            .find(|((location, _), need)| {
                **need == Need::Required && format!("{location:?}").starts_with(prefix)
            })
            .expect("a required cell");
        let mut matrix = seeded.matrix.clone();
        matrix.observed.remove(key);
        assert_eq!(
            matrix.missing(seeded.size),
            vec![key.clone()],
            "removing {key:?}"
        );
    }
}

/// Adds a property to the item node of one of `Backup`'s top-level arrays
///
/// # Arguments
///
/// * `tree` - The walked `Backup` schema
/// * `part` - The top-level array
/// * `prop` - The property to add
fn add_property(tree: &mut Node, part: &str, prop: (String, bool, Node)) {
    // find the part's item object and append the property
    let Node::Object(props) = tree else {
        panic!("Backup isn't an object");
    };
    let (_, _, node) = props
        .iter_mut()
        .find(|(name, ..)| name == part)
        .expect("a Backup part");
    let Node::Array(item) = node else {
        panic!("{part} isn't a list");
    };
    let Node::Object(item_props) = item.as_mut() else {
        panic!("{part} items aren't objects");
    };
    item_props.push(prop);
}

/// A field added to a `Backup` part needs cases the fixture doesn't have yet
///
/// # Arguments
///
/// * `seeded` - The seeded fixture's matrix
fn a_new_backup_field_is_missing(seeded: &ValueClasses) {
    // add a text field to users and derive the requirements again
    let mut tree = BackupSchema::load().tree();
    add_property(
        &mut tree,
        "users",
        (
            "added_field".to_owned(),
            true,
            Node::Scalar(Scalar::String(None), false),
        ),
    );
    let mut matrix = CoverageMatrix::default();
    let mut opaque = Vec::new();
    common::schema::require(&tree, "", &mut matrix, &mut opaque);
    matrix.observed.clone_from(&seeded.matrix.observed);
    // only the new field's cells are missing
    let missing = matrix.missing(seeded.size);
    let path = Location::Field {
        path: ".users[].added_field".to_owned(),
    };
    assert!(!missing.is_empty(), "the new field needs no cases");
    assert!(
        missing.iter().all(|(location, _)| *location == path),
        "{missing:?}"
    );
}

/// An object with no properties in the `Backup` schema is a problem, named by its path
fn an_opaque_node_is_a_problem() {
    // add an opaque field to images and collect the opaque nodes
    let mut tree = BackupSchema::load().tree();
    add_property(
        &mut tree,
        "images",
        ("hidden".to_owned(), true, Node::Opaque),
    );
    let mut matrix = CoverageMatrix::default();
    let mut opaque = Vec::new();
    common::schema::require(&tree, "", &mut matrix, &mut opaque);
    assert_eq!(opaque, vec![".images[].hidden".to_owned()]);
}

/// Near-cell-limit cells are covered by a full seed and reported as full only by a ci seed, and
/// without their cases they fail the full matrix
///
/// # Arguments
///
/// * `seeded` - The seeded fixture's matrix
fn near_limit_cells_are_full_only(seeded: &ValueClasses) {
    // every listed column's near-limit cell is covered in full and full only in ci
    for (table, column) in NEAR_LIMIT_COLUMNS {
        let state = seeded.matrix.state(
            &Location::column(table, column),
            &ValueClass::NearCellLimit,
            seeded.size,
        );
        let expected = match seeded.size {
            MatrixSize::Full => matches!(state, common::classes::CellState::Covered(_)),
            MatrixSize::Ci => state == common::classes::CellState::FullOnly,
        };
        assert!(
            expected,
            "{table}.{column} is {state:?} in the {:?} matrix",
            seeded.size
        );
    }
    // without near-limit cases the full matrix misses exactly those cells
    let mut unseeded = seeded.matrix.clone();
    unseeded
        .observed
        .retain(|(_, class), _| *class != ValueClass::NearCellLimit);
    let full = unseeded.missing(MatrixSize::Full);
    assert_eq!(full.len(), NEAR_LIMIT_COLUMNS.len(), "{full:?}");
    assert!(
        full.iter()
            .all(|(_, class)| *class == ValueClass::NearCellLimit),
        "{full:?}"
    );
}

/// JSON in a text column with no `COLUMN_DOMAINS` entry is a problem, naming the column
///
/// # Arguments
///
/// * `harness` - The harness naming the keyspace
async fn a_json_column_without_a_domain_is_a_problem(harness: &Harness) {
    // find a free text column of a backed-up table
    let tables = fixture::backed_up_tables(harness).await;
    let (table, column) = tables
        .iter()
        .find_map(|table| {
            table
                .columns
                .iter()
                .find(|column| {
                    column.cql == CqlType::Text
                        && column.kind == Kind::Regular
                        && common::classes::column_domain(&table.name, &column.name).is_none()
                        && common::classes::derived_source(&table.name, &column.name).is_none()
                })
                .map(|column| (table, column.name.clone()))
        })
        .expect("a free text column");
    // observe a row holding a JSON object there
    let row: BTreeMap<String, Option<CqlValue>> = BTreeMap::from([(
        column.clone(),
        Some(CqlValue::Text("{\"hidden\":1}".to_owned())),
    )]);
    let lists = common::enums::all();
    let planner = Planner::new(&harness.conf, &lists, MatrixSize::Ci, 0);
    let mut matrix = CoverageMatrix::default();
    let mut problems = Vec::new();
    common::columns::observe(table, &[row], &lists, &planner, &mut matrix, &mut problems);
    let expected = format!(
        "{}.{column} holds JSON but has no COLUMN_DOMAINS entry",
        table.name
    );
    assert!(problems.contains(&expected), "{problems:?}");
}

/// A list entry naming a column the table doesn't have is a problem, naming the entry
///
/// # Arguments
///
/// * `harness` - The harness naming the keyspace
async fn a_stale_list_entry_is_a_problem(harness: &Harness) {
    // check a made-up entry against a real table
    let tables = fixture::backed_up_tables(harness).await;
    let table = &tables[0];
    let problems = stale_entries(table, &[(table.name.as_str(), "no_such_column")]);
    assert_eq!(
        problems,
        vec![format!(
            "a list names {}.no_such_column, which doesn't exist",
            table.name
        )]
    );
}

/// A hash field of a backed-up Redis family that maps to no schema path is a problem
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn an_unmapped_hash_field_is_a_problem(harness: &Harness) {
    // add a field to one user's hash
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let ns = &harness.conf.thorium.namespace;
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{ns}:user_data:*"))
        .query_async(&mut *conn)
        .await
        .unwrap();
    let key = keys.first().expect("a seeded user").clone();
    let _: () = redis::cmd("HSET")
        .arg(&key)
        .arg("unmapped_field")
        .arg("1")
        .query_async(&mut *conn)
        .await
        .unwrap();
    let unmapped = common::records::unmapped_fields(harness, &BackupSchema::load().tree()).await;
    let _: () = redis::cmd("HDEL")
        .arg(&key)
        .arg("unmapped_field")
        .query_async(&mut *conn)
        .await
        .unwrap();
    // the field is named with its family
    assert!(
        unmapped
            .iter()
            .any(|problem| problem.starts_with("user_data field \"unmapped_field\"")),
        "{unmapped:?}"
    );
}

/// A column added to a backed-up table needs cases the fixture doesn't have yet
///
/// # Arguments
///
/// * `harness` - The harness naming the keyspace
async fn a_new_column_is_missing(harness: &Harness) {
    // add a column to the first backed-up table and read its live columns again
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let name = Registry::backed_up(Store::ScyllaTable)
        .next()
        .expect("a backed-up table")
        .name;
    scylla
        .query_unpaged(format!("ALTER TABLE {ns}.{name} ADD added_column text"), ())
        .await
        .expect("failed to add a column");
    let table = live_table(&scylla, ns, name).await;
    scylla
        .query_unpaged(format!("ALTER TABLE {ns}.{name} DROP added_column"), ())
        .await
        .expect("failed to drop the column");
    // the new column's cells are required and missing
    let lists = common::enums::all();
    let mut matrix = CoverageMatrix::default();
    common::columns::require(&table, &lists, MatrixSize::Ci, &mut matrix);
    let added = Location::column(name, "added_column");
    let missing = matrix.missing(MatrixSize::Ci);
    assert!(
        missing
            .iter()
            .any(|(location, class)| *location == added && *class == ValueClass::Null),
        "{missing:?}"
    );
    assert!(
        missing
            .iter()
            .any(|(location, class)| *location == added && *class == ValueClass::Large),
        "{missing:?}"
    );
}
