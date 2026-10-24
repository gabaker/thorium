//! Tests for the class table: its completeness, its entries, and the classes it gives

use std::collections::BTreeSet;

use super::scylla::ThoriumKeyspace;
use super::{
    BackupCoverage, CLASSES, ClassEntry, DataClass, Keyspace, ObjectDef, REDIS_FAMILIES,
    RevisionSnapshot, SchemaBackend, Store,
};
use crate::models::upgrades::{CATALOG, LAYOUTS};

/// Lists the objects that don't have exactly one class table entry of their store
///
/// # Arguments
///
/// * `objects` - The objects to look up, by store and name
/// * `classes` - The class table to look them up in
fn unclassified(objects: &[(Store, &str)], classes: &[ClassEntry]) -> Vec<String> {
    // count the entries of each object's store and name
    objects
        .iter()
        .filter_map(|(store, name)| {
            let found = classes
                .iter()
                .filter(|entry| entry.store == *store && entry.name == *name)
                .count();
            (found != 1).then(|| format!("{} {name} has {found} entries", store.label()))
        })
        .collect()
}

/// Lists every Scylla table and view the keyspace declares, by store and name
fn declared_scylla() -> Vec<(Store, &'static str)> {
    // a view is a view entry, everything else a table entry
    ThoriumKeyspace::OBJECTS
        .iter()
        .map(|object| match object {
            ObjectDef::View(_) => (Store::ScyllaView, object.name()),
            ObjectDef::Table(_) => (Store::ScyllaTable, object.name()),
        })
        .collect()
}

/// Lists the layout families `LAYOUTS` declares for a backend
///
/// # Arguments
///
/// * `backend` - The backend whose families to list
fn layout_families(backend: SchemaBackend) -> Vec<&'static str> {
    // keep the families of the backend, in declaration order
    LAYOUTS
        .iter()
        .filter(|layout| layout.backend == backend)
        .map(|layout| layout.family)
        .collect()
}

/// Every Scylla table and view, Redis layout family, and S3 bucket role the registries declare
/// has exactly one class table entry, no entry names an object that doesn't exist, and a declared
/// object with no entry is reported
#[test]
fn every_declared_object_has_one_class() {
    // every declared table and view has exactly one entry of its kind
    let scylla = declared_scylla();
    assert_eq!(unclassified(&scylla, CLASSES), Vec::<String>::new());
    // every S3 bucket role has exactly one entry
    let roles = layout_families(SchemaBackend::S3)
        .into_iter()
        .map(|role| (Store::S3, role))
        .collect::<Vec<_>>();
    assert_eq!(unclassified(&roles, CLASSES), Vec::<String>::new());
    // every Redis layout family has exactly one class, listed in the order LAYOUTS has them
    let families = REDIS_FAMILIES
        .iter()
        .map(|entry| entry.family)
        .collect::<Vec<_>>();
    assert_eq!(families, layout_families(SchemaBackend::Redis));
    // a Scylla entry names a declared object or a legacy one an older release created
    let released = RevisionSnapshot::baseline("2026-10-v01".parse().expect("a valid revision"));
    let released = released
        .objects
        .iter()
        .filter(|object| object.backend == SchemaBackend::Scylla)
        .map(|object| object.name.as_str())
        .collect::<BTreeSet<_>>();
    for entry in CLASSES {
        match entry.store {
            Store::ScyllaTable | Store::ScyllaView if entry.legacy => {
                assert!(
                    !scylla.iter().any(|(_, name)| *name == entry.name),
                    "{} is declared, so it isn't legacy",
                    entry.name
                );
                assert!(
                    released.contains(entry.name),
                    "{} is legacy but no older release created it",
                    entry.name
                );
            }
            Store::ScyllaTable | Store::ScyllaView => assert!(
                scylla.contains(&(entry.store, entry.name)),
                "{} {} isn't declared",
                entry.store.label(),
                entry.name
            ),
            Store::S3 => assert!(
                roles.contains(&(Store::S3, entry.name)),
                "bucket role {} isn't declared",
                entry.name
            ),
            Store::Redis => assert!(!entry.legacy, "{} can't be legacy", entry.name),
        }
    }
    // a declared object without an entry, or with two, is reported by name
    let missing = [(Store::ScyllaTable, "no_such_table")];
    assert_eq!(
        unclassified(&missing, CLASSES),
        ["Scylla table no_such_table has 0 entries"]
    );
    let doubled = [CLASSES[0], CLASSES[0]];
    assert_eq!(
        unclassified(&[(CLASSES[0].store, CLASSES[0].name)], &doubled),
        [format!("Scylla table {} has 2 entries", CLASSES[0].name)]
    );
}

