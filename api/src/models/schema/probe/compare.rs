//! Comparing an observed schema with a revision's snapshot, object by object and field by field

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use super::super::{
    ChangeKind, Fingerprint, RevisionSnapshot, SchemaChange, SnapshotObject, canonical_json,
};
use super::{BackendDrift, DriftReport, FieldDrift, ObjectDrift, ObservedSchema, canonical_filter};
use crate::models::upgrades::{
    BackendSchemaStatus, Catalog, CatalogEntry, DriftKind, RevisionId, SchemaBackend,
};

/// The objects older revisions created that are no longer declared and are registered as legacy
///
/// A registered legacy object is expected to be there or not: absent, it isn't reported missing
/// even against a snapshot that lists it, and present where the snapshot doesn't list it, it is
/// reported unexpected but acknowledged, so it raises no drift. Callers pass the legacy registry
/// through this trait; any closure over a backend and an object name implements it.
pub trait LegacyObjects {
    /// Whether an object is a registered legacy object
    ///
    /// # Arguments
    ///
    /// * `backend` - The object's backend
    /// * `name` - The object's name
    fn is_legacy(&self, backend: SchemaBackend, name: &str) -> bool;
}

impl<F> LegacyObjects for F
where
    F: Fn(SchemaBackend, &str) -> bool,
{
    /// Ask the closure whether an object is a registered legacy object
    ///
    /// # Arguments
    ///
    /// * `backend` - The object's backend
    /// * `name` - The object's name
    fn is_legacy(&self, backend: SchemaBackend, name: &str) -> bool {
        self(backend, name)
    }
}

/// Pick the revision whose snapshot the live schema is compared with
///
/// A schema apply converges the live schema to the revision it was started for and records that
/// schema's deployment fingerprint, while the recorded revision only moves once the whole upgrade
/// has run. Between the two, the live schema matches the snapshot of the revision the apply
/// converged to, not the recorded one, whose snapshot lacks the objects the apply created. The
/// recorded revision is the answer unless the applied fingerprint is another revision's, one the
/// deployment is upgrading to.
///
/// # Arguments
///
/// * `catalog` - The catalog of revisions and the deployment fingerprints of their snapshots
/// * `recorded` - The revision `schema_migrations` records
/// * `applied` - The deployment fingerprint `schema_migrations` records as applied
#[must_use]
pub fn converged_revision(
    catalog: &Catalog,
    recorded: &RevisionId,
    applied: &Fingerprint,
) -> RevisionId {
    // an entry names the applied schema when its snapshot's deployment fingerprint is it
    let names = |entry: &&CatalogEntry| entry.schema == Some(applied.as_str());
    // the recorded revision's own schema is the applied one, so no apply ran ahead of the record
    if catalog
        .edges
        .iter()
        .any(|entry| entry.id == recorded.as_str() && names(&entry))
    {
        return recorded.clone();
    }
    // otherwise the first revision after the recorded one whose snapshot is the applied schema
    catalog
        .edges
        .iter()
        .filter(names)
        .map(CatalogEntry::revision)
        .find(|revision| revision != recorded && catalog.at_or_below(recorded, revision))
        .unwrap_or_else(|| recorded.clone())
}

/// Rewrite a view's filter as its sorted set of conjuncts, so two filters with the same conjuncts
/// compare equal whatever their order, case, or repeats
///
/// # Arguments
///
/// * `object` - A snapshot entry, changed only when it is a Scylla view
fn with_filter_set(object: &SnapshotObject) -> SnapshotObject {
    // only a Scylla view has a filter
    let mut object = object.clone();
    if object.backend != SchemaBackend::Scylla || object.canonical["kind"] != "view" {
        return object;
    }
    // the view's key columns
    let mut keys = BTreeSet::new();
    if let Some(partition) = object.canonical["partition_key"].as_array() {
        keys.extend(partition.iter().filter_map(Value::as_str));
    }
    if let Some(clustering) = object.canonical["clustering_key"].as_array() {
        keys.extend(
            clustering
                .iter()
                .filter_map(|column| column["name"].as_str()),
        );
    }
    // a filter that doesn't parse is compared as written
    let filter = object.canonical["filter"]
        .as_str()
        .map(|filter| canonical_filter(filter, &keys).unwrap_or_else(|_| Some(filter.to_owned())));
    if let Some(filter) = filter {
        object.canonical["filter"] = json!(filter);
        object.fingerprint = Fingerprint::of(&object.canonical);
    }
    object
}

