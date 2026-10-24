//! Tests of the upgrade scenario harness's direct driver, against real Scylla, Redis, and S3:
//! approval gates, single-stepping, lossy confirmations, pass counts, optional-field variants,
//! and reason-coded corruptions

use std::collections::{BTreeMap, BTreeSet};
use thorium::models::upgrades::downgrade_fixtures::{CONTACTS, LOSSY, contact_user};
use thorium::models::upgrades::fixtures::{BASE, NEXT, RELEASE};
use thorium::models::upgrades::plan_fixtures::{
    self, FUSED, MANUAL_BETWEEN, USERS, USERS_V2, UserVariant,
};
use thorium::models::upgrades::{
    Catalog, CatalogEntry, ConfirmRefusal, Family, FieldPath, IssueCategory, IssueReason,
    MigrationPhase, Record, RecordClass, StepDef, StepKind, StepState, TransformDef,
    TransformIssue, UpgradeApproval, UpgradeConditionType,
};
use thorium::test_utilities::upgrade_harness::{
    BackupRef, Check, CorruptionKind, FamilyFixture, FieldShape, FieldValues, FixtureSeed,
    FixtureVerifier, HarnessError, HistoryExpectation, Injection, Interrupt, Layout,
    PassCountCheck, Scenario, VariantSet, Variation,
};

/// The backup reference every approval here names
fn snap(name: &str) -> BackupRef {
    // a reference to a backup
    BackupRef::new(name)
}

/// Build a user in a layout with a name, so no user is an empty hash
fn user_record(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // the layout picks the variant, and every user gets a name besides
    let variant = match layout.0 {
        1 => UserVariant::Layout1,
        2 => UserVariant::Layout2,
        _ => UserVariant::Layout3,
    };
    let mut record = plan_fixtures::user(index, variant, index % 2 == 0, None);
    if let Record::Redis { fields, .. } = &mut record {
        fields.insert("name".to_owned(), format!("user-{index}"));
    }
    record
}

/// Find the layout a user is in from the fields it holds
fn user_layout(record: &Record) -> Option<Layout> {
    // the newest layout's field decides
    let Record::Redis { fields, .. } = record else {
        return None;
    };
    if fields.contains_key("settings") {
        Some(Layout(3))
    } else if fields.contains_key("theme") {
        Some(Layout(2))
    } else if fields.contains_key("dark") {
        Some(Layout(1))
    } else {
        None
    }
}

/// How the users family is built and read
fn users_fixture() -> FamilyFixture {
    // no corruption override
    FamilyFixture {
        generate: user_record,
        layout_of: user_layout,
        corrupt: None,
    }
}

/// Seed users in a layout
fn users(layout: u32, count: usize) -> FixtureSeed {
    // no variants
    FixtureSeed {
        family: USERS,
        layout: Layout(layout),
        count,
        variants: None,
    }
}

/// Expect every user in a layout
fn users_in(layout: u32) -> Check {
    // a verifier with nothing else expected
    Check::Fixtures(FixtureVerifier::items_in(USERS, Layout(layout)))
}

/// Build a fixture edge
const fn edge(
    from: Option<&'static str>,
    id: &'static str,
    steps: &'static [StepDef],
) -> CatalogEntry {
    // a release edge with nothing special
    CatalogEntry {
        from,
        id,
        released_with: "0.0.0",
        description: "A fixture edge",
        schema: None,
        prerelease: false,
        sibling: false,
        steps,
    }
}

/// Build a step that runs a transform
const fn transform(id: &'static str, transform: &'static TransformDef) -> StepDef {
    // a data step in the migrate phase
    StepDef {
        id,
        kind: StepKind::Data,
        description: "A fixture transform",
        phase: Some(MigrationPhase::Migrate),
        transform: Some(transform),
        thoradm_migration: Some(id),
        manual_procedure: Some("A fixture procedure"),
        ..StepDef::BASE
    }
}

/// The first edge's steps: an additive step and a transform
const STEPPED_ONE: &[StepDef] = &[
    StepDef {
        id: "add-column",
        kind: StepKind::Infra,
        description: "An additive step",
        phase: Some(MigrationPhase::Expand),
        ..StepDef::BASE
    },
    transform("users-v2", &USERS_V2),
];

/// The second edge's steps
const STEPPED_TWO: &[StepDef] = &[transform("users-v3", &plan_fixtures::USERS_V3)];

