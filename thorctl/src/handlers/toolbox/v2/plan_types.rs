//! The types a toolbox import, update, diff, and remove share to describe what they would do
//!
//! A [`Plan`] is built by reading the target groups (without writing anything) and comparing them
//! with a loaded manifest. `--dry-run` prints it, `diff` renders it, and an import applies it in
//! the waves [`super::graph::waves`] computes. The types serialize so `-o json` can print them.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The length of the spec hash prefix added to a label that has to be published under a new label
pub const HASH_SUFFIX_LEN: usize = 8;

/// The kinds of item a manifest holds that are created in Thorium
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A tool
    Tool,
    /// A toolset
    Toolset,
    /// An agent
    Agent,
    /// A workflow
    Workflow,
    /// A pipeline
    Pipeline,
}

impl ItemKind {
    /// Get the lowercase name of this kind
    #[must_use]
    pub fn as_str(self) -> &'static str {
        // each kind has a fixed name
        match self {
            ItemKind::Tool => "tool",
            ItemKind::Toolset => "toolset",
            ItemKind::Agent => "agent",
            ItemKind::Workflow => "workflow",
            ItemKind::Pipeline => "pipeline",
        }
    }
}

impl fmt::Display for ItemKind {
    /// Write the lowercase name of this kind
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the fixed name
        f.write_str(self.as_str())
    }
}

/// An item of a manifest, named by its kind and name
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ItemRef {
    /// What kind of item this is
    pub kind: ItemKind,
    /// The item's name
    pub name: String,
}

impl ItemRef {
    /// Build an item reference
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of the item
    /// * `name` - The name of the item
    pub fn new(kind: ItemKind, name: impl Into<String>) -> Self {
        // gather the parts
        ItemRef {
            kind,
            name: name.into(),
        }
    }
}

impl fmt::Display for ItemRef {
    /// Write the item as `kind name`
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write the kind then the name
        write!(f, "{} {}", self.kind, self.name)
    }
}

/// Whether a group a manifest names exists in the target Thorium
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupState {
    /// The group exists and the caller can write to it
    Exists,
    /// The group exists but the caller can't write to it
    Forbidden,
    /// The group is missing (and is created when the import is told to create groups)
    Missing,
}

/// The state of one group a manifest names
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupPlan {
    /// The group's name
    pub name: String,
    /// Whether the group exists and can be written to
    pub state: GroupState,
    /// The description the group is created with when it is missing
    pub description: Option<String>,
}

/// What an import does with a network policy the manifest defines
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyState {
    /// The policy doesn't exist and is created
    Create,
    /// An identical policy exists and is reused
    Reuse,
    /// A different policy of the same name exists (replaced only with `--update-network-policy`)
    Differs,
}

/// What an import does with one network policy
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyPlan {
    /// The policy's name
    pub name: String,
    /// What is done with it
    pub state: PolicyState,
}

/// How a tool version in a manifest compares with the versions of the tool in Thorium
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionClass {
    /// The tool doesn't exist yet, so this version is published as part of creating it
    New,
    /// Some instance of the tool already has this version's spec hash
    Unchanged,
    /// The tool exists and has no instance with this label or this spec
    NewVersion,
    /// The label exists on the tool with a different spec hash
    LabelConflict,
}

/// What an import does with one version of a tool
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionPlan {
    /// The version label in the manifest
    pub label: String,
    /// How the version compares with what is in Thorium
    pub class: VersionClass,
    /// The spec hash of the version's spec, when it could be computed
    pub spec_hash: Option<String>,
    /// The label the version is published under when that differs from `label` (a label
    /// conflict published with `--overwrite`)
    pub publish_label: Option<String>,
}

/// What is to be done with an item
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    /// The item doesn't exist and is created
    Create,
    /// The item exists and matches the manifest
    Unchanged,
    /// The item exists and is updated (an in-place kind that differs, or a tool that gains versions)
    Update,
    /// The item is in the manifest but an update (which creates nothing) leaves it out
    NotImported,
    /// The item exists, differs, and is left alone because the import skips conflicts
    Skipped,
}

