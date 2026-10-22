//! Value classes, the coverage matrix, and the lists that adjust which classes a column needs
//!
//! A value class is a kind of value a stored column, `Backup` field, or object layout can hold.
//! The classes each location needs follow from its type in the live schema; three lists adjust
//! that for columns (`COLUMN_DOMAINS`, `DERIVED_COLUMNS`, `UNREPRESENTABLE`) and one for `Backup`
//! fields (`FIELD_DOMAINS`), each entry with a reason. The coverage matrix pairs every required
//! cell with the cases the fixture holds, observed from the stores themselves.

use chrono::{DateTime, Datelike, Utc};
use scylla::frame::response::result::{ColumnType, NativeType};
use scylla::serialize::value::SerializeValue;
use scylla::serialize::writers::CellWriter;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write;
use thoradm::backup::coverage::{Registry, Store};

/// The archive rollover size the harness backs up with
pub const TEST_ROLLOVER: usize = 65_536;

/// How long a Large text cell is: longer than the test rollover size
pub const LARGE_CELL: usize = TEST_ROLLOVER + 4_096;

/// How long a Large `Backup` string is
pub const LARGE_FIELD: usize = 65_536;

/// How many elements a Many collection cell holds
pub const MANY_CELL: usize = 1_000;

/// How many elements a Many `Backup` array or map holds
pub const MANY_FIELD: usize = 100;

/// The S3 multipart part size Thorium and thoradm upload with
pub const PART_SIZE: u64 = 5_242_880;

/// The longest S3 key
pub const MAX_S3_KEY: usize = 1_024;

/// Text with 2-, 3-, and 4-byte UTF-8, a combining mark, right-to-left text, and U+0000
pub const UNICODE: &str = "é€😀e\u{301}שלום\u{0}";

/// An argon2id password hash at Thorium's default costs, hashed the way the API hashes a password
///
/// A restore refuses a user whose stored hash isn't argon2id at those costs, so every generated
/// user with a password holds this one; restore checks only its algorithm and costs.
pub const PASSWORD_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$nqN2bDWkjDMrHhX2vCFZBw$hIWzd8Hi7gB7iJMDjoYWnW3ffEJQZD+9baEUSUplOeU";

/// A kind of value a stored column, field, or object layout can hold
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum ValueClass {
    /// The column unset, or a `Backup` field set to null
    Null,
    /// A `Backup` field missing from its record, as data written before the field existed
    Absent,
    /// An empty string, blob, or frozen collection
    Empty,
    /// Zero (and the nil UUID)
    Zero,
    /// Minus one, for signed numbers
    MinusOne,
    /// The type's minimum (the earliest time, for times)
    Min,
    /// The type's maximum (the latest time, and the all-ones UUID)
    Max,
    /// The value either side of a signed conversion of an unsigned field (`i64::MAX`, `i64::MAX + 1`)
    ConversionEdge,
    /// The epoch, for timestamps
    Epoch,
    /// A time before the epoch
    BeforeEpoch,
    /// 29 February
    LeapDay,
    /// Multi-byte and unusual text: 2-, 3-, and 4-byte UTF-8, combining marks, right-to-left, U+0000
    Unicode,
    /// Every byte value from 0 to 255
    AllBytes,
    /// Longer than the test rollover size (cells) or 64 KiB (fields)
    Large,
    /// Just under the largest mutation the test Scylla accepts; required in the `full` matrix only
    NearCellLimit,
    /// A collection with one element
    One,
    /// A collection with many elements
    Many,
    /// An empty JSON object or array in a JSON-valued column or field
    JsonEmpty,
    /// Nested JSON objects and arrays at depth 8
    JsonNested,
    /// One variant of an enum, by its serialized name
    Variant(String),
    /// An object of exactly this many bytes
    Bytes(u64),
    /// An object uploaded in several parts
    Multipart,
    /// A key or map key with Unicode, spaces, `%`, `+`, `:`, or S3's maximum length
    OddKey,
    /// The content type and metadata Thorium sets on an object
    Metadata,
}