/// An additive step then a transform on the first edge, and a transform on the second
const STEPPED: Catalog = Catalog {
    edges: &[
        edge(None, BASE, &[]),
        edge(Some(BASE), RELEASE, STEPPED_ONE),
        edge(Some(RELEASE), NEXT, STEPPED_TWO),
    ],
    floors: &[],
};

/// Approving up to a step leaves the plan blocked on the next gated step, with nothing past it
/// run
#[tokio::test]
async fn t_tc_16_approving_up_to_a_step_blocks_at_the_next_gate() {
    // a plan with a manual step between two transforms has three gates
    Scenario::new("approve-up-to")
        .catalog(MANUAL_BETWEEN)
        .family(USERS, users_fixture())
        .seed(users(1, 20))
        .target(NEXT)
        .expect_blocked("users-v2")
        .check(users_in(1))
        .check(Check::History(vec![]))
        .approve("users-v2", snap("snap-1"))
        .expect_blocked("by-hand")
        .check(users_in(2))
        .check(Check::History(vec![
            HistoryExpectation::new("users-v2", StepState::Done).with_backup("snap-1"),
        ]))
        .approve("by-hand", snap("snap-1"))
        .expect_blocked("users-v3")
        .check(users_in(2))
        .check(Check::History(vec![
            HistoryExpectation::new("users-v2", StepState::Done).with_backup("snap-1"),
            HistoryExpectation::new("by-hand", StepState::Done).with_backup("snap-1"),
        ]))
        .approve("users-v3", snap("snap-1"))
        .expect_revision(NEXT)
        .check(users_in(3))
        .check(Check::History(vec![
            HistoryExpectation::new("users-v2", StepState::Done).with_backup("snap-1"),
            HistoryExpectation::new("by-hand", StepState::Done).with_backup("snap-1"),
            HistoryExpectation::new("users-v3", StepState::Done).with_backup("snap-1"),
        ]))
        .run()
        .await
        .expect("the scenario holds");
    // expecting the wrong gate fails the scenario and names both steps
    let error = Scenario::new("wrong-gate")
        .catalog(MANUAL_BETWEEN)
        .family(USERS, users_fixture())
        .seed(users(1, 3))
        .target(NEXT)
        .expect_blocked("by-hand")
        .run()
        .await
        .expect_err("the plan blocks on users-v2 first");
    let text = error.to_string();
    assert!(
        text.contains("users-v2") && text.contains("by-hand"),
        "{text}"
    );
}

/// `step` runs exactly one step, including one that needs no approval, and the data after each
/// step matches that step's layout
#[tokio::test]
async fn t_tc_17_step_runs_exactly_one_step() {
    // step through an additive step and a gated transform
    let log = Scenario::new("stepping")
        .catalog(STEPPED)
        .family(USERS, users_fixture())
        .seed(users(1, 12))
        .target(RELEASE)
        .step()
        .check(Check::History(vec![HistoryExpectation::new(
            "add-column",
            StepState::Done,
        )]))
        .check(users_in(1))
        .step()
        .check(Check::History(vec![HistoryExpectation::new(
            "add-column",
            StepState::Done,
        )]))
        .check(users_in(1))
        .approve("users-v2", snap("snap-1"))
        .step()
        .check(users_in(2))
        .check(Check::History(vec![
            HistoryExpectation::new("add-column", StepState::Done),
            HistoryExpectation::new("users-v2", StepState::Done).with_backup("snap-1"),
        ]))
        .step()
        .expect_revision(RELEASE)
        .target(NEXT)
        .step()
        .check(users_in(2))
        .approve("users-v3", snap("snap-1"))
        .step()
        .check(users_in(3))
        .expect_revision(NEXT)
        .run()
        .await
        .expect("the scenario holds");
    // each step action says what it ran
    let steps: Vec<&str> = log
        .iter()
        .filter(|record| record.action == "step")
        .map(|record| record.outcome.as_str())
        .collect();
    assert_eq!(steps.len(), 6, "{steps:?}");
    assert!(steps[0].contains("add-column"), "{steps:?}");
    assert!(steps[1].contains("blocked on users-v2"), "{steps:?}");
    assert!(steps[2].contains("redis:users"), "{steps:?}");
    assert!(steps[3].contains("nothing left"), "{steps:?}");
    assert!(steps[4].contains("blocked on users-v3"), "{steps:?}");
    assert!(steps[5].contains("redis:users"), "{steps:?}");
}

