//! Tests for the schema derives and their runtime support, on fixture tables

use chrono::{DateTime, TimeZone, Utc};
use scylla::deserialize::FrameSlice;
use scylla::deserialize::row::{ColumnIterator, DeserializeRow};
use scylla::deserialize::value::DeserializeValue;
use scylla::frame::response::result::{
    CollectionType, ColumnSpec, ColumnType, NativeType, TableSpec,
};
use scylla::serialize::row::{RowSerializationContext, SerializeRow};
use scylla::serialize::value::SerializeValue;
use scylla::serialize::writers::{CellWriter, RowWriter};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use uuid::Uuid;

use super::widths::{BigInt, Int, SmallInt};
use super::{
    ConversionDirection, CqlDesc, CqlNative, CqlType, JsonText, Projection, ScyllaProjection,
    ScyllaStoreAsStr, ScyllaStoreJson, ScyllaTable, UnsignedError, UnsignedField, UnsignedRead,
    Width,
};
use super::{InList, TableStatements, plan_in_chunks, run_chunks, run_chunks_any, with_keyspace};

/// A submission row shaped like `samples_list`, with its key columns declared first
#[derive(Debug, Clone, ScyllaTable)]
#[thorium_table(name = "fixture_samples")]
pub struct FixtureSamplesTable {
    /// The group this submission is in
    #[thorium(partition)]
    pub group: String,
    /// The year this submission was uploaded in
    #[thorium(partition)]
    pub year: i32,
    /// The time bucket within the year
    #[thorium(partition)]
    pub bucket: i32,
    /// When this submission was uploaded
    #[thorium(cluster = "desc")]
    pub uploaded: DateTime<Utc>,
    /// The submission id
    #[thorium(cluster = "desc")]
    pub id: Uuid,
    /// The sha256 of the file
    pub sha256: String,
    /// The sha1 of the file
    pub sha1: String,
    /// The md5 of the file
    pub md5: String,
    /// The file name given at upload
    pub name: Option<String>,
    /// The description given at upload
    pub description: Option<String>,
    /// The user who submitted the file
    pub submitter: String,
    /// Where the file came from
    pub origin: Option<String>,
}

/// The same columns as `FixtureSamplesTable`, declared in a different order
#[derive(Debug, Clone, ScyllaTable)]
#[thorium_table(name = "fixture_samples")]
pub struct ReorderedSamplesTable {
    /// Where the file came from
    pub origin: Option<String>,
    /// The user who submitted the file
    pub submitter: String,
    /// The md5 of the file
    pub md5: String,
    /// The submission id
    #[thorium(cluster = "desc")]
    pub id: Uuid,
    /// The sha1 of the file
    pub sha1: String,
    /// The group this submission is in
    #[thorium(partition)]
    pub group: String,
    /// The file name given at upload
    pub name: Option<String>,
    /// When this submission was uploaded
    #[thorium(cluster = "desc")]
    pub uploaded: DateTime<Utc>,
    /// The description given at upload
    pub description: Option<String>,
    /// The time bucket within the year
    #[thorium(partition)]
    pub bucket: i32,
    /// The sha256 of the file
    pub sha256: String,
    /// The year this submission was uploaded in
    #[thorium(partition)]
    pub year: i32,
}

/// A row with two text columns of the same type, declared in the opposite order of the columns
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = FixtureSamplesTable)]
pub struct HashesRow {
    /// The md5 of the file
    pub md5: String,
    /// The sha1 of the file
    pub sha1: String,
    /// The sha256 of the file
    pub sha256: String,
    /// The submission id, read through a rename
    #[thorium(column = "id")]
    pub submission: Uuid,
}

/// A table with an unsigned scalar and an unsigned map-value column
#[derive(Debug, Clone, ScyllaTable)]
#[thorium_table(name = "fixture_counts")]
pub struct FixtureCountsTable {
    /// The id of the counted object
    #[thorium(partition)]
    pub id: Uuid,
    /// A total
    #[thorium(unsigned = "bigint")]
    pub total: u64,
    /// The count of each kind
    #[thorium(unsigned = "bigint")]
    pub per_kind: HashMap<String, usize>,
}

/// The unsigned columns of `FixtureCountsTable`
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = FixtureCountsTable)]
pub struct CountsRow {
    /// A total
    #[thorium(unsigned = "bigint")]
    pub total: u64,
    /// The count of each kind
    #[thorium(unsigned = "bigint")]
    pub per_kind: HashMap<String, usize>,
}

