//! The steps of a round trip: back up, wipe, restore, and rebuild the census, each run the way an
//! admin runs them
//!
//! The wipe removes everything a disaster would: the rows of every table the registry doesn't mark
//! EPHEMERAL, every Redis key of the namespace, and the objects of every bucket that isn't
//! EPHEMERAL. The one exception is the admin's own login (its record, its token, and its place in
//! the user set): the Redis half of a restore goes through the API, which needs an admin to accept
//! it, and the restore writes those same values back from the backup.

use super::harness::{
    BUCKET_ROLES, Harness, bucket_for, delete_redis_namespace, empty_bucket, redis_pool,
};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Output;
use thoradm::backup::coverage::{COVERAGE, Category, Coverage, Store};
use thoradm::backup::failures::{FAILURES_FILE, FailureRecord, read_records};

/// Runs a thoradm command and returns its output
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `args` - The command's arguments after the config flags
fn run(harness: &Harness, args: &[&std::ffi::OsStr]) -> Output {
    // run the binary against this namespace's configs
    harness
        .thoradm()
        .args(args)
        .output()
        .expect("failed to run thoradm")
}

/// Describes a finished thoradm run for an assertion message
///
/// # Arguments
///
/// * `output` - The run's output
#[must_use]
pub fn describe(output: &Output) -> String {
    // the exit code and the tail of stderr
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(40).collect();
    format!(
        "exit {:?}\n{}",
        output.status.code(),
        tail.into_iter().rev().collect::<Vec<_>>().join("\n")
    )
}

/// Backs up the namespace into a directory
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `dir` - Where to write the backup
#[must_use]
pub fn backup(harness: &Harness, dir: &Path) -> Output {
    // every component, with a small worker count and chunk multiplier
    run(
        harness,
        &[
            "--workers".as_ref(),
            "4".as_ref(),
            "backup".as_ref(),
            "new".as_ref(),
            "-m".as_ref(),
            "4".as_ref(),
            "-o".as_ref(),
            dir.as_os_str(),
        ],
    )
}

/// Restores a backup into the namespace
///
/// The restore accepts a backup's recorded failures: a round trip checks those records itself,
/// and restores what the backup holds.
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `dir` - The backup to restore
#[must_use]
pub fn restore(harness: &Harness, dir: &Path) -> Output {
    // confirm without a terminal
    restore_with(harness, dir, &["--accept-failures"])
}

/// Restores a backup into the namespace with extra `backup restore` arguments
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `dir` - The backup to restore
/// * `extra` - The extra arguments (`--legacy`, `--merge`, ...)
#[must_use]
pub fn restore_with(harness: &Harness, dir: &Path, extra: &[&str]) -> Output {
    // confirm without a terminal
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "--workers".as_ref(),
        "4".as_ref(),
        "backup".as_ref(),
        "restore".as_ref(),
        "--yes".as_ref(),
        "-b".as_ref(),
        dir.as_os_str(),
    ];
    args.extend(extra.iter().map(std::ffi::OsStr::new));
    run(harness, &args)
}

/// Checks a backup directory against its manifest with `backup verify`, without the cluster
///
/// # Arguments
///
/// * `harness` - The harness whose configs thoradm loads
/// * `dir` - The backup to check
#[must_use]
pub fn verify(harness: &Harness, dir: &Path) -> Output {
    // the offline check of the backup directory
    run(
        harness,
        &[
            "backup".as_ref(),
            "verify".as_ref(),
            "-b".as_ref(),
            dir.as_os_str(),
        ],
    )
}

/// Rebuilds the census of every kind thoradm counts
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
#[must_use]
pub fn census(harness: &Harness) -> Output {
    // every census kind
    run(
        harness,
        &[
            "--workers".as_ref(),
            "4".as_ref(),
            "census".as_ref(),
            "new".as_ref(),
        ],
    )
}

/// Returns the tables a wipe empties: every registry table that isn't EPHEMERAL
#[must_use]
pub fn wiped_tables() -> Vec<&'static str> {
    // excluded runtime tables are never wiped
    COVERAGE
        .iter()
        .filter(|entry| entry.store == Store::ScyllaTable && entry.category != Category::Ephemeral)
        .map(|entry| entry.name)
        .collect()
}

/// Returns the bucket roles a wipe empties: every role that isn't EPHEMERAL
#[must_use]
pub fn wiped_buckets() -> Vec<&'static str> {
    // per-reaction scratch and cache buckets are never wiped
    BUCKET_ROLES
        .iter()
        .copied()
        .filter(|role| {
            !COVERAGE.iter().any(|entry| {
                entry.store == Store::S3
                    && entry.name == *role
                    && entry.category == Category::Ephemeral
            })
        })
        .collect()
}

