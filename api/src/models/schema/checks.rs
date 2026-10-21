//! Compile-time checks and runtime error helpers that generated code calls
//!
//! The const functions run while the derives' `const _: () = ...` items are evaluated, so a
//! failed check is a compile error carrying the panic message.

use scylla::deserialize::{DeserializationError, TypeCheckError};
use scylla::serialize::SerializationError;

use super::cql_type::CqlDesc;
use super::cursors::{Order, cursor_order, tie_order};
use super::definition::{ColumnDef, ColumnRole, ObjectDef};
use super::queries_runtime::{InKind, Restriction};

/// The longest message a failed check can report
const MESSAGE_LEN: usize = 512;

/// Whether two strings are equal, in a const context
///
/// # Arguments
///
/// * `left` - The first string
/// * `right` - The second string
#[must_use]
pub const fn str_eq(left: &str, right: &str) -> bool {
    // compare the strings as bytes, since const code can't compare `str`s
    let (left, right) = (left.as_bytes(), right.as_bytes());
    // strings of different lengths can't be equal
    if left.len() != right.len() {
        return false;
    }
    // compare each byte in turn
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Panic with the concatenation of some parts, in a const context
///
/// # Arguments
///
/// * `parts` - The parts of the message
///
/// # Panics
///
/// Always, with the message; called from a failed compile-time check
pub const fn fail(parts: &[&str]) -> ! {
    let mut buf = [0_u8; MESSAGE_LEN];
    let mut len = 0;
    let mut part = 0;
    // copy each part into the buffer until it's full
    while part < parts.len() {
        let bytes = parts[part].as_bytes();
        let mut index = 0;
        while index < bytes.len() && len < MESSAGE_LEN {
            buf[len] = bytes[index];
            len += 1;
            index += 1;
        }
        part += 1;
    }
    let (message, _) = buf.split_at(len);
    match core::str::from_utf8(message) {
        Ok(message) => panic!("{}", message),
        Err(_) => panic!("a schema check failed"),
    }
}

/// Find a column of an object by name, in a const context
///
/// # Arguments
///
/// * `object` - The object to search
/// * `name` - The column's name
#[must_use]
pub const fn find_column(object: &ObjectDef, name: &str) -> Option<&'static ColumnDef> {
    let columns = object.columns();
    let mut index = 0;
    // walk the columns until one has this name
    while index < columns.len() {
        if str_eq(columns[index].name, name) {
            return Some(&columns[index]);
        }
        index += 1;
    }
    None
}

/// Check that a view column exists in its base table with the same type
///
/// # Arguments
///
/// * `view_desc` - The view column's type
/// * `base` - The base table's definition
/// * `view` - The view's name
/// * `column` - The column's name
pub const fn assert_view_column(view_desc: CqlDesc, base: &ObjectDef, view: &str, column: &str) {
    match find_column(base, column) {
        // the base column must have the view column's type
        Some(base_column) => {
            if !view_desc.const_eq(&base_column.desc) {
                fail(&[
                    "view ",
                    view,
                    ".",
                    column,
                    " has a different CQL type than its base ",
                    base.name(),
                ]);
            }
        }
        // a view can only select columns of its base
        None => fail(&[
            "view ",
            view,
            ".",
            column,
            " isn't a column of its base ",
            base.name(),
        ]),
    }
}

/// Check that a view's key holds every key column of its base table
///
/// # Arguments
///
/// * `base` - The base table's definition
/// * `view` - The view's definition
pub const fn assert_view_key(base: &ObjectDef, view: &ObjectDef) {
    let base_columns = base.columns();
    let view_columns = view.columns();
    let mut index = 0;
    // look at each of the base's columns in turn
    while index < base_columns.len() {
        let column = &base_columns[index];
        // only the base's key columns must be in the view's key
        if column.role.is_key() {
            let mut found = false;
            let mut search = 0;
            // search the view's key for this column
            while search < view_columns.len() {
                let candidate = &view_columns[search];
                if candidate.role.is_key() && str_eq(candidate.name, column.name) {
                    found = true;
                }
                search += 1;
            }
            // a base key column missing from the view's key fails the build
            if !found {
                fail(&[
                    "view ",
                    view.name(),
                    " is missing base key column `",
                    column.name,
                    "` from its key",
                ]);
            }
        }
        index += 1;
    }
}

