//! `backup new --prune-dangling`: the user and group references that stay dangling through every
//! read, and whose referent is confirmed gone, are dropped from the Redis state and recorded in
//! `pruned.jsonl`; without the flag they fail the backup, and every other problem fails it with
//! the flag too
//!
//! Every test changes the namespace's Redis state in ways another test's backup would trip over,
//! so each holds the binary's restore guard.

mod common;

use common::cycle::{describe, restore_with, verify};
use common::harness::{ADMIN, Harness, redis_pool};
use common::names::Names;
use common::seed;
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;
use std::time::Duration;
use thoradm::backup::failures::{FAILURES_FILE, FailureRecord, read_records};
use thoradm::backup::manifest::{MANIFEST_FILE, Manifest, file_sha256};
use thorium::client::{ClientSettings, Users};
use thorium::models::{
    Backup, BackupParams, GroupUpdate, GroupUsersUpdate, ScopedToken, ScopedTokenRequest,
    UserCreate,
};
use thorium::test_utilities::generators;

/// The file a pruning backup lists what it dropped in
const PRUNED_FILE: &str = "pruned.jsonl";

/// Runs one Redis command against the namespace's Redis, ignoring its reply
///
/// # Arguments
///
/// * `harness` - The harness naming the Redis server
/// * `command` - The command
async fn redis_run(harness: &Harness, command: &redis::Cmd) {
    // any reply is fine; only a failure to run matters
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let _: redis::Value = command.query_async(&mut *conn).await.unwrap();
}

/// Reads one field of a Redis hash
///
/// # Arguments
///
/// * `harness` - The harness naming the Redis server
/// * `key` - The hash
/// * `field` - The field
async fn hget(harness: &Harness, key: &str, field: &str) -> String {
    // the field must be there
    let pool = redis_pool(&harness.conf).await.unwrap();
    let mut conn = pool.get().await.unwrap();
    let value: Option<String> = redis::cmd("HGET")
        .arg(key)
        .arg(field)
        .query_async(&mut *conn)
        .await
        .unwrap();
    value.unwrap_or_else(|| panic!("{key} has no {field}"))
}

/// Adds a member to a Redis set
///
/// # Arguments
///
/// * `harness` - The harness naming the Redis server
/// * `set` - The set
/// * `member` - The member to add
async fn sadd(harness: &Harness, set: &str, member: &str) {
    // add the member
    redis_run(harness, redis::cmd("SADD").arg(set).arg(member)).await;
}

/// Removes a member from a Redis set
///
/// # Arguments
///
/// * `harness` - The harness naming the Redis server
/// * `set` - The set
/// * `member` - The member to remove
async fn srem(harness: &Harness, set: &str, member: &str) {
    // remove the member
    redis_run(harness, redis::cmd("SREM").arg(set).arg(member)).await;
}

/// Sets one field of a Redis hash
///
/// # Arguments
///
/// * `harness` - The harness naming the Redis server
/// * `key` - The hash
/// * `field` - The field
/// * `value` - Its new value
async fn hset(harness: &Harness, key: &str, field: &str, value: &str) {
    // write the field
    redis_run(harness, redis::cmd("HSET").arg(key).arg(field).arg(value)).await;
}

/// Builds the `backup new redis` command into a directory, with `--prune-dangling` when asked
///
/// # Arguments
///
/// * `harness` - The harness whose configs thoradm loads
/// * `out` - The backup directory
/// * `prune` - Whether to pass `--prune-dangling`
fn backup_command(harness: &Harness, out: &Path, prune: bool) -> std::process::Command {
    // only the Redis state, into a new directory
    let mut command = harness.thoradm();
    command.args(["backup", "new", "redis", "-o"]).arg(out);
    if prune {
        command.arg("--prune-dangling");
    }
    command
}

/// Runs `backup new redis` into a directory, with `--prune-dangling` when asked
///
/// # Arguments
///
/// * `harness` - The harness whose configs thoradm loads
/// * `out` - The backup directory
/// * `prune` - Whether to pass `--prune-dangling`
fn backup_redis(harness: &Harness, out: &Path, prune: bool) -> Output {
    // run it to its end
    backup_command(harness, out, prune)
        .output()
        .expect("failed to run thoradm")
}