/// Build a contacts fixture in a layout
fn contacts_record(layout: Layout, index: usize, _variation: &Variation) -> Record {
    // layout 1 holds one contact; layout 2 holds a list of one
    let address = format!("user{index}@example.com");
    if layout.0 == 1 {
        contact_user(index, "contact", &address)
    } else {
        contact_user(index, "contacts", &format!("[\"{address}\"]"))
    }
}

/// Find the layout a contacts user is in
fn contacts_layout(record: &Record) -> Option<Layout> {
    // the list is layout 2 and the single contact layout 1
    let Record::Redis { fields, .. } = record else {
        return None;
    };
    if fields.contains_key("contacts") {
        Some(Layout(2))
    } else if fields.contains_key("contact") {
        Some(Layout(1))
    } else {
        None
    }
}

/// A scenario on the lossy catalog that has moved contacts to layout 2
fn lossy_scenario(name: &str) -> Scenario {
    // seed layout 1, approve the forward step, and move up
    Scenario::new(name)
        .catalog(LOSSY)
        .family(
            CONTACTS,
            FamilyFixture {
                generate: contacts_record,
                layout_of: contacts_layout,
                corrupt: None,
            },
        )
        .seed(FixtureSeed {
            family: CONTACTS,
            layout: Layout(1),
            count: 10,
            variants: None,
        })
        .target(RELEASE)
        .approve("contacts-v2", snap("snap-0"))
        .expect_revision(RELEASE)
        .check(Check::Fixtures(FixtureVerifier::items_in(
            CONTACTS,
            Layout(2),
        )))
}

/// Expect every contact in a layout
fn contacts_in(layout: u32) -> Check {
    // a verifier with nothing else expected
    Check::Fixtures(FixtureVerifier::items_in(CONTACTS, Layout(layout)))
}

/// A lossy step blocks with a code, needs a backup and its own code, records the code and loss,
/// can't reuse a consumed code, and the dev setting works only when the gate is told it is dev
#[tokio::test]
async fn t_tc_18_lossy_approvals_use_the_blocked_steps_code() {
    // the lossy reverse blocks, refuses an approval with no backup, and runs with a backup
    let forward = HistoryExpectation::new("contacts-v2", StepState::Done).with_backup("snap-0");
    let reverse = |backup: &str| {
        HistoryExpectation::new("contacts-v2-reverse", StepState::Done)
            .with_backup(backup)
            .lossy()
    };
    lossy_scenario("lossy-confirm")
        .target(BASE)
        .expect_blocked_lossy("contacts-v2-reverse")
        .check(contacts_in(2))
        .check(Check::History(vec![forward.clone()]))
        .approve_loss_without_backup("contacts-v2-reverse")
        .expect_blocked_lossy("contacts-v2-reverse")
        .check(contacts_in(2))
        .approve_loss("contacts-v2-reverse", snap("snap-1"))
        .expect_revision(BASE)
        .check(contacts_in(1))
        .check(Check::History(vec![forward.clone(), reverse("snap-1")]))
        // a wrong literal code is refused, with the step still blocked
        .target(RELEASE)
        .expect_revision(RELEASE)
        .target(BASE)
        .expect_blocked_lossy("contacts-v2-reverse")
        .approve_loss_with("contacts-v2-reverse", snap("snap-2"), "WRONG000")
        .expect_confirm_refused("contacts-v2-reverse", ConfirmRefusal::Mismatch)
        // the code the first run used is consumed
        .approve_loss_reusing("contacts-v2-reverse", snap("snap-2"))
        .expect_confirm_refused("contacts-v2-reverse", ConfirmRefusal::Consumed)
        .check(contacts_in(2))
        .approve_loss("contacts-v2-reverse", snap("snap-2"))
        .expect_revision(BASE)
        .check(contacts_in(1))
        .run()
        .await
        .expect("the confirmations hold");
    // on a dev deployment the setting runs the step with no backup and no code
    lossy_scenario("lossy-dev")
        .dev(true)
        .set_dev_lossy(true)
        .target(BASE)
        .expect_revision(BASE)
        .check(contacts_in(1))
        .check(Check::History(vec![
            forward.clone(),
            HistoryExpectation {
                loss: true,
                ..HistoryExpectation::new("contacts-v2-reverse", StepState::Done)
            },
        ]))
        .check(Check::Condition {
            kind: UpgradeConditionType::LossySettingRefused,
            present: false,
        })
        .run()
        .await
        .expect("a dev deployment honors the setting");
    // anywhere else the setting is refused and the step keeps its gate
    lossy_scenario("lossy-not-dev")
        .set_dev_lossy(true)
        .target(BASE)
        .expect_blocked_lossy("contacts-v2-reverse")
        .check(Check::Condition {
            kind: UpgradeConditionType::LossySettingRefused,
            present: true,
        })
        .check(contacts_in(2))
        .approve_loss("contacts-v2-reverse", snap("snap-3"))
        .expect_revision(BASE)
        .check(Check::History(vec![forward, reverse("snap-3")]))
        .run()
        .await
        .expect("a refused setting leaves the gate in place");
}

