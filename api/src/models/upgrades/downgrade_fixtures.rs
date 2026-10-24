//! Test-only fixture catalogs with reverse edges, the transforms and artifacts their downgrades
//! are tested on, and a privilege order for the fill lint
//!
//! The release edges reuse the users transforms of the planner's fixtures (layout 1 `dark`,
//! layout 2 `theme`, layout 3 `settings`); the lossy catalog moves a user's single `contact` to a
//! `contacts` list, whose reverse fills a contact it can't derive with a sentinel.

use super::fixtures::{BASE, NEXT, RELEASE};
use super::plan_fixtures::{self, FILES_V2, USERS_V2, USERS_V3, UserVariant};
use super::{
    Catalog, CatalogEntry, CompatibilityTest, Family, Fill, FillValue, IssueCategory, IssueReason,
    MigrationPhase, PrivilegeOrder, Record, RecordClass, ReverseArtifacts, Reversibility,
    StepChanges, StepDef, StepFixtures, StepKind, TransformDef, TransformIssue, VariantGrants,
};

/// The procedure every fixture step that needs an approval names
const PROCEDURE: &str = "A fixture procedure";

/// Build a data step with a phase
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `phase` - Where it sits in expand, migrate, contract
const fn data(id: &'static str, phase: Option<MigrationPhase>) -> StepDef {
    StepDef {
        id,
        kind: StepKind::Data,
        description: "A fixture step",
        phase,
        manual_procedure: Some(PROCEDURE),
        ..StepDef::BASE
    }
}

/// Build a step running a transform, declaring how it is undone
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `transform` - The transform it runs
/// * `reversibility` - How it is undone
const fn transform(
    id: &'static str,
    transform: &'static TransformDef,
    reversibility: Reversibility,
) -> StepDef {
    StepDef {
        transform: Some(transform),
        thoradm_migration: Some(id),
        reversibility: Some(reversibility),
        ..data(id, Some(MigrationPhase::Migrate))
    }
}

/// Build a step of an edge leading down that reverses a forward step
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `reverses` - The forward step it undoes
const fn reverse(id: &'static str, reverses: &'static str) -> StepDef {
    StepDef {
        reverses: Some(reverses),
        thoradm_migration: Some(id),
        ..data(id, Some(MigrationPhase::Migrate))
    }
}

/// Build a rebuild: a migration without a transform
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `supersedes` - The steps it makes redundant
/// * `reverses` - The forward rebuild it undoes, for a rebuild on an edge leading down
const fn rebuild(
    id: &'static str,
    supersedes: &'static [&'static str],
    reverses: Option<&'static str>,
) -> StepDef {
    StepDef {
        thoradm_migration: Some(id),
        supersedes,
        reverses,
        ..data(id, Some(MigrationPhase::Migrate))
    }
}