impl std::fmt::Display for ValueClass {
    /// Names a class the way the coverage report shows it
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // variants and sizes carry their value; the rest are named as declared
        match self {
            ValueClass::Variant(name) => write!(f, "Variant({name})"),
            ValueClass::Bytes(size) => write!(f, "Bytes({size})"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// One stored location the coverage meta-test requires cases for
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Location {
    /// A column of a backed-up Scylla table; `files[]`, `children{key}`, and `children{value}`
    /// name a collection's elements, keys, and values
    Column {
        /// The table name
        table: String,
        /// The column name, with a collection suffix for its elements
        column: String,
    },
    /// A property path inside the `Backup` schema (`users[].settings.theme`)
    Field {
        /// The path from the root of `Backup`
        path: String,
    },
    /// A backed-up S3 bucket role, narrowed to one prefix layout when partly backed up
    Layout {
        /// The bucket role
        bucket: String,
        /// The prefix layout, or `None` for the whole bucket
        prefix: Option<String>,
    },
}

impl Location {
    /// Builds a column location
    ///
    /// # Arguments
    ///
    /// * `table` - The table name
    /// * `column` - The column name, with any collection suffix
    #[must_use]
    pub fn column(table: &str, column: &str) -> Self {
        // copy the names
        Location::Column {
            table: table.to_owned(),
            column: column.to_owned(),
        }
    }

    /// Returns the registry entry this location belongs to: its table, the `Backup` part it sits
    /// under, or its bucket role named as in `entry_name`
    #[must_use]
    pub fn entry(&self) -> String {
        // map a field to the Redis family its record lives in
        match self {
            Location::Column { table, .. } => table.clone(),
            Location::Field { path } => {
                let part = path
                    .trim_start_matches('.')
                    .split(['.', '[', '{'])
                    .next()
                    .unwrap_or("");
                match part {
                    "users" => "user_data:*".to_owned(),
                    "groups" => "groups_data:*".to_owned(),
                    "images" => "image_data:*".to_owned(),
                    "pipelines" => "pipeline_data:*".to_owned(),
                    "settings" => "system_settings".to_owned(),
                    other => other.to_owned(),
                }
            }
            Location::Layout { bucket, .. } => entry_name(Store::S3, bucket),
        }
    }
}

/// Names a registry entry the way the coverage report and `KNOWN_BROKEN` do: bucket roles carry an
/// `s3:` prefix, since a role can share its name with a table (`results`)
///
/// # Arguments
///
/// * `store` - The entry's store
/// * `name` - The entry's registry name
#[must_use]
pub fn entry_name(store: Store, name: &str) -> String {
    // only bucket roles collide with other stores' names
    match store {
        Store::S3 => format!("s3:{name}"),
        _ => name.to_owned(),
    }
}

impl std::fmt::Display for Location {
    /// Names a location the way the coverage report shows it
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // columns as table.column, fields as their path, layouts as bucket/prefix
        match self {
            Location::Column { table, column } => write!(f, "{table}.{column}"),
            Location::Field { path } => write!(f, "{path}"),
            Location::Layout { bucket, prefix } => match prefix {
                Some(prefix) => write!(f, "{bucket}/{prefix}"),
                None => write!(f, "{bucket}"),
            },
        }
    }
}

/// How a cell is required
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Need {
    /// The fixture must hold a case in every matrix size
    Required,
    /// The fixture must hold a case in the `full` matrix only
    FullOnly,
    /// The value can't be archived; it is written in its own namespace and must be skipped
    Unrepresentable,
}

/// The state of one cell of the coverage matrix
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CellState {
    /// The fixture holds this many cases
    Covered(u32),
    /// The class doesn't apply to the location's type or kind
    NotApplicable,
    /// Required only in the `full` matrix, which this run isn't
    FullOnly,
    /// Listed in `UNREPRESENTABLE`; written in its own namespace and expected to be skipped and recorded
    Unrepresentable,
    /// Required and not covered; fails the meta-test
    Missing,
}

/// Which size of the matrix a run uses, from `THORADM_TEST_MATRIX`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum MatrixSize {
    /// Every class, shape, mode, and golden, with dense shapes capped and near-limit values skipped
    Ci,
    /// Everything, uncapped, with random property seeds
    Full,
}

impl MatrixSize {
    /// Reads the matrix size from `THORADM_TEST_MATRIX`, defaulting to `ci`
    #[must_use]
    pub fn from_env() -> Self {
        // anything but "full" runs the ci matrix
        match std::env::var("THORADM_TEST_MATRIX").as_deref() {
            Ok("full") => MatrixSize::Full,
            _ => MatrixSize::Ci,
        }
    }
}

/// Which locations need which classes, and what the fixture holds
#[derive(Debug, Default, Clone, Serialize)]
pub struct CoverageMatrix {
    /// Every required cell and how it is required
    pub needs: BTreeMap<(Location, ValueClass), Need>,
    /// How many cases of each cell the stores hold, required or not
    pub observed: BTreeMap<(Location, ValueClass), u32>,
}

impl CoverageMatrix {
    /// Requires a cell
    ///
    /// # Arguments
    ///
    /// * `location` - The location
    /// * `class` - The class it must hold
    /// * `need` - How the cell is required
    pub fn require(&mut self, location: Location, class: ValueClass, need: Need) {
        // an unrepresentable marking wins over a plain requirement for the same cell
        let entry = self.needs.entry((location, class)).or_insert(need);
        if need == Need::Unrepresentable {
            *entry = need;
        }
    }

    /// Counts one case of a cell seen in the stores
    ///
    /// # Arguments
    ///
    /// * `location` - The location
    /// * `class` - The class the stored value has
    pub fn observe(&mut self, location: Location, class: ValueClass) {
        // count every case so the report can show how many each cell holds
        *self.observed.entry((location, class)).or_default() += 1;
    }

    /// Returns a required cell's state for a matrix size
    ///
    /// # Arguments
    ///
    /// * `location` - The location
    /// * `class` - The class
    /// * `size` - The matrix size of the run
    #[must_use]
    pub fn state(&self, location: &Location, class: &ValueClass, size: MatrixSize) -> CellState {
        // a cell nobody requires doesn't apply
        let key = (location.clone(), class.clone());
        let Some(need) = self.needs.get(&key) else {
            return CellState::NotApplicable;
        };
        // a case covers any requirement; otherwise the requirement decides
        let count = self.observed.get(&key).copied().unwrap_or(0);
        match (need, count, size) {
            (Need::Unrepresentable, _, _) => CellState::Unrepresentable,
            (_, count, _) if count > 0 => CellState::Covered(count),
            (Need::FullOnly, _, MatrixSize::Ci) => CellState::FullOnly,
            _ => CellState::Missing,
        }
    }

    /// Returns every cell the meta-test fails on
    ///
    /// # Arguments
    ///
    /// * `size` - The matrix size of the run
    #[must_use]
    pub fn missing(&self, size: MatrixSize) -> Vec<(Location, ValueClass)> {
        // keep only the cells that are required and have no case
        self.needs
            .keys()
            .filter(|(location, class)| self.state(location, class, size) == CellState::Missing)
            .cloned()
            .collect()
    }

