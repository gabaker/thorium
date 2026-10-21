//! The API's `Backup` schema, walked into the field locations the fixture must cover
//!
//! The schema comes from `Backup`'s `utoipa::ToSchema` derive, the same document the API serves,
//! so a field added to `Backup` or any of its parts shows up here without anyone listing it.
//! The walk names every property path (`.users[].settings.theme`), derives the value classes each
//! needs, generates `Backup` documents that hold those classes, and classifies the values of a
//! document read back from the API.

use super::classes::{
    CoverageMatrix, DERIVED_FIELDS, FIELD_DOMAINS, FieldDomain, LARGE_FIELD, Location, MANY_FIELD,
    Need, PASSWORD_HASH, UUID_KEYED_MAPS, ValueClass, WRITTEN_WHOLE, is_odd_key, is_unicode,
    json_classes, large, nested_json, odd_key, unicode,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use utoipa::{PartialSchema, ToSchema};

/// A scalar a schema leaf holds
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scalar {
    /// Text, with its format (`date-time`, `uuid`) when it has one
    String(Option<String>),
    /// An integer, with its format (`int32`, `int64`) and whether it is unsigned
    Integer {
        /// The format, `int32` or `int64`
        format: Option<String>,
        /// Whether the schema sets a minimum of zero
        unsigned: bool,
    },
    /// A floating-point number
    Number,
    /// A boolean
    Boolean,
}

/// One node of the walked schema
#[derive(Debug, Clone)]
pub enum Node {
    /// An object with named properties: each name, whether it is required, and its node
    Object(Vec<(String, bool, Node)>),
    /// A list of one kind of element
    Array(Box<Node>),
    /// A list of distinct elements whose order means nothing (a serialized set)
    Set(Box<Node>),
    /// A map from text keys to one kind of value
    Map(Box<Node>),
    /// One of several alternatives, each with the variant name the document carries
    OneOf(Vec<(String, Node)>),
    /// Text limited to a set of names
    Enum(Vec<String>),
    /// A scalar, and whether it may be null
    Scalar(Scalar, bool),
    /// Any JSON value
    Any,
    /// Only null
    Null,
    /// An object with no properties, which could hide fields
    Opaque,
}

impl Node {
    /// Returns whether a value of this node may be null
    #[must_use]
    pub fn nullable(&self) -> bool {
        // a nullable scalar, a null alternative, or a bare null
        match self {
            Node::Scalar(_, nullable) => *nullable,
            Node::OneOf(alternatives) => alternatives
                .iter()
                .any(|(_, node)| matches!(node, Node::Null)),
            Node::Null => true,
            _ => false,
        }
    }
}

/// The `Backup` schema: its root and every component it refers to
#[derive(Debug, Clone)]
pub struct BackupSchema {
    /// The root schema of `Backup`
    pub root: Value,
    /// Every component schema, by name
    pub components: BTreeMap<String, Value>,
}

impl BackupSchema {
    /// Loads the `Backup` schema from its derive
    ///
    /// # Panics
    ///
    /// Panics if the schema doesn't serialize to JSON.
    #[must_use]
    pub fn load() -> Self {
        // collect every component `Backup` refers to, then serialize everything to JSON
        let mut components = Vec::new();
        <thorium::models::Backup as ToSchema>::schemas(&mut components);
        let components = components
            .into_iter()
            .map(|(name, schema)| {
                (
                    name,
                    serde_json::to_value(schema).expect("schemas serialize"),
                )
            })
            .collect();
        let root = serde_json::to_value(<thorium::models::Backup as PartialSchema>::schema())
            .expect("schemas serialize");
        BackupSchema { root, components }
    }

    /// Walks the schema into a node tree
    #[must_use]
    pub fn tree(&self) -> Node {
        // start at the root with nothing visited
        self.walk(&self.root, &mut Vec::new())
    }

    /// Returns the walked node of one component
    ///
    /// # Arguments
    ///
    /// * `name` - The component's name
    #[must_use]
    pub fn component(&self, name: &str) -> Option<Node> {
        // walk the component on its own
        self.components
            .get(name)
            .map(|schema| self.walk(schema, &mut vec![name.to_owned()]))
    }

    /// Walks the component a reference names, or an opaque node for a recursive or unknown one
    ///
    /// # Arguments
    ///
    /// * `reference` - The `$ref` value
    /// * `visiting` - The components being walked, to stop at recursion
    fn walk_reference(&self, reference: &str, visiting: &mut Vec<String>) -> Node {
        // a component already being walked would recurse forever
        let name = reference.rsplit('/').next().unwrap_or(reference).to_owned();
        if visiting.contains(&name) {
            return Node::Opaque;
        }
        let Some(component) = self.components.get(&name) else {
            return Node::Opaque;
        };
        // walk the component with its name marked as visiting
        visiting.push(name);
        let node = self.walk(component, visiting);
        visiting.pop();
        node
    }

    /// Walks the alternatives of a `oneOf` into labeled variants
    ///
    /// # Arguments
    ///
    /// * `alternatives` - The alternatives' schema values
    /// * `visiting` - The components being walked, to stop at recursion
    fn walk_alternatives(&self, alternatives: &[Value], visiting: &mut Vec<String>) -> Node {
        // label each alternative by its only property or enum value
        let nodes = alternatives
            .iter()
            .map(|alternative| {
                let node = self.walk(alternative, visiting);
                // a referenced alternative carrying a whole struct is named by its component
                let label = match (alternative.get("$ref").and_then(Value::as_str), &node) {
                    (Some(reference), Node::Object(props)) if props.len() != 1 => {
                        reference.rsplit('/').next().unwrap_or(reference).to_owned()
                    }
                    (Some(reference), Node::OneOf(_)) => {
                        reference.rsplit('/').next().unwrap_or(reference).to_owned()
                    }
                    _ => variant_label(&node),
                };
                (label, node)
            })
            .collect();
        Node::OneOf(nodes)
    }

    /// Walks one schema value
    ///
    /// # Arguments
    ///
    /// * `schema` - The schema value
    /// * `visiting` - The components being walked, to stop at recursion
    fn walk(&self, schema: &Value, visiting: &mut Vec<String>) -> Node {
        // follow references into components
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            return self.walk_reference(reference, visiting);
        }
        // alternatives become variants named by their only property or enum value
        if let Some(alternatives) = schema.get("oneOf").and_then(Value::as_array) {
            return self.walk_alternatives(alternatives, visiting);
        }
        // allOf with one member is a wrapped reference
        if let Some(members) = schema.get("allOf").and_then(Value::as_array)
            && let Some(first) = members.first()
        {
            return self.walk(first, visiting);
        }
        // read the type, which may list null beside the real type
        let types: Vec<&str> = match schema.get("type") {
            Some(Value::String(kind)) => vec![kind.as_str()],
            Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let nullable = types.contains(&"null");
        let kind = types.iter().copied().find(|kind| *kind != "null");
        // a schema whose only type is null holds nothing else
        if nullable && kind.is_none() {
            return Node::Null;
        }
        // enums of names
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            return Node::Enum(
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect(),
            );
        }
        match kind {
            // an object lists its properties and which of them are required
            Some("object") | None if schema.get("properties").is_some() => {
                let required: Vec<&str> = schema
                    .get("required")
                    .and_then(Value::as_array)
                    .map_or_else(Vec::new, |names| {
                        names.iter().filter_map(Value::as_str).collect()
                    });
                let properties = schema["properties"]
                    .as_object()
                    .map_or_else(Vec::new, |props| {
                        props
                            .iter()
                            .map(|(name, prop)| {
                                (
                                    name.clone(),
                                    required.contains(&name.as_str()),
                                    self.walk(prop, visiting),
                                )
                            })
                            .collect()
                    });
                Node::Object(properties)
            }
            // an object without properties is a map when it types its values
            Some("object") => match schema.get("additionalProperties") {
                Some(Value::Object(values)) => Node::Map(Box::new(
                    self.walk(&Value::Object(values.clone()), visiting),
                )),
                _ => Node::Opaque,
            },
            // an array of distinct items is a set
            Some("array") => {
                let item =
                    Box::new(self.walk(schema.get("items").unwrap_or(&Value::Null), visiting));
                if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
                    Node::Set(item)
                } else {
                    Node::Array(item)
                }
            }
            Some("string") => Node::Scalar(
                Scalar::String(
                    schema
                        .get("format")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                ),
                nullable,
            ),
            Some("integer") => Node::Scalar(
                Scalar::Integer {
                    format: schema
                        .get("format")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    unsigned: schema.get("minimum").and_then(Value::as_f64) == Some(0.0),
                },
                nullable,
            ),
            Some("number") => Node::Scalar(Scalar::Number, nullable),
            Some("boolean") => Node::Scalar(Scalar::Boolean, nullable),
            Some("null") => Node::Null,
            None if schema.as_object().is_some_and(Map::is_empty) => Node::Any,
            _ => Node::Opaque,
        }
    }
}