/// Whether a projection field's type reads a column's type: equal, or a list read from a set
///
/// # Arguments
///
/// * `field` - The field's type
/// * `column` - The column's type
const fn projection_reads(field: &CqlDesc, column: &CqlDesc) -> bool {
    match (field, column) {
        (CqlDesc::List(field), CqlDesc::Set(column)) => field.const_eq(column),
        _ => field.const_eq(column),
    }
}

/// Check that a projection field reads a column of one of the objects it reads, by name
///
/// # Arguments
///
/// * `field` - The field's type
/// * `objects` - The objects the projection reads
/// * `name` - The column's name
/// * `label` - The start of the message naming the field, such as `projection Row.field -> `
pub const fn assert_projection_in_any(
    field: CqlDesc,
    objects: &[&ObjectDef],
    name: &str,
    label: &str,
) {
    let mut object = 0;
    // the first object with this column decides its type
    while object < objects.len() {
        if let Some(column) = find_column(objects[object], name) {
            if !projection_reads(&field, &column.desc) {
                fail(&[label, objects[object].name(), ".", name]);
            }
            return;
        }
        object += 1;
    }
    // no object the projection reads has this column
    fail(&[
        label,
        name,
        ": no object the projection reads has this column",
    ]);
}

/// Check that a field reading a redacted column is redacted too
///
/// # Arguments
///
/// * `redact` - Whether the field is redacted
/// * `redacted` - The redacted columns of each object the field may read
/// * `name` - The column's name
/// * `label` - The start of the message naming the field, such as `view secrets.`
pub const fn assert_redacted(redact: bool, redacted: &[&[&str]], name: &str, label: &str) {
    // a redacted field may read anything
    if redact {
        return;
    }
    // an unredacted field can't read a column any of its objects redacts
    let mut object = 0;
    while object < redacted.len() {
        let mut index = 0;
        while index < redacted[object].len() {
            if str_eq(redacted[object][index], name) {
                fail(&[
                    label,
                    name,
                    " reads a redacted column; mark the field #[thorium(redact)]",
                ]);
            }
            index += 1;
        }
        object += 1;
    }
}

/// Check that no two objects of a keyspace share a name
///
/// # Arguments
///
/// * `objects` - The keyspace's objects
pub const fn assert_unique_objects(objects: &[ObjectDef]) {
    let mut index = 0;
    // compare each object with every object after it
    while index < objects.len() {
        let mut other = index + 1;
        while other < objects.len() {
            // two objects with one name fail the build
            if str_eq(objects[index].name(), objects[other].name()) {
                fail(&["duplicate object `", objects[index].name(), "` in keyspace"]);
            }
            other += 1;
        }
        index += 1;
    }
}

/// A bind column a row has no field for
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownBindColumn {
    /// The table the row belongs to
    pub table: &'static str,
    /// The bind column the statement asked for
    pub column: String,
}

impl std::fmt::Display for UnknownBindColumn {
    /// Name the table and column
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "a {} row has no column {}.{}",
            self.table, self.table, self.column
        )
    }
}

impl std::error::Error for UnknownBindColumn {}

/// Build the error for a bind column a row has no field for
///
/// # Arguments
///
/// * `table` - The table the row belongs to
/// * `column` - The bind column the statement asked for
#[must_use]
pub fn unknown_bind_column(table: &'static str, column: &str) -> SerializationError {
    SerializationError::new(UnknownBindColumn {
        table,
        column: column.to_owned(),
    })
}

