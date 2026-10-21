//! The columns of the backed-up tables: which classes each needs, rows that hold them, and what
//! the stored rows actually hold
//!
//! Column names, kinds, and CQL types come from the live `system_schema.columns`, so a column
//! added to a backed-up table needs fixture cases as soon as it exists. Rows are written through
//! CQL with values the API couldn't produce, each row changing one column from an ordinary
//! baseline; derived columns are recomputed the way restore recomputes them, and every object a
//! row names is written to its bucket.

use super::classes::{
    COLUMN_DOMAINS, CoverageMatrix, DERIVED_COLUMNS, Domain, LARGE_CELL, Location, MANY_CELL,
    MatrixSize, NEAR_LIMIT_COLUMNS, Need, UNREPRESENTABLE, UNSET_UNREPRESENTABLE, ValueClass,
    column_domain, cql_text, derived_source, int_classes, is_odd_key, is_unicode, is_unicode_name,
    json_classes, large, max_time_millis, nested_json, odd_key, text_classes, time_classes,
    unicode, uuid_classes,
};
use super::enums::{self, EnumList};
use chrono::{DateTime, Datelike, Utc};
use futures::StreamExt;
use scylla::client::session::Session;
use scylla::value::{CqlTimestamp, CqlValue, Row};
use std::collections::BTreeMap;
use thoradm::backup::tables::entity_kind;
use thorium::Conf;
use thorium::models::TagType;

/// How long a near-limit name in the results bucket is, keeping its key under S3's limit
pub const LONG_NAME: usize = 950;

/// Builds a result file name near S3's key limit, nested in directories so no path segment
/// passes the 255 bytes file systems and S3 services allow per segment
///
/// # Arguments
///
/// * `tag` - Text that makes this name unique
#[must_use]
pub fn long_name(tag: &str) -> String {
    // nest directories of at most 200 bytes until the name reaches its length
    let mut name = format!("{tag}-");
    while name.len() + "end.bin".len() < LONG_NAME {
        let room = LONG_NAME - name.len() - "end.bin".len();
        name.push_str(&"n".repeat(room.min(199).saturating_sub(1)));
        name.push('/');
    }
    name.push_str("end.bin");
    name
}

/// The kind of a column in its table
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Part of the partition key
    Partition,
    /// A clustering column
    Clustering,
    /// A regular column
    Regular,
    /// A static column
    Static,
}

/// A CQL type, as `system_schema.columns` names it
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CqlType {
    /// `text` or `ascii`
    Text,
    /// `tinyint`
    TinyInt,
    /// `smallint`
    SmallInt,
    /// `int`
    Int,
    /// `bigint`
    BigInt,
    /// `timestamp`
    Timestamp,
    /// `uuid`
    Uuid,
    /// `timeuuid`
    TimeUuid,
    /// `boolean`
    Boolean,
    /// A non-frozen `set`
    Set(Box<CqlType>),
    /// A non-frozen `list`
    List(Box<CqlType>),
    /// A non-frozen `map`
    Map(Box<CqlType>, Box<CqlType>),
    /// Any type this harness doesn't generate yet, by name
    Other(String),
}

/// Parses a CQL type name from `system_schema.columns`
///
/// # Arguments
///
/// * `name` - The type's name (`map<text, uuid>`)
#[must_use]
pub fn parse_type(name: &str) -> CqlType {
    // collections name their element types inside angle brackets
    let name = name.trim();
    if let Some(inner) = name
        .strip_prefix("set<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        return CqlType::Set(Box::new(parse_type(inner)));
    }
    if let Some(inner) = name
        .strip_prefix("list<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        return CqlType::List(Box::new(parse_type(inner)));
    }
    if let Some(inner) = name
        .strip_prefix("map<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        let (key, value) = inner.split_once(',').unwrap_or((inner, ""));
        return CqlType::Map(Box::new(parse_type(key)), Box::new(parse_type(value)));
    }
    match name {
        "text" | "ascii" | "varchar" => CqlType::Text,
        "tinyint" => CqlType::TinyInt,
        "smallint" => CqlType::SmallInt,
        "int" => CqlType::Int,
        "bigint" => CqlType::BigInt,
        "timestamp" => CqlType::Timestamp,
        "uuid" => CqlType::Uuid,
        "timeuuid" => CqlType::TimeUuid,
        "boolean" => CqlType::Boolean,
        other => CqlType::Other(other.to_owned()),
    }
}

/// One column of a table
#[derive(Debug, Clone)]
pub struct Column {
    /// The column's name
    pub name: String,
    /// The column's kind
    pub kind: Kind,
    /// The column's type
    pub cql: CqlType,
    /// Whether some materialized view of the table keys its rows by this column
    pub view_key: bool,
}

impl Column {
    /// Returns whether the column is part of the primary key
    #[must_use]
    pub fn is_key(&self) -> bool {
        // partition and clustering columns form the key
        matches!(self.kind, Kind::Partition | Kind::Clustering)
    }

    /// Returns whether the column's values are limited to a key's 64 KiB, in the table or a view
    #[must_use]
    pub fn is_key_sized(&self) -> bool {
        // a view's key columns carry the same limit as the table's
        self.is_key() || self.view_key
    }
}

/// One table's live columns
#[derive(Debug, Clone)]
pub struct Table {
    /// The table's name
    pub name: String,
    /// Its columns, in schema order
    pub columns: Vec<Column>,
}

/// Reads a table's columns from the live schema
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `keyspace` - The keyspace
/// * `table` - The table
///
/// # Panics
///
/// Panics if the schema can't be read.
pub async fn live_table(scylla: &Session, keyspace: &str, table: &str) -> Table {
    // read every column's name, kind, and type
    let rows = scylla
        .query_unpaged(
            "SELECT column_name, kind, type FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
            (keyspace, table),
        )
        .await
        .expect("failed to read the schema")
        .into_rows_result()
        .expect("the schema has rows");
    // find the columns any view of this table keys its rows by
    let views = scylla
        .query_unpaged(
            "SELECT view_name, base_table_name FROM system_schema.views WHERE keyspace_name = ?",
            (keyspace,),
        )
        .await
        .expect("failed to read the views")
        .into_rows_result()
        .expect("the views have rows")
        .rows::<(String, String)>()
        .expect("the view rows decode")
        .filter_map(Result::ok)
        .filter(|(_, base)| base == table)
        .map(|(view, _)| view)
        .collect::<Vec<_>>();
    let mut view_keys = std::collections::BTreeSet::new();
    for view in views {
        let keys = scylla
            .query_unpaged(
                "SELECT column_name, kind FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
                (keyspace, view.as_str()),
            )
            .await
            .expect("failed to read a view's columns")
            .into_rows_result()
            .expect("the view's columns have rows")
            .rows::<(String, String)>()
            .expect("the view's columns decode")
            .filter_map(Result::ok)
            .filter(|(_, kind)| kind == "partition_key" || kind == "clustering")
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        view_keys.extend(keys);
    }
    let mut columns: Vec<Column> = rows
        .rows::<(String, String, String)>()
        .expect("the schema rows decode")
        .map(|row| {
            let (name, kind, cql) = row.expect("a schema row decodes");
            let kind = match kind.as_str() {
                "partition_key" => Kind::Partition,
                "clustering" => Kind::Clustering,
                "static" => Kind::Static,
                _ => Kind::Regular,
            };
            Column {
                view_key: view_keys.contains(&name),
                name,
                kind,
                cql: parse_type(&cql),
            }
        })
        .collect();
    columns.sort_by(|a, b| a.name.cmp(&b.name));
    Table {
        name: table.to_owned(),
        columns,
    }
}

