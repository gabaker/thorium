//! Checked conversions between unsigned Rust values and their signed CQL columns
//!
//! Scylla has no unsigned types, so an unsigned field names the signed column type it's stored
//! as (`#[thorium(unsigned = "bigint")]`). A value that doesn't fit is a typed error naming the
//! table and column, never a wrapped or truncated value.

use scylla::deserialize::value::DeserializeValue;
use scylla::frame::response::result::{CollectionType, ColumnType};
use scylla::serialize::SerializationError;
use scylla::serialize::value::SerializeValue;
use scylla::serialize::writers::{CellWriter, WrittenCellProof};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{BuildHasher, Hash};

use super::cql_type::{CqlDesc, CqlNative, CqlType, nested};

/// Which way a failed conversion was going, which decides how the API reports it: a value from a
/// request is a client error, a value read from storage is a server error
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionDirection {
    /// A Rust value bound to a statement
    Write,
    /// A stored value read into a Rust field
    Read,
}

/// A checked conversion between an unsigned Rust value and its signed CQL column that failed
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsignedError {
    /// The table or view the column belongs to
    pub table: &'static str,
    /// The column the value was written to or read from
    pub column: &'static str,
    /// The value that didn't fit, rendered as text
    pub value: String,
    /// Whether the value was being written or read
    pub direction: ConversionDirection,
}

impl std::fmt::Display for UnsignedError {
    /// Describe the value and the column it didn't fit
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.direction {
            ConversionDirection::Write => write!(
                f,
                "{} doesn't fit in the column {}.{}",
                self.value, self.table, self.column
            ),
            ConversionDirection::Read => write!(
                f,
                "the stored value {} of {}.{} doesn't fit its unsigned field",
                self.value, self.table, self.column
            ),
        }
    }
}

impl std::error::Error for UnsignedError {}

/// A bound value whose column isn't the collection type its field is
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsignedTypeError {
    /// The table or view the column belongs to
    pub table: &'static str,
    /// The column being bound
    pub column: &'static str,
    /// The kind of column the field needs
    pub expected: &'static str,
}

impl std::fmt::Display for UnsignedTypeError {
    /// Describe the column and the type it should have had
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the column {}.{} isn't a {}",
            self.table, self.column, self.expected
        )
    }
}

impl std::error::Error for UnsignedTypeError {}

/// The signed CQL integer types an unsigned field can be stored as
pub mod widths {
    /// `tinyint`
    pub struct TinyInt;
    /// `smallint`
    pub struct SmallInt;
    /// `int`
    pub struct Int;
    /// `bigint`
    pub struct BigInt;
}

/// A signed CQL integer type, named by one of the [`widths`] markers
pub trait Width {
    /// The column's scalar type
    const NATIVE: CqlNative;
    /// The Rust type the driver reads and writes for this column
    type Signed: SerializeValue + for<'f, 'm> DeserializeValue<'f, 'm> + std::fmt::Display;
}

impl Width for widths::TinyInt {
    /// The `tinyint` CQL type
    const NATIVE: CqlNative = CqlNative::TinyInt;
    /// A `tinyint` is read and written as an `i8`
    type Signed = i8;
}

impl Width for widths::SmallInt {
    /// The `smallint` CQL type
    const NATIVE: CqlNative = CqlNative::SmallInt;
    /// A `smallint` is read and written as an `i16`
    type Signed = i16;
}

impl Width for widths::Int {
    /// The `int` CQL type
    const NATIVE: CqlNative = CqlNative::Int;
    /// An `int` is read and written as an `i32`
    type Signed = i32;
}

impl Width for widths::BigInt {
    /// The `bigint` CQL type
    const NATIVE: CqlNative = CqlNative::BigInt;
    /// A `bigint` is read and written as an `i64`
    type Signed = i64;
}

/// An unsigned value (or a collection of them) written to a signed column of width `W`
///
/// Only widths that hold every value of the type are implemented, except that 64-bit types go
/// in a `bigint`, the widest, where values above `i64::MAX` are refused when written.
#[diagnostic::on_unimplemented(
    message = "`{Self}` can't be stored in this column type",
    note = "an unsigned type is stored in a signed column that holds its whole range: u8 in smallint or wider, u16 in int or wider, u32, u64, and usize in bigint"
)]
pub trait UnsignedField<W: Width> {
    /// The column's CQL type
    const DESC: CqlDesc;
    /// This type's form inside a collection
    #[doc(hidden)]
    const NESTED: CqlDesc = nested(&Self::DESC);

