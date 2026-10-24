//! Building reason codes for objects a migration can't convert
//!
//! A code comes from serde's classification of the error and the path of the failing field, never
//! from the error's text, so no stored value (a token, an unknown variant's name, a map key) can
//! reach a code. `serde_json::from_str` alone can't produce these codes: every shape error
//! classifies as `Data`, and a path tracker that reports struct fields and map keys alike can't
//! hide a secret key. So a value is parsed in two phases: its syntax into a generic value, then
//! its shape from that value through a deserializer that knows which maps are structs.

use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, EnumAccess, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};
use serde_value::{Value, ValueDeserializer};
use std::cell::{Cell, RefCell};
use std::fmt;

use super::{IssueCategory, IssueReason, PathSegment, ReasonBuilder};

/// A deserialization error that keeps only its classification
///
/// Its `Display` writes the category alone, so the error is safe to log, and the constructors
/// serde calls drop the message and every value it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonError {
    /// The kind of problem
    pub category: IssueCategory,
    /// A field name the error itself carries: the missing field of a `missing_field` error
    pub field: String,
}

impl ReasonError {
    /// Build an error of one category
    ///
    /// # Arguments
    ///
    /// * `category` - The kind of problem
    fn of(category: IssueCategory) -> Self {
        // no field name is known until the path is
        ReasonError {
            category,
            field: String::new(),
        }
    }
}

impl fmt::Display for ReasonError {
    /// Write the category only
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // the message is never kept, so only the category can be shown
        f.write_str(self.category.as_str())
    }
}

impl std::error::Error for ReasonError {}

impl de::Error for ReasonError {
    /// Any other error is an invalid value; the message is dropped
    ///
    /// # Arguments
    ///
    /// * `msg` - The message, which is never kept
    fn custom<T: fmt::Display>(msg: T) -> Self {
        // the message may hold stored data, so it is dropped unread
        let _ = msg;
        ReasonError::of(IssueCategory::InvalidValue)
    }

    /// A value of the wrong type is a bad type
    ///
    /// # Arguments
    ///
    /// * `unexp` - What was found, which is never kept
    /// * `exp` - What was expected
    fn invalid_type(unexp: de::Unexpected<'_>, exp: &dyn de::Expected) -> Self {
        // what was found may be data, so only the category is kept
        let _ = (unexp, exp);
        ReasonError::of(IssueCategory::BadType)
    }

    /// A value that breaks a rule of the layout is an invalid value
    ///
    /// # Arguments
    ///
    /// * `unexp` - What was found, which is never kept
    /// * `exp` - What was expected
    fn invalid_value(unexp: de::Unexpected<'_>, exp: &dyn de::Expected) -> Self {
        // what was found may be data, so only the category is kept
        let _ = (unexp, exp);
        ReasonError::of(IssueCategory::InvalidValue)
    }

    /// A sequence of the wrong length is an invalid value
    ///
    /// # Arguments
    ///
    /// * `len` - The length found
    /// * `exp` - What was expected
    fn invalid_length(len: usize, exp: &dyn de::Expected) -> Self {
        // a length breaks a rule of the layout
        let _ = (len, exp);
        ReasonError::of(IssueCategory::InvalidValue)
    }

    /// A variant the layout doesn't define
    ///
    /// # Arguments
    ///
    /// * `variant` - The variant found, which is data and never kept
    /// * `expected` - The variants the layout defines
    fn unknown_variant(variant: &str, expected: &'static [&'static str]) -> Self {
        // the variant's name is data, so it is dropped
        let _ = (variant, expected);
        ReasonError::of(IssueCategory::UnknownVariant)
    }

    /// A field the layout doesn't define, reported at its parent's path
    ///
    /// # Arguments
    ///
    /// * `field` - The field found, which is data and never kept
    /// * `expected` - The fields the layout defines
    fn unknown_field(field: &str, expected: &'static [&'static str]) -> Self {
        // the field's name is data, so it is dropped
        let _ = (field, expected);
        ReasonError::of(IssueCategory::UnknownField)
    }

    /// A required field that is absent, named by the layout's own static name
    ///
    /// # Arguments
    ///
    /// * `field` - The field the layout requires
    fn missing_field(field: &'static str) -> Self {
        // the name is the layout's, not data, so it stays
        ReasonError {
            category: IssueCategory::MissingField,
            field: field.to_owned(),
        }
    }

    /// A field given twice is an invalid value
    ///
    /// # Arguments
    ///
    /// * `field` - The field given twice
    fn duplicate_field(field: &'static str) -> Self {
        // a duplicate breaks a rule of the layout
        let _ = field;
        ReasonError::of(IssueCategory::InvalidValue)
    }
}