/// Wipes the namespace as a disaster would, keeping only the admin's login
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
///
/// # Panics
///
/// Panics if a store can't be wiped.
pub async fn wipe(harness: &Harness) {
    // empty every table that isn't runtime state
    let scylla = harness.scylla().await;
    let ns = &harness.conf.thorium.namespace;
    for table in wiped_tables() {
        scylla
            .query_unpaged(format!("TRUNCATE {ns}.{table}"), ())
            .await
            .unwrap_or_else(|err| panic!("failed to truncate {table}: {err}"));
    }
    // save the admin's login, delete every key of the namespace, and put the login back
    let login = admin_login(harness).await;
    delete_redis_namespace(&harness.conf)
        .await
        .expect("failed to delete the namespace's keys");
    put_admin_login(harness, &login).await;
    // empty every bucket that isn't runtime state
    let s3 = harness.s3();
    for role in wiped_buckets() {
        empty_bucket(&s3, &bucket_for(&harness.conf, role))
            .await
            .expect("failed to empty a bucket");
    }
}

/// Reads the harness admin's login: the fields of its user record
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
///
/// # Panics
///
/// Panics if Redis can't be read.
pub async fn admin_login(harness: &Harness) -> BTreeMap<String, String> {
    // the admin's user record holds its token and email
    let pool = redis_pool(&harness.conf)
        .await
        .expect("failed to connect to Redis");
    let mut conn = pool.get().await.expect("failed to connect to Redis");
    let ns = &harness.conf.thorium.namespace;
    redis::cmd("HGETALL")
        .arg(format!("{ns}:user_data:{}", super::harness::ADMIN))
        .query_async(&mut *conn)
        .await
        .expect("failed to read the admin's login")
}

/// Writes the harness admin's login back: its record, its token and email entries, and its place
/// in the user set, so the harness can authenticate after a restore replaced the admin
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `login` - The fields of the admin's user record, as [`admin_login`] read them
///
/// # Panics
///
/// Panics if Redis can't be written.
pub async fn put_admin_login(harness: &Harness, login: &BTreeMap<String, String>) {
    // one pipeline writes every part of the login
    let pool = redis_pool(&harness.conf)
        .await
        .expect("failed to connect to Redis");
    let mut conn = pool.get().await.expect("failed to connect to Redis");
    let ns = &harness.conf.thorium.namespace;
    let admin = super::harness::ADMIN;
    let data_key = format!("{ns}:user_data:{admin}");
    let mut pipe = redis::pipe();
    for (field, value) in login {
        pipe.cmd("HSET").arg(&data_key).arg(field).arg(value);
    }
    if let Some(token) = login.get("token") {
        pipe.cmd("HSET")
            .arg(format!("{ns}:users_token_map"))
            .arg(token)
            .arg(admin);
    }
    if let Some(email) = login.get("email") {
        pipe.cmd("HSET")
            .arg(format!("{ns}:users_email_map"))
            .arg(email)
            .arg(admin);
    }
    pipe.cmd("SADD").arg(format!("{ns}:users")).arg(admin);
    let _: () = pipe
        .query_async(&mut *conn)
        .await
        .expect("failed to restore the admin's login");
}

/// Returns every failure record a run left anywhere under a backup directory: the backup's own
/// failures file and each restore report's
///
/// # Arguments
///
/// * `dir` - The backup directory
#[must_use]
pub fn failure_records(dir: &Path) -> Vec<FailureRecord> {
    // walk the directory for failures files
    let mut records = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(listing) = std::fs::read_dir(&next) else {
            continue;
        };
        for item in listing.filter_map(Result::ok) {
            let path = item.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().is_some_and(|name| name == FAILURES_FILE) {
                records.extend(read_records(&path).expect("failed to read a failures file"));
            }
        }
    }
    records
}

/// Returns the registry entries, named as in `entry_name`, that a failure record's component
/// backs up
///
/// # Arguments
///
/// * `record` - The failure record
#[must_use]
pub fn record_entries(record: &FailureRecord) -> Vec<String> {
    // the entries of the record's component
    component_entries(&record.component)
}

/// Returns the registry entries, named as in `entry_name`, that a backup component covers
///
/// # Arguments
///
/// * `component` - The component's name, as runs and failure records print it
#[must_use]
pub fn component_entries(component: &str) -> Vec<String> {
    // every entry backed up by the component
    COVERAGE
        .iter()
        .filter(|entry| match entry.coverage {
            Coverage::BackedUp(backed_up) => backed_up.to_string() == component,
            Coverage::Excluded { .. } => false,
        })
        .map(|entry| super::classes::entry_name(entry.store, entry.name))
        .collect()
}
