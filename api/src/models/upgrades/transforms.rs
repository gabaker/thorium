//! Record-based object transforms, the stored records they read and write, and the reason codes
//! an object that can't be converted is recorded under

use std::fmt;
use std::str::FromStr;

use crate::models::InvalidEnum;

#[cfg(feature = "scylla-utils")]
use super::Family;
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
