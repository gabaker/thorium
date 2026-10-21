//! `backup restore --redis-flush-all`: the API flushes its whole Redis database with
//! `FLUSHDB ASYNC` instead of deleting only the namespace's keys, after the same validation and
//! before the same rebuild; without the flag only the namespace's keys are deleted
//!
//! A flush deletes every key in the Redis database, the keys of every test of this binary
//! included, so each test holds the binary's restore guard.

mod common;

use common::cycle::{admin_login, describe, put_admin_login, restore_with};
use common::harness::{Harness, redis_pool};
use common::names::Names;
use common::seed;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use thorium::Thorium;
use thorium::client::{ClientSettings, Users};
use thorium::models::{RestoreParams, UserCreate};

/// The Redis database a marker that a flush of the API's database must leave alone is written to
const OTHER_DB: u8 = 1;

/// The keys a test plants around a restore: one of the namespace, one of a neighbouring
/// namespace in the same database, and one of the namespace in another database
struct Markers {
    /// A key of the restored namespace that the backup doesn't hold
    own: String,
    /// A key of a neighbouring namespace whose name extends the restored one
    neighbour: String,
    /// A key of another namespace that holds nothing but the marker
    stranger: String,
}

impl Markers {
    /// Plants the markers in the harness's Redis
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the namespace and Redis server
    /// * `name` - A name that keeps this test's markers apart from other tests'
    async fn plant(harness: &Harness, name: &str) -> Self {
        // one key in the namespace, one in a neighbour sharing its prefix, and one elsewhere
        let ns = &harness.conf.thorium.namespace;
        let markers = Markers {
            own: format!("{ns}:flush_marker_{name}"),
            neighbour: format!("{ns}_neighbour:flush_marker_{name}"),
            stranger: format!("stranger_{ns}:flush_marker_{name}"),
        };
        let pool = redis_pool(&harness.conf).await.unwrap();
        let mut conn = pool.get().await.unwrap();
        let () = redis::pipe()
            .cmd("SET")
            .arg(&markers.own)
            .arg("own")
            .ignore()
            .cmd("SET")
            .arg(&markers.neighbour)
            .arg("neighbour")
            .ignore()
            .cmd("SET")
            .arg(&markers.stranger)
            .arg("stranger")
            .ignore()
            .query_async(&mut *conn)
            .await
            .unwrap();
        // the same namespace key in another database, which only FLUSHALL would delete
        let () = redis::pipe()
            .cmd("SELECT")
            .arg(OTHER_DB)
            .ignore()
            .cmd("SET")
            .arg(&markers.own)
            .arg("other database")
            .ignore()
            .cmd("SELECT")
            .arg(0)
            .ignore()
            .query_async(&mut *conn)
            .await
            .unwrap();
        markers
    }

    /// Reads which markers of the API's database are still there: the namespace's, the
    /// neighbour's, the stranger's, and the other database's
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the Redis server
    async fn present(&self, harness: &Harness) -> (bool, bool, bool, bool) {
        // check the three keys of the API's database, then the other database's key
        let pool = redis_pool(&harness.conf).await.unwrap();
        let mut conn = pool.get().await.unwrap();
        let (own, neighbour, stranger): (bool, bool, bool) = redis::pipe()
            .cmd("EXISTS")
            .arg(&self.own)
            .cmd("EXISTS")
            .arg(&self.neighbour)
            .cmd("EXISTS")
            .arg(&self.stranger)
            .query_async(&mut *conn)
            .await
            .unwrap();
        let (other,): (bool,) = redis::pipe()
            .cmd("SELECT")
            .arg(OTHER_DB)
            .ignore()
            .cmd("EXISTS")
            .arg(&self.own)
            .cmd("SELECT")
            .arg(0)
            .ignore()
            .query_async(&mut *conn)
            .await
            .unwrap();
        (own, neighbour, stranger, other)
    }

