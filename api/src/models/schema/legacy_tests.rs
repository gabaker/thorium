//! Tests for listing and dropping legacy objects against fixture registries, snapshots, and live
//! schemas, without a database

use std::collections::BTreeMap;

use super::{
    COUNT_TIMEOUT, DataClass, DowngradeOutcome, Droppable, LegacyRefusal, LegacySources, ViewError,
    classify, count_statement, declared_scylla, drop_statement, parse_id, plan_drops, record_id,
    resolve_legacy, support_window,
};
use crate::models::upgrades::{
    CATALOG, Catalog, LEGACY, LegacyDef, LegacyKind, LegacyNeed, LegacyUse, RevisionId,
    SchemaBackend, SupportFloor,
};

/// A legacy table the fixture registry holds that revision `2026-10-v02` reads, with rows that are
/// the only copy
const FOO: LegacyDef = LegacyDef {
    backend: SchemaBackend::Scylla,
    kind: LegacyKind::Table,
    name: "foo",
    class: DataClass::Core,
    created_by: &["2026-10-v01", "2026-10-v02"],
    used_by: &[LegacyUse {
        revision: "2026-10-v02",
        need: LegacyNeed::Required,
    }],
    retirement: "a fixture",
};

/// A legacy view the fixture registry holds that revision `2026-10-v01` reads and can rebuild
const BAR: LegacyDef = LegacyDef {
    backend: SchemaBackend::Scylla,
    kind: LegacyKind::View,
    name: "bar",
    class: DataClass::Derived,
    created_by: &["2026-10-v01", "2026-10-v02"],
    used_by: &[LegacyUse {
        revision: "2026-10-v01",
        need: LegacyNeed::Rebuildable,
    }],
    retirement: "a fixture",
};

/// The window of the production catalog, as the fixture sources use it
fn window() -> Vec<RevisionId> {
    // the production catalog's two released revisions
    ["2026-10-v02", "2026-10-v01"]
        .iter()
        .map(|id| id.parse().expect("a valid revision"))
        .collect()
}

/// Build sources over the two real exemplars plus the fixture objects
fn sources() -> LegacySources {
    // the production sources with the fixtures appended to its registry
    let mut sources = LegacySources::production();
    sources.registry.extend([FOO, BAR]);
    sources
}

/// The live keyspace of an older release: the two exemplars, the fixtures, and a table named in a
/// past snapshot that nothing declares or registers, plus one in no snapshot
fn live() -> BTreeMap<String, String> {
    // object name to kind
    [
        ("events", "table"),
        ("entities_by_name", "view"),
        ("foo", "table"),
        ("samples_list", "table"),
        ("tool_runs", "table"),
        ("hand_made", "table"),
    ]
    .iter()
    .map(|(name, kind)| ((*name).to_owned(), (*kind).to_owned()))
    .collect()
}

/// Give a fixture snapshot of `2026-10-v01` and `2026-10-v02` an object named `tool_runs` so it is
/// named in past snapshots
///
/// # Arguments
///
/// * `sources` - The fixture sources to add the object to
fn with_tool_runs(mut sources: LegacySources) -> LegacySources {
    // copy the events table's entry under another name into each released snapshot that lists it
    for snapshot in &mut sources.snapshots {
        if !snapshot
            .objects
            .iter()
            .any(|object| object.backend == SchemaBackend::Scylla && object.name == "events")
        {
            continue;
        }
        let mut object = snapshot
            .objects
            .iter()
            .find(|object| object.backend == SchemaBackend::Scylla && object.name == "events")
            .cloned()
            .expect("the snapshot lists events");
        object.name = "tool_runs".to_owned();
        snapshot.objects.push(object);
    }
    sources
}