/// The path walked so far, and the path at the first failure
#[derive(Default)]
struct Track {
    /// The segments from the root to where the walk is
    path: RefCell<Vec<PathSegment>>,
    /// The segments at the first failure, once there is one
    failed: RefCell<Option<Vec<PathSegment>>>,
}

impl Track {
    /// Record where a failure first surfaced, and pass the error on
    ///
    /// # Arguments
    ///
    /// * `error` - The error on its way up
    fn fail<E>(&self, error: E) -> E {
        // only the deepest wrapper sees the path as it was at the failure
        let mut failed = self.failed.borrow_mut();
        if failed.is_none() {
            *failed = Some(self.path.borrow().clone());
        }
        error
    }
}

/// Render a path as a reason code's field: names joined by dots, `*` for keys and indices
///
/// # Arguments
///
/// * `path` - The path to render
fn render(path: &[PathSegment]) -> String {
    // join the segments with dots, rendering data segments as `*`
    path.iter()
        .map(|segment| match segment {
            PathSegment::Field(name) => *name,
            PathSegment::Any => "*",
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Join a parent path and a field name
///
/// # Arguments
///
/// * `parent` - The parent path, possibly empty
/// * `field` - The name to add
fn join(parent: &str, field: &str) -> String {
    // an empty parent leaves the name alone
    match (parent.is_empty(), field.is_empty()) {
        (_, true) => parent.to_owned(),
        (true, false) => field.to_owned(),
        (false, false) => format!("{parent}.{field}"),
    }
}

/// A deserializer that walks a path while it delegates to another
struct PathDe<'t, D> {
    /// The deserializer delegated to
    inner: D,
    /// The path being walked
    track: &'t Track,
}

/// Forward each listed `deserialize_*` method to the inner deserializer with a wrapped visitor
macro_rules! forward {
    ($($method:ident),*) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
                // wrap the visitor so nested values walk the path
                let track = self.track;
                self.inner
                    .$method(PathVisitor { inner: visitor, track, fields: None })
                    .map_err(|error| track.fail(error))
            }
        )*
    };
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for PathDe<'_, D> {
    type Error = D::Error;

    forward!(
        deserialize_any,
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_option,
        deserialize_unit,
        deserialize_seq,
        deserialize_map,
        deserialize_identifier,
        deserialize_ignored_any
    );

    /// Read a unit struct
    ///
    /// # Arguments
    ///
    /// * `name` - The struct's name
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // a unit struct holds nothing, so there is no path to walk
        let track = self.track;
        self.inner
            .deserialize_unit_struct(
                name,
                PathVisitor {
                    inner: visitor,
                    track,
                    fields: None,
                },
            )
            .map_err(|error| track.fail(error))
    }

    /// Read a newtype struct, whose value has the struct's own path
    ///
    /// # Arguments
    ///
    /// * `name` - The struct's name
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // the wrapped value is read at the same path
        let track = self.track;
        self.inner
            .deserialize_newtype_struct(
                name,
                PathVisitor {
                    inner: visitor,
                    track,
                    fields: None,
                },
            )
            .map_err(|error| track.fail(error))
    }

    /// Read a tuple, whose elements are indices
    ///
    /// # Arguments
    ///
    /// * `len` - The tuple's length
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // elements are visited like a sequence's
        let track = self.track;
        self.inner
            .deserialize_tuple(
                len,
                PathVisitor {
                    inner: visitor,
                    track,
                    fields: None,
                },
            )
            .map_err(|error| track.fail(error))
    }

    /// Read a tuple struct, whose elements are indices
    ///
    /// # Arguments
    ///
    /// * `name` - The struct's name
    /// * `len` - The tuple's length
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // elements are visited like a sequence's
        let track = self.track;
        self.inner
            .deserialize_tuple_struct(
                name,
                len,
                PathVisitor {
                    inner: visitor,
                    track,
                    fields: None,
                },
            )
            .map_err(|error| track.fail(error))
    }

    /// Read a struct, whose declared field names keep their names in the path
    ///
    /// # Arguments
    ///
    /// * `name` - The struct's name
    /// * `fields` - The fields the struct declares
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // only a map visited as a struct may name its keys
        let track = self.track;
        self.inner
            .deserialize_struct(
                name,
                fields,
                PathVisitor {
                    inner: visitor,
                    track,
                    fields: Some(fields),
                },
            )
            .map_err(|error| track.fail(error))
    }

    /// Read an enum, whose payload has the enum's own path
    ///
    /// # Arguments
    ///
    /// * `name` - The enum's name
    /// * `variants` - The variants the enum declares
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // the payload is read at the same path
        let track = self.track;
        self.inner
            .deserialize_enum(
                name,
                variants,
                PathVisitor {
                    inner: visitor,
                    track,
                    fields: None,
                },
            )
            .map_err(|error| track.fail(error))
    }

    /// Whether the format is human readable, as the inner one says
    fn is_human_readable(&self) -> bool {
        // delegate
        self.inner.is_human_readable()
    }
}

