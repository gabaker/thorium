//! Tests of the reversibility checks: reverse-edge round trips, each kind's artifacts, the
//! restrictive-fill lint on security-sensitive fields, and contract steps that a later additive
//! step would undo

use super::downgrade_fixtures::{
    CONTACT_SENTINEL, CONTACTS, CONTACTS_LOSS, CONTACTS_V2, DOWNGRADE, DOWNGRADE_ARTIFACTS,
    DUAL_WRITE, DUAL_WRITE_ARTIFACTS, LOSSY, LOSSY_ARTIFACTS, ROLE_ORDER, ROLE_VARIANTS,
    contact_user,
};
use super::fixtures::{self, BASE, NEXT, RELEASE};
use super::plan_fixtures::USERS;
use super::{
    ARTIFACTS, Catalog, CatalogEntry, EnumMapping, EnumVariant, Fill, FillException, FillValue,
    IssueCategory, IssueReason, MigrationPhase, PrivilegeOrder, Record, RecordClass,
    ReverseArtifacts, Reversibility, SchemaBackend, SchemaTarget, SensitiveField, SensitiveKind,
    StepChanges, StepDef, StepFixtures, StepKind, TransformDef, VariantGrants, lint_fills,
};

/// Copy a catalog with one step replaced
///
/// # Arguments
///
/// * `catalog` - The catalog to copy
/// * `id` - The id of the step to replace
/// * `replace` - Build the replacement from the step
fn with_step(catalog: &Catalog, id: &str, replace: impl Fn(StepDef) -> StepDef) -> Catalog {
    // rebuild every edge, swapping the one step
    let edges = catalog
        .edges
        .iter()
        .map(|edge| CatalogEntry {
            steps: edge
                .steps
                .iter()
                .map(|step| if step.id == id { replace(*step) } else { *step })
                .collect::<Vec<_>>()
                .leak(),
            ..*edge
        })
        .collect::<Vec<_>>();
    Catalog {
        edges: edges.leak(),
        floors: catalog.floors,
    }
}

/// Whether some problem holds every one of the given pieces of text
///
/// # Arguments
///
/// * `problems` - The problems a check reported
/// * `pieces` - The text one problem must hold
fn reported(problems: &[String], pieces: &[&str]) -> bool {
    // look for one problem holding all of them
    problems
        .iter()
        .any(|problem| pieces.iter().all(|piece| problem.contains(piece)))
}

/// The layout 2 to 3 users transform without a `down`
static USERS_V3_NO_DOWN: TransformDef = TransformDef {
    down: None,
    ..super::plan_fixtures::USERS_V3
};

/// The layout 1 to 2 users transform without a `down`
static USERS_V2_NO_DOWN: TransformDef = TransformDef {
    down: None,
    ..super::plan_fixtures::USERS_V2
};

/// Build a user whose flag is neither true nor false, which `up` refuses, so its round trip fails
fn broken() -> Vec<Record> {
    // one user with a bad flag
    vec![Record::Redis {
        key: "users:broken".to_owned(),
        fields: [("dark".to_owned(), "maybe".to_owned())].into(),
    }]
}

/// Build a user in layout 2 with a theme the reverse can't turn into a flag
fn unrevertable() -> Vec<Record> {
    // one user with an unknown theme
    vec![Record::Redis {
        key: "users:purple".to_owned(),
        fields: [("theme".to_owned(), "purple".to_owned())].into(),
    }]
}

/// Build a user whose only contact is the sentinel, which upgrades to an empty list, so the lossy
/// reverse has to fill its contact
fn sentinel_contacts() -> Vec<Record> {
    // one user holding the sentinel
    vec![contact_user(0, "contact", CONTACT_SENTINEL)]
}

/// Fills of the lossy fixture step where a constant fill of the contact comes before its
/// sentinel fill, so the sentinel never lands
const SHADOWED_FILLS: &[Fill] = &[
    Fill {
        field: "contact",
        value: FillValue::Constant("nobody@example.com"),
    },
    Fill {
        field: "contact",
        value: FillValue::Sentinel(CONTACT_SENTINEL),
    },
];

/// Fills of the lossy fixture step that also write a `stale` field, which [`STALE_CONTACTS`]
/// refuses to call current
const STALE_FILLS: &[Fill] = &[
    Fill {
        field: "contact",
        value: FillValue::Sentinel(CONTACT_SENTINEL),
    },
    Fill {
        field: "stale",
        value: FillValue::Constant("yes"),
    },
];

/// Classify a contacts user as the fixture transform does, except that a user holding a `stale`
/// field is unrecognized
///
/// # Arguments
///
/// * `record` - The user to classify
fn stale_classify(record: &Record) -> RecordClass {
    // a stale field makes the user unrecognized, and anything else is classified as before
    if record.has_field("stale") {
        RecordClass::Unrecognized {
            reason: IssueReason::new(IssueCategory::UnknownField, "stale"),
        }
    } else {
        (CONTACTS_V2.classify)(record)
    }
}

/// The contacts transform with a classifier that refuses a `stale` field
static STALE_CONTACTS: TransformDef = TransformDef {
    classify: stale_classify,
    ..CONTACTS_V2
};

/// Whether the lint accepted everything: it reported no problem
///
/// # Arguments
///
/// * `problems` - The problems the lint reported
fn accepted(problems: &[String]) -> bool {
    // no problem means every fill and mapping passed
    problems.is_empty()
}

