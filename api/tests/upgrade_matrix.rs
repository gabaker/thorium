//! The pattern matrix: every kind of schema change driven up and down through the upgrade
//! harness's direct driver, against real Redis and Scylla, on the test-only fixture catalog
//!
//! Each of the matrix's revisions holds one kind of change: additions (`M1`), restructuring (`M2`),
//! joined multi-step changes (`M3`), removal and lossy restructuring (`M4`), and an irreversible
//! step with a sibling edge (`M5`, `M5B`).

mod fixture_catalog;

use fixture_catalog::{
    ACL, BASE, FILES, ITEMS, M1, M2, M3, M4, M5, M5B, OWNERS, PEOPLE, RANKS, SENTINEL, TAGS, TASKS,
    TOKENS, all_families, in_layout, scenario, seed,
};
use std::collections::BTreeSet;
use thorium::models::upgrades::{Catalog, Family, Record, StepState};
use thorium::test_utilities::upgrade_harness::{
    BackupRef, Check, FixtureVerifier, HistoryExpectation, Injection, Layout, PassCountCheck,
    RecordsCheck, Scenario,
};

/// The backup reference every approval here names
fn snap() -> BackupRef {
    // a reference to a backup
    BackupRef::new("snap-1")
}

/// A scenario that seeds every family with the same number of objects in layout 1
///
/// # Arguments
///
/// * `name` - The scenario's name
/// * `count` - How many objects each family holds
fn seeded(name: &str, count: usize) -> Scenario {
    // seeding wipes each family, so no earlier scenario's objects remain
    all_families()
        .into_iter()
        .fold(scenario(name), |scenario, family| {
            scenario.seed(seed(family, 1, count))
        })
}

/// A scenario that seeds some families with objects in layout 1 and leaves the rest empty
///
/// # Arguments
///
/// * `name` - The scenario's name
/// * `with` - The families that hold objects
/// * `count` - How many objects each of them holds
fn seeded_only(name: &str, with: &[Family], count: usize) -> Scenario {
    // seeding wipes each family, so no earlier scenario's objects remain
    all_families()
        .into_iter()
        .fold(scenario(name), |scenario, family| {
            let count = if with.contains(&family) { count } else { 0 };
            scenario.seed(seed(family, 1, count))
        })
}

/// A history entry of a step that needs an approval and loses nothing
///
/// # Arguments
///
/// * `step` - The step id
fn gated(step: &str) -> HistoryExpectation {
    // approved with the one backup
    HistoryExpectation::new(step, StepState::Done).with_backup("snap-1")
}

/// A history entry of a step that needs no approval
///
/// # Arguments
///
/// * `step` - The step id
fn ungated(step: &str) -> HistoryExpectation {
    // no backup is recorded
    HistoryExpectation::new(step, StepState::Done)
}

/// A history entry of a lossy step, with its backup, code, and loss report
///
/// # Arguments
///
/// * `step` - The step id
fn lossy(step: &str) -> HistoryExpectation {
    // approved with the backup, the code, and the loss
    HistoryExpectation::new(step, StepState::Done)
        .with_backup("snap-1")
        .lossy()
}

/// A history entry of a step a later step made redundant
///
/// # Arguments
///
/// * `step` - The step id
fn skipped(step: &str) -> HistoryExpectation {
    // recorded when the superseding step finishes
    HistoryExpectation::new(step, StepState::Skipped)
}

/// Expect every object of a family to hold a field, and judge its value
///
/// # Arguments
///
/// * `family` - The family
/// * `name` - What is checked
/// * `check` - Judges the objects
fn records(
    family: Family,
    name: &'static str,
    check: fn(&[Record]) -> Result<(), String>,
) -> Check {
    // a check that reads every object of the family
    Check::Records(RecordsCheck {
        family,
        name,
        check,
    })
}

/// Expect the passes over a family to have read and written each object this many times
///
/// # Arguments
///
/// * `family` - The family
/// * `reads` - Reads per object
/// * `writes` - Writes per object
fn passes(family: Family, reads: u64, writes: u64) -> Check {
    // totals over the objects seeded
    Check::Passes(PassCountCheck {
        family,
        reads_per_object: reads,
        writes_per_object: writes,
    })
}

