//! Tests of `thoradm migrate revert`: it shows the plan and each lossy step's confirmation code,
//! runs a lossy step only with its own code and a backup, consumes the code, and records the older
//! revision
//!
//! The runs plan over the fixture catalog, which holds one lossy step and the step that reverses it.

mod common;

use common::migrate::World;
use common::record;
use std::process::Output;
use thorium::models::schema::MigrationRecordKind;
use thorium::models::upgrades::{AppliedStep, StepState};

/// The environment variable that points thoradm at the fixture catalog
const CATALOG_ENV: &str = "THORADM_FIXTURE_CATALOG";

/// The revision the fixture deployment starts at
const FROM: &str = "2026-10-v02";

/// The revision the tests revert to
const TO: &str = "2026-10-v01";

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

/// Describe a failed run
///
/// # Arguments
///
/// * `output` - The command's output
fn stderr(output: &Output) -> String {
    // the log lines say why it failed
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Seed the record of a deployment at the second revision that finished the lossy forward step
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

/// Read the rows of `schema_migrations` of one kind as `(id, entry)`
///
/// # Arguments
///
/// * `world` - The world to read
/// * `kind` - The kind of row
async fn rows(world: &World, kind: &str) -> Vec<(String, Option<String>)> {
    // every row of the kind
    let result = world
        .session()
        .query_unpaged(
            format!(
                "SELECT id, entry FROM {}.schema_migrations WHERE kind = '{kind}'",
                world.ns
            ),
            (),
        )
        .await
        .expect("the rows read")
        .into_rows_result()
        .expect("rows");
    result
        .rows::<(String, Option<String>)>()
        .expect("typed rows")
        .map(|row| row.expect("a row"))
        .collect()
}

/// Read the revision the record holds
///
/// # Arguments
///
/// * `world` - The world to read
async fn recorded_revision(world: &World) -> Option<String> {
    // the revision row's one cell
    let result = world
        .session()
        .query_unpaged(
            format!(
                "SELECT revision FROM {}.schema_migrations WHERE kind = 'revision'",
                world.ns
            ),
            (),
        )
        .await
        .expect("the revision reads")
        .into_rows_result()
        .expect("rows");
    let rows = result
        .rows::<(Option<String>,)>()
        .expect("typed rows")
        .map(|row| row.expect("a row").0)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1, "{rows:?}");
    rows.into_iter().next().flatten()
}

/// Find the confirmation code a dry run prints
///
/// # Arguments
///
/// * `output` - The dry run's output
fn shown_code(output: &Output) -> String {
    // the line names the code after `--confirm`
    let stdout = String::from_utf8_lossy(&output.stdout);
    let after = stdout
        .split("--confirm ")
        .nth(1)
        .unwrap_or_else(|| panic!("no code in {stdout}"));
    after.chars().take(8).collect()
}

