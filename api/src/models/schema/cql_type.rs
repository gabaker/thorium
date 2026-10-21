//! CQL column types, described structurally and derived from Rust types

use chrono::{DateTime, Utc};
use scylla::deserialize::value::DeserializeValue;
use scylla::deserialize::{DeserializationError, FrameSlice, TypeCheckError};
use scylla::frame::response::result::{ColumnType, NativeType};
use scylla::serialize::SerializationError;
use scylla::serialize::value::SerializeValue;
use scylla::serialize::writers::{CellWriter, WrittenCellProof};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::BuildHasher;
use uuid::Uuid;

use crate::models::scylla_utils::errors::DeserializationError as StoreError;

/// A scalar CQL type, spelled one way only so aliases never reach a definition
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CqlNative {
    /// `text` (also written `varchar`)
    Text,
    /// `tinyint`, from `i8`
    TinyInt,
    /// `smallint`, from `i16`
    SmallInt,
    /// `int`, from `i32`
    Int,
    /// `bigint`, from `i64`
    BigInt,
    /// `boolean`, from `bool`
    Boolean,
    /// `float`, from `f32`
    Float,
    /// `double`, from `f64`
    Double,
    /// `uuid`, from `Uuid`
    Uuid,
    /// `timestamp`, from `DateTime<Utc>`
    Timestamp,
}

impl CqlNative {
    /// The canonical CQL spelling of this type
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            CqlNative::Text => "text",
            CqlNative::TinyInt => "tinyint",
            CqlNative::SmallInt => "smallint",
            CqlNative::Int => "int",
            CqlNative::BigInt => "bigint",
            CqlNative::Boolean => "boolean",
            CqlNative::Float => "float",
            CqlNative::Double => "double",
            CqlNative::Uuid => "uuid",
            CqlNative::Timestamp => "timestamp",
        }
    }
}

/// The structured CQL type of a column, compared structurally and rendered canonically
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CqlDesc {
    /// A scalar type
    Native(CqlNative),
    /// A `list<…>` of the inner type
    List(&'static CqlDesc),
    /// A `set<…>` of the inner type
    Set(&'static CqlDesc),
    /// A `map<…, …>` from the key type to the value type
    Map(&'static CqlDesc, &'static CqlDesc),
    /// A `frozen<…>` inner type, used for collections nested in collections
    Frozen(&'static CqlDesc),
}

impl CqlDesc {
    /// Whether this type is a collection, which must be frozen inside another collection
    #[must_use]
    pub const fn is_collection(&self) -> bool {
        matches!(self, CqlDesc::List(_) | CqlDesc::Set(_) | CqlDesc::Map(..))
    }

    /// Compare two types structurally in a const context
    ///
    /// # Arguments
    ///
    /// * `other` - The type to compare against
    #[must_use]
    pub const fn const_eq(&self, other: &CqlDesc) -> bool {
        match (self, other) {
            (CqlDesc::Native(left), CqlDesc::Native(right)) => *left as u8 == *right as u8,
            (CqlDesc::List(left), CqlDesc::List(right))
            | (CqlDesc::Set(left), CqlDesc::Set(right))
            | (CqlDesc::Frozen(left), CqlDesc::Frozen(right)) => left.const_eq(right),
            (CqlDesc::Map(left_key, left_value), CqlDesc::Map(right_key, right_value)) => {
                left_key.const_eq(right_key) && left_value.const_eq(right_value)
            }
            _ => false,
        }
    }

    /// Render this type as CQL, such as `list<frozen<map<text, int>>>`
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            CqlDesc::Native(native) => native.name().to_owned(),
            CqlDesc::List(inner) => format!("list<{}>", inner.render()),
            CqlDesc::Set(inner) => format!("set<{}>", inner.render()),
            CqlDesc::Map(key, value) => format!("map<{}, {}>", key.render(), value.render()),
            CqlDesc::Frozen(inner) => format!("frozen<{}>", inner.render()),
        }
    }
}

/// Get the form of a type used inside a collection: frozen when it's a collection itself
///
/// # Arguments
///
/// * `desc` - The element type
#[must_use]
pub const fn nested(desc: &'static CqlDesc) -> CqlDesc {
    if desc.is_collection() {
        CqlDesc::Frozen(desc)
    } else {
        *desc
    }
}

/// A Rust type that is stored in a Scylla column
///
/// There are no impls for `u8`, `u16`, `u32`, `u64`, or `usize`: unsigned fields need an explicit
/// `#[thorium(unsigned = "...")]` and go through checked conversions instead.
#[diagnostic::on_unimplemented(
    message = "the trait bound `{Self}: CqlType` is not satisfied",
    note = "unsigned integers have no CQL type; annotate the field with #[thorium(unsigned = \"bigint\")]"
)]
pub trait CqlType {
    /// The structured CQL type of this Rust type
    const DESC: CqlDesc;
    /// This type's form inside a collection
    #[doc(hidden)]
    const NESTED: CqlDesc = nested(&Self::DESC);
}

