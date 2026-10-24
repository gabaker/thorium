//! The object transforms of the pattern matrix: one family for each pattern a schema change can
//! take, each with its layouts, classification, `up`, and `down`
//!
//! Every `up` derives the new form from the old form whenever the old form is present, even beside
//! a stale new form, so a record reverted and edited at the previous revision is recomputed on
//! the next upgrade. Every `down` of a lossy step leaves the fields it can't derive absent, for
//! the step's fills.

use std::collections::BTreeMap;
use thorium::models::upgrades::{
    Family, FieldPath, IssueCategory, IssueReason, Record, RecordClass, TransformDef,
    TransformIssue,
};

/// The family that renames a field with dual writes, adds a derived key, and drops the old field
pub const ITEMS: Family = Family::Redis("matrix_items");

/// The family that splits a field in two and merges the halves back
pub const PEOPLE: Family = Family::Redis("matrix_people");

/// The family that changes a field's type into a new field
pub const TASKS: Family = Family::Redis("matrix_tasks");

/// The family that gains a partition key
pub const OWNERS: Family = Family::Redis("matrix_owners");

/// The family that gains a field in a pass that must run after the tags pass
pub const RANKS: Family = Family::Redis("matrix_ranks");

/// The family that gets an independent pass and then a new enum variant
pub const TAGS: Family = Family::Redis("matrix_tags");

/// The family whose security field can only be restored by a fill
pub const ACL: Family = Family::Redis("matrix_acl");

/// The family re-keyed by an identifier other objects reference, which can't be undone
pub const TOKENS: Family = Family::Redis("matrix_tokens");

/// The S3 prefix whose objects move to a new key layout
pub const FILES: Family = Family::S3Prefix {
    bucket: "ephemeral",
    prefix: "matrix/",
};

/// The key prefix of layout 1: `matrix/obj-<n>`
pub const FILES_OLD: &str = "matrix/obj-";

/// The key prefix of layout 2: `matrix/a-v2/obj-<n>`
pub const FILES_NEW: &str = "matrix/a-v2/obj-";

/// What a lossy reverse writes into a field it couldn't derive, which the previous revision
/// accepts but never writes
pub const SENTINEL: &str = "(unknown)";

/// The tag statuses layout 2 and earlier hold
const OLD_STATUSES: [&str; 2] = ["active", "inactive"];

/// The tag statuses layout 3 holds, which adds `archived`
const NEW_STATUSES: [&str; 3] = ["active", "inactive", "archived"];

/// Build the issue of a transform that couldn't convert a record
///
/// # Arguments
///
/// * `category` - The kind of problem
/// * `field` - The field's path
/// * `layout` - The layout the transform tried
fn issue(category: IssueCategory, field: &str, layout: u32) -> TransformIssue {
    // name the one layout tried
    TransformIssue {
        reason: IssueReason::new(category, field),
        layouts_tried: vec![layout],
    }
}

/// Build the class of a record no layout of a transform parses
///
/// # Arguments
///
/// * `category` - The kind of problem
/// * `field` - The field's path
fn unrecognized(category: IssueCategory, field: &str) -> RecordClass {
    // the reason names the category and the field
    RecordClass::Unrecognized {
        reason: IssueReason::new(category, field),
    }
}

/// Get the fields of a record, or the class of a record that isn't a Redis hash
///
/// # Arguments
///
/// * `record` - The record
fn hash(record: &Record) -> Result<&BTreeMap<String, String>, RecordClass> {
    // only Redis hashes hold fields
    match record {
        Record::Redis { fields, .. } => Ok(fields),
        _ => Err(unrecognized(IssueCategory::MalformedJson, "")),
    }
}