/// Names an alternative of a `oneOf`: its enum value, its only property, or "null"
///
/// # Arguments
///
/// * `node` - The alternative's walked node
fn variant_label(node: &Node) -> String {
    // serde writes unit variants as names and data variants as single-key objects
    match node {
        Node::Enum(values) if values.len() == 1 => values[0].clone(),
        Node::Object(props) if props.len() == 1 => props[0].0.clone(),
        Node::Null => "null".to_owned(),
        Node::OneOf(inner) => inner
            .iter()
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>()
            .join("|"),
        Node::Object(_) => "object".to_owned(),
        other => format!("{other:?}").chars().take(16).collect(),
    }
}

/// Returns the domain of a field path, if one narrows it
///
/// # Arguments
///
/// * `path` - The field path
#[must_use]
pub fn field_domain(path: &str) -> Option<FieldDomain> {
    // domains are listed with their reason
    FIELD_DOMAINS
        .iter()
        .find(|(listed, ..)| *listed == path)
        .map(|(_, domain, _)| *domain)
}

/// Returns whether a field path is a record identifier
///
/// # Arguments
///
/// * `path` - The field path
#[must_use]
pub fn is_identifier(path: &str) -> bool {
    // identifiers name records
    field_domain(path) == Some(FieldDomain::Identifier)
}

