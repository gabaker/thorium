//! Tests that the `images-to-tools` and `pipelines-to-tool-stages` migrations read each group's
//! listing once per pass: the number of listing reads (`SMEMBERS` and `SCAN`) a pass issues must
//! not grow with the rows it walks, which a listing repeated for every batch would do

mod common;

use common::migrate::World;
use std::collections::HashMap;
use thoradm::migrate::registry::{MIGRATIONS, MigrationDef};
use thorium::models::schema::{RunMode, RunOptions, check_pass};
use thorium::models::{Image, ImageScaler, WorkloadSpec};
use tokio::sync::Mutex;

/// The rows of each batch, small enough that every pass takes many batches
const BATCH_ROWS: usize = 50;

/// The rows of the small pass; the large pass has three times as many
const SMALL: usize = 500;

/// The most set reads an images pass may issue however many rows it walks (the groups, the group's
/// images, the group members, and the users the plan checks)
const MAX_LISTINGS: i64 = 8;

/// Serializes the tests, because they count commands the whole Redis server runs
static REDIS: Mutex<()> = Mutex::const_new(());

/// The calls of every Redis command the server has run, by lowercase command name
///
/// # Arguments
///
/// * `world` - The test world
async fn command_calls(world: &World) -> HashMap<String, i64> {
    // `INFO commandstats` has one `cmdstat_<name>:calls=<n>,...` line per command
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let info: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(&mut *conn)
        .await
        .expect("the command stats read");
    info.lines()
        .filter_map(|line| {
            let (name, rest) = line.strip_prefix("cmdstat_")?.split_once(':')?;
            let calls = rest.strip_prefix("calls=")?.split(',').next()?;
            Some((name.to_owned(), calls.parse().ok()?))
        })
        .collect()
}

/// How many calls of a command ran between two readings
///
/// # Arguments
///
/// * `before` - The reading before
/// * `after` - The reading after
/// * `name` - The lowercase command name
fn calls_between(before: &HashMap<String, i64>, after: &HashMap<String, i64>, name: &str) -> i64 {
    // a command that never ran has no line
    after.get(name).copied().unwrap_or(0) - before.get(name).copied().unwrap_or(0)
}

/// How many `SCAN` calls one walk of the keyspace takes, counted the way the pipelines listing
/// walks it
///
/// # Arguments
///
/// * `world` - The test world
async fn keyspace_scan_calls(world: &World) -> i64 {
    // the number of calls depends on the size of the server's hash table, not on the rows
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let mut calls = 0;
    let mut cursor = 0_u64;
    loop {
        let (next, _): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(format!("{}:pipelines:*", world.ns))
            .arg("COUNT")
            .arg(500)
            .query_async(&mut *conn)
            .await
            .expect("the scan reads");
        calls += 1;
        cursor = next;
        if cursor == 0 {
            return calls;
        }
    }
}

/// Store pipelines the way the API does, each with one stage that has no tool yet
///
/// # Arguments
///
/// * `world` - The test world
/// * `count` - How many pipelines to store
async fn seed_pipelines(world: &World, count: usize) {
    // one group, written in pipelined batches of 500
    let ns = &world.ns;
    let order = serde_json::to_string(&[["stage"]]).expect("order serializes");
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    for start in (0..count).step_by(500) {
        let mut pipe = redis::pipe();
        for index in start..(start + 500).min(count) {
            let name = format!("pipe-{index}");
            pipe.cmd("SADD")
                .arg(format!("{ns}:pipelines:perf"))
                .arg(&name)
                .ignore();
            pipe.cmd("HSET")
                .arg(format!("{ns}:pipeline_data:perf:{name}"))
                .arg("group")
                .arg("perf")
                .arg("name")
                .arg(&name)
                .arg("creator")
                .arg("io")
                .arg("order")
                .arg(&order)
                .ignore();
        }
        let _: () = pipe
            .query_async(&mut *conn)
            .await
            .expect("a pipeline batch stores");
    }
}