/// A column a projection reads that a result didn't have, or didn't fill
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingColumn {
    /// The projection reading the column
    pub projection: &'static str,
    /// The column it reads
    pub column: &'static str,
}

impl std::fmt::Display for MissingColumn {
    /// Name the projection and column
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the result has no column {} for {}",
            self.column, self.projection
        )
    }
}

impl std::error::Error for MissingColumn {}

/// A column whose type the projection field can't read
#[derive(Debug, Clone)]
pub struct ColumnTypeError {
    /// The projection reading the column
    pub projection: &'static str,
    /// The column it reads
    pub column: &'static str,
    /// The driver's type check error
    pub error: TypeCheckError,
}

impl std::fmt::Display for ColumnTypeError {
    /// Name the projection, the column, and the driver's error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} can't read column {}: {}",
            self.projection, self.column, self.error
        )
    }
}

impl std::error::Error for ColumnTypeError {}

/// Build the type check error for a column missing from a result
///
/// # Arguments
///
/// * `projection` - The projection reading the column
/// * `column` - The column it reads
#[must_use]
pub fn missing_column(projection: &'static str, column: &'static str) -> TypeCheckError {
    TypeCheckError::new(MissingColumn { projection, column })
}

/// Build the type check error for a column of a type the field can't read
///
/// # Arguments
///
/// * `projection` - The projection reading the column
/// * `column` - The column it reads
/// * `error` - The driver's type check error
#[must_use]
pub fn column_type_error(
    projection: &'static str,
    column: &'static str,
    error: TypeCheckError,
) -> TypeCheckError {
    TypeCheckError::new(ColumnTypeError {
        projection,
        column,
        error,
    })
}

/// A column value the projection field can't decode
#[derive(Debug, Clone)]
pub struct ColumnValueError {
    /// The projection reading the column
    pub projection: &'static str,
    /// The column it reads
    pub column: &'static str,
    /// The driver's or the conversion's deserialization error
    pub error: DeserializationError,
}

impl std::fmt::Display for ColumnValueError {
    /// Name the projection, the column, and the underlying error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} can't read column {}: {}",
            self.projection, self.column, self.error
        )
    }
}

impl std::error::Error for ColumnValueError {}

/// Build the deserialization error for a column value a field can't decode
///
/// # Arguments
///
/// * `projection` - The projection reading the column
/// * `column` - The column it reads
/// * `error` - The driver's or the conversion's deserialization error
#[must_use]
pub fn column_value_error(
    projection: &'static str,
    column: &'static str,
    error: DeserializationError,
) -> DeserializationError {
    DeserializationError::new(ColumnValueError {
        projection,
        column,
        error,
    })
}

/// Build the deserialization error for a column a row didn't fill
///
/// # Arguments
///
/// * `projection` - The projection reading the column
/// * `column` - The column it reads
#[must_use]
pub fn missing_value(projection: &'static str, column: &'static str) -> DeserializationError {
    DeserializationError::new(MissingColumn { projection, column })
}

/// Which key columns a statement's restrictions must cover
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The whole partition key and any prefix of the clustering key
    Prefix,
    /// Exactly the partition key
    Partition,
    /// Every key column
    Key,
}

/// Find the position of a field among an object's fields, in a const context
///
/// # Arguments
///
/// * `fields` - The object's Rust field names, in declaration order
/// * `name` - The field's name
/// * `object` - The object's name, for the error
///
/// # Panics
///
/// When the object has no such field, failing the build
#[must_use]
pub const fn field_index(fields: &[&str], name: &str, object: &str) -> usize {
    let mut index = 0;
    // walk the fields until one has this name
    while index < fields.len() {
        if str_eq(fields[index], name) {
            return index;
        }
        index += 1;
    }
    fail(&["`", object, "` has no field `", name, "`"])
}