    /// Adds another matrix's requirements and observations to this one
    ///
    /// # Arguments
    ///
    /// * `other` - The matrix to add
    pub fn merge(&mut self, other: CoverageMatrix) {
        // keep the strongest requirement and sum the observations
        for ((location, class), need) in other.needs {
            self.require(location, class, need);
        }
        for (key, count) in other.observed {
            *self.observed.entry(key).or_default() += count;
        }
    }
}

/// What a column's value names, when that narrows the classes it needs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// A hex digest of this many characters, used to build paths; needs its all-zero and
    /// all-`f` edges
    Hash(usize),
    /// An enum stored as text, named by its guarded list; needs every variant
    Enum(&'static str),
    /// JSON text copied verbatim; needs the JSON classes and the text classes valid for JSON
    Json,
    /// JSON thoradm decodes into a struct (`Resources`); needs that struct's zero and maximum
    TypedJson,
    /// A JSON map of attachment names to object ids; each id names an attachment object
    AttachmentMap,
    /// Result file names, each naming an object in the results bucket
    ObjectNames,
    /// Entity kinds, each mapped to how many entities of the kind a result found and naming the
    /// kind's entity blob in the results bucket; needs one kind, every kind, and the count's edges
    EntityCounts,
    /// The key of a graphic in the graphics bucket, which an entity row's `image` names
    GraphicKey,
}

/// Columns whose values name another object or hold structured text, with the reason
pub const COLUMN_DOMAINS: &[(&str, &str, Domain, &str)] = &[
    (
        "samples_list",
        "origin",
        Domain::Json,
        "the sample's origin, stored as JSON",
    ),
    (
        "s3_ids",
        "type",
        Domain::Enum("S3Objects"),
        "which bucket the object is in",
    ),
    (
        "comments",
        "sha256",
        Domain::Hash(64),
        "builds each attachment's key and backup path",
    ),
    (
        "comments",
        "files",
        Domain::AttachmentMap,
        "attachment names to the object ids that key them",
    ),
    (
        "results",
        "files",
        Domain::ObjectNames,
        "result file names that key result objects",
    ),
    (
        "entities",
        "image",
        Domain::GraphicKey,
        "the key of the entity's graphic in the graphics bucket",
    ),
    (
        "results",
        "entities",
        Domain::EntityCounts,
        "entity kinds that each name an entity blob of the result",
    ),
    (
        "results",
        "display_type",
        Domain::Enum("OutputDisplayType"),
        "an archived enum",
    ),
    (
        "results",
        "tool_version",
        Domain::Json,
        "the tool's version, stored as JSON",
    ),
    (
        "results_stream",
        "display_type",
        Domain::Enum("OutputDisplayType"),
        "an archived enum",
    ),
    (
        "results_stream",
        "tool_version",
        Domain::Json,
        "the tool's version, stored as JSON",
    ),
    ("tags", "type", Domain::Enum("TagType"), "an archived enum"),
    (
        "repos_list",
        "scheme",
        Domain::Json,
        "the repo's URL scheme, stored as a JSON string",
    ),
    (
        "repos_list",
        "default_checkout",
        Domain::Json,
        "the default checkout, stored as JSON",
    ),
    (
        "commitishes",
        "kind",
        Domain::Enum("CommitishKinds"),
        "an archived enum",
    ),
    (
        "commitishes",
        "data",
        Domain::Json,
        "the commitish's details, stored as JSON",
    ),
    (
        "commitish_list",
        "kind",
        Domain::Enum("CommitishKinds"),
        "an archived enum",
    ),
    (
        "network_policies",
        "ingress",
        Domain::Json,
        "the policy's ingress rules, stored as JSON",
    ),
    (
        "network_policies",
        "egress",
        Domain::Json,
        "the policy's egress rules, stored as JSON",
    ),
    (
        "entities",
        "kind",
        Domain::Enum("EntityKinds"),
        "an archived enum",
    ),
    (
        "entities",
        "kind_data",
        Domain::Json,
        "the entity's metadata, stored as JSON",
    ),
    (
        "associations",
        "kind",
        Domain::Enum("AssociationKind"),
        "an archived enum",
    ),
    (
        "associations",
        "direction",
        Domain::Enum("Directionality"),
        "an archived enum",
    ),
    (
        "associations",
        "source",
        Domain::Json,
        "the association's source, stored as JSON",
    ),
    (
        "associations",
        "target",
        Domain::Json,
        "the association's target, stored as JSON",
    ),
    (
        "notifications",
        "kind",
        Domain::Enum("NotificationType"),
        "an archived enum",
    ),
    (
        "notifications",
        "level",
        Domain::Enum("NotificationLevel"),
        "an archived enum",
    ),
    (
        "notifications",
        "key",
        Domain::Json,
        "the notified object's key, stored as JSON",
    ),
    (
        "nodes",
        "health",
        Domain::Enum("NodeHealth"),
        "an archived enum",
    ),
    (
        "nodes",
        "resources",
        Domain::TypedJson,
        "decoded into Resources by the archive struct",
    ),
];

/// Columns computed from another column, covered through their source, with the reason
pub const DERIVED_COLUMNS: &[(&str, &str, &str, &str)] = &[
    ("samples_list", "year", "uploaded", "the upload's year"),
    (
        "samples_list",
        "bucket",
        "uploaded",
        "the upload's time partition",
    ),
    ("tags", "year", "uploaded", "the upload's year"),
    ("tags", "bucket", "uploaded", "the upload's time partition"),
    ("tags", "key_lower", "key", "the key lower-cased"),
    ("tags", "value_lower", "value", "the value lower-cased"),
    ("results_stream", "year", "uploaded", "the upload's year"),
    (
        "results_stream",
        "bucket",
        "uploaded",
        "the upload's time partition",
    ),
    ("repos_list", "year", "uploaded", "the upload's year"),
    (
        "repos_list",
        "bucket",
        "uploaded",
        "the upload's time partition",
    ),
    (
        "commitish_list",
        "year",
        "timestamp",
        "the commit time's year",
    ),
    (
        "commitish_list",
        "bucket",
        "timestamp",
        "the commit time's partition",
    ),
    ("entities", "year", "created", "the creation's year"),
    (
        "entities",
        "bucket",
        "created",
        "the creation's time partition",
    ),
    ("associations", "year", "created", "the creation's year"),
    (
        "associations",
        "bucket",
        "created",
        "the creation's time partition",
    ),
];

/// The regular columns whose format-1 archive struct field isn't optional, so a format-1 backup
/// skips and records a row with one of them unset; format 2 archives such rows like any other
pub const FORMAT_1_REQUIRED: &[(&str, &str)] = &[
    ("associations", "submitter"),
    ("associations", "kind"),
    ("comments", "author"),
    ("comments", "comment"),
    ("comments", "files"),
    ("commitishes", "repo_data"),
    ("commitishes", "timestamp"),
    ("commitishes", "data"),
    ("commitish_list", "repo_data"),
    ("entities", "submitter"),
    ("entities", "kind_data"),
    ("entities", "name"),
    ("network_policies", "egress"),
    ("network_policies", "ingress"),
    ("network_policies", "id"),
    ("network_policies", "created"),
    ("network_policies", "default_policy"),
    ("network_policies", "forced_policy"),
    ("network_policies", "k8s_name"),
    ("nodes", "resources"),
    ("nodes", "health"),
    ("notifications", "msg"),
    ("notifications", "level"),
    ("repos_list", "url"),
    ("repos_list", "user"),
    ("repos_list", "provider"),
    ("repos_list", "creator"),
    ("repos_list", "scheme"),
    ("repos_list", "name"),
    ("results", "display_type"),
    ("results", "tool"),
    ("results", "result"),
    ("results_stream", "tool"),
    ("results_stream", "display_type"),
    ("samples_list", "submitter"),
    ("samples_list", "sha256"),
    ("samples_list", "md5"),
    ("samples_list", "sha1"),
];

/// Regular columns whose archive struct field isn't optional, so a row with the column unset can't
/// be archived; backup must skip and record each such row. Format 2 archives every regular column
/// as optional, so none is listed
pub const UNSET_UNREPRESENTABLE: &[(&str, &str)] = &[];

/// Values a column's CQL type holds but its archive struct can't, with the stored text and the
/// reason; backup must skip and record each one
pub const UNREPRESENTABLE: &[(&str, &str, &str, &str)] = &[
    (
        "notifications",
        "kind",
        "Mystery",
        "a notification kind this thoradm doesn't know",
    ),
    (
        "notifications",
        "level",
        "Mystery",
        "a notification level this thoradm doesn't know",
    ),
    (
        "nodes",
        "health",
        "\"Mystery\"",
        "a node health this thoradm doesn't know",
    ),
    (
        "nodes",
        "resources",
        "{\"cpu\":-1}",
        "a negative count in an unsigned field",
    ),
    (
        "tags",
        "type",
        "Mystery",
        "a tag type this thoradm doesn't know",
    ),
    (
        "s3_ids",
        "type",
        "Mystery",
        "an object type this thoradm doesn't know",
    ),
    (
        "entities",
        "kind",
        "\"Mystery\"",
        "an entity kind this thoradm doesn't know",
    ),
    (
        "associations",
        "kind",
        "Mystery",
        "an association kind this thoradm doesn't know",
    ),
    (
        "associations",
        "direction",
        "Mystery",
        "a direction this thoradm doesn't know",
    ),
    (
        "commitishes",
        "kind",
        "Mystery",
        "a commitish kind this thoradm doesn't know",
    ),
    (
        "commitish_list",
        "kind",
        "Mystery",
        "a commitish kind this thoradm doesn't know",
    ),
    (
        "results",
        "display_type",
        "Mystery",
        "a display type this thoradm doesn't know",
    ),
    (
        "results_stream",
        "display_type",
        "Mystery",
        "a display type this thoradm doesn't know",
    ),
];

/// The column of each table that holds its largest values, which gets a near-cell-limit case;
/// each must lie outside the table's primary key and every view's key, which the live schema
/// decides and the meta-test checks, because Scylla refuses key values over 64 KiB, so a table
/// whose every column keys it or a view (`commitish_list`) has no entry
pub const NEAR_LIMIT_COLUMNS: &[(&str, &str)] = &[
    ("samples_list", "description"),
    ("comments", "comment"),
    ("results", "result"),
    ("results_stream", "cmd"),
    ("repos_list", "name"),
    ("commitishes", "data"),
    ("network_policies", "ingress"),
    ("entities", "kind_data"),
    ("associations", "extra_source"),
    ("notifications", "msg"),
];

/// How a `Backup` field is narrowed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldDomain {
    /// The field names a record and becomes part of Redis keys, so it can't be empty
    Identifier,
    /// A semantic version, which needs its lowest and highest versions instead of free text
    SemVer,
    /// An optional list the API reads back as null when it is empty, so it holds null or a
    /// non-empty list
    NonEmpty,
    /// A stored password hash, which a restore only accepts as argon2id at Thorium's default
    /// costs, so it holds null or `PASSWORD_HASH`
    PasswordHash,
    /// The key of a user's icon in the graphics bucket, which a backup copies by name, so it
    /// holds null or `USER_ICON_KEY`, the one icon the fixture writes
    GraphicKey,
}

