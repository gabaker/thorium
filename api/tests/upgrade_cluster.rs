//! The upgrade scenario harness's cluster driver on a minithor dev cluster
//!
//! The scenario here is the example migration run through the operator: users written in layout
//! 1 under the previous revision's image, then the next image deployed, so the plan blocks at
//! the example's data step, is approved, and ends `Done`. It runs by hand on a dev VM, never in
//! CI, since no runner can run minithor.

use thorium::models::upgrades::{Family, Record, StepState, latest};
use thorium::test_utilities::upgrade_harness::{
    BackupRef, Check, ClusterDriver, FamilyFixture, FixtureSeed, FixtureVerifier,
    HistoryExpectation, Layout, Scenario, Variation,
};

mod users_example;

/// The family the example's users live in: `{namespace}:user_data:{username}`
const USERS: Family = Family::Redis("user_data");

/// Build a complete user in layout 1 (a flat `theme`) or layout 2 (`theme` inside `settings`)
fn user(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // the same fields a deployment holds, so the migration's verify can load the user
    let name = users_example::username("harness", index);
    let fields = if layout.0 == 1 {
        users_example::layout_1_fields(&name, index)
    } else {
        users_example::layout_2_fields(&name, index)
    };
    Record::Redis {
        key: format!("user_data:{name}"),
        fields: fields.into_iter().collect(),
    }
}

/// Find the layout a user is in
fn layout_of(record: &Record) -> Option<Layout> {
    // a flat `theme` is layout 1 and `settings` layout 2
    let Record::Redis { fields, .. } = record else {
        return None;
    };
    if fields.contains_key("settings") && !fields.contains_key("theme") {
        Some(Layout(2))
    } else if fields.contains_key("theme") {
        Some(Layout(1))
    } else {
        None
    }
}

/// Deploy the previous image, write layout 1 users, deploy the next image, approve the data step,
/// and check the users between the transitions
#[tokio::test]
#[ignore = "needs a minithor dev cluster and two local images; run by hand on a dev VM: \
THORIUM_TEST_CONFIG=<the cluster's thorium.yml, with its Scylla, Redis, and S3 reachable> \
THORIUM_TEST_PREVIOUS_IMAGE=<previous release image> THORIUM_TEST_NEXT_IMAGE=<next image> \
THORIUM_TEST_NAMESPACE=thorium THORIUM_TEST_CLUSTER=thorium \
[THORIUM_TEST_DEPLOY_COMMAND=<a deploy command with {image}>] \
cargo test -p thorium-api --features test-utilities --test upgrade_cluster -- --ignored"]
async fn t_tc_20_the_cluster_driver_runs_the_example_migration() {
    // the images and the cluster come from the environment
    let var = |name: &str, default: Option<&str>| {
        std::env::var(name)
            .ok()
            .or_else(|| default.map(str::to_owned))
            .unwrap_or_else(|| panic!("{name} must be set"))
    };
    let previous = var("THORIUM_TEST_PREVIOUS_IMAGE", None);
    let next = var("THORIUM_TEST_NEXT_IMAGE", None);
    let namespace = var("THORIUM_TEST_NAMESPACE", Some("thorium"));
    let cluster = var("THORIUM_TEST_CLUSTER", Some("thorium"));
    let target = latest().to_string();
    let step = "users-settings-theme";
    // the previous image writes layout 1, the next image blocks at the step until it is approved
    let scenario = Scenario::new("example-migration-on-a-cluster")
        .family(
            USERS,
            FamilyFixture {
                generate: user,
                layout_of,
                corrupt: None,
            },
        )
        .deploy(previous)
        .seed(FixtureSeed {
            family: USERS,
            layout: Layout(1),
            count: 20,
            variants: None,
        })
        .check(Check::Fixtures(FixtureVerifier::items_in(USERS, Layout(1))))
        .deploy(next)
        .target(target.clone())
        .expect_blocked(step)
        .check(Check::Fixtures(FixtureVerifier::items_in(USERS, Layout(1))))
        .approve(step, BackupRef::new("harness-snap"))
        .expect_revision(target)
        .check(Check::Fixtures(FixtureVerifier::items_in(USERS, Layout(2))))
        .check(Check::History(vec![
            HistoryExpectation::new(step, StepState::Done).with_backup("harness-snap"),
        ]));
    let mut driver = ClusterDriver::connect(&namespace, &cluster)
        .await
        .expect("the cluster driver connects");
    // a deploy command from the environment replaces the default, which suits a cluster whose
    // chart and values the default `minithor deploy --local-image {image}` needs no more of
    if let Ok(command) = std::env::var("THORIUM_TEST_DEPLOY_COMMAND") {
        driver =
            driver.with_deploy_command(command.split_whitespace().map(str::to_owned).collect());
    }
    scenario
        .run_with(&mut driver)
        .await
        .expect("the example migration runs through the operator");
}
