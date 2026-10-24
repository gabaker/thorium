//! Fingerprints of canonical schema forms and the revision snapshots built from them

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::LazyLock;

use super::definition::{ClusterOrder, ColumnDef, ColumnRole, ObjectDef};
use super::elastic::ElasticIndexDef;
use super::registry::{SchemaDefinition, SchemaObject};
use crate::models::upgrades::{LayoutDef, RevisionId, SchemaBackend};

/// The sha256 of a canonical form, as lowercase hex
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Get this fingerprint as lowercase hex
    #[must_use]
    pub fn as_str(&self) -> &str {
        // borrow the hex digest
        &self.0
    }
}

/// A released revision's full canonical schema, committed as
/// `api/schema/revisions/<revision>.json`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionSnapshot {
    /// The revision this snapshot belongs to
    pub revision: RevisionId,
    /// The deployment fingerprint: the sha256 of the backend fingerprints
    pub deployment: Fingerprint,
    /// Each backend's fingerprint: the sha256 of its sorted `(name, object fingerprint)` list
    pub backends: BTreeMap<SchemaBackend, Fingerprint>,
    /// Every object's canonical definition and fingerprint, sorted by backend and name
    pub objects: Vec<SnapshotObject>,
}

/// One object of a snapshot (also the form the probe reports what it observed in)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotObject {
    /// The backend the object belongs to
    pub backend: SchemaBackend,
    /// The object's name
    pub name: String,
    /// The sha256 of `canonical`
    pub fingerprint: Fingerprint,
    /// The canonical JSON form: sorted keys and columns, one spelling per type, server defaults
    /// stripped; for a layout, its family (or bucket role) mapped to its version
    pub canonical: serde_json::Value,
}

/// A CQL type's text that doesn't parse as a type
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeParseError {
    /// The text that didn't parse
    pub text: String,
    /// Why it didn't parse
    pub reason: &'static str,
}

impl std::fmt::Display for TypeParseError {
    /// Describe the type that didn't parse
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "`{}` isn't a CQL type: {}", self.text, self.reason)
    }
}

impl std::error::Error for TypeParseError {}

/// One token of a CQL type's text
#[derive(Debug, Clone, PartialEq, Eq)]
enum TypeToken {
    /// A type name, lowercased unless it was quoted
    Name(String),
    /// `<`
    Open,
    /// `>`
    Close,
    /// `,`
    Comma,
}

/// Split a CQL type's text into tokens, lowercasing unquoted names
///
/// # Arguments
///
/// * `text` - The type's text
fn type_tokens(text: &str) -> Result<Vec<TypeToken>, &'static str> {
    // walk the text one character at a time
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // whitespace only separates tokens
            c if c.is_whitespace() => (),
            '<' => tokens.push(TypeToken::Open),
            '>' => tokens.push(TypeToken::Close),
            ',' => tokens.push(TypeToken::Comma),
            // a quoted name (a user-defined type) keeps its case and quotes
            '"' => {
                let mut name = String::from('"');
                loop {
                    match chars.next() {
                        Some('"') if chars.peek() == Some(&'"') => {
                            chars.next();
                            name.push_str("\"\"");
                        }
                        Some('"') => break,
                        Some(c) => name.push(c),
                        None => return Err("an unterminated quoted name"),
                    }
                }
                name.push('"');
                tokens.push(TypeToken::Name(name));
            }
            // an unquoted name is case-insensitive, so it is lowercased
            c if c.is_ascii_alphanumeric() || c == '_' => {
                let mut name = c.to_ascii_lowercase().to_string();
                while let Some(next) = chars
                    .peek()
                    .filter(|next| next.is_ascii_alphanumeric() || **next == '_')
                {
                    name.push(next.to_ascii_lowercase());
                    chars.next();
                }
                tokens.push(TypeToken::Name(name));
            }
            _ => return Err("an unexpected character"),
        }
    }
    Ok(tokens)
}