/// Which part of a column a location names
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The whole cell
    Cell,
    /// A set's or list's elements
    Element,
    /// A map's keys
    Key,
    /// A map's values
    Value,
}

/// Splits a location's column into the column name and the part it names
///
/// # Arguments
///
/// * `column` - The location's column (`files[]`, `children{key}`)
#[must_use]
pub fn split_part(column: &str) -> (&str, Part) {
    // the suffix names the part
    if let Some(name) = column.strip_suffix("[]") {
        (name, Part::Element)
    } else if let Some(name) = column.strip_suffix("{key}") {
        (name, Part::Key)
    } else if let Some(name) = column.strip_suffix("{value}") {
        (name, Part::Value)
    } else {
        (column, Part::Cell)
    }
}

/// Returns the classes a scalar of a type needs, given its key kind
///
/// # Arguments
///
/// * `cql` - The scalar's type
/// * `kind` - The column's kind, which limits text in keys
/// * `key_sized` - Whether the column is limited to a key's size, in the table or a view
/// * `map_key` - Whether the scalar is a map key
fn scalar_classes(cql: &CqlType, kind: Kind, key_sized: bool, map_key: bool) -> Vec<ValueClass> {
    // each type needs the edges of its range
    match cql {
        CqlType::Text if map_key => {
            vec![ValueClass::Empty, ValueClass::Unicode, ValueClass::OddKey]
        }
        CqlType::Text => match (kind, key_sized) {
            (Kind::Partition, _) => vec![ValueClass::Unicode],
            (_, true) => vec![ValueClass::Empty, ValueClass::Unicode],
            (Kind::Clustering | Kind::Regular | Kind::Static, false) => {
                vec![ValueClass::Empty, ValueClass::Unicode, ValueClass::Large]
            }
        },
        CqlType::TinyInt | CqlType::SmallInt | CqlType::Int | CqlType::BigInt => vec![
            ValueClass::Zero,
            ValueClass::MinusOne,
            ValueClass::Min,
            ValueClass::Max,
        ],
        CqlType::Timestamp => vec![
            ValueClass::Epoch,
            ValueClass::BeforeEpoch,
            ValueClass::Max,
            ValueClass::LeapDay,
        ],
        CqlType::Uuid => vec![ValueClass::Zero, ValueClass::Max],
        CqlType::TimeUuid => vec![ValueClass::Min, ValueClass::Max],
        CqlType::Boolean => vec![
            ValueClass::Variant("true".to_owned()),
            ValueClass::Variant("false".to_owned()),
        ],
        CqlType::Set(_) | CqlType::List(_) | CqlType::Map(..) | CqlType::Other(_) => Vec::new(),
    }
}

/// Returns the variant names an enum domain needs
///
/// # Arguments
///
/// * `lists` - The guarded enum lists
/// * `name` - The enum's name
fn enum_variants<'a>(lists: &'a [EnumList], name: &str) -> &'a EnumList {
    // every enum domain names a guarded list
    lists
        .iter()
        .find(|list| list.name == name)
        .unwrap_or_else(|| panic!("no guarded list for the enum {name}"))
}

/// Adds the cells an entity-count map's keys and counts need to a matrix: every entity kind as a
/// key, and the edges of a count
///
/// # Arguments
///
/// * `lists` - The guarded enum lists
/// * `keys` - The location of the map's keys
/// * `counts` - The location of the map's counts
/// * `matrix` - The matrix to add cells to
fn require_entity_counts(
    lists: &[EnumList],
    keys: &Location,
    counts: &Location,
    matrix: &mut CoverageMatrix,
) {
    // each kind names its own entity blob, so every kind is a key
    for name in &enum_variants(lists, "EntityKinds").names {
        matrix.require(
            keys.clone(),
            ValueClass::Variant(name.clone()),
            Need::Required,
        );
    }
    // a count is a bigint with its edges
    for class in scalar_classes(&CqlType::BigInt, Kind::Regular, false, false) {
        matrix.require(counts.clone(), class, Need::Required);
    }
}

