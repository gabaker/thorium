//! Tests of how the integration test harness picks its config and what it does to the backends

use super::api::{
    BootstrapAction, NamespaceChoice, TEST_NAMESPACE_PREFIX, bootstrap_plan, choose_namespace,
    generated_namespace, test_config_path,
};
use crate::Conf;

/// The template config the harness falls back to
const TEMPLATE: &str = "../api/tests/thorium-testing.yml";

/// The harness loads the file `THORIUM_TEST_CONFIG` names, and the template when it is unset or
/// empty
#[test]
fn the_override_names_the_config_file() {
    // an unset or empty variable falls back to the template
    assert_eq!(test_config_path(None), TEMPLATE);
    assert_eq!(test_config_path(Some("")), TEMPLATE);
    // a set one wins
    assert_eq!(
        test_config_path(Some("/tmp/dev-thorium.yml")),
        "/tmp/dev-thorium.yml"
    );
}

/// A config loaded from the override has the file's values, and environment variables still win
/// over them
#[test]
fn the_override_file_loads_with_environment_overrides_on_top() {
    // write a config that differs from the template in a value no run overrides
    let template = std::fs::read_to_string(TEMPLATE).expect("the template is readable");
    let changed = template.replace("replication: 1", "replication: 3");
    assert_ne!(template, changed, "the template has no replication line");
    let dir = std::env::temp_dir().join(format!("harness-config-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    let path = dir.join("dev-thorium.yml");
    std::fs::write(&path, changed).expect("the config is written");
    let path = path.to_str().expect("a utf-8 path").to_owned();
    // the override file's value is read, and the template's isn't
    let from_override = Conf::new(test_config_path(Some(&path))).expect("the override loads");
    let from_template = Conf::new(test_config_path(None)).expect("the template loads");
    assert_eq!(from_override.scylla.replication, 3);
    assert_ne!(from_template.scylla.replication, 3);
    // an environment variable replaces the override file's value, since overrides apply on top
    // SAFETY: the variable is read by this test only, and set before the load that reads it
    unsafe { std::env::set_var("THORIUM__SCYLLA__REPLICATION", "5") };
    let layered = Conf::new(test_config_path(Some(&path)));
    // SAFETY: as above
    unsafe { std::env::remove_var("THORIUM__SCYLLA__REPLICATION") };
    std::fs::remove_dir_all(&dir).expect("the temporary directory is removed");
    assert_eq!(
        layered
            .expect("the layered config loads")
            .scylla
            .replication,
        5
    );
}

/// With the override set the bootstrap does nothing to any backend; without it the bootstrap wipes,
/// creates the buckets, and applies the schema to the emptied keyspace, in that order
#[test]
fn a_read_only_bootstrap_touches_nothing() {
    assert_eq!(bootstrap_plan(true), vec![]);
    assert_eq!(
        bootstrap_plan(false),
        vec![
            BootstrapAction::WipeDatabases,
            BootstrapAction::InitBuckets,
            BootstrapAction::ApplySchema
        ]
    );
}

/// A namespace named in the environment is kept whether or not the run is in CI
#[test]
fn a_named_namespace_is_kept() {
    assert_eq!(
        choose_namespace(true, false, false),
        Ok(NamespaceChoice::Keep)
    );
    assert_eq!(
        choose_namespace(true, true, false),
        Ok(NamespaceChoice::Keep)
    );
}

/// A read-only harness keeps the namespace its config names, even when none is in the environment
#[test]
fn a_read_only_harness_keeps_its_configs_namespace() {
    assert_eq!(
        choose_namespace(false, false, true),
        Ok(NamespaceChoice::Keep)
    );
    assert_eq!(
        choose_namespace(false, true, true),
        Ok(NamespaceChoice::Keep)
    );
}

/// A local run without a namespace makes one up, so it never shares the template's name
#[test]
fn a_local_run_without_a_namespace_generates_one() {
    assert_eq!(
        choose_namespace(false, false, false),
        Ok(NamespaceChoice::Generate)
    );
}

/// A CI run without a namespace fails and names the variable to set
#[test]
fn a_ci_run_without_a_namespace_fails() {
    let error = choose_namespace(false, true, false).expect_err("CI needs a namespace");
    assert!(error.contains("THORIUM__THORIUM__NAMESPACE"), "{error}");
}

/// A generated namespace has the stamp and random digits in the documented shape, is a test
/// namespace, and passes the rule a loaded config enforces
#[test]
fn a_generated_namespace_has_the_ci_shape() {
    let name = generated_namespace(0x0012_3456, 0xdead_beef);
    assert_eq!(name, "testing_ci_0123456_deadbeef");
    assert_eq!(name.len(), 27);
    assert!(name.starts_with(TEST_NAMESPACE_PREFIX));
    crate::conf::validate_namespace(&name).expect("the namespace is valid");
    // a small random value is padded to eight digits, and a stamp to seven
    assert_eq!(generated_namespace(1, 2), "testing_ci_0000001_00000002");
}
