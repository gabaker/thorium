//! Tests of the users example's transform: the classification of every layout and broken variant,
//! the conversion in both directions, idempotence, and the round trip of the reverse edge

use serde_json::{Value, json};

use super::users_theme::{USERS_SETTINGS_THEME, classify, down, up, user_hash};
use super::{FieldPath, IssueCategory, IssueReason, Record, RecordClass};

/// A user's AI settings in the form both layouts store them
const AI: &str = r#"{"endpoints":{},"default_endpoint":"x"}"#;

/// The reason code a broken variant is recorded under
///
/// # Arguments
///
/// * `category` - The kind of problem
/// * `field` - The field's path
fn unrecognized(category: IssueCategory, field: &str) -> RecordClass {
    // a reason built from the category and the path, never from the data
    RecordClass::Unrecognized {
        reason: IssueReason::new(category, field),
    }
}

/// Read a field of a user's hash
///
/// # Arguments
///
/// * `record` - The user
/// * `field` - The field
fn field(record: &Record, field: &str) -> Option<String> {
    // only a Redis hash has fields
    record.text(field)
}

/// Read the `settings` field of a user as JSON
///
/// # Arguments
///
/// * `record` - The user
fn settings(record: &Record) -> Option<Value> {
    // the field holds JSON text
    field(record, "settings").map(|raw| serde_json::from_str(&raw).expect("settings are JSON"))
}

