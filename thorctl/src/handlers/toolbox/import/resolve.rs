//! Turning the names in a manifest into the ids Thorium knows
//!
//! A manifest names other tools instead of holding ids, and the API wants ids. [`Ids`] holds the
//! ids of the tools the import has found or created, and resolves a name (and optionally a version
//! label) to the id of the tool or of that tool instance. Planning and applying use the same
//! resolution, so a plan's spec hashes are the hashes the apply will publish.

use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeMap;
use thorium::models::{HarnessToolAccess, ThorDigest, ToolSpec};
use uuid::Uuid;

use super::world::{InstanceInfo, LiveTool, World};
use crate::handlers::toolbox::v2::refs::{Ref, SlotKind, resolve_slots};
use crate::handlers::toolbox::v2::types::{ToolRef, ToolVersionManifest, ToolboxManifestV2};

/// The text that marks a reference whose target is not created yet
const PENDING: &str = "pending:";

/// One published version of a tool, as far as its id and spec hash are known
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownInstance {
    /// The id of the tool instance
    pub id: Uuid,
    /// The hash of the instance's spec
    pub spec_hash: String,
}

impl From<&InstanceInfo> for KnownInstance {
    /// Take the id and hash of a version Thorium holds
    ///
    /// # Arguments
    ///
    /// * `info` - The version to read
    fn from(info: &InstanceInfo) -> Self {
        // the two fields references need
        KnownInstance {
            id: info.id,
            spec_hash: info.spec_hash.clone(),
        }
    }
}

/// A tool whose id is known, with the versions that are known
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownTool {
    /// The tool's id
    pub id: Uuid,
    /// The known versions, keyed by label
    pub instances: BTreeMap<String, KnownInstance>,
    /// The label of the tool's latest version
    pub latest: Option<String>,
}

impl From<&LiveTool> for KnownTool {
    /// Take the ids of a tool Thorium holds
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool to read
    fn from(tool: &LiveTool) -> Self {
        // index the versions by label
        KnownTool {
            id: tool.id,
            instances: tool
                .instances
                .iter()
                .map(|instance| (instance.label.clone(), KnownInstance::from(instance)))
                .collect(),
            latest: tool.latest_label.clone(),
        }
    }
}

/// What resolving a name to an id found
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolve {
    /// The id
    Id(Uuid),
    /// The target is in the manifest and not created yet, so its id isn't known
    Pending(String),
    /// The target is neither in the manifest nor in Thorium
    Missing(String),
}

/// The ids of the tools a manifest's items refer to
#[derive(Debug, Clone, Default)]
pub struct Ids {
    /// The tools of the manifest that exist in Thorium or were created by this run, by name
    known: BTreeMap<String, KnownTool>,
}

impl Ids {
    /// Start from the tools of the manifest that already exist in Thorium
    ///
    /// # Arguments
    ///
    /// * `manifest` - The manifest being imported
    /// * `world` - What exists in Thorium
    #[must_use]
    pub fn from_world(manifest: &ToolboxManifestV2, world: &World) -> Self {
        // each manifest tool that is already in one of its groups is known
        let mut known = BTreeMap::new();
        for (name, tool) in &manifest.tools {
            if let Ok(Some(live)) = world.tool(name, &tool.groups) {
                known.insert(name.clone(), KnownTool::from(live));
            }
        }
        Ids { known }
    }

    /// Record a tool of the manifest that now exists
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name
    /// * `tool` - The tool's ids
    pub fn insert_tool(&mut self, name: &str, tool: KnownTool) {
        // a later record replaces an earlier one
        self.known.insert(name.to_owned(), tool);
    }

    /// Record a version of a tool that now exists, making it the tool's latest
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name
    /// * `label` - The version's label
    /// * `instance` - The version's id and hash
    pub fn insert_instance(&mut self, name: &str, label: &str, instance: KnownInstance) {
        // the newest version is the tool's latest
        if let Some(tool) = self.known.get_mut(name) {
            tool.instances.insert(label.to_owned(), instance);
            tool.latest = Some(label.to_owned());
        }
    }

