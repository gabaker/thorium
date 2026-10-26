//! Tools, toolsets, and agents as commands and edit views name them
//!
//! A tool is named by its name or ID, optionally pinned to a version label (`yara@1.2.0`); a
//! toolset the same way with an instance revision (`ghidra#r3`). The edit views of agents and
//! toolsets carry the same references as `{tool, version}` and `{toolset, instance}` pairs and
//! resolve them to IDs only when an update needs the IDs. The list helpers here are shared by the
//! agent and toolset edits.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use thorium::models::{Entity, EntityKinds, EntityMetadata};
use thorium::{Error, Thorium};
use uuid::Uuid;

use super::find_in;
use crate::utils::patch::{EditOp, PathSeg};

/// A tool, followed at its latest version or pinned to one version
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRef {
    /// The tool's name (or ID)
    pub tool: String,
    /// The version label the tool is pinned to; unset follows the latest version
    #[serde(default)]
    pub version: Option<String>,
}

impl ToolRef {
    /// Parse `TOOL` or `TOOL@VERSION`
    ///
    /// # Arguments
    ///
    /// * `raw` - The reference as typed
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        // the version follows the last `@` when both sides are set
        match raw.rsplit_once('@') {
            Some((tool, version)) if !tool.is_empty() && !version.is_empty() => ToolRef {
                tool: tool.to_owned(),
                version: Some(version.to_owned()),
            },
            _ => ToolRef {
                tool: raw.to_owned(),
                version: None,
            },
        }
    }

    /// Build the reference an edge to a tool or tool instance stands for
    ///
    /// # Arguments
    ///
    /// * `target` - The tool or tool instance at the end of the edge
    #[must_use]
    pub fn from_entity(target: &Entity) -> Self {
        // an instance is named `{tool}@{version}`
        match &target.metadata {
            EntityMetadata::ToolInstance(meta) => {
                let version = meta.version.to_string();
                let suffix = format!("@{version}");
                ToolRef {
                    tool: target
                        .name
                        .strip_suffix(&suffix)
                        .unwrap_or(&target.name)
                        .to_owned(),
                    version: Some(version),
                }
            }
            _ => ToolRef {
                tool: target.name.clone(),
                version: None,
            },
        }
    }

    /// Find the tool or tool instance this reference names
    ///
    /// # Arguments
    ///
    /// * `thorium` - The Thorium client
    /// * `groups` - The groups to look in
    /// * `label` - What the tool is called in errors, with its article
    pub async fn resolve(
        &self,
        thorium: &Thorium,
        groups: &[String],
        label: &str,
    ) -> Result<Uuid, Error> {
        // find the tool itself
        let tool = find_in(thorium, EntityKinds::Tool, label, &self.tool, groups).await?;
        // without a version the edge follows the tool's latest version
        let Some(version) = &self.version else {
            return Ok(tool.id);
        };
        // otherwise the edge is pinned to the tool's instance with this version label
        let instances = thorium.entities.tool_instances(tool.id).await?;
        instances
            .iter()
            .find(|instance| instance.version.to_string() == *version)
            .map(|instance| instance.id)
            .ok_or_else(|| Error::new(format!("{label} {} has no version {version}", tool.name)))
    }
}

/// A toolset, followed at its latest instance or pinned to one instance
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSetRef {
    /// The toolset's name (or ID)
    pub toolset: String,
    /// The revision of the instance the toolset is pinned to; unset follows the latest instance
    #[serde(default)]
    pub instance: Option<u64>,
}

