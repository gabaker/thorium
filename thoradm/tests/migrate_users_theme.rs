//! Tests of the users example's migrations through the real runner: users convert in one pass, a
//! second pass changes nothing, the checkpoint is the last username done, an unreadable user holds
//! the step until its code is accepted, and the migrations back to the first Helm revision leave
//! users in layout 2, which that revision reads

mod common;

use async_trait::async_trait;
use common::migrate::World;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use thoradm::migrate::context::spec_for;
use thoradm::migrate::registry::{Backend, MIGRATIONS, MigrationDef, Support};
use thorium::models::schema::{
    Batch, MigrateError, MigrationRunStatus, MigrationRunner, Outcome, PassOutcome, RunContext,
    RunMode, RunOptions, Slice, run_pass,
};
use thorium::models::upgrades::Record;
use tokio_util::sync::CancellationToken;

/// The AI settings of a user who has any
const AI: &str = r#"{"endpoints":{},"default_endpoint":"x"}"#;

/// Find a registered migration
///
/// # Arguments
///
/// * `id` - The migration's id
fn def(id: &str) -> &'static MigrationDef {
    // the registry holds the example's migrations
    MIGRATIONS
        .iter()
        .find(|def| def.id == id)
        .unwrap_or_else(|| panic!("{id} is registered"))
}