    /// Find the spec hash of a tool instance by its id
    ///
    /// # Arguments
    ///
    /// * `world` - What exists in Thorium
    /// * `id` - The instance's id
    #[must_use]
    pub fn instance_hash(&self, world: &World, id: Uuid) -> Option<String> {
        // look through what this run knows, then what Thorium holds
        self.known
            .values()
            .flat_map(|tool| tool.instances.values())
            .find(|instance| instance.id == id)
            .map(|instance| instance.spec_hash.clone())
            .or_else(|| {
                world
                    .tools
                    .iter()
                    .flat_map(|tool| tool.instances.iter())
                    .find(|instance| instance.id == id)
                    .map(|instance| instance.spec_hash.clone())
            })
    }

    /// Resolve a tool name, and optionally a version label, to an id
    ///
    /// Without a label the id is the tool's, which follows its latest version. With one it is
    /// the id of that version.
    ///
    /// # Arguments
    ///
    /// * `manifest` - The manifest being imported
    /// * `world` - What exists in Thorium
    /// * `name` - The tool's name
    /// * `label` - The version label to pin
    /// * `groups` - The groups of the item that refers to the tool
    #[must_use]
    pub fn resolve(
        &self,
        manifest: &ToolboxManifestV2,
        world: &World,
        name: &str,
        label: Option<&str>,
        groups: &[String],
    ) -> Resolve {
        // find the tool: the manifest's own tools are looked up by what this run knows
        let found = if manifest.tools.contains_key(name) {
            match self.known.get(name) {
                Some(tool) => tool.clone(),
                None => {
                    return Resolve::Pending(format!("tool {name} is not created yet"));
                }
            }
        // tools the manifest doesn't define must already be in the item's groups
        } else {
            match world.tool(name, groups) {
                Ok(Some(live)) => KnownTool::from(live),
                Ok(None) => {
                    return Resolve::Missing(format!(
                        "tool {name} is neither in the toolbox nor in {}",
                        groups.join(", ")
                    ));
                }
                Err(message) => return Resolve::Missing(message),
            }
        };
        // without a label the tool's own id stands for it
        let Some(label) = label else {
            return Resolve::Id(found.id);
        };
        // with one, the version must exist, or be one the manifest is going to publish
        match found.instances.get(label) {
            Some(instance) => Resolve::Id(instance.id),
            None if manifest
                .tools
                .get(name)
                .is_some_and(|tool| tool.versions.contains_key(label)) =>
            {
                Resolve::Pending(format!(
                    "version {label} of tool {name} is not published yet"
                ))
            }
            None => Resolve::Missing(format!("tool {name} has no version {label}")),
        }
    }

    /// Find the id and hash of a version of a tool, or of its latest version
    ///
    /// # Arguments
    ///
    /// * `manifest` - The manifest being imported
    /// * `world` - What exists in Thorium
    /// * `tool` - The reference to the tool
    /// * `groups` - The groups of the item that refers to the tool
    fn instance_of(
        &self,
        manifest: &ToolboxManifestV2,
        world: &World,
        tool: &ToolRef,
        groups: &[String],
    ) -> Result<KnownInstance, Resolve> {
        // find the tool the same way a reference does
        let found = match manifest.tools.get(&tool.tool) {
            Some(_) => self.known.get(&tool.tool).cloned().ok_or_else(|| {
                Resolve::Pending(format!("tool {} is not created yet", tool.tool))
            })?,
            None => match world.tool(&tool.tool, groups) {
                Ok(Some(live)) => KnownTool::from(live),
                Ok(None) => {
                    return Err(Resolve::Missing(format!(
                        "tool {} is neither in the toolbox nor in {}",
                        tool.tool,
                        groups.join(", ")
                    )));
                }
                Err(message) => return Err(Resolve::Missing(message)),
            },
        };
        // a pinned version, or else the latest one
        let label = tool.version.clone().or_else(|| found.latest.clone());
        match label
            .as_deref()
            .and_then(|label| found.instances.get(label))
        {
            Some(instance) => Ok(instance.clone()),
            None if manifest.tools.contains_key(&tool.tool) => Err(Resolve::Pending(format!(
                "tool {} has no published version to provide yet",
                tool.tool
            ))),
            None => Err(Resolve::Missing(format!(
                "tool {} has no version to provide",
                tool.tool
            ))),
        }
    }
}