/// Adds every cell a table's columns need to a matrix
///
/// # Arguments
///
/// * `table` - The table's live columns
/// * `lists` - The guarded enum lists
/// * `size` - The matrix size of the run
/// * `matrix` - The matrix to add cells to
pub fn require(table: &Table, lists: &[EnumList], size: MatrixSize, matrix: &mut CoverageMatrix) {
    // derived columns are covered through their source
    for column in table
        .columns
        .iter()
        .filter(|column| derived_source(&table.name, &column.name).is_none())
    {
        let at = |suffix: &str| Location::column(&table.name, &format!("{}{suffix}", column.name));
        // a regular or static column can be unset
        if !column.is_key() {
            matrix.require(at(""), ValueClass::Null, Need::Required);
        }
        // a domain replaces the classes the type alone would need
        let classes = match column_domain(&table.name, &column.name) {
            // an enum needs each variant, and a hash its edges
            Some(Domain::Enum(name)) => enum_variants(lists, name)
                .names
                .iter()
                .map(|name| ValueClass::Variant(name.clone()))
                .collect(),
            Some(Domain::Hash(_)) => vec![ValueClass::Min, ValueClass::Max],
            // free JSON needs its shapes, and a large value unless it keys a view
            Some(Domain::Json) => {
                let mut classes = vec![
                    ValueClass::JsonEmpty,
                    ValueClass::JsonNested,
                    ValueClass::Unicode,
                ];
                if !column.is_key_sized() {
                    classes.push(ValueClass::Large);
                }
                classes
            }
            // typed JSON gets its edges from the schema; an attachment map needs its sizes
            Some(Domain::TypedJson) => vec![ValueClass::Zero, ValueClass::Max],
            Some(Domain::AttachmentMap) => vec![
                ValueClass::JsonEmpty,
                ValueClass::One,
                ValueClass::Many,
                ValueClass::OddKey,
            ],
            // entity counts need every kind as a key, the count's edges, and one kind
            Some(Domain::EntityCounts) => {
                require_entity_counts(lists, &at("{key}"), &at("{value}"), matrix);
                vec![ValueClass::One]
            }
            // object names need odd and long names inside, and one or many of them
            Some(Domain::ObjectNames) => {
                for class in [ValueClass::Unicode, ValueClass::OddKey, ValueClass::Large] {
                    matrix.require(at("[]"), class, Need::Required);
                }
                vec![ValueClass::One, ValueClass::Many]
            }
            // without a domain the CQL type decides
            None => match &column.cql {
                CqlType::Set(item) | CqlType::List(item) => {
                    for class in scalar_classes(item, Kind::Regular, true, false) {
                        matrix.require(at("[]"), class, Need::Required);
                    }
                    vec![ValueClass::One, ValueClass::Many]
                }
                CqlType::Map(key, value) => {
                    for class in scalar_classes(key, Kind::Regular, true, true) {
                        matrix.require(at("{key}"), class, Need::Required);
                    }
                    for class in scalar_classes(value, Kind::Regular, false, false) {
                        matrix.require(at("{value}"), class, Need::Required);
                    }
                    vec![ValueClass::One, ValueClass::Many]
                }
                CqlType::Other(name) => panic!(
                    "{}.{} has a type the fixture can't generate: {name}",
                    table.name, column.name
                ),
                cql => scalar_classes(cql, column.kind, column.is_key_sized(), false),
            },
        };
        for class in classes {
            matrix.require(at(""), class, Need::Required);
        }
        // the table's largest column gets a value near the cell limit in the full matrix, unless
        // it keys the table or a view, where Scylla refuses values over 64 KiB
        if NEAR_LIMIT_COLUMNS.contains(&(table.name.as_str(), column.name.as_str()))
            && !column.is_key_sized()
        {
            let need = if size == MatrixSize::Full {
                Need::Required
            } else {
                Need::FullOnly
            };
            matrix.require(at(""), ValueClass::NearCellLimit, need);
        }
    }
    // values the archive struct can't hold are written in their own namespace
    for (_, column) in UNSET_UNREPRESENTABLE
        .iter()
        .filter(|(t, _)| *t == table.name)
    {
        matrix.require(
            Location::column(&table.name, column),
            ValueClass::Null,
            Need::Unrepresentable,
        );
    }
    for (_, column, text, _) in UNREPRESENTABLE.iter().filter(|(t, ..)| *t == table.name) {
        matrix.require(
            Location::column(&table.name, column),
            ValueClass::Variant(format!("unrepresentable {text}")),
            Need::Unrepresentable,
        );
    }
}

/// One planned row, and the objects it names
#[derive(Debug, Clone)]
pub struct RowPlan {
    /// The table the row goes in
    pub table: String,
    /// The row's values by column; a missing column stays unset
    pub values: BTreeMap<String, CqlValue>,
    /// The body every object the row names gets, when the row exists for an object layout
    pub body: Option<Body>,
}

/// What an object holds
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// These bytes, uploaded in one request
    Bytes(Vec<u8>),
    /// This many bytes, uploaded in parts of the part size
    Multipart(u64),
}

/// Plans rows that hold every class a table's columns need
pub struct Planner<'a> {
    /// The config of the namespace, for partition sizes and buckets
    pub conf: &'a Conf,
    /// The guarded enum lists, for enum domains
    pub lists: &'a [EnumList],
    /// The matrix size of the run
    pub size: MatrixSize,
    /// How long a near-cell-limit value is
    pub near_limit: usize,
    /// A counter that keeps generated keys unique
    counter: usize,
}

impl<'a> Planner<'a> {
    /// Starts a planner
    ///
    /// # Arguments
    ///
    /// * `conf` - The config of the namespace
    /// * `lists` - The guarded enum lists
    /// * `size` - The matrix size of the run
    /// * `near_limit` - How long a near-cell-limit value is
    #[must_use]
    pub fn new(conf: &'a Conf, lists: &'a [EnumList], size: MatrixSize, near_limit: usize) -> Self {
        // start the counter at zero
        Planner {
            conf,
            lists,
            size,
            near_limit,
            counter: 0,
        }
    }

    /// Returns a unique tag for a generated value
    fn tag(&mut self) -> String {
        // count up so every value differs
        self.counter += 1;
        format!("{:05}", self.counter)
    }

    /// Returns the CQL text of an enum variant by its serialized name
    ///
    /// # Arguments
    ///
    /// * `list` - The enum's name
    /// * `name` - The variant's name
    fn enum_text(&self, list: &str, name: &str) -> String {
        // find the variant's stored text in its list
        let list = enum_variants(self.lists, list);
        let index = list
            .names
            .iter()
            .position(|listed| listed == name)
            .unwrap_or(0);
        list.cql
            .as_ref()
            .expect("archived enums carry their stored text")[index]
            .clone()
    }

    /// Builds an ordinary value for a column
    ///
    /// # Arguments
    ///
    /// * `table` - The table
    /// * `column` - The column
    fn baseline(&mut self, table: &str, column: &Column) -> Option<CqlValue> {
        // a domain decides the ordinary value first
        let tag = self.tag();
        match column_domain(table, &column.name) {
            Some(Domain::Enum(list)) => {
                let first = enum_variants(self.lists, list).names[0].clone();
                return Some(CqlValue::Text(self.enum_text(list, &first)));
            }
            Some(Domain::Hash(len)) => {
                let digest = format!("{:x}", uuid::Uuid::new_v4().as_u128());
                return Some(CqlValue::Text(
                    digest.repeat(len.div_ceil(digest.len()))[..len].to_owned(),
                ));
            }
            Some(Domain::Json) => {
                return Some(CqlValue::Text(format!("{{\"fixture\":\"{tag}\"}}")));
            }
            Some(Domain::TypedJson) => {
                return Some(CqlValue::Text(cql_text(
                    &thorium::models::Resources::default(),
                )));
            }
            Some(Domain::AttachmentMap) => return Some(CqlValue::Text("{}".to_owned())),
            Some(Domain::ObjectNames | Domain::EntityCounts) => return None,
            None => (),
        }
        // otherwise the type decides it
        match &column.cql {
            CqlType::Text => Some(CqlValue::Text(format!("{}-{tag}", column.name))),
            CqlType::TinyInt => Some(CqlValue::TinyInt(1)),
            CqlType::SmallInt => Some(CqlValue::SmallInt(1)),
            CqlType::Int => Some(CqlValue::Int(1)),
            CqlType::BigInt => Some(CqlValue::BigInt(1)),
            CqlType::Timestamp => Some(CqlValue::Timestamp(CqlTimestamp(1_750_000_000_000))),
            CqlType::Uuid | CqlType::TimeUuid => Some(CqlValue::Uuid(uuid::Uuid::new_v4())),
            CqlType::Boolean => Some(CqlValue::Boolean(false)),
            CqlType::Set(_) | CqlType::List(_) | CqlType::Map(..) | CqlType::Other(_) => None,
        }
    }