/// A scenario that moves 30 users from layout 1 to 3 on a catalog
fn pass_counts(catalog: Catalog, manual: bool, reads: u64, writes: u64) -> Scenario {
    // approve every gate of the plan, then count the passes' reads and writes
    let scenario = Scenario::new("pass-counts")
        .catalog(catalog)
        .family(USERS, users_fixture())
        .seed(users(1, 30))
        .target(NEXT)
        .approve("users-v2", snap("snap-1"))
        .approve("users-v3", snap("snap-1"));
    let scenario = if manual {
        scenario.approve("by-hand", snap("snap-1"))
    } else {
        scenario
    };
    scenario
        .expect_revision(NEXT)
        .check(users_in(3))
        .check(Check::Passes(PassCountCheck {
            family: USERS,
            reads_per_object: reads,
            writes_per_object: writes,
        }))
}

/// The pass-count check fails a chain split in two passes and passes the fused one
#[tokio::test]
async fn t_tc_19_the_pass_count_check_tells_fused_from_unfused() {
    // the fused chain reads and writes each user once
    pass_counts(FUSED, false, 1, 1)
        .run()
        .await
        .expect("a fused chain touches each object once");
    // a manual step splits the chain, so each user is read and written twice
    let error = pass_counts(MANUAL_BETWEEN, true, 1, 1)
        .run()
        .await
        .expect_err("two passes touch each object twice");
    assert!(matches!(error, HarnessError::Failed(_)), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("reads") && text.contains("writes"), "{text}");
    // the split chain's own counts are two and two
    pass_counts(MANUAL_BETWEEN, true, 2, 2)
        .run()
        .await
        .expect("an unfused chain touches each object twice");
}

/// The family the optional-field fixtures live in
const PROFILES: Family = Family::Redis("profiles");

/// The optional nickname, a string
const NICK: FieldPath = FieldPath("nick");

/// The optional age, an integer
const AGE: FieldPath = FieldPath("age");

/// The optional role, one of `admin` and `user`
const ROLE: FieldPath = FieldPath("role");

/// Read an optional field's text: `None` when it is absent, empty, or null
fn text<'a>(fields: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    // absent, empty, and null all mean no value
    fields
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty() && *value != "null")
}

/// Find what keeps a layout 1 profile from converting
fn profile_problem(fields: &BTreeMap<String, String>) -> Option<IssueReason> {
    // a nickname that is a JSON container is the wrong type
    if text(fields, "nick").is_some_and(|nick| nick.starts_with(['[', '{'])) {
        return Some(IssueReason::new(IssueCategory::BadType, "nick"));
    }
    // an age must be an integer
    if text(fields, "age").is_some_and(|age| age.parse::<u32>().is_err()) {
        return Some(IssueReason::new(IssueCategory::BadType, "age"));
    }
    // a role must be a string naming a known role
    match text(fields, "role") {
        Some(role) if role.starts_with(['[', '{']) => {
            Some(IssueReason::new(IssueCategory::BadType, "role"))
        }
        Some(role) if !["admin", "user"].contains(&role) => {
            Some(IssueReason::new(IssueCategory::UnknownVariant, "role"))
        }
        _ => None,
    }
}

/// Classify a profile
fn profile_classify(record: &Record) -> RecordClass {
    // only Redis profiles classify
    let Record::Redis { fields, .. } = record else {
        return RecordClass::Unrecognized {
            reason: IssueReason::new(IssueCategory::MalformedJson, ""),
        };
    };
    // layout 2 holds the `profile` object
    if fields.contains_key("profile") {
        RecordClass::Current
    } else if let Some(reason) = profile_problem(fields) {
        RecordClass::Unrecognized { reason }
    } else {
        RecordClass::Migratable { layout: 1 }
    }
}