/// Implement `CqlType` for scalar types
macro_rules! native {
    ($($ty:ty => $native:ident),* $(,)?) => {
        $(
            impl CqlType for $ty {
                const DESC: CqlDesc = CqlDesc::Native(CqlNative::$native);
            }
        )*
    };
}

native!(
    String => Text,
    i8 => TinyInt,
    i16 => SmallInt,
    i32 => Int,
    i64 => BigInt,
    bool => Boolean,
    f32 => Float,
    f64 => Double,
    Uuid => Uuid,
    DateTime<Utc> => Timestamp,
);

impl<T: CqlType> CqlType for Option<T> {
    const DESC: CqlDesc = T::DESC;
    const NESTED: CqlDesc = T::NESTED;
}

impl<T: CqlType> CqlType for Vec<T> {
    const DESC: CqlDesc = CqlDesc::List(&T::NESTED);
}

impl<T: CqlType, S: BuildHasher> CqlType for HashSet<T, S> {
    const DESC: CqlDesc = CqlDesc::Set(&T::NESTED);
}

impl<T: CqlType> CqlType for BTreeSet<T> {
    const DESC: CqlDesc = CqlDesc::Set(&T::NESTED);
}

impl<K: CqlType, V: CqlType, S: BuildHasher> CqlType for HashMap<K, V, S> {
    const DESC: CqlDesc = CqlDesc::Map(&K::NESTED, &V::NESTED);
}

impl<K: CqlType, V: CqlType> CqlType for BTreeMap<K, V> {
    const DESC: CqlDesc = CqlDesc::Map(&K::NESTED, &V::NESTED);
}

/// A value stored as JSON in a `text` column, serialized and parsed in one place
///
/// Its `CqlType` is `text`, and it is written with `serde_json::to_string`, the same bytes the
/// hand-serialized JSON strings it replaces held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonText<T>(pub T);

impl<T> CqlType for JsonText<T> {
    const DESC: CqlDesc = CqlDesc::Native(CqlNative::Text);
}

impl<T: Serialize> SerializeValue for JsonText<T> {
    /// Write this value as JSON text
    ///
    /// # Arguments
    ///
    /// * `typ` - The column's type
    /// * `writer` - The cell to write to
    fn serialize<'b>(
        &self,
        typ: &ColumnType,
        writer: CellWriter<'b>,
    ) -> Result<WrittenCellProof<'b>, SerializationError> {
        // serialize the inner value as JSON
        let text = serde_json::to_string(&self.0).map_err(SerializationError::new)?;
        // write the JSON as text
        SerializeValue::serialize(&text, typ, writer)
    }
}

impl<'frame, 'metadata, T: DeserializeOwned> DeserializeValue<'frame, 'metadata> for JsonText<T> {
    /// Check that the column holds text
    ///
    /// # Arguments
    ///
    /// * `typ` - The column's type
    fn type_check(typ: &ColumnType) -> Result<(), TypeCheckError> {
        match typ {
            ColumnType::Native(NativeType::Text) => Ok(()),
            _ => Err(TypeCheckError::new(StoreError::ExpectedText)),
        }
    }

    /// Parse the stored JSON
    ///
    /// # Arguments
    ///
    /// * `_typ` - The column's type
    /// * `value` - The stored bytes, if the column isn't null
    fn deserialize(
        _typ: &'metadata ColumnType<'metadata>,
        value: Option<FrameSlice<'frame>>,
    ) -> Result<Self, DeserializationError> {
        // a null column has no JSON to parse
        let Some(value) = value else {
            return Err(DeserializationError::new(StoreError::ExpectedNotNull));
        };
        serde_json::from_slice(value.as_slice())
            .map(JsonText)
            .map_err(|_| DeserializationError::new(StoreError::UnknownValue))
    }
}