    /// Write this value to a cell, refusing a value its column can't hold
    ///
    /// # Arguments
    ///
    /// * `table` - The table being written, for errors
    /// * `column` - The column being written, for errors
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize_checked<'b>(
        &self,
        table: &'static str,
        column: &'static str,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError>;
}

/// An unsigned field (or a collection of them) read from a signed column of width `W`
pub trait UnsignedRead<W: Width>: Sized {
    /// The signed form the driver reads from the column
    type Stored: for<'f, 'm> DeserializeValue<'f, 'm>;

    /// Convert a stored value, refusing one the field can't hold
    ///
    /// # Arguments
    ///
    /// * `stored` - The value read from the column
    /// * `table` - The table being read, for errors
    /// * `column` - The column being read, for errors
    fn from_stored(
        stored: Self::Stored,
        table: &'static str,
        column: &'static str,
    ) -> Result<Self, UnsignedError>;
}

/// Implement the unsigned traits for each unsigned type and the widths that hold it
macro_rules! unsigned {
    ($($ty:ty => [$($width:ident),*]),* $(,)?) => {
        $($(
            impl UnsignedField<widths::$width> for $ty {
                /// The unsigned type is stored in the native column type of this width
                const DESC: CqlDesc = CqlDesc::Native(<widths::$width as Width>::NATIVE);

                /// Write this value to a cell, refusing a value its column can't hold
                ///
                /// # Arguments
                ///
                /// * `table` - The table being written, for errors
                /// * `column` - The column being written, for errors
                /// * `typ` - The column's type
                /// * `writer` - The cell to write to
                fn serialize_checked<'b>(
                    &self,
                    table: &'static str,
                    column: &'static str,
                    typ: &ColumnType,
                    writer: CellWriter<'b>,
                ) -> Result<WrittenCellProof<'b>, SerializationError> {
                    // convert to the column's signed type, refusing values it can't hold
                    let signed = <widths::$width as Width>::Signed::try_from(*self).map_err(|_| {
                        SerializationError::new(UnsignedError {
                            table,
                            column,
                            value: self.to_string(),
                            direction: ConversionDirection::Write,
                        })
                    })?;
                    signed.serialize(typ, writer)
                }
            }

            impl UnsignedRead<widths::$width> for $ty {
                /// The unsigned type is read as the signed type of this width
                type Stored = <widths::$width as Width>::Signed;

                /// Convert a stored value, refusing one the field can't hold
                ///
                /// # Arguments
                ///
                /// * `stored` - The value read from the column
                /// * `table` - The table being read, for errors
                /// * `column` - The column being read, for errors
                fn from_stored(
                    stored: Self::Stored,
                    table: &'static str,
                    column: &'static str,
                ) -> Result<Self, UnsignedError> {
                    // convert from the column's signed type, refusing negative or oversized values
                    <$ty>::try_from(stored).map_err(|_| UnsignedError {
                        table,
                        column,
                        value: stored.to_string(),
                        direction: ConversionDirection::Read,
                    })
                }
            }
        )*)*
    };
}

unsigned!(
    u8 => [SmallInt, Int, BigInt],
    u16 => [Int, BigInt],
    u32 => [BigInt],
    u64 => [BigInt],
    usize => [BigInt],
);

impl<W: Width, T: UnsignedField<W> + ?Sized> UnsignedField<W> for &T {
    /// A reference is stored in the same column type as the value it points to
    const DESC: CqlDesc = T::DESC;
    /// A reference takes the same form inside a collection as the value it points to
    const NESTED: CqlDesc = T::NESTED;

    /// Write this value to a cell, refusing a value its column can't hold
    ///
    /// # Arguments
    ///
    /// * `table` - The table being written, for errors
    /// * `column` - The column being written, for errors
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize_checked<'b>(
        &self,
        table: &'static str,
        column: &'static str,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        // write the borrowed value
        (**self).serialize_checked(table, column, typ, writer)
    }
}