/// Reads the `redis.json` of a backup
///
/// # Arguments
///
/// * `out` - The backup directory
fn redis_json(out: &Path) -> Backup {
    // the whole file is one backup
    let bytes = std::fs::read(out.join("redis.json")).expect("no redis.json");
    serde_json::from_slice(&bytes).unwrap()
}

/// Reads the lines of a backup's `pruned.jsonl` that name one record
///
/// # Arguments
///
/// * `out` - The backup directory
/// * `record` - The record the lines must name
fn pruned_lines(out: &Path, record: &str) -> Vec<Value> {
    // one JSON object per line
    let text = std::fs::read_to_string(out.join(PRUNED_FILE)).expect("no pruned.jsonl");
    text.lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["record"] == record)
        .collect()
}

/// Returns the line a pruning backup writes for one change, without its reason
///
/// # Arguments
///
/// * `line` - The line read from `pruned.jsonl`
fn without_reason(line: &Value) -> Value {
    // every line has a reason saying why; the rest says what was changed
    assert!(
        line["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty()),
        "{line}"
    );
    let mut line = line.clone();
    line.as_object_mut().unwrap().remove("reason");
    line
}

/// Returns the changes `pruned.jsonl` lists for one record, without their reasons
///
/// # Arguments
///
/// * `out` - The backup directory
/// * `record` - The record the changes must name
fn changes(out: &Path, record: &str) -> Vec<Value> {
    // each line checked for a reason, then compared without it
    pruned_lines(out, record)
        .iter()
        .map(without_reason)
        .collect()
}

/// Builds the line a pruning backup writes for a change to a scoped token's groups, without its
/// reason
///
/// # Arguments
///
/// * `token` - The token's name, owned by the admin
/// * `action` - What was done
/// * `group` - The group dropped, or `None` when the token was deleted
fn token_change(token: &str, action: &str, group: Option<&str>) -> Value {
    // a deleted token names no group
    serde_json::json!({
        "record": format!("scoped_tokens/{ADMIN}/{token}"),
        "field": "groups",
        "action": action,
        "name": group,
        "referent": group.map(|_| "group"),
    })
}

/// Sets the groups an admin's scoped token is stored with
///
/// # Arguments
///
/// * `harness` - The harness naming the Redis server
/// * `name` - The token's name
/// * `groups` - The groups to store
async fn set_token_groups(harness: &Harness, name: &str, groups: Vec<String>) {
    // rewrite the token's stored JSON with the groups
    let tokens = format!("{}:scoped_tokens:{ADMIN}", harness.conf.thorium.namespace);
    let mut token: ScopedToken = serde_json::from_str(&hget(harness, &tokens, name).await).unwrap();
    token.groups = groups;
    hset(
        harness,
        &tokens,
        name,
        &serde_json::to_string(&token).unwrap(),
    )
    .await;
}

/// Returns an admin's scoped token from a backup
///
/// # Arguments
///
/// * `backup` - The backup
/// * `name` - The token's name
fn held_token(backup: &Backup, name: &str) -> Option<ScopedToken> {
    // the admin's token of that name
    backup
        .scoped_tokens
        .iter()
        .find(|token| token.owner == ADMIN && token.name == name)
        .cloned()
}

/// Returns whether a failure names a record and some text in its error
///
/// # Arguments
///
/// * `records` - The failures recorded
/// * `key` - The record's key
/// * `text` - Text its error must contain
fn recorded(records: &[FailureRecord], key: &str, text: &str) -> bool {
    // the key is the record, and the error says what is wrong with it
    records
        .iter()
        .any(|record| record.key.as_deref() == Some(key) && record.error.contains(text))
}

/// Creates a plain user with the API's secret key, returning its name
///
/// # Arguments
///
/// * `harness` - The harness naming the API and its secret key
/// * `name` - The user's name
async fn create_user(harness: &Harness, name: &str) {
    // a verified user with a password
    let client = harness.client().await;
    let user = UserCreate::new(name, "prune-password", format!("{name}@example.invalid"))
        .skip_verification();
    let key = Some(harness.conf.thorium.secret_key.as_str());
    Users::create(&client.host, user, key, &ClientSettings::default())
        .await
        .unwrap();
}

/// Checks that a backup failed: exit 2, no `redis.json`, and no `pruned.jsonl`, returning its
/// failure records
///
/// # Arguments
///
/// * `run` - The backup's output
/// * `out` - The backup directory
fn assert_failed(run: &Output, out: &Path) -> Vec<FailureRecord> {
    // the run finished with records, and wrote neither file
    assert_eq!(run.status.code(), Some(2), "{}", describe(run));
    assert!(!out.join("redis.json").exists(), "{}", describe(run));
    assert!(!out.join(PRUNED_FILE).exists(), "{}", describe(run));
    read_records(&out.join(FAILURES_FILE)).unwrap()
}

/// A group owner naming a user that is gone fails a plain backup; with `--prune-dangling` it is
/// dropped from `redis.json`, listed in a private `pruned.jsonl` the manifest counts and pins,
/// and the run exits 2; verify accepts the backup, a restore needs `--accept-failures`, and the
/// restored group no longer names the user
#[tokio::test]
async fn a_confirmed_dangling_owner_is_dropped_only_with_the_flag() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    // a group whose owners include a user that doesn't exist
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("pluto");
    seed::group(&client, &group).await;
    let ghost = names.user("carme");
    for set in [
        format!("{ns}:groups_direct:{group}:owners"),
        format!("{ns}:groups_combined:{group}:owners"),
    ] {
        sadd(harness, &set, &ghost).await;
    }
    // without the flag the backup fails, naming the owner
    let plain = harness.scratch("owner-plain");
    let records = assert_failed(&backup_redis(harness, &plain, false), &plain);
    assert!(
        recorded(&records, &format!("groups/{group}"), &ghost),
        "{records:#?}"
    );
    // with it the owner is dropped from the group, and the run exits 2
    let pruned = harness.scratch("owner-pruned");
    let run = backup_redis(harness, &pruned, true);
    assert_eq!(run.status.code(), Some(2), "{}", describe(&run));
    let backup = redis_json(&pruned);
    let held = backup
        .groups
        .iter()
        .find(|held| held.name == group)
        .expect("the group isn't in the backup");
    assert!(!held.owners.direct.contains(&ghost), "{held:?}");
    assert!(!held.owners.combined.contains(&ghost), "{held:?}");
    assert!(held.owners.direct.contains(ADMIN), "{held:?}");
    assert!(backup.pruned.is_empty(), "{:?}", backup.pruned);
    // pruned.jsonl is private and lists the drop, and nothing failed
    let path = pruned.join(PRUNED_FILE);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let lines = pruned_lines(&pruned, &format!("groups/{group}"));
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert_eq!(
        without_reason(&lines[0]),
        serde_json::json!({
            "record": format!("groups/{group}"),
            "field": "owners",
            "action": "dropped",
            "name": ghost,
            "referent": "user",
        })
    );
    let total = std::fs::read_to_string(&path).unwrap().lines().count() as u64;
    // the manifest counts the lines and pins the file
    let manifest = Manifest::read(&pruned).await.unwrap().expect("no manifest");
    let entry = manifest
        .pruned
        .clone()
        .expect("the manifest counts nothing pruned");
    assert_eq!(entry.count, total);
    assert_eq!(entry.sha256, file_sha256(&path).await.unwrap());
    assert_eq!(manifest.failures, 0);
    // verify accepts the backup, and fails one whose pruned.jsonl changed
    let checked = verify(harness, &pruned);
    assert_eq!(checked.status.code(), Some(0), "{}", describe(&checked));
    let original = std::fs::read(&path).unwrap();
    std::fs::write(&path, [original.as_slice(), b"\n"].concat()).unwrap();
    let checked = verify(harness, &pruned);
    assert_eq!(checked.status.code(), Some(2), "{}", describe(&checked));
    std::fs::write(&path, &original).unwrap();
    // a restore is refused without --accept-failures, naming the file
    let refused = restore_with(harness, &pruned, &[]);
    assert_eq!(refused.status.code(), Some(1), "{}", describe(&refused));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains(PRUNED_FILE), "{}", describe(&refused));
    assert!(
        stderr.contains("--accept-failures"),
        "{}",
        describe(&refused)
    );
    // with it the restore writes the group without the owner
    let restored = restore_with(harness, &pruned, &["--accept-failures"]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let after = client.groups.get(&group).await.unwrap();
    assert!(!after.owners.combined.contains(&ghost), "{after:?}");
    assert!(after.owners.direct.contains(ADMIN), "{after:?}");
}

