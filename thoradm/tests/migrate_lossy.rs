//! Tests of lossy reverse steps in thoradm: the loss a check estimates, and `migrate run --confirm`
//! consuming a code that the history can't un-consume
//!
//! The runs plan over the fixture catalog, which holds one lossy step and the step that reverses it.

mod common;

use common::migrate::World;
use common::record;
use serde_json::{Value, json};
use std::process::Output;
use thorium::models::schema::MigrationRecordKind;
use thorium::models::upgrades::CONSUMED_REFUSAL;

/// The environment variable that points thoradm at the fixture catalog
const CATALOG_ENV: &str = "THORADM_FIXTURE_CATALOG";

/// The revision the fixture step belongs to
const TO: &str = "2026-10-v01";

/// The revision the fixture deployment starts at, which the lossy forward step reached
const FROM: &str = "2026-10-v02";

/// A thoradm command that plans over the fixture catalog
///
/// # Arguments
///
/// * `world` - The world to run against
fn thoradm(world: &World) -> std::process::Command {
    // the world's command, with the fixture catalog
    let mut command = world.thoradm();
    command.env(CATALOG_ENV, "1");
    command
}

/// Seed the record of a deployment at the second revision that finished the lossy forward step, so
/// the plan from the recorded revision holds the step that reverses it
///
/// # Arguments
///
/// * `world` - The world to seed
async fn seed_record(world: &World) {
    // the revision row replaces the one the world's apply recorded, and the forward step is done
    let mut revision = record::blank(MigrationRecordKind::Revision, "deployment");
    revision.revision = Some(FROM.to_owned());
    revision.version = Some("1.8.1".to_owned());
    let mut rows = record::Rows::default();
    rows.migrations.push(revision);
    rows.migrations.push(record::history_row(&record::step(
        FROM,
        "fixture-forward",
        1,
    )));
    record::insert(world.session(), &world.ns, &rows).await;
}

/// Describe a failed run
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    // the log lines say why it failed
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A check of a step that reverses a lossy one adds the loss to its report: each lost field with the
/// objects that hold a value to fill, and whether today's backup covers it
#[tokio::test]
async fn a_check_estimates_what_a_lossy_reverse_loses() {
    // seven layout 1 rows to convert
    let world = World::new("loss01").await;
    world.seed_valid("a", 7).await;
    let output = World::run(thoradm(&world).args([
        "migrate",
        "check",
        "--id",
        "fixture-lossy",
        "--slices",
        "1",
    ]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: Value = serde_json::from_str(
        stdout
            .lines()
            .find(|line| line.starts_with('{'))
            .expect("the report"),
    )
    .expect("json");
    // the loss names the step and its fields, with counts and coverage
    let loss = &report["loss"];
    assert_eq!(loss["step"], "fixture-reverse", "{report}");
    assert_eq!(loss["description"], "Rows lose their theme");
    let fields = loss["fields"].as_array().expect("fields");
    assert_eq!(fields.len(), 2, "{fields:?}");
    assert_eq!(
        (
            fields[0]["field"].clone(),
            fields[0]["objects"].clone(),
            fields[0]["backed_up"].clone()
        ),
        (json!("theme"), json!(7), json!(false))
    );
    assert_eq!(
        (
            fields[1]["field"].clone(),
            fields[1]["objects"].clone(),
            fields[1]["backed_up"].clone()
        ),
        (json!("samples_list.description"), json!(7), json!(true))
    );
    // a migration whose step reverses nothing lossy reports no loss
    let output = World::run(world.thoradm().args([
        "migrate",
        "check",
        "--id",
        "fixture-theme",
        "--slices",
        "1",
    ]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let plain: Value = serde_json::from_str(
        stdout
            .lines()
            .find(|line| line.starts_with('{'))
            .expect("the report"),
    )
    .expect("json");
    assert!(plain.get("loss").is_none(), "{plain}");
    world.drop_keyspace().await;
}

/// A run repeating a consumed code is refused before anything is written: while the history shows
/// the step finished the plan no longer holds it, and after that history is gone the `consumed_code`
/// row, which outlives it, still refuses the code
#[tokio::test]
async fn a_consumed_code_is_refused_even_when_the_history_forgot_it() {
    // a lossy step run once under a code, at the revision its forward step reached
    let world = World::new("loss03").await;
    world.seed_valid("a", 3).await;
    seed_record(&world).await;
    let running = json!({
        "revision": TO, "step": "fixture-reverse", "outcome": "Running",
        "at": "2026-10-22T10:00:00Z", "by": "thorium-operator 1.8.1",
        "backup": "snap-1", "confirm": "ABCD2345",
    })
    .to_string();
    let args = |extra: &[&str]| {
        let mut args = vec![
            "migrate",
            "run",
            "--id",
            "fixture-lossy",
            "--slices",
            "1",
            "--backup",
            "snap-1",
            "--confirm",
            "ABCD2345",
            "--running-entry",
        ];
        args.push(&running);
        args.extend(extra);
        args.into_iter().map(str::to_owned).collect::<Vec<_>>()
    };
    let first = World::run(thoradm(&world).args(args(&[])));
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    // the same step again is refused while the record shows it finished
    let second = World::run(thoradm(&world).args(args(&[])));
    assert_eq!(second.status.code(), Some(1), "{}", stderr(&second));
    assert!(
        stderr(&second).contains("the record shows it finished"),
        "{}",
        stderr(&second)
    );
    // the history is gone, as when the mirror's window or an older operator's rewrite lost it, and
    // the code is still consumed
    world
        .session()
        .query_unpaged(
            format!(
                "DELETE FROM {}.schema_migrations WHERE kind = 'history'",
                world.ns
            ),
            (),
        )
        .await
        .expect("the history deletes");
    // the forward step's entry is seeded again, so the plan holds the reverse step once more
    seed_record(&world).await;
    let third = World::run(thoradm(&world).args(args(&[])));
    assert_eq!(third.status.code(), Some(6), "{}", stderr(&third));
    assert!(
        stderr(&third).contains(CONSUMED_REFUSAL),
        "{}",
        stderr(&third)
    );
    // a code of its own runs
    let other = running.replace("ABCD2345", "EFGH6789");
    let fresh = World::run(thoradm(&world).args([
        "migrate",
        "run",
        "--id",
        "fixture-lossy",
        "--slices",
        "1",
        "--backup",
        "snap-1",
        "--confirm",
        "EFGH6789",
        "--running-entry",
        &other,
    ]));
    assert_eq!(fresh.status.code(), Some(0), "{}", stderr(&fresh));
    world.drop_keyspace().await;
}