/// The rank of a key column in its object's primary key: partition columns first, then
/// clustering columns, each in key order; `None` for a column that isn't a key column
///
/// # Arguments
///
/// * `object` - The object's definition
/// * `column` - The column's name
const fn key_rank(object: &ObjectDef, column: &str) -> Option<usize> {
    match find_column(object, column) {
        Some(def) => match def.role {
            ColumnRole::Partition(position) => Some(position as usize),
            ColumnRole::Cluster(position, _) => Some(256 + position as usize),
            ColumnRole::Static | ColumnRole::Regular => None,
        },
        None => None,
    }
}

/// Order some key columns of an object the way its primary key does
///
/// Returns the indexes of `columns` in key order, so a statement's restrictions and binds can
/// follow the key whatever order they were declared in.
///
/// # Arguments
///
/// * `object` - The object's definition
/// * `columns` - The key columns, as declared
///
/// # Panics
///
/// When a column isn't a key column of the object, failing the build
#[must_use]
pub const fn key_order<const K: usize>(object: &ObjectDef, columns: [&str; K]) -> [usize; K] {
    let mut ranks = [0_usize; K];
    let mut order = [0_usize; K];
    let mut index = 0;
    // rank each column by its place in the key
    while index < K {
        ranks[index] = match key_rank(object, columns[index]) {
            Some(rank) => rank,
            None => fail(&[
                "`",
                columns[index],
                "` isn't a key column of ",
                object.name(),
            ]),
        };
        order[index] = index;
        index += 1;
    }
    // sort the indexes by rank with an insertion sort
    let mut sorted = 1;
    while sorted < K {
        let mut slot = sorted;
        while slot > 0 && ranks[order[slot - 1]] > ranks[order[slot]] {
            order.swap(slot - 1, slot);
            slot -= 1;
        }
        sorted += 1;
    }
    order
}

/// Count an object's key columns with one role
///
/// # Arguments
///
/// * `object` - The object's definition
/// * `partition` - Whether to count partition key columns (or clustering columns)
const fn count_keys(object: &ObjectDef, partition: bool) -> usize {
    let columns = object.columns();
    let mut count = 0;
    let mut index = 0;
    // count the columns with the requested role
    while index < columns.len() {
        match columns[index].role {
            ColumnRole::Partition(_) if partition => count += 1,
            ColumnRole::Cluster(..) if !partition => count += 1,
            _ => {}
        }
        index += 1;
    }
    count
}

/// Whether a list of columns holds a column with some key rank
///
/// # Arguments
///
/// * `object` - The object's definition
/// * `columns` - The columns to search
/// * `rank` - The key rank to look for
const fn has_rank(object: &ObjectDef, columns: &[&str], rank: usize) -> bool {
    let mut index = 0;
    while index < columns.len() {
        if matches!(key_rank(object, columns[index]), Some(found) if found == rank) {
            return true;
        }
        index += 1;
    }
    false
}