/// A member that is missing from the API's first read but back by a later one is kept, so the
/// pruning backup drops nothing; a backup without the flag of a consistent state writes no
/// `pruned.jsonl` and no `pruned` field in its manifest or `redis.json`
#[tokio::test]
async fn a_reference_that_resolves_on_a_later_read_is_kept() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    // a group with a member
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("ceres");
    seed::group(&client, &group).await;
    let member = names.user("thebe");
    create_user(harness, &member).await;
    let update = GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(&member));
    client.groups.update(&group, &update).await.unwrap();
    // the member's account is missing when the API first reads the records, and back before its
    // last read, which comes at least 600 ms after the first; the request goes straight to the
    // API, so its first read starts at once
    let users = format!("{ns}:users");
    srem(harness, &users, &member).await;
    let params = BackupParams::default().prune_dangling();
    let backup = client.system.backup_with(&params);
    let restore_member = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        sadd(harness, &users, &member).await;
    };
    let (backup, ()) = tokio::join!(backup, restore_member);
    let backup = backup.unwrap();
    // the member is kept and nothing is pruned
    assert!(backup.pruned.is_empty(), "{:?}", backup.pruned);
    let held = backup
        .groups
        .iter()
        .find(|held| held.name == group)
        .expect("the group isn't in the backup");
    assert!(held.users.direct.contains(&member), "{held:?}");
    // a backup without the flag writes nothing about pruning
    let plain = harness.scratch("resolved-plain");
    let run = backup_redis(harness, &plain, false);
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    assert!(!plain.join(PRUNED_FILE).exists());
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(plain.join(MANIFEST_FILE)).unwrap()).unwrap();
    assert!(manifest.get("pruned").is_none(), "{manifest:#}");
    let redis: Value =
        serde_json::from_slice(&std::fs::read(plain.join("redis.json")).unwrap()).unwrap();
    assert!(redis.get("pruned").is_none());
}

