//! Classified differences between two schemas, found by diffing their canonical forms

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::classes::DataClass;
use super::definition::ObjectDef;
use super::fingerprint::{RevisionSnapshot, SnapshotObject};
use super::queries::ScyllaQueries;
use super::registry::Keyspace;
use super::scylla::ThoriumKeyspace;
use crate::models::upgrades::{LAYOUTS, SchemaBackend, StepDef, StepKind, is_legacy};

/// One difference between two schemas, classified
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaChange {
    /// The backend the changed object belongs to
    pub backend: SchemaBackend,
    /// The changed object's name
    pub object: String,
    /// The changed column, UDT field, or mapping property, when the change is below the object
    pub field: Option<String>,
    /// What changed
    pub kind: ChangeKind,
    /// How the change composes into an upgrade
    pub class: ChangeClass,
    /// The step kind a revision would normally give this change
    pub default_step: StepKind,
}

/// What a schema change is
///
/// A rename can't be told apart from a removal and an addition in a diff, so it shows as both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    /// A new object, column, UDT field, mapping property, index, or bucket
    Added,
    /// An object or field that is gone
    Removed,
    /// A column or field whose type changed
    TypeChanged {
        /// The old canonical type
        from: String,
        /// The new canonical type
        to: String,
    },
    /// A column whose key role or clustering order changed
    KeyChanged,
    /// An Elastic mapping type or analyzer change on a property
    MappingChanged,
    /// A Redis or S3 layout version bump
    LayoutChanged {
        /// The old layout version
        from: u32,
        /// The new layout version
        to: u32,
        /// Whether the new code still reads the old layout (`LayoutDef::reads_from`)
        reads_old: bool,
    },
}

/// How a change composes into an upgrade
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeClass {
    /// Applied automatically by `schema apply` (an `Infra` converge step)
    Additive,
    /// Needs a `Data` step with a registered migration and an approval
    Breaking,
    /// A removal or rename: a `Data` step with an approval, after every reader moved
    Contract,
    /// Needs no step and raises no warning: a breaking or contract change of an `Expiring`
    /// object, whose old data ages out, or the removal of an object registered as legacy
    Informational,
}

/// How a changed view reaches its new definition, since a view can't be altered
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewChange {
    /// The API reads the view: a replacement view under a new name (additive), the readers moved
    /// to it, and the old view dropped by a later contract step
    Replace,
    /// Nothing reads the view: a `Data` step drops it and creates it again
    Recreate,
}

impl ChangeClass {
    /// The step kind a revision normally gives a change of this class
    ///
    /// An informational change needs no step at all; it reports `Infra`, the kind that runs
    /// unattended and needs no approval.
    #[must_use]
    pub fn default_step(self) -> StepKind {
        // additive changes converge unattended; breaking and contract ones change stored data
        match self {
            ChangeClass::Additive | ChangeClass::Informational => StepKind::Infra,
            ChangeClass::Breaking | ChangeClass::Contract => StepKind::Data,
        }
    }

    /// Whether a change of this class is reported as an unreleased schema change; an
    /// informational change is listed without a warning
    #[must_use]
    pub fn warns(self) -> bool {
        // only an informational change is quiet
        self != ChangeClass::Informational
    }

    /// The class a change takes once its object's data class applies
    ///
    /// A breaking or contract change of an `Expiring` object is informational, since its old
    /// data ages out. Every other change keeps its class: `Runtime` and `Derived` objects need a
    /// `Data` step like `Core` ones, and an object the class table doesn't list is treated as
    /// `Core`.
    ///
    /// # Arguments
    ///
    /// * `data_class` - The changed object's data class, if the class table lists it
    #[must_use]
    pub fn for_data_class(self, data_class: Option<DataClass>) -> ChangeClass {
        // only a change that would need a step can become informational
        match (self, data_class) {
            (ChangeClass::Breaking | ChangeClass::Contract, Some(DataClass::Expiring)) => {
                ChangeClass::Informational
            }
            (class, _) => class,
        }
    }
}

impl ViewChange {
    /// Choose how a changed view reaches its new definition
    ///
    /// # Arguments
    ///
    /// * `read` - Whether the API reads the view
    #[must_use]
    pub fn for_view(read: bool) -> ViewChange {
        // a view something reads is replaced so its readers can move first
        if read {
            ViewChange::Replace
        } else {
            ViewChange::Recreate
        }
    }

