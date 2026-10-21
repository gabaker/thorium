//! Format 2 carries the columns format 1 dropped and the notifications' remaining TTLs, so a
//! backup and restore brings every one of them back
//!
//! The tests restore, so each holds the binary's restore guard.

mod common;

use common::classes::MatrixSize;
use common::columns::{Planner, RowPlan, live_table, write_rows};
use common::cycle::{backup, describe, restore, wipe};
use common::harness::Harness;
use common::objects::write_objects;
use scylla::client::session::Session;
use scylla::value::{CqlTimestamp, CqlValue, Row};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Plans an ordinary row of a table with some of its columns set to given values
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `planner` - The planner that fills in the other columns
/// * `table` - The table
/// * `set` - The columns to set, with their values
async fn row_with(
    harness: &Harness,
    planner: &mut Planner<'_>,
    table: &str,
    set: Vec<(&str, CqlValue)>,
) -> RowPlan {
    // an ordinary row with the given columns replaced, its derived columns recomputed
    let scylla = harness.scylla().await;
    let live = live_table(&scylla, &harness.conf.thorium.namespace, table).await;
    let mut values = planner.base_row(&live);
    for (column, value) in set {
        values.insert(column.to_owned(), value);
    }
    planner.derive(table, &mut values);
    RowPlan {
        table: table.to_owned(),
        values,
        body: None,
    }
}

/// Reads columns of the rows of a table whose key column holds a value
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `ns` - The keyspace
/// * `table` - The table
/// * `columns` - The columns to read
/// * `key` - The key column and the value it must hold
async fn read_columns(
    scylla: &Session,
    ns: &str,
    table: &str,
    columns: &[&str],
    key: (&str, &CqlValue),
) -> Vec<BTreeMap<String, Option<CqlValue>>> {
    // read the whole table and keep the rows the key picks out
    let statement = format!(
        "SELECT {}, \"{}\" FROM {ns}.{table}",
        columns.join(", "),
        key.0
    );
    let rows: Vec<Row> = scylla
        .query_unpaged(statement, ())
        .await
        .unwrap_or_else(|err| panic!("failed to read {table}: {err}"))
        .into_rows_result()
        .unwrap()
        .rows::<Row>()
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows.into_iter()
        .filter(|row| row.columns.last().and_then(Option::as_ref) == Some(key.1))
        .map(|row| {
            columns
                .iter()
                .map(|column| (*column).to_owned())
                .zip(row.columns)
                .collect()
        })
        .collect()
}

