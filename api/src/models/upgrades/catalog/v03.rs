//! Revision `2026-10-v03`: the revision after the first Helm revision, which applies the schema
//! before any step that changes data

#[cfg(feature = "scylla-utils")]
use crate::models::upgrades::USERS_SETTINGS_THEME;
use crate::models::upgrades::{
    CatalogEntry, MigrationPhase, Reversibility, SchemaBackend, SchemaTarget, StepDef, StepKind,
    UpgradeComponent,
};

/// The id of the step that applies the schema, the first step of this revision
pub const SCHEMA_APPLY_STEP: &str = "schema-apply";

/// The id of the step that converts users from layout 1 to layout 2, which moves each user's theme
/// into its settings
pub const USERS_SETTINGS_THEME_STEP: &str = "users-settings-theme";

/// The id of the step of the reverse edge that converts users from layout 2 back to layout 1
pub const USERS_SETTINGS_THEME_REVERSE_STEP: &str = "users-settings-theme-reverse";

/// The steps of revision `2026-10-v03`, in the order they run
///
/// This list is append only: each change that needs work on existing deployments in this revision
/// appends its steps here, after the steps already listed, and never reorders or removes them.
/// The first step converges Scylla to the schema this revision declares, so every later step finds
/// the tables, views, and columns it reads and writes.
pub(super) const STEPS: &[StepDef] = &[
    StepDef {
        id: SCHEMA_APPLY_STEP,
        kind: StepKind::Infra,
        description: "Apply the schema in a Job before any step that changes data, creating \
                      missing tables, columns, and views and recording what was applied",
        ..StepDef::BASE
    },
    StepDef {
        id: USERS_SETTINGS_THEME_STEP,
        kind: StepKind::Data,
        description: "Move each user's theme from its own field into the user's settings",
        pause: &[UpgradeComponent::Api],
        thoradm_migration: Some(USERS_SETTINGS_THEME_STEP),
        manual_procedure: Some(
            "Run `thoradm migrate run users-settings-theme` with the API scaled to zero. A user \
             whose theme can't be read is listed by `thoradm migrate issues`; fix the user or \
             accept the code with `acceptIssues` in the approval.",
        ),
        targets: &[SchemaTarget {
            backend: SchemaBackend::Redis,
            object: "users",
            field: None,
        }],
        phase: Some(MigrationPhase::Migrate),
        #[cfg(feature = "scylla-utils")]
        transform: Some(&USERS_SETTINGS_THEME),
        reversibility: Some(Reversibility::Backfill),
        // one user the API can't load breaks every user listing, so no unreadable user is tolerated
        group_max: 0,
        ..StepDef::BASE
    },
];

/// The edge from the first Helm revision to the revision that applies the schema first
///
/// Releasing a revision requires, besides the entry itself:
///
/// * `released_with` set to the Thorium version the revision ships in (the workspace version that
///   `.github/scripts/versions.py check` keeps in agreement across Cargo, the UI, the charts,
///   minithor, megathor, and the docs)
/// * an executor for every new step in `EXECUTORS` (`operator/src/upgrades/steps.rs`)
/// * every data migration a step names registered in thoradm's `MIGRATIONS`
/// * a review of how a namespace without recorded state is seeded
///   (`operator/src/upgrades/detect.rs` and the operator's `FIRST_HELM_REVISION`)
/// * the committed snapshot of the revision and its fingerprint in `schema`
///   (`cargo run -p thorium-api --example schema_report -- --write-snapshot <revision>`)
/// * the catalog test that pins [`super::latest`], and the operator and thoradm tests and probes
///   that name the newest revision
/// * the revision and its steps documented in the admin guide's "Upgrading Thorium"
///   (`api/docs/src/admins/deploy/upgrades.md`, section "Revisions"), and any chart, megathor, or
///   docs example that names the newest revision as a `targetRevision`
pub(super) const ENTRY: CatalogEntry = CatalogEntry {
    from: Some("2026-10-v02"),
    id: "2026-10-v03",
    released_with: "1.8.1",
    description: "Schema apply runs before any step that changes data",
    schema: Some("f9f605025a1d9dca23c939a5dec4b9699f7e525877b8439e214de03bdd6bd346"),
    prerelease: false,
    sibling: false,
    steps: STEPS,
};
