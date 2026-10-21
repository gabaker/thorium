//! The shapes builder: structural cases a single value can't show
//!
//! For every backed-up table it writes a one-row partition, a dense partition, a partition larger
//! than the archive writer's flush size, a partition of tombstones only, a row with a deleted cell,
//! a row shared by many groups, and rows at time-partition boundaries; it also writes a result
//! with many result files and notifications with TTLs. The namespace it seeds uses one-second
//! partitions, so neighboring times fall in different buckets.

use super::classes::{
    DERIVED_COLUMNS, MatrixSize, TEST_ROLLOVER, UNSET_UNREPRESENTABLE, column_domain,
    derived_source, large,
};
use super::columns::{CqlType, Kind, Planner, RowPlan, Table, live_table, write_rows};
use super::harness::Harness;
use super::objects::write_objects;
use chrono::{DateTime, Utc};
use scylla::value::{CqlTimestamp, CqlValue};
use std::collections::BTreeMap;
use thoradm::backup::coverage::{Registry, Store};
use thorium::Conf;

/// How many groups share one row
pub const SHARED_GROUPS: usize = 50;

/// How many result files one result names
pub const MANY_OBJECTS: usize = 300;

/// How many bytes the archive writer gathers before it writes to disk
pub const FLUSH_BYTES: usize = 1_048_576;

/// The times rows are written at to cross time-partition boundaries: the last millisecond of a
/// leap year, the first of the next year, a leap day, and two times one second apart
pub const BOUNDARY_TIMES: &[&str] = &[
    "2024-12-31T23:59:59.999Z",
    "2025-01-01T00:00:00.000Z",
    "2024-02-29T12:00:00.000Z",
    "2025-06-15T12:00:00.000Z",
    "2025-06-15T12:00:01.000Z",
];

/// Gives every time-partitioned data type one-second partitions
///
/// # Arguments
///
/// * `conf` - The namespace's config
pub fn one_second_partitions(conf: &mut Conf) {
    // every partition size the API derives buckets from
    conf.thorium.files.partition_size = 1;
    conf.thorium.results.partition_size = 1;
    conf.thorium.repos.partition_size = 1;
    conf.thorium.events.partition_size = 1;
    conf.thorium.entities.partition_size = 1;
    conf.thorium.associations.partition_size = 1;
    conf.thorium.tags.files.partition_size = 1;
    conf.thorium.tags.repos.partition_size = 1;
    conf.thorium.tags.entities.partition_size = 1;
}

/// Returns how many rows a dense partition holds in a matrix size
///
/// # Arguments
///
/// * `size` - The matrix size
#[must_use]
pub fn dense_rows(size: MatrixSize) -> usize {
    // capped in ci; in full, more than one 5,000-row page of the driver's range reader
    match size {
        MatrixSize::Ci => 300,
        MatrixSize::Full => 6_000,
    }
}

/// Returns the `index`th distinct value of a value's type, if the type allows it
///
/// # Arguments
///
/// * `value` - The value to vary
/// * `index` - Which distinct value to make
fn vary(value: &CqlValue, index: usize) -> Option<CqlValue> {
    // text gains a suffix, numbers and times an offset, ids a fresh value
    let offset = i64::try_from(index).ok()?;
    match value {
        CqlValue::Text(text) => Some(CqlValue::Text(format!("{text}-{index}"))),
        CqlValue::Ascii(text) => Some(CqlValue::Ascii(format!("{text}-{index}"))),
        CqlValue::Int(number) => Some(CqlValue::Int(
            number.checked_add(i32::try_from(index).ok()?)?,
        )),
        CqlValue::BigInt(number) => Some(CqlValue::BigInt(number.checked_add(offset)?)),
        CqlValue::Timestamp(CqlTimestamp(millis)) => Some(CqlValue::Timestamp(CqlTimestamp(
            millis.checked_add(offset)?,
        ))),
        CqlValue::Uuid(_) => Some(CqlValue::Uuid(uuid::Uuid::new_v4())),
        _ => None,
    }
}

/// Returns the clustering column a dense partition varies: one with no domain and no derivation
///
/// # Arguments
///
/// * `table` - The table's live columns
/// * `row` - An ordinary row of the table
fn varied_column(table: &Table, row: &BTreeMap<String, CqlValue>) -> Option<String> {
    // the first clustering column whose value can take many free values
    table
        .columns
        .iter()
        .filter(|column| column.kind == Kind::Clustering)
        .filter(|column| column_domain(&table.name, &column.name).is_none())
        .filter(|column| derived_source(&table.name, &column.name).is_none())
        .find(|column| {
            row.get(&column.name)
                .and_then(|value| vary(value, 1))
                .is_some()
        })
        .map(|column| column.name.clone())
}