/// Store K8s images the way the API does, in one group whose member `io` exists
///
/// # Arguments
///
/// * `world` - The test world
/// * `count` - How many images to store
async fn seed_images(world: &World, count: usize) {
    // the group, its member, and the member as a user
    let ns = &world.ns;
    let mut conn = world
        .backends
        .redis
        .get()
        .await
        .expect("a Redis connection");
    let _: () = redis::pipe()
        .cmd("SADD")
        .arg(format!("{ns}:groups"))
        .arg("perf")
        .ignore()
        .cmd("SADD")
        .arg(format!("{ns}:groups_combined:perf:users"))
        .arg("io")
        .ignore()
        .cmd("HSET")
        .arg(format!("{ns}:user_data:io"))
        .arg("username")
        .arg("io")
        .ignore()
        .query_async(&mut *conn)
        .await
        .expect("the group stores");
    // every image shares the fields of one template apart from its name and container image
    let template: Image = {
        let mut image = WorkloadSpec::default().project_image("perf", "template", 600.0);
        image.scaler = ImageScaler::K8s;
        "io".clone_into(&mut image.creator);
        image
    };
    let fields = [
        ("scaler", json(&template.scaler)),
        ("resources", json(&template.resources)),
        ("spawn_limit", json(&template.spawn_limit)),
        ("volumes", json(&template.volumes)),
        ("env", json(&template.env)),
        ("args", json(&template.args)),
        ("security_context", json(&template.security_context)),
        ("collect_logs", json(&template.collect_logs)),
        ("generator", json(&template.generator)),
        ("dependencies", json(&template.dependencies)),
        ("display_type", json(&template.display_type)),
        ("output_collection", json(&template.output_collection)),
        ("child_filters", json(&template.child_filters)),
        ("network_policies", json(&template.network_policies)),
    ];
    for start in (0..count).step_by(500) {
        let mut pipe = redis::pipe();
        for index in start..(start + 500).min(count) {
            let name = format!("img-{index}");
            let mut cmd = redis::cmd("HSET");
            cmd.arg(format!("{ns}:image_data:perf:{name}"))
                .arg("group")
                .arg("perf")
                .arg("name")
                .arg(&name)
                .arg("creator")
                .arg("io")
                .arg("runtime")
                .arg(template.runtime)
                .arg("image")
                .arg(serde_json::to_string(&format!("repo/{name}:1.0")).expect("a string"));
            for (field, value) in &fields {
                cmd.arg(*field).arg(value);
            }
            pipe.add_command(cmd).ignore();
            pipe.cmd("SADD")
                .arg(format!("{ns}:images:perf"))
                .arg(&name)
                .ignore();
        }
        let _: () = pipe
            .query_async(&mut *conn)
            .await
            .expect("an image batch stores");
    }
}

/// A value as the JSON string the API stores an image field as
///
/// # Arguments
///
/// * `value` - The value to serialize
fn json<T: serde::Serialize>(value: &T) -> String {
    // the stored form of every image field is its JSON
    serde_json::to_string(value).expect("the field serializes")
}

/// Find a registered migration
///
/// # Arguments
///
/// * `id` - The migration's id
fn def(id: &str) -> &'static MigrationDef {
    // every migration is registered like the others
    MIGRATIONS
        .iter()
        .find(|def| def.id == id)
        .expect("the migration is registered")
}

/// Check one migration over `rows` seeded rows and count the listing reads of the pass
///
/// Returns the `SMEMBERS` calls and `SCAN` calls the pass issued, and the `SCAN` calls one walk
/// of the keyspace takes.
///
/// # Arguments
///
/// * `id` - The migration, `images` or `pipelines`
/// * `rows` - How many rows to seed
async fn listings_of_a_check(id: &str, rows: usize) -> (i64, i64, i64) {
    // a world of its own, seeded with the rows
    let world = World::new_full(&format!("scan{id}{rows}")).await;
    let def = if id == "images" {
        seed_images(&world, rows).await;
        def("images-to-tools")
    } else {
        seed_pipelines(&world, rows).await;
        def("pipelines-to-tool-stages")
    };
    // run the check with small batches between two readings of the server's counters
    let reference = keyspace_scan_calls(&world).await;
    let ctx = world.ctx(RunMode::Check, BATCH_ROWS);
    let before = command_calls(&world).await;
    let run = check_pass(def.runner, &World::spec(def), &RunOptions::default(), &ctx)
        .await
        .expect("the check runs");
    let after = command_calls(&world).await;
    // every seeded row was walked
    let walked = run.report.counts.read;
    assert!(
        walked >= rows as u64,
        "{id}: the check walked {walked} of {rows} rows"
    );
    world.drop_keyspace().await;
    (
        calls_between(&before, &after, "smembers"),
        calls_between(&before, &after, "scan"),
        reference,
    )
}

/// A pipelines check lists every group's pipelines once, not once per batch
#[tokio::test]
async fn pipelines_listings_do_not_grow_with_rows() {
    // the listing is one walk of the keyspace and one read of each group's set, however many
    // batches the rows take
    let _guard = REDIS.lock().await;
    for rows in [SMALL, SMALL * 3] {
        let (smembers, scan, reference) = Box::pin(listings_of_a_check("pipelines", rows)).await;
        assert!(smembers <= 2, "{smembers} set reads for {rows} pipelines");
        assert!(
            scan <= reference + 2,
            "{scan} scan calls for {rows} pipelines, one walk takes {reference}"
        );
    }
}

/// An images check lists the groups and each group's images once, not once per batch
#[tokio::test]
async fn images_listings_do_not_grow_with_rows() {
    // the groups and the one group's images are each read once, however many batches the rows take
    let _guard = REDIS.lock().await;
    for rows in [SMALL, SMALL * 3] {
        let (smembers, scan, _) = Box::pin(listings_of_a_check("images", rows)).await;
        assert!(
            smembers <= MAX_LISTINGS,
            "{smembers} set reads for {rows} images"
        );
        assert_eq!(scan, 0, "{scan} scan calls for {rows} images");
    }
}
