//! The reverse edge of revision `2026-10-v03`: the way back to the first Helm revision

use super::v03::{USERS_SETTINGS_THEME_REVERSE_STEP, USERS_SETTINGS_THEME_STEP};
use crate::models::upgrades::{CatalogEntry, MigrationPhase, StepDef, StepKind, UpgradeComponent};

/// The steps of the reverse edge, in the order they run
///
/// This list is append only: each step that undoes a step of `2026-10-v03` is appended here, naming
/// the step it reverses. Applying the schema is not undone: the base code ignores columns it
/// doesn't read. Users are not moved back to layout 1 either: the base code reads layout 2, the
/// layout this revision keeps them in, so the one step only converts a user left in layout 1
/// and skips every user already in layout 2, losing no theme.
pub(super) const STEPS: &[StepDef] = &[StepDef {
    id: USERS_SETTINGS_THEME_REVERSE_STEP,
    kind: StepKind::Data,
    description: "Move the theme of any user stored in its own field into the user's settings, \
                  the layout the first Helm revision reads; users already there are skipped",
    pause: &[UpgradeComponent::Api],
    thoradm_migration: Some(USERS_SETTINGS_THEME_REVERSE_STEP),
    manual_procedure: Some(
        "Run `thoradm migrate run users-settings-theme-reverse` with the API scaled to zero. A \
         user whose theme or settings can't be read is listed by `thoradm migrate issues`; fix \
         the user or accept the code with `acceptIssues` in the approval.",
    ),
    phase: Some(MigrationPhase::Migrate),
    reverses: Some(USERS_SETTINGS_THEME_STEP),
    group_max: 0,
    ..StepDef::BASE
}];

/// The edge from the revision that applies the schema first back to the first Helm revision
pub(super) const ENTRY: CatalogEntry = CatalogEntry {
    from: Some("2026-10-v03"),
    id: "2026-10-v02",
    released_with: "1.8.1",
    description: "Revert the revision that applies the schema first",
    schema: Some("34f954f5941b0d85cab0e897236f57ff201724c443e8e6dc61f00e55e40e306c"),
    prerelease: false,
    sibling: false,
    steps: STEPS,
};