/// A user whose groups name a group that is gone fails a plain backup (422) and a restore through
/// the API (400), naming the group; with `--prune-dangling` the membership is dropped and listed
#[tokio::test]
async fn a_membership_in_a_missing_group_fails_without_the_flag_and_is_pruned_with_it() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    // a user whose groups include a group that doesn't exist
    let client = harness.client().await;
    let names = Names::new();
    let user = names.user("leda");
    create_user(harness, &user).await;
    let ghost = names.group("vesta");
    let memberships = format!("{ns}:user_groups:{user}");
    sadd(harness, &memberships, &ghost).await;
    // without the flag the backup fails, naming the user and the group
    let plain = harness.scratch("membership-plain");
    let records = assert_failed(&backup_redis(harness, &plain, false), &plain);
    assert!(
        recorded(&records, &format!("users/{user}"), &ghost),
        "{records:#?}"
    );
    // a restore of a backup holding the membership is refused before anything changes
    srem(harness, &memberships, &ghost).await;
    let mut held = client.system.backup().await.unwrap();
    held.users
        .iter_mut()
        .find(|held| held.username == user)
        .expect("the user isn't in the backup")
        .groups
        .push(ghost.clone());
    let Err(error) = client.system.restore(&held).await else {
        panic!("a restore of a membership in a missing group succeeded");
    };
    assert_eq!(error.status().map(|code| code.as_u16()), Some(400));
    let body = error.msg().unwrap_or_default();
    assert!(body.contains(&format!("users/{user}")), "{body}");
    assert!(body.contains(&ghost), "{body}");
    // with the flag the membership is dropped and listed
    sadd(harness, &memberships, &ghost).await;
    let pruned = harness.scratch("membership-pruned");
    let run = backup_redis(harness, &pruned, true);
    assert_eq!(run.status.code(), Some(2), "{}", describe(&run));
    let backup = redis_json(&pruned);
    let kept = backup
        .users
        .iter()
        .find(|held| held.username == user)
        .expect("the user isn't in the backup");
    assert!(!kept.groups.contains(&ghost), "{kept:?}");
    let lines = pruned_lines(&pruned, &format!("users/{user}"));
    assert_eq!(lines.len(), 1, "{lines:#?}");
    assert_eq!(
        without_reason(&lines[0]),
        serde_json::json!({
            "record": format!("users/{user}"),
            "field": "groups",
            "action": "dropped",
            "name": ghost,
            "referent": "group",
        })
    );
    // put the user back as it was
    srem(harness, &memberships, &ghost).await;
}