    /// Find how a change moves its view, when it changes an existing view's columns or key
    ///
    /// A new view and a dropped view aren't view changes: they are an additive and a contract
    /// change of their own.
    ///
    /// # Arguments
    ///
    /// * `change` - The change
    /// * `old` - The objects of the schema the change starts from
    /// * `read` - Whether the API reads a view, by name
    #[must_use]
    pub fn for_change(
        change: &SchemaChange,
        old: &[SnapshotObject],
        read: impl Fn(&str) -> bool,
    ) -> Option<ViewChange> {
        // only a change within an object can change a view's definition
        if change.backend != SchemaBackend::Scylla
            || (change.field.is_none()
                && matches!(change.kind, ChangeKind::Added | ChangeKind::Removed))
        {
            return None;
        }
        // the object must have been a view before the change
        let was_view = old.iter().any(|object| {
            object.backend == SchemaBackend::Scylla
                && object.name == change.object
                && object.canonical["kind"] == "view"
        });
        was_view.then(|| ViewChange::for_view(read(&change.object)))
    }

    /// The steps this way of changing a view takes, in order, each with its class and kind
    #[must_use]
    pub fn steps(self) -> &'static [(ChangeClass, StepKind)] {
        // a replacement is an additive new view, then a contract drop of the old one
        match self {
            ViewChange::Replace => &[
                (ChangeClass::Additive, StepKind::Infra),
                (ChangeClass::Contract, StepKind::Data),
            ],
            ViewChange::Recreate => &[(ChangeClass::Breaking, StepKind::Data)],
        }
    }
}

/// The role a column plays in a Scylla object's canonical form
#[derive(Debug, Clone, PartialEq, Eq)]
enum Role {
    /// A partition key column at a position
    Partition(usize),
    /// A clustering column at a position, with its order
    Cluster(usize, String),
    /// A static column
    Static,
    /// A regular column
    Regular,
}

impl Role {
    /// Whether this role is part of the primary key
    fn is_key(&self) -> bool {
        // only partition and clustering columns form the key
        matches!(self, Role::Partition(_) | Role::Cluster(..))
    }
}

/// Read every column of a Scylla object's canonical form with its type and role
///
/// # Arguments
///
/// * `canonical` - The object's canonical form
fn scylla_columns(canonical: &Value) -> BTreeMap<String, (String, Role)> {
    // read the key columns' roles
    let mut roles = BTreeMap::new();
    let names = |key: &str| canonical[key].as_array().cloned().unwrap_or_default();
    for (position, name) in names("partition_key").iter().enumerate() {
        roles.insert(text(name), Role::Partition(position));
    }
    for (position, column) in names("clustering_key").iter().enumerate() {
        roles.insert(
            text(&column["name"]),
            Role::Cluster(position, text(&column["order"])),
        );
    }
    for name in names("static") {
        roles.insert(text(&name), Role::Static);
    }
    // pair every column's type with its role
    names("columns")
        .iter()
        .map(|column| {
            let name = text(&column["name"]);
            let role = roles.remove(&name).unwrap_or(Role::Regular);
            (name, (text(&column["type"]), role))
        })
        .collect()
}

/// Read a JSON value as text, rendering anything that isn't a string as JSON
///
/// # Arguments
///
/// * `value` - The value to read
fn text(value: &Value) -> String {
    // strings are taken as they are
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

/// Collects the changes of one object as they are found
struct ObjectDiff<'a> {
    /// The object's backend
    backend: SchemaBackend,
    /// The object's name
    object: &'a str,
    /// Whether every change within the object is breaking, as for a view
    all_breaking: bool,
    /// The object's data class, if the class table lists it
    data_class: Option<DataClass>,
    /// The changes found so far
    changes: Vec<SchemaChange>,
}

impl ObjectDiff<'_> {
    /// Record one change within the object
    ///
    /// # Arguments
    ///
    /// * `field` - The changed field, if the change is below the object
    /// * `kind` - What changed
    /// * `class` - The change's class before the object's own rules apply
    fn push(&mut self, field: Option<String>, kind: ChangeKind, class: ChangeClass) {
        // a view can't be altered, so any change within it is breaking
        let class = if self.all_breaking && field.is_some() {
            ChangeClass::Breaking
        } else {
            class
        };
        // then the object's data class decides whether the change needs a step
        let class = class.for_data_class(self.data_class);
        self.changes.push(SchemaChange {
            backend: self.backend,
            object: self.object.to_owned(),
            field,
            kind,
            class,
            default_step: class.default_step(),
        });
    }
}

