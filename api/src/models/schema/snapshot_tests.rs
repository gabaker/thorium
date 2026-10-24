//! Tests for the committed revision snapshots and the catalog's hard checks against them

use serde_json::json;

use super::baseline::{DECLARED_SCYLLA, baseline_scylla};
use super::{
    BASELINE_REVISIONS, CatalogCheckError, Fingerprint, ObjectDef, RevisionSnapshot, SchemaBackend,
    SnapshotError, check_catalog, validate,
};
use crate::models::upgrades::{CATALOG, Catalog, CatalogEntry, StepDef, StepKind};

/// A recorded fingerprint no snapshot has, standing in for one left stale by a schema change
const STALE: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// A fixture revision whose recorded fingerprint isn't its snapshot's
const MISMATCHED: CatalogEntry = CatalogEntry {
    from: None,
    id: "2030-01-v01",
    released_with: "0.0.0",
    description: "A fixture revision recording a stale fingerprint",
    schema: Some(STALE),
    prerelease: false,
    sibling: false,
    steps: &[],
};

/// A fixture revision with a step naming an unregistered migration
const UNREGISTERED: CatalogEntry = CatalogEntry {
    from: Some("2030-01-v01"),
    id: "2030-01-v02",
    released_with: "0.0.0",
    description: "A fixture revision naming a migration nothing registers",
    schema: None,
    prerelease: false,
    sibling: false,
    steps: &[
        StepDef {
            id: "registered-step",
            kind: StepKind::Data,
            description: "A step whose migration is registered",
            thoradm_migration: Some("elastic-rebuild"),
            ..StepDef::BASE
        },
        StepDef {
            id: "unregistered-step",
            kind: StepKind::Data,
            description: "A step whose migration isn't registered",
            thoradm_migration: Some("no-such-migration"),
            ..StepDef::BASE
        },
    ],
};

/// A fixture catalog holding both fixture revisions
const FIXTURE: Catalog = Catalog {
    edges: &[MISMATCHED, UNREGISTERED],
    floors: &[],
};

/// Build a fixture snapshot of the declared schema under a revision
///
/// # Arguments
///
/// * `revision` - The revision the snapshot is for
fn declared(revision: &str) -> RevisionSnapshot {
    // snapshot the declared schema under the fixture revision
    RevisionSnapshot::declared(revision.parse().expect("a valid revision"))
}

/// A revision whose recorded fingerprint doesn't match its stored snapshot fails the hard check,
/// as does a step naming an unregistered migration id; the production catalog passes both
#[test]
fn catalog_fingerprints_and_migrations_are_consistent() {
    // the production catalog records each revision's committed snapshot
    assert_eq!(
        check_catalog(&Catalog::PRODUCTION, RevisionSnapshot::committed, None),
        Vec::new()
    );
    // a fixture catalog with a stale fingerprint, an unregistered migration, and a revision
    // without a recorded fingerprint fails each check
    let errors = check_catalog(
        &FIXTURE,
        |revision| Some(Ok(declared(revision))),
        Some(&["elastic-rebuild"]),
    );
    let deployment = declared("2030-01-v01").deployment;
    assert_eq!(
        errors,
        [
            CatalogCheckError::FingerprintMismatch {
                revision: "2030-01-v01".to_owned(),
                recorded: STALE.to_owned(),
                snapshot: deployment.clone(),
            },
            CatalogCheckError::NoSchema {
                revision: "2030-01-v02".to_owned(),
            },
            CatalogCheckError::UnregisteredMigration {
                step: "unregistered-step".to_owned(),
                migration: "no-such-migration".to_owned(),
            },
        ]
    );
    // without the registry, only the fingerprints are checked
    let errors = check_catalog(&FIXTURE, |revision| Some(Ok(declared(revision))), None);
    assert_eq!(errors.len(), 2);
    // a revision without a committed snapshot fails
    let errors = check_catalog(&FIXTURE, |_| None, Some(&["no-such-migration"]));
    assert!(errors.contains(&CatalogCheckError::NoSnapshot {
        revision: "2030-01-v01".to_owned()
    }));
    // a snapshot edited by hand without refreshing its fingerprints fails
    let tampered = |revision: &str| {
        let mut snapshot = declared(revision);
        snapshot.objects[0].canonical = json!("edited by hand");
        Some(snapshot.verify().map(|()| snapshot))
    };
    let errors = check_catalog(&FIXTURE, tampered, None);
    assert!(matches!(
        &errors[0],
        CatalogCheckError::BadSnapshot(SnapshotError::Object { revision, .. })
            if revision == "2030-01-v01"
    ));
}