    /// Deletes every marker that is left, so no test leaves keys outside its namespace
    ///
    /// # Arguments
    ///
    /// * `harness` - The harness naming the Redis server
    async fn remove(&self, harness: &Harness) {
        // delete the keys of the API's database, then the other database's key
        let pool = redis_pool(&harness.conf).await.unwrap();
        let mut conn = pool.get().await.unwrap();
        let () = redis::pipe()
            .cmd("DEL")
            .arg(&self.own)
            .arg(&self.neighbour)
            .arg(&self.stranger)
            .ignore()
            .cmd("SELECT")
            .arg(OTHER_DB)
            .ignore()
            .cmd("DEL")
            .arg(&self.own)
            .ignore()
            .cmd("SELECT")
            .arg(0)
            .ignore()
            .query_async(&mut *conn)
            .await
            .unwrap();
    }
}

/// Seeds a group and takes a backup of the Redis state alone, returning the backup and the group
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `name` - The scratch directory's name
async fn redis_backup(harness: &Harness, name: &str) -> (PathBuf, String) {
    // a group the restore must bring back
    let names = Names::new();
    let group = names.group("hestia");
    seed::group(&harness.client().await, &group).await;
    // back up the redis component only, so the restore writes no table or bucket
    let dir = harness.scratch(name);
    let output = harness
        .thoradm()
        .args(["backup", "new", "redis", "-o"])
        .arg(&dir)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    (dir, group)
}

/// Reads the warnings of the newest restore summary under a backup
///
/// # Arguments
///
/// * `backup` - The backup the restore read
fn summary_warnings(backup: &Path) -> Vec<String> {
    // the newest restore report holds the summary
    let mut reports: Vec<PathBuf> = std::fs::read_dir(backup.join("reports"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("restore-"))
        })
        .collect();
    reports.sort();
    let summary: serde_json::Value = serde_json::from_slice(
        &std::fs::read(reports.last().unwrap().join("summary.json")).unwrap(),
    )
    .unwrap();
    summary["warnings"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|warning| warning.as_str().map(ToOwned::to_owned))
        .collect()
}

/// Says what a confirmation names about the flush: the Redis host, the namespace, that every
/// key in the database is deleted, and the flag
///
/// # Arguments
///
/// * `harness` - The harness naming the Redis server and namespace
fn flush_confirmation(harness: &Harness) -> Vec<String> {
    // the host and port, the namespace, the database-wide deletion, and the flag
    let conf = &harness.conf;
    vec![
        format!(
            "deletes every key in the Redis database at {}:{}",
            conf.redis.host, conf.redis.port
        ),
        format!("namespace {}", conf.thorium.namespace),
        "--redis-flush-all".to_owned(),
        "other namespaces' keys included".to_owned(),
        "FLUSHDB ASYNC".to_owned(),
    ]
}

/// With `--redis-flush-all` the restore completes and every key of the API's Redis database is
/// gone, the namespace's and a neighbouring namespace's alike, while the same key in another
/// database survives; the confirmation and the summary say the whole database was flushed
#[tokio::test]
async fn the_flag_flushes_every_key_of_the_database_and_the_restore_completes() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, group) = redis_backup(harness, "flush-all").await;
    let markers = Markers::plant(harness, "flush").await;
    // restore with the flag
    let run = restore_with(harness, &backup, &["--redis-flush-all"]);
    let present = markers.present(harness).await;
    markers.remove(harness).await;
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    // every key of the database is gone, and the other database is untouched
    assert_eq!(
        present,
        (false, false, false, true),
        "(own, neighbour, stranger, other database) after a flush"
    );
    // the backup's state is back, so the API serves the group with the restored admin
    let client = harness.client().await;
    client
        .groups
        .get(&group)
        .await
        .expect("the restored group is missing");
    // the confirmation names the host, the namespace, the flag, and the database-wide deletion
    let stderr = String::from_utf8_lossy(&run.stderr);
    for text in flush_confirmation(harness) {
        assert!(stderr.contains(&text), "the log lacks {text:?}: {stderr}");
    }
    assert!(
        !stderr.contains("keys of other namespaces are left alone"),
        "{stderr}"
    );
    // the summary says the database was flushed, not that only the namespace was deleted
    let warnings = summary_warnings(&backup);
    let conf = &harness.conf;
    let flushed = format!(
        "the Redis restore flushed every key in the Redis database at {}:{} with FLUSHDB ASYNC \
         (--redis-flush-all) before loading the backup, namespace {}'s job queues and reaction \
         state and other namespaces' keys included",
        conf.redis.host, conf.redis.port, conf.thorium.namespace
    );
    assert!(warnings.contains(&flushed), "{warnings:#?}");
    assert!(
        !warnings
            .iter()
            .any(|warning| warning.contains("deleted every key of namespace")),
        "{warnings:#?}"
    );
}