/// Dropping a group's last direct owner, in a group with no owner metagroup, adds the `thorium`
/// user as its direct owner and member and lists the addition after the drop
#[tokio::test]
async fn the_last_owner_gives_way_to_the_thorium_user() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    // a group whose only owner is a user that doesn't exist
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("janus");
    seed::group(&client, &group).await;
    let ghost = names.user("adrastea");
    let owners = [
        format!("{ns}:groups_direct:{group}:owners"),
        format!("{ns}:groups_combined:{group}:owners"),
    ];
    let admin_groups = format!("{ns}:user_groups:{ADMIN}");
    for set in &owners {
        srem(harness, set, ADMIN).await;
        sadd(harness, set, &ghost).await;
    }
    srem(harness, &admin_groups, &group).await;
    // the pruned backup's group is owned by thorium, which is a member of it
    let pruned = harness.scratch("last-owner");
    let run = backup_redis(harness, &pruned, true);
    assert_eq!(run.status.code(), Some(2), "{}", describe(&run));
    let backup = redis_json(&pruned);
    let held = backup
        .groups
        .iter()
        .find(|held| held.name == group)
        .expect("the group isn't in the backup");
    let expected: std::collections::HashSet<String> = [ADMIN.to_owned()].into();
    assert_eq!(held.owners.direct, expected);
    assert_eq!(held.owners.combined, expected);
    let admin = backup
        .users
        .iter()
        .find(|held| held.username == ADMIN)
        .expect("the admin isn't in the backup");
    assert!(admin.groups.contains(&group), "{:?}", admin.groups);
    // the drop and the addition are listed, in that order
    let lines: Vec<Value> = pruned_lines(&pruned, &format!("groups/{group}"))
        .iter()
        .map(without_reason)
        .collect();
    assert_eq!(
        lines,
        vec![
            serde_json::json!({
                "record": format!("groups/{group}"),
                "field": "owners",
                "action": "dropped",
                "name": ghost,
                "referent": "user",
            }),
            serde_json::json!({
                "record": format!("groups/{group}"),
                "field": "owners",
                "action": "added",
                "name": ADMIN,
                "referent": "user",
            }),
        ]
    );
    // put the group back as it was
    for set in &owners {
        srem(harness, set, &ghost).await;
        sadd(harness, set, ADMIN).await;
    }
    sadd(harness, &admin_groups, &group).await;
}

/// When the `thorium` user takes the place of a group's last owner while it is one of the
/// group's users, it leaves the users, as the API leaves an owner no lower role: in the pruned
/// backup, which lists the change, and in the group the restore writes
#[tokio::test]
async fn the_thorium_user_holds_only_the_owner_role_it_takes() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    // a group whose only owner is a user that doesn't exist, with thorium one of its users
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("hyperion");
    seed::group(&client, &group).await;
    let ghost = names.user("phoebe");
    let owners = [
        format!("{ns}:groups_direct:{group}:owners"),
        format!("{ns}:groups_combined:{group}:owners"),
    ];
    let users = [
        format!("{ns}:groups_direct:{group}:users"),
        format!("{ns}:groups_combined:{group}:users"),
    ];
    for set in &owners {
        srem(harness, set, ADMIN).await;
        sadd(harness, set, &ghost).await;
    }
    for set in &users {
        sadd(harness, set, ADMIN).await;
    }
    // the pruned backup's group is owned by thorium, which is none of its users any more
    let pruned = harness.scratch("owner-not-user");
    let run = backup_redis(harness, &pruned, true);
    assert_eq!(run.status.code(), Some(2), "{}", describe(&run));
    let backup = redis_json(&pruned);
    let held = backup
        .groups
        .iter()
        .find(|held| held.name == group)
        .expect("the group isn't in the backup");
    let expected: std::collections::HashSet<String> = [ADMIN.to_owned()].into();
    assert_eq!(held.owners.direct, expected, "{held:?}");
    assert_eq!(held.owners.combined, expected, "{held:?}");
    for (role, members) in [
        ("managers", &held.managers),
        ("users", &held.users),
        ("monitors", &held.monitors),
    ] {
        assert!(
            !members.direct.contains(ADMIN) && !members.combined.contains(ADMIN),
            "thorium is still one of the group's {role}: {held:?}"
        );
    }
    // the list says thorium left the users after it took the owner's place
    assert_eq!(
        changes(&pruned, &format!("groups/{group}")),
        vec![
            serde_json::json!({
                "record": format!("groups/{group}"),
                "field": "owners",
                "action": "dropped",
                "name": ghost,
                "referent": "user",
            }),
            serde_json::json!({
                "record": format!("groups/{group}"),
                "field": "owners",
                "action": "added",
                "name": ADMIN,
                "referent": "user",
            }),
            serde_json::json!({
                "record": format!("groups/{group}"),
                "field": "users",
                "action": "dropped",
                "name": ADMIN,
                "referent": "user",
            }),
        ]
    );
    // the restored group holds thorium as its owner only
    let restored = restore_with(harness, &pruned, &["--accept-failures"]);
    assert_eq!(restored.status.code(), Some(0), "{}", describe(&restored));
    let after = harness
        .client()
        .await
        .groups
        .get(&group)
        .await
        .expect("the restored group is missing");
    assert!(after.owners.combined.contains(ADMIN), "{after:?}");
    assert!(
        !after.users.direct.contains(ADMIN) && !after.users.combined.contains(ADMIN),
        "thorium is both an owner and a user of the restored group: {after:?}"
    );
    // nothing of the ghost is left to put back, and thorium owns the group as it did
    for set in &users {
        srem(harness, set, ADMIN).await;
    }
}