/// Parse one type from tokens, rendering it canonically
///
/// # Arguments
///
/// * `tokens` - The remaining tokens
fn parse_type(
    tokens: &mut std::iter::Peekable<std::vec::IntoIter<TypeToken>>,
) -> Result<String, &'static str> {
    // a type starts with its name, with `varchar` spelled as `text`
    let name = match tokens.next() {
        Some(TypeToken::Name(name)) if name == "varchar" => "text".to_owned(),
        Some(TypeToken::Name(name)) => name,
        _ => return Err("a missing type name"),
    };
    // a type without parameters is complete
    if tokens.peek() != Some(&TypeToken::Open) {
        return Ok(name);
    }
    // skip the opening bracket and read each parameter up to the closing bracket
    tokens.next();
    let mut parameters = vec![parse_type(tokens)?];
    loop {
        match tokens.next() {
            Some(TypeToken::Comma) => parameters.push(parse_type(tokens)?),
            Some(TypeToken::Close) => break,
            _ => return Err("an unclosed parameter list"),
        }
    }
    // collections and frozen take a fixed number of parameters
    let expected = match name.as_str() {
        "list" | "set" | "frozen" => Some(1),
        "map" => Some(2),
        _ => None,
    };
    if expected.is_some_and(|expected| expected != parameters.len()) {
        return Err("the wrong number of type parameters");
    }
    Ok(format!("{name}<{}>", parameters.join(", ")))
}

/// Normalize a CQL type's text to its one canonical spelling
///
/// Names are lowercased (quoted names are kept as written), `varchar` becomes `text`, `frozen<…>`
/// is kept, and parameters are separated by `, `, so the result matches how the derives render
/// the same type.
///
/// # Arguments
///
/// * `text` - The type as CQL text, such as `FROZEN<MAP<VARCHAR, INT>>`
pub fn canonical_type(text: &str) -> Result<String, TypeParseError> {
    // build the error for this text
    let error = |reason| TypeParseError {
        text: text.to_owned(),
        reason,
    };
    // parse exactly one type from the text's tokens
    let mut tokens = type_tokens(text).map_err(error)?.into_iter().peekable();
    let canonical = parse_type(&mut tokens).map_err(error)?;
    if tokens.next().is_some() {
        return Err(error("text after the type"));
    }
    Ok(canonical)
}