/// Without the flag the restore deletes only the namespace's keys: a neighbouring namespace's
/// keys in the same database survive, and the confirmation says nothing of a flush
#[tokio::test]
async fn without_the_flag_a_neighbours_keys_survive() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, group) = redis_backup(harness, "namespace-wipe").await;
    let markers = Markers::plant(harness, "wipe").await;
    // restore without the flag
    let run = restore_with(harness, &backup, &[]);
    let present = markers.present(harness).await;
    markers.remove(harness).await;
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    // only the namespace's key is gone
    assert_eq!(
        present,
        (false, true, true, true),
        "(own, neighbour, stranger, other database) after a namespace wipe"
    );
    harness
        .client()
        .await
        .groups
        .get(&group)
        .await
        .expect("the restored group is missing");
    // the confirmation describes the namespace wipe only
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(
        stderr.contains("keys of other namespaces are left alone"),
        "{stderr}"
    );
    assert!(!stderr.contains("--redis-flush-all"), "{stderr}");
    assert!(
        !stderr.contains("deletes every key in the Redis database"),
        "{stderr}"
    );
}

/// A user who isn't an admin can't restore with the flush: the API answers 401 and no key of
/// the database is deleted
#[tokio::test]
async fn a_non_admin_cannot_flush_the_database() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let admin = harness.client().await;
    let backup = admin.system.backup().await.unwrap();
    // a plain user with a password
    let name = Names::new().user("callisto");
    let password = "flush-password";
    let user =
        UserCreate::new(&name, password, format!("{name}@example.invalid")).skip_verification();
    let key = Some(harness.conf.thorium.secret_key.as_str());
    Users::create(&admin.host, user, key, &ClientSettings::default())
        .await
        .unwrap();
    let client = Thorium::build(harness.api_url.clone())
        .basic_auth(name.clone(), password.to_owned())
        .build()
        .await
        .unwrap();
    // the user's flushing restore is refused, and every marker survives
    let markers = Markers::plant(harness, "non-admin").await;
    let refused = client
        .system
        .restore_with(&backup, &RestoreParams::default().flush_db())
        .await;
    let present = markers.present(harness).await;
    markers.remove(harness).await;
    let error = refused.expect_err("a non-admin restored with a flush");
    assert_eq!(error.status().map(|status| status.as_u16()), Some(401));
    assert_eq!(present, (true, true, true, true));
    // the user is removed, so the namespace holds the users it held before
    admin.users.delete(&name).await.unwrap();
}