/// The largest version number with every part at its maximum
const MAX_SEMVER: &str = "18446744073709551615.18446744073709551615.18446744073709551615";

/// Returns whether a map's keys must be UUIDs
///
/// # Arguments
///
/// * `path` - The map's path
#[must_use]
pub fn is_uuid_keyed(path: &str) -> bool {
    // UUID-keyed maps are listed with their reason
    UUID_KEYED_MAPS.iter().any(|(listed, _)| *listed == path)
}

/// Returns whether a field is a key, at any depth, of a record the API always writes whole
///
/// # Arguments
///
/// * `path` - The field path
#[must_use]
pub fn is_written_whole(path: &str) -> bool {
    // records written whole are listed with their reason
    WRITTEN_WHOLE.iter().any(|(record, _)| {
        path.strip_prefix(record)
            .is_some_and(|rest| !rest.is_empty())
    })
}

/// Returns whether a field is derived from other state
///
/// # Arguments
///
/// * `path` - The field path
#[must_use]
pub fn is_derived(path: &str) -> bool {
    // derived fields are listed with their source
    DERIVED_FIELDS.iter().any(|(listed, _)| *listed == path)
}

/// Returns whether a path is one of `Backup`'s top-level properties
///
/// # Arguments
///
/// * `path` - The field path
fn is_top_level(path: &str) -> bool {
    // a single leading dot and nothing nested
    path.starts_with('.') && !path[1..].contains(['.', '[', '{', '<'])
}

/// Returns the classes a scalar field needs
///
/// # Arguments
///
/// * `scalar` - The scalar
/// * `path` - The field path, which decides whether it may be empty
fn scalar_classes(scalar: &Scalar, path: &str) -> Vec<ValueClass> {
    // each scalar needs the edges of its type
    match scalar {
        Scalar::String(Some(format)) if format == "date-time" => {
            vec![ValueClass::Epoch, ValueClass::Max]
        }
        Scalar::String(Some(format)) if format == "uuid" => vec![ValueClass::Zero, ValueClass::Max],
        Scalar::String(_) if field_domain(path) == Some(FieldDomain::SemVer) => {
            vec![ValueClass::Min, ValueClass::Max]
        }
        Scalar::String(_) if field_domain(path) == Some(FieldDomain::PasswordHash) => Vec::new(),
        Scalar::String(_) if is_identifier(path) => vec![ValueClass::Unicode, ValueClass::Large],
        Scalar::String(_) => vec![ValueClass::Empty, ValueClass::Unicode, ValueClass::Large],
        Scalar::Integer {
            unsigned: true,
            format,
        } if format.as_deref() == Some("int64") => {
            vec![
                ValueClass::Zero,
                ValueClass::ConversionEdge,
                ValueClass::Max,
            ]
        }
        Scalar::Integer { unsigned: true, .. } => vec![ValueClass::Zero, ValueClass::Max],
        Scalar::Integer { .. } => vec![ValueClass::Zero, ValueClass::Min, ValueClass::Max],
        Scalar::Number => vec![ValueClass::Zero, ValueClass::MinusOne, ValueClass::Max],
        Scalar::Boolean => vec![
            ValueClass::Variant("true".to_owned()),
            ValueClass::Variant("false".to_owned()),
        ],
    }
}