/// Check that a statement's restrictions name key columns once each and cover the key the
/// way the statement needs: the whole partition key, then a prefix of the clustering key,
/// with a range only on the last restricted clustering column
///
/// # Arguments
///
/// * `object` - The object the statement targets
/// * `columns` - The restricted columns
/// * `ops` - How each restricted column is compared, in the order of `columns`
/// * `coverage` - Which key columns the restrictions must cover
/// * `label` - The statement, such as `SamplesQueries.get`, for errors
pub const fn assert_restrictions(
    object: &ObjectDef,
    columns: &[&str],
    ops: &[Restriction],
    coverage: Coverage,
    label: &str,
) {
    let mut index = 0;
    // every restricted column is a key column, named once
    while index < columns.len() {
        if key_rank(object, columns[index]).is_none() {
            fail(&[
                label,
                ": `",
                columns[index],
                "` isn't a key column of ",
                object.name(),
            ]);
        }
        let mut other = index + 1;
        while other < columns.len() {
            if str_eq(columns[index], columns[other]) {
                fail(&[label, ": `", columns[index], "` is restricted twice"]);
            }
            other += 1;
        }
        index += 1;
    }
    // the whole partition key must be restricted
    let partitions = count_keys(object, true);
    let mut position = 0;
    while position < partitions {
        if !has_rank(object, columns, position) {
            fail(&[
                label,
                ": the partition key of ",
                object.name(),
                " isn't fully restricted",
            ]);
        }
        position += 1;
    }
    // the restricted clustering columns must be a prefix of the clustering key
    let clusters = count_keys(object, false);
    let prefix = columns.len() - partitions;
    let mut position = 0;
    while position < prefix {
        if !has_rank(object, columns, 256 + position) {
            fail(&[
                label,
                ": the restricted clustering columns aren't a prefix of the key of ",
                object.name(),
            ]);
        }
        position += 1;
    }
    // some statements need exactly the partition key or the whole key
    match coverage {
        Coverage::Partition if prefix != 0 => {
            fail(&[
                label,
                ": a partition statement restricts only the partition key",
            ]);
        }
        Coverage::Key if prefix != clusters => {
            fail(&[label, ": a by-key statement restricts every key column"]);
        }
        _ => {}
    }
    assert_ranges(object, columns, ops, label);
}

/// Check that a statement's ranges follow CQL's slice rules: a partition column takes only
/// `=` or `IN`, and no column is restricted after a ranged clustering column
///
/// # Arguments
///
/// * `object` - The object the statement targets
/// * `columns` - The restricted columns, each a key column
/// * `ops` - How each restricted column is compared, in the order of `columns`
/// * `label` - The statement, for errors
const fn assert_ranges(object: &ObjectDef, columns: &[&str], ops: &[Restriction], label: &str) {
    let partitions = count_keys(object, true);
    let mut index = 0;
    // check each ranged column; `=` and `IN` restrictions are limited only by the key checks
    while index < columns.len() {
        if !matches!(ops[index], Restriction::Eq | Restriction::In) {
            let Some(rank) = key_rank(object, columns[index]) else {
                fail(&[label, ": `", columns[index], "` isn't a key column"]);
            };
            // a partition column is hashed, so it can't be compared by order
            if rank < partitions {
                fail(&[
                    label,
                    ": `",
                    columns[index],
                    "` is a partition column, so it takes only `eq` or `in`",
                ]);
            }
            // a range must be on the last restricted clustering column
            let mut other = 0;
            while other < columns.len() {
                if matches!(key_rank(object, columns[other]), Some(later) if later > rank) {
                    fail(&[
                        label,
                        ": `",
                        columns[other],
                        "` is restricted after the range on `",
                        columns[index],
                        "`",
                    ]);
                }
                other += 1;
            }
        }
        index += 1;
    }
}

/// Check that a statement's `GROUP BY` columns are a prefix of its object's primary key
///
/// # Arguments
///
/// * `object` - The object the statement targets
/// * `columns` - The grouped columns, in order
/// * `label` - The statement, for errors
pub const fn assert_group_by(object: &ObjectDef, columns: &[&str], label: &str) {
    let partitions = count_keys(object, true);
    let mut index = 0;
    // each grouped column must hold the next place in the key
    while index < columns.len() {
        let expected = if index < partitions {
            index
        } else {
            256 + index - partitions
        };
        match key_rank(object, columns[index]) {
            Some(rank) if rank == expected => {}
            _ => fail(&[
                label,
                ": GROUP BY must name a prefix of the key of ",
                object.name(),
            ]),
        }
        index += 1;
    }
}

