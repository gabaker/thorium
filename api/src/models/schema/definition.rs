//! The structured definitions the table and view derives emit

use scylla::frame::response::result::ColumnType;
use scylla::serialize::SerializationError;
use scylla::serialize::writers::{CellWriter, WrittenCellProof};

use super::cql_type::CqlDesc;

/// One column of a table or view
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnDef {
    /// The CQL column name
    pub name: &'static str,
    /// The column's CQL type
    pub desc: CqlDesc,
    /// The column's role in the primary key
    pub role: ColumnRole,
    /// Whether a lossy downgrade must fill this column restrictively
    pub security_sensitive: bool,
}

/// The role a column plays in its object's primary key
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnRole {
    /// A partition key column, at this position in the partition key
    Partition(u8),
    /// A clustering column, at this position in the clustering key, with its order
    Cluster(u8, ClusterOrder),
    /// A static column
    Static,
    /// A regular column
    Regular,
}

impl ColumnRole {
    /// Whether this column is part of the primary key
    #[must_use]
    pub const fn is_key(&self) -> bool {
        matches!(self, ColumnRole::Partition(_) | ColumnRole::Cluster(..))
    }
}

/// The order of a clustering column
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterOrder {
    /// Ascending, the CQL default
    Asc,
    /// Descending
    Desc,
}

/// A base table's definition
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableDef {
    /// The CQL table name, without the keyspace
    pub name: &'static str,
    /// Every column, in declaration order
    pub columns: &'static [ColumnDef],
    /// Where the table's default TTL comes from, if it has one
    pub ttl: Option<TtlSource>,
    /// The partition-size setting the table's bucket column names, for a time-bucketed table
    pub partition_size: Option<&'static str>,
}

/// The config value a table's default TTL is read from
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtlSource {
    /// The dotted path of the setting in the Thorium config, such as `thorium.retention.logs`
    pub config_path: &'static str,
}

/// A materialized view's definition
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewDef {
    /// The CQL view name, without the keyspace
    pub name: &'static str,
    /// The name of the base table this view selects from
    pub base: &'static str,
    /// Every column the view selects, listed explicitly
    pub columns: &'static [ColumnDef],
    /// A condition on one key column that replaces that column's `IS NOT NULL`, such as
    /// `default_policy = true`
    pub filter: Option<&'static str>,
}

/// The definition of one declared Scylla object
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectDef {
    /// A base table
    Table(TableDef),
    /// A materialized view
    View(ViewDef),
}

impl ObjectDef {
    /// Render this object's `CREATE ... IF NOT EXISTS` statement
    ///
    /// # Arguments
    ///
    /// * `keyspace` - The keyspace to create the object in
    /// * `ttl` - A table's default TTL in seconds, read from its TTL source's setting
    #[must_use]
    pub fn create_cql(&self, keyspace: &str, ttl: Option<u64>) -> String {
        match self {
            ObjectDef::Table(table) => table.create_cql(keyspace, ttl),
            ObjectDef::View(view) => view.create_cql(keyspace),
        }
    }

    /// The object's CQL name
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            ObjectDef::Table(table) => table.name,
            ObjectDef::View(view) => view.name,
        }
    }

    /// The object's columns
    #[must_use]
    pub const fn columns(&self) -> &'static [ColumnDef] {
        match self {
            ObjectDef::Table(table) => table.columns,
            ObjectDef::View(view) => view.columns,
        }
    }

    /// The names of the object's partition key columns, in key order
    #[must_use]
    pub fn partition_key(&self) -> Vec<&'static str> {
        // the partition columns sorted by their place in the key
        key_columns(self.columns(), true)
            .iter()
            .map(|column| column.name)
            .collect()
    }
}

/// The key columns of an object with the given role, in key order
///
/// # Arguments
///
/// * `columns` - The object's columns
/// * `partition` - Whether to get the partition key (or the clustering key)
fn key_columns(columns: &[ColumnDef], partition: bool) -> Vec<&ColumnDef> {
    // keep the columns with the requested role, with their positions
    let mut keys = columns
        .iter()
        .filter_map(|column| match column.role {
            ColumnRole::Partition(position) if partition => Some((position, column)),
            ColumnRole::Cluster(position, _) if !partition => Some((position, column)),
            _ => None,
        })
        .collect::<Vec<_>>();
    // put them in key order
    keys.sort_by_key(|(position, _)| *position);
    keys.into_iter().map(|(_, column)| column).collect()
}