/// Every catalog revision records a fingerprint and has a committed snapshot that is generated
/// text, matches its own canonical forms, and names its revision; every committed file is listed
#[test]
fn committed_snapshots_are_generated_and_complete() {
    // every catalog revision records the deployment fingerprint of its committed snapshot
    for entry in CATALOG {
        let snapshot = RevisionSnapshot::committed(entry.id)
            .unwrap_or_else(|| panic!("{} has no committed snapshot", entry.id))
            .unwrap_or_else(|error| panic!("{} has a bad snapshot: {error:?}", entry.id));
        assert_eq!(
            entry.schema,
            Some(snapshot.deployment.as_str()),
            "{} must record its snapshot's deployment fingerprint",
            entry.id
        );
    }
    // the baseline, the first Helm revision, and the revision that applies the schema first are
    // snapshotted
    let ids = RevisionSnapshot::committed_ids().collect::<Vec<_>>();
    assert_eq!(ids, ["2026-10-v01", "2026-10-v02", "2026-10-v03"]);
    // every file in the snapshot directory is listed, and each is the generator's exact text
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/revisions");
    let mut files = std::fs::read_dir(&dir)
        .expect("the snapshot directory reads")
        .map(|entry| entry.expect("a directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    files.sort();
    let mut expected = ids
        .iter()
        .map(|id| format!("{id}.json"))
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(files, expected);
    for id in ids {
        let text = std::fs::read_to_string(dir.join(format!("{id}.json"))).expect("snapshot reads");
        let snapshot = RevisionSnapshot::committed(id)
            .expect("listed")
            .expect("valid");
        assert_eq!(snapshot.revision.as_str(), id);
        assert_eq!(
            text,
            snapshot.to_file_text(),
            "{id}.json was edited by hand"
        );
    }
    // the revisions released before the registry are snapshots of the baseline
    assert_eq!(BASELINE_REVISIONS, ["2026-10-v01", "2026-10-v02"]);
    for id in BASELINE_REVISIONS {
        let snapshot = RevisionSnapshot::committed(id)
            .expect("listed")
            .expect("valid");
        assert_eq!(
            snapshot,
            RevisionSnapshot::baseline(id.parse().expect("a valid revision")),
            "{id}.json isn't the baseline's snapshot"
        );
    }
    // a snapshot's fingerprints are checked against its canonical forms
    let mut snapshot = declared("2030-01-v01");
    assert_eq!(snapshot.verify(), Ok(()));
    snapshot.deployment = Fingerprint::of(&json!("another"));
    assert_eq!(
        snapshot.verify(),
        Err(SnapshotError::Deployment {
            revision: "2030-01-v01".to_owned()
        })
    );
    // objects out of order, or an object listed twice, fail
    let order = Err(SnapshotError::Order {
        revision: "2030-01-v01".to_owned(),
    });
    let mut swapped = declared("2030-01-v01");
    swapped.objects.swap(0, 1);
    assert_eq!(swapped.verify(), order);
    let mut doubled = declared("2030-01-v01");
    doubled.objects.insert(1, doubled.objects[0].clone());
    assert_eq!(doubled.verify(), order);
    // a backend fingerprint the objects don't give fails, even with a matching deployment
    let mut tampered = declared("2030-01-v01");
    tampered
        .backends
        .insert(SchemaBackend::Redis, Fingerprint::of(&json!("another")));
    tampered.deployment = RevisionSnapshot::deployment_fingerprint(&tampered.backends);
    assert_eq!(
        tampered.verify(),
        Err(SnapshotError::Backends {
            revision: "2030-01-v01".to_owned()
        })
    );
    // a file holding another revision's snapshot fails
    let v01 = RevisionSnapshot::committed("2026-10-v01")
        .expect("listed")
        .expect("valid")
        .to_file_text();
    assert_eq!(
        RevisionSnapshot::parse("2026-10-v02", &v01),
        Err(SnapshotError::Revision {
            revision: "2026-10-v02".to_owned(),
            found: "2026-10-v01".to_owned(),
        })
    );
    // the generated text parses back to the same snapshot
    let snapshot = declared("2030-01-v01");
    let parsed: RevisionSnapshot =
        serde_json::from_str(&snapshot.to_file_text()).expect("generated text parses");
    assert_eq!(parsed, snapshot);
}

/// The baseline holds every object the setup statements created before the registry existed:
/// the declared objects named in the legacy setup statements, plus the `entities_by_name` view and
/// the `events` table the registry leaves out, each following the registry's rules, and every
/// layout at version 1 except the users family at 2
#[test]
fn baseline_is_the_released_schema() {
    // the declared objects are the ones the legacy setup statements create, in their order
    let legacy = include_str!("goldens/legacy_ddl.cql");
    let marker = "IF NOT EXISTS {ns}.";
    let legacy_names = legacy
        .match_indices(marker)
        .map(|(start, _)| {
            let rest = &legacy[start + marker.len()..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            &rest[..end]
        })
        .collect::<Vec<_>>();
    assert_eq!(legacy_names, DECLARED_SCYLLA);
    // the baseline's Scylla objects follow the registry's rules, with each view after its base
    let scylla = baseline_scylla();
    assert_eq!(validate(&scylla), Ok(()));
    let mut names = scylla.iter().map(ObjectDef::name).collect::<Vec<_>>();
    assert_eq!(names.len(), DECLARED_SCYLLA.len() + 2);
    names.retain(|name| !DECLARED_SCYLLA.contains(name));
    assert_eq!(names, ["entities_by_name", "events"]);
    // the elastic indexes and every layout are in both baseline revisions' snapshot
    let snapshot = RevisionSnapshot::baseline("2026-10-v02".parse().expect("a valid revision"));
    let count = |backend| {
        snapshot
            .objects
            .iter()
            .filter(|object| object.backend == backend)
            .count()
    };
    assert_eq!(count(SchemaBackend::Elastic), 4);
    assert_eq!(count(SchemaBackend::Redis), 23);
    assert_eq!(count(SchemaBackend::S3), 7);
    for object in &snapshot.objects {
        let expected = match (object.backend, object.name.as_str()) {
            (SchemaBackend::Redis, "users") => json!({ "users": 2 }),
            (SchemaBackend::Redis | SchemaBackend::S3, name) => json!({ name: 1 }),
            _ => continue,
        };
        assert_eq!(object.canonical, expected, "{} layout", object.name);
    }
}