/// Store a user's hash, replacing any left by an earlier run, and add it to the user index
///
/// # Arguments
///
/// * `world` - The test's world
/// * `name` - The username
/// * `fields` - The fields of the hash
async fn store(world: &World, name: &str, fields: &[(&str, &str)]) {
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

/// Remove every user of the world's namespace, so a rerun starts from nothing
///
/// # Arguments
///
/// * `world` - The test's world
async fn clear(world: &World) {
    // delete each user's hash, then the index
    let mut conn = world.backends.redis.get().await.expect("a connection");
    let users: Vec<String> = redis::cmd("SMEMBERS")
        .arg(format!("{}:users", world.ns))
        .query_async(&mut *conn)
        .await
        .expect("members");
    for user in users {
        let _: () = redis::cmd("DEL")
            .arg(format!("{}:user_data:{user}", world.ns))
            .query_async(&mut *conn)
            .await
            .expect("deleted");
    }
    let _: () = redis::cmd("DEL")
        .arg(format!("{}:users", world.ns))
        .query_async(&mut *conn)
        .await
        .expect("deleted");
}

/// Record the layout the Redis backend row holds for users, as `schema apply` does, so a pass
/// has a record to advance
///
/// # Arguments
///
/// * `world` - The test's world
/// * `layout` - The layout the row starts at
async fn record_users_layout(world: &World, layout: i64) {
    // the backend row of Redis, with the users family at the layout
    world
        .session()
        .query_unpaged(
            format!(
                "INSERT INTO {}.schema_migrations (kind, id, layouts) \
                 VALUES ('backend', 'redis', {{'users': {layout}}})",
                world.ns
            ),
            (),
        )
        .await
        .expect("the backend row is written");
}

/// The layout the Redis backend row records for users
///
/// # Arguments
///
/// * `world` - The test's world
async fn recorded_users_layout(world: &World) -> Option<i64> {
    // read the row's layouts map
    let rows = world
        .session()
        .query_unpaged(
            format!(
                "SELECT layouts FROM {}.schema_migrations WHERE kind = 'backend' AND id = 'redis'",
                world.ns
            ),
            (),
        )
        .await
        .expect("the row reads")
        .into_rows_result()
        .expect("rows");
    let (layouts,) = rows
        .first_row::<(Option<std::collections::BTreeMap<String, i64>>,)>()
        .expect("the backend row");
    layouts.and_then(|layouts| layouts.get("users").copied())
}

/// Read a user's hash
///
/// # Arguments
///
/// * `world` - The test's world
/// * `name` - The username
async fn hash(world: &World, name: &str) -> BTreeMap<String, String> {
    // every field of the hash
    let mut conn = world.backends.redis.get().await.expect("a connection");
    redis::cmd("HGETALL")
        .arg(format!("{}:user_data:{name}", world.ns))
        .query_async(&mut *conn)
        .await
        .expect("hash")
}

/// The `settings` field of a user as JSON
///
/// # Arguments
///
/// * `fields` - The user's hash
fn settings(fields: &BTreeMap<String, String>) -> Option<Value> {
    // the field holds JSON text
    fields
        .get("settings")
        .map(|raw| serde_json::from_str(raw).expect("settings are JSON"))
}

/// Seed `count` layout 1 users named `<prefix><nn>`, alternating the theme, with `ai` settings on
/// every third
///
/// # Arguments
///
/// * `world` - The test's world
/// * `prefix` - The usernames' prefix
/// * `count` - How many users
async fn seed_layout_1(world: &World, prefix: &str, count: usize) {
    // the flat theme, and settings only when the user has ai settings
    for n in 0..count {
        let name = format!("{prefix}{n:02}");
        let theme = if n % 2 == 0 { "\"Dark\"" } else { "\"Light\"" };
        let ai = format!(r#"{{"ai":{AI}}}"#);
        let mut fields = vec![("username", name.as_str()), ("theme", theme)];
        if n % 3 == 0 {
            fields.push(("settings", ai.as_str()));
        }
        store(world, &name, &fields).await;
    }
}

/// Run a migration through the runner
///
/// # Arguments
///
/// * `world` - The test's world
/// * `runner` - The runner to run
/// * `id` - The registered migration whose pass this is
/// * `accept` - The reason codes accepted
/// * `slices` - How many slices to split the family into
/// * `batch_rows` - The rows of each batch
async fn run(
    world: &World,
    runner: &dyn MigrationRunner,
    id: &str,
    accept: &[&str],
    slices: u32,
    batch_rows: usize,
) -> Result<PassOutcome, MigrateError> {
    // the registry's pass spec, with the codes accepted
    let spec = spec_for(def(id), accept.iter().map(|code| (*code).to_owned()), None);
    let ctx = world.ctx(RunMode::Run, batch_rows);
    let lease = world.lease("test").await;
    let opts = RunOptions {
        slices,
        ..RunOptions::default()
    };
    let outcome = run_pass(runner, &spec, &opts, &ctx, &world.store, &lease).await;
    lease.release().await;
    outcome
}

/// A runner that records every write and cancels the run after a number of batches
struct Recording {
    /// The migration it wraps
    inner: &'static dyn MigrationRunner,
    /// The token to cancel
    cancel: CancellationToken,
    /// Cancel after this many batches have been read, if any
    stop_after: Option<usize>,
    /// How many batches were read
    batches: AtomicUsize,
    /// The key of every record written, in order
    written: Mutex<Vec<String>>,
    /// The key of every record classified, in order
    processed: Mutex<Vec<String>>,
}

#[async_trait]
impl MigrationRunner for Recording {
    /// Split the family as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // the wrapped migration decides
        self.inner.slices(n, ctx).await
    }

    /// Read a batch as the wrapped migration does, cancelling once enough were read
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice
    /// * `after` - The last position done
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // read, then stop the run when the limit is reached
        let batch = self.inner.next_batch(slice, after, ctx).await?;
        let read = self.batches.fetch_add(1, Ordering::SeqCst) + 1;
        if self.stop_after.is_some_and(|limit| read >= limit) {
            self.cancel.cancel();
        }
        Ok(batch)
    }

    /// Classify as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `record` - The record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // remember what was classified before the wrapped migration decides
        if let Ok(mut processed) = self.processed.lock() {
            processed.push(thorium::models::schema::record_key(record).to_owned());
        }
        self.inner.process(record, mode, ctx).await
    }

    /// Write as the wrapped migration does, remembering the key
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // remember what was written before writing it
        if let Ok(mut written) = self.written.lock() {
            written.push(thorium::models::schema::record_key(record).to_owned());
        }
        self.inner.write(record, ctx).await
    }

    /// Finish as the wrapped migration does
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // the wrapped migration decides
        self.inner.finish(ctx).await
    }
}