/// Diff two canonical forms of the same Scylla table or view
///
/// # Arguments
///
/// * `diff` - Collects the object's changes
/// * `old` - The old canonical form
/// * `new` - The new canonical form
fn diff_scylla(diff: &mut ObjectDiff, old: &Value, new: &Value) {
    // a different kind, base or filter redefines the whole object
    if ["kind", "base", "filter"]
        .iter()
        .any(|key| old.get(key) != new.get(key))
    {
        diff.push(None, ChangeKind::KeyChanged, ChangeClass::Breaking);
    }
    // compare each column by name
    let old_columns = scylla_columns(old);
    let new_columns = scylla_columns(new);
    let names = old_columns
        .keys()
        .chain(new_columns.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for name in names {
        match (old_columns.get(&name), new_columns.get(&name)) {
            // a column added to or removed from the key changes the key
            (None, Some((_, role))) | (Some((_, role)), None) if role.is_key() => {
                diff.push(Some(name), ChangeKind::KeyChanged, ChangeClass::Breaking);
            }
            (None, Some(_)) => diff.push(Some(name), ChangeKind::Added, ChangeClass::Additive),
            (Some(_), None) => diff.push(Some(name), ChangeKind::Removed, ChangeClass::Contract),
            (Some((old_type, old_role)), Some((new_type, new_role))) => {
                // a type change and a role change are each breaking
                if old_type != new_type {
                    let kind = ChangeKind::TypeChanged {
                        from: old_type.clone(),
                        to: new_type.clone(),
                    };
                    diff.push(Some(name.clone()), kind, ChangeClass::Breaking);
                }
                if old_role != new_role {
                    diff.push(Some(name), ChangeKind::KeyChanged, ChangeClass::Breaking);
                }
            }
            (None, None) => (),
        }
    }
}

/// Diff two Elastic property maps, naming nested properties by their dotted path
///
/// # Arguments
///
/// * `diff` - Collects the index's changes
/// * `prefix` - The path of the properties' parent, empty at the top
/// * `old` - The old properties
/// * `new` - The new properties
fn diff_properties(diff: &mut ObjectDiff, prefix: &str, old: &Value, new: &Value) {
    // an absent map has no properties
    let empty = Map::new();
    let old = old.as_object().unwrap_or(&empty);
    let new = new.as_object().unwrap_or(&empty);
    let names = old.keys().chain(new.keys()).collect::<BTreeSet<_>>();
    for name in names {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        match (old.get(name), new.get(name)) {
            (None, Some(_)) => diff.push(Some(path), ChangeKind::Added, ChangeClass::Additive),
            (Some(_), None) => diff.push(Some(path), ChangeKind::Removed, ChangeClass::Contract),
            (Some(old), Some(new)) => {
                // the property's own mapping, without its children, is its type and analyzers
                let own = |value: &Value| {
                    let mut value = value.clone();
                    if let Some(map) = value.as_object_mut() {
                        map.remove("properties");
                    }
                    value
                };
                if own(old) != own(new) {
                    diff.push(
                        Some(path.clone()),
                        ChangeKind::MappingChanged,
                        ChangeClass::Breaking,
                    );
                }
                // then compare its children
                diff_properties(diff, &path, &old["properties"], &new["properties"]);
            }
            (None, None) => (),
        }
    }
}

/// Diff two canonical forms of the same Elastic index
///
/// # Arguments
///
/// * `diff` - Collects the index's changes
/// * `old` - The old canonical form
/// * `new` - The new canonical form
fn diff_elastic(diff: &mut ObjectDiff, old: &Value, new: &Value) {
    // anything in the body besides the properties is an index-wide mapping change
    let rest = |value: &Value| {
        let mut body = value["body"].clone();
        if let Some(mappings) = body.get_mut("mappings").and_then(Value::as_object_mut) {
            mappings.remove("properties");
        }
        body
    };
    if rest(old) != rest(new) {
        diff.push(None, ChangeKind::MappingChanged, ChangeClass::Breaking);
    }
    // then compare the properties one by one
    let properties = |value: &Value| value["body"]["mappings"]["properties"].clone();
    diff_properties(diff, "", &properties(old), &properties(new));
}

/// Diff two canonical forms of the same Redis or S3 layout
///
/// # Arguments
///
/// * `diff` - Collects the layout's changes
/// * `old` - The old canonical form, its family mapped to its version
/// * `new` - The new canonical form, its family mapped to its version
/// * `reads_from` - The oldest layout the new code reads, if known
fn diff_layout(diff: &mut ObjectDiff, old: &Value, new: &Value, reads_from: Option<u32>) {
    // a layout's canonical form maps its family to its version; a bare version reads the same
    let version = |value: &Value| {
        value
            .as_object()
            .and_then(|family| family.values().next())
            .unwrap_or(value)
            .as_u64()
            .and_then(|version| u32::try_from(version).ok())
            .unwrap_or_default()
    };
    let (from, to) = (version(old), version(new));
    // a newer layout is additive only when the new code still reads the old one
    let reads_old = to > from && reads_from.is_some_and(|oldest| oldest <= from);
    let class = if reads_old {
        ChangeClass::Additive
    } else {
        ChangeClass::Breaking
    };
    diff.push(
        None,
        ChangeKind::LayoutChanged {
            from,
            to,
            reads_old,
        },
        class,
    );
}

/// Index snapshot entries by backend and name
///
/// # Arguments
///
/// * `objects` - The entries to index
fn index(objects: &[SnapshotObject]) -> BTreeMap<(SchemaBackend, &str), &SnapshotObject> {
    // key each entry by its backend and name
    objects
        .iter()
        .map(|object| ((object.backend, object.name.as_str()), object))
        .collect()
}

impl SchemaChange {
    /// Diff two schemas, treating every layout version change as unreadable by the new code
    ///
    /// # Arguments
    ///
    /// * `old` - The objects of the schema to diff from
    /// * `new` - The objects of the schema to diff to
    #[must_use]
    pub fn between(old: &[SnapshotObject], new: &[SnapshotObject]) -> Vec<SchemaChange> {
        // a snapshot doesn't record which older layouts its code reads
        Self::between_with(old, new, |_, _| None)
    }

    /// Diff two schemas, per object and per field, and classify each difference
    ///
    /// Changes are listed by backend, then object, then field, and each object's data class
    /// comes from the class table.
    ///
    /// # Arguments
    ///
    /// * `old` - The objects of the schema to diff from
    /// * `new` - The objects of the schema to diff to
    /// * `reads_from` - The oldest layout the new code reads for a backend's family, if known
    #[must_use]
    pub fn between_with(
        old: &[SnapshotObject],
        new: &[SnapshotObject],
        reads_from: impl Fn(SchemaBackend, &str) -> Option<u32>,
    ) -> Vec<SchemaChange> {
        // look each object's data class up in the class table
        Self::between_classified(old, new, reads_from, DataClass::of)
    }

    /// Diff two schemas, per object and per field, and classify each difference with the data
    /// classes a lookup gives
    ///
    /// # Arguments
    ///
    /// * `old` - The objects of the schema to diff from
    /// * `new` - The objects of the schema to diff to
    /// * `reads_from` - The oldest layout the new code reads for a backend's family, if known
    /// * `data_class` - The data class of an object, by backend and name, if known
    pub(super) fn between_classified(
        old: &[SnapshotObject],
        new: &[SnapshotObject],
        reads_from: impl Fn(SchemaBackend, &str) -> Option<u32>,
        data_class: impl Fn(SchemaBackend, &str) -> Option<DataClass>,
    ) -> Vec<SchemaChange> {
        // index both sides by backend and name
        let old = index(old);
        let new = index(new);
        let keys = old
            .keys()
            .chain(new.keys())
            .copied()
            .collect::<BTreeSet<_>>();
        let mut changes = Vec::new();
        for (backend, name) in keys {
            let mut diff = ObjectDiff {
                backend,
                object: name,
                all_breaking: false,
                data_class: data_class(backend, name),
                changes: Vec::new(),
            };
            match (old.get(&(backend, name)), new.get(&(backend, name))) {
                // a whole new object is additive, and a removed one is a contract change unless
                // it is registered as legacy: it is already outside the declared schema
                (None, Some(_)) => diff.push(None, ChangeKind::Added, ChangeClass::Additive),
                (Some(_), None) if is_legacy(backend, name) => {
                    diff.push(None, ChangeKind::Removed, ChangeClass::Informational);
                }
                (Some(_), None) => diff.push(None, ChangeKind::Removed, ChangeClass::Contract),
                // an object on both sides is compared field by field when it changed
                (Some(old), Some(new)) if old.fingerprint != new.fingerprint => match backend {
                    SchemaBackend::Scylla => {
                        diff.all_breaking =
                            old.canonical["kind"] == "view" || new.canonical["kind"] == "view";
                        diff_scylla(&mut diff, &old.canonical, &new.canonical);
                    }
                    SchemaBackend::Elastic => {
                        diff_elastic(&mut diff, &old.canonical, &new.canonical);
                    }
                    SchemaBackend::Redis | SchemaBackend::S3 => diff_layout(
                        &mut diff,
                        &old.canonical,
                        &new.canonical,
                        reads_from(backend, name),
                    ),
                    // a backend this build doesn't know is compared as a whole
                    SchemaBackend::Unknown => {
                        diff.push(None, ChangeKind::KeyChanged, ChangeClass::Breaking);
                    }
                },
                _ => (),
            }
            changes.append(&mut diff.changes);
        }
        changes
    }

    /// Diff a revision's snapshot with the schema this build declares, using the layouts this
    /// build reads
    ///
    /// # Arguments
    ///
    /// * `latest` - The snapshot of the latest revision in the catalog
    #[must_use]
    pub fn unreleased(latest: &RevisionSnapshot) -> Vec<SchemaChange> {
        // diff to every object this build declares
        Self::unreleased_to(latest, SnapshotObject::declared())
    }

    /// Diff a revision's snapshot with a schema this build would declare, using the layouts this
    /// build reads
    ///
    /// # Arguments
    ///
    /// * `latest` - The snapshot of the latest revision in the catalog
    /// * `declared` - The objects of the declared schema
    #[must_use]
    pub(super) fn unreleased_to(
        latest: &RevisionSnapshot,
        declared: &[SnapshotObject],
    ) -> Vec<SchemaChange> {
        // the declared layouts say which older layouts this build reads
        Self::between_with(&latest.objects, declared, |backend, family| {
            LAYOUTS
                .iter()
                .find(|layout| layout.backend == backend && layout.family == family)
                .map(|layout| layout.reads_from)
        })
    }

    /// Whether a step in a list already carries this change the way its class needs
    ///
    /// An additive or informational change needs no step. A breaking change needs a `Data` step
    /// targeting it that runs a migration, and a contract change a `Data` step targeting it.
    ///
    /// # Arguments
    ///
    /// * `steps` - The steps to look through
    #[must_use]
    pub fn is_covered<'a>(&self, steps: impl IntoIterator<Item = &'a StepDef>) -> bool {
        // additive changes converge without a step of their own, and informational ones need none
        if matches!(
            self.class,
            ChangeClass::Additive | ChangeClass::Informational
        ) {
            return true;
        }
        // look for a data step targeting the object or this field of it
        steps.into_iter().any(|step| {
            let targets = step.targets.iter().any(|target| {
                target.backend == self.backend
                    && target.object == self.object
                    && (target.field.is_none() || target.field == self.field.as_deref())
            });
            let migrates = step.thoradm_migration.is_some() || step.transform.is_some();
            step.kind == StepKind::Data
                && targets
                && (self.class == ChangeClass::Contract || migrates)
        })
    }
}