    /// Builds a scalar value of a class
    ///
    /// # Arguments
    ///
    /// * `cql` - The scalar's type
    /// * `class` - The class to build
    fn scalar(&mut self, cql: &CqlType, class: &ValueClass) -> Option<CqlValue> {
        // each type and class pair has one canonical value
        let tag = self.tag();
        let value = match (cql, class) {
            (CqlType::Text, ValueClass::Empty) => CqlValue::Text(String::new()),
            (CqlType::Text, ValueClass::Unicode) => CqlValue::Text(unicode(&tag)),
            (CqlType::Text, ValueClass::Large) => CqlValue::Text(large(&tag, LARGE_CELL)),
            (CqlType::Text, ValueClass::NearCellLimit) => {
                CqlValue::Text(large(&tag, self.near_limit))
            }
            (CqlType::Text, ValueClass::OddKey) => CqlValue::Text(odd_key(&tag)),
            (CqlType::TinyInt, ValueClass::Zero) => CqlValue::TinyInt(0),
            (CqlType::TinyInt, ValueClass::MinusOne) => CqlValue::TinyInt(-1),
            (CqlType::TinyInt, ValueClass::Min) => CqlValue::TinyInt(i8::MIN),
            (CqlType::TinyInt, ValueClass::Max) => CqlValue::TinyInt(i8::MAX),
            (CqlType::SmallInt, ValueClass::Zero) => CqlValue::SmallInt(0),
            (CqlType::SmallInt, ValueClass::MinusOne) => CqlValue::SmallInt(-1),
            (CqlType::SmallInt, ValueClass::Min) => CqlValue::SmallInt(i16::MIN),
            (CqlType::SmallInt, ValueClass::Max) => CqlValue::SmallInt(i16::MAX),
            (CqlType::Int, ValueClass::Zero) => CqlValue::Int(0),
            (CqlType::Int, ValueClass::MinusOne) => CqlValue::Int(-1),
            (CqlType::Int, ValueClass::Min) => CqlValue::Int(i32::MIN),
            (CqlType::Int, ValueClass::Max) => CqlValue::Int(i32::MAX),
            (CqlType::BigInt, ValueClass::Zero) => CqlValue::BigInt(0),
            (CqlType::BigInt, ValueClass::MinusOne) => CqlValue::BigInt(-1),
            (CqlType::BigInt, ValueClass::Min) => CqlValue::BigInt(i64::MIN),
            (CqlType::BigInt, ValueClass::Max) => CqlValue::BigInt(i64::MAX),
            (CqlType::Timestamp, ValueClass::Epoch) => CqlValue::Timestamp(CqlTimestamp(0)),
            (CqlType::Timestamp, ValueClass::BeforeEpoch) => {
                CqlValue::Timestamp(CqlTimestamp(-86_400_000))
            }
            (CqlType::Timestamp, ValueClass::Max) => {
                CqlValue::Timestamp(CqlTimestamp(max_time_millis()))
            }
            (CqlType::Timestamp, ValueClass::LeapDay) => {
                CqlValue::Timestamp(CqlTimestamp(1_709_208_000_000))
            }
            (CqlType::Uuid, ValueClass::Zero) => CqlValue::Uuid(uuid::Uuid::nil()),
            (CqlType::Uuid, ValueClass::Max) => CqlValue::Uuid(uuid::Uuid::max()),
            (CqlType::Boolean, ValueClass::Variant(flag)) => CqlValue::Boolean(flag == "true"),
            _ => return None,
        };
        Some(value)
    }

    /// Builds a domain value of a class
    ///
    /// # Arguments
    ///
    /// * `domain` - The column's domain
    /// * `class` - The class to build
    fn domain_value(&mut self, domain: Domain, class: &ValueClass) -> Option<CqlValue> {
        // each domain builds its own edges
        let tag = self.tag();
        let text = match (domain, class) {
            (Domain::Enum(list), ValueClass::Variant(name)) => self.enum_text(list, name),
            (Domain::Hash(len), ValueClass::Min) => "0".repeat(len),
            (Domain::Hash(len), ValueClass::Max) => "f".repeat(len),
            (Domain::Json | Domain::AttachmentMap, ValueClass::JsonEmpty) => "{}".to_owned(),
            (Domain::Json, ValueClass::JsonNested) => nested_json().to_string(),
            (Domain::Json, ValueClass::Unicode) => {
                serde_json::json!({"text": unicode(&tag)}).to_string()
            }
            (Domain::Json, ValueClass::Large) => {
                serde_json::json!({"text": large(&tag, LARGE_CELL)}).to_string()
            }
            (Domain::Json, ValueClass::NearCellLimit) => {
                serde_json::json!({"text": large(&tag, self.near_limit)}).to_string()
            }
            (Domain::TypedJson, ValueClass::Zero) => {
                cql_text(&thorium::models::Resources::default())
            }
            (Domain::TypedJson, ValueClass::Max) => {
                let max = u64::MAX;
                let resources: thorium::models::Resources =
                    serde_json::from_value(serde_json::json!({
                        "cpu": max, "memory": max, "ephemeral_storage": max, "worker_slots": max,
                        "nvidia_gpu": max, "amd_gpu": max, "burstable": {"cpu": max, "memory": max}
                    }))
                    .expect("resources at their maximum decode");
                cql_text(&resources)
            }
            (Domain::AttachmentMap, ValueClass::One) => {
                attachment_map(&[format!("notes-{tag}.txt")])
            }
            (Domain::AttachmentMap, ValueClass::Many) => attachment_map(
                &(0..100)
                    .map(|index| format!("notes-{tag}-{index}.txt"))
                    .collect::<Vec<_>>(),
            ),
            (Domain::AttachmentMap, ValueClass::OddKey) => attachment_map(&[odd_key(&tag)]),
            (Domain::ObjectNames, ValueClass::One) => {
                return Some(CqlValue::Set(vec![CqlValue::Text(format!(
                    "out-{tag}.txt"
                ))]));
            }
            (Domain::EntityCounts, ValueClass::One) => {
                return Some(self.entity_counts(None, None));
            }
            (Domain::ObjectNames, ValueClass::Many) => {
                return Some(CqlValue::Set(
                    (0..MANY_CELL)
                        .map(|index| CqlValue::Text(format!("out-{tag}-{index}.txt")))
                        .collect(),
                ));
            }
            _ => return None,
        };
        Some(CqlValue::Text(text))
    }