/// Render an object's `PRIMARY KEY (...)` clause and its full clustering order, if it has
/// clustering columns
///
/// # Arguments
///
/// * `columns` - The object's columns
fn primary_key(columns: &[ColumnDef]) -> (String, Option<String>) {
    // list the partition and clustering columns in key order
    let partition = key_columns(columns, true)
        .iter()
        .map(|column| column.name)
        .collect::<Vec<_>>()
        .join(", ");
    let clustering = key_columns(columns, false);
    // the partition key is always parenthesized, followed by the clustering columns
    let mut key = format!("PRIMARY KEY (({partition})");
    for column in &clustering {
        key.push_str(", ");
        key.push_str(column.name);
    }
    key.push(')');
    // every clustering column's order is listed, ascending ones included
    let order = (!clustering.is_empty()).then(|| {
        let orders = clustering
            .iter()
            .map(|column| match column.role {
                ColumnRole::Cluster(_, ClusterOrder::Desc) => format!("{} DESC", column.name),
                _ => format!("{} ASC", column.name),
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("CLUSTERING ORDER BY ({orders})")
    });
    (key, order)
}

impl TableDef {
    /// Render this table's `CREATE TABLE IF NOT EXISTS` statement
    ///
    /// # Arguments
    ///
    /// * `keyspace` - The keyspace to create the table in
    /// * `ttl` - The table's default TTL in seconds, read from its TTL source's setting
    #[must_use]
    pub fn create_cql(&self, keyspace: &str, ttl: Option<u64>) -> String {
        // declare every column with its type, then the primary key
        let columns = self
            .columns
            .iter()
            .map(|column| format!("{} {}", column.name, column.desc.render()))
            .collect::<Vec<_>>()
            .join(", ");
        let (key, order) = primary_key(self.columns);
        // add the clustering order and default TTL as table options
        let options = order
            .into_iter()
            .chain(ttl.map(|ttl| format!("default_time_to_live = {ttl}")))
            .collect::<Vec<_>>();
        let with = if options.is_empty() {
            String::new()
        } else {
            format!(" WITH {}", options.join(" AND "))
        };
        format!(
            "CREATE TABLE IF NOT EXISTS {keyspace}.{} ({columns}, {key}){with}",
            self.name
        )
    }

    /// Render the `ALTER TABLE ... ADD` statement adding one of this table's columns to a table
    /// created without it
    ///
    /// # Arguments
    ///
    /// * `keyspace` - The keyspace the table is in
    /// * `column` - The column to add
    #[must_use]
    pub fn alter_add_cql(&self, keyspace: &str, column: &str) -> Option<String> {
        // only a declared column can be added
        let column = self.columns.iter().find(|def| def.name == column)?;
        Some(format!(
            "ALTER TABLE {keyspace}.{} ADD {} {}",
            self.name,
            column.name,
            column.desc.render()
        ))
    }
}

impl ViewDef {
    /// The key column this view's filter restricts in place of `IS NOT NULL`, if it has a filter
    #[must_use]
    pub fn filter_column(&self) -> Option<&'static str> {
        // the filter starts with the column it restricts
        self.filter.and_then(|filter| {
            filter
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .find(|part| !part.is_empty())
        })
    }

    /// Render this view's `CREATE MATERIALIZED VIEW IF NOT EXISTS` statement
    ///
    /// # Arguments
    ///
    /// * `keyspace` - The keyspace to create the view in
    #[must_use]
    pub fn create_cql(&self, keyspace: &str) -> String {
        // select the view's columns explicitly
        let columns = self
            .columns
            .iter()
            .map(|column| column.name)
            .collect::<Vec<_>>()
            .join(", ");
        // every key column must be set, except one the filter restricts instead
        let filter_column = self.filter_column();
        let mut conditions = key_columns(self.columns, true)
            .into_iter()
            .chain(key_columns(self.columns, false))
            .filter(|column| Some(column.name) != filter_column)
            .map(|column| format!("{} IS NOT NULL", column.name))
            .collect::<Vec<_>>();
        conditions.extend(self.filter.map(str::to_owned));
        let (key, order) = primary_key(self.columns);
        let with = order.map_or_else(String::new, |order| format!(" WITH {order}"));
        format!(
            "CREATE MATERIALIZED VIEW IF NOT EXISTS {keyspace}.{} AS SELECT {columns} FROM \
             {keyspace}.{} WHERE {} {key}{with}",
            self.name,
            self.base,
            conditions.join(" AND ")
        )
    }
}

/// A struct that declares one Scylla object, implemented by the table and view derives
pub trait ScyllaObject {
    /// The object's CQL name
    const NAME: &'static str;
    /// The object's definition, built from its fields' `CqlType::DESC` consts
    const DEF: ObjectDef;
    /// The columns whose values every generated `Debug` hides, which views and projections
    /// reading them must hide too
    const REDACTED: &'static [&'static str];
    /// The Rust field name of each column, in declaration order, so queries name columns by
    /// field
    const FIELDS: &'static [&'static str];
}