impl<W: Width, T: UnsignedField<W>> UnsignedField<W> for Option<T> {
    /// An optional value is stored in the same column type as the value itself
    const DESC: CqlDesc = T::DESC;
    /// An optional value takes the same form inside a collection as the value itself
    const NESTED: CqlDesc = T::NESTED;

    /// Write this value to a cell, refusing a value its column can't hold
    ///
    /// # Arguments
    ///
    /// * `table` - The table being written, for errors
    /// * `column` - The column being written, for errors
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize_checked<'b>(
        &self,
        table: &'static str,
        column: &'static str,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        // a missing value is a null column
        match self {
            Some(value) => value.serialize_checked(table, column, typ, writer),
            None => Ok(writer.set_null()),
        }
    }
}

impl<W: Width, T: UnsignedRead<W>> UnsignedRead<W> for Option<T> {
    /// A null column reads as `None`, otherwise as the value's stored type
    type Stored = Option<T::Stored>;

    /// Convert a stored value, refusing one the field can't hold
    ///
    /// # Arguments
    ///
    /// * `stored` - The value read from the column
    /// * `table` - The table being read, for errors
    /// * `column` - The column being read, for errors
    fn from_stored(
        stored: Self::Stored,
        table: &'static str,
        column: &'static str,
    ) -> Result<Self, UnsignedError> {
        // convert a present value and keep a null one
        stored
            .map(|stored| T::from_stored(stored, table, column))
            .transpose()
    }
}

/// Write a list or set of unsigned values, converting each element as it is written
///
/// # Arguments
///
/// * `len` - The number of elements
/// * `elements` - The elements to write
/// * `table` - The table being written, for errors
/// * `column` - The column being written, for errors
/// * `typ` - The column's type
/// * `writer` - The cell to write to
fn serialize_sequence<'t, 'b, W: Width, T: UnsignedField<W> + 't>(
    len: usize,
    elements: impl Iterator<Item = &'t T>,
    table: &'static str,
    column: &'static str,
    typ: &ColumnType,
    writer: CellWriter<'b>,
) -> Result<WrittenCellProof<'b>, SerializationError> {
    // the column must be a list or a set
    let ColumnType::Collection {
        typ: CollectionType::List(element_type) | CollectionType::Set(element_type),
        ..
    } = typ
    else {
        return Err(SerializationError::new(UnsignedTypeError {
            table,
            column,
            expected: "list or set",
        }));
    };
    let mut builder = writer.into_value_builder();
    // write the element count, then each element
    let count = i32::try_from(len).map_err(|_| {
        SerializationError::new(UnsignedError {
            table,
            column,
            value: len.to_string(),
            direction: ConversionDirection::Write,
        })
    })?;
    builder.append_bytes(&count.to_be_bytes());
    for element in elements {
        element.serialize_checked(table, column, element_type, builder.make_sub_writer())?;
    }
    builder.finish().map_err(SerializationError::new)
}

/// Write a map whose values are unsigned, converting each value as it is written
///
/// # Arguments
///
/// * `len` - The number of entries
/// * `entries` - The entries to write
/// * `table` - The table being written, for errors
/// * `column` - The column being written, for errors
/// * `typ` - The column's type
/// * `writer` - The cell to write to
fn serialize_mapping<'t, 'b, W: Width, K: SerializeValue + 't, V: UnsignedField<W> + 't>(
    len: usize,
    entries: impl Iterator<Item = (&'t K, &'t V)>,
    table: &'static str,
    column: &'static str,
    typ: &ColumnType,
    writer: CellWriter<'b>,
) -> Result<WrittenCellProof<'b>, SerializationError> {
    // the column must be a map
    let ColumnType::Collection {
        typ: CollectionType::Map(key_type, value_type),
        ..
    } = typ
    else {
        return Err(SerializationError::new(UnsignedTypeError {
            table,
            column,
            expected: "map",
        }));
    };
    let mut builder = writer.into_value_builder();
    // write the entry count, then each key and value
    let count = i32::try_from(len).map_err(|_| {
        SerializationError::new(UnsignedError {
            table,
            column,
            value: len.to_string(),
            direction: ConversionDirection::Write,
        })
    })?;
    builder.append_bytes(&count.to_be_bytes());
    for (key, value) in entries {
        key.serialize(key_type, builder.make_sub_writer())?;
        value.serialize_checked(table, column, value_type, builder.make_sub_writer())?;
    }
    builder.finish().map_err(SerializationError::new)
}

