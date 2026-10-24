//! Revision `2026-10-v01`: the baseline every converted cluster starts from

use crate::models::upgrades::CatalogEntry;

/// The baseline revision, which has no steps since every install starts at it
pub(super) const ENTRY: CatalogEntry = CatalogEntry {
    from: None,
    id: "2026-10-v01",
    released_with: "1.8.1",
    description: "Thorium deployed by the minithor or megathor scripts before the Helm \
                  charts (the baseline every converted cluster starts from)",
    schema: Some("34f954f5941b0d85cab0e897236f57ff201724c443e8e6dc61f00e55e40e306c"),
    prerelease: false,
    sibling: false,
    steps: &[],
};