/// The support window is the released revisions the catalog knows, newest first, none below the
/// last floor and no pre-release
#[test]
fn the_support_window_is_the_released_revisions_above_the_floor() {
    // the production catalog's window
    let window = support_window(&Catalog::PRODUCTION);
    assert_eq!(
        window.iter().map(RevisionId::as_str).collect::<Vec<_>>(),
        ["2026-10-v04", "2026-10-v03", "2026-10-v02", "2026-10-v01"]
    );
    // a catalog with a floor drops the revisions below it
    let floors: &'static [SupportFloor] = Box::leak(Box::new([SupportFloor {
        minimum: "2026-10-v02",
        intermediate: "1.8.0",
        since_version: "1.8.1",
    }]));
    let raised = Catalog {
        edges: CATALOG,
        floors,
    };
    assert_eq!(
        support_window(&raised)
            .iter()
            .map(RevisionId::as_str)
            .collect::<Vec<_>>(),
        ["2026-10-v04", "2026-10-v03", "2026-10-v02"]
    );
}

/// The three downgrade outcomes follow the object's use: unused is recreated, rebuildable
/// is rebuilt, and required is data loss with the row count
#[test]
fn downgrade_lines_follow_the_objects_use() {
    let window = window();
    // the exemplars use nothing
    for entry in LEGACY {
        let lines = super::downgrades(entry, 7, &window);
        assert_eq!(lines.len(), 2);
        assert!(
            lines
                .iter()
                .all(|line| line.outcome == DowngradeOutcome::RecreatedEmpty)
        );
        assert_eq!(
            lines[0].outcome.describe(entry.kind),
            "recreated (it does not use this object)"
        );
    }
    // a required use loses its rows on that revision only
    let lines = super::downgrades(&FOO, 42, &window);
    assert_eq!(lines[0].revision, "2026-10-v02");
    assert_eq!(lines[0].outcome, DowngradeOutcome::DataLoss { rows: 42 });
    assert_eq!(lines[0].outcome.describe(FOO.kind), "DATA LOSS (42 rows)");
    assert_eq!(lines[1].outcome, DowngradeOutcome::RecreatedEmpty);
    // a rebuildable view is rebuilt from its base table
    let lines = super::downgrades(&BAR, 9, &window);
    assert_eq!(lines[0].outcome, DowngradeOutcome::RecreatedEmpty);
    assert_eq!(lines[1].outcome, DowngradeOutcome::Rebuilt);
    assert_eq!(
        lines[1].outcome.describe(BAR.kind),
        "recreated and rebuilt from the base table"
    );
}

/// A required use by a revision in the window makes an object not droppable, naming the revision
/// and the rows; a use by a revision outside the window doesn't
#[test]
fn a_required_use_in_the_window_is_not_droppable() {
    // v02 is in the window
    let reason = match super::droppable(&FOO, 42, &window()) {
        Droppable::No { reason } => reason,
        Droppable::Yes => panic!("a required use must refuse"),
    };
    assert_eq!(reason, "2026-10-v02 uses it with required data (42 rows)");
    // with v02 out of the window nothing needs the rows
    let older = ["2026-10-v01".parse().expect("valid")];
    assert_eq!(super::droppable(&FOO, 42, &older), Droppable::Yes);
    // a rebuildable use never refuses
    assert_eq!(super::droppable(&BAR, 9, &window()), Droppable::Yes);
}