    /// Builds a collection cell of a class, or with one element of a class
    ///
    /// # Arguments
    ///
    /// * `cql` - The collection's type
    /// * `part` - Which part the class applies to
    /// * `class` - The class to build
    fn collection(&mut self, cql: &CqlType, part: Part, class: &ValueClass) -> Option<CqlValue> {
        // build sized collections of ordinary elements, or one element of the class
        match (cql, part, class) {
            (
                CqlType::Set(item) | CqlType::List(item),
                Part::Cell,
                ValueClass::One | ValueClass::Many,
            ) => {
                let count = if *class == ValueClass::One {
                    1
                } else {
                    MANY_CELL
                };
                let items = (0..count)
                    .map(|_| self.ordinary_scalar(item))
                    .collect::<Option<Vec<_>>>()?;
                Some(wrap(cql, items))
            }
            (CqlType::Set(item) | CqlType::List(item), Part::Element, class) => {
                Some(wrap(cql, vec![self.scalar(item, class)?]))
            }
            (CqlType::Map(key, value), Part::Cell, ValueClass::One | ValueClass::Many) => {
                let count = if *class == ValueClass::One {
                    1
                } else {
                    MANY_CELL
                };
                let entries = (0..count)
                    .map(|_| Some((self.ordinary_scalar(key)?, self.ordinary_scalar(value)?)))
                    .collect::<Option<Vec<_>>>()?;
                Some(CqlValue::Map(entries))
            }
            (CqlType::Map(key, value), Part::Key, class) => Some(CqlValue::Map(vec![(
                self.scalar(key, class)?,
                self.ordinary_scalar(value)?,
            )])),
            (CqlType::Map(key, value), Part::Value, class) => Some(CqlValue::Map(vec![(
                self.ordinary_scalar(key)?,
                self.scalar(value, class)?,
            )])),
            _ => None,
        }
    }

    /// Builds an ordinary scalar for a collection element
    ///
    /// # Arguments
    ///
    /// * `cql` - The element's type
    fn ordinary_scalar(&mut self, cql: &CqlType) -> Option<CqlValue> {
        // a unique, unremarkable value of the type
        let tag = self.tag();
        match cql {
            CqlType::Text => Some(CqlValue::Text(format!("item-{tag}"))),
            CqlType::BigInt => Some(CqlValue::BigInt(7)),
            CqlType::Int => Some(CqlValue::Int(7)),
            CqlType::Uuid => Some(CqlValue::Uuid(uuid::Uuid::new_v4())),
            _ => None,
        }
    }

    /// Plans one row per required cell of a table, each changing one column from a baseline
    ///
    /// # Arguments
    ///
    /// * `table` - The table's live columns
    /// * `matrix` - The matrix whose required cells to cover
    ///
    /// # Panics
    ///
    /// Panics if a required cell can't be generated, which means this planner needs a case for it.
    pub fn plan(&mut self, table: &Table, matrix: &CoverageMatrix) -> Vec<RowPlan> {
        // walk the table's required cells
        let mut plans = Vec::new();
        for ((location, class), need) in &matrix.needs {
            let Location::Column {
                table: name,
                column,
            } = location
            else {
                continue;
            };
            let wanted = *need == Need::Required
                || (*need == Need::FullOnly && self.size == MatrixSize::Full);
            if name != &table.name || !wanted {
                continue;
            }
            // start from an ordinary row and change the one column
            let (column_name, _) = split_part(column);
            let mut values = self.base_row(table);
            if *class == ValueClass::Null {
                values.remove(column_name);
            } else {
                let value = self
                    .value(table, column, class)
                    .unwrap_or_else(|| panic!("the fixture can't build {class} for {location}"));
                values.insert(column_name.to_owned(), value);
            }
            self.derive(&table.name, &mut values);
            plans.push(RowPlan {
                table: table.name.clone(),
                values,
                body: None,
            });
        }
        plans
    }

    /// Builds a value of a class for a column, or for the elements, keys, or values a collection
    /// suffix names, or `None` when the fixture has no such value
    ///
    /// # Arguments
    ///
    /// * `table` - The table's live columns
    /// * `column` - The column, with any collection suffix
    /// * `class` - The class to build
    ///
    /// # Panics
    ///
    /// Panics if the column isn't in the live schema.
    pub fn value(&mut self, table: &Table, column: &str, class: &ValueClass) -> Option<CqlValue> {
        // a domain decides first, then the column's type
        let (column_name, part) = split_part(column);
        let target = table
            .columns
            .iter()
            .find(|candidate| candidate.name == column_name)
            .unwrap_or_else(|| panic!("{}.{column_name} isn't in the live schema", table.name))
            .clone();
        match (column_domain(&table.name, column_name), part) {
            (Some(domain), Part::Cell) => self.domain_value(domain, class),
            (Some(Domain::ObjectNames), Part::Element) => self.object_name(class),
            (Some(Domain::EntityCounts), Part::Key) => match class {
                ValueClass::Variant(name) => Some(self.entity_counts(Some(name), None)),
                _ => None,
            },
            (Some(Domain::EntityCounts), Part::Value) => {
                let count = self.scalar(&CqlType::BigInt, class)?;
                Some(self.entity_counts(None, Some(count)))
            }
            (_, Part::Cell)
                if matches!(
                    target.cql,
                    CqlType::Set(_) | CqlType::List(_) | CqlType::Map(..)
                ) =>
            {
                self.collection(&target.cql, part, class)
            }
            (_, Part::Cell) => self.scalar(&target.cql, class),
            (_, part) => self.collection(&target.cql, part, class),
        }
    }

    /// Builds a result file name set holding one name of a class
    ///
    /// # Arguments
    ///
    /// * `class` - The class of the name
    fn object_name(&mut self, class: &ValueClass) -> Option<CqlValue> {
        // names are keyed under the result id, so long names stay under S3's key limit
        let tag = self.tag();
        let name = match class {
            ValueClass::Unicode => format!("{}.txt", unicode(&tag).replace('\0', "")),
            ValueClass::OddKey => odd_key(&tag),
            ValueClass::Large => long_name(&tag),
            _ => return None,
        };
        Some(CqlValue::Set(vec![CqlValue::Text(name)]))
    }

    /// Builds an entity count map holding one kind with its count
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind's serialized name, or `None` for the first listed kind
    /// * `count` - The count, or `None` for an ordinary one
    fn entity_counts(&self, kind: Option<&str>, count: Option<CqlValue>) -> CqlValue {
        // the map's keys are the kinds' stored text, as the API writes them
        let kind = kind.map_or_else(
            || enum_variants(self.lists, "EntityKinds").names[0].clone(),
            ToOwned::to_owned,
        );
        CqlValue::Map(vec![(
            CqlValue::Text(self.enum_text("EntityKinds", &kind)),
            count.unwrap_or(CqlValue::BigInt(1)),
        )])
    }

    /// Builds a row of ordinary values for every column of a table
    ///
    /// # Arguments
    ///
    /// * `table` - The table's live columns
    pub fn base_row(&mut self, table: &Table) -> BTreeMap<String, CqlValue> {
        // give every column its ordinary value, leaving unset the ones that have none
        let mut values = BTreeMap::new();
        for column in &table.columns {
            if let Some(value) = self.baseline(&table.name, column) {
                values.insert(column.name.clone(), value);
            }
        }
        self.derive(&table.name, &mut values);
        values
    }