/// Get the fields of a record to change them, or the issue of a record that isn't a Redis hash
///
/// # Arguments
///
/// * `record` - The record
/// * `layout` - The layout the transform tried
fn hash_mut(
    record: &mut Record,
    layout: u32,
) -> Result<&mut BTreeMap<String, String>, TransformIssue> {
    // only Redis hashes hold fields
    match record {
        Record::Redis { fields, .. } => Ok(fields),
        _ => Err(issue(IssueCategory::MalformedJson, "", layout)),
    }
}

/// Read a field a transform needs, or give the issue of its absence
///
/// # Arguments
///
/// * `fields` - The record's fields
/// * `field` - The field
/// * `layout` - The layout the transform tried
fn need(
    fields: &BTreeMap<String, String>,
    field: &str,
    layout: u32,
) -> Result<String, TransformIssue> {
    // an absent field is a missing field
    fields
        .get(field)
        .cloned()
        .ok_or_else(|| issue(IssueCategory::MissingField, field, layout))
}

/// Classify an items record for [`ITEMS_RENAME`]: `label` alone is layout 1, and any record with a
/// later field is past this transform
///
/// # Arguments
///
/// * `record` - The record
fn rename_classify(record: &Record) -> RecordClass {
    // layout 2 and later hold `display_name`, layout 1 only `label`
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    match (
        fields.contains_key("label"),
        fields.contains_key("display_name"),
    ) {
        (_, true) => RecordClass::Current,
        (true, false) => RecordClass::Migratable { layout: 1 },
        (false, false) => unrecognized(IssueCategory::MissingField, "label"),
    }
}

/// Upgrade an items record for [`ITEMS_RENAME`]: `display_name` is derived from `label`, which
/// stays beside it until the contract
///
/// # Arguments
///
/// * `record` - The record
fn rename_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the old field is authoritative whenever it is present
    let fields = hash_mut(&mut record, 1)?;
    match fields.get("label").cloned() {
        Some(label) => {
            fields.insert("display_name".to_owned(), label);
        }
        None if fields.contains_key("display_name") => (),
        None => return Err(issue(IssueCategory::MissingField, "label", 1)),
    }
    Ok(record)
}

/// Revert an items record for [`ITEMS_RENAME`]: `display_name` goes
///
/// # Arguments
///
/// * `record` - The record
fn rename_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the new field must be there to remove
    let fields = hash_mut(&mut record, 2)?;
    need(fields, "display_name", 2)?;
    fields.remove("display_name");
    Ok(record)
}

/// Classify an items record for [`ITEMS_KEY`]: a `display_key` makes it current
///
/// # Arguments
///
/// * `record` - The record
fn key_classify(record: &Record) -> RecordClass {
    // each layout shows in the newest field it holds
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    if fields.contains_key("display_key") {
        RecordClass::Current
    } else if fields.contains_key("display_name") {
        RecordClass::Migratable { layout: 2 }
    } else if fields.contains_key("label") {
        RecordClass::Migratable { layout: 1 }
    } else {
        unrecognized(IssueCategory::MissingField, "display_name")
    }
}

/// Upgrade an items record for [`ITEMS_KEY`]: `display_key` is the lowercased `display_name`
///
/// # Arguments
///
/// * `record` - The record
fn key_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the name is authoritative whenever it is present
    let fields = hash_mut(&mut record, 2)?;
    match fields.get("display_name").cloned() {
        Some(name) => {
            fields.insert("display_key".to_owned(), name.to_lowercase());
        }
        None if fields.contains_key("display_key") => (),
        None => return Err(issue(IssueCategory::MissingField, "display_name", 2)),
    }
    Ok(record)
}

/// Revert an items record for [`ITEMS_KEY`]: `display_key` goes
///
/// # Arguments
///
/// * `record` - The record
fn key_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the new field must be there to remove
    let fields = hash_mut(&mut record, 3)?;
    need(fields, "display_key", 3)?;
    fields.remove("display_key");
    Ok(record)
}