/// The catalog CI test fails when a step in a reverse edge has no `down` or no registered
/// round-trip test
#[test]
fn reverse_edge_steps_need_downs_and_round_trips() {
    // the downgrade catalog with its artifacts passes, as does the production catalog
    assert_eq!(DOWNGRADE.check(), Ok(()));
    assert_eq!(DOWNGRADE.check_artifacts(&DOWNGRADE_ARTIFACTS), Ok(()));
    assert_eq!(Catalog::PRODUCTION.check_artifacts(&ARTIFACTS), Ok(()));
    // without registered round trips every reverse of a transform fails
    let missing = ReverseArtifacts {
        round_trips: &[],
        ..DOWNGRADE_ARTIFACTS
    };
    let problems = DOWNGRADE
        .check_artifacts(&missing)
        .expect_err("no round trips");
    for step in ["users-v2", "users-v3"] {
        assert!(
            reported(&problems, &[step, "no registered round-trip test"]),
            "{problems:?}"
        );
    }
    // a reversed transform without a `down` fails
    let catalog = with_step(&DOWNGRADE, "users-v3", |step| StepDef {
        transform: Some(&USERS_V3_NO_DOWN),
        ..step
    });
    let problems = catalog
        .check_artifacts(&DOWNGRADE_ARTIFACTS)
        .expect_err("no down");
    assert!(
        reported(&problems, &["users-v3-reverse", "no `down`"]),
        "{problems:?}"
    );
    // a registered round trip that doesn't come back fails
    let failing = ReverseArtifacts {
        round_trips: vec![
            super::StepFixtures {
                step: "users-v2",
                fixtures: broken,
            },
            DOWNGRADE_ARTIFACTS.round_trips[1],
        ]
        .leak(),
        ..DOWNGRADE_ARTIFACTS
    };
    let problems = DOWNGRADE
        .check_artifacts(&failing)
        .expect_err("failing round trip");
    assert!(
        reported(&problems, &["round trip of users-v2 failed"]),
        "{problems:?}"
    );
    // a lossy round trip whose sentinel doesn't land fails, here because an earlier constant
    // fill of the same field takes its place
    let shadowed = with_step(&LOSSY, "contacts-v2", |step| StepDef {
        reversibility: Some(lossy_contacts(SHADOWED_FILLS, CONTACTS_LOSS)),
        ..step
    });
    let emptied = ReverseArtifacts {
        round_trips: &[StepFixtures {
            step: "contacts-v2",
            fixtures: sentinel_contacts,
        }],
        ..LOSSY_ARTIFACTS
    };
    let problems = shadowed.check_artifacts(&emptied).expect_err("no sentinel");
    assert!(
        reported(
            &problems,
            &["round trip of contacts-v2 failed", "wrote no sentinel"]
        ),
        "{problems:?}"
    );
    // so does a lossy round trip whose re-upgraded record isn't current
    let stale = with_step(&LOSSY, "contacts-v2", |step| StepDef {
        transform: Some(&STALE_CONTACTS),
        reversibility: Some(lossy_contacts(STALE_FILLS, CONTACTS_LOSS)),
        ..step
    });
    let problems = stale
        .check_artifacts(&LOSSY_ARTIFACTS)
        .expect_err("not current");
    assert!(
        reported(
            &problems,
            &["round trip of contacts-v2 failed", "isn't current"]
        ),
        "{problems:?}"
    );
    // a transform on a release edge with a reverse edge but no step reversing it fails the
    // structural check, as does one declaring no reversibility
    let unreversed = with_step(&DOWNGRADE, "users-v3-reverse", |step| StepDef {
        reverses: Some("rebuild-b"),
        ..step
    });
    let problems = unreversed.check().expect_err("unreversed");
    assert!(
        reported(&problems, &["users-v3", "no step of"]),
        "{problems:?}"
    );
    let undeclared = with_step(&DOWNGRADE, "users-v3", |step| StepDef {
        reversibility: None,
        ..step
    });
    let problems = undeclared.check().expect_err("undeclared");
    assert!(
        reported(&problems, &["users-v3", "declares no reversibility"]),
        "{problems:?}"
    );
}

/// CI rejects a `DualWrite` transform without a generated compatibility test or without a later
/// contract step that names it
#[test]
fn dual_writes_need_compatibility_tests_and_contracts() {
    // with both the dual write passes
    assert_eq!(DUAL_WRITE.check(), Ok(()));
    assert_eq!(DUAL_WRITE.check_artifacts(&DUAL_WRITE_ARTIFACTS), Ok(()));
    // without a compatibility test it fails
    let untested = ReverseArtifacts {
        compatibility: &[],
        ..DUAL_WRITE_ARTIFACTS
    };
    let problems = DUAL_WRITE.check_artifacts(&untested).expect_err("untested");
    assert!(
        reported(&problems, &["users-v2", "no generated compatibility test"]),
        "{problems:?}"
    );
    // a contract revision whose edge holds no contract step fails
    let uncontracted = with_step(&DUAL_WRITE, "drop-dark", |step| StepDef {
        phase: Some(MigrationPhase::Migrate),
        ..step
    });
    let problems = uncontracted
        .check_artifacts(&DUAL_WRITE_ARTIFACTS)
        .expect_err("no contract");
    assert!(
        reported(&problems, &["users-v2", "no later contract"]),
        "{problems:?}"
    );
    // so does naming the step's own revision, or one the catalog doesn't have
    for until in [RELEASE, "2026-10-v09"] {
        let unnamed = with_step(&DUAL_WRITE, "users-v2", |step| StepDef {
            reversibility: Some(Reversibility::DualWrite { until }),
            ..step
        });
        let problems = unnamed
            .check_artifacts(&DUAL_WRITE_ARTIFACTS)
            .expect_err("no later contract");
        assert!(reported(&problems, &["no later contract"]), "{problems:?}");
    }
}

