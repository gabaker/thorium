//! The users example's transform: a user's theme moves between a flat `theme` field of the user's
//! hash (layout 1) and the `settings` field (layout 2)
//!
//! Layout 1 stores the theme as JSON text in a `theme` field of the user's hash, and keeps in
//! `settings` only the AI settings, which are left out when a user has none. Layout 2 keeps the
//! whole of `UserSettings`, theme included, in `settings`. The classification, the conversion
//! forward, and the conversion back are plain functions of one user's hash, shared by the thoradm
//! migrations, the test harness, and the catalog's round-trip checks.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use super::{
    Family, FieldPath, IssueCategory, IssueReason, ReasonBuilder, Record, RecordClass,
    TransformDef, TransformIssue,
};
use crate::models::{AiSettings, Theme, UserSettings};

/// The `settings` field as layout 1 stores it, beside a flat theme: only the AI settings
#[derive(Deserialize)]
struct SettingsV1 {
    /// The AI settings, when the user has any; only parsing reads them, to reject a user whose
    /// settings the layout can't hold
    #[serde(default)]
    #[allow(dead_code)]
    ai: Option<AiSettings>,
}

/// Which layout a user's stored fields are in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stored {
    /// The theme is in its own field
    Flat,
    /// The theme is in `settings`, or the user has none and the API reads the default
    Nested,
}

/// Get the fields of a user's hash
///
/// # Arguments
///
/// * `record` - The user's hash
fn fields_of(record: &Record) -> Result<&BTreeMap<String, String>, IssueReason> {
    // only a Redis hash holds a user
    match record {
        Record::Redis { fields, .. } => Ok(fields),
        _ => Err(IssueReason::new(IssueCategory::InvalidValue, "")),
    }
}

/// Work out which layout a user's fields are in, or why they can't be read
///
/// # Arguments
///
/// * `fields` - The user's hash
fn read_stored(fields: &BTreeMap<String, String>) -> Result<Stored, IssueReason> {
    // the settings text must be JSON whichever layout the user is in
    let settings = fields.get("settings");
    if let Some(raw) = settings {
        serde_json::from_str::<Value>(raw)
            .map_err(|_| IssueReason::new(IssueCategory::MalformedJson, "settings"))?;
    }
    // a flat theme makes the user layout 1, whose settings may hold only AI settings
    if let Some(flat) = fields.get("theme") {
        ReasonBuilder::parse::<Theme>(flat, "theme")?;
        if let Some(raw) = settings {
            ReasonBuilder::parse::<SettingsV1>(raw, "settings")?;
        }
        return Ok(Stored::Flat);
    }
    // otherwise the settings are the whole of `UserSettings`, the way the API reads them, and a
    // user with no settings reads as the default
    if let Some(raw) = settings {
        ReasonBuilder::parse::<UserSettings>(raw, "settings")?;
    }
    Ok(Stored::Nested)
}

/// Read the `settings` field of a user as a JSON object, or an empty one when it has none
///
/// # Arguments
///
/// * `fields` - The user's hash, whose settings were already read as JSON
fn settings_object(fields: &BTreeMap<String, String>) -> Result<Map<String, Value>, IssueReason> {
    // a missing field is an empty object, and anything but an object is the wrong type
    match fields.get("settings") {
        None => Ok(Map::new()),
        Some(raw) => match serde_json::from_str::<Value>(raw) {
            Ok(Value::Object(settings)) => Ok(settings),
            Ok(_) => Err(IssueReason::new(IssueCategory::BadType, "settings")),
            Err(_) => Err(IssueReason::new(IssueCategory::MalformedJson, "settings")),
        },
    }
}

/// Classify a user for the conversion to layout 2
///
/// A user with a flat theme is migratable, whether or not its settings also hold a theme: the
/// flat field is the one the layout 1 code keeps current. A user whose settings hold a readable
/// theme, or that has no settings and so reads as the default theme, is current. Anything else is
/// recorded under the reason code of the field that couldn't be read.
///
/// # Arguments
///
/// * `record` - The user's hash
#[must_use]
pub fn classify(record: &Record) -> RecordClass {
    // read the layout the fields are in, or say why they can't be
    match fields_of(record).and_then(read_stored) {
        Ok(Stored::Flat) => RecordClass::Migratable { layout: 1 },
        Ok(Stored::Nested) => RecordClass::Current,
        Err(reason) => RecordClass::Unrecognized { reason },
    }
}

/// Classify a user for the conversion back to layout 1
///
/// A user in layout 2 is migratable, and one that already holds its theme flat is current.
///
/// # Arguments
///
/// * `record` - The user's hash
#[must_use]
pub fn classify_reverse(record: &Record) -> RecordClass {
    // the reverse reads the same layouts the other way round
    match fields_of(record).and_then(read_stored) {
        Ok(Stored::Flat) => RecordClass::Current,
        Ok(Stored::Nested) => RecordClass::Migratable { layout: 2 },
        Err(reason) => RecordClass::Unrecognized { reason },
    }
}

/// Build the issue of a user that can't be converted
///
/// # Arguments
///
/// * `reason` - Why
/// * `layouts` - The layouts tried
fn issue(reason: IssueReason, layouts: &[u32]) -> TransformIssue {
    // the reason names the field, never the data in it
    TransformIssue {
        reason,
        layouts_tried: layouts.to_vec(),
    }
}

