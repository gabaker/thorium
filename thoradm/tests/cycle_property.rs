//! The full-cycle property: generated batches of rows for every backed-up table and records for
//! every `Backup` field are seeded, backed up, wiped, and restored, and the oracle finds the
//! restored namespace equal to the seeded one
//!
//! Each row's regular columns are generated unset as well as set, each set column with a value of
//! one of the classes its type and domain need, so every unset cell must read back without a
//! value. The `ci` matrix runs a few cases under a fixed seed; the `full` matrix
//! (`THORADM_TEST_MATRIX=full`) runs more under a random seed it prints. `PROPTEST_RNG_SEED`
//! replays a seed, and `PROPTEST_CASES` and `PROPTEST_MAX_SHRINK_ITERS` override the case count and
//! the shrink limit. A failing case is shrunk to a minimal batch and saved under
//! `tests/proptest-regressions/`.

mod common;

use common::classes::{CoverageMatrix, Location, MatrixSize, Need, ValueClass, column_domain};
use common::columns::{Planner, RowPlan, Table, live_table, require, split_part, write_rows};
use common::cycle::{backup, census, describe, restore, wipe};
use common::harness::Harness;
use common::oracle::{Oracle, Snapshot};
use common::{enums, objects, records};
use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::{
    Config, FileFailurePersistence, RngSeed, TestCaseError, TestError, TestRunner,
};
use scylla::value::CqlValue;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use thoradm::backup::coverage::{Registry, Store};
use tokio::runtime::Runtime;

/// The cases the `ci` matrix runs
const CI_CASES: u32 = 8;

/// The cases the `full` matrix runs
const FULL_CASES: u32 = 64;

/// The seed every `ci` run uses, so a failure there reproduces anywhere
const CI_SEED: u64 = 0x6379_636c_6570_726f;

/// The most rows one case writes to one table
const MAX_ROWS: usize = 2;

/// The most record flavors one case writes
const MAX_FLAVORS: usize = 2;

/// The most shrink steps a failing case takes, each a full cycle
const MAX_SHRINK_ITERS: u32 = 48;

/// The most shrink steps the planted-difference check takes, enough to drop every other row
const PLANTED_SHRINK_ITERS: u32 = 12;

/// The table the planted-difference check writes, to keep its shrinking short
const PLANTED_TABLES: &[&str] = &["samples_list"];

/// The value a planted difference writes into the source after the backup
const PLANTED: &str = "a value the backup never saw";

/// Counts the cycles run, so every backup gets its own directory
static CYCLES: AtomicU64 = AtomicU64::new(0);

/// A backed-up table and the classes each of its varied columns can take
#[derive(Debug, Clone)]
struct TableShape {
    /// The table's live columns
    table: Table,
    /// Each regular column a row varies, with the column locations and classes its values can take
    varied: Vec<(String, Vec<(String, ValueClass)>)>,
    /// A regular text column with no domain, where a planted difference goes
    plantable: Option<String>,
}

/// How one generated row differs from an ordinary row
#[derive(Debug, Clone)]
struct RowGen {
    /// Whether each varied column is left unset
    unset: Vec<bool>,
    /// The class each varied column's value takes, among the column's classes
    picks: Vec<Index>,
}

/// Which generated row a planted difference changes after the backup
#[derive(Debug, Clone)]
struct Plant {
    /// Picks the row among the rows of the tables a difference can be planted in
    row: Index,
}

/// One generated case: the rows of each table, the record flavors, and any planted difference
#[derive(Debug, Clone)]
struct Batch {
    /// The rows of each table, in the order of the table shapes
    rows: Vec<Vec<RowGen>>,
    /// The record flavors to write, by their place in the flavor list
    flavors: Vec<usize>,
    /// A difference to plant in the source after the backup, which the restore can't bring back
    plant: Option<Plant>,
}

impl Batch {
    /// Returns how many rows the batch writes
    fn row_count(&self) -> usize {
        // every table's rows
        self.rows.iter().map(Vec::len).sum()
    }
}

