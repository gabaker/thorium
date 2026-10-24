//! Tests of the probe: canned `system_schema` rows and `_mapping` responses captured from real
//! servers normalize to the registry's canonical forms, and the comparison reports each kind of
//! difference

use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use super::super::{Fingerprint, RevisionSnapshot, SnapshotObject};
use super::{
    ColumnRow, IndexRow, ObservedSchema, SchemaRows, TypeRow, ViewRow, canonical_filter,
    canonical_replication, layout_objects, mapping_object,
};
use crate::models::elastic::ElasticIndex;
use crate::models::upgrades::{RevisionId, SchemaBackend};

/// The `system_schema` rows of a keyspace this release's `schema apply` created, from Scylla 6.2.3
const SCYLLA_6_2_3_NEW: &str = include_str!("../goldens/probe/scylla-6.2.3-new.json");

/// The `system_schema` rows of a keyspace an older release's setup created, with the `events` table
/// and the `entities_by_name` view, after `schema apply`, from Scylla 6.2.3
const SCYLLA_6_2_3_LEGACY: &str = include_str!("../goldens/probe/scylla-6.2.3-legacy.json");

/// Each index's `_mapping` response after the search-streamer's create body, keyed by the index's
/// stable name, from Elasticsearch 8.19.2
const ELASTIC_8_19_2: &str = include_str!("../goldens/probe/elastic-8.19.2-mappings.json");

/// Every captured set of `system_schema` rows, with what it was captured from
const CAPTURED_ROWS: [(&str, &str); 2] = [
    ("scylla-6.2.3-new", SCYLLA_6_2_3_NEW),
    ("scylla-6.2.3-legacy", SCYLLA_6_2_3_LEGACY),
];

/// Parse captured `system_schema` rows
///
/// # Arguments
///
/// * `text` - The captured rows as JSON
fn rows(text: &str) -> SchemaRows {
    // the rows are stored as the probe serializes them
    serde_json::from_str(text).expect("the captured rows parse")
}

/// The declared objects of one backend, by name
///
/// # Arguments
///
/// * `backend` - The backend
fn declared(backend: SchemaBackend) -> BTreeMap<String, SnapshotObject> {
    // keep the backend's declared entries, keyed by name
    SnapshotObject::declared()
        .iter()
        .filter(|object| object.backend == backend)
        .map(|object| (object.name.clone(), object.clone()))
        .collect()
}

/// Index snapshot entries by name
///
/// # Arguments
///
/// * `objects` - The entries
fn by_name(objects: Vec<SnapshotObject>) -> BTreeMap<String, SnapshotObject> {
    // key each entry by its name
    objects
        .into_iter()
        .map(|object| (object.name.clone(), object))
        .collect()
}

/// The stored clustering order of a view's columns, as `name ORDER` in key order
///
/// # Arguments
///
/// * `rows` - The captured rows
/// * `view` - The view's name
fn stored_order(rows: &SchemaRows, view: &str) -> Vec<String> {
    // the view's clustering columns, in key order
    let mut clustering = rows
        .columns
        .iter()
        .filter(|column| column.table_name == view && column.kind == "clustering")
        .collect::<Vec<_>>();
    clustering.sort_by_key(|column| column.position);
    clustering
        .iter()
        .map(|column| format!("{} {}", column.column_name, column.clustering_order))
        .collect()
}