/// A table holding a credential in one column
#[derive(Clone, ScyllaTable)]
#[thorium_table(name = "fixture_secrets")]
pub struct FixtureSecretsTable {
    /// The group this repo is in
    #[thorium(partition)]
    pub group: String,
    /// The repo's url
    pub url: String,
    /// The password used to clone the repo
    #[thorium(redact)]
    pub password: String,
}

/// A type stored as JSON text
#[derive(Debug, Serialize, Deserialize, ScyllaStoreJson)]
pub struct FixtureSettings {
    /// A setting
    pub level: i32,
}

/// A type stored as its name
#[derive(Debug, PartialEq, ScyllaStoreAsStr)]
pub enum FixtureColor {
    /// Red
    Red,
}

impl FixtureColor {
    /// The stored name of this color
    pub fn as_str(&self) -> &'static str {
        match self {
            FixtureColor::Red => "Red",
        }
    }
}

impl FromStr for FixtureColor {
    /// An unknown color name fails with the name itself
    type Err = String;

    /// Parse a stored color name
    ///
    /// # Arguments
    ///
    /// * `raw` - The stored name
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "Red" => Ok(FixtureColor::Red),
            _ => Err(raw.to_owned()),
        }
    }
}

/// The CQL `text` type
fn text() -> ColumnType<'static> {
    ColumnType::Native(NativeType::Text)
}

/// Build a column spec for a fixture table
///
/// # Arguments
///
/// * `name` - The column's name
/// * `typ` - The column's type
fn spec(name: &'static str, typ: ColumnType<'static>) -> ColumnSpec<'static> {
    ColumnSpec::borrowed(name, typ, TableSpec::borrowed("ks", "fixture"))
}

/// The bind columns of today's `samples.insert`, in its statement order
fn samples_insert_specs() -> Vec<ColumnSpec<'static>> {
    vec![
        spec("group", text()),
        spec("year", ColumnType::Native(NativeType::Int)),
        spec("bucket", ColumnType::Native(NativeType::Int)),
        spec("sha256", text()),
        spec("sha1", text()),
        spec("md5", text()),
        spec("id", ColumnType::Native(NativeType::Uuid)),
        spec("name", text()),
        spec("description", text()),
        spec("submitter", text()),
        spec("origin", text()),
        spec("uploaded", ColumnType::Native(NativeType::Timestamp)),
    ]
}

/// A submission with a distinct value in every column
fn fixture_samples_row() -> FixtureSamplesTable {
    FixtureSamplesTable {
        group: "minerva".to_owned(),
        year: 2026,
        bucket: 7,
        uploaded: Utc.with_ymd_and_hms(2026, 10, 19, 4, 5, 6).unwrap(),
        id: Uuid::from_u128(0x1234),
        sha256: "sha256-value".to_owned(),
        sha1: "sha1-value".to_owned(),
        md5: "md5-value".to_owned(),
        name: Some("name-value".to_owned()),
        description: None,
        submitter: "europa".to_owned(),
        origin: Some("{\"Downloaded\":{}}".to_owned()),
    }
}

/// Serialize a row against some bind columns
///
/// # Arguments
///
/// * `row` - The row to serialize
/// * `specs` - The bind columns in statement order
fn serialize_row<R: SerializeRow + ?Sized>(
    row: &R,
    specs: &[ColumnSpec<'static>],
) -> Result<Vec<u8>, scylla::serialize::SerializationError> {
    let ctx = RowSerializationContext::from_specs(specs);
    let mut buf = Vec::new();
    let mut writer = RowWriter::new(&mut buf);
    row.serialize(&ctx, &mut writer)?;
    Ok(buf)
}

/// Serialize values into the cells of one row, as Scylla sends them back
///
/// # Arguments
///
/// * `cells` - Each cell's serializer
fn row_bytes(cells: &[&dyn Fn(CellWriter<'_>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    for cell in cells {
        cell(CellWriter::new(&mut buf));
    }
    buf
}

/// Write one value into a cell of the given type
///
/// # Arguments
///
/// * `value` - The value to write
/// * `typ` - The column's type
/// * `writer` - The cell to write it to
fn put<T: SerializeValue>(value: T, typ: &ColumnType<'_>, writer: CellWriter<'_>) {
    value.serialize(typ, writer).expect("cell serializes");
}

/// Deserialize a row of a projection from cell bytes
///
/// # Arguments
///
/// * `specs` - The result's columns
/// * `bytes` - The row's cells
fn read_row<'f, P: DeserializeRow<'f, 'f>>(
    specs: &'f [ColumnSpec<'f>],
    bytes: &'f [u8],
) -> Result<P, String> {
    P::type_check(specs).map_err(|err| err.to_string())?;
    P::deserialize(ColumnIterator::new(specs, FrameSlice::new_borrowed(bytes)))
        .map_err(|err| err.to_string())
}