/// A backup the API refuses leaves Redis untouched with a flush too: the API validates it first
/// and answers 400, and a restore thoradm's preflight refuses never reaches the API
#[tokio::test]
async fn a_refused_backup_leaves_redis_untouched_with_the_flag() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let client = harness.client().await;
    // a backup holding one user twice, which the API's validation refuses
    let mut invalid = client.system.backup().await.unwrap();
    let twin = invalid.users[0].clone();
    invalid.users.push(twin);
    let markers = Markers::plant(harness, "refused").await;
    let refused = client
        .system
        .restore_with(&invalid, &RestoreParams::default().flush_db())
        .await;
    let present = markers.present(harness).await;
    let error = refused.expect_err("a backup with a duplicate user was restored");
    assert_eq!(error.status().map(|status| status.as_u16()), Some(400));
    assert_eq!(
        present,
        (true, true, true, true),
        "the API's refusal deleted keys"
    );
    // a restore without a terminal and without --yes is refused by thoradm's preflight
    let (backup, _group) = redis_backup(harness, "flush-refused").await;
    let run: Output = harness
        .thoradm()
        .args(["backup", "restore", "--redis-flush-all", "-b"])
        .arg(&backup)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm");
    let present = markers.present(harness).await;
    markers.remove(harness).await;
    assert_eq!(run.status.code(), Some(1), "{}", describe(&run));
    assert_eq!(
        present,
        (true, true, true, true),
        "the preflight's refusal deleted keys"
    );
}

/// Sets an ACL rule on the Redis server's default user, which the harness and the API use
///
/// # Arguments
///
/// * `harness` - The harness naming the Redis server
/// * `rule` - The ACL rule (`-flushdb`, `+@all`)
async fn acl(harness: &Harness, rule: &str) {
    // ACL SETUSER changes what every connection of the user may run at once
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg("default")
        .arg(rule)
        .query_async(&mut *conn)
        .await
        .unwrap();
}

/// A flagged restore that fails names the step it failed in, the flush itself included, and its
/// summary says the whole database was flushed when a step after the flush failed: a refused
/// flush deletes nothing, while a refused write after it leaves every key of the database gone
#[tokio::test]
async fn a_failed_restore_with_the_flag_says_what_the_flush_deleted() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let (backup, _) = redis_backup(harness, "flush-failed").await;
    let login = admin_login(harness).await;
    let conf = &harness.conf;
    let warning = format!(
        "the Redis restore was asked to flush every key in the Redis database at {}:{} with \
         FLUSHDB ASYNC (--redis-flush-all) and failed; when its error names a step after the \
         flush step, every key in that database was deleted, namespace {}'s and other \
         namespaces' alike, and when it names the flush step, nothing was deleted",
        conf.redis.host, conf.redis.port, conf.thorium.namespace
    );
    let mut problems = Vec::new();
    // Redis refuses the flush itself, then the first write after it (init's HSETNX)
    for (rule, step, flushed) in [("-flushdb", "flush", false), ("-hsetnx", "init", true)] {
        let markers = Markers::plant(harness, &format!("failed{step}")).await;
        acl(harness, rule).await;
        let run = restore_with(harness, &backup, &["--redis-flush-all"]);
        acl(harness, "+@all").await;
        let present = markers.present(harness).await;
        markers.remove(harness).await;
        // the run fails, naming the step
        if run.status.code() != Some(2) {
            problems.push(format!("{rule}: {}", describe(&run)));
        }
        let stderr = String::from_utf8_lossy(&run.stderr);
        if !stderr.contains(&format!("the restore failed in the {step} step")) {
            problems.push(format!(
                "{rule}: the error doesn't name the {step} step: {stderr}"
            ));
        }
        // a neighbouring namespace's key is gone only when the flush ran
        if present.2 == flushed {
            problems.push(format!(
                "{rule}: the stranger's key {} the restore",
                if flushed { "survived" } else { "is gone after" }
            ));
        }
        // and the summary says what a failed flagged restore deleted
        let warnings = summary_warnings(&backup);
        if !warnings.contains(&warning) {
            problems.push(format!(
                "{rule}: the summary lacks the flush warning: {warnings:#?}"
            ));
        }
        // the admin's login is gone with the flushed users; put it and the backup's state back
        if flushed {
            put_admin_login(harness, &login).await;
            let rerun = restore_with(harness, &backup, &["--redis-flush-all"]);
            assert_eq!(rerun.status.code(), Some(0), "{}", describe(&rerun));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
