//! A restore of another deployment's backup keeps the target's operator users, with their passwords
//! and tokens, and the target's nodes, while every other user and all data come from the backup;
//! `backup verify --cluster` counts what was kept as kept by design rather than as differences
//!
//! "Deployment A" is the namespace seeded with the clean fixture, operator users with A's passwords,
//! and a node of A's cluster; it is backed up, emptied, and becomes "deployment B" with operator
//! users of B's passwords and a node of B's cluster. The test restores, so it holds the binary's
//! restore guard.

mod common;

use common::cycle::{describe, wipe};
use common::fixture;
use common::harness::Harness;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use thorium::Thorium;
use thorium::client::{ClientSettings, Users};
use thorium::models::{AuthResponse, Resources, UserCreate};
use thorium::test_utilities::generators;

/// The users the Thorium operator provisions and logs in as
const OPERATOR_USERS: [&str; 3] = ["thorium-operator", "thorium", "thorium-kaboom"];

/// The operator users the test creates in each deployment with that deployment's password
const CREATED_USERS: [&str; 2] = ["thorium-operator", "thorium-kaboom"];

/// The password deployment A gives its operator users
const A_PASSWORD: &str = "deployment-a-password";

/// The password deployment B gives its operator users
const B_PASSWORD: &str = "deployment-b-password";

/// The password of the person deployment A holds
const PERSON_PASSWORD: &str = "person-password";

/// Empties the target and initializes the system again, as a fresh install has it
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn empty_target(harness: &Harness) {
    // wipe everything but the admin's login, then put back what a fresh install has
    wipe(harness).await;
    harness
        .client()
        .await
        .system
        .init()
        .await
        .expect("failed to initialize the system");
}

/// Gives a deployment the operator users the harness doesn't hold with a password and registers a
/// node of its cluster, returning the token `thorium-operator` was given
///
/// The harness admin is the operator's `thorium` user itself, and a password change would rotate
/// the token the harness works with, so it keeps the harness's credentials in both deployments.
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `password` - The password every operator user gets
/// * `cluster` - The cluster the deployment's node is in
async fn deployment(harness: &Harness, password: &str, cluster: &str) -> String {
    // the operator's own users, created with the secret key as the operator creates them
    let client = harness.client().await;
    let settings = ClientSettings::default();
    let mut token = String::new();
    for name in CREATED_USERS {
        let created = Thorium::bootstrap(
            &harness.api_url,
            name,
            password,
            &format!("{name}@example.invalid"),
            &harness.conf.thorium.secret_key,
            &settings,
        )
        .await
        .unwrap_or_else(|error| panic!("failed to create {name}: {error}"));
        if let (AuthResponse::Authed { token: given, .. }, "thorium-operator") = (created, name) {
            token = given;
        }
    }
    // a node of the deployment's own cluster
    generators::node(
        cluster,
        &format!("{cluster}-node"),
        Resources::default(),
        &client,
    )
    .await
    .expect("failed to register a node");
    token
}

/// Whether a user logs in with a password
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `username` - The user
/// * `password` - The password to try
async fn logs_in(harness: &Harness, username: &str, password: &str) -> bool {
    // basic auth as the operator does it
    let client = reqwest::Client::new();
    Users::auth_basic(&harness.api_url, username, password, &client)
        .await
        .is_ok()
}

/// Returns the clusters of the namespace's nodes
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn node_clusters(harness: &Harness) -> BTreeSet<String> {
    // read the nodes table directly
    let rows = harness
        .scylla()
        .await
        .query_unpaged(
            format!(
                "SELECT cluster FROM {}.nodes",
                harness.conf.thorium.namespace
            ),
            &[],
        )
        .await
        .unwrap()
        .into_rows_result()
        .unwrap();
    rows.rows::<(String,)>()
        .unwrap()
        .map(|row| row.unwrap().0)
        .collect()
}

/// Returns every username the API lists that isn't an operator user
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
async fn other_users(harness: &Harness) -> BTreeSet<String> {
    // every user but the operator's
    harness
        .client()
        .await
        .users
        .list()
        .await
        .unwrap()
        .into_iter()
        .filter(|name| !OPERATOR_USERS.contains(&name.as_str()))
        .collect()
}

/// Restores a backup without a terminal, with its report in a fresh directory beside the backup
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup to restore
/// * `extra` - Extra `backup restore` arguments
fn restore(harness: &Harness, backup: &Path, extra: &[&str]) -> (Output, PathBuf) {
    // confirm with --confirm
    let report = backup
        .parent()
        .unwrap()
        .join(format!("report-{}", uuid::Uuid::new_v4()));
    let output = harness
        .thoradm()
        .args(["--workers", "2", "backup", "restore", "--confirm", "-b"])
        .arg(backup)
        .args(extra)
        .arg("--report-dir")
        .arg(&report)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run thoradm");
    (output, report)
}