/// Check that a `SELECT DISTINCT` reads only partition key columns, as Scylla requires
///
/// # Arguments
///
/// * `columns` - The projection's selected columns
/// * `object` - The object the statement reads
/// * `label` - The statement, for errors
pub const fn assert_distinct(columns: &[&str], object: &ObjectDef, label: &str) {
    let mut index = 0;
    // every selected column must be a column of the partition key
    while index < columns.len() {
        match find_column(object, columns[index]) {
            Some(ColumnDef {
                role: ColumnRole::Partition(_),
                ..
            }) => {}
            _ => fail(&[
                label,
                ": a distinct select reads only partition key columns of ",
                object.name(),
                ", not `",
                columns[index],
                "`",
            ]),
        }
        index += 1;
    }
}

/// Check that a census selects only the columns it groups by and aggregates; with no grouped
/// columns it groups by the whole partition key
///
/// # Arguments
///
/// * `columns` - The projection's selected columns, aggregates included
/// * `object` - The object the census counts
/// * `group_by` - The grouped columns, or none for the partition key
/// * `label` - The statement, for errors
pub const fn assert_census_columns(
    columns: &[&str],
    object: &ObjectDef,
    group_by: &[&str],
    label: &str,
) {
    let mut index = 0;
    // every selected column must be an aggregate or a grouped column
    while index < columns.len() {
        // an aggregate counts the group's rows
        if !is_aggregate(columns[index]) {
            // a plain column must be one the census groups by
            let grouped = if group_by.is_empty() {
                matches!(
                    find_column(object, columns[index]),
                    Some(ColumnDef {
                        role: ColumnRole::Partition(_),
                        ..
                    })
                )
            } else {
                contains(group_by, columns[index])
            };
            if !grouped {
                fail(&[
                    label,
                    ": a census selects only the columns it groups by, not `",
                    columns[index],
                    "`",
                ]);
            }
        }
        index += 1;
    }
}

/// Check that every column a projection selects directly is a column of the object a
/// statement reads it from
///
/// # Arguments
///
/// * `columns` - The projection's selected columns, aggregates included
/// * `object` - The object the statement reads
/// * `label` - The statement, for errors
pub const fn assert_selects_from(columns: &[&str], object: &ObjectDef, label: &str) {
    let mut index = 0;
    while index < columns.len() {
        // aggregates select an expression, not a column
        if !is_aggregate(columns[index]) && find_column(object, columns[index]).is_none() {
            fail(&[
                label,
                ": ",
                object.name(),
                " has no column `",
                columns[index],
                "`",
            ]);
        }
        index += 1;
    }
}

/// Check that a projection redacts every column it selects directly that the object a
/// statement reads it from redacts, whatever objects the projection itself names
///
/// # Arguments
///
/// * `columns` - The projection's selected columns, aggregates included
/// * `redacted` - The columns of the projection's redacted fields
/// * `object` - The object the statement reads
/// * `object_redacted` - The columns that object redacts
/// * `label` - The statement, for errors
pub const fn assert_selects_redacted(
    columns: &[&str],
    redacted: &[&str],
    object: &ObjectDef,
    object_redacted: &[&str],
    label: &str,
) {
    let mut index = 0;
    // every column the object redacts must be read into a redacted field
    while index < columns.len() {
        if contains(object_redacted, columns[index]) && !contains(redacted, columns[index]) {
            fail(&[
                label,
                ": the projection reads `",
                columns[index],
                "`, which ",
                object.name(),
                " redacts; mark its field #[thorium(redact)]",
            ]);
        }
        index += 1;
    }
}

/// Whether a selected column is an aggregate expression like `count(*) AS count`
///
/// # Arguments
///
/// * `column` - The selected column
const fn is_aggregate(column: &str) -> bool {
    let bytes = column.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'(' || bytes[index] == b' ' {
            return true;
        }
        index += 1;
    }
    false
}

/// Which kind of key column an `IN` list restricts
///
/// # Arguments
///
/// * `object` - The object the statement targets
/// * `column` - The restricted column
#[must_use]
pub const fn in_kind(object: &ObjectDef, column: &str) -> InKind {
    match find_column(object, column) {
        Some(ColumnDef {
            role: ColumnRole::Partition(_),
            ..
        }) => InKind::PartitionKey,
        _ => InKind::Clustering,
    }
}