/// The key of the icon every fixture user's `image` names, which the fixture writes into the
/// graphics bucket
pub const USER_ICON_KEY: &str = "users/fixture/icon.png";

/// `Backup` fields whose values are limited, with the reason: names of records and lists that
/// can't be empty need no Empty case, versions need version edges, and password hashes hold one
/// valid hash
pub const FIELD_DOMAINS: &[(&str, FieldDomain, &str)] = &[
    (
        ".users[].password",
        FieldDomain::PasswordHash,
        "a restore refuses a hash that isn't argon2id at Thorium's default costs",
    ),
    (
        ".users[].image",
        FieldDomain::GraphicKey,
        "a backup copies the icon the field names, so it must name an object the bucket holds",
    ),
    (
        ".users[].username",
        FieldDomain::Identifier,
        "keys the user's record",
    ),
    (
        ".users[].email",
        FieldDomain::Identifier,
        "keys the email-to-user map",
    ),
    (
        ".users[].token",
        FieldDomain::Identifier,
        "keys the token-to-user map",
    ),
    (
        ".users[].groups[]",
        FieldDomain::Identifier,
        "names a group the user is in",
    ),
    (
        ".users[].actual_groups[]",
        FieldDomain::Identifier,
        "names a group the user is in",
    ),
    (
        ".groups[].name",
        FieldDomain::Identifier,
        "keys the group's record",
    ),
    (
        ".images[].name",
        FieldDomain::Identifier,
        "keys the image's record",
    ),
    (
        ".images[].group",
        FieldDomain::Identifier,
        "keys the image's record",
    ),
    (
        ".images[].used_by[]",
        FieldDomain::Identifier,
        "names a pipeline using the image",
    ),
    (
        ".pipelines[].name",
        FieldDomain::Identifier,
        "keys the pipeline's record",
    ),
    (
        ".pipelines[].group",
        FieldDomain::Identifier,
        "keys the pipeline's record",
    ),
    (
        ".pipelines[].order[][]",
        FieldDomain::Identifier,
        "names an image in the pipeline",
    ),
    (
        ".groups[].owners.combined[]",
        FieldDomain::Identifier,
        "names a member, whose group set is keyed by it",
    ),
    (
        ".groups[].owners.direct[]",
        FieldDomain::Identifier,
        "names a member, whose group set is keyed by it",
    ),
    (
        ".groups[].managers.combined[]",
        FieldDomain::Identifier,
        "names a member, whose group set is keyed by it",
    ),
    (
        ".groups[].managers.direct[]",
        FieldDomain::Identifier,
        "names a member, whose group set is keyed by it",
    ),
    (
        ".groups[].users.combined[]",
        FieldDomain::Identifier,
        "names a member, whose group set is keyed by it",
    ),
    (
        ".groups[].users.direct[]",
        FieldDomain::Identifier,
        "names a member, whose group set is keyed by it",
    ),
    (
        ".groups[].monitors.combined[]",
        FieldDomain::Identifier,
        "names a member, whose group set is keyed by it",
    ),
    (
        ".groups[].monitors.direct[]",
        FieldDomain::Identifier,
        "names a member, whose group set is keyed by it",
    ),
    (
        ".images[].network_policies[]",
        FieldDomain::Identifier,
        "names a network policy, whose used-by set is keyed by it",
    ),
    (
        ".scoped_tokens[].owner",
        FieldDomain::Identifier,
        "keys the owner's scoped tokens",
    ),
    (
        ".scoped_tokens[].name",
        FieldDomain::Identifier,
        "keys the token within its owner's scoped tokens",
    ),
    (
        ".scoped_tokens[].token",
        FieldDomain::Identifier,
        "keys the scoped token map",
    ),
    (
        ".scoped_tokens[].groups[]",
        FieldDomain::Identifier,
        "names a group the token is limited to",
    ),
    (
        ".network_policy_sets[].group",
        FieldDomain::Identifier,
        "keys the group's network-policy set",
    ),
    (
        ".network_policy_sets[].policies[]",
        FieldDomain::Identifier,
        "names a network policy, whose used-by set is keyed by it",
    ),
    (
        ".network_policy_sets[].policies",
        FieldDomain::NonEmpty,
        "a group whose set holds no policy has no set, and the backup leaves it out",
    ),
    (
        ".images[].version<ToolVersion><SemVer>.SemVer",
        FieldDomain::SemVer,
        "a semantic version",
    ),
    (
        ".images[].args.entrypoint",
        FieldDomain::NonEmpty,
        "an empty entrypoint reads back as null",
    ),
    (
        ".images[].args.command",
        FieldDomain::NonEmpty,
        "an empty command reads back as null",
    ),
];

