//! Tests of the API startup check's messages and of how it judges the applied fingerprint

use serde_json::{Value, json};

use super::check::judge_applied;
use super::{
    Fingerprint, RevisionSnapshot, SchemaBackend, SchemaCheckError, SchemaCheckPass, SnapshotError,
    SnapshotObject,
};
use crate::models::upgrades::{Catalog, CatalogEntry, RevisionId};

/// Build a fingerprint that stands for a named schema
///
/// # Arguments
///
/// * `name` - What the schema is called
fn fp(name: &str) -> Fingerprint {
    // hash the name like any canonical form
    Fingerprint::of(&json!(name))
}

/// Build a revision id
///
/// # Arguments
///
/// * `id` - The revision's text
fn revision(id: &str) -> RevisionId {
    // the ids in these tests are well formed
    id.parse().expect("a valid revision id")
}

/// Every refusal names what the admin does next, and the ones about a revision name it
#[test]
fn refusals_name_the_next_step() {
    // a missing schema says to apply it
    let missing = SchemaCheckError::Missing.to_string();
    assert!(missing.contains("thoradm schema apply"), "{missing}");
    // an older schema names the revision this build needs applied
    let older = SchemaCheckError::Older {
        applied: fp("old"),
        declared: fp("new"),
        revision: revision("2026-10-v03"),
    }
    .to_string();
    assert!(
        older.contains("hasn't applied revision 2026-10-v03 yet"),
        "{older}"
    );
    assert!(older.contains(fp("old").as_str()) && older.contains(fp("new").as_str()));
    // a newer schema names the revision and every breaking object
    let newer = SchemaCheckError::Newer {
        applied: fp("later"),
        declared: fp("new"),
        revision: revision("2026-10-v04"),
        breaking: vec!["nodes.cluster".to_owned(), "tags".to_owned()],
    }
    .to_string();
    assert!(newer.contains("2026-10-v04"), "{newer}");
    assert!(newer.contains("nodes.cluster, tags"), "{newer}");
    // an unknown fingerprint names both fingerprints and the command that compares them
    let unknown = SchemaCheckError::UnknownFingerprint {
        applied: fp("other"),
        declared: fp("new"),
    }
    .to_string();
    assert!(unknown.contains(fp("other").as_str()) && unknown.contains(fp("new").as_str()));
    assert!(unknown.contains("thoradm schema diff"), "{unknown}");
    // a changed setting names only the setting
    let changed = SchemaCheckError::ConfigChanged {
        setting: "thorium.files.password".to_owned(),
    }
    .to_string();
    assert!(changed.contains("thorium.files.password"), "{changed}");
    // the other two say what failed
    assert!(
        SchemaCheckError::PasswordHashParams
            .to_string()
            .contains("argon2id")
    );
    let unreadable = SchemaCheckError::Unreadable("timed out".to_owned()).to_string();
    assert!(unreadable.contains("timed out"), "{unreadable}");
}

/// A current schema starts silently, and a later additive one starts with a warning that names its
/// revision
#[test]
fn only_a_later_additive_schema_warns() {
    assert_eq!(SchemaCheckPass::Current.warning(), None);
    let warning = SchemaCheckPass::NewerAdditive {
        applied: fp("later"),
        revision: revision("2026-10-v04"),
    }
    .warning()
    .expect("a later schema warns");
    assert!(warning.contains("2026-10-v04"), "{warning}");
    assert!(warning.contains(fp("later").as_str()), "{warning}");
}

/// The revision this build declares in these tests
const OWN: &str = "2026-10-v03";

/// The older revision a deployment can still be at
const OLDER: &str = "2026-10-v02";

/// The later revision the fixture catalog knows
const LATER: &str = "2026-10-v04";

/// Build a snapshot of a revision from a list of objects, sorted and fingerprinted
///
/// # Arguments
///
/// * `revision_id` - The revision the snapshot belongs to
/// * `objects` - The objects it holds
fn snapshot(revision_id: &str, mut objects: Vec<SnapshotObject>) -> RevisionSnapshot {
    // objects are listed by backend and name
    objects.sort_by(|a, b| (a.backend, &a.name).cmp(&(b.backend, &b.name)));
    let backends = RevisionSnapshot::backend_fingerprints(&objects);
    RevisionSnapshot {
        revision: revision(revision_id),
        deployment: RevisionSnapshot::deployment_fingerprint(&backends),
        backends,
        objects,
    }
}