/// A tool version's spec made ready to send to Thorium
#[derive(Debug, Clone)]
pub struct PreparedSpec {
    /// The typed spec, with every reference replaced by an id
    pub spec: ToolSpec,
    /// The spec encoded as the API takes it
    pub json: String,
    /// The tool instances a harness version provides
    pub provides: Vec<Uuid>,
    /// The hash Thorium gives the spec
    pub spec_hash: String,
}

/// Whether a version's spec can be prepared yet
#[derive(Debug, Clone)]
pub enum Prepared {
    /// The spec is ready, with every id known
    Ready(Box<PreparedSpec>),
    /// The spec refers to something this run has not created yet
    Pending(String),
}

/// Make a tool version's spec ready to send: replace its references with ids, parse it as the API
/// does, and compute the hash Thorium will give it
///
/// # Arguments
///
/// * `manifest` - The manifest being imported
/// * `world` - What exists in Thorium
/// * `ids` - The ids the import knows
/// * `version` - The version whose spec to prepare
/// * `groups` - The groups of the tool the version belongs to
pub fn prepare_spec(
    manifest: &ToolboxManifestV2,
    world: &World,
    ids: &Ids,
    version: &ToolVersionManifest,
    groups: &[String],
) -> Result<Prepared, String> {
    // a version needs a spec by the time it is imported
    let Some(spec) = &version.spec else {
        return Err("has no spec".to_owned());
    };
    // replace each reference in a spec slot with the id of what it names
    let mut resolved: Value = spec.clone();
    let pending: RefCell<Option<String>> = RefCell::new(None);
    let problems = resolve_slots(&mut resolved, |reference, kind| match reference {
        Ref::Tool { name, label } => {
            // a tool slot follows the tool's latest; an instance slot pins a version
            let wanted = match kind {
                SlotKind::ToolId => None,
                SlotKind::ToolInstanceId => label.as_deref(),
            };
            match ids.resolve(manifest, world, name, wanted, groups) {
                Resolve::Id(id) => Ok(id),
                Resolve::Pending(reason) => {
                    pending.borrow_mut().get_or_insert(reason);
                    Err(PENDING.to_owned())
                }
                Resolve::Missing(reason) => Err(reason),
            }
        }
        other => Err(format!("{other} can't stand in a spec slot")),
    });
    // a reference to something not created yet means the id isn't known
    if let Some(reason) = pending.into_inner() {
        return Ok(Prepared::Pending(reason));
    }
    // anything else that couldn't be resolved is an error
    if let Some(problem) = problems.first() {
        return Err(format!("{}: {}", problem.path, problem.message));
    }
    // parse the typed spec the way the API does
    let mut typed = ToolSpec::parse_request(&resolved.to_string())?;
    // a default tool access block is stored unset, so it hashes like one left out
    if let ToolSpec::Harness(harness) = &mut typed
        && harness.interface.tool_access == Some(HarnessToolAccess::default())
    {
        harness.interface.tool_access = None;
    }
    // a container backing contributes the hash of the instance it pins
    let pinned = match typed.pinned_instance() {
        Some(id) => match ids.instance_hash(world, id) {
            Some(hash) => Some(hash),
            None => return Err(format!("the pinned tool instance {id} is not in Thorium")),
        },
        None => None,
    };
    // so do the native tools a harness provides
    let mut provides = Vec::new();
    let mut hashes = Vec::new();
    for tool in &version.provides {
        match ids.instance_of(manifest, world, tool, groups) {
            Ok(instance) => {
                provides.push(instance.id);
                hashes.push(instance.spec_hash);
            }
            Err(Resolve::Pending(reason)) => return Ok(Prepared::Pending(reason)),
            Err(Resolve::Missing(reason)) => return Err(reason),
            Err(Resolve::Id(_)) => return Err("provides resolved to an id".to_owned()),
        }
    }
    // hash the spec the way the API will
    let hash_refs: Vec<&str> = hashes.iter().map(String::as_str).collect();
    let spec_hash = ThorDigest::provided_tool_spec(&typed, pinned.as_deref(), &hash_refs)
        .map_err(|err| format!("failed to hash the spec: {err}"))?;
    let json =
        serde_json::to_string(&typed).map_err(|err| format!("failed to encode the spec: {err}"))?;
    Ok(Prepared::Ready(Box::new(PreparedSpec {
        spec: typed,
        json,
        provides,
        spec_hash,
    })))
}