/// Adds every cell a schema node needs to a matrix, and collects opaque nodes
///
/// # Arguments
///
/// * `node` - The node to require
/// * `path` - The node's path
/// * `matrix` - The matrix to add cells to
/// * `opaque` - Where opaque nodes are collected
pub fn require(node: &Node, path: &str, matrix: &mut CoverageMatrix, opaque: &mut Vec<String>) {
    // each kind of node needs its own classes, then its children's
    let location = || Location::Field {
        path: path.to_owned(),
    };
    match node {
        Node::Object(props) => {
            for (name, required, prop) in props {
                let child = format!("{path}.{name}");
                // derived fields hold what their source holds, so they need nothing of their own
                if is_derived(&child) {
                    continue;
                }
                // a key of a record the API always writes whole is never missing from it
                if !required && !is_written_whole(&child) {
                    matrix.require(
                        Location::Field {
                            path: child.clone(),
                        },
                        ValueClass::Absent,
                        Need::Required,
                    );
                }
                if prop.nullable() {
                    matrix.require(
                        Location::Field {
                            path: child.clone(),
                        },
                        ValueClass::Null,
                        Need::Required,
                    );
                }
                require(prop, &child, matrix, opaque);
            }
        }
        Node::Array(item) | Node::Set(item) => {
            // a top-level list holds every record of a namespace, so only its Many case is a
            // fixture case; empty and single-record namespaces are whole-namespace scenarios
            let sizes: &[ValueClass] = if is_top_level(path) {
                &[ValueClass::Many]
            } else if field_domain(path) == Some(FieldDomain::NonEmpty) {
                &[ValueClass::One, ValueClass::Many]
            } else {
                &[ValueClass::Empty, ValueClass::One, ValueClass::Many]
            };
            for class in sizes {
                matrix.require(location(), class.clone(), Need::Required);
            }
            require(item, &format!("{path}[]"), matrix, opaque);
        }
        Node::Map(value) => {
            for class in [ValueClass::Empty, ValueClass::One, ValueClass::Many] {
                matrix.require(location(), class, Need::Required);
            }
            if !is_uuid_keyed(path) {
                matrix.require(location(), ValueClass::OddKey, Need::Required);
            }
            require(value, &format!("{path}{{}}"), matrix, opaque);
        }
        Node::OneOf(alternatives) => {
            for (label, alternative) in alternatives {
                if matches!(alternative, Node::Null) {
                    continue;
                }
                matrix.require(
                    location(),
                    ValueClass::Variant(label.clone()),
                    Need::Required,
                );
                require(alternative, &format!("{path}<{label}>"), matrix, opaque);
            }
        }
        Node::Enum(values) => {
            for value in values {
                matrix.require(
                    location(),
                    ValueClass::Variant(value.clone()),
                    Need::Required,
                );
            }
        }
        Node::Scalar(scalar, _) => {
            for class in scalar_classes(scalar, path) {
                matrix.require(location(), class, Need::Required);
            }
        }
        Node::Any => {
            for class in [ValueClass::JsonEmpty, ValueClass::JsonNested] {
                matrix.require(location(), class, Need::Required);
            }
        }
        Node::Null => (),
        Node::Opaque => opaque.push(path.to_owned()),
    }
}

/// Which values one generated document holds
#[derive(Debug, Clone)]
pub struct Flavor {
    /// A short label naming the document's records
    pub label: String,
    /// The class every applicable scalar takes, or `None` for ordinary values
    pub scalar: Option<ValueClass>,
    /// The unsigned 64-bit value to use for conversion-edge integers
    pub conversion: u64,
    /// How many elements arrays and maps get at the targeted depth
    pub size: usize,
    /// The array and map nesting depth `size` applies to; other depths get one element
    pub size_depth: usize,
    /// Whether map keys are odd
    pub odd_keys: bool,
    /// Which variant every enum and alternative takes, modulo its variant count
    pub variant: usize,
    /// The object depth whose optional keys are removed from the stored records, as data written
    /// before those keys existed would look; depth zero is the records' own hash fields
    pub absent: Option<usize>,
    /// Whether nullable fields are set to null
    pub null: bool,
}

impl Flavor {
    /// Builds an ordinary flavor that later fields adjust
    ///
    /// # Arguments
    ///
    /// * `label` - The document's label
    #[must_use]
    pub fn plain(label: &str) -> Self {
        // one element everywhere, the first variant, every field present
        Flavor {
            label: label.to_owned(),
            scalar: None,
            conversion: i64::MAX as u64,
            size: 1,
            size_depth: 0,
            odd_keys: false,
            variant: 0,
            absent: None,
            null: false,
        }
    }
}

/// The shape of a schema that decides how many flavors cover it
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    /// The largest variant count of any enum or alternative
    pub variants: usize,
    /// The largest variant count of any alternative that carries data
    pub data_variants: usize,
    /// The deepest array and map nesting, counting the top-level record lists as one
    pub collection_depth: usize,
    /// The deepest object nesting
    pub object_depth: usize,
}

impl Shape {
    /// Measures a schema tree
    ///
    /// # Arguments
    ///
    /// * `tree` - The walked schema
    #[must_use]
    pub fn of(tree: &Node) -> Self {
        // measure each dimension separately
        Shape {
            variants: max_variants(tree),
            data_variants: data_variants(tree).max(1),
            collection_depth: collection_depth(tree),
            object_depth: object_depth(tree),
        }
    }
}

