//! Record-based object transforms, the stored records they read and write, and the reason codes
//! an object that can't be converted is recorded under

use std::fmt;
use std::str::FromStr;

use crate::models::InvalidEnum;

#[cfg(feature = "scylla-utils")]
use super::artifacts::within_role;
#[cfg(feature = "scylla-utils")]
use super::{Family, FillValue, Reversibility};
#[cfg(feature = "scylla-utils")]
use scylla::value::CqlValue;
#[cfg(feature = "scylla-utils")]
use std::collections::BTreeMap;

/// A record-based transform of one family between two layout versions
#[cfg(feature = "scylla-utils")]
#[derive(Debug, Clone, Copy)]
pub struct TransformDef {
    /// The family this transform rewrites
    pub family: Family,
    /// The families this transform only reads, which orders it after any pass writing them
    pub reads: &'static [Family],
    /// The layout this transform reads (for a Scylla table, a counter its transforms declare)
    pub from: u32,
    /// The layout this transform writes
    pub to: u32,
    /// Classify one record before touching it
    pub classify: fn(&Record) -> RecordClass,
    /// Convert a record from `from` to `to`
    ///
    /// Whenever the record holds the old form, `up` derives the new form from it, even when the
    /// record already holds a new form too: after a reverse the old form is authoritative and
    /// the new form may be stale, and a fused chain's after-a-reverse mode relies on `up` to
    /// recompute it.
    pub up: fn(Record) -> Result<Record, TransformIssue>,
    /// Convert a record from `to` back to `from`, required when the edge has a revert or reverse
    /// edge
    pub down: Option<fn(Record) -> Result<Record, TransformIssue>>,
    /// The fields the layouts allow to be absent, which the harness varies in its generated
    /// fixtures
    pub optional: &'static [FieldPath],
}

/// A stored object in its stored form, so old layouts stay expressible after their structs change
#[cfg(feature = "scylla-utils")]
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// A Scylla row as a column map
    Scylla {
        /// The row's primary key, rendered as text for issues and `--objects`
        key: String,
        /// Every column the row has, by name (`None` for a null)
        columns: BTreeMap<String, Option<CqlValue>>,
    },
    /// A Redis hash as a field map
    Redis {
        /// The hash's key
        key: String,
        /// Every field of the hash
        fields: BTreeMap<String, String>,
    },
    /// An S3 object, by key
    S3 {
        /// The object's key
        key: String,
    },
}

/// How a transform classified a record
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordClass {
    /// Already in the transform's target layout
    Current,
    /// In a layout the chain converts from
    Migratable {
        /// The layout the record is in
        layout: u32,
    },
    /// No layout parses; the record is left untouched and recorded as an issue
    Unrecognized {
        /// Why, as a reason code; never the record's contents
        reason: IssueReason,
    },
}

/// Why a transform couldn't convert one record
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransformIssue {
    /// Why, as a reason code; never the record's contents
    pub reason: IssueReason,
    /// The layouts tried before giving up
    pub layouts_tried: Vec<u32>,
}

#[cfg(feature = "scylla-utils")]
impl TransformDef {
    /// Check that reverting a record and upgrading it again matches the first upgrade
    ///
    /// Runs `up`, then `down`, then `up` on the fixture and compares the two upgraded records, so
    /// an object reverted with a branch's image and re-upgraded later ends up as it was.
    ///
    /// # Arguments
    ///
    /// * `fixture` - A record in this transform's `from` layout
    pub fn check_round_trip(&self, fixture: &Record) -> Result<(), String> {
        // a transform without a `down` can't be reverted at all
        let down = self
            .down
            .ok_or_else(|| "the transform has no `down`".to_owned())?;
        // upgrade, revert, and upgrade the fixture again
        let describe =
            |what: &str, issue: TransformIssue| format!("{what} failed: {}", issue.reason);
        let upgraded = (self.up)(fixture.clone()).map_err(|issue| describe("up", issue))?;
        let reverted = down(upgraded.clone()).map_err(|issue| describe("down", issue))?;
        let again = (self.up)(reverted).map_err(|issue| describe("up after down", issue))?;
        // the second upgrade must match the first
        if again == upgraded {
            Ok(())
        } else {
            Err(format!(
                "up -> down -> up gave {again:?}, but up gave {upgraded:?}"
            ))
        }
    }
}

