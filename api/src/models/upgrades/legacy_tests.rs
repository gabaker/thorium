//! Tests for the legacy registry: its entries are valid, append-only, and agree with the class
//! table and the revision snapshots

use std::collections::BTreeSet;

use super::{
    Catalog, LEGACY, LegacyDef, LegacyKind, LegacyNeed, RevisionId, SchemaBackend, is_legacy,
    legacy_def,
};
use crate::models::schema::{CLASSES, DataClass, RevisionSnapshot, SchemaObject, Store};

/// The ids the registry has held, in order; the registry may only grow, so every id here must
/// stay, and a new entry is appended here in the same change
///
/// The one exception is an object the build starts declaring again, whose guard entry leaves
/// with the change that declares it: the `entities_by_name` view left when exact-name entity
/// reads brought it back.
const GUARD: &[&str] = &["scylla:events"];

/// Every entry's id is unique and none is a declared object; each revision that created it has a
/// snapshot listing it as the entry's kind, each revision that uses it exists, was one that
/// created it, and the entry's note is not blank
#[test]
fn legacy_registry_is_valid() {
    // ids are unique
    let ids = LEGACY.iter().map(LegacyDef::id).collect::<Vec<_>>();
    assert_eq!(
        ids.iter().collect::<BTreeSet<_>>().len(),
        ids.len(),
        "{ids:?}"
    );
    // a declared object is never legacy
    let declared = SchemaObject::declared();
    for entry in LEGACY {
        assert!(
            !declared
                .iter()
                .any(|object| object.backend == entry.backend && object.name == entry.name),
            "{} is declared, so it isn't legacy",
            entry.id()
        );
        assert!(!entry.retirement.trim().is_empty(), "{}", entry.id());
        assert!(!entry.created_by.is_empty(), "{}", entry.id());
        // every revision that created it has a snapshot listing it with the entry's kind
        for revision in entry.created_by {
            let snapshot = RevisionSnapshot::committed(revision)
                .unwrap_or_else(|| panic!("{revision} has no snapshot"))
                .expect("the snapshot is valid");
            let object = snapshot
                .objects
                .iter()
                .find(|object| object.backend == entry.backend && object.name == entry.name)
                .unwrap_or_else(|| panic!("{revision}'s snapshot lacks {}", entry.id()));
            let kind = match entry.kind {
                LegacyKind::Table => "table",
                LegacyKind::View => "view",
            };
            assert_eq!(
                object.canonical["kind"],
                kind,
                "{} in {revision}",
                entry.id()
            );
        }
        // every revision that uses it exists, and created it
        for used in entry.used_by {
            let revision: RevisionId = used.revision.parse().expect("a valid revision");
            assert!(Catalog::PRODUCTION.knows(&revision), "{revision}");
            assert!(entry.created_by.contains(&used.revision), "{revision}");
            assert!(matches!(
                used.need,
                LegacyNeed::Rebuildable | LegacyNeed::Required
            ));
        }
    }
}

/// The registry only grows: the committed list of ids is a prefix of the registry's ids
#[test]
fn legacy_registry_is_append_only() {
    // every guarded id is still registered, in its place
    let ids = LEGACY.iter().map(LegacyDef::id).collect::<Vec<_>>();
    assert!(ids.len() >= GUARD.len(), "an entry was removed: {ids:?}");
    assert_eq!(ids[..GUARD.len()], *GUARD);
}

/// The exemplar is the `events` table, class `Runtime`, created by `2026-10-v01` and
/// `2026-10-v02` and used by no revision, and it carries the class and legacy mark the class
/// table gives the same object; the re-declared `entities_by_name` view is no longer registered
#[test]
fn the_exemplars_carry_their_classes() {
    // the events table
    let events = legacy_def(SchemaBackend::Scylla, "events").expect("events is registered");
    assert_eq!(events.kind, LegacyKind::Table);
    assert_eq!(events.class, DataClass::Runtime);
    assert_eq!(events.created_by, ["2026-10-v01", "2026-10-v02"]);
    assert_eq!(events.used_by, []);
    assert_eq!(events.id(), "scylla:events");
    // nothing else is registered, and lookups agree with the list
    assert_eq!(LEGACY.len(), 1);
    assert!(is_legacy(SchemaBackend::Scylla, "events"));
    assert!(!is_legacy(SchemaBackend::Redis, "events"));
    assert!(!is_legacy(SchemaBackend::Scylla, "samples_list"));
    // the view yara's exact-name reads re-declared left the registry when it came back
    assert!(!is_legacy(SchemaBackend::Scylla, "entities_by_name"));
    // the class table holds each as a legacy entry of the same class, in the same store kind
    for entry in LEGACY {
        let store = match entry.kind {
            LegacyKind::Table => Store::ScyllaTable,
            LegacyKind::View => Store::ScyllaView,
        };
        let row = CLASSES
            .iter()
            .find(|row| row.store == store && row.name == entry.name)
            .unwrap_or_else(|| panic!("{} has no class table entry", entry.id()));
        assert!(row.legacy, "{}", entry.id());
        assert_eq!(row.class, entry.class, "{}", entry.id());
    }
    // and every legacy entry of the class table is registered
    for row in CLASSES.iter().filter(|row| row.legacy) {
        assert!(is_legacy(row.store.backend(), row.name), "{}", row.name);
    }
}