/// Write a JSON value as canonical text onto a string
///
/// # Arguments
///
/// * `value` - The value to write
/// * `out` - The string to write onto
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        // an object's keys are written in sorted order, whatever order the map keeps
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        // an array keeps its order
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        // scalars use serde_json's compact rendering
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// Render a JSON value as canonical text: compact, with every object's keys sorted
///
/// # Arguments
///
/// * `value` - The value to render
#[must_use]
pub fn canonical_json(value: &Value) -> String {
    // write the value with sorted keys
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

impl Fingerprint {
    /// Fingerprint a canonical form: the sha256 of its canonical text
    ///
    /// # Arguments
    ///
    /// * `canonical` - The canonical form to hash
    #[must_use]
    pub fn of(canonical: &Value) -> Fingerprint {
        // hash the canonical text and spell the digest as lowercase hex
        let digest = Sha256::digest(canonical_json(canonical).as_bytes());
        let mut hex = String::with_capacity(64);
        for byte in digest {
            // writing to a String can't fail
            let _ = write!(hex, "{byte:02x}");
        }
        Fingerprint(hex)
    }
}

/// Build the canonical form of a list of columns: sorted by name, each with its canonical type
///
/// # Arguments
///
/// * `columns` - The object's columns
fn canonical_columns(columns: &[ColumnDef]) -> Value {
    // sort the columns by name so their source order doesn't matter
    let mut sorted = columns.iter().collect::<Vec<_>>();
    sorted.sort_by_key(|column| column.name);
    Value::Array(
        sorted
            .into_iter()
            .map(|column| json!({ "name": column.name, "type": column.desc.render() }))
            .collect(),
    )
}

/// Build the canonical key of an object: its partition key and its full clustering order, both
/// in key order
///
/// # Arguments
///
/// * `columns` - The object's columns
fn canonical_key(columns: &[ColumnDef]) -> (Value, Value) {
    // collect the key columns with their positions
    let mut partition = Vec::new();
    let mut clustering = Vec::new();
    for column in columns {
        match column.role {
            ColumnRole::Partition(position) => partition.push((position, json!(column.name))),
            ColumnRole::Cluster(position, order) => {
                let order = match order {
                    ClusterOrder::Asc => "asc",
                    ClusterOrder::Desc => "desc",
                };
                clustering.push((position, json!({ "name": column.name, "order": order })));
            }
            ColumnRole::Static | ColumnRole::Regular => (),
        }
    }
    // put each key in key order
    partition.sort_by_key(|(position, _)| *position);
    clustering.sort_by_key(|(position, _)| *position);
    let keys = |columns: Vec<(u8, Value)>| {
        Value::Array(columns.into_iter().map(|(_, column)| column).collect())
    };
    (keys(partition), keys(clustering))
}

/// Build the canonical form of a Scylla table or view
///
/// Columns are sorted by name, the keys keep their order and every clustering column's order,
/// and a view records only its extra filter, with whitespace collapsed. The default TTL, the
/// partition-size setting and the columns' fill flags are configuration, so they stay out.
///
/// # Arguments
///
/// * `def` - The object's definition
fn canonical_scylla(def: &ObjectDef) -> Value {
    // both kinds of object record their keys the same way
    let (partition_key, clustering_key) = canonical_key(def.columns());
    match def {
        ObjectDef::Table(table) => {
            // list the static columns by name
            let mut statics = table
                .columns
                .iter()
                .filter(|column| column.role == ColumnRole::Static)
                .map(|column| column.name)
                .collect::<Vec<_>>();
            statics.sort_unstable();
            json!({
                "kind": "table",
                "name": table.name,
                "columns": canonical_columns(table.columns),
                "partition_key": partition_key,
                "clustering_key": clustering_key,
                "static": statics,
            })
        }
        ObjectDef::View(view) => {
            // collapse the filter's whitespace so spacing doesn't matter
            let filter = view
                .filter
                .map(|filter| filter.split_whitespace().collect::<Vec<_>>().join(" "));
            json!({
                "kind": "view",
                "name": view.name,
                "base": view.base,
                "columns": canonical_columns(view.columns),
                "partition_key": partition_key,
                "clustering_key": clustering_key,
                "filter": filter,
            })
        }
    }
}

/// Remove the defaults Elastic adds to a mapping that a request didn't contain
///
/// Removed: `"type": "object"` beside `properties`, `"index": true`, `"doc_values": true`, and
/// `"store": false`, each the default for the fields Thorium maps.
///
/// # Arguments
///
/// * `value` - The mapping (or part of one) to strip in place
fn strip_elastic_defaults(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // drop the defaults at this level
            if map.contains_key("properties") && map.get("type") == Some(&json!("object")) {
                map.remove("type");
            }
            for (key, default) in [("index", true), ("doc_values", true), ("store", false)] {
                if map.get(key) == Some(&Value::Bool(default)) {
                    map.remove(key);
                }
            }
            // then strip every nested mapping
            map.values_mut().for_each(strip_elastic_defaults);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_elastic_defaults),
        _ => (),
    }
}

/// Build the canonical form of an Elastic index: its declared body with server defaults
/// stripped
///
/// # Arguments
///
/// * `def` - The index's definition
fn canonical_elastic(def: &ElasticIndexDef) -> Value {
    // strip the defaults Elastic would add, so an observed mapping compares equal
    let mut body = def.body.clone();
    strip_elastic_defaults(&mut body);
    json!({ "kind": "index", "name": def.name, "body": body })
}