/// `Backup` records the API always stores whole, as one JSON value written from its type, and the
/// parts every backup holds, with the reason; no stored record lacks one of their keys and no
/// backup lacks the parts, so they need no Absent case
pub const WRITTEN_WHOLE: &[(&str, &str)] = &[
    (
        ".scoped_tokens[]",
        "every write serializes the whole scoped token, whose type has had every key since scoped \
         tokens were added",
    ),
    (
        ".workload_runtimes",
        "every backup serializes the whole map, empty when no average was recorded",
    ),
];

/// `Backup` maps whose keys must be UUIDs, which the schema can't say, with the reason; they need
/// no odd-key case
pub const UUID_KEYED_MAPS: &[(&str, &str)] = &[
    (".images[].bans", "bans are keyed by their id"),
    (".pipelines[].bans", "bans are keyed by their id"),
    (
        ".workload_runtimes",
        "runtimes are keyed by their tool instance's id",
    ),
    (
        ".users[].settings.ai<AiSettings>.models.credentials",
        "a user's model credentials are keyed by their id",
    ),
    (
        ".users[].settings.ai<AiSettings>.models.credentials{}.bans",
        "bans are keyed by their id",
    ),
    (
        ".users[].settings.ai<AiSettings>.models.endpoints",
        "a user's model endpoints are keyed by their id",
    ),
    (
        ".users[].settings.ai<AiSettings>.models.endpoints{}.bans",
        "bans are keyed by their id",
    ),
    (
        ".users[].settings.ai<AiSettings>.models.endpoints{}.catalog[].bans",
        "bans are keyed by their id",
    ),
];

