//! The users example against real Redis and Scylla: the layout 1 fixture step, the verification
//! through the API's settings reader, and the example's edge on the test-only fixture catalog

mod fixture_catalog;
mod users_example;

use fixture_catalog::{R1, R2, USERS_BASE, USERS_THEME};
use thorium::models::schema::{users_theme_commands, verify_user};
use thorium::models::upgrades::{
    Catalog, CatalogEntry, IssueCategory, IssueReason, RecordClass, StepState, USERS_SETTINGS_THEME,
};
use thorium::test_utilities::upgrade_harness::{
    BackupRef, Check, FixtureSeed, FixtureVerifier, HistoryExpectation, Layout, Scenario,
};
use thorium::test_utilities::{CONF, redis_pool};
use users_example::{FAMILY, fixture, read, remove, seed_layout_1};

/// Held by every test that touches Redis, since the scenario's first start wipes the databases
/// and would remove the users another test just wrote
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The backup reference every approval here names
fn snap() -> BackupRef {
    // a reference to a backup
    BackupRef::new("snap-1")
}

/// A history entry of a step approved with the backup
///
/// # Arguments
///
/// * `step` - The step id
fn gated(step: &str) -> HistoryExpectation {
    // approved with the one backup
    HistoryExpectation::new(step, StepState::Done).with_backup("snap-1")
}

/// The fixture step writes users the migration classifies as migratable, each a complete user
#[tokio::test]
async fn seed_layout_1_writes_users_the_migration_classifies_migratable() {
    // no other test touches Redis meanwhile
    let _serial = SERIAL.lock().await;
    let pool = redis_pool().await;
    let namespace = CONF.thorium.namespace.clone();
    let names = seed_layout_1(&pool, &namespace, "ex07", 7).await;
    assert_eq!(names.len(), 7);
    for name in &names {
        let user = read(&pool, &namespace, name).await;
        // every user is complete, holds its theme flat, and classifies as migratable
        for field in ["username", "email", "role", "token", "theme"] {
            assert!(user.has_field(field), "{name} lacks {field}");
        }
        assert_eq!(
            (USERS_SETTINGS_THEME.classify)(&user),
            RecordClass::Migratable { layout: 1 },
            "{name}"
        );
    }
    remove(&pool, &namespace, &names).await;
}

/// Verification loads every user through the API's settings reader, and fails for a user the API
/// can't load, whether its settings are unreadable or its flat theme was never moved
#[tokio::test]
async fn verify_loads_every_user_and_fails_for_one_the_api_cannot_load() {
    // no other test touches Redis meanwhile
    let _serial = SERIAL.lock().await;
    let pool = redis_pool().await;
    let namespace = CONF.thorium.namespace.clone();
    let names = seed_layout_1(&pool, &namespace, "ex08", 6).await;
    // convert every user and store it the way the migration does
    let mut conn = pool.get().await.expect("a connection");
    for name in &names {
        let user = read(&pool, &namespace, name).await;
        let converted = (USERS_SETTINGS_THEME.up)(user).expect("converts");
        let key = format!("{namespace}:user_data:{name}");
        users_theme_commands(&converted, &key)
            .query_async::<()>(&mut *conn)
            .await
            .expect("written");
    }
    // every converted user loads
    for name in &names {
        let user = read(&pool, &namespace, name).await;
        assert_eq!(verify_user(&user), Ok(()), "{name}");
    }
    // a user whose settings no longer parse doesn't
    let _: () = redis::cmd("HSET")
        .arg(format!("{namespace}:user_data:{}", names[0]))
        .arg("settings")
        .arg("{\"theme\":")
        .query_async(&mut *conn)
        .await
        .expect("corrupted");
    let broken = read(&pool, &namespace, &names[0]).await;
    assert_eq!(
        verify_user(&broken),
        Err(IssueReason::new(IssueCategory::MalformedJson, "settings"))
    );
    // a user that still holds the flat theme doesn't either
    let _: () = redis::cmd("HSET")
        .arg(format!("{namespace}:user_data:{}", names[1]))
        .arg("theme")
        .arg("\"Dark\"")
        .query_async(&mut *conn)
        .await
        .expect("unconverted");
    let flat = read(&pool, &namespace, &names[1]).await;
    assert!(verify_user(&flat).is_err());
    drop(conn);
    remove(&pool, &namespace, &names).await;
}

