//! The revisions and steps of the pattern matrix
//!
//! `<base>-pre.matrix-1` to `-5` and `-5b` mirror the commit T series one to one: additions,
//! restructuring, joined multi-step changes, removal and lossy restructuring, and an irreversible
//! step, with a sibling edge at the last depth. Each pre-release edge has its revert edge.

use thorium::models::upgrades::{
    Catalog, CatalogEntry, EnumMapping, Fill, FillValue, MigrationPhase, Reversibility, StepDef,
    StepKind, TransformDef, USERS_SETTINGS_THEME, UpgradeComponent,
};

use super::transforms::{
    ACL_ALLOWED, FILES_LAYOUT, ITEMS_CONTRACT, ITEMS_KEY, ITEMS_RENAME, OWNERS_KEY, PEOPLE_MERGE,
    PEOPLE_SPLIT, RANKS_LINK, SENTINEL, TAGS_NORMALIZE, TAGS_STATUS, TASKS_TYPE, TOKENS_REKEY,
};

/// The latest release the matrix builds on: a revision of the production catalog
pub const BASE: &str = "2026-10-v04";

/// Matrix revision 1, which adds objects only (T1)
pub const M1: &str = "2026-10-v04-pre.matrix-1";

/// Matrix revision 2, which restructures (T2)
pub const M2: &str = "2026-10-v04-pre.matrix-2";

/// Matrix revision 3, which joins steps into shared passes (T3)
pub const M3: &str = "2026-10-v04-pre.matrix-3";

/// Matrix revision 4, which removes and restructures with loss (T4)
pub const M4: &str = "2026-10-v04-pre.matrix-4";

/// Matrix revision 5, which holds an irreversible step (T5)
pub const M5: &str = "2026-10-v04-pre.matrix-5";

/// Matrix revision 5b, the sibling of revision 5 without the irreversible step (T5b)
pub const M5B: &str = "2026-10-v04-pre.matrix-5b";

/// The release the users example's pre-release builds on: the first Helm revision, whose users are
/// in layout 2
pub const USERS_BASE: &str = "2026-10-v02";

/// The pre-release the users example starts from, which writes users in layout 1
pub const R1: &str = "2026-10-v02-pre.users-theme";

/// The release the users example reaches, which writes users in layout 2
pub const R2: &str = "2026-10-v03";

/// The procedure every fixture step that needs an approval names
const PROCEDURE: &str = "A fixture procedure";

/// Why the tokens step can't be undone
pub const TOKENS_JUSTIFICATION: &str = "token identifiers are referenced by other objects";

/// Build a step with a kind and a phase
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `kind` - What kind of step it is
/// * `phase` - Where it sits in expand, migrate, contract
const fn step(id: &'static str, kind: StepKind, phase: Option<MigrationPhase>) -> StepDef {
    // every step that needs an approval names a procedure
    StepDef {
        id,
        kind,
        description: "A matrix step",
        phase,
        manual_procedure: Some(PROCEDURE),
        ..StepDef::BASE
    }
}

/// Build an additive step, which every plan folds into its first converge
///
/// # Arguments
///
/// * `id` - The id of the step
const fn additive(id: &'static str) -> StepDef {
    // additive schema needs no approval
    step(id, StepKind::Infra, Some(MigrationPhase::Expand))
}

/// Build a step that runs a transform in the migrate phase
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `transform` - The transform it runs
/// * `reversibility` - How the step is undone
const fn migrate(
    id: &'static str,
    transform: &'static TransformDef,
    reversibility: Reversibility,
) -> StepDef {
    // a data step with a transform, named for its migration
    StepDef {
        transform: Some(transform),
        thoradm_migration: Some(id),
        reversibility: Some(reversibility),
        ..step(id, StepKind::Data, Some(MigrationPhase::Migrate))
    }
}