/// Rebuild an object with a changed canonical form and the fingerprint that form gives
///
/// # Arguments
///
/// * `object` - The object to change
/// * `change` - Edits the canonical form
fn edited(object: &SnapshotObject, change: impl FnOnce(&mut Value)) -> SnapshotObject {
    // edit a copy of the canonical form and hash it
    let mut canonical = object.canonical.clone();
    change(&mut canonical);
    SnapshotObject {
        backend: object.backend,
        name: object.name.clone(),
        fingerprint: Fingerprint::of(&canonical),
        canonical,
    }
}

/// The snapshot of what this build declares
fn declared() -> RevisionSnapshot {
    // the declared objects at the build's own revision
    RevisionSnapshot::declared(revision(OWN))
}

/// The declared snapshot with one Scylla table changed, as the later revision
///
/// # Arguments
///
/// * `table` - The table to change
/// * `change` - Edits the table's canonical form
fn later_with(table: &str, change: impl FnOnce(&mut Value)) -> RevisionSnapshot {
    // swap the changed table in
    let mut objects = declared().objects;
    let slot = objects
        .iter_mut()
        .find(|object| object.backend == SchemaBackend::Scylla && object.name == table)
        .expect("the table is declared");
    *slot = edited(slot, change);
    snapshot(LATER, objects)
}

/// Build the fixture catalog: an older revision, this build's own, and a later one
///
/// # Arguments
///
/// * `later` - The later revision's snapshot, if the catalog knows one
fn catalog(later: Option<&RevisionSnapshot>) -> Catalog {
    // the older revision's schema is one this build never declared
    let older = Box::leak(fp("older").as_str().to_owned().into_boxed_str());
    let own = Box::leak(declared().deployment.as_str().to_owned().into_boxed_str());
    let edge = |from, id, schema| CatalogEntry {
        from,
        id,
        released_with: "0.0.0",
        description: "A fixture edge",
        schema,
        prerelease: false,
        sibling: false,
        steps: &[],
    };
    let mut edges = vec![
        edge(None, "2026-10-v01", Some(&*older)),
        edge(Some("2026-10-v01"), OLDER, Some(&*older)),
        edge(Some(OLDER), OWN, Some(&*own)),
    ];
    // a later edge carries the later snapshot's fingerprint
    if let Some(later) = later {
        let schema = Box::leak(later.deployment.as_str().to_owned().into_boxed_str());
        edges.push(edge(Some(OWN), LATER, Some(&*schema)));
    }
    Catalog {
        edges: Box::leak(edges.into_boxed_slice()),
        floors: &[],
    }
}

/// Judge an applied fingerprint against the fixture catalog, loading the later snapshot when one
/// is given
///
/// # Arguments
///
/// * `later` - The later revision's snapshot, if the catalog knows one
/// * `applied` - The fingerprint recorded in the keyspace
fn judged(
    later: Option<&RevisionSnapshot>,
    applied: &Fingerprint,
) -> Result<SchemaCheckPass, SchemaCheckError> {
    // the loader knows only the later revision
    let load = |id: &str| -> Option<Result<RevisionSnapshot, SnapshotError>> {
        (id == LATER).then(|| later.cloned().map(Ok)).flatten()
    };
    judge_applied(&catalog(later), &load, &declared(), applied)
}

/// The fingerprint this build declares passes as current
#[test]
fn the_declared_fingerprint_is_current() {
    assert_eq!(
        judged(None, &declared().deployment),
        Ok(SchemaCheckPass::Current)
    );
}

/// An applied fingerprint of an earlier revision is refused, naming the revision this build needs
#[test]
fn an_older_fingerprint_names_the_revision_this_build_needs() {
    let err = judged(None, &fp("older")).expect_err("an older schema is refused");
    assert_eq!(
        err,
        SchemaCheckError::Older {
            applied: fp("older"),
            declared: declared().deployment,
            revision: revision(OWN),
        }
    );
}