/// Every layout and every broken variant classifies as the example says, with its reason code
#[test]
fn classification_covers_every_layout_and_broken_variant() {
    let ai_only = format!(r#"{{"ai":{AI}}}"#);
    let theme_and_ai = format!(r#"{{"theme":"Light","ai":{AI}}}"#);
    let cases: Vec<(&str, Record, RecordClass)> = vec![
        (
            "layout 1 without settings",
            user_hash("a", &[("theme", "\"Dark\"")]),
            RecordClass::Migratable { layout: 1 },
        ),
        (
            "layout 1 with ai settings",
            user_hash("a", &[("theme", "\"Ocean\""), ("settings", &ai_only)]),
            RecordClass::Migratable { layout: 1 },
        ),
        (
            "layout 2",
            user_hash("a", &[("settings", r#"{"theme":"Crab"}"#)]),
            RecordClass::Current,
        ),
        (
            "layout 2 with ai settings",
            user_hash("a", &[("settings", &theme_and_ai)]),
            RecordClass::Current,
        ),
        (
            "a user with no theme anywhere, which the API reads as the default",
            user_hash("a", &[("email", "a@example.com")]),
            RecordClass::Current,
        ),
        (
            "malformed settings JSON",
            user_hash("a", &[("settings", "{\"theme\":")]),
            unrecognized(IssueCategory::MalformedJson, "settings"),
        ),
        (
            "malformed settings JSON beside a flat theme",
            user_hash("a", &[("theme", "\"Dark\""), ("settings", "{\"ai\":")]),
            unrecognized(IssueCategory::MalformedJson, "settings"),
        ),
        (
            "an unknown flat theme",
            user_hash("a", &[("theme", "\"Neon\"")]),
            unrecognized(IssueCategory::UnknownVariant, "theme"),
        ),
        (
            "a flat theme that isn't JSON",
            user_hash("a", &[("theme", "Dark")]),
            unrecognized(IssueCategory::MalformedJson, "theme"),
        ),
        (
            "a settings theme of the wrong type",
            user_hash("a", &[("settings", r#"{"theme":7}"#)]),
            unrecognized(IssueCategory::BadType, "settings.theme"),
        ),
        (
            "settings holding ai but no theme and no flat theme",
            user_hash("a", &[("settings", &ai_only)]),
            unrecognized(IssueCategory::MissingField, "settings.theme"),
        ),
        (
            "both forms, which converts from the flat one",
            user_hash(
                "a",
                &[("theme", "\"Dark\""), ("settings", r#"{"theme":"Light"}"#)],
            ),
            RecordClass::Migratable { layout: 1 },
        ),
    ];
    for (name, record, expected) in cases {
        assert_eq!(classify(&record), expected, "{name}");
    }
}

/// Converting a layout 1 user moves the theme into `settings`, creates `settings` when it is
/// absent, keeps `ai` unchanged, removes the flat field, and leaves every other field alone
#[test]
fn converting_a_layout_1_user_moves_the_theme_into_settings() {
    let ai: Value = serde_json::from_str(AI).expect("ai json");
    let ai_only = format!(r#"{{"ai":{AI}}}"#);
    // no settings: the field is created with only the theme
    let bare = user_hash("a", &[("theme", "\"Dark\""), ("email", "a@example.com")]);
    let converted = up(bare).expect("converts");
    assert_eq!(field(&converted, "theme"), None);
    assert_eq!(settings(&converted), Some(json!({"theme": "Dark"})));
    assert_eq!(field(&converted, "email").as_deref(), Some("a@example.com"));
    // ai settings are kept beside the moved theme
    let with_ai = user_hash("a", &[("theme", "\"Ocean\""), ("settings", &ai_only)]);
    let converted = up(with_ai).expect("converts");
    assert_eq!(field(&converted, "theme"), None);
    assert_eq!(
        settings(&converted),
        Some(json!({"theme": "Ocean", "ai": ai}))
    );
    // a stale theme inside settings never wins over the flat field
    let both = user_hash(
        "a",
        &[("theme", "\"Light\""), ("settings", r#"{"theme":"Dark"}"#)],
    );
    let converted = up(both).expect("converts");
    assert_eq!(settings(&converted), Some(json!({"theme": "Light"})));
    // a user that can't convert is an issue with the classification's reason
    let broken = user_hash("a", &[("theme", "\"Neon\"")]);
    let issue = up(broken).expect_err("an unknown theme");
    assert_eq!(
        issue.reason,
        IssueReason::new(IssueCategory::UnknownVariant, "theme")
    );
}

/// Converting twice gives the same user, and a converted user classifies as current
#[test]
fn converting_twice_gives_the_same_user_and_it_classifies_current() {
    let ai_only = format!(r#"{{"ai":{AI}}}"#);
    for record in [
        user_hash("a", &[("theme", "\"Dark\"")]),
        user_hash("b", &[("theme", "\"Crab\""), ("settings", &ai_only)]),
        user_hash("c", &[("settings", r#"{"theme":"Light"}"#)]),
    ] {
        let once = up(record).expect("first conversion");
        assert_eq!(classify(&once), RecordClass::Current);
        let twice = up(once.clone()).expect("second conversion");
        assert_eq!(twice, once);
    }
}

/// A layout 2 user whose settings hold a theme and AI settings, which the first Helm revision
/// writes and reads, classifies as current and is returned as it is, so no migration toward that
/// revision touches it
#[test]
fn a_layout_2_user_with_a_theme_is_left_as_it_is() {
    let settings = format!(r#"{{"theme":"Crab","ai":{AI}}}"#);
    let user = user_hash("crab", &[("settings", &settings)]);
    assert_eq!(classify(&user), RecordClass::Current);
    assert_eq!(up(user.clone()).expect("unchanged"), user);
}

/// The reverse restores the flat theme, keeps `ai`, drops an emptied `settings`, converts users
/// that never had layout 1, and an upgrade after a changed flat theme picks up the change
#[test]
fn the_reverse_edge_round_trips_users() {
    let ai: Value = serde_json::from_str(AI).expect("ai json");
    let ai_only = format!(r#"{{"ai":{AI}}}"#);
    // up -> down -> up gives the first upgrade back for users from layout 1
    for record in [
        user_hash("a", &[("theme", "\"Dark\"")]),
        user_hash("b", &[("theme", "\"Ocean\""), ("settings", &ai_only)]),
    ] {
        let upgraded = up(record).expect("up");
        let reverted = down(upgraded.clone()).expect("down");
        assert_eq!(classify(&reverted), RecordClass::Migratable { layout: 1 });
        assert_eq!(up(reverted).expect("up again"), upgraded);
    }
    // the reverse writes exactly what the layout 1 code writes: the flat theme, and settings
    // holding only ai, or no settings at all
    let plain = down(user_hash("a", &[("settings", r#"{"theme":"Light"}"#)])).expect("down");
    assert_eq!(field(&plain, "theme").as_deref(), Some("\"Light\""));
    assert_eq!(field(&plain, "settings"), None);
    let kept = down(user_hash(
        "b",
        &[("settings", &format!(r#"{{"theme":"Crab","ai":{AI}}}"#))],
    ))
    .expect("down");
    assert_eq!(field(&kept, "theme").as_deref(), Some("\"Crab\""));
    assert_eq!(settings(&kept), Some(json!({"ai": ai})));
    // a user that only ever had layout 2 reverts, and upgrading it again gives it back
    let native = user_hash("c", &[("settings", &json!({"theme": "Ocean"}).to_string())]);
    let reverted = down(native.clone()).expect("down");
    assert_eq!(field(&reverted, "theme").as_deref(), Some("\"Ocean\""));
    assert_eq!(up(reverted).expect("up"), native);
    // a flat theme changed the way the layout 1 code changes it is what the next upgrade keeps
    let upgraded = up(user_hash("d", &[("theme", "\"Dark\"")])).expect("up");
    let mut reverted = down(upgraded).expect("down");
    if let Record::Redis { fields, .. } = &mut reverted {
        fields.insert("theme".to_owned(), "\"Crab\"".to_owned());
    }
    let again = up(reverted).expect("up after a changed theme");
    assert_eq!(settings(&again), Some(json!({"theme": "Crab"})));
    // settings holding a null `ai` hold no AI settings, so layout 1 keeps no settings for them
    let nulled = down(user_hash(
        "f",
        &[("settings", r#"{"theme":"Light","ai":null}"#)],
    ))
    .expect("down");
    assert_eq!(field(&nulled, "settings"), None);
    // a layout 2 user whose settings can't be read doesn't revert
    assert!(down(user_hash("e", &[("settings", "{\"theme\":")])).is_err());
}

/// The transform declares the fields the layouts allow to be absent
#[test]
fn the_transform_declares_its_optional_fields() {
    assert_eq!(
        USERS_SETTINGS_THEME.optional,
        &[FieldPath("settings"), FieldPath("settings.ai")]
    );
    assert_eq!((USERS_SETTINGS_THEME.from, USERS_SETTINGS_THEME.to), (1, 2));
}