#[cfg(feature = "scylla-utils")]
impl TransformDef {
    /// Revert one record from this transform's `to` layout to its `from` layout
    ///
    /// Runs `down`, then for a lossy reverse maps each new enum variant to its old variant and
    /// writes each fill whose field `down` left absent: a constant or sentinel as given, or a
    /// derived value computed from the record before `down` ran. A field `down` derived is kept.
    ///
    /// A fill of a field the reverse declares a privilege order for must write a role the order
    /// lists, holding no grant the record's role in the new layout lacks (or, when the record
    /// holds no role the order lists, no grant any listed role lacks). The fill lint runs a
    /// derived fill only on records holding the role field alone, so a derive reading any other
    /// field is held to this here: a fill that would widen the record's role makes it an issue
    /// instead of writing the role.
    ///
    /// # Arguments
    ///
    /// * `record` - The record in this transform's `to` layout
    /// * `reversibility` - How the step running this transform is undone, if it declares it
    pub fn revert(
        &self,
        record: Record,
        reversibility: Option<&Reversibility>,
    ) -> Result<Record, TransformIssue> {
        // a transform without a `down` can't revert anything
        let Some(down) = self.down else {
            return Err(TransformIssue {
                reason: IssueReason::new(IssueCategory::InvalidValue, ""),
                layouts_tried: vec![self.to],
            });
        };
        // only a lossy reverse maps variants and fills fields
        let Some(Reversibility::LossyRevert {
            fills,
            enum_mappings,
            privilege_orders,
            ..
        }) = reversibility
        else {
            return down(record);
        };
        // keep the new form, which derived fills read, and revert the record
        let new = record.clone();
        // revert the record with the transform's `down`
        let mut old = down(record)?;
        // map each variant added in the new layout to the old variant it stands for
        for mapping in *enum_mappings {
            if old.text(mapping.field).as_deref() == Some(mapping.new_variant) {
                old.set_text(mapping.field, mapping.old_variant)
                    .map_err(|reason| self.revert_issue(reason))?;
            }
        }
        // fill every field the reverse couldn't derive
        for fill in *fills {
            if old.has_field(fill.field) {
                continue;
            }
            let value = match fill.value {
                FillValue::Constant(value) | FillValue::Sentinel(value) => value.to_owned(),
                FillValue::Derived(derive) => derive(&new),
            };
            // a role fill may grant nothing the record's own role lacks, whatever it read
            let widens = privilege_orders
                .iter()
                .filter(|order| order.field == fill.field)
                .any(|order| !within_role(order, &new, fill.field, &value));
            if widens {
                return Err(
                    self.revert_issue(IssueReason::new(IssueCategory::InvalidValue, fill.field))
                );
            }
            old.set_text(fill.field, &value)
                .map_err(|reason| self.revert_issue(reason))?;
        }
        Ok(old)
    }

    /// Build the issue of a record this transform couldn't revert
    ///
    /// # Arguments
    ///
    /// * `reason` - Why
    fn revert_issue(&self, reason: IssueReason) -> TransformIssue {
        // name the layout the reverse read
        TransformIssue {
            reason,
            layouts_tried: vec![self.to],
        }
    }
}

#[cfg(feature = "scylla-utils")]
impl Record {
    /// Whether this record holds a field or column, null or not
    ///
    /// # Arguments
    ///
    /// * `field` - The field or column
    #[must_use]
    pub fn has_field(&self, field: &str) -> bool {
        // an S3 object has no fields
        match self {
            Record::Scylla { columns, .. } => columns.contains_key(field),
            Record::Redis { fields, .. } => fields.contains_key(field),
            Record::S3 { .. } => false,
        }
    }

    /// Get a field or a text column's value as text
    ///
    /// # Arguments
    ///
    /// * `field` - The field or column
    #[must_use]
    pub fn text(&self, field: &str) -> Option<String> {
        // only Redis fields and text columns hold text
        match self {
            Record::Scylla { columns, .. } => match columns.get(field) {
                Some(Some(CqlValue::Text(text) | CqlValue::Ascii(text))) => Some(text.clone()),
                _ => None,
            },
            Record::Redis { fields, .. } => fields.get(field).cloned(),
            Record::S3 { .. } => None,
        }
    }

    /// Set a field or column from its text form: a Redis field value as is, or a CQL literal
    /// read as the type of the column's current value (`null`, a quoted string, or an unquoted
    /// boolean or number when the column has no typed value)
    ///
    /// `null` clears the value on every backend: it removes a Redis field, so a fill that clears
    /// a credential never stores the text `null` as one.
    ///
    /// # Arguments
    ///
    /// * `field` - The field or column
    /// * `raw` - The value's text form
    pub fn set_text(&mut self, field: &str, raw: &str) -> Result<(), IssueReason> {
        // write the value in the form this kind of record stores
        match self {
            // a Scylla column takes the type its current value has
            Record::Scylla { columns, .. } => {
                let like = columns.get(field).cloned().flatten();
                let value = cql_literal(field, raw, like.as_ref())?;
                columns.insert(field.to_owned(), value);
                Ok(())
            }
            // a Redis field holds the text as is, and a null removes it
            Record::Redis { fields, .. } => {
                if raw.eq_ignore_ascii_case("null") {
                    fields.remove(field);
                } else {
                    fields.insert(field.to_owned(), raw.to_owned());
                }
                Ok(())
            }
            // an S3 object has no fields to set
            Record::S3 { .. } => Err(IssueReason::new(IssueCategory::UnknownField, field)),
        }
    }
}