impl<W: Width, T: UnsignedField<W>> UnsignedField<W> for [T] {
    /// A slice is stored as a CQL `list` of its element type, frozen when that is a collection
    const DESC: CqlDesc = CqlDesc::List(&T::NESTED);

    /// Write this value to a cell, refusing a value its column can't hold
    ///
    /// # Arguments
    ///
    /// * `table` - The table being written, for errors
    /// * `column` - The column being written, for errors
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize_checked<'b>(
        &self,
        table: &'static str,
        column: &'static str,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        serialize_sequence(self.len(), self.iter(), table, column, typ, writer)
    }
}

impl<W: Width, T: UnsignedField<W>> UnsignedField<W> for Vec<T> {
    /// A vector is stored as a CQL `list` of its element type, frozen when that is a collection
    const DESC: CqlDesc = CqlDesc::List(&T::NESTED);

    /// Write this value to a cell, refusing a value its column can't hold
    ///
    /// # Arguments
    ///
    /// * `table` - The table being written, for errors
    /// * `column` - The column being written, for errors
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize_checked<'b>(
        &self,
        table: &'static str,
        column: &'static str,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        serialize_sequence(self.len(), self.iter(), table, column, typ, writer)
    }
}

impl<W: Width, T: UnsignedRead<W>> UnsignedRead<W> for Vec<T> {
    /// A list column is read as a vector of the elements' stored type
    type Stored = Vec<T::Stored>;

    /// Convert a stored value, refusing one the field can't hold
    ///
    /// # Arguments
    ///
    /// * `stored` - The value read from the column
    /// * `table` - The table being read, for errors
    /// * `column` - The column being read, for errors
    fn from_stored(
        stored: Self::Stored,
        table: &'static str,
        column: &'static str,
    ) -> Result<Self, UnsignedError> {
        stored
            .into_iter()
            .map(|stored| T::from_stored(stored, table, column))
            .collect()
    }
}

impl<W: Width, T: UnsignedField<W>, S: BuildHasher> UnsignedField<W> for HashSet<T, S> {
    /// A hash set is stored as a CQL `set` of its element type, frozen when that is a collection
    const DESC: CqlDesc = CqlDesc::Set(&T::NESTED);

    /// Write this value to a cell, refusing a value its column can't hold
    ///
    /// # Arguments
    ///
    /// * `table` - The table being written, for errors
    /// * `column` - The column being written, for errors
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize_checked<'b>(
        &self,
        table: &'static str,
        column: &'static str,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        serialize_sequence(self.len(), self.iter(), table, column, typ, writer)
    }
}

impl<W: Width, T: UnsignedRead<W> + Eq + Hash, S: BuildHasher + Default> UnsignedRead<W>
    for HashSet<T, S>
{
    /// A set column is read as a vector of the elements' stored type, then collected
    type Stored = Vec<T::Stored>;

    /// Convert a stored value, refusing one the field can't hold
    ///
    /// # Arguments
    ///
    /// * `stored` - The value read from the column
    /// * `table` - The table being read, for errors
    /// * `column` - The column being read, for errors
    fn from_stored(
        stored: Self::Stored,
        table: &'static str,
        column: &'static str,
    ) -> Result<Self, UnsignedError> {
        stored
            .into_iter()
            .map(|stored| T::from_stored(stored, table, column))
            .collect()
    }
}

impl<W: Width, T: UnsignedField<W>> UnsignedField<W> for BTreeSet<T> {
    /// A sorted set is stored as a CQL `set` of its element type, frozen when that is a collection
    const DESC: CqlDesc = CqlDesc::Set(&T::NESTED);

    /// Write this value to a cell, refusing a value its column can't hold
    ///
    /// # Arguments
    ///
    /// * `table` - The table being written, for errors
    /// * `column` - The column being written, for errors
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize_checked<'b>(
        &self,
        table: &'static str,
        column: &'static str,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        serialize_sequence(self.len(), self.iter(), table, column, typ, writer)
    }
}