/// Classify an items record for [`ITEMS_CONTRACT`]: without `label` it is current
///
/// # Arguments
///
/// * `record` - The record
fn contract_classify(record: &Record) -> RecordClass {
    // a record that still holds `label` is in an earlier layout
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    match (
        fields.contains_key("label"),
        fields.contains_key("display_key"),
        fields.contains_key("display_name"),
    ) {
        (false, true, _) => RecordClass::Current,
        (true, true, _) => RecordClass::Migratable { layout: 3 },
        (true, false, true) => RecordClass::Migratable { layout: 2 },
        (true, false, false) => RecordClass::Migratable { layout: 1 },
        (false, false, _) => unrecognized(IssueCategory::MissingField, "display_key"),
    }
}

/// Upgrade an items record for [`ITEMS_CONTRACT`]: `label` is dropped, once `display_name` holds
/// what it held
///
/// # Arguments
///
/// * `record` - The record
fn contract_up(mut record: Record) -> Result<Record, TransformIssue> {
    // dropping the old field is only safe when the new one holds its value
    let fields = hash_mut(&mut record, 3)?;
    if fields.remove("label").is_some() {
        need(fields, "display_name", 3)?;
    }
    Ok(record)
}

/// Revert an items record for [`ITEMS_CONTRACT`]: nothing can be derived, so `label` stays absent
/// for the fill
///
/// # Arguments
///
/// * `record` - The record
#[allow(clippy::unnecessary_wraps)]
fn contract_down(record: Record) -> Result<Record, TransformIssue> {
    // the dropped field has no value left to restore
    Ok(record)
}

/// Classify a people record for [`PEOPLE_SPLIT`]: `full_name` alone is layout 1
///
/// # Arguments
///
/// * `record` - The record
fn split_classify(record: &Record) -> RecordClass {
    // layouts 2 and 3 are past this transform
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    if fields.contains_key("first") || fields.contains_key("name") {
        RecordClass::Current
    } else if fields.contains_key("full_name") {
        RecordClass::Migratable { layout: 1 }
    } else {
        unrecognized(IssueCategory::MissingField, "full_name")
    }
}

/// Upgrade a people record for [`PEOPLE_SPLIT`]: `full_name` splits at its first space
///
/// # Arguments
///
/// * `record` - The record
fn split_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the old field is authoritative whenever it is present
    let fields = hash_mut(&mut record, 1)?;
    match fields.remove("full_name") {
        Some(full) => {
            let (first, last) = full.split_once(' ').unwrap_or((full.as_str(), ""));
            fields.insert("first".to_owned(), first.to_owned());
            fields.insert("last".to_owned(), last.to_owned());
        }
        None => {
            need(fields, "first", 1)?;
        }
    }
    Ok(record)
}

/// Revert a people record for [`PEOPLE_SPLIT`]: the halves join with a space
///
/// # Arguments
///
/// * `record` - The record
fn split_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the first name must be there to join
    let fields = hash_mut(&mut record, 2)?;
    let first = need(fields, "first", 2)?;
    let last = fields.remove("last").unwrap_or_default();
    fields.remove("first");
    let full = if last.is_empty() || last == SENTINEL {
        first
    } else {
        format!("{first} {last}")
    };
    fields.insert("full_name".to_owned(), full);
    Ok(record)
}

/// Classify a people record for [`PEOPLE_MERGE`]: `name` is current
///
/// # Arguments
///
/// * `record` - The record
fn merge_classify(record: &Record) -> RecordClass {
    // layout 3 holds `name`, layout 2 the halves, and layout 1 `full_name`
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    if fields.contains_key("name") {
        RecordClass::Current
    } else if fields.contains_key("first") {
        RecordClass::Migratable { layout: 2 }
    } else if fields.contains_key("full_name") {
        RecordClass::Migratable { layout: 1 }
    } else {
        unrecognized(IssueCategory::MissingField, "first")
    }
}