impl ToolSetRef {
    /// Parse `TOOLSET` or `TOOLSET#rN`
    ///
    /// # Arguments
    ///
    /// * `raw` - The reference as typed
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        // the revision follows the last `#r` when it is a number
        if let Some((toolset, revision)) = raw.rsplit_once("#r")
            && !toolset.is_empty()
            && let Ok(revision) = revision.parse::<u64>()
        {
            return ToolSetRef {
                toolset: toolset.to_owned(),
                instance: Some(revision),
            };
        }
        ToolSetRef {
            toolset: raw.to_owned(),
            instance: None,
        }
    }

    /// Build the reference an edge to a toolset or toolset instance stands for
    ///
    /// # Arguments
    ///
    /// * `target` - The toolset or toolset instance at the end of the edge
    #[must_use]
    pub fn from_entity(target: &Entity) -> Self {
        // an instance is named `{toolset}#r{revision}`
        match &target.metadata {
            EntityMetadata::ToolSetInstance(meta) => {
                let suffix = format!("#r{}", meta.revision);
                ToolSetRef {
                    toolset: target
                        .name
                        .strip_suffix(&suffix)
                        .unwrap_or(&target.name)
                        .to_owned(),
                    instance: Some(meta.revision),
                }
            }
            _ => ToolSetRef {
                toolset: target.name.clone(),
                instance: None,
            },
        }
    }

    /// Find the toolset or toolset instance this reference names
    ///
    /// # Arguments
    ///
    /// * `thorium` - The Thorium client
    /// * `groups` - The groups to look in
    pub async fn resolve(&self, thorium: &Thorium, groups: &[String]) -> Result<Uuid, Error> {
        // find the toolset itself
        let toolset = find_in(
            thorium,
            EntityKinds::ToolSet,
            "a toolset",
            &self.toolset,
            groups,
        )
        .await?;
        // without an instance the edge follows the toolset's latest instance
        let Some(revision) = self.instance else {
            return Ok(toolset.id);
        };
        // otherwise the edge is pinned to the instance named `{toolset}#r{revision}`
        let name = format!("{}#r{revision}", toolset.name);
        let label = "a toolset instance";
        find_in(thorium, EntityKinds::ToolSetInstance, label, &name, groups)
            .await
            .map(|instance| instance.id)
    }
}

/// Find an AI agent by ID or name
///
/// # Arguments
///
/// * `thorium` - The Thorium client
/// * `groups` - The groups to look in
/// * `value` - The agent's ID or name
pub async fn resolve_agent(
    thorium: &Thorium,
    groups: &[String],
    value: &str,
) -> Result<Uuid, Error> {
    // an agent named by ID is read to check its kind
    find_in(thorium, EntityKinds::Agent, "an AI agent", value, groups)
        .await
        .map(|agent| agent.id)
}

/// Find the first repeated item of a list
///
/// # Arguments
///
/// * `items` - The items to check
pub(crate) fn duplicate<T: Ord + Clone>(items: &[T]) -> Option<T> {
    // remember what was seen
    let mut seen = BTreeSet::new();
    items
        .iter()
        .find(|item| !seen.insert((*item).clone()))
        .cloned()
}

/// Split two lists into the items only the second has and the items only the first has
///
/// # Arguments
///
/// * `before` - The list as it was read
/// * `after` - The list as it was edited
pub(crate) fn diff<T: Ord + Clone>(before: &[T], after: &[T]) -> (Vec<T>, Vec<T>) {
    // compare the lists as sets
    let old: BTreeSet<&T> = before.iter().collect();
    let new: BTreeSet<&T> = after.iter().collect();
    let added = new.difference(&old).map(|item| (*item).clone()).collect();
    let removed = old.difference(&new).map(|item| (*item).clone()).collect();
    (added, removed)
}

/// Build the edit that sets one top-level field of the view
///
/// # Arguments
///
/// * `key` - The top-level key
/// * `value` - The value to set
pub(crate) fn set_key(key: &str, value: Value) -> EditOp {
    // a one-step path
    EditOp::Set {
        path: vec![PathSeg::Key(key.to_owned())],
        value,
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolRef, ToolSetRef};

    /// A tool reference splits its version at the last `@`
    #[test]
    fn tool_refs_parse() {
        // a bare name follows the latest version
        assert_eq!(
            ToolRef::parse("yara"),
            ToolRef {
                tool: "yara".into(),
                version: None
            }
        );
        // a version pins it
        let pinned = ToolRef::parse("yara@1.2.0");
        assert_eq!(pinned.version.as_deref(), Some("1.2.0"));
        // an empty side is not a version
        assert_eq!(ToolRef::parse("yara@").version, None);
        assert_eq!(ToolRef::parse("@1.0").tool, "@1.0");
    }

    /// A toolset reference splits its instance at the last `#r` followed by a number
    #[test]
    fn toolset_refs_parse() {
        // a bare name follows the latest instance
        assert_eq!(ToolSetRef::parse("ghidra").instance, None);
        // a revision pins it
        let pinned = ToolSetRef::parse("ghidra#r3");
        assert_eq!(pinned.instance, Some(3));
        assert_eq!(pinned.toolset, "ghidra");
        // anything else is part of the name
        assert_eq!(ToolSetRef::parse("ghidra#rx").instance, None);
        assert_eq!(ToolSetRef::parse("ghidra#rx").toolset, "ghidra#rx");
    }
}