/// Move a profile's optional fields into one JSON `profile` field
fn profile_up(mut record: Record) -> Result<Record, TransformIssue> {
    // a profile that doesn't convert is an issue, and a current one stays as it is
    match profile_classify(&record) {
        RecordClass::Unrecognized { reason } => {
            return Err(TransformIssue {
                reason,
                layouts_tried: vec![1],
            });
        }
        RecordClass::Current => return Ok(record),
        RecordClass::Migratable { .. } => (),
    }
    // collect the optional fields that hold a value
    let Record::Redis { fields, .. } = &mut record else {
        return Ok(record);
    };
    let mut profile = serde_json::Map::new();
    for name in ["nick", "age", "role"] {
        let value = text(fields, name).map(str::to_owned);
        fields.remove(name);
        if let Some(value) = value {
            let value = if name == "age" {
                serde_json::Value::from(value.parse::<u32>().unwrap_or_default())
            } else {
                serde_json::Value::from(value)
            };
            profile.insert(name.to_owned(), value);
        }
    }
    fields.insert(
        "profile".to_owned(),
        serde_json::Value::Object(profile).to_string(),
    );
    Ok(record)
}

/// Move a profile's `profile` object back into flat fields
fn profile_down(mut record: Record) -> Result<Record, TransformIssue> {
    // a profile without a valid object can't revert
    let broken = |field: &str| TransformIssue {
        reason: IssueReason::new(IssueCategory::MalformedJson, field),
        layouts_tried: vec![2],
    };
    let Record::Redis { fields, .. } = &mut record else {
        return Err(broken(""));
    };
    let raw = fields.remove("profile").ok_or_else(|| broken("profile"))?;
    let profile: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&raw).map_err(|_| broken("profile"))?;
    // flatten each field of the object
    for (name, value) in profile {
        let value = match value {
            serde_json::Value::String(value) => value,
            other => other.to_string(),
        };
        fields.insert(name, value);
    }
    Ok(record)
}

/// The profiles transform, which declares its optional fields
const PROFILES_V2: TransformDef = TransformDef {
    family: PROFILES,
    reads: &[],
    from: 1,
    to: 2,
    classify: profile_classify,
    up: profile_up,
    down: Some(profile_down),
    optional: &[NICK, AGE, ROLE],
};

/// The values each optional profile field takes in each shape
fn profile_values(field: &FieldPath) -> FieldValues {
    // an unknown variant is only an error for the role, which is the one enum
    match field.0 {
        "nick" => FieldValues {
            valid: "bobby",
            wrong_type: "[7]",
            empty: "",
            unknown_variant: "bobby2",
        },
        "age" => FieldValues {
            valid: "42",
            wrong_type: "forty",
            empty: "",
            unknown_variant: "43",
        },
        _ => FieldValues {
            valid: "admin",
            wrong_type: "[7]",
            empty: "",
            unknown_variant: "__undefined__",
        },
    }
}

/// Build a profile in a layout with its optional fields shaped as a variation says
fn profile_record(layout: Layout, index: usize, variation: &Variation) -> Record {
    // layout 1 holds the flat optional fields
    let mut fields = BTreeMap::new();
    fields.insert("name".to_owned(), format!("profile-{index}"));
    for field in PROFILES_V2.optional {
        if let Some(value) = variation.shape(field).value(&profile_values(field)) {
            fields.insert(field.0.to_owned(), value);
        }
    }
    let record = Record::Redis {
        key: format!("profiles:{index}"),
        fields,
    };
    // layout 2 is what the transform makes of a valid layout 1 profile
    if layout.0 == 1 {
        record
    } else {
        profile_up(record).expect("a valid profile converts")
    }
}

/// Find the layout a profile is in
fn profile_layout(record: &Record) -> Option<Layout> {
    // the `profile` object is layout 2
    match record {
        Record::Redis { fields, .. } if fields.contains_key("profile") => Some(Layout(2)),
        Record::Redis { .. } => Some(Layout(1)),
        _ => None,
    }
}