/// CI rejects a `Backfill` transform without `down` or without new-layout fixture tests
#[test]
fn backfills_need_downs_and_new_layout_fixtures() {
    // without new-layout fixtures a backfill fails
    let missing = ReverseArtifacts {
        native: &DOWNGRADE_ARTIFACTS.native[..1],
        ..DOWNGRADE_ARTIFACTS
    };
    let problems = DOWNGRADE
        .check_artifacts(&missing)
        .expect_err("no fixtures");
    assert!(
        reported(&problems, &["users-v3", "no new-layout fixtures"]),
        "{problems:?}"
    );
    // a backfill without a `down` fails
    let catalog = with_step(&DOWNGRADE, "users-v2", |step| StepDef {
        transform: Some(&USERS_V2_NO_DOWN),
        ..step
    });
    let problems = catalog
        .check_artifacts(&DOWNGRADE_ARTIFACTS)
        .expect_err("no down");
    assert!(
        reported(
            &problems,
            &["Backfill step users-v2", "no transform with a `down`"]
        ),
        "{problems:?}"
    );
    // new-layout fixtures the reverse can't bring back fail
    let failing = ReverseArtifacts {
        native: vec![
            super::StepFixtures {
                step: "users-v2",
                fixtures: unrevertable,
            },
            DOWNGRADE_ARTIFACTS.native[1],
        ]
        .leak(),
        ..DOWNGRADE_ARTIFACTS
    };
    let problems = DOWNGRADE.check_artifacts(&failing).expect_err("failing");
    assert!(
        reported(&problems, &["new-layout fixture of users-v2"]),
        "{problems:?}"
    );
}

/// CI rejects a `LossyRevert` transform without a fill for every lost field in the snapshot
/// diff, or without a mapping for every enum variant added in the new revision
#[test]
fn lossy_reverts_need_fills_and_mappings() {
    // the lossy catalog with its artifacts passes
    assert_eq!(LOSSY.check_artifacts(&LOSSY_ARTIFACTS), Ok(()));
    // a field the diff drops that the step doesn't declare lost fails
    let changes = [StepChanges {
        step: "contacts-v2",
        lost: &["contact", "phone"],
        new_variants: &[],
    }];
    let dropped = ReverseArtifacts {
        changes: Vec::from(changes).leak(),
        ..LOSSY_ARTIFACTS
    };
    let problems = LOSSY
        .check_artifacts(&dropped)
        .expect_err("undeclared loss");
    assert!(
        reported(&problems, &["contacts-v2", "phone"]),
        "{problems:?}"
    );
    // a lost field without a fill fails
    let unfilled = with_step(&LOSSY, "contacts-v2", |step| StepDef {
        reversibility: Some(Reversibility::LossyRevert {
            fills: &[],
            lost_fields: &["contact"],
            enum_mappings: &[],
            privilege_orders: &[],
            description: "Users keep only their first contact",
        }),
        ..step
    });
    let problems = unfilled
        .check_artifacts(&LOSSY_ARTIFACTS)
        .expect_err("no fill");
    assert!(
        reported(
            &problems,
            &["contacts-v2", "no fill for the lost field contact"]
        ),
        "{problems:?}"
    );
    // a variant the new revision adds without a mapping fails, and a mapping fixes it
    let added = pager_artifacts();
    let problems = LOSSY.check_artifacts(&added).expect_err("no mapping");
    assert!(
        reported(&problems, &["contacts-v2", "no mapping", "Pager", "kind"]),
        "{problems:?}"
    );
    assert_eq!(pager_mapping("kind").check_artifacts(&added), Ok(()));
    // without a registered diff, compatibility test, or description it fails too
    let bare = ReverseArtifacts {
        round_trips: LOSSY_ARTIFACTS.round_trips,
        ..ReverseArtifacts::EMPTY
    };
    let problems = LOSSY.check_artifacts(&bare).expect_err("bare");
    assert!(
        reported(&problems, &["no registered snapshot diff"]),
        "{problems:?}"
    );
    assert!(
        reported(&problems, &["no generated compatibility test"]),
        "{problems:?}"
    );
    let undescribed = with_step(&LOSSY, "contacts-v2", |step| StepDef {
        reversibility: Some(lossy_contacts(SENTINEL_FILL, " ")),
        ..step
    });
    let problems = undescribed
        .check_artifacts(&LOSSY_ARTIFACTS)
        .expect_err("undescribed");
    assert!(reported(&problems, &["doesn't describe"]), "{problems:?}");
}

/// The artifacts of the lossy catalog with a `Pager` variant its revision adds to `kind`
fn pager_artifacts() -> ReverseArtifacts {
    // the fixture's changes plus the added variant
    ReverseArtifacts {
        changes: Vec::from([StepChanges {
            step: "contacts-v2",
            lost: &["contact"],
            new_variants: &[EnumVariant {
                field: "kind",
                variant: "Pager",
            }],
        }])
        .leak(),
        ..LOSSY_ARTIFACTS
    }
}

/// The lossy catalog with its step mapping `Pager` to `Phone` on a field
///
/// # Arguments
///
/// * `field` - The field the mapping names
fn pager_mapping(field: &'static str) -> Catalog {
    // keep the fixture's sentinel fill and add the mapping
    let mappings = vec![EnumMapping {
        field,
        new_variant: "Pager",
        old_variant: "Phone",
    }]
    .leak();
    with_step(&LOSSY, "contacts-v2", |step| StepDef {
        reversibility: Some(Reversibility::LossyRevert {
            fills: SENTINEL_FILL,
            lost_fields: &["contact"],
            enum_mappings: mappings,
            privilege_orders: &[],
            description: CONTACTS_LOSS,
        }),
        ..step
    })
}