/// Each entry holds what its store and class need: a backed-up object is core data (a view,
/// derived data), an excluded one gives a reason, a view names a base table with an entry, and
/// only backed-up tables list columns
#[test]
fn entries_are_well_formed() {
    // check each entry for what its kind needs
    for entry in CLASSES {
        match entry.backup {
            BackupCoverage::BackedUp => {
                // a view is derived from its base table; everything else backed up is core
                let class = if entry.store == Store::ScyllaView {
                    DataClass::Derived
                } else {
                    DataClass::Core
                };
                assert_eq!(entry.class, class, "{}", entry.name);
            }
            BackupCoverage::Excluded { reason } => {
                assert!(!reason.trim().is_empty(), "{} has no reason", entry.name);
            }
        }
        if let Some(gap) = entry.gap {
            assert!(!gap.trim().is_empty(), "{} has an empty gap", entry.name);
        }
        match entry.store {
            Store::ScyllaView => {
                let base = entry
                    .base_table
                    .unwrap_or_else(|| panic!("{} has no base", entry.name));
                let base_entry = CLASSES
                    .iter()
                    .find(|other| other.store == Store::ScyllaTable && other.name == base)
                    .unwrap_or_else(|| panic!("{}'s base {base} has no entry", entry.name));
                assert_eq!(base_entry.backup, entry.backup, "{}", entry.name);
                assert!(entry.columns.is_none());
            }
            Store::ScyllaTable => {
                let backed_up = entry.backup == BackupCoverage::BackedUp;
                assert_eq!(entry.columns.is_some(), backed_up, "{}", entry.name);
                assert!(entry.base_table.is_none());
            }
            Store::Redis | Store::S3 => {
                assert!(entry.columns.is_none() && entry.base_table.is_none());
            }
        }
        // only S3 entries name key prefixes
        assert!(entry.prefixes.is_empty() || entry.store == Store::S3);
    }
}

/// `cursors` is `Expiring`, and the reactions, jobs, workers and events state is `Runtime`, both
/// as layout families and as the key families backup coverage lists
#[test]
fn cursors_expire_and_working_state_is_runtime() {
    // the saved cursors expire, as a layout family and as keys
    assert_eq!(
        DataClass::of(SchemaBackend::Redis, "cursors"),
        Some(DataClass::Expiring)
    );
    let key_class = |pattern: &str| {
        CLASSES
            .iter()
            .find(|entry| entry.store == Store::Redis && entry.name == pattern)
            .map(|entry| entry.class)
    };
    assert_eq!(key_class("cursors:*"), Some(DataClass::Expiring));
    // reactions, jobs, workers and events have no TTL guarantee
    for family in ["reactions", "jobs", "workers", "events"] {
        assert_eq!(
            DataClass::of(SchemaBackend::Redis, family),
            Some(DataClass::Runtime),
            "{family}"
        );
    }
    for pattern in [
        "reactions:*",
        "reaction_data:*",
        "job_queue:*",
        "job_data:*",
        "workers:*",
        "worker_data:*",
        "event-handler:*",
    ] {
        assert_eq!(key_class(pattern), Some(DataClass::Runtime), "{pattern}");
    }
    // the events table older releases created is runtime state too
    assert_eq!(
        DataClass::of(SchemaBackend::Scylla, "events"),
        Some(DataClass::Runtime)
    );
    // core and derived objects keep their classes, and Elastic indexes aren't classified
    assert_eq!(
        DataClass::of(SchemaBackend::Scylla, "samples_list"),
        Some(DataClass::Core)
    );
    assert_eq!(
        DataClass::of(SchemaBackend::Redis, "census_samples"),
        Some(DataClass::Derived)
    );
    assert_eq!(
        DataClass::of(SchemaBackend::S3, "ephemeral"),
        Some(DataClass::Runtime)
    );
    assert_eq!(DataClass::of(SchemaBackend::Elastic, "samples"), None);
    assert_eq!(DataClass::of(SchemaBackend::Scylla, "no_such_table"), None);
}

/// The class table is outside the canonical forms: every committed revision snapshot is still the
/// text its revision generates, and the declared schema's fingerprints match their golden
#[test]
fn the_class_table_moves_no_fingerprint() {
    // every committed snapshot is the text its revision generates today
    for entry in CATALOG {
        // a pre-release records its branch's layouts, which the declared schema doesn't hold
        if entry.revision().is_prerelease() {
            continue;
        }
        let committed = RevisionSnapshot::committed(entry.id)
            .unwrap_or_else(|| panic!("{} has no committed snapshot", entry.id))
            .unwrap_or_else(|error| panic!("{} has a bad snapshot: {error:?}", entry.id));
        let revision = entry.id.parse().expect("a valid revision");
        let generated = if super::BASELINE_REVISIONS.contains(&entry.id) {
            RevisionSnapshot::baseline(revision)
        } else {
            RevisionSnapshot::declared(revision)
        };
        assert_eq!(
            committed.to_file_text(),
            generated.to_file_text(),
            "{}",
            entry.id
        );
    }
    // the declared deployment fingerprint is the one the golden records
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("goldens/fingerprints.json")).expect("golden parses");
    let declared = RevisionSnapshot::declared("2030-01-v01".parse().expect("a valid revision"));
    assert_eq!(
        golden["deployment"],
        serde_json::to_value(&declared.deployment).expect("fingerprint renders")
    );
}