/// Build a step that runs a transform in the contract phase, after everything else
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `transform` - The transform it runs
/// * `reversibility` - How the step is undone
const fn contract(
    id: &'static str,
    transform: &'static TransformDef,
    reversibility: Reversibility,
) -> StepDef {
    // the same step in the contract phase
    StepDef {
        phase: Some(MigrationPhase::Contract),
        ..migrate(id, transform, reversibility)
    }
}

/// Build a step of a revert edge that runs the `down` of a forward step
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `reverses` - The forward step it undoes
const fn reverse(id: &'static str, reverses: &'static str) -> StepDef {
    // a data step that names the forward step whose transform it runs backwards
    StepDef {
        reverses: Some(reverses),
        thoradm_migration: Some(id),
        ..step(id, StepKind::Data, Some(MigrationPhase::Migrate))
    }
}

/// Build a step of a revert edge that needs no data pass and no approval
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `reverses` - The forward step it undoes
const fn leave(id: &'static str, reverses: &'static str) -> StepDef {
    // a config step that only records that the forward step's result stays
    StepDef {
        reverses: Some(reverses),
        ..step(id, StepKind::Config, None)
    }
}

/// Build a rebuild: a migration with no transform that rebuilds an index
///
/// # Arguments
///
/// * `id` - The id of the step
/// * `supersedes` - The steps it makes redundant
/// * `reverses` - The forward rebuild it undoes, for a rebuild on a revert edge
const fn rebuild(
    id: &'static str,
    supersedes: &'static [&'static str],
    reverses: Option<&'static str>,
) -> StepDef {
    // a data step with a migration and no transform
    StepDef {
        thoradm_migration: Some(id),
        supersedes,
        reverses,
        ..step(id, StepKind::Data, Some(MigrationPhase::Migrate))
    }
}

/// Build a fixture edge
///
/// # Arguments
///
/// * `from` - The revision the edge starts from
/// * `id` - The revision the edge reaches
/// * `prerelease` - Whether the edge joins a pre-release id
/// * `sibling` - Whether the edge is a sibling path to a depth the primary chain reaches
/// * `steps` - The edge's steps
const fn edge(
    from: Option<&'static str>,
    id: &'static str,
    prerelease: bool,
    sibling: bool,
    steps: &'static [StepDef],
) -> CatalogEntry {
    // nothing else about an edge differs
    CatalogEntry {
        from,
        id,
        released_with: "0.0.0",
        description: "A matrix edge",
        schema: None,
        prerelease,
        sibling,
        steps,
    }
}

/// The reverse of an enum variant that exists only in the newer layout: it stands for `inactive`
const ARCHIVED: EnumMapping = EnumMapping {
    field: "status",
    new_variant: "archived",
    old_variant: "inactive",
};

/// What the contract of `label` loses
pub const ITEMS_LOSS: &str = "Items lose their labels";

/// What the merge of a person's names loses
pub const PEOPLE_LOSS: &str = "People lose the split between their first and last names";

/// What the new tag status loses
pub const TAGS_LOSS: &str = "Archived tags become inactive";

/// What the new access field loses
pub const ACL_LOSS: &str = "Records that were public become closed";

/// The dropped `label` comes back as the sentinel
pub const ITEMS_LOSSY: Reversibility = Reversibility::LossyRevert {
    fills: &[Fill {
        field: "label",
        value: FillValue::Sentinel(SENTINEL),
    }],
    lost_fields: &["label"],
    enum_mappings: &[],
    privilege_orders: &[],
    description: ITEMS_LOSS,
};

/// A name with no space comes back with the sentinel as its last name
pub const PEOPLE_LOSSY: Reversibility = Reversibility::LossyRevert {
    fills: &[Fill {
        field: "last",
        value: FillValue::Sentinel(SENTINEL),
    }],
    lost_fields: &["last"],
    enum_mappings: &[],
    privilege_orders: &[],
    description: PEOPLE_LOSS,
};

/// An archived tag comes back inactive
pub const TAGS_LOSSY: Reversibility = Reversibility::LossyRevert {
    fills: &[],
    lost_fields: &["status"],
    enum_mappings: &[ARCHIVED],
    privilege_orders: &[],
    description: TAGS_LOSS,
};

