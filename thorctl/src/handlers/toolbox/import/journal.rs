//! The journal of what an import has changed, which rolling back replays in reverse

use std::sync::Mutex;
use thorium::models::{Entity, NetworkPolicy, Pipeline};
use uuid::Uuid;

use super::classify::RootChange;
use crate::handlers::toolbox::v2::plan_types::ItemKind;

/// What undoes the changes an import made to the root of an existing tool
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootUndo {
    /// The description to put back (`None` clears it), when the import changed it
    #[allow(clippy::option_option)]
    pub description: Option<Option<String>>,
    /// The groups the import added the tool to
    pub remove_groups: Vec<String>,
    /// The tag values the import added
    pub remove_tags: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// The harness env to put back (`None` clears it), when the import changed it
    #[allow(clippy::option_option)]
    pub harness_env: Option<Option<thorium::models::HarnessEnv>>,
    /// The model ids the import disabled, to enable again
    pub enable_models: std::collections::BTreeSet<String>,
    /// The model ids the import enabled, to disable again
    pub disable_models: std::collections::BTreeSet<String>,
}

impl RootUndo {
    /// Work out how to undo a change to a tool root
    ///
    /// # Arguments
    ///
    /// * `live` - The tool before the change
    /// * `change` - The change that was made
    #[must_use]
    pub fn from_change(live: &super::world::LiveTool, change: &RootChange) -> Self {
        // each part of the change is undone by restoring what the tool had
        RootUndo {
            description: change
                .description
                .as_ref()
                .map(|_| live.description.clone()),
            remove_groups: change.add_groups.clone(),
            remove_tags: change.add_tags.clone(),
            harness_env: change
                .harness_env
                .as_ref()
                .map(|_| live.harness_env.clone()),
            enable_models: change.disable_models.clone(),
            disable_models: change.enable_models.clone(),
        }
    }
}

/// One change an import made, with what undoes it
#[derive(Debug, Clone)]
pub enum Entry {
    /// A group was created
    CreatedGroup {
        /// The group's name
        name: String,
    },
    /// A network policy was created
    CreatedPolicy {
        /// The policy's name
        name: String,
        /// The policy's id, when it could be found
        id: Option<Uuid>,
    },
    /// A network policy was changed
    UpdatedPolicy {
        /// The policy as it was
        original: Box<NetworkPolicy>,
    },
    /// A tool was created along with its first version
    CreatedTool {
        /// The tool's name
        name: String,
        /// The tool's id
        id: Uuid,
    },
    /// A version was published on a tool
    PublishedVersion {
        /// The tool's name
        tool: String,
        /// The version's label
        label: String,
        /// The id of the new tool instance
        id: Uuid,
    },
    /// The root of an existing tool was changed
    UpdatedToolRoot {
        /// The tool's name
        name: String,
        /// The tool's id
        id: Uuid,
        /// How to put the root back
        undo: RootUndo,
    },
    /// A toolset, agent, or workflow was created
    CreatedEntity {
        /// What kind of item it is
        kind: ItemKind,
        /// The item's name
        name: String,
        /// The item's id
        id: Uuid,
    },
    /// A toolset, agent, or workflow was changed
    UpdatedEntity {
        /// What kind of item it is
        kind: ItemKind,
        /// The item as it was
        original: Box<Entity>,
    },
    /// A pipeline was created
    CreatedPipeline {
        /// The pipeline's group
        group: String,
        /// The pipeline's name
        name: String,
    },
    /// A pipeline was changed
    UpdatedPipeline {
        /// The pipeline as it was
        original: Box<Pipeline>,
    },
}

impl Entry {
    /// Describe the change in a line
    #[must_use]
    pub fn describe(&self) -> String {
        // each kind of change has its own wording
        match self {
            Entry::CreatedGroup { name } => format!("created group {name}"),
            Entry::CreatedPolicy { name, .. } => format!("created network policy {name}"),
            Entry::UpdatedPolicy { original } => {
                format!("updated network policy {}", original.name)
            }
            Entry::CreatedTool { name, .. } => format!("created tool {name}"),
            Entry::PublishedVersion { tool, label, .. } => {
                format!("published {tool}@{label}")
            }
            Entry::UpdatedToolRoot { name, .. } => format!("updated tool {name}"),
            Entry::CreatedEntity { kind, name, .. } => format!("created {kind} {name}"),
            Entry::UpdatedEntity { kind, original } => {
                format!("updated {kind} {}", original.name)
            }
            Entry::CreatedPipeline { group, name } => {
                format!("created pipeline {group}/{name}")
            }
            Entry::UpdatedPipeline { original } => {
                format!("updated pipeline {}/{}", original.group, original.name)
            }
        }
    }
}

/// The changes an import has made so far, in the order they were made
#[derive(Debug, Default)]
pub struct Journal {
    /// The entries, behind a lock because items of one wave are applied at once
    entries: Mutex<Vec<Entry>>,
}

impl Journal {
    /// Record a change that was made
    ///
    /// # Arguments
    ///
    /// * `entry` - The change
    pub fn record(&self, entry: Entry) {
        // keep recording even if another task panicked while holding the lock
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry);
    }

    /// Take every entry out of the journal
    #[must_use]
    pub fn take(&self) -> Vec<Entry> {
        // the journal is empty afterwards
        std::mem::take(
            &mut *self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Describe each change in the order it was made
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        // describe without taking the entries
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(Entry::describe)
            .collect()
    }
}