/// The listing has one status per registered object with its presence, rows, and downgrades,
/// lists the live object named in past snapshots under not-registered, and counts the one no
/// snapshot names as an unmanaged extra
#[test]
fn the_listing_sorts_live_objects() {
    let sources = with_tool_runs(sources());
    let rows = BTreeMap::from([("events".to_owned(), 18204), ("foo".to_owned(), 42)]);
    let listing = classify(&sources, &live(), &rows);
    // the registered objects, in registry order, present or not
    let ids = listing
        .objects
        .iter()
        .map(|status| status.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        [
            "scylla:events",
            "scylla:entities_by_name",
            "scylla:foo",
            "scylla:bar"
        ]
    );
    let events = &listing.objects[0];
    assert!(events.present);
    assert_eq!(events.rows, 18204);
    assert_eq!(events.class, DataClass::Runtime);
    assert_eq!(events.created_by, ["2026-10-v01", "2026-10-v02"]);
    assert_eq!(events.droppable, Droppable::Yes);
    // a registered object the keyspace lacks is not present, with no rows
    let bar = &listing.objects[3];
    assert!(!bar.present);
    assert_eq!(bar.rows, 0);
    // a required use refuses a drop
    assert!(matches!(listing.objects[2].droppable, Droppable::No { .. }));
    // not-registered: live, in past snapshots, not declared, not registered
    assert_eq!(listing.not_registered.len(), 1);
    assert_eq!(listing.not_registered[0].name, "tool_runs");
    assert_eq!(
        listing.not_registered[0].snapshots,
        ["2026-10-v01", "2026-10-v02"]
    );
    // an object in no snapshot is only counted
    assert_eq!(listing.unmanaged_extras, 1);
}

/// Parsing an id splits the backend from the name, and refuses anything else
#[test]
fn ids_name_a_backend_and_an_object() {
    assert_eq!(
        parse_id("scylla:events"),
        Some((SchemaBackend::Scylla, "events"))
    );
    assert_eq!(
        parse_id("redis:users"),
        Some((SchemaBackend::Redis, "users"))
    );
    for bad in ["events", "scylla:", ":events", "cassandra:events", ""] {
        assert_eq!(parse_id(bad), None, "{bad:?}");
    }
}

/// Resolving ids refuses a declared object, an unregistered id (listing the registered ids), and
/// an object a past snapshot names, each with its message; nothing resolves past a refusal, and a
/// repeated id resolves once
#[test]
fn resolving_ids_refuses_declared_and_unregistered_objects() {
    let sources = with_tool_runs(sources());
    let ids = |names: &[&str]| names.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
    // registered ids resolve, once each
    let found = resolve_legacy(
        &ids(&["scylla:events", "scylla:entities_by_name", "scylla:events"]),
        &sources,
    )
    .expect("registered");
    assert_eq!(
        found.iter().map(LegacyDef::id).collect::<Vec<_>>(),
        ["scylla:events", "scylla:entities_by_name"]
    );
    // a declared object
    let refusal = resolve_legacy(&ids(&["scylla:events", "scylla:samples_list"]), &sources)
        .expect_err("declared");
    assert_eq!(
        refusal.to_string(),
        "scylla:samples_list is a declared object; refusing."
    );
    // an unregistered id lists the registered ones
    let refusal = resolve_legacy(&ids(&["scylla:tools"]), &sources).expect_err("unregistered");
    assert_eq!(
        refusal.to_string(),
        "scylla:tools is not a registered legacy object. Registered: scylla:events, \
         scylla:entities_by_name, scylla:foo, scylla:bar."
    );
    // an object a past snapshot names says so, and what to do
    let refusal =
        resolve_legacy(&ids(&["scylla:tool_runs"]), &sources).expect_err("not registered");
    let text = refusal.to_string();
    assert!(
        text.starts_with("scylla:tool_runs is not a registered legacy object."),
        "{text}"
    );
    assert!(text.contains("2026-10-v01, 2026-10-v02"), "{text}");
    assert!(
        text.contains("register it as legacy or declare it again"),
        "{text}"
    );
    // an id that isn't an id is unregistered too
    assert!(matches!(
        resolve_legacy(&ids(&["events"]), &sources),
        Err(LegacyRefusal::Unregistered { .. })
    ));
    // the declared names are the registry's Scylla objects, and no exemplar is among them
    let declared = declared_scylla();
    assert!(declared.contains("samples_list"));
    assert!(!declared.contains("events") && !declared.contains("entities_by_name"));
}