/// A table, the column that picks out its row, that column's value, and the columns set with
/// their values
type Expected = (
    &'static str,
    &'static str,
    CqlValue,
    Vec<(&'static str, CqlValue)>,
);

/// A result's `tool_version`, `children`, and `entities`, a results stream row's
/// `tool_version`, a repo's `default_checkout` and `earliest`, and an association's
/// `submissions_source` and `submissions_other` come back with every value after a backup and
/// restore
#[tokio::test]
async fn dropped_columns_come_back() {
    // one row of each table with every column format 1 dropped set
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let scylla = harness.scylla().await;
    let ns = harness.conf.thorium.namespace.clone();
    let lists = common::enums::all();
    let mut planner = Planner::new(&harness.conf, &lists, MatrixSize::Ci, 0);
    let result_id = CqlValue::Uuid(Uuid::new_v4());
    let child = Uuid::new_v4();
    let source_submission = Uuid::new_v4();
    let other_submission = Uuid::new_v4();
    let expected: Vec<Expected> = vec![
        (
            "results",
            "id",
            result_id.clone(),
            vec![
                ("id", result_id.clone()),
                (
                    "tool_version",
                    CqlValue::Text("1.2.3-format-two".to_owned()),
                ),
                (
                    "children",
                    CqlValue::Map(vec![(
                        CqlValue::Text("child.bin".to_owned()),
                        CqlValue::Uuid(child),
                    )]),
                ),
                (
                    "entities",
                    CqlValue::Map(vec![(
                        CqlValue::Text("Device".to_owned()),
                        CqlValue::BigInt(1),
                    )]),
                ),
            ],
        ),
        (
            "results_stream",
            "id",
            result_id.clone(),
            vec![
                ("id", result_id.clone()),
                ("tool_version", CqlValue::Text("4.5.6-stream".to_owned())),
            ],
        ),
        (
            "repos_list",
            "default_checkout",
            CqlValue::Text("format-two-branch".to_owned()),
            vec![
                (
                    "default_checkout",
                    CqlValue::Text("format-two-branch".to_owned()),
                ),
                (
                    "earliest",
                    CqlValue::Timestamp(CqlTimestamp(1_600_000_000_000)),
                ),
            ],
        ),
        (
            "associations",
            "submissions_source",
            CqlValue::Uuid(source_submission),
            vec![
                ("submissions_source", CqlValue::Uuid(source_submission)),
                ("submissions_other", CqlValue::Uuid(other_submission)),
            ],
        ),
    ];
    let mut plans = Vec::new();
    for (table, _, _, set) in &expected {
        plans.push(row_with(harness, &mut planner, table, set.clone()).await);
    }
    write_rows(&scylla, &ns, &plans).await;
    write_objects(&harness.s3(), &harness.conf, &plans).await;
    // back up, wipe, and restore
    let dir = harness.scratch("dropped-columns");
    let backed_up = backup(harness, &dir);
    assert_eq!(backed_up.status.code(), Some(0), "{}", describe(&backed_up));
    wipe(harness).await;
    let restored = restore(harness, &dir);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // every column comes back with its value
    let mut problems = Vec::new();
    for (table, key, value, set) in &expected {
        let columns: Vec<&str> = set.iter().map(|(column, _)| *column).collect();
        let found = read_columns(&scylla, &ns, table, &columns, (key, value)).await;
        if found.len() != 1 {
            problems.push(format!("{table}: {} rows came back, not 1", found.len()));
            continue;
        }
        for (column, want) in set {
            let got = found[0].get(*column).cloned().flatten();
            if got.as_ref() != Some(want) {
                problems.push(format!("{table}.{column}: wanted {want:?}, found {got:?}"));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Reads the remaining TTL of a notification's message, by its id
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `ns` - The keyspace
/// * `id` - The notification's id
async fn notification_ttl(scylla: &Session, ns: &str, id: Uuid) -> Option<i32> {
    // read every notification's id and TTL and keep the one asked for
    scylla
        .query_unpaged(format!("SELECT id, TTL(msg) FROM {ns}.notifications"), ())
        .await
        .expect("failed to read notification TTLs")
        .into_rows_result()
        .unwrap()
        .rows::<(Uuid, Option<i32>)>()
        .unwrap()
        .map(Result::unwrap)
        .find(|(found, _)| *found == id)
        .unwrap_or_else(|| panic!("notification {id} is missing"))
        .1
}

/// A permanent notification (written with `USING TTL 0`) restores with no TTL, and an expiring
/// one with a TTL no greater than it had left when the backup started, never the table's default;
/// the restore gives no default-TTL warning
#[tokio::test]
async fn notification_ttls_come_back() {
    // a permanent and an expiring notification, the second expiring well before the default
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let scylla = harness.scylla().await;
    let ns = harness.conf.thorium.namespace.clone();
    let lists = common::enums::all();
    let mut planner = Planner::new(&harness.conf, &lists, MatrixSize::Ci, 0);
    let live = live_table(&scylla, &ns, "notifications").await;
    let mut ids = Vec::new();
    for ttl in [0, 86_400] {
        let id = Uuid::new_v4();
        let mut values = planner.base_row(&live);
        values.insert("id".to_owned(), CqlValue::Uuid(id));
        let columns: Vec<String> = values
            .keys()
            .map(|column| format!("\"{column}\""))
            .collect();
        let marks = vec!["?"; columns.len()].join(", ");
        scylla
            .query_unpaged(
                format!(
                    "INSERT INTO {ns}.notifications ({}) VALUES ({marks}) USING TTL {ttl}",
                    columns.join(", ")
                ),
                values.values().cloned().collect::<Vec<CqlValue>>(),
            )
            .await
            .expect("failed to write a notification");
        ids.push(id);
    }
    let default = scylla
        .query_unpaged(
            "SELECT default_time_to_live FROM system_schema.tables WHERE keyspace_name = ? AND \
             table_name = 'notifications'",
            (&ns,),
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap()
        .first_row::<(i32,)>()
        .unwrap()
        .0;
    assert!(
        default > 86_400,
        "the table default {default} must outlast the expiring TTL"
    );
    // the expiring notification's TTL when the backup starts bounds what may come back
    let before = notification_ttl(&scylla, &ns, ids[1]).await;
    assert_eq!(notification_ttl(&scylla, &ns, ids[0]).await, None);
    let dir = harness.scratch("notification-ttls");
    let backed_up = backup(harness, &dir);
    assert_eq!(backed_up.status.code(), Some(0), "{}", describe(&backed_up));
    wipe(harness).await;
    let restored = restore(harness, &dir);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    // the permanent one stays permanent and the expiring one keeps at most what it had left
    let permanent = notification_ttl(&scylla, &ns, ids[0]).await;
    let expiring = notification_ttl(&scylla, &ns, ids[1]).await;
    assert_eq!(permanent, None, "the permanent notification got a TTL");
    let (Some(expiring), Some(before)) = (expiring, before) else {
        panic!("the expiring notification lost its TTL: {expiring:?}, before {before:?}");
    };
    assert!(
        expiring > 0 && expiring <= before,
        "the expiring notification came back with TTL {expiring}, above the {before} it had left"
    );
    let stderr = String::from_utf8_lossy(&restored.stderr);
    assert!(
        !stderr.contains("default TTL"),
        "a format-2 restore warned of the default TTL:\n{stderr}"
    );
}
