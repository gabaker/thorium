//! The registry of every declared Scylla object and the rules it must follow

use std::collections::HashSet;

use super::definition::{ColumnDef, ColumnRole, ObjectDef, TableDef, ViewDef};
use super::elastic::ElasticIndexDef;
use super::scylla::ThoriumKeyspace;
use crate::models::upgrades::{LAYOUTS, LayoutDef, SchemaBackend};

/// The ordered set of every declared Scylla object, implemented by `#[thorium::keyspace]`
pub trait Keyspace {
    /// Every object in creation order (each view after its base table)
    const OBJECTS: &'static [ObjectDef];
}

/// A rule of the declared schema an object breaks
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The object has no partition key column
    NoPartitionKey {
        /// The object
        object: &'static str,
    },
    /// A clustering column is declared before a partition key column
    ClusterBeforePartition {
        /// The object
        object: &'static str,
        /// The clustering column declared too early
        column: &'static str,
    },
    /// The object's key positions skip or repeat a position
    KeyPositions {
        /// The object
        object: &'static str,
    },
    /// Two objects share a name
    DuplicateName {
        /// The shared name
        object: &'static str,
    },
    /// A view's base isn't a table declared before it
    UnknownBase {
        /// The view
        view: &'static str,
        /// The base it names
        base: &'static str,
    },
    /// A view's key is missing one of its base's key columns
    BaseKeyMissing {
        /// The view
        view: &'static str,
        /// The base key column missing from the view's key
        column: &'static str,
    },
    /// A view column doesn't exist in its base
    ColumnNotInBase {
        /// The view
        view: &'static str,
        /// The column
        column: &'static str,
    },
    /// A view column's type differs from its base column's
    ColumnTypeMismatch {
        /// The view
        view: &'static str,
        /// The column
        column: &'static str,
    },
    /// A view's key holds more than one column that isn't in its base's key, which Scylla refuses
    TooManyNonKeyColumns {
        /// The view
        view: &'static str,
        /// The non-key base columns in the view's key
        columns: Vec<&'static str>,
    },
    /// A view's filter doesn't restrict one of its key columns
    FilterNotOnKey {
        /// The view
        view: &'static str,
        /// The filter
        filter: &'static str,
    },
}

