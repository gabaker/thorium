//! Tests for the declared schema registry, its canonical forms, and their fingerprints

use chrono::{DateTime, Utc};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use uuid::Uuid;

use super::scylla::{NetworkPoliciesDefaultView, SamplesListTable, ThoriumKeyspace};
use super::tests::FixtureSamplesTable;
use super::{
    ClusterOrder, ColumnDef, ColumnRole, CqlDesc, CqlNative, CqlType, ElasticIndexDef, Fingerprint,
    Keyspace, ObjectDef, RevisionSnapshot, SchemaBackend, SchemaDefinition, SchemaObject,
    ScyllaObject, ScyllaTable, SnapshotObject, TableDef, ViewDef, canonical_json, canonical_type,
};
use crate::models::elastic::{ElasticIndex, ElasticSource};
use crate::models::upgrades::{Enumeration, LAYOUTS, LayoutDef};

/// Build an Elastic config with a non-default max analyzed offset
fn elastic_conf() -> crate::conf::Elastic {
    // build a minimal config and let serde default everything else
    serde_json::from_value(json!({
        "node": "https://elastic:9200",
        "username": "thorium",
        "password": "password",
        "max_analyzed_offset": 4321,
    }))
    .expect("minimal elastic config parses")
}

/// The registry holds every keyspace object in creation order, the four Elastic indexes, and
/// every layout, with names unique within each backend
#[test]
fn registry_declares_every_object() {
    // build the registry's objects
    let declared = SchemaObject::declared();
    // the Scylla objects are the keyspace's, in creation order
    let scylla = declared
        .iter()
        .filter_map(|object| match &object.definition {
            SchemaDefinition::Scylla(def) => {
                assert_eq!(object.backend, SchemaBackend::Scylla);
                assert_eq!(object.name, def.name());
                Some(*def)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(scylla, ThoriumKeyspace::OBJECTS);
    // the Elastic objects are the four fixed indexes under their stable names
    let elastic = declared
        .iter()
        .filter(|object| object.backend == SchemaBackend::Elastic)
        .map(|object| object.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        elastic,
        ["SampleResults", "RepoResults", "SampleTags", "RepoTags"]
    );
    // every layout is declared under its family
    let layouts = declared
        .iter()
        .filter_map(|object| match &object.definition {
            SchemaDefinition::Layout(layout) => {
                assert_eq!(object.backend, layout.backend);
                assert_eq!(object.name, layout.family);
                Some(*layout)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(layouts, LAYOUTS);
    assert_eq!(declared.len(), scylla.len() + elastic.len() + layouts.len());
    // names are unique within each backend
    let mut names = BTreeSet::new();
    for object in &declared {
        assert!(
            names.insert((object.backend, object.name.as_str())),
            "{:?} object {} is declared twice",
            object.backend,
            object.name
        );
    }
}

/// Each declared Elastic index carries the mappings the search-streamer creates it with and the
/// source it belongs to
#[test]
fn elastic_definitions_reuse_the_streamers_body() {
    // declare every index, one per fixed index
    let declared = ElasticIndexDef::declared();
    assert_eq!(declared.len(), ElasticIndex::ALL.len());
    for (def, index) in declared.iter().zip(ElasticIndex::ALL.iter()) {
        // the stable name is the index's variant name
        assert_eq!(def.name, index.to_string());
        // the mappings are the ones the streamer's create body holds
        let body = index.create_body(&elastic_conf());
        assert_eq!(def.body, json!({ "mappings": body["mappings"] }));
        // results and tags indexes belong to their sources
        let source = if def.name.ends_with("Results") {
            ElasticSource::Results
        } else {
            ElasticSource::Tags
        };
        assert_eq!(def.source, source);
    }
}

/// The fixture samples table again, with its regular columns declared in another order between
/// its key columns
#[derive(Debug, Clone, ScyllaTable)]
#[thorium_table(name = "fixture_samples")]
pub struct ReorderedRegularsTable {
    /// Where the file came from
    pub origin: Option<String>,
    /// The group this submission is in
    #[thorium(partition)]
    pub group: String,
    /// The user who submitted the file
    pub submitter: String,
    /// The year this submission was uploaded in
    #[thorium(partition)]
    pub year: i32,
    /// The md5 of the file
    pub md5: String,
    /// The time bucket within the year
    #[thorium(partition)]
    pub bucket: i32,
    /// The description given at upload
    pub description: Option<String>,
    /// When this submission was uploaded
    #[thorium(cluster = "desc")]
    pub uploaded: DateTime<Utc>,
    /// The file name given at upload
    pub name: Option<String>,
    /// The submission id
    #[thorium(cluster = "desc")]
    pub id: Uuid,
    /// The sha1 of the file
    pub sha1: String,
    /// The sha256 of the file
    pub sha256: String,
}

/// The fixture samples table again, with other doc comments and a Rust field renamed while its
/// column name is kept
#[derive(Debug, Clone, ScyllaTable)]
#[thorium_table(name = "fixture_samples")]
pub struct RenamedSamplesTable {
    /// The owning group
    #[thorium(partition)]
    pub group: String,
    /// The upload year
    #[thorium(partition)]
    pub year: i32,
    /// The bucket in that year
    #[thorium(partition)]
    pub bucket: i32,
    /// The upload time
    #[thorium(cluster = "desc")]
    pub uploaded: DateTime<Utc>,
    /// The id of this submission
    #[thorium(cluster = "desc")]
    pub id: Uuid,
    /// The file's sha256
    pub sha256: String,
    /// The file's sha1
    pub sha1: String,
    /// The file's md5
    pub md5: String,
    /// The submitted name
    pub name: Option<String>,
    /// The submitted description
    pub description: Option<String>,
    /// Who uploaded the file, stored in the `submitter` column
    #[thorium(column = "submitter")]
    pub uploader: String,
    /// The file's origin
    pub origin: Option<String>,
}

/// A sha256 held in a Rust type alias of `String`
type Sha = String;

/// A table whose key columns are declared out of name order, with a column of an aliased type
#[derive(Debug, Clone, ScyllaTable)]
#[thorium_table(name = "fixture_keys")]
pub struct KeyOrderTable {
    /// The first partition column, last by name
    #[thorium(partition)]
    pub zeta: String,
    /// The second partition column, first by name
    #[thorium(partition)]
    pub alpha: i32,
    /// The first clustering column
    #[thorium(cluster = "desc")]
    pub omega: DateTime<Utc>,
    /// The second clustering column
    #[thorium(cluster)]
    pub beta: Uuid,
    /// A regular column declared before an earlier name
    pub sha: Sha,
    /// A regular column of a nested collection
    pub nested: Vec<HashSet<String>>,
}

/// The same table as `KeyOrderTable`, with its aliased column declared as `String`
#[derive(Debug, Clone, ScyllaTable)]
#[thorium_table(name = "fixture_keys")]
pub struct UnaliasedKeyOrderTable {
    /// The first partition column
    #[thorium(partition)]
    pub zeta: String,
    /// The second partition column
    #[thorium(partition)]
    pub alpha: i32,
    /// The first clustering column
    #[thorium(cluster = "desc")]
    pub omega: DateTime<Utc>,
    /// The second clustering column
    #[thorium(cluster)]
    pub beta: Uuid,
    /// The sha256, as a plain `String`
    pub sha: String,
    /// A regular column of a nested collection
    pub nested: Vec<HashSet<String>>,
}

/// Wrap a Scylla definition as a declared object
///
/// # Arguments
///
/// * `def` - The object's definition
fn scylla_object(def: ObjectDef) -> SchemaObject {
    // declare the definition under its own name
    SchemaObject {
        backend: SchemaBackend::Scylla,
        name: def.name().to_owned(),
        definition: SchemaDefinition::Scylla(def),
    }
}

/// Get the fingerprint of a Scylla definition
///
/// # Arguments
///
/// * `def` - The object's definition
fn fingerprint(def: ObjectDef) -> Fingerprint {
    // build the object's snapshot entry and keep its fingerprint
    SnapshotObject::of(&scylla_object(def)).fingerprint
}

/// Build a table definition from a name and columns, leaked so it is `'static` like a derive's
///
/// # Arguments
///
/// * `name` - The table's name
/// * `columns` - The table's columns
fn table(name: &'static str, columns: Vec<ColumnDef>) -> ObjectDef {
    // leak the columns so the definition can borrow them for 'static
    ObjectDef::Table(TableDef {
        name,
        columns: Box::leak(columns.into_boxed_slice()),
        ttl: None,
        partition_size: None,
    })
}

/// Get the fixture samples table's columns, to change one at a time
fn fixture_columns() -> Vec<ColumnDef> {
    // copy the derive's columns into an owned list
    FixtureSamplesTable::DEF.columns().to_vec()
}

/// Change one fixture column through a closure
///
/// # Arguments
///
/// * `name` - The column to change
/// * `change` - The change to make
fn change_column(name: &str, change: impl Fn(&mut ColumnDef)) -> ObjectDef {
    // find and change the named column
    let mut columns = fixture_columns();
    let column = columns
        .iter_mut()
        .find(|column| column.name == name)
        .unwrap_or_else(|| panic!("no column {name}"));
    change(column);
    table("fixture_samples", columns)
}

/// Adding doc comments, reordering regular columns in the source, and renaming a Rust field that
/// keeps its column name change neither the canonical form nor the fingerprint
#[test]
fn source_only_changes_keep_the_fingerprint() {
    // the three declarations of the same table
    let base = scylla_object(FixtureSamplesTable::DEF);
    let reordered = scylla_object(ReorderedRegularsTable::DEF);
    let renamed = scylla_object(RenamedSamplesTable::DEF);
    // the source differs, so the comparison below isn't trivially true
    assert_ne!(base.definition, reordered.definition);
    // each has the same canonical form and fingerprint
    for other in [&reordered, &renamed] {
        assert_eq!(base.definition.canonical(), other.definition.canonical());
        assert_eq!(
            SnapshotObject::of(&base).fingerprint,
            SnapshotObject::of(other).fingerprint
        );
    }
    // the form holds the column name, not the Rust field name or the docs
    let text = canonical_json(&renamed.definition.canonical());
    assert!(text.contains("\"submitter\""), "{text}");
    assert!(!text.contains("uploader"), "{text}");
    assert!(!text.contains("Who uploaded"), "{text}");
    // marking a column security-sensitive is about fills, not the stored schema
    let sensitive = change_column("submitter", |column| column.security_sensitive = true);
    assert_eq!(
        fingerprint(sensitive),
        SnapshotObject::of(&base).fingerprint,
        "security-sensitive column"
    );
    // extra whitespace in a view's filter keeps the view's fingerprint
    let ObjectDef::View(view) = NetworkPoliciesDefaultView::DEF else {
        panic!("network_policies_default is a view");
    };
    let spaced = ObjectDef::View(ViewDef {
        filter: Some("  default_policy   =\ttrue "),
        ..view
    });
    assert_ne!(spaced, NetworkPoliciesDefaultView::DEF);
    assert_eq!(
        fingerprint(spaced),
        fingerprint(NetworkPoliciesDefaultView::DEF),
        "view filter whitespace"
    );
}

/// Each change to the stored schema moves the fingerprint, one case per assertion
#[test]
fn stored_schema_changes_move_the_fingerprint() {
    // fingerprint the fixture table as declared
    let base = fingerprint(FixtureSamplesTable::DEF);
    // the fingerprint is a sha256 in lowercase hex
    assert_eq!(base.as_str().len(), 64);
    assert!(
        base.as_str()
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    );
    // a column's type changes
    let retyped = change_column("md5", |column| {
        column.desc = CqlDesc::Native(CqlNative::BigInt);
    });
    assert_ne!(fingerprint(retyped), base, "column type change");
    // a regular column becomes a clustering column
    let clustered = change_column("sha256", |column| {
        column.role = ColumnRole::Cluster(2, ClusterOrder::Asc);
    });
    assert_ne!(fingerprint(clustered), base, "regular to clustering");
    // a clustering column's order flips
    let flipped = change_column("id", |column| {
        column.role = ColumnRole::Cluster(1, ClusterOrder::Asc);
    });
    assert_ne!(fingerprint(flipped), base, "flipped clustering order");
    // a regular column becomes static
    let made_static = change_column("origin", |column| column.role = ColumnRole::Static);
    assert_ne!(fingerprint(made_static), base, "regular to static");
    // a column is added
    let mut added = fixture_columns();
    added.push(ColumnDef {
        name: "size",
        desc: CqlDesc::Native(CqlNative::BigInt),
        role: ColumnRole::Regular,
        security_sensitive: false,
    });
    assert_ne!(
        fingerprint(table("fixture_samples", added)),
        base,
        "added column"
    );
    // a column is removed
    let removed = fixture_columns()
        .into_iter()
        .filter(|column| column.name != "origin")
        .collect();
    assert_ne!(
        fingerprint(table("fixture_samples", removed)),
        base,
        "removed column"
    );
    // a column is renamed
    let renamed = change_column("origin", |column| column.name = "source");
    assert_ne!(fingerprint(renamed), base, "renamed column");
    // the table is renamed
    assert_ne!(
        fingerprint(table("fixture_submissions", fixture_columns())),
        base,
        "renamed table"
    );
    // the same columns rebuilt by hand keep the fingerprint, so the cases above differ only by
    // their change
    assert_eq!(
        fingerprint(table("fixture_samples", fixture_columns())),
        base
    );
}

/// The canonical JSON is byte-identical across runs, sorts keys whatever their insertion order,
/// and matches the committed golden for `samples_list`
#[test]
fn canonical_json_is_stable_and_matches_its_golden() {
    // declare the samples list table
    let object = scylla_object(SamplesListTable::DEF);
    // repeated runs give the same bytes
    let first = canonical_json(&object.definition.canonical());
    for _ in 0..10 {
        assert_eq!(canonical_json(&object.definition.canonical()), first);
    }
    // keys come out sorted whatever order they were inserted in
    let mut forward = serde_json::Map::new();
    let mut backward = serde_json::Map::new();
    for key in ["b", "a", "c"] {
        forward.insert(key.to_owned(), json!({ "y": 1, "x": [key] }));
    }
    for key in ["c", "a", "b"] {
        backward.insert(key.to_owned(), json!({ "x": [key], "y": 1 }));
    }
    let forward = canonical_json(&serde_json::Value::Object(forward));
    assert_eq!(
        forward,
        canonical_json(&serde_json::Value::Object(backward))
    );
    assert_eq!(
        forward,
        r#"{"a":{"x":["a"],"y":1},"b":{"x":["b"],"y":1},"c":{"x":["c"],"y":1}}"#
    );
    // the text matches the committed golden, and the fingerprint is the sha256 of that text
    let golden = golden("canonical_samples_list.json", &format!("{first}\n"));
    assert_eq!(golden.trim_end(), first);
    let snapshot = SnapshotObject::of(&object);
    let digest = <sha2::Sha256 as sha2::Digest>::digest(first.as_bytes());
    assert_eq!(snapshot.fingerprint.as_str(), format!("{digest:x}"));
}

/// Type aliases and case give one spelling, `frozen<…>` is kept, and key columns keep their key
/// order while the column list is sorted
#[test]
fn types_and_keys_normalize() {
    // aliases and case of text give one spelling
    for text in ["TEXT", "varchar", "text", " Text ", "VARCHAR"] {
        assert_eq!(canonical_type(text).as_deref(), Ok("text"), "{text}");
    }
    // frozen is kept, and nested aliases and spacing normalize
    assert_eq!(
        canonical_type("FROZEN<MAP<VARCHAR,INT>>").as_deref(),
        Ok("frozen<map<text, int>>")
    );
    assert_eq!(
        canonical_type("list< frozen<set<varchar>> >").as_deref(),
        Ok("list<frozen<set<text>>>")
    );
    // the text spelling agrees with the type the derives build from Rust types
    assert_eq!(
        canonical_type("LIST<FROZEN<SET<VARCHAR>>>").as_deref(),
        Ok(<Vec<HashSet<String>> as CqlType>::DESC.render().as_str())
    );
    assert_eq!(
        canonical_type("map<TEXT, BigInt>").as_deref(),
        Ok(<BTreeMap<String, i64> as CqlType>::DESC.render().as_str())
    );
    // malformed types are refused
    for text in ["", "list<text", "map<text,>", "set<text>>", "<text>"] {
        assert!(canonical_type(text).is_err(), "{text:?} parsed");
    }
    // collections and frozen refuse the wrong number of parameters
    for text in [
        "list<text, int>",
        "frozen<a, b>",
        "map<text>",
        "set<int, int>",
    ] {
        assert!(canonical_type(text).is_err(), "{text:?} parsed");
    }
    // a quoted name keeps its case and quotes, while an unquoted one is lowercased
    assert_eq!(
        canonical_type("FROZEN<\"MyType\">").as_deref(),
        Ok("frozen<\"MyType\">")
    );
    assert_eq!(canonical_type("MyType").as_deref(), Ok("mytype"));
    // a Rust type alias gives the same canonical form as the type it names
    assert_eq!(
        scylla_object(KeyOrderTable::DEF).definition.canonical(),
        scylla_object(UnaliasedKeyOrderTable::DEF)
            .definition
            .canonical()
    );
    // key columns keep their key order and orders, while the column list is sorted by name
    let canonical = scylla_object(KeyOrderTable::DEF).definition.canonical();
    assert_eq!(canonical["partition_key"], json!(["zeta", "alpha"]));
    assert_eq!(
        canonical["clustering_key"],
        json!([
            { "name": "omega", "order": "desc" },
            { "name": "beta", "order": "asc" },
        ])
    );
    let names = canonical["columns"]
        .as_array()
        .expect("columns are a list")
        .iter()
        .map(|column| column["name"].as_str().expect("a column has a name"))
        .collect::<Vec<_>>();
    assert_eq!(names, ["alpha", "beta", "nested", "omega", "sha", "zeta"]);
    // each column carries its canonical type
    assert_eq!(
        canonical["columns"][2],
        json!({ "name": "nested", "type": "list<frozen<set<text>>>" })
    );
    // key columns listed in neither key nor name order still come out in key order
    let column = |name, role| ColumnDef {
        name,
        desc: CqlDesc::Native(CqlNative::Text),
        role,
        security_sensitive: false,
    };
    let shuffled = vec![
        column("q", ColumnRole::Cluster(1, ClusterOrder::Asc)),
        column("z", ColumnRole::Partition(1)),
        column("x", ColumnRole::Cluster(0, ClusterOrder::Desc)),
        column("m", ColumnRole::Partition(2)),
        column("e", ColumnRole::Regular),
        column("c", ColumnRole::Cluster(2, ClusterOrder::Asc)),
        column("a", ColumnRole::Partition(0)),
    ];
    let canonical = scylla_object(table("fixture_shuffled", shuffled))
        .definition
        .canonical();
    assert_eq!(canonical["partition_key"], json!(["a", "z", "m"]));
    assert_eq!(
        canonical["clustering_key"],
        json!([
            { "name": "x", "order": "desc" },
            { "name": "q", "order": "asc" },
            { "name": "c", "order": "asc" },
        ])
    );
}

/// Get the fingerprint of the samples results index declared with another body
///
/// # Arguments
///
/// * `body` - The index body to declare
fn elastic_fingerprint(body: serde_json::Value) -> Fingerprint {
    // declare the samples results index with the body
    let def = ElasticIndexDef {
        body,
        ..ElasticIndexDef::of(&ElasticIndex::SampleResults)
    };
    let object = SchemaObject {
        backend: SchemaBackend::Elastic,
        name: def.name.to_owned(),
        definition: SchemaDefinition::Elastic(def),
    };
    // build its snapshot entry and keep its fingerprint
    SnapshotObject::of(&object).fingerprint
}

/// The defaults Elastic adds to a mapping keep an index's fingerprint, while the same keys with
/// other values move it
#[test]
fn elastic_server_defaults_keep_the_fingerprint() {
    // fingerprint the index as declared
    let declared = ElasticIndexDef::of(&ElasticIndex::SampleResults).body;
    let base = elastic_fingerprint(declared.clone());
    // the declared body holds none of the defaults, so stripping them is what keeps it
    let text = canonical_json(&declared);
    for key in ["\"object\"", "\"index\"", "\"doc_values\"", "\"store\""] {
        assert!(!text.contains(key), "{key} in {text}");
    }
    // the body Elastic reports, with its defaults added, keeps the fingerprint
    let mut defaults = declared.clone();
    defaults["mappings"]["type"] = json!("object");
    defaults["mappings"]["properties"]["group"]["index"] = json!(true);
    defaults["mappings"]["properties"]["group"]["doc_values"] = json!(true);
    defaults["mappings"]["properties"]["group"]["store"] = json!(false);
    assert_eq!(elastic_fingerprint(defaults), base, "server defaults");
    // each of those keys with a value other than its default moves it
    for (key, value) in [
        ("index", json!(false)),
        ("store", json!(true)),
        ("doc_values", json!(false)),
        ("type", json!("text")),
    ] {
        let mut changed = declared.clone();
        changed["mappings"]["properties"]["group"][key] = value.clone();
        assert_ne!(elastic_fingerprint(changed), base, "group {key} = {value}");
    }
}

/// A layout's canonical form maps its family to its version, so families at one version have
/// different fingerprints, and only a version bump moves a family's fingerprint
#[test]
fn layout_forms_name_their_family() {
    // find a declared layout's snapshot entry
    let entry = |backend: SchemaBackend, family: &str| {
        SnapshotObject::declared()
            .iter()
            .find(|object| object.backend == backend && object.name == family)
            .unwrap_or_else(|| panic!("no {family} layout"))
            .clone()
    };
    // the form maps the family or bucket role to its version
    assert_eq!(
        entry(SchemaBackend::Redis, "users").canonical,
        json!({ "users": 2 })
    );
    assert_eq!(
        entry(SchemaBackend::S3, "files").canonical,
        json!({ "files": 1 })
    );
    // no two layouts share a fingerprint, though most share a version
    let mut seen = BTreeMap::new();
    for object in SnapshotObject::declared()
        .iter()
        .filter(|object| matches!(object.backend, SchemaBackend::Redis | SchemaBackend::S3))
    {
        if let Some(other) = seen.insert(object.fingerprint.clone(), object.name.as_str()) {
            panic!("{other} and {} share a fingerprint", object.name);
        }
    }
    // fingerprint a variant of the users layout
    let users = *LAYOUTS
        .iter()
        .find(|layout| layout.family == "users")
        .expect("users is declared");
    let layout_fingerprint = |layout: LayoutDef| {
        SnapshotObject::of(&SchemaObject {
            backend: layout.backend,
            name: layout.family.to_owned(),
            definition: SchemaDefinition::Layout(layout),
        })
        .fingerprint
    };
    let base = layout_fingerprint(users);
    // what a layout reads, fills and enumerates by keeps the fingerprint
    let same_version = LayoutDef {
        reads_from: 1,
        security_sensitive: &[],
        enumerate: Some(Enumeration::Scan("users:*")),
        ..users
    };
    assert_eq!(layout_fingerprint(same_version), base);
    // a version bump moves it
    let bumped = LayoutDef {
        version: users.version + 1,
        ..users
    };
    assert_ne!(layout_fingerprint(bumped), base);
}

/// The keyspace, backend and deployment fingerprints match the committed snapshot, and a moved
/// fingerprint fails with a per-object diff
#[test]
fn fingerprints_match_their_snapshot() {
    // build every declared object's snapshot entry
    let objects = SnapshotObject::declared();
    // every declared object has an entry, sorted by backend and name
    assert_eq!(objects.len(), SchemaObject::declared().len());
    let keys = objects
        .iter()
        .map(|object| (object.backend, object.name.as_str()))
        .collect::<Vec<_>>();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted);
    // every backend has a fingerprint, and the deployment's is built from them
    let backends = RevisionSnapshot::backend_fingerprints(objects);
    assert_eq!(
        backends.keys().copied().collect::<Vec<_>>(),
        [
            SchemaBackend::Scylla,
            SchemaBackend::Elastic,
            SchemaBackend::Redis,
            SchemaBackend::S3
        ]
    );
    let deployment = RevisionSnapshot::deployment_fingerprint(&backends);
    // the fingerprints match the committed snapshot
    let actual = fingerprint_snapshot(&deployment, &backends, objects);
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(&actual).expect("snapshot renders")
    );
    let expected: serde_json::Value =
        serde_json::from_str(&golden("fingerprints.json", &rendered)).expect("golden parses");
    let diff = fingerprint_diff(&expected, &actual);
    assert!(
        diff.is_empty(),
        "the declared schema's fingerprints moved; rerun with THORIUM_BLESS=1 to accept them:\n{}",
        diff.join("\n")
    );
    // moving one object's fingerprint moves its backend's and the deployment's, and the diff
    // names the object
    let mut moved = objects.to_vec();
    let samples = moved
        .iter_mut()
        .find(|object| object.name == "samples_list")
        .expect("samples_list is declared");
    samples.fingerprint = Fingerprint::of(&json!("moved"));
    let moved_backends = RevisionSnapshot::backend_fingerprints(&moved);
    assert_ne!(
        moved_backends[&SchemaBackend::Scylla],
        backends[&SchemaBackend::Scylla]
    );
    assert_eq!(
        moved_backends[&SchemaBackend::Redis],
        backends[&SchemaBackend::Redis]
    );
    let moved_deployment = RevisionSnapshot::deployment_fingerprint(&moved_backends);
    assert_ne!(moved_deployment, deployment);
    let moved_diff = fingerprint_diff(
        &expected,
        &fingerprint_snapshot(&moved_deployment, &moved_backends, &moved),
    );
    assert_eq!(moved_diff.len(), 3, "{moved_diff:?}");
    assert!(moved_diff[0].starts_with("deployment: "), "{moved_diff:?}");
    assert!(moved_diff[1].starts_with("scylla: "), "{moved_diff:?}");
    assert!(
        moved_diff[2].starts_with("changed scylla/samples_list: "),
        "{moved_diff:?}"
    );
    // the declared snapshot under a revision carries the same fingerprints
    let snapshot = RevisionSnapshot::declared("2026-10-v02".parse().expect("a valid revision"));
    assert_eq!(snapshot.deployment, deployment);
    assert_eq!(snapshot.backends, backends);
    assert_eq!(snapshot.objects, objects);
}

/// Render the fingerprints of a schema as the committed snapshot holds them
///
/// # Arguments
///
/// * `deployment` - The deployment fingerprint
/// * `backends` - Each backend's fingerprint
/// * `objects` - Every object's snapshot entry
fn fingerprint_snapshot(
    deployment: &Fingerprint,
    backends: &BTreeMap<SchemaBackend, Fingerprint>,
    objects: &[SnapshotObject],
) -> serde_json::Value {
    // list each object's fingerprint under its backend and name
    let objects = objects
        .iter()
        .map(|object| {
            json!({
                "backend": object.backend,
                "name": object.name,
                "fingerprint": object.fingerprint,
            })
        })
        .collect::<Vec<_>>();
    json!({ "deployment": deployment, "backends": backends, "objects": objects })
}

/// List how two fingerprint snapshots differ: the deployment, then each backend, then each
/// object added, removed or changed
///
/// # Arguments
///
/// * `expected` - The committed snapshot
/// * `actual` - The snapshot of the declared schema
fn fingerprint_diff(expected: &serde_json::Value, actual: &serde_json::Value) -> Vec<String> {
    // collect each difference in order
    let mut diff = Vec::new();
    // compare the deployment fingerprints
    if expected["deployment"] != actual["deployment"] {
        diff.push(format!(
            "deployment: {} -> {}",
            expected["deployment"], actual["deployment"]
        ));
    }
    // compare each backend's fingerprint
    let backends = |snapshot: &serde_json::Value| {
        snapshot["backends"]
            .as_object()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeMap<_, _>>()
    };
    let (old, new) = (backends(expected), backends(actual));
    for name in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        if old.get(name) != new.get(name) {
            diff.push(format!(
                "{name}: {} -> {}",
                old.get(name).unwrap_or(&serde_json::Value::Null),
                new.get(name).unwrap_or(&serde_json::Value::Null)
            ));
        }
    }
    // compare each object's fingerprint by backend and name
    let objects = |snapshot: &serde_json::Value| {
        snapshot["objects"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|object| {
                let key = format!(
                    "{}/{}",
                    object["backend"].as_str().unwrap_or_default(),
                    object["name"].as_str().unwrap_or_default()
                );
                (key, object["fingerprint"].clone())
            })
            .collect::<BTreeMap<_, _>>()
    };
    let (old, new) = (objects(expected), objects(actual));
    for key in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        match (old.get(key), new.get(key)) {
            (Some(_), None) => diff.push(format!("removed {key}")),
            (None, Some(_)) => diff.push(format!("added {key}")),
            (Some(before), Some(after)) if before != after => {
                diff.push(format!("changed {key}: {before} -> {after}"));
            }
            _ => (),
        }
    }
    diff
}

/// Read a golden file under `api/src/models/schema/goldens`, or rewrite it when
/// `THORIUM_BLESS=1`
///
/// # Arguments
///
/// * `name` - The golden file's name
/// * `actual` - The generated text, written when blessing
fn golden(name: &str, actual: &str) -> String {
    // find the golden file in the source tree
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/models/schema/goldens")
        .join(name);
    // rewrite the golden from the generated text when blessing
    if std::env::var("THORIUM_BLESS").is_ok_and(|value| value == "1") {
        std::fs::write(&path, actual).expect("golden file is written");
    }
    std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!("{name} is unreadable ({error}); rerun with THORIUM_BLESS=1")
    })
}