/// `Backup` fields a backup never holds, with the reason; they need no classes of their own
pub const UNBACKED_FIELDS: &[(&str, &str)] = &[(
    ".scoped_tokens[].run",
    "a run token belongs to a run whose reaction and jobs are never backed up, so a backup leaves \
     out every scoped token that carries a run scope",
)];

/// `Backup` fields computed from other state when a record is read, with their source; they need
/// no classes of their own and must agree with their source
pub const DERIVED_FIELDS: &[(&str, &str)] = &[
    (
        ".groups[].analysts",
        "every analyst in Thorium, from the analyst set users' roles build",
    ),
    (
        ".users[].actual_groups",
        "filled in when an admin or analyst is read; empty in backups",
    ),
    (
        ".pruned",
        "filled in by a backup that pruned dangling references, listing what it changed; never \
         restored, and thoradm writes it to pruned.jsonl instead of redis.json",
    ),
];

/// A resource kind that doesn't restore yet, left out of whole-namespace oracle checks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownBroken {
    /// The resource kind, as the round-trip function names it
    pub kind: &'static str,
    /// The registry entries the kind covers, named as in `entry_name`
    pub entries: &'static [&'static str],
    /// The title of the finding that blocks it
    pub finding: &'static str,
}

/// The resource kinds that don't restore yet, each with the finding that blocks it
pub const KNOWN_BROKEN: &[KnownBroken] = &[];

/// Returns the domain of a column, if one narrows it
///
/// # Arguments
///
/// * `table` - The table
/// * `column` - The column
#[must_use]
pub fn column_domain(table: &str, column: &str) -> Option<Domain> {
    // look the column up in the domain list
    COLUMN_DOMAINS
        .iter()
        .find(|(t, c, _, _)| *t == table && *c == column)
        .map(|(_, _, domain, _)| *domain)
}

/// Returns the source of a derived column, if the column is derived
///
/// # Arguments
///
/// * `table` - The table
/// * `column` - The column
#[must_use]
pub fn derived_source(table: &str, column: &str) -> Option<&'static str> {
    // look the column up in the derived list
    DERIVED_COLUMNS
        .iter()
        .find(|(t, c, _, _)| *t == table && *c == column)
        .map(|(_, _, source, _)| *source)
}

/// Returns the text a value is stored as in a CQL text column, using the value's own Scylla
/// serialization
///
/// # Arguments
///
/// * `value` - The value to store
///
/// # Panics
///
/// Panics if the value doesn't serialize to text.
#[must_use]
pub fn cql_text<T: SerializeValue>(value: &T) -> String {
    // serialize the value as a text cell; the cell starts with its 4-byte length
    let mut buf = Vec::new();
    value
        .serialize(
            &ColumnType::Native(NativeType::Text),
            CellWriter::new(&mut buf),
        )
        .expect("the value serializes to text");
    String::from_utf8(buf[4..].to_vec()).expect("text cells hold UTF-8")
}

/// Returns whether text has every feature of the Unicode class
///
/// # Arguments
///
/// * `text` - The text to check
#[must_use]
pub fn is_unicode(text: &str) -> bool {
    // 2-, 3-, and 4-byte characters, a combining mark, right-to-left text, and U+0000
    let widths = [2, 3, 4]
        .iter()
        .all(|width| text.chars().any(|c| c.len_utf8() == *width));
    let combining = text.chars().any(|c| ('\u{300}'..='\u{36f}').contains(&c));
    let rtl = text.chars().any(|c| ('\u{590}'..='\u{8ff}').contains(&c));
    widths && combining && rtl && text.contains('\0')
}

/// Returns whether an object name has every Unicode feature an S3 key can hold (all but U+0000)
///
/// # Arguments
///
/// * `name` - The name to check
#[must_use]
pub fn is_unicode_name(name: &str) -> bool {
    // the Unicode class without the NUL S3 refuses in keys
    is_unicode(&format!("{name}\0"))
}

/// Builds Unicode-class text with a tag that keeps it unique
///
/// # Arguments
///
/// * `tag` - Text that makes this value unique
#[must_use]
pub fn unicode(tag: &str) -> String {
    // the class's features first, then the tag
    format!("{UNICODE}{tag}")
}

/// Builds Large-class text of a length with a tag that keeps it unique
///
/// # Arguments
///
/// * `tag` - Text that makes this value unique
/// * `len` - How long the text must be
#[must_use]
pub fn large(tag: &str, len: usize) -> String {
    // the tag first, padded to length with a readable filler
    let mut text = format!("{tag}-");
    while text.len() < len + 1 {
        text.push_str("jupiter ");
    }
    text
}

/// Builds JSON nested eight levels deep, with arrays of objects
#[must_use]
pub fn nested_json() -> serde_json::Value {
    // wrap a leaf in alternating objects and arrays of objects
    let mut value = serde_json::json!({"moon": "Europa", "radius_km": 1560.8});
    for depth in 0..8 {
        value = if depth % 2 == 0 {
            serde_json::json!([value, {"depth": depth}])
        } else {
            serde_json::json!({"level": depth, "inner": value})
        };
    }
    value
}