/// A later revision the catalog knows, whose only differences are a new table and a new column, lets
/// the API start with a warning naming the revision
#[test]
fn a_later_additive_revision_starts_with_a_warning() {
    // add a column to one table and a whole new table
    let mut later = later_with("nodes", |canonical| {
        canonical["columns"]
            .as_array_mut()
            .expect("columns")
            .push(json!({"name": "zone", "type": "text"}));
    });
    let nodes = later
        .objects
        .iter()
        .find(|object| object.name == "nodes")
        .expect("nodes")
        .clone();
    let mut objects = later.objects.clone();
    objects.push(edited(&nodes, |canonical| {
        canonical["name"] = json!("fixture_extra");
    }));
    objects.last_mut().expect("the new table").name = "fixture_extra".to_owned();
    later = snapshot(LATER, objects);
    let pass = judged(Some(&later), &later.deployment).expect("an additive schema starts");
    assert_eq!(
        pass,
        SchemaCheckPass::NewerAdditive {
            applied: later.deployment.clone(),
            revision: revision(LATER),
        }
    );
}

/// A later revision that drops a column the build reads, or changes a column's type, or drops a
/// table, is refused naming the revision and each changed object
#[test]
fn a_later_breaking_revision_is_refused() {
    // a dropped column
    let dropped = later_with("nodes", |canonical| {
        canonical["columns"]
            .as_array_mut()
            .expect("columns")
            .retain(|column| column["name"] != "health");
    });
    // a changed column type
    let retyped = later_with("nodes", |canonical| {
        for column in canonical["columns"].as_array_mut().expect("columns") {
            if column["name"] == "resources" {
                column["type"] = json!("blob");
            }
        }
    });
    for (later, breaking) in [(dropped, "nodes.health"), (retyped, "nodes.resources")] {
        let err = judged(Some(&later), &later.deployment).expect_err("a breaking schema");
        match err {
            SchemaCheckError::Newer {
                applied,
                declared: own,
                revision: found,
                breaking: names,
            } => {
                assert_eq!(applied, later.deployment);
                assert_eq!(own, declared().deployment);
                assert_eq!(found, revision(LATER));
                assert_eq!(names, vec![breaking.to_owned()]);
            }
            other => panic!("not a newer schema: {other:?}"),
        }
    }
    // a dropped table is named without a column
    let mut objects = declared().objects;
    objects.retain(|object| object.name != "nodes");
    let gone = snapshot(LATER, objects);
    let err = judged(Some(&gone), &gone.deployment).expect_err("a dropped table");
    assert!(
        matches!(&err, SchemaCheckError::Newer { breaking, .. } if breaking == &["nodes".to_owned()]),
        "{err:?}"
    );
}

/// A fingerprint no catalog revision names, and one a catalog revision names but whose snapshot
/// can't be loaded, are both refused as unknown
#[test]
fn an_unnamed_or_unloadable_fingerprint_is_unknown() {
    // no revision names it
    let err = judged(None, &fp("unnamed")).expect_err("an unknown schema is refused");
    assert_eq!(
        err,
        SchemaCheckError::UnknownFingerprint {
            applied: fp("unnamed"),
            declared: declared().deployment,
        }
    );
    // a revision names it but its snapshot isn't committed
    let later = later_with("nodes", |canonical| {
        canonical["columns"]
            .as_array_mut()
            .expect("columns")
            .retain(|column| column["name"] != "health");
    });
    let load = |_: &str| -> Option<Result<RevisionSnapshot, SnapshotError>> { None };
    let err = judge_applied(
        &catalog(Some(&later)),
        &load,
        &declared(),
        &later.deployment,
    )
    .expect_err("a revision without a snapshot can't be judged");
    assert!(
        matches!(err, SchemaCheckError::UnknownFingerprint { .. }),
        "{err:?}"
    );
}

/// A later revision that only changes an Expiring object's layout is still refused: the change is
/// informational for a migration, but a build reading the old layout can't read the new one
#[test]
fn a_later_change_to_an_expiring_object_is_refused() {
    // bump the layout of the cursors family, whose class is Expiring
    let mut objects = declared().objects;
    let slot = objects
        .iter_mut()
        .find(|object| object.backend == SchemaBackend::Redis && object.name == "cursors")
        .expect("the cursors family is declared");
    *slot = edited(slot, |canonical| {
        // one version past the declared one
        let next = canonical["cursors"].as_u64().expect("a layout version") + 1;
        canonical["cursors"] = json!(next);
    });
    let later = snapshot(LATER, objects);
    let err = judged(Some(&later), &later.deployment).expect_err("a layout bump is refused");
    assert!(
        matches!(&err, SchemaCheckError::Newer { breaking, .. } if breaking == &["cursors".to_owned()]),
        "{err:?}"
    );
}