/// Every flavor the fixture writes, so that together the documents hold every field class
///
/// Classes inside an alternative only appear when the document takes that alternative, so every
/// class-carrying flavor is written once per data-carrying variant.
///
/// # Arguments
///
/// * `shape` - The shape of the schema
#[must_use]
pub fn flavors(shape: Shape) -> Vec<Flavor> {
    // the flavors that put a class somewhere, before they are crossed with variants
    let mut classed = Vec::new();
    for (label, class) in [
        ("empty", ValueClass::Empty),
        ("unicode", ValueClass::Unicode),
        ("large", ValueClass::Large),
        ("zero", ValueClass::Zero),
        ("min", ValueClass::Min),
        ("max", ValueClass::Max),
        ("minus", ValueClass::MinusOne),
        ("epoch", ValueClass::Epoch),
        ("jsonempty", ValueClass::JsonEmpty),
        ("jsonnested", ValueClass::JsonNested),
    ] {
        let mut flavor = Flavor::plain(label);
        flavor.scalar = Some(class);
        classed.push(flavor);
    }
    // both sides of the signed conversion of unsigned 64-bit fields
    for (label, value) in [
        ("edge-low", i64::MAX as u64),
        ("edge-high", i64::MAX as u64 + 1),
    ] {
        let mut flavor = Flavor::plain(label);
        flavor.scalar = Some(ValueClass::ConversionEdge);
        flavor.conversion = value;
        classed.push(flavor);
    }
    // empty and many collections at each depth under the top-level records, and odd map keys
    for depth in 1..shape.collection_depth {
        let mut empty = Flavor::plain(&format!("empty-{depth}"));
        empty.size = 0;
        empty.size_depth = depth;
        classed.push(empty);
        let mut many = Flavor::plain(&format!("many-{depth}"));
        many.size = MANY_FIELD;
        many.size_depth = depth;
        classed.push(many);
    }
    let mut odd = Flavor::plain("oddkeys");
    odd.odd_keys = true;
    classed.push(odd);
    // null fields, and optional keys missing at each object depth
    let mut null = Flavor::plain("null");
    null.null = true;
    classed.push(null);
    for depth in 0..=shape.object_depth {
        let mut absent = Flavor::plain(&format!("absent-{depth}"));
        absent.absent = Some(depth);
        classed.push(absent);
    }
    // each of those once per data-carrying variant
    let mut flavors = Vec::new();
    for variant in 0..shape.data_variants {
        for flavor in &classed {
            let mut crossed = flavor.clone();
            crossed.label = format!("{}-v{variant}", flavor.label);
            crossed.variant = variant;
            flavors.push(crossed);
        }
    }
    // no and many top-level records, and settings collections, which only the first variant needs
    for (label, size) in [("empty-0", 0), ("many-0", MANY_FIELD)] {
        let mut flavor = Flavor::plain(label);
        flavor.size = size;
        flavors.push(flavor);
    }
    // every variant of every enum and alternative
    for variant in 0..shape.variants {
        let mut flavor = Flavor::plain(&format!("variant-{variant}"));
        flavor.variant = variant;
        flavors.push(flavor);
    }
    flavors
}

/// Returns the largest variant count of any alternative under a node that carries data
///
/// # Arguments
///
/// * `node` - The node to search
#[must_use]
pub fn data_variants(node: &Node) -> usize {
    // an alternative carries data unless every choice is a bare name or null
    match node {
        Node::Object(props) => props
            .iter()
            .map(|(_, _, prop)| data_variants(prop))
            .max()
            .unwrap_or(0),
        Node::Array(item) | Node::Set(item) | Node::Map(item) => data_variants(item),
        Node::OneOf(alternatives) => {
            let inner = alternatives
                .iter()
                .map(|(_, alternative)| data_variants(alternative))
                .max()
                .unwrap_or(0);
            let carries = alternatives
                .iter()
                .any(|(_, alternative)| !matches!(alternative, Node::Enum(_) | Node::Null));
            let own = alternatives
                .iter()
                .filter(|(_, alternative)| !matches!(alternative, Node::Null))
                .count();
            if carries { inner.max(own) } else { inner }
        }
        _ => 0,
    }
}

