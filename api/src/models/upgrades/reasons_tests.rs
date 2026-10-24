//! Tests of the reason codes `ReasonBuilder` builds

use serde::Deserialize;
use std::collections::BTreeMap;

use super::{IssueCategory, ReasonBuilder};

/// A theme setting, as one layout defines it
#[derive(Debug, Deserialize)]
enum Theme {
    /// The dark theme
    Dark,
    /// The light theme
    Light,
}

/// A user's settings, as one layout defines them
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Settings {
    /// The theme
    theme: Theme,
}

/// An alias, as one layout defines it
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Alias {
    /// The alias's name
    name: String,
}

/// A tag, as one layout defines it
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Tag {
    /// The tag's key
    k: String,
}

/// A user, as one layout defines it
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct User {
    /// The user's name
    name: String,
    /// The user's level
    level: u8,
    /// The user's settings
    settings: Settings,
    /// The user's aliases, by a key that can be a secret
    #[serde(default)]
    aliases: BTreeMap<String, Alias>,
    /// The user's tags
    #[serde(default)]
    tags: Vec<Tag>,
}

/// Parse a user and return the reason code the parse failed with
///
/// # Arguments
///
/// * `raw` - The stored text
fn code(raw: &str) -> String {
    // the text must fail to parse, and the code is what the builder wrote
    ReasonBuilder::parse::<User>(raw, "")
        .expect_err("the text parsed")
        .to_string()
}

/// A value seeded into the failing field never appears in the code, whatever the failure's
/// category
#[test]
fn a_seeded_value_never_reaches_a_code() {
    // a token where a number belongs, an unknown variant's name, and a secret map key
    let cases = [
        (
            r#"{"name":"a","level":"tok3n-secret","settings":{"theme":"Dark"}}"#,
            "bad_type:level",
        ),
        (
            r#"{"name":"a","level":1,"settings":{"theme":"tok3n-secret"}}"#,
            "unknown_variant:settings.theme",
        ),
        (
            r#"{"name":"a","level":1,"settings":{"theme":"Dark"},"aliases":{"tok3n-secret":{"name":5}}}"#,
            "bad_type:aliases.*.name",
        ),
    ];
    for (raw, expected) in cases {
        let code = code(raw);
        assert_eq!(code, expected, "{raw}");
        assert!(!code.contains("tok3n"), "{code}");
    }
}

/// Struct fields keep their names in a path, while map keys and sequence indices render as `*`
#[test]
fn map_keys_and_indices_render_as_stars() {
    // a bad type under a map value, then under a sequence element
    assert_eq!(
        code(r#"{"name":"a","level":1,"settings":{"theme":"Dark"},"aliases":{"x":{"name":1}}}"#),
        "bad_type:aliases.*.name"
    );
    assert_eq!(
        code(r#"{"name":"a","level":1,"settings":{"theme":"Dark"},"tags":[{"k":"a"},{"k":2}]}"#),
        "bad_type:tags.*.k"
    );
}

/// A missing field's code names the field, not its parent
#[test]
fn a_missing_field_is_named() {
    // the whole settings object, then one field inside it
    assert_eq!(code(r#"{"name":"a","level":1}"#), "missing_field:settings");
    assert_eq!(
        code(r#"{"name":"a","level":1,"settings":{}}"#),
        "missing_field:settings.theme"
    );
}

/// A syntax error is `malformed_json` with the field the text was stored in, or alone for a
/// whole record
#[test]
fn malformed_json_names_its_field() {
    // a text field that doesn't parse, and a whole record that doesn't
    let field = ReasonBuilder::parse::<User>("{not json", "settings").expect_err("parsed");
    assert_eq!(field.to_string(), "malformed_json:settings");
    assert_eq!(code("{not json"), "malformed_json");
    assert_eq!(code(""), "malformed_json");
}

/// A number that overflows its type is an invalid value
#[test]
fn an_overflow_is_an_invalid_value() {
    // 300 doesn't fit a u8
    let reason = ReasonBuilder::parse::<User>(
        r#"{"name":"a","level":300,"settings":{"theme":"Dark"}}"#,
        "",
    )
    .expect_err("parsed");
    assert_eq!(reason.category, IssueCategory::InvalidValue);
    assert_eq!(reason.to_string(), "invalid_value:level");
}

/// A record that parses gives its value back
#[test]
fn a_valid_record_parses() {
    // every field present and well typed
    let user = ReasonBuilder::parse::<User>(
        r#"{"name":"a","level":1,"settings":{"theme":"Light"},"aliases":{"k":{"name":"n"}},"tags":[{"k":"a"}]}"#,
        "",
    )
    .expect("parses");
    assert_eq!(user.name, "a");
    assert_eq!(user.aliases.len(), 1);
}

/// An unknown field is found by checking the record's keys, and its code names the parent path,
/// never the unknown key
#[test]
fn an_unknown_field_names_its_parent() {
    // a key the layout doesn't declare, at the root and inside `settings`
    let root =
        ReasonBuilder::check_unknown(["name", "tok3n-secret"], &["name"], "").expect_err("unknown");
    assert_eq!(root.to_string(), "unknown_field");
    let nested = ReasonBuilder::check_unknown(["theme", "tok3n-secret"], &["theme"], "settings")
        .expect_err("unknown");
    assert_eq!(nested.to_string(), "unknown_field:settings");
    assert!(ReasonBuilder::check_unknown(["name"], &["name"], "").is_ok());
}

/// A rejected write's code is `write_failed:rejected`, and any other refusal is `other`
#[test]
fn a_refused_write_names_its_kind() {
    // the field slot holds the kind of the write error
    assert_eq!(
        ReasonBuilder::write_failed(true).to_string(),
        "write_failed:rejected"
    );
    assert_eq!(
        ReasonBuilder::write_failed(false).to_string(),
        "write_failed:other"
    );
}
