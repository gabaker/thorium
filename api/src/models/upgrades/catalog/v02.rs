//! Revision `2026-10-v02`: the first revision deployed by the Helm charts

use crate::models::upgrades::{CatalogEntry, StepDef, StepKind, UpgradeComponent};

/// The manual procedure for rebuilding Elastic indexes whose mappings aren't the declared ones,
/// such as a `group` field that isn't a keyword
pub const REINDEX_PROCEDURE: &str = "Back up Scylla (the search-streamer rebuilds every index from \
it), then scale the search-streamer \
Deployment to 0 replicas BEFORE deleting anything (Elastic recreates a deleted index with \
dynamic mappings if anything writes to it). Run `thoradm migrate run --id elastic-rebuild`, \
which deletes each Thorium index whose mappings aren't the declared ones (`thoradm migrate check \
--id elastic-rebuild` counts them first) and the init session keys of its source, then scale \
the search-streamer back up: it recreates every missing index with the right mappings and \
streams every result and tag from Scylla into it.";

/// The revision the first Helm charts deployed, the newest whose Scylla and Redis layouts are the
/// ones Thorium wrote before revisions were recorded (its edge from the pre-Helm baseline changes
/// only Elastic and checks credentials): the operator suggests it for a Helm deployment with data
/// and no record, and a restore records it for a backup that holds no schema record
pub const FIRST_HELM_REVISION: &str = "2026-10-v02";

/// The steps of revision `2026-10-v02`, the first revision deployed by the Helm charts
pub(in crate::models::upgrades) const HELM_STEPS: &[StepDef] = &[
    StepDef {
        id: "scylla-role",
        kind: StepKind::Infra,
        description: "Check that Thorium's Scylla role logs in with the configured credentials",
        check_only: true,
        ..StepDef::BASE
    },
    StepDef {
        id: "elastic-identity",
        kind: StepKind::Infra,
        description: "Check that Thorium's Elastic user authenticates and holds the index \
                      privileges Thorium needs",
        check_only: true,
        ..StepDef::BASE
    },
    StepDef {
        id: "elastic-reindex-keyword-mappings",
        kind: StepKind::Data,
        description: "Reindex Elastic indexes created without mappings, whose group field is \
                      not a keyword (megathor deployments before the indexes were left to the \
                      search-streamer); skipped when every index is already correct",
        pause: &[UpgradeComponent::SearchStreamer],
        thoradm_migration: Some("elastic-rebuild"),
        manual_procedure: Some(REINDEX_PROCEDURE),
        ..StepDef::BASE
    },
    StepDef {
        id: "finalize",
        kind: StepKind::Config,
        description: "Record the new revision",
        ..StepDef::BASE
    },
];

/// The edge from the baseline to the first Helm revision
pub(super) const ENTRY: CatalogEntry = CatalogEntry {
    from: Some("2026-10-v01"),
    id: FIRST_HELM_REVISION,
    released_with: "1.8.1",
    description: "Thorium deployed by the Helm charts with the chart-managed operator",
    schema: Some("34f954f5941b0d85cab0e897236f57ff201724c443e8e6dc61f00e55e40e306c"),
    prerelease: false,
    sibling: false,
    steps: HELM_STEPS,
};