impl std::fmt::Display for RegistryError {
    /// Describe the broken rule
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // name the object and the rule it breaks
        match self {
            RegistryError::NoPartitionKey { object } => {
                write!(f, "{object} has no partition key column")
            }
            RegistryError::ClusterBeforePartition { object, column } => write!(
                f,
                "{object} declares clustering column {column} before a partition key column"
            ),
            RegistryError::KeyPositions { object } => {
                write!(f, "{object}'s key positions aren't 0, 1, 2, …")
            }
            RegistryError::DuplicateName { object } => {
                write!(f, "more than one object is named {object}")
            }
            RegistryError::UnknownBase { view, base } => {
                write!(
                    f,
                    "view {view}'s base {base} isn't a table declared before it"
                )
            }
            RegistryError::BaseKeyMissing { view, column } => {
                write!(f, "view {view}'s key is missing base key column {column}")
            }
            RegistryError::ColumnNotInBase { view, column } => {
                write!(f, "view {view}'s column {column} isn't in its base")
            }
            RegistryError::ColumnTypeMismatch { view, column } => {
                write!(
                    f,
                    "view {view}'s column {column} has a different type than its base's"
                )
            }
            RegistryError::TooManyNonKeyColumns { view, columns } => write!(
                f,
                "view {view}'s key holds more than one non-key base column: {}",
                columns.join(", ")
            ),
            RegistryError::FilterNotOnKey { view, filter } => {
                write!(
                    f,
                    "view {view}'s filter `{filter}` isn't on one of its key columns"
                )
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// Check that a column list's key positions are 0, 1, 2, … for each role
///
/// # Arguments
///
/// * `columns` - The object's columns
fn positions_are_contiguous(columns: &[ColumnDef]) -> bool {
    // collect each role's positions
    let mut partition = Vec::new();
    let mut cluster = Vec::new();
    for column in columns {
        match column.role {
            ColumnRole::Partition(position) => partition.push(position),
            ColumnRole::Cluster(position, _) => cluster.push(position),
            ColumnRole::Static | ColumnRole::Regular => (),
        }
    }
    // each role's positions must count up from 0 with no gaps or repeats
    [partition, cluster].into_iter().all(|mut positions| {
        positions.sort_unstable();
        positions
            .iter()
            .enumerate()
            .all(|(index, position)| usize::from(*position) == index)
    })
}

/// Check the key rules every object follows
///
/// # Arguments
///
/// * `object` - The object's name
/// * `columns` - The object's columns
/// * `errors` - The broken rules found so far
fn check_keys(object: &'static str, columns: &[ColumnDef], errors: &mut Vec<RegistryError>) {
    // every object needs a partition key
    if !columns
        .iter()
        .any(|column| matches!(column.role, ColumnRole::Partition(_)))
    {
        errors.push(RegistryError::NoPartitionKey { object });
    }
    // clustering columns come after every partition key column
    let last_partition = columns
        .iter()
        .rposition(|column| matches!(column.role, ColumnRole::Partition(_)));
    if let Some(last_partition) = last_partition {
        for column in &columns[..last_partition] {
            if matches!(column.role, ColumnRole::Cluster(..)) {
                errors.push(RegistryError::ClusterBeforePartition {
                    object,
                    column: column.name,
                });
            }
        }
    }
    if !positions_are_contiguous(columns) {
        errors.push(RegistryError::KeyPositions { object });
    }
}

/// Check the rules a view follows against its base table
///
/// # Arguments
///
/// * `view` - The view
/// * `base` - Its base table
/// * `errors` - The broken rules found so far
fn check_view(view: &ViewDef, base: &TableDef, errors: &mut Vec<RegistryError>) {
    let find = |name: &str| base.columns.iter().find(|column| column.name == name);
    // every view column exists in the base with the same type
    for column in view.columns {
        match find(column.name) {
            Some(base_column) if base_column.desc != column.desc => {
                errors.push(RegistryError::ColumnTypeMismatch {
                    view: view.name,
                    column: column.name,
                });
            }
            Some(_) => (),
            None => errors.push(RegistryError::ColumnNotInBase {
                view: view.name,
                column: column.name,
            }),
        }
    }
    // the view's key holds every base key column
    let view_key = view
        .columns
        .iter()
        .filter(|column| column.role.is_key())
        .map(|column| column.name)
        .collect::<HashSet<_>>();
    for column in base.columns.iter().filter(|column| column.role.is_key()) {
        if !view_key.contains(column.name) {
            errors.push(RegistryError::BaseKeyMissing {
                view: view.name,
                column: column.name,
            });
        }
    }
    // Scylla allows at most one non-key base column in a view's key
    let non_key = view
        .columns
        .iter()
        .filter(|column| column.role.is_key())
        .filter(|column| find(column.name).is_some_and(|base_column| !base_column.role.is_key()))
        .map(|column| column.name)
        .collect::<Vec<_>>();
    if non_key.len() > 1 {
        errors.push(RegistryError::TooManyNonKeyColumns {
            view: view.name,
            columns: non_key,
        });
    }
    // a filter replaces the `IS NOT NULL` of one key column
    if let Some(filter) = view.filter {
        let on_key = view
            .filter_column()
            .is_some_and(|column| view_key.contains(column));
        if !on_key {
            errors.push(RegistryError::FilterNotOnKey {
                view: view.name,
                filter,
            });
        }
    }
}

/// Check every rule of the declared schema, returning each one broken
///
/// # Arguments
///
/// * `objects` - The declared objects in creation order
pub fn validate(objects: &[ObjectDef]) -> Result<(), Vec<RegistryError>> {
    let mut errors = Vec::new();
    let mut names = HashSet::new();
    let mut tables: Vec<&TableDef> = Vec::new();
    for object in objects {
        // names are unique across tables and views
        if !names.insert(object.name()) {
            errors.push(RegistryError::DuplicateName {
                object: object.name(),
            });
        }
        check_keys(object.name(), object.columns(), &mut errors);
        match object {
            ObjectDef::Table(table) => tables.push(table),
            // a view is checked against a base table declared before it
            ObjectDef::View(view) => match tables.iter().find(|table| table.name == view.base) {
                Some(base) => check_view(view, base, &mut errors),
                None => errors.push(RegistryError::UnknownBase {
                    view: view.name,
                    base: view.base,
                }),
            },
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// One declared schema object of any backend
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaObject {
    /// The backend the object belongs to
    pub backend: SchemaBackend,
    /// The object's name: a Scylla object, an Elastic index key, or a layout family
    pub name: String,
    /// The object's declarative definition
    pub definition: SchemaDefinition,
}

/// The declarative definition of a schema object, the source its canonical form is built from
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaDefinition {
    /// A Scylla table or view, from its derive
    Scylla(ObjectDef),
    /// An Elastic index's mappings, from the search-streamer's index body
    Elastic(ElasticIndexDef),
    /// A Redis or S3 layout version constant
    Layout(LayoutDef),
}

impl SchemaObject {
    /// Every object Thorium declares: the Scylla keyspace in creation order, then the Elastic
    /// indexes, then the Redis and S3 layouts
    #[must_use]
    pub fn declared() -> Vec<SchemaObject> {
        // start with every Scylla object in creation order
        let scylla = ThoriumKeyspace::OBJECTS.iter().map(|def| SchemaObject {
            backend: SchemaBackend::Scylla,
            name: def.name().to_owned(),
            definition: SchemaDefinition::Scylla(*def),
        });
        // then the fixed Elastic indexes
        let elastic = ElasticIndexDef::declared()
            .into_iter()
            .map(|def| SchemaObject {
                backend: SchemaBackend::Elastic,
                name: def.name.to_owned(),
                definition: SchemaDefinition::Elastic(def),
            });
        // then each Redis family and S3 bucket role's layout
        let layouts = LAYOUTS.iter().map(|layout| SchemaObject {
            backend: layout.backend,
            name: layout.family.to_owned(),
            definition: SchemaDefinition::Layout(*layout),
        });
        scylla.chain(elastic).chain(layouts).collect()
    }
}