/// Every record comes back closed, whatever it was: the fill fails closed
pub const ACL_LOSSY: Reversibility = Reversibility::LossyRevert {
    fills: &[Fill {
        field: "public",
        value: FillValue::Constant("false"),
    }],
    lost_fields: &["public"],
    enum_mappings: &[],
    privilege_orders: &[],
    description: ACL_LOSS,
};

/// The additions of matrix revision 1: a table, a view, a column, an S3 prefix, and a mapping
const M1_STEPS: &[StepDef] = &[
    additive("matrix-table"),
    additive("matrix-view"),
    additive("matrix-column"),
    additive("matrix-prefix"),
    additive("matrix-mapping"),
];

/// Matrix revision 1's revert edge leaves the additions in place
const M1_REVERT: &[StepDef] = &[leave("matrix-additions-left", "matrix-table")];

/// The restructuring of matrix revision 2: a rename with dual writes, a split, a type change, a
/// key change, a move to a new S3 key layout, an index rebuild, and a view under a new name, which is listed last and still runs
/// first, since every plan converges additive steps before anything else
const M2_STEPS: &[StepDef] = &[
    migrate(
        "items-rename",
        &ITEMS_RENAME,
        Reversibility::DualWrite { until: M4 },
    ),
    migrate("people-split", &PEOPLE_SPLIT, Reversibility::Backfill),
    migrate("tasks-type", &TASKS_TYPE, Reversibility::Backfill),
    migrate("owners-key", &OWNERS_KEY, Reversibility::Backfill),
    migrate("files-layout", &FILES_LAYOUT, Reversibility::Backfill),
    rebuild("results-rebuild-1", &[], None),
    additive("items-by-owner-view"),
];

/// Matrix revision 2's revert edge: the rename needs no pass, every other transform runs backwards
const M2_REVERT: &[StepDef] = &[
    leave("items-rename-reverse", "items-rename"),
    reverse("people-split-reverse", "people-split"),
    reverse("tasks-type-reverse", "tasks-type"),
    reverse("owners-key-reverse", "owners-key"),
    reverse("files-layout-reverse", "files-layout"),
    rebuild("results-rebuild-1-reverse", &[], Some("results-rebuild-1")),
];

/// The joined steps of matrix revision 3: a second transform on items that fuses with the
/// rename, a pass on tags that stands alone, a transform on ranks that waits for the tags pass,
/// and a second rebuild that merges with the first
const M3_STEPS: &[StepDef] = &[
    migrate("items-key", &ITEMS_KEY, Reversibility::Backfill),
    migrate("tags-normalize", &TAGS_NORMALIZE, Reversibility::Backfill),
    migrate("ranks-link", &RANKS_LINK, Reversibility::Backfill),
    rebuild("results-rebuild-2", &["results-rebuild-1"], None),
];

/// Matrix revision 3's revert edge
const M3_REVERT: &[StepDef] = &[
    reverse("items-key-reverse", "items-key"),
    reverse("tags-normalize-reverse", "tags-normalize"),
    reverse("ranks-link-reverse", "ranks-link"),
    rebuild("results-rebuild-2-reverse", &[], Some("results-rebuild-2")),
];

/// The removals and lossy changes of matrix revision 4: the contract of `label`, the drop of the
/// old view, a merge of names, a new tag status, and a security field only a fill can restore
const M4_STEPS: &[StepDef] = &[
    contract("items-contract", &ITEMS_CONTRACT, ITEMS_LOSSY),
    StepDef {
        phase: Some(MigrationPhase::Contract),
        ..step("items-by-label-drop", StepKind::Data, None)
    },
    migrate("people-merge", &PEOPLE_MERGE, PEOPLE_LOSSY),
    migrate("tags-status", &TAGS_STATUS, TAGS_LOSSY),
    migrate("acl-allowed", &ACL_ALLOWED, ACL_LOSSY),
];