/// Upgrade a people record for [`PEOPLE_MERGE`]: the halves join into `name`, and a last name
/// that is empty or the sentinel adds nothing, so a sentinel never becomes part of a name
///
/// # Arguments
///
/// * `record` - The record
fn merge_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the halves are authoritative whenever they are present
    let fields = hash_mut(&mut record, 2)?;
    match fields.remove("first") {
        Some(first) => {
            let last = fields.remove("last").unwrap_or_default();
            let name = if last.is_empty() || last == SENTINEL {
                first
            } else {
                format!("{first} {last}")
            };
            fields.insert("name".to_owned(), name);
        }
        None => {
            need(fields, "name", 2)?;
        }
    }
    Ok(record)
}

/// Revert a people record for [`PEOPLE_MERGE`]: `name` splits at its first space, and a name with
/// no space leaves `last` absent for the fill
///
/// # Arguments
///
/// * `record` - The record
fn merge_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the name must be there to split
    let fields = hash_mut(&mut record, 3)?;
    let name = need(fields, "name", 3)?;
    fields.remove("name");
    match name.split_once(' ') {
        Some((first, last)) => {
            fields.insert("first".to_owned(), first.to_owned());
            fields.insert("last".to_owned(), last.to_owned());
        }
        None => {
            fields.insert("first".to_owned(), name);
        }
    }
    Ok(record)
}

/// Read a task's priority as a number
///
/// # Arguments
///
/// * `raw` - The stored priority
/// * `field` - The field it came from
/// * `layout` - The layout the transform tried
fn parse_priority(raw: &str, field: &str, layout: u32) -> Result<i64, TransformIssue> {
    // anything but an integer is a value of the wrong type
    raw.parse()
        .map_err(|_| issue(IssueCategory::BadType, field, layout))
}

/// Classify a tasks record for [`TASKS_TYPE`]: `priority_text` makes it current
///
/// # Arguments
///
/// * `record` - The record
fn type_classify(record: &Record) -> RecordClass {
    // layout 2 holds the text, layout 1 only the number
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    match (fields.get("priority_text"), fields.get("priority")) {
        (Some(_), _) => RecordClass::Current,
        (None, Some(raw)) if raw.parse::<i64>().is_ok() => RecordClass::Migratable { layout: 1 },
        (None, Some(_)) => unrecognized(IssueCategory::BadType, "priority"),
        (None, None) => unrecognized(IssueCategory::MissingField, "priority"),
    }
}

/// Upgrade a tasks record for [`TASKS_TYPE`]: `priority_text` is the number written out, and the
/// number stays beside it
///
/// # Arguments
///
/// * `record` - The record
fn type_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the number is authoritative whenever it is present
    let fields = hash_mut(&mut record, 1)?;
    match fields.get("priority").cloned() {
        Some(raw) => {
            let number = parse_priority(&raw, "priority", 1)?;
            fields.insert("priority_text".to_owned(), number.to_string());
        }
        None => {
            need(fields, "priority_text", 1)?;
        }
    }
    Ok(record)
}

/// Revert a tasks record for [`TASKS_TYPE`]: the number is read back from the text
///
/// # Arguments
///
/// * `record` - The record
fn type_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the text must hold a number
    let fields = hash_mut(&mut record, 2)?;
    let raw = need(fields, "priority_text", 2)?;
    let number = parse_priority(&raw, "priority_text", 2)?;
    fields.remove("priority_text");
    fields.insert("priority".to_owned(), number.to_string());
    Ok(record)
}

/// The partition an owner's records are filed under: its first letter, lowercased
///
/// # Arguments
///
/// * `owner` - The owner
fn bucket_of(owner: &str) -> String {
    // an owner with no letters files under an underscore
    owner
        .chars()
        .next()
        .map_or_else(|| "_".to_owned(), |first| first.to_lowercase().to_string())
}