/// Canned `system_schema` rows captured from Scylla 6.2.3 normalize to exactly the derive's
/// canonical form, and fingerprint, for every declared table and view, both for a keyspace this
/// release creates and for one an older release created; the canned rows hold what Scylla really
/// stores: the reordered `where_clause` of `comments_by_id`, the fully expanded clustering orders
/// of the samples and repos views, and the replication map
#[test]
fn canned_scylla_rows_normalize_to_the_declared_forms() {
    // compare each captured keyspace with the declared tables and views
    let declared = declared(SchemaBackend::Scylla);
    for (name, text) in CAPTURED_ROWS {
        // every declared object observes exactly as declared
        let observed = by_name(rows(text).objects().expect("the rows canonicalize"));
        for (object, expected) in &declared {
            let found = observed.get(object);
            assert_eq!(found, Some(expected), "{name}: {object}");
        }
        // and nothing else is there but the legacy objects an older release created
        let extra = observed
            .keys()
            .filter(|object| !declared.contains_key(*object))
            .cloned()
            .collect::<Vec<_>>();
        let expected_extra: &[&str] = if name.ends_with("legacy") {
            &["entities_by_name", "events"]
        } else {
            &[]
        };
        assert_eq!(extra, expected_extra, "{name}");
    }
    // the legacy rows hold Scylla's own rendering of the filters an older setup sent
    let legacy = rows(SCYLLA_6_2_3_LEGACY);
    let where_clause = |view: &str| {
        legacy
            .views
            .iter()
            .find(|row| row.view_name == view)
            .map(|row| row.where_clause.clone())
    };
    assert_eq!(
        where_clause("comments_by_id").as_deref(),
        Some(
            "id IS NOT null AND sha256 IS NOT null AND group IS NOT null AND uploaded IS NOT null"
        )
    );
    assert_eq!(
        where_clause("network_policies_default").as_deref(),
        Some("group IS NOT null AND name IS NOT null AND default_policy = true")
    );
    // and the full clustering orders of views whose DDL named only some columns
    assert_eq!(
        stored_order(&legacy, "samples"),
        [
            "group ASC",
            "uploaded DESC",
            "id DESC",
            "year ASC",
            "bucket ASC"
        ]
    );
    assert_eq!(
        stored_order(&legacy, "repos"),
        [
            "group ASC",
            "uploaded DESC",
            "year DESC",
            "bucket DESC",
            "id DESC"
        ]
    );
    // and the replication map as strings, which normalizes to integers and a bare class
    assert_eq!(
        legacy.replication.get("class").map(String::as_str),
        Some("org.apache.cassandra.locator.NetworkTopologyStrategy")
    );
    assert_eq!(
        canonical_replication(&legacy.replication),
        json!({ "class": "NetworkTopologyStrategy", "datacenter1": 1 })
    );
}