    /// Recomputes a row's derived columns from their sources, as restore does
    ///
    /// # Arguments
    ///
    /// * `table` - The table
    /// * `values` - The row's values
    pub fn derive(&self, table: &str, values: &mut BTreeMap<String, CqlValue>) {
        // year and bucket come from the source time and the table's partition size
        for (_, column, source, _) in DERIVED_COLUMNS.iter().filter(|(t, ..)| *t == table) {
            match *column {
                "year" | "bucket" => {
                    let Some(CqlValue::Timestamp(CqlTimestamp(millis))) = values.get(*source)
                    else {
                        continue;
                    };
                    let time = DateTime::<Utc>::from_timestamp_millis(*millis)
                        .expect("fixture times are in range");
                    let year = time.year();
                    let bucket = thorium::utils::helpers::partition(
                        time,
                        year,
                        self.partition_size(table, values),
                    );
                    values.insert("year".to_owned(), CqlValue::Int(year));
                    values.insert("bucket".to_owned(), CqlValue::Int(bucket));
                }
                lower => {
                    if let Some(CqlValue::Text(text)) = values.get(*source) {
                        let lowered = text.to_lowercase();
                        values.insert(lower.to_owned(), CqlValue::Text(lowered));
                    }
                }
            }
        }
    }

    /// Returns the partition size restore uses for a table's row
    ///
    /// # Arguments
    ///
    /// * `table` - The table
    /// * `values` - The row's values, for tables whose size depends on a column
    fn partition_size(&self, table: &str, values: &BTreeMap<String, CqlValue>) -> u16 {
        // each table reads its own size from the config
        let thorium = &self.conf.thorium;
        match table {
            "samples_list" => thorium.files.partition_size,
            "results_stream" => thorium.results.partition_size,
            "repos_list" | "commitish_list" => thorium.repos.partition_size,
            "entities" => thorium.entities.partition_size,
            "associations" => thorium.associations.partition_size,
            "tags" => {
                let tag_type = match values.get("type") {
                    Some(CqlValue::Text(text)) => tag_type_from_text(self.lists, text),
                    _ => TagType::Files,
                };
                thorium.tags.map_type(&tag_type).partition_size
            }
            other => panic!("{other} has no partition size"),
        }
    }
}

/// Wraps elements in the collection a type names
///
/// # Arguments
///
/// * `cql` - The collection's type
/// * `items` - The elements
fn wrap(cql: &CqlType, items: Vec<CqlValue>) -> CqlValue {
    // sets and lists differ only in their wrapper
    match cql {
        CqlType::List(_) => CqlValue::List(items),
        _ => CqlValue::Set(items),
    }
}

/// Builds an attachment map naming a fresh object id for each name
///
/// # Arguments
///
/// * `names` - The attachment names
fn attachment_map(names: &[String]) -> String {
    // each attachment name maps to the id that keys its object
    let map: BTreeMap<&String, uuid::Uuid> = names
        .iter()
        .map(|name| (name, uuid::Uuid::new_v4()))
        .collect();
    serde_json::to_string(&map).expect("attachment maps serialize")
}

/// Returns the tag type a stored tag type names
///
/// # Arguments
///
/// * `lists` - The guarded enum lists
/// * `text` - The stored text
fn tag_type_from_text(lists: &[EnumList], text: &str) -> TagType {
    // match the text against the stored form of each listed tag type
    let list = enum_variants(lists, "TagType");
    let index = list
        .cql
        .as_ref()
        .and_then(|cql| cql.iter().position(|stored| stored == text))
        .unwrap_or(0);
    enums::tag_types()[index]
}

/// Returns every object a row names: its bucket role and key
///
/// # Arguments
///
/// * `plan` - The row
#[must_use]
pub fn referenced_objects(plan: &RowPlan) -> Vec<(&'static str, String)> {
    // only three tables name objects
    let text = |column: &str| match plan.values.get(column) {
        Some(CqlValue::Text(text)) => Some(text.clone()),
        _ => None,
    };
    let uuid = |column: &str| match plan.values.get(column) {
        Some(CqlValue::Uuid(id)) => Some(*id),
        _ => None,
    };
    match plan.table.as_str() {
        "s3_ids" => {
            let role = if text("type").as_deref() == Some("Repo") {
                "repos"
            } else {
                "files"
            };
            uuid("id").map_or_else(Vec::new, |id| vec![(role, id.to_string())])
        }
        "comments" => {
            let (Some(sha256), Some(id), Some(files)) = (text("sha256"), uuid("id"), text("files"))
            else {
                return Vec::new();
            };
            let map: BTreeMap<String, uuid::Uuid> =
                serde_json::from_str(&files).unwrap_or_default();
            map.values()
                .map(|file| ("attachments", format!("{sha256}/{id}/{file}")))
                .collect()
        }
        "results" => {
            // each result file, and the entity blob of each kind the result found
            let Some(id) = uuid("id") else {
                return Vec::new();
            };
            let files = match plan.values.get("files") {
                Some(CqlValue::Set(files)) => files.as_slice(),
                _ => &[],
            };
            let kinds = match plan.values.get("entities") {
                Some(CqlValue::Map(entries)) => entries.as_slice(),
                _ => &[],
            };
            let names = files.iter().filter_map(|file| match file {
                CqlValue::Text(name) => Some(("results", format!("{id}/{name}"))),
                _ => None,
            });
            let blobs = kinds.iter().filter_map(|(kind, _)| match kind {
                CqlValue::Text(kind) => Some((
                    "results",
                    format!("{id}/__thorium_entities/{}.json", entity_kind(kind)),
                )),
                _ => None,
            });
            names.chain(blobs).collect()
        }
        _ => Vec::new(),
    }
}

/// Writes planned rows with CQL
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `keyspace` - The keyspace
/// * `plans` - The rows to write
///
/// # Panics
///
/// Panics if Scylla refuses a row, naming it.
pub async fn write_rows(scylla: &Session, keyspace: &str, plans: &[RowPlan]) {
    // insert each row with only its set columns, so unset columns hold no tombstones
    for plan in plans {
        let columns: Vec<&String> = plan.values.keys().collect();
        let marks = vec!["?"; columns.len()].join(", ");
        let names: Vec<String> = columns
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect();
        let statement = format!(
            "INSERT INTO {keyspace}.{} ({}) VALUES ({marks})",
            plan.table,
            names.join(", ")
        );
        let values: Vec<CqlValue> = plan.values.values().cloned().collect();
        scylla
            .query_unpaged(statement, values)
            .await
            .unwrap_or_else(|err| panic!("Scylla refused a {} row: {err}", plan.table));
    }
}