/// Why an existing item can't simply be brought in line with the manifest
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    /// A tool version's label exists with another spec hash
    LabelConflict,
    /// A toolset, agent, workflow, or pipeline exists and differs from the manifest
    InPlaceDiffers,
}

/// An existing item that differs from the manifest
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    /// The item that differs
    pub item: ItemRef,
    /// The group the existing item is in
    pub group: String,
    /// The version label involved (for a tool version conflict)
    pub label: Option<String>,
    /// How the item differs
    pub kind: ConflictKind,
    /// A short description of the difference
    pub detail: String,
}

/// How a run decides conflicts when no person is asked
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Apply every difference (`--overwrite`)
    Overwrite,
    /// Leave everything that exists alone (`--skip-conflicts`)
    SkipConflicts,
    /// Ask for each conflict with a terminal, or fail listing them without one
    Ask,
}

/// What a person decides for one conflict
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Apply the manifest's version of the item
    Apply,
    /// Leave the existing item as it is
    Skip,
    /// Stop the whole run before anything more is written
    Quit,
}

/// What an import does with one item of the manifest
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemPlan {
    /// The item
    pub item: ItemRef,
    /// The group the item is in (for a pipeline, its config's group)
    pub group: String,
    /// What is done with the item
    pub state: ItemState,
    /// What is done with each version (tools only)
    pub versions: Vec<VersionPlan>,
    /// A unified diff of an in-place item's normalized view, when it differs
    pub diff: Option<String>,
    /// The conflict this item raises, if any
    pub conflict: Option<Conflict>,
}

/// A read-only description of what importing a manifest would do
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// The state of each group the manifest names
    pub groups: Vec<GroupPlan>,
    /// What is done with each network policy the manifest defines
    pub policies: Vec<PolicyPlan>,
    /// What is done with each item, in dependency order
    pub items: Vec<ItemPlan>,
    /// Informational lines, such as a tool whose latest differs from the manifest's
    pub notes: Vec<String>,
}

/// How many items of a plan are in each state
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    /// Items that are created
    pub create: usize,
    /// Items that are updated
    pub update: usize,
    /// Items that already match
    pub unchanged: usize,
    /// Items an update leaves out
    pub not_imported: usize,
    /// Items skipped because they conflict
    pub skipped: usize,
}

impl Plan {
    /// Get every conflict in this plan
    #[must_use]
    pub fn conflicts(&self) -> Vec<&Conflict> {
        // collect the conflict of each item that raises one
        self.items
            .iter()
            .filter_map(|item| item.conflict.as_ref())
            .collect()
    }

    /// Count the items in each state
    #[must_use]
    pub fn summary(&self) -> PlanSummary {
        // tally each item by state
        let mut summary = PlanSummary::default();
        for item in &self.items {
            match item.state {
                ItemState::Create => summary.create += 1,
                ItemState::Update => summary.update += 1,
                ItemState::Unchanged => summary.unchanged += 1,
                ItemState::NotImported => summary.not_imported += 1,
                ItemState::Skipped => summary.skipped += 1,
            }
        }
        summary
    }

    /// Whether applying this plan would write anything
    ///
    /// Missing groups, policies to create, items to create or update, and tool versions to
    /// publish all count; an item that is only skipped or left out does not.
    #[must_use]
    pub fn has_changes(&self) -> bool {
        // any group, policy, or item that is written counts
        self.groups
            .iter()
            .any(|group| group.state == GroupState::Missing)
            || self
                .policies
                .iter()
                .any(|policy| policy.state == PolicyState::Create)
            || self
                .items
                .iter()
                .any(|item| matches!(item.state, ItemState::Create | ItemState::Update))
    }
}

/// Get the label a conflicting version is published under: `LABEL+` and the first characters
/// of its spec hash
///
/// # Arguments
///
/// * `label` - The label in the manifest
/// * `spec_hash` - The spec hash of the version's spec
#[must_use]
pub fn label_with_hash(label: &str, spec_hash: &str) -> String {
    // keep the label unique by adding a prefix of the hash
    let suffix: String = spec_hash.chars().take(HASH_SUFFIX_LEN).collect();
    format!("{label}+{suffix}")
}