/// Serialize one value into a single cell's bytes
///
/// # Arguments
///
/// * `write` - The serializer to run on the cell
fn cell_bytes(
    write: impl FnOnce(CellWriter<'_>) -> Result<(), scylla::serialize::SerializationError>,
) -> Result<Vec<u8>, scylla::serialize::SerializationError> {
    let mut buf = Vec::new();
    write(CellWriter::new(&mut buf))?;
    Ok(buf)
}

/// Read one cell back through a type's deserializer
///
/// # Arguments
///
/// * `typ` - The column's type
/// * `bytes` - The cell's bytes, with their length
fn read_cell<T: for<'f, 'm> DeserializeValue<'f, 'm>>(typ: &ColumnType<'_>, bytes: &[u8]) -> T {
    let mut slice = FrameSlice::new_borrowed(bytes);
    let value = slice.read_cql_bytes().expect("cell has a length");
    T::type_check(typ).expect("cell type checks");
    T::deserialize(typ, value).expect("cell deserializes")
}

/// `CqlType` renders every scalar, and `Option<T>` renders as `T`
#[test]
fn cql_type_renders_scalars() {
    assert_eq!(<String as CqlType>::DESC.render(), "text");
    assert_eq!(<i8 as CqlType>::DESC.render(), "tinyint");
    assert_eq!(<i16 as CqlType>::DESC.render(), "smallint");
    assert_eq!(<i32 as CqlType>::DESC.render(), "int");
    assert_eq!(<i64 as CqlType>::DESC.render(), "bigint");
    assert_eq!(<bool as CqlType>::DESC.render(), "boolean");
    assert_eq!(<f32 as CqlType>::DESC.render(), "float");
    assert_eq!(<f64 as CqlType>::DESC.render(), "double");
    assert_eq!(<Uuid as CqlType>::DESC.render(), "uuid");
    assert_eq!(<DateTime<Utc> as CqlType>::DESC.render(), "timestamp");
    // an optional column has its inner type's CQL type
    assert_eq!(<Option<i64> as CqlType>::DESC, <i64 as CqlType>::DESC);
    assert_eq!(<Option<String> as CqlType>::DESC.render(), "text");
}

/// `CqlType` renders collections, freezing nested ones, and compares structurally
#[test]
fn cql_type_renders_collections() {
    assert_eq!(<Vec<String> as CqlType>::DESC.render(), "list<text>");
    assert_eq!(<HashSet<Uuid> as CqlType>::DESC.render(), "set<uuid>");
    assert_eq!(<BTreeSet<i32> as CqlType>::DESC.render(), "set<int>");
    assert_eq!(
        <HashMap<String, i64> as CqlType>::DESC.render(),
        "map<text, bigint>"
    );
    assert_eq!(
        <BTreeMap<String, Uuid> as CqlType>::DESC.render(),
        "map<text, uuid>"
    );
    assert_eq!(
        <Vec<Vec<i32>> as CqlType>::DESC.render(),
        "list<frozen<list<int>>>"
    );
    assert_eq!(
        <HashMap<String, HashSet<String>> as CqlType>::DESC.render(),
        "map<text, frozen<set<text>>>"
    );
    // equality is structural, and agrees with the rendered form
    let descs = [
        <Vec<String> as CqlType>::DESC,
        <HashSet<String> as CqlType>::DESC,
        <BTreeSet<String> as CqlType>::DESC,
        <Vec<Vec<i32>> as CqlType>::DESC,
        <HashMap<String, i64> as CqlType>::DESC,
        <HashMap<String, HashSet<String>> as CqlType>::DESC,
        CqlDesc::Native(CqlNative::Text),
    ];
    for left in &descs {
        for right in &descs {
            assert_eq!(left.const_eq(right), left.render() == right.render());
            assert_eq!(left == right, left.render() == right.render());
        }
    }
    assert_ne!(
        <Vec<String> as CqlType>::DESC,
        <HashSet<String> as CqlType>::DESC
    );
}

/// Types stored as JSON or by name are `text`, and a list of named values is `list<text>`
#[test]
fn store_derives_are_text() {
    assert_eq!(<FixtureSettings as CqlType>::DESC.render(), "text");
    assert_eq!(<FixtureColor as CqlType>::DESC.render(), "text");
    assert_eq!(<Vec<FixtureColor> as CqlType>::DESC.render(), "list<text>");
    assert_eq!(
        <JsonText<HashMap<String, Uuid>> as CqlType>::DESC.render(),
        "text"
    );
    // the store derives still write and read their usual text
    let bytes = cell_bytes(|cell| put_ok(&FixtureColor::Red, &text(), cell)).unwrap();
    assert_eq!(read_cell::<String>(&text(), &bytes), "Red");
    assert_eq!(
        read_cell::<FixtureColor>(&text(), &bytes),
        FixtureColor::Red
    );
    let settings = FixtureSettings { level: 3 };
    let bytes = cell_bytes(|cell| put_ok(&settings, &text(), cell)).unwrap();
    assert_eq!(read_cell::<String>(&text(), &bytes), "{\"level\":3}");
}

/// Write a value into a cell, returning only whether it succeeded
///
/// # Arguments
///
/// * `value` - The value to write
/// * `typ` - The column's type
/// * `cell` - The cell to write
fn put_ok<T: SerializeValue + ?Sized>(
    value: &T,
    typ: &ColumnType<'_>,
    cell: CellWriter<'_>,
) -> Result<(), scylla::serialize::SerializationError> {
    value.serialize(typ, cell).map(|_| ())
}

/// Write an unsigned value through its checked conversion
///
/// # Arguments
///
/// * `value` - The value to write
/// * `typ` - The column's type
fn write_unsigned<W: Width, T: UnsignedField<W>>(
    value: &T,
    typ: &ColumnType<'_>,
) -> Result<Vec<u8>, scylla::serialize::SerializationError> {
    cell_bytes(|cell| {
        value
            .serialize_checked("fixture_counts", "total", typ, cell)
            .map(|_| ())
    })
}

/// Round trip one unsigned value through its column type
///
/// # Arguments
///
/// * `value` - The value to round trip
/// * `typ` - The column's type
fn round_trip<W: Width, T>(value: &T, typ: &ColumnType<'_>) -> T
where
    T: UnsignedField<W> + UnsignedRead<W>,
{
    let bytes = write_unsigned::<W, T>(value, typ).expect("value fits");
    let stored = read_cell::<T::Stored>(typ, &bytes);
    T::from_stored(stored, "fixture_counts", "total").expect("stored value converts")
}

/// Unsigned values round trip at 0, 1, and the largest value each width holds
#[test]
fn unsigned_round_trips() {
    let bigint = ColumnType::Native(NativeType::BigInt);
    let int = ColumnType::Native(NativeType::Int);
    let smallint = ColumnType::Native(NativeType::SmallInt);
    let max = u64::try_from(i64::MAX).unwrap();
    for value in [0, 1, max] {
        assert_eq!(round_trip::<BigInt, u64>(&value, &bigint), value);
    }
    for value in [0, 1, usize::try_from(i64::MAX).unwrap()] {
        assert_eq!(round_trip::<BigInt, usize>(&value, &bigint), value);
    }
    for value in [0, 1, u32::MAX] {
        assert_eq!(round_trip::<BigInt, u32>(&value, &bigint), value);
    }
    for value in [0, 1, u16::MAX] {
        assert_eq!(round_trip::<Int, u16>(&value, &int), value);
    }
    for value in [0, 1, u8::MAX] {
        assert_eq!(round_trip::<SmallInt, u8>(&value, &smallint), value);
    }
    // the stored value of the largest u64 a bigint holds is i64::MAX
    let bytes = write_unsigned::<BigInt, u64>(&max, &bigint).unwrap();
    assert_eq!(read_cell::<i64>(&bigint, &bytes), i64::MAX);
}

/// Unsigned values a bigint can't hold are refused on write, naming the table and column
#[test]
fn unsigned_write_errors() {
    let bigint = ColumnType::Native(NativeType::BigInt);
    let too_big = u64::try_from(i64::MAX).unwrap() + 1;
    for value in [too_big, u64::MAX] {
        let err = write_unsigned::<BigInt, u64>(&value, &bigint).unwrap_err();
        let unsigned = err
            .downcast_ref::<UnsignedError>()
            .expect("the error is an unsigned conversion error");
        assert_eq!(unsigned.table, "fixture_counts");
        assert_eq!(unsigned.column, "total");
        assert_eq!(unsigned.value, value.to_string());
        assert_eq!(unsigned.direction, ConversionDirection::Write);
        assert!(err.to_string().contains("fixture_counts.total"), "{err}");
    }
}

/// A negative stored value is a typed error naming the table and column, never a wrapped value
#[test]
fn unsigned_read_errors() {
    let err =
        <u64 as UnsignedRead<BigInt>>::from_stored(-1, "fixture_counts", "total").unwrap_err();
    assert_eq!(err.table, "fixture_counts");
    assert_eq!(err.column, "total");
    assert_eq!(err.value, "-1");
    assert_eq!(err.direction, ConversionDirection::Read);
    assert!(err.to_string().contains("fixture_counts.total"), "{err}");
    // a value too large for the field is refused the same way
    let err =
        <u16 as UnsignedRead<Int>>::from_stored(70_000, "fixture_counts", "small").unwrap_err();
    assert!(err.to_string().contains("fixture_counts.small"), "{err}");
}

/// The column types of `FixtureCountsTable` in a result
fn counts_specs() -> Vec<ColumnSpec<'static>> {
    let bigint = ColumnType::Native(NativeType::BigInt);
    vec![
        spec("id", ColumnType::Native(NativeType::Uuid)),
        spec("total", bigint.clone()),
        spec(
            "per_kind",
            ColumnType::Collection {
                frozen: false,
                typ: CollectionType::Map(Box::new(text()), Box::new(bigint)),
            },
        ),
    ]
}

/// Unsigned map values convert and fail like unsigned scalars, on the fixture table
#[test]
fn unsigned_collection_values() {
    let specs = counts_specs();
    let mut row = FixtureCountsTable {
        id: Uuid::from_u128(7),
        total: 12,
        per_kind: HashMap::from([("Device".to_owned(), 3), ("Other".to_owned(), 9)]),
    };
    // a row in range is written by name and read back through a projection
    let bytes = serialize_row(&row.as_ref(), &specs).expect("row serializes");
    let read: CountsRow = read_row(&specs, &bytes).expect("row reads");
    assert_eq!(read.total, 12);
    assert_eq!(read.per_kind, row.per_kind);
    // a map value above the bigint range is refused on write, naming the column
    row.per_kind.insert("Huge".to_owned(), usize::MAX);
    let err = serialize_row(&row.as_ref(), &specs).unwrap_err();
    assert!(err.to_string().contains("fixture_counts.per_kind"), "{err}");
    // a negative stored map value is refused on read, naming the column
    let bigint = ColumnType::Native(NativeType::BigInt);
    let negative = HashMap::from([("Device".to_owned(), -5_i64)]);
    let bytes = row_bytes(&[
        &|cell| put(Uuid::from_u128(7), &specs[0].typ().clone(), cell),
        &|cell| put(1_i64, &bigint, cell),
        &|cell| put(&negative, &specs[2].typ().clone(), cell),
    ]);
    let err = read_row::<CountsRow>(&specs, &bytes).unwrap_err();
    assert!(err.contains("fixture_counts.per_kind"), "{err}");
}

/// The generated row deserializer matches columns by name, whatever the field order
#[test]
fn projections_read_by_name() {
    // a result whose columns come in a different order than the row's fields
    let specs = vec![
        spec("sha256", text()),
        spec("id", ColumnType::Native(NativeType::Uuid)),
        spec("sha1", text()),
        spec("md5", text()),
    ];
    let id = Uuid::from_u128(99);
    let bytes = row_bytes(&[
        &|cell| put("the-sha256", &text(), cell),
        &|cell| put(id, &ColumnType::Native(NativeType::Uuid), cell),
        &|cell| put("the-sha1", &text(), cell),
        &|cell| put("the-md5", &text(), cell),
    ]);
    let row: HashesRow = read_row(&specs, &bytes).expect("row reads");
    assert_eq!(row.sha256, "the-sha256");
    assert_eq!(row.sha1, "the-sha1");
    assert_eq!(row.md5, "the-md5");
    assert_eq!(row.submission, id);
    // the generated column list follows the fields, with renames applied
    assert_eq!(HashesRow::COLUMNS, &["md5", "sha1", "sha256", "id"]);
    // a result missing one of the row's columns fails its type check
    let missing = vec![spec("sha256", text())];
    assert!(HashesRow::type_check(&missing).is_err());
}

/// A cell no field can decode fails with an error naming the projection and the column, for
/// plain and unsigned fields alike
#[test]
fn projection_value_errors_name_the_column() {
    // a uuid cell holding 3 bytes, which no uuid decodes from
    let specs = vec![
        spec("md5", text()),
        spec("sha1", text()),
        spec("sha256", text()),
        spec("id", ColumnType::Native(NativeType::Uuid)),
    ];
    let mut bytes = row_bytes(&[
        &|cell| put("the-md5", &text(), cell),
        &|cell| put("the-sha1", &text(), cell),
        &|cell| put("the-sha256", &text(), cell),
    ]);
    bytes.extend_from_slice(&[0, 0, 0, 3, 0, 0, 1]);
    let err = read_row::<HashesRow>(&specs, &bytes).unwrap_err();
    assert!(
        err.contains("HashesRow") && err.contains("column id"),
        "{err}"
    );
    // an unsigned field's bigint cell holding 3 bytes fails the same way
    let specs = counts_specs();
    let mut bytes = row_bytes(&[&|cell| put(Uuid::from_u128(7), &specs[0].typ().clone(), cell)]);
    bytes.extend_from_slice(&[0, 0, 0, 3, 0, 0, 1]);
    bytes.extend_from_slice(&row_bytes(&[&|cell| {
        put(HashMap::<String, i64>::new(), &specs[2].typ().clone(), cell);
    }]));
    let err = read_row::<CountsRow>(&specs, &bytes).unwrap_err();
    assert!(
        err.contains("CountsRow") && err.contains("column total"),
        "{err}"
    );
}

/// The generated row serializer binds by name, for the owned row and its borrowed row, whatever
/// the field order or the statement's column order
#[test]
fn rows_bind_by_name() {
    let specs = samples_insert_specs();
    let row = fixture_samples_row();
    // the bytes expected for today's statement order, bound positionally
    let expected = serialize_row(
        &(
            &row.group,
            row.year,
            row.bucket,
            &row.sha256,
            &row.sha1,
            &row.md5,
            row.id,
            &row.name,
            &row.description,
            &row.submitter,
            &row.origin,
            row.uploaded,
        ),
        &specs,
    )
    .expect("tuple serializes");
    // the borrowed row and the owned row write the same bytes
    assert_eq!(serialize_row(&row.as_ref(), &specs).unwrap(), expected);
    assert_eq!(serialize_row(&row, &specs).unwrap(), expected);
    // a table declaring the same columns in another order writes the same bytes
    let reordered = ReorderedSamplesTable {
        origin: row.origin.clone(),
        submitter: row.submitter.clone(),
        md5: row.md5.clone(),
        id: row.id,
        sha1: row.sha1.clone(),
        group: row.group.clone(),
        name: row.name.clone(),
        uploaded: row.uploaded,
        description: row.description.clone(),
        bucket: row.bucket,
        sha256: row.sha256.clone(),
        year: row.year,
    };
    assert_eq!(
        serialize_row(&reordered.as_ref(), &specs).unwrap(),
        expected
    );
    // a bind column the row doesn't have is an error naming the table and column
    let mut extra = specs.clone();
    extra.push(spec("nope", text()));
    let err = serialize_row(&row.as_ref(), &extra).unwrap_err();
    assert!(err.to_string().contains("fixture_samples.nope"), "{err}");
}

/// One borrowed row per group differs only in its group and borrows every shared value
#[test]
fn fan_out_borrows_shared_values() {
    let source = fixture_samples_row();
    let groups = vec!["minerva".to_owned(), "mars".to_owned(), "vesta".to_owned()];
    // build one row per group from one source
    let rows = groups
        .iter()
        .map(|group| FixtureSamplesTableRef {
            group,
            ..source.as_ref()
        })
        .collect::<Vec<_>>();
    for (row, group) in rows.iter().zip(&groups) {
        assert_eq!(row.group, group.as_str());
        assert_eq!(row.year, source.year);
        assert_eq!(row.id, source.id);
        // shared values point into the source's buffers, so nothing was cloned
        assert_eq!(row.sha256.as_ptr(), source.sha256.as_ptr());
        assert_eq!(row.submitter.as_ptr(), source.submitter.as_ptr());
        assert_eq!(
            row.name.map(str::as_ptr),
            source.name.as_deref().map(str::as_ptr)
        );
        assert_eq!(row.group.as_ptr(), group.as_ptr());
    }
}

/// A redacted field prints `<redacted>` in every generated `Debug`, and other fields print as
/// usual
#[test]
fn redacted_fields_never_print() {
    let row = FixtureSecretsTable {
        group: "minerva".to_owned(),
        url: "https://github.com/curl/curl".to_owned(),
        password: "hunter2".to_owned(),
    };
    for printed in [format!("{row:?}"), format!("{:?}", row.as_ref())] {
        assert!(printed.contains("<redacted>"), "{printed}");
        assert!(!printed.contains("hunter2"), "{printed}");
        assert!(printed.contains("minerva"), "{printed}");
        assert!(
            printed.contains("https://github.com/curl/curl"),
            "{printed}"
        );
    }
}

/// Compare generated text with a committed golden file, rewriting it when `THORIUM_BLESS=1`
///
/// # Arguments
///
/// * `name` - The golden file's name under `api/src/models/schema/goldens`
/// * `actual` - The generated text
fn assert_golden(name: &str, actual: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/models/schema/goldens")
        .join(name);
    // rewrite the golden file on request instead of comparing
    if std::env::var("THORIUM_BLESS").is_ok_and(|value| value == "1") {
        std::fs::write(&path, actual).expect("golden file is written");
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} can't be read: {err}", path.display()));
    assert_eq!(
        actual, expected,
        "{name} changed; rerun with THORIUM_BLESS=1 to accept the new text"
    );
}

/// Render a table's full-row statements, one per labeled section
///
/// # Arguments
///
/// * `table` - The table's name, for the labels
fn level_one_statements<T: TableStatements>(table: &str) -> String {
    [
        ("insert", T::INSERT),
        ("select_all", T::SELECT_ALL),
        ("scan", T::SCAN),
        ("census", T::CENSUS),
    ]
    .iter()
    .map(|(kind, cql)| format!("-- {table}.{kind}\n{cql}\n"))
    .collect::<Vec<_>>()
    .concat()
}

/// The full-row insert, select-all, token-range scan, and census statements of the fixture
/// tables match their committed text
#[test]
fn level_one_statements_match_goldens() {
    let rendered = [
        level_one_statements::<FixtureSamplesTable>("fixture_samples"),
        level_one_statements::<FixtureCountsTable>("fixture_counts"),
        level_one_statements::<FixtureSecretsTable>("fixture_secrets"),
    ]
    .concat();
    assert_golden("fixture_statements.cql", &rendered);
    // the statements name the keyspace through a placeholder the caller fills
    assert_eq!(
        with_keyspace(
            <FixtureCountsTable as TableStatements>::SELECT_ALL,
            "testing_su1"
        ),
        "SELECT id, total, per_kind FROM testing_su1.fixture_counts"
    );
}

/// The execution plan for the given `IN` lists, as the length of each chunk
///
/// # Arguments
///
/// * `lists` - The `IN` lists of one statement
fn chunk_lengths(lists: &[InList]) -> Vec<Vec<usize>> {
    plan_in_chunks(lists)
        .into_iter()
        .map(|ranges| ranges.into_iter().map(|range| range.len()).collect())
        .collect()
}

/// `IN` lists are split into the fewest executions within the chunk size and Scylla's limit on
/// each kind of key's cartesian product, and the executions' rows are concatenated in order
#[tokio::test]
async fn in_lists_are_chunked() {
    // one partition-key list of each size takes this many executions
    for (len, executions) in [(0, 0), (1, 1), (100, 1), (101, 2), (250, 3)] {
        let plan = plan_in_chunks(&[InList::partition_key(len)]);
        assert_eq!(plan.len(), executions, "{len} items");
    }
    // every item comes back once, in order, across the executions
    let items = (0..250).collect::<Vec<u32>>();
    let plan = plan_in_chunks(&[InList::partition_key(items.len())]);
    let rows = run_chunks(&plan, |ranges| {
        let rows = items[ranges[0].clone()].to_vec();
        async move { Ok::<_, String>(rows) }
    })
    .await
    .unwrap();
    assert_eq!(rows, items);
    // a per-statement chunk size is honored
    assert_eq!(
        chunk_lengths(&[InList::partition_key(101).chunk(50)]),
        vec![vec![50], vec![50], vec![1]]
    );
    // sha256s on a view's partition key times groups on a clustering column, 50 at a time
    let get_many = chunk_lengths(&[
        InList::partition_key(120).chunk(50),
        InList::clustering(60).chunk(50),
    ]);
    assert_eq!(get_many.len(), 6);
    assert!(
        get_many
            .iter()
            .all(|sizes| sizes[0] <= 50 && sizes[1] <= 50)
    );
    assert_eq!(
        get_many
            .iter()
            .map(|sizes| sizes[0] * sizes[1])
            .sum::<usize>(),
        120 * 60
    );
    // two partition-key lists keep their product within Scylla's limit
    let pull = chunk_lengths(&[InList::partition_key(3), InList::partition_key(40)]);
    assert!(
        pull.iter().all(|sizes| sizes[0] * sizes[1] <= 100),
        "{pull:?}"
    );
    assert_eq!(
        pull.iter().map(|sizes| sizes[0] * sizes[1]).sum::<usize>(),
        3 * 40
    );
    // an empty list means nothing to run
    assert_eq!(
        plan_in_chunks(&[InList::partition_key(3), InList::clustering(0)]).len(),
        0
    );
}

/// "Any" mode stops at the first execution with a hit
#[tokio::test]
async fn in_lists_stop_at_the_first_hit() {
    let plan = plan_in_chunks(&[InList::partition_key(250)]);
    let mut executed = 0;
    let found = run_chunks_any(&plan, |ranges| {
        executed += 1;
        // only the second chunk holds item 150
        let hit = ranges[0].contains(&150);
        async move { Ok::<_, String>(hit) }
    })
    .await
    .unwrap();
    assert!(found);
    assert_eq!(executed, 2);
    // with no hit every chunk runs
    let mut executed = 0;
    let found = run_chunks_any(&plan, |_| {
        executed += 1;
        async move { Ok::<_, String>(false) }
    })
    .await
    .unwrap();
    assert!(!found);
    assert_eq!(executed, 3);
}

/// The integration test config with its Scylla settings replaced
///
/// # Arguments
///
/// * `nodes` - The nodes to connect to
/// * `auth` - The credentials to connect with, if any
fn scylla_conf(nodes: &[&str], auth: Option<(&str, &str)>) -> crate::Conf {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/thorium-testing.yml");
    let mut conf = crate::Conf::new(path).expect("the test config loads");
    conf.scylla.nodes = nodes.iter().map(ToString::to_string).collect();
    conf.scylla.setup_time = 7;
    conf.scylla.auth = auth.map(|(username, password)| crate::conf::ScyllaAuth {
        username: username.to_owned(),
        password: password.to_owned(),
    });
    conf
}

/// The shared session builder takes its nodes, credentials, and connect timeout from the config
#[test]
fn session_builder_follows_the_config() {
    // nodes and the connect timeout come from the config
    let conf = scylla_conf(&["127.0.0.1:9042", "10.0.0.2:9043"], None);
    let builder = super::session_builder(&conf);
    assert_eq!(builder.config.known_nodes.len(), 2);
    assert_eq!(
        builder.config.connect_timeout,
        std::time::Duration::from_secs(7)
    );
    // no credentials means no authenticator
    assert!(builder.config.authenticator.is_none());
    // credentials add an authenticator
    let conf = scylla_conf(&["127.0.0.1:9042"], Some(("thorium", "corn")));
    assert!(super::session_builder(&conf).config.authenticator.is_some());
}

/// A data-shaping record's `Debug` output hides the files password's hash
#[test]
fn config_record_debug_redacts_the_files_password() {
    // build a record holding an argon2id hash
    let record = super::ConfigRecord {
        partition_sizes: BTreeMap::from([("files".to_owned(), 7)]),
        earliest: BTreeMap::from([("files".to_owned(), 0)]),
        files_password: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHQ$aGFzaGhhc2hoYXNo".to_owned(),
    };
    // the hash never reaches the debug text, while the settings do
    let debug = format!("{record:?}");
    assert!(!debug.contains("$argon2id"), "{debug}");
    assert!(debug.contains("<redacted>"), "{debug}");
    assert!(debug.contains("partition_sizes"), "{debug}");
}

/// Migration enums serialize as their variant names in JSON and are stored as `snake_case` text
#[test]
fn migration_enums_json_and_stored_text_differ() {
    // JSON uses the variant names
    assert_eq!(
        serde_json::to_string(&super::ObjectClass::Unrecognized).unwrap(),
        "\"Unrecognized\""
    );
    assert_eq!(
        serde_json::to_string(&super::MigrationRunStatus::DoneWithIssues).unwrap(),
        "\"DoneWithIssues\""
    );
    // the stored text is snake_case
    assert_eq!(super::ObjectClass::Unrecognized.as_str(), "unrecognized");
    assert_eq!(
        super::MigrationRunStatus::DoneWithIssues.as_str(),
        "done_with_issues"
    );
}