/// Build a fixture release or reverse edge
///
/// # Arguments
///
/// * `from` - The revision the edge starts from
/// * `id` - The revision the edge reaches
/// * `steps` - The edge's steps
const fn edge(
    from: Option<&'static str>,
    id: &'static str,
    steps: &'static [StepDef],
) -> CatalogEntry {
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

/// The steps of the release edge to [`RELEASE`] in [`DOWNGRADE`]
const RELEASE_STEPS: &[StepDef] = &[
    transform("users-v2", &USERS_V2, Reversibility::Backfill),
    rebuild("rebuild-a", &[], None),
];

/// The steps of the reverse edge from [`RELEASE`] in [`DOWNGRADE`]
const RELEASE_REVERSE: &[StepDef] = &[
    reverse("users-v2-reverse", "users-v2"),
    rebuild("rebuild-a-reverse", &[], Some("rebuild-a")),
];

/// The steps of the release edge to [`NEXT`] in [`DOWNGRADE`]
const NEXT_STEPS: &[StepDef] = &[
    transform("users-v3", &USERS_V3, Reversibility::Backfill),
    rebuild("rebuild-b", &["rebuild-a"], None),
];

/// The steps of the reverse edge from [`NEXT`] in [`DOWNGRADE`]
const NEXT_REVERSE: &[StepDef] = &[
    reverse("users-v3-reverse", "users-v3"),
    rebuild("rebuild-b-reverse", &[], Some("rebuild-b")),
];

/// Two release edges, each with a reverse edge: users move from layout 1 to 2 and 2 to 3, and
/// each edge rebuilds the results index, the second superseding the first
pub const DOWNGRADE: Catalog = Catalog {
    edges: &[
        edge(None, BASE, &[]),
        edge(Some(BASE), RELEASE, RELEASE_STEPS),
        edge(Some(RELEASE), BASE, RELEASE_REVERSE),
        edge(Some(RELEASE), NEXT, NEXT_STEPS),
        edge(Some(NEXT), RELEASE, NEXT_REVERSE),
    ],
    floors: &[],
};

/// Build users in layout 1, which the round trip of `users-v2` runs on
fn layout_1_users() -> Vec<Record> {
    // a spread of themes and optional flags
    plan_fixtures::users(UserVariant::Layout1, 16)
}

/// Build users in layout 2, which the round trip of `users-v3` and the new-layout fixtures of
/// `users-v2` run on
fn layout_2_users() -> Vec<Record> {
    // a spread of themes and optional flags
    plan_fixtures::users(UserVariant::Layout2, 16)
}

/// Build users in layout 3, which the new-layout fixtures of `users-v3` run on
fn layout_3_users() -> Vec<Record> {
    // a spread of themes and optional flags
    plan_fixtures::users(UserVariant::Layout3, 16)
}

/// The artifacts of [`DOWNGRADE`]: round-trip and new-layout fixtures for both users transforms
pub const DOWNGRADE_ARTIFACTS: ReverseArtifacts = ReverseArtifacts {
    round_trips: &[
        StepFixtures {
            step: "users-v2",
            fixtures: layout_1_users,
        },
        StepFixtures {
            step: "users-v3",
            fixtures: layout_2_users,
        },
    ],
    native: &[
        StepFixtures {
            step: "users-v2",
            fixtures: layout_2_users,
        },
        StepFixtures {
            step: "users-v3",
            fixtures: layout_3_users,
        },
    ],
    ..ReverseArtifacts::EMPTY
};

/// A release edge with a reverse edge, then a release edge without one
pub const ONE_WAY: Catalog = Catalog {
    edges: &[
        edge(None, BASE, &[]),
        edge(Some(BASE), RELEASE, RELEASE_STEPS),
        edge(Some(RELEASE), BASE, RELEASE_REVERSE),
        edge(Some(RELEASE), NEXT, NEXT_STEPS),
    ],
    floors: &[],
};

/// The justification of the irreversible fixture step
pub const KEYS_JUSTIFICATION: &str = "file keys are content hashes other objects reference";

/// Like [`DOWNGRADE`], but the edge to [`NEXT`] also moves files with an irreversible step
pub const IRREVERSIBLE: Catalog = Catalog {
    edges: &[
        edge(None, BASE, &[]),
        edge(Some(BASE), RELEASE, RELEASE_STEPS),
        edge(Some(RELEASE), BASE, RELEASE_REVERSE),
        edge(
            Some(RELEASE),
            NEXT,
            &[
                transform("users-v3", &USERS_V3, Reversibility::Backfill),
                transform(
                    "files-v2",
                    &FILES_V2,
                    Reversibility::Irreversible {
                        justification: KEYS_JUSTIFICATION,
                    },
                ),
            ],
        ),
        edge(
            Some(NEXT),
            RELEASE,
            &[reverse("users-v3-reverse", "users-v3")],
        ),
    ],
    floors: &[],
};

/// The contacts family the lossy fixture transform rewrites
pub const CONTACTS: Family = Family::Redis("contacts");

/// The contact the lossy reverse fills in when a user has none: an address the previous layout
/// accepts but never writes, which a re-upgrade reads as "no contact"
pub const CONTACT_SENTINEL: &str = "unknown@contact.invalid";

/// Build the issue of a contacts transform that couldn't convert a user
///
/// # Arguments
///
/// * `category` - The kind of problem
/// * `field` - The field's path
/// * `layout` - The layout the transform tried
fn contact_issue(category: IssueCategory, field: &str, layout: u32) -> TransformIssue {
    // name the one layout tried
    TransformIssue {
        reason: IssueReason::new(category, field),
        layouts_tried: vec![layout],
    }
}

/// Parse a user's `contacts` list
///
/// # Arguments
///
/// * `raw` - The stored list
fn parse_contacts(raw: &str) -> Option<Vec<String>> {
    // the list is a JSON array of addresses
    serde_json::from_str(raw).ok()
}

/// Classify a user for [`CONTACTS_V2`]: current with a valid `contacts` list, migratable with a
/// single `contact`
///
/// # Arguments
///
/// * `record` - The user to classify
fn contacts_classify(record: &Record) -> RecordClass {
    // only Redis users can be classified
    let Record::Redis { fields, .. } = record else {
        return RecordClass::Unrecognized {
            reason: IssueReason::new(IssueCategory::MalformedJson, ""),
        };
    };
    // the list must parse, and a single contact is the old layout
    match (fields.get("contacts"), fields.get("contact")) {
        (Some(raw), _) if parse_contacts(raw).is_some() => RecordClass::Current,
        (Some(_), _) => RecordClass::Unrecognized {
            reason: IssueReason::new(IssueCategory::MalformedJson, "contacts"),
        },
        (None, Some(_)) => RecordClass::Migratable { layout: 1 },
        (None, None) => RecordClass::Unrecognized {
            reason: IssueReason::new(IssueCategory::MissingField, "contact"),
        },
    }
}

/// Upgrade a user for [`CONTACTS_V2`]: its single contact becomes a list of one, and the sentinel
/// becomes an empty list rather than a fabricated address
///
/// # Arguments
///
/// * `record` - The user to upgrade
fn contacts_up(mut record: Record) -> Result<Record, TransformIssue> {
    // only Redis users convert
    let Record::Redis { fields, .. } = &mut record else {
        return Err(contact_issue(IssueCategory::MalformedJson, "", 1));
    };
    // a user holding the old contact is converted from it, even beside a stale list
    if let Some(contact) = fields.remove("contact") {
        let list = if contact == CONTACT_SENTINEL {
            Vec::new()
        } else {
            vec![contact]
        };
        let raw = serde_json::to_string(&list).unwrap_or_default();
        fields.insert("contacts".to_owned(), raw);
        return Ok(record);
    }
    // otherwise only a current user is left as it is
    match contacts_classify(&record) {
        RecordClass::Unrecognized { reason } => Err(TransformIssue {
            reason,
            layouts_tried: vec![1],
        }),
        _ => Ok(record),
    }
}

/// Revert a user for [`CONTACTS_V2`]: its first contact becomes its single contact; a user with
/// none is left without one for the fill
///
/// # Arguments
///
/// * `record` - The user to revert
fn contacts_down(mut record: Record) -> Result<Record, TransformIssue> {
    // only a Redis user with a valid list reverts
    let Record::Redis { fields, .. } = &mut record else {
        return Err(contact_issue(IssueCategory::MalformedJson, "", 2));
    };
    let raw = fields
        .remove("contacts")
        .ok_or_else(|| contact_issue(IssueCategory::MissingField, "contacts", 2))?;
    let list = parse_contacts(&raw)
        .ok_or_else(|| contact_issue(IssueCategory::MalformedJson, "contacts", 2))?;
    // keep the first contact, losing the rest
    if let Some(first) = list.into_iter().next() {
        fields.insert("contact".to_owned(), first);
    }
    Ok(record)
}

/// Users from a single `contact` to a `contacts` list
pub const CONTACTS_V2: TransformDef = TransformDef {
    family: CONTACTS,
    reads: &[],
    from: 1,
    to: 2,
    classify: contacts_classify,
    up: contacts_up,
    down: Some(contacts_down),
    optional: &[],
};

/// The loss description of the lossy fixture step
pub const CONTACTS_LOSS: &str = "Users keep only their first contact";

/// How the lossy fixture step is undone: the contact is filled with the sentinel
pub const CONTACTS_LOSSY: Reversibility = Reversibility::LossyRevert {
    fills: &[Fill {
        field: "contact",
        value: FillValue::Sentinel(CONTACT_SENTINEL),
    }],
    lost_fields: &["contact"],
    enum_mappings: &[],
    privilege_orders: &[],
    description: CONTACTS_LOSS,
};

/// One release edge moving contacts with a lossy reverse edge
pub const LOSSY: Catalog = Catalog {
    edges: &[
        edge(None, BASE, &[]),
        edge(
            Some(BASE),
            RELEASE,
            &[transform("contacts-v2", &CONTACTS_V2, CONTACTS_LOSSY)],
        ),
        edge(
            Some(RELEASE),
            BASE,
            &[reverse("contacts-v2-reverse", "contacts-v2")],
        ),
    ],
    floors: &[],
};

/// Build a user of the contacts fixtures
///
/// # Arguments
///
/// * `index` - The user's number, used in its key
/// * `field` - The field holding its contacts
/// * `value` - The stored value
#[must_use]
pub fn contact_user(index: usize, field: &str, value: &str) -> Record {
    // a Redis hash with the one field
    Record::Redis {
        key: format!("contacts:{index}"),
        fields: [(field.to_owned(), value.to_owned())].into(),
    }
}

/// Build users with a single contact, which the round trip of `contacts-v2` runs on
fn single_contacts() -> Vec<Record> {
    // a few addresses
    ["ann@example.com", "bob@example.com"]
        .iter()
        .enumerate()
        .map(|(index, contact)| contact_user(index, "contact", contact))
        .collect()
}

/// A compatibility test that always passes, standing in for the generated one
///
/// Its signature is the one every registered compatibility test has, so it returns a result it
/// never fails
#[allow(clippy::unnecessary_wraps)]
fn compatible() -> Result<(), String> {
    // the fixture layouts load through each other's projections by construction
    Ok(())
}

/// The artifacts of [`LOSSY`]
pub const LOSSY_ARTIFACTS: ReverseArtifacts = ReverseArtifacts {
    round_trips: &[StepFixtures {
        step: "contacts-v2",
        fixtures: single_contacts,
    }],
    compatibility: &[CompatibilityTest {
        step: "contacts-v2",
        test: compatible,
    }],
    changes: &[StepChanges {
        step: "contacts-v2",
        lost: &["contact"],
        new_variants: &[],
    }],
    ..ReverseArtifacts::EMPTY
};

/// A step that removes the dual write of `users-v2` in [`DUAL_WRITE`]
const DROP_DARK: StepDef = data("drop-dark", Some(MigrationPhase::Contract));

/// Users move from layout 1 to 2 with dual writes until [`NEXT`], whose edge drops the old field
pub const DUAL_WRITE: Catalog = Catalog {
    edges: &[
        edge(None, BASE, &[]),
        edge(
            Some(BASE),
            RELEASE,
            &[transform(
                "users-v2",
                &USERS_V2,
                Reversibility::DualWrite { until: NEXT },
            )],
        ),
        edge(
            Some(RELEASE),
            BASE,
            &[reverse("users-v2-reverse", "users-v2")],
        ),
        edge(Some(RELEASE), NEXT, &[DROP_DARK]),
    ],
    floors: &[],
};

/// The artifacts of [`DUAL_WRITE`]
pub const DUAL_WRITE_ARTIFACTS: ReverseArtifacts = ReverseArtifacts {
    round_trips: &[StepFixtures {
        step: "users-v2",
        fixtures: layout_1_users,
    }],
    compatibility: &[CompatibilityTest {
        step: "users-v2",
        test: compatible,
    }],
    ..ReverseArtifacts::EMPTY
};

/// The variants of the fixture role field: today's `UserRole` and three roles a new revision adds,
/// `Uploader` (narrower than `User`), `Viewer` (incomparable with `User`: it holds `view`, which
/// `User` lacks) and `Operator` (broader than `User`)
pub const ROLE_VARIANTS: &[&str] = &[
    "Admin",
    "Analyst",
    "Developer",
    "User",
    "Disabled",
    "Uploader",
    "Viewer",
    "Operator",
];

/// The privilege order of the fixture role field, from the grants today's `UserRole` variants
/// document
pub const ROLE_ORDER: PrivilegeOrder = PrivilegeOrder {
    field: "role",
    variants: &[
        VariantGrants {
            variant: "Admin",
            grants: &[
                "admin",
                "upload",
                "jobs",
                "all-samples",
                "tools",
                "k8s",
                "bare-metal",
                "windows",
                "external",
                "kvm",
                "view",
            ],
        },
        VariantGrants {
            variant: "Analyst",
            grants: &["upload", "jobs", "all-samples"],
        },
        VariantGrants {
            variant: "Developer",
            grants: &[
                "upload",
                "jobs",
                "tools",
                "k8s",
                "bare-metal",
                "windows",
                "external",
                "kvm",
            ],
        },
        VariantGrants {
            variant: "User",
            grants: &["upload", "jobs"],
        },
        VariantGrants {
            variant: "Disabled",
            grants: &[],
        },
        VariantGrants {
            variant: "Uploader",
            grants: &["upload"],
        },
        VariantGrants {
            variant: "Viewer",
            grants: &["view"],
        },
        VariantGrants {
            variant: "Operator",
            grants: &["upload", "jobs", "view"],
        },
    ],
};