/// Classify an owners record for [`OWNERS_KEY`]: a `bucket` makes it current
///
/// # Arguments
///
/// * `record` - The record
fn bucket_classify(record: &Record) -> RecordClass {
    // layout 2 and later hold the bucket
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    if fields.contains_key("bucket") {
        RecordClass::Current
    } else if fields.contains_key("owner") {
        RecordClass::Migratable { layout: 1 }
    } else {
        unrecognized(IssueCategory::MissingField, "owner")
    }
}

/// Upgrade an owners record for [`OWNERS_KEY`]: the bucket is derived from the owner
///
/// # Arguments
///
/// * `record` - The record
fn bucket_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the owner is authoritative whenever it is present
    let fields = hash_mut(&mut record, 1)?;
    match fields.get("owner").cloned() {
        Some(owner) => {
            fields.insert("bucket".to_owned(), bucket_of(&owner));
        }
        None => {
            need(fields, "bucket", 1)?;
        }
    }
    Ok(record)
}

/// Revert an owners record for [`OWNERS_KEY`]: the bucket goes
///
/// # Arguments
///
/// * `record` - The record
fn bucket_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the bucket must be there to remove
    let fields = hash_mut(&mut record, 2)?;
    need(fields, "bucket", 2)?;
    fields.remove("bucket");
    Ok(record)
}

/// Classify a ranks record for [`RANKS_LINK`]: a `tag_ref` makes it current
///
/// # Arguments
///
/// * `record` - The record
fn link_classify(record: &Record) -> RecordClass {
    // layout 2 holds the reference to a tag
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    if fields.contains_key("tag_ref") {
        RecordClass::Current
    } else if fields.contains_key("entry") {
        RecordClass::Migratable { layout: 1 }
    } else {
        unrecognized(IssueCategory::MissingField, "entry")
    }
}

/// Upgrade a ranks record for [`RANKS_LINK`]: the reference is the entry lowercased, the form
/// the tags pass gives a tag
///
/// # Arguments
///
/// * `record` - The record
fn link_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the entry is authoritative whenever it is present
    let fields = hash_mut(&mut record, 1)?;
    match fields.get("entry").cloned() {
        Some(entry) => {
            fields.insert("tag_ref".to_owned(), entry.trim().to_lowercase());
        }
        None => {
            need(fields, "tag_ref", 1)?;
        }
    }
    Ok(record)
}

/// Revert a ranks record for [`RANKS_LINK`]: the reference goes
///
/// # Arguments
///
/// * `record` - The record
fn link_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the reference must be there to remove
    let fields = hash_mut(&mut record, 2)?;
    need(fields, "tag_ref", 2)?;
    fields.remove("tag_ref");
    Ok(record)
}

/// Classify a tags record for [`TAGS_NORMALIZE`]: a `normalized` field makes it current
///
/// # Arguments
///
/// * `record` - The record
fn normalize_classify(record: &Record) -> RecordClass {
    // layout 2 and later hold the normalized tag
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    if fields.contains_key("normalized") {
        RecordClass::Current
    } else if fields.contains_key("tag") {
        RecordClass::Migratable { layout: 1 }
    } else {
        unrecognized(IssueCategory::MissingField, "tag")
    }
}

/// Upgrade a tags record for [`TAGS_NORMALIZE`]: the tag trimmed and lowercased
///
/// # Arguments
///
/// * `record` - The record
fn normalize_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the tag is authoritative whenever it is present
    let fields = hash_mut(&mut record, 1)?;
    match fields.get("tag").cloned() {
        Some(tag) => {
            fields.insert("normalized".to_owned(), tag.trim().to_lowercase());
        }
        None => {
            need(fields, "normalized", 1)?;
        }
    }
    Ok(record)
}

/// Revert a tags record for [`TAGS_NORMALIZE`]: the normalized tag goes
///
/// # Arguments
///
/// * `record` - The record
fn normalize_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the normalized tag must be there to remove
    let fields = hash_mut(&mut record, 2)?;
    need(fields, "normalized", 2)?;
    fields.remove("normalized");
    Ok(record)
}