/// A scoped token's groups that are gone are kept by a plain backup, as before; with
/// `--prune-dangling` they are dropped, narrowing the token, and a token left with no group is
/// deleted; the list names the tokens but never their values
#[tokio::test]
async fn a_scoped_tokens_missing_groups_are_dropped_and_an_empty_token_is_deleted() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    // two scoped tokens of the admin in a group
    let client = harness.client().await;
    let names = Names::new();
    let group = names.group("saturn");
    seed::group(&client, &group).await;
    let (narrowed, emptied) = (names.user("sinope"), names.user("metis"));
    let mut values = Vec::new();
    for name in [&narrowed, &emptied] {
        let token = client
            .users
            .create_scoped_token(&ScopedTokenRequest::new(name).group(&group))
            .await
            .unwrap();
        values.push(token.token);
    }
    // one also names a group that is gone, and the other names only one that is gone
    let (gone_a, gone_b) = (names.group("bacchus"), names.group("diana"));
    set_token_groups(harness, &narrowed, vec![group.clone(), gone_a.clone()]).await;
    set_token_groups(harness, &emptied, vec![gone_b.clone()]).await;
    // a plain backup keeps both tokens as they are
    let plain = harness.scratch("tokens-plain");
    let run = backup_redis(harness, &plain, false);
    assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
    let backup = redis_json(&plain);
    let kept = held_token(&backup, &narrowed).expect("the narrowed token is gone");
    assert!(kept.groups.contains(&gone_a), "{kept:?}");
    assert!(held_token(&backup, &emptied).is_some());
    // with the flag the names are dropped and the emptied token is deleted
    let pruned = harness.scratch("tokens-pruned");
    let run = backup_redis(harness, &pruned, true);
    assert_eq!(run.status.code(), Some(2), "{}", describe(&run));
    let backup = redis_json(&pruned);
    let kept = held_token(&backup, &narrowed).expect("the narrowed token is gone");
    assert_eq!(kept.groups, vec![group.clone()]);
    let deleted = held_token(&backup, &emptied);
    assert!(deleted.is_none(), "{deleted:?}");
    assert_eq!(
        changes(&pruned, &format!("scoped_tokens/{ADMIN}/{narrowed}")),
        vec![token_change(&narrowed, "dropped", Some(&gone_a))]
    );
    assert_eq!(
        changes(&pruned, &format!("scoped_tokens/{ADMIN}/{emptied}")),
        vec![
            token_change(&emptied, "dropped", Some(&gone_b)),
            token_change(&emptied, "deleted", None),
        ]
    );
    // no token value is written to the list
    let text = std::fs::read_to_string(pruned.join(PRUNED_FILE)).unwrap();
    for value in &values {
        assert!(!text.contains(value.as_str()));
    }
    // remove both tokens
    let tokens = format!("{ns}:scoped_tokens:{ADMIN}");
    let map = format!("{ns}:scoped_token_map");
    for (name, value) in [&narrowed, &emptied].into_iter().zip(&values) {
        redis_run(harness, redis::cmd("HDEL").arg(&tokens).arg(name.as_str())).await;
        redis_run(harness, redis::cmd("HDEL").arg(&map).arg(value.as_str())).await;
    }
}