/// Returns how deeply a JSON value nests
///
/// # Arguments
///
/// * `value` - The value to measure
#[must_use]
pub fn json_depth(value: &serde_json::Value) -> usize {
    // containers add a level to their deepest child
    match value {
        serde_json::Value::Array(items) => 1 + items.iter().map(json_depth).max().unwrap_or(0),
        serde_json::Value::Object(map) => 1 + map.values().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Returns the JSON classes a text value has, if it parses as JSON
///
/// # Arguments
///
/// * `text` - The stored text
#[must_use]
pub fn json_classes(text: &str) -> Vec<ValueClass> {
    // only object and array documents count as JSON-valued
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    match &value {
        serde_json::Value::Object(map) if map.is_empty() => vec![ValueClass::JsonEmpty],
        serde_json::Value::Array(items) if items.is_empty() => vec![ValueClass::JsonEmpty],
        serde_json::Value::Object(_) | serde_json::Value::Array(_) if json_depth(&value) >= 8 => {
            vec![ValueClass::JsonNested]
        }
        _ => Vec::new(),
    }
}

/// Returns the classes of a text value
///
/// # Arguments
///
/// * `text` - The stored text
/// * `large` - How long Large text is
#[must_use]
pub fn text_classes(text: &str, large: usize) -> Vec<ValueClass> {
    // empty, Unicode, and large text are classes of their own
    let mut classes = Vec::new();
    if text.is_empty() {
        classes.push(ValueClass::Empty);
    }
    if is_unicode(text) {
        classes.push(ValueClass::Unicode);
    }
    if text.len() > large {
        classes.push(ValueClass::Large);
    }
    classes
}

/// Returns whether text is an odd key: with spaces, `%`, `+`, `:`, and non-ASCII characters
///
/// # Arguments
///
/// * `key` - The key or name
#[must_use]
pub fn is_odd_key(key: &str) -> bool {
    // every character URL encoding and key separators treat specially, and Unicode
    [' ', '%', '+', ':'].iter().all(|c| key.contains(*c)) && !key.is_ascii()
}

/// Builds an odd key with a tag that keeps it unique
///
/// # Arguments
///
/// * `tag` - Text that makes this key unique
#[must_use]
pub fn odd_key(tag: &str) -> String {
    // every special character at once
    format!("odd key: 100% +{}{tag}", UNICODE.replace('\0', ""))
}

/// The latest time both Scylla and `chrono` hold, in milliseconds
#[must_use]
pub fn max_time_millis() -> i64 {
    // chrono's range is the narrower one
    DateTime::<Utc>::MAX_UTC.timestamp_millis()
}

/// The earliest time both Scylla and `chrono` hold, in milliseconds
#[must_use]
pub fn min_time_millis() -> i64 {
    // chrono's range is the narrower one
    DateTime::<Utc>::MIN_UTC.timestamp_millis()
}

/// Returns the classes of a timestamp
///
/// # Arguments
///
/// * `millis` - The timestamp in milliseconds since the epoch
#[must_use]
pub fn time_classes(millis: i64) -> Vec<ValueClass> {
    // the epoch, times before it, the edges of the range, and leap days
    let mut classes = Vec::new();
    if millis == 0 {
        classes.push(ValueClass::Epoch);
    }
    if millis < 0 {
        classes.push(ValueClass::BeforeEpoch);
    }
    if millis == max_time_millis() {
        classes.push(ValueClass::Max);
    }
    if millis == min_time_millis() {
        classes.push(ValueClass::Min);
    }
    if let Some(time) = DateTime::<Utc>::from_timestamp_millis(millis)
        && time.month() == 2
        && time.day() == 29
    {
        classes.push(ValueClass::LeapDay);
    }
    classes
}

/// Returns the classes of an integer of a width
///
/// # Arguments
///
/// * `value` - The integer
/// * `min` - The type's minimum
/// * `max` - The type's maximum
#[must_use]
pub fn int_classes(value: i128, min: i128, max: i128) -> Vec<ValueClass> {
    // zero, minus one, and the type's edges
    let mut classes = Vec::new();
    if value == 0 {
        classes.push(ValueClass::Zero);
    }
    if value == -1 {
        classes.push(ValueClass::MinusOne);
    }
    if value == min {
        classes.push(ValueClass::Min);
    }
    if value == max {
        classes.push(ValueClass::Max);
    }
    classes
}

/// Returns the classes of a UUID
///
/// # Arguments
///
/// * `value` - The UUID
#[must_use]
pub fn uuid_classes(value: uuid::Uuid) -> Vec<ValueClass> {
    // the nil UUID and the all-ones UUID are the edges
    match value.as_u128() {
        0 => vec![ValueClass::Zero],
        u128::MAX => vec![ValueClass::Max],
        _ => Vec::new(),
    }
}

/// Renders the coverage matrix as Markdown: one section per backed-up registry entry, with a
/// row per location and a column per value class it needs
///
/// # Arguments
///
/// * `matrix` - The matrix to render
/// * `size` - The matrix size of the run
#[must_use]
pub fn render_markdown(matrix: &CoverageMatrix, size: MatrixSize) -> String {
    // group the required cells by registry entry and location
    let mut entries: BTreeMap<String, BTreeMap<Location, Vec<ValueClass>>> = BTreeMap::new();
    for (location, class) in matrix.needs.keys() {
        entries
            .entry(location.entry())
            .or_default()
            .entry(location.clone())
            .or_default()
            .push(class.clone());
    }
    // open the report with its legend and totals
    let missing = matrix.missing(size).len();
    let mut out = format!(
        "# Coverage matrix\n\nMatrix size: {size:?}. Required cells: {}. Missing: {missing}.\n\n\
         Each cell shows its case count, `n/a`, `full only`, `unrepresentable`, or **MISSING**.\n\n",
        matrix.needs.len()
    );
    // one section per entry, with its known-broken finding when it doesn't restore yet
    for (entry, locations) in &entries {
        let classes: Vec<ValueClass> = {
            let mut all: Vec<ValueClass> = locations.values().flatten().cloned().collect();
            all.sort();
            all.dedup();
            all
        };
        let covered = locations
            .iter()
            .flat_map(|(location, needed)| needed.iter().map(move |class| (location, class)))
            .filter(|(location, class)| {
                matches!(matrix.state(location, class, size), CellState::Covered(_))
            })
            .count();
        let required: usize = locations.values().map(Vec::len).sum();
        let _ = writeln!(
            out,
            "<details><summary><code>{entry}</code>: {covered} of {required} cells covered</summary>\n"
        );
        if let Some(finding) = known_broken(entry) {
            let _ = writeln!(out, "Doesn't restore yet: {finding}\n");
        }
        let header: Vec<String> = classes.iter().map(ToString::to_string).collect();
        let _ = writeln!(out, "| Location | {} |", header.join(" | "));
        let _ = writeln!(out, "|---|{}|", vec!["---"; classes.len()].join("|"));
        for (location, needed) in locations {
            let cells: Vec<String> = classes
                .iter()
                .map(|class| {
                    if !needed.contains(class) {
                        return "n/a".to_owned();
                    }
                    match matrix.state(location, class, size) {
                        CellState::Covered(count) => count.to_string(),
                        CellState::NotApplicable => "n/a".to_owned(),
                        CellState::FullOnly => "full only".to_owned(),
                        CellState::Unrepresentable => "unrepresentable".to_owned(),
                        CellState::Missing => "**MISSING**".to_owned(),
                    }
                })
                .collect();
            let _ = writeln!(out, "| `{location}` | {} |", cells.join(" | "));
        }
        let _ = writeln!(out, "\n</details>\n");
    }
    // backed-up entries whose values are cells of another entry's fields, columns, or layouts
    let others: Vec<(String, &str)> = backed_up_entries()
        .into_iter()
        .filter(|(name, _)| !entries.contains_key(name))
        .collect();
    if !others.is_empty() {
        let _ = writeln!(out, "## Backed-up entries without cells of their own\n");
        let _ = writeln!(
            out,
            "Their values are counted as the cells of the entry they are read with.\n"
        );
        for (name, store) in others {
            let broken = known_broken(&name).map_or_else(String::new, |finding| {
                format!(": doesn't restore yet: {finding}")
            });
            let _ = writeln!(out, "- `{name}` ({store}){broken}");
        }
    }
    out
}

/// Returns every backed-up registry entry, named as in `entry_name`, with its store's label
#[must_use]
pub fn backed_up_entries() -> Vec<(String, &'static str)> {
    // every store's backed-up entries, in registry order
    [
        Store::ScyllaTable,
        Store::ScyllaView,
        Store::Redis,
        Store::S3,
    ]
    .into_iter()
    .flat_map(|store| {
        Registry::backed_up(store).map(move |entry| (entry_name(store, entry.name), store.label()))
    })
    .collect()
}

/// Returns the finding blocking an entry's restore, when it is known broken
///
/// # Arguments
///
/// * `entry` - The registry entry, named as in `entry_name`
#[must_use]
pub fn known_broken(entry: &str) -> Option<&'static str> {
    // look the entry up in the known-broken kinds
    KNOWN_BROKEN
        .iter()
        .find(|broken| broken.entries.contains(&entry))
        .map(|broken| broken.finding)
}

/// One cell of the coverage matrix in the JSON report
#[derive(Debug, Serialize)]
struct ReportCell {
    /// The registry entry the location belongs to
    entry: String,
    /// The location
    location: String,
    /// The value class
    class: String,
    /// The cell's state
    state: CellState,
    /// The finding blocking the entry's restore, when it is known broken
    known_broken: Option<&'static str>,
}

/// Renders the coverage matrix as JSON, one object per required cell
///
/// # Arguments
///
/// * `matrix` - The matrix to render
/// * `size` - The matrix size of the run
#[must_use]
pub fn render_json(matrix: &CoverageMatrix, size: MatrixSize) -> String {
    // describe every required cell with its entry and state
    let cells: Vec<ReportCell> = matrix
        .needs
        .keys()
        .map(|(location, class)| {
            let entry = location.entry();
            ReportCell {
                known_broken: known_broken(&entry),
                entry,
                location: location.to_string(),
                class: class.to_string(),
                state: matrix.state(location, class, size),
            }
        })
        .collect();
    // and every backed-up entry, including those without cells of their own
    let entries: Vec<serde_json::Value> = backed_up_entries()
        .into_iter()
        .map(|(name, store)| {
            let count = cells.iter().filter(|cell| cell.entry == name).count();
            serde_json::json!({"entry": name, "store": store, "cells": count, "known_broken": known_broken(&name)})
        })
        .collect();
    serde_json::to_string_pretty(
        &serde_json::json!({"size": size, "entries": entries, "cells": cells}),
    )
    .expect("the report serializes")
}

/// Writes the coverage report as Markdown and JSON into a directory
///
/// # Arguments
///
/// * `matrix` - The matrix to write
/// * `size` - The matrix size of the run
/// * `dir` - The report directory
pub fn write_report(
    matrix: &CoverageMatrix,
    size: MatrixSize,
    dir: &std::path::Path,
) -> std::io::Result<()> {
    // write both forms side by side
    std::fs::create_dir_all(dir)?;
    std::fs::write(
        dir.join("coverage-matrix.md"),
        render_markdown(matrix, size),
    )?;
    std::fs::write(dir.join("coverage-matrix.json"), render_json(matrix, size))
}

/// Returns the directory harness runs write reports into (`reports/` at the workspace root, or
/// `THORADM_REPORT_DIR`)
#[must_use]
pub fn report_dir() -> std::path::PathBuf {
    // let CI or a lane pick its own directory
    std::env::var("THORADM_REPORT_DIR").map_or_else(
        |_| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("reports")
        },
        std::path::PathBuf::from,
    )
}