/// Classify a tags record for [`TAGS_STATUS`]: `retention` makes it current, and a status the
/// layout doesn't define is unrecognized
///
/// # Arguments
///
/// * `record` - The record
fn status_classify(record: &Record) -> RecordClass {
    // layout 3 holds `retention` and may hold the new status
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    let status = fields.get("status").map(String::as_str);
    let current = fields.contains_key("retention");
    // the statuses a layout holds depend on the layout
    let allowed: &[&str] = if current {
        &NEW_STATUSES
    } else {
        &OLD_STATUSES
    };
    match status {
        Some(status) if !allowed.contains(&status) => {
            unrecognized(IssueCategory::UnknownVariant, "status")
        }
        _ if current => RecordClass::Current,
        _ if fields.contains_key("normalized") => RecordClass::Migratable { layout: 2 },
        _ if fields.contains_key("tag") => RecordClass::Migratable { layout: 1 },
        _ => unrecognized(IssueCategory::MissingField, "tag"),
    }
}

/// Upgrade a tags record for [`TAGS_STATUS`]: `retention` is added, and the status must be one the
/// old layout held
///
/// # Arguments
///
/// * `record` - The record
fn status_up(mut record: Record) -> Result<Record, TransformIssue> {
    // a record already in layout 3 is left as it is
    let fields = hash_mut(&mut record, 2)?;
    if fields.contains_key("retention") {
        return Ok(record);
    }
    // otherwise its status must be one layout 2 defines
    let status = need(fields, "status", 2)?;
    if !OLD_STATUSES.contains(&status.as_str()) {
        return Err(issue(IssueCategory::UnknownVariant, "status", 2));
    }
    fields.insert("retention".to_owned(), "keep".to_owned());
    Ok(record)
}

/// Revert a tags record for [`TAGS_STATUS`]: `retention` goes, and the enum mapping of the lossy
/// step rewrites a status the old layout doesn't define
///
/// # Arguments
///
/// * `record` - The record
fn status_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the retention must be there to remove
    let fields = hash_mut(&mut record, 3)?;
    need(fields, "retention", 3)?;
    fields.remove("retention");
    Ok(record)
}

/// Classify an access record for [`ACL_ALLOWED`]: `allowed` makes it current
///
/// # Arguments
///
/// * `record` - The record
fn allowed_classify(record: &Record) -> RecordClass {
    // layout 2 holds `allowed`, layout 1 the public flag
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    match (fields.get("allowed"), fields.get("public")) {
        (Some(_), _) => RecordClass::Current,
        (None, Some(flag)) if flag == "true" || flag == "false" => {
            RecordClass::Migratable { layout: 1 }
        }
        (None, Some(_)) => unrecognized(IssueCategory::InvalidValue, "public"),
        (None, None) => unrecognized(IssueCategory::MissingField, "public"),
    }
}

/// Upgrade an access record for [`ACL_ALLOWED`]: a public record is open to everyone and any
/// other to its owner
///
/// # Arguments
///
/// * `record` - The record
fn allowed_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the flag is authoritative whenever it is present
    let fields = hash_mut(&mut record, 1)?;
    match fields.remove("public") {
        Some(flag) if flag == "true" => {
            fields.insert("allowed".to_owned(), "everyone".to_owned());
        }
        Some(flag) if flag == "false" => {
            fields.insert("allowed".to_owned(), "owner".to_owned());
        }
        Some(_) => return Err(issue(IssueCategory::InvalidValue, "public", 1)),
        None => {
            need(fields, "allowed", 1)?;
        }
    }
    Ok(record)
}

