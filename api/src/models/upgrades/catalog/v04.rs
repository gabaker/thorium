//! Revision `2026-10-v04`: the revision that converts the data of earlier releases for tools,
//! tool instances, and workflows

use crate::models::upgrades::{
    CatalogEntry, MigrationPhase, Reversibility, StepDef, StepKind, UpgradeComponent,
};

/// The id of the step that waits for in-flight reactions to finish
pub const DRAIN_REACTIONS_STEP: &str = "drain-reactions";

/// The id of the step that converts each image into a tool
pub const IMAGES_TO_TOOLS_STEP: &str = "images-to-tools";

/// The id of the step that pins each pipeline stage to a tool instance
pub const PIPELINES_TO_TOOL_STAGES_STEP: &str = "pipelines-to-tool-stages";

/// The id of the step that moves provenance tool names to their legacy field
pub const PROVENANCE_TO_LEGACY_TOOLS_STEP: &str = "provenance-to-legacy-tools";

/// The id of the step that records the revision once every conversion is done
pub const FINALIZE_TOOL_ONLY_STEP: &str = "finalize-tool-only";

/// The components the conversion steps pause while they rewrite stored data, so nothing of the
/// previous release reads or writes converted data until the new release's Deployments are applied
const CONVERSION_PAUSE: &[UpgradeComponent] = &[
    UpgradeComponent::Api,
    UpgradeComponent::Scaler,
    UpgradeComponent::BaremetalScaler,
    UpgradeComponent::EventHandler,
    UpgradeComponent::SearchStreamer,
];

/// The components a reaction needs running to finish, which the drain waits on
const DRAIN_NEEDS: &[UpgradeComponent] = &[
    UpgradeComponent::Api,
    UpgradeComponent::Scaler,
    UpgradeComponent::BaremetalScaler,
];