/// Runs `backup verify --cluster` and returns its output and report
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `backup` - The backup to check the cluster against
fn verify_cluster(harness: &Harness, backup: &Path) -> (Output, Value) {
    // a fresh report directory beside the backup
    let report = backup
        .parent()
        .unwrap()
        .join(format!("verify-{}", uuid::Uuid::new_v4()));
    let output = harness
        .thoradm()
        .args(["--workers", "2", "backup", "verify", "--cluster", "-b"])
        .arg(backup)
        .arg("--report-dir")
        .arg(&report)
        .output()
        .expect("failed to run thoradm");
    let verify = std::fs::read(report.join("verify.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    (output, verify)
}

/// Checks deployment B after a restore of A's backup: B's operator users log in with B's
/// passwords and B's token, the harness admin `thorium` keeps working, B's node is the only one, A's other users and data are there, the
/// summary names what was kept, and `verify --cluster` passes, counting it as kept by design
///
/// # Arguments
///
/// * `harness` - The harness naming the namespace
/// * `mode` - The restore mode, for messages
/// * `run` - The restore's output and report directory
/// * `backup` - The backup restored
/// * `b_token` - The token B gave `thorium-operator`
/// * `a_users` - A's users other than the operator's
/// * `sample` - A sample of A's fixture
async fn check_b(
    harness: &Harness,
    mode: &str,
    run: &(Output, PathBuf),
    backup: &Path,
    b_token: &str,
    a_users: &BTreeSet<String>,
    sample: &str,
) {
    // the restore finished
    let (output, report) = run;
    assert_eq!(
        output.status.code(),
        Some(0),
        "{mode}: {}",
        describe(output)
    );
    // B's operator users log in with B's passwords and not A's
    for name in CREATED_USERS {
        assert!(
            logs_in(harness, name, B_PASSWORD).await,
            "{mode}: {name} lost B's password"
        );
        assert!(
            !logs_in(harness, name, A_PASSWORD).await,
            "{mode}: {name} took A's password"
        );
    }
    // the token B's operator holds still works
    let operator = Thorium::build(harness.api_url.clone())
        .token(b_token)
        .build()
        .await
        .expect("failed to build the operator's client");
    let me = operator.users.info().await;
    assert!(
        me.is_ok(),
        "{mode}: B's operator token stopped working: {me:?}"
    );
    // B's node is the only one
    assert_eq!(
        node_clusters(harness).await,
        BTreeSet::from(["cluster-b".to_owned()]),
        "{mode}"
    );
    // A's other users and data came back
    let found = other_users(harness).await;
    assert!(
        found.is_superset(a_users),
        "{mode}: {a_users:?} not all in {found:?}"
    );
    for person in a_users {
        assert!(
            logs_in(harness, person, PERSON_PASSWORD).await,
            "{mode}: {person} can't log in"
        );
    }
    let client = harness.client().await;
    assert!(
        client.files.get(sample).await.is_ok(),
        "{mode}: A's sample is missing"
    );
    // the summary says what was kept
    let summary: Value =
        serde_json::from_slice(&std::fs::read(report.join("summary.json")).unwrap()).unwrap();
    let kept: Vec<&str> = summary["kept"]
        .as_array()
        .unwrap_or_else(|| panic!("{mode}: no kept list in {summary:#}"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        kept,
        [
            "user thorium-operator",
            "user thorium",
            "user thorium-kaboom",
            "table nodes"
        ],
        "{mode}"
    );
    // verify passes, with the kept users and nodes counted as kept by design
    let (verified, verify) = verify_cluster(harness, backup);
    assert_eq!(
        verified.status.code(),
        Some(0),
        "{mode}: {}",
        describe(&verified)
    );
    let kept_checks = verify["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|check| check["check"] == "kept-by-design")
        .map(|check| check["component"].as_str().unwrap_or_default().to_owned())
        .collect::<BTreeSet<String>>();
    assert_eq!(
        kept_checks,
        BTreeSet::from(["nodes".to_owned(), "redis".to_owned()]),
        "{mode}: {verify:#}"
    );
}

/// A backup of deployment A restored into deployment B, plainly, with `--merge`, and with
/// `--wipe`, leaves B's operator users and nodes and brings A's other users and data
#[tokio::test]
async fn a_restore_keeps_the_targets_operator_users_and_nodes() {
    // deployment A: the clean fixture, operator users with A's password, and A's node
    let harness = Harness::get().await;
    let _guard = harness.restore_guard().await;
    empty_target(harness).await;
    let seeded = fixture::clean(harness).await;
    deployment(harness, A_PASSWORD, "cluster-a").await;
    // and a person of its own, whose password comes back with the restore
    let person = format!("alice-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let request = UserCreate::new(person.as_str(), PERSON_PASSWORD, "alice@example.invalid")
        .skip_verification();
    let key = Some(harness.conf.thorium.secret_key.as_str());
    Users::create(&harness.api_url, request, key, &ClientSettings::default())
        .await
        .expect("failed to create a person");
    let a_users = other_users(harness).await;
    assert!(a_users.contains(&person), "{a_users:?}");
    // a full backup of A
    let backup = harness.scratch("keeps-operator").join("backup");
    let taken = harness
        .thoradm()
        .args(["--workers", "2", "backup", "new", "-o"])
        .arg(&backup)
        .output()
        .expect("failed to run thoradm");
    assert_eq!(taken.status.code(), Some(0), "{}", describe(&taken));
    // deployment B: emptied, with B's passwords and node
    empty_target(harness).await;
    let b_token = deployment(harness, B_PASSWORD, "cluster-b").await;
    // a plain restore: B holds only its users and nodes, which a restore keeps
    let run = restore(harness, &backup, &[]);
    check_b(
        harness,
        "plain",
        &run,
        &backup,
        &b_token,
        &a_users,
        &seeded.sample,
    )
    .await;
    // a merge into what that left
    let run = restore(harness, &backup, &["--merge"]);
    check_b(
        harness,
        "merge",
        &run,
        &backup,
        &b_token,
        &a_users,
        &seeded.sample,
    )
    .await;
    // a wipe of what that left
    let ns = harness.conf.thorium.namespace.clone();
    let run = restore(harness, &backup, &["--wipe", &ns]);
    check_b(
        harness,
        "wipe",
        &run,
        &backup,
        &b_token,
        &a_users,
        &seeded.sample,
    )
    .await;
}