/// Revert an access record for [`ACL_ALLOWED`]: `allowed` goes, and the flag stays absent for the
/// fill, since nothing derived from `allowed` may open a record that was closed
///
/// # Arguments
///
/// * `record` - The record
fn allowed_down(mut record: Record) -> Result<Record, TransformIssue> {
    // the new field must be there to remove
    let fields = hash_mut(&mut record, 2)?;
    need(fields, "allowed", 2)?;
    fields.remove("allowed");
    Ok(record)
}

/// Classify a tokens record for [`TOKENS_REKEY`]: an `ident` makes it current
///
/// # Arguments
///
/// * `record` - The record
fn rekey_classify(record: &Record) -> RecordClass {
    // layout 2 holds the derived identifier
    let fields = match hash(record) {
        Ok(fields) => fields,
        Err(class) => return class,
    };
    if fields.contains_key("ident") {
        RecordClass::Current
    } else if fields.contains_key("token") {
        RecordClass::Migratable { layout: 1 }
    } else {
        unrecognized(IssueCategory::MissingField, "token")
    }
}

/// Upgrade a tokens record for [`TOKENS_REKEY`]: the identifier derives from the token
///
/// # Arguments
///
/// * `record` - The record
fn rekey_up(mut record: Record) -> Result<Record, TransformIssue> {
    // the token is authoritative whenever it is present
    let fields = hash_mut(&mut record, 1)?;
    match fields.get("token").cloned() {
        Some(token) => {
            fields.insert("ident".to_owned(), format!("id-{token}"));
        }
        None => {
            need(fields, "ident", 1)?;
        }
    }
    Ok(record)
}

/// Get the key of an S3 record, or the class of a record that isn't an S3 object
///
/// # Arguments
///
/// * `record` - The record
fn object_key(record: &Record) -> Result<&str, RecordClass> {
    // only S3 records hold an object key
    match record {
        Record::S3 { key } => Ok(key),
        _ => Err(unrecognized(IssueCategory::MalformedJson, "")),
    }
}

/// Classify a file for [`FILES_LAYOUT`]: a key in the new layout is current
///
/// # Arguments
///
/// * `record` - The record
fn layout_classify(record: &Record) -> RecordClass {
    // the key's prefix says which layout it is in
    match object_key(record) {
        Ok(key) if key.starts_with(FILES_NEW) => RecordClass::Current,
        Ok(key) if key.starts_with(FILES_OLD) => RecordClass::Migratable { layout: 1 },
        Ok(_) => unrecognized(IssueCategory::InvalidValue, "key"),
        Err(class) => class,
    }
}

/// Upgrade a file for [`FILES_LAYOUT`]: its key moves under the new prefix
///
/// # Arguments
///
/// * `record` - The record
fn layout_up(record: Record) -> Result<Record, TransformIssue> {
    // a key in the old layout moves, and one already moved stays
    match &record {
        Record::S3 { key } if key.starts_with(FILES_OLD) => Ok(Record::S3 {
            key: key.replacen(FILES_OLD, FILES_NEW, 1),
        }),
        Record::S3 { key } if key.starts_with(FILES_NEW) => Ok(record),
        Record::S3 { .. } => Err(issue(IssueCategory::InvalidValue, "key", 1)),
        _ => Err(issue(IssueCategory::MalformedJson, "", 1)),
    }
}

/// Revert a file for [`FILES_LAYOUT`]: its key moves back under the old prefix
///
/// # Arguments
///
/// * `record` - The record
#[allow(clippy::needless_pass_by_value)]
fn layout_down(record: Record) -> Result<Record, TransformIssue> {
    // only a key in the new layout moves back
    match &record {
        Record::S3 { key } if key.starts_with(FILES_NEW) => Ok(Record::S3 {
            key: key.replacen(FILES_NEW, FILES_OLD, 1),
        }),
        Record::S3 { .. } => Err(issue(IssueCategory::InvalidValue, "key", 2)),
        _ => Err(issue(IssueCategory::MalformedJson, "", 2)),
    }
}