/// A mapping counts only on the field its variant was added to, and the reverse runs it
#[test]
fn lossy_reverts_map_added_variants_on_their_own_field() {
    // a mapping of the same variant on another field doesn't count
    let added = pager_artifacts();
    let problems = pager_mapping("channel")
        .check_artifacts(&added)
        .expect_err("misplaced mapping");
    assert!(
        reported(&problems, &["contacts-v2", "no mapping", "Pager", "kind"]),
        "{problems:?}"
    );
    // the reverse runs the mapping: a user holding the added variant gets the old one, and a
    // user holding another variant keeps it
    let reversibility = pager_mapping("kind")
        .step("contacts-v2")
        .and_then(|(_, step)| step.reversibility)
        .expect("lossy");
    let user = |kind: &str| Record::Redis {
        key: "contacts:kind".to_owned(),
        fields: [
            ("contacts".to_owned(), "[\"ann@example.com\"]".to_owned()),
            ("kind".to_owned(), kind.to_owned()),
        ]
        .into(),
    };
    for (kind, expected) in [("Pager", "Phone"), ("Email", "Email")] {
        let reverted = CONTACTS_V2
            .revert(user(kind), Some(&reversibility))
            .expect("revert");
        assert_eq!(reverted.text("kind").as_deref(), Some(expected), "{kind}");
        assert_eq!(reverted.text("contact").as_deref(), Some("ann@example.com"));
    }
}

/// The sentinel fill of the lossy fixture step
const SENTINEL_FILL: &[Fill] = &[Fill {
    field: "contact",
    value: FillValue::Sentinel("unknown@contact.invalid"),
}];

/// Build the lossy fixture step's reversibility with other fills or another description
///
/// # Arguments
///
/// * `fills` - The fills
/// * `description` - The loss description
fn lossy_contacts(fills: &'static [Fill], description: &'static str) -> Reversibility {
    // keep the fixture's lost field and no mappings or orders
    Reversibility::LossyRevert {
        fills,
        lost_fields: &["contact"],
        enum_mappings: &[],
        privilege_orders: &[],
        description,
    }
}

/// Derive a contact from the stored `contacts` list as text, which only the new layout holds
///
/// # Arguments
///
/// * `record` - The user the fill is derived from
fn contacts_text(record: &Record) -> String {
    // copy the list, naming its absence
    record
        .text("contacts")
        .unwrap_or_else(|| "missing".to_owned())
}

/// A derived fill of the lossy fixture step's contact
const DERIVED_FILL: &[Fill] = &[Fill {
    field: "contact",
    value: FillValue::Derived(contacts_text),
}];

/// A derived fill reads the record in the new layout, before `down` ran, not the reverted one
#[test]
fn lossy_reverses_derive_fills_from_the_new_layout_record() {
    // a user with no contact to keep gets the fill derived from its list
    let reversibility = lossy_contacts(DERIVED_FILL, CONTACTS_LOSS);
    let reverted = CONTACTS_V2
        .revert(contact_user(0, "contacts", "[]"), Some(&reversibility))
        .expect("revert");
    assert_eq!(reverted.text("contact").as_deref(), Some("[]"));
    // a contact `down` derives is kept rather than filled
    let kept = CONTACTS_V2
        .revert(
            contact_user(1, "contacts", "[\"ann@example.com\"]"),
            Some(&reversibility),
        )
        .expect("revert");
    assert_eq!(kept.text("contact").as_deref(), Some("ann@example.com"));
}

/// CI rejects an `Irreversible` transform without a justification
#[test]
fn irreversible_steps_need_justifications() {
    // an irreversible step on a release edge with a justification passes
    let justified = with_step(&DOWNGRADE, "users-v3", |step| StepDef {
        reversibility: Some(Reversibility::Irreversible {
            justification: "the old layout can't hold the new data",
        }),
        ..step
    });
    assert_eq!(justified.check(), Ok(()));
    // without one, or with only blanks, it fails
    for justification in ["", "  "] {
        let unjustified = with_step(&DOWNGRADE, "users-v3", |step| StepDef {
            reversibility: Some(Reversibility::Irreversible { justification }),
            ..step
        });
        let problems = unjustified.check().expect_err("unjustified");
        assert!(
            reported(
                &problems,
                &["users-v3", "Irreversible without a justification"]
            ),
            "{problems:?}"
        );
    }
}

/// The security-sensitive fields of the lint tests: one of each kind, on users
const SENSITIVE: &[SensitiveField] = &[
    SensitiveField {
        family: USERS,
        field: "allowed",
        kind: SensitiveKind::Grant,
    },
    SensitiveField {
        family: USERS,
        field: "role",
        kind: SensitiveKind::Role {
            variants: ROLE_VARIANTS,
        },
    },
    SensitiveField {
        family: USERS,
        field: "groups",
        kind: SensitiveKind::GrantSet,
    },
    SensitiveField {
        family: USERS,
        field: "banned",
        kind: SensitiveKind::Denial {
            most_restrictive: "true",
        },
    },
    SensitiveField {
        family: USERS,
        field: "token",
        kind: SensitiveKind::Credential,
    },
    SensitiveField {
        family: USERS,
        field: "scope",
        kind: SensitiveKind::Other { deny: "deny-all" },
    },
];

/// Lint one lossy users step with these fills, mappings, and privilege orders
///
/// # Arguments
///
/// * `fills` - The step's fills
/// * `mappings` - The step's enum mappings
/// * `orders` - The step's privilege orders
/// * `exceptions` - The reviewed fill exceptions
fn lint(
    fills: &[Fill],
    mappings: &[EnumMapping],
    orders: &[PrivilegeOrder],
    exceptions: &[FillException],
) -> Vec<String> {
    // a users step filling and mapping what it is given
    let step = StepDef {
        id: "users-lossy",
        kind: StepKind::Data,
        transform: Some(&super::plan_fixtures::USERS_V2),
        reversibility: Some(Reversibility::LossyRevert {
            fills: fills.to_vec().leak(),
            lost_fields: &[],
            enum_mappings: mappings.to_vec().leak(),
            privilege_orders: orders.to_vec().leak(),
            description: "A fixture loss",
        }),
        ..StepDef::BASE
    };
    let artifacts = ReverseArtifacts {
        sensitive: SENSITIVE,
        fill_exceptions: exceptions.to_vec().leak(),
        ..ReverseArtifacts::EMPTY
    };
    lint_fills(&step, &artifacts)
}