/// What the declared shapes say a variation classifies as
fn declared_class(variation: &Variation) -> RecordClass {
    // the first field whose shape can't convert decides, in field order
    for field in [NICK, AGE, ROLE] {
        match variation.shape(&field) {
            FieldShape::WrongType => {
                return RecordClass::Unrecognized {
                    reason: IssueReason::new(IssueCategory::BadType, field.0),
                };
            }
            FieldShape::UnknownVariant if field == ROLE => {
                return RecordClass::Unrecognized {
                    reason: IssueReason::new(IssueCategory::UnknownVariant, "role"),
                };
            }
            _ => (),
        }
    }
    // every other shape is a missing or valid value
    RecordClass::Migratable { layout: 1 }
}

/// Every shape a field takes
const SHAPES: [FieldShape; 6] = [
    FieldShape::Present,
    FieldShape::Absent,
    FieldShape::Null,
    FieldShape::WrongType,
    FieldShape::Empty,
    FieldShape::UnknownVariant,
];

/// The catalog that moves profiles to layout 2
const PROFILE_CATALOG: Catalog = Catalog {
    edges: &[
        edge(None, BASE, &[]),
        edge(
            Some(BASE),
            RELEASE,
            &[transform("profiles-v2", &PROFILES_V2)],
        ),
    ],
    floors: &[],
};

/// A fixture transform's classifications match its declared classes over every generated
/// variant, `up` and `down` round-trip every variant that converts, and the pass over the
/// seeded variants converts what converts and groups what doesn't
#[tokio::test]
async fn t_tc_31_a_transform_handles_every_optional_field_shape() {
    // every combination of the three optional fields' shapes
    let set = VariantSet {
        fields: PROFILES_V2.optional.to_vec(),
        shapes: SHAPES.to_vec(),
    };
    let variations = set.variations();
    assert_eq!(variations.len(), 216);
    let mut groups: BTreeMap<String, u64> = BTreeMap::new();
    let mut flagged = Vec::new();
    for (index, variation) in variations.iter().enumerate() {
        // the transform classifies the generated object as the declared class
        let record = profile_record(Layout(1), index, variation);
        let class = (PROFILES_V2.classify)(&record);
        assert_eq!(class, declared_class(variation), "{variation:?}");
        match class {
            // a convertible one round-trips and loses nothing
            RecordClass::Migratable { .. } => {
                PROFILES_V2
                    .check_round_trip(&record)
                    .unwrap_or_else(|error| panic!("{variation:?}: {error}"));
                let up = (PROFILES_V2.up)(record.clone()).expect("converts");
                let down = (PROFILES_V2.down.expect("reverts"))(up).expect("reverts");
                let (Record::Redis { fields: before, .. }, Record::Redis { fields: after, .. }) =
                    (&record, &down)
                else {
                    panic!("redis records");
                };
                for name in ["nick", "age", "role"] {
                    assert_eq!(text(before, name), text(after, name), "{variation:?}");
                }
            }
            // the others are grouped by their reason and flagged
            RecordClass::Unrecognized { reason } => {
                *groups.entry(reason.to_string()).or_default() += 1;
                flagged.push(format!("profiles:{index}"));
            }
            RecordClass::Current => panic!("a layout 1 profile is never current"),
        }
    }
    // the seeded pass reads each variant once, converts the convertible ones, and groups the rest
    let accepted: Vec<String> = groups.keys().cloned().collect();
    Scenario::new("variants")
        .catalog(PROFILE_CATALOG)
        .family(
            PROFILES,
            FamilyFixture {
                generate: profile_record,
                layout_of: profile_layout,
                corrupt: None,
            },
        )
        .seed(FixtureSeed {
            family: PROFILES,
            layout: Layout(1),
            count: variations.len(),
            variants: Some(set),
        })
        .target(RELEASE)
        .approve_with(UpgradeApproval {
            step: "profiles-v2".to_owned(),
            backup: "snap-1".to_owned(),
            accept_issues: accepted,
            ..UpgradeApproval::default()
        })
        .expect_revision(RELEASE)
        .check(Check::Issues {
            migration: "redis:profiles".to_owned(),
            groups: groups.into_iter().collect(),
        })
        .check(Check::Fixtures(
            FixtureVerifier::items_in(PROFILES, Layout(2)).with_flagged(flagged),
        ))
        .run()
        .await
        .expect("the variants convert or group as declared");
}