/// Read a CQL literal as a column value, typed like an existing value when there is one
///
/// Returns `None` for `null`, and an `invalid_value` reason naming the column when the literal
/// doesn't parse as the column's type.
///
/// # Arguments
///
/// * `field` - The column, named in the reason
/// * `raw` - The literal
/// * `like` - The column's current value, whose type the literal is read as
#[cfg(feature = "scylla-utils")]
fn cql_literal(
    field: &str,
    raw: &str,
    like: Option<&CqlValue>,
) -> Result<Option<CqlValue>, IssueReason> {
    // a null clears the column
    if raw.eq_ignore_ascii_case("null") {
        return Ok(None);
    }
    // a literal that doesn't parse as the column's type is an invalid value
    let invalid = || IssueReason::new(IssueCategory::InvalidValue, field);
    // a quoted string drops its quotes and unescapes doubled quotes
    let quoted = raw
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
        .map(|inner| inner.replace("''", "'"));
    let text = || quoted.clone().unwrap_or_else(|| raw.to_owned());
    // read the literal as the type of the column's current value
    let value = match like {
        Some(CqlValue::Boolean(_)) => CqlValue::Boolean(raw.parse().map_err(|_| invalid())?),
        Some(CqlValue::TinyInt(_)) => CqlValue::TinyInt(raw.parse().map_err(|_| invalid())?),
        Some(CqlValue::SmallInt(_)) => CqlValue::SmallInt(raw.parse().map_err(|_| invalid())?),
        Some(CqlValue::Int(_)) => CqlValue::Int(raw.parse().map_err(|_| invalid())?),
        Some(CqlValue::BigInt(_)) => CqlValue::BigInt(raw.parse().map_err(|_| invalid())?),
        Some(CqlValue::Float(_)) => CqlValue::Float(raw.parse().map_err(|_| invalid())?),
        Some(CqlValue::Double(_)) => CqlValue::Double(raw.parse().map_err(|_| invalid())?),
        Some(CqlValue::Ascii(_)) => CqlValue::Ascii(text()),
        Some(CqlValue::Text(_)) => CqlValue::Text(text()),
        // a column of another type can't be filled from text
        Some(_) => return Err(invalid()),
        // without a current value an unquoted boolean or number keeps its type
        None => match (&quoted, raw.parse::<bool>(), raw.parse::<i64>()) {
            (Some(text), _, _) => CqlValue::Text(text.clone()),
            (None, Ok(flag), _) => CqlValue::Boolean(flag),
            (None, _, Ok(number)) => CqlValue::BigInt(number),
            (None, _, _) => CqlValue::Text(raw.to_owned()),
        },
    };
    Ok(Some(value))
}

/// A field's path inside a record, such as `settings.theme`; map keys and sequence indices are
/// written `*`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldPath(pub &'static str);

/// What kind of problem an issue is; a fixed list, so a code means the same in every release
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IssueCategory {
    /// A required field is absent; written `missing_field`
    MissingField,
    /// A field the layout doesn't define is present; written `unknown_field`, with the parent path
    UnknownField,
    /// A field holds a value of the wrong type; written `bad_type`
    BadType,
    /// An enum field holds a variant the layout doesn't define; written `unknown_variant`
    UnknownVariant,
    /// A JSON field, or the whole record, doesn't parse; written `malformed_json`
    MalformedJson,
    /// A field parses but breaks a rule of the layout; written `invalid_value`
    InvalidValue,
    /// The record's key disagrees with a key field inside it; written `key_mismatch`
    KeyMismatch,
    /// The record converted but its write was refused; written `write_failed`, with the write
    /// error's kind (`rejected` or `other`) in the field slot; timeouts and unavailable replicas
    /// are retried as process failures, never recorded
    WriteFailed,
}

impl IssueCategory {
    /// Every category, in the order they are declared
    pub const ALL: [IssueCategory; 8] = [
        Self::MissingField,
        Self::UnknownField,
        Self::BadType,
        Self::UnknownVariant,
        Self::MalformedJson,
        Self::InvalidValue,
        Self::KeyMismatch,
        Self::WriteFailed,
    ];

