//! Tests that the users migration's verification agrees with the API's own loading of a user

use std::collections::HashMap;

use super::users::cast;
use crate::models::schema::verify_user;
use crate::models::upgrades::Record;

/// A stored user's settings fields, and whether the API loads the user
type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, bool);

/// A user's hash with the fields every stored user has and the settings fields under test
///
/// # Arguments
///
/// * `extra` - The `theme` and `settings` fields to store
fn stored(extra: &[(&str, &str)]) -> HashMap<String, String> {
    // the fields every stored user has
    let mut raw = HashMap::from([
        ("username", "mcarson"),
        ("email", "mcarson@example.com"),
        ("role", "\"User\""),
        ("token", "token"),
        ("token_expiration", "\"2030-01-01T00:00:00Z\""),
    ])
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect::<HashMap<_, _>>();
    // add the settings fields under test
    raw.extend(
        extra
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
    );
    raw
}

/// The verification accepts exactly the users the API loads, and rejects the ones it can't
#[test]
fn verify_agrees_with_the_apis_cast() {
    let ai = r#"{"ai":{"endpoints":{},"default_endpoint":"x"}}"#;
    let cases: Vec<Case> = vec![
        (
            "settings with a theme",
            vec![("settings", r#"{"theme":"Light"}"#)],
            true,
        ),
        (
            "settings with ai",
            vec![(
                "settings",
                r#"{"theme":"Crab","ai":{"endpoints":{},"default_endpoint":"x"}}"#,
            )],
            true,
        ),
        ("no settings", vec![], true),
        (
            "malformed settings",
            vec![("settings", "{\"theme\":")],
            false,
        ),
        ("settings without a theme", vec![("settings", ai)], false),
        (
            "a theme of the wrong type",
            vec![("settings", r#"{"theme":7}"#)],
            false,
        ),
        (
            "an unknown theme",
            vec![("settings", r#"{"theme":"Neon"}"#)],
            false,
        ),
    ];
    for (name, extra, loads) in cases {
        // the API's own loading of the hash
        let raw = stored(&extra);
        assert_eq!(cast(raw.clone(), Vec::new()).is_ok(), loads, "cast: {name}");
        // the verification of the same hash, read as a record
        let record = Record::Redis {
            key: "user_data:mcarson".to_owned(),
            fields: raw.into_iter().collect(),
        };
        assert_eq!(verify_user(&record).is_ok(), loads, "verify: {name}");
    }
}
