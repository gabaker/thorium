//! A table with one field of every kind generates a borrowed row with the expected field types
#![deny(warnings)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use thorium::models::schema::{JsonText, ScyllaStoreAsStr, ScyllaStoreJson, ScyllaTable};
use uuid::Uuid;

/// A type stored as JSON text
#[derive(Debug, Serialize, Deserialize, ScyllaStoreJson)]
pub struct Settings {
    /// A setting
    pub level: i32,
}

/// A type stored as its name
#[derive(Debug, ScyllaStoreAsStr)]
pub enum Color {
    /// Red
    Red,
}

impl Color {
    /// The stored name of this color
    pub fn as_str(&self) -> &'static str {
        match self {
            Color::Red => "Red",
        }
    }
}

impl FromStr for Color {
    /// An unknown color name fails with the name itself
    type Err = String;

    /// Parse a stored color name
    ///
    /// # Arguments
    ///
    /// * `raw` - The stored name
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "Red" => Ok(Color::Red),
            _ => Err(raw.to_owned()),
        }
    }
}

/// An alias the derive can't see through
pub type Sha256 = String;

/// One field of every kind the borrowed row maps
#[derive(Debug, ScyllaTable)]
#[thorium_table(name = "every_kind")]
pub struct EveryKindTable {
    /// A text partition column
    #[thorium(partition)]
    pub text: String,
    /// A tinyint
    pub tiny: i8,
    /// An optional tinyint
    pub opt_tiny: Option<i8>,
    /// A smallint
    pub small: i16,
    /// An optional smallint
    pub opt_small: Option<i16>,
    /// An int
    pub int: i32,
    /// An optional int
    pub opt_int: Option<i32>,
    /// A bigint
    pub big: i64,
    /// An optional bigint
    pub opt_big: Option<i64>,
    /// A boolean
    pub flag: bool,
    /// An optional boolean
    pub opt_flag: Option<bool>,
    /// A float
    pub float: f32,
    /// An optional float
    pub opt_float: Option<f32>,
    /// A double
    pub double: f64,
    /// An optional double
    pub opt_double: Option<f64>,
    /// A uuid
    pub id: Uuid,
    /// An optional uuid
    pub opt_id: Option<Uuid>,
    /// A timestamp
    pub at: DateTime<Utc>,
    /// An optional timestamp
    pub opt_at: Option<DateTime<Utc>>,
    /// Optional text
    pub opt_text: Option<String>,
    /// A list
    pub list: Vec<String>,
    /// An optional list
    pub opt_list: Option<Vec<String>>,
    /// A set
    pub set: HashSet<String>,
    /// An ordered set
    pub tree_set: BTreeSet<String>,
    /// An optional set
    pub opt_set: Option<HashSet<String>>,
    /// A map
    pub map: HashMap<String, Uuid>,
    /// An ordered map
    pub tree_map: BTreeMap<String, Uuid>,
    /// An unsigned scalar
    #[thorium(unsigned = "bigint")]
    pub count: u64,
    /// A map of unsigned values
    #[thorium(unsigned = "bigint")]
    pub counts: HashMap<String, usize>,
    /// A JSON store type
    pub settings: Settings,
    /// An optional JSON store type
    pub opt_settings: Option<Settings>,
    /// A string store type
    pub color: Color,
    /// A list of a string store type
    pub colors: Vec<Color>,
    /// A JSON text wrapper
    pub json: JsonText<HashMap<String, Uuid>>,
    /// An alias
    pub alias: Sha256,
}

/// A type equal to `T`
trait SameAs<T> {}

impl<T> SameAs<T> for T {}

/// Assert a value's type is exactly `T`, without coercions
///
/// # Arguments
///
/// * `_value` - The value to check
fn expect<T, U: SameAs<T>>(_value: &U) {}

/// Assert the traits a borrowed row must have
fn traits<T: Copy + Send + Sync + std::fmt::Debug>() {}

/// Check every field of the borrowed row has the mapped type
///
/// # Arguments
///
/// * `row` - The borrowed row
fn check_fields<'a>(row: EveryKindTableRef<'a>) {
    expect::<&'a str, _>(&row.text);
    expect::<i8, _>(&row.tiny);
    expect::<Option<i8>, _>(&row.opt_tiny);
    expect::<i16, _>(&row.small);
    expect::<Option<i16>, _>(&row.opt_small);
    expect::<i32, _>(&row.int);
    expect::<Option<i32>, _>(&row.opt_int);
    expect::<i64, _>(&row.big);
    expect::<Option<i64>, _>(&row.opt_big);
    expect::<bool, _>(&row.flag);
    expect::<Option<bool>, _>(&row.opt_flag);
    expect::<f32, _>(&row.float);
    expect::<Option<f32>, _>(&row.opt_float);
    expect::<f64, _>(&row.double);
    expect::<Option<f64>, _>(&row.opt_double);
    expect::<Uuid, _>(&row.id);
    expect::<Option<Uuid>, _>(&row.opt_id);
    expect::<DateTime<Utc>, _>(&row.at);
    expect::<Option<DateTime<Utc>>, _>(&row.opt_at);
    expect::<Option<&'a str>, _>(&row.opt_text);
    expect::<&'a [String], _>(&row.list);
    expect::<Option<&'a [String]>, _>(&row.opt_list);
    expect::<&'a HashSet<String>, _>(&row.set);
    expect::<&'a BTreeSet<String>, _>(&row.tree_set);
    expect::<Option<&'a HashSet<String>>, _>(&row.opt_set);
    expect::<&'a HashMap<String, Uuid>, _>(&row.map);
    expect::<&'a BTreeMap<String, Uuid>, _>(&row.tree_map);
    expect::<u64, _>(&row.count);
    expect::<&'a HashMap<String, usize>, _>(&row.counts);
    expect::<&'a Settings, _>(&row.settings);
    expect::<Option<&'a Settings>, _>(&row.opt_settings);
    expect::<&'a Color, _>(&row.color);
    expect::<&'a [Color], _>(&row.colors);
    expect::<JsonText<&'a HashMap<String, Uuid>>, _>(&row.json);
    expect::<&'a Sha256, _>(&row.alias);
}

/// Check the borrowed row's trait bounds, `as_ref`, and field types at compile time
fn main() {
    // the borrowed row is cheap to copy and safe to hold across awaits
    traits::<EveryKindTableRef<'static>>();
    // as_ref borrows an owned row
    let _ = check_fields as fn(EveryKindTableRef<'_>);
    let _ = EveryKindTable::as_ref;
}