/// Build a fill
///
/// # Arguments
///
/// * `field` - The filled field
/// * `value` - What it writes
fn fill(field: &'static str, value: FillValue) -> Fill {
    // keep the field and value as given
    Fill { field, value }
}

/// Build a mapping of the role field
///
/// # Arguments
///
/// * `new_variant` - The added role
/// * `old_variant` - The role it maps to
fn role(new_variant: &'static str, old_variant: &'static str) -> EnumMapping {
    // map on the fixture role field
    EnumMapping {
        field: "role",
        new_variant,
        old_variant,
    }
}

/// Derive a user's role by promoting it to `Admin`, which widens every other role
///
/// # Arguments
///
/// * `_record` - The user before the reverse
fn promote_role(_record: &Record) -> String {
    // every role becomes the one with every grant
    "Admin".to_owned()
}

/// Derive a user's role by keeping it, which grants nothing it didn't hold
///
/// # Arguments
///
/// * `record` - The user before the reverse
fn keep_role(record: &Record) -> String {
    // copy the role field
    record.text("role").unwrap_or_default()
}

/// Derive a user's role by disabling it
///
/// # Arguments
///
/// * `_record` - The user before the reverse
fn disable_role(_record: &Record) -> String {
    // every role becomes the one with no grants
    "Disabled".to_owned()
}

/// Derive a user's role as `User` whatever it held, which is narrower than some roles and widens
/// others
///
/// # Arguments
///
/// * `_record` - The user before the reverse
fn user_role(_record: &Record) -> String {
    // every role becomes User
    "User".to_owned()
}

/// Derive a value by copying a `source` field, which can hold anything, `true` included
///
/// # Arguments
///
/// * `record` - The user before the reverse
fn copy_source(record: &Record) -> String {
    // copy the field as stored
    record.text("source").unwrap_or_default()
}

/// Derive a token by copying another field, a generated credential
///
/// # Arguments
///
/// * `record` - The user before the reverse
fn copy_token(record: &Record) -> String {
    // copy the key into the token
    match record {
        Record::Redis { key, .. } | Record::Scylla { key, .. } | Record::S3 { key } => key.clone(),
    }
}

/// The permissive-fill lint rejects each fill the restrictive-fill table rejects on a field
/// marked security-sensitive, and accepts each restrictive fill; role mappings and fills are
/// judged by the declared privilege order
#[test]
fn fill_lint_rejects_permissive_fills() {
    // judge roles by the fixture role field's privilege order
    let order = [ROLE_ORDER];
    // each restrictive fill is accepted
    let restrictive = [
        fill("allowed", FillValue::Constant("false")),
        fill("role", FillValue::Constant("Disabled")),
        fill("role", FillValue::Derived(disable_role)),
        fill("role", FillValue::Derived(keep_role)),
        fill("groups", FillValue::Constant("[]")),
        fill("banned", FillValue::Constant("true")),
        fill("token", FillValue::Constant("")),
        fill("scope", FillValue::Sentinel("deny-all")),
        fill("nickname", FillValue::Constant("anything")),
    ];
    assert_eq!(lint(&restrictive, &[], &order, &[]), Vec::<String>::new());
    // each permissive fill is rejected on its own, for the reason its kind gives
    let grant = "a boolean grant may only be filled with false";
    let set = "a set of grants may only be filled with the empty set";
    let denial = "a denial may only be filled with its most restrictive value true";
    let credential = "a credential may only be cleared";
    let permissive = [
        (fill("allowed", FillValue::Constant("true")), grant),
        (fill("allowed", FillValue::Derived(copy_source)), grant),
        (
            fill("role", FillValue::Constant("User")),
            "the role User isn't below",
        ),
        (
            fill("role", FillValue::Derived(promote_role)),
            "the derived role Admin for",
        ),
        (fill("groups", FillValue::Constant("[\"admins\"]")), set),
        (fill("groups", FillValue::Derived(copy_source)), set),
        (fill("banned", FillValue::Constant("false")), denial),
        (fill("banned", FillValue::Derived(copy_source)), denial),
        (fill("token", FillValue::Derived(copy_token)), credential),
        (fill("token", FillValue::Constant("generated")), credential),
        (
            fill("scope", FillValue::Constant("read")),
            "only the deny sentinel deny-all or a reviewed exception is allowed",
        ),
    ];
    for (one, why) in permissive {
        let problems = lint(&[one], &[], &order, &[]);
        let fills = format!("step users-lossy fills {}", one.field);
        assert!(reported(&problems, &[&fills, why]), "{problems:?}");
    }
    // a derived role fill is judged on every source role: User is narrower than some roles but
    // widens Disabled, Uploader, and Viewer
    let problems = lint(
        &[fill("role", FillValue::Derived(user_role))],
        &[],
        &order,
        &[],
    );
    for source in ["Disabled", "Uploader", "Viewer"] {
        let why = format!("the derived role User for {source} holds a grant it lacks");
        assert!(reported(&problems, &[&why]), "{problems:?}");
    }
    assert!(!reported(&problems, &["for Admin"]), "{problems:?}");
    // an enum mapping on a sensitive field that isn't a role is rejected
    let scope = EnumMapping {
        field: "scope",
        new_variant: "write",
        old_variant: "read",
    };
    let problems = lint(&[], &[scope], &order, &[]);
    assert!(
        reported(
            &problems,
            &["maps a variant of scope, a sensitive field that isn't a role"]
        ),
        "{problems:?}"
    );
    // a reviewed exception lets an otherwise rejected fill of another sensitive field through
    let exception = FillException {
        step: "users-lossy",
        field: "scope",
        review: "reviewed: read is the least scope the old layout has",
    };
    assert!(accepted(&lint(
        &[fill("scope", FillValue::Constant("read"))],
        &[],
        &order,
        &[exception]
    )));
    let unreviewed = FillException {
        review: " ",
        ..exception
    };
    let problems = lint(
        &[fill("scope", FillValue::Constant("read"))],
        &[],
        &order,
        &[unreviewed],
    );
    assert!(
        reported(&problems, &["only the deny sentinel deny-all"]),
        "{problems:?}"
    );
}