/// Returns the deepest array and map nesting under a node
///
/// # Arguments
///
/// * `node` - The node to measure
#[must_use]
pub fn collection_depth(node: &Node) -> usize {
    // collections add a level; other containers pass the deepest child on
    match node {
        Node::Object(props) => props
            .iter()
            .map(|(_, _, prop)| collection_depth(prop))
            .max()
            .unwrap_or(0),
        Node::Array(item) | Node::Set(item) | Node::Map(item) => 1 + collection_depth(item),
        Node::OneOf(alternatives) => alternatives
            .iter()
            .map(|(_, alternative)| collection_depth(alternative))
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

/// Returns the deepest object nesting under a node, counting the node's own properties as zero
///
/// # Arguments
///
/// * `node` - The node to measure
#[must_use]
pub fn object_depth(node: &Node) -> usize {
    // an object's properties sit one level above anything inside them
    match node {
        Node::Object(props) => props
            .iter()
            .map(|(_, _, prop)| match prop {
                Node::Object(_) | Node::Array(_) | Node::Set(_) | Node::Map(_) | Node::OneOf(_) => {
                    nested_objects(prop)
                }
                _ => 0,
            })
            .max()
            .unwrap_or(0),
        Node::Array(item) | Node::Set(item) | Node::Map(item) => object_depth(item),
        Node::OneOf(alternatives) => alternatives
            .iter()
            .map(|(_, alternative)| object_depth(alternative))
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

/// Returns how many object levels sit at or under a node
///
/// # Arguments
///
/// * `node` - The node to measure
fn nested_objects(node: &Node) -> usize {
    // objects count themselves; collections and alternatives pass through
    match node {
        Node::Object(_) => 1 + object_depth(node),
        Node::Array(item) | Node::Set(item) | Node::Map(item) => nested_objects(item),
        Node::OneOf(alternatives) => alternatives
            .iter()
            .map(|(_, alternative)| nested_objects(alternative))
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

/// Returns the largest variant count of any enum or alternative under a node
///
/// # Arguments
///
/// * `node` - The node to search
#[must_use]
pub fn max_variants(node: &Node) -> usize {
    // take the widest choice anywhere in the tree
    match node {
        Node::Object(props) => props
            .iter()
            .map(|(_, _, prop)| max_variants(prop))
            .max()
            .unwrap_or(1),
        Node::Array(item) | Node::Set(item) | Node::Map(item) => max_variants(item),
        Node::OneOf(alternatives) => alternatives
            .iter()
            .map(|(_, alternative)| max_variants(alternative))
            .max()
            .unwrap_or(1)
            .max(alternatives.len()),
        Node::Enum(values) => values.len(),
        Node::Scalar(Scalar::Boolean, _) => 2,
        _ => 1,
    }
}

/// Generates JSON documents for a schema node under a flavor
pub struct Generator<'a> {
    /// The flavor of the document
    pub flavor: &'a Flavor,
    /// A counter that keeps generated text unique
    counter: usize,
}

impl<'a> Generator<'a> {
    /// Starts a generator for a flavor
    ///
    /// # Arguments
    ///
    /// * `flavor` - The flavor of the document
    #[must_use]
    pub fn new(flavor: &'a Flavor) -> Self {
        // start the counter at zero
        Generator { flavor, counter: 0 }
    }

    /// Returns unique ordinary text
    ///
    /// # Arguments
    ///
    /// * `path` - The field the text is for
    fn ordinary(&mut self, path: &str) -> String {
        // name the field and the flavor so values are readable and unique
        self.counter += 1;
        let trimmed = path.trim_end_matches(['[', ']', '{', '}']);
        let leaf = trimmed
            .rsplit(['.', '[', '{', '<', '>'])
            .next()
            .unwrap_or("value");
        format!("{leaf}-{}-{}", self.flavor.label, self.counter)
    }

    /// Generates a value for a node
    ///
    /// # Arguments
    ///
    /// * `node` - The node
    /// * `path` - The node's path
    /// * `depth` - How many arrays and maps enclose the node
    pub fn value(&mut self, node: &Node, path: &str, depth: usize) -> Value {
        // build each kind of node from its children
        match node {
            Node::Object(props) => {
                let mut object = Map::new();
                for (name, _, prop) in props {
                    let child = format!("{path}.{name}");
                    if self.flavor.null && prop.nullable() {
                        object.insert(name.clone(), Value::Null);
                        continue;
                    }
                    object.insert(name.clone(), self.value(prop, &child, depth));
                }
                Value::Object(object)
            }
            Node::Array(item) | Node::Set(item) => {
                let count = self.count(depth);
                let child = format!("{path}[]");
                Value::Array(
                    (0..count)
                        .map(|_| self.value(item, &child, depth + 1))
                        .collect(),
                )
            }
            Node::Map(value) => {
                let count = self.count(depth);
                let child = format!("{path}{{}}");
                let mut map = Map::new();
                for index in 0..count {
                    let key = if is_uuid_keyed(path) {
                        uuid::Uuid::new_v4().to_string()
                    } else if self.flavor.odd_keys {
                        odd_key(&format!("{}-{index}", self.flavor.label))
                    } else {
                        format!("key-{}-{index}", self.flavor.label)
                    };
                    map.insert(key, self.value(value, &child, depth + 1));
                }
                Value::Object(map)
            }
            Node::OneOf(alternatives) => {
                let choices: Vec<&(String, Node)> = alternatives
                    .iter()
                    .filter(|(_, alternative)| !matches!(alternative, Node::Null))
                    .collect();
                if choices.is_empty() {
                    return Value::Null;
                }
                let (label, alternative) = choices[self.flavor.variant % choices.len()];
                self.value(alternative, &format!("{path}<{label}>"), depth)
            }
            Node::Enum(values) => Value::String(values[self.flavor.variant % values.len()].clone()),
            Node::Scalar(scalar, _) => self.scalar(scalar, path),
            Node::Any => match self.flavor.scalar {
                Some(ValueClass::JsonEmpty) => json!({}),
                Some(ValueClass::JsonNested) => nested_json(),
                _ => json!("fixture"),
            },
            Node::Null | Node::Opaque => Value::Null,
        }
    }

    /// Returns how many elements a collection at a depth gets
    ///
    /// # Arguments
    ///
    /// * `depth` - How many arrays and maps enclose the collection
    fn count(&self, depth: usize) -> usize {
        // only the targeted depth takes the flavor's size
        if depth == self.flavor.size_depth {
            self.flavor.size
        } else {
            1
        }
    }

    /// Generates a scalar value
    ///
    /// # Arguments
    ///
    /// * `scalar` - The scalar
    /// * `path` - Its path
    fn scalar(&mut self, scalar: &Scalar, path: &str) -> Value {
        // take the flavor's class where it applies to this scalar, else an ordinary value
        let class = self.flavor.scalar.clone();
        let tag = self.ordinary(path);
        match scalar {
            Scalar::String(Some(format)) if format == "date-time" => match class {
                Some(ValueClass::Epoch) => json!("1970-01-01T00:00:00Z"),
                Some(ValueClass::Max) => {
                    json!(chrono::DateTime::<chrono::Utc>::MAX_UTC.to_rfc3339())
                }
                _ => json!("2025-06-15T12:00:00Z"),
            },
            Scalar::String(Some(format)) if format == "uuid" => match class {
                Some(ValueClass::Zero) => json!(uuid::Uuid::nil()),
                Some(ValueClass::Max) => json!(uuid::Uuid::max()),
                _ => json!(uuid::Uuid::new_v4()),
            },
            Scalar::String(_) if field_domain(path) == Some(FieldDomain::SemVer) => match class {
                Some(ValueClass::Min) => json!("0.0.0"),
                Some(ValueClass::Max) => json!(MAX_SEMVER),
                _ => json!("1.2.3"),
            },
            Scalar::String(_) if field_domain(path) == Some(FieldDomain::PasswordHash) => {
                json!(PASSWORD_HASH)
            }
            Scalar::String(_) => match class {
                Some(ValueClass::Empty) if !is_identifier(path) => json!(""),
                Some(ValueClass::Unicode) => json!(unicode(&tag)),
                Some(ValueClass::Large) => json!(large(&tag, LARGE_FIELD)),
                _ => json!(tag),
            },
            Scalar::Integer { format, unsigned } => {
                let wide = format.as_deref() != Some("int32");
                match (class, *unsigned, wide) {
                    (Some(ValueClass::Zero), _, _) => json!(0),
                    (Some(ValueClass::Min), false, true) => json!(i64::MIN),
                    (Some(ValueClass::Min), false, false) => json!(i32::MIN),
                    (Some(ValueClass::Max), true, true) => json!(u64::MAX),
                    (Some(ValueClass::Max), false, true) => json!(i64::MAX),
                    (Some(ValueClass::Max), true, false) => json!(u32::MAX),
                    (Some(ValueClass::Max), false, false) => json!(i32::MAX),
                    (Some(ValueClass::ConversionEdge), true, true) => json!(self.flavor.conversion),
                    _ => json!(1),
                }
            }
            Scalar::Number => match class {
                Some(ValueClass::Zero) => json!(0.0),
                Some(ValueClass::MinusOne) => json!(-1.0),
                Some(ValueClass::Max) => json!(f64::MAX),
                _ => json!(1.5),
            },
            Scalar::Boolean => json!(self.flavor.variant.is_multiple_of(2)),
        }
    }
}

/// Counts the classes of every value in a document under a schema node
///
/// # Arguments
///
/// * `node` - The schema node
/// * `value` - The document's value at that node
/// * `path` - The node's path
/// * `matrix` - The matrix to count observations in
pub fn observe(node: &Node, value: &Value, path: &str, matrix: &mut CoverageMatrix) {
    // each kind of node classifies its own value, then its children's
    let here = Location::Field {
        path: path.to_owned(),
    };
    match (node, value) {
        (_, Value::Null) => matrix.observe(here, ValueClass::Null),
        (Node::Object(props), Value::Object(object)) => {
            for (name, _, prop) in props {
                let child = format!("{path}.{name}");
                // a missing nullable field is the None the API leaves out when it serializes
                match object.get(name) {
                    Some(inner) => observe(prop, inner, &child, matrix),
                    None if prop.nullable() => {
                        matrix.observe(Location::Field { path: child }, ValueClass::Null);
                    }
                    None => matrix.observe(Location::Field { path: child }, ValueClass::Absent),
                }
            }
        }
        (Node::Array(item) | Node::Set(item), Value::Array(items)) => {
            matrix.observe(here, size_class(items.len()));
            let child = format!("{path}[]");
            for inner in items {
                observe(item, inner, &child, matrix);
            }
        }
        (Node::Map(inner_node), Value::Object(map)) => {
            matrix.observe(here.clone(), size_class(map.len()));
            if map.keys().any(|key| is_odd_key(key)) {
                matrix.observe(here, ValueClass::OddKey);
            }
            let child = format!("{path}{{}}");
            for inner in map.values() {
                observe(inner_node, inner, &child, matrix);
            }
        }
        (Node::OneOf(alternatives), value) => {
            if let Some((label, alternative)) = alternatives
                .iter()
                .find(|(label, alternative)| matches_variant(label, alternative, value))
            {
                matrix.observe(here, ValueClass::Variant(label.clone()));
                observe(alternative, value, &format!("{path}<{label}>"), matrix);
            }
        }
        (Node::Enum(_), Value::String(name)) => {
            matrix.observe(here, ValueClass::Variant(name.clone()));
        }
        (Node::Scalar(scalar, _), value) => {
            for class in scalar_value_classes(scalar, value) {
                matrix.observe(here.clone(), class);
            }
        }
        (Node::Any, value) => {
            for class in json_classes(&value.to_string()) {
                matrix.observe(here.clone(), class);
            }
        }
        _ => (),
    }
}

/// Returns the size class of a collection length
///
/// # Arguments
///
/// * `len` - The collection's length
fn size_class(len: usize) -> ValueClass {
    // empty, one, or many; sizes in between count as one-or-more ordinary cases
    match len {
        0 => ValueClass::Empty,
        1 => ValueClass::One,
        len if len >= MANY_FIELD => ValueClass::Many,
        _ => ValueClass::Variant("some".to_owned()),
    }
}

/// Returns whether a value is the alternative a `oneOf` label names
///
/// # Arguments
///
/// * `label` - The alternative's label
/// * `alternative` - The alternative's node
/// * `value` - The value
pub fn matches_variant(label: &str, alternative: &Node, value: &Value) -> bool {
    // unit variants are their name; data variants are single-key objects named by the label;
    // a referenced alternative is whichever of its own alternatives matches
    match (alternative, value) {
        (Node::OneOf(inner), value) => inner
            .iter()
            .any(|(label, node)| matches_variant(label, node, value)),
        (Node::Enum(values), Value::String(name)) => values.contains(name),
        (Node::Object(props), Value::Object(map)) if props.len() == 1 => map.contains_key(label),
        (Node::Object(_), Value::Object(_)) => true,
        (Node::Scalar(..), value) => !value.is_object() && !value.is_null(),
        _ => false,
    }
}

/// Returns the classes of a scalar value
///
/// # Arguments
///
/// * `scalar` - The scalar's schema
/// * `value` - The value
fn scalar_value_classes(scalar: &Scalar, value: &Value) -> Vec<ValueClass> {
    // classify by the scalar's type and format
    match (scalar, value) {
        (Scalar::String(Some(format)), Value::String(text)) if format == "date-time" => {
            // serde's parser, unlike RFC 3339's, takes the signed six-digit years of extreme times
            match serde_json::from_value::<chrono::DateTime<chrono::Utc>>(Value::String(
                text.clone(),
            )) {
                Ok(time) if time.timestamp() == 0 => vec![ValueClass::Epoch],
                Ok(time)
                    if time.timestamp() == chrono::DateTime::<chrono::Utc>::MAX_UTC.timestamp() =>
                {
                    vec![ValueClass::Max]
                }
                _ => Vec::new(),
            }
        }
        (Scalar::String(Some(format)), Value::String(text)) if format == "uuid" => text
            .parse::<uuid::Uuid>()
            .map_or_else(|_| Vec::new(), super::classes::uuid_classes),
        (Scalar::String(_), Value::String(text)) if text == "0.0.0" || text == MAX_SEMVER => {
            vec![if text == "0.0.0" {
                ValueClass::Min
            } else {
                ValueClass::Max
            }]
        }
        (Scalar::String(_), Value::String(text)) => {
            let mut classes = Vec::new();
            if text.is_empty() {
                classes.push(ValueClass::Empty);
            }
            if is_unicode(text) {
                classes.push(ValueClass::Unicode);
            }
            if text.len() >= LARGE_FIELD {
                classes.push(ValueClass::Large);
            }
            classes
        }
        (Scalar::Integer { format, unsigned }, Value::Number(number)) => {
            let wide = format.as_deref() != Some("int32");
            let as_wide = number
                .as_i64()
                .map(i128::from)
                .or_else(|| number.as_u64().map(i128::from));
            let Some(value) = as_wide else {
                return Vec::new();
            };
            let (min, max) = match (*unsigned, wide) {
                (true, true) => (0, i128::from(u64::MAX)),
                (true, false) => (0, i128::from(u32::MAX)),
                (false, true) => (i128::from(i64::MIN), i128::from(i64::MAX)),
                (false, false) => (i128::from(i32::MIN), i128::from(i32::MAX)),
            };
            let mut classes = super::classes::int_classes(value, min, max);
            classes.retain(|class| *class != ValueClass::MinusOne);
            if *unsigned
                && wide
                && (value == i128::from(i64::MAX) || value == i128::from(i64::MAX) + 1)
            {
                classes.push(ValueClass::ConversionEdge);
            }
            if *unsigned {
                classes.retain(|class| *class != ValueClass::Min);
            }
            classes
        }
        (Scalar::Number, Value::Number(number)) => match number.as_f64() {
            Some(0.0) => vec![ValueClass::Zero],
            Some(value) if value < 0.0 => vec![ValueClass::MinusOne],
            // the largest finite double is written exactly, so it reads back bit for bit
            Some(value) if value.to_bits() == f64::MAX.to_bits() => vec![ValueClass::Max],
            _ => Vec::new(),
        },
        (Scalar::Boolean, Value::Bool(flag)) => vec![ValueClass::Variant(flag.to_string())],
        _ => Vec::new(),
    }
}