/// Items gain `display_name`, derived from `label`, with `label` kept beside it
pub const ITEMS_RENAME: TransformDef = TransformDef {
    family: ITEMS,
    reads: &[],
    from: 1,
    to: 2,
    classify: rename_classify,
    up: rename_up,
    down: Some(rename_down),
    optional: &[],
};

/// Items gain `display_key`, the lowercased `display_name`
pub const ITEMS_KEY: TransformDef = TransformDef {
    family: ITEMS,
    reads: &[],
    from: 2,
    to: 3,
    classify: key_classify,
    up: key_up,
    down: Some(key_down),
    optional: &[],
};

/// Items lose `label`
pub const ITEMS_CONTRACT: TransformDef = TransformDef {
    family: ITEMS,
    reads: &[],
    from: 3,
    to: 4,
    classify: contract_classify,
    up: contract_up,
    down: Some(contract_down),
    optional: &[],
};

/// People split `full_name` into `first` and `last`
pub const PEOPLE_SPLIT: TransformDef = TransformDef {
    family: PEOPLE,
    reads: &[],
    from: 1,
    to: 2,
    classify: split_classify,
    up: split_up,
    down: Some(split_down),
    optional: &[],
};

/// People merge `first` and `last` into `name`
pub const PEOPLE_MERGE: TransformDef = TransformDef {
    family: PEOPLE,
    reads: &[],
    from: 2,
    to: 3,
    classify: merge_classify,
    up: merge_up,
    down: Some(merge_down),
    optional: &[],
};

/// Tasks gain `priority_text`, their `priority` written out
pub const TASKS_TYPE: TransformDef = TransformDef {
    family: TASKS,
    reads: &[],
    from: 1,
    to: 2,
    classify: type_classify,
    up: type_up,
    down: Some(type_down),
    optional: &[],
};

/// Owners gain the `bucket` they are partitioned by
pub const OWNERS_KEY: TransformDef = TransformDef {
    family: OWNERS,
    reads: &[],
    from: 1,
    to: 2,
    classify: bucket_classify,
    up: bucket_up,
    down: Some(bucket_down),
    optional: &[],
};

/// Ranks gain a `tag_ref`, in a pass that runs after the tags pass
pub const RANKS_LINK: TransformDef = TransformDef {
    family: RANKS,
    reads: &[TAGS],
    from: 1,
    to: 2,
    classify: link_classify,
    up: link_up,
    down: Some(link_down),
    optional: &[],
};

/// Tags gain the `normalized` form of their tag
pub const TAGS_NORMALIZE: TransformDef = TransformDef {
    family: TAGS,
    reads: &[],
    from: 1,
    to: 2,
    classify: normalize_classify,
    up: normalize_up,
    down: Some(normalize_down),
    optional: &[],
};

/// Tags gain `retention` and may hold the `archived` status
pub const TAGS_STATUS: TransformDef = TransformDef {
    family: TAGS,
    reads: &[],
    from: 2,
    to: 3,
    classify: status_classify,
    up: status_up,
    down: Some(status_down),
    optional: &[],
};

/// Access records replace the `public` flag with `allowed`
pub const ACL_ALLOWED: TransformDef = TransformDef {
    family: ACL,
    reads: &[],
    from: 1,
    to: 2,
    classify: allowed_classify,
    up: allowed_up,
    down: Some(allowed_down),
    optional: &[],
};

/// Tokens gain an `ident` derived from the token, which other objects reference
pub const TOKENS_REKEY: TransformDef = TransformDef {
    family: TOKENS,
    reads: &[],
    from: 1,
    to: 2,
    classify: rekey_classify,
    up: rekey_up,
    down: None,
    optional: &[FieldPath("token")],
};

/// Files move to a new key layout
pub const FILES_LAYOUT: TransformDef = TransformDef {
    family: FILES,
    reads: &[],
    from: 1,
    to: 2,
    classify: layout_classify,
    up: layout_up,
    down: Some(layout_down),
    optional: &[],
};