/// With `--prune-dangling`, an image naming a group that is gone, a pipeline running an image
/// its group doesn't hold, a scoped token holding a primary token's value, and a group name that
/// isn't a valid key segment still fail the backup, each named, and nothing is written, even
/// though a dangling owner beside them could be pruned
#[tokio::test]
async fn everything_but_user_and_group_references_still_fails_with_the_flag() {
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    let ns = harness.conf.thorium.namespace.clone();
    // a group with an image, a pipeline, and a scoped token
    let client = harness.client().await;
    let names = Names::new();
    let (groups, images, pipes) = generators::gen_all(1, &client).await.unwrap();
    let group = groups[0].name.clone();
    let image_key = format!("{ns}:image_data:{group}:{}", images[0].name);
    let pipe_key = format!("{ns}:pipeline_data:{group}:{}", pipes[0].name);
    let token_name = names.user("ananke");
    client
        .users
        .create_scoped_token(&ScopedTokenRequest::new(&token_name).group(&group))
        .await
        .unwrap();
    let tokens = format!("{ns}:scoped_tokens:{ADMIN}");
    let original_token = hget(harness, &tokens, &token_name).await;
    // a prunable owner that is gone
    let ghost = names.user("lysithea");
    let owners = format!("{ns}:groups_direct:{group}:owners");
    sadd(harness, &owners, &ghost).await;
    // the image names a group that is gone
    let original_group = hget(harness, &image_key, "group").await;
    let gone_group = names.group("neptune");
    hset(harness, &image_key, "group", &gone_group).await;
    // the pipeline runs an image its group doesn't hold
    let original_order = hget(harness, &pipe_key, "order").await;
    hset(harness, &pipe_key, "order", r#"[["ghost-image-prune"]]"#).await;
    // the scoped token holds the admin's primary token
    let mut token: ScopedToken = serde_json::from_str(&original_token).unwrap();
    token.token.clone_from(&harness.token);
    hset(
        harness,
        &tokens,
        &token_name,
        &serde_json::to_string(&token).unwrap(),
    )
    .await;
    // a group whose name holds the key separator
    let bad = format!("bad:{}", names.suffix);
    let data: Vec<(String, String)> = {
        let pool = redis_pool(&harness.conf).await.unwrap();
        let mut conn = pool.get().await.unwrap();
        redis::cmd("HGETALL")
            .arg(format!("{ns}:groups_data:{group}"))
            .query_async(&mut *conn)
            .await
            .unwrap()
    };
    for (field, value) in &data {
        hset(harness, &format!("{ns}:groups_data:{bad}"), field, value).await;
    }
    sadd(harness, &format!("{ns}:groups"), &bad).await;
    // the backup fails, naming each, and writes nothing
    let out = harness.scratch("still-fatal");
    let records = assert_failed(&backup_redis(harness, &out, true), &out);
    let image = format!("images/{gone_group}/{}", images[0].name);
    assert!(
        recorded(&records, &image, "isn't in the backup"),
        "{records:#?}"
    );
    let pipeline = format!("pipelines/{group}/{}", pipes[0].name);
    assert!(
        recorded(&records, &pipeline, "ghost-image-prune"),
        "{records:#?}"
    );
    let scoped = format!("scoped_tokens/{ADMIN}/{token_name}");
    assert!(recorded(&records, &scoped, "primary token"), "{records:#?}");
    let invalid = format!("groups/{bad}");
    assert!(
        recorded(&records, &invalid, "valid Redis key segment"),
        "{records:#?}"
    );
    let manifest = Manifest::read(&out).await.unwrap().expect("no manifest");
    assert!(manifest.pruned.is_none());
    // put everything back as it was
    srem(harness, &format!("{ns}:groups"), &bad).await;
    redis_run(
        harness,
        redis::cmd("DEL").arg(format!("{ns}:groups_data:{bad}")),
    )
    .await;
    hset(harness, &tokens, &token_name, &original_token).await;
    hset(harness, &pipe_key, "order", &original_order).await;
    hset(harness, &image_key, "group", &original_group).await;
    srem(harness, &owners, &ghost).await;
}