/// Get a field of a record, or an empty string when the record has none
///
/// # Arguments
///
/// * `record` - The record
/// * `field` - The field
fn field(record: &Record, field: &str) -> String {
    // only Redis hashes hold fields
    match record {
        Record::Redis { fields, .. } => fields.get(field).cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

/// Whether a record holds a field
///
/// # Arguments
///
/// * `record` - The record
/// * `field` - The field
fn has(record: &Record, field: &str) -> bool {
    // only Redis hashes hold fields
    matches!(record, Record::Redis { fields, .. } if fields.contains_key(field))
}

/// The number at the end of a record's key
///
/// # Arguments
///
/// * `record` - The record
fn number(record: &Record) -> usize {
    // keys read `family:number`
    match record {
        Record::Redis { key, .. } => key
            .rsplit(':')
            .next()
            .and_then(|number| number.parse().ok())
            .unwrap_or(usize::MAX),
        _ => usize::MAX,
    }
}

/// Judge every record with a rule, naming the first that breaks it
///
/// # Arguments
///
/// * `records` - The records
/// * `what` - What the rule says
/// * `rule` - Whether a record keeps the rule
fn all(records: &[Record], what: &str, rule: impl Fn(&Record) -> bool) -> Result<(), String> {
    // report the first record that breaks the rule
    match records.iter().find(|record| !rule(record)) {
        Some(record) => Err(format!("{record:?} breaks the rule that {what}")),
        None => Ok(()),
    }
}

/// Every item's `display_name` equals its `label`
///
/// # Arguments
///
/// * `records` - The family's records to check
fn names_follow_labels(records: &[Record]) -> Result<(), String> {
    // the new form is derived from the old
    all(records, "display_name equals label", |record| {
        has(record, "label") && field(record, "display_name") == field(record, "label")
    })
}

/// Every item still holds both forms, whatever its number
///
/// # Arguments
///
/// * `records` - The family's records to check
fn items_hold_both_forms(records: &[Record]) -> Result<(), String> {
    // the dual write left both forms in place
    all(records, "both forms are held", |record| {
        has(record, "label") && has(record, "display_name")
    })?;
    // nine items: six from before the upgrade and three written after it
    if records.len() == 9 {
        Ok(())
    } else {
        Err(format!("expected 9 items, found {}", records.len()))
    }
}

/// The first two items' labels were edited at the previous revision, and every `display_name`
/// follows its label
///
/// # Arguments
///
/// * `records` - The family's records to check
fn edited_labels_were_followed(records: &[Record]) -> Result<(), String> {
    // the edited labels are the ones the display names came to hold
    names_follow_labels(records)?;
    all(records, "edited items show their edit", |record| {
        number(record) > 1 || field(record, "display_name") == format!("Edited-{}", number(record))
    })
}

/// Rewrite an item the way the previous release would edit its label
///
/// # Arguments
///
/// * `record` - The item
fn edit_label(mut record: Record) -> Record {
    // only the old field changes, which is the one that release reads
    let edited = format!("Edited-{}", number(&record));
    if let Record::Redis { fields, .. } = &mut record {
        fields.insert("label".to_owned(), edited);
    }
    record
}

/// The keys of the first two objects of a family
///
/// # Arguments
///
/// * `family` - The family's name
fn first_two(family: &str) -> Vec<String> {
    // keys are `family:number`
    vec![format!("{family}:0"), format!("{family}:1")]
}

/// Additions converge first in every plan, run no data pass up or down, and the revert edge only
/// records that they stay
#[tokio::test]
async fn t_tc_21_additions_converge_first_and_run_no_pass() {
    // the additions alone: no approval, no pass, and the data is untouched; the history lists
    // only converge steps, so no data pass runs (the pass counters are asserted where passes run)
    let additions = [
        "matrix-table",
        "matrix-view",
        "matrix-column",
        "matrix-prefix",
        "matrix-mapping",
    ];
    let added: Vec<HistoryExpectation> = additions.iter().map(|step| ungated(step)).collect();
    let mut left = added.clone();
    left.push(ungated("matrix-additions-left"));
    seeded("additions", 6)
        .target(M1)
        .expect_revision(M1)
        .check(Check::History(added.clone()))
        .check(in_layout(ITEMS, 1))
        .check(in_layout(PEOPLE, 1))
        .target(BASE)
        .expect_revision(BASE)
        .check(Check::History(left))
        .check(in_layout(ITEMS, 1))
        .run()
        .await
        .expect("the scenario holds");
    // a plan across two edges converges the additions of both before any gated step
    let mut converged = added.clone();
    converged.push(ungated("items-by-owner-view"));
    let mut second = converged.clone();
    second.extend([
        gated("items-rename"),
        gated("people-split"),
        gated("tasks-type"),
        gated("owners-key"),
        gated("files-layout"),
        gated("results-rebuild-1"),
    ]);
    let mut down = second.clone();
    down.extend([
        gated("files-layout-reverse"),
        gated("owners-key-reverse"),
        gated("tasks-type-reverse"),
        gated("people-split-reverse"),
        ungated("items-rename-reverse"),
        gated("results-rebuild-1-reverse"),
        ungated("matrix-additions-left"),
    ]);
    seeded("converge-first", 6)
        .target(M2)
        .expect_blocked("items-rename")
        .check(Check::History(converged))
        .check(in_layout(ITEMS, 1))
        .approve_all(snap())
        .expect_revision(M2)
        .check(Check::History(second))
        .target(BASE)
        .approve_all(snap())
        .expect_revision(BASE)
        .check(Check::History(down))
        .run()
        .await
        .expect("the scenario holds");
}

/// The rename writes both forms during `M2`, and its reverse runs no pass; objects written after
/// the upgrade read correctly at base; a label edited at base is followed on the re-upgrade
#[tokio::test]
async fn t_tc_22_a_rename_writes_both_forms_and_reverses_without_a_pass() {
    // upgrade, then write objects as the dual-writing release does, then downgrade
    seeded_only("rename", &[ITEMS], 6)
        .target(M2)
        .approve_all(snap())
        .expect_revision(M2)
        .check(in_layout(ITEMS, 2))
        .check(passes(ITEMS, 1, 1))
        .check(records(
            ITEMS,
            "display names follow labels",
            names_follow_labels,
        ))
        .seed(seed(ITEMS, 2, 3))
        .capture("dual")
        .target(BASE)
        .approve_all(snap())
        .expect_revision(BASE)
        .check(in_layout(ITEMS, 2))
        .check(records(ITEMS, "both forms are kept", items_hold_both_forms))
        .check(Check::SameDataAs("dual".to_owned()))
        .inject(Injection::Modify {
            family: ITEMS,
            keys: Some(first_two("matrix_items")),
            with: edit_label,
        })
        .target(M2)
        .approve_all(snap())
        .expect_revision(M2)
        .check(records(
            ITEMS,
            "edited labels are followed",
            edited_labels_were_followed,
        ))
        .run()
        .await
        .expect("the scenario holds");
}

/// Every person has first and last names that join to the full name, the first name being the
/// whole name for a person with one
///
/// # Arguments
///
/// * `records` - The family's records to check
fn people_are_split(records: &[Record]) -> Result<(), String> {
    // the halves are what the full name splits to
    all(records, "the names are split", |record| {
        let first = field(record, "first");
        let last = field(record, "last");
        let full = fixture_catalog::records::full_name(number(record));
        !has(record, "full_name")
            && if last.is_empty() {
                first == full
            } else {
                format!("{first} {last}") == full
            }
    })
}

/// Every person has a full name again, and none has the halves
///
/// # Arguments
///
/// * `records` - The family's records to check
fn people_are_joined(records: &[Record]) -> Result<(), String> {
    // the full name is what the halves join to
    all(records, "the names are joined", |record| {
        field(record, "full_name") == fixture_catalog::records::full_name(number(record))
            && !has(record, "first")
            && !has(record, "last")
    })
}

/// Every task has a text priority that writes out its number
///
/// # Arguments
///
/// * `records` - The family's records to check
fn tasks_are_typed(records: &[Record]) -> Result<(), String> {
    // the text is the number written out
    all(records, "priority_text is the priority", |record| {
        !has(record, "priority") || field(record, "priority_text") == field(record, "priority")
    })
}

/// Every task has its number back, and none has the text
///
/// # Arguments
///
/// * `records` - The family's records to check
fn tasks_are_numbers(records: &[Record]) -> Result<(), String> {
    // the number parses
    all(records, "priority is a number", |record| {
        field(record, "priority").parse::<i64>().is_ok() && !has(record, "priority_text")
    })
}

/// Every owner has the bucket its name files under
///
/// # Arguments
///
/// * `records` - The family's records to check
fn owners_are_bucketed(records: &[Record]) -> Result<(), String> {
    // the bucket is the owner's first letter, lowercased
    all(records, "the bucket is the first letter", |record| {
        field(record, "bucket")
            == field(record, "owner")
                .chars()
                .next()
                .map_or_else(String::new, |first| first.to_lowercase().to_string())
    })
}

/// Every owner has no bucket
///
/// # Arguments
///
/// * `records` - The family's records to check
fn owners_are_unbucketed(records: &[Record]) -> Result<(), String> {
    // the bucket went with the downgrade
    all(records, "no bucket remains", |record| {
        !has(record, "bucket")
    })
}

/// The edits made at base to the first person, task, and owner were followed
///
/// # Arguments
///
/// * `records` - The family's records to check
fn people_edit_followed(records: &[Record]) -> Result<(), String> {
    // person 0 was renamed to `Zed Quinn`
    all(records, "the edited person was followed", |record| {
        number(record) != 0 || (field(record, "first") == "Zed" && field(record, "last") == "Quinn")
    })
}

/// The task edited at base has its text recomputed
///
/// # Arguments
///
/// * `records` - The family's records to check
fn tasks_edit_followed(records: &[Record]) -> Result<(), String> {
    // task 1 now has priority 42
    all(records, "the edited task was followed", |record| {
        number(record) != 1 || field(record, "priority_text") == "42"
    })
}

/// The owner edited at base is filed under its new bucket
///
/// # Arguments
///
/// * `records` - The family's records to check
fn owners_edit_followed(records: &[Record]) -> Result<(), String> {
    // owner 0 is now `Zoe`
    all(records, "the edited owner was followed", |record| {
        number(record) != 0 || field(record, "bucket") == "z"
    })
}

/// Rename the first person the way the previous release would
///
/// # Arguments
///
/// * `record` - The person
fn rename_person(mut record: Record) -> Record {
    // the previous release reads the full name only
    if let Record::Redis { fields, .. } = &mut record {
        fields.insert("full_name".to_owned(), "Zed Quinn".to_owned());
    }
    record
}

/// Raise the priority of a task the way the previous release would
///
/// # Arguments
///
/// * `record` - The task
fn raise_priority(mut record: Record) -> Record {
    // the previous release reads the number only
    if let Record::Redis { fields, .. } = &mut record {
        fields.insert("priority".to_owned(), "42".to_owned());
    }
    record
}

/// Change the owner of an owners record the way the previous release would
///
/// # Arguments
///
/// * `record` - The owners record
fn change_owner(mut record: Record) -> Record {
    // the previous release knows no bucket
    if let Record::Redis { fields, .. } = &mut record {
        fields.insert("owner".to_owned(), "Zoe".to_owned());
    }
    record
}

/// Every file sits under the new key layout
///
/// # Arguments
///
/// * `records` - The family's records to check
fn files_are_moved(records: &[Record]) -> Result<(), String> {
    // the key's prefix is the new layout's
    all(
        records,
        "the key is in the new layout",
        |record| matches!(record, Record::S3 { key } if key.starts_with("matrix/a-v2/obj-")),
    )
}

/// Every file sits under the old key layout again
///
/// # Arguments
///
/// * `records` - The family's records to check
fn files_are_back(records: &[Record]) -> Result<(), String> {
    // the key's prefix is the old layout's
    all(
        records,
        "the key is in the old layout",
        |record| matches!(record, Record::S3 { key } if key.starts_with("matrix/obj-")),
    )
}

/// The split, the type change, the key change, and the move to a new S3 key layout each round
/// trip up, down, and up again, including objects created after the upgrade, and objects edited
/// at base are followed
#[tokio::test]
async fn t_tc_23_backfills_round_trip_including_new_and_edited_objects() {
    // upgrade the three families, write objects as the new release does, and downgrade
    seeded_only("round-trips", &[PEOPLE, TASKS, OWNERS, FILES], 6)
        .target(M2)
        .approve_all(snap())
        .expect_revision(M2)
        .check(in_layout(PEOPLE, 2))
        .check(in_layout(TASKS, 2))
        .check(in_layout(OWNERS, 2))
        .check(in_layout(FILES, 2))
        .check(records(FILES, "files are moved", files_are_moved))
        .check(records(PEOPLE, "names are split", people_are_split))
        .check(records(TASKS, "priorities are typed", tasks_are_typed))
        .check(records(OWNERS, "owners are bucketed", owners_are_bucketed))
        .seed(seed(PEOPLE, 2, 2))
        .seed(seed(TASKS, 2, 2))
        .seed(seed(OWNERS, 2, 2))
        .seed(seed(FILES, 2, 2))
        .target(BASE)
        .approve_all(snap())
        .expect_revision(BASE)
        .check(in_layout(PEOPLE, 1))
        .check(in_layout(TASKS, 1))
        .check(in_layout(OWNERS, 1))
        .check(in_layout(FILES, 1))
        .check(records(FILES, "files are back", files_are_back))
        .check(records(PEOPLE, "names are joined", people_are_joined))
        .check(records(TASKS, "priorities are numbers", tasks_are_numbers))
        .check(records(
            OWNERS,
            "owners are unbucketed",
            owners_are_unbucketed,
        ))
        .inject(Injection::Modify {
            family: PEOPLE,
            keys: Some(vec!["matrix_people:0".to_owned()]),
            with: rename_person,
        })
        .inject(Injection::Modify {
            family: TASKS,
            keys: Some(vec!["matrix_tasks:1".to_owned()]),
            with: raise_priority,
        })
        .inject(Injection::Modify {
            family: OWNERS,
            keys: Some(vec!["matrix_owners:0".to_owned()]),
            with: change_owner,
        })
        .target(M2)
        .approve_all(snap())
        .expect_revision(M2)
        .check(in_layout(PEOPLE, 2))
        .check(in_layout(FILES, 2))
        .check(records(
            PEOPLE,
            "the edit is followed",
            people_edit_followed,
        ))
        .check(records(TASKS, "the edit is followed", tasks_edit_followed))
        .check(records(
            OWNERS,
            "the edit is followed",
            owners_edit_followed,
        ))
        .run()
        .await
        .expect("the scenario holds");
}

/// The families that the passes up to `M3` rewrite, apart from the ranks pass that waits for them
const MOVED_BY_M3: [Family; 6] = [ITEMS, PEOPLE, TASKS, OWNERS, FILES, TAGS];

/// Expect each family's passes to have read and written every object this often
///
/// # Arguments
///
/// * `scenario` - The scenario
/// * `families` - The families
/// * `times` - How often each object was read and written
fn each_object(scenario: Scenario, families: &[Family], times: u64) -> Scenario {
    // one check per family
    families.iter().fold(scenario, |scenario, family| {
        scenario.check(passes(*family, times, times))
    })
}

/// base to `M3` in one plan runs one pass for each family with each object read and written
/// once, the dependent transform after the family it reads, and one rebuild, and leaves what the
/// one-at-a-time run leaves
#[tokio::test]
async fn t_tc_24_a_fused_upgrade_reads_and_writes_each_object_once() {
    // upgrade in one plan, and capture the result
    let fused = seeded("fused-up", 8)
        .target(M3)
        .approve_all(snap())
        .expect_revision(M3);
    // the passes touched each object once, and the families M3 doesn't touch not at all
    let fused = each_object(fused, &MOVED_BY_M3, 1);
    let fused = each_object(fused, &[RANKS], 1);
    let fused = each_object(fused, &[ACL, TOKENS], 0);
    // the history shows the tags pass before the ranks pass that reads it, and one rebuild
    fused
        .check(Check::History(vec![
            ungated("matrix-table"),
            ungated("matrix-view"),
            ungated("matrix-column"),
            ungated("matrix-prefix"),
            ungated("matrix-mapping"),
            ungated("items-by-owner-view"),
            gated("items-rename"),
            gated("items-key"),
            gated("people-split"),
            gated("tasks-type"),
            gated("owners-key"),
            gated("files-layout"),
            gated("tags-normalize"),
            gated("ranks-link"),
            gated("results-rebuild-2"),
            skipped("results-rebuild-1"),
        ]))
        .check(in_layout(ITEMS, 3))
        .check(in_layout(TAGS, 2))
        .check(in_layout(RANKS, 2))
        .capture("fused")
        .target(BASE)
        .approve_all(snap())
        .expect_revision(BASE)
        .target(M1)
        .approve_all(snap())
        .expect_revision(M1)
        .target(M2)
        .approve_all(snap())
        .expect_revision(M2)
        .target(M3)
        .approve_all(snap())
        .expect_revision(M3)
        .check(Check::SameDataAs("fused".to_owned()))
        .run()
        .await
        .expect("the scenario holds");
}

/// `M3` to base in one plan is the reverse equivalent of the fused upgrade, and leaves what the
/// one-at-a-time downgrade leaves
#[tokio::test]
async fn t_tc_25_a_fused_downgrade_reads_and_writes_each_object_once() {
    // go up in one plan, then down in one plan
    let up = seeded("fused-down", 8)
        .target(M3)
        .approve_all(snap())
        .expect_revision(M3)
        .capture("up")
        .target(BASE)
        .approve_all(snap())
        .expect_revision(BASE);
    // each pass ran once each way: the rename's reverse is no pass, and every other family has
    // one pass back
    let up = each_object(up, &[ITEMS, PEOPLE, TASKS, OWNERS, FILES, TAGS, RANKS], 2);
    up.check(in_layout(ITEMS, 2))
        .check(in_layout(PEOPLE, 1))
        .check(in_layout(TAGS, 1))
        .check(in_layout(RANKS, 1))
        .capture("down")
        .target(M3)
        .approve_all(snap())
        .expect_revision(M3)
        .check(Check::SameDataAs("up".to_owned()))
        .target(M2)
        .approve_all(snap())
        .expect_revision(M2)
        .target(M1)
        .approve_all(snap())
        .expect_revision(M1)
        .target(BASE)
        .approve_all(snap())
        .expect_revision(BASE)
        .check(Check::SameDataAs("down".to_owned()))
        .run()
        .await
        .expect("the scenario holds");
}

/// Every item has lost its `label` to the sentinel fill
///
/// # Arguments
///
/// * `records` - The family's records to check
fn labels_are_sentinels(records: &[Record]) -> Result<(), String> {
    // the dropped field comes back as the sentinel
    all(records, "label is the sentinel", |record| {
        field(record, "label") == SENTINEL
    })
}

/// A person with one name has the sentinel as last name, and anyone else has the names split
///
/// # Arguments
///
/// * `records` - The family's records to check
fn people_have_sentinels(records: &[Record]) -> Result<(), String> {
    // the fill covers exactly the names with no space
    all(records, "single names get the sentinel", |record| {
        let full = fixture_catalog::records::full_name(number(record));
        match full.split_once(' ') {
            Some((first, last)) => field(record, "first") == first && field(record, "last") == last,
            None => field(record, "first") == full && field(record, "last") == SENTINEL,
        }
    })
}

/// No tag holds the status the old layout doesn't define, and the archived tag is inactive
///
/// # Arguments
///
/// * `records` - The family's records to check
fn tags_are_mapped(records: &[Record]) -> Result<(), String> {
    // the new variant maps to its declared old variant
    all(records, "no tag is archived", |record| {
        field(record, "status") != "archived" && !has(record, "retention")
    })?;
    // the tag written as archived after the upgrade is now inactive
    all(records, "the archived tag is inactive", |record| {
        number(record) != 11 || field(record, "status") == "inactive"
    })
}

/// Every access record is closed, whatever it was
///
/// # Arguments
///
/// * `records` - The family's records to check
fn access_is_closed(records: &[Record]) -> Result<(), String> {
    // the fill fails closed, never open
    all(records, "the record is closed", |record| {
        field(record, "public") == "false" && !has(record, "allowed")
    })
}

/// Expect the plan to block on a lossy step, then accept its loss with the code it shows
///
/// # Arguments
///
/// * `scenario` - The scenario
/// * `step` - The lossy step
fn accept_loss(scenario: Scenario, step: &str) -> Scenario {
    // one block and one approval, as an admin gives them
    scenario
        .expect_blocked_lossy(step)
        .approve_loss(step, snap())
}

/// The lossy reverses of `M4`, in the order the plan blocks on them
const M4_LOSSY: [&str; 4] = [
    "acl-allowed-reverse",
    "tags-status-reverse",
    "people-merge-reverse",
    "items-contract-reverse",
];

/// `M4`'s contract and merge reverse only with accepted loss; dropped objects come back with
/// sentinels, the security field is filled closed, and the new enum variant maps to the old one
#[tokio::test]
async fn t_tc_26_lossy_reverses_run_only_with_accepted_loss() {
    // go up to M4 and write objects as the new release does
    let up = seeded("lossy", 8)
        .target(M4)
        .approve_all(snap())
        .expect_revision(M4)
        .check(in_layout(ITEMS, 4))
        .check(in_layout(PEOPLE, 3))
        .check(in_layout(TAGS, 3))
        .check(in_layout(ACL, 2))
        .seed(seed(ITEMS, 4, 2))
        .seed(seed(TAGS, 3, 4))
        .target(M3)
        .approve_all(snap());
    // every lossy step blocks for its own approval, one at a time
    let down = M4_LOSSY
        .iter()
        .fold(up, |scenario, step| accept_loss(scenario, step));
    down.expect_revision(M3)
        .check(Check::Fixtures(
            FixtureVerifier::items_in(ITEMS, Layout(3)).with_sentinel("label", SENTINEL),
        ))
        .check(records(ITEMS, "labels are sentinels", labels_are_sentinels))
        .check(in_layout(PEOPLE, 2))
        .check(records(
            PEOPLE,
            "single names get the sentinel",
            people_have_sentinels,
        ))
        .check(in_layout(TAGS, 2))
        .check(records(TAGS, "the new status is mapped", tags_are_mapped))
        .check(in_layout(ACL, 1))
        .check(records(ACL, "records are closed", access_is_closed))
        .check(Check::History(vec![
            ungated("matrix-table"),
            ungated("matrix-view"),
            ungated("matrix-column"),
            ungated("matrix-prefix"),
            ungated("matrix-mapping"),
            ungated("items-by-owner-view"),
            gated("items-rename"),
            gated("items-key"),
            gated("people-split"),
            gated("tasks-type"),
            gated("owners-key"),
            gated("files-layout"),
            gated("tags-normalize"),
            gated("ranks-link"),
            gated("results-rebuild-2"),
            skipped("results-rebuild-1"),
            gated("people-merge"),
            gated("tags-status"),
            gated("acl-allowed"),
            gated("items-contract"),
            gated("items-by-label-drop"),
            ungated("items-by-label-restore"),
            lossy("acl-allowed-reverse"),
            lossy("tags-status-reverse"),
            lossy("people-merge-reverse"),
            lossy("items-contract-reverse"),
        ]))
        .run()
        .await
        .expect("the scenario holds");
}

/// `M5` refuses any downgrade across it before anything runs, and `M5B` downgrades to base
/// through lossy steps only
#[tokio::test]
async fn t_tc_27_an_irreversible_step_refuses_and_the_sibling_downgrades() {
    // reach M5, where the tokens step can't be undone
    let history_at_m5 = vec![
        ungated("matrix-table"),
        ungated("matrix-view"),
        ungated("matrix-column"),
        ungated("matrix-prefix"),
        ungated("matrix-mapping"),
        ungated("items-by-owner-view"),
        gated("items-rename"),
        gated("items-key"),
        gated("people-split"),
        gated("tasks-type"),
        gated("owners-key"),
        gated("files-layout"),
        gated("tags-normalize"),
        gated("ranks-link"),
        gated("results-rebuild-2"),
        skipped("results-rebuild-1"),
        gated("people-merge"),
        gated("tags-status"),
        gated("acl-allowed"),
        gated("tokens-rekey"),
        gated("items-contract"),
        gated("items-by-label-drop"),
    ];
    seeded("irreversible", 6)
        .target(M5)
        .approve_all(snap())
        .expect_revision(M5)
        .check(in_layout(TOKENS, 2))
        .target(M4)
        .expect_refused("tokens-rekey")
        .target(M1)
        .expect_refused("tokens-rekey")
        .target(BASE)
        .expect_refused("tokens-rekey")
        .check(Check::History(history_at_m5))
        .check(in_layout(TOKENS, 2))
        .check(in_layout(ITEMS, 4))
        .run()
        .await
        .expect("the scenario holds");
    // the sibling has no irreversible step, so base is reachable once each lossy loss is accepted
    let up = seeded("sibling", 6)
        .target(M5B)
        .approve_all(snap())
        .expect_revision(M5B)
        .check(in_layout(TOKENS, 1))
        .target(BASE)
        .approve_all(snap());
    // only the lossy steps block: everything else was approved ahead
    let down = M4_LOSSY
        .iter()
        .fold(up, |scenario, step| accept_loss(scenario, step));
    down.expect_revision(BASE)
        .check(in_layout(ITEMS, 2))
        .check(in_layout(PEOPLE, 1))
        .check(in_layout(TAGS, 1))
        .check(in_layout(ACL, 1))
        .check(in_layout(TOKENS, 1))
        .run()
        .await
        .expect("the scenario holds");
}

/// No person's name carries the sentinel, and a person with one name has just that name
///
/// # Arguments
///
/// * `records` - The family's records to check
fn names_never_hold_the_sentinel(records: &[Record]) -> Result<(), String> {
    // the re-upgrade reads the sentinel as "unknown", never as part of a name
    all(records, "the name is the full name", |record| {
        field(record, "name") == fixture_catalog::records::full_name(number(record))
            && !field(record, "name").contains(SENTINEL)
    })
}

/// T2 to T4, back to T2, and to T4 again: the re-upgrade over sentinels yields unknown, never a
/// fabricated value
#[tokio::test]
async fn t_tc_28_a_re_upgrade_over_sentinels_never_fabricates_a_value() {
    // reach T2, then T4, then come back with the loss accepted
    let up = seeded_only("sentinels", &[PEOPLE, ITEMS], 6)
        .target(M2)
        .approve_all(snap())
        .expect_revision(M2)
        .target(M4)
        .approve_all(snap())
        .expect_revision(M4)
        .check(records(
            PEOPLE,
            "names are joined",
            names_never_hold_the_sentinel,
        ))
        .target(M2)
        .approve_all(snap());
    let down = M4_LOSSY
        .iter()
        .fold(up, |scenario, step| accept_loss(scenario, step));
    // the sentinels are in place at T2, and the second upgrade reads them as unknown
    down.expect_revision(M2)
        .check(records(
            PEOPLE,
            "single names get the sentinel",
            people_have_sentinels_at_m2,
        ))
        .check(records(ITEMS, "labels are sentinels", labels_are_sentinels))
        .target(M4)
        .approve_all(snap())
        .expect_revision(M4)
        .check(records(
            PEOPLE,
            "names never hold the sentinel",
            names_never_hold_the_sentinel,
        ))
        .check(records(ITEMS, "labels stay dropped", |records| {
            all(records, "no label remains", |record| !has(record, "label"))
        }))
        .run()
        .await
        .expect("the scenario holds");
}

/// A person with one name holds the sentinel as last name at the restructuring revision
///
/// # Arguments
///
/// * `records` - The family's records to check
fn people_have_sentinels_at_m2(records: &[Record]) -> Result<(), String> {
    // the same rule as after a downgrade to M3, with names split
    people_have_sentinels(records)
}

/// The layouts every family is in at a revision, going up one edge at a time from base: items,
/// people, tasks, owners, files, tags, ranks, access, tokens
type Layouts = [u32; 9];

/// Expect every family to be in the layouts of a state
///
/// # Arguments
///
/// * `scenario` - The scenario
/// * `layouts` - The layouts, in the order of [`all_families`]
fn state(scenario: Scenario, layouts: Layouts) -> Scenario {
    // one check per family
    all_families()
        .into_iter()
        .zip(layouts)
        .fold(scenario, |scenario, (family, layout)| {
            scenario.check(in_layout(family, layout))
        })
}

/// Every edge of the matrix, run up and down one at a time with each gate approved, leaves the
/// data each state expects
#[tokio::test]
async fn t_tc_29_every_edge_one_at_a_time_leaves_the_expected_data() {
    // up, one revision at a time, from freshly seeded objects in layout 1
    let scenario = state(seeded("edges", 8), [1; 9]);
    let scenario = state(
        scenario.target(M1).approve_all(snap()).expect_revision(M1),
        [1; 9],
    );
    let scenario = state(
        scenario.target(M2).approve_all(snap()).expect_revision(M2),
        [2, 2, 2, 2, 2, 1, 1, 1, 1],
    );
    let scenario = state(
        scenario.target(M3).approve_all(snap()).expect_revision(M3),
        [3, 2, 2, 2, 2, 2, 2, 1, 1],
    );
    let scenario = state(
        scenario.target(M4).approve_all(snap()).expect_revision(M4),
        [4, 3, 2, 2, 2, 3, 2, 2, 1],
    );
    // the sibling edge and back: additions only, so the data doesn't move
    let scenario = state(
        scenario
            .target(M5B)
            .approve_all(snap())
            .expect_revision(M5B),
        [4, 3, 2, 2, 2, 3, 2, 2, 1],
    );
    let scenario = state(
        scenario.target(M4).expect_revision(M4),
        [4, 3, 2, 2, 2, 3, 2, 2, 1],
    );
    // down, one revision at a time, accepting each lossy step's loss as it blocks
    let scenario = M4_LOSSY
        .iter()
        .fold(scenario.target(M3).approve_all(snap()), |scenario, step| {
            accept_loss(scenario, step)
        })
        .expect_revision(M3);
    let scenario = state(scenario, [3, 2, 2, 2, 2, 2, 2, 1, 1]);
    let scenario = state(
        scenario.target(M2).approve_all(snap()).expect_revision(M2),
        [2, 2, 2, 2, 2, 1, 1, 1, 1],
    );
    let scenario = state(
        scenario.target(M1).approve_all(snap()).expect_revision(M1),
        [2, 1, 1, 1, 1, 1, 1, 1, 1],
    );
    let scenario = state(
        scenario.target(BASE).expect_revision(BASE),
        [2, 1, 1, 1, 1, 1, 1, 1, 1],
    );
    // and up again to the last edge of the primary chain, which reaches the tokens
    let scenario = scenario.target(M4).approve_all(snap()).expect_revision(M4);
    state(
        scenario.target(M5).approve_all(snap()).expect_revision(M5),
        [4, 3, 2, 2, 2, 3, 2, 2, 2],
    )
    .run()
    .await
    .expect("the scenario holds");
}

/// Every pre-release id of the matrix follows the revision pattern, the catalog passes the
/// catalog consistency check, the base is a revision of the production catalog, and the sibling
/// is never the newest revision
#[test]
fn the_matrix_catalog_is_consistent() {
    // the catalog checks pass
    let matrix = fixture_catalog::MATRIX;
    if let Err(problems) = matrix.check() {
        panic!("the matrix catalog has problems: {problems:#?}");
    }
    // each id is the base and its suffix
    let ids = [M1, M2, M3, M4, M5, M5B];
    let suffixes = ["1", "2", "3", "4", "5", "5b"];
    for (id, suffix) in ids.iter().zip(suffixes) {
        assert_eq!(*id, format!("{BASE}-pre.matrix-{suffix}"));
    }
    // the base is a revision production plans between
    let base = BASE.parse().expect("a valid revision");
    assert!(
        Catalog::PRODUCTION.knows(&base),
        "{BASE} isn't in production"
    );
    // the newest revision of the primary chain is M5, never the sibling
    assert_eq!(matrix.latest().as_str(), M5);
    // every family a revision touches is one the fixtures know
    let known: BTreeSet<String> = all_families().iter().map(ToString::to_string).collect();
    for (_, step) in matrix.steps() {
        if let Some(transform) = step.transform {
            assert!(known.contains(&transform.family.to_string()));
        }
    }
}