/// The example's migrations are registered as Redis migrations the runner can run, with the group
/// limit the users family needs, and each has a pass of its own
#[test]
fn the_example_migrations_are_registered() {
    let up = def("users-settings-theme");
    let all = [up, def(RESTORING[0])];
    for migration in all {
        assert!(matches!(migration.backend, Backend::Redis));
        assert_eq!(migration.support, Support::Yes);
        assert_eq!(migration.group_max, 0);
    }
    // no two share a pass
    assert_ne!(all[0].pass, all[1].pass);
}

/// One pass converts every layout 1 user, leaves the users already in layout 2 alone, and a second
/// pass classifies every user as current and writes nothing
#[tokio::test]
async fn users_convert_in_one_pass_and_a_second_pass_changes_nothing() {
    let world = World::new("uthm").await;
    clear(&world).await;
    record_users_layout(&world, 1).await;
    seed_layout_1(&world, "old", 24).await;
    store(
        &world,
        "new00",
        &[("username", "new00"), ("settings", r#"{"theme":"Crab"}"#)],
    )
    .await;
    // the first pass converts the 24 layout 1 users
    let first = run(
        &world,
        def("users-settings-theme").runner,
        "users-settings-theme",
        &[],
        3,
        5,
    )
    .await
    .expect("the pass runs");
    assert_eq!(first.status, MigrationRunStatus::Done);
    assert_eq!(first.counts.migratable, 24);
    assert_eq!(first.counts.current, 1);
    assert_eq!(first.counts.written, 24);
    // the record of the users layout advanced with the pass
    assert_eq!(recorded_users_layout(&world).await, Some(2));
    // every user holds its theme in settings and no flat field
    let mut converted = BTreeMap::new();
    for n in 0..24 {
        let name = format!("old{n:02}");
        let fields = hash(&world, &name).await;
        assert!(!fields.contains_key("theme"), "{name}");
        let theme = if n % 2 == 0 { "Dark" } else { "Light" };
        let stored = settings(&fields).expect("settings");
        assert_eq!(stored["theme"], theme, "{name}");
        assert_eq!(stored.get("ai").is_some(), n % 3 == 0, "{name}");
        converted.insert(name, fields);
    }
    // the second pass finds nothing to do and changes no user
    let second = run(
        &world,
        def("users-settings-theme").runner,
        "users-settings-theme",
        &[],
        3,
        5,
    )
    .await
    .expect("the pass runs");
    assert_eq!(second.status, MigrationRunStatus::Done);
    assert_eq!(second.counts.migratable, 0);
    assert_eq!(second.counts.current, 25);
    assert_eq!(second.counts.written, 0);
    for (name, fields) in converted {
        assert_eq!(hash(&world, &name).await, fields, "{name}");
    }
    world.drop_keyspace().await;
}

/// The checkpoint is the last username done in sorted order, and resuming from it processes each
/// remaining user once
#[tokio::test]
async fn the_checkpoint_is_the_last_username_and_a_resume_converts_each_user_once() {
    let world = World::new("uthc").await;
    clear(&world).await;
    seed_layout_1(&world, "u", 10).await;
    let migration = def("users-settings-theme");
    // interrupt the first run after two batches of three users
    let cancel = CancellationToken::new();
    let stopper = Recording {
        inner: migration.runner,
        cancel: cancel.clone(),
        stop_after: Some(2),
        batches: AtomicUsize::new(0),
        written: Mutex::new(Vec::new()),
        processed: Mutex::new(Vec::new()),
    };
    let spec = spec_for(migration, std::iter::empty(), None);
    let mut ctx = world.ctx(RunMode::Run, 3);
    ctx.cancel = cancel;
    let lease = world.lease("test").await;
    let opts = RunOptions {
        slices: 1,
        ..RunOptions::default()
    };
    let first = run_pass(&stopper, &spec, &opts, &ctx, &world.store, &lease)
        .await
        .expect("the pass runs");
    lease.release().await;
    assert!(first.interrupted);
    // the checkpoint names the last user of the sorted prefix that was converted
    let row = world
        .store
        .pass_row(&spec.key)
        .await
        .expect("row")
        .expect("a pass row");
    let (workers, _) = world
        .store
        .rows(&spec.key, &row.attempt)
        .await
        .expect("rows");
    let position = workers[0].checkpoint.position.clone();
    let last = position.as_str().expect("a username").to_owned();
    let mut converted = Vec::new();
    for n in 0..10 {
        let name = format!("u{n:02}");
        if !hash(&world, &name).await.contains_key("theme") {
            converted.push(name);
        }
    }
    assert!(
        !converted.is_empty() && converted.len() < 10,
        "{converted:?}"
    );
    assert_eq!(converted.last(), Some(&last), "{converted:?}");
    // the resumed run converts the rest, each user once across both runs
    let resumed = Recording {
        inner: migration.runner,
        cancel: CancellationToken::new(),
        stop_after: None,
        batches: AtomicUsize::new(0),
        written: Mutex::new(Vec::new()),
        processed: Mutex::new(Vec::new()),
    };
    let ctx = world.ctx(RunMode::Run, 3);
    let lease = world.lease("test").await;
    let second = run_pass(&resumed, &spec, &opts, &ctx, &world.store, &lease)
        .await
        .expect("the pass runs");
    lease.release().await;
    assert_eq!(second.status, MigrationRunStatus::Done);
    assert!(second.resumed);
    let mut written = stopper.written.lock().expect("lock").clone();
    written.extend(resumed.written.lock().expect("lock").clone());
    written.sort();
    let mut expected = (0..10)
        .map(|n| format!("user_data:u{n:02}"))
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(written, expected, "each user is written exactly once");
    // the resumed run classified only the users after the checkpoint, each once
    let mut processed = resumed.processed.lock().expect("lock").clone();
    processed.sort();
    let after = (0..10)
        .map(|n| format!("u{n:02}"))
        .filter(|name| name.as_str() > last.as_str())
        .map(|name| format!("user_data:{name}"))
        .collect::<Vec<_>>();
    assert_eq!(processed, after, "the resume starts after the checkpoint");
    world.drop_keyspace().await;
}

/// One unreadable user holds the step: the run fails without finishing, and with that user's code
/// accepted the run finishes with issues, having converted every other user
#[tokio::test]
async fn an_unreadable_user_holds_the_step_until_its_code_is_accepted() {
    let world = World::new("uthg").await;
    clear(&world).await;
    record_users_layout(&world, 1).await;
    seed_layout_1(&world, "ok", 12).await;
    store(&world, "bad", &[("username", "bad"), ("theme", "\"Neon\"")]).await;
    let migration = def("users-settings-theme");
    // a group over the users' limit of 0 fails the pass
    let failed = run(&world, migration.runner, migration.id, &[], 2, 4)
        .await
        .expect("the pass runs");
    assert_eq!(failed.status, MigrationRunStatus::Failed);
    let group = failed
        .groups
        .iter()
        .find(|group| group.reason == "unknown_variant:theme")
        .expect("the group of the bad user");
    assert_eq!(group.count, 1);
    assert!(group.samples.contains("user_data:bad"));
    assert!(!group.accepted);
    // the bad user is untouched, and the record of the users layout didn't advance
    assert_eq!(hash(&world, "bad").await["theme"], "\"Neon\"");
    assert_eq!(recorded_users_layout(&world).await, Some(1));
    // accepting the code lets the pass finish with issues, every other user converted
    let accepted = run(
        &world,
        migration.runner,
        migration.id,
        &["unknown_variant:theme"],
        2,
        4,
    )
    .await
    .expect("the pass runs");
    assert_eq!(accepted.status, MigrationRunStatus::DoneWithIssues);
    assert_eq!(recorded_users_layout(&world).await, Some(2));
    for n in 0..12 {
        let fields = hash(&world, &format!("ok{n:02}")).await;
        assert!(!fields.contains_key("theme"));
        assert!(settings(&fields).is_some());
    }
    assert_eq!(hash(&world, "bad").await["theme"], "\"Neon\"");
    world.drop_keyspace().await;
}

/// The migrations that leave users the way the first Helm revision reads them, which is layout 2:
/// the reverse of the revision after it
const RESTORING: [&str; 1] = ["users-settings-theme-reverse"];

/// A migration back to the first Helm revision skips every user already in layout 2, keeps its
/// theme, converts a user stored with a flat theme, and records layout 2,
/// so every user loads on the first Helm revision
#[tokio::test]
async fn a_migration_back_to_the_first_revision_leaves_users_in_layout_2() {
    for id in RESTORING {
        let world = World::new("uback").await;
        clear(&world).await;
        record_users_layout(&world, 2).await;
        // users the first revision wrote, with a theme and AI settings in `settings`, and the
        // pre-release's users, whose flat theme and AI-only settings the first revision can't load
        let nested = format!(r#"{{"theme":"Crab","ai":{AI}}}"#);
        let ai_only = format!(r#"{{"ai":{AI}}}"#);
        store(
            &world,
            "crab",
            &[("username", "crab"), ("settings", &nested)],
        )
        .await;
        store(
            &world,
            "plain",
            &[("username", "plain"), ("settings", r#"{"theme":"Light"}"#)],
        )
        .await;
        store(
            &world,
            "flat",
            &[("username", "flat"), ("theme", "\"Dark\"")],
        )
        .await;
        store(
            &world,
            "flat-ai",
            &[
                ("username", "flat-ai"),
                ("theme", "\"Ocean\""),
                ("settings", &ai_only),
            ],
        )
        .await;
        let before = (hash(&world, "crab").await, hash(&world, "plain").await);
        let restore = def(id);
        let outcome = run(&world, restore.runner, restore.id, &[], 2, 4)
            .await
            .expect("the migration runs");
        assert_eq!(outcome.status, MigrationRunStatus::Done, "{id}");
        // the users already in layout 2 are untouched, theme included
        assert_eq!(
            (hash(&world, "crab").await, hash(&world, "plain").await),
            before,
            "{id}"
        );
        // the pre-release's users move their theme into settings, keeping the AI settings
        let flat = hash(&world, "flat").await;
        assert_eq!(
            settings(&flat),
            Some(serde_json::json!({"theme": "Dark"})),
            "{id}"
        );
        assert!(!flat.contains_key("theme"), "{id}");
        let flat_ai = hash(&world, "flat-ai").await;
        assert_eq!(
            settings(&flat_ai).expect("settings")["theme"],
            "Ocean",
            "{id}"
        );
        assert!(
            settings(&flat_ai).expect("settings")["ai"].is_object(),
            "{id}"
        );
        assert!(!flat_ai.contains_key("theme"), "{id}");
        // every user loads the way the first revision loads it, and the record says layout 2
        for name in ["crab", "plain", "flat", "flat-ai"] {
            let raw = hash(&world, name).await;
            let loaded = serde_json::from_str::<thorium::models::UserSettings>(&raw["settings"]);
            assert!(loaded.is_ok(), "{id}: {name}: {loaded:?}");
        }
        assert_eq!(recorded_users_layout(&world).await, Some(2), "{id}");
        world.drop_keyspace().await;
    }
}