/// The class every class table row is expected to carry, written out row by row so that a change to
/// one entry's class fails here and can't pass as a consequence of the table it is checked against
const EXPECTED_CLASSES: &[(Store, &str, DataClass)] = &[
    (Store::ScyllaTable, "samples_list", DataClass::Core),
    (Store::ScyllaTable, "s3_ids", DataClass::Core),
    (Store::ScyllaTable, "comments", DataClass::Core),
    (Store::ScyllaTable, "results", DataClass::Core),
    (Store::ScyllaTable, "results_stream", DataClass::Core),
    (Store::ScyllaTable, "tags", DataClass::Core),
    (Store::ScyllaTable, "repo_data", DataClass::Core),
    (Store::ScyllaTable, "repos_list", DataClass::Core),
    (Store::ScyllaTable, "commitishes", DataClass::Core),
    (Store::ScyllaTable, "commitish_list", DataClass::Core),
    (Store::ScyllaTable, "network_policies", DataClass::Core),
    (Store::ScyllaTable, "entities", DataClass::Core),
    (Store::ScyllaTable, "associations", DataClass::Core),
    (Store::ScyllaTable, "notifications", DataClass::Core),
    (Store::ScyllaTable, "nodes", DataClass::Core),
    (Store::ScyllaTable, "logs", DataClass::Runtime),
    (Store::ScyllaTable, "events", DataClass::Runtime),
    (Store::ScyllaTable, "schema_migrations", DataClass::Core),
    (
        Store::ScyllaTable,
        "schema_migration_issues",
        DataClass::Core,
    ),
    (Store::ScyllaView, "samples", DataClass::Derived),
    (Store::ScyllaView, "s3_sha256s", DataClass::Derived),
    (Store::ScyllaView, "comments_by_id", DataClass::Derived),
    (Store::ScyllaView, "results_auth", DataClass::Derived),
    (Store::ScyllaView, "results_ids", DataClass::Derived),
    (Store::ScyllaView, "tags_by_item", DataClass::Derived),
    (
        Store::ScyllaView,
        "tags_case_insensitive",
        DataClass::Derived,
    ),
    (Store::ScyllaView, "repos", DataClass::Derived),
    (Store::ScyllaView, "committed_repo_data", DataClass::Derived),
    (
        Store::ScyllaView,
        "network_policies_by_name",
        DataClass::Derived,
    ),
    (
        Store::ScyllaView,
        "network_policies_default",
        DataClass::Derived,
    ),
    (Store::ScyllaView, "entities_by_id", DataClass::Derived),
    (Store::ScyllaView, "entities_by_name", DataClass::Derived),
    (Store::Redis, "users", DataClass::Core),
    (Store::Redis, "user_data:*", DataClass::Core),
    (Store::Redis, "user_groups:*", DataClass::Core),
    (Store::Redis, "users_token_map", DataClass::Core),
    (Store::Redis, "users_email_map", DataClass::Core),
    (Store::Redis, "oauth:alias_to_user:*", DataClass::Core),
    (Store::Redis, "analysts", DataClass::Core),
    (Store::Redis, "groups", DataClass::Core),
    (Store::Redis, "groups_data:*", DataClass::Core),
    (Store::Redis, "groups_direct:*", DataClass::Core),
    (Store::Redis, "groups_metagroups:*", DataClass::Core),
    (Store::Redis, "groups_combined:*", DataClass::Core),
    (Store::Redis, "images:*", DataClass::Core),
    (Store::Redis, "image_data:*", DataClass::Core),
    (Store::Redis, "image_used_by:*", DataClass::Core),
    (Store::Redis, "pipelines:*", DataClass::Core),
    (Store::Redis, "pipeline_data:*", DataClass::Core),
    (Store::Redis, "system_settings", DataClass::Core),
    (Store::Redis, "scoped_token_map", DataClass::Core),
    (Store::Redis, "scoped_tokens:*", DataClass::Core),
    (Store::Redis, "netpols:*", DataClass::Core),
    (Store::Redis, "netpol_used_by:*", DataClass::Derived),
    (Store::Redis, "census:*", DataClass::Derived),
    (Store::Redis, "system_info", DataClass::Derived),
    (Store::Redis, "system_keys", DataClass::Runtime),
    (Store::Redis, "reactions:*", DataClass::Runtime),
    (Store::Redis, "reactions_group:*", DataClass::Runtime),
    (Store::Redis, "reaction_data:*", DataClass::Runtime),
    (Store::Redis, "reaction_jobs:*", DataClass::Runtime),
    (Store::Redis, "reaction_tags:*", DataClass::Runtime),
    (Store::Redis, "reaction_cache:*", DataClass::Runtime),
    (Store::Redis, "sub_reactions:*", DataClass::Runtime),
    (Store::Redis, "generators:*", DataClass::Runtime),
    (Store::Redis, "stage_logs:*", DataClass::Runtime),
    (Store::Redis, "logs:*", DataClass::Runtime),
    (Store::Redis, "job_queue:*", DataClass::Runtime),
    (Store::Redis, "job_data:*", DataClass::Runtime),
    (Store::Redis, "workers:*", DataClass::Runtime),
    (Store::Redis, "worker_data:*", DataClass::Runtime),
    (Store::Redis, "streams:*", DataClass::Runtime),
    (Store::Redis, "event-handler:*", DataClass::Runtime),
    (Store::Redis, "search-streamer:*", DataClass::Runtime),
    (Store::Redis, "cursors:*", DataClass::Expiring),
    (Store::Redis, "schema-writer", DataClass::Runtime),
    (Store::Redis, "rebucket:*", DataClass::Runtime),
    (Store::Redis, "image_ttc:*", DataClass::Runtime),
    (Store::Redis, "oauth:context:*", DataClass::Expiring),
    (
        Store::Redis,
        "oauth:registration_sessions:*",
        DataClass::Expiring,
    ),
    (Store::Redis, "oauth:link_token:*", DataClass::Expiring),
    (Store::S3, "files", DataClass::Core),
    (Store::S3, "repos", DataClass::Core),
    (Store::S3, "attachments", DataClass::Core),
    (Store::S3, "results", DataClass::Core),
    (Store::S3, "graphics", DataClass::Core),
    (Store::S3, "ephemeral", DataClass::Runtime),
    (Store::S3, "reaction_cache", DataClass::Runtime),
];