/// `migrate revert` shows the plan and each lossy step's code, and runs a lossy step only with that
/// code and a backup; the code is consumed and the older revision recorded
#[tokio::test]
async fn a_revert_runs_a_lossy_step_only_with_its_code_and_records_the_revision() {
    // a deployment at the second revision with rows in layout 1
    let world = World::new("loss02").await;
    world.seed_valid("a", 5).await;
    seed_record(&world).await;
    // the dry run lists the step, what it loses, and its code, and changes nothing
    let output = World::run(thoradm(&world).args(["migrate", "revert", "--to", TO, "--dry-run"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("fixture-reverse"), "{stdout}");
    assert!(stdout.contains("loses theme"), "{stdout}");
    let code = shown_code(&output);
    assert_eq!(code.len(), 8);
    assert!(world.rows().await.iter().all(|(_, _, layout)| *layout == 1));
    // without the code the revert refuses, and with the code but no backup it refuses too
    let output = World::run(thoradm(&world).args(["migrate", "revert", "--to", TO]));
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("needs its confirmation code"),
        "{}",
        stderr(&output)
    );
    let output =
        World::run(thoradm(&world).args(["migrate", "revert", "--to", TO, "--confirm", &code]));
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("--backup"), "{}", stderr(&output));
    // a wrong code is refused
    let output = World::run(thoradm(&world).args([
        "migrate",
        "revert",
        "--to",
        TO,
        "--confirm",
        "AAAAAAAA",
        "--backup",
        "snap-1",
    ]));
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(world.rows().await.iter().all(|(_, _, layout)| *layout == 1));
    // the right code and a backup run it
    let output = World::run(thoradm(&world).args([
        "migrate",
        "revert",
        "--to",
        TO,
        "--confirm",
        &code,
        "--backup",
        "snap-1",
        "--accept-backup-gaps",
    ]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(world.rows().await.iter().all(|(_, _, layout)| *layout == 2));
    // the older revision is recorded
    let recorded = recorded_revision(&world).await;
    assert_eq!(recorded.as_deref(), Some(TO));
    // the step's entries carry the code, the backup, and the loss; the code is consumed
    let history = rows(&world, "history")
        .await
        .into_iter()
        .filter_map(|(_, entry)| entry)
        .map(|entry| serde_json::from_str::<AppliedStep>(&entry).expect("an entry"))
        .filter(|entry| entry.step == "fixture-reverse")
        .collect::<Vec<_>>();
    let outcomes = history
        .iter()
        .map(|entry| entry.outcome)
        .collect::<Vec<_>>();
    assert!(outcomes.contains(&StepState::Running), "{outcomes:?}");
    assert!(outcomes.contains(&StepState::Done), "{outcomes:?}");
    for entry in &history {
        assert_eq!(entry.confirm.as_deref(), Some(code.as_str()), "{entry:?}");
        assert_eq!(entry.backup.as_deref(), Some("snap-1"));
        assert_eq!(
            entry
                .loss
                .as_ref()
                .map(|loss| loss.fields[0].field.as_str()),
            Some("theme")
        );
    }
    let consumed = rows(&world, "consumed_code").await;
    assert_eq!(
        consumed
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        [code.as_str()]
    );
    world.drop_keyspace().await;
}

/// `migrate revert` shows whether `thoradm backup` covers each lost field, and refuses a lossy step
/// that loses a field no backup covers unless `--accept-backup-gaps` accepts the gap, the way the
/// operator's approval gate does; nothing runs or is consumed while it refuses
#[tokio::test]
async fn a_revert_refuses_a_loss_no_backup_covers_unless_the_gap_is_accepted() {
    // a deployment at the second revision with rows in layout 1
    let world = World::new("loss03").await;
    world.seed_valid("a", 5).await;
    seed_record(&world).await;
    // the dry run says the lost field isn't covered by the backup
    let output = World::run(thoradm(&world).args(["migrate", "revert", "--to", TO, "--dry-run"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("not covered by thoradm backup"), "{stdout}");
    let code = shown_code(&output);
    // the right code and a backup are refused, naming the field and the flag
    let output = World::run(thoradm(&world).args([
        "migrate",
        "revert",
        "--to",
        TO,
        "--confirm",
        &code,
        "--backup",
        "snap-1",
    ]));
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("theme"), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("--accept-backup-gaps"),
        "{}",
        stderr(&output)
    );
    // nothing changed and the code isn't consumed
    assert!(world.rows().await.iter().all(|(_, _, layout)| *layout == 1));
    assert_eq!(rows(&world, "consumed_code").await, Vec::new());
    // accepting the gap runs the step
    let output = World::run(thoradm(&world).args([
        "migrate",
        "revert",
        "--to",
        TO,
        "--confirm",
        &code,
        "--backup",
        "snap-1",
        "--accept-backup-gaps",
    ]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(world.rows().await.iter().all(|(_, _, layout)| *layout == 2));
    world.drop_keyspace().await;
}

/// The revert's plan names the components a step expects to be stopped and says the revert stops
/// none of them
#[tokio::test]
async fn a_revert_plan_names_the_components_a_step_pauses() {
    // a deployment at the second revision, whose reverse step pauses two components
    let world = World::new("loss04").await;
    world.seed_valid("a", 3).await;
    seed_record(&world).await;
    // the dry run names both components and says it stops none of them
    let output = World::run(thoradm(&world).args(["migrate", "revert", "--to", TO, "--dry-run"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        stdout.contains("fixture-reverse expects event-handler, search-streamer to be stopped"),
        "{stdout}"
    );
    assert!(stdout.contains("stops none of them"), "{stdout}");
    world.drop_keyspace().await;
}

/// After a lossy step's run was interrupted, its `Running` entry changes the step's confirmation
/// code, so the old code is refused and the code a new dry run shows resumes and finishes it
#[tokio::test]
async fn an_interrupted_lossy_step_resumes_with_the_code_a_new_dry_run_shows() {
    // a deployment at the second revision, and the code the first dry run shows
    let world = World::new("loss05").await;
    world.seed_valid("a", 5).await;
    seed_record(&world).await;
    let output = World::run(thoradm(&world).args(["migrate", "revert", "--to", TO, "--dry-run"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let first = shown_code(&output);
    // the interrupted run left its `Running` entry under that code
    let mut running = record::step(TO, "fixture-reverse", 2);
    running.outcome = StepState::Running;
    running.confirm = Some(first.clone());
    running.backup = Some("snap-1".to_owned());
    let mut rows = record::Rows::default();
    rows.migrations.push(record::history_row(&running));
    record::insert(world.session(), &world.ns, &rows).await;
    // the first code no longer matches
    let output = World::run(thoradm(&world).args([
        "migrate",
        "revert",
        "--to",
        TO,
        "--confirm",
        &first,
        "--backup",
        "snap-1",
        "--accept-backup-gaps",
    ]));
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    // a new dry run shows a new code, which resumes and finishes the step
    let output = World::run(thoradm(&world).args(["migrate", "revert", "--to", TO, "--dry-run"]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let second = shown_code(&output);
    assert_ne!(second, first);
    let output = World::run(thoradm(&world).args([
        "migrate",
        "revert",
        "--to",
        TO,
        "--confirm",
        &second,
        "--backup",
        "snap-1",
        "--accept-backup-gaps",
    ]));
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(world.rows().await.iter().all(|(_, _, layout)| *layout == 2));
    assert_eq!(recorded_revision(&world).await.as_deref(), Some(TO));
    world.drop_keyspace().await;
}

/// The revision the real catalog's reverse edge leaves
const REAL_TO: &str = "2026-10-v02";

/// The real catalog's step that reverses the users migration
const REVERSE_STEP: &str = "users-settings-theme-reverse";

/// Seed the record of a deployment at the revision after the first Helm one, with every step of it
/// finished and the users family recorded in layout 2
///
/// # Arguments
///
/// * `world` - The world to seed
async fn seed_real_record(world: &World) {
    // the revision row, the finished steps, and the backend row of Redis
    let mut revision = record::blank(MigrationRecordKind::Revision, "deployment");
    revision.revision = Some("2026-10-v03".to_owned());
    revision.version = Some("1.8.1".to_owned());
    let mut redis = record::blank(MigrationRecordKind::Backend, "redis");
    redis.layouts = std::collections::BTreeMap::from([("users".to_owned(), 2)]);
    let mut rows = record::Rows::default();
    rows.migrations.push(revision);
    rows.migrations.push(redis);
    for (minute, step) in [(1, "schema-apply"), (2, "users-settings-theme")] {
        rows.migrations.push(record::history_row(&record::step(
            "2026-10-v03",
            step,
            minute,
        )));
    }
    record::insert(world.session(), &world.ns, &rows).await;
}

/// Store a user's hash and add it to the user index
///
/// # Arguments
///
/// * `world` - The world to store into
/// * `name` - The username
/// * `fields` - The fields of the hash
async fn store_user(world: &World, name: &str, fields: &[(&str, &str)]) {
    // the index set and the hash, as the API keeps them
    let mut conn = world.backends.redis.get().await.expect("a connection");
    let key = format!("{}:user_data:{name}", world.ns);
    let mut pipe = redis::pipe();
    pipe.cmd("DEL").arg(&key).ignore();
    pipe.cmd("SADD")
        .arg(format!("{}:users", world.ns))
        .arg(name)
        .ignore();
    for (field, value) in fields {
        pipe.cmd("HSET").arg(&key).arg(field).arg(value).ignore();
    }
    pipe.query_async::<()>(&mut *conn).await.expect("stored");
}

/// Seed users in layout 2, and in layout 1 when `flat` is set
///
/// # Arguments
///
/// * `world` - The world to seed
/// * `flat` - Whether one user also keeps its theme in its own field
async fn seed_users(world: &World, flat: bool) {
    // two users in the layout the first Helm revision reads
    for name in ["crab", "plain"] {
        store_user(
            world,
            name,
            &[("username", name), ("settings", r#"{"theme":"Light"}"#)],
        )
        .await;
    }
    // a user with its theme in its own field
    if flat {
        store_user(
            world,
            "flat",
            &[("username", "flat"), ("theme", "\"Dark\"")],
        )
        .await;
    }
}

/// The outcomes the history holds for the reverse step
///
/// # Arguments
///
/// * `world` - The world to read
async fn reverse_outcomes(world: &World) -> Vec<StepState> {
    // the step's entries, oldest first
    let mut entries = rows(world, "history")
        .await
        .into_iter()
        .filter_map(|(_, entry)| entry)
        .map(|entry| serde_json::from_str::<AppliedStep>(&entry).expect("an entry"))
        .filter(|entry| entry.step == REVERSE_STEP)
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.at.cmp(&b.at));
    entries.into_iter().map(|entry| entry.outcome).collect()
}

/// Revert the real catalog's deployment by hand
///
/// # Arguments
///
/// * `world` - The world to revert
fn revert_real(world: &World) -> Output {
    // the same command an admin runs, over the real catalog
    World::run(
        world
            .thoradm()
            .args(["migrate", "revert", "--to", REAL_TO, "--backup", "x"]),
    )
}

/// A revert whose users are all in layout 2 has nothing to change: the step records that it was
/// skipped, and the older revision is recorded
#[tokio::test]
async fn a_revert_with_every_user_in_layout_2_records_the_step_skipped() {
    // a deployment at the later revision whose users are all in layout 2
    let world = World::new("rvr01").await;
    seed_real_record(&world).await;
    seed_users(&world, false).await;
    let output = revert_real(&world);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    // the step's own entry says it had nothing to change, and the older revision is recorded
    assert_eq!(
        reverse_outcomes(&world).await,
        vec![StepState::Running, StepState::Skipped]
    );
    assert_eq!(recorded_revision(&world).await.as_deref(), Some(REAL_TO));
    world.drop_keyspace().await;
}

/// A revert that converts a user records the step done, and the older revision is recorded
#[tokio::test]
async fn a_revert_that_converts_a_user_records_the_step_done() {
    // a deployment with one user whose theme is in its own field
    let world = World::new("rvr02").await;
    seed_real_record(&world).await;
    seed_users(&world, true).await;
    let output = revert_real(&world);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        reverse_outcomes(&world).await,
        vec![StepState::Running, StepState::Done]
    );
    assert_eq!(recorded_revision(&world).await.as_deref(), Some(REAL_TO));
    world.drop_keyspace().await;
}

/// `migrate run` of the reverse step by hand records its outcome too, whether or not it changed a
/// user
#[tokio::test]
async fn a_run_by_hand_of_the_reverse_step_records_its_outcome() {
    // a pass over users that need no change
    let world = World::new("rvr03").await;
    seed_real_record(&world).await;
    seed_users(&world, false).await;
    let output = World::run(
        world
            .thoradm()
            .args(["migrate", "run", "--id", REVERSE_STEP]),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(reverse_outcomes(&world).await, vec![StepState::Skipped]);
    world.drop_keyspace().await;
}

/// A revert records the step's outcome even when the environment names a Job, as it does for a
/// thoradm started from inside a pod the operator created
#[tokio::test]
async fn a_revert_records_the_step_even_when_the_environment_names_a_job() {
    // a deployment whose users are all in layout 2, reverted with a Job name in the environment
    let world = World::new("rvr04").await;
    seed_real_record(&world).await;
    seed_users(&world, false).await;
    let output = World::run(
        world
            .thoradm()
            .env("THORIUM_JOB_NAME", "some-job")
            .args(["migrate", "revert", "--to", REAL_TO, "--backup", "x"]),
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        reverse_outcomes(&world).await,
        vec![StepState::Running, StepState::Skipped]
    );
    assert_eq!(recorded_revision(&world).await.as_deref(), Some(REAL_TO));
    world.drop_keyspace().await;
}

/// A second run by hand of the reverse step, which finds the pass already finished, still records
/// an outcome entry
#[tokio::test]
async fn a_rerun_by_hand_of_the_reverse_step_records_an_outcome() {
    // a pass that converts one user, then a second run over the same data
    let world = World::new("rvr05").await;
    seed_real_record(&world).await;
    seed_users(&world, true).await;
    for _ in 0..2 {
        let output = World::run(
            world
                .thoradm()
                .args(["migrate", "run", "--id", REVERSE_STEP]),
        );
        assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    }
    // each run recorded its own entry
    let outcomes = reverse_outcomes(&world).await;
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert_eq!(outcomes[0], StepState::Done, "{outcomes:?}");
    world.drop_keyspace().await;
}