/// Every declared view's filter is already in the probe's canonical filter form, so a view the
/// registry declares observes with exactly its declared canonical form
#[test]
fn declared_view_filters_are_canonical() {
    // normalize each declared view's filter with its own key columns
    for object in declared(SchemaBackend::Scylla).values() {
        // only views have filters
        if object.canonical["kind"] != "view" {
            continue;
        }
        let keys = object.canonical["partition_key"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(
                object.canonical["clustering_key"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|column| column["name"].as_str()),
            )
            .collect::<BTreeSet<_>>();
        let filter = object.canonical["filter"].as_str();
        let normalized = filter.and_then(|filter| canonical_filter(filter, &keys).unwrap());
        assert_eq!(normalized.as_deref(), filter, "{}", object.name);
    }
}

/// Parse the captured `_mapping` responses
fn mappings() -> BTreeMap<String, Value> {
    // the responses are keyed by each index's stable name
    serde_json::from_str(ELASTIC_8_19_2).expect("the captured mappings parse")
}

/// Canned `_mapping` responses captured from Elasticsearch 8.19.2 canonicalize to the mappings the
/// search-streamer's create body declares, and the defaults Elastic can add (`"type": "object"`,
/// `"index": true`, …) are stripped
#[test]
fn canned_mappings_canonicalize_to_the_declared_indexes() {
    // compare each captured response with its declared index
    let declared = declared(SchemaBackend::Elastic);
    let responses = mappings();
    for index in &ElasticIndex::ALL {
        // the captured response observes exactly as declared
        let name = index.to_string();
        let response = &responses[&name];
        let observed = mapping_object(index, response).expect("the mapping canonicalizes");
        assert_eq!(Some(&observed), declared.get(&name), "{name}");
        // and so does the same response with the server's defaults spelled out
        let mut defaults = response.clone();
        let concrete = defaults
            .as_object_mut()
            .and_then(|map| map.values_mut().next())
            .expect("the response holds one index");
        concrete["mappings"]["type"] = json!("object");
        let properties = concrete["mappings"]["properties"]
            .as_object_mut()
            .expect("the mapping has properties");
        for property in properties.values_mut() {
            property["index"] = json!(true);
            property["store"] = json!(false);
        }
        properties.insert(
            "group".to_owned(),
            json!({ "type": "keyword", "doc_values": true, "index": true }),
        );
        let observed = mapping_object(index, &defaults).expect("the mapping canonicalizes");
        assert_eq!(Some(&observed), declared.get(&name), "{name} with defaults");
    }
}

/// A response keyed by the concrete index behind an alias is read the same, a field mapped `text`
/// instead of `keyword` observes differently, and a response without exactly one index is refused
#[test]
fn mapping_responses_are_read_whatever_their_key() {
    // start from the captured sample tags response
    let responses = mappings();
    let response = &responses["SampleTags"];
    let declared = declared(SchemaBackend::Elastic)["SampleTags"].clone();
    // the same mappings under another key
    let mappings = response
        .as_object()
        .and_then(|map| map.values().next())
        .expect("the response holds one index")
        .clone();
    let aliased = json!({ "sample-tags-000002": mappings });
    let observed = mapping_object(&ElasticIndex::SampleTags, &aliased).unwrap();
    assert_eq!(observed, declared);
    // a dynamically mapped group is a different form
    let mut dynamic = aliased.clone();
    dynamic["sample-tags-000002"]["mappings"]["properties"]["group"] = json!({
        "type": "text",
        "fields": { "keyword": { "type": "keyword", "ignore_above": 256 } }
    });
    let observed = mapping_object(&ElasticIndex::SampleTags, &dynamic).unwrap();
    assert_ne!(observed.fingerprint, declared.fingerprint);
    // two indexes can't be told apart
    let two = json!({ "a": mappings, "b": mappings });
    assert!(mapping_object(&ElasticIndex::SampleTags, &two).is_err());
}

/// View filters compare as sets of conjuncts: order, case, repeats, spacing, and quotes on plain
/// names don't matter, `IS NOT NULL` on key columns is dropped, and anything else is kept
#[test]
fn view_filters_are_sets_of_conjuncts() {
    // a view keyed on these columns
    let keys = ["id", "group", "name", "default_policy"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let canonical = |clause: &str| canonical_filter(clause, &keys).unwrap();
    // only the key columns' `IS NOT NULL`s: no extra filter
    assert_eq!(canonical("id IS NOT NULL AND \"group\" IS NOT null"), None);
    assert_eq!(canonical(""), None);
    // the extra condition, however it is spelled or placed
    let expected = Some("default_policy = true".to_owned());
    assert_eq!(
        canonical("group IS NOT null AND name IS NOT null AND default_policy = true"),
        expected
    );
    assert_eq!(
        canonical("DEFAULT_POLICY=TRUE and NAME is not NULL and default_policy = true"),
        expected
    );
    // several extra conditions are sorted and deduplicated
    assert_eq!(
        canonical("kind = 'File' AND id IS NOT NULL AND default_policy = true AND kind = 'File'"),
        Some("default_policy = true AND kind = 'File'".to_owned())
    );
    // a non-key column's `IS NOT NULL`, a quoted mixed-case name, and string case are kept
    assert_eq!(
        canonical("extra IS NOT NULL AND \"Mixed\" = 'Ab'"),
        Some("\"Mixed\" = 'Ab' AND extra is not null".to_owned())
    );
    // an `AND` inside parentheses doesn't split a conjunct
    assert_eq!(
        canonical("(a = 1 AND b = 2) AND id IS NOT NULL"),
        Some("( a = 1 and b = 2 )".to_owned())
    );
    // text that can't be a filter is refused
    assert!(canonical_filter("a = 'open", &keys).is_err());
    assert!(canonical_filter("a ; b", &keys).is_err());
}

/// The views Scylla creates to back secondary indexes aren't listed, while the indexes themselves
/// and user-defined types, which Thorium never declares, are, each in a form of its own
#[test]
fn indexes_and_types_are_observed_as_their_own_objects() {
    // add a secondary index, its backing view, and a type to a captured keyspace
    let mut rows = rows(SCYLLA_6_2_3_NEW);
    rows.indexes.push(IndexRow {
        table_name: "nodes".to_owned(),
        index_name: "nodes_health".to_owned(),
        kind: "COMPOSITES".to_owned(),
        options: BTreeMap::from([("target".to_owned(), "health".to_owned())]),
    });
    rows.views.push(ViewRow {
        view_name: "nodes_health_index".to_owned(),
        base_table_name: "nodes".to_owned(),
        where_clause: "health IS NOT null".to_owned(),
        include_all_columns: false,
    });
    rows.columns.push(ColumnRow {
        table_name: "nodes_health_index".to_owned(),
        column_name: "health".to_owned(),
        kind: "partition_key".to_owned(),
        position: 0,
        clustering_order: "NONE".to_owned(),
        cql_type: "text".to_owned(),
    });
    rows.types.push(TypeRow {
        type_name: "addr".to_owned(),
        field_names: vec!["street".to_owned(), "num".to_owned()],
        field_types: vec!["varchar".to_owned(), "int".to_owned()],
    });
    let observed = by_name(rows.objects().unwrap());
    // the index and the type are listed, the backing view isn't
    assert!(!observed.contains_key("nodes_health_index"));
    assert_eq!(
        observed["nodes_health"].canonical,
        json!({
            "kind": "secondary_index", "name": "nodes_health", "table": "nodes",
            "index_kind": "composites", "options": { "target": "health" }
        })
    );
    assert_eq!(
        observed["addr"].canonical,
        json!({
            "kind": "type", "name": "addr",
            "fields": [{ "name": "street", "type": "text" }, { "name": "num", "type": "int" }]
        })
    );
    // a column type that doesn't parse is refused, naming the column
    let mut broken = SchemaRows::default();
    broken.tables.push("t".to_owned());
    broken.columns.push(ColumnRow {
        table_name: "t".to_owned(),
        column_name: "c".to_owned(),
        kind: "partition_key".to_owned(),
        position: 0,
        clustering_order: "NONE".to_owned(),
        cql_type: "map<text".to_owned(),
    });
    let error = broken.objects().unwrap_err().to_string();
    assert!(error.contains("t.c"), "{error}");
}

/// Recorded layout versions take the registry's canonical form, and an observed schema
/// fingerprints only the backends that were probed, an empty probed backend included
#[test]
fn observed_schemas_fingerprint_the_probed_backends() {
    // the declared layouts, as a record holds them
    let mut recorded: BTreeMap<SchemaBackend, BTreeMap<String, u32>> = BTreeMap::new();
    for object in SnapshotObject::declared() {
        if let Some((family, version)) = object
            .canonical
            .as_object()
            .filter(|_| matches!(object.backend, SchemaBackend::Redis | SchemaBackend::S3))
            .and_then(|map| map.iter().next())
        {
            let version = u32::try_from(version.as_u64().unwrap()).unwrap();
            recorded
                .entry(object.backend)
                .or_default()
                .insert(family.clone(), version);
        }
    }
    // the recorded layouts observe exactly as declared
    for (backend, layouts) in &recorded {
        let observed = by_name(layout_objects(*backend, layouts));
        assert_eq!(observed, declared(*backend), "{backend:?}");
    }
    // an observed schema of Redis alone, and of an empty Elastic too
    let redis = layout_objects(SchemaBackend::Redis, &recorded[&SchemaBackend::Redis]);
    let snapshot =
        RevisionSnapshot::declared(RevisionId::try_from("2026-10-v02".to_owned()).unwrap());
    let without_elastic = SnapshotObject::declared()
        .iter()
        .filter(|object| object.backend != SchemaBackend::Elastic)
        .cloned()
        .collect();
    let observed = ObservedSchema::new(
        without_elastic,
        &[SchemaBackend::Redis, SchemaBackend::Elastic],
    );
    assert_eq!(
        observed.objects, redis,
        "only the probed backends' objects are kept"
    );
    assert_eq!(
        observed.backends.keys().copied().collect::<Vec<_>>(),
        [SchemaBackend::Elastic, SchemaBackend::Redis]
    );
    assert_eq!(
        observed.backends[&SchemaBackend::Redis],
        snapshot.backends[&SchemaBackend::Redis]
    );
    assert_eq!(
        observed.backends[&SchemaBackend::Elastic],
        Fingerprint::of(&json!([]))
    );
    assert_eq!(
        observed.deployment,
        RevisionSnapshot::deployment_fingerprint(&observed.backends)
    );
}