/// Reads the shape of every backed-up table: the regular columns a row varies and the classes
/// each can take
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `only` - The tables to keep, or every backed-up table when empty
async fn table_shapes(harness: &Harness, only: &[&str]) -> Vec<TableShape> {
    // the classes the coverage matrix requires of each column in the ci matrix
    let scylla = harness.scylla().await;
    let lists = enums::all();
    let mut shapes = Vec::new();
    for entry in Registry::backed_up(Store::ScyllaTable) {
        if !only.is_empty() && !only.contains(&entry.name) {
            continue;
        }
        let table = live_table(&scylla, &harness.conf.thorium.namespace, entry.name).await;
        let mut matrix = CoverageMatrix::default();
        require(&table, &lists, MatrixSize::Ci, &mut matrix);
        let mut varied = Vec::new();
        for column in table.columns.iter().filter(|column| {
            !column.is_key() && common::classes::derived_source(&table.name, &column.name).is_none()
        }) {
            // every required class of the column or its elements, except unset, which is chosen
            // apart
            let choices: Vec<(String, ValueClass)> = matrix
                .needs
                .iter()
                .filter_map(|((location, class), need)| match location {
                    Location::Column {
                        table: name,
                        column: at,
                    } if *name == table.name
                        && split_part(at).0 == column.name
                        && *need == Need::Required
                        && *class != ValueClass::Null =>
                    {
                        Some((at.clone(), class.clone()))
                    }
                    _ => None,
                })
                .collect();
            varied.push((column.name.clone(), choices));
        }
        // a plain text column to plant a difference in
        let plantable = table
            .columns
            .iter()
            .find(|column| {
                !column.is_key()
                    && matches!(column.cql, common::columns::CqlType::Text)
                    && column_domain(&table.name, &column.name).is_none()
                    && common::classes::derived_source(&table.name, &column.name).is_none()
            })
            .map(|column| column.name.clone());
        shapes.push(TableShape {
            table,
            varied,
            plantable,
        });
    }
    shapes
}

/// Builds the strategy for one case
///
/// # Arguments
///
/// * `shapes` - The tables a case writes rows to
/// * `planted` - Whether every case plants a difference, in a batch of at least one row of each
///   table and the first record flavor
fn batches(shapes: &[TableShape], planted: bool) -> BoxedStrategy<Batch> {
    // each table's rows, each varying every varied column
    let rows: Vec<BoxedStrategy<Vec<RowGen>>> = shapes
        .iter()
        .map(|shape| {
            let columns = shape.varied.len();
            let row = (
                prop::collection::vec(prop::bool::weighted(0.3), columns),
                prop::collection::vec(any::<Index>(), columns),
            )
                .prop_map(|(unset, picks)| RowGen { unset, picks });
            let fewest = usize::from(planted);
            prop::collection::vec(row, fewest..=MAX_ROWS + fewest).boxed()
        })
        .collect();
    let (flavors, plant) = if planted {
        (
            Just(vec![0]).boxed(),
            any::<Index>().prop_map(|row| Some(Plant { row })).boxed(),
        )
    } else {
        (
            prop::collection::vec(0..records::flavor_count(), 1..=MAX_FLAVORS).boxed(),
            Just(None).boxed(),
        )
    };
    (rows, flavors, plant)
        .prop_map(|(rows, flavors, plant)| Batch {
            rows,
            flavors,
            plant,
        })
        .boxed()
}