/// The steps of revision `2026-10-v04`, in the order they run
///
/// This list is append only: each change that needs work on existing deployments in this revision
/// appends its steps here, after the steps already listed, and never reorders or removes them.
/// The append only rule applies once a revision is released. A draining step must precede any
/// step that pauses the API: the operator keeps paused components at zero for the rest of the
/// plan, so a step that waits for in-flight work must run before one that pauses. The drain lists
/// the components it needs running, so the planner also puts it ahead of the steps of an earlier
/// revision that pause them when an upgrade crosses both revisions.
/// The revision has no schema-apply step of its own: the schema it declares differs from the
/// previous revision's only in how pipelines are stored in Redis, and the operator applies the
/// schema for the image it deploys before it rolls anything out (see `2026-10-v03`).
pub(super) const STEPS: &[StepDef] = &[
    StepDef {
        id: DRAIN_REACTIONS_STEP,
        kind: StepKind::Data,
        description: "Wait until no reaction is Created or Started and no stage-path worker is \
                  registered, with Thorium running normally; the step only checks and deletes \
                  nothing, and is skipped when nothing is in flight",
        thoradm_migration: Some(DRAIN_REACTIONS_STEP),
        manual_procedure: Some(
            "Stop submitting reactions and let the unfinished ones finish, or delete a stuck one with \
         `thorctl reactions delete --group <group> --pipeline <pipeline> --status started` (and \
         `--status created`) while the previous API is still running; do not restart or delete API pods meanwhile. \
         The step is done once no reaction is Created or Started.",
        ),
        phase: Some(MigrationPhase::Migrate),
        needs_running: DRAIN_NEEDS,
        ..StepDef::BASE
    },
    StepDef {
        id: IMAGES_TO_TOOLS_STEP,
        kind: StepKind::Data,
        description: "Create a tool and its first tool instance for every image, tagged as migrated \
                  from that image",
        pause: CONVERSION_PAUSE,
        thoradm_migration: Some(IMAGES_TO_TOOLS_STEP),
        manual_procedure: Some(
            "Back up Thorium, then run `thoradm migrate run --id images-to-tools` with the cluster \
         config while the step is blocked; the step then finds nothing to change and is skipped.",
        ),
        phase: Some(MigrationPhase::Migrate),
        // the drain can wait long while Thorium keeps writing, so the check runs again once it ends
        reads_after: &[DRAIN_REACTIONS_STEP],
        // a revert across this step is refused, since images can't be rebuilt from tools
        reversibility: Some(Reversibility::Irreversible {
            justification: "Each image becomes a tool with a first tool instance, and the API refuses writes to images \
                            afterwards, so images created or changed since can't be copied back. Restore a backup \
                            taken before this step instead.",
        }),
        ..StepDef::BASE
    },
    StepDef {
        id: PIPELINES_TO_TOOL_STAGES_STEP,
        kind: StepKind::Data,
        description: "Pin each image stage of every pipeline to the first tool instance created \
                  from its image",
        pause: CONVERSION_PAUSE,
        thoradm_migration: Some(PIPELINES_TO_TOOL_STAGES_STEP),
        manual_procedure: Some(
            "Back up Thorium, then run `thoradm migrate run --id pipelines-to-tool-stages` with the \
         cluster config while the step is blocked; the step then finds nothing to change and is \
         skipped.",
        ),
        phase: Some(MigrationPhase::Migrate),
        // its check reads the tools the earlier step creates, and runs again once the drain ends
        reads_after: &[DRAIN_REACTIONS_STEP, IMAGES_TO_TOOLS_STEP],
        // a revert across this step is refused, since the previous release can't read layout 2
        reversibility: Some(Reversibility::Irreversible {
            justification: "Each converted pipeline is stored in a second layout that gains the stage_tools field, \
                            which the previous release doesn't read, and pipelines created or changed since exist \
                            only in that layout. Restore a backup taken before this step instead.",
        }),
        ..StepDef::BASE
    },
    StepDef {
        id: PROVENANCE_TO_LEGACY_TOOLS_STEP,
        kind: StepKind::Data,
        description: "Move the tool names stored in file system, decompiled function, and process \
                  tree provenance into their legacy tools field",
        pause: CONVERSION_PAUSE,
        thoradm_migration: Some(PROVENANCE_TO_LEGACY_TOOLS_STEP),
        manual_procedure: Some(
            "Back up Thorium, then run `thoradm migrate run --id provenance-to-legacy-tools` with the \
         cluster config while the step is blocked; the step then finds nothing to change and is \
         skipped.",
        ),
        phase: Some(MigrationPhase::Migrate),
        // the drain can wait long while Thorium keeps writing, so the check runs again once it ends
        reads_after: &[DRAIN_REACTIONS_STEP],
        // a revert across this step is refused, since the previous release reads the moved names
        reversibility: Some(Reversibility::Irreversible {
            justification: "The tool names in file system, decompiled function, and process tree provenance move to \
                            the legacy_tools field, and provenance written since uses \
                            the new fields, so the previous release can't read it. Restore a backup taken before \
                            this step instead.",
        }),
        ..StepDef::BASE
    },
    StepDef {
        id: FINALIZE_TOOL_ONLY_STEP,
        kind: StepKind::Config,
        description: "Record the revision once every conversion step is done or skipped",
        ..StepDef::BASE
    },
];

/// The edge from the revision that applies the schema first to the revision that converts tools
///
/// Releasing a revision requires, besides the entry itself, what the entry of `2026-10-v03`
/// lists: `released_with`, an executor for every new step, the registered migrations, the review
/// of how a namespace without recorded state is seeded, the committed snapshot and its
/// fingerprint, the catalog, operator, and thoradm tests that pin the newest revision, and the
/// documentation of the revision and its steps.
pub(super) const ENTRY: CatalogEntry = CatalogEntry {
    from: Some("2026-10-v03"),
    id: "2026-10-v04",
    released_with: "1.8.1",
    description: "Images, pipeline stages, and provenance are converted for tools",
    schema: Some("f9f605025a1d9dca23c939a5dec4b9699f7e525877b8439e214de03bdd6bd346"),
    prerelease: false,
    sibling: false,
    steps: STEPS,
};