/// Corrupt two of six users in a way and check the reason code the pass records
async fn corrupted(layout: u32, kind: CorruptionKind, field: &str, reason: &str) {
    // seed, corrupt the first two users, and run the whole chain with approvals
    let keys = vec!["users:fixture-0".to_owned(), "users:fixture-1".to_owned()];
    Scenario::new(format!("corrupt-{reason}"))
        .catalog(FUSED)
        .family(USERS, users_fixture())
        .seed(users(layout, 6))
        .inject(Injection::CorruptObjects {
            family: USERS,
            objects: keys.clone(),
            kind,
            field: Some(FieldPath(Box::leak(field.to_owned().into_boxed_str()))),
        })
        .target(NEXT)
        .approve_all(snap("snap-1"))
        .expect_revision(NEXT)
        .check(Check::Issues {
            migration: "redis:users".to_owned(),
            groups: vec![(reason.to_owned(), 2)],
        })
        .check(Check::Fixtures(
            FixtureVerifier::items_in(USERS, Layout(3)).with_flagged(keys),
        ))
        .run()
        .await
        .unwrap_or_else(|error| panic!("{kind:?}: {error}"));
}

/// Each corruption kind yields its reason code, a systematic break forms one group of every
/// object, and the issue check matches groups and counts
#[tokio::test]
async fn t_tc_32_corruptions_yield_their_reason_codes() {
    // each kind on a field some layout of the users family holds
    corrupted(
        3,
        CorruptionKind::MalformedJson,
        "settings",
        "malformed_json:settings",
    )
    .await;
    corrupted(
        1,
        CorruptionKind::MissingField,
        "dark",
        "missing_field:dark",
    )
    .await;
    corrupted(
        2,
        CorruptionKind::UnknownVariant,
        "theme",
        "unknown_variant:theme",
    )
    .await;
    corrupted(3, CorruptionKind::BadType, "settings", "bad_type:settings").await;
    // one field broken on every user is one group holding all of them
    let everyone: BTreeSet<String> = (0..30)
        .map(|index| format!("users:fixture-{index}"))
        .collect();
    Scenario::new("systematic")
        .catalog(FUSED)
        .family(USERS, users_fixture())
        .seed(users(3, 30))
        .inject(Injection::SystematicBreak {
            family: USERS,
            field: FieldPath("settings"),
        })
        .target(NEXT)
        .approve_all(snap("snap-1"))
        .approve_with(UpgradeApproval {
            step: "users-v3".to_owned(),
            backup: "snap-1".to_owned(),
            accept_issues: vec!["bad_type:settings".to_owned()],
            ..UpgradeApproval::default()
        })
        .expect_revision(NEXT)
        .check(Check::Issues {
            migration: "redis:users".to_owned(),
            groups: vec![("bad_type:settings".to_owned(), everyone.len() as u64)],
        })
        .run()
        .await
        .expect("a systematic break forms one group");
    // the issue check fails when the groups differ
    let error = Scenario::new("wrong-groups")
        .catalog(FUSED)
        .family(USERS, users_fixture())
        .seed(users(3, 6))
        .inject(Injection::CorruptObjects {
            family: USERS,
            objects: vec!["users:fixture-0".to_owned()],
            kind: CorruptionKind::BadType,
            field: Some(FieldPath("settings")),
        })
        .target(NEXT)
        .approve_all(snap("snap-1"))
        .expect_revision(NEXT)
        .check(Check::Issues {
            migration: "redis:users".to_owned(),
            groups: vec![("bad_type:settings".to_owned(), 2)],
        })
        .run()
        .await
        .expect_err("one object is one group of one");
    assert!(error.to_string().contains("bad_type:settings"), "{error}");
}

/// A pass stopped between batches resumes from its checkpoint and reads each object once
#[tokio::test]
async fn an_interrupted_pass_resumes_without_rereading() {
    // the pass is cancelled after its second batch, so it stops partway
    let log = Scenario::new("interrupted")
        .catalog(FUSED)
        .family(USERS, users_fixture())
        .seed(users(1, 60))
        .inject(Injection::InterruptAtBatch {
            batch: 2,
            signal: Interrupt::Term,
        })
        .target(NEXT)
        .approve_all(snap("snap-1"))
        .step()
        .check(Check::History(vec![]))
        .expect_revision(NEXT)
        .check(users_in(3))
        .check(Check::Passes(PassCountCheck {
            family: USERS,
            reads_per_object: 1,
            writes_per_object: 1,
        }))
        .run()
        .await
        .expect("the pass resumes");
    // the first step reports the interruption
    let step = log
        .iter()
        .find(|record| record.action == "step")
        .expect("a step ran");
    assert!(step.outcome.contains("interrupted"), "{}", step.outcome);
}