impl<W: Width, T: UnsignedRead<W> + Ord> UnsignedRead<W> for BTreeSet<T> {
    /// A set column is read as a vector of the elements' stored type, then collected
    type Stored = Vec<T::Stored>;

    /// Convert a stored value, refusing one the field can't hold
    ///
    /// # Arguments
    ///
    /// * `stored` - The value read from the column
    /// * `table` - The table being read, for errors
    /// * `column` - The column being read, for errors
    fn from_stored(
        stored: Self::Stored,
        table: &'static str,
        column: &'static str,
    ) -> Result<Self, UnsignedError> {
        stored
            .into_iter()
            .map(|stored| T::from_stored(stored, table, column))
            .collect()
    }
}

impl<W, K, V, S> UnsignedField<W> for HashMap<K, V, S>
where
    W: Width,
    K: CqlType + SerializeValue,
    V: UnsignedField<W>,
    S: BuildHasher,
{
    /// A hash map is stored as a CQL `map` of its key and value types, each frozen when it is a
    /// collection
    const DESC: CqlDesc = CqlDesc::Map(&K::NESTED, &V::NESTED);

    /// Write this value to a cell, refusing a value its column can't hold
    ///
    /// # Arguments
    ///
    /// * `table` - The table being written, for errors
    /// * `column` - The column being written, for errors
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize_checked<'b>(
        &self,
        table: &'static str,
        column: &'static str,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        serialize_mapping(self.len(), self.iter(), table, column, typ, writer)
    }
}

impl<W, K, V, S> UnsignedRead<W> for HashMap<K, V, S>
where
    W: Width,
    K: Eq + Hash + for<'f, 'm> DeserializeValue<'f, 'm>,
    V: UnsignedRead<W>,
    S: BuildHasher + Default,
{
    /// A map column is read with its keys as is and its values in their stored type
    type Stored = HashMap<K, V::Stored>;

    /// Convert a stored value, refusing one the field can't hold
    ///
    /// # Arguments
    ///
    /// * `stored` - The value read from the column
    /// * `table` - The table being read, for errors
    /// * `column` - The column being read, for errors
    fn from_stored(
        stored: Self::Stored,
        table: &'static str,
        column: &'static str,
    ) -> Result<Self, UnsignedError> {
        stored
            .into_iter()
            .map(|(key, stored)| Ok((key, V::from_stored(stored, table, column)?)))
            .collect()
    }
}

impl<W, K, V> UnsignedField<W> for BTreeMap<K, V>
where
    W: Width,
    K: CqlType + SerializeValue,
    V: UnsignedField<W>,
{
    /// A sorted map is stored as a CQL `map` of its key and value types, each frozen when it is a
    /// collection
    const DESC: CqlDesc = CqlDesc::Map(&K::NESTED, &V::NESTED);

    /// Write this value to a cell, refusing a value its column can't hold
    ///
    /// # Arguments
    ///
    /// * `table` - The table being written, for errors
    /// * `column` - The column being written, for errors
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize_checked<'b>(
        &self,
        table: &'static str,
        column: &'static str,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        serialize_mapping(self.len(), self.iter(), table, column, typ, writer)
    }
}

impl<W, K, V> UnsignedRead<W> for BTreeMap<K, V>
where
    W: Width,
    K: Ord + for<'f, 'm> DeserializeValue<'f, 'm>,
    V: UnsignedRead<W>,
{
    /// A map column is read with its keys as is and its values in their stored type
    type Stored = BTreeMap<K, V::Stored>;

    /// Convert a stored value, refusing one the field can't hold
    ///
    /// # Arguments
    ///
    /// * `stored` - The value read from the column
    /// * `table` - The table being read, for errors
    /// * `column` - The column being read, for errors
    fn from_stored(
        stored: Self::Stored,
        table: &'static str,
        column: &'static str,
    ) -> Result<Self, UnsignedError> {
        stored
            .into_iter()
            .map(|(key, stored)| Ok((key, V::from_stored(stored, table, column)?)))
            .collect()
    }
}