/// Matrix revision 4's revert edge: every transform reverses with its loss, and the dropped view
/// comes back from the previous revision's schema
const M4_REVERT: &[StepDef] = &[
    reverse("items-contract-reverse", "items-contract"),
    StepDef {
        reverses: Some("items-by-label-drop"),
        ..additive("items-by-label-restore")
    },
    reverse("people-merge-reverse", "people-merge"),
    reverse("tags-status-reverse", "tags-status"),
    reverse("acl-allowed-reverse", "acl-allowed"),
];

/// The step of matrix revision 5 that can't be undone
const M5_STEPS: &[StepDef] = &[migrate(
    "tokens-rekey",
    &TOKENS_REKEY,
    Reversibility::Irreversible {
        justification: TOKENS_JUSTIFICATION,
    },
)];

/// Matrix revision 5's revert edge, which holds the step that names the manual restore
const M5_REVERT: &[StepDef] = &[StepDef {
    reverses: Some("tokens-rekey"),
    ..step("tokens-rekey-reverse", StepKind::Manual, None)
}];

/// The sibling revision's only step, an addition
const M5B_STEPS: &[StepDef] = &[additive("sibling-note")];

/// The sibling revision's revert edge leaves its addition in place
const M5B_REVERT: &[StepDef] = &[leave("sibling-note-left", "sibling-note")];

/// The users example's step, which moves each user's theme into its settings with the production
/// transform
const USERS_THEME_STEPS: &[StepDef] = &[StepDef {
    pause: &[UpgradeComponent::Api],
    group_max: 0,
    ..migrate(
        "users-settings-theme",
        &USERS_SETTINGS_THEME,
        Reversibility::Backfill,
    )
}];

/// The users example's revert edge, which runs the step's `down`
const USERS_THEME_REVERT: &[StepDef] = &[StepDef {
    pause: &[UpgradeComponent::Api],
    group_max: 0,
    ..reverse("users-settings-theme-reverse", "users-settings-theme")
}];

/// The edges of the pattern matrix, on the latest release
pub const MATRIX_EDGES: &[CatalogEntry] = &[
    edge(None, BASE, false, false, &[]),
    edge(Some(BASE), M1, true, false, M1_STEPS),
    edge(Some(M1), BASE, true, false, M1_REVERT),
    edge(Some(M1), M2, true, false, M2_STEPS),
    edge(Some(M2), M1, true, false, M2_REVERT),
    edge(Some(M2), M3, true, false, M3_STEPS),
    edge(Some(M3), M2, true, false, M3_REVERT),
    edge(Some(M3), M4, true, false, M4_STEPS),
    edge(Some(M4), M3, true, false, M4_REVERT),
    edge(Some(M4), M5, true, false, M5_STEPS),
    edge(Some(M5), M4, true, false, M5_REVERT),
    edge(Some(M4), M5B, true, true, M5B_STEPS),
    edge(Some(M5B), M4, true, true, M5B_REVERT),
];

/// The pattern matrix: a baseline at the latest release, the five revisions stacked on it, and
/// the sibling at the last depth, each with its revert edge
pub const MATRIX: Catalog = Catalog {
    edges: MATRIX_EDGES,
    floors: &[],
};

/// The edges of the users example: a pre-release that writes users in layout 1, and the release
/// whose step converts them, each with the edge back
pub const USERS_THEME_EDGES: &[CatalogEntry] = &[
    edge(None, USERS_BASE, false, false, &[]),
    edge(Some(USERS_BASE), R1, true, false, &[]),
    edge(Some(R1), USERS_BASE, true, false, &[]),
    edge(Some(R1), R2, true, false, USERS_THEME_STEPS),
    edge(Some(R2), R1, true, false, USERS_THEME_REVERT),
];

/// The users example's `R1 -> R2` edge: `R1` exists only here, while `USERS_BASE` and `R2` are
/// revisions of the production catalog
pub const USERS_THEME: Catalog = Catalog {
    edges: USERS_THEME_EDGES,
    floors: &[],
};