/// A visitor that wraps the nested accessors so they walk the path
struct PathVisitor<'t, V> {
    /// The visitor delegated to
    inner: V,
    /// The path being walked
    track: &'t Track,
    /// The fields of the struct being visited, when it is one
    fields: Option<&'static [&'static str]>,
}

/// Forward each listed `visit_*` method that takes a plain value to the inner visitor
macro_rules! forward_visit {
    ($($method:ident($ty:ty)),*) => {
        $(
            fn $method<E: de::Error>(self, value: $ty) -> Result<V::Value, E> {
                // plain values hold no nested path
                self.inner.$method(value)
            }
        )*
    };
}

impl<'de, V: Visitor<'de>> Visitor<'de> for PathVisitor<'_, V> {
    type Value = V::Value;

    /// Describe what the inner visitor expects
    ///
    /// # Arguments
    ///
    /// * `formatter` - The formatter to write to
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // delegate
        self.inner.expecting(formatter)
    }

    forward_visit!(
        visit_bool(bool),
        visit_i8(i8),
        visit_i16(i16),
        visit_i32(i32),
        visit_i64(i64),
        visit_i128(i128),
        visit_u8(u8),
        visit_u16(u16),
        visit_u32(u32),
        visit_u64(u64),
        visit_u128(u128),
        visit_f32(f32),
        visit_f64(f64),
        visit_char(char),
        visit_str(&str),
        visit_string(String),
        visit_bytes(&[u8]),
        visit_byte_buf(Vec<u8>)
    );

    /// Visit a string borrowed from the input
    ///
    /// # Arguments
    ///
    /// * `value` - The string
    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        // plain values hold no nested path
        self.inner.visit_borrowed_str(value)
    }

    /// Visit bytes borrowed from the input
    ///
    /// # Arguments
    ///
    /// * `value` - The bytes
    fn visit_borrowed_bytes<E: de::Error>(self, value: &'de [u8]) -> Result<Self::Value, E> {
        // plain values hold no nested path
        self.inner.visit_borrowed_bytes(value)
    }

    /// Visit a null
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        // nothing nested
        self.inner.visit_none()
    }

    /// Visit a present optional value, read at the same path
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The value's deserializer
    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        // the value has the option's own path
        self.inner.visit_some(PathDe {
            inner: deserializer,
            track: self.track,
        })
    }

    /// Visit a unit
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        // nothing nested
        self.inner.visit_unit()
    }

    /// Visit a newtype struct, read at the same path
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The value's deserializer
    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        // the value has the struct's own path
        self.inner.visit_newtype_struct(PathDe {
            inner: deserializer,
            track: self.track,
        })
    }

    /// Visit a sequence, whose elements are indices
    ///
    /// # Arguments
    ///
    /// * `seq` - The sequence
    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
        // elements walk as `*`
        self.inner.visit_seq(PathSeq {
            inner: seq,
            track: self.track,
        })
    }

    /// Visit a map, whose keys are names only when it is a struct
    ///
    /// # Arguments
    ///
    /// * `map` - The map
    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
        // a struct's declared fields keep their names, every other key is `*`
        self.inner.visit_map(PathMap {
            inner: map,
            track: self.track,
            fields: self.fields,
            pending: None,
        })
    }

    /// Visit an enum, whose payload is read at the same path
    ///
    /// # Arguments
    ///
    /// * `data` - The enum's accessor
    fn visit_enum<A: EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        // the payload has the enum's own path
        self.inner.visit_enum(PathEnum {
            inner: data,
            track: self.track,
        })
    }
}