/// Describe one Scylla column of a canonical form: its type and its place in the key
///
/// # Arguments
///
/// * `canonical` - The table's or view's canonical form
/// * `column` - The column's name
fn scylla_field(canonical: &Value, column: &str) -> Option<String> {
    // find the column's type
    let columns = canonical["columns"].as_array()?;
    let found = columns.iter().find(|entry| entry["name"] == column)?;
    let cql_type = found["type"].as_str().unwrap_or_default();
    // then its role
    let named = |value: &Value| value == column;
    let partition = canonical["partition_key"].as_array();
    if let Some(at) = partition.and_then(|key| key.iter().position(named)) {
        return Some(format!("{cql_type}, partition key {at}"));
    }
    let clustering = canonical["clustering_key"].as_array();
    if let Some((at, entry)) = clustering.and_then(|key| {
        key.iter()
            .enumerate()
            .find(|(_, entry)| entry["name"] == column)
    }) {
        let order = entry["order"].as_str().unwrap_or("asc");
        return Some(format!("{cql_type}, clustering key {at} {order}"));
    }
    let statics = canonical["static"].as_array();
    if statics.is_some_and(|statics| statics.iter().any(named)) {
        return Some(format!("{cql_type}, static"));
    }
    Some(cql_type.to_owned())
}

/// Describe one Elastic property of a canonical form: its own mapping without its children
///
/// # Arguments
///
/// * `canonical` - The index's canonical form
/// * `path` - The property's dotted path
fn elastic_field(canonical: &Value, path: &str) -> Option<String> {
    // walk the path through each level's properties
    let mut node = &canonical["body"]["mappings"];
    for part in path.split('.') {
        node = node.get("properties")?.get(part)?;
    }
    // the property's own mapping is everything but its children
    let mut own = node.clone();
    if let Some(map) = own.as_object_mut() {
        map.remove("properties");
    }
    Some(canonical_json(&own))
}

/// Describe a field of an object's canonical form, as the report shows it
///
/// # Arguments
///
/// * `object` - The object, when it exists on this side
/// * `field` - The field's name or path
fn field_value(object: Option<&SnapshotObject>, field: &str) -> Option<String> {
    // each backend names its fields its own way
    let object = object?;
    match object.backend {
        SchemaBackend::Scylla => scylla_field(&object.canonical, field),
        SchemaBackend::Elastic => elastic_field(&object.canonical, field),
        _ => None,
    }
}

/// Describe the object-wide parts of two forms that differ: a Scylla object's kind, base, or
/// filter, an Elastic index's mapping settings, or a layout's version
///
/// # Arguments
///
/// * `expected` - The expected form
/// * `observed` - The observed form
fn object_fields(expected: &SnapshotObject, observed: &SnapshotObject) -> Vec<FieldDrift> {
    // render a part of a form, absent when it is null or missing
    let part = |value: Option<&Value>| match value {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => Some(canonical_json(other)),
    };
    let differing = |field: &str, left: Option<&Value>, right: Option<&Value>| {
        (left != right).then(|| FieldDrift {
            field: field.to_owned(),
            expected: part(left),
            observed: part(right),
        })
    };
    match expected.backend {
        SchemaBackend::Scylla => ["kind", "base", "filter"]
            .iter()
            .filter_map(|key| {
                differing(
                    key,
                    expected.canonical.get(key),
                    observed.canonical.get(key),
                )
            })
            .collect(),
        SchemaBackend::Elastic => {
            // everything in the mappings beside the properties
            let rest = |object: &SnapshotObject| {
                let mut body = object.canonical["body"].clone();
                if let Some(mappings) = body.get_mut("mappings").and_then(Value::as_object_mut) {
                    mappings.remove("properties");
                }
                body
            };
            let (left, right) = (rest(expected), rest(observed));
            differing("mappings", Some(&left), Some(&right))
                .into_iter()
                .collect()
        }
        _ => {
            // a layout's form maps its family to its version
            let version = |object: &SnapshotObject| object.canonical.get(&object.name).cloned();
            differing(
                "version",
                version(expected).as_ref(),
                version(observed).as_ref(),
            )
            .into_iter()
            .collect()
        }
    }
}

/// Fingerprint one observed field, so an acknowledgement of it lapses when it changes
///
/// # Arguments
///
/// * `field` - The field's name or path
/// * `value` - The field's observed value
fn field_fingerprint(field: &str, value: &str) -> Fingerprint {
    // the field's name and value together
    Fingerprint::of(&json!({ "field": field, "value": value }))
}