/// The fixture catalog's `R1 -> R2` edge passes the catalog checks, runs the production transform,
/// and ships the reverse that runs its `down`
#[test]
fn the_fixture_edge_ships_the_production_transform_and_its_reverse() {
    // the catalog passes the same checks as the production one
    if let Err(problems) = USERS_THEME.check() {
        panic!("the users catalog has problems: {problems:#?}");
    }
    // the edge from the pre-release to the release holds the step with the production transform
    let forward: &CatalogEntry = USERS_THEME.edge(R1, R2).expect("the R1 -> R2 edge");
    assert!(forward.prerelease);
    let step = forward
        .steps
        .iter()
        .find(|step| step.id == "users-settings-theme")
        .expect("the step");
    let transform = step.transform.expect("a transform");
    assert_eq!((transform.from, transform.to), (1, 2));
    assert_eq!(transform.optional, USERS_SETTINGS_THEME.optional);
    // a layout 1 user is migratable under it, which the stand-in transform didn't say
    let user = users_example::layout_1_fields("u", 1);
    let record = thorium::models::upgrades::user_hash(
        "u",
        &user
            .iter()
            .map(|(field, value)| (field.as_str(), value.as_str()))
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        (transform.classify)(&record),
        RecordClass::Migratable { layout: 1 }
    );
    // the edge back holds the step that runs the transform's `down`
    let back = USERS_THEME.edge(R2, R1).expect("the R2 -> R1 edge");
    assert!(back.prerelease && back.is_downward());
    let reverse = back
        .steps
        .iter()
        .find(|step| step.reverses == Some("users-settings-theme"))
        .expect("the reverse step");
    assert!(reverse.transform.is_none());
    assert!(transform.down.is_some());
    // the pre-release exists only in the fixture catalog: the production catalog has no edge to
    // it, and its release is the one whose edge holds the same step
    let production = Catalog::PRODUCTION;
    assert!(!production.knows(&R1.parse().expect("a revision id")));
    let (release, _) = production.step("users-settings-theme").expect("the step");
    assert_eq!((release.from, release.id), (Some(USERS_BASE), R2));
}

/// The users example's `R1 -> R2` edge moves the flat theme into the settings, and its revert
/// edge runs the step's `down`, including for users created after the upgrade
#[tokio::test]
async fn the_users_example_edge_converts_and_reverses_users() {
    // the scenario wipes the databases when it starts, so no other test runs meanwhile
    let _serial = SERIAL.lock().await;
    Scenario::new("users-example")
        .catalog(USERS_THEME)
        .family(FAMILY, fixture())
        .seed(FixtureSeed {
            family: FAMILY,
            layout: Layout(1),
            count: 6,
            variants: None,
        })
        .target(R2)
        .expect_blocked("users-settings-theme")
        .check(Check::Fixtures(FixtureVerifier::items_in(
            FAMILY,
            Layout(1),
        )))
        .approve("users-settings-theme", snap())
        .expect_revision(R2)
        .check(Check::Fixtures(FixtureVerifier::items_in(
            FAMILY,
            Layout(2),
        )))
        .seed(FixtureSeed {
            family: FAMILY,
            layout: Layout(2),
            count: 2,
            variants: None,
        })
        .target(R1)
        .approve("users-settings-theme-reverse", snap())
        .expect_revision(R1)
        .check(Check::Fixtures(FixtureVerifier::items_in(
            FAMILY,
            Layout(1),
        )))
        .check(Check::History(vec![
            gated("users-settings-theme"),
            gated("users-settings-theme-reverse"),
        ]))
        .run()
        .await
        .expect("the scenario holds");
}