/// A seed that deserializes through a path-walking deserializer
struct PathSeed<'t, S> {
    /// The seed delegated to
    inner: S,
    /// The path being walked
    track: &'t Track,
}

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for PathSeed<'_, S> {
    type Value = S::Value;

    /// Deserialize the value with the path tracked
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The deserializer to read from
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        // wrap the deserializer so nested values walk the path
        self.inner.deserialize(PathDe {
            inner: deserializer,
            track: self.track,
        })
    }
}

/// A sequence whose elements walk as `*`
struct PathSeq<'t, A> {
    /// The sequence delegated to
    inner: A,
    /// The path being walked
    track: &'t Track,
}

impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for PathSeq<'_, A> {
    type Error = A::Error;

    /// Read the next element inside a `*` segment
    ///
    /// # Arguments
    ///
    /// * `seed` - The seed to read the element with
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        // the element's index is data, so the segment is `*`
        let track = self.track;
        let inner = &mut self.inner;
        track.path.borrow_mut().push(PathSegment::Any);
        let result = inner.next_element_seed(PathSeed { inner: seed, track });
        track.path.borrow_mut().pop();
        result
    }

    /// The size hint of the inner sequence
    fn size_hint(&self) -> Option<usize> {
        // delegate
        self.inner.size_hint()
    }
}

/// A map whose keys are names only for a struct's declared fields
struct PathMap<'t, A> {
    /// The map delegated to
    inner: A,
    /// The path being walked
    track: &'t Track,
    /// The fields of the struct being read, when it is one
    fields: Option<&'static [&'static str]>,
    /// The segment of the key just read, for its value
    pending: Option<PathSegment>,
}

/// A seed that notes the text of a key as it is deserialized
struct KeySeed<'c, S> {
    /// The seed delegated to
    inner: S,
    /// Where the key's text is left
    seen: &'c Cell<Option<String>>,
}

/// A visitor that notes the text of a key before delegating
struct KeyVisitor<'c, V> {
    /// The visitor delegated to
    inner: V,
    /// Where the key's text is left
    seen: &'c Cell<Option<String>>,
}

impl<'de, V: Visitor<'de>> Visitor<'de> for KeyVisitor<'_, V> {
    type Value = V::Value;

    /// Describe what the inner visitor expects
    ///
    /// # Arguments
    ///
    /// * `formatter` - The formatter to write to
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // delegate
        self.inner.expecting(formatter)
    }

    /// Note a key's text, then delegate
    ///
    /// # Arguments
    ///
    /// * `value` - The key
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        // keep the text so the map can tell a declared field from data
        self.seen.set(Some(value.to_owned()));
        self.inner.visit_str(value)
    }

    /// Note a key's text, then delegate
    ///
    /// # Arguments
    ///
    /// * `value` - The key
    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        // keep the text so the map can tell a declared field from data
        self.seen.set(Some(value.clone()));
        self.inner.visit_string(value)
    }

    /// Note a key's text, then delegate
    ///
    /// # Arguments
    ///
    /// * `value` - The key
    fn visit_borrowed_str<E: de::Error>(self, value: &'de str) -> Result<Self::Value, E> {
        // keep the text so the map can tell a declared field from data
        self.seen.set(Some(value.to_owned()));
        self.inner.visit_borrowed_str(value)
    }
}

/// A deserializer that reads a key through [`KeyVisitor`]
struct KeyDe<'c, D> {
    /// The deserializer delegated to
    inner: D,
    /// Where the key's text is left
    seen: &'c Cell<Option<String>>,
}

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for KeySeed<'_, S> {
    type Value = S::Value;

    /// Deserialize the key, noting its text
    ///
    /// # Arguments
    ///
    /// * `deserializer` - The deserializer to read from
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        // wrap the deserializer so the key's text is noted
        self.inner.deserialize(KeyDe {
            inner: deserializer,
            seen: self.seen,
        })
    }
}