/// A list bound to a column that no statement restricts with `IN`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotAnInColumn {
    /// The object the column belongs to
    pub object: &'static str,
    /// The column
    pub column: &'static str,
}

impl std::fmt::Display for NotAnInColumn {
    /// Name the object and column
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{} isn't a key column, so it can't be bound as an IN list",
            self.object, self.column
        )
    }
}

impl std::error::Error for NotAnInColumn {}

/// Build the error for a list bound to a column that isn't a key column
///
/// # Arguments
///
/// * `object` - The object the column belongs to
/// * `column` - The column
#[must_use]
pub fn not_an_in_column(object: &'static str, column: &'static str) -> SerializationError {
    SerializationError::new(NotAnInColumn { object, column })
}

/// A value bound by a cursor to a column that isn't a key column
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotAKeyColumn {
    /// The object the column belongs to
    pub object: &'static str,
    /// The column
    pub column: &'static str,
}

impl std::fmt::Display for NotAKeyColumn {
    /// Name the object and column
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{} isn't a key column, so a cursor can't bind it",
            self.object, self.column
        )
    }
}

impl std::error::Error for NotAKeyColumn {}

/// Build the error for a cursor value bound to a column that isn't a key column
///
/// # Arguments
///
/// * `object` - The object the column belongs to
/// * `column` - The column
#[must_use]
pub fn not_a_key_column(object: &'static str, column: &'static str) -> SerializationError {
    SerializationError::new(NotAKeyColumn { object, column })
}

/// Find the position of a column among a projection's selected columns, in a const context
///
/// # Arguments
///
/// * `columns` - The projection's selected columns
/// * `name` - The column's name
/// * `label` - The cursor, for the error
///
/// # Panics
///
/// When the projection doesn't select the column, failing the build
#[must_use]
pub const fn column_index(columns: &[&str], name: &str, label: &str) -> usize {
    let mut index = 0;
    // walk the selected columns until one has this name
    while index < columns.len() {
        if str_eq(columns[index], name) {
            return index;
        }
        index += 1;
    }
    fail(&[
        label,
        ": its projection doesn't select `",
        name,
        "`, which the cursor reads",
    ])
}

/// Check that a cursor's partition columns are partition key columns, each named once, that
/// together make up the whole partition key
///
/// # Arguments
///
/// * `object` - The object the cursor reads
/// * `columns` - The cursor's partition columns
/// * `label` - The cursor, for errors
pub const fn assert_cursor_partition(object: &ObjectDef, columns: &[&str], label: &str) {
    let partitions = count_keys(object, true);
    let mut index = 0;
    // each column is a partition column named once
    while index < columns.len() {
        match key_rank(object, columns[index]) {
            Some(rank) if rank < partitions => {}
            _ => fail(&[
                label,
                ": `",
                columns[index],
                "` isn't a partition key column of ",
                object.name(),
            ]),
        }
        let mut other = index + 1;
        while other < columns.len() {
            if str_eq(columns[index], columns[other]) {
                fail(&[label, ": `", columns[index], "` is named twice"]);
            }
            other += 1;
        }
        index += 1;
    }
    // every partition column is named
    if columns.len() != partitions {
        fail(&[
            label,
            ": the cursor doesn't name every partition key column of ",
            object.name(),
        ]);
    }
}

/// Check that a cursor's clustering columns are a prefix of the clustering key, in key order
///
/// # Arguments
///
/// * `object` - The object the cursor reads
/// * `columns` - The cursor's clustering columns, the range or sort column first
/// * `label` - The cursor, for errors
pub const fn assert_cursor_clustering(object: &ObjectDef, columns: &[&str], label: &str) {
    let mut index = 0;
    // each column holds the next place in the clustering key
    while index < columns.len() {
        match key_rank(object, columns[index]) {
            Some(rank) if rank == 256 + index => {}
            _ => fail(&[
                label,
                ": `",
                columns[index],
                "` isn't the next clustering column of ",
                object.name(),
            ]),
        }
        index += 1;
    }
}