/// Turn one difference between the expected and observed schema into a drift entry, or `None`
/// for a registered legacy object that is absent, which is expected to be
///
/// # Arguments
///
/// * `change` - The difference
/// * `want` - The expected object, when there is one
/// * `have` - The observed object, when there is one
/// * `is_legacy` - Whether the object is a registered legacy object
fn drift_entry(
    change: &SchemaChange,
    want: Option<&SnapshotObject>,
    have: Option<&SnapshotObject>,
    is_legacy: bool,
) -> Option<ObjectDrift> {
    match (&change.field, &change.kind) {
        // a registered legacy object that is absent is expected to be
        (None, ChangeKind::Removed) if is_legacy => None,
        (None, ChangeKind::Removed) => Some(ObjectDrift {
            backend: change.backend,
            object: change.object.clone(),
            kind: DriftKind::Missing,
            fields: Vec::new(),
            acknowledged: false,
            lapsed: false,
            fingerprint: None,
        }),
        // an extra object is unexpected, and acknowledged when it is registered legacy
        (None, ChangeKind::Added) => Some(ObjectDrift {
            backend: change.backend,
            object: change.object.clone(),
            kind: DriftKind::Unexpected,
            fields: Vec::new(),
            acknowledged: is_legacy,
            lapsed: false,
            fingerprint: have.map(|object| object.fingerprint.clone()),
        }),
        // a difference in the object as a whole lists the parts that differ
        (None, _) => Some(ObjectDrift {
            backend: change.backend,
            object: change.object.clone(),
            kind: DriftKind::Changed,
            fields: match (want, have) {
                (Some(want), Some(have)) => object_fields(want, have),
                _ => Vec::new(),
            },
            acknowledged: false,
            lapsed: false,
            fingerprint: have.map(|object| object.fingerprint.clone()),
        }),
        // a difference within the object is reported for its field
        (Some(field), kind) => {
            let expected_value = field_value(want, field);
            let observed_value = field_value(have, field);
            let kind = match kind {
                ChangeKind::Added => DriftKind::Unexpected,
                ChangeKind::Removed => DriftKind::Missing,
                _ => DriftKind::Changed,
            };
            Some(ObjectDrift {
                backend: change.backend,
                object: format!("{}.{field}", change.object),
                kind,
                fingerprint: observed_value
                    .as_deref()
                    .map(|value| field_fingerprint(field, value)),
                fields: vec![FieldDrift {
                    field: field.clone(),
                    expected: expected_value,
                    observed: observed_value,
                }],
                acknowledged: false,
                lapsed: false,
            })
        }
    }
}

impl DriftReport {
    /// Compare an observed schema with a revision's snapshot
    ///
    /// Only the backends the probe observed are compared. Each object comes out as a match (left
    /// out of the report), `missing`, `unexpected`, or `changed`; a difference within an object is
    /// reported per field, named `<object>.<field>`, and a difference in the object as a whole
    /// (a Scylla object's kind, base, or filter, an index's mapping settings, a layout's version)
    /// under the object's own name. View filters are compared as sets of conjuncts. A registered
    /// legacy object absent from the live schema isn't missing, and one present where the snapshot
    /// doesn't list it is unexpected but acknowledged. Admins' acknowledgements aren't applied here.
    ///
    /// # Arguments
    ///
    /// * `expected` - The snapshot of the revision the deployment records
    /// * `observed` - What the probe observed
    /// * `legacy` - Which objects are registered legacy objects
    #[must_use]
    pub fn compare(
        expected: &RevisionSnapshot,
        observed: &ObservedSchema,
        legacy: &dyn LegacyObjects,
    ) -> DriftReport {
        // compare only the probed backends, with view filters as sets
        let probed = observed.backends.keys().copied().collect::<BTreeSet<_>>();
        let expected_objects = expected
            .objects
            .iter()
            .filter(|object| probed.contains(&object.backend))
            .map(with_filter_set)
            .collect::<Vec<_>>();
        let observed_objects = observed
            .objects
            .iter()
            .map(with_filter_set)
            .collect::<Vec<_>>();
        let index = |objects: &[SnapshotObject]| {
            objects
                .iter()
                .map(|object| ((object.backend, object.name.clone()), object.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        let (expected_index, observed_index) = (index(&expected_objects), index(&observed_objects));
        // turn each difference into a drift entry
        let mut backends = probed
            .iter()
            .map(|backend| (*backend, Vec::new()))
            .collect::<BTreeMap<SchemaBackend, Vec<ObjectDrift>>>();
        for change in SchemaChange::between(&expected_objects, &observed_objects) {
            let key = (change.backend, change.object.clone());
            let (want, have) = (expected_index.get(&key), observed_index.get(&key));
            let is_legacy = legacy.is_legacy(change.backend, &change.object);
            let Some(entry) = drift_entry(&change, want, have, is_legacy) else {
                continue;
            };
            backends.entry(change.backend).or_default().push(entry);
        }
        // a field can change in more than one way (type and key), so report it once
        for entries in backends.values_mut() {
            entries.dedup_by(|right, left| left.object == right.object && left.kind == right.kind);
        }
        // a backend drifts when any of its entries isn't acknowledged
        let backends = backends
            .into_iter()
            .map(|(backend, objects)| {
                let status = if objects.iter().all(|object| object.acknowledged) {
                    BackendSchemaStatus::Ok
                } else {
                    BackendSchemaStatus::Drift
                };
                (backend, BackendDrift { status, objects })
            })
            .collect();
        DriftReport {
            revision: expected.revision.clone(),
            expected: expected.deployment.clone(),
            observed: observed.deployment.clone(),
            backends,
        }
    }
}