/// Plans the rows of a batch, each an ordinary row with its varied columns unset or given a
/// value of the class it picked
///
/// # Arguments
///
/// * `planner` - The planner that builds the values
/// * `shapes` - The tables' shapes
/// * `batch` - The batch
fn plan(planner: &mut Planner<'_>, shapes: &[TableShape], batch: &Batch) -> Vec<RowPlan> {
    // one plan per generated row, in table order
    let mut plans = Vec::new();
    for (shape, rows) in shapes.iter().zip(&batch.rows) {
        for row in rows {
            let mut values = planner.base_row(&shape.table);
            for (index, (column, choices)) in shape.varied.iter().enumerate() {
                if row.unset[index] {
                    values.remove(column);
                    continue;
                }
                if choices.is_empty() {
                    continue;
                }
                let (at, class) = &choices[row.picks[index].index(choices.len())];
                if let Some(value) = planner.value(&shape.table, at, class) {
                    values.insert(column.clone(), value);
                }
            }
            planner.derive(&shape.table.name, &mut values);
            plans.push(RowPlan {
                table: shape.table.name.clone(),
                values,
                body: None,
            });
        }
    }
    plans
}

/// Runs one cycle of a batch: wipe, seed, census, back up, plant, snapshot, wipe, restore, and
/// compare, failing with every difference the oracle finds
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `shapes` - The tables' shapes
/// * `batch` - The batch
async fn cycle(
    harness: &Harness,
    shapes: &[TableShape],
    batch: &Batch,
) -> Result<(), TestCaseError> {
    // start from an empty namespace and seed the batch's rows, objects, and records
    wipe(harness).await;
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    let lists = enums::all();
    let mut planner = Planner::new(&harness.conf, &lists, MatrixSize::Ci, 0);
    let plans = plan(&mut planner, shapes, batch);
    write_rows(&scylla, ns, &plans).await;
    objects::write_objects(&harness.s3(), &harness.conf, &plans).await;
    Box::pin(records::seed_flavors(harness, &batch.flavors)).await;
    // the users the flavors seed name the fixture's icon, which the backup copies by that key
    objects::write_user_icon(&harness.s3(), &harness.conf).await;
    let counted = census(harness);
    prop_assert_eq!(counted.status.code(), Some(0), "{}", describe(&counted));
    // back up every component
    let dir: PathBuf =
        harness.scratch(&format!("cycle-{}", CYCLES.fetch_add(1, Ordering::Relaxed)));
    let backed_up = backup(harness, &dir);
    prop_assert_eq!(backed_up.status.code(), Some(0), "{}", describe(&backed_up));
    // a planted difference changes one value of one row the backup holds
    if let Some(plant) = &batch.plant {
        let plantable: Vec<(&RowPlan, &String)> = plans
            .iter()
            .filter_map(|plan| {
                let shape = shapes.iter().find(|shape| shape.table.name == plan.table)?;
                Some((plan, shape.plantable.as_ref()?))
            })
            .collect();
        if !plantable.is_empty() {
            let (row, column) = plantable[plant.row.index(plantable.len())];
            let mut changed = row.clone();
            changed
                .values
                .insert(column.clone(), CqlValue::Text(PLANTED.to_owned()));
            write_rows(&scylla, ns, &[changed]).await;
        }
    }
    // the source as the restore must bring it back, then the wipe and the restore
    let source = Snapshot::take(harness).await;
    wipe(harness).await;
    let restored = restore(harness, &dir);
    prop_assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let after = Snapshot::take(harness).await;
    let _ = std::fs::remove_dir_all(&dir);
    // every row, cell, record, and object comes back, unset cells unset
    let differences = Oracle::default().compare(&source, &after);
    prop_assert!(
        differences.is_empty(),
        "{} differences:\n{}",
        differences.len(),
        differences
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
    Ok(())
}

/// Builds the runner config: the case count and seed for the matrix size, and where failing seeds
/// are saved
///
/// # Arguments
///
/// * `persistence` - The file failing seeds are saved to and replayed from
fn config(persistence: &'static str) -> (Config, u64) {
    // the defaults carry any PROPTEST_* overrides from the environment
    let defaults = Config::default();
    let matrix = MatrixSize::from_env();
    // an explicit seed wins, then the matrix decides between the fixed seed and a random one
    let seed = match defaults.rng_seed {
        RngSeed::Fixed(seed) => seed,
        RngSeed::Random => match matrix {
            MatrixSize::Ci => CI_SEED,
            MatrixSize::Full => rand::random(),
        },
    };
    // an explicit case count wins, then the matrix's count
    let cases = if std::env::var_os("PROPTEST_CASES").is_some() {
        defaults.cases
    } else {
        match matrix {
            MatrixSize::Ci => CI_CASES,
            MatrixSize::Full => FULL_CASES,
        }
    };
    // an explicit shrink limit wins, then the property's own
    let max_shrink_iters = if std::env::var_os("PROPTEST_MAX_SHRINK_ITERS").is_some() {
        defaults.max_shrink_iters
    } else {
        MAX_SHRINK_ITERS
    };
    let config = Config {
        cases,
        rng_seed: RngSeed::Fixed(seed),
        max_shrink_iters,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(persistence))),
        ..defaults
    };
    (config, seed)
}