/// The lint judges role mappings and role fills by the step's privilege order: a target may
/// hold no grant its source lacks, and the order must exist and list every variant
#[test]
fn fill_lint_judges_roles_by_the_privilege_order() {
    // judge roles by the fixture role field's privilege order
    let order = [ROLE_ORDER];
    // a rejected mapping names the variants and the grant the target holds beyond the source
    let widens = |new: &str, old: &str| {
        format!("maps {new} to {old} on role, which holds a grant {new} lacks")
    };
    // a new role narrower than User may map to Disabled but not to User
    let problems = lint(&[], &[role("Uploader", "User")], &order, &[]);
    assert!(
        reported(&problems, &[&widens("Uploader", "User")]),
        "{problems:?}"
    );
    assert!(accepted(&lint(
        &[],
        &[role("Uploader", "Disabled")],
        &order,
        &[]
    )));
    // Viewer holds `view`, which User lacks, so the two are incomparable: Viewer may map to
    // Disabled but not to User
    let problems = lint(&[], &[role("Viewer", "User")], &order, &[]);
    assert!(
        reported(&problems, &[&widens("Viewer", "User")]),
        "{problems:?}"
    );
    assert!(accepted(&lint(
        &[],
        &[role("Viewer", "Disabled")],
        &order,
        &[]
    )));
    // a new role broader than User may map to User
    assert!(accepted(&lint(
        &[],
        &[role("Operator", "User")],
        &order,
        &[]
    )));
    // Analyst and Developer are incomparable, so neither maps to the other
    for (new, old) in [("Analyst", "Developer"), ("Developer", "Analyst")] {
        let problems = lint(&[], &[role(new, old)], &order, &[]);
        assert!(reported(&problems, &[&widens(new, old)]), "{problems:?}");
    }
    // a constant role fill other than Disabled is rejected
    for constant in ["User", "Viewer", "Analyst", "Uploader"] {
        let problems = lint(
            &[fill("role", FillValue::Constant(constant))],
            &[],
            &order,
            &[],
        );
        let why = format!("the role {constant} isn't below");
        assert!(reported(&problems, &[&why]), "{problems:?}");
    }
    // a role a privilege order doesn't list is rejected
    let problems = lint(
        &[fill("role", FillValue::Sentinel("Nobody"))],
        &[],
        &order,
        &[],
    );
    assert!(
        reported(&problems, &["the role Nobody isn't below"]),
        "{problems:?}"
    );
    // a mapping or fill without a privilege order is rejected
    let problems = lint(&[], &[role("Viewer", "Disabled")], &[], &[]);
    assert!(
        reported(&problems, &["without a privilege order"]),
        "{problems:?}"
    );
    let problems = lint(
        &[fill("role", FillValue::Constant("Disabled"))],
        &[],
        &[],
        &[],
    );
    assert!(
        reported(&problems, &["without a privilege order"]),
        "{problems:?}"
    );
    // an order that leaves out a variant is rejected
    let partial = PrivilegeOrder {
        field: "role",
        variants: &ROLE_ORDER.variants[..5],
    };
    let problems = lint(&[], &[role("Viewer", "Disabled")], &[partial], &[]);
    assert!(
        reported(&problems, &["leaves out", "Viewer", "Operator"]),
        "{problems:?}"
    );
}

/// Classify any user as in the new layout, for the role-dropping fixture transform
///
/// # Arguments
///
/// * `_record` - The user to classify
fn any_user(_record: &Record) -> RecordClass {
    // every user counts as converted
    RecordClass::Current
}

/// Keep a user as it is, the role-dropping fixture transform's `up`
///
/// # Arguments
///
/// * `record` - The user to upgrade
#[allow(clippy::unnecessary_wraps)]
fn keep_user(record: Record) -> Result<Record, super::TransformIssue> {
    // nothing changes going up
    Ok(record)
}

/// Revert a user by dropping its role and the new layout's `access`, so role fills run
///
/// # Arguments
///
/// * `record` - The user to revert
#[allow(clippy::unnecessary_wraps)]
fn drop_role(mut record: Record) -> Result<Record, super::TransformIssue> {
    // remove both forms of the role
    if let Record::Redis { fields, .. } = &mut record {
        fields.remove("role");
        fields.remove("access");
    }
    Ok(record)
}

/// A users transform whose `down` leaves the role to the fills
static ROLE_DROPPER: TransformDef = TransformDef {
    family: USERS,
    reads: &[],
    from: 1,
    to: 2,
    classify: any_user,
    up: keep_user,
    down: Some(drop_role),
    optional: &[],
};