/// Convert a user from layout 1 to layout 2
///
/// The flat theme is merged into `settings`, which is created when the user has none and keeps the
/// AI settings it holds, and the flat field is removed. Whenever the user holds a flat theme it
/// wins over a theme already inside `settings`, so a user whose flat theme was changed after a
/// reverse keeps the change. A user already in layout 2 is returned as it is.
///
/// # Arguments
///
/// * `record` - The user's hash in layout 1
pub fn up(record: Record) -> Result<Record, TransformIssue> {
    // read the layout the user is in
    let Record::Redis { key, mut fields } = record else {
        return Err(issue(
            IssueReason::new(IssueCategory::InvalidValue, ""),
            &[1],
        ));
    };
    match read_stored(&fields) {
        Ok(Stored::Flat) => (),
        Ok(Stored::Nested) => return Ok(Record::Redis { key, fields }),
        Err(reason) => return Err(issue(reason, &[1, 2])),
    }
    // move the flat theme into the settings object, creating it when the user has none
    let theme = fields
        .remove("theme")
        .and_then(|flat| serde_json::from_str::<Value>(&flat).ok())
        .ok_or_else(|| {
            issue(
                IssueReason::new(IssueCategory::MalformedJson, "theme"),
                &[1],
            )
        })?;
    let mut settings = settings_object(&fields).map_err(|reason| issue(reason, &[1]))?;
    settings.insert("theme".to_owned(), theme);
    fields.insert("settings".to_owned(), Value::Object(settings).to_string());
    Ok(Record::Redis { key, fields })
}

/// Convert a user from layout 2 to layout 1
///
/// The theme moves out of `settings` into the flat field, a user with no settings gets the
/// default theme the API reads for it, and `settings` is removed when only the theme was in it,
/// exactly as the layout 1 code stores a user without AI settings. A user already holding a flat
/// theme is returned as it is.
///
/// # Arguments
///
/// * `record` - The user's hash in layout 2
pub fn down(record: Record) -> Result<Record, TransformIssue> {
    // read the layout the user is in
    let Record::Redis { key, mut fields } = record else {
        return Err(issue(
            IssueReason::new(IssueCategory::InvalidValue, ""),
            &[2],
        ));
    };
    match read_stored(&fields) {
        Ok(Stored::Nested) => (),
        Ok(Stored::Flat) => return Ok(Record::Redis { key, fields }),
        Err(reason) => return Err(issue(reason, &[2, 1])),
    }
    // take the theme out of the settings, falling back to the default theme
    let mut settings = settings_object(&fields).map_err(|reason| issue(reason, &[2]))?;
    let theme = match settings.remove("theme") {
        Some(theme) => theme.to_string(),
        None => serde_json::to_string(&Theme::default())
            .map_err(|_| issue(IssueReason::new(IssueCategory::InvalidValue, "theme"), &[2]))?,
    };
    // a null `ai` is no AI settings, which layout 1 leaves out
    if settings.get("ai").is_some_and(Value::is_null) {
        settings.remove("ai");
    }
    // keep what is left in settings, or drop the field when nothing is
    if settings.is_empty() {
        fields.remove("settings");
    } else {
        fields.insert("settings".to_owned(), Value::Object(settings).to_string());
    }
    fields.insert("theme".to_owned(), theme);
    Ok(Record::Redis { key, fields })
}

/// Users with the theme in `settings`, built from users with the theme in its own field
pub const USERS_SETTINGS_THEME: TransformDef = TransformDef {
    family: Family::Redis("users"),
    reads: &[],
    from: 1,
    to: 2,
    classify,
    up,
    down: Some(down),
    optional: &[FieldPath("settings"), FieldPath("settings.ai")],
};

/// Build a user's hash from its fields
///
/// The key is `user_data:<name>`, relative to the namespace, as the migrations read it.
///
/// # Arguments
///
/// * `name` - The username
/// * `fields` - The fields the hash holds
#[must_use]
pub fn user_hash(name: &str, fields: &[(&str, &str)]) -> Record {
    // the key is relative to the namespace, as the runner reads it
    Record::Redis {
        key: format!("user_data:{name}"),
        fields: fields
            .iter()
            .map(|(field, value)| ((*field).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

/// The AI settings the fixtures give users that have some
const FIXTURE_AI: &str = r#"{"endpoints":{},"default_endpoint":"x"}"#;

/// The `settings` field of a fixture user in layout 2
///
/// # Arguments
///
/// * `theme` - The theme's name
/// * `ai` - Whether the user has AI settings
fn nested_settings(theme: &str, ai: bool) -> String {
    // the AI settings come first, as the conversion leaves them when it adds the theme
    let mut settings = Map::new();
    if ai {
        let ai: Value = serde_json::from_str(FIXTURE_AI).unwrap_or(Value::Null);
        settings.insert("ai".to_owned(), ai);
    }
    settings.insert("theme".to_owned(), Value::String(theme.to_owned()));
    Value::Object(settings).to_string()
}

/// Users in layout 1 the round trip of the step runs on: with and without AI settings
#[must_use]
pub fn round_trip_fixtures() -> Vec<Record> {
    // the AI settings are stored beside a flat theme in `settings`
    let ai_only = format!(r#"{{"ai":{FIXTURE_AI}}}"#);
    vec![
        user_hash("round-trip-a", &[("theme", "\"Dark\"")]),
        user_hash(
            "round-trip-b",
            &[("theme", "\"Ocean\""), ("settings", &ai_only)],
        ),
        user_hash("round-trip-c", &[("theme", "\"Automatic\"")]),
    ]
}

/// Users in layout 2 that never had layout 1: with and without AI settings
#[must_use]
pub fn native_fixtures() -> Vec<Record> {
    // each reverts to a flat theme and converts back to the same settings
    vec![
        user_hash(
            "native-a",
            &[("settings", &nested_settings("Light", false))],
        ),
        user_hash("native-b", &[("settings", &nested_settings("Crab", true))]),
    ]
}