/// The runtime every test of the binary shares, which keeps the harness's API and pools alive
static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    // the harness spawns its API and pools on this runtime
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
});

/// Generated batches of rows for every backed-up table, with regular columns set and unset, and
/// records of every `Backup` field come back equal after a backup, wipe, and restore
#[test]
fn generated_batches_survive_a_full_cycle() {
    // the harness, the tables' shapes, and the seed, printed so any run can be repeated
    let runtime = &*RUNTIME;
    let harness = runtime.block_on(Harness::get());
    let _guard = runtime.block_on(harness.restore_guard());
    let shapes = runtime.block_on(table_shapes(harness, &[]));
    let (config, seed) = config("tests/proptest-regressions/cycle_property.txt");
    eprintln!("full-cycle property: {} cases, seed {seed}", config.cases);
    let mut runner = TestRunner::new(config);
    let result = runner.run(&batches(&shapes, false), |batch| {
        runtime.block_on(cycle(harness, &shapes, &batch))
    });
    // a failure names the seed and prints the shrunk batch
    match result {
        Ok(()) => {}
        Err(TestError::Fail(reason, batch)) => {
            panic!(
                "the full-cycle property failed under seed {seed}: {reason}\nminimal batch: {batch:#?}"
            )
        }
        Err(TestError::Abort(reason)) => {
            panic!("the full-cycle property aborted under seed {seed}: {reason}")
        }
    }
}

/// A difference planted in one generated value after the backup fails the property, which
/// shrinks the batch to the one row holding that value and saves the failing seed
#[test]
fn a_planted_difference_shrinks_to_its_value() {
    // a few tables, every case planting a difference, seeds saved outside the repository
    let runtime = &*RUNTIME;
    let harness = runtime.block_on(Harness::get());
    let _guard = runtime.block_on(harness.restore_guard());
    let shapes = runtime.block_on(table_shapes(harness, PLANTED_TABLES));
    let saved = harness.scratch("planted-regressions").join("planted.txt");
    let persistence: &'static str = Box::leak(saved.display().to_string().into_boxed_str());
    let (config, seed) = config(persistence);
    eprintln!("planted difference: seed {seed}");
    let mut runner = TestRunner::new(Config {
        cases: 1,
        max_shrink_iters: PLANTED_SHRINK_ITERS,
        ..config
    });
    let result = runner.run(&batches(&shapes, true), |batch| {
        runtime.block_on(cycle(harness, &shapes, &batch))
    });
    // the property fails on the planted value, shrunk to its row, and the seed is saved
    let Err(TestError::Fail(reason, batch)) = result else {
        panic!("the planted difference wasn't found: {result:?}");
    };
    let reason = reason.to_string();
    assert_eq!(
        batch.row_count(),
        1,
        "the batch didn't shrink to one row: {batch:#?}"
    );
    assert!(
        reason.contains(PLANTED),
        "the shrunk failure isn't the planted value: {reason}"
    );
    let saved = std::fs::read_to_string(persistence).unwrap_or_default();
    assert!(
        saved.contains("cc "),
        "the failing seed wasn't saved: {saved}"
    );
}