/// Derive a role from a `requested_role` field the user controls
///
/// # Arguments
///
/// * `record` - The user before the reverse
fn requested_role(record: &Record) -> String {
    // copy the requested role, defaulting to no grants
    record
        .text("requested_role")
        .unwrap_or_else(|| "Disabled".to_owned())
}

/// Derive a role from the new layout's renamed `access` field, where `owner` stands for `Admin`
///
/// # Arguments
///
/// * `record` - The user before the reverse
fn access_role(record: &Record) -> String {
    // translate the renamed field's values to roles
    match record.text("access").as_deref() {
        Some("owner") => "Admin".to_owned(),
        Some(other) => other.to_owned(),
        None => "Disabled".to_owned(),
    }
}

/// Build a lossy reverse filling the role with one value and judging it by the fixture order
///
/// # Arguments
///
/// * `value` - The role fill
fn role_fill(value: FillValue) -> Reversibility {
    // fill only the role, with the fixture privilege order
    Reversibility::LossyRevert {
        fills: vec![Fill {
            field: "role",
            value,
        }]
        .leak(),
        lost_fields: &["role"],
        enum_mappings: &[],
        privilege_orders: &[ROLE_ORDER],
        description: "A fixture loss",
    }
}

/// A role fill never writes a role broader than the one the user held, or than any role when
/// the user's role is unknown, whatever its derive read: such a user becomes an issue
#[test]
fn role_fills_never_widen_a_users_role_at_runtime() {
    // build a user from its fields
    let user = |pairs: &[(&str, &str)]| Record::Redis {
        key: "users:role".to_owned(),
        fields: pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    };
    let widened = IssueReason::new(IssueCategory::InvalidValue, "role");
    // a derive reading a field the user controls can't promote a User to Admin
    let requested = role_fill(FillValue::Derived(requested_role));
    let issue = ROLE_DROPPER
        .revert(
            user(&[("role", "User"), ("requested_role", "Admin")]),
            Some(&requested),
        )
        .expect_err("widened");
    assert_eq!(issue.reason, widened);
    // nor write a role the order doesn't list
    let issue = ROLE_DROPPER
        .revert(
            user(&[("role", "User"), ("requested_role", "SuperAdmin")]),
            Some(&requested),
        )
        .expect_err("unlisted");
    assert_eq!(issue.reason, widened);
    // a narrower role passes
    let reverted = ROLE_DROPPER
        .revert(
            user(&[("role", "User"), ("requested_role", "Uploader")]),
            Some(&requested),
        )
        .expect("narrower");
    assert_eq!(reverted.text("role").as_deref(), Some("Uploader"));
    // a derive reading the new layout's renamed field has no role to compare with, so only a
    // role narrower than every role passes
    let renamed = role_fill(FillValue::Derived(access_role));
    for access in ["owner", "Viewer", "User"] {
        let issue = ROLE_DROPPER
            .revert(user(&[("access", access)]), Some(&renamed))
            .expect_err("no source role");
        assert_eq!(issue.reason, widened, "{access}");
    }
    let reverted = ROLE_DROPPER
        .revert(user(&[("access", "Disabled")]), Some(&renamed))
        .expect("no grants");
    assert_eq!(reverted.text("role").as_deref(), Some("Disabled"));
    // a constant fill with no grants always passes
    let disabled = role_fill(FillValue::Constant("Disabled"));
    let reverted = ROLE_DROPPER
        .revert(user(&[("role", "Admin")]), Some(&disabled))
        .expect("disabled");
    assert_eq!(reverted.text("role").as_deref(), Some("Disabled"));
    // the lint still judges the derives on the role field alone, so it can't see these
    let step = StepDef {
        id: "users-roles",
        transform: Some(&ROLE_DROPPER),
        reversibility: Some(requested),
        ..StepDef::BASE
    };
    let artifacts = ReverseArtifacts {
        sensitive: SENSITIVE,
        ..ReverseArtifacts::EMPTY
    };
    assert!(accepted(&lint_fills(&step, &artifacts)));
}

/// The lint rejects a privilege order that lists a variant twice, and a step that declares two
/// orders for one field, since the first entry found would decide the variant's grants
#[test]
fn fill_lint_rejects_malformed_privilege_orders() {
    // an order listing Admin first with no grants, then with its real ones
    let doubled = PrivilegeOrder {
        field: "role",
        variants: [
            &[VariantGrants {
                variant: "Admin",
                grants: &[],
            }],
            ROLE_ORDER.variants,
        ]
        .concat()
        .leak(),
    };
    let problems = lint(
        &[fill("role", FillValue::Constant("Admin"))],
        &[],
        &[doubled],
        &[],
    );
    assert!(
        reported(
            &problems,
            &["privilege order for role", "lists Admin more than once"]
        ),
        "{problems:?}"
    );
    // two orders for the role, the first giving every variant no grants
    let empty = PrivilegeOrder {
        field: "role",
        variants: ROLE_ORDER
            .variants
            .iter()
            .map(|variant| VariantGrants {
                grants: &[],
                ..*variant
            })
            .collect::<Vec<_>>()
            .leak(),
    };
    let problems = lint(&[], &[role("Viewer", "Admin")], &[empty, ROLE_ORDER], &[]);
    assert!(
        reported(
            &problems,
            &["step users-lossy", "more than one privilege order for role"]
        ),
        "{problems:?}"
    );
    // either order alone judges the same fills as before
    assert!(!accepted(&lint(
        &[],
        &[role("Viewer", "Admin")],
        &[ROLE_ORDER],
        &[]
    )));
}