/// Reads every row of a table as column names and values
///
/// # Arguments
///
/// * `scylla` - The Scylla session
/// * `keyspace` - The keyspace
/// * `table` - The table
///
/// # Panics
///
/// Panics if the table can't be read.
pub async fn read_rows(
    scylla: &Session,
    keyspace: &str,
    table: &str,
) -> Vec<BTreeMap<String, Option<CqlValue>>> {
    // page through the whole table
    let pager = scylla
        .query_iter(format!("SELECT * FROM {keyspace}.{table}"), &[])
        .await
        .unwrap_or_else(|err| panic!("failed to read {table}: {err}"));
    let names: Vec<String> = pager
        .column_specs()
        .iter()
        .map(|spec| spec.name().to_owned())
        .collect();
    let mut stream = pager
        .rows_stream::<Row>()
        .expect("rows decode as generic values");
    let mut rows = Vec::new();
    while let Some(row) = stream.next().await {
        let row = row.unwrap_or_else(|err| panic!("failed to read a {table} row: {err}"));
        rows.push(names.iter().cloned().zip(row.columns).collect());
    }
    rows
}

/// Counts the classes of every value of a table's rows, and collects rows that break the
/// domain and derived-column rules
///
/// # Arguments
///
/// * `table` - The table's live columns
/// * `rows` - The table's rows
/// * `lists` - The guarded enum lists
/// * `planner` - A planner, for recomputing derived columns
/// * `matrix` - The matrix to count observations in
/// * `problems` - Where rule violations are collected
pub fn observe(
    table: &Table,
    rows: &[BTreeMap<String, Option<CqlValue>>],
    lists: &[EnumList],
    planner: &Planner<'_>,
    matrix: &mut CoverageMatrix,
    problems: &mut Vec<String>,
) {
    // classify every cell of every row
    for row in rows {
        for column in &table.columns {
            let value = row.get(&column.name).cloned().flatten();
            let at =
                |suffix: &str| Location::column(&table.name, &format!("{}{suffix}", column.name));
            let Some(value) = value else {
                matrix.observe(at(""), ValueClass::Null);
                continue;
            };
            let domain = column_domain(&table.name, &column.name);
            for (suffix, class) in classify(&column.cql, domain, &value, lists) {
                matrix.observe(at(suffix), class);
            }
            // text that holds JSON must be listed with a domain
            if let (CqlValue::Text(text), None) = (&value, domain)
                && (!json_classes(text).is_empty()
                    || serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(text)
                        .is_ok())
            {
                problems.push(format!(
                    "{}.{} holds JSON but has no COLUMN_DOMAINS entry",
                    table.name, column.name
                ));
            }
        }
        // derived columns must equal what restore would recompute from their sources
        let mut values: BTreeMap<String, CqlValue> = row
            .iter()
            .filter_map(|(name, value)| value.clone().map(|value| (name.clone(), value)))
            .collect();
        let stored = values.clone();
        planner.derive(&table.name, &mut values);
        for (_, column, ..) in DERIVED_COLUMNS.iter().filter(|(t, ..)| *t == table.name) {
            if stored.get(*column) != values.get(*column) {
                problems.push(format!(
                    "{}.{column} is {:?} but its source gives {:?}",
                    table.name,
                    stored.get(*column),
                    values.get(*column)
                ));
            }
        }
    }
    // every listed column must exist
    problems.extend(stale_entries(table, &listed_columns()));
}

/// Returns every table and column the column lists name
#[must_use]
pub fn listed_columns() -> Vec<(&'static str, &'static str)> {
    // the domain, derived, unrepresentable, and near-limit lists
    COLUMN_DOMAINS
        .iter()
        .map(|(t, c, ..)| (*t, *c))
        .chain(DERIVED_COLUMNS.iter().map(|(t, c, ..)| (*t, *c)))
        .chain(UNREPRESENTABLE.iter().map(|(t, c, ..)| (*t, *c)))
        .chain(UNSET_UNREPRESENTABLE.iter().copied())
        .chain(NEAR_LIMIT_COLUMNS.iter().copied())
        .collect()
}

/// Returns a problem for every list entry naming a column a table doesn't have
///
/// # Arguments
///
/// * `table` - The table's live columns
/// * `entries` - The listed tables and columns
#[must_use]
pub fn stale_entries(table: &Table, entries: &[(&str, &str)]) -> Vec<String> {
    // entries for this table must name one of its columns
    entries
        .iter()
        .filter(|(listed, column)| {
            *listed == table.name
                && !table
                    .columns
                    .iter()
                    .any(|candidate| candidate.name == *column)
        })
        .map(|(listed, column)| format!("a list names {listed}.{column}, which doesn't exist"))
        .collect()
}

/// Returns a problem for every near-limit entry naming a column that keys its table or a view,
/// whose values Scylla caps at a key's 64 KiB
///
/// # Arguments
///
/// * `table` - The table's live columns
/// * `entries` - The near-limit tables and columns
#[must_use]
pub fn key_sized_entries(table: &Table, entries: &[(&str, &str)]) -> Vec<String> {
    // entries for this table must name a column outside every key
    entries
        .iter()
        .filter(|(listed, column)| {
            *listed == table.name
                && table
                    .columns
                    .iter()
                    .any(|candidate| candidate.name == *column && candidate.is_key_sized())
        })
        .map(|(listed, column)| {
            format!("a near-limit entry names {listed}.{column}, which keys its table or a view")
        })
        .collect()
}

/// Returns the classes of one stored value, each with the location suffix it belongs to
///
/// # Arguments
///
/// * `cql` - The column's type
/// * `domain` - The column's domain
/// * `value` - The stored value
/// * `lists` - The guarded enum lists
fn classify(
    cql: &CqlType,
    domain: Option<Domain>,
    value: &CqlValue,
    lists: &[EnumList],
) -> Vec<(&'static str, ValueClass)> {
    // domains classify their own values
    let mut found = Vec::new();
    match (domain, value) {
        (Some(Domain::Enum(list)), CqlValue::Text(text)) => {
            let list = enum_variants(lists, list);
            if let Some(index) = list
                .cql
                .as_ref()
                .and_then(|cql| cql.iter().position(|stored| stored == text))
            {
                found.push(("", ValueClass::Variant(list.names[index].clone())));
            }
        }
        (Some(Domain::Hash(len)), CqlValue::Text(text)) => {
            if text.len() == len && text.chars().all(|c| c == '0') {
                found.push(("", ValueClass::Min));
            }
            if text.len() == len && text.chars().all(|c| c == 'f') {
                found.push(("", ValueClass::Max));
            }
        }
        (Some(Domain::Json), CqlValue::Text(text)) => {
            found.extend(json_classes(text).into_iter().map(|class| ("", class)));
            let parsed =
                serde_json::from_str::<serde_json::Value>(text).unwrap_or(serde_json::Value::Null);
            if json_strings(&parsed).iter().any(|inner| is_unicode(inner)) {
                found.push(("", ValueClass::Unicode));
            }
            if text.len() > LARGE_CELL {
                found.push(("", ValueClass::Large));
            }
            near_limit(text.len(), &mut found);
        }
        (Some(Domain::TypedJson), CqlValue::Text(text)) => {
            let parsed =
                serde_json::from_str::<serde_json::Value>(text).unwrap_or(serde_json::Value::Null);
            let numbers = json_numbers(&parsed);
            if !numbers.is_empty() && numbers.iter().all(|number| *number == 0) {
                found.push(("", ValueClass::Zero));
            }
            if !numbers.is_empty() && numbers.iter().all(|number| *number == u64::MAX) {
                found.push(("", ValueClass::Max));
            }
        }
        (Some(Domain::AttachmentMap), CqlValue::Text(text)) => {
            let map: BTreeMap<String, uuid::Uuid> = serde_json::from_str(text).unwrap_or_default();
            match map.len() {
                0 => found.push(("", ValueClass::JsonEmpty)),
                1 => found.push(("", ValueClass::One)),
                len if len >= 100 => found.push(("", ValueClass::Many)),
                _ => (),
            }
            if map.keys().any(|name| is_odd_key(name)) {
                found.push(("", ValueClass::OddKey));
            }
        }
        (Some(Domain::EntityCounts), CqlValue::Map(entries)) => {
            found.push(("", collection_size(entries.len())));
            let kinds = enum_variants(lists, "EntityKinds");
            for (key, count) in entries {
                if let CqlValue::Text(kind) = key
                    && let Some(index) = kinds
                        .cql
                        .as_ref()
                        .and_then(|cql| cql.iter().position(|stored| stored == kind))
                {
                    found.push(("{key}", ValueClass::Variant(kinds.names[index].clone())));
                }
                found.extend(
                    classify_plain(&CqlType::BigInt, count)
                        .into_iter()
                        .map(|(_, class)| ("{value}", class)),
                );
            }
        }
        (Some(Domain::ObjectNames), CqlValue::Set(names)) => {
            found.push(("", collection_size(names.len())));
            for name in names {
                if let CqlValue::Text(name) = name {
                    if is_unicode_name(name) {
                        found.push(("[]", ValueClass::Unicode));
                    }
                    if is_odd_key(name) {
                        found.push(("[]", ValueClass::OddKey));
                    }
                    if name.len() >= LONG_NAME {
                        found.push(("[]", ValueClass::Large));
                    }
                }
            }
        }
        (None, value) => found.extend(classify_plain(cql, value)),
        _ => (),
    }
    found
}

