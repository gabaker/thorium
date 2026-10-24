//! Tests of the namespace the schema and migrate tests default to

#[path = "common/base_namespace.rs"]
mod base_namespace;

use base_namespace::{choose, generated};

/// A namespace named in the environment is used as given, in CI or not
#[test]
fn a_named_namespace_is_used() {
    assert_eq!(
        choose(
            Some("testing_ci_0123456_deadbeef"),
            false,
            || unreachable!()
        ),
        Ok("testing_ci_0123456_deadbeef".to_owned())
    );
    assert_eq!(
        choose(Some("testing_ci_0123456_deadbeef"), true, || unreachable!()),
        Ok("testing_ci_0123456_deadbeef".to_owned())
    );
}

/// A local run without a namespace, or with an empty one, makes one up
#[test]
fn a_local_run_without_a_namespace_makes_one_up() {
    assert_eq!(
        choose(None, false, || "testing_x".to_owned()),
        Ok("testing_x".to_owned())
    );
    assert_eq!(
        choose(Some(""), false, || "testing_y".to_owned()),
        Ok("testing_y".to_owned())
    );
}

/// A CI run without a namespace fails and names the variable
#[test]
fn a_ci_run_without_a_namespace_fails() {
    for configured in [None, Some("")] {
        let error = choose(configured, true, || unreachable!()).expect_err("CI needs one");
        assert!(error.contains("THORIUM__THORIUM__NAMESPACE"), "{error}");
    }
}

/// A namespace that is not a test namespace is refused, so a test never drops a real keyspace
#[test]
fn a_namespace_outside_the_test_prefix_is_refused() {
    for name in ["testing", "thorium", "prod_cli"] {
        let error = choose(Some(name), false, || unreachable!()).expect_err("refused");
        assert!(error.contains("testing_"), "{error}");
    }
}

/// A made-up namespace has the stamp and random digits in the shape the pipeline uses
#[test]
fn a_generated_namespace_has_the_ci_shape() {
    assert_eq!(
        generated(0x0012_3456, 0xdead_beef),
        "testing_ci_0123456_deadbeef"
    );
    assert_eq!(generated(1, 2), "testing_ci_0000001_00000002");
}
