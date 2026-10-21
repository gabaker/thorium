//! Values a column can hold but its archive struct can't are skipped and recorded by backup, while
//! rows with any regular column unset are backed up and restored with that cell unset
//!
//! This namespace holds one ordinary row of each backed-up table, one row per listed value, and one
//! row per regular column of each table with only that column unset.

mod common;

use common::classes::{KNOWN_BROKEN, MatrixSize, UNREPRESENTABLE, UNSET_UNREPRESENTABLE};
use common::columns::{Planner, RowPlan, Table, live_table, write_rows};
use common::cycle::{backup, describe, failure_records, restore, restore_with, wipe};
use common::harness::Harness;
use common::oracle::{Difference, Oracle, Snapshot, render};
use scylla::value::CqlValue;
use std::collections::{BTreeMap, BTreeSet};
use thoradm::backup::coverage::{Registry, Store};
use thoradm::backup::failures::FailureKind;

/// The table of the nodes a restore keeps when the target holds any
const NODES: &str = "nodes";
/// Renders a planned row's primary key the way a snapshot keys its rows
///
/// # Arguments
///
/// * `table` - The table's live columns
/// * `values` - The row's values
fn row_key(table: &Table, values: &BTreeMap<String, CqlValue>) -> String {
    // the key columns in schema order, each rendered as a snapshot renders it
    table
        .columns
        .iter()
        .filter(|column| column.is_key())
        .map(|column| render(values.get(&column.name)).unwrap_or_else(|| "null".to_owned()))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Returns a message for each `UNSET_UNREPRESENTABLE` entry whose row came back after the restore,
/// which means its column round-trips and the entry must go
///
/// # Arguments
///
/// * `listed` - The entries, as table and column
/// * `rows` - The rendered key of each entry's row, by entry
/// * `after` - The namespace after the restore
fn stale_unset_entries(
    listed: &[(&str, &str)],
    rows: &BTreeMap<(String, String), String>,
    after: &Snapshot,
) -> Vec<String> {
    // an entry is stale when the row with only its column unset is in the restored table
    listed
        .iter()
        .filter(|(table, column)| {
            rows.get(&((*table).to_owned(), (*column).to_owned()))
                .is_some_and(|key| {
                    after
                        .scylla
                        .get(*table)
                        .is_some_and(|found| found.contains_key(key))
                })
        })
        .map(|(table, column)| {
            format!(
                "UNSET_UNREPRESENTABLE lists {table}.{column}, but a row with it unset round-trips; \
                 remove the entry"
            )
        })
        .collect()
}

/// Returns the differences a restore of this namespace may not have: anything but the skipped
/// rows, at most one view row and one census stream per skipped row, and the kinds that don't
/// restore yet
///
/// # Arguments
///
/// * `tables` - The tables the namespace was seeded with
/// * `expected` - How many rows of each table the backup skips
/// * `differences` - What the oracle found against the source
fn unexplained(
    tables: &BTreeSet<&'static str>,
    expected: &BTreeMap<&'static str, usize>,
    differences: &[Difference],
) -> Vec<String> {
    // the tables that don't restore yet, with their views
    let broken: BTreeSet<&str> = KNOWN_BROKEN
        .iter()
        .flat_map(|broken| broken.entries.iter().copied())
        .flat_map(|table| std::iter::once(table).chain(Registry::views_of(table)))
        .collect();
    // check every table before failing, so one table's problem can't hide another's: a table
    // lacks exactly its skipped rows, and each of its views and its census lacks at most one row
    // or stream per skipped row, which went with it; nothing else differs
    let mut problems = Vec::new();
    let mut checked: BTreeSet<&str> = BTreeSet::new();
    for name in tables.iter().filter(|name| !broken.contains(**name)) {
        let skipped = expected.get(name).copied().unwrap_or(0);
        let entries: Vec<&str> = std::iter::once(*name)
            .chain(Registry::views_of(name))
            .collect();
        checked.extend(entries.iter().copied());
        for entry in entries {
            let mine: Vec<_> = differences
                .iter()
                .filter(|difference| difference.entry == entry)
                .collect();
            let shown: Vec<String> = mine.iter().map(ToString::to_string).collect();
            let rows = mine
                .iter()
                .filter(|difference| {
                    difference.found.is_none() && difference.location.ends_with(']')
                })
                .count();
            let streams = mine
                .iter()
                .filter(|difference| {
                    difference.found.is_none() && difference.location.starts_with("census ")
                })
                .count();
            let lost_rows_ok = if entry == *name {
                rows == skipped
            } else {
                rows <= skipped
            };
            if !lost_rows_ok || streams > skipped || rows + streams != mine.len() {
                problems.push(format!(
                    "{entry} (of {name}, {skipped} rows skipped) lost {rows} rows and {streams} \
                     census streams:\n{}",
                    shown.join("\n")
                ));
            }
        }
    }
    // no entry the namespace wasn't seeded with differs either
    let others: Vec<String> = differences
        .iter()
        .filter(|difference| {
            !checked.contains(difference.entry.as_str())
                && !broken.contains(difference.entry.as_str())
        })
        .map(ToString::to_string)
        .collect();
    if !others.is_empty() {
        problems.push(format!("other entries changed:\n{}", others.join("\n")));
    }
    problems
}

/// Every listed value is skipped and recorded once and the backup exits 2, while no row with a
/// regular column unset is skipped; every other row of every table comes back, each unset cell
/// reading back without a value, with its view rows and the census streams a census of the source
/// holds. Restored with `--merge` over a target whose matching rows hold a value in each of those
/// cells, every cell takes the backup's value, the unset ones reading back without a value
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn unrepresentable_values_are_skipped_and_recorded() {
    // one ordinary row per backed-up table, one row per listed value, and one row per regular
    // column with only that column unset
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let lists = common::enums::all();
    let mut planner = Planner::new(&harness.conf, &lists, MatrixSize::Ci, 0);
    let mut expected: BTreeMap<&str, usize> = BTreeMap::new();
    let tables: BTreeSet<&str> = Registry::backed_up(Store::ScyllaTable)
        .map(|entry| entry.name)
        .collect();
    let mut plans = Vec::new();
    let mut unset_rows: Vec<(Table, RowPlan)> = Vec::new();
    let mut entry_rows: BTreeMap<(String, String), String> = BTreeMap::new();
    for name in &tables {
        let table = live_table(&scylla, ns, name).await;
        let values = planner.base_row(&table);
        plans.push(RowPlan {
            table: (*name).to_owned(),
            values,
            body: None,
        });
        for (_, column, text, _) in UNREPRESENTABLE.iter().filter(|(listed, ..)| listed == name) {
            let mut values = planner.base_row(&table);
            values.insert((*column).to_owned(), CqlValue::Text((*text).to_owned()));
            plans.push(RowPlan {
                table: (*name).to_owned(),
                values,
                body: None,
            });
            *expected.entry(name).or_default() += 1;
        }
        for column in table.columns.iter().filter(|column| !column.is_key()) {
            let mut values = planner.base_row(&table);
            values.remove(&column.name);
            entry_rows.insert(
                ((*name).to_owned(), column.name.clone()),
                row_key(&table, &values),
            );
            if UNSET_UNREPRESENTABLE.contains(&(*name, column.name.as_str())) {
                *expected.entry(name).or_default() += 1;
            }
            let plan = RowPlan {
                table: (*name).to_owned(),
                values,
                body: None,
            };
            plans.push(plan.clone());
            unset_rows.push((table.clone(), plan));
        }
    }
    write_rows(&scylla, ns, &plans).await;
    // a census of the rows written, as the API keeps one, which the restore rebuilds; the
    // listed values can't be counted, as they can't be backed up, so the census logs errors
    let counted = harness
        .thoradm()
        .args(["census", "new", "all"])
        .output()
        .expect("failed to run thoradm");
    assert!(
        matches!(counted.status.code(), Some(0 | 2)),
        "{}",
        describe(&counted)
    );
    let source = Snapshot::take(harness).await;
    // the backup skips each listed value once and exits 2
    let dir = harness.scratch("unrepresentable");
    let backed_up = backup(harness, &dir);
    assert_eq!(backed_up.status.code(), Some(2), "{}", describe(&backed_up));
    let mut skipped: BTreeMap<String, usize> = BTreeMap::new();
    for record in failure_records(&dir)
        .into_iter()
        .filter(|record| record.kind == FailureKind::Skipped)
    {
        *skipped
            .entry(record.table.clone().unwrap_or_default())
            .or_default() += 1;
    }
    let expected_by_name: BTreeMap<String, usize> = expected
        .iter()
        .map(|(name, count)| ((*name).to_owned(), *count))
        .collect();
    assert_eq!(skipped, expected_by_name, "skips per table");
    // every other row of every table comes back, its unset cells unset
    wipe(harness).await;
    let restored = restore(harness, &dir);
    assert!(
        matches!(restored.status.code(), Some(0 | 2)),
        "{}",
        describe(&restored)
    );
    let after = Snapshot::take(harness).await;
    let mut problems = stale_unset_entries(UNSET_UNREPRESENTABLE, &entry_rows, &after);
    let differences = Oracle::default().compare(&source, &after);
    problems.extend(unexplained(&tables, &expected, &differences));
    assert!(problems.is_empty(), "{}", problems.join("\n\n"));
    // give every unset cell, and every other regular cell of those rows, a value of the target's
    // own, then merge the backup back in: the backup's values win, its unset cells included
    let mut target = Vec::new();
    for (table, plan) in &unset_rows {
        let mut values = planner.base_row(table);
        for column in table.columns.iter().filter(|column| column.is_key()) {
            match plan.values.get(&column.name) {
                Some(value) => values.insert(column.name.clone(), value.clone()),
                None => values.remove(&column.name),
            };
        }
        planner.derive(&table.name, &mut values);
        target.push(RowPlan {
            table: table.name.clone(),
            values,
            body: None,
        });
    }
    write_rows(&scylla, ns, &target).await;
    let before_merge = Snapshot::take(harness).await;
    let merged = restore_with(harness, &dir, &["--accept-failures", "--merge"]);
    assert!(
        matches!(merged.status.code(), Some(0 | 2)),
        "{}",
        describe(&merged)
    );
    let mut after_merge = Snapshot::take(harness).await;
    // a target that holds nodes keeps them, so the merge leaves its nodes as they were and the
    // rest is compared without them
    assert_eq!(
        before_merge.scylla.get(NODES),
        after_merge.scylla.get(NODES),
        "the merge changed the target's nodes"
    );
    after_merge.scylla.remove(NODES);
    let mut merged_source = source.clone();
    merged_source.scylla.remove(NODES);
    let merged_tables = tables
        .iter()
        .copied()
        .filter(|table| *table != NODES)
        .collect();
    let differences = Oracle::default().compare(&merged_source, &after_merge);
    let problems = unexplained(&merged_tables, &expected, &differences);
    assert!(
        problems.is_empty(),
        "after --merge:\n{}",
        problems.join("\n\n")
    );
}

/// An `UNSET_UNREPRESENTABLE` entry whose row came back after the restore is named as stale, and
/// one whose row the backup skipped isn't
#[test]
fn a_listed_unset_column_that_round_trips_is_named() {
    // two entries: one row came back, the other was skipped
    let rows: BTreeMap<(String, String), String> = [
        (
            ("nodes".to_owned(), "health".to_owned()),
            "a | b".to_owned(),
        ),
        (("tags".to_owned(), "item".to_owned()), "c | d".to_owned()),
    ]
    .into_iter()
    .collect();
    let mut after = Snapshot::default();
    after
        .scylla
        .entry("nodes".to_owned())
        .or_default()
        .insert("a | b".to_owned(), BTreeMap::new());
    let stale = stale_unset_entries(&[("nodes", "health"), ("tags", "item")], &rows, &after);
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert!(stale[0].contains("nodes.health"), "{stale:?}");
}