    /// The text this category is written as in a reason code, which never changes once released
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // map each category to its written form
        match self {
            Self::MissingField => "missing_field",
            Self::UnknownField => "unknown_field",
            Self::BadType => "bad_type",
            Self::UnknownVariant => "unknown_variant",
            Self::MalformedJson => "malformed_json",
            Self::InvalidValue => "invalid_value",
            Self::KeyMismatch => "key_mismatch",
            Self::WriteFailed => "write_failed",
        }
    }
}

impl fmt::Display for IssueCategory {
    /// Write this category in its written form
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the fixed text for this category
        f.write_str(self.as_str())
    }
}

impl FromStr for IssueCategory {
    /// Text that names no issue category fails with an [`InvalidEnum`]
    type Err = InvalidEnum;

    /// Parse a category from its written form
    ///
    /// # Arguments
    ///
    /// * `raw` - The written form to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // find the category written this way, rejecting anything else
        Self::ALL
            .into_iter()
            .find(|category| category.as_str() == raw)
            .ok_or_else(|| InvalidEnum(format!("Unknown issue category {raw:?}")))
    }
}

/// Why one object became an issue, rendered `<category>:<field>` or `<category>`; built from
/// serde's error classification and the field path, never from the error's text
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IssueReason {
    /// The kind of problem
    pub category: IssueCategory,
    /// The field's path with map keys and indices as `*`, the parent path for an unknown field,
    /// the write error's kind (`rejected` or `other`) for a refused write, or empty for the whole
    /// record
    pub field: String,
}

impl IssueReason {
    /// Build a reason code
    ///
    /// # Arguments
    ///
    /// * `category` - The kind of problem
    /// * `field` - The field's path, or empty for the whole record
    pub fn new<T: Into<String>>(category: IssueCategory, field: T) -> Self {
        // keep the category and the field as given
        IssueReason {
            category,
            field: field.into(),
        }
    }
}

impl fmt::Display for IssueReason {
    /// Write this reason as `<category>:<field>`, or `<category>` for the whole record
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // a reason about the whole record has no field part
        if self.field.is_empty() {
            f.write_str(self.category.as_str())
        } else {
            write!(f, "{}:{}", self.category, self.field)
        }
    }
}

impl FromStr for IssueReason {
    /// Text whose category is unknown fails with an [`InvalidEnum`]
    type Err = InvalidEnum;

    /// Parse a reason from its `<category>:<field>` or `<category>` form
    ///
    /// # Arguments
    ///
    /// * `raw` - The reason code to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // the category ends at the first colon, and everything after it is the field
        let (category, field) = raw.split_once(':').unwrap_or((raw, ""));
        Ok(IssueReason::new(category.parse()?, field))
    }
}

/// The field path walked while building a reason code from a stored value and a layout type
///
/// It holds only the path so far: a struct field by its declared static name, or `*` for a map
/// key or sequence index, so a reason code never carries stored data or an error's text
#[derive(Debug, Clone, Default)]
pub struct ReasonBuilder {
    /// The path walked so far
    pub path: Vec<PathSegment>,
}

/// One step of a field path as `ReasonBuilder` records it
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    /// A struct field, by its declared static name
    Field(&'static str),
    /// A map key or sequence index, rendered `*` because it is data
    Any,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reason codes render as `<category>:<field>` or `<category>` and parse back
    #[test]
    fn reason_codes_round_trip() {
        // a reason about a field names it after the category
        let field = IssueReason::new(IssueCategory::MissingField, "settings.theme");
        assert_eq!(field.to_string(), "missing_field:settings.theme");
        assert_eq!(
            "missing_field:settings.theme".parse::<IssueReason>().ok(),
            Some(field)
        );
        // a reason about the whole record is the category alone
        let whole = IssueReason::new(IssueCategory::MalformedJson, "");
        assert_eq!(whole.to_string(), "malformed_json");
        assert_eq!("malformed_json".parse::<IssueReason>().ok(), Some(whole));
        // only the first colon splits, so a field may hold more
        let nested = "bad_type:a:b".parse::<IssueReason>().expect("nested");
        assert_eq!(nested.field, "a:b");
        // every category round trips through its written form
        for category in IssueCategory::ALL {
            assert_eq!(
                category.as_str().parse::<IssueCategory>().ok(),
                Some(category)
            );
        }
        // unknown categories are rejected
        assert!("nope:x".parse::<IssueReason>().is_err());
        assert!("".parse::<IssueReason>().is_err());
    }
}