/// Returns a regular text column that takes free text and keys no view, for large values
///
/// # Arguments
///
/// * `table` - The table's live columns
fn free_text_column(table: &Table) -> Option<String> {
    // a plain text column with no domain or derivation
    table
        .columns
        .iter()
        .filter(|column| {
            column.kind == Kind::Regular && column.cql == CqlType::Text && !column.view_key
        })
        .filter(|column| column_domain(&table.name, &column.name).is_none())
        .find(|column| derived_source(&table.name, &column.name).is_none())
        .map(|column| column.name.clone())
}

/// Returns a regular column whose deletion leaves a row the archive can still hold
///
/// # Arguments
///
/// * `table` - The table's live columns
/// * `row` - An ordinary row of the table
fn deletable_column(table: &Table, row: &BTreeMap<String, CqlValue>) -> Option<String> {
    // a set regular column that isn't required by the archive struct or derived
    table
        .columns
        .iter()
        .filter(|column| column.kind == Kind::Regular && row.contains_key(&column.name))
        .filter(|column| {
            !UNSET_UNREPRESENTABLE.contains(&(table.name.as_str(), column.name.as_str()))
        })
        .find(|column| derived_source(&table.name, &column.name).is_none())
        .map(|column| column.name.clone())
}

/// Builds a statement and values that select one row or partition by its key columns
///
/// # Arguments
///
/// * `table` - The table's live columns
/// * `row` - The row's values
/// * `clustering` - Whether the clustering columns are part of the selection
fn key_filter(
    table: &Table,
    row: &BTreeMap<String, CqlValue>,
    clustering: bool,
) -> (String, Vec<CqlValue>) {
    // every partition key column, and the clustering columns when a single row is meant
    let keys: Vec<&String> = table
        .columns
        .iter()
        .filter(|column| {
            column.kind == Kind::Partition || (clustering && column.kind == Kind::Clustering)
        })
        .map(|column| &column.name)
        .collect();
    let filter = keys
        .iter()
        .map(|name| format!("\"{name}\" = ?"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let values = keys
        .iter()
        .filter_map(|name| row.get(*name).cloned())
        .collect();
    (filter, values)
}

/// The rows and deletions of one table's shapes
struct TableShapes {
    /// The rows to write, with their objects
    plans: Vec<RowPlan>,
    /// The rows to write and then delete, which get no objects: an object no row names is out
    /// of every backup, as Thorium deletes an object with the last row naming it
    doomed: Vec<RowPlan>,
    /// Statements with their values, run after the rows are written
    deletes: Vec<(String, Vec<CqlValue>)>,
}

/// Builds every shape of one table
///
/// # Arguments
///
/// * `planner` - The planner that builds ordinary rows
/// * `table` - The table's live columns
/// * `keyspace` - The keyspace
/// * `size` - The matrix size
fn table_shapes(
    planner: &mut Planner<'_>,
    table: &Table,
    keyspace: &str,
    size: MatrixSize,
) -> TableShapes {
    // a partition of one row
    let name = table.name.clone();
    let mut shapes = TableShapes {
        plans: Vec::new(),
        doomed: Vec::new(),
        deletes: Vec::new(),
    };
    let plan = |values: BTreeMap<String, CqlValue>| RowPlan {
        table: name.clone(),
        values,
        body: None,
    };
    shapes.plans.push(plan(planner.base_row(table)));
    // a dense partition, a partition past the flush size, and a partition of tombstones only
    let base = planner.base_row(table);
    if let Some(column) = varied_column(table, &base) {
        let dense = dense_rows(size);
        for index in 0..dense {
            let mut values = base.clone();
            values.insert(
                column.clone(),
                vary(&base[&column], index).expect("the column varies"),
            );
            planner.derive(&name, &mut values);
            shapes.plans.push(plan(values));
        }
        if let Some(text) = free_text_column(table) {
            let big = planner.base_row(table);
            let rows = FLUSH_BYTES / TEST_ROLLOVER * 2 + 1;
            for index in 0..rows {
                let mut values = big.clone();
                values.insert(
                    column.clone(),
                    vary(&big[&column], index).expect("the column varies"),
                );
                values.insert(
                    text.clone(),
                    CqlValue::Text(large(&format!("{name}-{index}"), TEST_ROLLOVER)),
                );
                planner.derive(&name, &mut values);
                shapes.plans.push(plan(values));
            }
        }
        let gone = planner.base_row(table);
        for index in 0..3 {
            let mut values = gone.clone();
            values.insert(
                column.clone(),
                vary(&gone[&column], index).expect("the column varies"),
            );
            planner.derive(&name, &mut values);
            shapes.doomed.push(plan(values));
        }
        let (filter, values) = key_filter(table, &gone, false);
        shapes.deletes.push((
            format!("DELETE FROM {keyspace}.{name} WHERE {filter}"),
            values,
        ));
    }
    // a row with one deleted cell
    let cell = planner.base_row(table);
    if let Some(column) = deletable_column(table, &cell) {
        let (filter, values) = key_filter(table, &cell, true);
        shapes.deletes.push((
            format!("DELETE \"{column}\" FROM {keyspace}.{name} WHERE {filter}"),
            values,
        ));
        shapes.plans.push(plan(cell));
    }
    // one row shared by many groups
    if table
        .columns
        .iter()
        .any(|column| column.name == "group" && column.kind == Kind::Partition)
    {
        let shared = planner.base_row(table);
        for index in 0..SHARED_GROUPS {
            let mut values = shared.clone();
            values.insert(
                "group".to_owned(),
                CqlValue::Text(format!("shared-group-{index}")),
            );
            planner.derive(&name, &mut values);
            shapes.plans.push(plan(values));
        }
    }
    // rows at time-partition boundaries
    shapes
        .plans
        .extend(boundary_rows(planner, table).into_iter().map(plan));
    shapes
}

/// Builds rows whose partitioning time sits at each boundary time, for tables partitioned by time
///
/// # Arguments
///
/// * `planner` - The planner that builds ordinary rows
/// * `table` - The table's live columns
fn boundary_rows(planner: &mut Planner<'_>, table: &Table) -> Vec<BTreeMap<String, CqlValue>> {
    // the column the table's year and bucket are derived from, if it has one
    let Some((_, _, source, _)) = DERIVED_COLUMNS
        .iter()
        .find(|(listed, ..)| *listed == table.name)
    else {
        return Vec::new();
    };
    // one row per boundary time, with its partition columns derived again
    BOUNDARY_TIMES
        .iter()
        .map(|time| {
            let millis = time
                .parse::<DateTime<Utc>>()
                .expect("a boundary time parses")
                .timestamp_millis();
            let mut values = planner.base_row(table);
            values.insert(
                (*source).to_owned(),
                CqlValue::Timestamp(CqlTimestamp(millis)),
            );
            planner.derive(&table.name, &mut values);
            values
        })
        .collect()
}

/// Seeds every shape into a harness's namespace
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
///
/// # Panics
///
/// Panics if a row, deletion, or object is refused.
pub async fn seed(harness: &Harness) {
    // every backed-up table's shapes
    let size = MatrixSize::from_env();
    let lists = super::enums::all();
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let mut planner = Planner::new(&harness.conf, &lists, size, 0);
    let mut plans = Vec::new();
    let mut doomed = Vec::new();
    let mut deletes = Vec::new();
    for entry in Registry::backed_up(Store::ScyllaTable) {
        let table = live_table(&scylla, ns, entry.name).await;
        let shapes = table_shapes(&mut planner, &table, ns, size);
        plans.extend(shapes.plans);
        doomed.extend(shapes.doomed);
        deletes.extend(shapes.deletes);
    }
    // a result naming many result files
    let results = live_table(&scylla, ns, "results").await;
    let mut many = planner.base_row(&results);
    let names = (0..MANY_OBJECTS)
        .map(|index| CqlValue::Text(format!("many-{index}.txt")))
        .collect();
    many.insert("files".to_owned(), CqlValue::Set(names));
    planner.derive("results", &mut many);
    plans.push(RowPlan {
        table: "results".to_owned(),
        values: many,
        body: None,
    });
    // write the rows, the objects of those that stay, and then the deletions
    write_rows(&scylla, ns, &plans).await;
    write_rows(&scylla, ns, &doomed).await;
    write_objects(&harness.s3(), &harness.conf, &plans).await;
    for (statement, values) in deletes {
        scylla
            .query_unpaged(statement.clone(), values)
            .await
            .unwrap_or_else(|err| panic!("Scylla refused {statement}: {err}"));
    }
}

/// Rewrites one notification with a TTL
///
/// # Arguments
///
/// * `harness` - The harness naming the keyspace
/// * `skip` - How many notifications to pass over, so each call rewrites another
/// * `ttl` - The TTL in seconds
///
/// # Panics
///
/// Panics if no notification exists or Scylla refuses the write.
pub async fn notification_ttl(harness: &Harness, skip: usize, ttl: i32) {
    // copy a notification back over itself with the TTL
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let rows: Vec<(String,)> = scylla
        .query_unpaged(format!("SELECT JSON * FROM {ns}.notifications"), ())
        .await
        .expect("failed to read notifications")
        .into_rows_result()
        .expect("notifications have rows")
        .rows::<(String,)>()
        .expect("notification rows decode")
        .filter_map(Result::ok)
        .collect();
    let (row,) = rows
        .into_iter()
        .nth(skip)
        .expect("the fixture has enough notifications");
    scylla
        .query_unpaged(
            format!("INSERT INTO {ns}.notifications JSON ? USING TTL {ttl}"),
            (row,),
        )
        .await
        .expect("failed to give a notification a TTL");
}