/// Check that a time cursor consumes all of its tie columns in one direction, so a tie query
/// reads them with one comparison
///
/// # Arguments
///
/// * `object` - The object the cursor reads
/// * `range` - The range column
/// * `ties` - The tie columns
/// * `order` - The consumption order the cursor names, if any
/// * `label` - The cursor, for errors
pub const fn assert_tie_orders(
    object: &ObjectDef,
    range: &str,
    ties: &[&str],
    order: Option<Order>,
    label: &str,
) {
    let first = tie_order(object, range, ties[0], order);
    let mut index = 1;
    // every later tie column must be consumed like the first
    while index < ties.len() {
        if !tie_order(object, range, ties[index], order).same(first) {
            fail(&[
                label,
                ": the tie columns are consumed in different directions, so one comparison \
                 can't resume them",
            ]);
        }
        index += 1;
    }
}

/// Check that an unpaged time cursor consumes its range column newest first, the order its
/// pages are filled in
///
/// # Arguments
///
/// * `object` - The object the cursor reads
/// * `range` - The range column
/// * `order` - The consumption order the cursor names, if any
/// * `label` - The cursor, for errors
pub const fn assert_newest_first(
    object: &ObjectDef,
    range: &str,
    order: Option<Order>,
    label: &str,
) {
    if !cursor_order(object, range, order).same(Order::Desc) {
        fail(&[
            label,
            ": a time cursor fills its pages newest first, so it must consume its range \
             column descending",
        ]);
    }
}

/// Whether a column is one an object redacts, in a const context
///
/// # Arguments
///
/// * `redacted` - The object's redacted columns
/// * `column` - The column
#[must_use]
pub const fn is_redacted(redacted: &[&str], column: &str) -> bool {
    contains(redacted, column)
}

/// Whether a list of names holds a name, in a const context
///
/// # Arguments
///
/// * `names` - The names to search
/// * `name` - The name to look for
const fn contains(names: &[&str], name: &str) -> bool {
    let mut index = 0;
    while index < names.len() {
        if str_eq(names[index], name) {
            return true;
        }
        index += 1;
    }
    false
}

/// Check that a subset insert writes every key column of its table, each once
///
/// # Arguments
///
/// * `object` - The table inserted into
/// * `columns` - The inserted columns
/// * `label` - The statement, for errors
pub const fn assert_insert_columns(object: &ObjectDef, columns: &[&str], label: &str) {
    let table = object.columns();
    let mut index = 0;
    // every key column of the table must be written
    while index < table.len() {
        if table[index].role.is_key() && !contains(columns, table[index].name) {
            fail(&[
                label,
                ": an insert writes every key column, but not `",
                table[index].name,
                "`",
            ]);
        }
        index += 1;
    }
    // and no column is written twice
    let mut index = 0;
    while index < columns.len() {
        let mut other = index + 1;
        while other < columns.len() {
            if str_eq(columns[index], columns[other]) {
                fail(&[label, ": `", columns[index], "` is written twice"]);
            }
            other += 1;
        }
        index += 1;
    }
}

/// Check that an update sets only regular or static columns, since a key column can't change
///
/// # Arguments
///
/// * `object` - The table updated
/// * `columns` - The set columns
/// * `label` - The statement, for errors
pub const fn assert_set_columns(object: &ObjectDef, columns: &[&str], label: &str) {
    let mut index = 0;
    while index < columns.len() {
        if key_rank(object, columns[index]).is_some() {
            fail(&[
                label,
                ": `",
                columns[index],
                "` is a key column, so an update can't set it",
            ]);
        }
        index += 1;
    }
}