/// The class every Redis layout family is expected to carry, in `REDIS_FAMILIES` order
const EXPECTED_FAMILIES: &[(&str, DataClass)] = &[
    ("users", DataClass::Core),
    ("groups", DataClass::Core),
    ("images", DataClass::Core),
    ("pipelines", DataClass::Core),
    ("network_policies", DataClass::Core),
    ("jobs", DataClass::Runtime),
    ("reactions", DataClass::Runtime),
    ("streams", DataClass::Runtime),
    ("system", DataClass::Core),
    ("workers", DataClass::Runtime),
    ("events", DataClass::Runtime),
    ("search_events", DataClass::Runtime),
    ("search_streamer_init", DataClass::Runtime),
    ("oauth", DataClass::Core),
    ("vendors", DataClass::Core),
    ("cursors", DataClass::Expiring),
    ("census_samples", DataClass::Derived),
    ("census_repos", DataClass::Derived),
    ("census_commitish", DataClass::Derived),
    ("census_tags", DataClass::Derived),
    ("census_tags_case_insensitive", DataClass::Derived),
    ("census_entities", DataClass::Derived),
    ("census_associations", DataClass::Derived),
];

/// Every class table row and every Redis layout family has exactly the class written for it in
/// the expected tables, and the tables list nothing else; a lookup by backend and name agrees
#[test]
fn every_row_has_its_expected_class() {
    // the tables hold the same rows in the same order
    let classes = CLASSES
        .iter()
        .map(|entry| (entry.store, entry.name, entry.class))
        .collect::<Vec<_>>();
    assert_eq!(classes.len(), EXPECTED_CLASSES.len());
    for (found, expected) in classes.iter().zip(EXPECTED_CLASSES) {
        assert_eq!(found, expected);
    }
    let families = REDIS_FAMILIES
        .iter()
        .map(|entry| (entry.family, entry.class))
        .collect::<Vec<_>>();
    assert_eq!(families.len(), EXPECTED_FAMILIES.len());
    for (found, expected) in families.iter().zip(EXPECTED_FAMILIES) {
        assert_eq!(found, expected);
    }
    // looking an object up by its backend and name gives its class
    for (store, name, class) in EXPECTED_CLASSES {
        let found = match store {
            Store::Redis => CLASSES
                .iter()
                .find(|entry| entry.store == Store::Redis && entry.name == *name)
                .map(|entry| entry.class),
            _ => DataClass::of(store.backend(), name),
        };
        assert_eq!(found, Some(*class), "{name}");
    }
    for (family, class) in EXPECTED_FAMILIES {
        assert_eq!(
            DataClass::of(SchemaBackend::Redis, family),
            Some(*class),
            "{family}"
        );
    }
}