/// Build the canonical form of a Redis or S3 layout: its family (or bucket role) mapped to its
/// version, such as `{"users": 2}`
///
/// # Arguments
///
/// * `layout` - The layout's definition
fn canonical_layout(layout: &LayoutDef) -> Value {
    // only the family and its version are schema: how the family is enumerated and filled is not
    let mut form = serde_json::Map::new();
    form.insert(layout.family.to_owned(), json!(layout.version));
    Value::Object(form)
}

impl SchemaDefinition {
    /// Build this definition's canonical form
    #[must_use]
    pub fn canonical(&self) -> Value {
        // build the form each backend defines
        match self {
            SchemaDefinition::Scylla(def) => canonical_scylla(def),
            SchemaDefinition::Elastic(def) => canonical_elastic(def),
            SchemaDefinition::Layout(layout) => canonical_layout(layout),
        }
    }
}

/// Every declared object's snapshot entry, sorted by backend and name
static DECLARED: LazyLock<Vec<SnapshotObject>> = LazyLock::new(|| {
    // build each declared object's entry and sort them by backend and name
    let mut objects = SchemaObject::declared()
        .iter()
        .map(SnapshotObject::of)
        .collect::<Vec<_>>();
    objects.sort_by(|left, right| (left.backend, &left.name).cmp(&(right.backend, &right.name)));
    objects
});

impl SnapshotObject {
    /// Build the snapshot entry of a declared object
    ///
    /// # Arguments
    ///
    /// * `object` - The declared object
    #[must_use]
    pub fn of(object: &SchemaObject) -> SnapshotObject {
        // build the canonical form and hash it
        let canonical = object.definition.canonical();
        SnapshotObject {
            backend: object.backend,
            name: object.name.clone(),
            fingerprint: Fingerprint::of(&canonical),
            canonical,
        }
    }

    /// Every declared object's snapshot entry, sorted by backend and name, computed once
    #[must_use]
    pub fn declared() -> &'static [SnapshotObject] {
        // build the entries on first use and borrow them after
        &DECLARED
    }
}

impl RevisionSnapshot {
    /// Compute each backend's fingerprint: the sha256 of its sorted `(name, object fingerprint)`
    /// list
    ///
    /// # Arguments
    ///
    /// * `objects` - The snapshot entries of every object
    #[must_use]
    pub fn backend_fingerprints(
        objects: &[SnapshotObject],
    ) -> BTreeMap<SchemaBackend, Fingerprint> {
        // group each backend's (name, fingerprint) pairs, sorted by name
        let mut pairs: BTreeMap<SchemaBackend, BTreeMap<&str, &Fingerprint>> = BTreeMap::new();
        for object in objects {
            pairs
                .entry(object.backend)
                .or_default()
                .insert(&object.name, &object.fingerprint);
        }
        // hash each backend's list of pairs
        pairs
            .into_iter()
            .map(|(backend, pairs)| {
                let list = pairs
                    .into_iter()
                    .map(|(name, fingerprint)| json!([name, fingerprint]))
                    .collect::<Vec<_>>();
                (backend, Fingerprint::of(&Value::Array(list)))
            })
            .collect()
    }

    /// Compute the deployment fingerprint: the sha256 of the backend fingerprints, as an object
    /// keyed by backend
    ///
    /// # Arguments
    ///
    /// * `backends` - Each backend's fingerprint
    #[must_use]
    pub fn deployment_fingerprint(backends: &BTreeMap<SchemaBackend, Fingerprint>) -> Fingerprint {
        // the backends serialize as an object keyed by their lowercase names
        Fingerprint::of(&json!(backends))
    }

    /// Build the snapshot of the schema this build declares, under a revision
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision the snapshot is recorded for
    #[must_use]
    pub fn declared(revision: RevisionId) -> RevisionSnapshot {
        // fingerprint the declared objects, then their backends, then the deployment
        let objects = SnapshotObject::declared().to_vec();
        let backends = Self::backend_fingerprints(&objects);
        RevisionSnapshot {
            revision,
            deployment: Self::deployment_fingerprint(&backends),
            backends,
            objects,
        }
    }
}