/// Returns the classes of a value with no domain, by its type
///
/// # Arguments
///
/// * `cql` - The column's type
/// * `value` - The stored value
fn classify_plain(cql: &CqlType, value: &CqlValue) -> Vec<(&'static str, ValueClass)> {
    // scalars by their edges, collections by size and element
    let mut found: Vec<(&'static str, ValueClass)> = Vec::new();
    match value {
        CqlValue::Text(text) | CqlValue::Ascii(text) => {
            found.extend(
                text_classes(text, LARGE_CELL)
                    .into_iter()
                    .map(|class| ("", class)),
            );
            near_limit(text.len(), &mut found);
        }
        CqlValue::TinyInt(int) => found.extend(
            int_classes(i128::from(*int), i128::from(i8::MIN), i128::from(i8::MAX))
                .into_iter()
                .map(|class| ("", class)),
        ),
        CqlValue::SmallInt(int) => found.extend(
            int_classes(i128::from(*int), i128::from(i16::MIN), i128::from(i16::MAX))
                .into_iter()
                .map(|class| ("", class)),
        ),
        CqlValue::Int(int) => found.extend(
            int_classes(i128::from(*int), i128::from(i32::MIN), i128::from(i32::MAX))
                .into_iter()
                .map(|class| ("", class)),
        ),
        CqlValue::BigInt(int) => found.extend(
            int_classes(i128::from(*int), i128::from(i64::MIN), i128::from(i64::MAX))
                .into_iter()
                .map(|class| ("", class)),
        ),
        CqlValue::Timestamp(CqlTimestamp(millis)) => {
            found.extend(time_classes(*millis).into_iter().map(|class| ("", class)));
        }
        CqlValue::Uuid(id) => found.extend(uuid_classes(*id).into_iter().map(|class| ("", class))),
        CqlValue::Boolean(flag) => found.push(("", ValueClass::Variant(flag.to_string()))),
        CqlValue::Set(items) | CqlValue::List(items) => {
            found.push(("", collection_size(items.len())));
            let item_type = match cql {
                CqlType::Set(item) | CqlType::List(item) => item.as_ref().clone(),
                _ => CqlType::Text,
            };
            for item in items {
                found.extend(
                    classify_plain(&item_type, item)
                        .into_iter()
                        .map(|(_, class)| ("[]", class)),
                );
            }
        }
        CqlValue::Map(entries) => {
            found.push(("", collection_size(entries.len())));
            let (key_type, value_type) = match cql {
                CqlType::Map(key, value) => (key.as_ref().clone(), value.as_ref().clone()),
                _ => (CqlType::Text, CqlType::Text),
            };
            for (key, inner) in entries {
                found.extend(
                    classify_plain(&key_type, key)
                        .into_iter()
                        .map(|(_, class)| ("{key}", class)),
                );
                if let CqlValue::Text(text) = key
                    && is_odd_key(text)
                {
                    found.push(("{key}", ValueClass::OddKey));
                }
                found.extend(
                    classify_plain(&value_type, inner)
                        .into_iter()
                        .map(|(_, class)| ("{value}", class)),
                );
            }
        }
        _ => (),
    }
    found
}

/// Adds the near-cell-limit class to a value long enough for it
///
/// # Arguments
///
/// * `len` - The value's length
/// * `found` - The classes found so far
fn near_limit(len: usize, found: &mut Vec<(&'static str, ValueClass)>) {
    // anything past a megabyte only comes from the near-limit cases
    if len > 1_048_576 {
        found.push(("", ValueClass::NearCellLimit));
    }
}

/// Returns the size class of a collection
///
/// # Arguments
///
/// * `len` - The collection's length
fn collection_size(len: usize) -> ValueClass {
    // one, many, or an ordinary size in between
    match len {
        1 => ValueClass::One,
        len if len >= MANY_CELL => ValueClass::Many,
        _ => ValueClass::Variant("some".to_owned()),
    }
}

/// Returns every string inside a JSON value
///
/// # Arguments
///
/// * `value` - The JSON value
fn json_strings(value: &serde_json::Value) -> Vec<&str> {
    // walk containers and keep strings
    match value {
        serde_json::Value::String(text) => vec![text.as_str()],
        serde_json::Value::Array(items) => items.iter().flat_map(json_strings).collect(),
        serde_json::Value::Object(map) => map.values().flat_map(json_strings).collect(),
        _ => Vec::new(),
    }
}

/// Returns every unsigned number inside a JSON value
///
/// # Arguments
///
/// * `value` - The JSON value
fn json_numbers(value: &serde_json::Value) -> Vec<u64> {
    // walk containers and keep numbers
    match value {
        serde_json::Value::Number(number) => number.as_u64().into_iter().collect(),
        serde_json::Value::Array(items) => items.iter().flat_map(json_numbers).collect(),
        serde_json::Value::Object(map) => map.values().flat_map(json_numbers).collect(),
        _ => Vec::new(),
    }
}