/// The Elastic indexes a list of changes must rebuild, each named once, in order
///
/// Every breaking or contract change within an existing index needs a rebuild; a new index, a
/// new property, and a dropped index don't.
///
/// # Arguments
///
/// * `changes` - The changes to look through
#[must_use]
pub fn rebuilds(changes: &[SchemaChange]) -> Vec<String> {
    // collect the indexes with a non-additive change below the index
    let mut indexes = Vec::new();
    for change in changes {
        let within = change.field.is_some() || change.kind == ChangeKind::MappingChanged;
        if change.backend == SchemaBackend::Elastic
            && matches!(change.class, ChangeClass::Breaking | ChangeClass::Contract)
            && within
            && !indexes.contains(&change.object)
        {
            indexes.push(change.object.clone());
        }
    }
    indexes
}

/// The views the API reads through its generated statements
#[must_use]
pub fn views_read() -> BTreeSet<String> {
    // the generated statements are named `<object>.<statement>`
    let objects = ScyllaQueries::statements("ns")
        .into_iter()
        .filter_map(|(name, _)| name.split_once('.').map(|(object, _)| object.to_owned()))
        .collect::<BTreeSet<_>>();
    // keep the objects that are declared views
    ThoriumKeyspace::OBJECTS
        .iter()
        .filter(|object| matches!(object, ObjectDef::View(_)))
        .map(|object| object.name().to_owned())
        .filter(|name| objects.contains(name))
        .collect()
}