/// A fill of `null`, which the lint accepts as a cleared credential, clears the field on a Redis
/// record as it clears the column on a Scylla row, rather than storing the text `null`
#[test]
fn null_fills_clear_fields_on_every_backend() {
    // the reverse leaves the token absent and its fill clears it
    let cleared = Reversibility::LossyRevert {
        fills: &[
            Fill {
                field: "contact",
                value: FillValue::Sentinel(CONTACT_SENTINEL),
            },
            Fill {
                field: "token",
                value: FillValue::Constant("null"),
            },
        ],
        lost_fields: &["contact"],
        enum_mappings: &[],
        privilege_orders: &[],
        description: CONTACTS_LOSS,
    };
    let reverted = CONTACTS_V2
        .revert(contact_user(0, "contacts", "[]"), Some(&cleared))
        .expect("revert");
    assert!(!reverted.has_field("token"), "{reverted:?}");
    assert_eq!(reverted.text("contact").as_deref(), Some(CONTACT_SENTINEL));
    // setting a Redis field to null removes it
    let mut user = contact_user(1, "token", "secret");
    user.set_text("token", "null").expect("set");
    assert!(!user.has_field("token"), "{user:?}");
    // and a Scylla column to null clears it
    let mut row = Record::Scylla {
        key: "users:1".to_owned(),
        columns: [(
            "token".to_owned(),
            Some(scylla::value::CqlValue::Text("secret".to_owned())),
        )]
        .into(),
    };
    row.set_text("token", "null").expect("set");
    assert_eq!(row.text("token"), None);
}

/// The lint only judges fields marked sensitive in the step's family, and the catalog check runs
/// it over every lossy step
#[test]
fn fill_lint_covers_marked_fields_of_the_steps_family() {
    // a field of the same name in another family isn't sensitive
    let step = StepDef {
        id: "contacts-lossy",
        transform: Some(&super::downgrade_fixtures::CONTACTS_V2),
        reversibility: Some(Reversibility::LossyRevert {
            fills: &[Fill {
                field: "allowed",
                value: FillValue::Constant("true"),
            }],
            lost_fields: &[],
            enum_mappings: &[],
            privilege_orders: &[],
            description: "A fixture loss",
        }),
        ..StepDef::BASE
    };
    assert_ne!(
        step.transform.map(|transform| transform.family),
        Some(USERS)
    );
    assert_eq!(
        step.transform.map(|transform| transform.family),
        Some(CONTACTS)
    );
    let artifacts = ReverseArtifacts {
        sensitive: SENSITIVE,
        ..ReverseArtifacts::EMPTY
    };
    assert!(accepted(&lint_fills(&step, &artifacts)));
    // the catalog check runs the lint over every lossy step
    let catalog = with_step(&LOSSY, "contacts-v2", |step| StepDef {
        reversibility: Some(lossy_contacts(
            &[Fill {
                field: "contact",
                value: FillValue::Constant("root@example.com"),
            }],
            "Users keep only their first contact",
        )),
        ..step
    });
    let sensitive_contacts = ReverseArtifacts {
        sensitive: &[SensitiveField {
            family: CONTACTS,
            field: "contact",
            kind: SensitiveKind::Credential,
        }],
        ..LOSSY_ARTIFACTS
    };
    let problems = catalog
        .check_artifacts(&sensitive_contacts)
        .expect_err("lint");
    assert!(
        reported(&problems, &["contacts-v2", "contact", "cleared"]),
        "{problems:?}"
    );
}

/// A contract step dropping an object that an additive step of a later edge adds back fails the
/// catalog check, since the converge placed first would add it before the drop
#[test]
fn contracts_undone_by_later_additive_steps_fail() {
    // a contract dropping a column on the first edge and an additive step re-adding it later
    let column = |field| SchemaTarget {
        backend: SchemaBackend::Scylla,
        object: "samples",
        field: Some(field),
    };
    let contract = |targets: &'static [SchemaTarget]| StepDef {
        id: "drop-column",
        kind: StepKind::Data,
        description: "A fixture step",
        manual_procedure: Some("A fixture procedure"),
        phase: Some(MigrationPhase::Contract),
        targets,
        ..StepDef::BASE
    };
    let additive = |targets: &'static [SchemaTarget]| StepDef {
        id: "add-column",
        kind: StepKind::Infra,
        description: "A fixture step",
        phase: Some(MigrationPhase::Expand),
        targets,
        ..StepDef::BASE
    };
    let catalog = |dropped: &'static [SchemaTarget], added: &'static [SchemaTarget]| Catalog {
        edges: vec![
            fixtures::ONE_EDGE.edges[0],
            CatalogEntry {
                steps: vec![contract(dropped)].leak(),
                ..fixtures::ONE_EDGE.edges[1]
            },
            CatalogEntry {
                from: Some(RELEASE),
                id: NEXT,
                steps: vec![additive(added)].leak(),
                ..fixtures::ONE_EDGE.edges[1]
            },
        ]
        .leak(),
        floors: &[],
    };
    // the same column dropped then added fails
    let same: &'static [SchemaTarget] = vec![column("old")].leak();
    let problems = catalog(same, same).check().expect_err("re-added");
    assert!(
        reported(&problems, &["drop-column", "add-column", "adds back"]),
        "{problems:?}"
    );
    // adding a different column is fine
    let other: &'static [SchemaTarget] = vec![column("new")].leak();
    assert_eq!(catalog(same, other).check(), Ok(()));
    // an additive step on the same edge, ahead of the contract, is fine
    let earlier = Catalog {
        edges: vec![
            fixtures::ONE_EDGE.edges[0],
            CatalogEntry {
                steps: vec![additive(same), contract(same)].leak(),
                ..fixtures::ONE_EDGE.edges[1]
            },
        ]
        .leak(),
        floors: &[],
    };
    assert_eq!(earlier.check(), Ok(()));
    // the base the edges start from is the fixture baseline
    assert_eq!(fixtures::ONE_EDGE.edges[1].from, Some(BASE));
}