/// The column at one position of a declared object's fields, implemented by the table and view
/// derives for every column
///
/// Queries structs name columns by field; the field's position selects this impl, which gives
/// the column's CQL name and the types its binds take.
pub trait ColumnAt<const N: usize>: ScyllaObject {
    /// The column's CQL name
    const COLUMN: &'static str;
    /// The field's own type
    type Owned;
    /// The type a bind of one value takes, the same as the object's borrowed row field
    type Bind<'a>;

    /// Write one bound value of this column, through its checked conversion if it's unsigned
    ///
    /// # Arguments
    ///
    /// * `value` - The value to write
    /// * `typ` - The bind marker's CQL type
    /// * `cell` - The writer for the value
    fn serialize_bind<'b>(
        value: &Self::Bind<'_>,
        typ: &ColumnType,
        cell: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError>;

    /// Write a list of this column's values for an `IN` restriction; only key columns can be
    /// restricted, so other columns refuse
    ///
    /// # Arguments
    ///
    /// * `values` - The values to write
    /// * `typ` - The bind marker's CQL type
    /// * `cell` - The writer for the list
    fn serialize_list<'b>(
        values: &[Self::Owned],
        typ: &ColumnType,
        cell: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        // the arguments are unused because the column can't be restricted with a list
        let _ = (values, typ, cell);
        Err(super::checks::not_an_in_column(Self::NAME, Self::COLUMN))
    }

    /// Write one value of this column given as the field's own type, as cursors bind the key
    /// values they read back; only key columns are bound this way, so other columns refuse
    ///
    /// # Arguments
    ///
    /// * `value` - The value to write
    /// * `typ` - The bind marker's CQL type
    /// * `cell` - The writer for the value
    fn serialize_owned<'b>(
        value: &Self::Owned,
        typ: &ColumnType,
        cell: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        // the arguments are unused because only key columns bind their own type
        let _ = (value, typ, cell);
        Err(super::checks::not_a_key_column(Self::NAME, Self::COLUMN))
    }
}

/// The field at one position of a projection, implemented by the projection derive for every
/// field, so generated cursors read the key columns of the rows they page through
pub trait ProjectedAt<const N: usize>: Projection {
    /// The field's type
    type Value;

    /// The field's value in a row
    fn value(&self) -> &Self::Value;
}

/// A row struct that reads some columns of one or more objects, implemented by the projection
/// derive
pub trait Projection {
    /// The selected column names (or aggregate expressions), in field order
    const COLUMNS: &'static [&'static str];
    /// The columns of the fields marked `#[thorium(redact)]`, which its `Debug` hides
    const REDACTED: &'static [&'static str];
}

/// A row that writes each of its table's columns by name, implemented by the table derive for
/// the borrowed row, so a wrapper can answer bind markers that aren't columns
pub trait BindColumns {
    /// The name of the table the row belongs to
    const TABLE: &'static str;

    /// Write one bind column from the field with that column's name, or return `None` when the
    /// row has no such column
    ///
    /// # Arguments
    ///
    /// * `name` - The bind column's name, as the prepared statement reports it
    /// * `typ` - The bind column's CQL type
    /// * `cell` - The writer for this column's value
    fn serialize_column<'b>(
        &self,
        name: &str,
        typ: &ColumnType,
        cell: CellWriter<'b>,
    ) -> Option<Result<WrittenCellProof<'b>, SerializationError>>;
}

/// The full-row statements of a table, generated by the table derive with a `{ns}` placeholder
/// for the keyspace
pub trait TableStatements: ScyllaObject {
    /// The table's borrowed row, which its full-row inserts bind
    type Ref<'a>: BindColumns;
    /// Insert or overwrite a whole row, binding every column by name
    const INSERT: &'static str;
    /// Select every column of every row
    const SELECT_ALL: &'static str;
    /// Select every column of the rows in a token range of the partition key
    const SCAN: &'static str;
    /// Count the rows of each partition in a token range of the partition key
    const CENSUS: &'static str;
}

/// Fill a generated statement's `{ns}` placeholder with a keyspace
///
/// # Arguments
///
/// * `cql` - The generated statement
/// * `keyspace` - The keyspace the statement targets
#[must_use]
pub fn with_keyspace(cql: &str, keyspace: &str) -> String {
    cql.replace("{ns}", keyspace)
}