/// Planning drops refuses the whole list when one object's rows are needed, and otherwise renders
/// each object's drop statement for its kind with its rows and downgrade lines
#[test]
fn planning_refuses_required_data_and_renders_the_statements() {
    let sources = sources();
    let rows = BTreeMap::from([("events".to_owned(), 5), ("foo".to_owned(), 42)]);
    let listing = classify(&sources, &live(), &rows);
    // the two exemplars plan, a table and a view each with its own statement
    let entries = LEGACY.to_vec();
    let plan = plan_drops("ks", &entries, &listing).expect("droppable");
    assert_eq!(plan.drops.len(), 2);
    assert_eq!(plan.drops[0].statement, "DROP TABLE IF EXISTS ks.events");
    assert_eq!(
        plan.drops[1].statement,
        "DROP MATERIALIZED VIEW IF EXISTS ks.entities_by_name"
    );
    assert_eq!(plan.drops[0].rows, 5);
    assert!(plan.drops[0].present);
    assert!(plan.drops[1].present);
    assert_eq!(plan.drops[0].downgrades.len(), 4);
    // one object that a window revision needs refuses the whole list, naming the revision and rows
    let entries = [LEGACY[0], FOO];
    let refusal = plan_drops("ks", &entries, &listing).expect_err("required");
    assert_eq!(
        refusal.to_string(),
        "refused: scylla:foo is used by 2026-10-v02 (required data, 42 rows); a downgrade would \
         lose it."
    );
    // a live object of the wrong kind can't be dropped as the registered kind
    let mut wrong = listing.clone();
    wrong.objects[0].live_kind = Some("view".to_owned());
    let refusal = plan_drops("ks", &[LEGACY[0]], &wrong).expect_err("wrong kind");
    assert!(
        matches!(refusal, LegacyRefusal::WrongKind { .. }),
        "{refusal}"
    );
    // statements match the object's kind
    assert_eq!(
        drop_statement("ks", &BAR),
        "DROP MATERIALIZED VIEW IF EXISTS ks.bar"
    );
    assert_eq!(drop_statement("ks", &FOO), "DROP TABLE IF EXISTS ks.foo");
}

/// An audit row's id is the drop's time with nanoseconds, then the object's id, so rows sort
/// oldest first
#[test]
fn audit_rows_sort_oldest_first() {
    use chrono::{TimeZone, Utc};
    let early = Utc.with_ymd_and_hms(2026, 10, 22, 12, 0, 0).unwrap();
    let late = early + chrono::Duration::nanoseconds(1);
    let (first, second) = (
        record_id(early, "scylla:events"),
        record_id(late, "scylla:events"),
    );
    assert_eq!(first, "2026-10-22T12:00:00.000000000Z/scylla:events");
    assert!(first < second);
}

/// The undeclared-view refusal says that `--recreate-view` never drops it and names the one
/// command that drops a registered legacy object
#[test]
fn the_undeclared_view_message_names_drop_legacy() {
    let message = ViewError::Undeclared {
        name: "events".to_owned(),
    }
    .to_string();
    assert!(message.contains("drop-legacy"), "{message}");
    assert!(message.contains("--recreate-view"), "{message}");
    assert!(message.contains("declared views:"), "{message}");
}

/// The row count statement asks the server for a long timeout and carries a request timeout a
/// little longer, so the driver's 30 second default doesn't cut a count of a large object short
#[test]
fn the_count_statement_carries_its_own_request_timeout() {
    // a count of one object, in its keyspace
    let statement = count_statement("ks", "events");
    assert_eq!(
        statement.contents,
        "SELECT COUNT(*) FROM ks.events BYPASS CACHE USING TIMEOUT 10m"
    );
    // the request timeout outlasts the server-side one
    assert_eq!(statement.get_request_timeout(), Some(COUNT_TIMEOUT));
    assert!(COUNT_TIMEOUT > std::time::Duration::from_mins(10));
}