/// Forward each listed `deserialize_*` method of a key with a noting visitor
macro_rules! forward_key {
    ($($method:ident),*) => {
        $(
            fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
                // note the key's text as the visitor receives it
                self.inner.$method(KeyVisitor { inner: visitor, seen: self.seen })
            }
        )*
    };
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for KeyDe<'_, D> {
    type Error = D::Error;

    forward_key!(
        deserialize_any,
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_option,
        deserialize_unit,
        deserialize_seq,
        deserialize_map,
        deserialize_identifier,
        deserialize_ignored_any
    );

    /// Read a unit struct key
    ///
    /// # Arguments
    ///
    /// * `name` - The struct's name
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // note the key's text as the visitor receives it
        self.inner.deserialize_unit_struct(
            name,
            KeyVisitor {
                inner: visitor,
                seen: self.seen,
            },
        )
    }

    /// Read a newtype struct key
    ///
    /// # Arguments
    ///
    /// * `name` - The struct's name
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // note the key's text as the visitor receives it
        self.inner.deserialize_newtype_struct(
            name,
            KeyVisitor {
                inner: visitor,
                seen: self.seen,
            },
        )
    }

    /// Read a tuple key
    ///
    /// # Arguments
    ///
    /// * `len` - The tuple's length
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // note the key's text as the visitor receives it
        self.inner.deserialize_tuple(
            len,
            KeyVisitor {
                inner: visitor,
                seen: self.seen,
            },
        )
    }

    /// Read a tuple struct key
    ///
    /// # Arguments
    ///
    /// * `name` - The struct's name
    /// * `len` - The tuple's length
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // note the key's text as the visitor receives it
        self.inner.deserialize_tuple_struct(
            name,
            len,
            KeyVisitor {
                inner: visitor,
                seen: self.seen,
            },
        )
    }

    /// Read a struct key
    ///
    /// # Arguments
    ///
    /// * `name` - The struct's name
    /// * `fields` - The fields the struct declares
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // note the key's text as the visitor receives it
        self.inner.deserialize_struct(
            name,
            fields,
            KeyVisitor {
                inner: visitor,
                seen: self.seen,
            },
        )
    }

    /// Read an enum key
    ///
    /// # Arguments
    ///
    /// * `name` - The enum's name
    /// * `variants` - The variants the enum declares
    /// * `visitor` - The visitor to hand the value to
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // note the key's text as the visitor receives it
        self.inner.deserialize_enum(
            name,
            variants,
            KeyVisitor {
                inner: visitor,
                seen: self.seen,
            },
        )
    }

    /// Whether the format is human readable, as the inner one says
    fn is_human_readable(&self) -> bool {
        // delegate
        self.inner.is_human_readable()
    }
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for PathMap<'_, A> {
    type Error = A::Error;

    /// Read the next key, deciding whether its name belongs in the path
    ///
    /// # Arguments
    ///
    /// * `seed` - The seed to read the key with
    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        // read the key, noting its text
        let noted = Cell::new(None);
        let key = self.inner.next_key_seed(KeySeed {
            inner: seed,
            seen: &noted,
        })?;
        // a key naming one of a struct's declared fields keeps its name; every other key is data
        let name = noted.take();
        self.pending = Some(
            match (self.fields, name) {
                (Some(fields), Some(name)) => fields
                    .iter()
                    .find(|field| **field == name)
                    .map(|field| PathSegment::Field(field)),
                _ => None,
            }
            .unwrap_or(PathSegment::Any),
        );
        Ok(key)
    }

    /// Read the value of the key just read, inside the key's segment
    ///
    /// # Arguments
    ///
    /// * `seed` - The seed to read the value with
    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        // walk into the key's segment while the value is read
        let segment = self.pending.take().unwrap_or(PathSegment::Any);
        let track = self.track;
        let inner = &mut self.inner;
        track.path.borrow_mut().push(segment);
        let result = inner.next_value_seed(PathSeed { inner: seed, track });
        track.path.borrow_mut().pop();
        result
    }

    /// The size hint of the inner map
    fn size_hint(&self) -> Option<usize> {
        // delegate
        self.inner.size_hint()
    }
}

/// An enum whose payload is read at the enum's own path
struct PathEnum<'t, A> {
    /// The accessor delegated to
    inner: A,
    /// The path being walked
    track: &'t Track,
}

/// A variant whose payload is read at the enum's own path
struct PathVariant<'t, A> {
    /// The accessor delegated to
    inner: A,
    /// The path being walked
    track: &'t Track,
}

impl<'de, 't, A: EnumAccess<'de>> EnumAccess<'de> for PathEnum<'t, A> {
    type Error = A::Error;
    type Variant = PathVariant<'t, A::Variant>;

    /// Read the variant
    ///
    /// # Arguments
    ///
    /// * `seed` - The seed to read the variant with
    fn variant_seed<V: DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        // read the variant, then hand its payload accessor on wrapped
        let track = self.track;
        let (value, variant) = self.inner.variant_seed(seed)?;
        Ok((
            value,
            PathVariant {
                inner: variant,
                track,
            },
        ))
    }
}

impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for PathVariant<'_, A> {
    type Error = A::Error;

    /// A unit variant has no payload
    fn unit_variant(self) -> Result<(), Self::Error> {
        // nothing nested
        self.inner.unit_variant()
    }

    /// Read a newtype variant's payload
    ///
    /// # Arguments
    ///
    /// * `seed` - The seed to read the payload with
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        // the payload has the enum's own path
        self.inner.newtype_variant_seed(PathSeed {
            inner: seed,
            track: self.track,
        })
    }

    /// Read a tuple variant's payload
    ///
    /// # Arguments
    ///
    /// * `len` - The tuple's length
    /// * `visitor` - The visitor to hand the payload to
    fn tuple_variant<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // the payload's elements are indices
        self.inner.tuple_variant(
            len,
            PathVisitor {
                inner: visitor,
                track: self.track,
                fields: None,
            },
        )
    }

    /// Read a struct variant's payload
    ///
    /// # Arguments
    ///
    /// * `fields` - The fields the variant declares
    /// * `visitor` - The visitor to hand the payload to
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        // the declared fields keep their names
        self.inner.struct_variant(
            fields,
            PathVisitor {
                inner: visitor,
                track: self.track,
                fields: Some(fields),
            },
        )
    }
}

impl ReasonBuilder {
    /// Parse a stored JSON text as a layout type, or say why it can't be
    ///
    /// The text is parsed in two phases. A syntax error is `malformed_json`; a shape error is
    /// classified by its kind and located by the path of the field that failed. The code never
    /// holds a value the text contained.
    ///
    /// # Arguments
    ///
    /// * `raw` - The stored text
    /// * `field` - The field the text was stored in, or empty when it is the whole record
    pub fn parse<T: DeserializeOwned>(raw: &str, field: &str) -> Result<T, IssueReason> {
        // phase one: the syntax, into a generic value
        let value = serde_json::from_str::<Value>(raw)
            .map_err(|_| IssueReason::new(IssueCategory::MalformedJson, field))?;
        // phase two: the shape, from that value
        Self::parse_value(value, field)
    }

    /// Read a generic value as a layout type, or say why it can't be
    ///
    /// # Arguments
    ///
    /// * `value` - The parsed value
    /// * `field` - The field the value was stored in, or empty when it is the whole record
    pub fn parse_value<T: DeserializeOwned>(value: Value, field: &str) -> Result<T, IssueReason> {
        // read the layout type through the deserializer that tracks the path
        let track = Track::default();
        let result = T::deserialize(PathDe {
            inner: ValueDeserializer::<ReasonError>::new(value),
            track: &track,
        });
        match result {
            Ok(layout) => Ok(layout),
            Err(error) => {
                // the failure's path, then the error's own field for a missing one
                let at = track.failed.take().unwrap_or_default();
                let path = join(field, &render(&at));
                let field = join(&path, &error.field);
                Err(IssueReason::new(error.category, field))
            }
        }
    }

    /// Check a record's keys against the fields its layout declares, since no layout denies
    /// unknown fields and so serde never reports one
    ///
    /// The reason names the parent path, never the unknown key, which is data.
    ///
    /// # Arguments
    ///
    /// * `keys` - The keys the record holds
    /// * `declared` - The fields the layout declares
    /// * `parent` - The path of the record, or empty for the whole record
    pub fn check_unknown<'a>(
        keys: impl IntoIterator<Item = &'a str>,
        declared: &[&str],
        parent: &str,
    ) -> Result<(), IssueReason> {
        // any key the layout doesn't declare is an unknown field
        if keys.into_iter().any(|key| !declared.contains(&key)) {
            Err(IssueReason::new(IssueCategory::UnknownField, parent))
        } else {
            Ok(())
        }
    }

    /// The reason a refused write is recorded under
    ///
    /// # Arguments
    ///
    /// * `rejected` - Whether the backend rejected the write deterministically (`rejected`), as
    ///   opposed to any other error that isn't retried (`other`)
    #[must_use]
    pub fn write_failed(rejected: bool) -> IssueReason {
        // the field slot holds the write error's kind
        IssueReason::new(
            IssueCategory::WriteFailed,
            if rejected { "rejected" } else { "other" },
        )
    }
}
